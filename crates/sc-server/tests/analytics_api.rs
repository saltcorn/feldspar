//! The Analytics UI's API against a real Postgres (analytics TODO A1.8, A1.9,
//! A1.13): datasets and workspaces through their endpoints, and what named
//! datasets change for models — a fit that notices its dataset was edited, and
//! a model of an aggregate that `predict("…")` refuses on the table.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
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
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// Sixty houses whose price is an exact linear function of area and bedrooms,
/// in two neighbourhoods.
const SCHEMA: &str = "
    CREATE TABLE neighbourhoods (id bigint primary key, name text);
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        bedrooms bigint,
        price double precision,
        neighbourhood bigint references neighbourhoods(id)
    );
    INSERT INTO neighbourhoods VALUES (1, 'North'), (2, 'South');
    INSERT INTO houses (id, area, bedrooms, price, neighbourhood)
      SELECT i, 50 + i, 1 + (i % 5), 1000 * (50 + i) + 20000 * (1 + (i % 5)), 1 + (i % 2)
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
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
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

    /// Fit model `id`, wait for it, activate it, and answer the instance.
    async fn fit(&mut self, id: &str) -> Value {
        let started = self
            .ok("POST", &format!("/api/models/{id}/fit"), Some(json!({})))
            .await;
        let instance = started["id"].as_str().unwrap().to_owned();
        for _ in 0..600 {
            let body = self
                .ok("GET", &format!("/api/model-instances/{instance}"), None)
                .await;
            if body["status"] != json!("fitting") {
                assert_eq!(body["status"], json!("fitted"), "{body}");
                self.ok(
                    "POST",
                    &format!("/api/model-instances/{instance}/activate"),
                    None,
                )
                .await;
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the fit never finished");
    }
}

async fn setup() -> sc_error::Result<(Client, TestDb)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
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
    Ok((client, db))
}

fn op(id: &str, kind: &str, params: Value) -> Value {
    json!({ "id": id, "enabled": true, "kind": kind, "params": params })
}

/// A dataset of `price`, `area` and `bedrooms` over `houses`.
fn prices(name: &str) -> Value {
    json!({
        "name": name,
        "base": { "kind": "table", "table": "houses" },
        "operations": [
            op("a", "select", json!({ "columns": [
                { "column": "price" }, { "column": "area" }, { "column": "bedrooms" }
            ]})),
        ],
    })
}

#[tokio::test]
async fn datasets_are_created_read_edited_and_deleted_through_the_api() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // The base picker's tables, with their columns and keys.
    let tables = client.ok("GET", "/api/datasets/tables", None).await;
    let houses = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "houses")
        .expect("houses");
    assert_eq!(houses["primary_key"], json!("id"));
    let hood = houses["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "neighbourhood")
        .unwrap();
    assert_eq!(hood["key"]["table"], json!("neighbourhoods"));

    // Create, on a table, with no operations yet.
    let created = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "House prices by area",
                         "base": { "kind": "table", "table": "houses" } })),
        )
        .await;
    let id = created["dataset"]["id"].as_str().unwrap().to_owned();
    let columns = created["report"]["base"]["shape"]["columns"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(columns, 5);
    // A second one of the same name is refused with a sentence.
    let err = client
        .refused(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "House prices by area",
                         "base": { "kind": "table", "table": "houses" } })),
        )
        .await;
    assert!(err.contains("already exists"), "{err}");

    // The editor reads a stage of a definition it has not saved yet.
    let mut def = created["dataset"].clone();
    def["operations"] = json!([
        op(
            "c1",
            "calculated",
            json!({ "name": "price_per_m2", "formula": "price / area" })
        ),
        op(
            "c2",
            "calculated",
            json!({ "name": "hood", "formula": "neighbourhoodⱵname" })
        ),
        op("f1", "filter", json!({ "formula": "price > 100000" })),
        op(
            "a1",
            "aggregate",
            json!({
                "group_by": [{ "name": "neighbourhood", "formula": "neighbourhood" }],
                "summaries": [
                    { "name": "mean_ppm", "function": "mean", "column": "price_per_m2" },
                    { "name": "n", "function": "count" }
                ]
            })
        ),
    ]);
    let stage = client
        .ok(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": def, "upto": 2, "limit": 5 })),
        )
        .await;
    assert_eq!(stage["total"], json!(60));
    assert_eq!(stage["rows"].as_array().unwrap().len(), 5);
    let names: Vec<&str> = stage["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "id",
            "area",
            "bedrooms",
            "price",
            "neighbourhood",
            "price_per_m2",
            "hood"
        ]
    );
    let last = client
        .ok(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": def })),
        )
        .await;
    assert_eq!(last["total"], json!(2));
    assert_eq!(last["grain"]["kind"], json!("group"));

    // The shapes: every operation's, and what a formula may name.
    let shapes = client
        .ok(
            "POST",
            "/api/datasets/shapes",
            Some(json!({ "dataset": def })),
        )
        .await;
    assert_eq!(shapes["operations"].as_array().unwrap().len(), 4);
    assert!(shapes["tables"]["neighbourhoods"].is_array());
    assert_eq!(
        shapes["children"]["neighbourhoods"][0]["table"],
        json!("houses")
    );

    // One operation checked where it would go.
    let bad = client
        .ok(
            "POST",
            "/api/datasets/validate",
            Some(json!({ "dataset": def, "position": 4,
                         "operation": op("x", "filter", json!({ "formula": "price > 1" })) })),
        )
        .await;
    assert!(
        bad["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`price`"),
        "{bad}"
    );
    let good = client
        .ok(
            "POST",
            "/api/datasets/validate",
            Some(json!({ "dataset": def, "position": 4,
                         "operation": op("x", "filter", json!({ "formula": "n > 1" })) })),
        )
        .await;
    assert_eq!(good["error"], Value::Null, "{good}");
    assert_eq!(good["shape"]["columns"].as_array().unwrap().len(), 3);

    // A column's values, most frequent first.
    let values = client
        .ok(
            "POST",
            "/api/datasets/values",
            Some(json!({ "dataset": def, "upto": 2, "column": "hood" })),
        )
        .await;
    assert_eq!(values.as_array().unwrap().len(), 2);

    // Save the operations; a changed base is refused.
    let mut body = def.clone();
    let updated = client
        .ok("PUT", &format!("/api/datasets/{id}"), Some(body.clone()))
        .await;
    assert_eq!(
        updated["dataset"]["operations"].as_array().unwrap().len(),
        4
    );
    body["base"] = json!({ "kind": "table", "table": "neighbourhoods" });
    let err = client
        .refused("PUT", &format!("/api/datasets/{id}"), Some(body))
        .await;
    assert!(err.contains("cannot be changed"), "{err}");

    // Listed, got, cloned, and a dataset over it reads its operations first.
    let listed = client.ok("GET", "/api/datasets", None).await;
    assert_eq!(listed[0]["operations"], json!(4));
    assert_eq!(listed[0]["error"], Value::Null);
    let got = client.ok("GET", &format!("/api/datasets/{id}"), None).await;
    assert_eq!(got["report"]["operations"][3]["status"], json!("ok"));
    let copy = client
        .ok(
            "POST",
            &format!("/api/datasets/{id}/clone"),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        copy["dataset"]["name"],
        json!("House prices by area (copy)")
    );
    let over = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "over", "base": { "kind": "dataset", "dataset": id } })),
        )
        .await;
    assert_eq!(
        over["report"]["base"]["shape"]["grain"]["keys"],
        json!(["neighbourhood"])
    );

    // What reads it, and deleting it while something does.
    let usage = client
        .ok("GET", &format!("/api/datasets/{id}/usage"), None)
        .await;
    assert_eq!(usage["datasets"][0]["name"], json!("over"));
    let err = client
        .refused("DELETE", &format!("/api/datasets/{id}"), None)
        .await;
    assert!(err.contains("`over`"), "{err}");
    let over_id = over["dataset"]["id"].as_str().unwrap();
    client
        .ok("DELETE", &format!("/api/datasets/{over_id}"), None)
        .await;
    client
        .ok("DELETE", &format!("/api/datasets/{id}"), None)
        .await;
    let (status, _) = client
        .send("GET", &format!("/api/datasets/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn workspaces_are_listed_renamed_saved_and_deleted() -> sc_error::Result<()> {
    let (mut client, db) = setup().await?;

    // Six kinds, of which A2's Data explorer is the first here; the
    // Dataset editor and the model editor are not kinds of workspace.
    let kinds = client.ok("GET", "/api/workspace-kinds", None).await;
    let kinds = kinds.as_array().unwrap();
    assert_eq!(kinds.len(), 6);
    let here: Vec<&Value> = kinds
        .iter()
        .filter(|k| k["available"] == json!(true))
        .collect();
    assert_eq!(here.len(), 1);
    assert_eq!(here[0]["kind"], json!("data_explorer"));
    assert_eq!(here[0]["arrives_in"], Value::Null);
    assert!(!kinds.iter().any(|k| k["kind"] == "dataset_editor"));
    assert!(!kinds.iter().any(|k| k["kind"] == "model_fit"));

    let err = client
        .refused(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Draft", "kind": "report" })),
        )
        .await;
    assert!(err.contains("milestone A4"), "{err}");
    // Models open in the model editor, not in a workspace.
    let err = client
        .refused(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Fits", "kind": "model_fit" })),
        )
        .await;
    assert!(err.contains("not a kind of workspace"), "{err}");
    let created = client
        .ok(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Plots", "kind": "data_explorer" })),
        )
        .await;
    assert_eq!(created["kind"], json!("data_explorer"));
    client
        .ok(
            "DELETE",
            &format!("/api/workspaces/{}", created["id"].as_str().unwrap()),
            None,
        )
        .await;
    let err = client
        .refused(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Houses data", "kind": "dataset_editor" })),
        )
        .await;
    assert!(err.contains("not a kind of workspace"), "{err}");

    // What the endpoints do to one that exists, put there through the store.
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    let seeded = sc_analytics::Workspace::new(
        "House plots",
        sc_analytics::WorkspaceKind::DataExplorer,
        None,
    );
    sc_analytics::create_workspace(&catalog, &seeded).await?;
    let id = seeded.id.to_string();
    let ws = client
        .ok("GET", &format!("/api/workspaces/{id}"), None)
        .await;
    assert_eq!(ws["state"], json!({}));
    assert_eq!(ws["kind"], json!("data_explorer"));

    let state = json!({ "dataset": "d1", "x": ["area"] });
    client
        .ok(
            "PUT",
            &format!("/api/workspaces/{id}/state"),
            Some(json!({ "state": state })),
        )
        .await;
    client
        .ok(
            "PUT",
            &format!("/api/workspaces/{id}"),
            Some(json!({ "name": "Houses" })),
        )
        .await;
    let back = client
        .ok("GET", &format!("/api/workspaces/{id}"), None)
        .await;
    assert_eq!(back["name"], json!("Houses"));
    assert_eq!(back["state"], state);
    assert_eq!(
        client.ok("GET", "/api/workspaces", None).await[0]["id"],
        json!(id)
    );

    client
        .ok("DELETE", &format!("/api/workspaces/{id}"), None)
        .await;
    assert_eq!(client.ok("GET", "/api/workspaces", None).await, json!([]));
    Ok(())
}

#[tokio::test]
async fn the_analytics_endpoints_are_the_admins() -> sc_error::Result<()> {
    let (client, _db) = setup().await?;
    let mut anonymous = Client {
        router: client.router.clone(),
        cookies: HashMap::new(),
    };
    for path in ["/api/datasets", "/api/workspaces", "/api/datasets/tables"] {
        let (status, _) = anonymous.send("GET", path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
    Ok(())
}

#[tokio::test]
async fn editing_a_dataset_flags_its_fits_and_they_still_predict_as_fitted() -> sc_error::Result<()>
{
    let (mut client, _db) = setup().await?;
    let dataset = client
        .ok("POST", "/api/datasets", Some(prices("prices")))
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
    assert_eq!(model["dataset"]["name"], json!("prices"));
    assert_eq!(model["table_name"], json!("houses"));
    let id = model["id"].as_str().unwrap().to_owned();
    let fit = client.fit(&id).await;
    assert_eq!(fit["dataset_changed"], json!(false), "{fit}");

    // The dataset is edited: a filter, and a renamed column the fit read.
    let mut edited = prices("prices");
    edited["operations"] = json!([
        op("f", "filter", json!({ "formula": "bedrooms > 2" })),
        op(
            "a",
            "select",
            json!({ "columns": [
                { "column": "price" }, { "column": "area" }, { "column": "bedrooms", "rename": "rooms" }
            ]})
        ),
    ]);
    client
        .ok("PUT", &format!("/api/datasets/{dataset_id}"), Some(edited))
        .await;
    let listed = client
        .ok("GET", &format!("/api/models/{id}/instances"), None)
        .await;
    assert_eq!(listed[0]["dataset_changed"], json!(true), "{listed}");
    let saved = client.ok("GET", &format!("/api/models/{id}"), None).await;
    assert_eq!(saved["active_instance"]["dataset_changed"], json!(true));

    // The fit still predicts, the way it was fitted: through the definition
    // it recorded, where `bedrooms` is still `bedrooms`.
    let predicted = client
        .ok(
            "POST",
            "/api/model-predictions",
            Some(json!({ "model": id, "filter": "id === 7" })),
        )
        .await;
    let value = predicted["predictions"][0]["value"].as_f64().unwrap();
    assert!(
        (value - (1000.0 * 57.0 + 20000.0 * 3.0)).abs() < 1.0,
        "{predicted}"
    );
    Ok(())
}

#[tokio::test]
async fn a_model_of_an_aggregate_fits_and_predict_on_the_table_refuses_it() -> sc_error::Result<()>
{
    let (mut client, _db) = setup().await?;
    let dataset = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({
                "name": "by bedrooms",
                "base": { "kind": "table", "table": "houses" },
                "operations": [op("g", "aggregate", json!({
                    "group_by": [{ "name": "bedrooms", "formula": "bedrooms" }],
                    "summaries": [
                        { "name": "mean_price", "function": "mean", "column": "price" },
                        { "name": "mean_area", "function": "mean", "column": "area" }
                    ]
                }))],
            })),
        )
        .await;
    let dataset_id = dataset["dataset"]["id"].as_str().unwrap().to_owned();
    let model = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "By size",
                "provider": "linear_regression",
                "dataset": { "dataset_id": dataset_id },
                "configuration": { "label": "mean_price" },
                "split": { "train": 1.0, "validation": 0.0, "test": 0.0, "seed": 7 },
            })),
        )
        .await;
    // Its rows are split by their group keys, so it fits.
    let fit = client.fit(model["id"].as_str().unwrap()).await;
    assert_eq!(fit["rows"]["selected"], json!(5), "{fit}");

    // A row of `houses` is not one of its inputs.
    let err = client
        .refused(
            "POST",
            "/api/tables/houses/fields",
            Some(json!({
                "name": "guess",
                "type": "float8",
                "kind": { "type": "calc", "expression": "predict(\"By size\")" },
            })),
        )
        .await;
    assert!(
        err.contains("cannot predict a row of `houses`")
            && err.contains("each row is one combination of `bedrooms`"),
        "{err}"
    );
    Ok(())
}
