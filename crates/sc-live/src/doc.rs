//! A collaborative document's CRDT state, through `yrs` (L5.2).
//!
//! A `document` stream's topic is a row, and the row's `collab_doc` field holds
//! the document as **Yjs's own binary update format** (v1), which `yrs` — the
//! Rust port of Yjs — reads and writes. That is the whole point of using it:
//! the browser keeps the standard `yjs` package and every editor binding built
//! on it (ProseMirror/TipTap, CodeMirror, Monaco), and the server can still
//! merge, read and write what they produce. The format test in
//! `tests/doc_format.rs` replays updates recorded from the JS package.
//!
//! This module is pure: a [`CollabDoc`] is one document in memory, with the
//! operations the room, the persister and `live.document` need. Nothing here
//! knows about sockets, rows or access.
//!
//! - **The root is named and typed** ([`DocShape`]): `yrs` must be told whether
//!   the shared type under `root` is a `Y.Text` (CodeMirror) or a
//!   `Y.XmlFragment` (ProseMirror/TipTap), because the update format does not
//!   say until it is read. A root a client wrote as the other kind is
//!   reinterpreted, never a panic.
//! - **Indices are UTF-16 code units**, as in JavaScript, so a text edit made
//!   from a code body means the same position a browser would.
//! - **An update is untrusted input.** Decoding and applying one runs behind
//!   `catch_unwind`: a malformed update is an error for its sender, never a
//!   panic in the room every other member shares.

use std::panic::{AssertUnwindSafe, catch_unwind};

use sc_error::{Error, Result};
use yrs::types::text::YChange;
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Text, TextRef, Transact, Update,
    XmlElementPrelim, XmlFragmentRef, XmlTextPrelim,
};

/// What the root shared type is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// A `Y.Text` — what CodeMirror's and Monaco's bindings edit.
    Text,
    /// A `Y.XmlFragment` — what ProseMirror's (and so TipTap's) binding edits.
    Xml,
}

impl RootKind {
    /// The setting's spelling (`text` | `xml`).
    pub fn parse(s: &str) -> Option<RootKind> {
        match s {
            "text" => Some(RootKind::Text),
            "xml" => Some(RootKind::Xml),
            _ => None,
        }
    }

    /// The setting's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RootKind::Text => "text",
            RootKind::Xml => "xml",
        }
    }
}

/// How the optional `text_mirror` column is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorFormat {
    /// Plain text: the text of a `text` root, or the text walked out of an
    /// `xml` root (one line per block).
    Text,
    /// `JSON.stringify(root.toJSON())`, as Yjs would produce it.
    Json,
}

impl MirrorFormat {
    /// The setting's spelling (`text` | `json`).
    pub fn parse(s: &str) -> Option<MirrorFormat> {
        match s {
            "text" => Some(MirrorFormat::Text),
            "json" => Some(MirrorFormat::Json),
            _ => None,
        }
    }
}

/// The root a document stream reads: its name and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocShape {
    /// The shared type's name (`default`, TipTap's, unless configured).
    pub root: String,
    /// What it is.
    pub kind: RootKind,
}

impl DocShape {
    /// A shape.
    pub fn new(root: impl Into<String>, kind: RootKind) -> DocShape {
        DocShape {
            root: root.into(),
            kind,
        }
    }
}

/// One document in memory.
pub struct CollabDoc {
    doc: Doc,
    shape: DocShape,
}

impl std::fmt::Debug for CollabDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabDoc")
            .field("shape", &self.shape)
            .finish_non_exhaustive()
    }
}

/// A fresh `yrs` document counting positions as JavaScript does.
fn new_doc() -> Doc {
    Doc::with_options(Options {
        offset_kind: OffsetKind::Utf16,
        ..Options::default()
    })
}

/// Run `f`, turning a panic inside `yrs` into an error naming `what`.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(Error::invalid(format!("{what}: not a valid Yjs update"))),
    }
}

impl CollabDoc {
    /// An empty document.
    pub fn empty(shape: DocShape) -> CollabDoc {
        CollabDoc {
            doc: new_doc(),
            shape,
        }
    }

    /// The document a stored value holds: empty for `None` (a null field) or
    /// an empty value, the stored state otherwise.
    pub fn load(shape: DocShape, stored: Option<&[u8]>) -> Result<CollabDoc> {
        let doc = CollabDoc::empty(shape);
        if let Some(bytes) = stored.filter(|b| !b.is_empty()) {
            doc.apply(bytes)
                .map_err(|e| Error::invalid(format!("the stored document cannot be read: {e}")))?;
        }
        Ok(doc)
    }

    /// The shape this document is read with.
    pub fn shape(&self) -> &DocShape {
        &self.shape
    }

    /// Apply an update (Yjs v1). Applying one twice, or out of order, is
    /// harmless — that is what a CRDT is for.
    pub fn apply(&self, update: &[u8]) -> Result<()> {
        guarded("the update", || {
            let update = Update::decode_v1(update)
                .map_err(|e| Error::invalid(format!("the update: {e}")))?;
            self.doc
                .transact_mut()
                .apply_update(update)
                .map_err(|e| Error::invalid(format!("the update: {e}")))
        })
    }

    /// The whole state as one update — what `doc_sync` carries and what is
    /// stored.
    pub fn state(&self) -> Vec<u8> {
        self.doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    /// What this document has beyond `state_vector` (an encoded Yjs state
    /// vector), as one update.
    pub fn diff(&self, state_vector: &[u8]) -> Result<Vec<u8>> {
        let sv = StateVector::decode_v1(state_vector)
            .map_err(|e| Error::invalid(format!("the state vector: {e}")))?;
        Ok(self.doc.transact().encode_state_as_update_v1(&sv))
    }

    /// The encoded state vector: what this document has seen.
    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }

    fn text_ref(&self) -> TextRef {
        self.doc.get_or_insert_text(self.shape.root.as_str())
    }

    fn xml_ref(&self) -> XmlFragmentRef {
        self.doc
            .get_or_insert_xml_fragment(self.shape.root.as_str())
    }

    /// The document's plain text: a `text` root's string, or the text walked
    /// out of an `xml` root — one line per block element, formatting marks
    /// left out.
    pub fn text(&self) -> String {
        match self.shape.kind {
            RootKind::Text => {
                let text = self.text_ref();
                let txn = self.doc.transact();
                text.get_string(&txn)
            }
            RootKind::Xml => {
                let xml = self.xml_ref();
                let txn = self.doc.transact();
                let mut lines = Vec::new();
                for child in xml.children(&txn) {
                    let mut line = String::new();
                    plain_text(&child, &txn, &mut line);
                    lines.push(line);
                }
                lines.join("\n")
            }
        }
    }

    /// `JSON.stringify(root.toJSON())` as Yjs computes it: the string of a
    /// `text` root, the XML serialisation of an `xml` root.
    pub fn json(&self) -> String {
        let s = match self.shape.kind {
            RootKind::Text => self.text(),
            RootKind::Xml => {
                let xml = self.xml_ref();
                let txn = self.doc.transact();
                xml.get_string(&txn)
            }
        };
        serde_json::Value::String(s).to_string()
    }

    /// The mirror column's value in `format`.
    pub fn mirror(&self, format: MirrorFormat) -> String {
        match format {
            MirrorFormat::Text => self.text(),
            MirrorFormat::Json => self.json(),
        }
    }

    /// The length of a `text` root, in UTF-16 code units.
    pub fn text_len(&self) -> Result<u32> {
        self.require(RootKind::Text, "measure the text")?;
        let text = self.text_ref();
        let txn = self.doc.transact();
        Ok(text.len(&txn))
    }

    fn require(&self, kind: RootKind, what: &str) -> Result<()> {
        if self.shape.kind == kind {
            Ok(())
        } else {
            Err(Error::invalid(format!(
                "cannot {what}: the document's root `{}` is {}, not {}",
                self.shape.root,
                describe(self.shape.kind),
                describe(kind)
            )))
        }
    }

    /// Edit a `text` root — delete `delete` code units at `index`, then insert
    /// `insert` there — and answer the edit as an update to hand the room's
    /// other members. Positions past the end are refused, not clamped.
    pub fn edit_text(&self, index: u32, delete: u32, insert: &str) -> Result<Vec<u8>> {
        self.require(RootKind::Text, "edit the text")?;
        let text = self.text_ref();
        let before = self.state_vector();
        {
            let mut txn = self.doc.transact_mut();
            let len = text.len(&txn);
            let end = index.checked_add(delete).filter(|e| *e <= len);
            if end.is_none() {
                return Err(Error::invalid(format!(
                    "the edit ({index}, delete {delete}) is outside the text, which is {len} long"
                )));
            }
            if delete > 0 {
                text.remove_range(&mut txn, index, delete);
            }
            if !insert.is_empty() {
                text.insert(&mut txn, index, insert);
            }
        }
        self.diff(&before)
    }

    /// Append a paragraph holding `text` to an `xml` root — a ProseMirror
    /// `paragraph` element with one text node, which is what TipTap's schema
    /// reads — and answer the edit as an update.
    pub fn append_paragraph(&self, text: &str) -> Result<Vec<u8>> {
        self.require(RootKind::Xml, "append a paragraph")?;
        let xml = self.xml_ref();
        let before = self.state_vector();
        {
            let mut txn = self.doc.transact_mut();
            let paragraph = xml.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            if !text.is_empty() {
                paragraph.push_back(&mut txn, XmlTextPrelim::new(text));
            }
        }
        self.diff(&before)
    }
}

/// How a kind is said in a sentence.
fn describe(kind: RootKind) -> &'static str {
    match kind {
        RootKind::Text => "text",
        RootKind::Xml => "xml",
    }
}

/// Append the plain text under `node` to `out`.
fn plain_text<T: ReadTxn>(node: &XmlOut, txn: &T, out: &mut String) {
    match node {
        XmlOut::Text(t) => {
            for chunk in t.diff(txn, YChange::identity) {
                if let yrs::Out::Any(yrs::Any::String(s)) = chunk.insert {
                    out.push_str(&s);
                }
            }
        }
        XmlOut::Element(e) => {
            for child in e.children(txn) {
                plain_text(&child, txn, out);
            }
        }
        XmlOut::Fragment(f) => {
            for child in f.children(txn) {
                plain_text(&child, txn, out);
            }
        }
    }
}

/// Merge two stored states (or any two updates) into one. CRDT merging is
/// commutative and idempotent, which is what makes two processes persisting
/// one document safe: each merges what is stored with what it has.
pub fn merge_states(a: &[u8], b: &[u8]) -> Result<Vec<u8>> {
    let parts: Vec<&[u8]> = [a, b].into_iter().filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => Ok(Vec::new()),
        [one] => Ok(one.to_vec()),
        _ => guarded("merging", || {
            yrs::merge_updates_v1(parts.iter().copied())
                .map_err(|e| Error::invalid(format!("merging two documents: {e}")))
        }),
    }
}

/// The text of a stored document, without keeping it — what `live.document`
/// reads when no room is open.
pub fn stored_text(shape: &DocShape, stored: Option<&[u8]>) -> Result<String> {
    Ok(CollabDoc::load(shape.clone(), stored)?.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_doc() -> CollabDoc {
        CollabDoc::empty(DocShape::new("default", RootKind::Text))
    }

    #[test]
    fn a_text_edit_travels_as_an_update() {
        let a = text_doc();
        let up = a.edit_text(0, 0, "Hello").unwrap();
        let b = text_doc();
        b.apply(&up).unwrap();
        assert_eq!(b.text(), "Hello");
        let up = b.edit_text(5, 0, " world").unwrap();
        a.apply(&up).unwrap();
        // Applying the same update twice changes nothing.
        a.apply(&up).unwrap();
        assert_eq!(a.text(), "Hello world");
        assert_eq!(a.json(), "\"Hello world\"");
    }

    #[test]
    fn positions_count_utf16_code_units_as_javascript_does() {
        let d = text_doc();
        d.edit_text(0, 0, "👋x").unwrap();
        assert_eq!(d.text_len().unwrap(), 3);
        d.edit_text(2, 1, "y").unwrap();
        assert_eq!(d.text(), "👋y");
        let err = d.edit_text(2, 5, "").unwrap_err().to_string();
        assert!(err.contains("outside the text"), "{err}");
    }

    #[test]
    fn a_paragraph_is_appended_to_an_xml_root_and_read_as_a_line() {
        let d = CollabDoc::empty(DocShape::new("default", RootKind::Xml));
        d.append_paragraph("one").unwrap();
        let up = d.append_paragraph("two").unwrap();
        assert_eq!(d.text(), "one\ntwo");
        assert_eq!(
            d.json(),
            "\"<paragraph>one</paragraph><paragraph>two</paragraph>\""
        );
        // The update carries only the second paragraph.
        let other = CollabDoc::empty(DocShape::new("default", RootKind::Xml));
        other.apply(&d.state()).unwrap();
        other.apply(&up).unwrap();
        assert_eq!(other.text(), "one\ntwo");
        // And a text edit on an xml root is refused, saying why.
        let err = d.edit_text(0, 0, "x").unwrap_err().to_string();
        assert!(err.contains("is xml, not text"), "{err}");
    }

    #[test]
    fn concurrent_states_merge_and_merging_is_idempotent() {
        let base = text_doc();
        base.edit_text(0, 0, "base").unwrap();
        let a = CollabDoc::load(base.shape().clone(), Some(&base.state())).unwrap();
        let b = CollabDoc::load(base.shape().clone(), Some(&base.state())).unwrap();
        a.edit_text(0, 0, "A ").unwrap();
        b.edit_text(4, 0, " B").unwrap();
        let merged = merge_states(&a.state(), &b.state()).unwrap();
        let again = merge_states(&merged, &a.state()).unwrap();
        for state in [&merged, &again] {
            let d = CollabDoc::load(base.shape().clone(), Some(state)).unwrap();
            assert_eq!(d.text(), "A base B");
        }
        assert_eq!(merge_states(&[], &merged).unwrap(), merged);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let d = text_doc();
        for junk in [&b"\xff\xff\xff\xff"[..], b"hello", &[1, 200, 3, 4, 5, 6, 7]] {
            assert!(d.apply(junk).is_err(), "{junk:?}");
        }
        assert!(CollabDoc::load(d.shape().clone(), Some(b"\x05\x06")).is_err());
        assert!(merge_states(b"\x01\x02\x03", b"\x09\x09").is_err());
        // An empty stored value is an empty document.
        assert_eq!(
            CollabDoc::load(d.shape().clone(), Some(&[]))
                .unwrap()
                .text(),
            ""
        );
    }
}
