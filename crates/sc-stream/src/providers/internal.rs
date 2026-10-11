//! The built-in `internal` stream provider (TODO.md "Live updates" §2, L2.1).
//!
//! Every other provider observes something **outside** Saltcorn — a broker, a
//! module's polled feed. An `internal` stream's elements are Saltcorn's own:
//! the `publish` action in a trigger or a workflow step, `live.publish` in a
//! code body, and (from L4, if its settings allow) a page. It is what a toast
//! that says "your report is ready" and a progress bar that moves while a job
//! runs are made of: a stream, so it is configured, exposed, observed and
//! triggered on exactly as an MQTT stream is, with no second mechanism beside
//! it.
//!
//! ## Its topics are what make it useful
//!
//! `topics` decides how its elements are split ([`TopicSpec`]), and so who
//! receives each one:
//!
//! - **`single`** — one topic, under the stream's `min_role`: an announcement
//!   to everybody who may watch.
//! - **`user`** — a topic per user id. A subscriber gets exactly their own
//!   topic without naming it, so "the user who clicked the button" is told
//!   their job finished and nobody else is.
//! - **`row`** — a topic per row of `table`. Subscribing needs read access to
//!   that row; it is the shape a board's presence or a document's activity
//!   takes.
//!
//! ## Nothing is subscribed to, so there is nothing to keep running
//!
//! [`subscribe`](StreamProvider::subscribe) starts no flow: the sink it is
//! handed is held by the running stream for as long as the subscription
//! lives, and publishers deliver through it
//! ([`RunningStream::publish_value`](crate::RunningStream::publish_value)).
//! The subscription is an idle task, so the supervisor's "running", "stopped"
//! and "restarted" mean what they mean for every other stream — and publishing
//! to a disabled stream is refused rather than lost.
//!
//! ## Retaining the last element
//!
//! With `retain_last` on, the latest element per topic is kept for
//! `retain_ttl_s` (an hour by default) and handed to a new subscriber before
//! anything else — MQTT's retained message ([`Replay::Latest`]). With it off a
//! new subscriber is handed nothing: a toast from a minute ago is not news.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sc_auth::{COL_PASSWORD_HASH, USERS_TABLE};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::element::{ElementField, ElementType};
use crate::provider::{Replay, StreamProvider, StreamSink, TopicSpec};
use crate::subscription::Subscription;

/// The name this provider is registered and stored under.
pub const INTERNAL: &str = "internal";

/// The declared keys of an element — MQTT's repeating group, reused.
pub const CFG_KEYS: &str = "keys";
/// How elements are split into topics: `single`, `user` or `row`.
pub const CFG_TOPICS: &str = "topics";
/// The table whose rows a `row` stream's topics are.
pub const CFG_TABLE: &str = "table";
/// Whether the latest element per topic is kept for a new subscriber.
pub const CFG_RETAIN_LAST: &str = "retain_last";
/// How long a retained element stays worth sending, in seconds.
pub const CFG_RETAIN_TTL: &str = "retain_ttl_s";
/// Whether a page may publish (L4), under §3 rule 7.
pub const CFG_CLIENT_PUBLISH: &str = "client_publish";
/// Whether a topic's members can see who else is subscribed (L4).
pub const CFG_PRESENCE: &str = "presence";
/// The columns of `users` other members are shown (L4).
pub const CFG_PRESENCE_FIELDS: &str = "presence_fields";

/// One topic, under the stream's `min_role`.
pub const TOPICS_SINGLE: &str = "single";
/// A topic per user.
pub const TOPICS_USER: &str = "user";
/// A topic per row of a table.
pub const TOPICS_ROW: &str = "row";

/// How long a retained element is kept by default: long enough that a page
/// reloaded during any reasonable job finds its progress, short enough that
/// yesterday's is gone.
pub const DEFAULT_RETAIN_TTL_S: u64 = 3600;
/// The longest a retained element may be kept: a week. Past that it is not
/// "the latest progress" of anything, and an inbox that remembers is a table
/// (TODO.md "Live updates" §7).
pub const MAX_RETAIN_TTL_S: u64 = 7 * 24 * 3600;
/// How many topics' latest elements one stream keeps. A per-user stream on a
/// large site has a topic per user who has ever had a job run; past this the
/// one published to longest ago is forgotten first.
pub const RETAIN_MAX_TOPICS: usize = 10_000;

/// The `internal` provider. Holds nothing: every stream's state is its
/// running stream's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Internal;

#[async_trait]
impl StreamProvider for Internal {
    fn name(&self) -> &str {
        INTERNAL
    }

    fn label(&self) -> &str {
        "Internal"
    }

    fn description(&self) -> &str {
        "Elements published by Saltcorn itself: the publish action, live.publish in a code \
         body — toasts, progress and notifications for an application's pages"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            // The same JSON list MQTT's json payload declares, so the Streams
            // form and the generated client read both the same way.
            FormField::new(CFG_KEYS, BasicType::Json)
                .label("Declared keys of an element")
                .default_value(Json::Array(Vec::new()))
                .required(),
            FormField::new(CFG_TOPICS, BasicType::Text)
                .label("Topics")
                .options([TOPICS_SINGLE, TOPICS_USER, TOPICS_ROW])
                .default_value(TOPICS_SINGLE)
                .required(),
            // Only a row stream has a table; required then, and checked against
            // the schema on save (`validate_in`).
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table whose rows are the topics")
                .show_if(CFG_TOPICS, vec![json!(TOPICS_ROW)]),
            FormField::new(CFG_RETAIN_LAST, BasicType::Bool)
                .label("Keep the latest element per topic for a new subscriber")
                .default_value(false),
            FormField::new(CFG_RETAIN_TTL, BasicType::Int)
                .label("Keep it for (seconds)")
                .default_value(DEFAULT_RETAIN_TTL_S)
                .show_if(CFG_RETAIN_LAST, vec![json!(true)]),
            FormField::new(CFG_CLIENT_PUBLISH, BasicType::Bool)
                .label("Pages may publish")
                .default_value(false),
            FormField::new(CFG_PRESENCE, BasicType::Bool)
                .label("Members can see who else is here")
                .default_value(false),
            FormField::new(CFG_PRESENCE_FIELDS, BasicType::Json)
                .label("User fields shown to other members")
                .default_value(Json::Array(Vec::new()))
                .show_if(CFG_PRESENCE, vec![json!(true)]),
        ]
    }

    fn element_type(&self, config: &Attrs) -> Result<ElementType> {
        Ok(ElementType::Json {
            keys: declared_keys(config)?,
        })
    }

    fn topic_spec(&self, config: &Attrs) -> Result<TopicSpec> {
        match topics(config)? {
            TOPICS_USER => Ok(TopicSpec::User),
            TOPICS_ROW => Ok(TopicSpec::Row {
                table: table(config)?,
            }),
            _ => Ok(TopicSpec::Single),
        }
    }

    fn replay(&self, config: &Attrs) -> Result<Replay> {
        if !flag(config, CFG_RETAIN_LAST) {
            return Ok(Replay::Nothing);
        }
        Ok(Replay::Latest {
            ttl: Duration::from_secs(retain_ttl(config)?),
            max_topics: RETAIN_MAX_TOPICS,
        })
    }

    fn accepts_publish(&self) -> bool {
        true
    }

    fn validate(&self, config: &Attrs) -> Result<()> {
        self.element_type(config)?.validate()?;
        let spec = self.topic_spec(config)?;
        self.replay(config)?;
        if flag(config, CFG_CLIENT_PUBLISH) && spec == TopicSpec::User {
            // §3 rule 7: a user topic never accepts a page's publish — a page
            // naming somebody else's id would be a toast in their name.
            return Err(Error::invalid(format!(
                "`{CFG_CLIENT_PUBLISH}`: a stream with a topic per user cannot take publishes \
                 from pages; only the server publishes to a user"
            )));
        }
        let _ = presence_fields(config)?;
        Ok(())
    }

    fn validate_in(&self, config: &Attrs, catalog: &Catalog) -> Result<()> {
        if let TopicSpec::Row { table } = self.topic_spec(config)? {
            let Some(found) = catalog.get(&table)? else {
                return Err(Error::invalid(format!(
                    "`{CFG_TABLE}`: there is no table `{table}`"
                )));
            };
            // A topic is one row's key, as a string. A composite key has no
            // one string to be, and a table with no key has no row to name.
            if found.primary_key.len() != 1 {
                return Err(Error::invalid(format!(
                    "`{CFG_TABLE}`: `{table}` has {}, and a row topic is named by a \
                     single-column primary key",
                    match found.primary_key.len() {
                        0 => "no primary key".to_owned(),
                        n => format!("a primary key of {n} columns"),
                    }
                )));
            }
        }
        let fields = presence_fields(config)?;
        if !fields.is_empty() {
            let users = catalog.get(USERS_TABLE)?.ok_or_else(|| {
                Error::invalid(format!(
                    "`{CFG_PRESENCE_FIELDS}`: there is no `{USERS_TABLE}` table to show fields of"
                ))
            })?;
            for field in &fields {
                if field == COL_PASSWORD_HASH {
                    return Err(Error::invalid(format!(
                        "`{CFG_PRESENCE_FIELDS}`: `{COL_PASSWORD_HASH}` is never shown to other \
                         users"
                    )));
                }
                if !users.fields.iter().any(|f| f.base.name == *field) {
                    return Err(Error::invalid(format!(
                        "`{CFG_PRESENCE_FIELDS}`: `{field}` is not a field of `{USERS_TABLE}`"
                    )));
                }
            }
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        _stream: &str,
        config: &Attrs,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<Subscription> {
        // The configuration is re-checked on the way in, as MQTT's is: a row
        // saved before a rule tightened must not start as if it were still
        // valid.
        self.validate(config)?;
        // Idle until dropped. The sink is held by the running stream, which
        // is what publishers deliver through.
        Ok(Subscription::spawn(|mut stop| async move {
            stop.stopped().await;
        }))
    }
}

/// The declared keys — an absent or empty list is `ElementType::validate`'s
/// refusal ("declare at least one key"), not this one's.
fn declared_keys(config: &Attrs) -> Result<Vec<ElementField>> {
    match config.get(CFG_KEYS) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value::<Vec<ElementField>>(value.clone()).map_err(|e| {
            Error::invalid(format!(
                "`{CFG_KEYS}`: each declared key is an object with a `name` and a `type` (one of \
                 `bool`, `int`, `float`, `decimal`, `text`, `json`, `uuid`, `date`, `time`, \
                 `timestamp`), and optionally `required`; {e}"
            ))
        }),
    }
}

/// The `topics` setting, defaulted to `single`.
fn topics(config: &Attrs) -> Result<&'static str> {
    match text(config, CFG_TOPICS)
        .unwrap_or_else(|| TOPICS_SINGLE.to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        TOPICS_SINGLE => Ok(TOPICS_SINGLE),
        TOPICS_USER => Ok(TOPICS_USER),
        TOPICS_ROW => Ok(TOPICS_ROW),
        other => Err(Error::invalid(format!(
            "`{CFG_TOPICS}`: `{other}` is not a kind of topic; it is one of `{TOPICS_SINGLE}`, \
             `{TOPICS_USER}` or `{TOPICS_ROW}`"
        ))),
    }
}

/// The table of a `row` stream, which it cannot be without.
fn table(config: &Attrs) -> Result<String> {
    text(config, CFG_TABLE).ok_or_else(|| {
        Error::invalid(format!(
            "`{CFG_TABLE}`: a stream with a topic per row needs the table the rows are of"
        ))
    })
}

/// How long a retained element is kept, bounded.
fn retain_ttl(config: &Attrs) -> Result<u64> {
    let seconds = match config.get(CFG_RETAIN_TTL) {
        None | Some(Json::Null) => return Ok(DEFAULT_RETAIN_TTL_S),
        Some(value) => value.as_u64().ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_RETAIN_TTL}` must be a whole number of seconds, got {value}"
            ))
        })?,
    };
    if !(1..=MAX_RETAIN_TTL_S).contains(&seconds) {
        return Err(Error::invalid(format!(
            "`{CFG_RETAIN_TTL}` must be between 1 and {MAX_RETAIN_TTL_S} seconds (a week), got \
             {seconds}"
        )));
    }
    Ok(seconds)
}

/// Whether a page may publish to a stream with this configuration (§3 rule
/// 7) — the setting alone; who may is the live socket's to decide.
pub fn client_publish(config: &Attrs) -> bool {
    flag(config, CFG_CLIENT_PUBLISH)
}

/// Whether a topic's members can see who else is subscribed (L4.3).
pub fn presence(config: &Attrs) -> bool {
    flag(config, CFG_PRESENCE)
}

/// The presence fields, as a list of column names.
pub fn presence_fields(config: &Attrs) -> Result<Vec<String>> {
    match config.get(CFG_PRESENCE_FIELDS) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value::<Vec<String>>(value.clone()).map_err(|_| {
            Error::invalid(format!(
                "`{CFG_PRESENCE_FIELDS}` is a list of field names of `{USERS_TABLE}`, such as \
                 [\"name\"]; got {value}"
            ))
        }),
    }
}

/// A non-blank text setting.
fn text(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A boolean setting, off when absent.
fn flag(config: &Attrs, key: &str) -> bool {
    config.get(key).and_then(Json::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: Json) -> Attrs {
        pairs.as_object().cloned().unwrap_or_default()
    }

    fn job_status() -> Attrs {
        config(json!({
            "keys": [
                { "name": "message", "type": "text", "required": true },
                { "name": "progress", "type": "float" }
            ],
            "topics": "user",
            "retain_last": true
        }))
    }

    #[test]
    fn the_declared_settings_are_the_ones_the_design_names() {
        let names: Vec<String> = Internal
            .config_spec()
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "keys",
                "topics",
                "table",
                "retain_last",
                "retain_ttl_s",
                "client_publish",
                "presence",
                "presence_fields"
            ]
        );
        // `table` shows only for a row stream: the conditional the form renders.
        let spec = Internal.config_spec();
        let table = spec.iter().find(|f| f.name() == CFG_TABLE).unwrap();
        assert!(!table.applies(&spec, &config(json!({ "topics": "user" }))));
        assert!(table.applies(&spec, &config(json!({ "topics": "row" }))));
        assert!(Internal.accepts_publish());
    }

    #[test]
    fn the_element_type_and_the_topics_are_functions_of_the_configuration() {
        let job = job_status();
        assert_eq!(
            Internal.element_type(&job).unwrap(),
            ElementType::json([
                ElementField::new("message", BasicType::Text).required(),
                ElementField::new("progress", BasicType::Float),
            ])
        );
        assert_eq!(Internal.topic_spec(&job).unwrap(), TopicSpec::User);
        assert_eq!(
            Internal.replay(&job).unwrap(),
            Replay::Latest {
                ttl: Duration::from_secs(DEFAULT_RETAIN_TTL_S),
                max_topics: RETAIN_MAX_TOPICS
            }
        );

        let mut single = job.clone();
        single.insert(CFG_TOPICS.into(), json!("single"));
        single.insert(CFG_RETAIN_LAST.into(), json!(false));
        assert_eq!(Internal.topic_spec(&single).unwrap(), TopicSpec::Single);
        assert_eq!(Internal.replay(&single).unwrap(), Replay::Nothing);

        let mut row = job.clone();
        row.insert(CFG_TOPICS.into(), json!("row"));
        row.insert(CFG_TABLE.into(), json!("boards"));
        assert_eq!(
            Internal.topic_spec(&row).unwrap(),
            TopicSpec::Row {
                table: "boards".into()
            }
        );
    }

    #[test]
    fn each_refusal_names_its_setting() {
        let cases: Vec<(Json, &str)> = vec![
            (
                json!({ "keys": [], "topics": "single" }),
                "at least one key",
            ),
            (
                json!({ "keys": [{ "name": "m", "type": "text" }], "topics": "everyone" }),
                CFG_TOPICS,
            ),
            (
                json!({ "keys": [{ "name": "m", "type": "text" }], "topics": "row" }),
                CFG_TABLE,
            ),
            (
                json!({ "keys": [{ "name": "m", "type": "text" }], "retain_last": true,
                        "retain_ttl_s": 0 }),
                CFG_RETAIN_TTL,
            ),
            (
                json!({ "keys": [{ "name": "m", "type": "text" }], "topics": "user",
                        "client_publish": true }),
                CFG_CLIENT_PUBLISH,
            ),
            (
                json!({ "keys": [{ "name": "m", "type": "text" }], "presence_fields": "name" }),
                CFG_PRESENCE_FIELDS,
            ),
        ];
        for (raw, needle) in cases {
            let error = Internal.validate(&config(raw.clone())).unwrap_err();
            assert!(error.to_string().contains(needle), "{raw}: {error}");
        }
        // A TTL out of range is not a refusal while retaining is off: the
        // setting is hidden, and kept for when it is switched back on.
        Internal
            .validate(&config(json!({
                "keys": [{ "name": "m", "type": "text" }],
                "retain_ttl_s": 0
            })))
            .unwrap();
        Internal.validate(&job_status()).unwrap();
    }

    #[test]
    fn client_publishing_and_presence_are_off_unless_switched_on() {
        let job = job_status();
        assert!(!client_publish(&job));
        assert!(!presence(&job));
        let board = config(json!({
            "keys": [{ "name": "dragging", "type": "int" }],
            "topics": "row",
            "table": "boards",
            "client_publish": true,
            "presence": true,
            "presence_fields": ["name"]
        }));
        assert!(client_publish(&board));
        assert!(presence(&board));
        assert_eq!(presence_fields(&board).unwrap(), vec!["name".to_owned()]);
        Internal.validate(&board).unwrap();
    }
}
