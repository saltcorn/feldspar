//! The model editor's API against a real Postgres (analytics TODO A3.3–A3.4):
//! a model's outputs with their plots drawn, a fit's progress pushed over a
//! socket, the list of fits saying which are out of date, and the view state
//! beside the model.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::StreamExt;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// Sixty houses whose price is close to a linear function of area and
/// bedrooms.
const SCHEMA: &str = "
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        bedrooms bigint,
        price double precision
    );
    INSERT INTO houses (id, area, bedrooms, price)
      SELECT i, 50 + i, 1 + (i % 5), 1000 * (50 + i) + 20000 * (1 + (i % 5)) + 500 * (i % 7)
      FROM generate_series(1, 60) AS i;
";

struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    async fn refused(&mut self, method: &str, path: &str, body: Option<Value>) -> String {
        let (status, value) = self.send(method, path, body).await;
        assert!(
            status.is_client_error(),
            "{method} {path}: {status} {value}"
        );
        value["error"].as_str().unwrap_or_default().to_owned()
    }
}

struct Server {
    client: Client,
    addr: std::net::SocketAddr,
    _db: TestDb,
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl Server {
    /// Open the progress socket of fit `id`, as the admin or as nobody.
    async fn progress(
        &self,
        id: &str,
        as_admin: bool,
    ) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://{}/api/model-instances/{id}/progress", self.addr)
            .into_client_request()
            .unwrap();
        if as_admin && let Some(token) = self.client.cookies.get(sc_server::SESSION_COOKIE) {
            request.headers_mut().insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{}={token}", sc_server::SESSION_COOKIE)).unwrap(),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(stream, _)| stream)
    }
}

/// Every frame a socket sends until it closes, and the close reason.
async fn frames(mut socket: Socket) -> (Vec<Value>, Option<String>) {
    let mut out = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(60), socket.next())
            .await
            .expect("the socket went quiet");
        match message {
            Some(Ok(Message::Text(text))) => out.push(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(frame))) => {
                return (out, frame.map(|f| f.reason.to_string()));
            }
            Some(Ok(_)) => {}
            None | Some(Err(_)) => return (out, None),
        }
    }
}

async fn serve() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    // For a calculated field that predicts (`what_uses_a_model…`).
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_triggers(dispatcher)
            .with_models(models),
    );
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    client
        .ok(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    Ok(Server {
        client,
        addr,
        _db: db,
    })
}

fn op(id: &str, kind: &str, params: Value) -> Value {
    json!({ "id": id, "enabled": true, "kind": kind, "params": params })
}

/// A dataset of `price`, `area` and `bedrooms` over `houses`, and a linear
/// regression of price on it: the model's id and the dataset's.
async fn house_prices(client: &mut Client) -> (String, String) {
    let dataset = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({
                "name": "prices",
                "base": { "kind": "table", "table": "houses" },
                "operations": [
                    op("a", "select", json!({ "columns": [
                        { "column": "price" }, { "column": "area" }, { "column": "bedrooms" }
                    ]})),
                ],
            })),
        )
        .await;
    let dataset_id = dataset["dataset"]["id"].as_str().unwrap().to_owned();
    let model = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "House prices",
                "provider": "linear_regression",
                "dataset": { "dataset_id": dataset_id },
                "configuration": { "label": "price" },
                "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
            })),
        )
        .await;
    (model["id"].as_str().unwrap().to_owned(), dataset_id)
}

fn output<'a>(outputs: &'a Value, name: &str) -> &'a Value {
    outputs["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == name)
        .unwrap_or_else(|| panic!("no output {name} in {outputs}"))
}

#[tokio::test]
async fn a_fit_is_watched_over_its_socket_and_its_outputs_are_drawn() -> sc_error::Result<()> {
    let mut server = serve().await?;
    let (model, dataset) = house_prices(&mut server.client).await;
    let client = &mut server.client;

    // Never fitted: nothing to show, and nothing refused.
    let none = client
        .ok("GET", &format!("/api/models/{model}/outputs"), None)
        .await;
    assert_eq!(none["fit"], Value::Null);
    assert_eq!(none["outputs"], json!([]));

    // Fit it and watch: whatever progress frames there were, the last frame
    // says how it finished, and the socket then closes.
    let started = client
        .ok("POST", &format!("/api/models/{model}/fit"), Some(json!({})))
        .await;
    let fit = started["id"].as_str().unwrap().to_owned();
    let socket = server
        .progress(&fit, true)
        .await
        .expect("an admin connects");
    let (sent, reason) = frames(socket).await;
    let last = sent.last().expect("at least the finished frame");
    assert_eq!(last["type"], "finished", "{sent:?}");
    assert_eq!(last["status"], "fitted", "{last}");
    assert_eq!(last["error"], Value::Null);
    assert_eq!(reason.as_deref(), Some("the fit has finished"));
    for frame in &sent[..sent.len() - 1] {
        assert_eq!(frame["type"], "progress", "{frame}");
        let stage = frame["progress"]["stage"].as_str().unwrap_or("reading");
        assert!(
            ["reading", "fitting", "scoring"].contains(&stage),
            "{frame}"
        );
    }
    // A finished fit's socket sends only how it finished.
    let (again, _) = frames(server.progress(&fit, true).await.unwrap()).await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0]["type"], "finished");
    // Nobody's socket is refused before the upgrade; a fit that is not there
    // is closed with a reason.
    assert!(server.progress(&fit, false).await.is_err());
    let (nothing, reason) = frames(
        server
            .progress(&uuid::Uuid::new_v4().to_string(), true)
            .await
            .unwrap(),
    )
    .await;
    assert!(nothing.is_empty());
    assert!(reason.unwrap_or_default().contains("there is no fit"));

    // The newest fitted fit is the one shown, though none is active yet.
    let client = &mut server.client;
    let outputs = client
        .ok("GET", &format!("/api/models/{model}/outputs"), None)
        .await;
    assert_eq!(outputs["fit"]["id"], json!(fit));
    assert_eq!(outputs["fit"]["dataset_changed"], json!(false));
    let names: Vec<&str> = outputs["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "coefficients",
            "statistics",
            "metrics",
            "residuals_fitted",
            "actual_predicted",
            "qq",
            "residual_histogram"
        ]
    );
    // The coefficient table, read from the fit: the intercept and the two
    // slopes.
    let coefficients = output(&outputs, "coefficients");
    assert_eq!(coefficients["kind"], "table");
    assert_eq!(coefficients["table"]["rows"].as_array().unwrap().len(), 3);
    let metrics = output(&outputs, "metrics");
    assert_eq!(
        metrics["table"]["columns"],
        json!(["metric", "train", "test"])
    );
    // The residual plot is drawn, its points the scored rows: every house.
    let residuals = output(&outputs, "residuals_fitted");
    assert_eq!(residuals["kind"], "plot");
    assert_eq!(residuals["spec"]["data"]["kind"], "fit_output");
    assert_eq!(residuals["spec"]["data"]["instance"], json!(fit));
    let points = &residuals["plot"]["layers"][0];
    assert_eq!(points["rows"].as_array().unwrap().len(), 60, "{points}");
    // The Q-Q plot is optional: its spec, but not drawn …
    let qq = output(&outputs, "qq");
    assert_eq!(qq["optional"], json!(true));
    assert!(qq["plot"].is_null());
    // … until it is asked for, here or by its spec.
    let drawn = client
        .ok(
            "GET",
            &format!("/api/models/{model}/outputs?fit={fit}&include=qq"),
            None,
        )
        .await;
    assert!(output(&drawn, "qq")["plot"]["layers"].is_array());
    let rendered = client
        .ok(
            "POST",
            "/api/plots/render",
            Some(json!({ "spec": qq["spec"] })),
        )
        .await;
    assert_eq!(
        rendered["layers"][0]["rows"].as_array().unwrap().len(),
        60,
        "{rendered}"
    );

    // Edit the dataset: the fit says so, in the outputs and in the list.
    let mut def = client
        .ok("GET", &format!("/api/datasets/{dataset}"), None)
        .await["dataset"]
        .clone();
    def["operations"].as_array_mut().unwrap().push(op(
        "b",
        "filter",
        json!({ "formula": "area > 60" }),
    ));
    client
        .ok("PUT", &format!("/api/datasets/{dataset}"), Some(def))
        .await;
    let outputs = client
        .ok("GET", &format!("/api/models/{model}/outputs"), None)
        .await;
    assert_eq!(outputs["fit"]["dataset_changed"], json!(true));
    let fits = client
        .ok("GET", &format!("/api/models/{model}/instances"), None)
        .await;
    assert_eq!(fits[0]["dataset_changed"], json!(true), "{fits}");

    // A fit of another model, or no fit at all, is refused by name.
    let refused = client
        .refused(
            "GET",
            &format!("/api/models/{model}/outputs?fit={}", uuid::Uuid::new_v4()),
            None,
        )
        .await;
    assert!(refused.contains("no model instance"), "{refused}");
    Ok(())
}

#[tokio::test]
async fn the_view_state_is_patched_beside_the_model_and_a_clone_carries_it() -> sc_error::Result<()>
{
    let mut server = serve().await?;
    let (model, _) = house_prices(&mut server.client).await;
    let client = &mut server.client;

    let got = client
        .ok("GET", &format!("/api/models/{model}"), None)
        .await;
    assert_eq!(got["view_state"], json!({}));

    // Two screens' keys, patched one after the other: both are kept.
    let path = format!("/api/models/{model}/view-state");
    client
        .ok(
            "PATCH",
            &path,
            Some(json!({ "patch": { "open": ["qq"], "collapsed": ["coefficients"] } })),
        )
        .await;
    let patched = client
        .ok(
            "PATCH",
            &path,
            Some(json!({ "patch": { "compare": { "with": "x" }, "collapsed": null } })),
        )
        .await;
    assert_eq!(
        patched["view_state"],
        json!({ "open": ["qq"], "compare": { "with": "x" } })
    );
    let refused = client
        .refused("PATCH", &path, Some(json!({ "patch": [1] })))
        .await;
    assert!(refused.contains("`patch`"), "{refused}");

    // A fit, then a patch: the fit is not out of date.
    let started = client
        .ok("POST", &format!("/api/models/{model}/fit"), Some(json!({})))
        .await;
    let fit = started["id"].as_str().unwrap().to_owned();
    let (sent, _) = frames(server.progress(&fit, true).await.unwrap()).await;
    assert_eq!(sent.last().unwrap()["status"], "fitted");
    let client = &mut server.client;
    client
        .ok("PATCH", &path, Some(json!({ "patch": { "fit": fit } })))
        .await;
    let fits = client
        .ok("GET", &format!("/api/models/{model}/instances"), None)
        .await;
    assert_eq!(fits[0]["dataset_changed"], json!(false), "{fits}");

    // Saving the model's definition leaves the view state alone.
    let mut definition = client
        .ok("GET", &format!("/api/models/{model}"), None)
        .await;
    definition["description"] = json!("edited");
    definition["dataset"] = json!({ "dataset_id": definition["dataset"]["dataset_id"] });
    let saved = client.ok("POST", "/api/models", Some(definition)).await;
    let expected = json!({ "open": ["qq"], "compare": { "with": "x" }, "fit": fit });
    assert_eq!(saved["view_state"], expected);
    let got = client
        .ok("GET", &format!("/api/models/{model}"), None)
        .await;
    assert_eq!(got["description"], "edited");
    assert_eq!(got["view_state"], expected);

    // A clone carries it, under a free name, with no fits.
    let copy = client
        .ok(
            "POST",
            &format!("/api/models/{model}/clone"),
            Some(json!({})),
        )
        .await;
    assert_eq!(copy["name"], "House prices (copy)");
    assert_eq!(copy["view_state"], expected);
    assert_eq!(copy["instances"], json!(0));
    let named = client
        .ok(
            "POST",
            &format!("/api/models/{model}/clone"),
            Some(json!({ "name": "Bigger model" })),
        )
        .await;
    assert_eq!(named["name"], "Bigger model");
    Ok(())
}

/// What uses a model (analytics TODO A3.5), for the model list's delete
/// warning: a calculated field calling `predict("…")` on it, a code body
/// naming it with `models.get("…")`, and a workflow whose `fit_model` step
/// fits it — and nothing for a model nobody names.
#[tokio::test]
async fn what_uses_a_model_is_listed_for_the_delete_warning() -> sc_error::Result<()> {
    let mut server = serve().await?;
    let (model, dataset) = house_prices(&mut server.client).await;
    let client = &mut server.client;
    let usage = format!("/api/models/{model}/usage");
    assert_eq!(
        client.ok("GET", &usage, None).await,
        json!({ "fields": [], "triggers": [], "workspaces": [] })
    );

    // A field that predicts with it needs an active fit to be saved.
    let started = client
        .ok("POST", &format!("/api/models/{model}/fit"), Some(json!({})))
        .await;
    let fit = started["id"].as_str().unwrap().to_owned();
    for _ in 0..400 {
        let got = client
            .ok("GET", &format!("/api/model-instances/{fit}"), None)
            .await;
        if got["status"] != json!("fitting") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    client
        .ok(
            "POST",
            &format!("/api/model-instances/{fit}/activate"),
            None,
        )
        .await;
    client
        .ok(
            "POST",
            "/api/tables/houses/fields",
            Some(json!({
                "name": "guess", "type": "float8",
                "kind": { "type": "calc", "expression": "predict(\"House prices\")" },
            })),
        )
        .await;
    let code = client
        .ok(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "report", "description": "", "when": "none", "channel": null,
                "only_if": null, "action": "run_js_code",
                "configuration": { "code": "const m = await models.get(\"House prices\"); return m.fit.id;" },
                "min_role": null, "enabled": true,
            })),
        )
        .await;
    let flow = client
        .ok(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "nightly", "description": "", "when": "none", "channel": null,
                "only_if": null, "body": "workflow", "action": null, "configuration": null,
                "min_role": null, "enabled": true,
            })),
        )
        .await;
    let flow_id = flow["id"].as_str().unwrap().to_owned();
    client
        .ok(
            "POST",
            &format!("/api/workflows/{flow_id}"),
            Some(json!({ "description": "refit", "workflow": {
                "start": "refit",
                "steps": [{
                    "name": "refit",
                    "kind": { "type": "action", "action": "fit_model",
                              "configuration": { "model": "House prices" } },
                    "next": { "type": "end" },
                }],
            } })),
        )
        .await;

    let used = client.ok("GET", &usage, None).await;
    assert_eq!(
        used["fields"],
        json!([{ "table": "houses", "field": "guess" }])
    );
    let mut triggers = used["triggers"].as_array().unwrap().clone();
    triggers.sort_by_key(|t| t["name"].as_str().unwrap().to_owned());
    assert_eq!(
        triggers,
        vec![
            json!({ "id": flow_id, "name": "nightly", "how": "fits" }),
            json!({ "id": code["id"], "name": "report", "how": "names" }),
        ]
    );

    // Another model on the same dataset is named by none of them.
    let other = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "House prices, again",
                "provider": "linear_regression",
                "dataset": { "dataset_id": dataset },
                "configuration": { "label": "price" },
            })),
        )
        .await;
    let other = other["id"].as_str().unwrap();
    assert_eq!(
        client
            .ok("GET", &format!("/api/models/{other}/usage"), None)
            .await,
        json!({ "fields": [], "triggers": [], "workspaces": [] })
    );
    let missing = uuid_like();
    let (status, _) = client
        .send("GET", &format!("/api/models/{missing}/usage"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// An id no model has.
fn uuid_like() -> &'static str {
    "00000000-0000-4000-8000-000000000000"
}
