//! An LLM prompt cell (analytics TODO A10.5): a prompt answered by an agent
//! whose tools are the `nb` functions.
//!
//! The agent is `sc-agent`'s, run by its [`Runner`] like any other, with one
//! trait — [`NotebookTrait`] — and the server's default LLM provider (L.3).
//! It is told the notebook's earlier cells and the datasets and tables there
//! are, with their columns. Every tool it calls is an `nb` plan
//! ([`nb::answer`], the plans a JavaScript or Python cell sends), and each one
//! that worked is **recorded** with the cell, with the prompt it answered:
//!
//! ```json
//! { "kind": "calls", "prompt": "a box plot of price by neighbourhood",
//!   "calls": [ { "function": "plot", "arguments": { "spec": { … } } },
//!              { "function": "test", "arguments": { "data": …, "spec": { … } } },
//!              { "function": "say", "arguments": { "text": "Prices differ …" } } ] }
//! ```
//!
//! Running the cell again **replays** them — the same plans, answered against
//! the data as it is now, without the LLM — until **Regenerate** asks it again,
//! or the prompt has changed since they were recorded.

use std::sync::{Arc, Mutex};

use sc_agent::{
    Agent, AgentRegistry, AgentTrait, Conclusion, EnabledTrait, ModelRole, Runner, ToolsContext,
    TraitContext,
};
use sc_analytics::nb;
use sc_analytics::notebook::{Cell, CellKind, Output, RecordedCall};
use sc_analytics::panel::Panel;
use sc_catalog::Catalog;
use sc_dataset::Caller;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

use crate::notebook::{RunContext, Session, error_output};

/// The trait's name, and the in-memory agent's.
const NOTEBOOK: &str = "notebook";

/// The most columns of one dataset the agent is told about.
const MAX_COLUMNS: usize = 40;

/// The most datasets and tables it is told the columns of.
const MAX_SOURCES: usize = 30;

/// How much of a drawn panel the model is told, in bytes.
const TOLD: usize = 4_000;

/// Run (or replay) one prompt cell.
pub(crate) async fn run(
    cx: &RunContext<'_>,
    session: &Session,
    cell: &Cell,
    earlier: &[Cell],
) -> Vec<Output> {
    let _ = session;
    let caller = cx.caller();
    let recorded = cell.outputs.iter().find_map(|o| match o {
        Output::Calls { calls, prompt } => Some((calls.clone(), prompt.clone())),
        _ => None,
    });
    if let Some((calls, prompt)) = recorded
        && !cx.regenerate
        && prompt.as_deref().is_none_or(|p| p == cell.source)
    {
        return replay(cx.catalog, &caller, calls, &cell.source).await;
    }
    match ask(cx, cell, earlier).await {
        Ok(outputs) => outputs,
        Err(e) => vec![error_output(e.message())],
    }
}

/// The recorded calls, answered again in order: the outputs they make, and
/// the calls themselves. A call that no longer works is the cell's error.
async fn replay(
    catalog: &Catalog,
    caller: &Caller,
    calls: Vec<RecordedCall>,
    prompt: &str,
) -> Vec<Output> {
    let mut outputs = Vec::new();
    for call in &calls {
        match answer_call(catalog, caller, &call.function, &call.arguments).await {
            Ok((_, Some(output))) => outputs.push(output),
            Ok((_, None)) => {}
            Err(e) => {
                outputs.push(Output::Error {
                    message: format!("replaying `{}`: {}", call.function, e.message()),
                    line: None,
                });
                break;
            }
        }
    }
    outputs.push(Output::Calls {
        calls,
        prompt: Some(prompt.to_owned()),
    });
    outputs
}

/// One tool call: what the model is told, and the output it makes — a
/// panel, a dataset's rows, the agent's words — or none, for a question it
/// asked about the data.
async fn answer_call(
    catalog: &Catalog,
    caller: &Caller,
    function: &str,
    args: &Json,
) -> Result<(Json, Option<Output>)> {
    match function {
        "say" => {
            let text = args
                .get("text")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok((json!({ "said": true }), Some(Output::Text { text })))
        }
        "columns" | "rows" => {
            let answer = nb::answer(catalog, caller, &plan(function, args)).await?;
            Ok((answer, None))
        }
        "show_dataset" => {
            let mut lowered = match plan("show_dataset", args).get("data").cloned() {
                Some(Json::String(name)) => json!({ "__nb": "dataset", "base": name, "steps": [] }),
                Some(other) => other,
                None => Json::Null,
            };
            if let Json::Object(fields) = &mut lowered {
                fields.remove("op");
                fields.remove("what");
            }
            let output = nb::output_of(catalog, caller, &lowered).await?;
            let told = match &output {
                Some(Output::Dataset {
                    columns,
                    rows,
                    total,
                    ..
                }) => {
                    json!({ "shown": "dataset", "columns": columns, "first_rows": rows.iter().take(5).collect::<Vec<_>>(), "total": total })
                }
                _ => json!({ "shown": "dataset" }),
            };
            Ok((told, output))
        }
        "plot" | "table" | "test" | "card" | "markdown" => {
            let what = match function {
                "markdown" => "text",
                other => other,
            };
            let answer = nb::answer(catalog, caller, &plan(what, args)).await?;
            let panel = Panel::from_json(&answer["panel"])?;
            let drawn = sc_analytics::panel::render_panel(catalog, caller, &panel).await?;
            let mut drawn = serde_json::to_string(&drawn).unwrap_or_default();
            if drawn.len() > TOLD {
                let mut cut = TOLD;
                while !drawn.is_char_boundary(cut) {
                    cut -= 1;
                }
                drawn.truncate(cut);
                drawn.push('…');
            }
            let told = json!({ "shown": panel.body.kind(), "drawn": drawn });
            Ok((
                told,
                Some(Output::Panel {
                    panel: Box::new(panel),
                }),
            ))
        }
        other => Err(Error::invalid(format!(
            "`{other}` is not a notebook tool; they are columns, rows, plot, table, test, card, \
             markdown and show_dataset"
        ))),
    }
}

/// A tool's arguments as the `nb` plan they are.
fn plan(what: &str, args: &Json) -> Json {
    let mut plan = match args {
        Json::Object(fields) => fields.clone(),
        _ => serde_json::Map::new(),
    };
    plan.insert("op".into(), json!("nb"));
    plan.insert("what".into(), json!(what));
    // A tool's `data` is the chain's own shape: a name, or `{ base, steps }`.
    for key in ["data"] {
        if let Some(Json::Object(data)) = plan.get_mut(key)
            && data.contains_key("base")
            && !data.contains_key("__nb")
        {
            data.insert("__nb".into(), json!("dataset"));
        }
    }
    if let Some(Json::Object(spec)) = plan.get_mut("spec")
        && let Some(Json::Object(data)) = spec.get_mut("data")
        && data.contains_key("base")
        && !data.contains_key("__nb")
    {
        data.insert("__nb".into(), json!("dataset"));
    }
    Json::Object(plan)
}

/// Ask the agent, recording what it calls.
async fn ask(cx: &RunContext<'_>, cell: &Cell, earlier: &[Cell]) -> Result<Vec<Output>> {
    let services = cx.apps.agents().ok_or_else(|| {
        Error::config("this server has no agents installed, so an LLM prompt cannot be answered")
    })?;
    // None configured — or no providers table yet — is the connector's to
    // say, which a scripted one never does.
    let provider = sc_llm::default_llm_provider(cx.catalog)
        .await
        .ok()
        .flatten()
        .map(|p| p.name)
        .unwrap_or_default();
    let context = context(cx, earlier).await?;
    let agent = Agent::new(NOTEBOOK, provider)
        .description("Answers a notebook's LLM prompt cells")
        .system_prompt(format!(
            "You answer one cell of a data analysis notebook. Use the tools to make what the \
             person asks for — plots, summary tables, hypothesis tests, stat cards — from the \
             datasets and tables below; each tool's result is shown under the cell. Check a \
             dataset's columns before you plot it. When you are done, answer in a sentence or \
             two saying what you made and what it shows.\n\n{context}"
        ))
        .with_trait(EnabledTrait::new(NOTEBOOK));
    let executor = services
        .providers()
        .connect(cx.catalog, &agent, ModelRole::Executor)
        .await
        .map_err(|e| match provider_missing(&agent) {
            true => Error::config(
                "there is no LLM provider to answer this prompt: add one in Settings → LLM",
            ),
            false => e,
        })?;
    let recorder = Arc::new(Recorder::default());
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(NotebookTrait {
        recorder: Arc::clone(&recorder),
        caller: cx.caller(),
    }))?;
    let runner = Runner::new(
        cx.catalog,
        &registry,
        &agent,
        executor,
        cx.run_caller.clone(),
    );
    let (_run, conclusion) = runner.start(cell.source.clone()).await?;
    let mut outputs = std::mem::take(&mut *lock(&recorder.outputs));
    let mut calls = std::mem::take(&mut *lock(&recorder.calls));
    match conclusion {
        Conclusion::Answered { answer } if !answer.trim().is_empty() => {
            calls.push(RecordedCall {
                function: "say".into(),
                arguments: json!({ "text": answer.trim() }),
            });
            outputs.push(Output::Text {
                text: answer.trim().to_owned(),
            });
        }
        Conclusion::Answered { .. } => {}
        other => outputs.push(Output::Error {
            message: format!(
                "the LLM stopped before it finished: {}",
                conclusion_words(&other)
            ),
            line: None,
        }),
    }
    calls.truncate(sc_analytics::notebook::MAX_CALLS);
    outputs.truncate(sc_analytics::notebook::MAX_OUTPUTS - 1);
    outputs.push(Output::Calls {
        calls,
        prompt: Some(cell.source.clone()),
    });
    Ok(outputs)
}

fn provider_missing(agent: &Agent) -> bool {
    agent.provider.trim().is_empty()
}

fn conclusion_words(conclusion: &Conclusion) -> String {
    match conclusion {
        Conclusion::MaxSteps => "it used every step it was given".to_owned(),
        Conclusion::Aborted => "it was stopped".to_owned(),
        other => format!("{other:?}"),
    }
}

/// What the agent is told: the cells above, and the data with its columns.
async fn context(cx: &RunContext<'_>, earlier: &[Cell]) -> Result<String> {
    let mut out = String::new();
    let cells: Vec<&Cell> = earlier
        .iter()
        .filter(|c| !c.source.trim().is_empty())
        .collect();
    if !cells.is_empty() {
        out.push_str("The notebook so far:\n");
        for c in cells.iter().rev().take(10).rev() {
            let what = match c.kind {
                CellKind::Text => "text",
                CellKind::Code => "prompt",
            };
            out.push_str(&format!("- ({what}) {}\n", c.source.trim()));
        }
        out.push('\n');
    }
    let (datasets, tables) = nb::known_data(cx.catalog).await?;
    out.push_str("The data, by name, with its columns:\n");
    let caller = cx.caller();
    for name in datasets.iter().chain(tables.iter()).take(MAX_SOURCES) {
        let plan = json!({ "op": "nb", "what": "columns", "data": name });
        let columns = match nb::answer(cx.catalog, &caller, &plan).await {
            Ok(Json::Array(columns)) => columns
                .iter()
                .take(MAX_COLUMNS)
                .map(|c| {
                    format!(
                        "{} ({})",
                        c["name"].as_str().unwrap_or_default(),
                        c["type"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "(does not read)".to_owned(),
        };
        let kind = match datasets.contains(name) {
            true => "dataset",
            false => "table",
        };
        out.push_str(&format!("- {kind} `{name}`: {columns}\n"));
    }
    Ok(out)
}

/// What one prompt's tools made and called.
#[derive(Default)]
struct Recorder {
    outputs: Mutex<Vec<Output>>,
    calls: Mutex<Vec<RecordedCall>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The trait whose tools are the `nb` functions.
struct NotebookTrait {
    recorder: Arc<Recorder>,
    /// Who the prompt's reads are read as: the administrator who ran it.
    caller: Caller,
}

#[async_trait::async_trait]
impl AgentTrait for NotebookTrait {
    fn name(&self) -> &str {
        NOTEBOOK
    }

    fn description(&self) -> &str {
        "A notebook's nb functions: plots, tables, tests and cards from datasets"
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        tools()
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        // A call that fails is the model's to read and fix, and is not
        // recorded: what is replayed is what worked.
        let (told, output) = answer_call(ctx.catalog, &self.caller, tool, args).await?;
        lock(&self.recorder.calls).push(RecordedCall {
            function: tool.to_owned(),
            arguments: args.clone(),
        });
        if let Some(output) = output {
            lock(&self.recorder.outputs).push(output);
        }
        Ok(told)
    }
}

/// The tools, each an `nb` function. `data` is a dataset's or a table's name,
/// or `{ base, steps }` — a dataset built from one, as `nb.dataset(base)`
/// and its chained steps are.
fn tools() -> Vec<ToolSpec> {
    let data = json!({
        "description": "A dataset's or a table's name, or a dataset built from one: \
            { \"base\": name, \"steps\": [ { \"method\": \"filter\", \"args\": [\"price > 100\"] }, \
            { \"method\": \"aggregate\", \"args\": [[\"town\"], { \"n\": \"count()\", \"mean_price\": \"mean(price)\" }] } ] }. \
            The methods are filter(condition), mutate({ name: formula }), select(...columns), \
            sort(...columns, -column for descending), aggregate(group_by, { name: \"fn(column)\" }), \
            limit(n) and sample(n)."
    });
    let object = |properties: Json, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
    vec![
        ToolSpec::new(
            "columns",
            "The columns of a dataset or table, with their types. Use it before plotting data \
             you have not seen.",
            object(json!({ "data": data }), &["data"]),
        ),
        ToolSpec::new(
            "rows",
            "The first rows of a dataset or table, to see its values.",
            object(
                json!({ "data": data, "limit": { "type": "integer" } }),
                &["data"],
            ),
        ),
        ToolSpec::new(
            "plot",
            "Make a plot, shown under the cell. `spec` is { data, mark, x, y, color, size, \
             shape, label, stat, bins }: mark is point, line, bar, area, box, text, rect, or \
             histogram, boxplot, density, smooth; x, y, color … are column names; stat is \
             count, mean, sum, median, min, max, boxplot, density or smooth. A bar with no y \
             counts rows.",
            object(json!({ "spec": { "type": "object" } }), &["spec"]),
        ),
        ToolSpec::new(
            "table",
            "Make a summary table, shown under the cell. `spec` is { rows, columns, cells }: \
             rows and columns are column names to group by, cells like \"count()\" or \
             \"mean(price)\".",
            object(
                json!({ "data": data, "spec": { "type": "object" } }),
                &["data"],
            ),
        ),
        ToolSpec::new(
            "test",
            "Run the hypothesis tests that fit the data, shown under the cell. `spec` is \
             { y, x, by, paired, mu, level }: y the column (or columns) tested, x the column \
             it is compared across or with; the right tests are chosen from the columns' kinds.",
            object(
                json!({ "data": data, "spec": { "type": "object" } }),
                &["data", "spec"],
            ),
        ),
        ToolSpec::new(
            "card",
            "Make a stat card — one number — over a stored dataset. `spec` is { value }, the \
             value like \"mean(price)\" or \"count()\".",
            object(
                json!({ "data": data, "spec": { "type": "object" } }),
                &["data", "spec"],
            ),
        ),
        ToolSpec::new(
            "markdown",
            "Show some Markdown under the cell: a heading, or a note on what a result means.",
            object(json!({ "markdown": { "type": "string" } }), &["markdown"]),
        ),
        ToolSpec::new(
            "show_dataset",
            "Show a dataset under the cell: its first rows, which the person can save as a \
             named dataset.",
            object(json!({ "data": data }), &["data"]),
        ),
    ]
}
