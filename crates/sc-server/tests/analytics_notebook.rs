//! Running a notebook's cells through the API (analytics TODO A10.3–A10.6,
//! A10.10): a session that keeps one cell's globals for the next, the `nb`
//! surface in JavaScript and Python, restart, interrupt and a runaway cell,
//! a fit, Save as dataset, the usage index — and an LLM prompt cell answered
//! by a scripted provider, replayed without it.
//!
//! **A10.10**, the milestone's definition of done, over `feldspar demo
//! analytics` (A10.9): the demo's JavaScript and Python notebooks run all,
//! stopping at the fit while part 3's `House prices` is not there; `d`'s rows
//! and the bars checked against the means and counts SQL makes from the same
//! houses; the model made and the fit's coefficients and residual plot drawn;
//! the notebook in `Houses`' usage; the outputs stored. And the demo's LLM
//! notebook answered by a scripted provider, then replayed without it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_agent::ModelRole;
use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, ServerConfig, admin_handlers, build_router_with_apps, default_js_evaluator,
    install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Sixty houses in two neighbourhoods, as `analytics_api` has them.
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
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let value = match bytes.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        };
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    /// Create a notebook in `language` with these code cells, answering its id.
    async fn notebook(&mut self, language: &str, cells: &[(&str, &str)]) -> String {
        let cells: Vec<Value> = cells
            .iter()
            .map(|(id, source)| json!({ "id": id, "kind": "code", "source": source }))
            .collect();
        let made = self
            .ok(
                "POST",
                "/api/workspaces",
                Some(
                    json!({ "name": format!("A {language} notebook"), "kind": "notebook",
                             "state": { "language": language, "cells": cells } }),
                ),
            )
            .await;
        made["id"].as_str().unwrap().to_owned()
    }

    /// Run cell `cell` of notebook `id`, answering the cell.
    async fn run(&mut self, id: &str, cell: &str) -> Value {
        self.run_with(id, cell, json!({})).await
    }

    async fn run_with(&mut self, id: &str, cell: &str, body: Value) -> Value {
        let ran = self
            .ok(
                "POST",
                &format!("/api/workspaces/{id}/cells/{cell}/run"),
                Some(body),
            )
            .await;
        ran["cell"].clone()
    }

    async fn render(&mut self, panel: &Value) -> Value {
        self.ok(
            "POST",
            "/api/panels/render",
            Some(json!({ "panel": panel })),
        )
        .await
    }
}

async fn setup(models: Option<FakeModels>) -> sc_error::Result<(Client, TestDb)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    serve(db, models, false).await
}

/// A server over `db` with the admin logged in; after `feldspar demo
/// analytics` when `demo`.
async fn serve(
    db: TestDb,
    models: Option<FakeModels>,
    demo: bool,
) -> sc_error::Result<(Client, TestDb)> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    if demo {
        sc_catalog::bootstrap_table_meta(&catalog).await?;
        sc_catalog::bootstrap_field_meta(&catalog).await?;
        sc_analytics::demo::demo_analytics(&catalog, false).await?;
    }
    let mut agents = sc_server::install_agents(&catalog).await?;
    if let Some(models) = models {
        agents = agents.with_providers(Arc::new(models));
    }
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_triggers(dispatcher)
            .with_agents(agents)
            .with_models(models)
            .with_python(sc_server::default_python_adapter()),
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

/// The stored dataset `prices` and the model `House prices` over it.
async fn prices_and_model(client: &mut Client) -> String {
    let dataset = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({
                "name": "prices",
                "base": { "kind": "table", "table": "houses" },
                "operations": [{ "id": "a", "enabled": true, "kind": "select", "params": {
                    "columns": [{ "column": "price" }, { "column": "area" },
                                { "column": "bedrooms" }, { "column": "neighbourhood" }] } }],
            })),
        )
        .await;
    let dataset_id = dataset["dataset"]["id"].as_str().unwrap().to_owned();
    client
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
    dataset_id
}

fn kinds(cell: &Value) -> Vec<String> {
    cell["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| match o["kind"].as_str().unwrap() {
            "panel" => format!("panel:{}", o["panel"]["kind"].as_str().unwrap()),
            other => other.to_owned(),
        })
        .collect()
}

/// The Try it's cells, in `language`: a table of the mean price by
/// neighbourhood, a bar chart of it from the cell above, a typo, a fit.
fn try_it(language: &str) -> Vec<(&'static str, &'static str)> {
    match language {
        "javascript" => vec![
            (
                "table",
                "const d = nb.dataset(\"prices\").filter(\"price > 100000\")\n  \
                 .aggregate([\"neighbourhood\"], { mean_price: \"mean(price)\", n: \"count()\" });\n\
                 console.log(\"neighbourhoods:\", 2);\n\
                 nb.table(d)",
            ),
            (
                "plot",
                "nb.plot({ data: d, mark: \"bar\", x: \"neighbourhood\", y: \"mean_price\" })",
            ),
            (
                "typo",
                "const x = 1;\nnb.plot({ data: d, mark: \"bar\", x: \"neighbourhod\", y: \"mean_price\" })",
            ),
            ("runaway", "let i = 0;\nwhile (true) { i++; }"),
            ("still", "typeof d"),
            (
                "fit",
                "const fit = await nb.fit(\"House prices\");\nnb.show(fit.output(\"coefficients\"));\nfit",
            ),
            ("rows", "(await d.rows()).length"),
            // A dataset is not a thenable: shown, or a cell's value, it is
            // not waited for forever.
            ("show", "nb.show(d);\nd"),
        ],
        _ => vec![
            (
                "table",
                "d = nb.dataset(\"prices\").filter(\"price > 100000\").aggregate(\n    \
                 [\"neighbourhood\"], mean_price=\"mean(price)\", n=\"count()\")\n\
                 print(\"neighbourhoods:\", 2)\n\
                 nb.table(d)",
            ),
            (
                "plot",
                "nb.plot(data=d, mark=\"bar\", x=\"neighbourhood\", y=\"mean_price\")",
            ),
            (
                "typo",
                "x = 1\nnb.plot(data=d, mark=\"bar\", x=\"neighbourhod\", y=\"mean_price\")",
            ),
            ("runaway", "i = 0\nwhile True:\n    i += 1"),
            ("still", "type(d).__name__"),
            (
                "fit",
                "fit = nb.fit(\"House prices\")\nnb.show(fit.output(\"coefficients\"))\nfit",
            ),
            ("rows", "len(d.rows())"),
            ("show", "nb.show(d)\nd"),
        ],
    }
}

async fn the_try_it(language: &str) -> sc_error::Result<()> {
    let (mut client, _db) = setup(None).await?;
    let dataset_id = prices_and_model(&mut client).await;
    let id = client.notebook(language, &try_it(language)).await;

    // No session until a cell runs.
    let status = client
        .ok("GET", &format!("/api/workspaces/{id}/session"), None)
        .await;
    assert_eq!(status["state"], json!("none"));

    // The summary table, after what the cell printed; its rows are the two
    // neighbourhoods.
    let table = client.run(&id, "table").await;
    assert_eq!(kinds(&table), ["text", "panel:summary_table"], "{table}");
    assert_eq!(table["outputs"][0]["text"], json!("neighbourhoods: 2"));
    assert_eq!(table["run"]["count"], json!(1));
    let drawn = client.render(&table["outputs"][1]["panel"]).await;
    assert!(drawn.get("error").is_none(), "{drawn}");
    assert_eq!(drawn["table"]["total"], json!(2), "{drawn}");
    let rows = drawn["table"]["body"]["rows"].as_array().expect("rows");
    assert!(rows.len() >= 2, "a row per neighbourhood: {drawn}");

    // The bar chart reads `d`, which the cell above defined.
    let plot = client.run(&id, "plot").await;
    assert_eq!(kinds(&plot), ["panel:plot"], "{plot}");
    let drawn = client.render(&plot["outputs"][0]["panel"]).await;
    let bars = drawn["plot"]["layers"][0]["rows"].as_array().expect("bars");
    assert_eq!(bars.len(), 2, "{drawn}");

    // A typo is the plot's own sentence, on the line it came from.
    let typo = client.run(&id, "typo").await;
    assert_eq!(kinds(&typo), ["error"], "{typo}");
    let message = typo["outputs"][0]["message"].as_str().unwrap();
    assert!(message.contains("neighbourhod"), "{message}");
    assert_eq!(typo["outputs"][0]["line"], json!(2), "{typo}");

    // A runaway cell is stopped, and the session still runs the next one:
    // `d` is still there.
    let runaway = client.run(&id, "runaway").await;
    assert_eq!(kinds(&runaway), ["error"], "{runaway}");
    let still = client.run(&id, "still").await;
    let expected = match language {
        "javascript" => json!("object"),
        _ => json!("Dataset"),
    };
    assert_eq!(still["outputs"][0]["value"], expected, "{still}");

    // The fit: its coefficient table shown, and as the cell's value.
    let fit = client.run(&id, "fit").await;
    assert_eq!(kinds(&fit), ["panel:fit_table", "panel:fit_table"], "{fit}");
    let drawn = client.render(&fit["outputs"][0]["panel"]).await;
    assert!(
        drawn["output"]["table"]["rows"]
            .as_array()
            .is_some_and(|r| !r.is_empty()),
        "{drawn}"
    );
    let rows = client.run(&id, "rows").await;
    assert_eq!(rows["outputs"][0]["value"], json!(2), "{rows}");
    let show = client.run(&id, "show").await;
    assert_eq!(kinds(&show), ["dataset", "dataset"], "{show}");
    assert_eq!(show["outputs"][0]["total"], json!(2), "{show}");

    // The outputs are stored: the notebook reopens with them.
    let ws = client
        .ok("GET", &format!("/api/workspaces/{id}"), None)
        .await;
    assert_eq!(
        ws["state"]["cells"][0]["outputs"][1]["kind"],
        json!("panel")
    );

    // The usage index finds the notebook through the dataset its table is
    // built on.
    let usage = client
        .ok("GET", &format!("/api/datasets/{dataset_id}/usage"), None)
        .await;
    let uses = usage["workspaces"].as_array().unwrap();
    assert!(
        uses.iter()
            .any(|u| u["id"] == json!(id) && u["kind"] == json!("notebook")),
        "{usage}"
    );

    // Save as dataset: the table's dataset is stored, and the cell, run
    // again, refers to it.
    let inline = ws["state"]["cells"][0]["outputs"][1]["panel"]["content"]["spec"]["data"].clone();
    assert_eq!(inline["kind"], json!("inline"), "{inline}");
    let inline_id = inline["definition"]["id"].as_str().unwrap().to_owned();
    client
        .ok(
            "POST",
            &format!("/api/workspaces/{id}/save-dataset"),
            Some(json!({ "dataset": inline_id, "name": format!("Dear houses ({language})") })),
        )
        .await;
    let table = client.run(&id, "table").await;
    assert_eq!(
        table["outputs"][1]["panel"]["content"]["spec"]["data"],
        json!({ "kind": "dataset", "dataset": inline_id }),
        "{table}"
    );

    // Run all runs every cell in order, stopping at the typo.
    let all = client
        .ok("POST", &format!("/api/workspaces/{id}/run-all"), None)
        .await;
    let ran: Vec<&str> = all["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ran, ["table", "plot", "typo"], "{all}");
    assert_eq!(all["session"]["state"], json!("idle"));

    // Restart: what the cells defined is gone.
    let status = client
        .ok("POST", &format!("/api/workspaces/{id}/restart"), None)
        .await;
    assert_eq!(status["state"], json!("none"));
    let gone = client.run(&id, "still").await;
    match language {
        "javascript" => assert_eq!(gone["outputs"][0]["value"], json!("undefined"), "{gone}"),
        _ => assert_eq!(kinds(&gone), ["error"], "{gone}"),
    }
    assert_eq!(
        gone["run"]["count"],
        json!(1),
        "a new session counts from 1"
    );
    let status = client
        .ok("POST", &format!("/api/workspaces/{id}/interrupt"), None)
        .await;
    assert_eq!(status["state"], json!("idle"), "nothing was running");
    Ok(())
}

#[tokio::test]
async fn a_javascript_notebook_runs_the_try_it_in_a_session() -> sc_error::Result<()> {
    the_try_it("javascript").await
}

#[cfg(feature = "python")]
#[tokio::test]
async fn a_python_notebook_runs_the_try_it_in_a_session() -> sc_error::Result<()> {
    the_try_it("python").await
}

#[tokio::test]
async fn a_cell_is_run_by_id_and_a_text_cell_or_another_kind_is_refused() -> sc_error::Result<()> {
    let (mut client, _db) = setup(None).await?;
    let id = client.notebook("javascript", &[("a", "1 + 1")]).await;
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/workspaces/{id}/cells/nope/run"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // The source sent is the one run, and stored.
    let cell = client
        .run_with(&id, "a", json!({ "source": "6 * 7" }))
        .await;
    assert_eq!(cell["outputs"][0]["value"], json!(42));
    assert_eq!(cell["source"], json!("6 * 7"));
    let report = client
        .ok(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "R", "kind": "report", "state": { "blocks": [] } })),
        )
        .await;
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/workspaces/{}/run-all", report["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert!(status.is_client_error(), "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("not a notebook"),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn an_llm_prompt_is_answered_with_the_nb_tools_and_replayed_without_the_llm()
-> sc_error::Result<()> {
    let script = || {
        [
            Reply::calls("columns", json!({ "data": "houses" })),
            Reply::calls_many([
                (
                    "plot".to_owned(),
                    json!({ "spec": { "data": "houses", "mark": "boxplot", "x": "neighbourhood", "y": "price" } }),
                ),
                (
                    "test".to_owned(),
                    json!({ "data": "houses", "spec": { "y": "price", "x": "neighbourhood" } }),
                ),
            ]),
            Reply::says("Prices hardly differ between the two neighbourhoods."),
        ]
    };
    let provider = Arc::new(FakeProvider::new(script().into_iter().chain(script())));
    let models = FakeModels::new().role(ModelRole::Executor, Arc::clone(&provider));
    let (mut client, _db) = setup(Some(models)).await?;
    let prompt = "a box plot of price by neighbourhood, with the right test";
    let id = client.notebook("llm", &[("ask", prompt)]).await;

    let cell = client.run(&id, "ask").await;
    assert_eq!(
        kinds(&cell),
        ["panel:plot", "panel:test_result", "text", "calls"],
        "{cell}"
    );
    let calls = &cell["outputs"][3];
    assert_eq!(calls["prompt"], json!(prompt));
    let functions: Vec<&str> = calls["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["function"].as_str().unwrap())
        .collect();
    assert_eq!(functions, ["columns", "plot", "test", "say"]);
    let asked = provider.requests().len();
    assert_eq!(asked, 3);
    // The agent was told about the data.
    let first = format!("{:?}", provider.requests()[0]);
    assert!(first.contains("houses"), "the data is in its context");

    // Run again: the same outputs, and the LLM is not asked.
    let again = client.run(&id, "ask").await;
    assert_eq!(kinds(&again), kinds(&cell), "{again}");
    assert_eq!(provider.requests().len(), asked, "replayed without the LLM");
    let drawn = client.render(&again["outputs"][1]["panel"]).await;
    assert!(drawn["tests"].is_object(), "{drawn}");

    // Regenerate asks it again.
    let regenerated = client
        .run_with(&id, "ask", json!({ "regenerate": true }))
        .await;
    assert_eq!(kinds(&regenerated), kinds(&cell));
    assert_eq!(provider.requests().len(), asked * 2);
    Ok(())
}

// --- A10.10: the Try it over the demo's own notebooks ----------------------

/// The mean price and the count of the houses over 100,000 in each
/// neighbourhood, by its name: what the demo notebooks' table must say,
/// counted here in SQL from the rows.
async fn expected_means(db: &TestDb) -> sc_error::Result<Vec<(String, f64, i64)>> {
    let rows = db
        .client()
        .await?
        .query(
            "SELECT n.name, avg(h.price), count(*) FROM houses h \
             JOIN neighbourhoods n ON n.id = h.neighbourhood \
             WHERE h.price > 100000 GROUP BY n.name ORDER BY n.name",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                r.get::<_, f64>(1),
                r.get::<_, i64>(2),
            )
        })
        .collect())
}

/// The demo notebook called `name`, by id.
async fn demo_notebook(client: &mut Client, name: &str) -> String {
    let listed = client.ok("GET", "/api/workspaces", None).await;
    let found = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == json!(name))
        .unwrap_or_else(|| panic!("no notebook `{name}` in {listed}"));
    assert_eq!(found["kind"], json!("notebook"));
    found["id"].as_str().unwrap().to_owned()
}

/// The model `House prices` of the tutorial's part 3: a linear regression of
/// `price` on `area` and the neighbourhood's name over the houses sold.
async fn house_prices(client: &mut Client) {
    let dataset = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({
                "name": "Price, area and neighbourhood",
                "base": { "kind": "table", "table": "houses" },
                "operations": [
                    { "id": "f", "enabled": true, "kind": "filter",
                      "params": { "formula": "sold === true" } },
                    { "id": "c", "enabled": true, "kind": "calculated",
                      "params": { "name": "neighbourhood_name", "formula": "neighbourhoodⱵname" } },
                    { "id": "s", "enabled": true, "kind": "select", "params": { "columns": [
                        { "column": "price" }, { "column": "area" },
                        { "column": "neighbourhood_name" }] } }],
            })),
        )
        .await;
    client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "House prices",
                "provider": "linear_regression",
                "dataset": { "dataset_id": dataset["dataset"]["id"] },
                "configuration": { "label": "price" },
                "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
            })),
        )
        .await;
}

/// The column `name`'s index in `columns`: a drawn layer's, named by
/// channel, or a dataset output's.
fn column(columns: &Value, name: &str) -> usize {
    columns
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column `{name}` in {columns}"))
}

async fn the_demo_try_it(notebook: &str) -> sc_error::Result<()> {
    let (mut client, db) = serve(TestDb::new().await?, None, true).await?;
    let expected = expected_means(&db).await?;
    assert_eq!(expected.len(), 5, "the demo's five neighbourhoods");
    let id = demo_notebook(&mut client, notebook).await;

    // Before part 3's model is made, Run all stops at the fit, with a sentence
    // naming it; the table and the plot before it ran.
    let all = client
        .ok("POST", &format!("/api/workspaces/{id}/run-all"), None)
        .await;
    let cells = all["cells"].as_array().unwrap();
    let ran: Vec<&str> = cells.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ran, ["summary", "plot", "fit"], "{all}");
    assert_eq!(kinds(&cells[2]), ["error"], "{all}");
    let message = cells[2]["outputs"][0]["message"].as_str().unwrap();
    assert!(message.contains("House prices"), "{message}");

    // `d`, shown: a row per neighbourhood, each with the mean and the count
    // SQL makes from the same rows.
    assert_eq!(kinds(&cells[0]), ["dataset"], "{all}");
    let shown = &cells[0]["outputs"][0];
    assert_eq!(shown["total"], json!(5), "{shown}");
    assert_eq!(shown["data"]["kind"], json!("inline"), "{shown}");
    let at = |name: &str| column(&shown["columns"], name);
    let (hood, mean, n) = (at("hood"), at("mean_price"), at("n"));
    let mut rows: Vec<(String, f64, i64)> = shown["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[hood].as_str().unwrap().to_owned(),
                r[mean].as_f64().unwrap(),
                r[n].as_i64().unwrap(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rows.len(), expected.len(), "{shown}");
    for (got, want) in rows.iter().zip(&expected) {
        assert_eq!((&got.0, got.2), (&want.0, want.2));
        assert!((got.1 - want.1).abs() < 1e-6, "{got:?} against {want:?}");
    }

    // The bar chart, from `d` of the cell above: a bar per neighbourhood,
    // each as high as its mean.
    assert_eq!(kinds(&cells[1]), ["panel:plot"], "{all}");
    let drawn = client.render(&cells[1]["outputs"][0]["panel"]).await;
    let layer = &drawn["plot"]["layers"][0];
    let (x, y) = (
        column(&layer["columns"], "x"),
        column(&layer["columns"], "y"),
    );
    let mut bars: Vec<(String, f64)> = layer["rows"]
        .as_array()
        .expect("bars")
        .iter()
        .map(|r| (r[x].as_str().unwrap().to_owned(), r[y].as_f64().unwrap()))
        .collect();
    bars.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(bars.len(), expected.len(), "{drawn}");
    for ((hood, mean), (name, want, _)) in bars.iter().zip(&expected) {
        assert_eq!(hood, name);
        assert!((mean - want).abs() < 1e-6, "{hood}: {mean} against {want}");
    }

    // Part 3's model made, the fit runs: its coefficients and its residuals.
    house_prices(&mut client).await;
    let fit = client.run(&id, "fit").await;
    assert_eq!(kinds(&fit), ["panel:fit_table", "panel:plot"], "{fit}");
    let drawn = client.render(&fit["outputs"][0]["panel"]).await;
    assert!(
        drawn["output"]["table"]["rows"]
            .as_array()
            .is_some_and(|r| r.len() >= 5),
        "an intercept, the area and the neighbourhoods: {drawn}"
    );
    let drawn = client.render(&fit["outputs"][1]["panel"]).await;
    assert!(
        drawn["plot"]["layers"][0]["rows"]
            .as_array()
            .is_some_and(|r| r.len() > 100),
        "a point per house sold: {drawn}"
    );

    // The notebook in the usage index: the demo's `Houses`, which its cells
    // are built on, lists it for the delete warning.
    let houses = client.ok("GET", "/api/datasets", None).await;
    let houses = houses
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == json!("Houses"))
        .expect("the demo's Houses")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let usage = client
        .ok("GET", &format!("/api/datasets/{houses}/usage"), None)
        .await;
    assert!(
        usage["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == json!(id) && u["kind"] == json!("notebook")),
        "{usage}"
    );

    // Reopened, every output is there, without running anything.
    let ws = client
        .ok("GET", &format!("/api/workspaces/{id}"), None)
        .await;
    let stored: Vec<usize> = ws["state"]["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == json!("code"))
        .map(|c| c["outputs"].as_array().map_or(0, Vec::len))
        .collect();
    assert_eq!(stored, [1, 1, 2], "{ws}");
    Ok(())
}

#[tokio::test]
async fn the_demo_javascript_notebook_runs_the_try_it() -> sc_error::Result<()> {
    the_demo_try_it(sc_analytics::demo::DEMO_NOTEBOOKS[0]).await
}

#[cfg(feature = "python")]
#[tokio::test]
async fn the_demo_python_notebook_runs_the_try_it() -> sc_error::Result<()> {
    the_demo_try_it(sc_analytics::demo::DEMO_NOTEBOOKS[1]).await
}

/// The demo's LLM notebook, answered by a scripted provider, and replayed
/// without it.
#[tokio::test]
async fn the_demo_llm_notebook_is_answered_and_replayed() -> sc_error::Result<()> {
    let provider = Arc::new(FakeProvider::new([
        Reply::calls(
            "plot",
            json!({ "spec": { "data": "Houses", "mark": "boxplot", "x": "neighbourhood", "y": "price" } }),
        ),
        Reply::calls(
            "test",
            json!({ "data": "Houses", "spec": { "y": "price", "x": "neighbourhood" } }),
        ),
        Reply::says("Prices differ between the neighbourhoods."),
    ]));
    let models = FakeModels::new().role(ModelRole::Executor, Arc::clone(&provider));
    let (mut client, _db) = serve(TestDb::new().await?, Some(models), true).await?;
    let id = demo_notebook(&mut client, sc_analytics::demo::DEMO_NOTEBOOKS[2]).await;
    let cell = client.run(&id, "ask").await;
    assert_eq!(
        kinds(&cell),
        ["panel:plot", "panel:test_result", "text", "calls"],
        "{cell}"
    );
    let asked = provider.requests().len();
    let again = client.run(&id, "ask").await;
    assert_eq!(kinds(&again), kinds(&cell), "{again}");
    assert_eq!(provider.requests().len(), asked, "replayed without the LLM");
    let drawn = client.render(&again["outputs"][1]["panel"]).await;
    assert!(drawn["tests"].is_object(), "{drawn}");
    Ok(())
}
