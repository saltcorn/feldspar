//! Default values on `NOT NULL` fields: a JavaScript expression an insert that
//! leaves the field out (or sends null) takes its value from, with `user` (the
//! writing user) and the row's other values in scope.
//!
//! Through the real formula engine, because the expression is reified: it runs
//! before the `INSERT`, and the ownership check sees the row with its defaults
//! in — so an `owner` defaulted to the writer is what lets the writer write.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_api::rows::Executor;
use sc_api::schema_edit::{self, ApplyOptions, FieldSettings, FieldSpec, Operation, TableSettings};
use sc_auth::User;
use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_query::Value;
use sc_test_harness::TestDb;
use serde_json::json;

async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog.set_formula_evaluator(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>)?;
    Ok(catalog)
}

fn field(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    }
}

fn required_with_default(name: &str, type_name: &str, expression: &str) -> FieldSpec {
    FieldSpec {
        required: true,
        default_expression: Some(expression.into()),
        ..field(name, type_name)
    }
}

/// `notes`, owned by whoever's email is in `owner`, which defaults to the
/// writer's; and a `slug` computed from the row's own title.
async fn create_notes(catalog: &Catalog, ownership: &str) -> Result<()> {
    schema_edit::apply(
        catalog,
        &[Operation::CreateTable {
            name: "notes".into(),
            database: String::new(),
            settings: TableSettings {
                min_role_read: Some(1),
                min_role_write: Some(1),
                ownership_formula: Some(ownership.into()),
                ..TableSettings::default()
            },
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                FieldSpec {
                    required: true,
                    ..field("title", "text")
                },
                required_with_default("owner", "text", "user.email"),
                required_with_default("slug", "text", "title.toLowerCase().split(' ').join('-')"),
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;
    Ok(())
}

fn writer(email: &str) -> User {
    let mut user = User::new(uuid::Uuid::new_v4(), 40).unwrap();
    user.extra.insert("email".into(), Value::Text(email.into()));
    user
}

#[tokio::test]
async fn a_left_out_field_takes_its_default_from_the_user_and_the_row() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    create_notes(&catalog, "owner === user.email").await?;
    let notes = catalog.require("notes")?;
    assert_eq!(
        notes.field("owner").unwrap().default_expression.as_deref(),
        Some("user.email"),
        "the default survives the overlay round trip"
    );
    assert!(notes.field("owner").unwrap().has_default());

    // A role-40 writer below the table's floors: only the ownership formula
    // lets them write, and it reads the `owner` the default fills in.
    let ada = writer("ada@example.com");
    let evaluator = catalog.formula_evaluator();
    let row = sc_api::insert_row_as(
        &catalog,
        &notes,
        &json!({ "title": "Hello World" }),
        40,
        Some(&ada),
        evaluator.as_ref(),
        &[],
        &Executor::Pooled,
    )
    .await?;
    assert_eq!(row["owner"], json!("ada@example.com"));
    assert_eq!(row["slug"], json!("hello-world"));

    // A value given is kept, and an explicit null is filled like a missing one.
    let row = sc_api::insert_row_as(
        &catalog,
        &notes,
        &json!({ "title": "Second", "owner": "ada@example.com", "slug": null }),
        40,
        Some(&ada),
        evaluator.as_ref(),
        &[],
        &Executor::Pooled,
    )
    .await?;
    assert_eq!(row["slug"], json!("second"));

    // A given owner that is not the writer is still refused.
    let err = sc_api::insert_row_as(
        &catalog,
        &notes,
        &json!({ "title": "Third", "owner": "bob@example.com" }),
        40,
        Some(&ada),
        evaluator.as_ref(),
        &[],
        &Executor::Pooled,
    )
    .await
    .expect_err("not ada's row");
    assert!(err.to_string().contains("outside"), "{err}");

    // No user: `user.email` gives nothing, and the error names the field.
    let err = sc_api::rows::create_row(&catalog, &notes, &json!({ "title": "Fourth" }))
        .await
        .expect_err("nobody to default to");
    assert!(
        err.to_string()
            .contains("default value of `notes.owner` could not be computed"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn a_default_is_refused_on_a_nullable_field_and_when_it_does_not_validate() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    create_notes(&catalog, "true").await?;

    let alter = |field: &str, settings: FieldSettings| Operation::AlterField {
        table: "notes".into(),
        field: field.into(),
        settings,
    };
    let options = ApplyOptions::default();

    // Not on a nullable field: adding one, or making a defaulted field nullable.
    let err = schema_edit::apply(
        &catalog,
        &[Operation::AddField {
            table: "notes".into(),
            field: FieldSpec {
                default_expression: Some("'x'".into()),
                ..field("tag", "text")
            },
        }],
        &options,
    )
    .await
    .expect_err("nullable");
    assert!(err.to_string().contains("only a required"), "{err}");
    let err = schema_edit::apply(
        &catalog,
        &[alter(
            "owner",
            FieldSettings {
                required: Some(false),
                ..FieldSettings::default()
            },
        )],
        &options,
    )
    .await
    .expect_err("would leave a default on a nullable field");
    assert!(err.to_string().contains("only a required"), "{err}");

    // An expression naming a field the table does not have.
    let err = schema_edit::apply(
        &catalog,
        &[alter(
            "slug",
            FieldSettings {
                default_expression: Some("nonesuch + 1".into()),
                ..FieldSettings::default()
            },
        )],
        &options,
    )
    .await
    .expect_err("unknown field");
    assert!(
        err.to_string().contains("default value of `notes.slug`"),
        "{err}"
    );

    // Altering another setting keeps the default; an empty one clears it.
    schema_edit::apply(
        &catalog,
        &[alter(
            "owner",
            FieldSettings {
                label: Some("Owner".into()),
                ..FieldSettings::default()
            },
        )],
        &options,
    )
    .await?;
    let owner = catalog.require("notes")?.field("owner").unwrap().clone();
    assert_eq!(owner.default_expression.as_deref(), Some("user.email"));
    schema_edit::apply(
        &catalog,
        &[alter(
            "owner",
            FieldSettings {
                default_expression: Some(String::new()),
                ..FieldSettings::default()
            },
        )],
        &options,
    )
    .await?;
    let owner = catalog.require("notes")?.field("owner").unwrap().clone();
    assert_eq!(owner.default_expression, None);
    Ok(())
}

/// `tasks`, nullable `owner` with no default, and `formula` as its ownership.
async fn create_tasks(catalog: &Catalog, formula: &str, rls: bool) -> Result<()> {
    schema_edit::apply(
        catalog,
        &[Operation::CreateTable {
            name: "tasks".into(),
            database: String::new(),
            settings: TableSettings {
                min_role_read: Some(1),
                min_role_write: Some(1),
                ownership_formula: Some(formula.into()),
                rls_enabled: Some(rls),
                ..TableSettings::default()
            },
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                field("title", "text"),
                field("owner", "text"),
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_field_the_insert_leaves_out_is_null_to_the_ownership_formula() -> Result<()> {
    // With and without RLS: the runtime check and the `WITH CHECK` policy see
    // the same row — the left-out `owner` as null — so they agree. (`!owner`
    // is not translatable to SQL, so the RLS table spells it as a comparison.)
    for (rls, formula) in [
        (false, "!owner || owner === user.email"),
        (false, "owner === null || owner === user.email"),
        (true, "owner === null || owner === user.email"),
    ] {
        let db = TestDb::new().await?;
        let catalog = setup(&db).await?;
        create_tasks(&catalog, formula, rls).await?;
        let tasks = catalog.require("tasks")?;
        let ada = writer("ada@example.com");
        let evaluator = catalog.formula_evaluator();
        let insert = |body: serde_json::Value| {
            let (catalog, tasks, ada, evaluator) = (&catalog, &tasks, &ada, &evaluator);
            async move {
                sc_api::insert_row_as(
                    catalog,
                    tasks,
                    &body,
                    40,
                    Some(ada),
                    evaluator.as_ref(),
                    &[],
                    &Executor::Pooled,
                )
                .await
            }
        };

        // `owner` left out (and the identity `id`, which the database fills).
        let row = insert(json!({ "title": "unowned" })).await?;
        assert_eq!(row["owner"], json!(null), "rls={rls}");
        assert!(row["id"].is_number(), "rls={rls}");
        insert(json!({ "title": "mine", "owner": "ada@example.com" })).await?;
        let err = insert(json!({ "title": "theirs", "owner": "bob@example.com" }))
            .await
            .expect_err("not ada's row");
        // The policy's refusal surfaces as a not-found (`map_policy_violation`).
        let expected = if rls { "no such row" } else { "outside" };
        assert!(err.to_string().contains(expected), "rls={rls}: {err}");
    }
    Ok(())
}

#[tokio::test]
async fn new_date_is_a_default_for_date_and_timestamp_fields() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    schema_edit::apply(
        &catalog,
        &[Operation::CreateTable {
            name: "events".into(),
            database: String::new(),
            settings: TableSettings::default(),
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                field("title", "text"),
                required_with_default("day", "date", "new Date()"),
                required_with_default("at", "timestamp", "new Date()"),
                required_with_default("fixed", "date", "new Date('2026-03-04T10:00:00Z')"),
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;
    let events = catalog.require("events")?;
    assert_eq!(
        events.field("day").unwrap().default_expression.as_deref(),
        Some("new Date()")
    );

    let before = chrono::Utc::now();
    let row = sc_api::rows::create_row(&catalog, &events, &json!({ "title": "Launch" })).await?;
    let after = chrono::Utc::now();

    let day = row["day"].as_str().unwrap();
    assert!(
        [before.date_naive(), after.date_naive()]
            .iter()
            .any(|d| d.to_string() == day),
        "day {day} is not today"
    );
    let at = chrono::DateTime::parse_from_rfc3339(row["at"].as_str().unwrap()).unwrap();
    assert!(
        at >= before - chrono::Duration::seconds(1) && at <= after + chrono::Duration::seconds(1),
        "at {at} is not now"
    );
    assert_eq!(row["fixed"], json!("2026-03-04"));
    Ok(())
}

#[tokio::test]
async fn a_boolean_literal_is_a_default_for_a_bool_field() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    schema_edit::apply(
        &catalog,
        &[Operation::CreateTable {
            name: "flags".into(),
            database: String::new(),
            settings: TableSettings::default(),
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                field("title", "text"),
                required_with_default("active", "bool", "true"),
                required_with_default("archived", "bool", "false"),
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;
    let flags = catalog.require("flags")?;
    assert_eq!(
        flags
            .field("archived")
            .unwrap()
            .default_expression
            .as_deref(),
        Some("false")
    );

    // Left out: each takes its default — `false` is a value, not a missing one.
    let row = sc_api::rows::create_row(&catalog, &flags, &json!({ "title": "a" })).await?;
    assert_eq!(row["active"], json!(true));
    assert_eq!(row["archived"], json!(false));

    // Given: the body's value wins, including a `false` over a `true` default.
    let row = sc_api::rows::create_row(
        &catalog,
        &flags,
        &json!({ "title": "b", "active": false, "archived": true }),
    )
    .await?;
    assert_eq!(row["active"], json!(false));
    assert_eq!(row["archived"], json!(true));
    Ok(())
}
