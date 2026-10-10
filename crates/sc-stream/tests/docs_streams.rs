//! The streams milestone's documentation, against the code it documents.
//!
//! `docs/tutorial-streams.md` tells an admin which boxes to fill in and what to
//! publish, and `docs/TECHNICAL_DESIGN.md` §14.3 states where the entity lives
//! and what it promises. Both go stale silently: a renamed setting or a changed
//! default leaves a document that is confidently wrong, which is worse than no
//! document. So the facts the two assert about this crate are asserted here
//! against this crate — `docs_agents.rs`'s arrangement, for its reason.
//!
//! What this does **not** do is check prose. It checks the names and the
//! numbers.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "mqtt")]
use sc_stream::providers::mqtt;
use sc_stream::store::{
    COL_ATTRIBUTES, COL_CONFIGURATION, COL_DESCRIPTION, COL_MIN_ROLE, COL_NAME, COL_PROVIDER,
    STREAMS_TABLE,
};
use sc_stream::supervisor::StreamConfig;

/// Walk up to the workspace root (the ancestor whose `Cargo.toml` declares
/// `[workspace]`), as `repo_hygiene` does.
fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file()
            && fs::read_to_string(&manifest)
                .unwrap_or_default()
                .contains("[workspace]")
        {
            return dir;
        }
        assert!(dir.pop(), "no [workspace] Cargo.toml above this crate");
    }
}

/// A document with its line breaks folded, so a phrase the file wraps over two
/// lines is still one phrase to search for.
fn doc(rel: &str) -> String {
    let path = workspace_root().join(rel);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing {rel}: {e}"));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The same, with the markdown emphasis taken out.
fn plain(rel: &str) -> String {
    doc(rel).replace(['`', '*'], "")
}

fn tutorial() -> String {
    doc("docs/tutorial-streams.md")
}

fn design() -> String {
    doc("docs/TECHNICAL_DESIGN.md")
}

/// The leading clause of a form label — everything before the first `:`, `(` or
/// `,` — because that is what a document's table quotes of a label that goes on
/// to explain itself.
fn leading_clause(label: &str) -> String {
    let clause = label.split([':', '(', ',']).next().unwrap_or(label).trim();
    match clause.is_empty() {
        true => label
            .split_whitespace()
            .take(5)
            .collect::<Vec<_>>()
            .join(" "),
        false => clause.to_owned(),
    }
}

/// The label of one of the MQTT provider's settings, as the form shows it.
#[cfg(feature = "mqtt")]
fn mqtt_label(key: &str) -> String {
    use sc_stream::StreamProvider;

    mqtt::Mqtt
        .config_spec()
        .into_iter()
        .find(|field| field.name() == key)
        .unwrap_or_else(|| panic!("the mqtt provider has no `{key}` setting any more"))
        .base
        .label
}

/// Every setting the tutorial's step 3 table tells an admin to fill in.
///
/// A renamed label there is a reader looking for a box that is not on the
/// screen, which is the whole failure mode a walkthrough has.
#[cfg(feature = "mqtt")]
#[test]
fn the_tutorial_names_every_mqtt_setting() {
    let tutorial = plain("docs/tutorial-streams.md");
    for key in [
        mqtt::CFG_HOST,
        mqtt::CFG_PORT,
        mqtt::CFG_USE_TLS,
        mqtt::CFG_CLIENT_ID,
        mqtt::CFG_USERNAME,
        mqtt::CFG_PASSWORD,
        mqtt::CFG_TOPIC,
        mqtt::CFG_QOS,
        mqtt::CFG_CLEAN_SESSION,
        mqtt::CFG_PAYLOAD,
    ] {
        let clause = leading_clause(&mqtt_label(key));
        assert!(
            tutorial.contains(&clause),
            "docs/tutorial-streams.md does not name the `{key}` setting (`{clause}…`)"
        );
    }
}

/// The three payload settings are the three element types, and the tutorial
/// says so because that is the one choice the whole shape of a stream hangs on.
#[cfg(feature = "mqtt")]
#[test]
fn both_documents_name_the_three_payload_kinds() {
    let tutorial = tutorial();
    let design = design();
    for payload in [mqtt::PAYLOAD_JSON, mqtt::PAYLOAD_TEXT, mqtt::PAYLOAD_BINARY] {
        assert!(
            tutorial.contains(&format!("`{payload}`")),
            "the tutorial does not name the `{payload}` payload"
        );
        assert!(
            design.contains(&format!("`{payload}`")) || design.contains(payload),
            "§14.3 does not name the `{payload}` payload"
        );
    }
    // The two ports are in the tutorial's table and in its TLS note.
    for port in [mqtt::PORT_PLAIN, mqtt::PORT_TLS] {
        assert!(
            tutorial.contains(&port.to_string()),
            "the tutorial does not name port {port}"
        );
    }
}

/// §14.3 states the storage rule, so it must name the table and its columns —
/// and must **not** promise a column the row does not have.
#[test]
fn the_design_states_the_columns_of_the_row() {
    let design = design();
    assert!(
        design.contains(STREAMS_TABLE),
        "§14.3 does not name `{STREAMS_TABLE}`"
    );
    for column in [
        COL_NAME,
        COL_DESCRIPTION,
        COL_PROVIDER,
        COL_CONFIGURATION,
        COL_MIN_ROLE,
        COL_ATTRIBUTES,
    ] {
        assert!(
            design.contains(&format!("`{column}`")),
            "§14.3 does not name the `{column}` column"
        );
    }
    // The load-bearing negative: the element type is computed, never stored,
    // and the document says so rather than leaving a reader to assume a column.
    assert!(
        design.contains("`element_type` is **not** a column"),
        "§14.3 no longer says the element type is not a column"
    );
    assert!(
        design.contains("_fd_stream_elements"),
        "§14.3 no longer says out loud that elements are not stored"
    );
}

/// The delivery numbers. A default that moves without the document moving is a
/// reader sizing a broker against a figure that is no longer true.
#[test]
fn both_documents_carry_the_delivery_defaults() {
    let config = StreamConfig::default();
    let design = design();
    assert!(
        design.contains(&format!(
            "{} buffered",
            thousands(config.channel_capacity as u64)
        )),
        "§14.3 does not carry the channel capacity ({})",
        config.channel_capacity
    );
    assert!(
        design.contains(&format!(
            "{} elements/second",
            thousands(config.max_elements_per_second)
        )),
        "§14.3 does not carry the element-rate cap ({})",
        config.max_elements_per_second
    );
    for doc_name in ["docs/TECHNICAL_DESIGN.md", "docs/tutorial-streams.md"] {
        assert!(
            doc(doc_name).contains(&config.ring_capacity.to_string())
                || plain(doc_name).contains("hundred"),
            "{doc_name} does not say how many envelopes the ring replays ({})",
            config.ring_capacity
        );
    }
    // The backoff cap, which is what "started again within a minute" in the
    // tutorial's step 7 rests on.
    assert_eq!(
        config.retry_max.as_secs(),
        60,
        "the backoff cap moved; both documents say a minute"
    );
}

/// `1024` on the wire is `1 024` in the prose, with the thin space this tree's
/// documents use.
fn thousands(n: u64) -> String {
    match n >= 1_000 {
        true => format!("{} {:03}", n / 1_000, n % 1_000),
        false => n.to_string(),
    }
}

/// The envelope is a wire contract, so both documents show it field for field.
#[test]
fn both_documents_show_the_envelope_fields() {
    let tutorial = tutorial();
    let design = design();
    for field in ["stream", "value", "received_at", "source"] {
        assert!(
            tutorial.contains(&format!("\"{field}\"")),
            "the tutorial's envelope has no `{field}`"
        );
        assert!(
            design.contains(&format!("\"{field}\"")),
            "§14.3's envelope has no `{field}`"
        );
    }
}

/// What the tutorial tells a reader to write in a trigger, and what an app's
/// client is called. Both are names this milestone chose, and a document is the
/// only place they are written down for a reader.
#[test]
fn the_tutorial_carries_the_names_a_reader_types() {
    let tutorial = tutorial();
    for phrase in [
        // §8: a stream event reads the envelope, never a row.
        "payload.value.temperature",
        "payload.source.topic",
        // §10 and the live socket: the generated client's accessor, its
        // envelope type, and the socket's path.
        "live.boiler",
        "BoilerEnvelope",
        "{mount}/live",
        // §6: the limitation, said out loud, with the escape hatch.
        "$share/feldspar/house/+/temp",
    ] {
        assert!(
            tutorial.contains(phrase),
            "docs/tutorial-streams.md no longer carries `{phrase}`"
        );
    }
}

/// §14.3 exists, is where it says it is, and carries the three promises the
/// crate's own module docs make. A section that drifts from the crate is the
/// failure this test is for.
#[test]
fn the_design_carries_the_three_promises() {
    let design = design();
    assert!(
        design.contains("### 14.3 Streams: dataflows as an entity (`sc-stream`)"),
        "§14.3 is not where it says it is"
    );
    let plain_design = plain("docs/TECHNICAL_DESIGN.md");
    for promise in [
        "an element is not stored",
        "nobody may block the flow",
        "One process, one subscription",
    ] {
        assert!(
            plain_design.contains(promise),
            "§14.3 no longer states: {promise}"
        );
    }
    // The crate is in the layering, as every other crate is.
    assert!(
        design.contains("| `sc-stream` | `sc-catalog` `sc-db` `sc-error` `sc-query` `sc-types` |"),
        "the direct-dependency table has no `sc-stream` row"
    );
    // The provider seam is an extension point, listed with the others.
    assert!(
        design.contains("| `StreamProvider` | `sc-stream` |"),
        "§2.1's extension-point table does not list `StreamProvider`"
    );
    // The event, in the triggers section rather than only here.
    assert!(
        design.contains("#### `EventKind::Stream`: an event from outside"),
        "§10.2 does not carry the stream event"
    );
}
