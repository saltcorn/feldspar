//! Context an agent starts from (TODO-agent-skills AS8): `preload_rows`,
//! `preload_code`, `memory` (with its store and `consolidate_memory`) and
//! `skill_file`, against a real database, the real JavaScript runtime and a
//! real file store.
//!
//! The one property everything else here could be traded for, and is pinned
//! three ways — by the store's rule, by the trait's tools, and through whole
//! runs: **a personal memory never reaches another user's run**.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use common::{Env, as_user, config, reader};
use sc_action::{Action, ActionContext, Event, EventKind};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, EnabledTrait, ProviderConnector, RunCaller, RunId, Runner, SessionContext, Turn,
    save_agent,
};
use sc_catalog::Catalog;
use sc_core_actions::builtin_actions;
use sc_core_traits::memory::{
    KIND_PREFERENCES, MemoryFilter, MemoryReader, NewMemory, list_memories, preferences_of,
    replace_preferences, store_memory,
};
use sc_core_traits::{
    CFG_CONTENTS, CFG_FIELDS, CFG_FILTER, CFG_HEADING, CFG_PERSONAL, CFG_PLACEMENT,
    CFG_PRELOAD_CODE, CFG_READ_TOOL, CFG_ROOT, CFG_SKILL_FILE, CFG_STORE, CFG_TABLE,
    ConsolidateMemory, PLACEMENT_SYSTEM, TOOL_SEARCH_MEMORY, TOOL_STORE_MEMORY,
    bootstrap_agent_memory, builtin_traits,
};
use sc_error::Result;
use sc_llm::{ConnectedModel, LlmProvider};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

// --- helpers -------------------------------------------------------------------

/// What a trait puts in the session header, and the run state it leaves, for a
/// run on behalf of `caller` started by `row` (if any).
async fn header(
    env: &Env,
    trait_: &str,
    cfg: &Attrs,
    caller: &RunCaller,
    row: Option<&Json>,
) -> Result<(Option<String>, Json)> {
    let trait_ = env.registry.require(trait_)?.clone();
    let mut state = Json::Null;
    let mut cx = SessionContext {
        catalog: &env.catalog,
        caller,
        agent: "librarian",
        run: RunId::new(),
        mode: sc_agent::RunMode::Act,
        brief: "What should I read?",
        trait_state: &mut state,
        evaluator: env.evaluator.as_ref(),
        triggers: env.dispatcher.as_ref(),
        row,
    };
    let text = trait_.session_header(cfg, &mut cx).await?;
    Ok((text, state))
}

/// What `on_turn` appends to the system prompt, given the run state.
async fn appended(env: &Env, trait_: &str, cfg: &Attrs, state: &Json) -> Result<Vec<String>> {
    let trait_ = env.registry.require(trait_)?.clone();
    let caller = RunCaller::system();
    let mut turn = Turn::new(&caller, "librarian", 2);
    turn.state = Some(state);
    trait_.on_turn(cfg, &mut turn).await?;
    Ok(turn.extra_system().to_vec())
}

// --- preload_rows ----------------------------------------------------------------

fn preload(filter: &str) -> Attrs {
    config(&[
        (CFG_TABLE, json!("books")),
        (CFG_FILTER, json!(filter)),
        (CFG_FIELDS, json!(["title", "pages", "owner"])),
        (CFG_CONTENTS, json!("{{title}} ({{pages}} pages)")),
        (CFG_HEADING, json!("Your books:")),
    ])
}

#[tokio::test]
async fn preload_rows_puts_the_users_rows_in_the_session_header() -> Result<()> {
    let env = Env::new().await?.with_engine();
    // Every reader may read every book: what narrows it to Ada's is the filter.
    env.own("books", "pages > 0").await?;
    let cfg = preload("{ owner: user.email }");
    env.check("preload_rows", &cfg).await?;
    // No tool: the rows arrive with the session.
    assert!(env.tools("preload_rows", &cfg).is_empty());

    let (text, state) = header(&env, "preload_rows", &cfg, &as_user("ada@example.com"), None).await?;
    let text = text.expect("a header");
    assert!(text.starts_with("Your books:"), "{text}");
    assert!(text.contains("Dune (412 pages)"), "{text}");
    assert!(text.contains("Ilium (576 pages)"), "{text}");
    assert!(!text.contains("Emma"), "{text}");
    // Kept nothing for the system prompt.
    assert!(state.is_null());
    Ok(())
}

#[tokio::test]
async fn preload_rows_reads_as_the_caller() -> Result<()> {
    let env = Env::new().await?.with_engine();
    // Every row, as far as the filter goes — but the table only shows a
    // reader their own, and the trait reads as the reader.
    env.own("books", "owner === user.email").await?;
    let cfg = preload("");
    let (text, _) = header(&env, "preload_rows", &cfg, &as_user("bob@example.com"), None).await?;
    let text = text.unwrap();
    assert!(text.contains("Emma") && text.contains("Ubik"), "{text}");
    assert!(!text.contains("Dune"), "{text}");
    Ok(())
}

#[tokio::test]
async fn preload_rows_filters_on_the_triggering_row_into_the_system_prompt() -> Result<()> {
    let env = Env::new().await?.with_engine();
    let mut cfg = preload("row ? { owner: row.owner } : { title: 'none' }");
    cfg.insert(CFG_PLACEMENT.to_owned(), json!(PLACEMENT_SYSTEM));
    let row = json!({ "id": 9, "owner": "bob@example.com" });
    let (text, state) = header(&env, "preload_rows", &cfg, &RunCaller::triggered(), Some(&row)).await?;
    // Nothing in the header: it went to the system prompt, kept in the run
    // state and appended on every step.
    assert!(text.is_none());
    let extra = appended(&env, "preload_rows", &cfg, &state).await?;
    assert_eq!(extra.len(), 1);
    assert!(extra[0].contains("Emma (474 pages)") && !extra[0].contains("Dune"), "{extra:?}");
    // In a chat no row started, `row` is null rather than unbound.
    let (text, _) = header(&env, "preload_rows", &preload("row ? { owner: row.owner } : { title: 'Dune' }"), &RunCaller::system(), None).await?;
    let text = text.unwrap();
    assert!(text.contains("Dune") && !text.contains("Emma"), "{text}");
    Ok(())
}

#[tokio::test]
async fn preload_rows_refuses_what_it_could_not_preload_on_save() -> Result<()> {
    let env = Env::new().await?;
    let mut bad = preload("");
    bad.insert(CFG_CONTENTS.to_owned(), json!("{{title}} by {{author}}"));
    let err = env.check("preload_rows", &bad).await.unwrap_err();
    assert!(err.to_string().contains("author"), "{err}");
    let mut bad = preload("");
    bad.insert(CFG_PLACEMENT.to_owned(), json!("footnote"));
    assert!(env.check("preload_rows", &bad).await.is_err());
    let err = env
        .check("preload_rows", &preload("{ owner: "))
        .await
        .unwrap_err();
    assert!(err.to_string().contains(CFG_FILTER), "{err}");
    Ok(())
}

// --- preload_code ------------------------------------------------------------------

#[tokio::test]
async fn preload_code_runs_once_as_the_caller_and_says_where_its_text_goes() -> Result<()> {
    let env = Env::new().await?.with_triggers(builtin_actions()?)?;
    let cfg = config(&[(
        CFG_PRELOAD_CODE,
        json!(
            "const rows = await db.books.where({ owner: user.email }).orderBy(\"id\").rows(); \
             return `${user.email} asked \"${prompt}\" and owns ${rows.map(r => r.title).join(', ')}`"
        ),
    )]);
    env.check("preload_code", &cfg).await?;
    let (text, _) = header(&env, "preload_code", &cfg, &as_user("ada@example.com"), None).await?;
    assert_eq!(
        text.as_deref(),
        Some("ada@example.com asked \"What should I read?\" and owns Dune, Ilium")
    );

    // An object says where each part goes itself.
    let cfg = config(&[(
        CFG_PRELOAD_CODE,
        json!("return { system_prompt: 'Answer in French.', messages: ['Today is Monday.'] }"),
    )]);
    let (text, state) = header(&env, "preload_code", &cfg, &RunCaller::system(), None).await?;
    assert_eq!(text.as_deref(), Some("Today is Monday."));
    assert_eq!(appended(&env, "preload_code", &cfg, &state).await?, ["Answer in French."]);

    // A body that throws fails the session, naming the trait.
    let cfg = config(&[(CFG_PRELOAD_CODE, json!("throw new Error('no')"))]);
    let err = header(&env, "preload_code", &cfg, &RunCaller::system(), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("preload_code"), "{err}");
    Ok(())
}

// --- the memory store and trait ----------------------------------------------------

async fn memory_env() -> Result<Env> {
    let env = Env::new().await?;
    bootstrap_agent_memory(&env.catalog).await?;
    // Twice is a no-op, constraint and all.
    let table = bootstrap_agent_memory(&env.catalog).await?;
    assert!(sc_catalog::full_text_constraint(&table).is_some());
    Ok(env)
}

fn memory(personal: bool) -> Attrs {
    config(&[(CFG_PERSONAL, json!(personal))])
}

async fn search(env: &Env, cfg: &Attrs, caller: &RunCaller, query: &str) -> Result<Vec<String>> {
    let found = env
        .call_tool("memory", cfg, TOOL_SEARCH_MEMORY, json!({ "query": query }), caller)
        .await?;
    Ok(found["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["contents"].as_str().unwrap().to_owned())
        .collect())
}

#[tokio::test]
async fn a_personal_memory_is_only_read_back_for_its_own_user() -> Result<()> {
    let env = memory_env().await?;
    let personal = memory(true);
    env.check("memory", &personal).await?;
    let ada = RunCaller::user(reader("ada@example.com"));
    let bob = RunCaller::user(reader("bob@example.com"));

    env.call_tool(
        "memory",
        &personal,
        TOOL_STORE_MEMORY,
        json!({ "contents": "Ada drinks Earl Grey tea", "topic": "drinks" }),
        &ada,
    )
    .await?;
    // Ada finds it; Bob, a triggered run and an anonymous visitor do not.
    assert_eq!(search(&env, &personal, &ada, "tea").await?, ["Ada drinks Earl Grey tea"]);
    assert!(search(&env, &personal, &bob, "tea").await?.is_empty());
    assert!(search(&env, &personal, &RunCaller::triggered(), "tea").await?.is_empty());
    assert!(search(&env, &personal, &RunCaller::anonymous(), "tea").await?.is_empty());

    // A shared memory is everybody's.
    let shared = memory(false);
    env.call_tool(
        "memory",
        &shared,
        TOOL_STORE_MEMORY,
        json!({ "contents": "The tea room closes at five" }),
        &bob,
    )
    .await?;
    assert_eq!(search(&env, &personal, &bob, "tea").await?, ["The tea room closes at five"]);
    let mut for_ada = search(&env, &personal, &ada, "tea").await?;
    for_ada.sort();
    assert_eq!(for_ada, ["Ada drinks Earl Grey tea", "The tea room closes at five"]);

    // Nobody to file it under, nothing personal stored.
    let err = env
        .call_tool(
            "memory",
            &personal,
            TOOL_STORE_MEMORY,
            json!({ "contents": "a secret" }),
            &RunCaller::anonymous(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nobody"), "{err}");
    // And the store refuses one even when asked directly.
    assert!(
        store_memory(
            &env.catalog,
            &NewMemory {
                agent: "librarian".into(),
                user: None,
                run: None,
                kind: "memory".into(),
                personal: true,
                topic: String::new(),
                contents: "orphan".into(),
            },
        )
        .await
        .is_err()
    );

    // Another agent's memories are another agent's.
    let mut filter = MemoryFilter::new(MemoryReader::Admin, 100);
    filter.agent = Some("someone_else".into());
    assert!(list_memories(&env.catalog, &filter).await?.is_empty());
    // The admin sees everything, and can search it.
    let mut filter = MemoryFilter::new(MemoryReader::Admin, 100);
    filter.search = Some("Earl Grey".into());
    let found = list_memories(&env.catalog, &filter).await?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].topic, "drinks");
    assert_eq!(found[0].user, ada.user.as_ref().map(|u| u.id));
    Ok(())
}

#[tokio::test]
async fn consolidated_preferences_reach_only_their_users_session() -> Result<()> {
    let env = memory_env().await?;
    let ada = RunCaller::user(reader("ada@example.com"));
    let bob = RunCaller::user(reader("bob@example.com"));
    let ada_id = ada.user.as_ref().unwrap().id;
    replace_preferences(&env.catalog, "librarian", ada_id, "Ada likes short answers.").await?;
    let cfg = memory(true);
    let (text, _) = header(&env, "memory", &cfg, &ada, None).await?;
    assert!(text.unwrap().contains("Ada likes short answers."));
    assert_eq!(header(&env, "memory", &cfg, &bob, None).await?.0, None);
    assert_eq!(header(&env, "memory", &cfg, &RunCaller::system(), None).await?.0, None);
    // Switched off, not even Ada's.
    let off = config(&[(sc_core_traits::CFG_PREFERENCES_IN_HEADER, json!(false))]);
    assert_eq!(header(&env, "memory", &off, &ada, None).await?.0, None);
    // Replacing keeps one.
    replace_preferences(&env.catalog, "librarian", ada_id, "Ada likes long answers.").await?;
    let mut filter = MemoryFilter::new(MemoryReader::Admin, 100);
    filter.kind = Some(KIND_PREFERENCES.into());
    assert_eq!(list_memories(&env.catalog, &filter).await?.len(), 1);
    Ok(())
}

// --- whole runs ----------------------------------------------------------------------

/// A connector that hands out one script per connection and keeps every
/// provider it made, so a test can read what each run was sent.
struct Scripted {
    scripts: Mutex<Vec<Vec<Reply>>>,
    made: Mutex<Vec<Arc<FakeProvider>>>,
}

impl Scripted {
    fn new(scripts: Vec<Vec<Reply>>) -> Arc<Scripted> {
        Arc::new(Scripted {
            scripts: Mutex::new(scripts.into_iter().rev().collect()),
            made: Mutex::new(Vec::new()),
        })
    }

    fn made(&self) -> Vec<Arc<FakeProvider>> {
        self.made.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ProviderConnector for Scripted {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        let script = self.scripts.lock().unwrap().pop().unwrap_or_default();
        let provider = Arc::new(FakeProvider::new(script));
        self.made.lock().unwrap().push(Arc::clone(&provider));
        Ok(ConnectedModel::unconfigured(provider as Arc<dyn LlmProvider>))
    }
}

fn librarian() -> Agent {
    Agent::new("librarian", "main")
        .system_prompt("You help readers.")
        .with_trait(EnabledTrait::new("memory").config(CFG_PERSONAL, true))
}

/// Run `librarian` once as `caller`, answered by `script`; what it was sent.
async fn run_once(env: &Env, caller: RunCaller, script: Vec<Reply>, says: &str) -> Result<String> {
    let provider = Arc::new(FakeProvider::new(script));
    let agent = librarian();
    let runner = Runner::new(
        &env.catalog,
        &env.registry,
        &agent,
        ConnectedModel::unconfigured(Arc::clone(&provider) as Arc<dyn LlmProvider>),
        caller,
    );
    runner.start(says).await?;
    Ok(format!("{:?}", provider.requests()))
}

#[tokio::test]
async fn a_personal_memory_never_reaches_another_users_run() -> Result<()> {
    let env = memory_env().await?;
    let ada = RunCaller::user(reader("ada@example.com"));
    let bob = RunCaller::user(reader("bob@example.com"));

    // Ada tells the agent something about herself, and it remembers.
    run_once(
        &env,
        ada.clone(),
        vec![
            Reply::calls(
                TOOL_STORE_MEMORY,
                json!({ "contents": "Ada is allergic to penicillin", "topic": "health" }),
            ),
            Reply::says("Noted."),
        ],
        "Remember that I am allergic to penicillin.",
    )
    .await?;

    // Bob's run searches for it, and is told nothing.
    let sent = run_once(
        &env,
        bob.clone(),
        vec![
            Reply::calls(TOOL_SEARCH_MEMORY, json!({ "query": "penicillin allergic" })),
            Reply::says("I don't know."),
        ],
        "Who is allergic to penicillin?",
    )
    .await?;
    assert!(!sent.contains("Ada is allergic"), "{sent}");
    assert!(sent.contains("\\\"count\\\":0") || sent.contains("\"count\":0"), "{sent}");

    // Ada's next run finds it.
    let sent = run_once(
        &env,
        ada.clone(),
        vec![
            Reply::calls(TOOL_SEARCH_MEMORY, json!({ "query": "penicillin" })),
            Reply::says("You are allergic to penicillin."),
        ],
        "What am I allergic to?",
    )
    .await?;
    assert!(sent.contains("Ada is allergic to penicillin"), "{sent}");

    // Consolidated into preferences, it starts Ada's sessions — and not Bob's.
    replace_preferences(
        &env.catalog,
        "librarian",
        ada.user.as_ref().unwrap().id,
        "Allergic to penicillin.",
    )
    .await?;
    let for_bob = run_once(&env, bob, vec![Reply::says("Hello.")], "Hi").await?;
    assert!(!for_bob.contains("penicillin"), "{for_bob}");
    let for_ada = run_once(&env, ada, vec![Reply::says("Hello.")], "Hi").await?;
    assert!(for_ada.contains("Allergic to penicillin."), "{for_ada}");
    Ok(())
}

#[tokio::test]
async fn consolidate_memory_summarises_each_users_own_memories() -> Result<()> {
    let env = memory_env().await?;
    let ada = reader("ada@example.com");
    let bob = reader("bob@example.com");
    save_agent(&env.catalog, &env.registry, &librarian()).await?;
    for (user, text) in [
        (&ada, "Ada prefers tea"),
        (&ada, "Ada reads science fiction"),
        (&bob, "Bob prefers coffee"),
    ] {
        store_memory(
            &env.catalog,
            &NewMemory {
                agent: "librarian".into(),
                user: Some(user.id),
                run: None,
                kind: "memory".into(),
                personal: true,
                topic: String::new(),
                contents: text.into(),
            },
        )
        .await?;
    }
    // One cheap-role connection, answering once per user.
    let providers = Scripted::new(vec![vec![
        Reply::says("PROFILE ONE"),
        Reply::says("PROFILE TWO"),
    ]]);
    let action = ConsolidateMemory::new(
        Arc::new(builtin_traits()?),
        Arc::clone(&providers) as Arc<dyn ProviderConnector>,
    );
    let cfg = config(&[("agent", json!("librarian"))]);
    let event = Event::new(EventKind::None);
    let mut ctx = ActionContext::new(&env.catalog, &event, &cfg, "nightly");
    let result = action.run(&mut ctx).await?;
    assert_eq!(result["consolidated"].as_array().unwrap().len(), 2);

    // Each request carried one user's memories, never the other's.
    let made = providers.made();
    assert_eq!(made.len(), 1);
    let requests = made[0].requests();
    assert_eq!(requests.len(), 2);
    let mut profiles = Vec::new();
    for (request, profile) in requests.iter().zip(["PROFILE ONE", "PROFILE TWO"]) {
        let sent = format!("{:?}", request.messages);
        let about_ada = sent.contains("Ada prefers tea");
        assert_eq!(about_ada, sent.contains("science fiction"), "{sent}");
        assert_ne!(about_ada, sent.contains("Bob prefers coffee"), "{sent}");
        profiles.push((if about_ada { ada.id } else { bob.id }, profile));
    }
    for (user, profile) in profiles {
        let preferences = preferences_of(&env.catalog, "librarian", user).await?.unwrap();
        assert_eq!(preferences.contents, profile);
    }
    Ok(())
}

// --- skill_file ------------------------------------------------------------------------

const SKILL: &str = "---\nname: pdf\ndescription: Fill in PDF forms\n---\n\n# PDF forms\n\n\
                     Read [the form guide](forms/FORM.md) before filling anything in.\n";

#[tokio::test]
async fn a_skill_file_is_in_the_header_and_its_files_are_readable() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("skills", None).await?;
    env.put(&dir, "pdf/SKILL.md", SKILL)?;
    env.put(&dir, "pdf/forms/FORM.md", "Use black ink.")?;
    env.put(&dir, "secret.txt", "not part of the skill")?;
    let cfg = config(&[
        (CFG_STORE, json!("skills")),
        (CFG_ROOT, json!("pdf")),
        (CFG_SKILL_FILE, json!("SKILL.md")),
        (CFG_READ_TOOL, json!(true)),
    ]);
    env.check("skill_file", &cfg).await?;
    let tools = env.tools("skill_file", &cfg);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "read_skill_file_skills_pdf");

    let caller = as_user("ada@example.com");
    let (text, _) = header(&env, "skill_file", &cfg, &caller, None).await?;
    let text = text.unwrap();
    assert!(text.contains("# PDF forms"), "{text}");
    assert!(text.contains("`forms/FORM.md`"), "{text}");

    let read = env
        .call_tool("skill_file", &cfg, "read_skill_file_skills_pdf", json!({ "path": "forms/FORM.md" }), &caller)
        .await?;
    assert_eq!(read["contents"], json!("Use black ink."));
    // Nothing outside the skill's directory.
    assert!(
        env.call_tool("skill_file", &cfg, "read_skill_file_skills_pdf", json!({ "path": "../secret.txt" }), &caller)
            .await
            .is_err()
    );

    // Without the tool, the header does not offer it.
    let mut quiet = cfg.clone();
    quiet.insert(CFG_READ_TOOL.to_owned(), json!(false));
    assert!(env.tools("skill_file", &quiet).is_empty());
    let (text, _) = header(&env, "skill_file", &quiet, &caller, None).await?;
    assert!(!text.unwrap().contains("read_skill_file"));

    // A missing skill fails the session, by name.
    let mut missing = cfg.clone();
    missing.insert(CFG_SKILL_FILE.to_owned(), json!("NOPE.md"));
    let err = header(&env, "skill_file", &missing, &caller, None).await.unwrap_err();
    assert!(err.to_string().contains("NOPE.md"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_skill_in_a_store_the_caller_cannot_open_is_refused() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("admin_skills", Some(1)).await?;
    env.put(&dir, "SKILL.md", SKILL)?;
    let cfg = config(&[(CFG_STORE, json!("admin_skills"))]);
    assert!(header(&env, "skill_file", &cfg, &as_user("ada@example.com"), None).await.is_err());
    let (text, _) = header(&env, "skill_file", &cfg, &RunCaller::system(), None).await?;
    assert!(text.unwrap().contains("PDF forms"));
    Ok(())
}
