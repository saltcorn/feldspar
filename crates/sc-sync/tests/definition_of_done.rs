//! The definition of done of `TODO-app-sync.md` (S6.3), step by step: two
//! instances, A and B, in one process, linked to the same branch of a bare
//! repository on this machine, with a second bare repository behind the git
//! store `web`.
//!
//! "B serves the `helpdesk` application" is checked as far as a test
//! installation goes: the application, its views and its trigger are on B,
//! and the deploy asked the host to build and mount it. Serving a built
//! application is `sc-server`'s, and is tested there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use sc_api::schema_edit::{FieldSettings, Operation};
use sc_error::Result;
use sc_files::{CFG_BRANCH, CFG_DIR, FileStoreDef, SyncContents};
use sc_sync::registry::{self, Edit, EditBy};
use sc_sync::{
    Class, CommitRequest, Confirm, LinkedStatus, OpClass, Outcome, Policy, Source, SourceControl,
    SyncLink, list_deployments,
};

use crate::fixture::{Installation, code_app, file_field, git, put};

fn source(inst: &Installation) -> SourceControl<'_> {
    SourceControl::new(inst.deployer())
}

fn commit(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.into(),
        actor: Some("ann@example.com".into()),
        source: Some(Source::Ui),
        ..CommitRequest::default()
    }
}

/// What Status shows, by key.
fn shown(status: &LinkedStatus) -> BTreeMap<String, Class> {
    status
        .status
        .shown()
        .map(|e| (e.key.clone(), e.class))
        .collect()
}

fn class_of(status: &LinkedStatus, key: &str) -> Option<Class> {
    status.status.entry(key).map(|e| e.class)
}

/// A bare repository at `dir/name`, and its `file://` URL.
fn bare(dir: &Path, name: &str) -> (PathBuf, String) {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q", "--bare", "--initial-branch=main"]);
    let url = format!("file://{}", path.display());
    (path, url)
}

/// Every file of `url`'s `main` as it is now, from a fresh clone.
fn remote_files(dir: &Path, url: &str) -> BTreeMap<String, Vec<u8>> {
    let path = dir.join(format!("look-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["clone", "-q", url, "."]);
    git(&path, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .map(|name| (name.to_owned(), std::fs::read(path.join(name)).unwrap()))
        .collect()
}

fn commit_count(repo: &Path) -> usize {
    git(repo, &["rev-list", "--count", "main"]).parse().unwrap()
}

/// Deploy what `sc` last pulled, ticking `tick` on top of the defaults.
async fn deploy(sc: &SourceControl<'_>, tick: &[&str]) -> Result<sc_sync::Receipt> {
    let planned = sc.plan_deploy(None, Source::Ui).await?;
    assert!(
        planned.plan.blockers.is_empty(),
        "{:#?}",
        planned.plan.blockers
    );
    let mut confirm = Confirm::defaults(&planned.plan);
    for id in tick {
        confirm = confirm.tick(id);
    }
    sc.deploy(planned.commit.as_deref(), &confirm, None, Source::Ui)
        .await
}

/// Plan a registry save of `path` on `inst`, its JSON changed by `change`.
async fn plan_save(
    inst: &Installation,
    path: &str,
    change: impl FnOnce(&mut Json),
) -> Result<(Edit, sc_sync::Plan)> {
    let deployer = inst.deployer();
    let export = deployer.instance_export().await?;
    let opened = registry::get(&export, path).expect("the entry is there");
    let mut value = opened.value.clone();
    change(&mut value);
    let edit = Edit {
        path: path.into(),
        value: Some(value),
        version: Some(opened.entry.version),
    };
    let planned = deployer.plan_edit(&edit).await?;
    Ok((
        Edit {
            value: planned.value,
            ..edit
        },
        planned.plan,
    ))
}

/// A registry save of `path` on `inst` with the plan's default ticks.
async fn save(inst: &Installation, path: &str, change: impl FnOnce(&mut Json)) -> Result<()> {
    let (edit, plan) = plan_save(inst, path, change).await?;
    assert!(plan.blockers.is_empty(), "{:#?}", plan.blockers);
    let by = EditBy {
        actor: Some("admin@example.com".into()),
        source: Source::Ui,
        policy: Policy::default(),
    };
    let receipt = inst
        .deployer()
        .edit(&edit, &Confirm::defaults(&plan), &by)
        .await?;
    assert_eq!(receipt.receipt.result, Outcome::Converged, "{receipt:#?}");
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_definition_of_done() -> Result<()> {
    let a = Installation::new().await?;
    let b = Installation::new().await?;
    let (repo, url) = bare(&a.dir, "structure.git");
    let (sa, sb) = (source(&a), source(&b));
    sa.set_link(SyncLink::new(&url, "main")).await?;

    // 1. On A: `tickets` (title, status), `old_notes`, the application
    //    `helpdesk` with a list and an edit view on `tickets`, and an insert
    //    trigger. Status shows each as Local; Commit pushes one commit.
    a.table("tickets", &["title", "status"]).await?;
    a.table("old_notes", &["body"]).await?;
    a.local_store("code", |d| d.with_sync_contents(SyncContents::Include))
        .await?;
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = a.application(body).await?;
    for (name, pattern) in [("list", "List"), ("edit", "Edit")] {
        let view = sc_viewpattern::View::new(app.id, name, pattern, "tickets");
        sc_viewpattern::save_view(&a.catalog, &view).await?;
    }
    let trigger = sc_backup::codec::trigger_from_body(
        sc_action::TriggerId::new(),
        &json!({
            "name": "notify", "when": "insert", "channel": "tickets",
            "action": "send_email",
            "configuration": { "to": "desk@example.com", "subject": "New ticket", "text": "!" },
        }),
    )?;
    sc_action::save_trigger(&a.catalog, &a.registries.actions, &trigger).await?;

    let status = sa.status().await?;
    let made = [
        "table:tickets",
        "table:old_notes",
        "application:helpdesk",
        "view:helpdesk/list",
        "view:helpdesk/edit",
        "trigger:notify",
    ];
    for key in made {
        assert_eq!(class_of(&status, key), Some(Class::LocalNew), "{key}");
    }
    assert!(shown(&status).values().all(|c| *c == Class::LocalNew));
    let first = sa.commit(&commit("The helpdesk")).await?;
    assert_eq!(commit_count(&repo), 1);
    assert!(shown(&sa.status().await?).is_empty());

    // 2. On B: Pull shows them as Incoming; Deploy converges; B has the
    //    application, and an export from B is the commit byte for byte.
    sb.set_link(SyncLink::new(&url, "main")).await?;
    let status = sb.status().await?;
    for key in made {
        assert_eq!(class_of(&status, key), Some(Class::IncomingNew), "{key}");
    }
    let receipt = deploy(&sb, &[]).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    let b_app = sc_app::load_application_by_subdomain(&b.catalog, "helpdesk")
        .await?
        .expect("helpdesk is on B");
    assert!(
        sc_viewpattern::load_view(&b.catalog, b_app.id, "list")
            .await?
            .is_some()
    );
    assert!(
        b.host
            .built
            .lock()
            .unwrap()
            .contains(&"helpdesk".to_owned())
    );
    let committed = remote_files(&a.dir, &url);
    assert_eq!(b.tree().await?.to_files(), committed);
    assert_eq!(
        list_deployments(&b.catalog, 1).await?[0].commit.as_deref(),
        Some(first.commit.as_str())
    );
    b.sql("INSERT INTO old_notes (body) VALUES ('first'), ('second')")
        .await?;

    // 3. On A: rename `status` to `state`, delete the edit view, drop
    //    `old_notes`. Commit.
    a.schema(vec![Operation::AlterField {
        table: "tickets".into(),
        field: "status".into(),
        settings: FieldSettings {
            name: Some("state".into()),
            ..FieldSettings::default()
        },
    }])
    .await?;
    sc_viewpattern::delete_view(&a.catalog, app.id, "edit").await?;
    a.schema(vec![Operation::DropTable {
        table: "old_notes".into(),
    }])
    .await?;
    sa.commit(&commit("Rename, delete, drop")).await?;

    // 4. On B, the plan: a field rename (not a drop and create), a ticked
    //    metadata delete of the view, an unticked drop of `old_notes`.
    sb.pull().await?;
    let planned = sb.plan_deploy(None, Source::Ui).await?;
    let ops: BTreeMap<&str, (OpClass, bool)> = planned
        .plan
        .operations
        .iter()
        .map(|op| (op.id.as_str(), (op.class, op.ticked)))
        .collect();
    let rename = planned
        .plan
        .operations
        .iter()
        .find(|op| op.action == "rename_field")
        .unwrap_or_else(|| panic!("no field rename: {ops:#?}"));
    assert_eq!(rename.entity, "table:tickets");
    assert!(
        !ops.keys()
            .any(|id| id.starts_with("drop_field") || id.starts_with("add_field")),
        "{ops:#?}"
    );
    assert_eq!(
        ops.get("delete:view:helpdesk/edit"),
        Some(&(OpClass::MetadataDelete, true))
    );
    assert_eq!(
        ops.get("drop_table:table:old_notes"),
        Some(&(OpClass::DataDestructive, false))
    );

    // 5. B deploys without the drop: `old_notes` and its rows stay, and
    //    Status still offers it as an incoming delete.
    let receipt = deploy(&sb, &[]).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert_eq!(receipt.applied.deferred, ["drop_table:table:old_notes"]);
    assert_eq!(b.text("SELECT count(*)::text FROM old_notes").await, "2");
    let status = sb.status().await?;
    assert_eq!(
        shown(&status),
        BTreeMap::from([("table:old_notes".to_owned(), Class::IncomingDelete)])
    );
    assert!(b.catalog.get("tickets")?.unwrap().field("state").is_some());
    assert!(
        sc_viewpattern::load_view(&b.catalog, b_app.id, "edit")
            .await?
            .is_none()
    );

    // 6. An admin on B edits the list view: Deploy is refused with a sentence
    //    about uncommitted changes. Discard restores it, and Deploy is allowed.
    let mut view = sc_viewpattern::load_view(&b.catalog, b_app.id, "list")
        .await?
        .unwrap();
    let before = view.description.clone();
    view.description = "edited on B".into();
    sc_viewpattern::save_view(&b.catalog, &view).await?;
    let planned = sb.plan_deploy(None, Source::Ui).await?;
    let refusal: Vec<&str> = planned
        .plan
        .blockers
        .iter()
        .map(|b| b.message.as_str())
        .collect();
    assert!(
        refusal.iter().any(|m| m.contains("uncommitted change")
            && m.contains("view:helpdesk/list")
            && m.contains("commit or discard")),
        "{refusal:?}"
    );
    let keys = ["view:helpdesk/list"];
    let plan = sb.plan_discard(&keys).await?;
    sb.discard(&keys, &Confirm::defaults(&plan), None, Source::Ui)
        .await?;
    let view = sc_viewpattern::load_view(&b.catalog, b_app.id, "list")
        .await?
        .unwrap();
    assert_eq!(view.description, before);
    let planned = sb.plan_deploy(None, Source::Ui).await?;
    assert!(
        planned.plan.blockers.is_empty(),
        "{:#?}",
        planned.plan.blockers
    );

    // 7. B deploys with the drop ticked: a safety backup holding the rows of
    //    `old_notes` is written first, and it is in the receipt.
    let receipt = deploy(&sb, &["drop_table:table:old_notes"]).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(b.catalog.get("old_notes")?.is_none());
    let [backup] = receipt.backups.as_slice() else {
        panic!("one backup: {:?}", receipt.backups);
    };
    let (held, _) = sc_backup::inspect(Path::new(backup), None)?;
    let rows = held
        .tables
        .iter()
        .find(|t| t.name == "old_notes")
        .expect("old_notes is in the safety backup");
    assert_eq!(rows.count, Some(2));
    let ledger = list_deployments(&b.catalog, 1).await?;
    assert_eq!(ledger[0].id, receipt.id);
    assert_eq!(&ledger[0].backups, &receipt.backups);
    assert!(shown(&sb.status().await?).is_empty());

    // 8. File stores. On A: `assets` (include) with logo.png; `uploads`, which
    //    a File field on `tickets` writes to; the git store `web` on a second
    //    bare repository.
    let (_, assets) = a
        .local_store("assets", |d| d.with_sync_contents(SyncContents::Include))
        .await?;
    put(&assets, "logo.png", b"\x89PNG logo");
    let (_, uploads) = a.local_store("uploads", |d| d).await?;
    put(&uploads, "ticket-1.pdf", b"%PDF a customer's file");
    a.schema(vec![Operation::AddField {
        table: "tickets".into(),
        field: file_field("attachment", "uploads", None),
    }])
    .await?;
    let (web_repo, web_url) = bare(&a.dir, "web.git");
    let checkout = a.dir.join("web-checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    put(&checkout, "index.html", b"<h1>v1</h1>");
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-q", "-m", "v1"]);
    git(&checkout, &["remote", "add", "origin", &web_url]);
    git(&checkout, &["push", "-q", "-u", "origin", "main"]);
    let v1 = git(&checkout, &["rev-parse", "HEAD"]);
    let web = FileStoreDef::git("web", web_url.clone())
        .with(CFG_DIR, checkout.to_string_lossy())
        .with(CFG_BRANCH, "main");
    sc_catalog::save_file_store(&a.catalog, &web).await?;

    // A Commit carries logo.png, the three stores and web's pin, and none of
    // uploads' files.
    sa.commit(&commit("Stores")).await?;
    let committed = remote_files(&a.dir, &url);
    let paths: BTreeSet<&str> = committed.keys().map(String::as_str).collect();
    for path in [
        "file-stores/assets/files/logo.png",
        "file-stores/assets.json",
        "file-stores/uploads.json",
        "file-stores/web.json",
    ] {
        assert!(paths.contains(path), "{path} not in {paths:#?}");
    }
    assert!(
        !paths.iter().any(|p| p.starts_with("file-stores/uploads/")),
        "{paths:#?}"
    );
    let pin = |files: &BTreeMap<String, Vec<u8>>| -> String {
        let web: Json = serde_json::from_slice(&files["file-stores/web.json"]).unwrap();
        web["commit"].as_str().unwrap().to_owned()
    };
    assert_eq!(pin(&committed), v1);

    // After a push to `web`, Status on A shows the moved pin — which blocks
    // no deploy, on A or on B.
    put(&checkout, "index.html", b"<h1>v2</h1>");
    git(&checkout, &["commit", "-q", "-am", "v2"]);
    git(&checkout, &["push", "-q"]);
    let v2 = git(&checkout, &["rev-parse", "HEAD"]);
    let status = sa.status().await?;
    assert_eq!(class_of(&status, "file_store:web"), Some(Class::PinMoved));
    assert!(
        status.status.deploy_blockers().is_empty(),
        "{:#?}",
        status.status
    );
    let planned = sa.plan_deploy(None, Source::Ui).await?;
    assert!(
        planned.plan.blockers.is_empty(),
        "{:#?}",
        planned.plan.blockers
    );
    let receipt = deploy(&sb, &[]).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");

    // After the next Commit and Deploy, B has logo.png, an empty `uploads`,
    // and `web` checked out at the pinned commit.
    sa.commit(&commit("web at v2")).await?;
    assert_eq!(pin(&remote_files(&a.dir, &url)), v2);
    sb.pull().await?;
    let receipt = deploy(&sb, &[]).await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    let b_stores = b.dir.join("data/local-stores");
    assert_eq!(
        std::fs::read(b_stores.join("assets/logo.png")).unwrap(),
        b"\x89PNG logo"
    );
    assert_eq!(
        std::fs::read_dir(b_stores.join("uploads")).unwrap().count(),
        0
    );
    let b_web = b.dir.join("data/git-stores/web");
    assert_eq!(git(&b_web, &["rev-parse", "HEAD"]), v2);
    assert_eq!(
        std::fs::read_to_string(b_web.join("index.html")).unwrap(),
        "<h1>v2</h1>"
    );
    assert_eq!(commit_count(&web_repo), 2);
    assert!(
        shown(&sb.status().await?).is_empty(),
        "{:#?}",
        sb.status().await?.status
    );

    // 9. The registry. On A, the list view's JSON is changed and saved: Status
    //    shows it as Local, and Commit commits it.
    let view_path = "applications/helpdesk/views/list.json";
    save(&a, view_path, |v| {
        v["description"] = json!("Every open ticket")
    })
    .await?;
    let status = sa.status().await?;
    assert_eq!(
        shown(&status),
        BTreeMap::from([("view:helpdesk/list".to_owned(), Class::LocalChange)])
    );
    let committed = sa.commit(&commit("Describe the list")).await?;
    assert_eq!(committed.entities, ["view:helpdesk/list"]);

    // The email section, edited in the registry, is applied on A, Status
    // shows nothing, and no commit contains it.
    sc_config::set_config(
        &a.catalog,
        sc_config::email::SMTP_HOST,
        json!("smtp.a.example"),
    )
    .await?;
    save(&a, "settings/email.json", |v| {
        v["smtp_host"] = json!("relay.a.example");
    })
    .await?;
    assert_eq!(
        sc_config::stored_config(&a.catalog, sc_config::email::SMTP_HOST).await?,
        Some(json!("relay.a.example"))
    );
    assert!(shown(&sa.status().await?).is_empty());
    let nothing = sa.commit(&commit("Email")).await.unwrap_err().to_string();
    assert!(nothing.contains("nothing to commit"), "{nothing}");
    let committed = remote_files(&a.dir, &url);
    assert!(
        !committed.keys().any(|p| p.contains("email")),
        "{:?}",
        committed.keys()
    );
    assert!(
        !committed
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes).contains("relay.a.example"))
    );

    // Removing `title` from tickets' JSON shows a data-destructive drop,
    // unticked.
    let (_, plan) = plan_save(&a, "tables/tickets.json", |v| {
        v["fields"]
            .as_array_mut()
            .unwrap()
            .retain(|f| f["name"] != "title");
    })
    .await?;
    let drop = plan
        .op("drop_field:table:tickets#title")
        .unwrap_or_else(|| panic!("{:#?}", plan.operations));
    assert_eq!((drop.class, drop.ticked), (OpClass::DataDestructive, false));
    Ok(())
}
