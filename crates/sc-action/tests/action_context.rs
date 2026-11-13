//! Phase 1 integration test: what one action run can see.
//!
//! [`ActionContext`] holds a [`Catalog`], which needs a database, so its contract
//! is pinned here rather than in a unit test — and pinning it *is* worth a
//! database, because every built-in action in Phase 3 reads its configuration and
//! its event through exactly these accessors, and an action that silently sees
//! nothing is the failure mode this phase exists to prevent.
//!
//! What is asserted: an action reaches the event, its configuration and the
//! catalog; a missing required setting and a missing JavaScript engine are each an
//! error naming the trigger, not a quiet no-op; the run context is writable and
//! survives the run; that a **workflow step**'s settings see that context and a
//! trigger's own action body does not; and that the cascade chain defaults to the
//! trigger itself.

use sc_types::Role;
use std::sync::Arc;

use sc_action::{Action, ActionContext, ActionRegistry, Event, EventKind};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{ErrorKind, Result};
use sc_expr::Ambient;
use sc_query::Value;
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::{Value as Json, json};

/// A catalog over a per-test database.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// An action that reports everything it can see, so a test can assert on it: the
/// configured message, the event's channel and row, whether the catalog resolved
/// a table, and the trigger it ran for.
struct Report;

#[async_trait::async_trait]
impl Action for Report {
    fn name(&self) -> &str {
        "report"
    }

    fn description(&self) -> &str {
        "Report what the action context exposes"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![FormField::new("message", BasicType::Text).required()]
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let message = ctx.require_str("message")?;
        // The catalog is live: the action can resolve the table its event names.
        let table = match ctx.event.channel.as_deref() {
            Some(name) => ctx.catalog.require(name)?.fields.len(),
            None => 0,
        };
        ctx.context.insert("ran".into(), json!(true));
        Ok(json!({
            "message": message,
            "trigger": ctx.trigger,
            "channel": ctx.event.channel,
            "title": ctx.event.row_object().get("title"),
            "old_title": ctx.event.old_row_object().get("title"),
            "fields": table,
            "chain": ctx.chain,
        }))
    }
}

async fn books(cat: &Catalog) -> Result<()> {
    cat.create_table(
        "books",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn an_action_sees_its_event_its_config_and_the_catalog() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    books(&cat).await?;

    let event = Event::new(EventKind::Update)
        .on("books")
        .row(json!({ "id": 1, "title": "new" }))
        .old_row(json!({ "id": 1, "title": "old" }))
        .caller(Role::ADMIN, Some(json!({ "email": "admin@example.com" })));
    let mut config = Attrs::new();
    config.insert("message".into(), json!("hello"));

    let mut ctx = ActionContext::new(&cat, &event, &config, "audit");
    let result = Report.run(&mut ctx).await?;

    assert_eq!(result["message"], json!("hello"));
    assert_eq!(result["trigger"], json!("audit"));
    assert_eq!(result["channel"], json!("books"));
    assert_eq!(result["title"], json!("new"));
    assert_eq!(result["old_title"], json!("old"));
    // The catalog really is the live one: `books` has the two fields created.
    assert_eq!(result["fields"], json!(2));
    // The chain names the trigger being run, so an event this action caused would
    // descend from it.
    assert_eq!(result["chain"], json!(["audit"]));
    // The run context survives the run — the seam a workflow's durable context
    // grows into.
    assert_eq!(ctx.context.get("ran"), Some(&json!(true)));
    Ok(())
}

#[tokio::test]
async fn a_step_of_a_run_reads_the_context_and_a_trigger_body_does_not() -> Result<()> {
    // Decision 8, both halves (TODO 9.1). An action's settings are read in the
    // scope its *caller* decides, and the two callers are a trigger firing once
    // and a workflow step. The action does not choose between them and must not
    // have to: it asks `shape()` and `bindings()`, and gets whichever it is in.
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    books(&cat).await?;

    let event = Event::new(EventKind::Update)
        .on("books")
        .row(json!({ "id": 1, "title": "new" }))
        .old_row(json!({ "id": 1, "title": "old" }));
    let config = Attrs::new();

    // A trigger's own action body: no run, so no `context` — not an empty one.
    let plain = ActionContext::new(&cat, &event, &config, "audit");
    assert!(plain.run_context().is_none());
    assert!(!plain.shape()?.declares_ambient(Ambient::Context));
    assert!(!plain.bindings().ambient.contains_key(&Ambient::Context));
    // What the event puts in scope is unchanged either way.
    assert!(plain.shape()?.declares_ambient(Ambient::Row));

    // The same action as a workflow step: `context` is in scope and bound to
    // what the steps before it left, and `context.anything` resolves because
    // nothing declares what a run has accumulated.
    let context: Attrs = [("total".to_owned(), json!(120))].into_iter().collect();
    let step = ActionContext::new(&cat, &event, &config, "write").with_run_context(context);
    assert_eq!(
        step.run_context().and_then(|c| c.get("total")),
        Some(&json!(120))
    );
    let shape = step.shape()?;
    assert!(shape.declares_ambient(Ambient::Context));
    assert!(shape.ambient_field_set(Ambient::Context).is_none());
    assert!(
        shape.declares_ambient(Ambient::Row),
        "the event is still there"
    );
    let bound = step.bindings();
    assert_eq!(
        bound.ambient[&Ambient::Context]
            .as_ref()
            .and_then(|c| c.get("total")),
        Some(&Value::Int(120))
    );

    // The first step of a run has an empty context and still reads it: presence
    // is scope, so `context.x` there is a null rather than an unknown name.
    let first = ActionContext::new(&cat, &event, &config, "first").with_run_context(Attrs::new());
    assert!(first.shape()?.declares_ambient(Ambient::Context));
    assert!(first.bindings().ambient.contains_key(&Ambient::Context));
    Ok(())
}

#[tokio::test]
async fn a_missing_setting_fails_the_run_naming_the_trigger() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    books(&cat).await?;

    let event = Event::new(EventKind::Insert).on("books");
    // The setting the spec declares required is simply absent.
    let config = Attrs::new();
    let mut ctx = ActionContext::new(&cat, &event, &config, "notify");
    let err = Report.run(&mut ctx).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("notify"), "{msg}");
    assert!(msg.contains("message"), "{msg}");
    // A mis-configured trigger is an application error: the admin fixes it.
    assert_eq!(err.kind(), ErrorKind::Application);
    Ok(())
}

#[tokio::test]
async fn a_context_without_an_engine_refuses_rather_than_skipping() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let event = Event::new(EventKind::None);
    let config = Attrs::new();
    let ctx = ActionContext::new(&cat, &event, &config, "compute");
    // No `with_evaluator`: an action needing formulas gets a named error, never a
    // silent no-op (principle 5). (`.err()` rather than `unwrap_err()`: the `Ok`
    // side is a trait object with no `Debug`.)
    let err = ctx.evaluator().err().unwrap();
    assert!(err.to_string().contains("compute"), "{err}");
    assert_eq!(err.kind(), ErrorKind::Application);
    Ok(())
}

#[tokio::test]
async fn a_registered_action_runs_through_the_registry() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    books(&cat).await?;

    // The path every trigger takes: a stored action *name* resolved to the
    // implementation, then run.
    let mut registry = ActionRegistry::new();
    registry.register(Arc::new(Report))?;

    let event = Event::new(EventKind::Insert)
        .on("books")
        .row(json!({ "id": 7, "title": "registered" }));
    let mut config = Attrs::new();
    config.insert("message".into(), json!("via registry"));

    let action = registry.require("report")?.clone();
    let mut ctx = ActionContext::new(&cat, &event, &config, "on-insert")
        .with_chain(vec!["outer".into(), "on-insert".into()]);
    let result = action.run(&mut ctx).await?;

    assert_eq!(result["message"], json!("via registry"));
    assert_eq!(result["title"], json!("registered"));
    assert_eq!(result["chain"], json!(["outer", "on-insert"]));
    Ok(())
}

/// TODO-code-quality Q3.4: an action is handed its authority, not left to build
/// it from `event.role` / `event.user`. Its rows are the **trigger's** — the
/// administrator, in the event's user's name, with the chain that led here,
/// on the step's transaction — and the event's caller is a principal of its
/// own, for what a delegated operation acts as.
#[tokio::test]
async fn an_action_is_handed_the_triggers_authority_and_the_events_caller() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = Arc::new(catalog(&db).await?);
    db.on_teardown(cat.teardown_hook());

    let user = json!({ "id": "8f3b1c6e-2d4a-4b1e-9a57-0c2d3e4f5a6b", "email": "s@x" });
    let event = Event::new(EventKind::Update).caller(Role::of(40), Some(user.clone()));
    let config = Attrs::new();
    let ctx = ActionContext::new(&cat, &event, &config, "audit")
        .with_chain(vec!["outer".into(), "audit".into()]);

    let scoped = ctx.scoped();
    assert!(scoped.role().is_admin(), "a trigger writes as the admin");
    let principal = scoped.principal().expect("a caller, not the system");
    assert_eq!(principal.user(), Some(&user), "in the event's user's name");
    assert_eq!(principal.chain(), ["outer", "audit"]);
    assert!(
        scoped.unchecked().is_some(),
        "the admin's rows are unchecked"
    );
    assert!(matches!(
        scoped.executor(),
        sc_access::rows::Executor::Pooled
    ));

    let caller = ctx.caller();
    assert_eq!(caller.role(), Role::of(40));
    assert_eq!(caller.user(), Some(&user));
    assert_eq!(caller.chain(), ["outer", "audit"]);

    // A workflow step's transaction is where the scoped handle's statements run.
    let tx = sc_catalog::SharedTx::begin_primary(&cat)?;
    let ctx = ActionContext::new(&cat, &event, &config, "audit").with_transaction(tx);
    assert!(matches!(
        ctx.scoped().executor(),
        sc_access::rows::Executor::Transaction(_)
    ));
    Ok(())
}
