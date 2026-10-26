//! Agent metering against the LLM credits ledger (TODO-payments P6.3), end to
//! end on the scripted provider, a real `_fd_runs` and real credit tables.
//!
//! Every scripted call reports 10 input and 5 output tokens, and the agent
//! here charges 100 credits per 1,000 input tokens and 200 per 1,000 output:
//! **2 credits a call**, whatever the model's prices.

use crate::common;

use std::sync::Arc;

use common::{Counter, catalog, registry};
use rust_decimal::Decimal;
use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_agent::{
    ATTR_CREDITS_MIN_BALANCE, ATTR_CREDITS_PER_1K_INPUT, ATTR_CREDITS_PER_1K_OUTPUT,
    ATTR_CREDITS_PER_DOLLAR, Agent, Budget, Conclusion, EnabledTrait, ModelRole, ProviderConnector,
    RunCaller, Runner, save_agent,
};
use sc_auth::{NewCreditGrant, Role, User};
use sc_catalog::Catalog;
use sc_error::Result;
use sc_test_harness::TestDb;
use serde_json::json;

fn metered_agent() -> Agent {
    Agent::new("builder", "main")
        .system_prompt("You build things.")
        .with_trait(EnabledTrait::new("tally"))
        // Anybody may use it: a session of it is checked against the caller.
        .min_role(100)
        .attribute(ATTR_CREDITS_PER_1K_INPUT, 100)
        .attribute(ATTR_CREDITS_PER_1K_OUTPUT, 200)
}

/// The agent catalog, plus the users and credit tables and one user at `role`.
async fn with_user(db: &TestDb, role: u8) -> Result<(Catalog, User)> {
    let catalog = catalog(db).await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_auth::save_role(&catalog, &Role::new(80, "Member")).await?;
    let user = sc_auth::create_user(&catalog, "payer@example.com", "pw", role).await?;
    Ok((catalog, user))
}

async fn grant(catalog: &Catalog, user: &User, amount: i64, key: &str) -> Result<()> {
    sc_auth::grant_credits(
        catalog,
        NewCreditGrant {
            user_id: user.id,
            amount: Decimal::from(amount),
            source: "test".into(),
            key: key.into(),
            kind: String::new(),
            expires_at: None,
            note: None,
            rollover: None,
        },
        None,
    )
    .await?;
    Ok(())
}

async fn balance(catalog: &Catalog, user: &User) -> Result<Decimal> {
    Ok(sc_auth::credit_balance(catalog, user.id).await?.balance)
}

async fn debits(catalog: &Catalog, user: &User) -> Result<Vec<sc_auth::CreditEntry>> {
    Ok(sc_auth::list_credit_entries(catalog, user.id, 100)
        .await?
        .into_iter()
        .filter(|e| e.kind == sc_auth::ENTRY_DEBIT)
        .collect())
}

const OUT_OF_CREDITS: Conclusion = Conclusion::OverBudget {
    budget: Budget::Credits,
};

#[tokio::test]
async fn a_metered_run_debits_each_call_and_stops_when_the_caller_runs_out() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, user) = with_user(&db, 80).await?;
    let registry = registry(Counter::new())?;
    let agent = metered_agent();
    grant(&catalog, &user, 5, "start").await?;

    // 5 → 3 → 1 → -1: the third call takes it into debt, because the call had
    // already been made, and the run stops before a fourth.
    let provider = Arc::new(FakeProvider::repeating(
        Reply::calls("tally", json!({})),
        10,
    ));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::user(user.clone()),
    );
    let (mut run, conclusion) = runner.start("count for ever").await?;
    assert_eq!(conclusion, OUT_OF_CREDITS);
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(-1));

    // One entry per call, keyed by the run and the step, with what was used.
    let mut entries = debits(&catalog, &user).await?;
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
    assert_eq!(
        keys,
        (1..=3)
            .map(|s| format!("{}:{s}", run.id))
            .collect::<Vec<_>>()
    );
    for entry in &entries {
        assert_eq!(entry.amount, Decimal::from(-2));
        assert_eq!(entry.run, Some(run.id.0));
        assert_eq!(entry.agent.as_deref(), Some("builder"));
        assert!(
            entry
                .model
                .as_deref()
                .is_some_and(|m| m.starts_with("fake"))
        );
        assert_eq!(
            (entry.input_tokens, entry.output_tokens),
            (Some(10), Some(5))
        );
    }

    // In debt, the next turn stops before it calls the model…
    let idle = Arc::new(FakeProvider::new([Reply::says("unreachable")]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(idle.clone()),
        RunCaller::user(user.clone()),
    );
    assert_eq!(runner.continue_run(&mut run, "more").await?, OUT_OF_CREDITS);
    assert!(idle.requests().is_empty());

    // …and after a top-up, which repays the debt first, it carries on.
    grant(&catalog, &user, 10, "top-up").await?;
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(9));
    let answer = Arc::new(FakeProvider::new([Reply::says("done")]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(answer),
        RunCaller::user(user.clone()),
    );
    let conclusion = runner.continue_run(&mut run, "finish").await?;
    assert_eq!(conclusion.answer(), Some("done"));
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(7));
    Ok(())
}

#[tokio::test]
async fn a_run_needs_the_minimum_balance_to_start() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, user) = with_user(&db, 80).await?;
    let registry = registry(Counter::new())?;
    let agent = metered_agent().attribute(ATTR_CREDITS_MIN_BALANCE, 5);
    grant(&catalog, &user, 3, "some").await?;

    let provider = Arc::new(FakeProvider::new([Reply::says("never")]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::user(user.clone()),
    );
    let (_, conclusion) = runner.start("hello").await?;
    assert_eq!(conclusion, OUT_OF_CREDITS);
    assert!(provider.requests().is_empty(), "no model call was made");
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(3));

    // With the minimum it starts, and may then run below it.
    grant(&catalog, &user, 2, "more").await?;
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("tally", json!({})),
        Reply::says("done"),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider),
        RunCaller::user(user.clone()),
    );
    let (_, conclusion) = runner.start("hello").await?;
    assert_eq!(conclusion.answer(), Some("done"));
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(1));
    Ok(())
}

#[tokio::test]
async fn an_anonymous_caller_is_refused_and_an_admin_is_never_charged() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, admin) = with_user(&db, 1).await?;
    let registry = registry(Counter::new())?;
    let agent = metered_agent();

    let anonymous = RunCaller::anonymous();
    let provider = Arc::new(FakeProvider::new([Reply::says("never")]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        anonymous,
    );
    let err = runner.start("hello").await.unwrap_err();
    assert!(err.to_string().contains("signed-in user"), "{err}");
    assert!(provider.requests().is_empty());

    // An admin with no credits at all — the copilot's caller — and a
    // trigger's run are answered, and nothing is charged.
    for caller in [RunCaller::user(admin.clone()), RunCaller::system()] {
        let provider = Arc::new(FakeProvider::new([Reply::says("free")]));
        let runner = Runner::new(&catalog, &registry, &agent, common::model(provider), caller);
        let (_, conclusion) = runner.start("hello").await?;
        assert_eq!(conclusion.answer(), Some("free"));
    }
    assert!(debits(&catalog, &admin).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn sub_agents_and_a_tools_own_calls_are_charged_to_the_same_caller() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, user) = with_user(&db, 80).await?;
    let registry = registry(Counter::new())?;
    let agent = metered_agent();
    save_agent(&catalog, &registry, &agent).await?;
    grant(&catalog, &user, 20, "plenty").await?;

    // The parent asks the cheap role through a tool, then starts a session
    // of itself; the session answers, and so does the parent: five calls.
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("aside", json!({})),
        Reply::says("hi"),
        Reply::calls("start_session", json!({"mode": "explore"})),
        Reply::says("found it"),
        Reply::says("all done"),
    ]));
    let models: Arc<dyn ProviderConnector> =
        Arc::new(FakeModels::new().role(ModelRole::Executor, provider.clone()));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::user(user.clone()),
    )
    .with_connector(&models);
    let (run, conclusion) = runner.start("build it").await?;
    assert_eq!(conclusion.answer(), Some("all done"));
    assert_eq!(provider.requests().len(), 5);
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(10));

    let entries = debits(&catalog, &user).await?;
    assert_eq!(entries.len(), 5);
    assert_eq!(
        entries.iter().filter(|e| e.key.contains(":aside:")).count(),
        1,
        "the tool's own call"
    );
    let runs: std::collections::BTreeSet<_> = entries.iter().filter_map(|e| e.run).collect();
    assert_eq!(runs.len(), 2, "the parent's run and the session's");
    assert!(runs.contains(&run.id.0));
    Ok(())
}

#[tokio::test]
async fn a_rate_per_dollar_is_refused_until_every_model_has_a_price() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, _) = with_user(&db, 80).await?;
    let registry = registry(Counter::new())?;
    let agent = Agent::new("builder", "main")
        .with_trait(EnabledTrait::new("tally"))
        .attribute(ATTR_CREDITS_PER_DOLLAR, 20);
    let err = save_agent(&catalog, &registry, &agent).await.unwrap_err();
    assert!(err.to_string().contains("LLM credits per dollar"), "{err}");

    // A rate per token beside it is the fallback, so it saves.
    let fallback = agent.clone().attribute(ATTR_CREDITS_PER_1K_OUTPUT, 1);
    save_agent(&catalog, &registry, &fallback).await?;
    Ok(())
}

#[tokio::test]
async fn a_tools_image_call_is_ledgered_and_charged_at_its_own_cost() -> Result<()> {
    // TODO-agent-skills AS3.5.
    let db = TestDb::new().await?;
    let (catalog, user) = with_user(&db, 80).await?;
    let registry = registry(Counter::new())?;
    // 100 credits a dollar for a call with a cost, the token rates otherwise.
    let agent = metered_agent().attribute(ATTR_CREDITS_PER_DOLLAR, 100);
    grant(&catalog, &user, 20, "plenty").await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("media", json!({"cost": 0.04})),
        Reply::calls("media", json!({"input_tokens": 20})),
        Reply::says("two pictures"),
    ]));
    let models: Arc<dyn ProviderConnector> =
        Arc::new(FakeModels::new().role(ModelRole::Executor, provider.clone()));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::user(user.clone()),
    )
    .with_connector(&models);
    let (run, conclusion) = runner.start("draw twice").await?;
    assert_eq!(conclusion.answer(), Some("two pictures"));

    // Three chat calls at 2 credits; the priced image at 0.04 × 100 = 4; the
    // unpriced one by its 20 input tokens at 100 a thousand = 2.
    assert_eq!(balance(&catalog, &user).await?, Decimal::from(8));
    let images: Vec<_> = debits(&catalog, &user)
        .await?
        .into_iter()
        .filter(|e| e.key.contains(":image:"))
        .collect();
    assert_eq!(images.len(), 2);
    let mut amounts: Vec<Decimal> = images.iter().map(|e| e.amount).collect();
    amounts.sort();
    assert_eq!(amounts, [Decimal::from(-4), Decimal::from(-2)]);
    assert!(
        images
            .iter()
            .all(|e| e.model.as_deref() == Some("gpt-image-1") && e.run == Some(run.id.0))
    );

    // Both are asides of the run's ledger, marked as image calls.
    let stored = sc_agent::require_run(&catalog, run.id).await?;
    let state = stored.agent_loop()?;
    let asides = state.ledger().asides();
    assert_eq!(asides.len(), 2);
    assert!(asides.iter().all(|a| a.kind == sc_llm::ModelKind::Image));
    assert_eq!(
        asides.iter().filter_map(|a| a.cost).collect::<Vec<_>>(),
        [0.04]
    );
    Ok(())
}
