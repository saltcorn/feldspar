//! The milestones' definitions of done: each Try it, through the API, over
//! `feldspar demo analytics`'s rows.
//!
//! **A1** (analytics TODO A1.20, A1.21):
//! the front page's two lists, holding only the demo's datasets, with no kind
//! of workspace but the Data explorer here yet; the dataset "House prices by
//! area" built operation by operation; every stage read and its rows checked
//! against the same numbers computed here from the base rows; the Filter
//! switched off and on; the Aggregate broken by renaming the column it reads,
//! and repaired; the dataset in the front page's list; and a model over a
//! named dataset fitted, predicting through a calculated field on every
//! house.
//!
//! **A2** (analytics TODO A2.16): the Data explorer's Try it — a workspace,
//! the demo's datasets, a scatter plot coloured and wrapped, a smoother on a
//! log scale with a reference line, a box plot with its tests, the same
//! filtered to two neighbourhoods, paired measurements, a summary table, a
//! histogram and a sampled scatter plot of a million events, and the state
//! reopened. The bins and box statistics are checked against the rows read
//! through the dataset; the tests against R's answers for the same rows
//! (`tests/r/demo_reference.R` → `demo_reference.json`).
//!
//! **A3** (analytics TODO A3.10): the model editor's Try it — the front
//! page's models, a linear regression of `price` on `area` and
//! `neighbourhood` fitted with every output it declares drawn (the optional
//! Q-Q plot only when asked for), the editor's view state kept without making
//! the fit out of date, a clone given a copy of the dataset with `year_built`
//! and the two compared, the dataset edited and the fits flagged, the stub
//! posterior provider (`posterior_api.rs`'s sampler) bound over the
//! neighbourhoods with its summary and its trace, rank and density plots
//! drawn, **Open as model** as the explorer does it, and what uses a model
//! listed for the delete warning. The real CmdStan half is
//! `stan_models.rs`'s `radon_in_the_model_editor`, behind `#[ignore]`.
//!
//! **A4** (analytics TODO A4.7): the Report's Try it — an explorer and a
//! report; the explorer's plot with its tests copied into the report as a
//! drop copies it, and left alone when the explorer changes; a heading,
//! Markdown, a page break and a model's coefficient table and residual plot
//! as the model editor's cards make them, reordered; a row added to `houses`
//! appearing in the report's plot but not in the fit's; the page set to A4
//! landscape; a panel copied from one report into a second; and the usage
//! index finding the reports for the delete warnings. The print dialog and
//! the PDF are walked by hand; the pagination is `report/pages.test.ts`.

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
    /// A request, answering the status, the content type and the body.
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Vec<u8>) {
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
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
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
        (status, content_type, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, _, bytes) = self.raw(method, path, body).await;
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
    setup_on(TestDb::new().await?).await
}

/// A server over `db`, after `feldspar demo analytics`, with the admin
/// logged in. On a database with PostGIS the demo makes its map tables too.
async fn setup_on(db: TestDb) -> sc_error::Result<(Client, TestDb)> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    // `feldspar demo analytics`, as the Try it begins.
    sc_analytics::demo::demo_analytics(&catalog, false).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    // The stub posterior provider, for A3's posterior half without CmdStan.
    let mut registry = models.base_registry()?;
    registry.register(Arc::new(crate::posterior_api::Sampler))?;
    models.set_registry(Arc::new(registry));
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

    // 1. The front page: only the demo's datasets (A2.15) and no workspaces
    // yet, and every kind of workspace listed with the milestone that brings
    // it — every kind but those here now: the Data explorer, the Report and
    // the Map.
    let listed = client.ok("GET", "/api/datasets", None).await;
    let mut names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Events", "Houses", "Measurements"]);
    assert_eq!(client.ok("GET", "/api/workspaces", None).await, json!([]));
    let kinds = client.ok("GET", "/api/workspace-kinds", None).await;
    assert!(
        kinds
            .as_array()
            .unwrap()
            .iter()
            .filter(
                |k| !["data_explorer", "report", "map"].contains(&k["kind"].as_str().unwrap_or(""))
            )
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
    let ours = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(id))
        .unwrap();
    assert_eq!(ours["name"], json!("House prices by area"));
    assert_eq!(ours["operations"], json!(4));
    let reopened = client.ok("GET", &format!("/api/datasets/{id}"), None).await;
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

/// R's answers for the demo's rows (`tests/r/demo_reference.R`).
fn reference() -> Value {
    serde_json::from_str(include_str!("r/demo_reference.json")).unwrap()
}

/// `a` and `b` agree to nine significant digits.
fn close(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0),
        _ => false,
    }
}

macro_rules! assert_close {
    ($a:expr, $b:expr, $what:expr) => {
        assert!(close(&$a, &$b), "{}: {} against R's {}", $what, $a, $b)
    };
}

impl Client {
    /// The id of the stored dataset called `name`.
    async fn dataset_named(&mut self, name: &str) -> String {
        let listed = self.ok("GET", "/api/datasets", None).await;
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == json!(name))
            .unwrap_or_else(|| panic!("no dataset `{name}` in {listed}"))["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// What the explorer asks for when the drop zones change.
    async fn suggest(&mut self, body: Value) -> Value {
        let answer = self.ok("POST", "/api/plots/suggest", Some(body)).await;
        assert!(answer.get("error").is_none(), "{answer}");
        answer
    }

    /// A spec drawn; a refusal is a failure here.
    async fn draw(&mut self, spec: &Value) -> Value {
        let drawn = self
            .ok("POST", "/api/plots/render", Some(json!({ "spec": spec })))
            .await;
        assert!(drawn.get("error").is_none(), "{drawn}");
        drawn
    }

    async fn tests(&mut self, spec: Value) -> Value {
        let answer = self
            .ok("POST", "/api/plots/tests", Some(json!({ "spec": spec })))
            .await;
        assert!(answer.get("error").is_none(), "{answer}");
        answer
    }
}

/// The value of `column` in `row` of a layer's data.
fn cell<'a>(layer: &'a Value, row: &'a Value, column: &str) -> &'a Value {
    let at = layer["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c == column)
        .unwrap_or_else(|| panic!("no column `{column}` in {}", layer["columns"]));
    &row[at]
}

/// The test `kind` of a section, and what it answered.
fn result<'a>(section: &'a Value, kind: &str) -> &'a Value {
    let entry = section["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["test"] == json!(kind))
        .unwrap_or_else(|| panic!("no {kind} in {section}"));
    &entry["result"]
}

#[tokio::test]
async fn the_try_it_of_milestone_a2() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let r = reference();

    // 1. A Data explorer workspace, and the demo's dataset of houses.
    let workspace = client
        .ok(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Exploring houses", "kind": "data_explorer" })),
        )
        .await;
    let workspace = workspace["id"].as_str().unwrap().to_owned();
    let houses = client.dataset_named("Houses").await;
    let houses_def = client
        .ok("GET", &format!("/api/datasets/{houses}"), None)
        .await["dataset"]
        .clone();
    let (names, rows, total) = client.stage(&houses_def, 0).await;
    assert_eq!(total, r["houses"].as_i64().unwrap());
    let col = |name: &str| names.iter().position(|n| n == name).unwrap();
    let (price, year) = (col("price"), col("year_built"));
    let sold: Vec<&Vec<Value>> = rows.iter().filter(|r| !r[price].is_null()).collect();
    assert_eq!(sold.len() as i64, r["scatter"]["n"].as_i64().unwrap());

    // 2. The scatter plot from the gallery fills X and Y with the first two
    // numbers; `price` dragged onto Y, `neighbourhood` onto Color; the mark
    // changed to a line and back.
    let scatter = client
        .suggest(json!({ "dataset": houses, "preset": "scatter" }))
        .await;
    let mut assignment = scatter["assignment"].clone();
    assert_eq!(assignment["x"]["field"], json!("area"));
    assert_eq!(assignment["y"][0]["field"], json!("bedrooms"));
    assert_eq!(scatter["spec"]["layers"][0]["mark"], json!("point"));
    assignment["y"] = json!([{ "field": "price" }]);
    assignment["color"] = json!({ "field": "neighbourhood" });
    let suggested = client
        .suggest(json!({ "dataset": houses, "assignment": assignment }))
        .await;
    let spec = suggested["spec"].clone();
    assert_eq!(spec["layers"][0]["mark"], json!("point"));
    let drawn = client.draw(&spec).await;
    let points = &drawn["layers"][0];
    // Every house is a row; those with no price are not drawn.
    let drawn_rows = points["rows"].as_array().unwrap();
    assert_eq!(drawn_rows.len(), rows.len());
    let with_price = drawn_rows
        .iter()
        .filter(|row| !cell(points, row, "y").is_null())
        .count();
    assert_eq!(with_price, sold.len());
    assert_eq!(points["sampled"], json!(false));
    assert_eq!(drawn["domains"]["color"]["values"], json!([1, 2, 3, 4, 5]));
    let line = client
        .suggest(json!({ "dataset": houses, "assignment": assignment, "mark": "line" }))
        .await;
    assert_eq!(line["spec"]["layers"][0]["mark"], json!("line"));
    client.draw(&line["spec"]).await;
    let back = client
        .suggest(json!({ "dataset": houses, "assignment": assignment, "mark": "point" }))
        .await;
    assert_eq!(back["spec"], spec);

    // 3. `year_built`, binned, on Wrap: one small plot per bin, the points
    // shared out as the rows say.
    let mut wrapped = assignment.clone();
    wrapped["wrap"] = json!({ "field": "year_built", "bin": {} });
    let suggested = client
        .suggest(json!({ "dataset": houses, "assignment": wrapped }))
        .await;
    let drawn = client.draw(&suggested["spec"]).await;
    let bins = &drawn["bins"]["year_built"];
    let (origin, width) = (
        bins["origin"].as_f64().unwrap(),
        bins["width"].as_f64().unwrap(),
    );
    let bin_of = |row: &Vec<Value>| ((row[year].as_f64().unwrap() - origin) / width).floor() as i64;
    let every_bin: std::collections::BTreeSet<i64> = rows.iter().map(bin_of).collect();
    let mut expected: BTreeMap<i64, usize> = BTreeMap::new();
    for row in &sold {
        *expected.entry(bin_of(row)).or_default() += 1;
    }
    let facets = drawn["facets"]["wrap"].as_array().unwrap();
    assert_eq!(facets.len(), every_bin.len(), "{bins} {facets:?}");
    let layer = &drawn["layers"][0];
    let mut drawn_per_bin: BTreeMap<i64, usize> = BTreeMap::new();
    for row in layer["rows"].as_array().unwrap() {
        if cell(layer, row, "y").is_null() {
            continue;
        }
        let lower = cell(layer, row, "wrap").as_f64().unwrap();
        *drawn_per_bin
            .entry(((lower - origin) / width).round() as i64)
            .or_default() += 1;
    }
    assert_eq!(drawn_per_bin, expected);

    // 4. The layers panel: a linear smoother, Y on a log scale, a reference
    // line. The smoother is R's `lm(price ~ area)`.
    let mut layered = spec.clone();
    layered["layers"].as_array_mut().unwrap().push(
        json!({ "mark": "line", "stat": { "kind": "smooth", "method": "linear" },
                      "encoding": { "x": { "field": "area" }, "y": { "field": "price" } } }),
    );
    layered["scales"] = json!({ "y": { "kind": "log" } });
    layered["references"] = json!([{ "channel": "y", "value": 300000, "label": "300k" }]);
    let drawn = client.draw(&layered).await;
    assert_eq!(drawn["warnings"], json!([]));
    let smoother = &drawn["layers"][1];
    assert_eq!(smoother["total"], json!(sold.len()));
    let curve = smoother["rows"].as_array().unwrap();
    let fit = &r["smoother"];
    let line_at = |x: f64| fit["intercept"].as_f64().unwrap() + fit["slope"].as_f64().unwrap() * x;
    for end in [&curve[0], curve.last().unwrap()] {
        let x = cell(smoother, end, "x").as_f64().unwrap();
        let y = cell(smoother, end, "y");
        assert_close!(*y, json!(line_at(x)), format!("the smoother at {x}"));
        let (lo, hi) = (
            cell(smoother, end, "y_lower").as_f64().unwrap(),
            cell(smoother, end, "y_upper").as_f64().unwrap(),
        );
        assert!(lo < y.as_f64().unwrap() && y.as_f64().unwrap() < hi);
    }
    assert_close!(
        *cell(smoother, &curve[0], "x"),
        fit["x_min"],
        "where it starts"
    );
    assert_close!(
        *cell(smoother, curve.last().unwrap(), "x"),
        fit["x_max"],
        "where it ends"
    );

    // 5. `price` on Y and `neighbourhood` on X: a box plot, its statistics
    // R's quartiles and whiskers, and an ANOVA, Kruskal–Wallis and Tukey's
    // comparisons as R has them.
    let boxed = json!({ "x": { "field": "neighbourhood" }, "y": [{ "field": "price" }] });
    let suggested = client
        .suggest(json!({ "dataset": houses, "assignment": boxed }))
        .await;
    assert_eq!(suggested["spec"]["layers"][0]["mark"], json!("box"));
    let drawn = client.draw(&suggested["spec"]).await;
    let layer = &drawn["layers"][0];
    let boxes = layer["rows"].as_array().unwrap();
    assert_eq!(boxes.len(), 5);
    for (row, expected) in boxes.iter().zip(r["boxes"].as_array().unwrap()) {
        let what = format!("box {}", cell(layer, row, "x"));
        assert_eq!(cell(layer, row, "n"), &expected["n"], "{what}");
        for (column, name) in [
            ("y_q1", "q1"),
            ("y_median", "median"),
            ("y_q3", "q3"),
            ("y_lower", "lower"),
            ("y_upper", "upper"),
        ] {
            assert_close!(
                *cell(layer, row, column),
                expected[name],
                format!("{what} {name}")
            );
        }
    }
    let data = |id: &str| json!({ "kind": "dataset", "dataset": id });
    let answer = client
        .tests(json!({ "data": data(&houses), "y": [{ "field": "price" }],
                       "x": { "field": "neighbourhood" } }))
        .await;
    assert_eq!(answer["design"], json!("number_by_groups"));
    let section = &answer["sections"][0];
    assert_eq!(section["n"], json!(sold.len()));
    let anova = result(section, "anova");
    assert_close!(anova["statistic"]["value"], r["anova"]["statistic"], "F");
    assert_close!(anova["df"][0], r["anova"]["df"][0], "ANOVA's df");
    assert_close!(anova["df"][1], r["anova"]["df"][1], "ANOVA's residual df");
    assert_close!(anova["p_value"], r["anova"]["p"], "ANOVA's p");
    let kruskal = result(section, "kruskal_wallis");
    assert_close!(
        kruskal["statistic"]["value"],
        r["kruskal"]["statistic"],
        "H"
    );
    assert_close!(kruskal["p_value"], r["kruskal"]["p"], "Kruskal–Wallis's p");
    let comparisons = section["comparisons"].as_array().unwrap();
    assert_eq!(comparisons.len(), 10);
    for (c, expected) in comparisons.iter().zip(r["tukey"].as_array().unwrap()) {
        let level =
            |i: &Value| section["levels"][i.as_u64().unwrap() as usize]["value"].to_string();
        assert_eq!(
            (level(&c["a"]), level(&c["b"])),
            (
                expected["a"].as_str().unwrap().to_owned(),
                expected["b"].as_str().unwrap().to_owned()
            )
        );
        assert_close!(c["difference"], expected["diff"], "Tukey's difference");
        assert_close!(c["p_value"], expected["p"], "Tukey's p");
    }
    // Neighbourhood makes no difference to price here that the tests can
    // see — area does. Harbour's prices fail the normality check, so the
    // sentence reports Kruskal–Wallis rather than the ANOVA.
    let failed: Vec<&Value> = section["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["ok"] == json!(false))
        .collect();
    assert_eq!(failed.len(), 1, "{section}");
    assert_eq!(
        (&failed[0]["check"], &failed[0]["of"]),
        (&json!("normality"), &json!(4))
    );
    assert_eq!(section["preferred"], json!("kruskal_wallis"));

    // Filtered to two neighbourhoods, the explorer switches to Welch's t-test
    // and the rank-sum test.
    let mut filtered = houses_def.clone();
    filtered["operations"] = json!([op(
        "two",
        "filter",
        json!({ "formula": "neighbourhood <= 2" })
    )]);
    client.save(&houses, &filtered).await;
    let answer = client
        .tests(json!({ "data": data(&houses), "y": [{ "field": "price" }],
                       "x": { "field": "neighbourhood" } }))
        .await;
    let section = &answer["sections"][0];
    assert_eq!(section["levels"].as_array().unwrap().len(), 2);
    let welch = result(section, "welch_t");
    assert_close!(
        welch["statistic"]["value"],
        r["welch"]["statistic"],
        "Welch's t"
    );
    assert_close!(welch["df"][0], r["welch"]["df"], "Welch's df");
    assert_close!(welch["p_value"], r["welch"]["p"], "Welch's p");
    let rank_sum = result(section, "mann_whitney");
    assert_close!(
        rank_sum["statistic"]["value"],
        r["rank_sum"]["statistic"],
        "W"
    );
    assert_close!(rank_sum["p_value"], r["rank_sum"]["p"], "the rank-sum p");
    let kinds: Vec<&Value> = section["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["test"])
        .collect();
    assert_eq!(kinds, [&json!("welch_t"), &json!("mann_whitney")]);
    assert_eq!(section["preferred"], json!("welch_t"));
    client.save(&houses, &houses_def).await;

    // 6. Paired measurements: before and after on Y, in paired mode.
    let measurements = client.dataset_named("Measurements").await;
    let paired = json!({ "y": [{ "field": "before" }, { "field": "after" }] });
    let suggested = client
        .suggest(json!({ "dataset": measurements, "assignment": paired }))
        .await;
    client.draw(&suggested["spec"]).await;
    let answer = client
        .tests(json!({ "data": data(&measurements),
                       "y": [{ "field": "before" }, { "field": "after" }],
                       "paired": true }))
        .await;
    assert_eq!(answer["design"], json!("paired"));
    let section = &answer["sections"][0];
    assert_eq!(section["n"], r["measurements"]);
    let t = result(section, "paired_t");
    assert_close!(
        t["statistic"]["value"],
        r["paired_t"]["statistic"],
        "the paired t"
    );
    assert_close!(t["df"][0], r["paired_t"]["df"], "the paired t's df");
    assert_close!(t["p_value"], r["paired_t"]["p"], "the paired t's p");
    assert_close!(
        t["estimate"]["value"],
        r["paired_t"]["estimate"],
        "the mean difference"
    );
    let v = result(section, "paired_signed_rank");
    assert_close!(v["statistic"]["value"], r["signed_rank"]["statistic"], "V");
    assert_close!(v["p_value"], r["signed_rank"]["p"], "the signed-rank p");

    // 7. The same drop zones as a summary table: rows by neighbourhood, the
    // mean price in the cells.
    let table = client
        .ok(
            "POST",
            "/api/plots/table",
            Some(json!({ "spec": {
                "data": data(&houses),
                "rows": [{ "field": "neighbourhood" }],
                "cells": [{ "field": "price", "function": "mean" }],
            } })),
        )
        .await;
    assert_eq!(table["body"]["columns"], json!(["r0", "n", "v0"]));
    let body = table["body"]["rows"].as_array().unwrap();
    for (row, expected) in body.iter().zip(r["table"].as_array().unwrap()) {
        assert_eq!(row[0], expected["neighbourhood"]);
        assert_eq!(row[1], expected["n"]);
        assert_close!(row[2], expected["mean"], "a mean price");
    }
    assert_eq!(table["total"], r["houses"]);

    // 8. A million events: the histogram returns only its bins, and a bin
    // holds the rows a Filter over the same range leaves.
    let events = client.dataset_named("Events").await;
    let events_def = client
        .ok("GET", &format!("/api/datasets/{events}"), None)
        .await["dataset"]
        .clone();
    let histogram = client
        .suggest(json!({ "dataset": events, "preset": "histogram",
                         "assignment": { "x": { "field": "duration_ms" } } }))
        .await;
    let started = std::time::Instant::now();
    let drawn = client.draw(&histogram["spec"]).await;
    eprintln!(
        "a histogram of a million events took {:?}",
        started.elapsed()
    );
    let layer = &drawn["layers"][0];
    let bins = layer["rows"].as_array().unwrap();
    assert!(bins.len() > 20 && bins.len() < 500, "{} bins", bins.len());
    let counted: u64 = bins
        .iter()
        .map(|b| cell(layer, b, "y").as_u64().unwrap())
        .sum();
    assert_eq!(counted, 1_000_000);
    assert_eq!(layer["total"], json!(1_000_000));
    let fullest = bins
        .iter()
        .max_by_key(|b| cell(layer, b, "y").as_u64().unwrap())
        .unwrap();
    let (lo, hi) = (cell(layer, fullest, "x"), cell(layer, fullest, "x_end"));
    let mut in_bin = events_def.clone();
    in_bin["operations"] = json!([op(
        "bin",
        "filter",
        json!({ "formula": format!("duration_ms >= {lo} && duration_ms < {hi}") })
    )]);
    let (_, _, total) = client.stage(&in_bin, 1).await;
    assert_eq!(json!(total), *cell(layer, fullest, "y"));

    // A scatter plot of them says it shows a sample.
    let scatter = client
        .suggest(json!({ "dataset": events, "assignment": {
            "x": { "field": "duration_ms" }, "y": [{ "field": "size_kb" }] } }))
        .await;
    assert_eq!(scatter["spec"]["layers"][0]["mark"], json!("point"));
    let drawn = client.draw(&scatter["spec"]).await;
    let layer = &drawn["layers"][0];
    assert_eq!(layer["sampled"], json!(true));
    assert_eq!(layer["total"], json!(1_000_000));
    assert_eq!(layer["rows"].as_array().unwrap().len(), 10_000);

    // 9. The workspace keeps what it was left with.
    let state = json!({ "dataset": houses, "assignment": boxed, "view": "plot",
                        "tests": { "show": true, "paired": false, "mu": 0 } });
    client
        .ok(
            "PUT",
            &format!("/api/workspaces/{workspace}/state"),
            Some(json!({ "state": state })),
        )
        .await;
    let reopened = client
        .ok("GET", &format!("/api/workspaces/{workspace}"), None)
        .await;
    assert_eq!(reopened["state"], state);
    assert_eq!(reopened["kind"], json!("data_explorer"));
    Ok(())
}

// --- A3: models in the Analytics UI ------------------------------------------

impl Client {
    /// Start a fit of `model` and wait for it, answering the finished fit.
    async fn fit(&mut self, model: &str) -> Value {
        let started = self
            .ok("POST", &format!("/api/models/{model}/fit"), Some(json!({})))
            .await;
        let instance = started["id"].as_str().unwrap().to_owned();
        for _ in 0..800 {
            let fit = self
                .ok("GET", &format!("/api/model-instances/{instance}"), None)
                .await;
            if fit["status"] != json!("fitting") {
                assert_eq!(fit["status"], json!("fitted"), "{fit}");
                return fit;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the fit {instance} never finished");
    }

    /// A model's outputs, with the optional plots `include` drawn.
    async fn outputs(&mut self, model: &str, include: &str) -> Value {
        self.ok(
            "GET",
            &format!("/api/models/{model}/outputs?include={include}"),
            None,
        )
        .await
    }

    /// A new dataset, answering its id.
    async fn dataset(&mut self, def: Value) -> String {
        let made = self.ok("POST", "/api/datasets", Some(def)).await;
        made["dataset"]["id"].as_str().unwrap().to_owned()
    }
}

/// The output `name` of a `getModelOutputs` answer.
fn output_of<'a>(answer: &'a Value, name: &str) -> &'a Value {
    answer["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == json!(name))
        .unwrap_or_else(|| panic!("no output `{name}` in {answer}"))
}

/// The names of a `getModelOutputs` answer's outputs.
fn output_names(answer: &Value) -> Vec<String> {
    answer["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Every output of `answer` is shown: a table with its rows, or a plot drawn
/// with data in its first layer — none refused, none with an error.
fn all_drawn(answer: &Value) {
    for o in answer["outputs"].as_array().unwrap() {
        assert!(o.get("error").is_none(), "{} has an error: {o}", o["name"]);
        match o["kind"].as_str().unwrap() {
            "table" => assert!(
                !o["table"]["rows"].as_array().unwrap().is_empty()
                    || o["table"]["text"].is_string(),
                "{} is an empty table",
                o["name"]
            ),
            _ => {
                assert!(o["spec"].is_object(), "{} has no spec", o["name"]);
                let plot = &o["plot"];
                assert!(plot["error"].is_null(), "{} was refused: {plot}", o["name"]);
                assert!(
                    !plot["layers"][0]["rows"].as_array().unwrap().is_empty(),
                    "{} drew nothing: {plot}",
                    o["name"]
                );
            }
        }
    }
}

/// The terms of a coefficient table.
fn terms(answer: &Value) -> Vec<String> {
    output_of(answer, "coefficients")["table"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_try_it_of_milestone_a3() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let client = &mut client;

    // 2. The front page's models: none yet. A dataset of the sold houses'
    //    price, area and neighbourhood, and a linear regression of price on
    //    the other two.
    assert_eq!(client.ok("GET", "/api/models", None).await, json!([]));
    let prices = client
        .dataset(json!({
            "name": "House prices",
            "base": { "kind": "table", "table": "houses" },
            "operations": [
                op("sold", "filter", json!({ "formula": "sold === true" })),
                op("name", "calculated", json!({
                    "name": "neighbourhood_name", "formula": "neighbourhoodⱵname",
                })),
                op("keep", "select", json!({ "columns": [
                    { "column": "price" }, { "column": "area" }, { "column": "neighbourhood_name" },
                ] })),
            ],
        }))
        .await;
    let model = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "House prices",
                "provider": "linear_regression",
                "dataset": { "dataset_id": prices },
                "configuration": { "label": "price" },
            })),
        )
        .await;
    let model = model["id"].as_str().unwrap().to_owned();
    assert_eq!(
        view_state(client, &model).await,
        json!({}),
        "a new model's view state is empty"
    );

    // 3. Fit it: the outputs are the coefficient table, the metrics and the
    //    two residual plots, drawn; the Q-Q plot and the histogram are in
    //    "More plots", declared but not drawn.
    let first = client.fit(&model).await;
    let first_id = first["id"].as_str().unwrap().to_owned();
    let outputs = client.outputs(&model, "").await;
    assert_eq!(outputs["fit"]["id"], json!(first_id));
    assert_eq!(outputs["fit"]["dataset_changed"], json!(false));
    assert_eq!(
        output_names(&outputs),
        [
            "coefficients",
            "statistics",
            "metrics",
            "residuals_fitted",
            "actual_predicted",
            "qq",
            "residual_histogram",
        ]
    );
    // An intercept, `area`, and a level per neighbourhood but the baseline.
    let terms_first = terms(&outputs);
    assert!(terms_first.contains(&"area".to_owned()), "{terms_first:?}");
    assert_eq!(
        terms_first
            .iter()
            .filter(|t| t.starts_with("neighbourhood_name"))
            .count(),
        4,
        "{terms_first:?}"
    );
    for optional in ["qq", "residual_histogram"] {
        assert_eq!(output_of(&outputs, optional)["optional"], json!(true));
        assert!(output_of(&outputs, optional).get("plot").is_none());
    }
    // Every declared output, drawn when asked for.
    let every = client.outputs(&model, "qq,residual_histogram").await;
    all_drawn(&every);
    let qq = &output_of(&every, "qq")["plot"]["layers"][0];
    assert_eq!(
        qq["rows"].as_array().unwrap().len() as i64,
        first["rows"]["selected"].as_i64().unwrap() - first["rows"]["dropped"].as_i64().unwrap(),
        "a point per scored row"
    );

    // 4. The editor's view state: the Q-Q plot opened, the coefficients
    //    folded, this fit selected — read back as it was left, and the fit
    //    not out of date for it.
    client
        .ok(
            "PATCH",
            &format!("/api/models/{model}/view-state"),
            Some(json!({ "patch": {
                "editor_plots": ["qq"],
                "editor_collapsed": ["coefficients"],
                "editor_fit": first_id,
            } })),
        )
        .await;
    assert_eq!(
        view_state(client, &model).await,
        json!({
            "editor_plots": ["qq"],
            "editor_collapsed": ["coefficients"],
            "editor_fit": first_id,
        })
    );
    let fits = client
        .ok("GET", &format!("/api/models/{model}/instances"), None)
        .await;
    assert_eq!(fits[0]["dataset_changed"], json!(false));

    // 5. Clone it, give the clone a copy of the dataset with `year_built`,
    //    fit it, and compare: the clone's coefficients have the new term.
    let clone = client
        .ok(
            "POST",
            &format!("/api/models/{model}/clone"),
            Some(json!({})),
        )
        .await;
    assert_eq!(clone["name"], json!("House prices (copy)"));
    assert_eq!(clone["view_state"]["editor_plots"], json!(["qq"]));
    let clone_id = clone["id"].as_str().unwrap().to_owned();
    let copy = client
        .ok(
            "POST",
            &format!("/api/datasets/{prices}/clone"),
            Some(json!({})),
        )
        .await;
    let copy_id = copy["dataset"]["id"].as_str().unwrap().to_owned();
    let mut copy_def = copy["dataset"].clone();
    copy_def["operations"][2]["params"]["columns"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "column": "year_built" }));
    client
        .ok("PUT", &format!("/api/datasets/{copy_id}"), Some(copy_def))
        .await;
    let mut clone_body = clone.clone();
    clone_body["dataset"] = json!({ "dataset_id": copy_id });
    client.ok("POST", "/api/models", Some(clone_body)).await;
    client.fit(&clone_id).await;
    let compared = [
        client.outputs(&model, "").await,
        client.outputs(&clone_id, "").await,
    ];
    assert!(!terms(&compared[0]).contains(&"year_built".to_owned()));
    assert!(terms(&compared[1]).contains(&"year_built".to_owned()));
    assert_eq!(
        output_names(&compared[0]),
        output_names(&compared[1]),
        "two regressions line up output by output"
    );

    // 6. Edit the model's dataset: its fit says the dataset has changed; the
    //    clone's, on the copy, does not.
    let mut def = client
        .ok("GET", &format!("/api/datasets/{prices}"), None)
        .await["dataset"]
        .clone();
    def["operations"]
        .as_array_mut()
        .unwrap()
        .insert(1, op("big", "filter", json!({ "formula": "area > 60" })));
    client
        .ok("PUT", &format!("/api/datasets/{prices}"), Some(def))
        .await;
    assert_eq!(
        client.outputs(&model, "").await["fit"]["dataset_changed"],
        json!(true)
    );
    let fits = client
        .ok("GET", &format!("/api/models/{model}/instances"), None)
        .await;
    assert_eq!(fits[0]["dataset_changed"], json!(true));
    assert_eq!(
        client.outputs(&clone_id, "").await["fit"]["dataset_changed"],
        json!(false)
    );

    // 7. A posterior — the stub sampler standing in for CmdStan — bound over
    //    the houses and their neighbourhoods: its summary per variable, and
    //    its trace, rank and density plots, all drawn.
    let homes = client
        .dataset(json!({
            "name": "Sold houses",
            "base": { "kind": "table", "table": "houses" },
            "operations": [
                op("sold", "filter", json!({ "formula": "sold === true" })),
                op("keep", "select", json!({ "columns": [
                    { "column": "neighbourhood" }, { "column": "area" }, { "column": "price" },
                ] })),
            ],
        }))
        .await;
    let areas = client
        .dataset(json!({
            "name": "Neighbourhoods",
            "base": { "kind": "table", "table": "neighbourhoods" },
            "operations": [],
        }))
        .await;
    let posterior = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "Prices by neighbourhood",
                "provider": "sampler",
                "dataset": { "dataset_id": homes },
                "related": [{ "name": "neighbourhoods", "dataset_id": areas, "label": "name" }],
                "configuration": { "bindings": {
                    "N": { "kind": "count", "dataset": "main" },
                    "J": { "kind": "size", "dimension": "neighbourhoods" },
                    "county": { "kind": "index", "dataset": "main", "column": "neighbourhood",
                                "dimension": "neighbourhoods" },
                    "x": { "kind": "column", "dataset": "main", "column": "area" },
                    "y": { "kind": "column", "dataset": "main", "column": "price" },
                } },
            })),
        )
        .await;
    let posterior = posterior["id"].as_str().unwrap().to_owned();
    let sampled = client.fit(&posterior).await;
    assert_eq!(sampled["outcome"]["outcome"], json!("posterior"));
    let drawn = client.outputs(&posterior, "rank,density").await;
    let names = output_names(&drawn);
    for name in ["alpha", "sigma", "trace", "rank", "density"] {
        assert!(names.contains(&name.to_owned()), "no {name} in {names:?}");
    }
    assert!(output_of(&drawn, "rank")["optional"] == json!(true));
    all_drawn(&drawn);
    // The summary of `alpha` is labelled by the neighbourhoods' names.
    let alpha = &output_of(&drawn, "alpha")["table"];
    assert_eq!(alpha["rows"].as_array().unwrap().len(), 5, "{alpha}");
    // The trace plot is one small multiple per parameter, a line per chain.
    let trace = &output_of(&drawn, "trace")["plot"];
    assert!(
        trace["facets"]["wrap"].as_array().unwrap().len() >= 2,
        "{trace}"
    );

    // 8. Open as model, as the Data explorer does it from a box plot of
    //    `price` by `neighbourhood`: a dataset on the explorer's naming the
    //    neighbourhood by its `name` (a key is a category, not a number) and
    //    keeping the two columns, and a linear regression of the one on the
    //    other.
    let houses = client.ok("GET", "/api/datasets", None).await;
    let houses = houses
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == json!("Houses"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let by = client
        .dataset(json!({
            "name": "Houses: price by neighbourhood",
            "description": "",
            "base": { "kind": "dataset", "dataset": houses },
            "operations": [
                op("op1", "calculated", json!({
                    "name": "neighbourhood_name", "formula": "neighbourhoodⱵname",
                })),
                op("op2", "select", json!({ "columns": [
                    { "column": "price" }, { "column": "neighbourhood_name" },
                ] })),
            ],
        }))
        .await;
    let opened = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "id": null, "name": "price by neighbourhood", "description": "",
                "provider": "linear_regression", "dataset": { "dataset_id": by },
                "related": [], "configuration": { "label": "price" },
                "hyperparameters": {},
                "split": { "train": 0.8, "validation": 0, "test": 0.2, "seed": 0 },
                "attributes": {},
            })),
        )
        .await;
    let opened = opened["id"].as_str().unwrap().to_owned();
    let fitted = client.fit(&opened).await;
    // The unsold houses have no price, and are dropped.
    assert!(fitted["rows"]["dropped"].as_i64().unwrap() > 0);
    let opened_terms = terms(&client.outputs(&opened, "").await);
    assert_eq!(
        opened_terms.len(),
        5,
        "an intercept and four levels: {opened_terms:?}"
    );
    assert!(
        opened_terms
            .iter()
            .all(|t| t == "(intercept)" || t.starts_with("neighbourhood_name"))
    );

    // The delete warning: what uses "House prices" by name.
    assert_eq!(
        client
            .ok("GET", &format!("/api/models/{model}/usage"), None)
            .await,
        json!({ "fields": [], "triggers": [], "workspaces": [] })
    );
    client
        .ok(
            "POST",
            &format!("/api/model-instances/{first_id}/activate"),
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
                "kind": { "type": "calc", "expression": "predict(\"House prices\") + 0" },
            })),
        )
        .await;
    client
        .ok(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "nightly refit", "description": "", "when": "none", "channel": null,
                "only_if": null, "action": "fit_model",
                "configuration": { "model": "House prices" },
                "min_role": null, "enabled": true,
            })),
        )
        .await;
    assert_eq!(
        client
            .ok("GET", &format!("/api/models/{model}/usage"), None)
            .await,
        json!({
            "fields": [{ "table": "houses", "field": "estimated_price" }],
            "triggers": [{
                "id": client.ok("GET", "/api/triggers", None).await[0]["id"],
                "name": "nightly refit", "how": "fits",
            }],
            "workspaces": [],
        })
    );
    // The clone is used by nothing.
    assert_eq!(
        client
            .ok("GET", &format!("/api/models/{clone_id}/usage"), None)
            .await,
        json!({ "fields": [], "triggers": [], "workspaces": [] })
    );
    Ok(())
}

/// A model's view state, as `getModel` answers it.
async fn view_state(client: &mut Client, model: &str) -> Value {
    client
        .ok("GET", &format!("/api/models/{model}"), None)
        .await["view_state"]
        .clone()
}

// --- A4: reports, and drag and drop -----------------------------------------

impl Client {
    /// A new workspace of `kind`, answering its id.
    async fn workspace(&mut self, name: &str, kind: &str) -> String {
        let made = self
            .ok(
                "POST",
                "/api/workspaces",
                Some(json!({ "name": name, "kind": kind })),
            )
            .await;
        made["id"].as_str().unwrap().to_owned()
    }

    /// Save a workspace's state.
    async fn save_state(&mut self, id: &str, state: &Value) {
        self.ok(
            "PUT",
            &format!("/api/workspaces/{id}/state"),
            Some(json!({ "state": state })),
        )
        .await;
    }

    /// A workspace's stored state.
    async fn state_of(&mut self, id: &str) -> Value {
        self.ok("GET", &format!("/api/workspaces/{id}"), None).await["state"].clone()
    }

    /// A panel drawn as a report draws it; a sentence instead is a failure.
    async fn render(&mut self, panel: &Value) -> Value {
        let drawn = self
            .ok(
                "POST",
                "/api/panels/render",
                Some(json!({ "panel": panel })),
            )
            .await;
        assert!(drawn.get("error").is_none(), "{drawn}");
        drawn
    }
}

/// What a drop does with a dragged panel (`readPanelDrag`): the JSON as it
/// was when the drag began, with an identity of its own.
fn dropped(dragged: &Value) -> Value {
    let mut copy = dragged.clone();
    copy["id"] = json!(uuid::Uuid::new_v4().to_string());
    copy
}

/// A report block holding `panel`.
fn panel_block(panel: &Value) -> Value {
    json!({ "id": panel["id"], "kind": "panel", "panel": panel })
}

/// The kinds of a report's blocks, in order.
fn block_kinds(state: &Value) -> Vec<String> {
    state["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// The `(name, kind, panels)` of a usage answer's workspaces.
fn users(usage: &Value) -> Vec<(String, String, i64)> {
    usage["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["name"].as_str().unwrap().to_owned(),
                w["kind"].as_str().unwrap().to_owned(),
                w["panels"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_try_it_of_milestone_a4() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let client = &mut client;
    let houses = client.dataset_named("Houses").await;
    let houses_rows = client
        .ok("GET", "/api/tables/houses/rows?limit=500", None)
        .await
        .as_array()
        .unwrap()
        .len();

    // 1. The Data explorer of A2 beside a new Report: both kinds are here.
    let kinds = client.ok("GET", "/api/workspace-kinds", None).await;
    for kind in ["data_explorer", "report"] {
        let k = kinds
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == json!(kind))
            .unwrap();
        assert_eq!(k["available"], json!(true), "{k}");
    }
    let explorer = client.workspace("Exploring houses", "data_explorer").await;
    let assignment = json!({
        "x": { "field": "area" }, "y": [{ "field": "price" }],
        "color": { "field": "neighbourhood" },
    });
    client
        .save_state(
            &explorer,
            &json!({ "dataset": houses, "assignment": assignment, "view": "plot" }),
        )
        .await;
    let report = client.workspace("House prices report", "report").await;
    assert_eq!(client.state_of(&report).await, json!({}));

    // 2. The explorer's plot, with its tests, dragged into the report: the
    //    drag carries the panel as it is, the drop keeps a copy.
    let scatter = client
        .suggest(json!({ "dataset": houses, "assignment": assignment }))
        .await["spec"]
        .clone();
    let data = json!({ "kind": "dataset", "dataset": houses });
    let dragged = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "title": "price by area — Houses",
        "kind": "test_result",
        "content": {
            "tests": { "data": data, "y": [{ "field": "price" }], "x": { "field": "area" } },
            "plot": scatter,
        },
    });
    let explorer_panel = dropped(&dragged);
    assert_ne!(explorer_panel["id"], dragged["id"]);
    client
        .save_state(
            &report,
            &json!({ "blocks": [panel_block(&explorer_panel)] }),
        )
        .await;
    // The plot in the explorer changes — a box plot now — and the report's
    // copy does not.
    let boxed = json!({ "x": { "field": "neighbourhood" }, "y": [{ "field": "price" }] });
    client
        .save_state(
            &explorer,
            &json!({ "dataset": houses, "assignment": boxed, "view": "plot" }),
        )
        .await;
    let kept = client.state_of(&report).await;
    assert_eq!(kept["blocks"][0]["panel"], explorer_panel);
    let drawn = client.render(&explorer_panel).await;
    assert_eq!(drawn["kind"], json!("test_result"));
    assert_eq!(drawn["plot"]["layers"][0]["total"], json!(houses_rows));
    assert_eq!(drawn["tests"]["design"], json!("two_numbers"));

    // 3. A heading and Markdown above it, a page break; then a coefficient
    //    table and a residual plot dragged from the model editor's output
    //    cards (the table names the fit, the plot is its spec over the fit's
    //    output data).
    let sold = client
        .dataset(json!({
            "name": "Sold houses",
            "base": { "kind": "table", "table": "houses" },
            "operations": [
                op("sold", "filter", json!({ "formula": "sold === true" })),
                op("keep", "select", json!({ "columns": [
                    { "column": "price" }, { "column": "area" }, { "column": "bedrooms" },
                ] })),
            ],
        }))
        .await;
    let model = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "House prices",
                "provider": "linear_regression",
                "dataset": { "dataset_id": sold },
                "configuration": { "label": "price" },
            })),
        )
        .await;
    let model = model["id"].as_str().unwrap().to_owned();
    let fit = client.fit(&model).await;
    let fit_id = fit["id"].as_str().unwrap().to_owned();
    let outputs = client.outputs(&model, "").await;
    let coefficients = dropped(&json!({
        "id": "card-coefficients", "title": "Coefficients — House prices",
        "kind": "fit_table", "content": { "fit": fit_id, "output": "coefficients" },
    }));
    let residuals = dropped(&json!({
        "id": "card-residuals", "title": "Residuals against fitted values — House prices",
        "kind": "plot", "content": { "spec": output_of(&outputs, "residuals_fitted")["spec"] },
    }));
    let heading = json!({ "id": "h1", "kind": "heading", "text": "House prices", "level": 1 });
    let text = json!({ "id": "t1", "kind": "text",
                       "markdown": "Prices rise with **area**; see `price_per_m2`." });
    let page_break = json!({ "id": "pb", "kind": "page_break" });
    let blocks = json!([
        heading,
        text,
        panel_block(&explorer_panel),
        page_break,
        panel_block(&residuals),
        panel_block(&coefficients),
    ]);
    client
        .save_state(&report, &json!({ "blocks": blocks }))
        .await;
    // Reordered: the coefficient table above the residual plot.
    let mut reordered = blocks.as_array().unwrap().clone();
    reordered.swap(4, 5);
    client
        .save_state(&report, &json!({ "blocks": reordered }))
        .await;
    let state = client.state_of(&report).await;
    assert_eq!(
        block_kinds(&state),
        ["heading", "text", "panel", "page_break", "panel", "panel"]
    );
    assert_eq!(state["blocks"][4]["panel"]["kind"], json!("fit_table"));
    let table = client.render(&coefficients).await;
    let terms: Vec<&str> = table["output"]["table"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap())
        .collect();
    assert!(
        terms.contains(&"area") && terms.contains(&"bedrooms"),
        "{table}"
    );
    let fitted = client.render(&residuals).await;
    let scored = fitted["plot"]["layers"][0]["rows"]
        .as_array()
        .unwrap()
        .len();
    assert!(scored > 0, "{fitted}");
    // A report refuses a block it does not have, naming it.
    let mut broken = reordered.clone();
    broken.push(json!({ "id": "c", "kind": "chart" }));
    let (status, err) = client
        .send(
            "PUT",
            &format!("/api/workspaces/{report}/state"),
            Some(json!({ "state": { "blocks": broken } })),
        )
        .await;
    assert!(!status.is_success(), "{err}");
    assert!(err.to_string().contains("not a kind of block"), "{err}");

    // 4. A row added to `houses` in the admin is in the report's plot the
    //    next time it is drawn: panels are live views of their datasets. The
    //    residual plot is of the fit's scored rows, which a new row does not
    //    change until the model is fitted again.
    client
        .ok(
            "POST",
            "/api/tables/houses/rows",
            Some(json!({
                "address": "1 New Street", "area": 95.0, "bedrooms": 3, "neighbourhood": 1,
                "year_built": 2024, "sold": true, "price": 210000.0,
            })),
        )
        .await;
    let drawn = client.render(&explorer_panel).await;
    assert_eq!(drawn["plot"]["layers"][0]["total"], json!(houses_rows + 1));
    let fitted = client.render(&residuals).await;
    assert_eq!(
        fitted["plot"]["layers"][0]["rows"]
            .as_array()
            .unwrap()
            .len(),
        scored
    );

    // 5. A4 landscape. (The PDF is the browser's print dialog, walked by hand;
    //    the pagination is `report/pages.test.ts`.)
    let page = json!({ "size": "A4", "orientation": "landscape" });
    client
        .save_state(&report, &json!({ "blocks": reordered, "page": page }))
        .await;
    let state = client.state_of(&report).await;
    assert_eq!(state["page"], page);
    let (status, _) = client
        .send(
            "PUT",
            &format!("/api/workspaces/{report}/state"),
            Some(json!({ "state": { "blocks": reordered, "page": { "size": "B5", "orientation": "portrait" } } })),
        )
        .await;
    assert!(!status.is_success(), "a B5 page is refused");

    // 6. The explorer's panel dragged from this report into a second one: a
    //    copy with an identity of its own.
    let second = client.workspace("Summary for the board", "report").await;
    let again = dropped(&state["blocks"][2]["panel"]);
    assert_ne!(again["id"], explorer_panel["id"]);
    assert_eq!(again["content"], explorer_panel["content"]);
    client
        .save_state(
            &second,
            &json!({ "blocks": [panel_block(&again)], "page": { "size": "Letter", "orientation": "portrait" } }),
        )
        .await;

    // 7. The delete warning for `Houses` lists both reports and the explorer;
    //    that of the model's dataset lists only its model (the report reads
    //    the fit, not the dataset); the model's lists the first report, whose
    //    two panels show its fit.
    let usage = client
        .ok("GET", &format!("/api/datasets/{houses}/usage"), None)
        .await;
    let mut found = users(&usage);
    found.sort();
    assert_eq!(
        found,
        vec![
            ("Exploring houses".to_owned(), "data_explorer".to_owned(), 0),
            ("House prices report".to_owned(), "report".to_owned(), 1),
            ("Summary for the board".to_owned(), "report".to_owned(), 1),
        ],
        "{usage}"
    );
    let usage = client
        .ok("GET", &format!("/api/datasets/{sold}/usage"), None)
        .await;
    assert_eq!(usage["models"][0]["id"], json!(model), "{usage}");
    assert_eq!(usage["workspaces"], json!([]), "{usage}");
    let usage = client
        .ok("GET", &format!("/api/models/{model}/usage"), None)
        .await;
    assert_eq!(
        users(&usage),
        vec![("House prices report".to_owned(), "report".to_owned(), 2)],
        "{usage}"
    );

    // Reopened, the report is as it was left.
    assert_eq!(client.state_of(&report).await, state);
    Ok(())
}

// --- A5: maps ----------------------------------------------------------------

/// The tutorial's file (part 5, step 1): four police stations in the demo's
/// city.
const POLICE_STATIONS: &[u8] =
    include_bytes!("../../../docs/tutorial-data/police-stations.geojson");

/// A GeoJSON point's longitude and latitude.
fn lon_lat(point: &Value) -> (f64, f64) {
    assert_eq!(point["type"], "Point", "{point}");
    let c = &point["coordinates"];
    (c[0].as_f64().unwrap(), c[1].as_f64().unwrap())
}

/// A polygon's outer ring.
fn ring(polygon: &Value) -> Vec<(f64, f64)> {
    assert_eq!(polygon["type"], "Polygon", "{polygon}");
    polygon["coordinates"][0]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
        .collect()
}

/// Whether `(x, y)` is inside a closed ring, by the even–odd rule — in the
/// plane of longitude and latitude, as `ST_Within` over a geometry decides.
fn inside(ring: &[(f64, f64)], (x, y): (f64, f64)) -> bool {
    let mut odd = false;
    for k in 1..ring.len() {
        let ((x1, y1), (x2, y2)) = (ring[k - 1], ring[k]);
        if (y1 > y) != (y2 > y) && x < x1 + (y - y1) * (x2 - x1) / (y2 - y1) {
            odd = !odd;
        }
    }
    odd
}

/// The great-circle distance in metres on the mean Earth sphere: within half
/// a per cent of the spheroid's that `Geo.distance` measures.
fn haversine((lon1, lat1): (f64, f64), (lon2, lat2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let a = ((p2 - p1) / 2.0).sin().powi(2)
        + p1.cos() * p2.cos() * ((lon2 - lon1).to_radians() / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

/// The features of a GeoJSON layer as `(id, properties, geometry)`.
fn features(drawn_layer: &Value) -> Vec<(i64, Value, Value)> {
    assert_eq!(drawn_layer["data"]["delivery"], "geojson", "{drawn_layer}");
    drawn_layer["data"]["data"]["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["id"].as_i64().unwrap(),
                f["properties"].clone(),
                f["geometry"].clone(),
            )
        })
        .collect()
}

/// A query-string component, every byte but a letter or digit escaped.
fn encode_component(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The `(x, y)` of the Web Mercator tile at `zoom` holding `(lon, lat)`.
fn tile_of((lon, lat): (f64, f64), zoom: u32) -> (u32, u32) {
    let n = f64::from(1u32 << zoom);
    let lat = lat.to_radians();
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor();
    (x as u32, y as u32)
}

impl Client {
    /// A map spec drawn; a layer refused is a failure here.
    async fn draw_map(&mut self, spec: &Value) -> Value {
        let drawn = self
            .ok("POST", "/api/maps/render", Some(json!({ "spec": spec })))
            .await;
        for layer in drawn["layers"].as_array().unwrap() {
            assert_ne!(layer["data"]["delivery"], "none", "{layer}");
        }
        drawn
    }
}

#[tokio::test]
async fn the_try_it_of_milestone_a5() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let (mut client, _db) = setup_on(db).await?;
    let client = &mut client;
    let [west, south, east, north] = sc_analytics::demo::DEMO_EXTENT;
    let in_city =
        |(lon, lat): (f64, f64)| (west..=east).contains(&lon) && (south..=north).contains(&lat);

    // 1. A GeoJSON file imported as a new table; its rows are a map layer.
    use base64::Engine as _;
    let (status, imported) = client
        .send(
            "POST",
            "/api/tables/geo",
            Some(json!({
                "name": "police_stations", "file_name": "police-stations.geojson",
                "content_base64": base64::engine::general_purpose::STANDARD.encode(POLICE_STATIONS),
            })),
        )
        .await;
    assert!(status.is_success(), "{status} {imported}");
    let stations = client
        .ok(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "Police stations",
                         "base": { "kind": "table", "table": "police_stations" } })),
        )
        .await["dataset"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let suggested = client
        .ok(
            "POST",
            "/api/maps/suggest",
            Some(json!({ "dataset": stations, "assignment": { "label": { "field": "name" } } })),
        )
        .await;
    let drawn = client.draw_map(&suggested["spec"]).await;
    let stations_drawn = features(&drawn["layers"][0]);
    assert_eq!(stations_drawn.len(), 4);
    assert_eq!(drawn["layers"][0]["data"]["geometry"], json!(["point"]));
    let (_, central, at) = &stations_drawn[0];
    assert_eq!(central["name"], "Central", "{central}");
    assert_eq!(central["officers"], 64);
    assert_eq!(lon_lat(at), (4.8512, 45.7486));

    // 2. The incidents in the explorer's map, coloured by category.
    let incidents = client.dataset_named("Incidents").await;
    let districts = client.dataset_named("Districts").await;
    let explorer = client
        .workspace("Exploring incidents", "data_explorer")
        .await;
    let assignment = json!({ "color": { "field": "category" } });
    client
        .save_state(
            &explorer,
            &json!({ "dataset": incidents, "assignment": assignment, "view": "map" }),
        )
        .await;
    let suggested = client
        .ok(
            "POST",
            "/api/maps/suggest",
            Some(json!({ "dataset": incidents, "assignment": assignment })),
        )
        .await;
    assert_eq!(
        suggested["sources"][0]["source"],
        json!({ "kind": "column", "column": "location" })
    );
    let drawn = client.draw_map(&suggested["spec"]).await;
    let layer = &drawn["layers"][0];
    assert_eq!(layer["data"]["count"], 2400, "{}", layer["data"]["count"]);
    assert_eq!(
        layer["domains"]["color"]["values"],
        json!([
            "antisocial behaviour",
            "burglary",
            "theft",
            "vandalism",
            "vehicle crime"
        ])
    );
    let points: Vec<(i64, (f64, f64))> = features(layer)
        .iter()
        .map(|(id, _, g)| (*id, lon_lat(g)))
        .collect();
    assert!(points.iter().all(|(_, p)| in_city(*p)));

    // 3. Open in map: a Map workspace whose first layer is the explorer's.
    let map = client.workspace("Incidents map", "map").await;
    let mut incidents_layer = suggested["spec"]["layers"][0].clone();
    incidents_layer["id"] = json!("incidents");
    incidents_layer["name"] = json!("Incidents");
    client
        .save_state(
            &map,
            &json!({ "layers": [incidents_layer], "reference": [] }),
        )
        .await;

    // 4. Aggregate → Count per region with the districts: a dataset of a
    //    Spatial join and an Aggregate, every district with its count.
    let districts_layer = json!({ "id": "districts", "name": "Districts", "dataset": districts,
                                  "geometry": { "kind": "column", "column": "outline" } });
    let outlines: Vec<(i64, Vec<(f64, f64)>)> = features(
        &client
            .draw_map(&json!({ "layers": [districts_layer] }))
            .await["layers"][0],
    )
    .iter()
    .map(|(id, _, g)| (*id, ring(g)))
    .collect();
    assert_eq!(outlines.len(), sc_analytics::demo::DEMO_DISTRICTS);
    let mut expected: BTreeMap<i64, i64> = outlines.iter().map(|(id, _)| (*id, 0)).collect();
    for (_, p) in &points {
        let homes: Vec<i64> = outlines
            .iter()
            .filter(|(_, r)| inside(r, *p))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(homes.len(), 1, "{p:?} is in {homes:?}");
        *expected.get_mut(&homes[0]).unwrap() += 1;
    }
    let (status, run) = client
        .send(
            "POST",
            "/api/maps/tools/run",
            Some(json!({ "tool": "count_per_region",
                         "params": { "layer": incidents_layer, "regions": districts_layer } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    assert_eq!(run["dataset"]["name"], "Incidents per Districts");
    let kinds: Vec<&str> = run["dataset"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["spatial_join", "aggregate", "complete"]);
    let (names, rows, total) = client.stage(&run["dataset"], kinds.len()).await;
    assert_eq!(total, 12);
    let counted: BTreeMap<i64, i64> = rows
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), r[1].as_i64().unwrap()))
        .collect();
    assert_eq!(counted, expected, "{names:?}");
    // The new dataset is a global one, and opens in the Dataset editor.
    let listed = client.ok("GET", "/api/datasets", None).await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == "Incidents per Districts"),
        "{listed}"
    );
    // Graduated colours in five natural-breaks classes: Jenks over the counts.
    let mut per_district = run["layer"].clone();
    per_district["id"] = json!("per-district");
    per_district["style"] =
        json!({ "kind": "graduated", "method": "natural_breaks", "classes": 5 });
    let layers = json!([districts_layer, incidents_layer, per_district]);
    let drawn = client.draw_map(&json!({ "layers": layers })).await;
    let classes: Vec<f64> = drawn["layers"][2]["classes"]
        .as_array()
        .unwrap_or_else(|| panic!("no classes: {}", drawn["layers"][2]))
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let mut counts: Vec<f64> = expected.values().map(|c| *c as f64).collect();
    counts.sort_by(f64::total_cmp);
    assert_eq!(
        classes,
        sc_analytics::classify::breaks(
            &counts,
            5,
            sc_analytics::classify::Classification::NaturalBreaks
        )
    );
    assert_eq!(classes.len(), 6);
    assert_eq!((classes[0], classes[5]), (counts[0], counts[11]));

    // 5. The attribute table sorted by count: the top three districts first,
    //    selected — the selection is the workspace's. An aggregate's rows
    //    have no key, so the server answers them in the dataset's order and
    //    the table sorts them (`sortRows`); each row's id is its place.
    let table = client
        .ok(
            "POST",
            "/api/layers/rows",
            Some(json!({ "layer": per_district,
                         "sort": { "formula": names[1], "descending": true } })),
        )
        .await;
    assert_eq!(
        (table["keyed"].clone(), table["sorted"].clone()),
        (json!(false), json!(false))
    );
    let mut sorted: Vec<(i64, i64, i64)> = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .zip(table["ids"].as_array().unwrap())
        .map(|(r, id)| {
            (
                id.as_i64().unwrap(),
                r[0].as_i64().unwrap(),
                r[1].as_i64().unwrap(),
            )
        })
        .collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.2));
    let top3: Vec<i64> = sorted.iter().take(3).map(|(id, _, _)| *id).collect();
    let mut busiest: Vec<(i64, i64)> = expected.iter().map(|(d, c)| (*d, *c)).collect();
    busiest.sort_by_key(|r| std::cmp::Reverse(r.1));
    let top_districts: Vec<i64> = sorted.iter().take(3).map(|(_, d, _)| *d).collect();
    assert_eq!(
        top_districts,
        busiest.iter().take(3).map(|(d, _)| *d).collect::<Vec<_>>()
    );
    client
        .save_state(
            &map,
            &json!({ "layers": layers, "reference": [],
                     "selection": { "layer": "per-district", "ids": top3 },
                     "active": "per-district" }),
        )
        .await;

    // 6. The incidents within 1 km of a point in the busiest district, saved
    //    as a dataset.
    let ring_of = &outlines
        .iter()
        .find(|(id, _)| *id == top_districts[0])
        .unwrap()
        .1;
    let corners = &ring_of[..ring_of.len() - 1];
    let centre = (
        corners.iter().map(|p| p.0).sum::<f64>() / corners.len() as f64,
        corners.iter().map(|p| p.1).sum::<f64>() / corners.len() as f64,
    );
    let found = client
        .ok(
            "POST",
            "/api/layers/select",
            Some(json!({ "layer": incidents_layer,
                         "by": { "by": "near_point", "longitude": centre.0,
                                 "latitude": centre.1, "distance": 1000 } })),
        )
        .await;
    let near: Vec<i64> = found["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert!(near.len() > 20, "{} incidents near {centre:?}", near.len());
    for (id, p) in &points {
        let d = haversine(centre, *p);
        if near.contains(id) {
            assert!(d < 1005.0, "incident {id} is {d} m away and selected");
        } else {
            assert!(d > 995.0, "incident {id} is {d} m away and not selected");
        }
    }
    let (status, saved) = client
        .send(
            "POST",
            "/api/layers/selection",
            Some(
                json!({ "layer": incidents_layer, "name": "Incidents near the busiest district",
                         "condition": found["condition"] }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(
        saved["dataset"]["base"],
        json!({ "kind": "dataset", "dataset": incidents })
    );
    let upto = saved["dataset"]["operations"].as_array().unwrap().len();
    let (_, _, total) = client.stage(&saved["dataset"], upto).await;
    assert_eq!(total, near.len() as i64);

    // 7. A reference layer from a tile service, and the layers' opacity.
    let mut layers = layers.clone();
    layers[0]["opacity"] = json!(0.5);
    let reference = json!([{ "id": "osm", "name": "OpenStreetMap", "kind": "tiles",
                             "url": "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
                             "opacity": 0.6 }]);
    let view = json!({ "center": [4.85, 45.75], "zoom": 12 });
    let state = json!({ "layers": layers, "reference": reference, "view": view,
                        "selection": { "layer": "per-district", "ids": top3 } });
    client.save_state(&map, &state).await;
    let kept = client.state_of(&map).await;
    assert_eq!(kept["reference"][0]["opacity"], json!(0.6));
    assert_eq!(kept["layers"][0]["opacity"], json!(0.5));
    let (status, refused) = client
        .send(
            "PUT",
            &format!("/api/workspaces/{map}/state"),
            Some(json!({ "state": { "layers": layers, "reference": [{
                "id": "bad", "name": "Bad", "kind": "tiles", "url": "https://example.com/tiles.png",
                "opacity": 1 }] } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // 8. The whole map dragged into a report, drawn as the report draws it.
    let report = client.workspace("Incidents report", "report").await;
    let panel = json!({
        "id": uuid::Uuid::new_v4().to_string(), "kind": "map", "title": "Incidents map",
        "content": { "spec": { "layers": layers, "reference": reference, "view": view } },
    });
    client
        .save_state(&report, &json!({ "blocks": [panel_block(&panel)] }))
        .await;
    let rendered = client.render(&panel).await;
    assert_eq!(rendered["kind"], "map");
    assert_eq!(rendered["map"]["layers"].as_array().unwrap().len(), 3);
    assert_eq!(rendered["map"]["layers"][2]["classes"], json!(classes));
    let usage = client
        .ok("GET", &format!("/api/datasets/{incidents}/usage"), None)
        .await;
    let mut using = users(&usage);
    using.sort();
    assert_eq!(
        using,
        [
            (
                "Exploring incidents".to_owned(),
                "data_explorer".to_owned(),
                0
            ),
            ("Incidents map".to_owned(), "map".to_owned(), 1),
            ("Incidents report".to_owned(), "report".to_owned(), 1),
        ]
    );

    // A vector tile of the incidents: the city's tile at zoom 12 has them,
    // a tile of the open ocean has none.
    let tile_layer = json!({ "dataset": incidents,
                             "geometry": { "kind": "column", "column": "location" },
                             "properties": ["category"] });
    let query = encode_component(&tile_layer.to_string());
    let (x, y) = tile_of(centre, 12);
    let (status, content_type, bytes) = client
        .raw(
            "GET",
            &format!("/api/layers/tiles/12/{x}/{y}?layer={query}"),
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(content_type, "application/vnd.mapbox-vector-tile");
    assert!(
        bytes.windows(8).any(|w| w == b"features"),
        "the tile names its layer"
    );
    assert!(
        bytes.windows(8).any(|w| w == b"burglary"),
        "the tile has incidents"
    );
    let (status, _, empty) = client
        .raw(
            "GET",
            &format!("/api/layers/tiles/12/0/2048?layer={query}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!empty.windows(8).any(|w| w == b"burglary"));
    Ok(())
}
