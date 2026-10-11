//! The built-in `table_changes` stream provider (TODO.md "Live updates" §2,
//! L3.3 and L3.4).
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
//! An element on the stream carries `{op, key, row, old}`: the row as the
//! write left it (`row`, null for a delete) and as it was before (`old`, for
//! an update or a delete). `old` is **not declared** in the element type — a
//! declared shape is a floor, and an undeclared key is carried (see
//! [`ElementType`]) — because no page is ever sent it: it is what the hub's
//! fan-out needs to decide, per subscriber, whether the change is an
//! `update`, an `enter`, a `leave` or nothing (§3 rule 5). What a subscriber
//! receives is `{op, key, row?}`, which is exactly the declared type. So a new
//! subscriber is replayed nothing ([`Replay::Nothing`]): a ring of raw rows
//! handed to whoever subscribes next would skip that decision.
//!
//! ## The element type is the table's
//!
//! [`element_type`](StreamProvider::element_type) can only say `row` is JSON:
//! it sees the configuration, which names the table. Asked with the catalog
//! ([`element_type_in`](StreamProvider::element_type_in)) it declares `row`'s
//! keys — one per column an application's REST read returns, so the users
//! table's password hash is not among them — and that is what a generated
//! client is typed from. A column added to the table moves the type at the next
//! client emit, which the schema observer triggers for every app that includes
//! the table. With `payload: key` there is no `row` at all: a page is told
//! `{op, key}` and fetches the row itself, for a wide row or a joined read.
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
/// What a page is sent: `row` (the default) or `key`.
pub const CFG_PAYLOAD: &str = "payload";

/// The keys of an element.
pub const KEY_OP: &str = "op";
/// The written row's primary key, as the row holds it.
pub const KEY_KEY: &str = "key";
/// The row after the write — `null` for a delete. What a page is sent, in the
/// REST read's projection, for an `insert`, an `update` and an `enter`.
pub const KEY_ROW: &str = "row";
/// The row before the write — absent for an insert. Carried on the stream for
/// the fan-out's access decision and never sent to a page.
pub const KEY_OLD: &str = "old";

const ALL_OPS: [WriteOp; 3] = [WriteOp::Insert, WriteOp::Update, WriteOp::Delete];

/// What a page is sent of a changed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Payload {
    /// The row, in the REST read's projection.
    #[default]
    Row,
    /// Only `{op, key}`; the page fetches the row (a wide row, a joined read).
    Key,
}

impl Payload {
    /// Its stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Payload::Row => "row",
            Payload::Key => "key",
        }
    }
}

/// What a `table_changes` stream is configured to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSettings {
    /// The table.
    pub table: String,
    /// The writes, in insert/update/delete order, each at most once.
    pub ops: Vec<WriteOp>,
    /// What a page is sent of the row.
    pub payload: Payload,
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
        let payload = match config.get(CFG_PAYLOAD) {
            None | Some(Json::Null) => Payload::Row,
            Some(Json::String(name)) => match name.trim().to_ascii_lowercase().as_str() {
                "" | "row" => Payload::Row,
                "key" => Payload::Key,
                other => {
                    return Err(Error::invalid(format!(
                        "`{CFG_PAYLOAD}`: `{other}` is not a payload; it is `row` (the row is \
                         sent) or `key` (only the key is, and the page fetches the row)"
                    )));
                }
            },
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{CFG_PAYLOAD}` is `row` or `key`, got {other}"
                )));
            }
        };
        Ok(ChangeSettings {
            table,
            ops,
            payload,
        })
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
            FormField::new(CFG_PAYLOAD, BasicType::Text)
                .label("What a page is sent")
                .options([Payload::Row.as_str(), Payload::Key.as_str()])
                .default_value(Json::from(Payload::Row.as_str())),
        ]
    }

    fn element_type(&self, config: &Attrs) -> Result<ElementType> {
        let settings = ChangeSettings::from_config(config)?;
        Ok(change_type(&settings, None))
    }

    fn element_type_in(&self, config: &Attrs, catalog: &Catalog) -> Result<ElementType> {
        let settings = ChangeSettings::from_config(config)?;
        let table = catalog.get(&settings.table)?.ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_TABLE}`: there is no table `{}`",
                settings.table
            ))
        })?;
        let columns = table
            .fields
            .iter()
            .filter(|field| !sc_auth::is_api_hidden_column(&table.name, &field.base.name))
            .map(|field| {
                // What REST types the column as: its basic type, or text for a
                // rich type it cannot name.
                let basic = field
                    .base
                    .type_
                    .as_basic()
                    .cloned()
                    .unwrap_or(BasicType::Text);
                let basic = match basic {
                    BasicType::Other(_) => BasicType::Text,
                    known => known,
                };
                let key = ElementField::new(field.base.name.clone(), basic);
                // A required column is present in every row a page is sent.
                if field.required || field.primary_key {
                    key.required()
                } else {
                    key
                }
            });
        Ok(change_type(&settings, Some(columns.collect())))
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
        let Some(table) = catalog.get(&settings.table)? else {
            return Err(Error::invalid(format!(
                "`{CFG_TABLE}`: there is no table `{}`",
                settings.table
            )));
        };
        // A page applies a change by key — the card that moved, the row that
        // left — so a table whose rows have no one-column key has changes no
        // page could place.
        if table.primary_key.len() != 1 {
            return Err(Error::invalid(format!(
                "`{CFG_TABLE}`: `{}` has no one-column primary key, so a page could not tell \
                 which row a change is about",
                table.name
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

/// The element type for `settings`: `{op, key}`, and `row` unless the payload
/// is only the key — with `row`'s keys when the table's columns are known.
fn change_type(settings: &ChangeSettings, columns: Option<Vec<ElementField>>) -> ElementType {
    let mut keys = vec![
        ElementField::new(KEY_OP, BasicType::Text).required(),
        ElementField::new(KEY_KEY, BasicType::Json).required(),
    ];
    if settings.payload == Payload::Row {
        let row = ElementField::new(KEY_ROW, BasicType::Json);
        keys.push(match columns {
            Some(columns) => row.with_keys(columns),
            None => row,
        });
    }
    ElementType::json(keys)
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
        assert_eq!(all.payload, Payload::Row);
        let keyed =
            ChangeSettings::from_config(&config(json!({ "table": "cards", "payload": "key" })))
                .unwrap();
        assert_eq!(keyed.payload, Payload::Key);

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
            (
                json!({ "table": "cards", "payload": "diff" }),
                "`diff` is not a payload",
            ),
            (json!({ "table": "cards", "payload": 1 }), "`row` or `key`"),
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

    #[test]
    fn the_element_is_op_key_and_row_and_a_key_payload_has_no_row() {
        let names = |ty: ElementType| -> Vec<String> {
            ty.keys().iter().map(|key| key.name.clone()).collect()
        };
        let row = TableChanges
            .element_type(&config(json!({ "table": "cards" })))
            .unwrap();
        assert_eq!(names(row.clone()), vec!["op", "key", "row"]);
        // Without the catalog, `row` is JSON of no declared shape; `old` is
        // never declared, because no page is sent it.
        assert!(row.keys()[2].keys.is_empty());
        let key = TableChanges
            .element_type(&config(json!({ "table": "cards", "payload": "key" })))
            .unwrap();
        assert_eq!(names(key), vec!["op", "key"]);
    }
}
