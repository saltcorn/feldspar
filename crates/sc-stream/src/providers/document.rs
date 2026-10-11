//! The built-in `document` stream provider (TODO.md "Live updates" §2, L5.3).
//!
//! A collaborative document stored in a `collab_doc` field of a row. A
//! subscriber names the row (the topic is a row's key, [`TopicSpec::Row`]),
//! is sent the document (`doc_sync`) and every other member's edits
//! (`doc_update`), and — with **update** access to the row — may send edits of
//! its own. The edits are Yjs updates, merged by the live hub's document room
//! in `yrs`, and persisted back into the field (with the optional
//! `text_mirror` kept equal to the document, so it can be searched, filtered
//! and read over REST). Presence is always on: a member's presence state is
//! its editor's awareness (cursor, selection, colour), opaque to the server.
//!
//! ## The stream carries nothing
//!
//! A document's edits go from the room to its members directly, as presence
//! does, and never through the stream's ring and broadcast: a dropped edit
//! would leave a member's copy diverged until it re-synced, and a ring of raw
//! edits handed to the next subscriber is not a document. The element type
//! (`{update}`) is declared so a generated client has a type to name, nothing
//! publishes into the stream, and a new subscriber is replayed nothing. A
//! trigger that wants to know a document changed listens for the row's
//! update, which every persist is.
//!
//! ## Settings
//!
//! `table`, `field` (a `collab_doc` column of it), `root` (the shared type's
//! name, `default` — what TipTap's Collaboration extension uses), `root_kind`
//! (`text` for a `Y.Text`, as CodeMirror binds; `xml` for a `Y.XmlFragment`,
//! as ProseMirror and TipTap bind), and an optional `text_mirror` (a text
//! column of the table) with `mirror_format` (`text` or `json`).

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::{Catalog, Table};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, COLLAB_DOC, FormField};
use serde_json::{Value as Json, json};

use crate::element::{ElementField, ElementType};
use crate::provider::{Replay, StreamProvider, StreamSink, TopicSpec};
use crate::subscription::Subscription;

/// The name this provider is registered and stored under.
pub const DOCUMENT: &str = "document";

/// The table whose rows hold the documents.
pub const CFG_TABLE: &str = "table";
/// The `collab_doc` column holding each row's document.
pub const CFG_FIELD: &str = "field";
/// The root shared type's name.
pub const CFG_ROOT: &str = "root";
/// What the root is: `text` or `xml`.
pub const CFG_ROOT_KIND: &str = "root_kind";
/// A text column kept equal to the document, if any.
pub const CFG_TEXT_MIRROR: &str = "text_mirror";
/// How the mirror is written: `text` or `json`.
pub const CFG_MIRROR_FORMAT: &str = "mirror_format";

/// The root's name when none is configured: TipTap's.
pub const DEFAULT_ROOT: &str = "default";

/// The element's one key: an edit, as a base64 Yjs update.
pub const KEY_UPDATE: &str = "update";

/// What a `document` stream is configured to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSettings {
    /// The table.
    pub table: String,
    /// The `collab_doc` column.
    pub field: String,
    /// The root shared type's name.
    pub root: String,
    /// `text` or `xml`.
    pub root_kind: String,
    /// The mirror column, if any.
    pub text_mirror: Option<String>,
    /// `text` or `json`.
    pub mirror_format: String,
}

/// A trimmed, non-empty string setting.
fn text(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// One of `choices`, or the first when unset.
fn choice(config: &Attrs, key: &str, choices: &[&str]) -> Result<String> {
    match text(config, key) {
        None => Ok(choices[0].to_owned()),
        Some(v) if choices.contains(&v.as_str()) => Ok(v),
        Some(v) => Err(Error::invalid(format!(
            "`{key}`: `{v}` is not one of {}",
            choices
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

impl DocSettings {
    /// Read and check a stream's configuration.
    pub fn from_config(config: &Attrs) -> Result<DocSettings> {
        let table = text(config, CFG_TABLE).ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_TABLE}`: a document stream needs the table whose rows hold the documents"
            ))
        })?;
        let field = text(config, CFG_FIELD).ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_FIELD}`: a document stream needs the `{COLLAB_DOC}` field holding each \
                 document"
            ))
        })?;
        let text_mirror = text(config, CFG_TEXT_MIRROR);
        if text_mirror.as_deref() == Some(field.as_str()) {
            return Err(Error::invalid(format!(
                "`{CFG_TEXT_MIRROR}` is the field the document is kept in; choose a text field \
                 beside it"
            )));
        }
        Ok(DocSettings {
            table,
            field,
            root: text(config, CFG_ROOT).unwrap_or_else(|| DEFAULT_ROOT.to_owned()),
            root_kind: choice(config, CFG_ROOT_KIND, &["text", "xml"])?,
            text_mirror,
            mirror_format: choice(config, CFG_MIRROR_FORMAT, &["text", "json"])?,
        })
    }

    /// Check the settings against the table they name.
    pub fn check_against(&self, table: &Table) -> Result<()> {
        if table.primary_key.len() != 1 {
            return Err(Error::invalid(format!(
                "`{CFG_TABLE}`: `{}` has no one-column primary key, and a document's topic is \
                 one row's key",
                table.name
            )));
        }
        let field = table
            .fields
            .iter()
            .find(|f| f.base.name == self.field)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{CFG_FIELD}`: `{}` has no field `{}`",
                    table.name, self.field
                ))
            })?;
        if field.base.type_.name() != COLLAB_DOC {
            return Err(Error::invalid(format!(
                "`{CFG_FIELD}`: `{}.{}` is {}, and a document is kept in a `{COLLAB_DOC}` field",
                table.name,
                self.field,
                field.base.type_.name()
            )));
        }
        if let Some(mirror) = &self.text_mirror {
            let found = table
                .fields
                .iter()
                .find(|f| f.base.name == *mirror)
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`{CFG_TEXT_MIRROR}`: `{}` has no field `{mirror}`",
                        table.name
                    ))
                })?;
            let is_text = match found.base.type_.as_basic() {
                Some(basic) => *basic == BasicType::Text,
                None => found.base.type_.sql_type() == "text",
            };
            if !is_text || found.is_calc() {
                return Err(Error::invalid(format!(
                    "`{CFG_TEXT_MIRROR}`: `{}.{mirror}` must be a stored text field, to hold \
                     the document's text",
                    table.name
                )));
            }
        }
        Ok(())
    }
}

/// The `document` provider. Holds nothing: a document's state is its room's
/// and its row's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Document;

#[async_trait]
impl StreamProvider for Document {
    fn name(&self) -> &str {
        DOCUMENT
    }

    fn label(&self) -> &str {
        "Collaborative document"
    }

    fn description(&self) -> &str {
        "A document stored in a field of a row, edited by several people at once: a page \
         subscribes to one row's document and is sent everybody else's edits"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_FIELD, BasicType::Text)
                .label("Document field (collab_doc)")
                .required(),
            FormField::new(CFG_ROOT, BasicType::Text)
                .label("Root shared type")
                .default_value(json!(DEFAULT_ROOT)),
            FormField::new(CFG_ROOT_KIND, BasicType::Text)
                .label("Root kind (text: CodeMirror, Monaco; xml: TipTap, ProseMirror)")
                .options(["xml", "text"])
                .default_value(json!("xml")),
            FormField::new(CFG_TEXT_MIRROR, BasicType::Text).label("Text mirror field (optional)"),
            FormField::new(CFG_MIRROR_FORMAT, BasicType::Text)
                .label("Mirror format")
                .options(["text", "json"])
                .default_value(json!("text")),
        ]
    }

    fn element_type(&self, config: &Attrs) -> Result<ElementType> {
        DocSettings::from_config(config)?;
        Ok(ElementType::json(vec![
            ElementField::new(KEY_UPDATE, BasicType::Text).required(),
        ]))
    }

    fn topic_spec(&self, config: &Attrs) -> Result<TopicSpec> {
        Ok(TopicSpec::Row {
            table: DocSettings::from_config(config)?.table,
        })
    }

    fn replay(&self, _config: &Attrs) -> Result<Replay> {
        Ok(Replay::Nothing)
    }

    fn validate(&self, config: &Attrs) -> Result<()> {
        DocSettings::from_config(config).map(|_| ())
    }

    fn validate_in(&self, config: &Attrs, catalog: &Catalog) -> Result<()> {
        let settings = DocSettings::from_config(config)?;
        let table = catalog.get(&settings.table)?.ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_TABLE}`: there is no table `{}`",
                settings.table
            ))
        })?;
        settings.check_against(&table)
    }

    async fn subscribe(
        &self,
        _stream: &str,
        config: &Attrs,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<Subscription> {
        self.validate(config)?;
        // Idle until dropped: the hub's document rooms do the work.
        Ok(Subscription::spawn(|mut stop| async move {
            stop.stopped().await;
        }))
    }
}

/// The settings of a `document` stream's configuration, or `None` for a
/// stream of another provider (by name) or an unreadable configuration.
pub fn settings_of(provider: &str, config: &Attrs) -> Option<DocSettings> {
    (provider == DOCUMENT)
        .then(|| DocSettings::from_config(config).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(v: Json) -> Attrs {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn the_settings_default_to_tiptaps_root_and_a_plain_text_mirror() {
        let s =
            DocSettings::from_config(&config(json!({"table": "notes", "field": "body"}))).unwrap();
        assert_eq!(s.root, "default");
        assert_eq!(s.root_kind, "text");
        assert_eq!(s.mirror_format, "text");
        assert_eq!(s.text_mirror, None);
        let s = DocSettings::from_config(&config(json!({
            "table": "notes", "field": "body", "root_kind": "xml",
            "text_mirror": "body_text", "mirror_format": "json", "root": "prosemirror"
        })))
        .unwrap();
        assert_eq!(
            (s.root.as_str(), s.root_kind.as_str()),
            ("prosemirror", "xml")
        );
        assert_eq!(s.text_mirror.as_deref(), Some("body_text"));
    }

    #[test]
    fn bad_settings_are_refused_with_a_sentence() {
        for (cfg, says) in [
            (json!({"field": "body"}), "needs the table"),
            (json!({"table": "notes"}), "collab_doc"),
            (
                json!({"table": "notes", "field": "body", "root_kind": "map"}),
                "not one of",
            ),
            (
                json!({"table": "notes", "field": "body", "text_mirror": "body"}),
                "text field beside it",
            ),
        ] {
            let err = DocSettings::from_config(&config(cfg))
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{err}");
        }
    }

    #[test]
    fn the_topic_is_a_row_and_nothing_is_replayed() {
        let cfg = config(json!({"table": "notes", "field": "body"}));
        assert_eq!(
            Document.topic_spec(&cfg).unwrap(),
            TopicSpec::Row {
                table: "notes".into()
            }
        );
        assert_eq!(Document.replay(&cfg).unwrap(), Replay::Nothing);
        assert!(!Document.accepts_publish());
        assert!(settings_of(DOCUMENT, &cfg).is_some());
        assert!(settings_of("internal", &cfg).is_none());
    }
}
