//! The executor (S3): a plan applied through each kind's own save path, the
//! renames, deferred operations and the next base, the safety backup, the
//! lock and the stale-plan check, the convergence re-diff, Discard, the
//! destructive policy, and file stores.
//!
//! Two installations in one process stand in for instances A and B: A is
//! built by hand and exported, B is deployed to.

use std::collections::BTreeSet;

use serde_json::json;

use sc_error::Result;
use sc_files::{CFG_BRANCH, CFG_DIR, CFG_PUBLIC_URL, FileStoreDef};
use sc_sync::{
    Class, Confirm, Destructive, Entities, OpClass, Outcome, Policy, Source, Status, StructureTree,
    list_deployments,
};

use crate::fixture::{Installation, code_app, entity, git, put, request};

/// The helpdesk A exports: a table, an application with a list view.
async fn helpdesk(a: &Installation) -> Result<sc_app::Application> {
    a.table("tickets", &["title", "status"]).await?;
    a.table("old_notes", &["body"]).await?;
    a.local_store("code", |d| d).await?;
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = a.application(body).await?;
    let mut view = sc_viewpattern::View::new(app.id, "list", "List", "tickets");
    view.configuration = json!({ "columns": [{ "type": "Field", "field_name": "status" }] })
        .as_object()
        .cloned()
        .unwrap();
    sc_viewpattern::save_view(&a.catalog, &view).await?;
    Ok(app)
}

/// `tree` without the file at `path`.
fn without(tree: &StructureTree, path: &str) -> StructureTree {
    let mut out = tree.clone();
    assert!(out.remove(path).is_some(), "no {path} to take out");
    out
}

#[tokio::test]
async fn an_exported_tree_deployed_to_an_empty_instance_exports_the_same() -> Result<()> {
    // §3.1's acceptance test, over the export test's installation: every kind
    // it has, a secret of each sort, a store's synced files.
    let a = Installation::new().await?;
    crate::export::helpdesk(&a).await?;
    let tree = a.tree().await?;

    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &tree, |c| c).await?;
    assert!(receipt.applied.failed.is_none(), "{receipt:#?}");
    assert!(receipt.applied.deferred.is_empty(), "{receipt:#?}");
    assert_eq!(receipt.result, Outcome::Converged, "{:#?}", receipt.drift);
    let there = b.tree().await?;

    // Byte for byte — except the entities that were created without the
    // secret this instance has not been given yet (§3.3), which are the same
    // entity with the secret unset.
    let secretive: BTreeSet<String> = receipt.needs_secrets.iter().cloned().collect();
    assert!(
        secretive.contains("db_connection:reporting") && secretive.contains("trigger:notify"),
        "{secretive:?}"
    );
    let (here_entities, there_entities) = (Entities::of(&tree), Entities::of(&there));
    let secret_paths: BTreeSet<&str> = here_entities
        .list
        .iter()
        .filter(|e| secretive.contains(&e.key()))
        .map(|e| e.path.as_str())
        .collect();
    // An LLM provider cannot be saved without its key, nor an MCP server
    // without its header's value, so they are not created yet; everything
    // else is.
    let skipped: Vec<&str> = receipt
        .applied
        .needs_secret
        .iter()
        .map(|f| f.op.as_str())
        .collect();
    assert_eq!(
        skipped,
        ["create:llm_provider:claude", "create:mcp_server:tools"]
    );
    let (mut here_files, there_files) = (tree.to_files(), there.to_files());
    here_files.remove("llm-providers/claude.json");
    here_files.remove("mcp-servers/tools.json");
    assert_eq!(
        here_files.keys().collect::<Vec<_>>(),
        there_files.keys().collect::<Vec<_>>()
    );
    for (path, bytes) in &here_files {
        if secret_paths.contains(path.as_str()) {
            let here = here_entities.list.iter().find(|e| &e.path == path).unwrap();
            let there = there_entities
                .list
                .iter()
                .find(|e| &e.path == path)
                .unwrap();
            assert!(here.same_as(there), "{path}:\n{here:#?}\n{there:#?}");
        } else {
            assert_eq!(
                String::from_utf8_lossy(bytes),
                String::from_utf8_lossy(&there_files[path]),
                "{path}"
            );
        }
    }
    // And no secret value travelled.
    assert!(!crate::fixture::all_text(&there).contains("SECRET"));

    // B is now in sync with what it deployed: nothing local, and nothing
    // incoming but the two waiting for their secrets.
    let status = Status::of(&receipt.base, &tree, &there);
    let shown: Vec<_> = status.shown().map(|e| (e.key.as_str(), e.class)).collect();
    assert_eq!(
        shown,
        [
            ("llm_provider:claude", Class::IncomingNew),
            ("mcp_server:tools", Class::IncomingNew),
        ]
    );

    // The ledger has it.
    let ledger = list_deployments(&b.catalog, 10).await?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].id, receipt.id);
    assert_eq!(ledger[0].result, Outcome::Converged);
    Ok(())
}

#[tokio::test]
async fn a_view_on_a_renamed_table_and_field_still_works_after_the_deploy() -> Result<()> {
    let a = Installation::new().await?;
    let app = helpdesk(&a).await?;
    let first = a.tree().await?;
    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    b.sql("INSERT INTO tickets (id, title, status) VALUES (1, 'printer', 'open')")
        .await?;

    // On A: the table and the field are renamed, and the view follows them.
    a.schema(vec![sc_api::schema_edit::Operation::RenameTable {
        table: "tickets".into(),
        to: "issues".into(),
    }])
    .await?;
    a.schema(vec![sc_api::schema_edit::Operation::AlterField {
        table: "issues".into(),
        field: "status".into(),
        settings: sc_api::schema_edit::FieldSettings {
            name: Some("state".into()),
            ..Default::default()
        },
    }])
    .await?;
    let mut app_body = entity(&a.tree().await?, "applications/helpdesk/application.json").clone();
    app_body["tables"] = json!(["issues"]);
    let app = sc_backup::codec::application_from_body(app.id, &app_body)?;
    sc_app::save_application(&a.catalog, &app).await?;
    let mut view = sc_viewpattern::load_view(&a.catalog, app.id, "list")
        .await?
        .unwrap();
    view.table_name = Some("issues".into());
    view.configuration = json!({ "columns": [{ "type": "Field", "field_name": "state" }] })
        .as_object()
        .cloned()
        .unwrap();
    sc_viewpattern::save_view(&a.catalog, &view).await?;
    let second = a.tree().await?;

    let plan = b.plan_deploy(&receipt.base, &second).await?;
    let actions: Vec<&str> = plan.operations.iter().map(|op| op.action).collect();
    assert!(actions.contains(&"rename_table"), "{actions:?}");
    assert!(actions.contains(&"rename_field"), "{actions:?}");
    assert!(
        !actions
            .iter()
            .any(|a| a.starts_with("drop") || *a == "create_table"),
        "{actions:?}"
    );
    let receipt = b.deploy(&receipt.base, &second, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");

    // The row is still there, under the new names, and the view names them.
    assert_eq!(
        b.text("SELECT state FROM issues WHERE id = 1").await,
        "open"
    );
    let view = sc_viewpattern::load_view(&b.catalog, app.id, "list")
        .await?
        .unwrap();
    assert_eq!(view.table_name.as_deref(), Some("issues"));
    assert_eq!(view.configuration["columns"][0]["field_name"], "state");
    sc_viewpattern::validate_view(&b.catalog, &view).await?;
    assert_eq!(b.tree().await?.to_files(), second.to_files());
    Ok(())
}

#[tokio::test]
async fn a_deferred_drop_stays_incoming_and_is_not_a_local_change() -> Result<()> {
    let a = Installation::new().await?;
    helpdesk(&a).await?;
    let first = a.tree().await?;
    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;
    b.sql("INSERT INTO old_notes (id, body) VALUES (1, 'keep me')")
        .await?;

    let second = without(&first, "tables/old_notes.json");
    let receipt = b.deploy(&receipt.base, &second, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{:#?}", receipt.drift);
    assert_eq!(receipt.applied.deferred, ["drop_table:table:old_notes"]);
    assert!(receipt.backups.is_empty(), "nothing destructive was ticked");
    assert_eq!(
        b.text("SELECT body FROM old_notes WHERE id = 1").await,
        "keep me"
    );

    // Still offered: incoming delete, which is not local and blocks nothing,
    // so a Commit leaves it alone.
    let live = b.tree().await?;
    let status = Status::of(&receipt.base, &second, &live);
    let entry = status.entry("table:old_notes").unwrap();
    assert_eq!(entry.class, Class::IncomingDelete);
    assert!(!entry.class.is_local());
    assert!(status.deploy_blockers().is_empty());
    let plan = b.plan_deploy(&receipt.base, &second).await?;
    assert!(plan.blockers.is_empty(), "{:#?}", plan.blockers);
    let op = plan.op("drop_table:table:old_notes").unwrap();
    assert_eq!((op.class, op.ticked), (OpClass::DataDestructive, false));
    Ok(())
}

#[tokio::test]
async fn a_ticked_drop_is_backed_up_first_and_the_backup_restores_it() -> Result<()> {
    let a = Installation::new().await?;
    helpdesk(&a).await?;
    let first = a.tree().await?;
    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;
    b.sql("INSERT INTO old_notes (id, body) VALUES (1, 'one'), (2, 'two')")
        .await?;
    b.sql("INSERT INTO tickets (id, title, status) VALUES (1, 'not dropped', 'open')")
        .await?;

    let second = without(&first, "tables/old_notes.json");
    let receipt = b
        .deploy(&receipt.base, &second, |c| {
            c.tick("drop_table:table:old_notes")
        })
        .await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(b.catalog.get("old_notes")?.is_none());
    assert_eq!(receipt.backups.len(), 1, "{:?}", receipt.backups);
    let ledger = list_deployments(&b.catalog, 1).await?;
    assert_eq!(ledger[0].backups, receipt.backups);

    // The backup holds the structure and the dropped table's rows, not the
    // rest of the data.
    let path = std::path::Path::new(&receipt.backups[0]);
    let (available, _) = sc_backup::inspect(path, None)?;
    let rows = |name: &str| {
        available
            .tables
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.count)
    };
    assert_eq!(rows("old_notes"), Some(2));
    assert_eq!(rows("tickets"), None);

    // Restoring it brings the rows back.
    let selection = sc_backup::Selection::everything(&available);
    sc_backup::restore_backup(
        &b.catalog,
        &b.host,
        path,
        None,
        &selection,
        sc_catalog::Editor::Operator(sc_catalog::Operator::at_cli("a test restore")),
    )
    .await?;
    assert_eq!(
        b.text("SELECT body FROM old_notes WHERE id = 2").await,
        "two"
    );
    Ok(())
}

#[tokio::test]
async fn a_plan_confirmed_after_a_live_edit_is_stale() -> Result<()> {
    let a = Installation::new().await?;
    let app = helpdesk(&a).await?;
    let first = a.tree().await?;
    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;

    // A adds a table; B reviews the plan, then someone edits the list view.
    a.table("contacts", &["name"]).await?;
    let second = a.tree().await?;
    let request = request(&receipt.base, &second);
    let plan = b.deployer().plan_deploy(&request).await?;
    assert!(plan.op("create_table:table:contacts").is_some());
    let mut view = sc_viewpattern::load_view(&b.catalog, app.id, "list")
        .await?
        .unwrap();
    view.description = "edited during the review".into();
    sc_viewpattern::save_view(&b.catalog, &view).await?;

    let refused = b
        .deployer()
        .deploy(&request, &Confirm::defaults(&plan))
        .await
        .unwrap_err();
    assert!(refused.causes().contains("stale"), "{}", refused.causes());
    assert!(b.catalog.get("contacts")?.is_none(), "nothing was applied");

    // The lock was given back: the new plan says why it cannot deploy now.
    let plan = b.deployer().plan_deploy(&request).await?;
    assert_eq!(plan.blockers.len(), 1, "{:#?}", plan.blockers);
    assert!(
        plan.blockers[0].message.contains("view:helpdesk/list"),
        "{}",
        plan.blockers[0].message
    );
    let refused = b
        .deployer()
        .deploy(&request, &Confirm::defaults(&plan))
        .await
        .unwrap_err();
    assert!(
        refused.causes().contains("uncommitted change"),
        "{}",
        refused.causes()
    );
    Ok(())
}

#[tokio::test]
async fn the_lock_refuses_a_second_deploy_while_one_runs() -> Result<()> {
    let b = Installation::new().await?;
    let tree = b.tree().await?;
    let held = sc_sync::take_lock(&b.catalog, "deploy").await?;
    let refused = b.deploy(&tree, &tree, |c| c).await.unwrap_err();
    assert!(
        refused.causes().contains("another deploy has been running"),
        "{}",
        refused.causes()
    );
    held.release(&b.catalog).await?;
    b.deploy(&tree, &tree, |c| c).await?;
    Ok(())
}

#[tokio::test]
async fn a_divergence_after_the_deploy_is_recorded_as_drifted() -> Result<()> {
    let a = Installation::new().await?;
    let app = helpdesk(&a).await?;
    let tree = a.tree().await?;
    let b = Installation::new().await?;
    // Something changes the view between the apply and the re-diff.
    *b.host.diverge.lock().unwrap() = Some((app.id, "list".into()));
    let receipt = b.deploy(&StructureTree::new(), &tree, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Drifted);
    assert_eq!(receipt.drift, ["update the view `helpdesk/list`"]);
    let ledger = list_deployments(&b.catalog, 1).await?;
    assert_eq!(ledger[0].result, Outcome::Drifted);
    assert_eq!(
        ledger[0].summary["drift"],
        json!(["update the view `helpdesk/list`"])
    );
    Ok(())
}

#[tokio::test]
async fn discard_restores_a_view_and_dropping_a_new_table_is_destructive() -> Result<()> {
    let a = Installation::new().await?;
    let app = helpdesk(&a).await?;
    let tree = a.tree().await?;
    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &tree, |c| c).await?;
    let base = receipt.base;

    // On B: the view is edited and a table is made and filled.
    let mut view = sc_viewpattern::load_view(&b.catalog, app.id, "list")
        .await?
        .unwrap();
    view.description = "a local edit".into();
    sc_viewpattern::save_view(&b.catalog, &view).await?;
    b.table("scratch", &["note"]).await?;
    b.sql("INSERT INTO scratch (id, note) VALUES (1, 'mine')")
        .await?;
    let live = b.tree().await?;
    let status = Status::of(&base, &tree, &live);
    assert_eq!(
        status.entry("view:helpdesk/list").unwrap().class,
        Class::LocalChange
    );
    assert_eq!(
        status.entry("table:scratch").unwrap().class,
        Class::LocalNew
    );

    let request = request(&base, &tree);
    let keys = ["view:helpdesk/list", "table:scratch"];
    let plan = b.deployer().plan_discard(&request, &keys, None).await?;
    let drop = plan.op("drop_table:table:scratch").unwrap();
    assert_eq!((drop.class, drop.ticked), (OpClass::DataDestructive, false));
    let restore = plan.op("update:view:helpdesk/list").unwrap();
    assert_eq!((restore.class, restore.ticked), (OpClass::Safe, true));

    // Discarded with the defaults: the view is back, the table stays.
    let receipt = b
        .deployer()
        .discard(&request, &keys, None, &Confirm::defaults(&plan))
        .await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    let view = sc_viewpattern::load_view(&b.catalog, app.id, "list")
        .await?
        .unwrap();
    assert_eq!(view.description, "");
    assert_eq!(
        b.text("SELECT note FROM scratch WHERE id = 1").await,
        "mine"
    );
    assert_eq!(receipt.base, base, "a discard leaves the base where it was");
    let live = b.tree().await?;
    let status = Status::of(&base, &tree, &live);
    assert_eq!(
        status.entry("view:helpdesk/list").unwrap().class,
        Class::InSync
    );
    assert_eq!(
        status.entry("table:scratch").unwrap().class,
        Class::LocalNew
    );
    let ledger = list_deployments(&b.catalog, 1).await?;
    assert_eq!(ledger[0].kind, sc_sync::LedgerKind::Discard);
    Ok(())
}

#[tokio::test]
async fn the_never_policy_refuses_a_destructive_tick_except_from_the_cli() -> Result<()> {
    let a = Installation::new().await?;
    helpdesk(&a).await?;
    let first = a.tree().await?;
    let b = Installation::new().await?;
    let base = b.deploy(&StructureTree::new(), &first, |c| c).await?.base;
    let second = without(&first, "tables/old_notes.json");

    let never = Policy {
        destructive: Destructive::Never,
        ..Policy::default()
    };
    let mut ui = request(&base, &second);
    ui.policy = never;
    ui.source = Source::Ui;
    let plan = b.deployer().plan_deploy(&ui).await?;
    let ticked = Confirm::defaults(&plan).tick("drop_table:table:old_notes");

    // The admin API, even asking for it.
    let mut asking = ticked.clone();
    asking.allow_destructive = true;
    let refused = b.deployer().deploy(&ui, &asking).await.unwrap_err();
    assert!(refused.causes().contains("policy"), "{}", refused.causes());
    assert!(b.catalog.get("old_notes")?.is_some());

    // The CLI without the flag, then with it.
    let mut cli = ui.clone();
    cli.source = Source::Cli;
    let refused = b.deployer().deploy(&cli, &ticked).await.unwrap_err();
    assert!(
        refused.causes().contains("--allow-destructive"),
        "{}",
        refused.causes()
    );
    let receipt = b.deployer().deploy(&cli, &asking).await?;
    assert_eq!(receipt.result, Outcome::Converged);
    assert!(b.catalog.get("old_notes")?.is_none());

    // Without the policy, untouched operations deploy as they always do.
    let refused_tick = b
        .deployer()
        .deploy(&cli, &Confirm::defaults(&plan).tick("no_such_op"))
        .await
        .unwrap_err();
    assert!(
        refused_tick.causes().contains("stale") || refused_tick.causes().contains("no_such_op")
    );
    Ok(())
}

// --- file stores (S3.8) -------------------------------------------------------------

/// A bare repository with one commit, and a working clone of it at `checkout`.
/// Returns the commit.
fn web_repo(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let remote = dir.join("web.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let checkout = dir.join("web");
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
    let v1 = git(&checkout, &["rev-parse", "HEAD"]);
    (remote, checkout, v1)
}

/// A new commit in `checkout`, pushed. Returns it.
fn push_v2(checkout: &std::path::Path) -> String {
    put(checkout, "index.html", b"<h1>v2</h1>");
    git(checkout, &["commit", "-q", "-am", "v2"]);
    git(checkout, &["push", "-q"]);
    git(checkout, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn an_existing_git_store_keeps_its_own_url_and_moves_to_the_pin() -> Result<()> {
    let a = Installation::new().await?;
    let (remote, a_checkout, _) = web_repo(&a.dir);
    let url = format!("file://{}", remote.display());
    let def = FileStoreDef::git("web", url.clone())
        .with(CFG_DIR, a_checkout.to_string_lossy())
        .with(CFG_BRANCH, "main");
    sc_catalog::save_file_store(&a.catalog, &def).await?;

    // B has its own `web`, reaching the same repository by SSH (the clone's
    // origin is the bare repository; the URL is what B's admin typed).
    let b = Installation::new().await?;
    let b_checkout = b.dir.join("web");
    git(&b.dir, &["clone", "-q", &remote.to_string_lossy(), "web"]);
    let ssh = "git@github.com:example/web.git";
    let def = FileStoreDef::git("web", ssh)
        .with(CFG_DIR, b_checkout.to_string_lossy())
        .with(CFG_BRANCH, "main");
    sc_catalog::save_file_store(&b.catalog, &def).await?;
    let base = b.tree().await?;

    // A pushes and exports; B deploys.
    let v2 = push_v2(&a_checkout);
    let tree = a.tree().await?;
    assert_eq!(entity(&tree, "file-stores/web.json")["commit"], json!(v2));
    let receipt = b.deploy(&base, &tree, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");

    let here = sc_catalog::load_file_store_by_name(&b.catalog, "web")
        .await?
        .unwrap();
    assert_eq!(here.setting(sc_files::CFG_URL), Some(ssh));
    assert_eq!(git(&b_checkout, &["rev-parse", "HEAD"]), v2);
    assert_eq!(
        git(&b_checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
    assert_eq!(
        std::fs::read_to_string(b_checkout.join("index.html")).unwrap(),
        "<h1>v2</h1>"
    );
    Ok(())
}

#[tokio::test]
async fn a_new_git_store_is_cloned_from_its_public_url_at_the_pin() -> Result<()> {
    let a = Installation::new().await?;
    let (remote, a_checkout, _) = web_repo(&a.dir);
    let public = format!("file://{}", remote.display());
    let def = FileStoreDef::git("web", "git@example.com:private/web.git")
        .with(CFG_DIR, a_checkout.to_string_lossy())
        .with(CFG_BRANCH, "main")
        .with(CFG_PUBLIC_URL, public.clone());
    sc_catalog::save_file_store(&a.catalog, &def).await?;
    let v2 = push_v2(&a_checkout);
    // A new commit after the export is not what B gets.
    let tree = a.tree().await?;
    put(&a_checkout, "index.html", b"<h1>v3</h1>");
    git(&a_checkout, &["commit", "-q", "-am", "v3"]);
    git(&a_checkout, &["push", "-q"]);

    let b = Installation::new().await?;
    let plan = b.plan_deploy(&StructureTree::new(), &tree).await?;
    assert!(
        plan.needs_credentials.is_empty(),
        "{:?}",
        plan.needs_credentials
    );
    let receipt = b.deploy(&StructureTree::new(), &tree, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");

    let here = sc_catalog::load_file_store_by_name(&b.catalog, "web")
        .await?
        .unwrap();
    assert_eq!(here.setting(sc_files::CFG_URL), Some(public.as_str()));
    let clone = b.dir.join("data/git-stores/web");
    assert_eq!(sc_files::clone_path(&here)?, clone);
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), v2);
    assert_eq!(
        std::fs::read_to_string(clone.join("index.html")).unwrap(),
        "<h1>v2</h1>"
    );
    Ok(())
}

#[tokio::test]
async fn synced_files_are_written_and_a_deleted_one_is_a_metadata_delete() -> Result<()> {
    let a = Installation::new().await?;
    let (_, assets) = a
        .local_store("assets", |d| {
            d.with_sync_contents(sc_files::SyncContents::Include)
        })
        .await?;
    put(&assets, "logo.png", b"\x89PNG logo");
    put(&assets, "css/site.css", b"body { margin: 0 }\n");
    a.local_store("uploads", |d| d).await?;
    let first = a.tree().await?;
    assert!(first.get("file-stores/assets/files/logo.png").is_some());

    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    let here = b.dir.join("data/local-stores/assets");
    assert_eq!(
        std::fs::read(here.join("logo.png")).unwrap(),
        b"\x89PNG logo"
    );
    assert!(here.join("css/site.css").is_file());
    let uploads = b.dir.join("data/local-stores/uploads");
    assert!(uploads.is_dir());
    assert_eq!(std::fs::read_dir(&uploads).unwrap().count(), 0);

    // A deletes the logo.
    std::fs::remove_file(assets.join("logo.png")).unwrap();
    let second = a.tree().await?;
    let plan = b.plan_deploy(&receipt.base, &second).await?;
    let op = plan.op("delete:store_file:assets/logo.png").unwrap();
    assert_eq!((op.class, op.ticked), (OpClass::MetadataDelete, true));
    let receipt = b.deploy(&receipt.base, &second, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(!here.join("logo.png").exists());
    assert!(here.join("css/site.css").is_file());
    Ok(())
}

#[tokio::test]
async fn a_checklist_is_created_with_its_id_and_updated_in_place() -> Result<()> {
    let a = Installation::new().await?;
    let made = sc_check::edit::create_checklist(
        &a.catalog,
        &sc_check::StepRegistry::installed(),
        &sc_check::Editor::Human(uuid::Uuid::new_v4()),
        sc_check::ChecklistInput::new("smoke"),
        Vec::new(),
    )
    .await?;
    let first = a.tree().await?;
    let b = Installation::new().await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    let there = sc_check::store::load_checklist_by_name(&b.catalog, "smoke")
        .await?
        .unwrap();
    assert_eq!(there.id, made.id);

    let mut changed = first.clone();
    let mut value = entity(&first, "checklists/smoke.json").clone();
    value["description"] = json!("the pages answer");
    changed.insert_json("checklists/smoke.json", value);
    let receipt = b.deploy(&receipt.base, &changed, |c| c).await?;
    assert_eq!(receipt.applied.applied, ["update:checklist:smoke"]);
    let there = sc_check::store::load_checklist(&b.catalog, made.id)
        .await?
        .unwrap();
    assert_eq!(there.description, "the pages answer");
    assert_eq!(b.tree().await?.to_files(), changed.to_files());
    Ok(())
}

#[tokio::test]
async fn a_provided_table_is_created_with_its_id_renamed_reconfigured_and_forgotten() -> Result<()>
{
    use crate::fixture::FeedProvider;
    let a = Installation::new().await?;
    FeedProvider::install(&a).await?;
    let table = sc_api::provided_tables::create(
        &a.catalog,
        "headlines",
        "@test/feed",
        "Feed",
        json!({ "url": "https://example.com/rss", "token": "FEED-SECRET-7" })
            .as_object()
            .cloned()
            .unwrap(),
    )
    .await?;
    let first = a.tree().await?;
    let exported = entity(&first, "tables/headlines.json");
    assert_eq!(
        exported["provider"]["configuration"]["token"],
        sc_types::SECRET_SENTINEL
    );

    let b = Installation::new().await?;
    FeedProvider::install(&b).await?;
    let receipt = b.deploy(&StructureTree::new(), &first, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert_eq!(receipt.applied.applied, ["create:table:headlines"]);
    let here = b.catalog.require("headlines")?;
    assert_eq!(here.overlay, table.overlay, "the table keeps its id");
    assert_eq!(here.fields.len(), 2);
    // The same table, its token not yet set here (§3.3).
    assert_eq!(receipt.needs_secrets, ["table:headlines"]);
    let there = b.tree().await?;
    let (mine, theirs) = (Entities::of(&first), Entities::of(&there));
    let pick = |e: &Entities| {
        e.list
            .iter()
            .find(|e| e.path == "tables/headlines.json")
            .cloned()
            .unwrap()
    };
    assert!(pick(&mine).same_as(&pick(&theirs)));

    // B's admin gives it a token of its own; A renames it, changes its URL
    // and its label. The deploy renames rather than recreates, keeps B's
    // token and takes the rest.
    let meta = sc_catalog::load_table_meta_by_name(&b.catalog, "headlines")
        .await?
        .unwrap();
    let mut configuration = meta.provider().unwrap().configuration;
    configuration.insert("token".into(), json!("B-OWN-TOKEN"));
    sc_api::provided_tables::configure(&b.catalog, "headlines", configuration).await?;
    a.schema(vec![sc_api::schema_edit::Operation::RenameTable {
        table: "headlines".into(),
        to: "news".into(),
    }])
    .await?;
    sc_api::provided_tables::configure(
        &a.catalog,
        "news",
        json!({ "url": "https://example.com/news", "token": "FEED-SECRET-7" })
            .as_object()
            .cloned()
            .unwrap(),
    )
    .await?;
    let second = a.tree().await?;
    let mut settings = entity(&second, "tables/news.json").clone();
    settings["label"] = json!("News");
    let mut second = second.clone();
    second.insert_json("tables/news.json", settings.clone());
    let mut meta = sc_catalog::load_table_meta_by_name(&a.catalog, "news")
        .await?
        .unwrap();
    meta.label = "News".into();
    sc_catalog::save_table_meta(&a.catalog, &meta).await?;
    assert_eq!(
        String::from_utf8_lossy(&a.tree().await?.to_files()["tables/news.json"]),
        String::from_utf8_lossy(&second.to_files()["tables/news.json"])
    );

    let receipt = b.deploy(&receipt.base, &second, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(b.catalog.get("headlines")?.is_none());
    let meta = sc_catalog::load_table_meta_by_name(&b.catalog, "news")
        .await?
        .unwrap();
    assert_eq!(meta.id.0, table.overlay.unwrap().0);
    assert_eq!(meta.label, "News");
    let def = meta.provider().unwrap();
    assert_eq!(def.configuration["url"], "https://example.com/news");
    assert_eq!(def.configuration["token"], "B-OWN-TOKEN");

    // A deletes it: a ticked metadata delete, which forgets the definition.
    let third = without(&second, "tables/news.json");
    let plan = b.plan_deploy(&receipt.base, &third).await?;
    let op = plan.op("delete:table:news").unwrap();
    assert_eq!((op.class, op.ticked), (OpClass::MetadataDelete, true));
    let receipt = b.deploy(&receipt.base, &third, |c| c).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(b.catalog.get("news")?.is_none());
    assert!(
        sc_catalog::load_table_meta_by_name(&b.catalog, "news")
            .await?
            .is_none()
    );
    Ok(())
}
