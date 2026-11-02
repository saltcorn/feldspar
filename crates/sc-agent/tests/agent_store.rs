//! `_fd_agents` against a **real Postgres** (principle 4): the row is the
//! agent's definition, so what a save writes and a load reads back is the whole
//! of whether a configured agent survives a restart.
//!
//! Validation is tested here rather than in a unit test because it is not a
//! property of the [`Agent`] value — it is a question about the world the agent
//! is stored in: is that provider connected, does that trait's collection still
//! exist. Those are only real against a real catalog.

use crate::common;
use sc_types::Role;

use common::{Counter, catalog, registry};
use sc_agent::{
    Agent, AgentRegistry, Agents, EnabledTrait, delete_agent, list_agents, load_agent,
    load_agent_by_name, require_agent, save_agent, validate_agent,
};
use sc_error::Result;
use sc_test_harness::TestDb;

/// An agent with one `count` trait over `books`.
fn librarian() -> Agent {
    Agent::new("librarian", "main")
        .description("answers questions about books")
        .model("claude-opus-5")
        .system_prompt("You answer questions about books.")
        .with_trait(EnabledTrait::new("count").config("collection", "books"))
        .min_role(Role::of(40))
        .attribute(sc_agent::ATTR_MAX_STEPS, 5)
}

#[tokio::test]
async fn an_agent_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let agent = librarian();
    save_agent(&catalog, &registry, &agent).await?;

    let loaded = load_agent(&catalog, agent.id)
        .await?
        .expect("the row that was just written");
    assert_eq!(loaded, agent);

    // And by name, which is how a chat client and `run_agent` resolve it.
    assert_eq!(
        load_agent_by_name(&catalog, "librarian").await?.as_ref(),
        Some(&agent)
    );
    assert_eq!(require_agent(&catalog, "librarian").await?, agent);

    // The sparse attributes survive, which is what makes the step budget the
    // admin's rather than the code's.
    assert_eq!(loaded.max_steps(), 5);
    assert_eq!(loaded.min_role, Some(Role::of(40)));
    Ok(())
}

#[tokio::test]
async fn the_same_trait_twice_keeps_both_configurations_and_their_order() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let agent = Agent::new("counter", "main")
        .with_trait(EnabledTrait::new("count").config("collection", "orders"))
        .with_trait(EnabledTrait::new("count").config("collection", "books"));
    save_agent(&catalog, &registry, &agent).await?;

    let loaded = require_agent(&catalog, "counter").await?;
    assert_eq!(loaded.traits.len(), 2);
    // The order is the order the tools are offered in, so it is part of the
    // definition rather than an incidental of the storage.
    assert_eq!(loaded.traits[0].config["collection"], "orders");
    assert_eq!(loaded.traits[1].config["collection"], "books");
    Ok(())
}

#[tokio::test]
async fn an_edit_updates_in_place_and_a_delete_removes_it() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let mut agent = librarian();
    save_agent(&catalog, &registry, &agent).await?;
    agent.system_prompt = "You are terse.".to_owned();
    agent.model = None;
    save_agent(&catalog, &registry, &agent).await?;

    assert_eq!(list_agents(&catalog).await?.len(), 1);
    let loaded = require_agent(&catalog, "librarian").await?;
    assert_eq!(loaded.system_prompt, "You are terse.");
    // Cleared in the form, and cleared in the row: back to the provider's model.
    assert_eq!(loaded.model, None);

    assert!(delete_agent(&catalog, agent.id).await?);
    assert!(!delete_agent(&catalog, agent.id).await?);
    assert!(list_agents(&catalog).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_second_agent_may_not_take_a_name_already_in_use() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    save_agent(&catalog, &registry, &librarian()).await?;
    let clash = Agent::new("librarian", "main");
    let err = save_agent(&catalog, &registry, &clash).await.unwrap_err();
    assert!(err.to_string().contains("librarian"), "{err}");
    assert_eq!(list_agents(&catalog).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn an_agent_naming_a_provider_that_is_not_connected_is_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let err = save_agent(&catalog, &registry, &Agent::new("orphan", "openai"))
        .await
        .unwrap_err();
    // Named, so the admin knows it is the provider and not the agent.
    assert!(err.to_string().contains("openai"), "{err}");
    assert!(list_agents(&catalog).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_agent_whose_model_does_not_resolve_says_which_is_missing() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    // A model the provider has no row for.
    let err = save_agent(
        &catalog,
        &registry,
        &Agent::new("a", "main").model("gpt-5.1"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("agent `a`"), "{err}");
    assert!(err.contains("no model named `gpt-5.1`"), "{err}");

    // A provider with models but no default, and an agent that names none.
    let bare = sc_llm::LlmProviderDef::new("bare", sc_llm::ANTHROPIC_BACKEND)
        .with(sc_llm::CFG_API_KEY, "sk-x");
    sc_llm::save_llm_provider(&catalog, &bare).await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(bare.id, "claude-haiku-4-5"),
    )
    .await?;
    let err = save_agent(&catalog, &registry, &Agent::new("b", "bare"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no default model"), "{err}");
    // Naming the model it does have is fine.
    save_agent(
        &catalog,
        &registry,
        &Agent::new("b", "bare").model("claude-haiku-4-5"),
    )
    .await?;

    // A model deleted later drops the agent from the live set, with its reason.
    let model = sc_llm::require_llm_model(&catalog, &bare, Some("claude-haiku-4-5")).await?;
    sc_llm::delete_llm_model(&catalog, model.id, &[]).await?;
    let agents = Agents::load(&catalog, &registry).await?;
    let issue = agents
        .issues()
        .iter()
        .find(|i| i.agent == "b")
        .expect("the agent whose model went");
    assert!(
        issue.problem.contains("claude-haiku-4-5"),
        "{}",
        issue.problem
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_trait_and_a_bad_configuration_are_both_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    // A trait nothing implements.
    let agent = Agent::new("a", "main").with_trait(EnabledTrait::new("query_the_web"));
    let err = save_agent(&catalog, &registry, &agent).await.unwrap_err();
    assert!(err.to_string().contains("query_the_web"), "{err}");

    // A configuration the trait's spec does not declare: `collection` is
    // required and missing.
    let agent = Agent::new("a", "main").with_trait(EnabledTrait::new("count"));
    let err = save_agent(&catalog, &registry, &agent).await.unwrap_err();
    assert!(err.to_string().contains("collection"), "{err}");

    // A configuration the spec accepts but the *trait* rejects — the check only
    // the trait can make.
    let agent =
        Agent::new("a", "main").with_trait(EnabledTrait::new("count").config("collection", "cars"));
    let err = save_agent(&catalog, &registry, &agent).await.unwrap_err();
    assert!(err.to_string().contains("cars"), "{err}");
    // And it says *which* enabled trait, because one may be enabled many times.
    assert!(err.to_string().contains("trait 1"), "{err}");
    Ok(())
}

#[tokio::test]
async fn two_traits_whose_tools_would_collide_are_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    // Both derive the tool name `count_books`. Refused here, where it is
    // fixable, rather than discovered when the model picks the wrong one.
    let agent = Agent::new("a", "main")
        .with_trait(EnabledTrait::new("count").config("collection", "books"))
        .with_trait(EnabledTrait::new("count").config("collection", "books"));
    let err = save_agent(&catalog, &registry, &agent).await.unwrap_err();
    assert!(err.to_string().contains("count_books"), "{err}");
    assert!(err.to_string().contains("trait 1"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_agent_whose_world_changed_leaves_the_live_set_with_its_reason() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let counter = Counter::new();
    let registry = registry(counter)?;

    save_agent(&catalog, &registry, &librarian()).await?;
    save_agent(
        &catalog,
        &registry,
        &Agent::new("plain", "main").description("no traits at all"),
    )
    .await?;

    // Both are live while the world matches what they were saved against.
    let agents = Agents::load(&catalog, &registry).await?;
    assert_eq!(agents.all().len(), 2);
    assert!(agents.issues().is_empty());
    assert!(agents.by_name("librarian").is_some());

    // Now the plugin that provided `count` is gone — the same shape as a dropped
    // table or a deleted provider, and the case validation-on-load exists for.
    let empty = AgentRegistry::new();
    let agents = Agents::load(&catalog, &empty).await?;
    assert_eq!(agents.all().len(), 1);
    assert!(agents.by_name("librarian").is_none());
    assert_eq!(agents.issues().len(), 1);
    let issue = &agents.issues()[0];
    assert_eq!(issue.agent, "librarian");
    assert!(issue.problem.contains("count"), "{}", issue.problem);

    // It is dropped from the live set but **not** from storage: it stays listed
    // and editable, because editing it is the repair.
    assert_eq!(list_agents(&catalog).await?.len(), 2);

    // And asking for it says why it cannot run, rather than that it does not
    // exist — a distinction that sends the admin to the right place.
    let err = agents.require("librarian").unwrap_err();
    assert!(err.to_string().contains("not usable"), "{err}");
    let err = agents.require("nobody").unwrap_err();
    assert!(err.to_string().contains("no agent named"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_agent_with_no_traits_at_all_is_valid() -> Result<()> {
    // The Phase 2 "done when": an agent that only talks. It must not need a
    // trait to be a legal definition.
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let agent = Agent::new("talker", "main").system_prompt("You chat.");
    validate_agent(&catalog, &registry, &agent).await?;
    save_agent(&catalog, &registry, &agent).await?;
    assert!(require_agent(&catalog, "talker").await?.traits.is_empty());
    Ok(())
}
