//! Schema edits a module makes (TODO ST1.2, ST1.3): the `schema` ask, driven
//! directly — each grant on and off, a prefix escape in each kind of operation,
//! a key to `users`, this server's own tables, a batch refused whole, and the
//! tables a batch creates connected to the calling application.
//!
//! Who may reach the ask at all (an admin-only trigger, not one an application
//! opened to its users) is the dispatcher's half, and `sc-server`'s
//! `bundled_schema` checks it end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_module::{Ask, ModuleSchema, SchemaGrant};
use sc_test_harness::TestDb;
use serde_json::{Value, json};

const MODULE: &str = "@feldspar/schema";

async fn catalog(db: &TestDb) -> Arc<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Arc::new(
        Catalog::init(driver as Arc<dyn DatabaseDriver>)
            .await
            .unwrap(),
    );
    db.on_teardown(cat.teardown_hook());
    sc_auth::bootstrap(&cat).await.unwrap();
    sc_app::bootstrap(&cat).await.unwrap();
    bootstrap_table_meta(&cat).await.unwrap();
    bootstrap_field_meta(&cat).await.unwrap();
    cat
}

/// A `code` framework's settings, as the admin UI's form posts them.
fn code_framework() -> sc_app::FrameworkRef {
    sc_app::FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts")
}

fn grant(create: bool, edit: bool, drop: bool) -> SchemaGrant {
    SchemaGrant {
        create,
        edit,
        drop,
        prefix: "ut_".to_owned(),
    }
}

/// Ask for `operations` under `grant`, answering the answer or the refusal.
async fn apply(cat: &Catalog, grant: &SchemaGrant, operations: Value) -> Result<Value, String> {
    ModuleSchema::new(cat, MODULE, grant)
        .ask(json!({ "op": "apply", "operations": operations }))
        .await
        .map_err(|e| e.to_string())
}

fn create(table: &str) -> Value {
    json!({ "op": "create_table", "table": table,
            "fields": [{ "field": "id", "type": "int", "primary_key": true }] })
}

#[tokio::test]
async fn each_grant_allows_its_operations_and_nothing_else() {
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await;

    // Create only: a table, yes; a field on it, no.
    let only_create = grant(true, false, false);
    let answer = apply(&cat, &only_create, json!([create("ut_notes")]))
        .await
        .unwrap();
    assert_eq!(answer["tables_created"], json!(["ut_notes"]), "{answer}");
    assert!(answer["generation"].as_u64().is_some(), "{answer}");
    // With no application, nothing is connected, and the answer says so.
    assert_eq!(answer["connected"], Value::Null);
    let notes = answer["notes"].to_string();
    assert!(
        notes.contains("not connected to any application"),
        "{notes}"
    );
    assert!(notes.contains("next run"), "{notes}");
    assert!(cat.get("ut_notes").unwrap().is_some());

    let add = json!([{ "op": "add_field", "table": "ut_notes", "field": "body", "type": "text" }]);
    let refused = apply(&cat, &only_create, add.clone()).await.unwrap_err();
    assert!(refused.contains("may not add a field"), "{refused}");
    assert!(refused.contains("`edit`"), "{refused}");

    // Edit: the field, a setting, a rename of a field, a constraint.
    let edit = grant(false, true, false);
    apply(&cat, &edit, add).await.unwrap();
    apply(
        &cat,
        &edit,
        json!([
            { "op": "alter_table", "table": "ut_notes", "label": "Notes" },
            { "op": "alter_field", "table": "ut_notes", "field": "body", "label": "Body" },
        ]),
    )
    .await
    .unwrap();
    let refused = apply(&cat, &edit, json!([create("ut_more")]))
        .await
        .unwrap_err();
    assert!(refused.contains("may not create a table"), "{refused}");

    // Drop is off unless granted: neither a field nor a table.
    for op in [
        json!({ "op": "drop_field", "table": "ut_notes", "field": "body" }),
        json!({ "op": "drop_table", "table": "ut_notes" }),
    ] {
        let refused = apply(&cat, &grant(true, true, false), json!([op]))
            .await
            .unwrap_err();
        assert!(refused.contains("may not drop"), "{refused}");
    }
    assert!(
        cat.get("ut_notes")
            .unwrap()
            .unwrap()
            .field("body")
            .is_some()
    );
    let drop = grant(false, false, true);
    apply(
        &cat,
        &drop,
        json!([{ "op": "drop_field", "table": "ut_notes", "field": "body" }]),
    )
    .await
    .unwrap();
    let answer = apply(
        &cat,
        &drop,
        json!([{ "op": "drop_table", "table": "ut_notes" }]),
    )
    .await
    .unwrap();
    assert_eq!(answer["tables_dropped"], json!(["ut_notes"]));
    assert!(cat.get("ut_notes").unwrap().is_none());
}

#[tokio::test]
async fn every_table_an_operation_changes_must_start_with_the_prefix() {
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await;
    let all = grant(true, true, true);
    apply(&cat, &all, json!([create("ut_books")]))
        .await
        .unwrap();
    // A table outside the prefix, made by an admin.
    sc_api::schema_edit::apply(
        &cat,
        &[sc_api::schema_edit::Operation::CreateTable {
            name: "books".into(),
            database: String::new(),
            settings: Default::default(),
            fields: vec![sc_api::schema_edit::FieldSpec {
                name: "id".into(),
                type_name: "int".into(),
                primary_key: true,
                ..Default::default()
            }],
        }],
        &sc_api::schema_edit::ApplyOptions::default(),
    )
    .await
    .unwrap();

    for (what, op) in [
        ("create", create("books2")),
        (
            "alter_table",
            json!({ "op": "alter_table", "table": "books", "label": "B" }),
        ),
        (
            "add_field",
            json!({ "op": "add_field", "table": "books", "field": "x", "type": "int" }),
        ),
        (
            "alter_field",
            json!({ "op": "alter_field", "table": "books", "field": "id", "label": "I" }),
        ),
        (
            "drop_field",
            json!({ "op": "drop_field", "table": "books", "field": "id" }),
        ),
        (
            "drop_table",
            json!({ "op": "drop_table", "table": "books" }),
        ),
        (
            "rename_table from",
            json!({ "op": "rename_table", "table": "books", "rename_to": "ut_b" }),
        ),
        (
            "rename_table to",
            json!({ "op": "rename_table", "table": "ut_books", "rename_to": "books_x" }),
        ),
        (
            "add_constraint",
            json!({ "op": "add_constraint", "table": "books",
                                    "constraint": { "type": "index", "fields": ["id"] } }),
        ),
        (
            "drop_constraint",
            json!({ "op": "drop_constraint", "table": "books",
                                     "constraint": { "name": "books_pkey" } }),
        ),
        // Exactly the prefix is not a table under it.
        ("the bare prefix", create("ut_")),
    ] {
        let refused = apply(&cat, &all, json!([op])).await.unwrap_err();
        assert!(
            refused.contains("may only change tables whose names start with `ut_`"),
            "{what}: {refused}"
        );
    }
    assert!(cat.get("books").unwrap().is_some());
    assert!(cat.get("ut_books").unwrap().is_some(), "not renamed");

    // A key may point at a table outside the prefix: `users`, or the app's own.
    let answer = apply(
        &cat,
        &all,
        json!([{ "op": "create_table", "table": "ut_loans", "fields": [
            { "field": "id", "type": "int", "primary_key": true },
            { "field": "borrower", "references": "users" },
            { "field": "book", "references": "books" },
        ] }]),
    )
    .await
    .unwrap();
    assert_eq!(answer["tables_created"], json!(["ut_loans"]), "{answer}");
}

#[tokio::test]
async fn this_servers_own_tables_are_refused_whatever_the_prefix() {
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await;
    // A prefix that happens to match `users`' neighbours is still bounded.
    let role = SchemaGrant {
        prefix: "role_".into(),
        ..grant(true, true, true)
    };
    let refused = apply(
        &cat,
        &role,
        json!([{ "op": "add_field", "table": "role_grants", "field": "x", "type": "int" }]),
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("one of this server's own tables"),
        "{refused}"
    );
    let refused = apply(
        &cat,
        &grant(true, true, true),
        json!([{ "op": "alter_table", "table": "users", "label": "People" }]),
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("one of this server's own tables"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_refused_operation_refuses_the_whole_batch() {
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await;
    let all = grant(true, true, true);
    // The second operation escapes the prefix: the first is not applied.
    let refused = apply(&cat, &all, json!([create("ut_first"), create("second")]))
        .await
        .unwrap_err();
    assert!(refused.contains("operation 1"), "{refused}");
    assert!(cat.get("ut_first").unwrap().is_none());

    // And one the schema editor refuses (a key to no table) rolls back too.
    let refused = apply(
        &cat,
        &all,
        json!([
            create("ut_first"),
            { "op": "add_field", "table": "ut_first", "field": "x", "references": "ut_nowhere" },
        ]),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("ut_nowhere"), "{refused}");
    assert!(cat.get("ut_first").unwrap().is_none());

    // A dry run checks and applies nothing.
    let answer = ModuleSchema::new(&cat, MODULE, &all)
        .ask(json!({ "op": "apply", "operations": [create("ut_first")], "dry_run": true }))
        .await
        .unwrap();
    assert_eq!(answer["dry_run"], json!(true), "{answer}");
    assert!(cat.get("ut_first").unwrap().is_none());
}

/// TODO ST1.3: a table a batch creates is connected to the calling application
/// (and not to another), a table it drops is disconnected, and the row API is
/// not opened.
#[tokio::test]
async fn created_tables_are_connected_to_the_calling_application() {
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await;
    let wiki = sc_app::save_application(
        &cat,
        &sc_app::Application::new("Wiki", "wiki", code_framework()),
    )
    .await
    .unwrap();
    let other = sc_app::save_application(
        &cat,
        &sc_app::Application::new("Other", "other", code_framework()),
    )
    .await
    .unwrap();
    let all = grant(true, true, true);
    let schema = || ModuleSchema::new(&cat, MODULE, &all).for_application(Some(wiki.id.0));

    let answer = schema()
        .ask(json!({ "op": "apply", "operations": [create("ut_pages")] }))
        .await
        .unwrap();
    assert_eq!(
        answer["connected"]["application"],
        json!(wiki.id.0),
        "{answer}"
    );
    assert_eq!(answer["connected"]["added"], json!(["ut_pages"]));
    // No server serves applications here, and the answer says so.
    assert!(
        answer["notes"].to_string().contains("next start"),
        "{answer}"
    );
    let stored = sc_app::load_application(&cat, wiki.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.can_access_table("ut_pages"));
    assert!(
        !stored.serves_table_api("ut_pages"),
        "the row API stays off"
    );
    let stored = sc_app::load_application(&cat, other.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.can_access_table("ut_pages"));

    let answer = schema()
        .ask(json!({ "op": "apply", "operations": [{ "op": "drop_table", "table": "ut_pages" }] }))
        .await
        .unwrap();
    assert_eq!(
        answer["connected"]["removed"],
        json!(["ut_pages"]),
        "{answer}"
    );
    let stored = sc_app::load_application(&cat, wiki.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.can_access_table("ut_pages"));
}
