//! `run_agent` — an agent as a trigger body (§11.5).
//!
//! The other direction from [`RunTrigger`](crate::RunTrigger): that one hands an
//! agent a trigger to call, this one hands a trigger an agent to run. Together
//! they are what "an agent is a kind of `Action`" means in v2 — an agent is its
//! own record (decision 4), and *this* is the single registered action that runs
//! one, so a row insert can start an agent without the trigger model learning
//! anything about agents and without the agent stopping being editable as an
//! agent.
//!
//! Four properties, each deliberate:
//!
//! - **The prompt is a formula, in the event's scope** (§10.1's rule, decision
//!   7): `row.title + " needs a summary"` on an insert trigger is the whole
//!   feature. It is validated on save *and* on load like every other configured
//!   formula, so an identifier that does not resolve is a message on the form
//!   rather than a firing that fails.
//! - **It runs with the trigger's authority** ([`RunCaller::system`], decision
//!   5). Nobody is present — a row was written — so there is no user to run the
//!   agent's tools as, and the run row records `user: null` for exactly that
//!   reason. The agent's own `min_role` is therefore not a gate here: the floor
//!   describes who may *chat* with it, and a trigger clears every floor. What
//!   gates a triggered run is the trigger, which is the thing an admin already
//!   knows how to guard (§10.2, §13.2).
//! - **It does not stream.** An action returns a value (§10.1), and a caller who
//!   wants deltas is a chat client. What it returns is the final assistant
//!   message and the **run id**, so the run is `getRun`-able afterwards and the
//!   chat panel's history is where a triggered conversation is read.
//! - **The run is written after every step, as any run is**, so a triggered run
//!   that failed halfway is still a readable transcript with a reason on it
//!   rather than a line in a log.
//!
//! ## What a triggered run cannot do
//!
//! Run a trigger. An agent whose traits include
//! [`RunTrigger`](crate::RunTrigger) needs *the* dispatcher, and the dispatcher
//! is what is running this action — so handing it back down would close a loop
//! (trigger → agent → trigger → agent) with nothing counting the depth, since a
//! tool call is not a firing and carries no chain. The tool therefore answers
//! with `TraitContext::require_triggers`' configuration error, which the model
//! reads as a tool result and can report. Chat is where an agent runs triggers,
//! until a run carries a firing chain.

use std::sync::Arc;

use sc_action::{
    Action, ActionContext, ConfigCheck, EVENT_SCOPE, check_formula, event_formula_value,
    required_formula,
};
use sc_agent::{AgentRegistry, Conclusion, ProviderConnector, RunCaller, Runner, save_run};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

/// The agent this action runs, by name.
pub const CFG_AGENT: &str = "agent";
/// The formula computing what to say to it.
pub const CFG_PROMPT: &str = "prompt";

/// Run one configured agent with a prompt derived from the event.
///
/// Holds the two things an agent run needs that a trigger's configuration cannot
/// name: which traits exist (the same registry the admin API validated the agent
/// against — an agent saved against one trait set and run against another would
/// be accepted in one place and refused in the other) and how a provider is
/// connected. The second is the seam that lets every test here drive a whole run
/// against a script rather than a vendor (decision 7).
pub struct RunAgent {
    traits: Arc<AgentRegistry>,
    providers: Arc<dyn ProviderConnector>,
}

impl RunAgent {
    /// The action, over `traits`, connecting providers through `providers`.
    pub fn new(traits: Arc<AgentRegistry>, providers: Arc<dyn ProviderConnector>) -> RunAgent {
        RunAgent { traits, providers }
    }
}

#[async_trait::async_trait]
impl Action for RunAgent {
    fn name(&self) -> &str {
        "run_agent"
    }

    fn description(&self) -> &str {
        "Run an agent with a prompt built from the event, and return its answer"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_AGENT, BasicType::Text)
                .label("Agent")
                .required(),
            FormField::new(CFG_PROMPT, BasicType::Text)
                .label("Prompt")
                .required(),
        ]
    }

    /// The agent must exist and the prompt must resolve in the scope this
    /// trigger's event will give it — on save, in front of the admin, and again
    /// on load, so a trigger naming a deleted agent leaves the live set with a
    /// reason instead of failing when a row is inserted.
    ///
    /// The agent is resolved against **storage**, not against the live set, for
    /// [`RunTrigger`](crate::RunTrigger)'s reason read the other way round: an
    /// agent that is stored but does not currently validate is a repairable
    /// state, and the trigger that names it should not also be invalid — one
    /// broken thing, one error, in the place it can be fixed. Firing then says
    /// what is wrong with the *agent*, in the words the agent's own validation
    /// used.
    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let name = configured_agent(check.config)?;
        if sc_agent::load_agent_by_name(check.catalog, &name)
            .await?
            .is_none()
        {
            return Err(Error::invalid(format!("no agent named `{name}`")));
        }
        let formula = required_formula(check.config, CFG_PROMPT)?;
        check_formula(
            check.shape,
            EVENT_SCOPE,
            &formula,
            &format!("`{CFG_PROMPT}`"),
        )?;
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let name = configured_agent(ctx.config).map_err(named)?;
        let formula = required_formula(ctx.config, CFG_PROMPT).map_err(named)?;
        let prompt = prompt_text(
            event_formula_value(ctx, &formula, &format!("`{CFG_PROMPT}`")).await?,
            ctx.trigger,
        )?;

        // The live set, so an agent whose provider vanished or whose trait was
        // configured against a dropped table says *that* rather than failing
        // somewhere inside the loop.
        let agents = sc_agent::Agents::load(ctx.catalog, &self.traits)
            .await
            .map_err(&named)?;
        let agent = agents.require(&name).map_err(&named)?;
        let executor = self
            .providers
            .connect(ctx.catalog, agent, sc_agent::ModelRole::Executor)
            .await
            .map_err(&named)?;

        let mut runner = Runner::new(
            ctx.catalog,
            &self.traits,
            agent,
            executor,
            // Decision 5, at the one place a run is created: nobody is present,
            // so the run carries the trigger's authority and records no user.
            RunCaller::system(),
        )
        // The connector this action already holds, handed on so an agent a
        // trigger started can delegate exactly as one a person is chatting with
        // does. Unlike the dispatcher — which is *running* this action and
        // cannot be handed back down without closing an uncounted cycle — a
        // sub-agent run counts its own depth, so there is nothing here to
        // withhold.
        .with_connector(&self.providers);
        // A tool that reads a table whose ownership formula does not translate
        // needs the engine; the dispatcher running this action has one wherever
        // the deployment does.
        if let Ok(evaluator) = ctx.evaluator() {
            runner = runner.with_evaluator(evaluator);
        }

        // Built here rather than through `Runner::start` for the one thing that
        // call cannot give it: a description. A run list carries no transcript,
        // so without this a triggered conversation would be a timestamp in the
        // chat panel's history.
        let mut run = runner
            .new_run(&prompt)
            .map_err(&named)?
            .description(format!("trigger `{}`", ctx.trigger));
        save_run(ctx.catalog, &run).await.map_err(&named)?;

        // A failure of the *loop* — the provider refused, the key is wrong — is
        // this action's failure, reported like any other action's. The run row
        // keeps the transcript and the reason either way (`Runner::drive` marks
        // it before returning), which is what makes a failed triggered run
        // readable afterwards.
        let conclusion = runner.drive(&mut run).await.map_err(|e| {
            Error::invalid(format!("trigger `{}`: agent `{name}`: {e}", ctx.trigger))
        })?;
        // A stuck agent did not do what the trigger asked, and nobody is
        // watching to notice: it is this action's failure, with the reason. The
        // run row keeps the transcript to read it against.
        if let Conclusion::Stuck { reason } = &conclusion {
            return Err(Error::invalid(format!(
                "trigger `{}`: agent `{name}` got stuck (run {}): {reason}",
                ctx.trigger, run.id
            )));
        }

        Ok(json!({
            "agent": agent.name,
            // A string, because a JSON number cannot hold a UUID and the caller
            // that wants the transcript is going to put this in a URL.
            "run": run.id.to_string(),
            "answer": conclusion.answer().unwrap_or_default(),
            "conclusion": conclusion_name(&conclusion),
        }))
    }
}

/// The configured agent's name.
fn configured_agent(config: &Attrs) -> Result<String> {
    sc_action::config_str(config, CFG_AGENT)
}

/// What the prompt formula computed, as something to say to a model.
///
/// A string is itself; anything else is its JSON text, because a formula that
/// computed an object has still computed something and rendering it beats
/// refusing it. **Nothing** is refused: an agent asked an empty question would
/// spend a provider call to answer nothing, which is the silent failure
/// principle 5 rules out.
fn prompt_text(value: Json, trigger: &str) -> Result<String> {
    let text = match value {
        Json::String(s) => s,
        Json::Null => String::new(),
        other => other.to_string(),
    };
    if text.trim().is_empty() {
        return Err(Error::invalid(format!(
            "trigger `{trigger}`: the `{CFG_PROMPT}` formula produced nothing to ask the agent"
        )));
    }
    Ok(text)
}

/// The stored spelling of how a run ended, for the action's result.
fn conclusion_name(conclusion: &Conclusion) -> &'static str {
    match conclusion {
        Conclusion::Answered { .. } => "answered",
        Conclusion::MaxSteps => "max_steps",
        Conclusion::Aborted => "aborted",
        Conclusion::OverBudget { .. } => "over_budget",
        Conclusion::Stuck { .. } => "stuck",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trigger_with_no_agent_configured_says_so() {
        let err = configured_agent(&Attrs::new()).unwrap_err();
        assert!(err.to_string().contains(CFG_AGENT), "{err}");
    }

    #[test]
    fn a_prompt_that_computed_nothing_is_refused_rather_than_asked() {
        // Null, an empty string and whitespace are all "nothing to say" — an
        // agent asked one of those would spend a provider call to answer it.
        for empty in [Json::Null, json!(""), json!("   ")] {
            let err = prompt_text(empty.clone(), "nightly").unwrap_err();
            assert!(err.to_string().contains(CFG_PROMPT), "{empty}: {err}");
        }
        // Anything that did compute something is asked, rendered as its JSON
        // text where it is not already a string.
        assert_eq!(prompt_text(json!("summarise"), "t").unwrap(), "summarise");
        assert_eq!(prompt_text(json!(42), "t").unwrap(), "42");
        assert_eq!(
            prompt_text(json!({ "title": "A" }), "t").unwrap(),
            r#"{"title":"A"}"#
        );
    }

    #[test]
    fn every_ending_has_a_name_the_caller_can_branch_on() {
        assert_eq!(
            conclusion_name(&Conclusion::Answered {
                answer: "hi".to_owned()
            }),
            "answered"
        );
        assert_eq!(conclusion_name(&Conclusion::MaxSteps), "max_steps");
        assert_eq!(conclusion_name(&Conclusion::Aborted), "aborted");
        assert_eq!(
            conclusion_name(&Conclusion::OverBudget {
                budget: sc_agent::Budget::Cost
            }),
            "over_budget"
        );
        assert_eq!(
            conclusion_name(&Conclusion::Stuck {
                reason: "looping".to_owned()
            }),
            "stuck"
        );
    }
}
