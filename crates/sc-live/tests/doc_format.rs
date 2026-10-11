//! The format compatibility L5 relies on (TODO.md L5.2): updates made by the
//! JavaScript `yjs` package — recorded by `fixtures/record_yjs_updates.js` from
//! two clients editing one document — apply in `yrs` and read back as the same
//! text, for a `text` root (CodeMirror's) and an `xml` root (TipTap's). And what
//! `yrs` encodes, `yjs` can read: the state re-encoded here, loaded again, is
//! the same document.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sc_live::doc::{CollabDoc, DocShape, MirrorFormat, RootKind, merge_states};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/yjs_updates.json")).unwrap()
}

fn bytes(v: &Value) -> Vec<u8> {
    STANDARD.decode(v.as_str().unwrap()).unwrap()
}

#[test]
fn a_text_root_written_by_yjs_reads_the_same_in_yrs() {
    let f = &fixture()["text"];
    let shape = DocShape::new(f["root"].as_str().unwrap(), RootKind::Text);

    // Update by update, as a room receives them.
    let doc = CollabDoc::empty(shape.clone());
    for u in f["updates"].as_array().unwrap() {
        doc.apply(&bytes(u)).unwrap();
    }
    assert_eq!(doc.text(), f["text"].as_str().unwrap());
    assert_eq!(doc.mirror(MirrorFormat::Json), f["json"].as_str().unwrap());

    // The whole state at once, as a stored field holds it.
    let stored = CollabDoc::load(shape.clone(), Some(&bytes(&f["state"]))).unwrap();
    assert_eq!(stored.text(), f["text"].as_str().unwrap());
    // Text positions are JavaScript's: the emoji is two code units.
    assert_eq!(
        stored.text_len().unwrap() as usize,
        f["text"].as_str().unwrap().encode_utf16().count()
    );

    // Re-encoded by yrs and read again: nothing lost.
    let again = CollabDoc::load(shape, Some(&stored.state())).unwrap();
    assert_eq!(again.text(), f["text"].as_str().unwrap());
}

#[test]
fn an_xml_root_written_by_yjs_reads_the_same_in_yrs() {
    let f = &fixture()["xml"];
    let shape = DocShape::new(f["root"].as_str().unwrap(), RootKind::Xml);
    let doc = CollabDoc::empty(shape.clone());
    for u in f["updates"].as_array().unwrap() {
        doc.apply(&bytes(u)).unwrap();
    }
    // `root.toJSON()` is the XML serialisation, marks included.
    assert_eq!(doc.mirror(MirrorFormat::Json), f["json"].as_str().unwrap());
    let xml: String = serde_json::from_str(f["json"].as_str().unwrap()).unwrap();
    assert_eq!(xml, f["xml"].as_str().unwrap());
    // The plain text: a line per block, the bold mark gone.
    assert_eq!(
        doc.text(),
        "Notes\nFirst line, bold\nSecond paragraph by Bob\n"
    );

    let stored = CollabDoc::load(shape.clone(), Some(&bytes(&f["state"]))).unwrap();
    assert_eq!(
        stored.mirror(MirrorFormat::Json),
        f["json"].as_str().unwrap()
    );

    // A paragraph appended by the server lands after Bob's.
    stored.append_paragraph("Added by a workflow").unwrap();
    assert!(
        stored.text().ends_with("\nAdded by a workflow"),
        "{}",
        stored.text()
    );
}

#[test]
fn the_recorded_updates_merge_into_the_recorded_state() {
    let f = &fixture()["text"];
    let shape = DocShape::new("default", RootKind::Text);
    let mut merged = Vec::new();
    for u in f["updates"].as_array().unwrap() {
        merged = merge_states(&merged, &bytes(u)).unwrap();
    }
    let doc = CollabDoc::load(shape, Some(&merged)).unwrap();
    assert_eq!(doc.text(), f["text"].as_str().unwrap());
}
