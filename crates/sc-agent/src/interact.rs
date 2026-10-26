//! Interactive chat (TODO-agent-skills AS6): what a tool can ask the person,
//! what it can show them, and what the composer can carry.
//!
//! Three things, each kept out of the model's request:
//!
//! - A **request** ([`UserRequest`]): a tool asks the person something — pick
//!   from options, type an answer, approve a plan, press a button. The run's
//!   turn ends there, concluded [`AwaitingUser`](crate::Conclusion::AwaitingUser)
//!   and stored, so no connection and no task waits on a person. The answer is
//!   the next user message, carrying `{ answers_request, value }` ([`Answer`]);
//!   the trait that asked turns it into the text the model reads
//!   ([`AgentTrait::on_answer`](crate::AgentTrait::on_answer)), and an answer to
//!   a request that is no longer waiting is refused.
//! - A **display part** ([`DisplayPart`]): rows, an image or audio a tool shows
//!   the **person** and not the model. Parts are stored in `_fd_run_parts`, the
//!   run keeps a [`DisplayRef`] to each under the tool call that made it, and
//!   nothing of them reaches a request.
//! - A **composer control** ([`ComposerControl`]): a select or a button a trait
//!   adds to the chat's input row. Its value travels with the message as
//!   `{ controls: { name: value } }`, is checked against the declaration, and is
//!   what [`Turn::controls`](crate::Turn::controls) holds for the turn.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use uuid::Uuid;

/// The most options a choice may offer.
pub const MAX_CHOICE_OPTIONS: usize = 50;

/// The most bytes one display part may hold: an image or a clip, not a film.
pub const MAX_DISPLAY_BYTES: usize = 16 * 1024 * 1024;

/// The most rows one rows part may hold.
pub const MAX_DISPLAY_ROWS: usize = 1000;

/// One option of a choice or a select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    /// What the answer carries when this is chosen.
    pub value: String,
    /// What the person reads; the value when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// A line under the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ChoiceOption {
    /// An option whose label is its value.
    pub fn new(value: impl Into<String>) -> ChoiceOption {
        ChoiceOption {
            value: value.into(),
            label: String::new(),
            description: None,
        }
    }

    /// Set the label, returning `self` for chaining.
    pub fn label(mut self, label: impl Into<String>) -> ChoiceOption {
        self.label = label.into();
        self
    }

    /// Set the description, returning `self` for chaining.
    pub fn description(mut self, description: impl Into<String>) -> ChoiceOption {
        self.description = Some(description.into());
        self
    }

    /// The label, or the value when there is none.
    pub fn shown(&self) -> &str {
        if self.label.is_empty() {
            &self.value
        } else {
            &self.label
        }
    }
}

/// What kind of answer a request wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestKind {
    /// Pick one option, or several when `multiple`.
    Choice {
        /// The options, in order.
        options: Vec<ChoiceOption>,
        /// Whether several may be picked; the answer is then a list.
        #[serde(default)]
        multiple: bool,
    },
    /// Type an answer.
    Text {
        /// A hint shown in the empty input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    /// Approve (or decline) what the prompt shows — a plan.
    Approve {
        /// The approving button's label.
        label: String,
        /// What the trait gets back with the answer, unchanged.
        #[serde(default)]
        payload: Json,
    },
    /// Press a button.
    Button {
        /// The button's label.
        label: String,
        /// What the trait gets back with the answer, unchanged.
        #[serde(default)]
        payload: Json,
    },
}

/// A question a tool put to the person. The run's turn ends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRequest {
    /// Its identity: what an answer names.
    pub id: String,
    /// What the person reads: the question, or the plan to approve.
    pub prompt: String,
    /// What kind of answer it wants.
    #[serde(flatten)]
    pub kind: RequestKind,
    /// The tool call that asked, set by the loop.
    #[serde(default)]
    pub tool_call: String,
    /// The trait instance that asked (its state key), set by the loop: who is
    /// given the answer.
    #[serde(default)]
    pub trait_key: String,
}

impl UserRequest {
    /// A request asking `prompt`.
    pub fn new(prompt: impl Into<String>, kind: RequestKind) -> UserRequest {
        UserRequest {
            id: Uuid::new_v4().to_string(),
            prompt: prompt.into(),
            kind,
            tool_call: String::new(),
            trait_key: String::new(),
        }
    }

    /// Check what can be checked before the person sees it: a prompt, and a
    /// choice with options that are distinct.
    pub fn check(&self) -> Result<()> {
        if self.prompt.trim().is_empty() {
            return Err(Error::invalid("a question for the person needs a prompt"));
        }
        if let RequestKind::Choice { options, .. } = &self.kind {
            if options.is_empty() || options.len() > MAX_CHOICE_OPTIONS {
                return Err(Error::invalid(format!(
                    "a choice needs between 1 and {MAX_CHOICE_OPTIONS} options"
                )));
            }
            let mut seen = std::collections::BTreeSet::new();
            for option in options {
                if option.value.trim().is_empty() || !seen.insert(option.value.as_str()) {
                    return Err(Error::invalid(format!(
                        "a choice's options need distinct values; `{}` is blank or repeated",
                        option.value
                    )));
                }
            }
        }
        Ok(())
    }

    /// Check an answer against what was asked: one of the options (a list of
    /// them when several may be picked), text, or a yes or no.
    pub fn check_answer(&self, value: &Json) -> Result<()> {
        let refused = |what: &str| {
            Err(Error::invalid(format!(
                "the answer to \"{}\" should be {what}, got {value}",
                self.prompt
            )))
        };
        match &self.kind {
            RequestKind::Choice { options, multiple } => {
                let known = |v: &Json| {
                    v.as_str()
                        .is_some_and(|v| options.iter().any(|o| o.value == v))
                };
                match value {
                    Json::Array(values) if *multiple && values.iter().all(known) => Ok(()),
                    v if !*multiple && known(v) => Ok(()),
                    _ if *multiple => refused("a list of the offered options"),
                    _ => refused("one of the offered options"),
                }
            }
            RequestKind::Text { .. } => match value {
                Json::String(s) if !s.trim().is_empty() => Ok(()),
                _ => refused("some text"),
            },
            RequestKind::Approve { .. } => match value {
                Json::Bool(_) => Ok(()),
                _ => refused("true (approved) or false (declined)"),
            },
            RequestKind::Button { .. } => Ok(()),
        }
    }

    /// The text the model reads for `value` when the trait that asked says
    /// nothing of its own: the question and the answer, in words.
    pub fn answer_text(&self, value: &Json) -> String {
        let answer = match (&self.kind, value) {
            (RequestKind::Choice { options, .. }, Json::Array(values)) => values
                .iter()
                .filter_map(Json::as_str)
                .map(|v| label_of(options, v))
                .collect::<Vec<_>>()
                .join(", "),
            (RequestKind::Choice { options, .. }, Json::String(v)) => label_of(options, v),
            (RequestKind::Approve { .. }, Json::Bool(true)) => "Approved.".to_owned(),
            (RequestKind::Approve { .. }, Json::Bool(false)) => "Declined.".to_owned(),
            (RequestKind::Button { label, .. }, _) => format!("Pressed \"{label}\"."),
            (_, Json::String(text)) => text.clone(),
            (_, other) => other.to_string(),
        };
        format!("Your question: {}\nMy answer: {answer}", self.prompt.trim())
    }
}

fn label_of(options: &[ChoiceOption], value: &str) -> String {
    options
        .iter()
        .find(|o| o.value == value)
        .map_or_else(|| value.to_owned(), |o| o.shown().to_owned())
}

/// An answer to a request, as the next user message carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The request answered.
    pub answers_request: String,
    /// The value: an option's value (a list of them), text, or a yes or no.
    #[serde(default)]
    pub value: Json,
}

/// Something a tool shows the person and not the model.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayPart {
    /// Rows, as a table, or through a named view where the surface has one.
    Rows {
        /// A heading.
        title: Option<String>,
        /// The view to render them through, by name.
        view: Option<String>,
        /// The columns, in order.
        columns: Vec<String>,
        /// The rows, as objects.
        rows: Vec<Json>,
    },
    /// An image.
    Image {
        /// Its media type.
        mime_type: String,
        /// The bytes.
        data: Vec<u8>,
        /// A file name, for a download.
        name: Option<String>,
    },
    /// Audio.
    Audio {
        /// Its media type.
        mime_type: String,
        /// The bytes.
        data: Vec<u8>,
        /// A file name, for a download.
        name: Option<String>,
    },
}

impl DisplayPart {
    /// Its kind: `rows`, `image` or `audio`.
    pub fn kind(&self) -> &'static str {
        match self {
            DisplayPart::Rows { .. } => "rows",
            DisplayPart::Image { .. } => "image",
            DisplayPart::Audio { .. } => "audio",
        }
    }

    /// Check its size.
    pub fn check(&self) -> Result<()> {
        match self {
            DisplayPart::Rows { rows, .. } if rows.len() > MAX_DISPLAY_ROWS => {
                Err(Error::invalid(format!(
                    "a table shown to the person is at most {MAX_DISPLAY_ROWS} rows; this one \
                     has {}",
                    rows.len()
                )))
            }
            DisplayPart::Image { data, .. } | DisplayPart::Audio { data, .. }
                if data.len() > MAX_DISPLAY_BYTES =>
            {
                Err(Error::invalid(format!(
                    "a file shown to the person is at most {} MB",
                    MAX_DISPLAY_BYTES / (1024 * 1024)
                )))
            }
            _ => Ok(()),
        }
    }

    /// A one-line description for a reference: what a transcript says before
    /// the part is fetched.
    pub fn summary(&self) -> String {
        match self {
            DisplayPart::Rows { title, rows, .. } => match title {
                Some(title) => format!("{title} ({} rows)", rows.len()),
                None => format!("{} rows", rows.len()),
            },
            DisplayPart::Image { name, data, .. } | DisplayPart::Audio { name, data, .. } => {
                format!(
                    "{} ({} KB)",
                    name.as_deref().unwrap_or(self.kind()),
                    data.len().div_ceil(1024)
                )
            }
        }
    }

    /// Its media type: JSON for rows.
    pub fn mime_type(&self) -> &str {
        match self {
            DisplayPart::Rows { .. } => "application/json",
            DisplayPart::Image { mime_type, .. } | DisplayPart::Audio { mime_type, .. } => {
                mime_type
            }
        }
    }

    /// What is stored for it besides the bytes: the rows, or the file's name.
    pub(crate) fn stored_data(&self) -> Json {
        match self {
            DisplayPart::Rows {
                title,
                view,
                columns,
                rows,
            } => json!({ "title": title, "view": view, "columns": columns, "rows": rows }),
            DisplayPart::Image { name, .. } | DisplayPart::Audio { name, .. } => {
                json!({ "name": name })
            }
        }
    }

    /// The bytes, for an image or audio.
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        match self {
            DisplayPart::Rows { .. } => None,
            DisplayPart::Image { data, .. } | DisplayPart::Audio { data, .. } => Some(data),
        }
    }
}

/// What a run keeps of a display part: enough to say what it is and fetch it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRef {
    /// The stored part's id, in `_fd_run_parts`.
    pub id: Uuid,
    /// `rows`, `image` or `audio`.
    pub kind: String,
    /// Its media type.
    pub mime_type: String,
    /// What it is, in a line.
    pub summary: String,
}

/// A control a trait adds to the chat's input row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposerControl {
    /// What its value is sent under, in `controls`.
    pub name: String,
    /// What the person reads beside it.
    pub label: String,
    /// What kind of control.
    #[serde(flatten)]
    pub kind: ControlKind,
}

/// The kinds of composer control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlKind {
    /// A select. Its value is the chosen option's value.
    Select {
        /// The options.
        options: Vec<ChoiceOption>,
        /// The option chosen until the person picks another.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// A mode that is on or off; its value is `true` or `false`.
    Toggle {
        /// Whether it starts on.
        #[serde(default)]
        default: bool,
    },
    /// A button that sends a message at once — its label — with this control
    /// set to `true`.
    Button,
}

/// Check the controls a message carries against those `declared`: every name
/// declared, a select's value one of its options, a button's `true`.
pub fn check_controls(declared: &[ComposerControl], sent: &BTreeMap<String, Json>) -> Result<()> {
    for (name, value) in sent {
        let Some(control) = declared.iter().find(|c| &c.name == name) else {
            return Err(Error::invalid(format!(
                "this agent's chat has no control named `{name}`{}",
                if declared.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; it has {}",
                        declared
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            )));
        };
        let fine = match &control.kind {
            ControlKind::Select { options, .. } => value
                .as_str()
                .is_some_and(|v| options.iter().any(|o| o.value == v)),
            ControlKind::Toggle { .. } => value.is_boolean(),
            ControlKind::Button => value == &Json::Bool(true),
        };
        if !fine {
            return Err(Error::invalid(format!(
                "`{value}` is not a value control `{name}` takes"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(multiple: bool) -> UserRequest {
        UserRequest::new(
            "Which colour?",
            RequestKind::Choice {
                options: vec![
                    ChoiceOption::new("r").label("Red"),
                    ChoiceOption::new("g").label("Green"),
                ],
                multiple,
            },
        )
    }

    #[test]
    fn an_answer_is_checked_against_what_was_asked() {
        let one = choice(false);
        one.check().unwrap();
        one.check_answer(&json!("r")).unwrap();
        assert!(one.check_answer(&json!("blue")).is_err());
        assert!(one.check_answer(&json!(["r"])).is_err());
        let many = choice(true);
        many.check_answer(&json!(["r", "g"])).unwrap();
        assert!(many.check_answer(&json!("r")).is_err());

        let text = UserRequest::new("Name?", RequestKind::Text { placeholder: None });
        text.check_answer(&json!("Ada")).unwrap();
        assert!(text.check_answer(&json!("  ")).is_err());
        let approve = UserRequest::new(
            "The plan",
            RequestKind::Approve {
                label: "Approve".into(),
                payload: Json::Null,
            },
        );
        approve.check_answer(&json!(true)).unwrap();
        assert!(approve.check_answer(&json!("yes")).is_err());
    }

    #[test]
    fn the_model_reads_the_answer_by_its_label() {
        assert_eq!(
            choice(true).answer_text(&json!(["r", "g"])),
            "Your question: Which colour?\nMy answer: Red, Green"
        );
        let approve = UserRequest::new(
            "The plan",
            RequestKind::Approve {
                label: "Go".into(),
                payload: Json::Null,
            },
        );
        assert!(approve.answer_text(&json!(false)).ends_with("Declined."));
    }

    #[test]
    fn a_bad_choice_or_control_value_is_refused() {
        let blank = UserRequest::new(
            "Pick",
            RequestKind::Choice {
                options: vec![ChoiceOption::new("a"), ChoiceOption::new("a")],
                multiple: false,
            },
        );
        assert!(blank.check().is_err());

        let declared = vec![
            ComposerControl {
                name: "model".into(),
                label: "Model".into(),
                kind: ControlKind::Select {
                    options: vec![ChoiceOption::new("fast"), ChoiceOption::new("smart")],
                    default: Some("fast".into()),
                },
            },
            ComposerControl {
                name: "run".into(),
                label: "Run it".into(),
                kind: ControlKind::Button,
            },
        ];
        let sent = |v: Json| -> BTreeMap<String, Json> {
            v.as_object().unwrap().clone().into_iter().collect()
        };
        check_controls(&declared, &sent(json!({ "model": "smart", "run": true }))).unwrap();
        assert!(check_controls(&declared, &sent(json!({ "model": "huge" }))).is_err());
        let err = check_controls(&declared, &sent(json!({ "temperature": 1 })))
            .unwrap_err()
            .to_string();
        assert!(err.contains("model, run"), "{err}");
        // Serialised flat, as the chat surfaces send them.
        assert_eq!(
            serde_json::to_value(&declared[1]).unwrap(),
            json!({ "name": "run", "label": "Run it", "kind": "button" })
        );
    }
}
