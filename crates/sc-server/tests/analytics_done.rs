//! Milestone A1's definition of done (analytics TODO A1.20, A1.21): the Try
//! it, through the API, over `feldspar demo analytics`'s rows.
//!
//! The front page's two lists, empty, with no kind of workspace here yet; the
//! dataset "House prices by area" built operation by operation; every stage
//! read and its rows checked against the same numbers computed here from the
//! base rows; the Filter switched off and on; the Aggregate broken by renaming
//! the column it reads, and repaired; the dataset in the front page's list;
//! and a model over a named dataset fitted, predicting through a calculated
//! field on every house.

use std::collections::{BTreeMap, HashMap};
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

    /// Every row of the stage after `upto` operations of `def`.
    async fn stage(&mut self, def: &Value, upto: usize) -> (Vec<String>, Vec<Vec<Value>>, i64) {
        let page = self
            .ok(
                "POST",
                "/api/datasets/stage",
                Some(json!({ "dataset": def, "upto": upto, "limit": 1000 })),
            )
            .await;
        let names = page["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect();
        let rows = page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_array().unwrap().clone())
            .collect();
        (names, rows, page["total"].as_i64().unwrap())
    }

    /// Save `def`'s operations, answering the report.
    async fn save(&mut self, id: &str, def: &Value) -> Value {
        self.ok("PUT", &format!("/api/datasets/{id}"), Some(def.clone()))
            .await["report"]
            .clone()
    }
}

async fn setup() -> sc_error::Result<(Client, TestDb)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    // `feldspar demo analytics`, as the Try it begins.
    sc_analytics::demo::demo_analytics(&catalog, false).await?;
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
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    Ok((client, db))
}

fn op(id: &str, kind: &str, params: Value) -> Value {
    json!({ "id": id, "enabled": true, "kind": kind, "params": params })
}

fn f(v: &Value) -> Option<f64> {
    v.as_f64()
}

#[tokio::test]
async fn the_try_it_of_milestone_a1() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // 1. The front page: no datasets and no workspaces yet, and every kind of
    // workspace listed with the milestone that brings it.
    assert_eq!(client.ok("GET", "/api/datasets", None).await, json!([]));
    assert_eq!(client.ok("GET", "/api/workspaces", None).await, json!([]));
    let kinds = client.ok("GET", "/api/workspace-kinds", None).await;
    assert!(
        kinds
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k["available"] == json!(false) && k["arrives_in"].is_string()),
        "{kinds}"
    );

    // 2. The dataset on `houses`: its base is the table's rows.
    let created = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "House prices by area",
                         "base": { "kind": "table", "table": "houses" } })),
        )
        .await;
    let id = created["dataset"]["id"].as_str().unwrap().to_owned();
    let mut def = created["dataset"].clone();
    let (base_names, base_rows, base_total) = client.stage(&def, 0).await;
    assert_eq!(base_total, 200);
    let col = |name: &str| base_names.iter().position(|n| n == name).unwrap();
    let (price, area, hood) = (col("price"), col("area"), col("neighbourhood"));

    // What the operations should give, computed here from the base rows.
    let names_of: BTreeMap<i64, &str> = [
        (1, "Riverside"),
        (2, "Old Town"),
        (3, "Northgate"),
        (4, "Harbour"),
        (5, "Hillcrest"),
    ]
    .into_iter()
    .collect();
    let mut expected_filtered: BTreeMap<i64, (i64, f64)> = BTreeMap::new();
    let mut expected_all: BTreeMap<i64, i64> = BTreeMap::new();
    for row in &base_rows {
        let h = row[hood].as_i64().unwrap();
        *expected_all.entry(h).or_default() += 1;
        if let Some(p) = f(&row[price]).filter(|p| *p > 100_000.0) {
            let e = expected_filtered.entry(h).or_default();
            e.0 += 1;
            e.1 += p / f(&row[area]).unwrap();
        }
    }

    // 3–4. The operations, saved as the editor saves them.
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
    let report = client.save(&id, &def).await;
    assert!(
        report["operations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["status"] == json!("ok")),
        "{report}"
    );

    // 5. Every stage, checked.
    let (names, rows, total) = client.stage(&def, 1).await;
    assert_eq!(total, 200);
    let ppm = names.iter().position(|n| n == "price_per_m2").unwrap();
    for (row, base) in rows.iter().zip(&base_rows) {
        match (f(&base[price]), f(&base[area])) {
            (Some(p), Some(a)) => assert!((f(&row[ppm]).unwrap() - p / a).abs() < 1e-6),
            _ => assert!(row[ppm].is_null()),
        }
    }
    let (names, rows, _) = client.stage(&def, 2).await;
    let hood_name = names.iter().position(|n| n == "hood").unwrap();
    for (row, base) in rows.iter().zip(&base_rows) {
        let h = base[hood].as_i64().unwrap();
        assert_eq!(row[hood_name], json!(names_of[&h]));
    }
    let filtered: i64 = expected_filtered.values().map(|(n, _)| n).sum();
    let (_, _, total) = client.stage(&def, 3).await;
    assert_eq!(total, filtered);
    let (names, rows, total) = client.stage(&def, 4).await;
    assert_eq!(names, ["neighbourhood", "mean_ppm", "n"]);
    assert_eq!(total, expected_filtered.len() as i64);
    for row in &rows {
        let (n, sum) = expected_filtered[&row[0].as_i64().unwrap()];
        assert_eq!(row[2], json!(n));
        assert!((f(&row[1]).unwrap() - sum / n as f64).abs() < 1e-6);
    }

    // The Filter switched off: every house counts.
    def["operations"][2]["enabled"] = json!(false);
    client.save(&id, &def).await;
    let (_, rows, _) = client.stage(&def, 4).await;
    for row in &rows {
        assert_eq!(row[2], json!(expected_all[&row[0].as_i64().unwrap()]));
    }
    def["operations"][2]["enabled"] = json!(true);
    client.save(&id, &def).await;

    // The column the Aggregate reads renamed: the Aggregate is marked, naming
    // it, and the stages before it still read.
    def["operations"][0]["params"]["name"] = json!("ppm");
    let report = client.save(&id, &def).await;
    let aggregate = &report["operations"][3];
    assert_eq!(aggregate["status"], json!("invalid"));
    assert!(
        aggregate["error"]
            .as_str()
            .unwrap()
            .contains("`price_per_m2`"),
        "{aggregate}"
    );
    assert_eq!(client.stage(&def, 3).await.2, filtered);
    let (status, _) = client
        .send(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": def, "upto": 4 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    def["operations"][0]["params"]["name"] = json!("price_per_m2");
    let report = client.save(&id, &def).await;
    assert_eq!(report["operations"][3]["status"], json!("ok"));

    // 6. The front page lists the dataset with its four operations, and it
    // opens as it was saved.
    let listed = client.ok("GET", "/api/datasets", None).await;
    assert_eq!(listed[0]["id"], json!(id));
    assert_eq!(listed[0]["name"], json!("House prices by area"));
    assert_eq!(listed[0]["operations"], json!(4));
    let reopened = client
        .ok("GET", &format!("/api/datasets/{id}"), None)
        .await;
    let ids: Vec<&str> = reopened["dataset"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["c1", "c2", "f1", "a1"]);

    // 7. A model over a named dataset that keeps the grain: fitted, and
    // predicting every house through the models tutorial's calculated field.
    let prices = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({
                "name": "House prices",
                "base": { "kind": "table", "table": "houses" },
                "operations": [
                    op("s", "filter", json!({ "formula": "sold === true" })),
                    op("i", "calculated", json!({ "name": "income", "formula": "neighbourhoodⱵaverage_income" })),
                    op("k", "select", json!({ "columns": [
                        { "column": "price" }, { "column": "area" },
                        { "column": "bedrooms" }, { "column": "income" }
                    ]})),
                ],
            })),
        )
        .await;
    let model = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "House prices",
                "provider": "linear_regression",
                "dataset": { "dataset_id": prices["dataset"]["id"] },
                "configuration": { "label": "price" },
                "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
            })),
        )
        .await;
    let model_id = model["id"].as_str().unwrap().to_owned();
    let started = client
        .ok(
            "POST",
            &format!("/api/models/{model_id}/fit"),
            Some(json!({})),
        )
        .await;
    let instance = started["id"].as_str().unwrap().to_owned();
    let mut fitted = Value::Null;
    for _ in 0..600 {
        fitted = client
            .ok("GET", &format!("/api/model-instances/{instance}"), None)
            .await;
        if fitted["status"] != json!("fitting") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(fitted["status"], json!("fitted"), "{fitted}");
    assert!(
        fitted["metrics"]["train"]["r2"].as_f64().unwrap() > 0.8,
        "{fitted}"
    );
    client
        .ok(
            "POST",
            &format!("/api/model-instances/{instance}/activate"),
            None,
        )
        .await;
    client
        .ok(
            "POST",
            "/api/tables/houses/fields",
            Some(json!({
                "name": "estimated_price",
                "type": "float8",
                "kind": { "type": "calc", "expression": "predict(\"House prices\")" },
            })),
        )
        .await;
    let houses = client
        .ok("GET", "/api/tables/houses/rows?limit=500", None)
        .await;
    let houses = houses.as_array().unwrap();
    assert_eq!(houses.len(), 200);
    // Every house has a number — the unsold ones too, which the dataset's
    // filter left out of the fit.
    assert!(
        houses
            .iter()
            .all(|h| h["estimated_price"].as_f64().is_some())
    );
    assert!(houses.iter().any(|h| h["sold"] == json!(false)));
    Ok(())
}
