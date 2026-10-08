//! The plot endpoints against a real Postgres (analytics TODO A2.2, A2.6):
//! the gallery, a spec suggested from the drop zones and drawn, a histogram of
//! a million rows that returns only its bins, a scatter plot of them that says
//! it is a sample, and the sentences a spec that cannot be drawn answers.

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

/// Sixty houses in two neighbourhoods, and a million events generated in SQL:
/// `value` runs 0–999.5 and `kind` is `a` for every third event.
const SCHEMA: &str = "
    CREATE TABLE neighbourhoods (id bigint primary key, name text);
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        price double precision,
        neighbourhood bigint references neighbourhoods(id)
    );
    INSERT INTO neighbourhoods VALUES (1, 'North'), (2, 'South');
    INSERT INTO houses (id, area, price, neighbourhood)
      SELECT i, 50 + i, 1000 * (50 + i) + 5000 * (i % 2), 1 + (i % 2)
      FROM generate_series(1, 60) AS i;
    CREATE TABLE events (id bigint primary key, value double precision, kind text);
    INSERT INTO events (id, value, kind)
      SELECT i, (i % 1000) + (i % 2) * 0.5, CASE WHEN i % 3 = 0 THEN 'a' ELSE 'b' END
      FROM generate_series(1, 1000000) AS i;
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

    /// Create a dataset on `table` with no operations, and answer its id.
    async fn dataset(&mut self, name: &str, table: &str) -> String {
        let created = self
            .ok(
                "POST",
                "/api/datasets",
                Some(json!({ "name": name, "base": { "kind": "table", "table": table } })),
            )
            .await;
        created["dataset"]["id"].as_str().unwrap().to_owned()
    }

    async fn render(&mut self, spec: Value) -> Value {
        self.ok("POST", "/api/plots/render", Some(json!({ "spec": spec })))
            .await
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
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    Ok((client, db))
}

#[tokio::test]
async fn plots_are_suggested_drawn_and_refused_through_the_api() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // The gallery: eleven plot types, four of them reshaping the data
    // themselves, and the map waiting for A5.
    let gallery = client.ok("GET", "/api/plots/gallery", None).await;
    let items = gallery.as_array().unwrap();
    assert_eq!(items.len(), 12);
    let reshaping: Vec<&str> = items
        .iter()
        .filter(|i| i["reshapes"] == json!(true))
        .map(|i| i["preset"].as_str().unwrap())
        .collect();
    assert_eq!(
        reshaping,
        vec!["splom", "parallel", "correlation", "mosaic"]
    );
    // The map is here since A5.7, drawn by `suggestMap` rather than as a plot.
    let map = items.iter().find(|i| i["preset"] == "map").unwrap();
    assert_eq!(map["available"], json!(true));
    assert_eq!(map["arrives_in"], Value::Null);

    // `price` on Y and `neighbourhood` on X: a box plot, without asking.
    let houses = client.dataset("Houses", "houses").await;
    let suggested = client
        .ok(
            "POST",
            "/api/plots/suggest",
            Some(json!({
                "dataset": houses,
                "assignment": { "x": { "field": "neighbourhood" }, "y": [{ "field": "price" }] },
            })),
        )
        .await;
    let spec = suggested["spec"].clone();
    assert_eq!(spec["layers"][0]["mark"], json!("box"));
    assert_eq!(spec["layers"][0]["stat"]["kind"], json!("boxplot"));
    let drawn = client.render(spec).await;
    let layer = &drawn["layers"][0];
    assert_eq!(layer["rows"].as_array().unwrap().len(), 2);
    assert_eq!(layer["total"], json!(60));
    assert_eq!(drawn["domains"]["x"]["values"], json!([1, 2]));

    // The scatter plot preset fills X and Y itself, and says what it chose.
    let scatter = client
        .ok(
            "POST",
            "/api/plots/suggest",
            Some(json!({ "dataset": houses, "preset": "scatter" })),
        )
        .await;
    assert_eq!(scatter["assignment"]["x"]["field"], json!("area"));
    assert_eq!(scatter["assignment"]["y"][0]["field"], json!("price"));
    let drawn = client.render(scatter["spec"].clone()).await;
    assert_eq!(drawn["layers"][0]["rows"].as_array().unwrap().len(), 60);
    assert_eq!(drawn["layers"][0]["sampled"], json!(false));

    // Nothing to draw yet, and a map asked of the plot rules: sentences, not
    // errors.
    let empty = client
        .ok(
            "POST",
            "/api/plots/suggest",
            Some(json!({ "dataset": houses })),
        )
        .await;
    assert_eq!(
        empty["error"],
        json!("drop a column on X or Y to draw a plot")
    );
    let map = client
        .ok(
            "POST",
            "/api/plots/suggest",
            Some(json!({ "dataset": houses, "preset": "map" })),
        )
        .await;
    assert!(map["error"].as_str().unwrap().contains("suggestMap"), "{map}");

    // A spec that cannot be drawn answers why, with every reason.
    let refused = client
        .render(json!({
            "data": { "kind": "dataset", "dataset": houses },
            "layers": [{ "mark": "point",
                         "encoding": { "x": { "field": "colour" },
                                       "size": { "field": "neighbourhood" } } }],
        }))
        .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .starts_with("X: `colour` is not a column of the dataset"),
        "{refused}"
    );
    assert!(refused.get("layers").is_none());
    // A spec that is not a spec at all is a bad request.
    let (status, body) = client
        .send(
            "POST",
            "/api/plots/render",
            Some(json!({ "spec": { "layers": "none" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

#[tokio::test]
async fn summary_tables_and_reshaping_presets_through_the_api() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let houses = client.dataset("Houses", "houses").await;
    let data = json!({ "kind": "dataset", "dataset": houses });

    // Rows by neighbourhood, the mean price in the cells, with totals. Prices
    // are 1000·(50 + i) + 5000·(i mod 2): North's 30 houses are the even i
    // (mean i 31), South's the odd (mean i 30, and 5000 more).
    let table = client
        .ok(
            "POST",
            "/api/plots/table",
            Some(json!({ "spec": {
                "data": data,
                "rows": [{ "field": "neighbourhood" }],
                "cells": [{ "field": "price", "function": "mean" }],
            } })),
        )
        .await;
    assert_eq!(table["cells"], json!(["mean of price"]));
    assert_eq!(table["body"]["columns"], json!(["r0", "n", "v0"]));
    assert_eq!(
        table["body"]["rows"],
        json!([[1, 30, 81000.0], [2, 30, 85000.0]])
    );
    assert_eq!(table["grand_total"]["rows"], json!([[60, 83000.0]]));
    assert_eq!(table["total"], json!(60));

    // A float as rows must be binned: a sentence, not an error.
    let refused = client
        .ok(
            "POST",
            "/api/plots/table",
            Some(json!({ "spec": { "data": data, "rows": [{ "field": "area" }] } })),
        )
        .await;
    assert!(
        refused["error"].as_str().unwrap().ends_with("bin it"),
        "{refused}"
    );

    // The correlation heatmap preset picks the numbers and draws every pair.
    let suggested = client
        .ok(
            "POST",
            "/api/plots/suggest",
            Some(json!({ "dataset": houses, "preset": "correlation" })),
        )
        .await;
    assert_eq!(
        suggested["assignment"]["y"],
        json!([{ "field": "area" }, { "field": "price" }])
    );
    let drawn = client.render(suggested["spec"].clone()).await;
    assert_eq!(drawn["layers"][0]["rows"].as_array().unwrap().len(), 4);
    let first = &drawn["layers"][0]["rows"][0];
    assert_eq!(
        (&first[0], &first[1], &first[3]),
        (&json!("area"), &json!("area"), &json!(60))
    );
    assert!((first[2].as_f64().unwrap() - 1.0).abs() < 1e-12, "{first}");
    Ok(())
}

#[tokio::test]
async fn a_histogram_of_a_million_rows_returns_only_its_bins() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let events = client.dataset("Events", "events").await;
    let data = json!({ "kind": "dataset", "dataset": events });

    let started = std::time::Instant::now();
    let histogram = client
        .render(json!({
            "data": data,
            "layers": [{ "mark": "bar", "stat": { "kind": "count" },
                         "encoding": { "x": { "field": "value", "bin": {} } } }],
        }))
        .await;
    let took = started.elapsed();
    let layer = &histogram["layers"][0];
    let rows = layer["rows"].as_array().unwrap();
    // Freedman–Diaconis over a uniform 0–999.5: IQR ≈ 500, so
    // 2·500·10⁶^(−1/3) = 10 — a hundred bins, not a million rows.
    assert_eq!(histogram["bins"]["value"]["width"], json!(10.0));
    assert_eq!(rows.len(), 100);
    let counted: u64 = rows.iter().map(|r| r[2].as_u64().unwrap()).sum();
    assert_eq!(counted, 1_000_000);
    assert_eq!(layer["total"], json!(1_000_000));
    assert_eq!(rows[0], json!([0.0, 10.0, 10_000]));
    eprintln!("a histogram of a million rows took {took:?}");

    // Coloured by kind: the bins split in two, still only bins.
    let by_kind = client
        .render(json!({
            "data": data,
            "layers": [{ "mark": "bar", "stat": { "kind": "count" },
                         "encoding": { "x": { "field": "value", "bin": {} },
                                       "color": { "field": "kind" } } }],
        }))
        .await;
    assert_eq!(by_kind["layers"][0]["rows"].as_array().unwrap().len(), 200);
    assert_eq!(by_kind["domains"]["color"]["values"], json!(["a", "b"]));

    // A scatter plot of them draws a sample, and says so.
    let scatter = client
        .render(json!({
            "data": data,
            "layers": [{ "mark": "point",
                         "encoding": { "x": { "field": "id" }, "y": { "field": "value" } } }],
        }))
        .await;
    let layer = &scatter["layers"][0];
    assert_eq!(layer["sampled"], json!(true));
    assert_eq!(layer["total"], json!(1_000_000));
    assert_eq!(layer["rows"].as_array().unwrap().len(), 10_000);
    Ok(())
}

/// The hypothesis tests (A2.12–A2.14) through `runTests`: Welch's t-test and
/// the rank-sum test of price by neighbourhood (R: `t.test(price[hood == 1],
/// price[hood == 2])` and `wilcox.test` of the same, for the sixty houses),
/// the million events tested with their rank tests on a sample, Wrap
/// repeating the analysis, and the sentence for roles with no test.
#[tokio::test]
async fn hypothesis_tests_run_through_the_api() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let houses = client.dataset("Houses", "houses").await;
    let events = client.dataset("Events", "events").await;
    let data = |id: &str| json!({ "kind": "dataset", "dataset": id });

    let answer = client
        .ok(
            "POST",
            "/api/plots/tests",
            Some(json!({ "spec": {
                "data": data(&houses),
                "y": [{ "field": "price" }],
                "x": { "field": "neighbourhood" },
            }})),
        )
        .await;
    assert_eq!(answer["design"], "number_by_groups", "{answer}");
    let section = &answer["sections"][0];
    assert_eq!(section["n"], 60);
    assert_eq!(section["levels"][0]["value"], 1);
    assert_eq!(section["levels"][0]["mean"], 81000.0);
    assert_eq!(section["levels"][1]["mean"], 85000.0);
    let welch = &section["tests"][0];
    assert_eq!(welch["test"], "welch_t");
    assert_eq!(welch["role"], "main");
    let t = welch["result"]["statistic"]["value"].as_f64().unwrap();
    assert!((t + 0.87988269012812).abs() < 1e-9, "{t}");
    let p = welch["result"]["p_value"].as_f64().unwrap();
    assert!((p - 0.382554069814939).abs() < 1e-9, "{p}");
    assert_eq!(welch["result"]["df"][0], 58.0);
    let rank = &section["tests"][1];
    assert_eq!(
        (rank["test"].as_str(), rank["role"].as_str()),
        (Some("mann_whitney"), Some("alternative"))
    );
    assert_eq!(rank["result"]["statistic"]["value"], 392.0);
    let p = rank["result"]["p_value"].as_f64().unwrap();
    assert!((p - 0.395083093639199).abs() < 1e-9, "{p}");
    assert_eq!(rank["result"]["method"], "normal approximation");
    assert_eq!(section["preferred"], "welch_t");
    assert!(
        section["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["ok"] == true),
        "{section}"
    );

    // A million events: the t-test reads them all, in SQL; the rank tests
    // read a sample of 5,000 and say so.
    let answer = client
        .ok(
            "POST",
            "/api/plots/tests",
            Some(json!({ "spec": {
                "data": data(&events),
                "y": [{ "field": "value" }],
                "x": { "field": "kind" },
            }})),
        )
        .await;
    let section = &answer["sections"][0];
    assert_eq!(section["n"], 1_000_000);
    assert_eq!(section["sampled"], 5000);
    assert_eq!(section["tests"][0]["result"]["n"], 1_000_000);
    assert_eq!(section["tests"][0]["result"]["sampled"], false);
    assert_eq!(section["tests"][1]["result"]["sampled"], true);

    // Wrap: one section per kind.
    let answer = client
        .ok(
            "POST",
            "/api/plots/tests",
            Some(json!({ "spec": {
                "data": data(&events),
                "y": [{ "field": "value" }],
                "by": { "field": "kind" },
            }})),
        )
        .await;
    assert_eq!(answer["design"], "one_number");
    let by: Vec<&Value> = answer["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| &s["by"])
        .collect();
    assert_eq!(by, vec![&json!("a"), &json!("b")]);
    assert_eq!(answer["sections"][0]["n"], 333_333);

    // Roles with no test answer a sentence, not an error.
    let answer = client
        .ok(
            "POST",
            "/api/plots/tests",
            Some(json!({ "spec": { "data": data(&houses), "y": [] }})),
        )
        .await;
    assert_eq!(answer["error"], "put a column on Y to test it");
    let (status, _) = client
        .send(
            "POST",
            "/api/plots/tests",
            Some(json!({ "spec": { "y": [] } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}
