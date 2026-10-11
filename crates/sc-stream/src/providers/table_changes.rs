//! The built-in `table_changes` stream provider (TODO.md "Live updates" §2,
//! L3.3; L3.4 completes it).
//!
//! One element per committed insert, update or delete on a table. Its elements
//! are made by **writes**, not by a publisher: the live hub is a listener on
//! the catalog's write seam, and while somebody is subscribed to a stream of
//! this provider it hands each committed write on the stream's table to the
//! stream ([`RunningStream::publish_change`](crate::RunningStream::publish_change)).
//! Nothing else may put an element in — not the `publish` action, not a code
//! body, not a page — because an element here *is* the claim that a row
//! changed.
//!
//! ## The element is the change, not what a subscriber is sent
//!
//! An element carries `{op, key, old, new}`: both images of the row, as the
//! write saw them. Who may see which of them, and in what shape (`update`,
//! `enter`, `leave`), is decided per subscriber by the hub's fan-out, off the
//! write's path (§3 rule 5). So a new subscriber is replayed nothing
//! ([`Replay::Nothing`]): a ring of raw rows handed to whoever subscribes next
//! would skip exactly that decision.
//!
//! ## Its topics
//!
//! [`TopicSpec::PerElementRow`]: every element is checked against the
//! subscriber's read access to the row it carries.

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::{Catalog, WriteOp};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::element::{ElementField, ElementType};
use crate::provider::{Replay, StreamProvider, StreamSink, TopicSpec};
use crate::subscription::Subscription;

/// The name this provider is registered and stored under.
pub const TABLE_CHANGES: &str = "table_changes";

/// The table whose writes the stream carries.
pub const CFG_TABLE: &str = "table";
/// Which writes: any of `insert`, `update`, `delete`.
pub const CFG_OPS: &str = "ops";

/// The keys of an element.
pub const KEY_OP: &str = "op";
/// The written row's primary key, as a topic spells it (`null` for a table
/// without a one-column key).
pub const KEY_KEY: &str = "key";
/// The row before the write — `null` for an insert.
pub const KEY_OLD: &str = "old";
/// The row after the write — `null` for a delete.
pub const KEY_NEW: &str = "new";

const ALL_OPS: [WriteOp; 3] = [WriteOp::Insert, WriteOp::Update, WriteOp::Delete];

/// What a `table_changes` stream is configured to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSettings {
    /// The table.
    pub table: String,
    /// The writes, in insert/update/delete order, each at most once.
    pub ops: Vec<WriteOp>,
}

impl ChangeSettings {
    /// Read and check a stream's configuration.
    pub fn from_config(config: &Attrs) -> Result<ChangeSettings> {
        let table = config
            .get(CFG_TABLE)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{CFG_TABLE}`: a table-changes stream needs the table whose writes it carries"
                ))
            })?
            .to_owned();
        let ops = match config.get(CFG_OPS) {
            None | Some(Json::Null) => ALL_OPS.to_vec(),
            Some(value) => {
                let names = serde_json::from_value::<Vec<String>>(value.clone()).map_err(|_| {
                    Error::invalid(format!(
                        "`{CFG_OPS}` is a list of the writes to carry, any of \"insert\", \
                         \"update\" and \"delete\"; got {value}"
                    ))
                })?;
                let mut ops = Vec::new();
                for name in &names {
                    let op = ALL_OPS
                        .into_iter()
                        .find(|op| op.as_str() == name.trim().to_ascii_lowercase())
                        .ok_or_else(|| {
                            Error::invalid(format!(
                                "`{CFG_OPS}`: `{name}` is not a write; it is one of `insert`, \
                                 `update` or `delete`"
                            ))
                        })?;
                    ops.push(op);
                }
                // Insert/update/delete order, each once, whatever the list said.
                ALL_OPS.into_iter().filter(|op| ops.contains(op)).collect()
            }
        };
        if ops.is_empty() {
            return Err(Error::invalid(format!(
                "`{CFG_OPS}`: choose at least one of `insert`, `update` and `delete`, or the \
                 stream carries nothing"
            )));
        }
        Ok(ChangeSettings { table, ops })
    }

    /// Whether the stream carries `op`.
    pub fn carries(&self, op: WriteOp) -> bool {
        self.ops.contains(&op)
    }
}

/// The `table_changes` provider. Holds nothing: the hub hands each write to
/// the running stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct TableChanges;

#[async_trait]
impl StreamProvider for TableChanges {
    fn name(&self) -> &str {
        TABLE_CHANGES
    }

    fn label(&self) -> &str {
        "Table changes"
    }

    fn description(&self) -> &str {
        "One element per committed insert, update or delete on a table, each checked against \
         the subscriber's read access to the row"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_OPS, BasicType::Json)
                .label("Writes to carry")
                .default_value(Json::from(
                    ALL_OPS.iter().map(|op| op.as_str()).collect::<Vec<_>>(),
                )),
        ]
    }

    fn element_type(&self, config: &Attrs) -> Result<ElementType> {
        ChangeSettings::from_config(config)?;
        Ok(ElementType::json([
            ElementField::new(KEY_OP, BasicType::Text).required(),
            ElementField::new(KEY_KEY, BasicType::Json),
            ElementField::new(KEY_OLD, BasicType::Json),
            ElementField::new(KEY_NEW, BasicType::Json),
        ]))
    }

    fn topic_spec(&self, config: &Attrs) -> Result<TopicSpec> {
        Ok(TopicSpec::PerElementRow {
            table: ChangeSettings::from_config(config)?.table,
        })
    }

    fn replay(&self, _config: &Attrs) -> Result<Replay> {
        Ok(Replay::Nothing)
    }

    fn validate(&self, config: &Attrs) -> Result<()> {
        ChangeSettings::from_config(config).map(|_| ())
    }

    fn validate_in(&self, config: &Attrs, catalog: &Catalog) -> Result<()> {
        let settings = ChangeSettings::from_config(config)?;
        if catalog.get(&settings.table)?.is_none() {
            return Err(Error::invalid(format!(
                "`{CFG_TABLE}`: there is no table `{}`",
                settings.table
            )));
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        _stream: &str,
        config: &Attrs,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<Subscription> {
        self.validate(config)?;
        // Idle until dropped, as `internal`'s is: the sink is held by the
        // running stream, which the hub hands each committed write to.
        Ok(Subscription::spawn(|mut stop| async move {
            stop.stopped().await;
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(pairs: Json) -> Attrs {
        pairs.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn the_settings_default_to_every_write_and_keep_their_order() {
        let all = ChangeSettings::from_config(&config(json!({ "table": "cards" }))).unwrap();
        assert_eq!(all.table, "cards");
        assert_eq!(all.ops, ALL_OPS.to_vec());

        let some = ChangeSettings::from_config(&config(
            json!({ "table": "cards", "ops": ["Delete", "insert", "delete"] }),
        ))
        .unwrap();
        assert_eq!(some.ops, vec![WriteOp::Insert, WriteOp::Delete]);
        assert!(!some.carries(WriteOp::Update));
    }

    #[test]
    fn a_stream_with_no_table_or_no_writes_or_an_unknown_write_is_refused() {
        for (bad, says) in [
            (json!({}), "needs the table"),
            (json!({ "table": "cards", "ops": [] }), "at least one"),
            (json!({ "table": "cards", "ops": ["upsert"] }), "`upsert`"),
            (json!({ "table": "cards", "ops": "insert" }), "is a list"),
        ] {
            let err = TableChanges.validate(&config(bad.clone())).unwrap_err();
            assert!(err.to_string().contains(says), "{bad}: {err}");
        }
    }

    #[test]
    fn its_topics_are_per_element_and_it_replays_nothing_and_takes_no_publish() {
        let cards = config(json!({ "table": "cards" }));
        assert_eq!(
            TableChanges.topic_spec(&cards).unwrap(),
            TopicSpec::PerElementRow {
                table: "cards".into()
            }
        );
        assert_eq!(TableChanges.replay(&cards).unwrap(), Replay::Nothing);
        assert!(!TableChanges.accepts_publish());
        TableChanges
            .element_type(&cards)
            .unwrap()
            .validate()
            .unwrap();
    }
}
