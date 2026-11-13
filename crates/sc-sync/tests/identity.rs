//! Identity (S1.3, APP_SYNC_PROPOSAL §3.2): every table and field has an id,
//! a column added behind Feldspar's back gets one on its first export and
//! keeps it, and an entity created with a given id keeps that one.

use serde_json::{Value as Json, json};
use uuid::Uuid;

use sc_api::schema_edit::{self, ApplyOptions, GivenIds, Operation, TableSettings};
use sc_error::Result;

use crate::fixture::{Installation, code_app, entity, text_field};

fn field_id(table: &Json, name: &str) -> Json {
    table["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!(name))
        .unwrap_or_else(|| panic!("no field `{name}` in {table}"))["id"]
        .clone()
}

#[tokio::test]
async fn a_column_added_by_raw_ddl_gets_an_id_on_first_export_and_keeps_it() -> Result<()> {
    let inst = Installation::new().await?;
    // A whole table nobody described, and a column added to it by hand.
    inst.sql("CREATE TABLE notes (id serial PRIMARY KEY, body text)")
        .await?;
    inst.sql("ALTER TABLE notes ADD COLUMN pinned boolean")
        .await?;
    assert!(inst.catalog.require("notes")?.overlay.is_none());

    let first = inst.export().await?;
    // One table row and three field rows were made to hold the ids.
    assert_eq!(first.overlays_created, 4);
    let notes = entity(&first.tree, "tables/notes.json").clone();
    assert!(notes["id"].is_string(), "{notes}");
    let pinned = field_id(&notes, "pinned");
    assert!(pinned.is_string(), "{notes}");

    // The next export writes nothing and gives the same ids.
    let second = inst.export().await?;
    assert_eq!(second.overlays_created, 0);
    assert_eq!(entity(&second.tree, "tables/notes.json"), &notes);

    // Another hand-made column gets its own id, and the others keep theirs.
    inst.sql("ALTER TABLE notes ADD COLUMN colour text").await?;
    let third = inst.export().await?;
    assert_eq!(third.overlays_created, 1);
    let after = entity(&third.tree, "tables/notes.json");
    assert_eq!(after["id"], notes["id"]);
    assert_eq!(field_id(after, "pinned"), pinned);
    assert!(field_id(after, "colour").is_string());
    assert_ne!(field_id(after, "colour"), pinned);

    // Making the rows changed nothing about the table: it is still admin-only,
    // and the hand-made columns are still the plain columns they were.
    let table = inst.catalog.require("notes")?;
    assert_eq!(table.access, sc_catalog::AccessRules::default());
    let pinned = table.field("pinned").unwrap();
    assert_eq!(pinned.kind, sc_catalog::DataFieldKind::Plain);
    Ok(())
}

#[tokio::test]
async fn a_key_found_in_the_database_stays_a_key_when_its_row_is_made() -> Result<()> {
    let inst = Installation::new().await?;
    inst.sql("CREATE TABLE owners (id serial PRIMARY KEY)")
        .await?;
    inst.sql("CREATE TABLE pets (id serial PRIMARY KEY, owner int REFERENCES owners(id))")
        .await?;
    let before = inst
        .catalog
        .require("pets")?
        .field("owner")
        .unwrap()
        .kind
        .clone();
    assert!(
        matches!(before, sc_catalog::DataFieldKind::Key { .. }),
        "{before:?}"
    );
    inst.export().await?;
    let after = inst
        .catalog
        .require("pets")?
        .field("owner")
        .unwrap()
        .kind
        .clone();
    assert_eq!(after, before);
    Ok(())
}

#[tokio::test]
async fn a_table_and_field_created_with_given_ids_keep_them() -> Result<()> {
    let inst = Installation::new().await?;
    let table_id = Uuid::new_v4();
    let title_id = Uuid::new_v4();
    let later_id = Uuid::new_v4();
    let mut ids = GivenIds::default();
    ids.tables.insert("tickets".into(), table_id);
    ids.fields
        .insert(("tickets".into(), "title".into()), title_id);
    ids.fields
        .insert(("tickets".into(), "later".into()), later_id);
    let options = ApplyOptions {
        ids,
        ..ApplyOptions::default()
    };
    // No settings and a plain field: without a given id neither would have an
    // overlay row at all.
    schema_edit::apply(
        &inst.catalog,
        &[Operation::CreateTable {
            name: "tickets".into(),
            database: String::new(),
            settings: TableSettings::default(),
            fields: vec![text_field("title"), text_field("status")],
        }],
        &options,
    )
    .await?;
    schema_edit::apply(
        &inst.catalog,
        &[Operation::AddField {
            table: "tickets".into(),
            field: text_field("later"),
        }],
        &options,
    )
    .await?;
    assert_eq!(
        inst.catalog.require("tickets")?.overlay,
        Some(sc_catalog::TableMetaId(table_id))
    );

    let tree = inst.tree().await?;
    let tickets = entity(&tree, "tables/tickets.json");
    assert_eq!(tickets["id"], json!(table_id.to_string()));
    assert_eq!(field_id(tickets, "title"), json!(title_id.to_string()));
    assert_eq!(field_id(tickets, "later"), json!(later_id.to_string()));
    // A field with no given id still gets one — a fresh one.
    let status = field_id(tickets, "status");
    assert!(status.is_string());
    assert_ne!(status, json!(title_id.to_string()));
    Ok(())
}

#[tokio::test]
async fn the_other_definitions_keep_a_given_id() -> Result<()> {
    let inst = Installation::new().await?;
    inst.table("tickets", &["title"]).await?;
    inst.local_store("code", |d| d).await?;

    // Each kind's create path is its codec, which takes the id.
    let app_id = sc_app::AppId::new();
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = sc_backup::codec::application_from_body(app_id, &body)?;
    let app = sc_app::save_application(&inst.catalog, &app).await?;
    assert_eq!(app.id, app_id);

    let view_id = sc_viewpattern::ViewId::new();
    let view = sc_backup::codec::view_from_body(
        view_id,
        app.id,
        &json!({ "name": "list", "viewpattern": "List", "table_name": "tickets",
                 "configuration": { "columns": [] } }),
    )?;
    sc_viewpattern::save_view(&inst.catalog, &view).await?;

    let trigger_id = sc_action::TriggerId::new();
    let trigger = sc_backup::codec::trigger_from_body(
        trigger_id,
        &json!({ "name": "notify", "when": "insert", "channel": "tickets",
                 "action": "send_email",
                 "configuration": { "to": "a@example.com", "subject": "s", "text": "t" } }),
    )?;
    sc_action::save_trigger(&inst.catalog, &inst.registries.actions, &trigger).await?;

    let tree = inst.tree().await?;
    assert_eq!(
        entity(&tree, "applications/helpdesk/application.json")["id"],
        json!(app_id.to_string())
    );
    assert_eq!(
        entity(&tree, "applications/helpdesk/views/list.json")["id"],
        json!(view_id.0.to_string())
    );
    assert_eq!(
        entity(&tree, "triggers/notify.json")["id"],
        json!(trigger_id.0.to_string())
    );
    Ok(())
}
