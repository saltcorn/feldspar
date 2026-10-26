//! `run_js_code` against the real JavaScript runtime.
//!
//! What is pinned here: the tool is named and shaped by its configuration; the
//! code runs, with each argument a variable and `args` all of them; it reads
//! the database as the run's caller; and nothing about it is a trigger.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Env, as_user, config};
use sc_agent::RunCaller;
use sc_core_actions::builtin_actions;
use sc_core_traits::{CFG_ARGUMENTS, CFG_CODE, CFG_TOOL_DESCRIPTION, CFG_TOOL_NAME};
use sc_error::Result;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

const SCHEMA: &str = "
    CREATE TABLE books (id bigint primary key, title text);
    INSERT INTO books VALUES (1, 'Dune'), (2, 'Emma');
";

async fn setup() -> Result<Env> {
    Env::with_schema(SCHEMA)
        .await?
        .with_triggers(builtin_actions()?)
}

fn tool(name: &str, code: &str, arguments: Json) -> Attrs {
    config(&[
        (CFG_TOOL_NAME, json!(name)),
        (CFG_TOOL_DESCRIPTION, json!("Shout it")),
        (CFG_CODE, json!(code)),
        (CFG_ARGUMENTS, arguments),
    ])
}

#[tokio::test]
async fn the_tool_is_named_and_described_by_its_configuration() -> Result<()> {
    let env = setup().await?;
    let cfg = tool(
        "shout",
        "return text.toUpperCase()",
        json!([{ "name": "text", "type": "string", "description": "what to shout" }]),
    );
    env.check("run_js_code", &cfg).await?;
    let tools = env.tools("run_js_code", &cfg);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "shout");
    assert_eq!(tools[0].description, "Shout it");
    assert_eq!(
        tools[0].parameters,
        json!({ "type": "object",
            "properties": { "text": { "type": "string", "description": "what to shout" } },
            "required": ["text"], "additionalProperties": false })
    );
    Ok(())
}

#[tokio::test]
async fn the_code_runs_with_its_arguments_as_variables() -> Result<()> {
    let env = setup().await?;
    let cfg = tool(
        "shout",
        "return { loud: text.toUpperCase(), times, all: args }",
        json!([{ "name": "text" }, { "name": "times", "type": "integer" }]),
    );
    let result = env
        .call(
            "run_js_code",
            &cfg,
            json!({ "text": "hi" }),
            &RunCaller::system(),
        )
        .await?;
    // An argument the model left out is null, not a ReferenceError.
    assert_eq!(
        result,
        json!({ "loud": "HI", "times": null, "all": { "text": "hi" } })
    );
    // A v1 tool with no arguments at all.
    let word = tool("word_of_the_day", "return \"Strawberry\"", json!([]));
    assert_eq!(
        env.call("run_js_code", &word, json!({}), &RunCaller::system())
            .await?,
        json!("Strawberry")
    );
    // And no trigger was made to run either.
    assert!(sc_action::list_triggers(&env.catalog).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_code_reads_the_database_as_the_caller() -> Result<()> {
    let env = setup().await?;
    let cfg = tool(
        "titles",
        "const rows = await db.books.orderBy(\"id\").rows(); \
         return { who: user ? user.email : null, titles: rows.map(r => r.title) }",
        json!([]),
    );
    let result = env
        .call("run_js_code", &cfg, json!({}), &as_user("ada@example.com"))
        .await?;
    assert_eq!(result["who"], json!("ada@example.com"));
    assert_eq!(result["titles"], json!(["Dune", "Emma"]));
    Ok(())
}

#[tokio::test]
async fn a_tool_that_would_hide_the_bodys_own_names_is_refused_on_save() -> Result<()> {
    let env = setup().await?;
    let err = env
        .check(
            "run_js_code",
            &tool("t", "return 1", json!([{ "name": "db" }])),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`db`"), "{err}");
    let err = env
        .check("run_js_code", &tool("two words", "return 1", json!([])))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tool_name"), "{err}");
    Ok(())
}
