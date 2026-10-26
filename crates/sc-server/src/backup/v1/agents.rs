//! v1's agents — an `Agent` action from the `@saltcorn/agents` plugin — as this
//! system's agents (§11), and the LLM plugin's settings as the provider they
//! call through.
//!
//! In v1 an agent is a trigger whose action is `Agent`: its configuration holds
//! the system prompt, the model and a list of *skills*. Here an agent is an
//! entity of its own with *traits*, and calls a named LLM provider. The skills
//! that have a counterpart cross as the trait that does the same job:
//!
//! | v1 skill | here |
//! |---|---|
//! | `Table` (query, insert) | `query_table` and/or `insert_row` on that table |
//! | `Run JavaScript code` | `run_js_code`, the code kept in the agent |
//! | `Trigger` | `run_trigger` on that trigger |
//!
//! A JavaScript tool's arguments are bare variables in its code in both
//! systems, so the code crosses as written. Nothing is added to the trigger
//! list: the tool is the agent's own.
//!
//! The provider is v1's `large-language-model` plugin configuration — the
//! vendor, the key and the default model — when it names a vendor this system
//! has a backend for. The restore keeps a provider of the same name that is
//! already here, key and all, and the agent then calls through that one.

use sc_llm::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_BASE_URL, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND,
};
use serde_json::{Map, Value as Json, json};

use super::{array, name_of, text};

/// The v1 action an agent is a trigger of.
pub(super) const AGENT_ACTION: &str = "Agent";
/// The v1 plugin holding the LLM settings.
const LLM_PLUGIN: &str = "large-language-model";

/// What v1's agents became: the agents, and the provider they call.
#[derive(Default)]
pub(super) struct ConvertedAgents {
    pub(super) agents: Vec<Json>,
    pub(super) providers: Vec<Json>,
}

pub(super) fn convert_agents(pack: &Map<String, Json>, notes: &mut Vec<String>) -> ConvertedAgents {
    let mut out = ConvertedAgents::default();
    let agents: Vec<Map<String, Json>> = array(pack, "triggers")
        .into_iter()
        .filter_map(|t| t.as_object().cloned())
        .filter(|t| text(t, "action") == AGENT_ACTION)
        .collect();
    if agents.is_empty() {
        return out;
    }
    let provider = provider(pack, notes);
    let provider_name = provider
        .as_ref()
        .and_then(|p| p.get("name").and_then(Json::as_str))
        // Nothing to derive one from: the agent still crosses, naming a
        // provider the restore reports missing, which an admin can create.
        .unwrap_or("saltcorn-llm")
        .to_owned();
    out.providers.extend(provider);

    for agent in &agents {
        let name = name_of(agent);
        let config = agent
            .get("configuration")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default();
        let min_role = agent.get("min_role").and_then(Json::as_i64);
        let mut traits = Vec::new();
        for skill in array(&config, "skills") {
            let Some(skill) = skill.as_object() else {
                continue;
            };
            let kind = text(skill, "skill_type");
            match kind.as_str() {
                "Table" => {
                    let table = text(skill, "table_name");
                    if truthy(skill.get("query")) {
                        let mut config = json!({ "table": table });
                        let hidden = list(&text(skill, "hidden_fields"));
                        if !hidden.is_empty() {
                            config["fields"] = json!(
                                table_fields(pack, &table)
                                    .into_iter()
                                    .filter(|f| !hidden.contains(f))
                                    .collect::<Vec<_>>()
                            );
                        }
                        traits.push(json!({ "trait": "query_table", "config": config }));
                    }
                    if truthy(skill.get("insert")) {
                        traits.push(json!({ "trait": "insert_row", "config": { "table": table } }));
                    }
                }
                "Run JavaScript code" => {
                    let tool = text(skill, "tool_name");
                    if tool.is_empty() {
                        notes.push(format!(
                            "a JavaScript tool of agent `{name}` was not imported: it has no name"
                        ));
                        continue;
                    }
                    traits.push(js_tool(skill, &tool));
                }
                "Trigger" => {
                    let trigger = text(skill, "trigger_name");
                    traits
                        .push(json!({ "trait": "run_trigger", "config": { "trigger": trigger } }));
                }
                other => notes.push(format!(
                    "the `{other}` skill of agent `{name}` was not imported: this system \
                     has no counterpart for it"
                )),
            }
        }
        let mut value = json!({
            "name": name,
            "description": text(agent, "description"),
            "provider": provider_name,
            "system_prompt": text(&config, "sys_prompt"),
            "model": text(&config, "model"),
            "traits": traits,
            "attributes": {},
        });
        if let Some(role) = min_role {
            value["min_role"] = json!(role);
        }
        out.agents.push(value);
    }
    out
}

/// One v1 JavaScript tool as a `run_js_code` trait: its name, description,
/// code and arguments, with v1's argument types read as JSON Schema's.
fn js_tool(skill: &Map<String, Json>, tool: &str) -> Json {
    let arguments: Vec<Json> = array(skill, "toolargs")
        .iter()
        .filter_map(Json::as_object)
        .filter(|a| !text(a, "name").trim().is_empty())
        .map(|a| {
            let type_ = match text(a, "argtype").as_str() {
                t @ ("number" | "integer" | "boolean" | "object" | "array") => t.to_owned(),
                _ => "string".to_owned(),
            };
            json!({
                "name": text(a, "name").trim(),
                "type": type_,
                "description": text(a, "description"),
            })
        })
        .collect();
    json!({ "trait": "run_js_code", "config": {
        "tool_name": tool,
        "description": text(skill, "tool_description"),
        "code": text(skill, "js_code"),
        "arguments": arguments,
    } })
}

/// The LLM provider v1's `large-language-model` plugin was configured with, in
/// the shape `llm-providers.json` carries one — or `None`, with a note, when
/// it names a vendor this system has no backend for.
fn provider(pack: &Map<String, Json>, notes: &mut Vec<String>) -> Option<Json> {
    let plugin = array(pack, "plugins")
        .into_iter()
        .filter_map(|p| p.as_object().cloned())
        .find(|p| name_of(p) == LLM_PLUGIN);
    let Some(config) = plugin
        .as_ref()
        .and_then(|p| p.get("configuration"))
        .and_then(Json::as_object)
    else {
        notes.push(
            "the agents call an LLM provider named `saltcorn-llm`: the backup has no \
             LLM plugin configuration to make one from, so create it before using them"
                .to_owned(),
        );
        return None;
    };
    let backend = text(config, "backend");
    let vendor = match backend.as_str() {
        "AI SDK" => text(config, "ai_sdk_provider"),
        other => other.to_owned(),
    };
    let api_key = text(config, "api_key");
    let (name, backend_here, settings) = match vendor.as_str() {
        "OpenAI" => (
            "openai",
            OPENAI_RESPONSES_BACKEND,
            json!({ CFG_API_KEY: api_key }),
        ),
        "Anthropic" => {
            let key = Some(text(config, "anthropic_api_key"))
                .filter(|k| !k.is_empty())
                .unwrap_or(api_key);
            ("anthropic", ANTHROPIC_BACKEND, json!({ CFG_API_KEY: key }))
        }
        "OpenAI-compatible API" => (
            "openai-compatible",
            OPENAI_CHAT_BACKEND,
            json!({ CFG_API_KEY: api_key, CFG_BASE_URL: text(config, "endpoint") }),
        ),
        other => {
            notes.push(format!(
                "the LLM provider was not imported: the Saltcorn 1 LLM plugin uses `{other}`, \
                 which this system has no backend for; create a provider named \
                 `saltcorn-llm` for the agents to use"
            ));
            return None;
        }
    };
    let model = text(config, "model");
    let models: Vec<Json> = (!model.is_empty())
        .then(|| json!({ "name": model, "is_default": true, "config": {} }))
        .into_iter()
        .collect();
    Some(json!({
        "name": name,
        "description": "Imported from the Saltcorn 1 large-language-model plugin's settings.",
        "backend": backend_here,
        "config": settings,
        "models": models,
    }))
}

/// The field names of a pack table, in order.
fn table_fields(pack: &Map<String, Json>, table: &str) -> Vec<String> {
    array(pack, "tables")
        .iter()
        .filter_map(Json::as_object)
        .find(|t| name_of(t) == table)
        .map(|t| {
            array(t, "fields")
                .iter()
                .filter_map(Json::as_object)
                .map(name_of)
                .collect()
        })
        .unwrap_or_default()
}

/// v1's form checkboxes are `"on"`, `true` or absent.
fn truthy(value: Option<&Json>) -> bool {
    match value {
        Some(Json::Bool(b)) => *b,
        Some(Json::String(s)) => !s.is_empty() && s != "false" && s != "off",
        Some(Json::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

fn list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> Map<String, Json> {
        json!({
            "tables": [ { "name": "document", "fields": [
                { "name": "id", "type": "Integer" }, { "name": "contents", "type": "String" },
                { "name": "secret", "type": "String" } ] } ],
            "plugins": [ { "name": "large-language-model", "configuration": {
                "backend": "AI SDK", "ai_sdk_provider": "OpenAI", "model": "gpt-x",
                "api_key": "sk-test" } } ],
            "triggers": [
                { "name": "MyAgent", "action": "Agent", "when_trigger": "Never", "min_role": 40,
                  "configuration": { "model": "", "sys_prompt": "Be brief", "skills": [
                    { "skill_type": "Run JavaScript code", "tool_name": "word_of_the_day",
                      "tool_description": "the word", "js_code": "return \"Strawberry\"",
                      "toolargs": [ { "name": "" } ] },
                    { "skill_type": "Run JavaScript code", "tool_name": "echo",
                      "js_code": "return text",
                      "toolargs": [ { "name": "text", "argtype": "string",
                                      "description": "what to say" } ] },
                    { "skill_type": "Table", "table_name": "document", "query": "on",
                      "insert": true, "hidden_fields": "secret" },
                    { "skill_type": "Retrieval" } ] } },
                { "name": "Other", "action": "run_js_code", "when_trigger": "Never" },
            ],
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn an_agent_trigger_becomes_an_agent_with_its_skills_as_traits() {
        let mut notes = Vec::new();
        let out = convert_agents(&pack(), &mut notes);
        assert_eq!(out.agents.len(), 1);
        let agent = &out.agents[0];
        assert_eq!(agent["name"], json!("MyAgent"));
        assert_eq!(agent["provider"], json!("openai"));
        assert_eq!(agent["system_prompt"], json!("Be brief"));
        assert_eq!(agent["min_role"], json!(40));
        // The JavaScript tools are the agent's own, code and arguments as written.
        assert_eq!(
            agent["traits"],
            json!([
                { "trait": "run_js_code", "config": { "tool_name": "word_of_the_day",
                    "description": "the word", "code": "return \"Strawberry\"",
                    "arguments": [] } },
                { "trait": "run_js_code", "config": { "tool_name": "echo",
                    "description": "", "code": "return text",
                    "arguments": [ { "name": "text", "type": "string",
                                     "description": "what to say" } ] } },
                { "trait": "query_table", "config": { "table": "document",
                                                       "fields": ["id", "contents"] } },
                { "trait": "insert_row", "config": { "table": "document" } },
            ])
        );
        // The provider is the plugin's settings, its model the default.
        assert_eq!(
            out.providers,
            vec![json!({ "name": "openai",
                "description": "Imported from the Saltcorn 1 large-language-model plugin's settings.",
                "backend": "openai_responses", "config": { "api_key": "sk-test" },
                "models": [ { "name": "gpt-x", "is_default": true, "config": {} } ] })]
        );
        assert!(
            notes.iter().any(|n| n.contains("`Retrieval` skill")),
            "{notes:?}"
        );
    }
}
