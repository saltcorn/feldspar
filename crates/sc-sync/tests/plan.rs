//! A plan made from a live installation's export and validated against its
//! data (S2.3, S2.4): the dry runs, the role check and the git store check
//! that the planner cannot make from trees alone.

use serde_json::json;

use sc_error::Result;
use sc_files::{CFG_BRANCH, CFG_DIR};
use sc_sync::{OpClass, Plan, StructureTree, plan, validate};
use sc_types::Role;

use crate::fixture::{Installation, code_app, entity, git, put};

/// `tree` with the field `field` of `table` changed by `edit`.
fn edit_field(
    tree: &StructureTree,
    table: &str,
    field: &str,
    edit: impl FnOnce(&mut serde_json::Value),
) -> StructureTree {
    let path = format!("tables/{table}.json");
    let mut value = entity(tree, &path).clone();
    let f = value["fields"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["name"] == field)
        .unwrap();
    edit(f);
    let mut out = tree.clone();
    out.insert_json(path, value);
    out
}

async fn validated(inst: &Installation, live: &StructureTree, target: &StructureTree) -> Plan {
    let mut plan = plan(live, target, None);
    validate(&inst.catalog, &mut plan).await.unwrap();
    plan
}

#[tokio::test]
async fn making_a_field_with_nulls_required_blocks_with_the_row_count() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title", "status"]).await?;
    inst.sql(
        "INSERT INTO tickets (id, title, status) VALUES \
         (1, 'a', 'open'), (2, 'b', NULL), (3, 'c', NULL)",
    )
    .await?;
    let live = inst.tree().await?;
    let target = edit_field(&live, "tickets", "status", |f| {
        f["required"] = json!(true);
        f["nullable"] = json!(false);
    });

    let plan = validated(&inst, &live, &target).await;
    let op = plan.op("alter_field:table:tickets#status").unwrap();
    assert_eq!(op.class, OpClass::CanFail);
    assert!(op.ticked);
    assert_eq!(plan.blockers.len(), 1, "{:#?}", plan.blockers);
    let blocker = &plan.blockers[0];
    assert_eq!(blocker.op.as_deref(), Some(op.id.as_str()));
    assert!(
        blocker.message.contains("2 rows have no value"),
        "{}",
        blocker.message
    );

    // The dry run wrote nothing.
    assert_eq!(inst.tree().await?, live);

    // With the values filled in, the same change goes through.
    inst.sql("UPDATE tickets SET status = 'open'").await?;
    let plan = validated(&inst, &live, &target).await;
    assert!(plan.blockers.is_empty(), "{:#?}", plan.blockers);
    Ok(())
}

#[tokio::test]
async fn an_sql_type_change_blocks_with_the_add_migrate_drop_suggestion() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title", "status"]).await?;
    let live = inst.tree().await?;
    let target = edit_field(&live, "tickets", "status", |f| {
        f["sql_type"] = json!("int8");
        f["type"] = json!("integer");
    });
    let plan = validated(&inst, &live, &target).await;
    let op = plan.op("alter_field:table:tickets#status").unwrap();
    assert_eq!(op.class, OpClass::Unsupported);
    assert!(!op.ticked);
    assert_eq!(plan.blockers.len(), 1, "{:#?}", plan.blockers);
    let message = &plan.blockers[0].message;
    assert!(message.contains("`tickets.status`"), "{message}");
    assert!(message.contains("from `text` to `int8`"), "{message}");
    assert!(
        message.contains("add a new field, copy the values across, then drop the old one"),
        "{message}"
    );
    Ok(())
}

#[tokio::test]
async fn a_rename_and_a_drop_from_a_real_export() -> Result<()> {
    // Step 4 of the definition of done, as far as the planner goes: a field
    // rename, a ticked metadata delete, an unticked drop of a table with rows.
    let inst = Installation::new().await?;
    inst.table("tickets", &["title", "status"]).await?;
    inst.table("old_notes", &["body"]).await?;
    inst.sql("INSERT INTO old_notes (id, body) VALUES (1, 'kept')")
        .await?;
    inst.local_store("code", |d| d).await?;
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = inst.application(body).await?;
    for (name, pattern) in [("list", "List"), ("edit", "Edit")] {
        sc_viewpattern::save_view(
            &inst.catalog,
            &sc_viewpattern::View::new(app.id, name, pattern, "tickets"),
        )
        .await?;
    }
    let live = inst.tree().await?;

    let mut target = edit_field(&live, "tickets", "status", |f| f["name"] = json!("state"));
    target.remove("applications/helpdesk/views/edit.json");
    target.remove("tables/old_notes.json");

    let plan = validated(&inst, &live, &target).await;
    let summary: Vec<(&str, &str, OpClass, bool)> = plan
        .operations
        .iter()
        .map(|op| (op.action, op.entity.as_str(), op.class, op.ticked))
        .collect();
    assert_eq!(
        summary,
        [
            ("rename_field", "table:tickets", OpClass::Safe, true),
            (
                "delete",
                "view:helpdesk/edit",
                OpClass::MetadataDelete,
                true
            ),
            (
                "drop_table",
                "table:old_notes",
                OpClass::DataDestructive,
                false
            ),
        ]
    );
    assert!(plan.blockers.is_empty(), "{:#?}", plan.blockers);
    Ok(())
}

#[tokio::test]
async fn deleting_a_role_users_hold_is_data_destructive() -> Result<()> {
    let inst = Installation::new().await?;
    let catalog = &inst.catalog;
    sc_auth::save_role(catalog, &sc_auth::RoleDef::new(Role::of(40), "staff")).await?;
    sc_auth::save_role(catalog, &sc_auth::RoleDef::new(Role::of(50), "unused")).await?;
    sc_auth::create_user(
        catalog,
        "someone@example.com",
        "a-long-password-1",
        Role::of(40),
    )
    .await?;
    let live = inst.tree().await?;
    let mut roles = entity(&live, "roles.json").clone();
    roles
        .as_array_mut()
        .unwrap()
        .retain(|r| r["role"] != 40 && r["role"] != 50);
    let mut target = live.clone();
    target.insert_json("roles.json", roles);

    let plan = validated(&inst, &live, &target).await;
    let held = plan.op("delete:role:40").unwrap();
    assert_eq!(held.class, OpClass::DataDestructive);
    assert!(!held.ticked);
    assert!(held.summary.contains("1 user holds"), "{}", held.summary);
    let unused = plan.op("delete:role:50").unwrap();
    assert_eq!(unused.class, OpClass::MetadataDelete);
    assert!(unused.ticked);
    Ok(())
}

#[tokio::test]
async fn a_pin_change_on_a_store_with_unpushed_commits_blocks_and_names_it() -> Result<()> {
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
    let v1 = git(&checkout, &["rev-parse", "HEAD"]);
    git(
        &checkout,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&checkout, &["push", "-q", "-u", "origin", "main"]);

    let def = sc_files::FileStoreDef::git("web", remote.to_string_lossy())
        .with(CFG_DIR, checkout.to_string_lossy())
        .with(CFG_BRANCH, "main");
    sc_catalog::save_file_store(&inst.catalog, &def).await?;

    // A commit here that the remote does not have.
    put(&checkout, "index.html", b"<h1>v2</h1>");
    git(&checkout, &["commit", "-q", "-am", "v2"]);
    let live = inst.tree().await?;
    assert_ne!(entity(&live, "file-stores/web.json")["commit"], json!(v1));
    let mut store = entity(&live, "file-stores/web.json").clone();
    store["commit"] = json!(v1);
    let mut target = live.clone();
    target.insert_json("file-stores/web.json", store);

    let plan = validated(&inst, &live, &target).await;
    assert_eq!(plan.blockers.len(), 1, "{:#?}", plan.blockers);
    let message = &plan.blockers[0].message;
    assert!(message.contains("the git store `web`"), "{message}");
    assert!(message.contains("1 commit not pushed"), "{message}");

    // Uncommitted work is named too.
    put(&checkout, "notes.txt", b"draft");
    let plan = validated(&inst, &live, &target).await;
    let message = &plan.blockers[0].message;
    assert!(message.contains("1 uncommitted change"), "{message}");
    std::fs::remove_file(checkout.join("notes.txt")).unwrap();

    // Once pushed, the pin can move.
    git(&checkout, &["push", "-q"]);
    let plan = validated(&inst, &live, &target).await;
    assert!(plan.blockers.is_empty(), "{:#?}", plan.blockers);
    Ok(())
}
