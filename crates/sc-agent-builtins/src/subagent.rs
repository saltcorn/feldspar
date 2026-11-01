//! `subagent` — one configured agent as one tool (§11.3).
//!
//! The trait that makes an agent a *capability of another agent*: the parent
//! keeps the conversation, hands one bounded task to a specialist, and reads back
//! what it concluded. What the specialist did to get there — the twenty tool
//! calls, the file it read twice, the query it got wrong first — happened in a
//! context window that is not the parent's, and stays there.
//!
//! ## Why this is worth having at all
//!
//! Two reasons, and they are different reasons.
//!
//! - **Context.** A parent that ran the specialist's tools itself would carry
//!   every intermediate result for the rest of the conversation. A parent that
//!   delegates carries one paragraph. This is the reason multi-agent systems
//!   exist at all in practice, and it is why the child does **not** inherit the
//!   parent's transcript: a sub-agent given its parent's history would spend
//!   exactly the tokens the delegation was meant to save.
//! - **Scope.** An agent is a role floor plus a set of traits. "The agent that
//!   may edit the source" and "the agent that may answer customers" want
//!   different tool sets and different floors, and composing them by delegation
//!   keeps each one's definition readable — which is the first question §11.3
//!   says an admin must be able to answer off an agent's record.
//!
//! ## Delegation, not handoff
//!
//! The alternative shape — hand the conversation over, let the specialist own
//! every message from then on — is not what this does, and
//! [`sc_agent::delegate`] records why: a run has one subject and one authority on
//! it, and a transcript that changed agent halfway would be a record of two
//! agents' work under one heading. Here the parent is answerable for the answer.
//!
//! ## The briefing is the whole channel
//!
//! Because the child sees nothing but what this tool sends it, a bare sentence is
//! the failure mode: "look into that" is not a task to an agent that cannot see
//! what "that" was. So the tool asks the model for a **task**, the **context** it
//! cannot otherwise have, and the **output** wanted — the three things a
//! delegated task demonstrably needs — and assembles them into one briefing under
//! headings. A model that is asked for those three writes them; a model handed
//! one free-text field writes a sentence.
//!
//! ## What comes back
//!
//! The sub-agent's final message **verbatim**, its run id, and how many steps it
//! took. Verbatim because a paraphrase is a second chance to lose the finding,
//! and the run id because that is how the transcript is read (`getRun`) rather
//! than by shipping it back into the parent's context.
//!
//! A sub-agent that ended *without* something to hand back — out of steps, or
//! having called tools all turn and then said nothing — comes back as a tool
//! **error**, not as a result with an empty `answer` field. That failure is
//! specific to this pattern and quiet by nature: a parent that reads `answer: ""`
//! as an answer will report to the person that there was nothing to find.

use sc_agent::{
    AgentTrait, DEFAULT_MAX_DEPTH, DelegateRequest, ToolsContext, TraitCheck, TraitContext,
    load_agent_by_name,
};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::files::slugify;
use crate::table::config_str;

/// The agent this trait delegates to.
pub const CFG_AGENT: &str = "agent";
/// What the parent model is told about when to use it.
pub const CFG_WHEN_TO_USE: &str = "when_to_use";
/// The step budget one delegation may spend.
pub const CFG_MAX_STEPS: &str = "max_steps";
/// How deep delegation may nest below this agent.
pub const CFG_MAX_DEPTH: &str = "max_depth";
/// Text put before the briefing: what the sub-agent starts on, whatever the
/// parent writes (TODO-agent-skills AS9.5).
pub const CFG_HANDOVER_PROMPT: &str = "handover_prompt";
/// Text put after the report the parent reads: what it should do with it
/// (AS9.5).
pub const CFG_HANDOFF_PROMPT: &str = "handoff_prompt";

/// The deepest chain an admin may configure.
///
/// Five, matching the depth the ecosystem's tools settled on: past that the token
/// cost of a chain is not something anyone reasons about, and a task that
/// genuinely needs six levels of agent is a workflow (§10.3), not a conversation.
pub const MAX_CONFIGURABLE_DEPTH: u32 = 5;

/// The briefing's task — required, because it is the delegation.
pub const ARG_TASK: &str = "task";
/// Facts the sub-agent cannot see for itself.
pub const ARG_CONTEXT: &str = "context";
/// What the sub-agent should hand back.
pub const ARG_OUTPUT: &str = "output";

/// Delegate a task to one configured agent.
pub struct Subagent;

/// The tool one `subagent` instance offers, derived from the agent it names.
pub fn tool_name(agent: &str) -> String {
    format!("delegate_to_{}", slugify(agent))
}

#[async_trait::async_trait]
impl AgentTrait for Subagent {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Hand one bounded task to another agent, which works in a context of its \
         own and reports back"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_AGENT, BasicType::Text)
                .label("Agent")
                .required(),
            // The one field that changes how well this works. The parent model
            // chooses between its tools by their descriptions, and "delegate to
            // `researcher`" says nothing about when it should; this is where the
            // admin says what the specialist is for.
            FormField::new(CFG_WHEN_TO_USE, BasicType::Text)
                .label("When to use it")
                .multiline(),
            FormField::new(CFG_MAX_STEPS, BasicType::Int).label("Step budget per delegation"),
            FormField::new(CFG_MAX_DEPTH, BasicType::Int)
                .label("Maximum delegation depth")
                .default_value(DEFAULT_MAX_DEPTH),
            // v1's two prompts: one the admin puts in front of every briefing
            // (the sub-agent's standing orders for this caller), and one the
            // parent reads after every report (what to do with it).
            FormField::new(CFG_HANDOVER_PROMPT, BasicType::Text)
                .label("Handover prompt")
                .sublabel("Put before every briefing the sub-agent is sent")
                .multiline(),
            FormField::new(CFG_HANDOFF_PROMPT, BasicType::Text)
                .label("Handoff prompt")
                .sublabel("Put after every report, for this agent to act on")
                .multiline(),
        ]
    }

    /// The sub-agent must exist, must not be this agent, and the two bounds must
    /// be numbers that mean something — on save, in front of the admin, and again
    /// on load, so an agent naming a deleted sub-agent leaves the live set with a
    /// reason instead of failing when the model calls it.
    ///
    /// Resolved against **storage** rather than the live set, for
    /// [`RunTrigger`](crate::RunTrigger)'s reason: a sub-agent that is stored but
    /// does not currently validate is a repairable state, and the agent that
    /// names it should not also be invalid — one broken thing, one error, in the
    /// place it can be fixed. Delegating then says what is wrong with the
    /// *sub-agent*, in that agent's own validation's words.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let name = configured_agent(check.config)?;
        // The one cycle that can be seen without running anything, refused where
        // it is cheapest to fix. Longer cycles (`a → b → a`) need the chain a run
        // carries and are refused there, by name.
        if name == check.agent.trim() {
            return Err(Error::invalid(format!(
                "`{name}` cannot delegate to itself"
            )));
        }
        if load_agent_by_name(check.catalog, &name).await?.is_none() {
            // With the alternatives, because this field is free text (the
            // sub-agent is not a pick-list for `run_trigger`'s reason: the spec
            // is declared where `_fd_agents` cannot be read) and a typo is the
            // likeliest way to get here.
            let others: Vec<String> = sc_agent::list_agents(check.catalog)
                .await?
                .into_iter()
                .map(|a| format!("`{}`", a.name))
                .filter(|n| n != &format!("`{}`", check.agent.trim()))
                .collect();
            return Err(Error::invalid(match others.is_empty() {
                true => format!("no agent named `{name}`, and there are no others"),
                false => format!(
                    "no agent named `{name}`; the agents it could delegate to are {}",
                    others.join(", ")
                ),
            }));
        }
        if let Some(steps) = config_u32(check.config, CFG_MAX_STEPS)?
            && steps == 0
        {
            return Err(Error::invalid(format!(
                "`{CFG_MAX_STEPS}` must be at least 1: an agent that may not call \
                 the model cannot answer"
            )));
        }
        let depth = max_depth(check.config)?;
        if depth == 0 || depth > MAX_CONFIGURABLE_DEPTH {
            return Err(Error::invalid(format!(
                "`{CFG_MAX_DEPTH}` must be between 1 and {MAX_CONFIGURABLE_DEPTH}, got {depth}"
            )));
        }
        Ok(())
    }

    /// One tool, named for the agent it delegates to.
    ///
    /// The catalog cannot say what that agent *is* — an agent is a row, and
    /// reading one is async, which this is not — so the description is what the
    /// configuration knows: the agent's name, and the admin's own sentence about
    /// when to use it. That is the same bargain
    /// [`RunTrigger`](crate::RunTrigger) makes, and it is why `when_to_use` is a
    /// form field rather than something derived.
    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let agent = config_str(config, CFG_AGENT);
        let when = config_str(config, CFG_WHEN_TO_USE);
        let mut description = format!(
            "Hand one self-contained task to the `{agent}` agent and wait for what \
             it concludes."
        );
        if !when.is_empty() {
            description.push_str(&format!(" Use it when: {when}"));
        }
        description.push_str(&format!(
            " `{agent}` cannot see this conversation — only what you send here — \
             so say everything it needs. It works on its own and returns one \
             report; it cannot ask you a follow-up question. Prefer one large \
             task over several small ones: each delegation is a separate \
             conversation with its own cost."
        ));

        vec![ToolSpec::new(
            tool_name(&agent),
            description,
            json!({
                "type": "object",
                "properties": {
                    ARG_TASK: {
                        "type": "string",
                        "description":
                            "What `{agent}` is to do, in full, as an instruction to \
                             someone who has read nothing above. State the boundary \
                             too: what is *not* being asked for."
                                .replace("{agent}", &agent),
                    },
                    ARG_CONTEXT: {
                        "type": "string",
                        "description":
                            "Facts it cannot see for itself and would otherwise have \
                             to rediscover: what has been established so far, the \
                             names, ids and values it needs, what has already been \
                             tried.",
                    },
                    ARG_OUTPUT: {
                        "type": "string",
                        "description":
                            "What you want back, and in what shape — a list, a \
                             number, a paragraph, a decision with its reason. You \
                             will get its final message and nothing else.",
                    },
                },
                "required": [ARG_TASK],
                "additionalProperties": false,
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
        let agent = configured_agent(config)?;
        let mut report = delegate_with(
            ctx,
            &agent,
            args,
            config_u32(config, CFG_MAX_STEPS)?,
            max_depth(config)?,
            &config_str(config, CFG_HANDOVER_PROMPT),
        )
        .await?;
        // After the report, never inside it: the answer stays verbatim, and
        // what the parent is to do with it is a field of its own.
        let handoff = config_str(config, CFG_HANDOFF_PROMPT);
        if !handoff.is_empty() {
            report["handoff"] = Json::String(handoff);
        }
        Ok(report)
    }
}

/// Hand `args`' briefing to `agent` and return its report — or, when it ended
/// with nothing to hand back, the error that says why.
///
/// Shared with `admin_copilot`'s `delegate_to_coding_agent`, which names its
/// sub-agent per call rather than per configuration: one delegation, refused in
/// the same words whichever tool asked.
pub(crate) async fn delegate(
    ctx: &mut TraitContext<'_>,
    agent: &str,
    args: &Json,
    max_steps: Option<u32>,
    max_depth: u32,
) -> Result<Json> {
    delegate_with(ctx, agent, args, max_steps, max_depth, "").await
}

/// [`delegate`], with `handover` put before the briefing when it is not empty.
async fn delegate_with(
    ctx: &mut TraitContext<'_>,
    agent: &str,
    args: &Json,
    max_steps: Option<u32>,
    max_depth: u32,
    handover: &str,
) -> Result<Json> {
    let mut briefing = briefing(agent, ctx.agent, args)?;
    if !handover.trim().is_empty() {
        briefing = format!("{}\n\n{briefing}", handover.trim());
    }
    let delegate = ctx.require_delegate()?;

    let outcome = delegate
        .delegate(
            DelegateRequest::new(agent, &briefing, ctx.run)
                .max_steps(max_steps)
                .max_depth(max_depth),
        )
        .await?;

    // A sub-agent that finished without saying anything is a failure of the
    // delegation, reported as one — see the module docs. The message is
    // written for the reader it has: a model deciding what to do next.
    let Some(answer) = outcome.answer() else {
        return Err(Error::invalid(match &outcome.conclusion {
            sc_agent::Conclusion::MaxSteps => format!(
                "`{agent}` used its whole budget of {} steps without reaching a \
                 conclusion. Its transcript is run {}. Ask it again for a \
                 smaller piece of the task, or do the work here.",
                outcome.steps, outcome.run
            ),
            sc_agent::Conclusion::OverBudget { budget } => format!(
                "`{agent}` ran out of its {budget} budget after {} steps without \
                 reaching a conclusion. Its transcript is run {}. Ask it for a \
                 smaller piece of the task, or do the work here.",
                outcome.steps, outcome.run
            ),
            sc_agent::Conclusion::Stuck { reason } => format!(
                "`{agent}` was stopped after {} steps because it was going round in \
                 circles: {reason}. Its transcript is run {}. Do the work here, or \
                 ask for it differently.",
                outcome.steps, outcome.run
            ),
            // Nobody chats with a sub-agent, so its question has nobody to
            // answer it (TODO-agent-skills AS6).
            sc_agent::Conclusion::AwaitingUser { request } => format!(
                "`{agent}` stopped to ask the person a question, which a sub-agent \
                 cannot: \"{}\" (run {}). Answer it yourself in a new briefing, or ask \
                 the person here.",
                request.prompt, outcome.run
            ),
            _ => format!(
                "`{agent}` finished without reporting anything (run {}). It may \
                 have done the work and failed to say so. Ask again, stating in \
                 `{ARG_OUTPUT}` exactly what it must include in its final \
                 message.",
                outcome.run
            ),
        }));
    };

    Ok(json!({
        "agent": outcome.agent,
        // A string, because a JSON number cannot hold a UUID — and the
        // reader that wants the transcript is going to put this in a URL.
        "run": outcome.run.to_string(),
        "steps": outcome.steps,
        // Verbatim. A summary here would be a second chance to lose the
        // finding, and the parent is about to read this anyway.
        "answer": answer,
    }))
}

/// The configured sub-agent's name.
fn configured_agent(config: &Attrs) -> Result<String> {
    let name = config_str(config, CFG_AGENT);
    if name.is_empty() {
        return Err(Error::invalid(format!("`{CFG_AGENT}` is required")));
    }
    Ok(name)
}

/// A whole number configuration value, or `None` where the admin set none.
fn config_u32(config: &Attrs, key: &str) -> Result<Option<u32>> {
    match config.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| Error::invalid(format!("`{key}` should be a whole number, got {n}"))),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a number, got {other}"
        ))),
    }
}

/// The configured depth bound, defaulting to [`DEFAULT_MAX_DEPTH`].
fn max_depth(config: &Attrs) -> Result<u32> {
    Ok(config_u32(config, CFG_MAX_DEPTH)?.unwrap_or(DEFAULT_MAX_DEPTH))
}

/// The one message the sub-agent will see, from what the model sent.
///
/// Assembled here rather than passed through, for the reason the module gives:
/// this message is the entire channel, so its shape is part of the design and not
/// the model's to choose. The framing sentence names the parent and says what the
/// sub-agent's final message is for — the one instruction that fixes the failure
/// mode where a sub-agent does the work and reports none of it.
fn briefing(agent: &str, parent: &str, args: &Json) -> Result<String> {
    let field = |key: &str| args.get(key).and_then(Json::as_str).unwrap_or("").trim();
    let task = field(ARG_TASK);
    if task.is_empty() {
        return Err(Error::invalid(format!(
            "`{ARG_TASK}` is required: say what `{agent}` is to do"
        )));
    }

    let mut out = format!(
        "The `{parent}` agent has asked you to do one task. It cannot see your \
         work — only the final message you end with — so that message must \
         contain everything it needs, in full. Do not ask a question back; nobody \
         will answer it.\n\n## Task\n\n{task}\n"
    );
    for (heading, value) in [
        ("Context", field(ARG_CONTEXT)),
        ("Expected output", field(ARG_OUTPUT)),
    ] {
        if !value.is_empty() {
            out.push_str(&format!("\n## {heading}\n\n{value}\n"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_the_agent_it_delegates_to() {
        assert_eq!(tool_name("researcher"), "delegate_to_researcher");
        // A name a provider would refuse becomes one it accepts, the same way
        // every other derived name in this crate does.
        assert_eq!(tool_name("Schema Builder!"), "delegate_to_schema_builder");
    }

    #[test]
    fn a_trait_with_no_agent_configured_says_so() {
        let err = configured_agent(&Attrs::new()).unwrap_err();
        assert!(err.to_string().contains(CFG_AGENT), "{err}");
    }

    #[test]
    fn the_depth_bound_defaults_and_reads_a_configured_number() {
        assert_eq!(max_depth(&Attrs::new()).unwrap(), DEFAULT_MAX_DEPTH);
        let config: Attrs = [(CFG_MAX_DEPTH.to_owned(), json!(1))].into_iter().collect();
        assert_eq!(max_depth(&config).unwrap(), 1);
        // Not a number is refused rather than silently defaulted: a bound that
        // did not mean what the admin typed is worse than no bound.
        let config: Attrs = [(CFG_MAX_DEPTH.to_owned(), json!("deep"))]
            .into_iter()
            .collect();
        assert!(max_depth(&config).is_err());
    }

    #[test]
    fn a_briefing_carries_the_three_things_and_says_what_the_last_message_is_for() {
        let brief = briefing(
            "researcher",
            "librarian",
            &json!({
                ARG_TASK: "Find how many books Ada owns",
                ARG_CONTEXT: "Ada is ada@example.com",
                ARG_OUTPUT: "A single number",
            }),
        )
        .unwrap();
        assert!(brief.contains("`librarian` agent has asked you"), "{brief}");
        // The instruction that fixes the "did the work, reported none of it"
        // failure mode.
        assert!(brief.contains("only the final message"), "{brief}");
        assert!(brief.contains("## Task\n\nFind how many books"), "{brief}");
        assert!(brief.contains("## Context\n\nAda is ada@"), "{brief}");
        assert!(brief.contains("## Expected output\n\nA single"), "{brief}");
    }

    #[test]
    fn the_optional_parts_of_a_briefing_leave_no_empty_headings() {
        let brief = briefing("researcher", "librarian", &json!({ARG_TASK: "Count them"})).unwrap();
        assert!(brief.contains("## Task"), "{brief}");
        assert!(!brief.contains("## Context"), "{brief}");
        assert!(!brief.contains("## Expected output"), "{brief}");
    }

    #[test]
    fn a_delegation_with_nothing_to_do_is_refused_before_a_run_is_started() {
        // An empty task would spend a whole sub-agent run to answer nothing —
        // the same refusal `run_agent` makes of an empty prompt.
        for args in [json!({}), json!({ARG_TASK: "   "}), json!(null)] {
            let err = briefing("researcher", "librarian", &args).unwrap_err();
            assert!(err.to_string().contains(ARG_TASK), "{args}: {err}");
        }
    }
}
