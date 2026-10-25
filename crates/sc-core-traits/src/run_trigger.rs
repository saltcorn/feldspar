//! `run_trigger` — one configured trigger as one tool (§11.3).
//!
//! This is the trait that connects an agent to the whole of §10: anything a
//! trigger can do — insert rows, call a web service, run JavaScript, and a
//! workflow once §10.3 lands, because a workflow is a trigger — becomes
//! something an agent can be given, one trigger at a time and by name.
//!
//! Three properties, each of which is the reason the trait is not simply "run
//! any action":
//!
//! - **It runs the dispatcher's trigger, not a copy of it.** The call goes
//!   through the *same* [`TriggerDispatcher`](sc_action::TriggerDispatcher) every
//!   other event fires on, so the `only_if` runs, the cascade depth is counted,
//!   a disabled trigger stays disabled and a trigger that failed validation says
//!   why. An agent is one more thing that can ask; it is not a second way to
//!   fire.
//! - **The trigger's own `min_role` still gates it.** Exposing an agent to a
//!   role does not thereby expose everything the agent can reach: a user at role
//!   80 chatting with an agent that has `run_trigger` over an admin-only trigger
//!   is refused, in the tool result, naming the trigger. A trigger-started run
//!   carries the trigger's authority and clears any floor, which is decision 5
//!   read the other way round.
//! - **The payload is the event's payload.** Whatever the model sends is what
//!   the trigger's action sees as the event — which is what `POST
//!   {mount}/actions/{name}` sends and what the admin's Run button sends, so a
//!   trigger behaves the same however it was asked.

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::table::config_str;

/// The trigger this trait exposes.
pub const CFG_TRIGGER: &str = "trigger";

/// Run one configured trigger.
pub struct RunTrigger;

/// The tool one `run_trigger` instance offers, derived from its trigger.
pub fn tool_name(trigger: &str) -> String {
    format!("run_{trigger}")
}

#[async_trait::async_trait]
impl AgentTrait for RunTrigger {
    fn name(&self) -> &str {
        "run_trigger"
    }

    fn description(&self) -> &str {
        "Run one trigger, with a payload the agent supplies"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TRIGGER, BasicType::Text)
                .label("Trigger")
                .required(),
        ]
    }

    /// The trigger must exist — checked against **storage**, on save and again on
    /// load, so an agent naming a deleted trigger leaves the live set with a
    /// reason instead of failing when the model calls it.
    ///
    /// Storage rather than the live trigger set, deliberately: a trigger that is
    /// stored but does not currently validate is a *repairable* state, and an
    /// agent that named it should not also be invalid — one broken thing should
    /// produce one error, in the place it can be fixed. Calling the tool then
    /// says what is wrong with the trigger, in the words the trigger's own
    /// validation used.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let name = configured_trigger(check.config)?;
        if sc_action::load_trigger_by_name(check.catalog, &name)
            .await?
            .is_none()
        {
            return Err(Error::invalid(format!("no trigger named `{name}`")));
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let configured = config_str(config, CFG_TRIGGER);
        // The catalog cannot answer "what does this trigger do?" — the trigger
        // set is `sc-action`'s and reading it is async, which this is not. So the
        // description is what the configuration knows, and it is enough: the
        // trigger's name is the admin's own word for it.
        vec![ToolSpec::new(
            tool_name(&configured),
            format!(
                "Run the `{configured}` action. The arguments are its payload: \
                 an object, whose keys are whatever that action reads. Returns \
                 what the action returned. This does something rather than \
                 reporting something — do not call it to find out what it would \
                 do."
            ),
            json!({
                "type": "object",
                "description": "The event payload the action receives.",
                // Open on purpose: the payload's shape is the action's business
                // and the trigger's configuration may read any of it. A closed
                // schema here would be this crate guessing at another crate's
                // contract, and guessing wrong is a call the vendor refuses.
                "additionalProperties": true,
            }),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let name = configured_trigger(config)?;
        let dispatcher = ctx.require_triggers()?;
        let trigger = dispatcher.triggers()?.require(&name)?.clone();

        // The trigger's own floor, checked before anything happens. `min_role`
        // absent means admin-only — the safe reading a trigger nobody has
        // thought about the access of gets everywhere else (§10.2), and the one
        // that must not be relaxed by being reached through an agent.
        if !ctx.caller.meets_role(trigger.min_role) {
            return Err(Error::auth(format!(
                "you may not run `{name}`; it needs role {} or better",
                trigger.min_role.unwrap_or(1)
            )));
        }

        let payload = match args {
            Json::Null => json!({}),
            other => other.clone(),
        };
        let caller = sc_api::caller_context_at(ctx.caller.role, ctx.caller.user.as_ref())
            .chained(ctx.caller.tool_chain());
        let result = dispatcher
            .run_trigger(ctx.catalog, &name, payload, Some(&caller))
            .await?;
        Ok(json!({ "trigger": name, "result": result }))
    }
}

/// The configured trigger's name.
fn configured_trigger(config: &Attrs) -> Result<String> {
    let name = config_str(config, CFG_TRIGGER);
    if name.is_empty() {
        return Err(Error::invalid(format!("`{CFG_TRIGGER}` is required")));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_trigger() {
        assert_eq!(tool_name("reindex"), "run_reindex");
    }

    #[test]
    fn a_trait_with_no_trigger_configured_says_so() {
        let err = configured_trigger(&Attrs::new()).unwrap_err();
        assert!(err.to_string().contains(CFG_TRIGGER), "{err}");
    }
}
