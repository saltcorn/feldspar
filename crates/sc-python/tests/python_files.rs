//! `fs` from a **Python** body, against real directories and the real action
//! (phase 3.2 and 3.5).
//!
//! `sc-action-builtins/tests/code_files.rs`, question for question, in the other
//! language — and pinned to bytes on a disk for the same reason: what can be
//! wrong is what reached the filesystem and what was made of what came back, and
//! a mocked store would assert those about a file that never existed. The whole
//! path is here: the Python `fs`, the operation it builds, `sc-api`'s
//! `FileStoreHost`, `sc-files`'s `LocalFileStore`, and the file that is really
//! there afterwards.
//!
//! The **spelling** is Python's — `read_text()`, `write()`, `iterdir()`,
//! `set_meta(min_role=…)`, and dicts whose keys are `is_directory` rather than
//! `isDirectory`. What crosses the seam underneath is the same operation the
//! JavaScript body builds, which is why the rules below are not re-decided here:
//! the store floor, the path-cumulative folder rule and the read cap are the
//! host's, and they answer a Python body exactly as they answer a JavaScript
//! one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_action::{ActionContext, Event, EventKind};
use sc_action_builtins::builtin_actions;
use sc_catalog::{Catalog, bootstrap_file_stores, save_file_store};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{CodeAdapter, PYTHON};
use sc_files::{FileMeta, FileStoreDef, LocalFileStore};
use sc_python::PythonRuntime;
use sc_test_harness::TestDb;
use sc_types::Attrs;

use serde_json::{Value as Json, json};

/// A fresh directory that exists, and its path.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sc-python-files-{tag}-{}-{:?}",
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
        .caller(Role::ADMIN, Some(json!({ "email": "admin@example.com" })))
}

/// The same, fired by an ordinary user — what `as_user()` delegates to.
fn fired_by_a_user() -> Event {
    Event::new(EventKind::None)
        .payload(json!({}))
        .caller(Role::of(80), Some(json!({ "email": "ada@example.com" })))
}

fn adapters() -> BTreeMap<String, Arc<dyn CodeAdapter>> {
    let runtime: Arc<dyn CodeAdapter> = Arc::new(PythonRuntime::new());
    [(PYTHON.to_owned(), runtime)].into_iter().collect()
}

/// Run `run_python_code` through the registry — the path a firing trigger takes.
async fn run(catalog: &Catalog, event: &Event, code: &str) -> Result<Json> {
    let adapters = adapters();
    let registry = builtin_actions()?;
    let action = registry.require("run_python_code")?.clone();
    let cfg: Attrs = [("code".to_owned(), json!(code))].into_iter().collect();
    let mut ctx = ActionContext::new(catalog, event, &cfg, "files").with_adapters(&adapters);
    action.run(&mut ctx).await
}

/// What is on disk at `path`, relative to the store root.
fn on_disk(dir: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(path)).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_reads_and_writes_a_connected_store() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "read-write").await?;
    std::fs::write(dir.join("the_file.txt"), "hello").unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"
the_file = fs("docs").open("the_file.txt")
info = the_file.stat()
# Writing creates the parent directories on the way.
wrote = fs("docs").open("reports/2026-08.json").write({"rows": 12, "ok": True})
return {
    "exists": the_file.exists(),
    "the_string": the_file.read_text(),
    "size": info["size"],
    "mime": info["mime_type"],
    "dated": type(info["modified"]).__name__,
    "wrote": wrote,
    "stores": list(fs.stores),
    "back": fs("docs").open("reports/2026-08.json").read_json(),
    "name": the_file.name,
    "parent_is_root": the_file.parent.path == "",
}
"#,
    )
    .await?;

    assert_eq!(out["exists"], json!(true));
    assert_eq!(out["the_string"], json!("hello"));
    assert_eq!(out["size"], json!(5));
    assert_eq!(out["mime"], json!("text/plain"));
    assert_eq!(out["dated"], json!("str"), "a real modification time");
    assert_eq!(out["wrote"], json!(24));
    assert_eq!(out["stores"], json!(["docs"]));
    assert_eq!(out["back"], json!({ "rows": 12, "ok": true }));
    assert_eq!(out["name"], json!("the_file.txt"));
    assert_eq!(out["parent_is_root"], json!(true));
    // And the bytes are really there, under the name the body used — the same
    // 21 bytes a JavaScript body's `JSON.stringify` would have written, because
    // the seam carries text and both languages compacted the same object.
    assert_eq!(
        on_disk(&dir, "reports/2026-08.json").unwrap(),
        r#"{"rows": 12, "ok": true}"#
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_refuses_to_replace_and_write_replaces() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "create").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"
import saltcorn as sc
file = fs("docs").open("once.txt")
file.create("first")
refused = None
try:
    file.create("second")
except sc.FileError as e:
    refused = str(e)
file.write("third")
return {"refused": refused, "now": file.read_text(),
        "gone": file.delete(), "again": file.delete()}
"#,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_is_made_listed_walked_and_deleted() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "walk").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"
notes = fs("docs").dir("notes")
notes.create()
notes.file("a.txt").write("a")
notes.file("b.txt").write("bb")
notes.dir("old").file("c.txt").write("ccc")
total = 0
names = []
# Iterating the directory is iterating its listing, which is one host call.
for entry in notes:
    names.append(entry.name + ("/" if entry.is_directory else ""))
    if not entry.is_directory:
        total += len(entry.read_text())
return {"names": names, "total": total,
        "root": fs("docs").root.path,
        "removed": notes.dir("old").delete(),
        "left": len(notes.list())}
"#,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_crosses_two_stores_without_carrying_the_bytes_through_the_body() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "copy").await?;
    let archive = temp_dir("copy-archive");
    catalog.connect_file_store(Arc::new(LocalFileStore::new("archive", &archive)?))?;
    std::fs::write(dir.join("report.txt"), "quarterly").unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"
file = fs("docs").open("report.txt")
copy = file.copy_to(fs("archive").open("2026/report.txt"))
# The other spelling of the same thing: writing a file *is* a copy, and the
# bytes never enter the interpreter either way.
written = fs("archive").open("2026/again.txt").write(file)
moved = file.move_to("done/report.txt")
return {"copy": str(copy), "written": written,
        "moved": moved.path, "gone": file.exists()}
"#,
    )
    .await?;

    assert_eq!(out["copy"], json!("archive:2026/report.txt"));
    assert_eq!(out["written"], json!(9));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_written_from_a_body_are_the_bytes_on_disk() -> Result<()> {
    // The seam is JSON, so anything that is not text travels base64 — in both
    // directions, and this is the round trip: `bytes` in, the same `bytes` out,
    // with a real file in the middle that is not valid UTF-8.
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "bytes").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"
file = fs("docs").open("logo.png")
wrote = file.write(bytes([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))
read = file.read_bytes()
import saltcorn as sc
refused = None
try:
    file.read_text()
except sc.FileError as e:
    refused = str(e)
return {"wrote": wrote, "read": list(read), "refused": refused}
"#,
    )
    .await?;

    assert_eq!(out["wrote"], json!(8));
    assert_eq!(
        out["read"],
        json!([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
    );
    // Reading bytes as text is refused rather than mangled, and the message
    // names the way to read them.
    let said = out["refused"].as_str().unwrap_or_default();
    assert!(said.contains("not valid UTF-8"), "{said}");
    assert!(said.contains("bytes()"), "{said}");
    assert_eq!(
        std::fs::read(dir.join("logo.png")).unwrap(),
        vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_a_body_sets_is_the_stores_own() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "meta").await?;
    std::fs::write(dir.join("secret.txt"), "s").unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"
file = fs("docs").open("secret.txt")
file.set_meta(min_role=10, attributes={"origin": "trigger"})
return file.meta()
"#,
    )
    .await?;

    // Snake case, which is this surface's spelling of the same three facts.
    assert_eq!(out["min_role"], json!(10));
    assert_eq!(out["effective_min_role"], json!(10));
    assert_eq!(out["attributes"]["origin"], json!("trigger"));
    // The same metadata the store itself reads: a body's `set_meta` is not a
    // second place rules are kept.
    let store = catalog.require_file_store("docs", sc_catalog::Unchecked::system("test"))?;
    let meta = store
        .get_meta(&sc_files::StorePath::from_static("secret.txt"))
        .await?;
    assert_eq!(meta.min_role, Some(sc_types::Role::of(10)));
    assert_eq!(
        meta.attributes.get("origin").map(String::as_str),
        Some("trigger")
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegated_body_obeys_the_folder_rule_and_the_trigger_does_not() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "rule").await?;
    std::fs::create_dir_all(dir.join("private")).unwrap();
    std::fs::write(dir.join("private/notes.txt"), "confidential").unwrap();
    std::fs::write(dir.join("open.txt"), "public").unwrap();
    // Admin-only, on the *folder* — the file below it carries no rule of its own,
    // which is the whole point of the rule being path-cumulative.
    let store = catalog.require_file_store("docs", sc_catalog::Unchecked::system("test"))?;
    store
        .set_meta(
            &sc_files::StorePath::from_static("private"),
            &FileMeta {
                min_role: Some(Role::ADMIN),
                ..Default::default()
            },
        )
        .await?;

    let code = r#"
import saltcorn as sc
said = {}
try:
    said["trigger"] = fs("docs").open("private/notes.txt").read_text()
except sc.FileError as e:
    said["trigger"] = "refused: " + str(e)
try:
    said["user"] = fs("docs").as_user().open("private/notes.txt").read_text()
except sc.FileError:
    said["user"] = "refused"
said["open"] = fs("docs").as_user().open("open.txt").read_text()
# A listing is filtered rather than refused: naming an entry the caller cannot
# open would leak exactly what the rule hides.
said["listed"] = [e.name for e in fs("docs").as_user().root.list()]
said["all"] = [e.name for e in fs("docs").root.list()]
return said
"#;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegated_body_obeys_the_stores_own_floor() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "floor").await?;
    std::fs::write(dir.join("a.txt"), "guarded").unwrap();
    // The floor lives in the store's *definition*, not beside the bytes: the
    // outermost entry on every path in the store (§1.1).
    let mut def = FileStoreDef::local("docs", dir.to_string_lossy());
    def.min_role = Some(Role::ADMIN);
    save_file_store(&catalog, &def).await?;

    let out = run(
        &catalog,
        &fired_by_a_user(),
        r#"
import saltcorn as sc
said = {"trigger": fs("docs").open("a.txt").read_text()}
try:
    said["user"] = fs("docs").as_user().open("a.txt").read_text()
except sc.FileError:
    said["user"] = "refused"
# A delegated write is refused by the same floor.
try:
    fs("docs").as_user().open("b.txt").write("x")
    said["wrote"] = True
except sc.FileError:
    said["wrote"] = False
return said
"#,
    )
    .await?;

    assert_eq!(out["trigger"], json!("guarded"));
    assert_eq!(out["user"], json!("refused"));
    assert_eq!(out["wrote"], json!(false));
    assert!(on_disk(&dir, "b.txt").is_none(), "and nothing was written");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_larger_than_one_read_is_refused_rather_than_truncated() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "cap").await?;
    // One byte past what a body may carry across the seam.
    let big = vec![b'x'; (sc_api::code_host::MAX_FILE_BYTES + 1) as usize];
    std::fs::write(dir.join("big.bin"), &big).unwrap();

    let out = run(
        &catalog,
        &fired(),
        r#"
import saltcorn as sc
file = fs("docs").open("big.bin")
info = file.stat()
try:
    file.read_text()
    return {"size": info["size"], "said": None}
except sc.FileError as e:
    return {"size": info["size"], "said": str(e)}
"#,
    )
    .await?;

    assert_eq!(out["size"], json!(big.len()));
    let said = out["said"].as_str().unwrap_or_default();
    assert!(said.contains("8 MB"), "the cap is named: {said}");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_is_not_connected_is_named_when_it_is_asked_for() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "unknown").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"
import saltcorn as sc
try:
    fs("uplods")
    return "allowed"
except sc.FileError as e:
    return str(e)
"#,
    )
    .await?;
    let said = out.as_str().unwrap();
    assert!(said.contains("no file store named `uplods`"), "{said}");
    assert!(said.contains("docs"), "and what there is instead: {said}");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_that_leaves_the_store_is_refused_where_it_is_written() -> Result<()> {
    // Refused in the guest, where the message can name what the body wrote, and
    // refused again by the host, which trusts nothing it is sent. This asserts
    // the first: a `TypeError`, because it is a mistake in the body rather than
    // a refusal a body might catch and fall back from.
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "traversal").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"
said = {}
for path in ["../etc/passwd", "/etc/passwd", "a/../../b"]:
    try:
        fs("docs").open(path)
        said[path] = "allowed"
    except TypeError as e:
        said[path] = str(e)
return said
"#,
    )
    .await?;
    assert!(
        out["../etc/passwd"]
            .as_str()
            .unwrap()
            .contains("leaves the file store"),
        "{out}"
    );
    assert!(
        out["/etc/passwd"]
            .as_str()
            .unwrap()
            .contains("is an absolute path"),
        "{out}"
    );
    assert!(
        out["a/../../b"]
            .as_str()
            .unwrap()
            .contains("leaves the file store"),
        "{out}"
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_file_budget_is_the_javascript_ones_and_names_what_it_is_for() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, dir) = setup(&db, "budget").await?;

    let out = run(
        &catalog,
        &fired(),
        r#"
import saltcorn as sc
done = 0
try:
    while True:
        fs("docs").open("one.txt").exists()
        done += 1
except sc.FileError as e:
    return {"done": done, "said": str(e)}
return {"done": done, "said": None}
"#,
    )
    .await?;
    assert_eq!(
        out["done"].as_u64(),
        Some(u64::from(sc_expr::DEFAULT_MAX_FILE_OPS)),
        "{out}"
    );
    let said = out["said"].as_str().unwrap_or_default();
    assert!(said.contains("file operations in one run"), "{said}");
    assert!(
        said.contains("walk over a directory cannot run away"),
        "the bound says what it is for: {said}"
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_with_no_file_store_has_no_name_for_it() -> Result<()> {
    let error = PythonRuntime::new()
        .run(sc_expr::CodeCall {
            code: "return fs(\"docs\").root.path".to_owned(),
            ..sc_expr::CodeCall::default()
        })
        .await
        .expect_err("a pure body cannot reach a file store");
    let said = error.to_string();
    assert!(said.contains("NameError"), "{said}");
    assert!(said.contains("fs"), "{said}");

    let error = PythonRuntime::new()
        .run(sc_expr::CodeCall {
            code: "import saltcorn\nreturn saltcorn.fs(\"docs\").root.stat()".to_owned(),
            ..sc_expr::CodeCall::default()
        })
        .await
        .expect_err("still no file store");
    assert!(
        error.to_string().contains("cannot reach a file store"),
        "{error}"
    );
    Ok(())
}
