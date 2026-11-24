//! A module's **agent skills**, as the agent traits an agent may be given.
//!
//! Saltcorn v1's agents plugin collects skills from every plugin's
//! `exchange.agent_skills`: a class with a `skill_name`, static
//! `configFields`, and — on an instance constructed with the skill's
//! configuration — `systemPrompt()` and `provideTools()`, each tool a
//! function declaration and a `process`. A module here exports the same
//! (or `agent_skills` at the top level; [`AgentSkillManifest`]), and each one
//! becomes an agent trait beside the built-in ones.
//!
//! The trait itself (`sc_agent::AgentTrait`) is two layers up, so this is the
//! half that knows modules: [`ModuleAgentSkill`] says what a skill is called,
//! what it is configured with, what it offers a configuration and how to call
//! one of its tools; the server wraps each in the trait.
//!
//! # What a configured skill offers is asked once
//!
//! v1 builds the tools and the prompt from a configured instance, and an
//! agent trait's `tools` and `prompt` are synchronous. So a configuration is
//! **described** by the worker when an agent is checked — on save, and on every
//! load ([`ModuleAgentSkill::describe`]) — and the answer is kept here;
//! [`described`](ModuleAgentSkill::described) answers from that, or from what
//! the skill offered with no settings when it was loaded.
//!
//! # The shell
//!
//! A skill of an unsafe module ([`crate::unsafe_modules`]) can run scripts —
//! the bundled bash module's `run_bash` does — so its tools are an
//! administrator's: the call carries the `shell` ask only when it is given an
//! administrator's proof, which the server gives only for a run at the admin
//! console. A skill of any other module never carries it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sc_error::{Error, Result};
use sc_proc::AdminProof;
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

use crate::host::{AgentSkillManifest, AskSurface, CallHosts, ModuleHost};
use crate::modules::ModuleSet;
use crate::spec::config_fields_to_form_fields;
use crate::unsafe_modules::ModuleShell;

/// How many configurations' descriptions one skill keeps. An agent's
/// configuration changes only when an admin saves it, so this is many more than
/// a server has; past it the cache starts again.
const MAX_DESCRIBED: usize = 256;

/// One tool a configured skill offers.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillTool {
    /// The name the model calls.
    pub name: String,
    /// What it does, for the model.
    pub description: String,
    /// JSON Schema for its arguments.
    pub parameters: Json,
}

/// What a configured skill offers an agent.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkillDescription {
    /// Its tools.
    pub tools: Vec<SkillTool>,
    /// Its part of the system prompt.
    pub prompt: Option<String>,
}

impl SkillDescription {
    /// Read the worker's `{ tools, prompt }`; a tool with no name is left out.
    pub fn from_json(value: &Json) -> SkillDescription {
        let tools = value
            .get("tools")
            .and_then(Json::as_array)
            .map(|tools| tools.iter().filter_map(tool_of).collect())
            .unwrap_or_default();
        let prompt = value
            .get("prompt")
            .and_then(Json::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(str::to_owned);
        SkillDescription { tools, prompt }
    }
}

fn tool_of(value: &Json) -> Option<SkillTool> {
    let name = value.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    Some(SkillTool {
        name: name.to_owned(),
        description: value
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        parameters: match value.get("parameters") {
            Some(p @ Json::Object(_)) => p.clone(),
            _ => json!({ "type": "object", "properties": {} }),
        },
    })
}

/// Who calls a skill's tool.
#[derive(Debug, Clone, Default)]
pub struct SkillCaller {
    /// The user the run is for, as a row's fields (`User::fields_json`), or
    /// `None` for a run with nobody.
    pub user: Option<Json>,
    /// The administrator at the console the run is for, when it is — what lets
    /// an unsafe module's skill run scripts.
    pub admin: Option<AdminProof>,
}

/// One agent skill supplied by one module.
pub struct ModuleAgentSkill {
    module: String,
    manifest: AgentSkillManifest,
    config_spec: Vec<FormField>,
    description: String,
    /// Whether its module declared itself unsafe (and so was loaded under the
    /// switch).
    unsafe_module: bool,
    /// Whether the operator's switch is on, asked on every call:
    /// [`unsafe_modules_allowed`](crate::unsafe_modules::unsafe_modules_allowed),
    /// or a test's answer.
    switch: fn() -> bool,
    host: Arc<ModuleHost>,
    /// What each configuration offers, by the configuration's JSON.
    described: Mutex<HashMap<String, SkillDescription>>,
}

impl std::fmt::Debug for ModuleAgentSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleAgentSkill")
            .field("module", &self.module)
            .field("name", &self.manifest.name)
            .field("unsafe_module", &self.unsafe_module)
            .finish_non_exhaustive()
    }
}

impl ModuleAgentSkill {
    /// The skill `manifest` of `module`, over `host`. A settings field that
    /// does not translate was an issue on the module's card when it loaded,
    /// and is left out here.
    pub fn new(
        module: impl Into<String>,
        manifest: AgentSkillManifest,
        unsafe_module: bool,
        host: Arc<ModuleHost>,
    ) -> ModuleAgentSkill {
        let module = module.into();
        let (config_spec, _) = config_fields_to_form_fields(
            &manifest.config_fields,
            &format!("the agent skill `{}`", manifest.name),
        );
        let label = if manifest.label.trim().is_empty() {
            manifest.name.clone()
        } else {
            manifest.label.trim().to_owned()
        };
        let description =
            if manifest.description.trim().is_empty() || manifest.description.trim() == label {
                format!("{label}, from the module {module}")
            } else {
                format!("{}, from the module {module}", manifest.description.trim())
            };
        ModuleAgentSkill {
            module,
            manifest,
            config_spec,
            description,
            unsafe_module,
            switch: crate::unsafe_modules::unsafe_modules_allowed,
            host,
            described: Mutex::new(HashMap::new()),
        }
    }

    /// The same skill, with the operator's switch answered by `allowed`
    /// instead of the environment — for a test, which cannot set a process's
    /// environment safely while other tests run.
    #[doc(hidden)]
    #[must_use]
    pub fn with_switch(mut self, allowed: fn() -> bool) -> ModuleAgentSkill {
        self.switch = allowed;
        self
    }

    /// The name an agent stores it under.
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// One line for the trait picker, naming the module.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Its settings.
    pub fn config_spec(&self) -> Vec<FormField> {
        self.config_spec.clone()
    }

    /// The package it came from.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// Whether its module declared itself unsafe: its tools are then only an
    /// administrator's at the console, because they can run scripts.
    pub fn is_unsafe(&self) -> bool {
        self.unsafe_module
    }

    /// What `config` offers, as last described: the worker's answer for this
    /// configuration if [`describe`](Self::describe) has asked, or else what
    /// the skill offered with no settings.
    pub fn described(&self, config: &Attrs) -> SkillDescription {
        let key = Json::Object(config.clone()).to_string();
        if let Ok(cache) = self.described.lock()
            && let Some(found) = cache.get(&key)
        {
            return found.clone();
        }
        SkillDescription {
            tools: self.manifest.tools.iter().filter_map(tool_of).collect(),
            prompt: self.manifest.prompt.clone(),
        }
    }

    /// Ask the worker what `config` offers, and keep the answer for
    /// [`described`](Self::described). An error is the module's own, and is
    /// what an agent with this configuration is refused with.
    pub async fn describe(&self, config: &Attrs) -> Result<SkillDescription> {
        let configuration = Json::Object(config.clone());
        let answer = self
            .host
            .agent_skill_describe(&self.module, &self.manifest.name, &configuration)
            .await?;
        let described = SkillDescription::from_json(&answer);
        if let Ok(mut cache) = self.described.lock() {
            if cache.len() >= MAX_DESCRIBED {
                cache.clear();
            }
            cache.insert(configuration.to_string(), described.clone());
        }
        Ok(described)
    }

    /// Call `tool` of this skill configured with `config`, with the model's
    /// `args`, for `caller`. Answers what the tool's `process` returned.
    pub async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        caller: &SkillCaller,
    ) -> Result<Json> {
        let shell = match (&caller.admin, self.unsafe_module) {
            (Some(proof), true) => Some(
                ModuleShell::with_switch(&self.module, self.switch).on_behalf_of(proof.clone()),
            ),
            (None, true) => {
                return Err(Error::invalid(format!(
                    "the tool `{tool}` comes from the module {}, which declares itself unsafe \
                     (it can run programs on the server), so only an administrator's run at \
                     the admin console may use it",
                    self.module
                )));
            }
            (_, false) => None,
        };
        let mut call = CallHosts::default();
        if let Some(shell) = &shell {
            call = call.with_ask(AskSurface::Shell, shell);
        }
        self.host
            .agent_skill_call(
                &self.module,
                &self.manifest.name,
                json!({
                    "configuration": Json::Object(config.clone()),
                    "tool": tool,
                    "args": args,
                    "user": caller.user.clone().unwrap_or(Json::Null),
                }),
                call,
            )
            .await
    }
}

/// Every agent skill of every loaded module in `set`, over `host`, in the
/// set's order.
pub fn module_agent_skills(host: &Arc<ModuleHost>, set: &ModuleSet) -> Vec<Arc<ModuleAgentSkill>> {
    let mut skills = Vec::new();
    for loaded in set.modules() {
        let Some(manifest) = &loaded.manifest else {
            continue;
        };
        for skill in &manifest.agent_skills {
            skills.push(Arc::new(ModuleAgentSkill::new(
                loaded.module.name.clone(),
                skill.clone(),
                manifest.unsafe_reason.is_some(),
                Arc::clone(host),
            )));
        }
    }
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> AgentSkillManifest {
        AgentSkillManifest {
            name: "run_bash".into(),
            label: "Bash".into(),
            description: "Run Bash command".into(),
            config_fields: vec![
                json!({ "name": "timeout", "label": "Timeout (s)", "type": "Integer" }),
            ],
            tools: vec![
                json!({ "name": "run_bash", "description": "Run a bash command" }),
                json!({ "description": "no name, so not a tool" }),
            ],
            prompt: Some("use run_bash".into()),
        }
    }

    fn skill(unsafe_module: bool) -> ModuleAgentSkill {
        ModuleAgentSkill::new(
            "@feldspar/bash",
            manifest(),
            unsafe_module,
            Arc::new(ModuleHost::new("/tmp/does-not-need-to-exist")),
        )
    }

    #[test]
    fn a_skill_is_named_described_and_configured_from_its_manifest() {
        let skill = skill(true);
        assert_eq!(skill.name(), "run_bash");
        assert_eq!(
            skill.description(),
            "Run Bash command, from the module @feldspar/bash"
        );
        assert_eq!(skill.config_spec().len(), 1);
        assert!(skill.is_unsafe());
        // Undescribed, a configuration is offered what no settings were.
        let described = skill.described(&Attrs::new());
        assert_eq!(described.tools.len(), 1);
        assert_eq!(described.tools[0].name, "run_bash");
        assert_eq!(described.tools[0].parameters["type"], json!("object"));
        assert_eq!(described.prompt.as_deref(), Some("use run_bash"));
    }

    #[test]
    fn a_description_reads_the_workers_answer() {
        let described = SkillDescription::from_json(&json!({
            "tools": [{ "name": "a", "description": "A", "parameters": { "type": "object",
                        "properties": { "x": { "type": "string" } } } }],
            "prompt": "  ",
        }));
        assert_eq!(
            described.tools[0].parameters["properties"]["x"]["type"],
            json!("string")
        );
        assert_eq!(described.prompt, None);
    }

    #[tokio::test]
    async fn an_unsafe_skill_is_refused_to_a_run_without_an_administrator() {
        let err = skill(true)
            .call(
                &Attrs::new(),
                "run_bash",
                &json!({ "command": "true" }),
                &SkillCaller::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("admin console"), "{err}");
    }
}
