//! `live.publish` and `live.document` from a Python body (TODO.md "Live updates" L2.8, L5.5).
//!
//! No database and no hub: what a real [`LiveHost`] adds over the one here is
//! the running stream, which `sc-server`'s socket tests drive. What is under
//! test is the surface — the three-argument and two-argument spellings, the
//! topic as text or a number, the order of a loop's publishes, a refusal as a
//! catchable `saltcorn.LiveError`, and that a body with no host cannot name `live`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use sc_error::{Error, Result};
use sc_expr::{CodeCall, DocRequest, LiveHost, LiveTopics};
use sc_python::PythonRuntime;
use serde_json::{Value as Json, json};

/// A host that records what it was asked to publish and refuses a stream
/// called `stopped`, the way the hub refuses one that is not running.
#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<(String, Option<String>, Json)>>,
}

impl LiveHost for Recorder {
    fn topics(&self, _stream: &str) -> Result<LiveTopics> {
        Ok(LiveTopics::User)
    }

    fn publish(&self, stream: &str, topic: Option<String>, value: Json) -> Result<Option<String>> {
        if stream == "stopped" {
            return Err(Error::invalid(
                "stream `stopped` is not running on this server",
            ));
        }
        self.published
            .lock()
            .unwrap()
            .push((stream.to_owned(), topic.clone(), value));
        Ok(topic)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_publishes_in_order_and_a_refusal_is_a_live_error() {
    let recorder = Arc::new(Recorder::default());
    let host: Arc<dyn LiveHost> = recorder.clone();
    let out = PythonRuntime::new()
        .run(CodeCall {
            code: r#"
import saltcorn
for i in range(1, 11):
    live.publish("job_status", "u-1", {"message": "step", "progress": i / 10})
live.publish("announcements", {"text": "no topic"})
numbered = live.publish("board_activity", 7, {"n": 1})
try:
    live.publish("stopped", None, {})
    refused = None
except saltcorn.LiveError as e:
    refused = str(e)
return {"numbered": numbered, "refused": refused}
"#
            .to_owned(),
            live: Some(&host),
            ..CodeCall::default()
        })
        .await
        .expect("the body runs");
    assert_eq!(
        out["numbered"],
        json!("7"),
        "a number topic crosses as text"
    );
    assert!(
        out["refused"].as_str().unwrap().contains("not running"),
        "{out}"
    );
    let published = recorder.published.lock().unwrap().clone();
    assert_eq!(published.len(), 12);
    for (i, (stream, topic, value)) in published[..10].iter().enumerate() {
        assert_eq!(stream, "job_status");
        assert_eq!(topic.as_deref(), Some("u-1"));
        let expected = (i as f64 + 1.0) / 10.0;
        assert!((value["progress"].as_f64().unwrap() - expected).abs() < 1e-9);
    }
    assert_eq!(
        published[10],
        (
            "announcements".to_owned(),
            None,
            json!({ "text": "no topic" })
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_without_a_live_host_cannot_name_live() {
    let error = PythonRuntime::new()
        .run(CodeCall {
            code: r#"live.publish("job_status", None, {})"#.to_owned(),
            ..CodeCall::default()
        })
        .await
        .expect_err("`live` is not bound")
        .to_string();
    assert!(error.contains("live"), "{error}");
    assert!(error.contains("NameError"), "{error}");
}

/// One document, as the hub's `live.document` answers: its text after each
/// request (L5.5).
#[derive(Default)]
struct Notes {
    text: Mutex<String>,
}

#[async_trait::async_trait]
impl LiveHost for Notes {
    fn topics(&self, _stream: &str) -> Result<LiveTopics> {
        Ok(LiveTopics::Single)
    }

    fn publish(&self, _: &str, topic: Option<String>, _: Json) -> Result<Option<String>> {
        Ok(topic)
    }

    async fn document(&self, stream: &str, key: &str, request: DocRequest) -> Result<String> {
        if stream != "note_body" || key != "7" {
            return Err(Error::invalid(format!("no document `{stream}`/{key}")));
        }
        let mut text = self.text.lock().unwrap();
        match request {
            DocRequest::Read => {}
            DocRequest::Append { text: more } => text.push_str(&more),
            DocRequest::Edit {
                index,
                delete,
                insert,
            } => {
                let (i, d) = (index as usize, delete as usize);
                text.replace_range(i..i + d, &insert);
            }
        }
        Ok(text.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_python_body_reads_and_edits_a_document() {
    let notes = Arc::new(Notes::default());
    let host: Arc<dyn LiveHost> = notes.clone();
    let out = PythonRuntime::new()
        .run(CodeCall {
            code: r#"
import saltcorn
doc = live.document("note_body", 7)
empty = doc.text()
doc.append("Hello world")
doc.insert(0, ">> ")
doc.delete(0, 3)
edited = doc.edit(6, 5, "there")
try:
    live.document("note_body", 8).text()
    refused = None
except saltcorn.LiveError as e:
    refused = str(e)
try:
    doc.insert(-1, "x")
    typed = None
except TypeError as e:
    typed = str(e)
return {"empty": empty, "edited": edited, "refused": refused, "typed": typed}
"#
            .to_owned(),
            live: Some(&host),
            ..CodeCall::default()
        })
        .await
        .expect("the body runs");
    assert_eq!(out["empty"], json!(""));
    assert_eq!(out["edited"], json!("Hello there"));
    assert!(
        out["refused"].as_str().unwrap().contains("no document"),
        "{out}"
    );
    assert!(
        out["typed"].as_str().unwrap().contains("whole number"),
        "{out}"
    );
    assert_eq!(*notes.text.lock().unwrap(), "Hello there");
}
