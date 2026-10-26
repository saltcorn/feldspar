//! `plan_approval` — the model submits a plan and waits for the person to
//! approve it (TODO-agent-skills AS10.2; v1's "Plan approval" skill).
//!
//! One tool, `submit_plan_for_approval` unless the configuration renames it.
//! The plan is shown to the person with an Approve button (an `approve`
//! request, AS6.1), the run's turn ends, and the answer is the next user
//! message (AS6.2): `approval_prompt` when approved, `decline_prompt` when
//! not.
//!
//! **Why this is not `RunMode::Plan`.** A run's mode is fixed on its row when
//! it starts, and the only trait that reads it is `coding`, which withholds
//! its edit tools outside `act`; no other trait offers fewer tools in `plan`.
//! Making this trait mean "plan, approve, act" would need a run that changes
//! mode mid-conversation and every table and action trait to declare which of
//! its tools are read-only — a larger change than v1's skill, which is a
//! prompt and a button. So the plan is a tool call, the approval a request,
//! and what the model may do before approval is what the system prompt tells
//! it (decided in AS10.2; see TODO-agent-skills.md).

use sc_agent::{AgentTrait, RequestKind, ToolsContext, TraitCheck, TraitContext, UserRequest};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::table::config_str;

/// The tool's name, `submit_plan_for_approval` when unset.
pub const CFG_PLAN_TOOL_NAME: &str = "tool_name";
/// The system prompt the trait adds, in place of its own.
pub const CFG_PLAN_PROMPT: &str = "prompt";
/// The message the model is sent when the plan is approved.
pub const CFG_APPROVAL_PROMPT: &str = "approval_prompt";
/// The message the model is sent when the plan is declined.
pub const CFG_DECLINE_PROMPT: &str = "decline_prompt";
/// The approving button's label.
pub const CFG_APPROVE_LABEL: &str = "approve_label";

/// The tool's name when the configuration gives none: v1's.
pub const DEFAULT_PLAN_TOOL: &str = "submit_plan_for_approval";

const DEFAULT_APPROVAL_PROMPT: &str = "I approve the plan. Carry it out.";
const DEFAULT_DECLINE_PROMPT: &str = "I do not approve this plan. Ask me what should change \
     before doing anything.";
const DEFAULT_APPROVE_LABEL: &str = "Approve";
const ARG_PLAN: &str = "plan";

/// Submit a plan for the person's approval.
pub struct PlanApproval;

/// The tool name this configuration gives `plan_approval`.
pub fn tool_name(config: &Attrs) -> String {
    match config_str(config, CFG_PLAN_TOOL_NAME) {
        name if name.is_empty() => DEFAULT_PLAN_TOOL.to_owned(),
        name => name,
    }
}

fn or_default(config: &Attrs, key: &str, default: &str) -> String {
    match config_str(config, key) {
        text if text.is_empty() => default.to_owned(),
        text => text,
    }
}

#[async_trait::async_trait]
impl AgentTrait for PlanApproval {
    fn name(&self) -> &str {
        "plan_approval"
    }

    fn description(&self) -> &str {
        "The agent submits a plan, and carries it out once the person approves it"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_PLAN_TOOL_NAME, BasicType::Text)
                .label("Tool name")
                .sublabel("What the model calls it; `submit_plan_for_approval` when empty"),
            FormField::new(CFG_PLAN_PROMPT, BasicType::Text)
                .label("System prompt")
                .multiline()
                .sublabel("When to plan and what a plan holds; a short default when empty"),
            FormField::new(CFG_APPROVAL_PROMPT, BasicType::Text)
                .label("Prompt on approval")
                .multiline()
                .sublabel("If the person approves the plan, what should the agent do?"),
            FormField::new(CFG_DECLINE_PROMPT, BasicType::Text)
                .label("Prompt on decline")
                .multiline(),
            FormField::new(CFG_APPROVE_LABEL, BasicType::Text)
                .label("Approve button label"),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let name = tool_name(check.config);
        if name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::invalid(format!(
                "`{CFG_PLAN_TOOL_NAME}` `{name}` must be at most 64 letters, digits, `_` or `-`"
            )));
        }
        Ok(())
    }

    fn prompt(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Option<String> {
        let name = tool_name(config);
        Some(or_default(
            config,
            CFG_PLAN_PROMPT,
            &format!(
                "Before you change anything, work out a plan and submit it with the `{name}` \
                 tool. Do not carry out any step of it until the user has approved it; their \
                 decision arrives as the next user message."
            ),
        ))
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            tool_name(config),
            "Submit a plan for approval by the user. The conversation pauses until they \
             decide; if they approve it, further instructions about carrying it out follow.",
            json!({
                "type": "object",
                "properties": {
                    ARG_PLAN: {
                        "type": "string",
                        "description": "The plan, as Markdown: the steps, in order",
                    },
                },
                "required": [ARG_PLAN],
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
        let plan = args
            .get(ARG_PLAN)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::invalid(format!("`{ARG_PLAN}` is required, as text")))?;
        ctx.await_user(UserRequest::new(
            plan,
            RequestKind::Approve {
                label: or_default(config, CFG_APPROVE_LABEL, DEFAULT_APPROVE_LABEL),
                payload: Json::Null,
            },
        ))?;
        Ok(json!(
            "The plan has been put to the user for approval. Wait for their decision, which \
             arrives as the next user message; do not start on it before then."
        ))
    }

    async fn on_answer(
        &self,
        config: &Attrs,
        _request: &UserRequest,
        value: &Json,
    ) -> Result<String> {
        Ok(if value == &Json::Bool(true) {
            or_default(config, CFG_APPROVAL_PROMPT, DEFAULT_APPROVAL_PROMPT)
        } else {
            or_default(config, CFG_DECLINE_PROMPT, DEFAULT_DECLINE_PROMPT)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approving_sends_the_approval_prompt_and_declining_the_other() {
        let config: Attrs = [("approval_prompt".to_owned(), json!("Go ahead."))]
            .into_iter()
            .collect();
        let request = UserRequest::new(
            "1. Do it",
            RequestKind::Approve {
                label: "Approve".into(),
                payload: Json::Null,
            },
        );
        assert_eq!(
            PlanApproval
                .on_answer(&config, &request, &json!(true))
                .await
                .unwrap(),
            "Go ahead."
        );
        assert!(
            PlanApproval
                .on_answer(&config, &request, &json!(false))
                .await
                .unwrap()
                .contains("do not approve")
        );
        assert_eq!(tool_name(&Attrs::new()), DEFAULT_PLAN_TOOL);
    }
}
