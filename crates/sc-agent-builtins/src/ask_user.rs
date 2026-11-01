//! `ask_user` — the model asks the person a question in the chat
//! (TODO-agent-skills AS10.1; v1's "Ask user question" skill).
//!
//! One tool, `ask_user_question` unless the configuration renames it. A call
//! puts **one** question to the person: pick one option, tick several, or
//! write an answer. The run's turn ends there, in the `awaiting_user` state
//! (AS6.1), and the person's answer is the next user message (AS6.2), in the
//! words of the configured answer prompt — v1's `{{ question }}` and
//! `{{ answer }}` templates.
//!
//! - **A "discuss" option.** When the model sets `allow_discussion`, a choice
//!   gets one more option, labelled by `discuss_label`: the person declines to
//!   answer yet, and the model is sent `discuss_prompt` instead. A free-text
//!   question needs none — the person can always say anything they like as an
//!   ordinary message, which passes the question over.
//! - **One question per call**, where v1 put up to six in tabs: a request is
//!   one question (AS6.1), and a model that needs two asks twice.

use sc_agent::{
    AgentTrait, ChoiceOption, RequestKind, ToolsContext, TraitCheck, TraitContext, UserRequest,
};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::table::config_str;

/// The tool's name, `ask_user_question` when unset.
pub const CFG_ASK_TOOL_NAME: &str = "tool_name";
/// Text added to the system prompt after the trait's own.
pub const CFG_ASK_PROMPT: &str = "prompt";
/// The message the model is sent for an answer: `{{ question }}`,
/// `{{ answer }}` and `{{ answer_description }}`.
pub const CFG_ANSWER_PROMPT: &str = "answer_prompt";
/// The label of the "discuss" option.
pub const CFG_DISCUSS_LABEL: &str = "discuss_label";
/// The message the model is sent when the person chooses to discuss:
/// `{{ question }}`.
pub const CFG_DISCUSS_PROMPT: &str = "discuss_prompt";

/// The tool's name when the configuration gives none: v1's.
pub const DEFAULT_ASK_TOOL: &str = "ask_user_question";
/// The value the "discuss" option carries.
pub const DISCUSS_VALUE: &str = "__discuss__";

const DEFAULT_ANSWER_PROMPT: &str =
    "In answer to the question \"{{ question }}\", I choose: {{ answer }}";
const DEFAULT_TEXT_ANSWER_PROMPT: &str =
    "In answer to the question \"{{ question }}\", I write: {{ answer }}";
const DEFAULT_DISCUSS_PROMPT: &str = "I do not want to answer yet. Let's discuss the question \
     \"{{ question }}\" instead. Do not assume an answer, and do not carry on with the task \
     until I have decided.";
const DEFAULT_DISCUSS_LABEL: &str = "Discuss instead";

const ARG_QUESTION: &str = "question";
const ARG_TYPE: &str = "type";
const ARG_OPTIONS: &str = "options";
const ARG_ALLOW_DISCUSSION: &str = "allow_discussion";
const TYPE_SINGLE: &str = "single_select";
const TYPE_MULTI: &str = "multi_select";
const TYPE_TEXT: &str = "free_text";

/// Ask the person a question in the chat.
pub struct AskUser;

/// The tool name this configuration gives `ask_user`.
pub fn tool_name(config: &Attrs) -> String {
    match config_str(config, CFG_ASK_TOOL_NAME) {
        name if name.is_empty() => DEFAULT_ASK_TOOL.to_owned(),
        name => name,
    }
}

#[async_trait::async_trait]
impl AgentTrait for AskUser {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Lets the agent ask the person a question in the chat and wait for the answer"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_ASK_TOOL_NAME, BasicType::Text)
                .label("Tool name")
                .sublabel("What the model calls it; `ask_user_question` when empty"),
            FormField::new(CFG_ASK_PROMPT, BasicType::Text)
                .label("Additional system prompt")
                .multiline(),
            FormField::new(CFG_ANSWER_PROMPT, BasicType::Text)
                .label("Prompt on answer")
                .multiline()
                .sublabel(
                    "The message the agent is sent for an answer. Use {{ question }}, \
                     {{ answer }} and {{ answer_description }}",
                ),
            FormField::new(CFG_DISCUSS_LABEL, BasicType::Text)
                .label("Discuss label")
                .sublabel("The option that declines to answer yet; \"Discuss instead\" when empty"),
            FormField::new(CFG_DISCUSS_PROMPT, BasicType::Text)
                .label("Prompt on discuss")
                .multiline()
                .sublabel("The message the agent is sent on discuss. Use {{ question }}"),
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
                "`{CFG_ASK_TOOL_NAME}` `{name}` must be at most 64 letters, digits, `_` or `-`"
            )));
        }
        Ok(())
    }

    fn prompt(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Option<String> {
        let name = tool_name(config);
        let mut text = format!(
            "When you need a decision that only the user can give, ask them with the `{name}` \
             tool rather than writing the question in your reply. A question can be single \
             choice, multiple choice or free text. The conversation stops until the user \
             answers, and their answer comes back to you as the next user message. Do not use \
             it for anything you can work out yourself, and ask one question at a time."
        );
        let extra = config_str(config, CFG_ASK_PROMPT);
        if !extra.is_empty() {
            text.push_str("\n\n");
            text.push_str(&extra);
        }
        Some(text)
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            tool_name(config),
            "Put one question to the user and wait for their answer, which arrives as the \
             next user message. Do not guess the answer, and do not carry on until it arrives.",
            json!({
                "type": "object",
                "properties": {
                    ARG_QUESTION: {
                        "type": "string",
                        "description": "The question, as a single sentence",
                    },
                    ARG_TYPE: {
                        "type": "string",
                        "enum": [TYPE_SINGLE, TYPE_MULTI, TYPE_TEXT],
                        "description": "single_select: the user picks one option. \
                            multi_select: the user ticks any number of them. free_text: the \
                            user writes an answer; no options",
                    },
                    ARG_OPTIONS: {
                        "type": "array",
                        "description": "For single_select and multi_select: two to six \
                            alternatives, each a short distinct label",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string" },
                                "description": {
                                    "type": "string",
                                    "description": "Optional: what choosing it means",
                                },
                            },
                            "required": ["label"],
                        },
                    },
                    ARG_ALLOW_DISCUSSION: {
                        "type": "boolean",
                        "description": "Offer the user a way to discuss the question instead \
                            of answering it: set it when the options may not be exhaustive",
                    },
                },
                "required": [ARG_QUESTION, ARG_TYPE],
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
        let question = args
            .get(ARG_QUESTION)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| Error::invalid(format!("`{ARG_QUESTION}` is required, as text")))?;
        let type_ = args
            .get(ARG_TYPE)
            .and_then(Json::as_str)
            .unwrap_or(TYPE_SINGLE);
        let kind = match type_ {
            TYPE_TEXT => RequestKind::Text { placeholder: None },
            TYPE_SINGLE | TYPE_MULTI => {
                let mut options = options(args.get(ARG_OPTIONS))?;
                if args
                    .get(ARG_ALLOW_DISCUSSION)
                    .and_then(Json::as_bool)
                    .unwrap_or(false)
                {
                    options.push(ChoiceOption::new(DISCUSS_VALUE).label(discuss_label(config)));
                }
                RequestKind::Choice {
                    options,
                    multiple: type_ == TYPE_MULTI,
                }
            }
            other => {
                return Err(Error::invalid(format!(
                    "`{ARG_TYPE}` is `{TYPE_SINGLE}`, `{TYPE_MULTI}` or `{TYPE_TEXT}`, not \
                     `{other}`"
                )));
            }
        };
        ctx.await_user(UserRequest::new(question, kind))?;
        Ok(json!(format!(
            "The question \"{question}\" has been put to the user. The conversation is paused \
             until they answer; their answer, or a wish to discuss it instead, arrives as the \
             next user message. Do not proceed, and do not guess what they will answer."
        )))
    }

    async fn on_answer(
        &self,
        config: &Attrs,
        request: &UserRequest,
        value: &Json,
    ) -> Result<String> {
        let question = request.prompt.trim();
        let RequestKind::Choice { options, .. } = &request.kind else {
            // Free text.
            let answer = value.as_str().unwrap_or_default().trim();
            return Ok(fill(
                &template(config, CFG_ANSWER_PROMPT, DEFAULT_TEXT_ANSWER_PROMPT),
                question,
                answer,
                "",
            ));
        };
        let chosen: Vec<&str> = match value {
            Json::Array(values) => values.iter().filter_map(Json::as_str).collect(),
            Json::String(v) => vec![v.as_str()],
            _ => Vec::new(),
        };
        if chosen.contains(&DISCUSS_VALUE) {
            return Ok(fill(
                &template(config, CFG_DISCUSS_PROMPT, DEFAULT_DISCUSS_PROMPT),
                question,
                "",
                "",
            ));
        }
        let picked: Vec<&ChoiceOption> = chosen
            .iter()
            .filter_map(|v| options.iter().find(|o| o.value == *v))
            .collect();
        let answer = if picked.is_empty() {
            "none of them".to_owned()
        } else {
            picked
                .iter()
                .map(|o| o.shown())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let description = picked
            .iter()
            .filter_map(|o| o.description.as_deref())
            .collect::<Vec<_>>()
            .join("; ");
        Ok(fill(
            &template(config, CFG_ANSWER_PROMPT, DEFAULT_ANSWER_PROMPT),
            question,
            &answer,
            &description,
        ))
    }
}

/// The options the model gave: objects with a label (and a description), or
/// plain strings, each label once.
fn options(given: Option<&Json>) -> Result<Vec<ChoiceOption>> {
    let Some(Json::Array(items)) = given else {
        return Err(Error::invalid(format!(
            "a `{TYPE_SINGLE}` or `{TYPE_MULTI}` question needs `{ARG_OPTIONS}`, a list of \
             {{ label, description }}"
        )));
    };
    let mut out: Vec<ChoiceOption> = Vec::with_capacity(items.len());
    for item in items {
        let (label, description) = match item {
            Json::String(label) => (label.trim().to_owned(), None),
            Json::Object(obj) => (
                obj.get("label")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                obj.get("description")
                    .and_then(Json::as_str)
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned),
            ),
            _ => (String::new(), None),
        };
        if label.is_empty() || label == DISCUSS_VALUE {
            return Err(Error::invalid(format!(
                "each of `{ARG_OPTIONS}` needs a label"
            )));
        }
        if out.iter().any(|o| o.value == label) {
            return Err(Error::invalid(format!(
                "the option `{label}` is given twice; each label must be distinct"
            )));
        }
        let mut option = ChoiceOption::new(label);
        if let Some(description) = description {
            option = option.description(description);
        }
        out.push(option);
    }
    Ok(out)
}

fn discuss_label(config: &Attrs) -> String {
    match config_str(config, CFG_DISCUSS_LABEL) {
        label if label.is_empty() => DEFAULT_DISCUSS_LABEL.to_owned(),
        label => label,
    }
}

fn template(config: &Attrs, key: &str, default: &str) -> String {
    match config_str(config, key) {
        text if text.is_empty() => default.to_owned(),
        text => text,
    }
}

/// Fill `{{ question }}`, `{{ answer }}` and `{{ answer_description }}` (v1's
/// placeholders, `{{! name }}` too). Not HTML-escaped: the result is a chat
/// message. An unknown placeholder is left as it is.
pub fn fill(template: &str, question: &str, answer: &str, description: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim().trim_start_matches('!').trim();
        match name {
            "question" => out.push_str(question),
            "answer" => out.push_str(answer),
            "answer_description" => out.push_str(description),
            _ => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(value: Json) -> Attrs {
        value.as_object().unwrap().clone().into_iter().collect()
    }

    #[test]
    fn templates_are_filled_as_v1_fills_them() {
        assert_eq!(
            fill(
                "Q: {{ question }} A: {{answer}} ({{! answer_description }}) {{ other }}",
                "Why?",
                "Because",
                "d"
            ),
            "Q: Why? A: Because (d) {{ other }}"
        );
        assert_eq!(fill("no braces", "q", "a", ""), "no braces");
        assert_eq!(fill("{{ question", "q", "a", ""), "{{ question");
    }

    #[tokio::test]
    async fn an_answer_and_a_discussion_read_as_the_configured_prompts() {
        let request = UserRequest::new(
            "Which database?",
            RequestKind::Choice {
                options: vec![
                    ChoiceOption::new("Postgres").description("the big one"),
                    ChoiceOption::new("SQLite"),
                    ChoiceOption::new(DISCUSS_VALUE).label("Discuss instead"),
                ],
                multiple: false,
            },
        );
        let plain = config(json!({}));
        assert_eq!(
            AskUser
                .on_answer(&plain, &request, &json!("Postgres"))
                .await
                .unwrap(),
            "In answer to the question \"Which database?\", I choose: Postgres"
        );
        let custom = config(json!({
            "answer_prompt": "{{ answer }} ({{ answer_description }})",
            "discuss_prompt": "Talk about {{ question }}",
        }));
        assert_eq!(
            AskUser
                .on_answer(&custom, &request, &json!("Postgres"))
                .await
                .unwrap(),
            "Postgres (the big one)"
        );
        assert_eq!(
            AskUser
                .on_answer(&custom, &request, &json!(DISCUSS_VALUE))
                .await
                .unwrap(),
            "Talk about Which database?"
        );
        let text = UserRequest::new("Your name?", RequestKind::Text { placeholder: None });
        assert!(
            AskUser
                .on_answer(&plain, &text, &json!("Ada"))
                .await
                .unwrap()
                .ends_with("I write: Ada")
        );
    }

    #[test]
    fn options_must_be_labelled_and_distinct() {
        assert_eq!(
            options(Some(&json!(["a", { "label": "b" }])))
                .unwrap()
                .len(),
            2
        );
        assert!(options(Some(&json!(["a", "a"]))).is_err());
        assert!(options(Some(&json!([{ "description": "x" }]))).is_err());
        assert!(options(None).is_err());
    }
}
