//! The link and the repository (S4): `_fd_sync`, the mirror's rules, a commit
//! of a selection with its conflict resolutions, branches, linking from each
//! starting point, and a git store's pin on Commit — against local bare
//! repositories over `file://`.

use std::collections::BTreeSet;
use std::path::Path;

use sc_api::schema_edit::Operation;
use sc_error::Result;
use sc_files::{CFG_BRANCH, CFG_DIR, FileStoreDef};
use sc_sync::{
    Class, CommitRequest, Confirm, Credential, DEPLOYMENTS_TABLE, LedgerKind, LinkedStatus,
    OpClass, Outcome, SYNC_TABLE, Source, SourceControl, SyncLink, list_deployments, load_base,
};

use crate::fixture::{Installation, git, put, text_field};

/// An empty bare repository under `dir`, and its `file://` URL.
fn bare(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q", "--bare", "--initial-branch=main"]);
    format!("file://{}", path.display())
}

fn source(inst: &Installation) -> SourceControl<'_> {
    SourceControl::new(inst.deployer())
}

/// What Status shows, as `(key, class)`.
fn shown(status: &LinkedStatus) -> Vec<(String, Class)> {
    status
        .status
        .shown()
        .map(|e| (e.key.clone(), e.class))
        .collect()
}

fn class_of(status: &LinkedStatus, key: &str) -> Option<Class> {
    status.status.entry(key).map(|e| e.class)
}

fn commit(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.into(),
        actor: Some("ann@example.com".into()),
        source: Some(Source::Cli),
        ..CommitRequest::default()
    }
}

/// Deploy the tip with the plan's default ticks, plus `tick`.
async fn deploy(sc: &SourceControl<'_>, tick: &[&str]) -> Result<sc_sync::Receipt> {
    let planned = sc.plan_deploy(None, Source::Cli).await?;
    assert!(
        planned.plan.blockers.is_empty(),
        "{:#?}",
        planned.plan.blockers
    );
    let mut confirm = Confirm::defaults(&planned.plan);
    for id in tick {
        confirm = confirm.tick(id);
    }
    sc.deploy(planned.commit.as_deref(), &confirm, None, Source::Cli)
        .await
}

/// A clone of `url` in `dir/name`, as another person's working copy.
fn clone(dir: &Path, url: &str, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["clone", "-q", url, "."]);
    path
}

/// The files of the remote branch `branch` (`git ls-tree` in a fresh clone).
fn remote_paths(dir: &Path, url: &str, branch: &str) -> Vec<String> {
    let path = clone(dir, url, &format!("look-{}", uuid::Uuid::new_v4().simple()));
    git(&path, &["checkout", "-q", branch]);
    git(&path, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn the_bootstrap_is_idempotent_and_an_existing_database_gains_the_sync_tables() -> Result<()>
{
    // The fixture is a database from before source control: neither table.
    let inst = Installation::new().await?;
    assert!(inst.catalog.get(SYNC_TABLE)?.is_none());
    assert!(inst.catalog.get(DEPLOYMENTS_TABLE)?.is_none());

    sc_sync::bootstrap(&inst.catalog).await?;
    assert!(inst.catalog.get(SYNC_TABLE)?.is_some());
    assert!(inst.catalog.get(DEPLOYMENTS_TABLE)?.is_some());

    // Again, with a link stored: nothing is lost.
    let link = SyncLink::new("file:///nowhere.git", "main");
    sc_sync::link::save_link(&inst.catalog, &link).await?;
    sc_sync::bootstrap(&inst.catalog).await?;
    inst.catalog.reload().await?;
    sc_sync::bootstrap(&inst.catalog).await?;
    assert_eq!(sc_sync::load_link(&inst.catalog).await?, Some(link));
    Ok(())
}

#[tokio::test]
async fn an_https_token_is_stored_sealed_and_shown_nowhere() -> Result<()> {
    let inst = Installation::new().await?;
    let credential = Credential::https_token(&inst.catalog, Some("bot"), "tok-abc123")?;
    let mut link = SyncLink::new("https://git.example.com/acme/structure.git", "main");
    link.credential = credential;
    sc_sync::link::save_link(&inst.catalog, &link).await?;

    let stored = inst.text("SELECT credential_secret FROM _fd_sync").await;
    assert!(stored.starts_with("v1:"), "{stored}");
    assert!(!stored.contains("tok-abc123"));
    let loaded = sc_sync::load_link(&inst.catalog).await?.unwrap();
    assert!(matches!(
        &loaded.credential,
        Credential::HttpsToken { username, .. } if username == "bot"
    ));
    let shown = format!("{loaded:?} {}", serde_json::to_string(&loaded).unwrap());
    assert!(!shown.contains("tok-abc123"), "{shown}");
    assert!(!shown.contains(&stored), "{shown}");
    Ok(())
}

#[tokio::test]
async fn a_generated_key_is_the_links_credential_before_and_after_linking() -> Result<()> {
    let inst = Installation::new().await?;
    let sc = source(&inst);
    let key = sc.generate_key().await?;
    let Credential::DeployKey { public_key, .. } = &key else {
        panic!("{key:?}");
    };
    assert!(public_key.starts_with("ssh-ed25519 "));
    assert_eq!(sc.credential().await?, key);
    assert_eq!(sc.link().await?, None, "a key alone is no link");
    Ok(())
}

// --- linking (S4.6) -----------------------------------------------------------------

#[tokio::test]
async fn linking_an_empty_repository_makes_everything_local_and_commit_exports_it() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title", "status"]).await?;
    let url = bare(&inst.dir, "structure.git");
    let sc = source(&inst);

    let linked = sc.set_link(SyncLink::new(&url, "main")).await?;
    assert_eq!(linked.tip, None);
    assert!(linked.base_taken);
    let status = sc.status().await?;
    let shown = shown(&status);
    assert!(!shown.is_empty());
    assert!(
        shown.iter().all(|(_, c)| *c == Class::LocalNew),
        "{shown:#?}"
    );
    assert_eq!(class_of(&status, "table:tickets"), Some(Class::LocalNew));

    // The first commit exports the installation, byte for byte, as one
    // commit.
    let receipt = sc.commit(&commit("first export")).await?;
    let live = inst.tree().await?;
    let mut expected: Vec<String> = live.paths().map(str::to_owned).collect();
    expected.sort();
    assert_eq!(remote_paths(&inst.dir, &url, "main"), expected);
    let checkout = clone(&inst.dir, &url, "log");
    for (path, bytes) in live.to_files() {
        assert_eq!(
            std::fs::read(checkout.join(&path)).unwrap(),
            bytes,
            "{path}"
        );
    }
    let log = git(&checkout, &["log", "--format=%s|%ae"]);
    assert_eq!(log, "first export|ann@example.com");
    assert_eq!(receipt.branch, "main");

    // Then nothing is left to commit, and the ledger has the commit.
    assert!(shown_of(&sc).await?.is_empty());
    let err = sc.commit(&commit("again")).await.unwrap_err().to_string();
    assert!(err.contains("nothing to commit"), "{err}");
    let ledger = list_deployments(&inst.catalog, 10).await?;
    assert_eq!(ledger[0].kind, LedgerKind::Commit);
    assert_eq!(ledger[0].commit.as_deref(), Some(receipt.commit.as_str()));
    Ok(())
}

async fn shown_of(sc: &SourceControl<'_>) -> Result<Vec<(String, Class)>> {
    Ok(shown(&sc.status().await?))
}

#[tokio::test]
async fn linking_an_empty_instance_makes_everything_incoming_and_deploy_builds_it() -> Result<()> {
    let a = Installation::new().await?;
    a.table("tickets", &["title", "status"]).await?;
    let url = bare(&a.dir, "structure.git");
    let sa = source(&a);
    sa.set_link(SyncLink::new(&url, "main")).await?;
    sa.commit(&commit("from A")).await?;

    let b = Installation::new().await?;
    let sb = source(&b);
    let linked = sb.set_link(SyncLink::new(&url, "main")).await?;
    assert!(linked.tip.is_some());
    let status = sb.status().await?;
    let shown = shown(&status);
    assert!(!shown.is_empty());
    assert!(shown.iter().all(|(_, c)| c.is_incoming()), "{shown:#?}");
    assert_eq!(class_of(&status, "table:tickets"), Some(Class::IncomingNew));

    let receipt = deploy(&sb, &[]).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(b.catalog.get("tickets")?.is_some());
    // The next base is stored: B is in sync.
    assert!(
        shown_of(&sb).await?.is_empty(),
        "{:#?}",
        shown_of(&sb).await?
    );
    let base = load_base(&b.catalog).await?.unwrap();
    assert_eq!(base.commit, linked.tip);
    Ok(())
}

#[tokio::test]
async fn linking_where_both_have_content_keeps_what_is_identical_in_sync() -> Result<()> {
    let a = Installation::new().await?;
    a.table("shared", &["name"]).await?;
    a.table("from_a", &["x"]).await?;
    let url = bare(&a.dir, "structure.git");
    let sa = source(&a);
    sa.set_link(SyncLink::new(&url, "main")).await?;
    sa.commit(&commit("from A")).await?;

    // B built `shared` the same way, by itself, with ids of its own.
    let b = Installation::new().await?;
    b.table("shared", &["name"]).await?;
    b.table("from_b", &["y"]).await?;
    let sb = source(&b);
    sb.set_link(SyncLink::new(&url, "main")).await?;
    let status = sb.status().await?;
    assert_eq!(class_of(&status, "table:shared"), Some(Class::InSync));
    assert_eq!(class_of(&status, "table:from_a"), Some(Class::IncomingNew));
    assert_eq!(class_of(&status, "table:from_b"), Some(Class::LocalNew));
    Ok(())
}

// --- the mirror's rules (S4.3) ------------------------------------------------------

#[tokio::test]
async fn a_push_that_loses_a_race_fails_cleanly_and_a_retry_succeeds() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title"]).await?;
    let url = bare(&inst.dir, "structure.git");
    source(&inst).set_link(SyncLink::new(&url, "main")).await?;
    source(&inst).commit(&commit("first")).await?;
    inst.table("notes", &["body"]).await?;
    let base_before = load_base(&inst.catalog).await?;

    // Someone else pushes between this commit being written and pushed.
    let elsewhere = clone(&inst.dir, &url, "elsewhere");
    let racing = source(&inst).before_push(|| {
        put(&elsewhere, "README.md", b"# structure\n");
        git(&elsewhere, &["add", "-A"]);
        git(&elsewhere, &["commit", "-qm", "readme"]);
        git(&elsewhere, &["push", "-q", "origin", "HEAD:main"]);
    });
    let err = racing
        .commit(&commit("notes"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("moved on the remote"), "{err}");
    assert!(err.contains("commit again"), "{err}");
    drop(racing);

    // Nothing changed here but the mirror, which is at the remote's tip.
    assert_eq!(load_base(&inst.catalog).await?, base_before);
    let theirs = git(&elsewhere, &["rev-parse", "HEAD"]);
    let status = source(&inst).status().await?;
    assert_eq!(status.tip.as_deref(), Some(theirs.as_str()));
    assert_eq!(class_of(&status, "table:notes"), Some(Class::LocalNew));

    // Pressing Commit again puts it on top of theirs, keeping their file.
    let receipt = source(&inst).commit(&commit("notes")).await?;
    git(&elsewhere, &["pull", "-q", "origin", "main"]);
    assert_eq!(git(&elsewhere, &["rev-parse", "HEAD"]), receipt.commit);
    assert_eq!(git(&elsewhere, &["rev-parse", "HEAD^"]), theirs);
    assert!(elsewhere.join("README.md").exists());
    assert!(elsewhere.join("tables/notes.json").exists());
    assert!(shown_of(&source(&inst)).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_pull_after_a_force_push_resets_the_branch() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title"]).await?;
    let url = bare(&inst.dir, "structure.git");
    let sc = source(&inst);
    sc.set_link(SyncLink::new(&url, "main")).await?;
    let first = sc.commit(&commit("first")).await?.commit;

    let elsewhere = clone(&inst.dir, &url, "elsewhere");
    git(&elsewhere, &["checkout", "-q", "--orphan", "rewrite"]);
    git(&elsewhere, &["rm", "-rq", "--cached", "."]);
    put(&elsewhere, "README.md", b"# rewritten\n");
    git(&elsewhere, &["add", "README.md"]);
    git(&elsewhere, &["commit", "-qm", "rewritten"]);
    git(
        &elsewhere,
        &["push", "-q", "--force", "origin", "HEAD:main"],
    );
    let rewritten = git(&elsewhere, &["rev-parse", "HEAD"]);

    let pulled = sc.pull().await?;
    assert_eq!(pulled.before.as_deref(), Some(first.as_str()));
    assert_eq!(pulled.tip.as_deref(), Some(rewritten.as_str()));
    assert!(pulled.forced);
    // Live did not change; Status now sees the whole structure deleted
    // upstream.
    assert!(inst.catalog.get("tickets")?.is_some());
    let status = sc.status().await?;
    assert_eq!(
        class_of(&status, "table:tickets"),
        Some(Class::IncomingDelete)
    );
    Ok(())
}

#[tokio::test]
async fn a_subdirectory_link_leaves_the_rest_of_the_repository_alone() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title"]).await?;
    let url = bare(&inst.dir, "mono.git");
    let other = clone(&inst.dir, &url, "other");
    put(&other, "src/main.rs", b"fn main() {}\n");
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "code"]);
    git(&other, &["push", "-q", "origin", "HEAD:main"]);

    let sc = source(&inst);
    sc.set_link(SyncLink::new(&url, "main").in_subdirectory("structure/"))
        .await?;
    assert_eq!(
        class_of(&sc.status().await?, "table:tickets"),
        Some(Class::LocalNew)
    );
    sc.commit(&commit("structure")).await?;
    let paths = remote_paths(&inst.dir, &url, "main");
    assert!(paths.contains(&"src/main.rs".to_owned()), "{paths:?}");
    assert!(
        paths.contains(&"structure/tables/tickets.json".to_owned()),
        "{paths:?}"
    );
    assert!(!paths.contains(&"tables/tickets.json".to_owned()));
    Ok(())
}

// --- committing a selection, and conflicts (S4.4) -----------------------------------

#[tokio::test]
async fn a_commit_of_a_selection_commits_only_those() -> Result<()> {
    let inst = Installation::new().await?;
    let url = bare(&inst.dir, "structure.git");
    let sc = source(&inst);
    sc.set_link(SyncLink::new(&url, "main")).await?;
    sc.commit(&commit("empty installation")).await?;
    inst.table("tickets", &["title"]).await?;
    inst.table("notes", &["body"]).await?;

    let receipt = sc
        .commit(&CommitRequest {
            keys: Some(vec!["table:tickets".into()]),
            ..commit("tickets only")
        })
        .await?;
    assert_eq!(receipt.entities, ["table:tickets"]);
    let paths = remote_paths(&inst.dir, &url, "main");
    assert!(paths.contains(&"tables/tickets.json".to_owned()));
    assert!(!paths.contains(&"tables/notes.json".to_owned()));
    let status = sc.status().await?;
    assert_eq!(class_of(&status, "table:tickets"), Some(Class::InSync));
    assert_eq!(class_of(&status, "table:notes"), Some(Class::LocalNew));

    // What is not a local change cannot be chosen.
    let err = sc
        .commit(&CommitRequest {
            keys: Some(vec!["table:tickets".into()]),
            ..commit("again")
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no local change"), "{err}");
    Ok(())
}

/// Add the text field `field` to each of `tables`.
async fn add_field(inst: &Installation, tables: &[&str], field: &str) -> Result<()> {
    for table in tables {
        inst.schema(vec![Operation::AddField {
            table: (*table).into(),
            field: text_field(field),
        }])
        .await?;
    }
    Ok(())
}

fn field_names(tree: &sc_sync::StructureTree, table: &str) -> BTreeSet<String> {
    tree.json(&format!("tables/{table}.json")).unwrap()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_conflict_is_kept_discarded_or_merged_and_never_committed_unasked() -> Result<()> {
    // A and B share t1, t2 and t3.
    let a = Installation::new().await?;
    for t in ["t1", "t2", "t3"] {
        a.table(t, &["name"]).await?;
    }
    let url = bare(&a.dir, "structure.git");
    let sa = source(&a);
    sa.set_link(SyncLink::new(&url, "main")).await?;
    sa.commit(&commit("tables")).await?;
    let b = Installation::new().await?;
    let sb = source(&b);
    sb.set_link(SyncLink::new(&url, "main")).await?;
    deploy(&sb, &[]).await?;

    // B adds `x` to all three and commits; A adds `y` to all three.
    add_field(&b, &["t1", "t2", "t3"], "x").await?;
    sb.commit(&commit("x")).await?;
    add_field(&a, &["t1", "t2", "t3"], "y").await?;
    sa.pull().await?;
    let status = sa.status().await?;
    for t in ["t1", "t2", "t3"] {
        assert_eq!(
            class_of(&status, &format!("table:{t}")),
            Some(Class::Conflict)
        );
    }

    // Commit never takes a conflict nobody chose for.
    let err = sa.commit(&commit("all")).await.unwrap_err().to_string();
    assert!(err.contains("table:t1, table:t2, table:t3"), "{err}");
    assert!(err.contains("keep mine"), "{err}");

    // t1: keep mine commits A's version over B's.
    sa.commit(&CommitRequest {
        keys: Some(vec!["table:t1".into()]),
        keep_mine: BTreeSet::from(["table:t1".to_owned()]),
        ..commit("t1: keep mine")
    })
    .await?;

    // t2: discard mine returns it to the base, and it is incoming.
    let plan = sa.plan_discard(&["table:t2"]).await?;
    let mut confirm = Confirm::defaults(&plan);
    for op in plan
        .operations
        .iter()
        .filter(|op| op.class == OpClass::DataDestructive)
    {
        confirm = confirm.tick(&op.id);
    }
    sa.discard(&["table:t2"], &confirm, None, Source::Cli)
        .await?;

    // t3: merged (B's `x` taken in here too, as a save in the registry editor
    // would), then keep mine.
    add_field(&a, &["t3"], "x").await?;
    sa.commit(&CommitRequest {
        keys: Some(vec!["table:t3".into()]),
        keep_mine: BTreeSet::from(["table:t3".to_owned()]),
        ..commit("t3: merged")
    })
    .await?;

    let status = sa.status().await?;
    assert_eq!(class_of(&status, "table:t1"), Some(Class::InSync));
    assert_eq!(class_of(&status, "table:t2"), Some(Class::Incoming));
    assert_eq!(class_of(&status, "table:t3"), Some(Class::InSync));
    let mirror =
        sc_sync::Mirror::open(&a.catalog, &a.dir.join("data"), &sa.link().await?.unwrap())?;
    let tip = mirror.tree_at(status.tip.as_deref()).await?;
    let names = |t| field_names(&tip, t);
    assert!(names("t1").contains("y") && !names("t1").contains("x"));
    assert!(names("t2").contains("x") && !names("t2").contains("y"));
    assert!(names("t3").contains("x") && names("t3").contains("y"));
    let live = a.tree().await?;
    assert!(!field_names(&live, "t2").contains("y"), "discarded");
    Ok(())
}

// --- branches (S4.5) ----------------------------------------------------------------

#[tokio::test]
async fn switching_branch_changes_status_and_leaves_live_alone() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title"]).await?;
    let url = bare(&inst.dir, "structure.git");
    let sc = source(&inst);
    sc.set_link(SyncLink::new(&url, "main")).await?;
    let on_main = sc.commit(&commit("main")).await?.commit;

    // A branch made from the tip is pushed on creation.
    sc.create_branch("feature").await?;
    assert_eq!(sc.branches().await?, ["feature", "main"]);
    assert_eq!(sc.link().await?.unwrap().branch, "feature");
    let feature = clone(&inst.dir, &url, "feature");
    git(&feature, &["checkout", "-q", "feature"]);
    assert_eq!(git(&feature, &["rev-parse", "HEAD"]), on_main);
    let err = sc.create_branch("main").await.unwrap_err().to_string();
    assert!(err.contains("already has a branch `main`"), "{err}");

    inst.table("extra", &["x"]).await?;
    let receipt = sc.commit(&commit("extra")).await?;
    assert_eq!(receipt.branch, "feature");
    let live = inst.tree().await?;

    sc.switch_branch("main").await?;
    let status = sc.status().await?;
    assert_eq!(status.tip.as_deref(), Some(on_main.as_str()));
    assert_eq!(
        class_of(&status, "table:extra"),
        Some(Class::IncomingDelete)
    );
    assert_eq!(class_of(&status, "table:tickets"), Some(Class::InSync));
    assert_eq!(inst.tree().await?, live, "a switch does not touch live");

    sc.switch_branch("feature").await?;
    assert!(shown_of(&sc).await?.is_empty());
    let err = sc.switch_branch("nope").await.unwrap_err().to_string();
    assert!(err.contains("no branch `nope`"), "{err}");
    Ok(())
}

// --- a git store's pin on Commit (S4.7) ---------------------------------------------

#[tokio::test]
async fn a_pin_its_remote_lacks_is_refused_by_name_and_uncommitted_work_is_warned() -> Result<()> {
    let inst = Installation::new().await?;
    let remote = inst.dir.join("web.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let checkout = inst.dir.join("web");
    std::fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q"]);
    put(&checkout, "index.html", b"<h1>v1</h1>");
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-q", "-m", "v1"]);
    git(
        &checkout,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&checkout, &["push", "-q", "-u", "origin", "main"]);
    let def = FileStoreDef::git("web", remote.to_string_lossy())
        .with(CFG_DIR, checkout.to_string_lossy())
        .with(CFG_BRANCH, "main");
    sc_catalog::save_file_store(&inst.catalog, &def).await?;

    let url = bare(&inst.dir, "structure.git");
    let sc = source(&inst);
    sc.set_link(SyncLink::new(&url, "main")).await?;
    let first = sc.commit(&commit("with web at v1")).await?;
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);

    // A commit in the store that its remote does not have.
    put(&checkout, "index.html", b"<h1>v2</h1>");
    git(&checkout, &["commit", "-q", "-am", "v2"]);
    let status = sc.status().await?;
    assert_eq!(class_of(&status, "file_store:web"), Some(Class::PinMoved));
    let base_before = load_base(&inst.catalog).await?;
    let err = sc
        .commit(&commit("web at v2"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("the git store `web`"), "{err}");
    assert!(err.contains("its remote does not have"), "{err}");
    assert_eq!(load_base(&inst.catalog).await?, base_before);

    // Pushed, it commits; uncommitted work in the store is a warning.
    git(&checkout, &["push", "-q"]);
    put(&checkout, "draft.html", b"wip");
    let receipt = sc.commit(&commit("web at v2")).await?;
    assert_eq!(receipt.warnings.len(), 1, "{:?}", receipt.warnings);
    assert!(
        receipt.warnings[0].contains("`web` has 1 uncommitted change"),
        "{:?}",
        receipt.warnings
    );
    Ok(())
}
