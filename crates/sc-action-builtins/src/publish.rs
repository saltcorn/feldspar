//! `publish` — put one element on an `internal` stream (TODO.md "Live updates"
//! §7, L2.7).
//!
//! The server-side half of a toast and a progress bar. A trigger body or a
//! workflow `Action` step names an internal stream, a **topic** and a
//! **value**, each part a formula over the event, and the element goes out to
//! everybody subscribed to that topic — through the stream, so the same
//! element also reaches the admin's Observe screen and any trigger listening
//! to the stream.
//!
//! ```text
//! stream: job_status
//! topic:  user.id                       (the default for a `user` stream)
//! value:  { message: '"Report started"', progress: "0" }
//! ```
//!
//! ## "The user who clicked the button"
//!
//! For a stream with a topic per user the topic defaults to `user.id` — the
//! event's caller. A workflow run carries the user who started it, so a
//! `publish` step three steps and ten minutes after the click still tells the
//! person who clicked, and nobody else. An event with **no** user — a periodic
//! trigger, a stream event — has nobody to tell, and that is an error naming
//! the trigger rather than a message dropped on the floor.
//!
//! ## Trusted, as every action is
//!
//! An action is admin-authored configuration (§3 rule 8): it publishes to any
//! topic, as an `insert_row` writes any row. Every check made here is about
//! the **shape** of the publish — the stream exists, is one Saltcorn publishes
//! to, the topic is its kind and the value matches its element type — and the
//! live hub makes those checks again where the element lands.

use sc_action::{
    Action, ActionContext, ConfigCheck, EVENT_SCOPE, check_formula, config_str,
    event_formula_value, formula_map, optional_formula,
};
use sc_error::{Error, Result};
use sc_expr::{Formula, LiveHost, LiveTopics};
use sc_types::{BasicType, FormField};
use serde_json::{Map, Value as Json, json};

/// The stream to publish to.
pub(crate) const CFG_STREAM: &str = "stream";
/// The topic, a formula; blank is the stream's default.
pub(crate) const CFG_TOPIC: &str = "topic";
/// The element, as a map of key → formula.
pub(crate) const CFG_VALUE: &str = "value";

/// The options query the `stream` picker declares: the internal streams this
/// server has. Resolved where the declaration is served (`sc-server`'s
/// `listActions`), because streams are rows and a `config_spec` is synchronous.
pub const INTERNAL_STREAMS_QUERY: &str = "internal_streams";

/// The formula a `user` stream's topic defaults to: the event's caller.
const DEFAULT_USER_TOPIC: &str = "user.id";

/// Publish one element on an internal stream.
pub struct Publish;

#[async_trait::async_trait]
impl Action for Publish {
    fn name(&self) -> &str {
        "publish"
    }

    fn description(&self) -> &str {
        "Publish an element on an internal stream — a toast, a progress update — to the pages \
         subscribed to its topic"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_STREAM, BasicType::Text)
                .label("Stream")
                .server_query(INTERNAL_STREAMS_QUERY)
                .required(),
            FormField::new(CFG_TOPIC, BasicType::Text)
                .label("Topic (a formula; blank is the user's own for a per-user stream)"),
            FormField::new(CFG_VALUE, BasicType::Json)
                .label("Element values")
                .required(),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let stream = config_str(check.config, CFG_STREAM)?;
        // The stream, when this process can say: a server always can, and a
        // process with no live hub (a build tool) checks the formulas alone
        // rather than refusing a configuration it cannot judge.
        if let Some(host) = check.catalog.live_host() {
            let topics = host
                .topics(&stream)
                .map_err(|e| Error::invalid(format!("`{CFG_STREAM}`: {e}")))?;
            if topics == LiveTopics::Single && optional_formula(check.config, CFG_TOPIC)?.is_some()
            {
                return Err(Error::invalid(format!(
                    "`{CFG_TOPIC}`: stream `{stream}` has no topics, so the topic must be blank"
                )));
            }
            if matches!(topics, LiveTopics::Row { .. })
                && optional_formula(check.config, CFG_TOPIC)?.is_none()
            {
                return Err(Error::invalid(format!(
                    "`{CFG_TOPIC}`: stream `{stream}` has a topic per row, so the topic is the \
                     formula for the row's key"
                )));
            }
        }
        if let Some(topic) = optional_formula(check.config, CFG_TOPIC)? {
            check_formula(check.shape, EVENT_SCOPE, &topic, &format!("`{CFG_TOPIC}`"))?;
        }
        for (key, formula) in formula_map(check.config, CFG_VALUE)? {
            check_formula(check.shape, EVENT_SCOPE, &formula, &format!("`{key}`"))?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let stream = config_str(ctx.config, CFG_STREAM)?;
        let host = ctx.catalog.live_host().ok_or_else(|| {
            Error::config(format!(
                "trigger `{}`: this server has no live updates, so there is nothing to publish to",
                ctx.trigger
            ))
        })?;
        let topic = self.topic(ctx, host.as_ref(), &stream).await?;
        let mut value = Map::new();
        for (key, formula) in formula_map(ctx.config, CFG_VALUE)? {
            let evaluated = event_formula_value(ctx, &formula, &format!("`{key}`")).await?;
            value.insert(key, evaluated);
        }
        let value = Json::Object(value);
        let published = host
            .publish(&stream, topic, value.clone())
            .map_err(|e| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger)))?;
        Ok(json!({ "stream": stream, "topic": published, "value": value }))
    }
}

impl Publish {
    /// The topic this firing publishes on: the configured formula's value, or
    /// the stream's default — the event's user for a `user` stream, none for a
    /// `single` one.
    async fn topic(
        &self,
        ctx: &ActionContext<'_>,
        host: &dyn LiveHost,
        stream: &str,
    ) -> Result<Option<String>> {
        let topics = host
            .topics(stream)
            .map_err(|e| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger)))?;
        let formula = match optional_formula(ctx.config, CFG_TOPIC)? {
            Some(formula) => formula,
            None => match topics {
                LiveTopics::Single => return Ok(None),
                LiveTopics::User => {
                    Formula::parse(DEFAULT_USER_TOPIC).map_err(|e| Error::msg(e.to_string()))?
                }
                LiveTopics::Row { .. } => {
                    return Err(Error::invalid(format!(
                        "trigger `{}`: stream `{stream}` has a topic per row, and no topic \
                         formula names the row",
                        ctx.trigger
                    )));
                }
            },
        };
        let value = event_formula_value(ctx, &formula, &format!("`{CFG_TOPIC}`")).await?;
        match value {
            Json::Null if topics == LiveTopics::User => Err(Error::invalid(format!(
                "trigger `{}`: stream `{stream}` has a topic per user, and this event has no \
                 user to publish to — a periodic or stream trigger runs as nobody",
                ctx.trigger
            ))),
            Json::Null => Ok(None),
            Json::String(text) => Ok(Some(text)),
            Json::Number(n) => Ok(Some(n.to_string())),
            other => Err(Error::invalid(format!(
                "trigger `{}`: `{CFG_TOPIC}` must be text or a number, got {other}",
                ctx.trigger
            ))),
        }
    }
}
