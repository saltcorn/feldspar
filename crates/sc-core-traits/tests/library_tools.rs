//! `library_tools` against the real JavaScript runtime: each tool calls one
//! function of a code library, as the run's caller, and a function may propose
//! a change that is applied only once the person approves it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Env, as_user, config};
use sc_agent::{RequestKind, RunCaller, RunId, TraitContext};
use sc_catalog::{CodeLibrary, bootstrap_code_libraries, save_code_library};
use sc_core_actions::builtin_actions;
use sc_core_traits::{CFG_LIBRARY, CFG_LIBRARY_TOOLS};
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
