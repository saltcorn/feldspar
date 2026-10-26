//! Request options a model row (and an agent) can set on every call:
//! [`ReasoningEffort`] and the `extra_params` passthrough.
//!
//! **Why on the model row.** How hard a model thinks, and which
//! vendor-specific fields its host wants, are facts about the model and the
//! host: an OpenAI-compatible host serving a reasoning model that thinks for
//! thirty seconds over "hello" needs `reasoning_effort: low` on *every* request,
//! whoever sends it — the agent loop, the test button, a single prompt. So the
//! row's settings are applied where the row becomes a callable model
//! ([`connect_model`](crate::connect_model)), by [`WithRequestDefaults`], and
//! nothing that builds an [`LlmRequest`] has to know about them. An agent's own
//! `reasoning_effort` is set on the request and wins over the row's.
//!
//! **How each wire says it** (`rig_bridge::additional_params`):
//!
//! - Chat Completions: `reasoning_effort`, as is.
//! - Responses: `reasoning.effort`.
//! - Anthropic: `output_config.effort`. Its levels start at `low` and current
//!   models cannot switch thinking off, so `none` and `minimal` send `low`.
//!
//! **`extra_params`** is a JSON object merged into the request body last, so it
//! can say what no setting here does (`top_p`, a host's own switches, a
//! vendor's `thinking` object). It is the admin's escape hatch and wins over
//! what this crate computed. On the Responses wire only the fields rig's
//! request type knows survive; Chat Completions and Anthropic pass any field.

use std::sync::Arc;

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::message::LlmRequest;
use crate::provider::{LlmProvider, LlmStream};

/// Model (and agent) setting: how much the model reasons before answering.
pub const CFG_REASONING_EFFORT: &str = "reasoning_effort";
/// Model setting: a JSON object merged into every request body.
pub const CFG_EXTRA_PARAMS: &str = "extra_params";

/// The request fields `extra_params` may not set: they are what the request
/// *is*, and an override of one would be a different request than the loop
/// built — the conversation, the tools, the model, whether it streams.
pub const RESERVED_EXTRA_PARAMS: [&str; 8] = [
    "model",
    "messages",
    "input",
    "system",
    "tools",
    "stream",
    "preamble",
    "instructions",
];

/// How much a model reasons before it answers.
///
/// The union of the vendors' levels; each wire maps it to what it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// No reasoning, where the model can switch it off.
    None,
    /// The least reasoning short of none.
    Minimal,
    /// Little reasoning: chat, lookups, simple tool calls.
    Low,
    /// The usual default.
    Medium,
    /// Thorough.
    High,
    /// More than high, where the model offers it.
    Xhigh,
    /// The most the model offers.
    Max,
}

impl ReasoningEffort {
    /// Every level, in order — the settings' options.
    pub const ALL: [ReasoningEffort; 7] = [
        ReasoningEffort::None,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Xhigh,
        ReasoningEffort::Max,
    ];

    /// The stored spelling, which is also what OpenAI's APIs take.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }

    /// The level a stored spelling names.
    pub fn parse(text: &str) -> Option<ReasoningEffort> {
        ReasoningEffort::ALL
            .into_iter()
            .find(|e| e.as_str() == text.trim())
    }

    /// The level from a setting's value: absent or blank is `None`, and a value
    /// that names no level is the error that says which ones do.
    pub fn from_setting(value: Option<&Json>, what: &str) -> Result<Option<ReasoningEffort>> {
        match value {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Json::String(s)) => ReasoningEffort::parse(s).map(Some).ok_or_else(|| {
                Error::invalid(format!(
                    "{what}: `{s}` is not a reasoning effort; use one of {}",
                    ReasoningEffort::ALL.map(ReasoningEffort::as_str).join(", ")
                ))
            }),
            Some(other) => Err(Error::invalid(format!(
                "{what}: a reasoning effort is text, not {other}"
            ))),
        }
    }

    /// The spelling Anthropic's `output_config.effort` takes.
    pub(crate) fn anthropic(self) -> &'static str {
        match self {
            ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
            other => other.as_str(),
        }
    }
}

/// `extra_params` from a setting's value: absent or blank is empty, and
/// anything but an object without reserved keys is refused.
///
/// A string is parsed as JSON, so a value typed into a text box and one sent
/// as an object are the same setting.
pub fn extra_params_from_setting(value: Option<&Json>, what: &str) -> Result<Map<String, Json>> {
    let parsed = match value {
        None | Some(Json::Null) => return Ok(Map::new()),
        Some(Json::String(s)) if s.trim().is_empty() => return Ok(Map::new()),
        Some(Json::String(s)) => serde_json::from_str::<Json>(s).map_err(|e| {
            Error::invalid(format!("{what}: `{CFG_EXTRA_PARAMS}` is not JSON: {e}"))
        })?,
        Some(other) => other.clone(),
    };
    let Json::Object(map) = parsed else {
        return Err(Error::invalid(format!(
            "{what}: `{CFG_EXTRA_PARAMS}` should be a JSON object of request fields"
        )));
    };
    if let Some(key) = map
        .keys()
        .find(|k| RESERVED_EXTRA_PARAMS.contains(&k.as_str()))
    {
        return Err(Error::invalid(format!(
            "{what}: `{CFG_EXTRA_PARAMS}` may not set `{key}`, which the request itself carries"
        )));
    }
    Ok(map)
}

/// What a model row adds to every request sent to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestDefaults {
    /// The row's reasoning effort, used when the request sets none.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The row's extra request fields; a request's own of the same name win.
    pub extra_params: Map<String, Json>,
}

impl RequestDefaults {
    /// The defaults a model row's settings declare. Validated on save, so a
    /// value that does not parse here is refused rather than dropped.
    pub fn from_config(config: &Attrs, what: &str) -> Result<RequestDefaults> {
        Ok(RequestDefaults {
            reasoning_effort: ReasoningEffort::from_setting(
                config.get(CFG_REASONING_EFFORT),
                what,
            )?,
            extra_params: extra_params_from_setting(config.get(CFG_EXTRA_PARAMS), what)?,
        })
    }

    /// Whether these change nothing.
    pub fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none() && self.extra_params.is_empty()
    }

    /// Fill `req`'s unset options from these.
    pub fn apply(&self, req: &mut LlmRequest) {
        if req.reasoning_effort.is_none() {
            req.reasoning_effort = self.reasoning_effort;
        }
        for (key, value) in &self.extra_params {
            req.extra_params
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
}

/// A provider whose every request gets a model row's [`RequestDefaults`].
pub struct WithRequestDefaults {
    inner: Arc<dyn LlmProvider>,
    defaults: RequestDefaults,
}

impl WithRequestDefaults {
    /// `inner`, with `defaults` applied to each request — or `inner` itself when
    /// there are none, so a row with no options costs nothing.
    pub fn wrap(inner: Arc<dyn LlmProvider>, defaults: RequestDefaults) -> Arc<dyn LlmProvider> {
        if defaults.is_empty() {
            inner
        } else {
            Arc::new(WithRequestDefaults { inner, defaults })
        }
    }
}

#[async_trait]
impl LlmProvider for WithRequestDefaults {
    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn stream(&self, mut req: LlmRequest) -> Result<LlmStream> {
        self.defaults.apply(&mut req);
        self.inner.stream(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_level_round_trips_and_an_unknown_one_names_the_choices() {
        for effort in ReasoningEffort::ALL {
            assert_eq!(ReasoningEffort::parse(effort.as_str()), Some(effort));
        }
        assert_eq!(
            ReasoningEffort::from_setting(Some(&json!(" ")), "m").unwrap(),
            None
        );
        let err = ReasoningEffort::from_setting(Some(&json!("tiny")), "model `m`")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`tiny`") && err.contains("none, minimal, low"),
            "{err}"
        );
        assert_eq!(ReasoningEffort::None.anthropic(), "low");
        assert_eq!(ReasoningEffort::Xhigh.anthropic(), "xhigh");
    }

    #[test]
    fn extra_params_are_an_object_without_the_requests_own_fields() {
        assert!(extra_params_from_setting(None, "m").unwrap().is_empty());
        let parsed = extra_params_from_setting(Some(&json!(r#"{"top_p": 0.9}"#)), "m").unwrap();
        assert_eq!(parsed["top_p"], json!(0.9));
        assert!(extra_params_from_setting(Some(&json!("[1]")), "m").is_err());
        assert!(extra_params_from_setting(Some(&json!("{nope")), "m").is_err());
        let err = extra_params_from_setting(Some(&json!({"messages": []})), "m")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`messages`"), "{err}");
    }

    #[test]
    fn the_request_wins_over_the_row() {
        let mut config = Attrs::new();
        config.insert(CFG_REASONING_EFFORT.into(), json!("low"));
        config.insert(CFG_EXTRA_PARAMS.into(), json!({"top_p": 0.5, "seed": 1}));
        let defaults = RequestDefaults::from_config(&config, "m").unwrap();

        let mut bare = LlmRequest::prompt("hi");
        defaults.apply(&mut bare);
        assert_eq!(bare.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(bare.extra_params["seed"], json!(1));

        let mut own = LlmRequest::prompt("hi");
        own.reasoning_effort = Some(ReasoningEffort::High);
        own.extra_params.insert("top_p".into(), json!(0.1));
        defaults.apply(&mut own);
        assert_eq!(own.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(own.extra_params["top_p"], json!(0.1));
        assert_eq!(own.extra_params["seed"], json!(1));
    }
}
