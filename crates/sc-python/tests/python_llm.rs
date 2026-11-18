//! `llm` from a Python body: `llm.generate` and `llm.embed` lower to the
//! `op: "llm"` requests on the `db` host that the JavaScript `llm` sends, which
//! is what `sc-api`'s host answers (`code_host_llm.rs` there, end to end).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_error::Result;
use sc_expr::{CodeCall, CodeHost};
use sc_python::PythonRuntime;
use serde_json::{Value as Json, json};

/// Keeps every request; answers `generate` with `{ text: "hi", … }` and `embed`
/// with `[len]` per text.
#[derive(Default)]
struct Echo {
    requests: Mutex<Vec<Json>>,
}

#[async_trait]
impl CodeHost for Echo {
    async fn call(&self, request: Json) -> Result<Json> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(match request["what"].as_str() {
            Some("generate") => json!({ "text": "hi", "provider": "stub", "model": "m",
                                        "usage": {}, "cost": null }),
            _ => Json::Array(
                request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| json!([t.as_str().unwrap().len()]))
                    .collect(),
            ),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn llm_lowers_to_the_requests_the_javascript_llm_sends() {
    let host = Arc::new(Echo::default());
    let answer = PythonRuntime::new()
        .run(CodeCall {
            code: r#"
one = llm.generate("Hello")
full = llm.generate("c", messages=[{"role": "user", "content": "a"}], system="Be brief.",
                    model="m", temperature=0, detail=True)
try:
    llm.embed(7)
    refused = None
except TypeError as e:
    refused = str(e)
return [one, full["provider"], llm.embed("four"), llm.embed(["a", "bb"], provider="stub"),
        refused]
"#
            .to_owned(),
            host: Some(host.as_ref() as &dyn CodeHost),
            ..CodeCall::default()
        })
        .await
        .unwrap();
    assert_eq!(
        answer,
        json!([
            "hi",
            "stub",
            [4],
            [[1], [2]],
            "llm.embed() takes text, not int"
        ])
    );
    let requests = host.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert_eq!(
        requests[0],
        json!({ "op": "llm", "what": "generate",
                "messages": [{ "role": "user", "content": "Hello" }] })
    );
    assert_eq!(
        requests[1],
        json!({ "op": "llm", "what": "generate", "system": "Be brief.", "model": "m",
                "temperature": 0,
                "messages": [{ "role": "user", "content": "a" },
                             { "role": "user", "content": "c" }] })
    );
    assert_eq!(
        requests[3],
        json!({ "op": "llm", "what": "embed", "input": ["a", "bb"], "provider": "stub" })
    );
}
