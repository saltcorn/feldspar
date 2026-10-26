//! `prompt_picker` — a select of prompts in the chat's input row
//! (TODO-agent-skills AS10.5; v1's "Prompt picker" skill).
//!
//! The options are either **predefined** in the configuration (a label and a
//! prompt each) or **read from a table** (a label field and a prompt field,
//! with an optional `where` filter), as the person chatting: a row they may
//! not read is not an option they are offered.
//!
//! What choosing one does is the configuration's `send`:
//!
//! - **Off** (the default): the choice is a composer select whose value
//!   travels with every message (AS6.4), and the chosen prompt is added to the
//!   system prompt of each turn it is sent with ([`AgentTrait::on_turn`]). The
//!   option's value is its position or its row's primary key — the prompt
//!   itself never reaches the browser.
//! - **On**: choosing an option sends its prompt at once as the person's
//!   message (a [`ControlKind::Send`] control), as v1's "To user input" did.
//!
//! It offers no tool.

use sc_agent::{
    AgentTrait, ChoiceOption, ComposerControl, ControlKind, ControlsContext, ToolsContext,
    TraitCheck, TraitContext, Turn,
};
use sc_api::rows::RowQuery;
use sc_catalog::{Catalog, Table};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::table::{all_fields, config_str, where_expr};

/// The control's name, `prompt` when unset: what its value is sent under.
pub const CFG_PICKER_NAME: &str = "name";
/// What the person reads beside the select.
pub const CFG_PICKER_LABEL: &str = "label";
/// `predefined` (the default) or `table`.
pub const CFG_SOURCE: &str = "source";
/// The predefined options: `[{ "label", "prompt" }]`.
pub const CFG_PROMPTS: &str = "prompts";
/// The table the options are read from.
pub const CFG_PROMPT_TABLE: &str = "table";
/// The field an option's label is read from.
pub const CFG_LABEL_FIELD: &str = "label_field";
/// The field an option's prompt is read from.
pub const CFG_PROMPT_FIELD: &str = "prompt_field";
/// Which rows are options: a `where` object, as `query_table` takes.
pub const CFG_PROMPT_WHERE: &str = "where";
/// Whether choosing an option sends its prompt at once as a message.
pub const CFG_SEND: &str = "send";

/// [`CFG_SOURCE`]: options in the configuration.
pub const SOURCE_PREDEFINED: &str = "predefined";
/// [`CFG_SOURCE`]: options read from a table.
pub const SOURCE_TABLE: &str = "table";

/// The most rows a table-sourced picker offers.
pub const MAX_PICKER_ROWS: u64 = 200;

const DEFAULT_NAME: &str = "prompt";

/// A select of prompts in the chat's input row.
pub struct PromptPicker;

/// One option: what the person reads, what the select sends, and the prompt.
struct Prompt {
    label: String,
    key: String,
    prompt: String,
}

fn control_name(config: &Attrs) -> String {
    match config_str(config, CFG_PICKER_NAME) {
        name if name.is_empty() => DEFAULT_NAME.to_owned(),
        name => name,
    }
}

fn sends(config: &Attrs) -> bool {
    config.get(CFG_SEND).and_then(Json::as_bool).unwrap_or(false)
}

fn from_table(config: &Attrs) -> Result<bool> {
    match config_str(config, CFG_SOURCE).as_str() {
        "" | SOURCE_PREDEFINED => Ok(false),
        SOURCE_TABLE => Ok(true),
        other => Err(Error::invalid(format!(
            "`{CFG_SOURCE}` is `{SOURCE_PREDEFINED}` or `{SOURCE_TABLE}`, not `{other}`"
        ))),
    }
}

/// The predefined options, each with a label and a prompt.
fn predefined(config: &Attrs) -> Result<Vec<Prompt>> {
    let Some(Json::Array(items)) = config.get(CFG_PROMPTS) else {
        return Err(Error::invalid(format!(
            "`{CFG_PROMPTS}` must be a list of {{ \"label\", \"prompt\" }}"
        )));
    };
    let mut out: Vec<Prompt> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let text = |key: &str| {
            item.get(key)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let (label, prompt) = (text("label"), text("prompt"));
        if label.is_empty() || prompt.is_empty() {
            return Err(Error::invalid(format!(
                "`{CFG_PROMPTS}` entry {} needs a label and a prompt",
                i + 1
            )));
        }
        if out.iter().any(|p| p.label == label) {
            return Err(Error::invalid(format!(
                "`{CFG_PROMPTS}` has the label `{label}` twice"
            )));
        }
        out.push(Prompt {
            label,
            key: i.to_string(),
            prompt,
        });
    }
    Ok(out)
}

/// The configured table and its two fields, checked.
fn table_source(catalog: &Catalog, config: &Attrs) -> Result<(Table, String, String, String)> {
    let name = config_str(config, CFG_PROMPT_TABLE);
    if name.is_empty() {
        return Err(Error::invalid(format!(
            "`{CFG_PROMPT_TABLE}` is required when `{CFG_SOURCE}` is `{SOURCE_TABLE}`"
        )));
    }
    let table = catalog.require(&name)?;
    let field = |key: &str| -> Result<String> {
        let field = config_str(config, key);
        if table.fields.iter().any(|f| f.base.name == field) {
            Ok(field)
        } else {
            Err(Error::invalid(format!(
                "`{key}` must name a field of `{}`; it has {}",
                table.name,
                all_fields(&table).join(", ")
            )))
        }
    };
    let label = field(CFG_LABEL_FIELD)?;
    let prompt = field(CFG_PROMPT_FIELD)?;
    let pk = table
        .fields
        .iter()
        .find(|f| f.primary_key)
        .map(|f| f.base.name.clone())
        .ok_or_else(|| {
            Error::invalid(format!("table `{}` has no primary key to name a row by", table.name))
        })?;
    Ok((table, label, prompt, pk))
}

/// The options read from the table, as `caller` may read it.
async fn table_prompts(
    catalog: &Catalog,
    config: &Attrs,
    caller: &sc_agent::RunCaller,
    evaluator: Option<&std::sync::Arc<dyn sc_expr::JsEvaluator>>,
) -> Result<Vec<Prompt>> {
    let (table, label_field, prompt_field, pk) = table_source(catalog, config)?;
    let filter = where_expr(&table, &all_fields(&table), config.get(CFG_PROMPT_WHERE))?;
    let query = RowQuery::new().where_(filter).limit(MAX_PICKER_ROWS);
    let found = sc_api::read_rows_as(
        catalog,
        &table,
        &query,
        caller.role,
        caller.user.as_ref(),
        evaluator,
    )
    .await?;
    let text = |v: Option<&Json>| match v {
        Some(Json::String(s)) => s.trim().to_owned(),
        Some(Json::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    Ok(found
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|row| {
            let prompt = text(row.get(&prompt_field));
            let key = text(row.get(&pk));
            let label = match text(row.get(&label_field)) {
                l if l.is_empty() => key.clone(),
                l => l,
            };
            (!prompt.is_empty() && !key.is_empty()).then_some(Prompt { label, key, prompt })
        })
        .collect())
}

/// The composer control for `prompts`.
fn control(config: &Attrs, prompts: Vec<Prompt>) -> Vec<ComposerControl> {
    if prompts.is_empty() {
        return Vec::new();
    }
    let label = match config_str(config, CFG_PICKER_LABEL) {
        l if l.is_empty() => "Prompt".to_owned(),
        l => l,
    };
    let kind = if sends(config) {
        ControlKind::Send {
            options: prompts
                .into_iter()
                .map(|p| ChoiceOption::new(p.prompt).label(p.label))
                .collect(),
        }
    } else {
        // An empty first option: no prompt until the person picks one.
        let mut options = vec![ChoiceOption::new("").label("—")];
        options.extend(
            prompts
                .into_iter()
                .map(|p| ChoiceOption::new(p.key).label(p.label)),
        );
        ControlKind::Select {
            options,
            default: Some(String::new()),
        }
    };
    vec![ComposerControl {
        name: control_name(config),
        label,
        kind,
    }]
}

#[async_trait::async_trait]
impl AgentTrait for PromptPicker {
    fn name(&self) -> &str {
        "prompt_picker"
    }

    fn description(&self) -> &str {
        "A select of prompts in the chat: added to the system prompt, or sent as a message"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_PICKER_LABEL, BasicType::Text)
                .label("Label")
                .sublabel("What the person reads beside the select"),
            FormField::new(CFG_PICKER_NAME, BasicType::Text)
                .label("Control name")
                .sublabel("What the choice is sent under; `prompt` when empty"),
            FormField::new(CFG_SOURCE, BasicType::Text)
                .label("Source")
                .options([SOURCE_PREDEFINED, SOURCE_TABLE]),
            FormField::new(CFG_PROMPTS, BasicType::Json)
                .label("Prompts")
                .sublabel("Predefined: a list of { \"label\", \"prompt\" }"),
            FormField::new(CFG_PROMPT_TABLE, BasicType::Text).label("Table"),
            FormField::new(CFG_LABEL_FIELD, BasicType::Text).label("Label field"),
            FormField::new(CFG_PROMPT_FIELD, BasicType::Text).label("Prompt field"),
            FormField::new(CFG_PROMPT_WHERE, BasicType::Json)
                .label("Filter")
                .sublabel("Optional: only rows matching this `where` object"),
            FormField::new(CFG_SEND, BasicType::Bool)
                .label("Send on pick")
                .sublabel(
                    "Choosing a prompt sends it as the person's message; otherwise it is \
                     added to the system prompt",
                ),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let name = control_name(check.config);
        if name == sc_agent::MODEL_CONTROL {
            return Err(Error::invalid(format!(
                "`{CFG_PICKER_NAME}` `{name}` is the model picker's; choose another"
            )));
        }
        if from_table(check.config)? {
            let (table, ..) = table_source(check.catalog, check.config)?;
            where_expr(
                &table,
                &all_fields(&table),
                check.config.get(CFG_PROMPT_WHERE),
            )?;
        } else if predefined(check.config)?.is_empty() {
            return Err(Error::invalid(format!(
                "`{CFG_PROMPTS}` needs at least one prompt"
            )));
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        _args: &Json,
        _ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        Err(Error::invalid(format!(
            "`prompt_picker` offers no tool, so there is no `{tool}`"
        )))
    }

    fn controls(&self, config: &Attrs) -> Vec<ComposerControl> {
        // The table's rows need a caller: [`controls_in`] reads them.
        if from_table(config).unwrap_or(true) {
            return Vec::new();
        }
        control(config, predefined(config).unwrap_or_default())
    }

    async fn controls_in(
        &self,
        config: &Attrs,
        cx: &ControlsContext<'_>,
    ) -> Result<Vec<ComposerControl>> {
        if !from_table(config)? {
            return Ok(self.controls(config));
        }
        let prompts = table_prompts(cx.catalog, config, cx.caller, None).await?;
        Ok(control(config, prompts))
    }

    async fn on_turn(&self, config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        if sends(config) {
            return Ok(());
        }
        let Some(Json::String(key)) = turn.controls.get(&control_name(config)) else {
            return Ok(());
        };
        if key.is_empty() {
            return Ok(());
        }
        let prompts = if from_table(config)? {
            let catalog = turn.catalog.ok_or_else(|| {
                Error::config("`prompt_picker` reads its prompts from a table, and this turn has no catalog")
            })?;
            table_prompts(catalog, config, turn.caller, turn.evaluator).await?
        } else {
            predefined(config)?
        };
        if let Some(chosen) = prompts.into_iter().find(|p| &p.key == key) {
            turn.append_system(chosen.prompt);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(value: Json) -> Attrs {
        value.as_object().unwrap().clone().into_iter().collect()
    }

    #[test]
    fn predefined_prompts_are_a_select_by_position_or_a_send_of_the_prompt() {
        let prompts = json!([
            { "label": "Pirate", "prompt": "Speak like a pirate" },
            { "label": "Poet", "prompt": "Answer in verse" },
        ]);
        let select = PromptPicker.controls(&config(json!({ "prompts": prompts })));
        let ControlKind::Select { options, .. } = &select[0].kind else {
            panic!("a select: {select:?}");
        };
        assert_eq!(select[0].name, "prompt");
        assert_eq!(options.len(), 3);
        assert_eq!(options[2].value, "1");
        assert!(!format!("{select:?}").contains("verse"), "the prompt stays on the server");

        let send = PromptPicker.controls(&config(json!({ "prompts": prompts, "send": true })));
        assert_eq!(
            serde_json::to_value(&send[0]).unwrap()["options"][1],
            json!({ "value": "Answer in verse", "label": "Poet" })
        );
        assert_eq!(serde_json::to_value(&send[0]).unwrap()["kind"], "send");
        assert!(predefined(&config(json!({ "prompts": [{ "label": "x" }] }))).is_err());
    }

    #[tokio::test]
    async fn the_chosen_prompt_is_added_to_the_turn() {
        let config = config(json!({ "prompts": [
            { "label": "Pirate", "prompt": "Speak like a pirate" },
        ]}));
        let caller = sc_agent::RunCaller::system();
        let mut turn = Turn::new(&caller, "a", 1);
        turn.controls.insert("prompt".into(), json!("0"));
        PromptPicker.on_turn(&config, &mut turn).await.unwrap();
        assert_eq!(turn.system_prompt("Base"), "Base\n\nSpeak like a pirate");
        let mut none = Turn::new(&caller, "a", 1);
        none.controls.insert("prompt".into(), json!(""));
        PromptPicker.on_turn(&config, &mut none).await.unwrap();
        assert_eq!(none.system_prompt("Base"), "Base");
    }
}
