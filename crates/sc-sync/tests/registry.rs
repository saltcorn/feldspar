//! The registry editor (R1, R2): the instance export, and a save as a
//! one-entity deploy against live.

use serde_json::{Value as Json, json};

use sc_error::Result;
use sc_sync::registry::{self, version};
use sc_sync::{
    Class, Confirm, Edit, EditBy, EditReceipt, InstanceOnly, LedgerKind, OpClass, Policy, Source,
    Status, list_deployments,
};

use crate::fixture::{Installation, all_text, code_app};

/// A helpdesk with a table, an application and a list view, a local store, an
/// SMTP password, an identity provider with a client secret, a custom domain
/// and a build host.
async fn installation() -> Result<(Installation, sc_app::Application)> {
    let inst = Installation::new().await?;
    let catalog = &inst.catalog;
    inst.table("tickets", &["title", "status"]).await?;
    inst.local_store("code", |d| d).await?;
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = inst.application(body).await?;
    let view = sc_viewpattern::View::new(app.id, "list", "List", "tickets");
    sc_viewpattern::save_view(catalog, &view).await?;

    sc_config::set_config(
        catalog,
        sc_config::email::SMTP_HOST,
        json!("mail.example.org"),
    )
    .await?;
    sc_config::set_config(
        catalog,
        sc_config::email::SMTP_PASSWORD,
        json!("SMTP-SECRET-9"),
    )
    .await?;
    sc_config::set_config(
        catalog,
        sc_config::ssl::SSL_EXTRA_DOMAINS,
        json!("desk.example.org"),
    )
    .await?;
    sc_config::authentication::save_oauth_provider_records(
        catalog,
        &[json!({
            "preset": "custom",
            "name": "corp",
            "label": "Corp",
            "client_id": "corp-client",
            "client_secret": "OAUTH-SECRET-4",
            "authorization_url": "https://id.example.org/authorize",
            "token_url": "https://id.example.org/token",
            "userinfo_url": "https://id.example.org/userinfo",
        })
        .as_object()
        .cloned()
        .unwrap()],
    )
    .await?;
    sc_app::bootstrap_build_hosts(catalog).await?;
    sc_app::add_build_host(catalog, "mac-mini", &["ios".to_owned()]).await?;
    Ok((inst, app))
}

fn by() -> EditBy {
    EditBy {
        actor: Some("admin@example.com".into()),
        source: Source::Ui,
        policy: Policy::default(),
    }
}

/// Open `path`, change its JSON with `change`, plan and save it with the
/// plan's default ticks adjusted by `confirm`.
async fn save(
    inst: &Installation,
    path: &str,
    change: impl FnOnce(&mut Json),
    confirm: impl FnOnce(Confirm) -> Confirm,
) -> Result<EditReceipt> {
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
    assert!(planned.plan.blockers.is_empty(), "{:#?}", planned.plan);
    let confirm = confirm(Confirm::defaults(&planned.plan));
    let edit = Edit {
        value: planned.value,
        ..edit
    };
    deployer.edit(&edit, &confirm, &by()).await
}

#[tokio::test]
async fn the_instance_tree_holds_what_is_this_instances_own_and_no_secret() -> Result<()> {
    let (inst, _) = installation().await?;
    let portable = inst.export().await?;
    let instance = inst.deployer().instance_export().await?;

    // The email section and the custom domain (the TLS section's extra
    // domains) are this instance's: in the instance tree, marked, and not in
    // the portable one.
    for (path, key) in [
        ("settings/email.json", "smtp_host"),
        ("settings/ssl.json", "ssl_extra_domains"),
        ("settings/internal.json", "oauth_providers"),
        ("build-hosts/mac-mini.json", "targets"),
    ] {
        let value = instance.tree.json(path).unwrap_or_else(|| panic!("{path}"));
        assert!(value.get(key).is_some(), "{path}: {value}");
        assert_eq!(
            instance.instance_only.get(path),
            Some(&InstanceOnly::Whole),
            "{path}"
        );
        assert!(portable.tree.json(path).is_none(), "{path} is portable");
    }
    assert_eq!(
        instance.tree.json("settings/ssl.json").unwrap()["ssl_extra_domains"],
        "desk.example.org"
    );
    // A portable section is in both, unmarked.
    assert!(instance.tree.json("settings/localisation.json").is_some());
    assert!(
        !instance
            .instance_only
            .contains_key("settings/localisation.json")
    );

    // A local store's directory is this instance's value in a portable file.
    let store = instance.tree.json("file-stores/code.json").unwrap();
    assert!(store["config"]["path"].as_str().is_some(), "{store}");
    assert_eq!(
        instance.instance_only.get("file-stores/code.json"),
        Some(&InstanceOnly::Keys {
            keys: vec!["/config/path".into()]
        })
    );
    assert!(
        portable.tree.json("file-stores/code.json").unwrap()["config"]
            .get("path")
            .is_none()
    );

    // The portable export has no marks, and neither tree a secret.
    assert!(portable.instance_only.is_empty());
    for tree in [&portable.tree, &instance.tree] {
        let text = all_text(tree);
        for secret in ["SMTP-SECRET-9", "OAUTH-SECRET-4", "fsbh_"] {
            assert!(!text.contains(secret), "{secret} in an export");
        }
    }
    assert_eq!(
        instance.tree.json("settings/email.json").unwrap()["smtp_password"],
        sc_types::SECRET_SENTINEL
    );
    assert_eq!(
        instance.tree.json("settings/internal.json").unwrap()["oauth_providers"][0]["client_secret"],
        sc_types::SECRET_SENTINEL
    );

    // The instance tree is the portable tree plus the rest: every portable
    // file is there, and the same but for a store's own settings.
    for (path, entry) in portable.tree.iter() {
        if path.starts_with("file-stores/") && path.ends_with(".json") {
            continue;
        }
        assert_eq!(instance.tree.get(path), Some(entry), "{path}");
    }
    // And listing it marks the same files.
    let listed = registry::list(&instance);
    let email = listed
        .iter()
        .find(|e| e.path == "settings/email.json")
        .unwrap();
    assert_eq!(email.instance_only, Some(InstanceOnly::Whole));
    let view = listed
        .iter()
        .find(|e| e.path == "applications/helpdesk/views/list.json")
        .unwrap();
    assert_eq!(
        (view.kind.as_str(), view.application.as_deref()),
        ("view", Some("helpdesk"))
    );
    Ok(())
}

#[tokio::test]
async fn a_view_edit_applies_shows_as_local_and_is_recorded() -> Result<()> {
    let (inst, app) = installation().await?;
    let synced = inst.tree().await?;
    let path = "applications/helpdesk/views/list.json";

    let receipt = save(
        &inst,
        path,
        |v| v["description"] = json!("Every open ticket"),
        |c| c,
    )
    .await?;
    assert_eq!(receipt.receipt.result.as_str(), "converged", "{receipt:#?}");
    assert_eq!(receipt.path.as_deref(), Some(path));
    assert!(receipt.changes.is_empty(), "{:#?}", receipt.changes);
    let view = sc_viewpattern::load_view(&inst.catalog, app.id, "list")
        .await?
        .unwrap();
    assert_eq!(view.description, "Every open ticket");

    // With the tree before the save as base and tip, the view is a local
    // change, and nothing else is.
    let live = inst.tree().await?;
    let status = Status::of(&synced, &synced, &live);
    let shown: Vec<_> = status.shown().map(|e| (e.key.as_str(), e.class)).collect();
    assert_eq!(shown, [("view:helpdesk/list", Class::LocalChange)]);

    // The ledger has the save, with the view before and after.
    let rows = list_deployments(&inst.catalog, 10).await?;
    let row = rows.iter().find(|r| r.kind == LedgerKind::Edit).unwrap();
    assert_eq!(row.actor.as_deref(), Some("admin@example.com"));
    assert_eq!(row.before.as_ref().unwrap()["description"], "");
    assert_eq!(
        row.after.as_ref().unwrap()["description"],
        "Every open ticket"
    );
    assert_eq!(row.summary["path"], path);
    Ok(())
}

#[tokio::test]
async fn a_settings_edit_applies_and_is_not_a_local_change() -> Result<()> {
    let (inst, _) = installation().await?;
    let synced = inst.tree().await?;
    let receipt = save(
        &inst,
        "settings/email.json",
        |v| v["smtp_host"] = json!("relay.example.org"),
        |c| c,
    )
    .await?;
    assert_eq!(receipt.receipt.result.as_str(), "converged", "{receipt:#?}");
    assert_eq!(
        sc_config::stored_config(&inst.catalog, sc_config::email::SMTP_HOST).await?,
        Some(json!("relay.example.org"))
    );
    // The sentinel kept the password.
    assert_eq!(
        sc_config::stored_config(&inst.catalog, sc_config::email::SMTP_PASSWORD).await?,
        Some(json!("SMTP-SECRET-9"))
    );
    let live = inst.tree().await?;
    assert_eq!(Status::of(&synced, &synced, &live).shown().count(), 0);
    assert_eq!(live, synced);
    Ok(())
}

#[tokio::test]
async fn a_sentinel_keeps_a_secret_and_a_new_value_replaces_it() -> Result<()> {
    let (inst, _) = installation().await?;
    // The client secret inside a list item, handed back as the sentinel, is
    // kept while the item's label changes.
    save(
        &inst,
        "settings/internal.json",
        |v| v["oauth_providers"][0]["label"] = json!("Corporate"),
        |c| c,
    )
    .await?;
    let records = sc_config::authentication::oauth_provider_records(&inst.catalog).await?;
    assert_eq!(records[0]["label"], "Corporate");
    assert_eq!(records[0]["client_secret"], "OAUTH-SECRET-4");

    // A typed value replaces the stored one, and reads back as the sentinel,
    // which is not reported as a difference.
    let receipt = save(
        &inst,
        "settings/email.json",
        |v| v["smtp_password"] = json!("NEW-SECRET-2"),
        |c| c,
    )
    .await?;
    assert!(receipt.changes.is_empty(), "{:#?}", receipt.changes);
    assert_eq!(
        receipt.stored.unwrap()["smtp_password"],
        sc_types::SECRET_SENTINEL
    );
    assert_eq!(
        sc_config::stored_config(&inst.catalog, sc_config::email::SMTP_PASSWORD).await?,
        Some(json!("NEW-SECRET-2"))
    );
    Ok(())
}

#[tokio::test]
async fn removing_a_field_is_an_unticked_data_destructive_drop() -> Result<()> {
    let (inst, _) = installation().await?;
    inst.sql("INSERT INTO tickets (id, title, status) VALUES (1, 'Printer', 'open')")
        .await?;
    let path = "tables/tickets.json";
    let deployer = inst.deployer();
    let export = deployer.instance_export().await?;
    let opened = registry::get(&export, path).unwrap();
    let mut value = opened.value.clone();
    value["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["name"] != "title");
    let edit = Edit {
        path: path.into(),
        value: Some(value),
        version: Some(opened.entry.version),
    };
    let planned = deployer.plan_edit(&edit).await?;
    let ops: Vec<_> = planned
        .plan
        .operations
        .iter()
        .map(|op| (op.id.as_str(), op.class, op.ticked))
        .collect();
    assert_eq!(
        ops,
        [(
            "drop_field:table:tickets#title",
            OpClass::DataDestructive,
            false
        )]
    );

    // Saved as offered, the drop is deferred and the column stays.
    let edit = Edit {
        value: planned.value.clone(),
        ..edit
    };
    let receipt = deployer
        .edit(&edit, &Confirm::defaults(&planned.plan), &by())
        .await?;
    assert_eq!(
        receipt.receipt.applied.deferred,
        ["drop_field:table:tickets#title"]
    );
    assert_eq!(inst.text("SELECT title FROM tickets").await, "Printer");
    // What is stored still has the field, and the receipt says so.
    assert!(
        receipt
            .changes
            .iter()
            .any(|c| c.path.starts_with("fields[title]")),
        "{:#?}",
        receipt.changes
    );

    // Ticked, it is dropped after a safety backup.
    let receipt = deployer
        .edit(
            &edit,
            &Confirm::defaults(&planned.plan).tick("drop_field:table:tickets#title"),
            &by(),
        )
        .await?;
    assert_eq!(receipt.receipt.backups.len(), 1, "{receipt:#?}");
    let table = inst.catalog.get("tickets")?.unwrap();
    assert!(table.fields.iter().all(|f| f.base.name != "title"));
    Ok(())
}

#[tokio::test]
async fn a_save_over_a_change_made_since_it_was_opened_is_stale() -> Result<()> {
    let (inst, app) = installation().await?;
    let path = "applications/helpdesk/views/list.json";
    let deployer = inst.deployer();
    let export = deployer.instance_export().await?;
    let opened = registry::get(&export, path).unwrap();

    // Someone else saves the view from its own screen.
    let mut view = sc_viewpattern::load_view(&inst.catalog, app.id, "list")
        .await?
        .unwrap();
    view.description = "Theirs".into();
    sc_viewpattern::save_view(&inst.catalog, &view).await?;

    let mut value = opened.value.clone();
    value["description"] = json!("Mine");
    let edit = Edit {
        path: path.into(),
        value: Some(value),
        version: Some(opened.entry.version.clone()),
    };
    let e = deployer.plan_edit(&edit).await.unwrap_err().to_string();
    assert!(e.contains("stale"), "{e}");

    // A plan made, then live changed before the confirm: the digest is stale.
    let now = deployer.instance_export().await?;
    let reopened = registry::get(&now, path).unwrap();
    let edit = Edit {
        version: Some(reopened.entry.version),
        ..edit
    };
    let planned = deployer.plan_edit(&edit).await?;
    sc_config::set_config(
        &inst.catalog,
        sc_config::email::SMTP_HOST,
        json!("x.example"),
    )
    .await?;
    let e = deployer
        .edit(&edit, &Confirm::defaults(&planned.plan), &by())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("stale"), "{e}");
    let view = sc_viewpattern::load_view(&inst.catalog, app.id, "list")
        .await?
        .unwrap();
    assert_eq!(view.description, "Theirs");
    assert_ne!(version(&reopened.value), opened.entry.version);
    Ok(())
}

#[tokio::test]
async fn new_and_delete() -> Result<()> {
    let (inst, app) = installation().await?;
    let deployer = inst.deployer();
    let path = "applications/helpdesk/pages/home.json";
    let edit = Edit {
        path: path.into(),
        value: Some(json!({
            "name": "home",
            "title": "Home",
            "description": "",
            "layout": {},
            "min_role": 100,
            "attributes": {},
        })),
        version: None,
    };
    let planned = deployer.plan_edit(&edit).await?;
    assert_eq!(planned.plan.operations[0].id, "create:page:helpdesk/home");
    let id = planned.value.as_ref().unwrap()["id"].clone();
    let receipt = deployer
        .edit(
            &Edit {
                value: planned.value.clone(),
                ..edit
            },
            &Confirm::defaults(&planned.plan),
            &by(),
        )
        .await?;
    assert!(receipt.receipt.applied.failed.is_none(), "{receipt:#?}");
    assert_eq!(receipt.stored.as_ref().unwrap()["id"], id);
    assert!(
        sc_viewpattern::list_pages(&inst.catalog, app.id)
            .await?
            .iter()
            .any(|p| p.name == "home")
    );

    // Deleting it is a metadata delete, ticked.
    let delete = Edit {
        path: path.into(),
        value: None,
        version: receipt.stored_version.clone(),
    };
    let planned = deployer.plan_edit(&delete).await?;
    assert_eq!(planned.plan.operations[0].class, OpClass::MetadataDelete);
    assert!(planned.plan.operations[0].ticked);
    let receipt = deployer
        .edit(&delete, &Confirm::defaults(&planned.plan), &by())
        .await?;
    assert!(receipt.stored.is_none());
    assert!(
        sc_viewpattern::list_pages(&inst.catalog, app.id)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_store_directory_and_a_build_host_are_saved_as_typed() -> Result<()> {
    let (inst, _) = installation().await?;
    let moved = inst.dir.join("moved-code");
    std::fs::create_dir_all(&moved).unwrap();
    let to = moved.to_string_lossy().into_owned();
    save(
        &inst,
        "file-stores/code.json",
        |v| v["config"]["path"] = json!(to),
        |c| c,
    )
    .await?;
    let stores = sc_catalog::list_file_stores(&inst.catalog).await?;
    let code = stores.iter().find(|s| s.name == "code").unwrap();
    assert_eq!(code.setting(sc_files::CFG_PATH), Some(to.as_str()));

    save(
        &inst,
        "build-hosts/mac-mini.json",
        |v| v["name"] = json!("mac-studio"),
        |c| c,
    )
    .await?;
    let hosts = sc_app::list_build_hosts(&inst.catalog).await?;
    assert_eq!(hosts[0].name, "mac-studio");
    Ok(())
}
