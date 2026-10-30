//! v1's agents — an `Agent` action from the `@saltcorn/agents` plugin — as this
//! system's agents (§11), and the LLM plugin's settings as the provider they
//! call through.
//!
//! In v1 an agent is a trigger whose action is `Agent`: its configuration holds
//! the system prompt, the model and a list of *skills*. Here an agent is an
//! entity of its own with *traits*, and calls a named LLM provider. Every skill
//! crosses as the trait that does the same job (TODO-agent-skills AS12):
//!
//! | v1 skill | here |
//! |---|---|
//! | `Table` (query, insert) | `query_table` and/or `edit_table_rows` (insert only) on that table; a list view is `show_results` |
//! | `Run JavaScript code` | `run_js_code`, the code kept in the agent; a button is `mode: button` |
//! | `Trigger` | `run_trigger` on that trigger |
//! | `Subagent` | `subagent` delegating to that agent, with its two prompts |
//! | `Fetch` | `http`, its tool still `fetch_web_page`, its cookie jar `cookies` |
//! | `Retrieval by full-text search` | `search_table` |
//! | `Retrieval by embedding` | `similar_rows` |
//! | `Preload Data` | `preload_rows` or `preload_code`, into the system prompt |
//! | `Memory` | `memory`, with v1's memories moved into `_fd_agent_memory` |
//! | `Model Context Protocol` | an MCP server definition and `mcp_server` |
//! | `Web search` | `web_search` |
//! | `Generate and run JavaScript code` | `generate_js` |
//! | `Ask user question`, `Plan approval` | `ask_user`, `plan_approval` |
//! | `Image generation`, `Text to speech` | `generate_image`, `speech` |
//! | `External Skill (SKILL.md)` | `skill_file`, over the imported file store |
//! | `Model picker` | the agent attribute `pickable_models` |
//! | `Prompt picker` | `prompt_picker` |
//!
//! A converted skill's additional prompt is appended to the agent's system
//! prompt, except Preload Data's, which heads the rows it loads. Compaction is
//! built in here, and Adaptive Feedback does nothing in v1. What a skill needs
//! outside the agent is arranged here too and applied where it belongs: a
//! full-text constraint on every table an agent searches (v1 searched without
//! one), the embedding model on every vector field an agent embeds into, the
//! models of each kind the provider needs, the MCP servers, and the file store
//! for generated images and skill files. Agents are written with the agents
//! they delegate to first, so a chain restores in one pass.
//!
//! A JavaScript tool's arguments are bare variables in its code in both
//! systems, so the code crosses as written. Nothing is added to the trigger
//! list: the tool is the agent's own.
//!
//! The provider is v1's `large-language-model` plugin configuration — the
//! vendor, the key and the default model — when it names a vendor this system
//! has a backend for. The restore keeps a provider of the same name that is
//! already here, key and all, and the agent then calls through that one. v1's
//! alternative configurations on that same vendor become more of its models,
//! which is what the model picker offers.
//!
//! v1's memory is personal or shared per memory, chosen by the model; here an
//! agent's `memory` trait stores one or the other. It is imported as personal,
//! the safe choice, and v1's shared memories stay shared.

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::{ConstraintKind, TableConstraint};
use sc_llm::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_BASE_URL, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND,
};
use sc_types::vector::{ATTR_EMBED_MODEL, ATTR_EMBED_PROVIDER};
use serde_json::{Map, Value as Json, json};

use super::{array, name_of, text};

/// The v1 action an agent is a trigger of.
pub(super) const AGENT_ACTION: &str = "Agent";
/// The v1 action that consolidates personal memories into preferences.
pub(super) const CONSOLIDATE_ACTION: &str = "consolidate_agent_memory";
/// The table v1's Memory skill keeps its memories in.
pub(super) const V1_MEMORY_TABLE: &str = "AgentLongTermMemory";
/// The v1 plugin holding the LLM settings.
const LLM_PLUGIN: &str = "large-language-model";
/// The language of the full-text constraint added to a table an agent searches.
const SEARCH_LANGUAGE: &str = "english";
/// Where in the imported store generated images are saved.
const IMAGE_ROOT: &str = "agent-images";
/// Where in the imported store a skill's directory is expected.
const SKILLS_ROOT: &str = "skills";
/// The longest MCP server name (`sc_mcp::MAX_NAME_CHARS`).
const MCP_NAME_CHARS: usize = 24;

/// The names a v1 query expression may use that are not the triggering row's
/// fields: JavaScript's own globals, and `user`.
const NOT_ROW_FIELDS: &[&str] = &[
    "user",
    "Math",
    "Date",
    "JSON",
    "Number",
    "String",
    "Boolean",
    "Array",
    "Object",
    "parseInt",
    "parseFloat",
    "isNaN",
    "undefined",
    "NaN",
    "Infinity",
    "encodeURIComponent",
    "decodeURIComponent",
];

/// What v1's agents became: the agents, the provider and MCP servers they
/// call, and what they need of the tables, the store and the memory store.
#[derive(Default)]
pub(super) struct ConvertedAgents {
    pub(super) agents: Vec<Json>,
    pub(super) providers: Vec<Json>,
    pub(super) mcp_servers: Vec<Json>,
    /// Tables an agent searches by full text, which the restore gives a
    /// full-text constraint.
    full_text: BTreeSet<String>,
    /// Vector fields an agent embeds text into, `(table, field)`, with the
    /// provider and embedding model that computes them.
    embedded: BTreeMap<(String, String), (String, String)>,
    /// Whether a trait keeps files in the imported store, which the import
    /// must then define even when the backup carries no files.
    pub(super) needs_store: bool,
    /// The agents with v1's Memory skill, in backup order.
    memory_agents: Vec<String>,
    /// v1 trigger ids of agents, where the backup says them.
    agent_ids: BTreeMap<i64, String>,
}

pub(super) fn convert_agents(
    pack: &Map<String, Json>,
    store: &str,
    notes: &mut Vec<String>,
) -> ConvertedAgents {
    let mut out = ConvertedAgents::default();
    let agents: Vec<Map<String, Json>> = array(pack, "triggers")
        .into_iter()
        .filter_map(|t| t.as_object().cloned())
        .filter(|t| text(t, "action") == AGENT_ACTION)
        .collect();
    if agents.is_empty() {
        return out;
    }
    let mut provider = provider(pack, notes);
    let provider_name = provider
        .as_ref()
        .map(|p| p.name.clone())
        // Nothing to derive one from: the agent still crosses, naming a
        // provider the restore reports missing, which an admin can create.
        .unwrap_or_else(|| "saltcorn-llm".to_owned());

    for agent in subagents_first(agents) {
        let name = name_of(&agent);
        if let Some(id) = agent.get("id").and_then(Json::as_i64) {
            out.agent_ids.insert(id, name.clone());
        }
        let config = agent
            .get("configuration")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default();
        let min_role = agent.get("min_role").and_then(Json::as_i64);
        let mut converted = Agent {
            name: name.clone(),
            ..Agent::default()
        };
        // The system prompt, then each converted skill's additional prompt:
        // there is no prompt per trait here, and the agent's is where it reads.
        let mut prompt = vec![text(&config, "sys_prompt")];
        for skill in array(&config, "skills") {
            let Some(skill) = skill.as_object() else {
                continue;
            };
            let before = converted.traits.len();
            let mut cx = Context {
                pack,
                store,
                provider: provider.as_mut(),
                out: &mut out,
                notes,
            };
            cx.convert_skill(skill, &mut converted);
            if converted.traits.len() > before && text(skill, "skill_type") != "Preload Data" {
                prompt.push(text(skill, "add_sys_prompt"));
            }
        }
        let system_prompt: Vec<String> = prompt
            .into_iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        // v1 runs an agent on one of the alternative configurations when its
        // `alt_config` says so: here that is the model it was made one of.
        let mut model = text(&config, "model");
        let alt = text(&config, "alt_config");
        if !alt.is_empty() {
            match provider.as_mut().and_then(|p| p.alternative(pack, &alt)) {
                Some(alt_model) => model = alt_model,
                None => notes.push(format!(
                    "agent `{name}` ran on the alternative LLM configuration `{alt}`, which \
                     is not one of the imported provider's models; it runs on the provider's \
                     default model"
                )),
            }
        }
        let mut value = json!({
            "name": name,
            "description": text(&agent, "description"),
            "provider": provider_name,
            "system_prompt": system_prompt.join("\n\n"),
            "model": model,
            "traits": converted.traits,
            "attributes": converted.attributes,
        });
        if let Some(role) = min_role {
            value["min_role"] = json!(role);
        }
        out.agents.push(value);
    }
    out.providers.extend(provider.map(Provider::into_json));
    out
}

impl ConvertedAgents {
    /// What the agents need of the converted table `table`: a full-text
    /// constraint where one searches it, and the embedding model on each
    /// vector field one embeds into.
    pub(super) fn amend_table(
        &self,
        table: &str,
        fields: &mut [Json],
        constraints: &mut Vec<Json>,
    ) {
        if self.full_text.contains(table) {
            let kind = ConstraintKind::FullTextSearch {
                language: SEARCH_LANGUAGE.to_owned(),
            };
            constraints.push(json!({
                "name": TableConstraint::derived_name(table, &kind, ""),
                "type": kind.type_name(),
                "language": SEARCH_LANGUAGE,
            }));
        }
        for field in fields.iter_mut() {
            let field_name = field
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some((provider, model)) = self.embedded.get(&(table.to_owned(), field_name)) else {
                continue;
            };
            let attributes = field
                .as_object_mut()
                .map(|f| f.entry("attributes").or_insert_with(|| json!({})));
            if let Some(Json::Object(attributes)) = attributes {
                attributes.insert(ATTR_EMBED_PROVIDER.to_owned(), json!(provider));
                attributes.insert(ATTR_EMBED_MODEL.to_owned(), json!(model));
            }
        }
    }

    /// Whether v1's memory table is moved into the memory store rather than
    /// imported as a table: whenever there is an agent to file it under.
    pub(super) fn takes_memories(&self) -> bool {
        !self.agents.is_empty()
    }

    /// The rows of v1's `AgentLongTermMemory` as `_fd_agent_memory` entries,
    /// each filed under its agent and its user (through `user`, which turns a
    /// v1 user number into the UUID it was given, or null).
    ///
    /// v1 says the agent by its trigger id, which a backup's triggers do not
    /// carry: where the backup does not say it, the one agent with a Memory
    /// skill is the agent. A preferences memory is nobody's agent's in v1, so
    /// each agent with the skill gets a copy.
    pub(super) fn convert_memories(
        &self,
        rows: &[Json],
        user: impl Fn(&Json) -> Json,
        notes: &mut Vec<String>,
    ) -> Vec<Json> {
        let mut out = Vec::new();
        let mut unmatched = 0;
        let mut ownerless = 0;
        for row in rows {
            let Some(row) = row.as_object() else {
                continue;
            };
            let preferences = text(row, "memory_type") == "PersonalPreferences";
            let agents: Vec<&String> = if preferences {
                self.memory_agents.iter().collect()
            } else {
                match row.get("agent_trigger_id").and_then(Json::as_i64) {
                    Some(id) if self.agent_ids.contains_key(&id) => vec![&self.agent_ids[&id]],
                    _ if self.memory_agents.len() == 1 => vec![&self.memory_agents[0]],
                    _ => Vec::new(),
                }
            };
            if agents.is_empty() {
                unmatched += 1;
                continue;
            }
            let personal = row.get("personal").and_then(Json::as_bool).unwrap_or(false);
            let user_id = row.get("user_id").map(&user).unwrap_or(Json::Null);
            if personal && user_id.is_null() {
                ownerless += 1;
                continue;
            }
            for agent in agents {
                out.push(json!({
                    "agent": agent,
                    "user_id": user_id,
                    "written_at": row.get("written_at").cloned().unwrap_or(Json::Null),
                    "kind": if preferences { "preferences" } else { "memory" },
                    "personal": personal,
                    "topic": text(row, "topic"),
                    "contents": text(row, "contents"),
                }));
            }
        }
        if unmatched > 0 {
            notes.push(format!(
                "{} of `{V1_MEMORY_TABLE}` not imported: the backup does not say which \
                 agent wrote {}",
                super::plural(unmatched, "memory was", "memories were"),
                if unmatched == 1 { "it" } else { "them" }
            ));
        }
        if ownerless > 0 {
            notes.push(format!(
                "{} of `{V1_MEMORY_TABLE}` not imported: personal, about a user this \
                 backup does not carry",
                super::plural(ownerless, "memory was", "memories were"),
            ));
        }
        out
    }

    /// v1's `consolidate_agent_memory` trigger as this system's
    /// `consolidate_memory` triggers: one per agent with the Memory skill,
    /// since v1's consolidated every agent's memories at once and here an
    /// action names its agent.
    pub(super) fn consolidations(&self, trigger: &str) -> Vec<(String, Json)> {
        let single = self.memory_agents.len() == 1;
        self.memory_agents
            .iter()
            .map(|agent| {
                let name = if single {
                    trigger.to_owned()
                } else {
                    format!("{trigger} ({agent})")
                };
                (name, json!({ "agent": agent }))
            })
            .collect()
    }
}

/// One agent as it is being converted.
#[derive(Default)]
struct Agent {
    name: String,
    traits: Vec<Json>,
    attributes: Map<String, Json>,
}

impl Agent {
    fn add(&mut self, trait_: &str, config: Json) {
        self.traits
            .push(json!({ "trait": trait_, "config": config }));
    }
}

/// What converting one skill reads and adds to.
struct Context<'a> {
    pack: &'a Map<String, Json>,
    store: &'a str,
    provider: Option<&'a mut Provider>,
    out: &'a mut ConvertedAgents,
    notes: &'a mut Vec<String>,
}

impl Context<'_> {
    /// One v1 skill of `agent` as the traits that do its job here, or a note
    /// saying why it has none.
    fn convert_skill(&mut self, skill: &Map<String, Json>, agent: &mut Agent) {
        let name = agent.name.clone();
        let kind = text(skill, "skill_type");
        match kind.as_str() {
            "Table" => {
                let table = text(skill, "table_name");
                if truthy(skill.get("query")) {
                    let mut config = json!({ "table": table });
                    let hidden = list(&text(skill, "hidden_fields"));
                    if !hidden.is_empty() {
                        config["fields"] = json!(
                            table_fields(self.pack, &table)
                                .into_iter()
                                .filter(|f| !hidden.contains(f))
                                .collect::<Vec<_>>()
                        );
                    }
                    if let Some(max) = positive(skill.get("max_rows")) {
                        config["max_rows"] = json!(max);
                    }
                    let view = text(skill, "list_view");
                    if !view.is_empty() {
                        config["show_results"] = json!(true);
                        if self.has_view(&view) {
                            config["results_view"] = json!(view);
                        } else {
                            self.notes.push(format!(
                                "the `{table}` table skill of agent `{name}` showed its \
                                 results in the view `{view}`, which the backup does not \
                                 carry: the rows are shown as a table"
                            ));
                        }
                    }
                    // v1's query tool always matched a phrase against every
                    // field; here that takes the table's full-text constraint.
                    self.out.full_text.insert(table.clone());
                    agent.add("query_table", config);
                }
                if truthy(skill.get("insert")) {
                    agent.add(
                        "edit_table_rows",
                        json!({ "table": table, "allow_insert": true }),
                    );
                }
            }
            "Run JavaScript code" => {
                if text(skill, "mode") == "Button" {
                    let label = Some(text(skill, "button_label"))
                        .filter(|l| !l.trim().is_empty())
                        .unwrap_or_else(|| "Run".to_owned());
                    agent.add(
                        "run_js_code",
                        json!({ "mode": "button", "button_label": label,
                                "code": text(skill, "js_code") }),
                    );
                    return;
                }
                let tool = text(skill, "tool_name");
                if tool.is_empty() {
                    self.notes.push(format!(
                        "a JavaScript tool of agent `{name}` was not imported: it has no name"
                    ));
                    return;
                }
                agent.traits.push(js_tool(skill, &tool));
            }
            "Trigger" => {
                agent.add(
                    "run_trigger",
                    json!({ "trigger": text(skill, "trigger_name") }),
                );
            }
            "Subagent" => {
                let target = text(skill, "agent_name");
                // v1 told the model what the sub-agent is for with its description.
                let when_to_use = array(self.pack, "triggers")
                    .iter()
                    .filter_map(Json::as_object)
                    .find(|t| name_of(t) == target)
                    .map(|t| text(t, "description"))
                    .unwrap_or_default();
                let mut config = json!({ "agent": target });
                if !when_to_use.is_empty() {
                    config["when_to_use"] = json!(when_to_use);
                }
                for field in ["handover_prompt", "handoff_prompt"] {
                    let prompt = text(skill, field);
                    if !prompt.trim().is_empty() {
                        config[field] = json!(prompt);
                    }
                }
                agent.add("subagent", config);
            }
            "Fetch" => {
                // Named so the tool is `fetch_web_page`, as it was in v1, and
                // allowed to send, as v1's was. Public addresses only, as every
                // `http` trait is unless an admin says otherwise.
                let mut config = json!({ "name": "web_page", "may_send": true });
                if truthy(skill.get("cookiejar")) {
                    config["cookies"] = json!(true);
                }
                agent.add("http", config);
            }
            "Retrieval by full-text search" => {
                let table = text(skill, "table_name");
                let mut config = json!({ "table": table, "mode": retrieval_mode(skill) });
                retrieval_options(skill, &mut config);
                self.list_view_note(skill, &name, &kind);
                self.out.full_text.insert(table);
                agent.add("search_table", config);
            }
            "Retrieval by embedding" => self.embedding(skill, agent),
            "Preload Data" => self.preload(skill, agent),
            "Memory" => {
                agent.add(
                    "memory",
                    json!({ "personal": true,
                            "preferences_in_header": truthy(skill.get("inject_personal_prefs")) }),
                );
                self.out.memory_agents.push(name);
            }
            "Model Context Protocol" => self.mcp(skill, agent),
            "Web search" => {
                let config = match text(skill, "search_provider").as_str() {
                    "By URL template" => json!({
                        "search_provider": "url_template",
                        "url_template": text(skill, "url_template"),
                        "header": text(skill, "header"),
                    }),
                    vendor @ ("Tavily" | "Firecrawl") => json!({
                        "search_provider": vendor.to_lowercase(),
                        "api_key": text(skill, "api_key"),
                        "include_domains": text(skill, "include_domains"),
                        "exclude_domains": text(skill, "exclude_domains"),
                    }),
                    other => {
                        self.notes.push(format!(
                            "the web search skill of agent `{name}` was not imported: this \
                             system has no search provider `{other}`"
                        ));
                        return;
                    }
                };
                agent.add("web_search", config);
            }
            "Generate and run JavaScript code" => {
                let tool = Some(text(skill, "tool_name"))
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "run_generated_code".to_owned());
                let table_access = match (
                    truthy(skill.get("allow_table")),
                    truthy(skill.get("table_read_only")),
                ) {
                    (false, _) => "none",
                    (true, true) => "read",
                    (true, false) => "write",
                };
                agent.add(
                    "generate_js",
                    json!({
                        "tool_name": tool,
                        "tool_description": text(skill, "tool_description"),
                        "code_description": text(skill, "code_description"),
                        "allow_fetch": truthy(skill.get("allow_fetch")),
                        "table_access": table_access,
                        // v1's "system functions" are its code pages' and
                        // modules' functions: here, the code libraries.
                        "allow_libraries": truthy(skill.get("allow_functions")),
                        "follow_up_prompt": text(skill, "follow_up_prompt"),
                    }),
                );
            }
            "Ask user question" => {
                let mut config = json!({});
                for (from, to) in [
                    ("question_sys_prompt", "prompt"),
                    ("question_answer_prompt", "answer_prompt"),
                    ("question_discuss_label", "discuss_label"),
                    ("question_discuss_prompt", "discuss_prompt"),
                ] {
                    set_text(&mut config, to, &text(skill, from));
                }
                agent.add("ask_user", config);
            }
            "Plan approval" => {
                let mut config = json!({});
                set_text(&mut config, "prompt", &text(skill, "ini_sys_prompt"));
                set_text(
                    &mut config,
                    "approval_prompt",
                    &text(skill, "approval_prompt"),
                );
                agent.add("plan_approval", config);
            }
            "Image generation" => {
                let Some((provider, model)) = self.model_of("image", "", &name, &kind) else {
                    return;
                };
                let format = Some(text(skill, "format"))
                    .filter(|f| !f.is_empty())
                    .unwrap_or_else(|| "png".to_owned());
                // A jpeg cannot be transparent; v1 let the vendor refuse it.
                let transparent = truthy(skill.get("transparent")) && format != "jpeg";
                self.out.needs_store = true;
                agent.add(
                    "generate_image",
                    json!({
                        "store": self.store, "root": IMAGE_ROOT,
                        "provider": provider, "model": model,
                        "size": text(skill, "size"), "quality": text(skill, "quality"),
                        "format": format, "transparent": transparent,
                    }),
                );
            }
            "Text to speech" => {
                let Some((provider, model)) = self.model_of("speech", "", &name, &kind) else {
                    return;
                };
                let mut config = json!({ "provider": provider, "model": model });
                set_text(&mut config, "voice", &text(skill, "voice"));
                set_text(&mut config, "format", &text(skill, "format"));
                set_text(&mut config, "instructions", &text(skill, "instructions"));
                if let Some(speed) = number(skill.get("speed")) {
                    config["speed"] = json!(speed);
                }
                agent.add("speech", config);
            }
            "External Skill (SKILL.md)" => {
                let dir = text(skill, "skills_dir");
                let leaf = dir
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|l| !l.is_empty())
                    .unwrap_or("skill")
                    .to_owned();
                let root = format!("{SKILLS_ROOT}/{leaf}");
                self.out.needs_store = true;
                agent.add(
                    "skill_file",
                    json!({ "store": self.store, "root": root, "file": "SKILL.md",
                            "read_tool": true }),
                );
                // The file itself is on the v1 server, not in its backup.
                self.notes.push(format!(
                    "the External Skill of agent `{name}` reads `SKILL.md` from `{root}` in \
                     the file store `{}`: copy the contents of `{dir}` on the Saltcorn 1 \
                     server there",
                    self.store
                ));
            }
            "Model picker" => self.model_picker(agent),
            "Prompt picker" => self.prompt_picker(skill, agent),
            // Compaction is built in here, on for every agent, and Adaptive
            // Feedback is a placeholder in v1 that does nothing: neither leaves
            // the imported agent short of anything.
            "Compaction" | "Adaptive Feedback" => {}
            other => self.notes.push(format!(
                "the `{other}` skill of agent `{name}` was not imported: this system has no \
                 counterpart for it"
            )),
        }
    }

    /// Retrieval by embedding: `similar_rows` over the vector field, which is
    /// given the embedding model that turns a phrase into one of its vectors.
    fn embedding(&mut self, skill: &Map<String, Json>, agent: &mut Agent) {
        let name = agent.name.clone();
        let vec_field = text(skill, "vec_field");
        let Some((table, field)) = vec_field.split_once('.') else {
            self.notes.push(format!(
                "the embedding retrieval of agent `{name}` was not imported: it names no \
                 vector field"
            ));
            return;
        };
        let wanted = self
            .provider
            .as_deref()
            .and_then(|p| p.alternative_embedding(self.pack, &text(skill, "alt_config_embedding")))
            .unwrap_or_default();
        let Some((provider, model)) =
            self.model_of("embedding", &wanted, &name, "Retrieval by embedding")
        else {
            return;
        };
        let mut config = json!({
            "table": table, "vector_field": field, "mode": retrieval_mode(skill),
        });
        let relation = text(skill, "doc_relation");
        if !relation.is_empty() {
            config["relation"] = json!(relation);
        }
        if let Some(limit) = positive(skill.get("limit")) {
            config["limit"] = json!(limit);
        }
        retrieval_options(skill, &mut config);
        self.list_view_note(skill, &name, "Retrieval by embedding");
        self.out
            .embedded
            .insert((table.to_owned(), field.to_owned()), (provider, model));
        agent.add("similar_rows", config);
    }

    /// Preload Data: rows of a table, or what a piece of code returns, in the
    /// system prompt, where v1 put them.
    fn preload(&mut self, skill: &Map<String, Json>, agent: &mut Agent) {
        let name = agent.name.clone();
        if text(skill, "data_source") == "Code" {
            let code = text(skill, "code");
            if code.contains("systemPrompt") || code.contains("interactions") {
                self.notes.push(format!(
                    "the Preload Data code of agent `{name}` returns `systemPrompt` or \
                     `interactions`: here the code returns `system_prompt` and `messages`; \
                     change it to match"
                ));
            }
            agent.add(
                "preload_code",
                json!({ "code": code, "placement": "system_prompt" }),
            );
            return;
        }
        let table = text(skill, "table_name");
        let filter = match preload_filter(&text(skill, "preload_query")) {
            Ok(filter) => filter,
            Err(e) => {
                self.notes.push(format!(
                    "the Preload Data skill of agent `{name}` was not imported: its query \
                     `{}` could not be translated: {}",
                    text(skill, "preload_query"),
                    e.causes()
                ));
                return;
            }
        };
        let mut config = json!({ "table": table, "placement": "system_prompt" });
        set_text(&mut config, "filter", &filter);
        set_text(&mut config, "heading", &text(skill, "add_sys_prompt"));
        let contents = text(skill, "contents_expr");
        if contents.trim().is_empty() {
            let hidden = list(&text(skill, "hidden_fields"));
            if !hidden.is_empty() {
                config["fields"] = json!(
                    table_fields(self.pack, &table)
                        .into_iter()
                        .filter(|f| !hidden.contains(f))
                        .collect::<Vec<_>>()
                );
            }
        } else {
            config["contents"] = json!(contents);
        }
        agent.add("preload_rows", config);
    }

    /// Model Context Protocol: v1 handed the server to the vendor; here it is
    /// a server definition of its own, its header kept with the others' secrets.
    fn mcp(&mut self, skill: &Map<String, Json>, agent: &mut Agent) {
        let label = text(skill, "server_label");
        let url = text(skill, "server_url").trim().to_owned();
        let server = mcp_name(&label);
        if url.is_empty() {
            self.notes.push(format!(
                "the MCP server `{label}` of agent `{}` was not imported: it has no URL",
                agent.name
            ));
            return;
        }
        let headers: Vec<Json> = Some(text(skill, "header_key"))
            .filter(|k| !k.trim().is_empty())
            .map(|k| json!({ "name": k.trim(), "value": text(skill, "header_value") }))
            .into_iter()
            .collect();
        let known = self
            .out
            .mcp_servers
            .iter()
            .any(|s| s["name"] == json!(server));
        if !known {
            self.out.mcp_servers.push(json!({
                "name": server,
                "description": format!("Imported from the Saltcorn 1 agent skill `{label}`."),
                "url": url,
                "transport": "streamable_http",
                "headers": headers,
            }));
        }
        agent.add("mcp_server", json!({ "server": server }));
    }

    /// Model picker: v1's alternative configurations, as far as they are
    /// models of the imported provider.
    fn model_picker(&mut self, agent: &mut Agent) {
        let Some(provider) = self.provider.as_deref_mut() else {
            self.notes.push(format!(
                "the model picker of agent `{}` was not imported: there is no imported \
                 provider for its models",
                agent.name
            ));
            return;
        };
        let (picks, left) = provider.alternatives(self.pack);
        if !left.is_empty() {
            self.notes.push(format!(
                "the model picker of agent `{}` does not offer {}: {} not on the imported \
                 provider",
                agent.name,
                left.iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if left.len() == 1 { "it is" } else { "they are" }
            ));
        }
        if picks.is_empty() {
            return;
        }
        agent
            .attributes
            .insert("pickable_models".to_owned(), Json::Array(picks));
    }

    fn prompt_picker(&mut self, skill: &Map<String, Json>, agent: &mut Agent) {
        let name = agent.name.clone();
        let mut config = json!({ "send": truthy(skill.get("run_on_select")) });
        set_text(&mut config, "label", &text(skill, "placeholder"));
        if text(skill, "source") == "Table" {
            config["source"] = json!("table");
            config["table"] = json!(text(skill, "table"));
            config["label_field"] = json!(text(skill, "label_field"));
            config["prompt_field"] = json!(text(skill, "prompt_field"));
            let query = text(skill, "query");
            if !query.trim().is_empty() {
                match static_where(&query) {
                    Some(where_) => config["where"] = where_,
                    None => {
                        self.notes.push(format!(
                            "the prompt picker of agent `{name}` offers every row of `{}`: \
                             its query `{query}` is not a fixed `where` object",
                            text(skill, "table")
                        ));
                    }
                }
            }
        } else {
            config["source"] = json!("predefined");
            let prompts: Vec<Json> = match skill.get("options_array").and_then(Json::as_array) {
                Some(options) => options
                    .iter()
                    .filter_map(Json::as_object)
                    .map(|o| {
                        json!({ "label": text(o, "promptpicker_label"),
                                "prompt": text(o, "promptpicker_sysprompt") })
                    })
                    .collect(),
                None => match serde_json::from_str::<Map<String, Json>>(&text(skill, "options_obj"))
                {
                    Ok(options) => options
                        .iter()
                        .map(|(label, prompt)| {
                            json!({ "label": label, "prompt": prompt.as_str().unwrap_or_default() })
                        })
                        .collect(),
                    Err(e) => {
                        self.notes.push(format!(
                            "the prompt picker of agent `{name}` was not imported: its \
                             prompts are not a JSON object: {e}"
                        ));
                        return;
                    }
                },
            };
            config["prompts"] = json!(prompts);
        }
        agent.add("prompt_picker", config);
    }

    /// The provider and model of `kind` a media or embedding skill calls, made one of the
    /// provider's models — or `None`, with a note, when there is none to call.
    fn model_of(
        &mut self,
        kind: &str,
        wanted: &str,
        agent: &str,
        skill: &str,
    ) -> Option<(String, String)> {
        let found = self
            .provider
            .as_deref_mut()
            .map(|p| (p.name.clone(), p.model_of(kind, wanted)));
        match found {
            Some((provider, Some(model))) => Some((provider, model)),
            Some((provider, None)) => {
                self.notes.push(format!(
                    "the {skill} skill of agent `{agent}` was not imported: the imported \
                     provider `{provider}` has no {kind} models"
                ));
                None
            }
            None => {
                self.notes.push(format!(
                    "the {skill} skill of agent `{agent}` was not imported: there is no \
                     imported provider to call"
                ));
                None
            }
        }
    }

    fn has_view(&self, view: &str) -> bool {
        array(self.pack, "views")
            .iter()
            .filter_map(Json::as_object)
            .any(|v| name_of(v) == view)
    }

    fn list_view_note(&mut self, skill: &Map<String, Json>, agent: &str, kind: &str) {
        let view = text(skill, "list_view");
        if !view.is_empty() {
            self.notes.push(format!(
                "the {kind} skill of agent `{agent}` showed what it found in the view \
                 `{view}`: here the model is given the rows"
            ));
        }
    }
}

/// v1's two retrieval modes as this system's.
fn retrieval_mode(skill: &Map<String, Json>) -> &'static str {
    match text(skill, "mode").as_str() {
        "Search on every user input" => "every_message",
        _ => "tool",
    }
}

/// The hidden fields and document format both retrieval skills have.
fn retrieval_options(skill: &Map<String, Json>, config: &mut Json) {
    let hidden = list(&text(skill, "hidden_fields"));
    if !hidden.is_empty() {
        config["hidden_fields"] = json!(hidden);
    }
    set_text(config, "doc_format", &text(skill, "doc_format"));
}

/// v1's Preload Data query — a JavaScript expression over the triggering
/// row's fields and `user` — as `preload_rows`' filter, which reads the row as
/// `row` (null in a chat, where v1's was `{}`).
fn preload_filter(query: &str) -> sc_error::Result<String> {
    if query.trim().is_empty() {
        return Ok(String::new());
    }
    let out = sc_expr::rewrite_free_refs(query, &mut |path| {
        let head = path[0].as_str();
        (!NOT_ROW_FIELDS.contains(&head)).then(|| (1, format!("row?.{head}")))
    })?;
    Ok(out.trim().to_owned())
}

/// A v1 query that is a fixed `where` object — every value a JSON literal —
/// as that object.
fn static_where(query: &str) -> Option<Json> {
    if let Ok(Json::Object(object)) = serde_json::from_str(query) {
        return Some(Json::Object(object));
    }
    let entries = sc_expr::object_literal_entries(query).ok()?;
    let mut out = Map::new();
    for (key, value) in entries {
        out.insert(key, serde_json::from_str(value.trim()).ok()?);
    }
    Some(Json::Object(out))
}

/// v1's MCP server label as a server name here: lowercase letters, digits,
/// `-` and `_`, as its tools' `<server>__<tool>` names need.
fn mcp_name(label: &str) -> String {
    let mut out = String::new();
    for c in label.trim().chars() {
        let c = c.to_ascii_lowercase();
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            c
        } else {
            '_'
        };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    let out: String = out.trim_matches('_').chars().take(MCP_NAME_CHARS).collect();
    let out = out.trim_end_matches('_').to_owned();
    if out.is_empty() {
        "mcp".to_owned()
    } else {
        out
    }
}

/// The agents with each one's sub-agents before it, so that one restore pass
/// saves a whole delegation chain: the `subagent` trait refuses an agent that
/// is not stored yet. A cycle keeps the backup's order for what is left.
fn subagents_first(mut pending: Vec<Map<String, Json>>) -> Vec<Map<String, Json>> {
    let targets = |agent: &Map<String, Json>| -> Vec<String> {
        let config = agent.get("configuration").and_then(Json::as_object);
        config
            .map(|c| array(c, "skills"))
            .unwrap_or_default()
            .iter()
            .filter_map(Json::as_object)
            .filter(|s| text(s, "skill_type") == "Subagent")
            .map(|s| text(s, "agent_name"))
            .collect()
    };
    let mut ordered: Vec<Map<String, Json>> = Vec::new();
    while !pending.is_empty() {
        let ready = pending.iter().position(|agent| {
            targets(agent)
                .iter()
                .all(|t| *t == name_of(agent) || !pending.iter().any(|other| name_of(other) == *t))
        });
        ordered.push(pending.remove(ready.unwrap_or(0)));
    }
    ordered
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

// --- the provider ---------------------------------------------------------------

/// A vendor this system has a backend for.
#[derive(Clone, Copy, PartialEq)]
enum Vendor {
    OpenAi,
    Anthropic,
    Compatible,
}

/// The provider being made of v1's LLM plugin settings, with the models the
/// agents turn out to need.
struct Provider {
    name: String,
    vendor: Vendor,
    /// The plugin's own settings, for its alternative configurations and its
    /// embedding, image and speech models.
    plugin: Map<String, Json>,
    document: Json,
    models: Vec<Json>,
}

/// The model of a kind a v1 plugin names when it names none: its own defaults.
fn default_model(kind: &str) -> &'static str {
    match kind {
        "embedding" => "text-embedding-3-small",
        "image" => "gpt-image-1",
        _ => "tts-1",
    }
}

impl Provider {
    /// The model of `kind` to call — `wanted`, or the plugin's own setting for
    /// the kind, or its default — added to the provider's models if it is not
    /// one yet. `None` for a kind the vendor has no endpoint for.
    fn model_of(&mut self, kind: &str, wanted: &str) -> Option<String> {
        if kind != "chat" && self.vendor == Vendor::Anthropic {
            return None;
        }
        let setting = match kind {
            "embedding" => "embed_model",
            "image" => "image_model",
            "speech" => "tts_model",
            _ => "model",
        };
        let name = [wanted.to_owned(), text(&self.plugin, setting)]
            .into_iter()
            .find(|n| !n.trim().is_empty())
            .unwrap_or_else(|| default_model(kind).to_owned());
        self.ensure(&name, kind);
        Some(name)
    }

    /// `name` among the models, as `kind` — its kind's default if it is the
    /// first of it. A name the provider already has keeps the kind it has.
    fn ensure(&mut self, name: &str, kind: &str) {
        if self.models.iter().any(|m| m["name"] == json!(name)) {
            return;
        }
        // The plugin's own model is written without a kind: a chat model.
        let kind_of = |m: &Json| m["kind"].as_str().unwrap_or("chat").to_owned();
        let first = !self.models.iter().any(|m| kind_of(m) == kind);
        self.models.push(json!({
            "name": name, "kind": kind, "is_default": first, "config": {},
        }));
    }

    /// v1's alternative configurations, each as a model a person may pick, as
    /// far as each is a model of this provider; and the names of the others.
    fn alternatives(&mut self, pack: &Map<String, Json>) -> (Vec<Json>, Vec<String>) {
        let mut picks: Vec<Json> = Vec::new();
        let mut left = Vec::new();
        for alt in alt_configs(pack) {
            let label = text(&alt, "name");
            if label.trim().is_empty() {
                continue;
            }
            match self.alternative_model(&alt) {
                Some(model) if !picks.iter().any(|p| p["model"] == json!(model)) => {
                    self.ensure(&model, "chat");
                    picks.push(json!({ "provider": self.name, "model": model, "label": label }));
                }
                Some(_) => {}
                None => left.push(label),
            }
        }
        (picks, left)
    }

    /// The model the alternative configuration named `name` is, made one of
    /// this provider's chat models, when it is on this provider.
    fn alternative(&mut self, pack: &Map<String, Json>, name: &str) -> Option<String> {
        if name.trim().is_empty() {
            return None;
        }
        let alt = alt_configs(pack)
            .into_iter()
            .find(|a| text(a, "name") == name)?;
        let model = self.alternative_model(&alt)?;
        self.ensure(&model, "chat");
        Some(model)
    }

    /// The embedding model of the alternative configuration named `name`,
    /// when it is on this provider.
    fn alternative_embedding(&self, pack: &Map<String, Json>, name: &str) -> Option<String> {
        if name.trim().is_empty() {
            return None;
        }
        let alt = alt_configs(pack)
            .into_iter()
            .find(|a| text(a, "name") == name)?;
        self.alternative_model(&alt)?;
        Some(text(&alt, "embed_model")).filter(|m| !m.trim().is_empty())
    }

    /// An alternative configuration's model, if it is on this provider: the
    /// same vendor and, where it says them, the same endpoint and key.
    fn alternative_model(&self, alt: &Map<String, Json>) -> Option<String> {
        let model = text(alt, "model");
        if model.trim().is_empty() {
            return None;
        }
        let same = |key: &str, mine: &str| {
            let theirs = text(alt, key);
            theirs.trim().is_empty() || theirs == text(&self.plugin, mine)
        };
        let on_this = match text(&self.plugin, "backend").as_str() {
            "AI SDK" => {
                text(alt, "alt_provider") == text(&self.plugin, "ai_sdk_provider")
                    && match self.vendor {
                        Vendor::Anthropic => same("anthropic_api_key", "anthropic_api_key"),
                        _ => same("api_key", "api_key"),
                    }
            }
            _ => {
                self.vendor == Vendor::Compatible
                    && same("endpoint", "endpoint")
                    && same("api_key", "api_key")
            }
        };
        on_this.then_some(model)
    }

    fn into_json(self) -> Json {
        let mut document = self.document;
        document["models"] = Json::Array(self.models);
        document
    }
}

/// The LLM plugin's alternative configurations, whichever backend's list
/// holds them.
fn alt_configs(pack: &Map<String, Json>) -> Vec<Map<String, Json>> {
    let Some(config) = llm_plugin(pack) else {
        return Vec::new();
    };
    let key = match text(&config, "backend").as_str() {
        "AI SDK" => "alt_aisdk_configs",
        _ => "altconfigs",
    };
    array(&config, key)
        .into_iter()
        .filter_map(|a| a.as_object().cloned())
        .collect()
}

fn llm_plugin(pack: &Map<String, Json>) -> Option<Map<String, Json>> {
    array(pack, "plugins")
        .into_iter()
        .filter_map(|p| p.as_object().cloned())
        .find(|p| name_of(p) == LLM_PLUGIN)?
        .get("configuration")
        .and_then(Json::as_object)
        .cloned()
}

/// The LLM provider v1's `large-language-model` plugin was configured with, in
/// the shape `llm-providers.json` carries one — or `None`, with a note, when
/// it names a vendor this system has no backend for.
fn provider(pack: &Map<String, Json>, notes: &mut Vec<String>) -> Option<Provider> {
    let Some(config) = llm_plugin(pack) else {
        notes.push(
            "the agents call an LLM provider named `saltcorn-llm`: the backup has no \
             LLM plugin configuration to make one from, so create it before using them"
                .to_owned(),
        );
        return None;
    };
    let backend = text(&config, "backend");
    let vendor_name = match backend.as_str() {
        "AI SDK" => text(&config, "ai_sdk_provider"),
        other => other.to_owned(),
    };
    let api_key = text(&config, "api_key");
    let (name, vendor, backend_here, settings) = match vendor_name.as_str() {
        "OpenAI" => (
            "openai",
            Vendor::OpenAi,
            OPENAI_RESPONSES_BACKEND,
            json!({ CFG_API_KEY: api_key }),
        ),
        "Anthropic" => {
            let key = Some(text(&config, "anthropic_api_key"))
                .filter(|k| !k.is_empty())
                .unwrap_or(api_key);
            (
                "anthropic",
                Vendor::Anthropic,
                ANTHROPIC_BACKEND,
                json!({ CFG_API_KEY: key }),
            )
        }
        "OpenAI-compatible API" => (
            "openai-compatible",
            Vendor::Compatible,
            OPENAI_CHAT_BACKEND,
            json!({ CFG_API_KEY: api_key, CFG_BASE_URL: text(&config, "endpoint") }),
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
    let model = text(&config, "model");
    let models: Vec<Json> = (!model.is_empty())
        .then(|| json!({ "name": model, "is_default": true, "config": {} }))
        .into_iter()
        .collect();
    Some(Provider {
        name: name.to_owned(),
        vendor,
        document: json!({
            "name": name,
            "description": "Imported from the Saltcorn 1 large-language-model plugin's settings.",
            "backend": backend_here,
            "config": settings,
        }),
        plugin: config,
        models,
    })
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

/// Set `key` to `value` unless it is blank: an unset option is the trait's
/// default, which is what v1's blank meant.
fn set_text(config: &mut Json, key: &str, value: &str) {
    if !value.trim().is_empty() {
        config[key] = json!(value);
    }
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

/// A v1 number field: a number or a numeric string.
fn number(value: Option<&Json>) -> Option<f64> {
    match value? {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A v1 number field read as a positive integer.
fn positive(value: Option<&Json>) -> Option<i64> {
    let n = number(value)?;
    (n >= 1.0).then_some(n.floor() as i64)
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
        let out = convert_agents(&pack(), "store", &mut notes);
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
                { "trait": "edit_table_rows",
                  "config": { "table": "document", "allow_insert": true } },
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
        // v1's query tool searched every field: the table gets the constraint.
        let mut constraints = Vec::new();
        out.amend_table("document", &mut [], &mut constraints);
        assert_eq!(
            constraints,
            vec![
                json!({ "name": "sc_fts_document", "type": "full_text_search",
                         "language": "english" })
            ]
        );
    }

    fn agent(name: &str, description: &str, skills: Json) -> Json {
        json!({ "name": name, "action": "Agent", "description": description,
                "configuration": { "sys_prompt": "", "skills": skills } })
    }

    fn convert_with(plugin: Json, tables: Json, triggers: Json) -> (ConvertedAgents, Vec<String>) {
        let pack = json!({
            "tables": tables,
            "views": [ { "name": "Docs" } ],
            "plugins": [ { "name": "large-language-model", "configuration": plugin } ],
            "triggers": triggers,
        });
        let mut notes = Vec::new();
        let out = convert_agents(pack.as_object().unwrap(), "MyApp", &mut notes);
        (out, notes)
    }

    fn convert(triggers: Json) -> (ConvertedAgents, Vec<String>) {
        let pack = json!({ "tables": [], "plugins": [], "triggers": triggers });
        let mut notes = Vec::new();
        let out = convert_agents(pack.as_object().unwrap(), "store", &mut notes);
        (out, notes)
    }

    fn openai() -> Json {
        json!({ "backend": "AI SDK", "ai_sdk_provider": "OpenAI", "model": "gpt-x",
                "api_key": "sk-test", "embed_model": "text-embedding-3-large",
                "alt_aisdk_configs": [
                    { "name": "Fast", "alt_provider": "OpenAI", "model": "gpt-x-mini",
                      "api_key": "" },
                    { "name": "Elsewhere", "alt_provider": "Anthropic", "model": "claude",
                      "anthropic_api_key": "k" },
                    { "name": "", "alt_provider": "OpenAI", "model": "gpt-5" } ] })
    }

    /// The one trait a single-skill agent converts to.
    fn only_trait(skill: Json) -> (Json, ConvertedAgents, Vec<String>) {
        let (out, notes) = convert_with(
            openai(),
            json!([ { "name": "chunks", "fields": [
                { "name": "id", "type": "Integer" }, { "name": "text", "type": "String" },
                { "name": "embedding", "type": "PGVector" },
                { "name": "doc", "type": "Key" } ] } ]),
            json!([agent("A", "", json!([skill]))]),
        );
        let traits = out.agents[0]["traits"].as_array().unwrap().clone();
        assert_eq!(traits.len(), 1, "{traits:?} {notes:?}");
        (traits[0].clone(), out, notes)
    }

    #[test]
    fn subagents_are_delegated_to_and_written_first_with_their_prompts() {
        let (out, notes) = convert(json!([
            agent(
                "Boss",
                "",
                json!([ { "skill_type": "Subagent", "agent_name": "Middle",
                          "handover_prompt": "You work for Boss",
                          "handoff_prompt": "Analyse it" } ])
            ),
            agent(
                "Middle",
                "Does the middle part",
                json!([
                { "skill_type": "Subagent", "agent_name": "Leaf" } ])
            ),
            agent("Leaf", "", json!([])),
        ]));
        let names: Vec<&Json> = out.agents.iter().map(|a| &a["name"]).collect();
        assert_eq!(names, [&json!("Leaf"), &json!("Middle"), &json!("Boss")]);
        assert_eq!(
            out.agents[2]["traits"],
            json!([ { "trait": "subagent", "config": { "agent": "Middle",
                        "when_to_use": "Does the middle part",
                        "handover_prompt": "You work for Boss",
                        "handoff_prompt": "Analyse it" } } ])
        );
        assert_eq!(
            out.agents[1]["traits"],
            json!([ { "trait": "subagent", "config": { "agent": "Leaf" } } ])
        );
        assert!(!notes.iter().any(|n| n.contains("prompt")), "{notes:?}");
    }

    #[test]
    fn a_delegation_cycle_still_converts_every_agent() {
        let (out, _) = convert(json!([
            agent(
                "A",
                "",
                json!([ { "skill_type": "Subagent", "agent_name": "B" } ])
            ),
            agent(
                "B",
                "",
                json!([ { "skill_type": "Subagent", "agent_name": "A" } ])
            ),
        ]));
        assert_eq!(out.agents.len(), 2);
    }

    #[test]
    fn fetch_buttons_list_views_and_compaction_each_get_a_deliberate_outcome() {
        let (out, notes) = convert_with(
            openai(),
            json!([]),
            json!([ {
            "name": "Helper", "action": "Agent",
            "configuration": { "sys_prompt": "Be brief", "skills": [
                { "skill_type": "Fetch", "cookiejar": true },
                { "skill_type": "Table", "table_name": "document", "query": "on",
                  "max_rows": "20", "list_view": "Docs", "add_sys_prompt": "Prefer recent." },
                { "skill_type": "Table", "table_name": "other", "query": "on",
                  "list_view": "Gone" },
                { "skill_type": "Run JavaScript code", "mode": "Button",
                  "button_label": "Go", "js_code": "return 1", "add_sys_prompt": "Pressed." },
                { "skill_type": "Compaction", "trigger_tokens": 1000 },
                { "skill_type": "Adaptive Feedback" } ] } } ]),
        );
        let agent = &out.agents[0];
        assert_eq!(
            agent["traits"],
            json!([
                { "trait": "http", "config": { "name": "web_page", "may_send": true,
                                               "cookies": true } },
                { "trait": "query_table", "config": { "table": "document", "max_rows": 20,
                    "show_results": true, "results_view": "Docs" } },
                { "trait": "query_table", "config": { "table": "other",
                    "show_results": true } },
                { "trait": "run_js_code", "config": { "mode": "button", "button_label": "Go",
                                                      "code": "return 1" } },
            ])
        );
        assert_eq!(
            agent["system_prompt"],
            json!("Be brief\n\nPrefer recent.\n\nPressed.")
        );
        assert!(notes.iter().any(|n| n.contains("view `Gone`")), "{notes:?}");
        // Compaction is built in here and Adaptive Feedback does nothing in
        // v1, so neither is reported.
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn full_text_retrieval_is_search_table_and_the_table_gets_its_constraint() {
        let (found, out, notes) = only_trait(json!({
            "skill_type": "Retrieval by full-text search", "mode": "Search on every user input",
            "table_name": "chunks", "hidden_fields": "doc", "doc_format": "{{text}}" }));
        assert_eq!(
            found,
            json!({ "trait": "search_table", "config": { "table": "chunks",
                "mode": "every_message", "hidden_fields": ["doc"], "doc_format": "{{text}}" } })
        );
        assert!(notes.is_empty(), "{notes:?}");
        let mut constraints = Vec::new();
        out.amend_table("chunks", &mut [], &mut constraints);
        assert_eq!(constraints[0]["type"], json!("full_text_search"));
    }

    #[test]
    fn embedding_retrieval_is_similar_rows_and_the_field_gets_its_model() {
        let (found, out, notes) = only_trait(json!({
            "skill_type": "Retrieval by embedding", "mode": "Tool",
            "vec_field": "chunks.embedding", "doc_relation": "doc", "limit": "5" }));
        assert_eq!(
            found,
            json!({ "trait": "similar_rows", "config": { "table": "chunks",
                "vector_field": "embedding", "mode": "tool", "relation": "doc", "limit": 5 } })
        );
        assert!(notes.is_empty(), "{notes:?}");
        let mut fields = vec![json!({ "name": "embedding", "type": "vector(3)" })];
        out.amend_table("chunks", &mut fields, &mut Vec::new());
        assert_eq!(
            fields[0]["attributes"],
            json!({ "embed_provider": "openai", "embed_model": "text-embedding-3-large" })
        );
        // The plugin's embedding model is one of the provider's, of its kind.
        assert!(
            out.providers[0]["models"]
                .as_array()
                .unwrap()
                .contains(&json!({
                "name": "text-embedding-3-large", "kind": "embedding",
                "is_default": true, "config": {} })),
            "{:?}",
            out.providers
        );
    }

    #[test]
    fn preload_data_reads_the_triggering_row_as_row() {
        let (found, _, notes) = only_trait(json!({
            "skill_type": "Preload Data", "data_source": "Table", "table_name": "chunks",
            "preload_query": "{doc: doc_id, owner: user.id}", "add_sys_prompt": "Context:",
            "hidden_fields": "embedding" }));
        assert_eq!(
            found,
            json!({ "trait": "preload_rows", "config": { "table": "chunks",
                "placement": "system_prompt", "filter": "{doc: row?.doc_id, owner: user.id}",
                "heading": "Context:", "fields": ["id", "text", "doc"] } })
        );
        assert!(notes.is_empty(), "{notes:?}");
        let (found, _, _) = only_trait(json!({
            "skill_type": "Preload Data", "data_source": "Code", "code": "return 'hi'" }));
        assert_eq!(
            found,
            json!({ "trait": "preload_code", "config": { "code": "return 'hi'",
                                                         "placement": "system_prompt" } })
        );
        // A query that is not JavaScript is reported, not guessed at.
        let (out, notes) = convert(json!([agent(
            "A",
            "",
            json!([ {
            "skill_type": "Preload Data", "table_name": "t", "preload_query": "{a: " } ])
        )]));
        assert_eq!(out.agents[0]["traits"], json!([]));
        assert!(
            notes.iter().any(|n| n.contains("could not be translated")),
            "{notes:?}"
        );
    }

    #[test]
    fn memories_are_filed_under_their_agent_and_user_and_consolidation_names_the_agent() {
        let (found, out, notes) = only_trait(json!({
            "skill_type": "Memory", "inject_personal_prefs": true }));
        assert_eq!(
            found,
            json!({ "trait": "memory", "config": { "personal": true,
                                                   "preferences_in_header": true } })
        );
        assert!(notes.is_empty(), "{notes:?}");
        let rows = json!([
            { "id": 1, "user_id": 7, "agent_trigger_id": 3, "memory_type": "Episodic",
              "personal": true, "contents": "Likes tea", "written_at": "2026-01-02T03:04:05.000Z" },
            { "id": 2, "user_id": null, "memory_type": "Episodic", "personal": false,
              "contents": "Closed on the 24th" },
            { "id": 3, "user_id": 7, "memory_type": "PersonalPreferences", "personal": true,
              "contents": "The user likes tea" },
            { "id": 4, "user_id": 8, "memory_type": "Episodic", "personal": true,
              "contents": "Nobody's" },
        ]);
        let mut notes = Vec::new();
        let user = |id: &Json| match id.as_i64() {
            Some(7) => json!("u-7"),
            _ => Json::Null,
        };
        let memories = out.convert_memories(rows.as_array().unwrap(), user, &mut notes);
        assert_eq!(
            memories,
            vec![
                json!({ "agent": "A", "user_id": "u-7", "written_at": "2026-01-02T03:04:05.000Z",
                        "kind": "memory", "personal": true, "topic": "", "contents": "Likes tea" }),
                json!({ "agent": "A", "user_id": null, "written_at": null, "kind": "memory",
                        "personal": false, "topic": "", "contents": "Closed on the 24th" }),
                json!({ "agent": "A", "user_id": "u-7", "written_at": null,
                        "kind": "preferences", "personal": true, "topic": "",
                        "contents": "The user likes tea" }),
            ]
        );
        assert!(
            notes.iter().any(|n| n.contains("1 memory was")),
            "{notes:?}"
        );
        assert_eq!(
            out.consolidations("Nightly"),
            vec![("Nightly".to_owned(), json!({ "agent": "A" }))]
        );
    }

    #[test]
    fn an_mcp_skill_is_a_server_definition_and_mcp_server() {
        let (found, out, notes) = only_trait(json!({
            "skill_type": "Model Context Protocol", "server_label": "My Docs Server!",
            "server_url": "https://mcp.example.com/mcp", "header_key": "Authorization",
            "header_value": "Bearer s3cret" }));
        assert_eq!(
            found,
            json!({ "trait": "mcp_server", "config": { "server": "my_docs_server" } })
        );
        assert_eq!(
            out.mcp_servers,
            vec![json!({ "name": "my_docs_server",
                "description": "Imported from the Saltcorn 1 agent skill `My Docs Server!`.",
                "url": "https://mcp.example.com/mcp", "transport": "streamable_http",
                "headers": [ { "name": "Authorization", "value": "Bearer s3cret" } ] })]
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(mcp_name("a__b--c"), "a_b--c");
        assert_eq!(mcp_name("???"), "mcp");
    }

    #[test]
    fn web_search_generated_code_questions_and_plans_cross_setting_for_setting() {
        let (found, _, _) = only_trait(json!({
            "skill_type": "Web search", "search_provider": "Tavily", "api_key": "tv",
            "include_domains": "a.com", "exclude_domains": "" }));
        assert_eq!(
            found,
            json!({ "trait": "web_search", "config": { "search_provider": "tavily",
                "api_key": "tv", "include_domains": "a.com", "exclude_domains": "" } })
        );
        let (found, _, _) = only_trait(json!({
            "skill_type": "Web search", "search_provider": "By URL template",
            "url_template": "https://s.example/?q={{q}}", "header": "X-Key: 1" }));
        assert_eq!(found["config"]["search_provider"], json!("url_template"));
        assert_eq!(found["config"]["header"], json!("X-Key: 1"));

        let (found, _, _) = only_trait(json!({
            "skill_type": "Generate and run JavaScript code", "tool_name": "compute",
            "tool_description": "Compute", "code_description": "Tables: t",
            "allow_fetch": true, "allow_table": true, "table_read_only": true,
            "allow_functions": true, "follow_up_prompt": "Explain" }));
        assert_eq!(
            found,
            json!({ "trait": "generate_js", "config": { "tool_name": "compute",
                "tool_description": "Compute", "code_description": "Tables: t",
                "allow_fetch": true, "table_access": "read", "allow_libraries": true,
                "follow_up_prompt": "Explain" } })
        );

        let (found, _, _) = only_trait(json!({
            "skill_type": "Ask user question", "question_sys_prompt": "Ask when unsure",
            "question_answer_prompt": "{{ answer }}", "question_discuss_label": "" }));
        assert_eq!(
            found,
            json!({ "trait": "ask_user", "config": { "prompt": "Ask when unsure",
                                                     "answer_prompt": "{{ answer }}" } })
        );
        let (found, _, _) = only_trait(json!({
            "skill_type": "Plan approval", "ini_sys_prompt": "Plan first",
            "approval_prompt": "Go" }));
        assert_eq!(
            found,
            json!({ "trait": "plan_approval", "config": { "prompt": "Plan first",
                                                          "approval_prompt": "Go" } })
        );
    }

    #[test]
    fn media_skills_name_the_provider_and_a_model_of_their_kind() {
        let (found, out, notes) = only_trait(json!({
            "skill_type": "Image generation", "size": "1024x1024", "quality": "auto",
            "format": "jpeg", "transparent": true }));
        assert_eq!(
            found,
            json!({ "trait": "generate_image", "config": { "store": "MyApp",
                "root": "agent-images", "provider": "openai", "model": "gpt-image-1",
                "size": "1024x1024", "quality": "auto", "format": "jpeg",
                "transparent": false } })
        );
        assert!(out.needs_store);
        assert!(notes.is_empty(), "{notes:?}");
        let (found, out, _) = only_trait(json!({
            "skill_type": "Text to speech", "voice": "nova", "speed": "1.5", "format": "mp3" }));
        assert_eq!(
            found,
            json!({ "trait": "speech", "config": { "provider": "openai", "model": "tts-1",
                "voice": "nova", "format": "mp3", "speed": 1.5 } })
        );
        let speech = out.providers[0]["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == json!("speech"))
            .cloned();
        assert_eq!(speech.unwrap()["is_default"], json!(true));

        // Anthropic has no image models: reported, not converted.
        let (out, notes) = convert_with(
            json!({ "backend": "AI SDK", "ai_sdk_provider": "Anthropic", "model": "claude",
                    "anthropic_api_key": "k" }),
            json!([]),
            json!([agent(
                "A",
                "",
                json!([ { "skill_type": "Image generation" } ])
            )]),
        );
        assert_eq!(out.agents[0]["traits"], json!([]));
        assert!(
            notes.iter().any(|n| n.contains("no image models")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_external_skill_reads_the_imported_store_and_says_what_to_copy() {
        let (found, out, notes) = only_trait(json!({
            "skill_type": "External Skill (SKILL.md)", "skills_dir": "/srv/skills/pdf/" }));
        assert_eq!(
            found,
            json!({ "trait": "skill_file", "config": { "store": "MyApp",
                "root": "skills/pdf", "file": "SKILL.md", "read_tool": true } })
        );
        assert!(out.needs_store);
        assert!(
            notes
                .iter()
                .any(|n| n.contains("copy the contents of `/srv/skills/pdf/`")),
            "{notes:?}"
        );
    }

    #[test]
    fn the_model_picker_offers_the_alternatives_on_the_imported_provider() {
        let (out, notes) = convert_with(
            openai(),
            json!([]),
            json!([agent("A", "", json!([ { "skill_type": "Model picker" } ]))]),
        );
        assert_eq!(out.agents[0]["traits"], json!([]));
        assert_eq!(
            out.agents[0]["attributes"],
            json!({ "pickable_models": [
                { "provider": "openai", "model": "gpt-x-mini", "label": "Fast" } ] })
        );
        assert!(
            out.providers[0]["models"]
                .as_array()
                .unwrap()
                .contains(&json!({ "name": "gpt-x-mini", "kind": "chat",
                                   "is_default": false, "config": {} })),
            "{:?}",
            out.providers
        );
        assert!(notes.iter().any(|n| n.contains("`Elsewhere`")), "{notes:?}");
    }

    #[test]
    fn the_prompt_picker_keeps_its_prompts_or_its_table() {
        let (found, _, notes) = only_trait(json!({
            "skill_type": "Prompt picker", "placeholder": "Style", "source": "Predefined",
            "options_obj": "{\"Pirate\": \"Speak like a pirate\"}" }));
        assert_eq!(
            found,
            json!({ "trait": "prompt_picker", "config": { "send": false, "label": "Style",
                "source": "predefined",
                "prompts": [ { "label": "Pirate", "prompt": "Speak like a pirate" } ] } })
        );
        assert!(notes.is_empty(), "{notes:?}");
        let (found, _, notes) = only_trait(json!({
            "skill_type": "Prompt picker", "source": "Table", "run_on_select": true,
            "table": "prompts", "label_field": "title", "prompt_field": "body",
            "query": "{active: true}" }));
        assert_eq!(
            found,
            json!({ "trait": "prompt_picker", "config": { "send": true, "source": "table",
                "table": "prompts", "label_field": "title", "prompt_field": "body",
                "where": { "active": true } } })
        );
        assert!(notes.is_empty(), "{notes:?}");
    }
}
