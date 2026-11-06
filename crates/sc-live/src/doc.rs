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
//! - **And bounded** (TODO-type-hardening H5.3). `yrs` recurses over a value's
//!   nesting as it decodes and over the document's as it deletes, collects and
//!   prints, and a stack overflow is not a panic: it ends the process. Every
//!   update is read by [`crate::yjs_scan`] first, which refuses a value nested
//!   past its bound and counts the shared types the update creates, and the
//!   document is kept within [`MAX_DOC_DEPTH`] levels of them.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use sc_error::{Error, Result};
use yrs::types::text::YChange;
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Text, TextRef, Transact, Update,
    XmlElementPrelim, XmlFragmentRef, XmlTextPrelim,
};

use crate::yjs_scan;

/// How deep shared types may nest in a document — the root, an element in it,
/// an element in that — before an update that would go deeper is refused
/// (TODO-type-hardening H5.3). A rich-text document is a handful deep (a
/// table's cell's paragraph; a list in a list item); this is far past any
/// editor and well inside what `yrs`'s recursion can take.
pub const MAX_DOC_DEPTH: usize = 256;

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
    /// At least as deep as the document's shared types nest, counting the
    /// ones still pending and the root as one: what was measured, plus every
    /// type created since.
    /// Measured again when it would pass [`MAX_DOC_DEPTH`].
    depth_bound: AtomicUsize,
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
            // The root is a level before any update makes one.
            depth_bound: AtomicUsize::new(1),
        }
    }

    /// The document a stored value holds: empty for `None` (a null field) or
    /// an empty value, the stored state otherwise.
    ///
    /// The stored state is measured before it is applied, and one deeper than
    /// [`MAX_DOC_DEPTH`] is not loaded.
    pub fn load(shape: DocShape, stored: Option<&[u8]>) -> Result<CollabDoc> {
        let doc = CollabDoc::empty(shape);
        if let Some(bytes) = stored.filter(|b| !b.is_empty()) {
            guarded("the stored document", || {
                yjs_scan::scan(bytes, &StateVector::default()).map_err(Error::invalid)?;
                let depth = yjs_scan::depth(bytes, &[]).map_err(Error::invalid)?;
                if depth > MAX_DOC_DEPTH {
                    return Err(too_deep());
                }
                apply_to(&doc.doc, bytes)?;
                doc.depth_bound.store(depth, Ordering::Relaxed);
                Ok(())
            })
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
    ///
    /// Refused, and not applied, when a value in it nests too deep or it would
    /// take the document past [`MAX_DOC_DEPTH`].
    pub fn apply(&self, update: &[u8]) -> Result<()> {
        guarded("the update", || {
            let known = self.state_vector_now();
            let scan = yjs_scan::scan(update, &known)
                .map_err(|e| Error::invalid(format!("the update: {e}")))?;
            self.admit(update, scan.new_types)?;
            apply_to(&self.doc, update)
        })
    }

    /// Whether an update creating `new_types` shared types keeps the document
    /// within [`MAX_DOC_DEPTH`], recording what it may add.
    ///
    /// Each new type adds at most one level, so the cheap answer is the bound
    /// plus the count. When that would pass the limit (most of what the bound
    /// counted are siblings, not levels) the document is measured as it would
    /// be with the update — its state, its pending update and this one, read
    /// by [`yjs_scan::depth`] — and the bound starts again from that.
    fn admit(&self, update: &[u8], new_types: usize) -> Result<()> {
        if new_types == 0 {
            return Ok(());
        }
        let bound = self.depth_bound.load(Ordering::Relaxed);
        if bound.saturating_add(new_types) <= MAX_DOC_DEPTH {
            self.depth_bound
                .store(bound.saturating_add(new_types), Ordering::Relaxed);
            return Ok(());
        }
        let state = self.state();
        let pending = pending_update(&self.doc);
        let more: Vec<&[u8]> = pending.as_deref().into_iter().chain([update]).collect();
        let depth = yjs_scan::depth(&state, &more).map_err(Error::invalid)?;
        if depth > MAX_DOC_DEPTH {
            return Err(too_deep());
        }
        self.depth_bound.store(depth, Ordering::Relaxed);
        Ok(())
    }

    /// The state vector, read in a transaction of its own.
    fn state_vector_now(&self) -> StateVector {
        self.doc.transact().state_vector()
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
    /// out of an `xml` root — one line per textblock (every list item, paragraph and table cell), formatting marks
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
                    block_lines(&child, &txn, &mut lines);
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
        // The root, the paragraph and its text: three levels, at most.
        self.depth_bound.fetch_max(3, Ordering::Relaxed);
        self.diff(&before)
    }
}

/// Decode and apply one update, already scanned.
fn apply_to(doc: &Doc, update: &[u8]) -> Result<()> {
    let update =
        Update::decode_v1(update).map_err(|e| Error::invalid(format!("the update: {e}")))?;
    doc.transact_mut()
        .apply_update(update)
        .map_err(|e| Error::invalid(format!("the update: {e}")))
}

fn too_deep() -> Error {
    Error::invalid(format!(
        "the update would nest the document more than {MAX_DOC_DEPTH} levels deep, so it was \
         not applied"
    ))
}

/// The update `doc` holds back, waiting for what it depends on.
fn pending_update(doc: &Doc) -> Option<Vec<u8>> {
    let txn = doc.transact();
    txn.store()
        .pending_update()
        .map(|pending| pending.update.encode_v1())
}

/// How a kind is said in a sentence.
fn describe(kind: RootKind) -> &'static str {
    match kind {
        RootKind::Text => "text",
        RootKind::Xml => "xml",
    }
}

/// Append the lines of the block `node` to `lines`. A textblock — an element
/// holding text, such as a paragraph, a heading or a code block — is one line
/// (a code block's own newlines kept); a container of blocks — a list, a list
/// item, a blockquote, a table, a row, a cell — is the lines of its children,
/// so every item, every paragraph inside an item and every cell is a line. An
/// element with nothing in it (an empty paragraph) is an empty line.
fn block_lines<T: ReadTxn>(node: &XmlOut, txn: &T, lines: &mut Vec<String>) {
    match node {
        XmlOut::Text(_) => {
            let mut line = String::new();
            inline_text(node, txn, &mut line);
            lines.push(line);
        }
        XmlOut::Element(e) => {
            let children: Vec<XmlOut> = e.children(txn).collect();
            let textblock = children.is_empty()
                || children
                    .iter()
                    .any(|c| matches!(c, XmlOut::Text(_)) || is_line_break(c));
            if textblock {
                let mut line = String::new();
                for child in &children {
                    inline_text(child, txn, &mut line);
                }
                lines.push(line);
            } else {
                for child in &children {
                    block_lines(child, txn, lines);
                }
            }
        }
        XmlOut::Fragment(f) => {
            for child in f.children(txn) {
                block_lines(&child, txn, lines);
            }
        }
    }
}

/// ProseMirror's `hardBreak` (TipTap's `<br>`): a line break inside a
/// textblock.
fn is_line_break(node: &XmlOut) -> bool {
    matches!(node, XmlOut::Element(e) if e.tag().as_ref() == "hardBreak")
}

/// Append the text inside a textblock's child `node` to `out`, formatting
/// marks left out; a hard break is a newline.
fn inline_text<T: ReadTxn>(node: &XmlOut, txn: &T, out: &mut String) {
    match node {
        XmlOut::Text(t) => {
            for chunk in t.diff(txn, YChange::identity) {
                if let yrs::Out::Any(yrs::Any::String(s)) = chunk.insert {
                    out.push_str(&s);
                }
            }
        }
        XmlOut::Element(_) if is_line_break(node) => out.push('\n'),
        XmlOut::Element(e) => {
            for child in e.children(txn) {
                inline_text(&child, txn, out);
            }
        }
        XmlOut::Fragment(f) => {
            for child in f.children(txn) {
                inline_text(&child, txn, out);
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
            // Decoded whole by the merge: each is read for its values' depth
            // first, as an update is.
            for part in &parts {
                yjs_scan::scan(part, &StateVector::default())
                    .map_err(|e| Error::invalid(format!("merging two documents: {e}")))?;
            }
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
    use yrs::XmlElementRef;

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

    /// Field report (PaddiPad): a bullet list `eins / zwei / drei` was
    /// mirrored as `einszweidrei`. Every list item, every paragraph inside an
    /// item, every table cell is a line; a code block keeps its lines and a
    /// hard break is one.
    #[test]
    fn nested_blocks_of_an_xml_root_read_one_line_each() {
        let d = CollabDoc::empty(DocShape::new("default", RootKind::Xml));
        {
            let xml = d.xml_ref();
            let mut txn = d.doc.transact_mut();
            let block = |txn: &mut yrs::TransactionMut, parent: &XmlElementRef, tag: &str| {
                parent.push_back(txn, XmlElementPrelim::empty(tag))
            };
            let para = |txn: &mut yrs::TransactionMut, parent: &XmlElementRef, text: &str| {
                let p = parent.push_back(txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(txn, XmlTextPrelim::new(text));
            };
            let h = xml.push_back(&mut txn, XmlElementPrelim::empty("heading"));
            h.push_back(&mut txn, XmlTextPrelim::new("Liste"));
            let list = xml.push_back(&mut txn, XmlElementPrelim::empty("bulletList"));
            for word in ["eins", "zwei", "drei"] {
                let item = block(&mut txn, &list, "listItem");
                para(&mut txn, &item, word);
            }
            // An item with two paragraphs and a nested task list.
            let item = block(&mut txn, &list, "listItem");
            para(&mut txn, &item, "vier");
            para(&mut txn, &item, "fünf");
            let tasks = block(&mut txn, &item, "taskList");
            let task = block(&mut txn, &tasks, "taskItem");
            para(&mut txn, &task, "sechs");
            // A table, a code block and a hard break.
            let table = xml.push_back(&mut txn, XmlElementPrelim::empty("table"));
            let row = block(&mut txn, &table, "tableRow");
            for cell in ["a1", "b1"] {
                let c = block(&mut txn, &row, "tableCell");
                para(&mut txn, &c, cell);
            }
            let code = xml.push_back(&mut txn, XmlElementPrelim::empty("codeBlock"));
            code.push_back(&mut txn, XmlTextPrelim::new("let x;\nlet y;"));
            let p = xml.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("vor"));
            p.push_back(&mut txn, XmlElementPrelim::empty("hardBreak"));
            p.push_back(&mut txn, XmlTextPrelim::new("nach"));
            xml.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            let p = xml.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("Ende"));
        }
        assert_eq!(
            d.text(),
            "Liste\neins\nzwei\ndrei\nvier\nfünf\nsechs\na1\nb1\nlet x;\nlet y;\nvor\nnach\n\nEnde"
        );
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

    /// A document whose root holds `depth - 1` elements, one inside the next:
    /// `depth` levels, the root included.
    fn nested_xml(depth: usize) -> Vec<u8> {
        let doc = new_doc();
        {
            let mut txn = doc.transact_mut();
            let root = yrs::WriteTxn::get_or_insert_xml_fragment(&mut txn, "default");
            let mut parent = root.push_back(&mut txn, XmlElementPrelim::empty("div"));
            for _ in 2..depth {
                parent = parent.push_back(&mut txn, XmlElementPrelim::empty("div"));
            }
        }
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    fn xml_doc() -> CollabDoc {
        CollabDoc::empty(DocShape::new("default", RootKind::Xml))
    }

    #[test]
    fn a_document_nested_past_the_limit_is_refused_and_not_applied() {
        let d = xml_doc();
        d.append_paragraph("before").unwrap();
        let err = d
            .apply(&nested_xml(MAX_DOC_DEPTH + 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than 256 levels deep"), "{err}");
        // Nothing of it was applied, and the document goes on.
        assert_eq!(d.text(), "before");
        d.append_paragraph("after").unwrap();
        assert_eq!(d.text(), "before\nafter");

        // At the limit is a document like any other.
        let ok = xml_doc();
        ok.apply(&nested_xml(MAX_DOC_DEPTH)).unwrap();
        assert!(CollabDoc::load(ok.shape().clone(), Some(&ok.state())).is_ok());
        // And a stored document past it is not loaded.
        let stored = nested_xml(MAX_DOC_DEPTH + 1);
        let err = CollabDoc::load(ok.shape().clone(), Some(&stored))
            .unwrap_err()
            .to_string();
        assert!(err.contains("levels deep"), "{err}");
    }

    #[test]
    fn nesting_one_level_an_update_is_stopped_at_the_limit() {
        // The editor's way to the bottom: each update one element deeper.
        let source = new_doc();
        let target = xml_doc();
        let mut parent = {
            let mut txn = source.transact_mut();
            let root = yrs::WriteTxn::get_or_insert_xml_fragment(&mut txn, "default");
            root.push_back(&mut txn, XmlElementPrelim::empty("div"))
        };
        let mut sent = source
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let mut levels = 2;
        loop {
            match target.apply(&sent) {
                Ok(()) => {}
                Err(e) => {
                    assert!(e.to_string().contains("levels deep"), "{e}");
                    break;
                }
            }
            assert!(levels <= MAX_DOC_DEPTH, "level {levels} was let in");
            let before = source.transact().state_vector();
            {
                let mut txn = source.transact_mut();
                parent = parent.push_back(&mut txn, XmlElementPrelim::empty("div"));
            }
            sent = source.transact().encode_diff_v1(&before);
            levels += 1;
        }
        assert_eq!(levels, MAX_DOC_DEPTH + 1);
        assert_eq!(
            yjs_scan::depth(&target.state(), &[]).unwrap(),
            MAX_DOC_DEPTH
        );
    }

    #[test]
    fn a_value_nested_half_a_million_deep_is_refused_before_it_is_decoded() {
        // One `Any` under root `default`: arrays in arrays, two bytes a level.
        let mut update = vec![1, 1, 1, 0, 8, 1, 7];
        update.extend_from_slice(b"default");
        update.push(1);
        for _ in 0..500_000 {
            update.extend([117, 1]);
        }
        update.extend([126, 0]);
        let d = text_doc();
        let err = d.apply(&update).unwrap_err().to_string();
        assert!(err.contains("nests more than"), "{err}");
        assert!(CollabDoc::load(d.shape().clone(), Some(&update)).is_err());
        assert!(merge_states(&update, &d.state()).is_err());
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
