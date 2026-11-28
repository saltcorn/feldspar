//! The agent traits the JavaScript modules supply (TODO-app-constructor
//! AC12.1), installed into the agent registry after every module change.
//!
//! The two crates do not know each other: `sc-module` runs a module's
//! `agent_traits` and reports their declarations, and `sc-agent` runs an
//! [`AgentTrait`]. This is the bridge — a trait whose tools are the module's
//! declaration and whose calls go to the module's worker — as
//! `check_steps` is for checklist step types.
//!
//! A tool call runs as the run's caller: a `Table` read or write in it is the
//! caller's, under their role and ownership rules ([`TableHost::viewer_only`]),
//! as a route's is. A module's tools are no way around what the person chatting
//! may do.

use std::sync::Arc;

use sc_agent::{AgentRegistry, AgentTrait, RunCaller, RunMode, SessionContext, ToolsContext, TraitContext};
use sc_api::code_host::{TableHost, TriggerRunHost, schema_snapshot};
use sc_error::{Error, Result};
use sc_expr::{CodeHosts, TriggerHost};
use sc_llm::ToolSpec;
use sc_module::{AgentTraitManifest, CallHosts, ModuleHost, ModuleSet};
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

/// Install the agent traits the modules in `set` supply into `registry`, in
/// place of the last set. A trait whose name is taken is reported, one line
/// each, and the rest are installed.
pub(crate) fn install_module_traits(
    host: &Arc<ModuleHost>,
    set: &ModuleSet,
    registry: &AgentRegistry,
) {
    let mut traits: Vec<(String, Arc<dyn AgentTrait>)> = Vec::new();
    for loaded in set.modules() {
        let Some(manifest) = &loaded.manifest else {
            continue;
        };
        for declared in &manifest.agent_traits {
            traits.push((
                loaded.module.name.clone(),
                Arc::new(ModuleAgentTrait::new(
                    Arc::clone(host),
                    &loaded.module.name,
                    declared.clone(),
                )),
            ));
        }
    }
    for issue in registry.install_module_traits(traits) {
        eprintln!("feldspar: {issue}");
    }
}

/// One agent trait of one module.
pub(crate) struct ModuleAgentTrait {
    host: Arc<ModuleHost>,
    module: String,
    manifest: AgentTraitManifest,
    description: String,
    config_spec: Vec<FormField>,
}

impl ModuleAgentTrait {
    pub(crate) fn new(
        host: Arc<ModuleHost>,
        module: &str,
        manifest: AgentTraitManifest,
    ) -> ModuleAgentTrait {
        // As a checklist step's: the translation's issues are the module's,
        // already on its card from the load.
        let (config_spec, _) = sc_module::config_fields_to_form_fields(
            &manifest.config_fields,
            &format!("the agent trait `{}`", manifest.name),
        );
        let description = if manifest.description.trim().is_empty() {
            format!("Supplied by the module {module}")
        } else {
            manifest.description.clone()
        };
        ModuleAgentTrait {
            host,
            module: module.to_owned(),
            manifest,
            description,
            config_spec,
        }
    }

    /// One call into the module, as the run's caller; the answer's `state`
    /// becomes the trait's state in the run.
    #[allow(clippy::too_many_arguments)]
    async fn call_module(
        &self,
        catalog: &sc_catalog::Catalog,
        caller: &sc_agent::RunCaller,
        triggers: Option<&Arc<sc_action::TriggerDispatcher>>,
        evaluator: Option<&Arc<dyn sc_expr::JsEvaluator>>,
        trait_state: &mut Json,
        request: Json,
        context: Json,
    ) -> Result<Json> {
        let principal = caller.principal();
        let table = TableHost::new(catalog)
            .for_caller(&principal)
            .viewer_only()
            .with_evaluator(evaluator.cloned());
        let trigger_host = triggers.map(|d| {
            TriggerRunHost::new(d, catalog)
                .caused_by(principal.role(), principal.user().cloned())
        });
        let schema = schema_snapshot(catalog).ok();
        let call = CallHosts {
            hosts: CodeHosts {
                host: Some(&table),
                triggers: trigger_host.as_ref().map(|t| t as &dyn TriggerHost),
                ..CodeHosts::default()
            },
            schema: schema.as_deref(),
            ..CallHosts::default()
        };
        let mut message = request;
        if let Json::Object(map) = &mut message {
            let mut context = context;
            if let Json::Object(cx) = &mut context {
                cx.insert("state".to_owned(), trait_state.clone());
                cx.insert(
                    "user".to_owned(),
                    principal.user().cloned().unwrap_or(Json::Null),
                );
            }
            map.insert("context".to_owned(), context);
        }
        let answer = self
            .host
            .call_agent_trait(&self.module, &self.manifest.name, message, call)
            .await?;
        let Json::Object(mut answer) = answer else {
            return Err(Error::config(format!(
                "the module {} answered its agent trait `{}` with something other than an object",
                self.module, self.manifest.name
            )));
        };
        *trait_state = answer.remove("state").unwrap_or(Json::Null);
        Ok(answer.remove("result").unwrap_or(Json::Null))
    }
}

fn offered(mode: RunMode, mutates: bool) -> bool {
    !mutates || mode == RunMode::Act
}

/// Whether `caller` may have a tool marked `admin_only` (TODO-app-constructor
/// AC12.7): an administrator at the console, which a run a trigger or an
/// application's page started never is. With no caller (the trait picker,
/// validation) the tool is listed.
fn admitted(caller: Option<&RunCaller>, admin_only: bool) -> bool {
    !admin_only || caller.is_none_or(|c| c.console_admin().is_some())
}

#[async_trait::async_trait]
impl AgentTrait for ModuleAgentTrait {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn config_spec(&self) -> Vec<FormField> {
        self.config_spec.clone()
    }

    fn tools(&self, cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        self.manifest
            .tools
            .iter()
            .filter(|t| offered(cx.mode, t.mutates) && admitted(cx.caller, t.admin_only))
            .map(|t| {
                let parameters = if t.parameters.is_object() {
                    t.parameters.clone()
                } else {
                    json!({ "type": "object", "properties": {} })
                };
                ToolSpec::new(&t.name, &t.description, parameters)
            })
            .collect()
    }

    fn prompt(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Option<String> {
        self.manifest.prompt.clone()
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let Some(declared) = self.manifest.tools.iter().find(|t| t.name == tool) else {
            return Err(Error::invalid(format!(
                "the agent trait `{}` has no tool `{tool}`",
                self.manifest.name
            )));
        };
        if !admitted(Some(ctx.caller), declared.admin_only) {
            return Err(Error::auth(format!(
                "`{tool}` is for an administrator at the console, and this run is not one's"
            )));
        }
        if !offered(ctx.mode, declared.mutates) {
            return Err(Error::invalid(format!(
                "`{tool}` changes things, and this run is in {} mode, which only reads",
                ctx.mode.as_str()
            )));
        }
        let context = json!({
            "config": config,
            "run": { "id": ctx.run.to_string(), "agent": ctx.agent, "mode": ctx.mode.as_str() },
        });
        let request = json!({ "call": "tool", "tool": tool, "args": args });
        self.call_module(
            ctx.catalog,
            ctx.caller,
            ctx.triggers,
            ctx.evaluator,
            ctx.trait_state,
            request,
            context,
        )
        .await
    }

    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        if !self.manifest.session_header {
            return Ok(None);
        }
        let context = json!({
            "config": config,
            "run": { "id": cx.run.to_string(), "agent": cx.agent, "mode": cx.mode.as_str() },
            "brief": cx.brief,
        });
        let request = json!({ "call": "session_header" });
        let header = self
            .call_module(
                cx.catalog,
                cx.caller,
                cx.triggers,
                cx.evaluator,
                cx.trait_state,
                request,
                context,
            )
            .await?;
        Ok(header.as_str().filter(|h| !h.is_empty()).map(str::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_module::AgentToolManifest;

    fn manifest() -> AgentTraitManifest {
        AgentTraitManifest {
            name: "shop_notes".into(),
            description: String::new(),
            config_fields: vec![json!({ "name": "tag", "label": "Tag", "type": "String" })],
            prompt: Some("Write notes.".into()),
            tools: vec![
                AgentToolManifest {
                    name: "read_notes".into(),
                    description: "Read the notes".into(),
                    parameters: Json::Null,
                    mutates: false,
                    admin_only: false,
                },
                AgentToolManifest {
                    name: "save_note".into(),
                    description: "Save a note".into(),
                    parameters: json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
                    mutates: true,
                    admin_only: false,
                },
            ],
            session_header: false,
        }
    }

    #[test]
    fn a_module_trait_reads_as_its_declaration() {
        let host = Arc::new(ModuleHost::new("/tmp/does-not-need-to-exist"));
        let t = ModuleAgentTrait::new(host, "@acme/shop", manifest());
        assert_eq!(t.name(), "shop_notes");
        assert!(t.description().contains("@acme/shop"));
        assert_eq!(t.config_spec().len(), 1);
    }

    #[test]
    fn a_mutating_tool_is_offered_only_to_a_run_that_acts() {
        assert!(offered(RunMode::Act, true));
        assert!(!offered(RunMode::Plan, true));
        assert!(!offered(RunMode::Explore, true));
        assert!(offered(RunMode::Explore, false));
    }

    #[test]
    fn module_traits_join_the_registry_and_never_shadow_a_builtin() {
        let host = Arc::new(ModuleHost::new("/tmp/does-not-need-to-exist"));
        let registry = sc_agent_builtins::builtin_traits().expect("the built-in traits");
        let builtins = registry.len();
        let mut clash = manifest();
        clash.name = "coding".into();
        let issues = registry.install_module_traits(vec![
            (
                "@acme/shop".into(),
                Arc::new(ModuleAgentTrait::new(Arc::clone(&host), "@acme/shop", manifest())),
            ),
            (
                "@acme/other".into(),
                Arc::new(ModuleAgentTrait::new(Arc::clone(&host), "@acme/other", manifest())),
            ),
            (
                "@acme/coder".into(),
                Arc::new(ModuleAgentTrait::new(Arc::clone(&host), "@acme/coder", clash)),
            ),
        ]);
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues[0].contains("@acme/other") && issues[0].contains("@acme/shop"));
        assert!(issues[1].contains("built-in"));
        // A clone made before sees the module's trait: the set is shared.
        let clone = registry.clone();
        assert_eq!(clone.len(), builtins + 1);
        assert!(clone.require("shop_notes").is_ok());
        assert!(clone.names().iter().any(|n| n == "shop_notes"));
        // The coding trait is still the built-in one.
        assert!(!clone.require("coding").expect("coding").description().contains("Supplied by"));
        // Installing an empty set takes it away again.
        assert!(registry.install_module_traits(Vec::new()).is_empty());
        assert!(clone.require("shop_notes").is_err());
    }
}
