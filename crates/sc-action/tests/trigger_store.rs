//! Phase 2 integration test: a trigger round-tripping through `_fd_triggers`,
//! every way it can be refused on save, and the cached live set.
//!
//! Against a real database, because all three are about storage: the round trip
//! must survive the SQL types, a strict read must reject a mangled column rather
//! than default it, and the cache must drop a trigger whose *table* has gone —
//! which needs a table to drop.
//!
//! The refusals are the substance of the phase. A trigger that cannot work is
//! refused while the admin is looking at the form; every one of these would
//! otherwise be a trigger that silently never fires, or fires wrongly.

use sc_types::Role;
use std::sync::Arc;

use sc_action::{
    Action, ActionContext, ActionRegistry, EventKind, TRIGGERS_TABLE, Trigger, Triggers,
    bootstrap_triggers, delete_trigger, list_triggers, load_trigger, load_trigger_by_name,
    save_trigger,
};
use sc_catalog::{Catalog, DataField};
use sc_db::{DatabaseDriver, SchemaChange};
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_query::{Assignment, Expr, Statement, Update, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, FormField, TypeRef};
use serde_json::Value as Json;

/// A catalog over a per-test database, with `_fd_triggers` and a `books` table.
async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_triggers(&cat).await?;
    cat.create_table(
        "books",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            DataField::plain("pages", TypeRef::Basic(BasicType::Int)),
        ],
    )
    .await?;
    Ok(cat)
}

/// An action with one declared setting, so a configuration can be right or wrong.
struct Notify;

#[async_trait::async_trait]
impl Action for Notify {
    fn name(&self) -> &str {
        "notify"
    }
    fn description(&self) -> &str {
        "Test action with one required setting"
    }
    fn config_spec(&self) -> Vec<FormField> {
        vec![FormField::new("message", BasicType::Text).required()]
    }
    async fn run(&self, _ctx: &mut ActionContext<'_>) -> Result<Json> {
        Ok(Json::Null)
    }
}

/// The registry every test validates against: the built-ins plus `notify`.
/// Fallible (a duplicate name would be a bug in this file), so the tests `?` it
/// rather than unwrapping inside a helper.
fn registry() -> Result<ActionRegistry> {
    let mut reg = ActionRegistry::new();
    reg.register(Arc::new(Notify))?;
    Ok(reg)
}

/// A valid table trigger on `books`.
fn books_trigger(name: &str) -> Trigger {
    Trigger::new(name, EventKind::Insert, "notify")
        .on("books")
        .config("message", "hello")
}

#[tokio::test]
async fn a_trigger_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let mut trigger = books_trigger("audit")
        .description("write an audit row")
        .only_if("pages > 100 && title !== old.title")
        .min_role(Role::of(4));
    trigger.set_enabled(false);
    save_trigger(&cat, &reg, &trigger).await?;

    let loaded = load_trigger(&cat, trigger.id).await?.expect("by id");
    assert_eq!(loaded, trigger);
    assert_eq!(
        load_trigger_by_name(&cat, "audit").await?.as_ref(),
        Some(&trigger)
    );
    assert!(!loaded.is_enabled());

    // Saving again updates in place rather than inserting a second row.
    let edited = Trigger::with_id(trigger.id, "audit", EventKind::Update, "notify")
        .on("books")
        .config("message", "goodbye");
    save_trigger(&cat, &reg, &edited).await?;
    let all = list_triggers(&cat).await?;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].when, EventKind::Update);
    // The optional fields were cleared, and read back as absent rather than "".
    assert_eq!(all[0].only_if, None);
    assert_eq!(all[0].description, "");
    assert_eq!(all[0].min_role, None);
    assert!(all[0].is_enabled());

    // A second trigger lists after the first, by name.
    save_trigger(&cat, &reg, &books_trigger("a_first")).await?;
    let listed = list_triggers(&cat).await?;
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["a_first", "audit"]);

    assert!(delete_trigger(&cat, trigger.id).await?);
    assert!(!delete_trigger(&cat, trigger.id).await?);
    assert!(load_trigger(&cat, trigger.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn every_way_a_trigger_can_be_wrong_is_refused_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    // Each case: what is wrong, and the words the admin must see.
    let cases: Vec<(Trigger, &str)> = vec![
        (
            Trigger::new("t", EventKind::Insert, "send_email").on("books"),
            "unknown action `send_email`",
        ),
        (
            // The action's required setting is missing.
            Trigger::new("t", EventKind::Insert, "notify").on("books"),
            "message",
        ),
        (
            // A table event with no table.
            Trigger::new("t", EventKind::Insert, "notify").config("message", "x"),
            "must name the table",
        ),
        (
            Trigger::new("t", EventKind::Insert, "notify")
                .on("nonexistent")
                .config("message", "x"),
            "no table named `nonexistent`",
        ),
        (
            // A channel-less event given a table.
            Trigger::new("t", EventKind::Login, "notify")
                .on("books")
                .config("message", "x"),
            "has no table",
        ),
        (books_trigger("t").only_if("pages > "), "`only if`"),
        (
            books_trigger("t").only_if("shoe_size > 1"),
            "unknown identifier `shoe_size`",
        ),
        (
            books_trigger("t").only_if("old.shoe_size > 1"),
            "`old.shoe_size`",
        ),
        (
            // The flags are the event, so naming one is refused (decision 3).
            books_trigger("t").only_if("_insert && pages > 1"),
            "operation flags",
        ),
        (
            // An "only if" needs a row to test.
            Trigger::new("t", EventKind::Login, "notify")
                .config("message", "x")
                .only_if("pages > 1"),
            "needs a row to test",
        ),
        (
            // A stream event with no stream. `sc-action` stops at "present and
            // non-empty": resolving the name would need `sc-stream`, which is
            // the dependency this crate does not have (§8).
            Trigger::new("t", EventKind::Stream, "notify").config("message", "x"),
            "must name the stream",
        ),
        (
            // A stream element is not a row, so a row-shaped `only_if` is the
            // unknown identifier it deserves rather than a null that reads as
            // "no".
            Trigger::new("t", EventKind::Stream, "notify")
                .on("boiler")
                .config("message", "x")
                .only_if("row.pages > 1"),
            "unknown identifier `row`",
        ),
        (
            Trigger::new("", EventKind::Login, "notify").config("message", "x"),
            "needs a name",
        ),
    ];

    for (trigger, expected) in cases {
        let err = save_trigger(&cat, &reg, &trigger)
            .await
            .err()
            .unwrap_or_else(|| panic!("expected a refusal mentioning {expected}"));
        let msg = err.to_string();
        assert!(msg.contains(expected), "expected `{expected}` in: {msg}");
        // Nothing was written by a refused save.
        assert!(
            list_triggers(&cat).await?.is_empty(),
            "a refused save stored something: {msg}"
        );
    }

    // A `min_role` outside the 1–100 scale cannot be constructed (`Role`).

    // A stream trigger saves with a stream nothing here has heard of — this
    // crate cannot resolve one — and its `only_if` reads the envelope through
    // `payload` (§8).
    let stream_trigger = Trigger::new("boiler_hot", EventKind::Stream, "notify")
        .on("boiler")
        .config("message", "hot")
        .only_if("payload.value.temperature > 30");
    save_trigger(&cat, &reg, &stream_trigger).await?;
    sc_action::delete_trigger(&cat, stream_trigger.id).await?;

    // Two triggers cannot share a name.
    save_trigger(&cat, &reg, &books_trigger("dup")).await?;
    let msg = save_trigger(&cat, &reg, &books_trigger("dup"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(msg.contains("already used"), "{msg}");
    Ok(())
}

#[tokio::test]
async fn a_mangled_column_is_reported_not_defaulted() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let trigger = books_trigger("audit");
    save_trigger(&cat, &reg, &trigger).await?;

    // Write an event nothing implements — a downgrade, or a hand-edited row.
    let update = Update::new(
        TRIGGERS_TABLE,
        vec![Assignment::new(
            "event".to_owned(),
            Expr::lit("insert_validate"),
        )],
    )
    .filter(Expr::col("id").eq(Expr::Lit(Value::Uuid(trigger.id.0))));
    cat.primary().query(&Statement::from(update)).await?;

    let err = list_triggers(&cat).await.err().unwrap().to_string();
    assert!(err.contains("audit"), "{err}");
    assert!(err.contains("insert_validate"), "{err}");
    Ok(())
}

#[tokio::test]
async fn the_cache_drops_what_cannot_fire_and_says_why() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    save_trigger(&cat, &reg, &books_trigger("on_books")).await?;
    save_trigger(
        &cat,
        &reg,
        &Trigger::new("on_login", EventKind::Login, "notify").config("message", "hi"),
    )
    .await?;

    let mut live = Triggers::load(&cat, &reg).await?;
    assert_eq!(live.all().len(), 2);
    assert!(live.issues().is_empty());
    // Matching: the insert on `books` fires exactly the one trigger.
    let fired: Vec<&str> = live
        .matching(EventKind::Insert, Some("books"))
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(fired, vec!["on_books"]);
    assert_eq!(live.matching(EventKind::Login, None).count(), 1);
    assert_eq!(live.matching(EventKind::Delete, Some("books")).count(), 0);

    // Drop the table behind the trigger's back — a schema change nobody told
    // Saltcorn about, which is exactly the case a restored dump produces.
    cat.primary()
        .apply_schema(&SchemaChange::DropTable {
            name: "books".to_owned(),
            if_exists: false,
        })
        .await?;
    cat.reload().await?;

    live.reload(&cat, &reg).await?;
    // Fail closed: the trigger is gone from the live set, with the reason kept.
    assert_eq!(live.all().len(), 1);
    assert_eq!(live.matching(EventKind::Insert, Some("books")).count(), 0);
    let issue = &live.issues()[0];
    assert_eq!(issue.trigger, "on_books");
    assert!(
        issue.problem.contains("no table named `books`"),
        "{issue:?}"
    );
    // Asking for it by name gets the reason, not a "no such trigger".
    let err = live.require("on_books").err().unwrap();
    assert!(err.to_string().contains("not usable"), "{err}");
    assert!(!matches!(err.repr(), Repr::NotFound(_)));
    let missing = live.require("never_defined").err().unwrap();
    assert!(matches!(missing.repr(), Repr::NotFound(_)), "{missing}");

    // …and the trigger is still *stored*, so fixing the table brings it back.
    assert!(load_trigger_by_name(&cat, "on_books").await?.is_some());
    Ok(())
}

#[tokio::test]
async fn a_workflow_bodied_trigger_round_trips_and_stays_a_workflow() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    // A workflow inherits everything a trigger is — its event, its table, its
    // `only_if`, its floor, its enabled flag — and differs in exactly one thing:
    // there is no action and no configuration, because its steps are a version
    // of their own (§10.3).
    let trigger = Trigger::workflow("approve_orders", EventKind::Insert)
        .on("books")
        .only_if("pages > 100")
        .min_role(Role::of(40))
        .description("the order approval workflow");
    save_trigger(&cat, &reg, &trigger).await?;

    let loaded = load_trigger_by_name(&cat, "approve_orders")
        .await?
        .expect("by name");
    assert_eq!(loaded, trigger);
    assert!(loaded.is_workflow());
    assert_eq!(loaded.action(), None);
    assert_eq!(loaded.configuration(), None);
    assert_eq!(loaded.only_if.as_deref(), Some("pages > 100"));
    assert_eq!(loaded.min_role, Some(Role::of(40)));

    // It is in the live set like any other trigger: validation has no action to
    // check, and everything the two bodies share is checked the same way.
    let live = Triggers::load(&cat, &reg).await?;
    assert!(live.issues().is_empty(), "{:?}", live.issues());
    assert_eq!(live.matching(EventKind::Insert, Some("books")).count(), 1);

    // The two bodies are stored side by side and read back as what they are.
    save_trigger(&cat, &reg, &books_trigger("plain")).await?;
    let listed = list_triggers(&cat).await?;
    let bodies: Vec<&str> = listed.iter().map(|t| t.body.as_str()).collect();
    assert_eq!(bodies, vec!["workflow", "action"]);
    Ok(())
}

#[tokio::test]
async fn a_row_whose_body_and_action_disagree_is_refused_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let trigger = Trigger::workflow("approve_orders", EventKind::Insert).on("books");
    save_trigger(&cat, &reg, &trigger).await?;

    // A hand-edited row, or half a downgrade: a workflow body still naming an
    // action. Running that action would run the thing the admin replaced, so it
    // is reported naming the trigger rather than read as either half.
    let update = Update::new(
        TRIGGERS_TABLE,
        vec![Assignment::new("action".to_owned(), Expr::lit("notify"))],
    )
    .filter(Expr::col("id").eq(Expr::Lit(Value::Uuid(trigger.id.0))));
    cat.primary().query(&Statement::from(update)).await?;

    let err = list_triggers(&cat).await.err().unwrap().to_string();
    assert!(err.contains("approve_orders"), "{err}");
    assert!(err.contains("has no action"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_catalog_with_no_triggers_table_has_no_triggers() -> Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    // No bootstrap: the table's absence means "no triggers were ever defined",
    // which is a state a database Saltcorn has just met is legitimately in.
    let live = Triggers::load(&cat, &registry()?).await?;
    assert!(live.all().is_empty() && live.issues().is_empty());
    assert_eq!(live.matching(EventKind::Startup, None).count(), 0);
    Ok(())
}
