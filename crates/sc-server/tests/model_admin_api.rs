//! Phase 5 integration test: the **model** API, driven through the assembled
//! router as the admin SPA drives it (TODO "Predictive models", task 5.7).
//!
//! Four things are asserted here and nowhere else, because each one spans the
//! endpoint declaration, the handler, the row and — for the last two — a
//! spawned job or a firing trigger:
//!
//! 1. **The whole lifecycle over HTTP.** The providers are listed against a
//!    dataset (so a label picker offers *these* columns), the dataset previews,
//!    the model saves, fits, and its instance activates.
//! 2. **A fit is a job, polled to completion.** `fitModel` answers `fitting`
//!    immediately and the row becomes `fitted` on its own — which is the whole
//!    of §8, and the reason there is no in-memory job registry.
//! 3. **A fit that fails leaves the sentence on the instance.** Not an HTTP
//!    error: the request that started it has long since returned, so the only
//!    place the reason can be is the row.
//! 4. **A row the dataset's filter excludes is predicted through the dataset.**
//!    The model's *active* instance answers about a house inserted after the
//!    fit, with its join path computed by the row layer. (This was the
//!    `predict_row` action's test; milestone 31 removed the action, and the
//!    same prediction is `predict("…")` in a formula from its Phase 3.)
#![allow(clippy::unwrap_used, clippy::expect_used)]

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

/// Sixty houses whose price is an exact linear function of two columns, plus a
/// `neighbourhoods` table to hang a join path off and an `estimate` column for
/// the trigger to write into.
///
/// Exact rather than noisy on purpose: the fit's R² is then 1 and the assertion
/// can be about the *machinery* rather than about how well a regression happened
/// to do on random data.
const SCHEMA: &str = "
    CREATE TABLE neighbourhoods (id bigint primary key, average_income double precision);
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        bedrooms bigint,
        price double precision,
        estimate double precision,
        neighbourhood bigint references neighbourhoods(id)
    );
    INSERT INTO neighbourhoods VALUES (1, 40000), (2, 90000);
    INSERT INTO houses (id, area, bedrooms, price, neighbourhood)
      SELECT i,
             50 + i,
             1 + (i % 5),
             1000 * (50 + i) + 20000 * (1 + (i % 5)),
             1 + (i % 2)
      FROM generate_series(1, 60) AS i;
";

/// A cookie-jar-carrying client over the router (CSRF + session), as the other
/// admin-API tests use.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
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
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if value.is_empty() {
                        self.cookies.remove(name);
                    } else {
                        self.cookies.insert(name.to_owned(), value.to_owned());
                    }
                }
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

    /// Poll one instance until it stops saying `fitting`.
    ///
    /// The whole of §8 from the client's side: the fit is a spawned task, so
    /// there is nothing to await — the row is the registry and this is what the
    /// screen does.
    async fn await_fit(&mut self, instance: &str) -> Value {
        for _ in 0..600 {
            let (status, body) = self
                .send("GET", &format!("/api/model-instances/{instance}"), None)
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            if body["status"] != json!("fitting") {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the fit of instance {instance} never finished");
    }
}

struct Server {
    client: Client,
    catalog: Arc<Catalog>,
    _db: TestDb,
}

/// The dataset of the milestone's own definition of done, minus the aggregation:
/// two plain columns, a join path, and the label.
fn dataset() -> Value {
    json!({
        "table": "houses",
        "columns": [
            { "name": "price", "expr": "price" },
            { "name": "area", "expr": "area" },
            { "name": "bedrooms", "expr": "bedrooms" },
            { "name": "income", "expr": "neighbourhoodⱵaverage_income" },
        ],
    })
}

/// The body `saveModel` takes for a linear regression of `price`.
fn model_body(name: &str) -> Value {
    json!({
        "name": name,
        "description": "what a house is worth",
        "provider": "linear_regression",
        "dataset": dataset(),
        "configuration": { "label": "price", "intercept": true },
        "hyperparameters": {},
        "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
        "attributes": {},
    })
}

async fn setup() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(&format!(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$; {SCHEMA}"
        ))
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
            .with_triggers(dispatcher.clone())
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
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    Ok(Server {
        client,
        catalog,
        _db: db,
    })
}

#[tokio::test]
async fn a_model_is_previewed_saved_fitted_and_activated() -> sc_error::Result<()> {
    let mut server = setup().await?;

    // The dataset first, because everything else is about it: the four things
    // GOALS asks a dataset for are one language, and the preview is what makes
    // the builder a thing you can see the answer of.
    let (status, preview) = server
        .client
        .send(
            "POST",
            "/api/model-datasets/preview",
            Some(json!({ "dataset": dataset(), "limit": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let columns = preview["columns"].as_array().unwrap();
    assert_eq!(columns.len(), 4);
    assert_eq!(columns[0]["name"], json!("price"));
    assert_eq!(columns[0]["type"], json!("float"));
    // The join path answered per row, and the limit honoured.
    assert_eq!(preview["rows"].as_array().unwrap().len(), 5);
    assert!(preview["rows"][0]["income"].as_f64().is_some(), "{preview}");
    assert_eq!(preview["primary_key"], json!("id"));
    assert_eq!(preview["split_error"], Value::Null);

    // The providers, resolved **against that dataset**: a label picker offers
    // these columns rather than a free-text box, which is the whole reason
    // `config_spec` takes a shape.
    let query = format!(
        "/api/model-providers?dataset={}",
        urlencoding(&dataset().to_string())
    );
    let (status, listed) = server.client.send("GET", &query, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["builtins_compiled_out"], json!(false));
    let providers = listed["providers"].as_array().unwrap();
    let linear = providers
        .iter()
        .find(|p| p["name"] == json!("linear_regression"))
        .unwrap_or_else(|| panic!("no linear_regression among {providers:?}"));
    assert_eq!(linear["module"], Value::Null);
    let label = linear["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("label"))
        .unwrap();
    let options: Vec<&str> = label["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(options, vec!["price", "area", "bedrooms", "income"]);
    // Without a configuration naming one, the outcome cannot be resolved — and
    // that is reported as the sentence the form is waiting for rather than as a
    // failed request.
    assert_eq!(linear["outcome"], Value::Null);
    assert!(
        linear["outcome_error"]
            .as_str()
            .is_some_and(|e| e.contains("label")),
        "{linear}"
    );

    // The same call with a configuration resolves the outcome.
    let query = format!(
        "/api/model-providers?dataset={}&configuration={}",
        urlencoding(&dataset().to_string()),
        urlencoding(&json!({ "label": "price" }).to_string())
    );
    let (_, listed) = server.client.send("GET", &query, None).await;
    let linear = listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == json!("linear_regression"))
        .unwrap()
        .clone();
    assert_eq!(linear["outcome"]["outcome"], json!("regression"));
    assert_eq!(linear["outcome"]["label"], json!("price"));

    // Save, and it is listed with its table and no reason not to fit it.
    let (status, saved) = server
        .client
        .send("POST", "/api/models", Some(model_body("house prices")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let id = saved["id"].as_str().unwrap().to_owned();
    assert_eq!(saved["table_name"], json!("houses"));
    assert_eq!(saved["error"], Value::Null);
    assert_eq!(saved["instances"], json!(0));

    let (status, models) = server.client.send("GET", "/api/models", None).await;
    assert_eq!(status, StatusCode::OK, "{models}");
    assert_eq!(models.as_array().unwrap().len(), 1);
    assert_eq!(models[0]["provider"], json!("linear_regression"));
    assert_eq!(models[0]["dataset"]["columns"][3]["name"], json!("income"));

    // …and by table, which is what `fit_model`'s picker asks.
    let (_, by_table) = server
        .client
        .send("GET", "/api/models?table=houses", None)
        .await;
    assert_eq!(by_table.as_array().unwrap().len(), 1);
    let (_, other_table) = server
        .client
        .send("GET", "/api/models?table=neighbourhoods", None)
        .await;
    assert!(other_table.as_array().unwrap().is_empty());

    // Fit. The response is the row, saying `fitting`, before the work is done.
    let (status, started) = server
        .client
        .send(
            "POST",
            &format!("/api/models/{id}/fit"),
            Some(json!({ "name": "first" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert_eq!(started["status"], json!("fitting"));
    assert_eq!(started["active"], json!(false));
    let instance = started["id"].as_str().unwrap().to_owned();

    let fitted = server.client.await_fit(&instance).await;
    assert_eq!(fitted["status"], json!("fitted"), "{fitted}");
    assert_eq!(fitted["error"], Value::Null);
    assert_eq!(fitted["outcome"]["outcome"], json!("regression"));
    // The metrics are the **host's**, computed by scoring the fit back over each
    // split — so a provider from a module would be scored by this same code.
    let r2 = fitted["metrics"]["train"]["r2"].as_f64().unwrap();
    assert!(r2 > 0.999, "an exact linear relation should fit: {fitted}");
    assert!(fitted["metrics"]["test"]["r2"].as_f64().unwrap() > 0.999);
    // The split's actual counts, and what the encoding dropped.
    let rows = &fitted["rows"];
    assert_eq!(rows["selected"], json!(60));
    assert_eq!(rows["dropped"], json!(0));
    assert!(rows["train"].as_u64().unwrap() > 30, "{rows}");
    assert!(rows["test"].as_u64().unwrap() > 0, "{rows}");
    // And the parameters are the **provider's**: a coefficient table with the
    // standard errors, t and p smartcore does not give.
    let parameters = fitted["parameters"].as_array().unwrap();
    let table = parameters
        .iter()
        .find(|p| p["block"] == json!("table"))
        .unwrap_or_else(|| panic!("no coefficient table in {parameters:?}"));
    let headings: Vec<&str> = table["columns"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(headings.contains(&"p"), "{headings:?}");

    // Activate it, and the model now carries it.
    let (status, active) = server
        .client
        .send(
            "POST",
            &format!("/api/model-instances/{instance}/activate"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{active}");
    assert_eq!(active["active"], json!(true));
    let (_, model) = server
        .client
        .send("GET", &format!("/api/models/{id}"), None)
        .await;
    assert_eq!(model["instances"], json!(1));
    assert_eq!(model["active_instance"]["id"], json!(instance));

    // Predict a literal row: the "try a row" box, and a what-if about a house
    // that is not in the table at all.
    let (status, predicted) = server
        .client
        .send(
            "POST",
            "/api/model-predictions",
            Some(json!({
                "model": id,
                "rows": [{ "area": 100.0, "bedrooms": 3, "income": 90000.0 }],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{predicted}");
    assert_eq!(predicted["outcome"]["outcome"], json!("regression"));
    let value = predicted["predictions"][0]["value"].as_f64().unwrap();
    // price = 1000·area + 20000·bedrooms, exactly, so the answer is checkable.
    assert!(
        (value - 160_000.0).abs() < 1.0,
        "expected ~160000, got {value} ({predicted})"
    );

    // …and over the dataset, restricted by a filter formula: the derived
    // columns are the row layer's answer, not the caller's.
    let (status, predicted) = server
        .client
        .send(
            "POST",
            "/api/model-predictions",
            Some(json!({ "model": id, "filter": "id == 1" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{predicted}");
    let predictions = predicted["predictions"].as_array().unwrap();
    assert_eq!(predictions.len(), 1, "{predicted}");
    assert_eq!(predictions[0]["key"], json!("int:1"));
    assert!((predictions[0]["value"].as_f64().unwrap() - 91_000.0).abs() < 1.0);

    // Deleting the model takes its instances with it — the difference from an
    // agent's runs, which outlive the agent.
    let (status, deleted) = server
        .client
        .send("DELETE", &format!("/api/models/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, gone) = server
        .client
        .send("GET", &format!("/api/model-instances/{instance}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    Ok(())
}

#[tokio::test]
async fn a_fit_that_fails_leaves_the_sentence_on_the_instance() -> sc_error::Result<()> {
    let mut server = setup().await?;

    // A model that saves perfectly well — and then the world changes underneath
    // it. This is the shape a fit failure actually has: everything checkable on
    // the form was checked there (a dataset selecting nothing is refused on
    // save, as the assertion below its sibling shows), so what is left is what
    // only the job can discover.
    let (status, saved) = server
        .client
        .send("POST", "/api/models", Some(model_body("house prices")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let id = saved["id"].as_str().unwrap().to_owned();

    server
        ._db
        .client()
        .await?
        .batch_execute("DELETE FROM houses")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let (status, started) = server
        .client
        .send("POST", &format!("/api/models/{id}/fit"), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let instance = started["id"].as_str().unwrap().to_owned();

    let failed = server.client.await_fit(&instance).await;
    assert_eq!(failed["status"], json!("failed"), "{failed}");
    // The sentence is on the **row**, because the request that started the fit
    // returned long before it failed. There is nowhere else it could be.
    let error = failed["error"].as_str().unwrap_or_default();
    assert!(error.contains("selects no rows"), "{failed}");
    // A failed fit has nothing to apply, and it says so rather than answering.
    assert_eq!(failed["active"], json!(false));

    // …and it cannot be made active, which is what keeps a trigger from naming
    // a model whose active fit does not work.
    let (status, refused) = server
        .client
        .send(
            "POST",
            &format!("/api/model-instances/{instance}/activate"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // And what *can* be checked on the form is: a dataset whose filter selects
    // nothing has no column types, so the label picker's column is unknown and
    // the save is refused while the admin is still looking at it.
    let mut body = model_body("impossible");
    body["dataset"]["filter"] = json!("bedrooms > 100");
    let (status, refused) = server.client.send("POST", "/api/models", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    Ok(())
}

#[tokio::test]
async fn a_row_the_datasets_filter_excludes_is_predicted_through_the_dataset()
-> sc_error::Result<()> {
    let mut server = setup().await?;

    // A fitted, active model over `houses` — **with a filter**, which is the
    // shape of every real model of this kind: it is fitted on the houses that
    // have a price, and asked about the one that does not. A dataset's filter
    // says which rows the fit was computed from, not which rows may be
    // predicted, so a prediction reads past it.
    let mut body = model_body("house prices");
    body["dataset"]["filter"] = json!("price !== null");
    let (status, saved) = server.client.send("POST", "/api/models", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let id = saved["id"].as_str().unwrap().to_owned();
    let (_, started) = server
        .client
        .send("POST", &format!("/api/models/{id}/fit"), Some(json!({})))
        .await;
    let instance = started["id"].as_str().unwrap().to_owned();
    let fitted = server.client.await_fit(&instance).await;
    assert_eq!(fitted["status"], json!("fitted"), "{fitted}");
    server
        .client
        .send(
            "POST",
            &format!("/api/model-instances/{instance}/activate"),
            None,
        )
        .await;

    // The one model action is offered with **this table's** models as its
    // options, which is what `config_spec_for` plus the query resolution buys.
    let (status, actions) = server
        .client
        .send("GET", "/api/actions?table=houses", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{actions}");
    let names: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert!(!names.contains(&"predict_row"), "{names:?}");
    let fit = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("fit_model"))
        .unwrap_or_else(|| panic!("no fit_model among {actions}"));
    let model_field = fit["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("model"))
        .unwrap();
    assert_eq!(model_field["options"], json!(["house prices"]));

    // A new house, which the dataset's filter excludes (it has no price).
    let (status, row) = server
        .client
        .send(
            "POST",
            "/api/tables/houses/rows",
            Some(json!({
                "id": 500, "area": 100.0, "bedrooms": 3, "price": Value::Null,
                "neighbourhood": 2
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");

    // Predicted by the model's active fit, through the dataset — so the join
    // path is the row layer's answer — at 1000·area + 20000·bedrooms.
    let (status, answer) = server
        .client
        .send(
            "POST",
            "/api/model-predictions",
            Some(json!({ "model": id, "filter": "id == 500" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["instance"], json!(instance), "{answer}");
    let estimate = answer["predictions"][0]["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("no prediction: {answer}"));
    assert!(
        (estimate - 160_000.0).abs() < 1.0,
        "expected ~160000, got {estimate}"
    );
    assert!(server.catalog.get("houses")?.is_some());
    Ok(())
}

/// Percent-encode a query-parameter value.
///
/// A dataset in a query string is what `listModelProviders` takes (there is no
/// id to name an unsaved dataset by), and the test drives the URL the generated
/// client would build.
fn urlencoding(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A varying-intercept-free model of `price`, the smallest program with a
/// `data` block worth binding.
const PRICES_STAN: &str = "
data {
  int<lower=1> N;
  vector[N] y;
}
parameters {
  real mu;
  real<lower=0> sigma;
}
model {
  y ~ normal(mu, sigma);
}
";

/// The Stan provider is registered beside the built-ins whether or not this
/// machine has a CmdStan, reads its program out of a connected file store, and
/// a Stan model saves only once every `data` variable of that program is bound
/// (Stan TODO 2.5 and §10's save-time checks).
#[tokio::test]
async fn a_stan_model_saves_only_with_its_programs_data_bound() -> sc_error::Result<()> {
    let mut server = setup().await?;
    let dir = std::env::temp_dir().join(format!("sc-server-stan-models-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("prices.stan"), PRICES_STAN).unwrap();
    server
        .catalog
        .connect_file_store(Arc::new(sc_files::LocalFileStore::new("models", &dir)?))?;

    let (status, listed) = server
        .client
        .send("GET", "/api/model-providers", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let stan = listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == json!("stan"))
        .unwrap_or_else(|| panic!("no stan provider in {listed}"))
        .clone();
    assert_eq!(stan["outcome_spec"]["kind"], json!("posterior"), "{stan}");
    // The program's store is a picker over the connected stores.
    let store = stan["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("program_store"))
        .unwrap()
        .clone();
    assert!(
        store["options"]
            .as_array()
            .is_some_and(|o| o.contains(&json!("models"))),
        "{store}"
    );

    let body = |bindings: Value| {
        let mut body = model_body("price posterior");
        body["provider"] = json!("stan");
        body["configuration"] = json!({
            "program_store": "models",
            "program": "prices.stan",
            "bindings": bindings,
        });
        body
    };

    let (status, refused) = server
        .client
        .send(
            "POST",
            "/api/models",
            Some(body(json!({ "N": { "kind": "count", "dataset": "main" } }))),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("the data variable `y` (vector[N]) has no binding"),
        "{refused}"
    );

    let (status, refused) = server
        .client
        .send(
            "POST",
            "/api/models",
            Some(body(json!({
                "N": { "kind": "count", "dataset": "main" },
                "y": { "kind": "column", "dataset": "main", "column": "price" },
                "z": { "kind": "value", "value": 1 },
            }))),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("binds `z`, which the program's `data` block does not declare"),
        "{refused}"
    );

    let (status, saved) = server
        .client
        .send(
            "POST",
            "/api/models",
            Some(body(json!({
                "N": { "kind": "count", "dataset": "main" },
                "y": { "kind": "column", "dataset": "main", "column": "price" },
            }))),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(saved["error"], Value::Null, "{saved}");

    // The program is read afresh: removed from its store, the model is listed
    // with that reason and stays editable.
    std::fs::remove_file(dir.join("prices.stan")).unwrap();
    let (status, models) = server.client.send("GET", "/api/models", None).await;
    assert_eq!(status, StatusCode::OK, "{models}");
    assert!(
        models
            .to_string()
            .contains("the program `prices.stan` is not in the file store `models`"),
        "{models}"
    );
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
