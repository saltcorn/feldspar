//! `library_tools` against the real JavaScript runtime: each tool calls one
//! function of a code library, as the run's caller, and a function may propose
//! a change that is applied only once the person approves it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Env, as_user, config};
use sc_action_builtins::builtin_actions;
use sc_agent::{RequestKind, RunCaller, RunId, TraitContext};
use sc_agent_builtins::{CFG_LIBRARY, CFG_LIBRARY_TOOLS};
use sc_catalog::{CodeLibrary, bootstrap_code_libraries, save_code_library};
use sc_error::Result;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

const SCHEMA: &str = "
    CREATE TABLE books (id bigint primary key, title text);
    INSERT INTO books VALUES (1, 'Dune'), (2, 'Emma');
";

/// The application's tools, as its developers would write them.
const PADDI: &str = r#"
exports.search = async ({ query }) =>
  (await db.books.rows()).filter((b) => b.title.includes(query)).map((b) => b.title);
exports.whoami = async () => (user ? user.email : null);
exports.proposeRename = async ({ id, title }) => ({
  $proposal: { prompt: `Rename book ${id} to ${title}`, apply: "rename", payload: { id, title } },
  note: "drafted",
});
exports.rename = async ({ id, title }) => {
  await db.books.where({ id }).update({ title });
  return "renamed";
};
"#;

async fn setup() -> Result<Env> {
    let env = Env::with_schema(SCHEMA)
        .await?
        .with_triggers(builtin_actions()?)?;
    bootstrap_code_libraries(&env.catalog).await?;
    save_code_library(&env.catalog, &CodeLibrary::new("paddi", PADDI)).await?;
    Ok(env)
}

fn cfg() -> Attrs {
    config(&[
        (CFG_LIBRARY, json!("paddi")),
        (
            CFG_LIBRARY_TOOLS,
            json!([
                { "name": "search_docs", "function": "search", "description": "Find books",
                  "parameters": { "type": "object",
                                  "properties": { "query": { "type": "string" } },
                                  "required": ["query"] } },
                { "name": "whoami" },
                { "name": "propose_rename", "function": "proposeRename" },
            ]),
        ),
    ])
}

/// Call `tool`, keeping what the call asked of the person.
async fn call(
    env: &Env,
    tool: &str,
    args: Json,
    caller: &RunCaller,
) -> Result<(Json, Option<sc_agent::UserRequest>)> {
    let trait_ = env.registry.require("library_tools")?.clone();
    let mut state = Json::Null;
    let mut ctx = context(env, caller, &mut state);
    let result = trait_.call(&cfg(), tool, &args, &mut ctx).await?;
    Ok((result, ctx.extras.awaiting.take()))
}

fn context<'a>(env: &'a Env, caller: &'a RunCaller, state: &'a mut Json) -> TraitContext<'a> {
    TraitContext {
        catalog: &env.catalog,
        caller,
        agent: "assistant",
        run: RunId::new(),
        mode: sc_agent::RunMode::Act,
        trait_state: state,
        evaluator: env.evaluator.as_ref(),
        triggers: env.dispatcher.as_ref(),
        delegate: None,
        previews: None,
        browser: None,
        requests: None,
        modules: None,
        signals: Vec::new(),
        images: Vec::new(),
        extras: Default::default(),
    }
}

#[tokio::test]
async fn each_tool_is_one_function_with_its_own_schema() -> Result<()> {
    let env = setup().await?;
    env.check("library_tools", &cfg()).await?;
    let tools = env.tools("library_tools", &cfg());
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search_docs", "whoami", "propose_rename"]);
    assert_eq!(tools[0].description, "Find books");
    assert_eq!(tools[0].parameters["required"], json!(["query"]));

    let (found, asked) = call(
        &env,
        "search_docs",
        json!({ "query": "Du" }),
        &RunCaller::system(),
    )
    .await?;
    assert_eq!(found, json!(["Dune"]));
    assert!(asked.is_none());

    // A library that is not there is refused on save.
    let missing = config(&[
        (CFG_LIBRARY, json!("nope")),
        (CFG_LIBRARY_TOOLS, json!([{ "name": "x" }])),
    ]);
    assert!(env.check("library_tools", &missing).await.is_err());
    Ok(())
}

#[tokio::test]
async fn a_function_runs_as_the_person_chatting() -> Result<()> {
    let env = setup().await?;
    let (who, _) = call(&env, "whoami", json!({}), &as_user("ada@example.com")).await?;
    assert_eq!(who, json!("ada@example.com"));
    Ok(())
}

#[tokio::test]
async fn a_proposal_is_applied_only_when_the_person_approves() -> Result<()> {
    let env = setup().await?;
    let ada = as_user("ada@example.com");
    let (told, asked) = call(
        &env,
        "propose_rename",
        json!({ "id": 1, "title": "Dune Messiah" }),
        &ada,
    )
    .await?;
    // The model hears it is waiting, and the rest of what the function said.
    assert_eq!(told["proposal"], json!("Rename book 1 to Dune Messiah"));
    assert_eq!(told["result"], json!({ "note": "drafted" }));
    let request = asked.expect("the proposal is put to the person");
    assert!(
        matches!(request.kind, RequestKind::Approve { .. }),
        "{request:?}"
    );
    // Nothing changed yet.
    let title =
        |rows: Vec<Json>| rows.iter().find(|r| r["id"] == json!(1)).unwrap()["title"].clone();
    assert_eq!(title(env.rows("books").await?), json!("Dune"));

    let trait_ = env.registry.require("library_tools")?.clone();
    let mut state = Json::Null;
    let declined = trait_
        .on_answer_in(
            &cfg(),
            &request,
            &json!(false),
            &mut context(&env, &ada, &mut state),
        )
        .await?;
    assert!(declined.contains("declined"), "{declined}");
    assert_eq!(title(env.rows("books").await?), json!("Dune"));

    let approved = trait_
        .on_answer_in(
            &cfg(),
            &request,
            &json!(true),
            &mut context(&env, &ada, &mut state),
        )
        .await?;
    assert!(
        approved.contains("applied") && approved.contains("renamed"),
        "{approved}"
    );
    assert_eq!(title(env.rows("books").await?), json!("Dune Messiah"));
    Ok(())
}

/// The user report behind TODO T1, shrunk to its shape: a library tool runs
/// the app's one `case_action` trigger, which writes an outbox row, whose
/// insert trigger hands it on, whose insert trigger "delivers" it. The tool
/// call is **not** a level of the cascade, and a trigger the bound refuses
/// at the bottom fails the tool call instead of it saying "ok".
#[tokio::test]
async fn a_library_tool_is_not_a_cascade_level_and_a_refusal_below_it_fails_the_call() -> Result<()>
{
    use sc_action::{EventKind, MAX_DEPTH, Trigger, save_trigger};

    let env = Env::with_schema(
        "CREATE TABLE outbox (id serial primary key, msg text);
         CREATE TABLE deliveries (id serial primary key, msg text);
         CREATE TABLE delivered (id serial primary key, msg text);",
    )
    .await?
    .with_triggers(builtin_actions()?)?;
    bootstrap_code_libraries(&env.catalog).await?;
    save_code_library(
        &env.catalog,
        &CodeLibrary::new(
            "cases",
            r#"exports.sendMail = async ({ msg }) => {
                 await trigger("case_action").run({ msg });
                 return "ok";
               };"#,
        ),
    )
    .await?;
    let registry = env.dispatcher.as_ref().expect("a dispatcher").registry();
    let js = |name: &str, when: EventKind, code: &str| {
        Trigger::new(name, when, "run_js_code").config("code", code)
    };
    for trigger in [
        js(
            "case_action",
            EventKind::None,
            "await db.outbox.insert({ msg: payload.msg });",
        ),
        js(
            "mail_outbox_send",
            EventKind::Insert,
            "await db.deliveries.insert({ msg: row.msg });",
        )
        .on("outbox"),
        js(
            "mail_deliver",
            EventKind::Insert,
            "await db.delivered.insert({ msg: row.msg });",
        )
        .on("deliveries"),
    ] {
        save_trigger(&env.catalog, &registry, &trigger).await?;
    }
    env.reload_triggers().await?;

    let tools = config(&[
        (CFG_LIBRARY, json!("cases")),
        (
            CFG_LIBRARY_TOOLS,
            json!([{ "name": "case_send_mail", "function": "sendMail" }]),
        ),
    ]);
    // A run started `levels` triggers deep (a button's trigger, then the
    // `run_agent` one, …), calling the tool.
    let send = |levels: usize, msg: &'static str| {
        let env = &env;
        let tools = &tools;
        async move {
            let caller =
                RunCaller::system().chained((0..levels).map(|i| format!("outer_{i}")).collect());
            let trait_ = env.registry.require("library_tools")?.clone();
            let mut state = Json::Null;
            let mut ctx = context(env, &caller, &mut state);
            // As the agent driver runs a tool call.
            sc_catalog::cascade::surfacing_refusals(trait_.call(
                tools,
                "case_send_mail",
                &json!({ "msg": msg }),
                &mut ctx,
            ))
            .await
        }
    };

    // The three triggers fill the bound exactly: had the tool call counted,
    // `mail_deliver` would be one past it.
    let ok = send(MAX_DEPTH - 3, "fits").await?;
    assert_eq!(ok, json!("ok"));
    let delivered = env.rows("delivered").await?;
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["msg"], json!("fits"));

    // One level more and `mail_deliver` is refused. `case_action` itself
    // succeeded and the outbox row was written, but the call says what was cut
    // short, naming the chain, rather than "ok".
    let err = send(MAX_DEPTH - 2, "too deep").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("trigger `mail_deliver` was not run"), "{msg}");
    assert!(
        msg.contains("case_action → mail_outbox_send → mail_deliver"),
        "{msg}"
    );
    assert!(msg.contains(&format!("limit of {MAX_DEPTH}")), "{msg}");
    assert_eq!(env.rows("outbox").await?.len(), 2);
    assert_eq!(env.rows("delivered").await?.len(), 1);
    Ok(())
}
