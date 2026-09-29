//! `models` from a Python body (milestone 31 §3): `models.get(name)` and the
//! handle it answers, as `op: "models"` requests on the `db` host.
//!
//! No database: what is under test is the lowering — that the Python handle
//! sends exactly the requests the JavaScript handle sends, which is what
//! `sc-api`'s host answers and `sc-server`'s `model_handle.rs` exercises end to
//! end. The host here answers `get` with a model and echoes everything else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_expr::{CodeCall, CodeHost};
use sc_python::PythonRuntime;
use serde_json::{Value as Json, json};

/// Keeps every request; answers `get` with `House prices` (a regression) or
/// `Radon` (a posterior), `predict` with each row's `id` times ten, and
/// everything else with itself. A request for a variable called `missing` is
/// refused, as the host refuses one.
#[derive(Default)]
struct Echo {
    requests: Mutex<Vec<Json>>,
}

fn fit(id: &str) -> Json {
    json!({ "id": id, "name": "fit", "status": "fitted", "active": true,
            "warnings": [], "metrics": {}, "parameters": [] })
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
        Ok(match request["what"].as_str() {
            Some("get") if request["model"] == json!("Radon") => json!({
                "name": "Radon", "provider": "stan", "table": "homes",
                "outcome": { "outcome": "posterior" }, "fit": fit("f-radon"),
                "variables": ["alpha", "beta"],
                "no_prediction": "`Radon` is a posterior: read its draws in a code body \
                                  (`m.draws(…)`, on `models.get(…)`)",
            }),
            Some("get") => json!({
                "name": request["model"], "provider": "linear_regression", "table": "houses",
                "outcome": { "outcome": "regression", "label": "price" },
                "fit": fit(request.get("fit").and_then(Json::as_str).unwrap_or("f-houses")),
                "variables": null, "no_prediction": null,
            }),
            Some("predict") => Json::Array(
                request["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| json!(r["id"].as_i64().unwrap_or(0) * 10))
                    .collect(),
            ),
            _ => request,
        })
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
async fn a_model_handle_lowers_to_the_requests_the_javascript_handle_sends() {
    let host = Arc::new(Echo::default());
    let answer = run(
        &host,
        r#"
m = models.get("House prices")
r = models.get("Radon")
d = r.draws("alpha", keys=["Anoka"], chains=[1, 2], thin=10)
s = r.summary("alpha", elements={"counties": ["27001"]})
w = r.as_user().write_posterior(variable="alpha", statistics={"mean": "alpha_mean"})
return [m.name, m.table, m.fit["id"], m.predict({"id": 3}),
        m.predict([{"id": 1}, {"area": 90}], detail=True), list(r.variables), d, s, w]
"#,
    )
    .await
    .unwrap();
    assert_eq!(answer[0], json!("House prices"));
    assert_eq!(answer[1], json!("houses"));
    assert_eq!(answer[2], json!("f-houses"));
    assert_eq!(answer[3], json!(30));
    assert_eq!(answer[4], json!([10, 0]));
    assert_eq!(answer[5], json!(["alpha", "beta"]));
    assert_eq!(
        answer[6],
        json!({ "op": "models", "what": "draws", "model": "Radon", "fit": "f-radon",
                "variable": "alpha", "elements": { "1": ["Anoka"] }, "chains": [1, 2],
                "warmup": false, "thin": 10 })
    );
    assert_eq!(
        answer[7],
        json!({ "op": "models", "what": "summary", "model": "Radon", "fit": "f-radon",
                "variable": "alpha", "elements": { "counties": ["27001"] } })
    );
    assert_eq!(
        answer[8],
        json!({ "op": "models", "what": "write_posterior", "model": "Radon", "fit": "f-radon",
                "write": { "variable": "alpha", "statistics": { "mean": "alpha_mean" } },
                "authority": "user" })
    );
    let requests = host.requests.lock().unwrap().clone();
    // Two gets, two predicts (one row is a batch of one), draws, summary, write.
    assert_eq!(requests.len(), 7, "{requests:?}");
    assert_eq!(
        requests[6],
        json!({ "op": "models", "what": "predict", "model": "House prices", "fit": "f-houses",
                "rows": [{ "id": 1 }, { "area": 90 }], "detail": true })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_posterior_methods_are_absent_on_another_model_and_say_why() {
    let host = Arc::new(Echo::default());
    let answer = run(
        &host,
        r#"
import saltcorn
m = models.get("House prices")
said = [hasattr(m, "draws")]
try:
    m.draws("alpha")
except AttributeError as e:
    said.append(str(e))
r = models.get("Radon")
try:
    r.predict({"id": 1})
except saltcorn.DbError as e:
    said.append(str(e))
return said
"#,
    )
    .await
    .unwrap();
    assert_eq!(answer[0], json!(false));
    assert_eq!(
        answer[1],
        json!("`House prices` is a linear_regression regression; `draws` is for posterior models")
    );
    assert!(
        answer[2].as_str().unwrap().contains("`m.draws(…)`"),
        "{answer}"
    );
    // Only the two gets reached the host.
    assert_eq!(host.requests.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handle_refuses_what_it_can_see_is_wrong_and_raises_what_the_host_refuses() {
    let host = Arc::new(Echo::default());
    let err = run(&host, r#"models.get("Radon").draws()"#)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("draws()"), "{err}");
    let err = run(
        &host,
        r#"models.get("Radon").summary("alpha", keys=[1], elements=[[1]])"#,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("either keys= or elements="),
        "{err}"
    );
    let err = run(&host, r#"models.get("Radon").draws("alpha", thin=0)"#)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("thin="), "{err}");
    // A refusal is the surface's own error, so a body can catch it.
    let answer = run(
        &host,
        r#"
import saltcorn
try:
    models.get("Radon").draws("missing")
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
    // Four gets, and the one request the host refused: none for the mistakes
    // the handle could see.
    assert_eq!(host.requests.lock().unwrap().len(), 5);
}
