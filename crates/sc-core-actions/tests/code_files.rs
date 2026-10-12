//! `fs` **inside a code body**, against real directories and the real engine.
//!
//! Pinned to bytes on a disk for the reason the `fetch` tests are pinned to
//! bytes on a wire: what can be wrong here is what reached the filesystem and
//! what was made of what came back, and a mocked store would assert those about
//! a file that never existed. The prelude's own shapes are covered by unit tests
//! in `sc-expr` against a fake host; this is the whole path — the guest's `fs`,
//! the operation it builds, `sc-api`'s `FileStoreHost`, `sc-files`'s
//! `LocalFileStore`, and the file that is really there afterwards.
//!
//! Asserted here: a body reads and writes a connected store; `create` refuses to
//! replace and `write` replaces; a directory is made, listed, walked and
//! deleted; a copy moves bytes across two stores without carrying them through
//! the sandbox; metadata a body sets is the store's own; a **delegated** body
//! obeys both the folder rule and the store's floor while the trigger's own
//! authority does not; a file larger than one read may carry is refused rather
//! than truncated; and a store that is not connected is named the moment it is
//! asked for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_action::{ActionContext, Event, EventKind};
use sc_catalog::{Catalog, bootstrap_file_stores, save_file_store};
use sc_core_actions::builtin_actions;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_files::{FileMeta, FileStoreDef, LocalFileStore};
use sc_test_harness::TestDb;
use sc_types::Attrs;

use serde_json::{Value as Json, json};

/// A fresh directory that exists, and its path as a string.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sc-code-files-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A catalog with `docs` connected to a fresh directory, and that directory.
///
/// `_fd_file_stores` is bootstrapped even where no definition is saved: a
/// delegated operation reads the store's floor from it, and a table that is not
/// there is a different failure from a store that has no floor.
async fn setup(db: &TestDb, tag: &str) -> Result<(Catalog, PathBuf)> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_file_stores(&catalog).await?;
    let dir = temp_dir(tag);
    catalog.connect_file_store(Arc::new(LocalFileStore::new("docs", &dir)?))?;
    Ok((catalog, dir))
}

/// The event most tests fire for: a directly-run trigger by an admin.
fn fired() -> Event {
    Event::new(EventKind::None)
        .payload(json!({}))
        .caller(1, Some(json!({ "email": "admin@example.com" })))
}

/// The same, fired by an ordinary user — what `asUser()` delegates to.
fn fired_by_a_user() -> Event {
    Event::new(EventKind::None)
        .payload(json!({}))
        .caller(80, Some(json!({ "email": "ada@example.com" })))
}

/// Run `run_js_code` through the registry — the path a firing trigger takes.
async fn run(catalog: &Catalog, event: &Event, code: &str) -> Result<Json> {
    let engine: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let registry = builtin_actions()?;
    let action = registry.require("run_js_code")?.clone();
    let cfg: Attrs = [("code".to_owned(), json!(code))].into_iter().collect();
    let mut ctx = ActionContext::new(catalog, event, &cfg, "files").with_evaluator(&engine);
    action.run(&mut ctx).await
}

/// What is on disk at `path`, relative to the store root.
fn on_disk(dir: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(path)).ok()
}

#[tokio::test]
async fn a_body_reads_and_writes_a_connected_store() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "read-write").await?;
    std::fs::write(dir.join("the_file.txt"), "hello").unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"const theFile = fs("docs").open("the_file.txt");
           const exists = await theFile.exists();
           const theString = await theFile.text();
           const info = await theFile.stat();
           // Writing creates the parent directories on the way.
           const wrote = await fs("docs").open("reports/2026-08.json").write({ rows: 12, ok: true });
           return {
             exists: exists, theString: theString,
             size: info.size, mime: info.mimeType, dated: typeof info.modified,
             wrote: wrote, stores: fs.stores,
             back: await fs("docs").open("reports/2026-08.json").json(),
           };"#,
    )
    .await?;

    assert_eq!(out["exists"], json!(true));
    assert_eq!(out["theString"], json!("hello"));
    assert_eq!(out["size"], json!(5));
    assert_eq!(out["mime"], json!("text/plain"));
    assert_eq!(out["dated"], json!("string"), "a real modification time");
    assert_eq!(out["wrote"], json!(21));
    assert_eq!(out["stores"], json!(["docs"]));
    assert_eq!(out["back"], json!({ "rows": 12, "ok": true }));
    // And the bytes are really there, under the name the body used.
    assert_eq!(
        on_disk(&dir, "reports/2026-08.json").unwrap(),
        r#"{"rows":12,"ok":true}"#
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn create_refuses_to_replace_and_write_replaces() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "create").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"const file = fs("docs").open("once.txt");
           await file.create("first");
           let refused = null;
           try { await file.create("second"); } catch (e) { refused = e.message; }
           await file.write("third");
           return { refused: refused, now: await file.text(),
                    gone: await file.delete(), again: await file.delete() };"#,
    )
    .await?;

    assert!(
        out["refused"].as_str().unwrap().contains("already exists"),
        "{out}"
    );
    assert_eq!(out["now"], json!("third"), "write replaces");
    assert_eq!(out["gone"], json!(true), "there was something to delete");
    assert_eq!(out["again"], json!(false), "and now there is not");
    assert!(on_disk(&dir, "once.txt").is_none());

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_directory_is_made_listed_walked_and_deleted() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "walk").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"const notes = fs("docs").dir("notes");
           await notes.create();
           await notes.file("a.txt").write("a");
           await notes.file("b.txt").write("bb");
           await notes.dir("old").file("c.txt").write("ccc");
           const listed = await notes.list();
           let total = 0;
           for (const entry of listed) {
             if (!entry.isDirectory) total += (await entry.text()).length;
           }
           const names = listed.map((e) => e.name + (e.isDirectory ? "/" : ""));
           return { names: names, total: total,
                    root: fs("docs").root.path,
                    removed: await notes.dir("old").delete(),
                    left: (await notes.list()).length };"#,
    )
    .await?;

    assert_eq!(out["names"], json!(["a.txt", "b.txt", "old/"]));
    assert_eq!(out["total"], json!(3));
    assert_eq!(out["root"], json!(""));
    assert_eq!(out["removed"], json!(true));
    assert_eq!(out["left"], json!(2));
    assert!(!dir.join("notes/old").exists(), "the directory is gone");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_copy_crosses_two_stores_without_carrying_the_bytes_through_the_body() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "copy").await?;
    let archive = temp_dir("copy-archive");
    catalog.connect_file_store(Arc::new(LocalFileStore::new("archive", &archive)?))?;
    std::fs::write(dir.join("report.txt"), "quarterly").unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"const file = fs("docs").open("report.txt");
           const copy = await file.copyTo(fs("archive").open("2026/report.txt"));
           // The other spelling of the same thing: writing a file *is* a copy.
           const bytes = await fs("archive").open("2026/again.txt").write(file);
           const moved = await file.moveTo("done/report.txt");
           return { copy: copy.store.name + ":" + copy.path, bytes: bytes,
                    moved: moved.path, gone: await file.exists() };"#,
    )
    .await?;

    assert_eq!(out["copy"], json!("archive:2026/report.txt"));
    assert_eq!(out["bytes"], json!(9));
    assert_eq!(out["moved"], json!("done/report.txt"));
    assert_eq!(out["gone"], json!(false));
    assert_eq!(on_disk(&archive, "2026/report.txt").unwrap(), "quarterly");
    assert_eq!(on_disk(&archive, "2026/again.txt").unwrap(), "quarterly");
    assert_eq!(on_disk(&dir, "done/report.txt").unwrap(), "quarterly");
    assert!(
        on_disk(&dir, "report.txt").is_none(),
        "a move is not a copy"
    );

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&archive).ok();
    Ok(())
}

#[tokio::test]
async fn metadata_a_body_sets_is_the_stores_own() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "meta").await?;
    std::fs::write(dir.join("secret.txt"), "s").unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"const file = fs("docs").open("secret.txt");
           await file.setMeta({ minRole: 10, attributes: { origin: "trigger" } });
           return await file.meta();"#,
    )
    .await?;

    assert_eq!(out["minRole"], json!(10));
    assert_eq!(out["effectiveMinRole"], json!(10));
    assert_eq!(out["attributes"]["origin"], json!("trigger"));
    // The same metadata the store itself reads: a body's `setMeta` is not a
    // second place rules are kept.
    let store = catalog.require_file_store("docs")?;
    let meta = store.get_meta("secret.txt").await?;
    assert_eq!(meta.min_role, Some(10));
    assert_eq!(
        meta.attributes.get("origin").map(String::as_str),
        Some("trigger")
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_delegated_body_obeys_the_folder_rule_and_the_trigger_does_not() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "rule").await?;
    std::fs::create_dir_all(dir.join("private")).unwrap();
    std::fs::write(dir.join("private/notes.txt"), "confidential").unwrap();
    std::fs::write(dir.join("open.txt"), "public").unwrap();
    // Admin-only, on the *folder* — the file below it carries no rule of its own,
    // which is the whole point of the rule being path-cumulative.
    let store = catalog.require_file_store("docs")?;
    store
        .set_meta(
            "private",
            &FileMeta {
                min_role: Some(1),
                ..Default::default()
            },
        )
        .await?;

    let code = r#"const said = {};
        try { said.trigger = await fs("docs").open("private/notes.txt").text(); }
        catch (e) { said.trigger = "refused: " + e.message; }
        try { said.user = await fs("docs").asUser().open("private/notes.txt").text(); }
        catch (e) { said.user = "refused"; }
        said.open = await fs("docs").asUser().open("open.txt").text();
        // A listing is filtered rather than refused: naming what the caller
        // cannot open would leak exactly what the rule hides.
        said.listed = (await fs("docs").asUser().root.list()).map((e) => e.name);
        said.all = (await fs("docs").root.list()).map((e) => e.name);
        return said;"#;

    // Fired by a user: the trigger's own authority reads the file, the delegated
    // handle does not.
    let out = run(&catalog, &fired_by_a_user(), code).await?;
    assert_eq!(out["trigger"], json!("confidential"));
    assert_eq!(out["user"], json!("refused"));
    assert_eq!(out["open"], json!("public"));
    assert_eq!(out["listed"], json!(["open.txt"]));
    assert_eq!(out["all"], json!(["open.txt", "private"]));

    // Fired by an admin: delegation is to *that* caller, who clears the rule.
    let out = run(&catalog, &fired(), code).await?;
    assert_eq!(out["user"], json!("confidential"));

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_delegated_body_obeys_the_stores_own_floor() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "floor").await?;
    std::fs::write(dir.join("a.txt"), "guarded").unwrap();
    // The floor lives in the store's *definition*, not beside the bytes: the
    // outermost entry on every path in the store (§1.1).
    let mut def = FileStoreDef::local("docs", dir.to_string_lossy());
    def.min_role = Some(1);
    save_file_store(&catalog, &def).await?;

    let out = run(
        &catalog,
        &fired_by_a_user(),
        r#"const said = {};
           said.trigger = await fs("docs").open("a.txt").text();
           try { said.user = await fs("docs").asUser().open("a.txt").text(); }
           catch (e) { said.user = "refused"; }
           // A delegated write is refused by the same floor.
           try { await fs("docs").asUser().open("b.txt").write("x"); said.wrote = true; }
           catch (e) { said.wrote = false; }
           return said;"#,
    )
    .await?;

    assert_eq!(out["trigger"], json!("guarded"));
    assert_eq!(out["user"], json!("refused"));
    assert_eq!(out["wrote"], json!(false));
    assert!(on_disk(&dir, "b.txt").is_none(), "and nothing was written");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// An event fired by user `id` at role 80 — two of these are two different
/// people at the same role, which is what the owner rule (F2 §2.1) tells apart.
fn fired_by(id: &str) -> Event {
    Event::new(EventKind::None).payload(json!({})).caller(
        80,
        Some(json!({ "id": id, "email": format!("{id}@example.com") })),
    )
}

#[tokio::test]
async fn a_delegated_write_is_the_callers_and_nobody_else_may_change_it() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "owner").await?;

    // A saves a private file under their own authority: it is recorded as
    // theirs, and its rule is tightened to admin-only as it is written.
    let out = run(
        &catalog,
        &fired_by("user-a"),
        r#"const file = fs("docs").asUser().open("drops/a.txt");
           await file.write("mine", { minRole: 1, filename: "Ä report.txt" });
           await fs("docs").asUser().dir("drops/a-dir").create();
           return { meta: await file.meta(), back: await file.text() };"#,
    )
    .await?;
    assert_eq!(out["meta"]["owner"], json!("user-a"));
    assert_eq!(out["meta"]["minRole"], json!(1));
    assert_eq!(out["meta"]["attributes"]["filename"], json!("Ä report.txt"));
    assert_eq!(
        out["back"],
        json!("mine"),
        "the owner reads it past the rule"
    );
    let store = catalog.require_file_store("docs")?;
    assert_eq!(
        store.get_meta("drops/a-dir").await?.owner.as_deref(),
        Some("user-a")
    );

    // B, at the same role, cannot read, overwrite, delete, move or re-own it,
    // and does not see it listed.
    let out = run(
        &catalog,
        &fired_by("user-b"),
        r#"const file = fs("docs").asUser().open("drops/a.txt");
           const said = {};
           const attempt = async (name, f) => {
             try { await f(); said[name] = "done"; } catch (e) { said[name] = "refused"; }
           };
           await attempt("read", () => file.text());
           await attempt("write", () => file.write("theirs"));
           await attempt("delete", () => file.delete());
           await attempt("move", () => file.moveTo("drops/b.txt"));
           await attempt("dirDelete", () => fs("docs").asUser().dir("drops").delete());
           await attempt("setMeta", () => file.setMeta({ minRole: 1, owner: "user-b" }));
           // B's own write names somebody else as the owner: refused, too.
           await attempt("chooseOwner", () =>
             fs("docs").asUser().open("b.txt").write("x", { owner: "user-a" }));
           said.listed = (await fs("docs").asUser().root.list()).map((e) => e.name);
           return said;"#,
    )
    .await?;
    for op in [
        "read",
        "write",
        "delete",
        "move",
        "dirDelete",
        "setMeta",
        "chooseOwner",
    ] {
        assert_eq!(out[op], json!("refused"), "{op}: {out}");
    }
    assert!(!out["listed"].as_array().unwrap().contains(&json!("b.txt")));
    assert_eq!(on_disk(&dir, "drops/a.txt").unwrap(), "mine");
    assert!(on_disk(&dir, "b.txt").is_none());

    // A lists the private folder's root and sees their own entries, though
    // the folder `drops` has no rule A could not clear — and A may change it.
    let out = run(
        &catalog,
        &fired_by("user-a"),
        r#"const file = fs("docs").asUser().open("drops/a.txt");
           await file.write("still mine");
           // The owner cannot be changed under user authority, even by its owner.
           let given = "kept";
           try { await file.setMeta({ minRole: 1, owner: "user-b" }); given = "given"; }
           catch (e) { given = "refused"; }
           return { given, meta: await file.meta() };"#,
    )
    .await?;
    assert_eq!(out["given"], json!("refused"));
    assert_eq!(out["meta"]["owner"], json!("user-a"));
    assert_eq!(on_disk(&dir, "drops/a.txt").unwrap(), "still mine");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn an_admin_write_records_no_owner_unless_it_names_one() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "admin-owner").await?;

    let out = run(
        &catalog,
        &fired_by("user-a"),
        r#"const plain = fs("docs").open("plain.txt");
           await plain.write("p");
           const given = fs("docs").open("given.txt");
           await given.write("g", { owner: "user-c", min_role: 1 });
           // A copy carries the options too.
           await fs("docs").open("copied.txt").write(given, { owner: "user-d" });
           let typo = "accepted";
           try { await plain.write("p", { onwer: "x" }); } catch (e) { typo = e.message; }
           // Admin authority may re-own, and clear, an owner.
           await plain.setMeta({ owner: "user-e" });
           const reowned = (await plain.meta()).owner;
           await plain.setMeta({ owner: null });
           return {
             plain: (await plain.meta()).owner, reowned,
             given: await given.meta(), copied: (await fs("docs").open("copied.txt").meta()).owner,
             typo,
           };"#,
    )
    .await?;
    assert_eq!(out["plain"], json!(null), "admin authority records nobody");
    assert_eq!(out["reowned"], json!("user-e"));
    assert_eq!(out["given"]["owner"], json!("user-c"));
    assert_eq!(out["given"]["minRole"], json!(1));
    assert_eq!(out["copied"], json!("user-d"));
    assert!(
        out["typo"].as_str().unwrap().contains("not a write option"),
        "{out}"
    );

    // And the owner named is an authority: user-c reads the admin-only file.
    let out = run(
        &catalog,
        &fired_by("user-c"),
        r#"return await fs("docs").asUser().open("given.txt").text();"#,
    )
    .await?;
    assert_eq!(out, json!("g"));

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_file_larger_than_one_read_is_refused_rather_than_truncated() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "cap").await?;
    // One byte past what a body may carry across the seam.
    let big = vec![b'x'; (sc_api::code_host::MAX_FILE_BYTES + 1) as usize];
    std::fs::write(dir.join("big.bin"), &big).unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"const file = fs("docs").open("big.bin");
           const info = await file.stat();
           try { await file.text(); return { size: info.size, said: null }; }
           catch (e) { return { size: info.size, said: e.message }; }"#,
    )
    .await?;

    assert_eq!(out["size"], json!(big.len()));
    let said = out["said"].as_str().unwrap_or_default();
    assert!(said.contains("8 MB"), "the cap is named: {said}");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_store_that_is_not_connected_is_named_when_it_is_asked_for() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "unknown").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"try { fs("uplods"); return "allowed"; } catch (e) { return e.message; }"#,
    )
    .await?;
    let said = out.as_str().unwrap();
    assert!(said.contains("no file store named `uplods`"), "{said}");
    assert!(said.contains("docs"), "and what there is instead: {said}");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
