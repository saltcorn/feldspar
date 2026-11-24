//! A module's agent skills as **agent traits** (`sc_module::agent_skills`).
//!
//! `sc-module` is below `sc-agent`, so it says what a skill is and how to call
//! one, and this is where a skill becomes an [`AgentTrait`] — the one place
//! that sees both. Every module change rebuilds the set and installs it into
//! the agent services ([`AgentServices::install_module_traits`]), beside the
//! built-in traits: an agent given `run_bash` validates once the bash module is
//! installed, and says "unknown agent trait" again once it is removed.
//!
//! # Who may use an unsafe module's tools
//!
//! A skill of a module that declares itself unsafe can run programs on the
//! server — the bundled bash module's `run_bash` runs whatever the model
//! writes, as the server's operating-system user, or over SSH from it. That is
//! the coding trait's shell by another name, so it is offered on the shell's
//! terms: only to a run whose caller is an administrator **at the admin
//! console** ([`RunCaller::console_admin`]), never to a chat through an
//! application's exposed agent or a run a trigger computed the prompt of, and
//! only in `act` mode. Elsewhere the trait contributes no tools and no prompt,
//! and a call is refused saying why.
//!
//! [`AgentServices::install_module_traits`]: crate::agents::AgentServices::install_module_traits

use std::sync::Arc;

use sc_agent::{AgentTrait, RunCaller, RunMode, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::ConsoleAdmin;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_module::{ModuleAgentSkill, SkillCaller, SkillDescription};
use sc_types::{Attrs, FormField};
use serde_json::Value as Json;

/// One module's agent skill, as an agent trait.
pub struct ModuleAgentTrait {
    skill: Arc<ModuleAgentSkill>,
}

impl ModuleAgentTrait {
    /// The trait `skill` is.
    pub fn new(skill: Arc<ModuleAgentSkill>) -> ModuleAgentTrait {
        ModuleAgentTrait { skill }
    }

    /// Whether this run may be offered the skill's tools: always, unless its
    /// module is unsafe — then only an administrator's run at the console, in
    /// `act` mode. `None` is no run in particular (validation, the admin UI),
    /// which sees the tools as configured.
    fn offered(&self, caller: Option<&RunCaller>, mode: RunMode) -> bool {
        if !self.skill.is_unsafe() {
            return true;
        }
        match caller {
            None => true,
            Some(caller) => caller.console_admin().is_some() && mode == RunMode::Act,
        }
    }

    fn described(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Option<SkillDescription> {
        self.offered(cx.caller, cx.mode)
            .then(|| self.skill.described(config))
    }
}

/// Every agent skill the loaded modules supply, as traits.
pub fn module_agent_traits(skills: Vec<Arc<ModuleAgentSkill>>) -> Vec<Arc<dyn AgentTrait>> {
    skills
        .into_iter()
        .map(|skill| Arc::new(ModuleAgentTrait::new(skill)) as Arc<dyn AgentTrait>)
        .collect()
}

#[async_trait::async_trait]
impl AgentTrait for ModuleAgentTrait {
    fn name(&self) -> &str {
        self.skill.name()
    }

    fn description(&self) -> &str {
        self.skill.description()
    }

    fn config_spec(&self) -> Vec<FormField> {
        self.skill.config_spec()
    }

    /// Ask the module what this configuration offers — which is also what
    /// [`tools`](AgentTrait::tools) and [`prompt`](AgentTrait::prompt) answer
    /// from afterwards. A skill that cannot describe itself for this
    /// configuration takes the agent out of the live set with the module's
    /// reason.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        self.skill
            .describe(check.config)
            .await
            .map(drop)
            .map_err(|e| {
                Error::config(format!(
                    "the agent skill `{}` of the module {} could not say what it offers: {}",
                    self.skill.name(),
                    self.skill.module(),
                    sc_error::format_chain(&e)
                ))
            })
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        self.described(cx, config)
            .map(|described| {
                described
                    .tools
                    .into_iter()
                    .map(|tool| ToolSpec::new(tool.name, tool.description, tool.parameters))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn prompt(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Option<String> {
        self.described(cx, config)?.prompt
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        if !self.offered(Some(ctx.caller), ctx.mode) {
            return Err(Error::invalid(format!(
                "agent `{}` may not use `{tool}` in this run: it comes from the module {}, which \
                 declares itself unsafe (it can run programs on the server), so it is only for \
                 an administrator's run at the admin console, in act mode",
                ctx.agent,
                self.skill.module()
            )));
        }
        let caller = SkillCaller {
            user: ctx.caller.user_ref().map(sc_auth::User::fields_json),
            admin: ctx.caller.console_admin().map(ConsoleAdmin::spawn_proof),
        };
        self.skill.call(config, tool, args, &caller).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_module::{AgentSkillManifest, ModuleHost};
    use serde_json::json;

    fn module_trait(unsafe_module: bool) -> ModuleAgentTrait {
        let manifest = AgentSkillManifest {
            name: "run_bash".into(),
            label: "Bash".into(),
            description: "Run Bash command".into(),
            config_fields: Vec::new(),
            tools: vec![json!({ "name": "run_bash", "description": "Run a bash command" })],
            prompt: Some("use run_bash".into()),
        };
        ModuleAgentTrait::new(Arc::new(ModuleAgentSkill::new(
            "@feldspar/bash",
            manifest,
            unsafe_module,
            Arc::new(ModuleHost::new("/tmp/does-not-need-to-exist")),
        )))
    }

    fn user(role: sc_types::Role) -> sc_auth::User {
        sc_auth::User::new(uuid::Uuid::new_v4(), role)
    }

    #[test]
    fn an_unsafe_modules_tools_are_only_an_administrators_at_the_console_in_act_mode() {
        let bash = module_trait(true);
        assert_eq!(bash.name(), "run_bash");
        // Validation and the admin UI list it as configured.
        assert!(bash.offered(None, RunMode::Plan));
        // A system run an administrator set going carries the console's proof.
        assert!(bash.offered(Some(&RunCaller::system()), RunMode::Act));
        assert!(!bash.offered(Some(&RunCaller::system()), RunMode::Plan));
        // Nobody vouched for these being at the console.
        for caller in [
            RunCaller::user(user(sc_types::Role::ADMIN)),
            RunCaller::exposed(user(sc_types::Role::ADMIN)),
            RunCaller::triggered(),
            RunCaller::anonymous(),
        ] {
            assert!(!bash.offered(Some(&caller), RunMode::Act), "{caller:?}");
        }
    }

    #[test]
    fn a_safe_modules_tools_are_everyones() {
        let skill = module_trait(false);
        assert!(skill.offered(Some(&RunCaller::anonymous()), RunMode::Explore));
        assert!(skill.description().contains("@feldspar/bash"));
    }
}
