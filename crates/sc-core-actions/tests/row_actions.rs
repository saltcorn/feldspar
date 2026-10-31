//! The elementary row-writing actions against a real database (TODO Phase 3).
//!
//! What is pinned here — each of these is a decision the actions would otherwise
//! be free to get wrong quietly:
//!
//! - `insert_row` computes its fields from the event (`row.x`, `old.x`, `user.x`)
//!   and inserts them **through the `rows` layer**, so the target table's type
//!   coercion applies and a value the column cannot hold is refused by field name;
//! - `old` on an insert is in scope and *null*, not an evaluation error;
//! - the `where` predicate selects the same rows twice: once translated into SQL,
//!   once through the reified evaluator (an untranslatable spelling of the same
//!   rule) — the parity property doing production work;
//! - the **scope rule**: with a field name shared between the event's table and
//!   the target, `status` is the target row's and `row.status` is the event's, and
//!   they select different rows;
//! - `update_rows` assignments read the row they are replacing (`count + 1`) and
//!   touch nothing the predicate did not select;
//! - `delete_rows` deletes exactly the selected rows;
//! - and every way the configuration can be wrong is refused **on save** by
//!   `validate_trigger`, naming what the admin has to fix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_catalog::Authority;
use sc_types::Role;
use std::sync::Arc;

use sc_action::{ActionContext, Event, EventKind, Trigger, validate_trigger};
use sc_api::rows;
use sc_catalog::{
    Catalog, Principal, TableMeta, bootstrap_table_meta, enable_rls, save_table_meta,
};
use sc_core_actions::builtin_actions;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// Two tables that **share a field name** (`books.status` and `tasks.status`), so
/// the scope rule has something to be wrong about, plus an audit table with a
/// generated key and a keyless table no row can be addressed in.
const SCHEMA: &str = "
    CREATE TABLE books (id bigint primary key, title text, status text, pages bigint);
    CREATE TABLE tasks (id bigint primary key, status text, count bigint);
    CREATE TABLE audit (id bigserial primary key, what text, was text,
        who text, at_pages bigint);
    CREATE TABLE keyless (a bigint);
    INSERT INTO books VALUES (1, 'A Book', 'done', 100);
    INSERT INTO tasks VALUES (1, 'draft', 0), (2, 'done', 0), (3, 'draft', 5);
";

async fn setup(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

fn engine() -> Arc<dyn JsEvaluator> {
    Arc::new(DenoEvaluator::new())
}

/// A configuration from `(key, value)` pairs.
fn config(entries: &[(&str, Json)]) -> Attrs {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// The event the actions in these tests run for: an update of `books` row 1,
/// caused by an admin. Its `row.status` is `done` and its `old.status` is `draft`
/// — the two values the scope tests select on.
fn book_update() -> Event {
    Event::new(EventKind::Update)
        .on("books")
        .row(json!({ "id": 1, "title": "A Book", "status": "done", "pages": 100 }))
        .old_row(json!({ "id": 1, "title": "Draft Title", "status": "draft", "pages": 80 }))
        .caller(Role::ADMIN, Some(json!({ "email": "admin@example.com" })))
}

/// Run a named built-in action for `event` with `config`, through the registry —
/// the path a firing trigger takes.
async fn run(
    catalog: &Catalog,
    action: &str,
    event: &Event,
    config: &Attrs,
    engine: &Arc<dyn JsEvaluator>,
) -> Result<Json> {
    let registry = builtin_actions()?;
    let action = registry.require(action)?.clone();
    let mut ctx =
        ActionContext::new(catalog, event, config, "on-books-update").with_evaluator(engine);
    action.run(&mut ctx).await
}

/// Every row of a table as JSON, ordered by primary key, for assertions.
async fn all_rows(catalog: &Catalog, table: &str) -> Result<Vec<Json>> {
    let table = catalog.require(table)?;
    let mut rows: Vec<Json> = rows::list_rows(catalog, &table)
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default();
    rows.sort_by_key(|r| r["id"].as_i64().unwrap_or_default());
    Ok(rows)
}

#[tokio::test]
async fn insert_row_computes_its_fields_from_the_event() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let engine = engine();

    let cfg = config(&[
        ("table", json!("audit")),
        (
            "values",
            json!({
                "what": "row.title",
                "was": "old.title",
                "who": "user.email",
                "at_pages": "row.pages * 2",
            }),
        ),
    ]);
    let inserted = run(&catalog, "insert_row", &book_update(), &cfg, &engine).await?;

    // The action's result is the inserted row, key and all — what a workflow step
    // needs to refer to what it just created.
    assert_eq!(inserted["what"], json!("A Book"));
    assert_eq!(inserted["was"], json!("Draft Title"));
    assert_eq!(inserted["who"], json!("admin@example.com"));
    assert_eq!(inserted["at_pages"], json!(200));
    assert!(inserted["id"].is_number(), "generated key: {inserted}");

    // And it is really in the table.
    let audit = all_rows(&catalog, "audit").await?;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["what"], json!("A Book"));

    // The same configuration on an *insert* event: `old` is in scope and null, so
    // `old.title` is a null column rather than an evaluation error.
    let insert = Event::new(EventKind::Insert)
        .on("books")
        .row(json!({ "id": 2, "title": "Second", "status": "draft", "pages": 5 }))
        .caller(Role::ADMIN, Some(json!({ "email": "admin@example.com" })));
    let second = run(&catalog, "insert_row", &insert, &cfg, &engine).await?;
    assert_eq!(second["what"], json!("Second"));
    assert_eq!(second["was"], Json::Null);
    assert_eq!(second["at_pages"], json!(10));
    Ok(())
}

#[tokio::test]
async fn an_insert_rows_value_goes_through_the_row_layers_coercion() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    // `at_pages` is a bigint column and the formula computes a string. The write
    // path — not the action — refuses it, naming the field: the point of going
    // through `rows` rather than building an INSERT here.
    let cfg = config(&[
        ("table", json!("audit")),
        ("values", json!({ "at_pages": "row.title" })),
    ]);
    let err = run(&catalog, "insert_row", &book_update(), &cfg, &engine())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("at_pages"), "{msg}");
    assert_eq!(err.kind(), sc_error::ErrorKind::Application);
    // Nothing was written.
    assert!(all_rows(&catalog, "audit").await?.is_empty());
    Ok(())
}

/// The `where` semantics, asserted **twice over the same case**: a predicate that
/// translates to SQL and an untranslatable spelling of the same rule must select
/// the same rows and update them identically.
#[tokio::test]
async fn the_where_predicate_selects_the_same_rows_symbolically_and_reified() -> Result<()> {
    for predicate in [
        // Translatable: the event's row inlines as a literal and the database filters.
        "status === row.status",
        // Untranslatable (a method call on an array): fetch, then the evaluator decides.
        "[status].some(s => s === row.status)",
    ] {
        let db = TestDb::new().await?;
        let catalog = setup(&db).await?;
        let cfg = config(&[
            ("table", json!("tasks")),
            ("where", json!(predicate)),
            ("assignments", json!({ "count": "count + 1" })),
        ]);
        let result = run(&catalog, "update_rows", &book_update(), &cfg, &engine()).await?;

        // `row.status` is `done`, so only task 2 is selected — by either strategy.
        assert_eq!(result["updated"], json!(1), "{predicate}");
        assert_eq!(result["ids"], json!([2]), "{predicate}");
        let tasks = all_rows(&catalog, "tasks").await?;
        let counts: Vec<Json> = tasks.iter().map(|t| t["count"].clone()).collect();
        // The assignment read the row it replaced (`count + 1`), and the rows the
        // predicate did not select were left alone.
        assert_eq!(counts, vec![json!(0), json!(1), json!(5)], "{predicate}");
    }
    Ok(())
}

/// The scope rule, pinned where it can actually bite: `tasks.status` and
/// `books.status` are both called `status`, and the two formulas differ only by
/// the `row.` prefix.
#[tokio::test]
async fn a_bare_field_is_the_target_rows_and_row_is_the_events() -> Result<()> {
    let cases = [
        // The target table's own field: the two draft tasks.
        ("status === \"draft\"", json!([1, 3])),
        // The event's row (`books.status` = `done`): the one done task.
        ("status === row.status", json!([2])),
        // Both at once, and they are different fields of different rows.
        ("status !== row.status", json!([1, 3])),
    ];
    for (predicate, expected) in cases {
        let db = TestDb::new().await?;
        let catalog = setup(&db).await?;
        let cfg = config(&[
            ("table", json!("tasks")),
            ("where", json!(predicate)),
            ("assignments", json!({ "count": "count + 100" })),
        ]);
        let result = run(&catalog, "update_rows", &book_update(), &cfg, &engine()).await?;
        assert_eq!(result["ids"], expected, "{predicate}");
    }
    Ok(())
}

#[tokio::test]
async fn delete_rows_deletes_exactly_the_selected_rows() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    // Deliberately the reified path — `delete_rows` shares one `where`
    // implementation with `update_rows`, and this is the half a delete exercises.
    let cfg = config(&[
        ("table", json!("tasks")),
        ("where", json!("[status].some(s => s === old.status)")),
    ]);
    let result = run(&catalog, "delete_rows", &book_update(), &cfg, &engine()).await?;

    // `old.status` is `draft`: tasks 1 and 3 go, task 2 stays.
    assert_eq!(result["deleted"], json!(2));
    assert_eq!(result["ids"], json!([1, 3]));
    let remaining: Vec<Json> = all_rows(&catalog, "tasks")
        .await?
        .iter()
        .map(|t| t["id"].clone())
        .collect();
    assert_eq!(remaining, vec![json!(2)]);
    Ok(())
}

#[tokio::test]
async fn a_predicate_matching_nothing_writes_nothing_and_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let cfg = config(&[
        ("table", json!("tasks")),
        ("where", json!("status === \"nonesuch\"")),
    ]);
    let result = run(&catalog, "delete_rows", &book_update(), &cfg, &engine()).await?;
    assert_eq!(result["deleted"], json!(0));
    assert_eq!(all_rows(&catalog, "tasks").await?.len(), 3);
    Ok(())
}

/// Every way an action's configuration can be wrong, refused **on save** — the
/// check `save_trigger` runs and the live set re-runs on load, so a broken action
/// configuration is reported to the admin instead of discovered when it fires.
#[tokio::test]
async fn a_broken_configuration_is_refused_on_save_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let registry = builtin_actions()?;

    /// One refusal: the action, the configuration, and the words the message must
    /// contain.
    type Case<'a> = (&'a str, &'a [(&'a str, Json)], &'a [&'a str]);

    let cases: &[Case<'_>] = &[
        // The target table has to exist.
        (
            "insert_row",
            &[("table", json!("nope")), ("values", json!({ "what": "1" }))],
            &["nope"],
        ),
        // …and have a field of that name…
        (
            "insert_row",
            &[
                ("table", json!("audit")),
                ("values", json!({ "nonesuch": "1" })),
            ],
            &["audit", "nonesuch"],
        ),
        // …and the values must be formulas in the *event's* scope: a bare
        // identifier is the mistake `row.title` was meant to be.
        (
            "insert_row",
            &[
                ("table", json!("audit")),
                ("values", json!({ "what": "title" })),
            ],
            &["title", "unknown identifier"],
        ),
        // An unparseable formula names the field it belongs to.
        (
            "insert_row",
            &[
                ("table", json!("audit")),
                ("values", json!({ "what": "row." })),
            ],
            &["what", "parse error"],
        ),
        // An empty map is a configuration that would do nothing.
        (
            "insert_row",
            &[("table", json!("audit")), ("values", json!({}))],
            &["values", "no fields"],
        ),
        // A formula given as something other than a string.
        (
            "insert_row",
            &[("table", json!("audit")), ("values", json!({ "what": 7 }))],
            &["values", "what", "formula"],
        ),
        // Each matched row is written by primary key, so a keyless target is
        // refused here rather than at fire time.
        (
            "update_rows",
            &[
                ("table", json!("keyless")),
                ("where", json!("a === 1")),
                ("assignments", json!({ "a": "2" })),
            ],
            &["keyless", "primary key"],
        ),
        (
            "delete_rows",
            &[("table", json!("keyless")), ("where", json!("a === 1"))],
            &["keyless", "primary key"],
        ),
        // The predicate is validated against the *target* table.
        (
            "delete_rows",
            &[
                ("table", json!("tasks")),
                ("where", json!("nonesuch === 1")),
            ],
            &["nonesuch"],
        ),
        // The operation flags are the trigger's own event, as `only_if` says too.
        (
            "delete_rows",
            &[("table", json!("tasks")), ("where", json!("_delete"))],
            &["_insert", "operation"],
        ),
        // An assignment to a field the target table does not have.
        (
            "update_rows",
            &[
                ("table", json!("tasks")),
                ("where", json!("status === \"draft\"")),
                ("assignments", json!({ "nonesuch": "1" })),
            ],
            &["tasks", "nonesuch"],
        ),
    ];

    for (action, entries, expected) in cases {
        let trigger = Trigger::new("on-books-update", EventKind::Update, *action)
            .on("books")
            .with_configuration(config(entries));
        let err = validate_trigger(&catalog, &registry, &trigger)
            .await
            .unwrap_err();
        let msg = err.to_string();
        // Every refusal names the trigger and the action, so a list of triggers
        // showing this message is actionable on its own.
        assert!(msg.contains("on-books-update"), "{msg}");
        assert!(msg.contains(action), "{msg}");
        for fragment in *expected {
            assert!(msg.contains(fragment), "expected `{fragment}` in: {msg}");
        }
        assert_eq!(err.kind(), sc_error::ErrorKind::Application, "{msg}");
    }
    Ok(())
}

/// A configuration valid for one event is invalid for another: the ambient scope
/// comes from the trigger's event, so `row.title` is a field on an `insert`
/// trigger and an unknown identifier on a `login` one.
#[tokio::test]
async fn the_events_scope_decides_whether_a_formula_resolves() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let registry = builtin_actions()?;
    let cfg = config(&[
        ("table", json!("audit")),
        ("values", json!({ "what": "row.title" })),
    ]);

    let on_insert = Trigger::new("audit-books", EventKind::Insert, "insert_row")
        .on("books")
        .with_configuration(cfg.clone());
    validate_trigger(&catalog, &registry, &on_insert).await?;

    // The same action, the same configuration, an event with no row.
    let on_login =
        Trigger::new("audit-logins", EventKind::Login, "insert_row").with_configuration(cfg);
    let msg = validate_trigger(&catalog, &registry, &on_login)
        .await
        .unwrap_err()
        .to_string();
    assert!(msg.contains("row"), "{msg}");
    assert!(msg.contains("unknown identifier"), "{msg}");

    // A field of the event's row that does not exist is caught the same way.
    let typo = Trigger::new("audit-books", EventKind::Insert, "insert_row")
        .on("books")
        .with_configuration(config(&[
            ("table", json!("audit")),
            ("values", json!({ "what": "row.titel" })),
        ]));
    let msg = validate_trigger(&catalog, &registry, &typo)
        .await
        .unwrap_err()
        .to_string();
    assert!(msg.contains("titel"), "{msg}");
    Ok(())
}

/// A trigger's writes are the **admin's**, not the caller's: on an RLS-enforced
/// table the action inserts a row it does not own and deletes one it does not
/// own, both of which the caller's own authority would refuse.
///
/// The same test proves the policies are genuinely live and `FORCE`d — an
/// outsider cannot even see the row — so this is not passing because RLS was
/// silently off.
#[tokio::test]
async fn an_actions_writes_carry_admin_authority_on_an_rls_table() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE secrets (id bigint primary key, owner text, note text);
             INSERT INTO secrets VALUES (1, 'someone@else.com', 'theirs');",
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    bootstrap_table_meta(&catalog).await?;
    catalog.reload().await?;
    let mut meta = TableMeta::new("secrets");
    meta.set_ownership_formula(Some("owner === user.email"));
    meta.set_rls_enabled(true);
    save_table_meta(&catalog, &meta).await?;
    let secrets = catalog.require("secrets")?;
    assert!(secrets.rls_enabled, "the overlay is live");
    enable_rls(&catalog, &secrets).await?;

    // The policies decide: a caller who owns nothing here sees nothing.
    let outsider = Principal::new(Role::PUBLIC, Some(json!({ "email": "nobody@example.com" })));
    let visible = rows::list_rows_ctx(&catalog, &secrets, Authority::Caller(&outsider)).await?;
    assert_eq!(visible.as_array().map(Vec::len), Some(0), "{visible}");

    // The trigger inserts a row owned by a third party — refused for any caller
    // the formula does not grant, allowed here because a trigger is the admin's
    // configuration.
    let cfg = config(&[
        ("table", json!("secrets")),
        (
            "values",
            json!({
                "id": "row.id + 100",
                "owner": "\"other@example.com\"",
                "note": "row.title",
            }),
        ),
    ]);
    let inserted = run(&catalog, "insert_row", &book_update(), &cfg, &engine()).await?;
    assert_eq!(inserted["id"], json!(101));
    assert_eq!(inserted["owner"], json!("other@example.com"));

    // …and deletes the pre-existing row, which it does not own either. The
    // predicate had to read it through the policies to select it, so this covers
    // the read half as well.
    let cfg = config(&[
        ("table", json!("secrets")),
        ("where", json!("owner === \"someone@else.com\"")),
    ]);
    let result = run(&catalog, "delete_rows", &book_update(), &cfg, &engine()).await?;
    assert_eq!(result["deleted"], json!(1));
    assert_eq!(result["ids"], json!([1]));

    // Read back through an admin context, because the table is `FORCE`d: a read
    // with no caller context at all sees nothing here, policies applying to the
    // table's owner too. (That is why the action sets one.)
    let admin = Principal::anonymous(Role::ADMIN);
    let remaining = rows::list_rows_ctx(&catalog, &secrets, Authority::Caller(&admin)).await?;
    let ids: Vec<Json> = remaining
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(101)]);
    Ok(())
}
