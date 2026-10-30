//! Checklists against a real Postgres (TODO-checklists C1.17): a data
//! checklist that inserts a row, runs a trigger and asserts its effect, and
//! leaves nothing behind; `data.expect_denied` turning red when a role is
//! loosened; the agent policy of §4; the production guard; and a step the host
//! cannot run making the run incomplete.

use std::path::PathBuf;
use std::sync::Arc;

use sc_action::{
    Action, ActionContext, ActionRegistry, EventKind, Trigger, TriggerDispatcher,
    bootstrap_triggers, save_trigger,
};
use sc_app::{Application, FrameworkRef, save_application};
use sc_auth::{ROLE_ADMIN, Role, User, create_user, save_role, set_check_user};
use sc_catalog::{Catalog, FileStoreId, TableMeta, save_table_meta};
use sc_check::{
    Change, CheckEnv, ChecklistGrant, ChecklistInput, Editor, Outcome, ProposalChange, RunStatus,
    SkipReason, StepDraft, StepRegistry, edit, run_checklist, store,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_email::{Email, Mailbox, Mailer, RecordingMailer};
use sc_error::{Repr, Result};
use sc_test_harness::TestDb;
use sc_types::FormField;
use serde_json::{Value as Json, json};

/// A trigger action that records the order it was run for in `audit`.
struct Stamp;

#[async_trait::async_trait]
impl Action for Stamp {
    fn name(&self) -> &str {
        "stamp"
    }
    fn description(&self) -> &str {
        "Write an audit row for the payload's order (test only)"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let order = ctx.event.payload["order_id"].clone();
        let audit = ctx.catalog.get("audit")?.expect("the audit table");
        sc_api::rows::create_row(
            ctx.catalog,
            &audit,
            &json!({ "order_id": order, "note": "stamped" }),
        )
        .await
    }
}

/// A trigger action that mails the payload's customer a confirmation.
struct Confirm;

#[async_trait::async_trait]
impl Action for Confirm {
    fn name(&self) -> &str {
        "confirm"
    }
    fn description(&self) -> &str {
        "Mail the payload's customer a confirmation (test only)"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let to = ctx.event.payload["email"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut email = Email::new(Mailbox {
            name: None,
            address: "shop@example.com".to_owned(),
        });
        email.to.push(Mailbox {
            name: None,
            address: to,
        });
        email.subject = "Your order is confirmed".to_owned();
        email.text = Some("Thank you for your order.".to_owned());
        ctx.mailer()?.send(&email).await?;
        Ok(Json::Null)
    }
}

struct Env {
    catalog: Arc<Catalog>,
    triggers: Arc<TriggerDispatcher>,
    /// What the dispatcher's mailer was actually handed.
    mailer: Arc<RecordingMailer>,
    registry: StepRegistry,
    admin: User,
    member: User,
    _db: TestDb,
    _dir: TempDir,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

async fn setup(tag: &str) -> Result<Env> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE orders (id serial primary key, customer text not null, status text); \
             CREATE TABLE audit (id serial primary key, order_id int, note text);",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    bootstrap_triggers(&catalog).await?;
    sc_check::bootstrap_checklists(&catalog).await?;

    let dir = std::env::temp_dir().join(format!("sc-check-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("site")).unwrap();
    catalog.connect_file_store(Arc::new(sc_files::LocalFileStore::new("files", &dir)?))?;

    save_role(&catalog, &Role::new(80, "Member")).await?;
    let admin = create_user(&catalog, "admin@example.com", "pw-admin", ROLE_ADMIN).await?;
    let member = create_user(&catalog, "check-member@example.com", "pw-member", 80).await?;
    // Members may read and write orders, and only read the audit trail.
    save_table_meta(&catalog, &TableMeta::new("orders").access(80, 80)).await?;
    save_table_meta(&catalog, &TableMeta::new("audit").access(80, 1)).await?;

    let mut actions = ActionRegistry::new();
    actions.register(Arc::new(Stamp))?;
    actions.register(Arc::new(Confirm))?;
    let actions = Arc::new(actions);
    let trigger = Trigger::new("stamp_order", EventKind::None, "stamp").min_role(80);
    save_trigger(&catalog, &actions, &trigger).await?;
    let confirm = Trigger::new("confirm_order", EventKind::None, "confirm").min_role(80);
    save_trigger(&catalog, &actions, &confirm).await?;
    let mailer = Arc::new(RecordingMailer::new());
    let triggers = Arc::new(
        TriggerDispatcher::new(actions).with_mailer(Arc::clone(&mailer) as Arc<dyn Mailer>),
    );
    triggers.reload(&catalog).await?;

    Ok(Env {
        catalog,
        triggers,
        mailer,
        registry: StepRegistry::builtin(),
        admin,
        member,
        _db: db,
        _dir: TempDir(dir),
    })
}

impl Env {
    fn env(&self) -> CheckEnv<'_> {
        CheckEnv::new(&self.catalog, &self.registry).with_triggers(Some(&self.triggers))
    }

    fn human(&self) -> Editor {
        Editor::Human(self.admin.id)
    }

    async fn count(&self, table: &str) -> i64 {
        self._db
            .client()
            .await
            .unwrap()
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0)
    }
}

fn agent(run: &str, grant: ChecklistGrant) -> Editor {
    Editor::Agent {
        run: run.to_owned(),
        grant,
    }
}

#[tokio::test]
async fn a_data_checklist_inserts_runs_a_trigger_asserts_and_cleans_up() -> Result<()> {
    let env = setup("data").await?;
    let mut input = ChecklistInput::new("Orders are stamped");
    input.run_as = Some(env.member.id);
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        input,
        vec![
            StepDraft::new("data.insert")
                .param("table", "orders")
                .param("row", json!({ "customer": "Test customer {{ run.id }}" }))
                .capture("order")
                .label("Create an order"),
            StepDraft::new("data.run_trigger")
                .param("trigger", "stamp_order")
                .param("payload", json!({ "order_id": "{{ order.id }}" }))
                .label("Stamp it"),
            StepDraft::new("data.expect_rows")
                .param("table", "audit")
                .param("where", json!({ "order_id": "{{ order.id }}" }))
                .param("count", 1)
                .param("fields", json!({ "note": "stamped" }))
                .label("The stamp is there"),
            StepDraft::new("data.expect_rows")
                .param("table", "orders")
                .param("where", json!({ "customer": "Test customer {{ run.id }}" }))
                .param("count", 1),
            StepDraft::new("core.manual").param("instruction", "The order email looks right"),
        ],
    )
    .await?;
    assert!(
        checklist.mutates,
        "data.insert writes, so the checklist mutates"
    );

    let run = run_checklist(&env.env(), &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    assert_eq!(run.steps.len(), 5);
    assert_eq!(run.steps[4].outcome, Outcome::Skipped);
    assert_eq!(run.steps[4].skip, Some(SkipReason::Manual));
    assert!(run.cleanup_errors.is_empty(), "{:?}", run.cleanup_errors);
    // The inserted order is gone again; the trigger's audit row is the
    // application's, found by the run's id convention, and stays.
    assert_eq!(env.count("orders").await, 0);
    assert_eq!(env.count("audit").await, 1);

    // The run is recorded with the step versions it ran.
    let stored = store::load_run(&env.catalog, run.id)
        .await?
        .expect("stored");
    assert_eq!(stored.status, RunStatus::Passed);
    let current: Vec<_> = store::current_steps(&env.catalog, checklist.id)
        .await?
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(stored.step_versions(), current);
    assert_eq!(stored.trigger, "manual");
    Ok(())
}

#[tokio::test]
async fn mail_sent_during_a_run_is_kept_on_the_run_and_not_sent() -> Result<()> {
    let env = setup("outbox").await?;
    let mut input = ChecklistInput::new("Orders are confirmed by mail");
    input.run_as = Some(env.member.id);
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        input,
        vec![
            StepDraft::new("data.run_trigger")
                .param("trigger", "confirm_order")
                .param(
                    "payload",
                    json!({ "email": "customer-{{ run.id }}@example.com" }),
                ),
            StepDraft::new("data.expect_sent")
                .param("kind", "email")
                .param("to", "customer-{{ run.id }}@example.com")
                .param("subject", "confirmed")
                .param("count", 1)
                .label("The confirmation was sent"),
            StepDraft::new("data.expect_sent")
                .param("kind", "webhook")
                .param("count", 0)
                .label("No webhook was called"),
        ],
    )
    .await?;

    let run = run_checklist(&env.env(), &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    assert!(env.mailer.sent().is_empty(), "a run must not send mail");
    assert_eq!(run.outbox.len(), 1);
    assert_eq!(
        run.outbox[0].to,
        vec![format!("customer-{}@example.com", run.id)]
    );
    let stored = store::load_run(&env.catalog, run.id)
        .await?
        .expect("stored");
    assert_eq!(stored.outbox, run.outbox);

    // Outside a run the same trigger sends.
    env.triggers
        .run_trigger(
            &env.catalog,
            "confirm_order",
            json!({ "email": "real@example.com" }),
            None,
        )
        .await?;
    assert_eq!(env.mailer.only()?.to[0].address, "real@example.com");

    // An expectation the outbox does not meet fails the step.
    let failing = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        ChecklistInput::new("Nothing was mailed"),
        vec![StepDraft::new("data.expect_sent").param("to", "nobody@example.com")],
    )
    .await?;
    let run = run_checklist(&env.env(), &failing, "manual").await?;
    assert_eq!(run.status, RunStatus::Failed, "{:#?}", run.steps);
    Ok(())
}

#[tokio::test]
async fn expect_denied_fails_once_the_tables_role_is_loosened() -> Result<()> {
    let env = setup("denied").await?;
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        ChecklistInput::new("Visitors cannot write orders"),
        vec![
            StepDraft::new("data.expect_denied")
                .param("table", "orders")
                .param("operation", "insert")
                .param("row", json!({ "customer": "x" })),
            StepDraft::new("data.expect_denied")
                .param("table", "orders")
                .param("operation", "read"),
        ],
    )
    .await?;
    // It only asks: nothing is written, so it may run on production.
    assert!(!checklist.mutates);

    let run = run_checklist(&env.env(), &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);

    // Somebody opens the table to the public.
    let meta = sc_catalog::load_table_meta_by_name(&env.catalog, "orders")
        .await?
        .expect("an overlay")
        .access(100, 100);
    save_table_meta(&env.catalog, &meta).await?;
    let run = run_checklist(&env.env(), &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.steps[0].outcome, Outcome::Fail);
    assert!(
        run.steps[0].message.contains("may insert"),
        "{}",
        run.steps[0].message
    );
    assert_eq!(env.count("orders").await, 0, "expect_denied writes nothing");
    Ok(())
}

#[tokio::test]
async fn an_agent_appends_freely_and_proposes_everything_else() -> Result<()> {
    let env = setup("policy").await?;
    let mut as_member = ChecklistInput::new("Orders");
    as_member.run_as = Some(env.member.id);
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        as_member,
        vec![
            StepDraft::new("data.expect_rows")
                .param("table", "orders")
                .param("count", 0)
                .label("No orders yet"),
        ],
    )
    .await?;
    let accepted = store::current_steps(&env.catalog, checklist.id).await?[0].clone();
    assert!(accepted.accepted, "a person's step is accepted as written");

    let me = agent("run-1", ChecklistGrant::Append);
    // Appending is always allowed, and the step waits for a person to see it.
    let appended = edit::append_steps(
        &env.catalog,
        &env.registry,
        &me,
        checklist.id,
        vec![
            StepDraft::new("data.expect_rows")
                .param("table", "audit")
                .param("count", 0),
        ],
    )
    .await?;
    assert!(!appended[0].accepted);
    assert_eq!(appended[0].author.to_text(), "agent:run-1");

    // Its own unaccepted step it edits directly...
    let change = edit::request_change(
        &env.catalog,
        &env.registry,
        &me,
        checklist.id,
        ProposalChange::Edit {
            step: appended[0].id,
            draft: StepDraft::new("data.expect_rows")
                .param("table", "audit")
                .param("count", 0)
                .label("No audit rows"),
        },
        "",
    )
    .await?;
    assert_eq!(change, Change::Applied);
    // ...but not another run's.
    let other = agent("run-2", ChecklistGrant::Append);
    let own = store::current_steps(&env.catalog, checklist.id).await?[1].clone();
    let err = edit::remove_step(&env.catalog, &env.registry, &other, own.id)
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Auth(_)), "{err}");

    // It cannot delete an accepted step, nor the checklist; the refusal names
    // the grant that would allow it.
    let err = edit::remove_step(&env.catalog, &env.registry, &me, accepted.id)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("checklists: full"), "{err}");
    let err = edit::delete_checklist(&env.catalog, &me, checklist.id)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("checklists: full"), "{err}");
    // Nor edit an accepted step directly.
    assert!(
        edit::edit_step(
            &env.catalog,
            &env.registry,
            &me,
            accepted.id,
            StepDraft::new("data.expect_rows")
                .param("table", "orders")
                .param("count", 5),
        )
        .await
        .is_err()
    );

    // Its edit of the accepted step becomes a proposal, and the old step keeps
    // running until a person accepts it.
    let weaker = StepDraft::new("data.expect_rows")
        .param("table", "orders")
        .param("count", 1)
        .label("One order");
    let Change::Proposed(proposal) = edit::request_change(
        &env.catalog,
        &env.registry,
        &me,
        checklist.id,
        ProposalChange::Edit {
            step: accepted.id,
            draft: weaker,
        },
        "the fixture now creates an order",
    )
    .await?
    else {
        panic!("an agent's edit of an accepted step is a proposal");
    };
    let steps = store::current_steps(&env.catalog, checklist.id).await?;
    assert_eq!(
        steps[0].id, accepted.id,
        "the old step is still the current one"
    );
    let run = run_checklist(&env.env(), &checklist, "agent:run-1").await?;
    assert_eq!(run.steps[0].step, accepted.id);
    assert_eq!(run.status, RunStatus::Passed);

    // An agent cannot accept its own proposal; a person can.
    assert!(
        edit::accept_proposal(&env.catalog, &env.registry, &me, proposal.id)
            .await
            .is_err()
    );
    edit::accept_proposal(&env.catalog, &env.registry, &env.human(), proposal.id).await?;
    let steps = store::current_steps(&env.catalog, checklist.id).await?;
    assert_ne!(steps[0].id, accepted.id);
    assert_eq!(steps[0].draft.label, "One order");
    assert!(steps[0].accepted);
    assert_eq!(steps[0].author.to_text(), "agent:run-1");
    assert!(
        store::proposals_of(&env.catalog, checklist.id)
            .await?
            .is_empty()
    );
    // The history is kept: the old version points at the new one.
    let old = store::require_step(&env.catalog, accepted.id).await?;
    assert_eq!(old.superseded_by, Some(steps[0].id));

    // Under `none` it may not even append.
    let err = edit::append_steps(
        &env.catalog,
        &env.registry,
        &agent("run-3", ChecklistGrant::None),
        checklist.id,
        vec![StepDraft::new("core.wait").param("ms", 1)],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("checklists: append"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_agent_may_only_run_a_checklist_as_a_check_user() -> Result<()> {
    let env = setup("runas").await?;
    let me = agent("run-1", ChecklistGrant::Full);
    let mut input = ChecklistInput::new("As the admin");
    input.run_as = Some(env.admin.id);
    let err = edit::create_checklist(&env.catalog, &env.registry, &me, input.clone(), vec![])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("check user"), "{err}");

    // Once an admin makes the member a check user, the agent may use it.
    assert!(!env.member.is_check_user());
    let member = set_check_user(&env.catalog, env.member.id, true).await?;
    assert!(member.is_check_user());
    input.run_as = Some(member.id);
    input.name = "As a member".to_owned();
    let checklist = edit::create_checklist(&env.catalog, &env.registry, &me, input, vec![]).await?;

    // And a proposal to switch it to the admin is refused, not stored.
    let err = edit::request_change(
        &env.catalog,
        &env.registry,
        &agent("run-1", ChecklistGrant::Append),
        checklist.id,
        ProposalChange::RunAs {
            user: Some(env.admin.id),
        },
        "needs more rights",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("check user"), "{err}");
    assert!(
        store::proposals_of(&env.catalog, checklist.id)
            .await?
            .is_empty()
    );
    // A person may point a checklist at anybody.
    let mut by_hand = ChecklistInput::new("As the admin");
    by_hand.run_as = Some(env.admin.id);
    edit::create_checklist(&env.catalog, &env.registry, &env.human(), by_hand, vec![]).await?;
    Ok(())
}

#[tokio::test]
async fn a_mutating_checklist_is_refused_on_production() -> Result<()> {
    let env = setup("prod").await?;
    env.catalog.set_production(true);
    let insert = || {
        vec![
            StepDraft::new("data.insert")
                .param("table", "orders")
                .param("row", json!({ "customer": "x" })),
        ]
    };
    let mut writes = ChecklistInput::new("Writes");
    writes.run_as = Some(env.member.id);
    let checklist =
        edit::create_checklist(&env.catalog, &env.registry, &env.human(), writes, insert()).await?;
    let run = run_checklist(&env.env(), &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Error);
    assert!(run.steps.is_empty());
    let message = run.message.unwrap_or_default();
    assert!(message.contains("production"), "{message}");
    assert_eq!(env.count("orders").await, 0);

    // Allowed there by a person, it runs.
    let mut allowed = ChecklistInput::new("Writes, allowed");
    allowed.run_as = Some(env.member.id);
    allowed
        .attributes
        .insert("allow_on_production".to_owned(), json!(true));
    // An agent may not allow one.
    assert!(
        edit::create_checklist(
            &env.catalog,
            &env.registry,
            &agent("r", ChecklistGrant::Full),
            allowed.clone(),
            insert(),
        )
        .await
        .is_err()
    );
    let checklist =
        edit::create_checklist(&env.catalog, &env.registry, &env.human(), allowed, insert())
            .await?;
    let run = run_checklist(&env.env(), &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    Ok(())
}

#[tokio::test]
async fn a_step_the_host_cannot_run_makes_the_run_incomplete() -> Result<()> {
    let env = setup("incomplete").await?;
    let app = Application::new(
        "Shop",
        "shop",
        FrameworkRef::new("none")
            .with("store", "files")
            .with("source", "site"),
    )
    .with_file_store(FileStoreId("files".to_owned()));
    save_application(&env.catalog, &app).await?;
    let mut input = ChecklistInput::new("Shop answers");
    input.application = Some(app.id.0);
    input.run_as = Some(env.member.id);
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        input,
        vec![
            StepDraft::new("data.expect_rows")
                .param("table", "orders")
                .param("count", 0),
            StepDraft::new("http.expect_status")
                .param("path", "/")
                .param("status", 200),
        ],
    )
    .await?;
    // No application router on this host: the data step runs, the HTTP one is
    // skipped, and the run is not green.
    let run = run_checklist(&env.env(), &checklist, "cli").await?;
    assert_eq!(run.status, RunStatus::Incomplete, "{:#?}", run.steps);
    assert_eq!(run.steps[0].outcome, Outcome::Pass);
    assert_eq!(run.steps[1].outcome, Outcome::Skipped);
    assert_eq!(run.steps[1].skip, Some(SkipReason::Unavailable));

    // And an HTTP step on a checklist with no application is refused on save.
    let err = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        ChecklistInput::new("No app"),
        vec![StepDraft::new("http.request").param("path", "/")],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("has none"), "{err}");
    // As is a parameter the type does not take.
    let err = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        ChecklistInput::new("Typo"),
        vec![StepDraft::new("core.wait").param("msec", 1)],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown setting `msec`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn browser_steps_on_a_host_without_chromium_make_the_run_incomplete() -> Result<()> {
    let env = setup("no-browser").await?;
    let web = Application::new(
        "Web",
        "web",
        FrameworkRef::new("code")
            .with("store", "files")
            .with("source", "site")
            .with("output", "site/dist")
            .with("command", "true"),
    )
    .with_file_store(FileStoreId("files".to_owned()));
    save_application(&env.catalog, &web).await?;
    let mut input = ChecklistInput::new("Home page");
    input.application = Some(web.id.0);
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        input,
        vec![
            StepDraft::new("browser.open").param("path", "/"),
            StepDraft::new("browser.click").param("locator", "button \"Save\""),
            StepDraft::new("browser.expect_text").param("text", "Saved"),
        ],
    )
    .await?;
    // Browser steps never make a checklist "mutating": one that only opens
    // pages and clicks runs anywhere (§3).
    assert!(!checklist.mutates);

    // No browser on this host: every browser step is skipped, saying why, and
    // the run is not green.
    let run = run_checklist(&env.env(), &checklist, "cli").await?;
    assert_eq!(run.status, RunStatus::Incomplete, "{:#?}", run.steps);
    for step in &run.steps {
        assert_eq!(step.outcome, Outcome::Skipped, "{step:?}");
        assert_eq!(step.skip, Some(SkipReason::Unavailable));
        assert!(
            step.message.contains("no headless browser"),
            "{}",
            step.message
        );
        assert_eq!(step.screenshot, None);
    }

    // A locator that is not one is refused when the step is saved.
    let err = edit::append_steps(
        &env.catalog,
        &env.registry,
        &env.human(),
        checklist.id,
        vec![StepDraft::new("browser.click").param("locator", "#save")],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("is not a locator"), "{err}");

    // An application whose framework serves no UI is offered no browser steps,
    // and one that has them anyway errors rather than being skipped.
    let api = Application::new(
        "Api",
        "api",
        FrameworkRef::new("none")
            .with("store", "files")
            .with("source", "site"),
    )
    .with_file_store(FileStoreId("files".to_owned()));
    save_application(&env.catalog, &api).await?;
    let offered: Vec<&str> = env
        .registry
        .offered(Some(&api))
        .iter()
        .map(|t| t.name())
        .collect();
    assert!(offered.contains(&"http.request"), "{offered:?}");
    assert!(
        !offered.iter().any(|n| n.starts_with("browser.")),
        "{offered:?}"
    );
    assert!(
        env.registry
            .offered(Some(&web))
            .iter()
            .any(|t| t.name() == "browser.open")
    );
    let mut input = ChecklistInput::new("Api page");
    input.application = Some(api.id.0);
    let checklist = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        input,
        vec![StepDraft::new("browser.open").param("path", "/")],
    )
    .await?;
    let run = run_checklist(&env.env(), &checklist, "cli").await?;
    assert_eq!(run.status, RunStatus::Error, "{:#?}", run.steps);
    assert!(
        run.steps[0].message.contains("serves no UI"),
        "{}",
        run.steps[0].message
    );
    Ok(())
}
