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
//! - **It runs with the trigger's authority** ([`RunCaller::triggered`],
//!   decision 5) over rows, but without the tools that act as the server itself
//!   — the shell, module installs, the project's scripts and checks, the admin
//!   copilot — because the prompt is computed from an event, and whoever can
//!   cause the event can write into it. [`CFG_TRUSTED_INPUT`] restores them
//!   ([`RunCaller::system`]) for a trigger whose event only trusted people
//!   cause. The run row then records `user: null`, and the agent's own
//!   `min_role` is not a gate: the floor describes who may *chat* with it, and
//!   a trigger clears every floor. What gates a triggered run is the trigger,
//!   which is the thing an admin already knows how to guard (§10.2, §13.2).
//! - **Or as whoever caused the event** ([`CFG_RUN_AS`] = `event_user`,
//!   [`RunCaller::event_user`]): the signed-in user who called
//!   `POST /api/actions/<trigger>` or wrote the row, at their own role, or the
//!   public role when nobody was signed in. The tools then see what that person
//!   may see, the run records them, and the agent's `min_role` applies as it
//!   does in a chat. Still never the server's own tools.
//! - **It does not stream.** An action returns a value (§10.1), and a caller who
//!   wants deltas is a chat client. What it returns is the final assistant
//!   message and the **run id**, so the run is `getRun`-able afterwards and the
//!   chat panel's history is where a triggered conversation is read.
//! - **The run is written after every step, as any run is**, so a triggered run
//!   that failed halfway is still a readable transcript with a reason on it
//!   rather than a line in a log.
//!
//! ## Running triggers from a triggered run
//!
//! The agent's tools get *the* dispatcher — the one running this action — so
//! `run_trigger` and `run_js_code` work as they do in a chat. The run carries
//! this firing's chain ([`RunCaller::chained`]), and every trigger its tools run
//! or write they cause carries it on, so a trigger → agent → trigger loop is
//! one cascade and stops at the dispatcher's depth limit like any other.

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
/// Whether the event this trigger fires on is one only trusted people can
/// cause, so the run may have the tools that act as the server itself (the
/// shell, module installs, the project's scripts and checks, the admin
/// copilot). Off by default: see [`RunCaller::triggered`].
pub const CFG_TRUSTED_INPUT: &str = "trusted_input";

/// Who the agent's tools run as: [`RUN_AS_TRIGGER`] (the default) or
/// [`RUN_AS_EVENT_USER`].
pub const CFG_RUN_AS: &str = "run_as";
/// [`CFG_RUN_AS`]: the trigger's own authority, and no user.
pub const RUN_AS_TRIGGER: &str = "trigger";
/// [`CFG_RUN_AS`]: whoever caused the event, at their own role.
pub const RUN_AS_EVENT_USER: &str = "event_user";

/// Whether [`CFG_RUN_AS`] says `event_user`; refused when it names neither.
fn runs_as_event_user(config: &Attrs) -> Result<bool> {
    match config.get(CFG_RUN_AS) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::String(s)) if s.trim().is_empty() || s == RUN_AS_TRIGGER => Ok(false),
        Some(Json::String(s)) if s == RUN_AS_EVENT_USER => Ok(true),
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_RUN_AS}` should be `{RUN_AS_TRIGGER}` or `{RUN_AS_EVENT_USER}`, not {other}"
        ))),
    }
}

/// Whether [`CFG_TRUSTED_INPUT`] is on. Anything but `true` is off.
fn trusted_input(config: &Attrs) -> bool {
    config
        .get(CFG_TRUSTED_INPUT)
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

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
                .sublabel(
                    "A formula over the event, not a {{ }} template: \
                     `payload.prompt`, or `\"Summarise \" + row.title`",
                )
                .required(),
            FormField::new(CFG_RUN_AS, BasicType::Text)
                .label("Run the agent's tools as")
                .options([RUN_AS_TRIGGER.to_owned(), RUN_AS_EVENT_USER.to_owned()])
                .default_value(RUN_AS_TRIGGER),
            FormField::new(CFG_TRUSTED_INPUT, BasicType::Bool)
                .label(
                    "Only trusted people can cause this event: let the agent use the shell, \
                     install modules, run scripts and checks, and use the admin copilot",
                )
                .default_value(false),
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
        let event_user = runs_as_event_user(check.config)?;
        if event_user && trusted_input(check.config) {
            return Err(Error::invalid(format!(
                "`{CFG_TRUSTED_INPUT}` gives the agent the server's own tools, which a run \
                 as the event's user never has; turn one of them off"
            )));
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
            .map_err(named)?;
        let agent = agents.require(&name).map_err(named)?;
        // Decision 5, at the one place a run is created. As the event's user:
        // their authority, gated by the agent's floor as a chat is. Otherwise
        // nobody is present, so the run carries the trigger's authority and
        // records no user — without the server's own tools, unless the admin
        // said the event is one nobody untrusted can cause. An event a cascade
        // of untrusted origin raised is one, whatever the trigger says
        // (security review #32).
        let caller = if runs_as_event_user(ctx.config).map_err(named)? {
            let user =
                sc_auth::User::from_json(ctx.event.role, ctx.event.user.as_ref()).map_err(named)?;
            let caller = RunCaller::event_user(user);
            if let Some(floor) = agent.min_role
                && !caller.meets_role(Some(floor))
            {
                return Err(Error::auth(format!(
                    "trigger `{}`: agent `{name}` is for role {floor} and above, \
                     and the user who caused this event is not",
                    ctx.trigger
                )));
            }
            caller
        } else if trusted_input(ctx.config) && !sc_catalog::is_untrusted_chain(&ctx.chain) {
            RunCaller::system()
        } else {
            RunCaller::triggered()
        };
        // This firing's chain, so whatever the agent's tools run or write is
        // the same cascade, bounded by the same depth limit.
        let caller = caller.chained(ctx.chain.clone());
        let dispatcher = ctx.shared_triggers();
        // After the floor, so a person the agent is not for costs no connection.
        let executor = self
            .providers
            .connect(ctx.catalog, agent, sc_agent::ModelRole::Executor)
            .await
            .map_err(named)?;
        let mut runner = Runner::new(ctx.catalog, &self.traits, agent, executor, caller)
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
        // The row the event is about, for a trait whose session header is
        // computed from it (`preload_rows`' filter, AS8.1).
        if let Some(row) = &ctx.event.row {
            runner = runner.with_row(row.clone());
        }
        // The dispatcher running this action, for `run_trigger` and
        // `run_js_code`: the chain above is what keeps a loop through them
        // bounded.
        if let Some(dispatcher) = &dispatcher {
            runner = runner.with_triggers(dispatcher);
        }

        // Built here rather than through `Runner::start` for the one thing that
        // call cannot give it: a description. A run list carries no transcript,
        // so without this a triggered conversation would be a timestamp in the
        // chat panel's history.
        let mut run = runner
            .new_run(&prompt)
            .map_err(named)?
            .description(format!("trigger `{}`", ctx.trigger));
        save_run(ctx.catalog, &run).await.map_err(named)?;

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
        // Nobody is chatting with a triggered run: it is left waiting, and
        // answered from the admin's chat.
        Conclusion::AwaitingUser { .. } => "awaiting_user",
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
