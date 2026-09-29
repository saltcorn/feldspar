//! `models` from a Python body (Stan TODO 7.6): a fitted posterior's draws, its
//! summary and the fit, as `op: "models"` requests on the `db` host.
//!
//! No database: what is under test is the lowering — that the Python calls
//! send exactly the requests the JavaScript `models` sends, which is what
//! `sc-api`'s host answers and `sc-server`'s `posterior_api.rs` exercises end to
//! end. The host here echoes what it was asked.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_expr::{CodeCall, CodeHost};
use sc_python::PythonRuntime;
use serde_json::{Value as Json, json};

/// Keeps every request and answers it with itself; a request for a variable
/// called `missing` is refused, as the host refuses one.
#[derive(Default)]
struct Echo {
    requests: Mutex<Vec<Json>>,
}

#[async_trait]
impl CodeHost for Echo {
    async fn call(&self, request: Json) -> Result<Json> {
        self.requests
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        if request.get("variable") == Some(&json!("missing")) {
            return Err(Error::invalid(
                "instance 1 has no variable `missing` (its variables are `alpha`)",
            ));
        }
        Ok(request)
    }
}

async fn run(host: &Arc<Echo>, code: &str) -> Result<Json> {
    PythonRuntime::new()
        .run(CodeCall {
            code: code.to_owned(),
            host: Some(host.as_ref() as &dyn CodeHost),
            ..CodeCall::default()
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_lowers_to_the_requests_the_javascript_models_sends() {
    let host = Arc::new(Echo::default());
    let answer = run(
        &host,
        r#"
d = models.draws("Radon", "alpha", keys=["Anoka"], chains=[1, 2], thin=10)
s = models.summary("Radon", "alpha", elements={"counties": ["27001"]})
i = models.instance("Radon")
return [d, s, i]
"#,
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        json!([
            { "op": "models", "what": "draws", "model": "Radon", "variable": "alpha",
              "elements": { "1": ["Anoka"] }, "chains": [1, 2], "warmup": false, "thin": 10 },
            { "op": "models", "what": "summary", "model": "Radon", "variable": "alpha",
              "elements": { "counties": ["27001"] } },
            { "op": "models", "what": "instance", "model": "Radon" },
        ])
    );
    assert_eq!(host.requests.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_refuses_what_it_can_see_is_wrong_and_raises_what_the_host_refuses() {
    let host = Arc::new(Echo::default());
    let err = run(&host, r#"models.draws("Radon")"#).await.unwrap_err();
    assert!(err.to_string().contains("draws()"), "{err}");
    let err = run(
        &host,
        r#"models.summary("Radon", "alpha", keys=[1], elements=[[1]])"#,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("either keys= or elements="),
        "{err}"
    );
    let err = run(&host, r#"models.draws("Radon", "alpha", thin=0)"#)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("thin="), "{err}");
    // A refusal is the surface's own error, so a body can catch it.
    let answer = run(
        &host,
        r#"
import saltcorn
try:
    models.draws("Radon", "missing")
except saltcorn.DbError as e:
    return str(e)
"#,
    )
    .await
    .unwrap();
    assert!(
        answer
            .as_str()
            .unwrap()
            .contains("has no variable `missing`"),
        "{answer}"
    );
    // No request was sent for any of the mistakes the handle could see.
    assert_eq!(host.requests.lock().unwrap().len(), 1);
}
