//! Milestone 31 Phase 2: the model host on the catalog (2.1) and the model
//! handle in a code body (2.6), over the built-in `linear_regression` and
//! `logistic_regression`. The posterior half — `writePosterior` from a
//! workflow step, and under a caller's authority — is in `posterior_api.rs`,
//! beside the stub sampler it needs.
//!
//! Both models are fitted on the **sold** houses (the dataset's filter) and
//! asked about house 500, which is not sold: a dataset's filter says which
//! rows a fit was computed from, not which rows it may be asked about.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, PredictRows};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// Sixty sold houses whose price is an exact linear function of two columns,
/// with a size band that is `large` above an area of 80 — and house 500, not
/// sold, which both models are asked about.
const SCHEMA: &str = "
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        bedrooms bigint,
        price double precision,
        band text,
        sold boolean not null default true,
        estimate double precision
    );
    INSERT INTO houses (id, area, bedrooms, price, band)
      SELECT i, 50 + i, 1 + (i % 5), 1000 * (50 + i) + 20000 * (1 + (i % 5)),
             CASE WHEN 50 + i > 80 THEN 'large' ELSE 'small' END
      FROM generate_series(1, 60) AS i;
    INSERT INTO houses (id, area, bedrooms, price, band, sold)
      VALUES (500, 100, 3, NULL, NULL, false);
";

struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let body = match body {
            Some(mut b) if crate::named_datasets::carries_a_model(method, path) => {
                let mut ids = Vec::new();
                for (pointer, create) in crate::named_datasets::inline_datasets(&b) {
                    let (status, made) =
                        Box::pin(self.send("POST", "/api/datasets", Some(create))).await;
                    assert!(status.is_success(), "creating a dataset: {status} {made}");
                    ids.push((pointer, made["dataset"]["id"].as_str().unwrap().to_owned()));
                }
                crate::named_datasets::use_ids(&mut b, ids);
                Some(b)
            }
            other => other,
        };
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

    /// Save a model over the sold houses, fit it and make the fit active,
    /// answering the fit's id.
    async fn active_model(
        &mut self,
        name: &str,
        provider: &str,
        columns: &[&str],
        label: &str,
    ) -> String {
        let saved = self
            .ok(
                "POST",
                "/api/models",
                Some(json!({
                    "name": name,
                    "description": "",
                    "provider": provider,
                    "dataset": {
                        "table": "houses",
                        "columns": columns
                            .iter()
                            .map(|c| json!({ "name": c, "expr": c }))
                            .collect::<Vec<_>>(),
                        "filter": "sold === true",
                    },
                    "configuration": { "label": label },
                    "hyperparameters": {},
                    "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
                    "attributes": {},
                })),
            )
            .await;
        let id = saved["id"].as_str().unwrap().to_owned();
        let started = self
            .ok("POST", &format!("/api/models/{id}/fit"), Some(json!({})))
            .await;
        let instance = started["id"].as_str().unwrap().to_owned();
        for _ in 0..800 {
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
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the fit of {name} never finished");
    }

    /// A `run_js_code` trigger, answering its id.
    async fn code(&mut self, name: &str, when: &str, channel: Option<&str>, code: &str) -> String {
        let created = self
            .ok(
                "POST",
                "/api/triggers",
                Some(json!({
                    "name": name, "description": "", "when": when, "channel": channel,
                    "only_if": null, "action": "run_js_code",
                    "configuration": { "code": code },
                    "min_role": null, "enabled": true,
                })),
            )
            .await;
        created["id"].as_str().unwrap().to_owned()
    }

    /// Run a trigger, answering its action's result.
    async fn run(&mut self, trigger: &str) -> Value {
        let ran = self
            .ok(
                "POST",
                &format!("/api/triggers/{trigger}/run"),
                Some(json!({})),
            )
            .await;
        ran["result"].clone()
    }

    async fn house(&mut self, id: i64) -> Value {
        let rows = self.ok("GET", "/api/tables/houses/rows", None).await;
        rows.as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"].as_i64() == Some(id)).cloned())
            .unwrap_or_else(|| panic!("no house {id}"))
    }
}

async fn setup() -> Result<(Client, Arc<Catalog>, TestDb)> {
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
    client
        .ok(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    Ok((client, catalog, db))
}

/// 1000·area + 20000·bedrooms, which the regression recovers exactly.
fn price(area: f64, bedrooms: f64) -> f64 {
    1000.0 * area + 20000.0 * bedrooms
}

fn close(value: &Value, expected: f64) -> bool {
    value.as_f64().is_some_and(|v| (v - expected).abs() < 1.0)
}

#[tokio::test]
async fn the_model_host_answers_keys_in_the_order_asked_and_literal_rows_by_their_features()
-> Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    client
        .active_model(
            "House prices",
            "linear_regression",
            &["price", "area", "bedrooms"],
            "price",
        )
        .await;
    client
        .active_model(
            "Size band",
            "logistic_regression",
            &["band", "area"],
            "band",
        )
        .await;
    // Installed on the catalog by `install_models`, as `serve` does.
    let host = catalog.model_host().expect("the model host is installed");

    // Keys, in the order asked — not the dataset's — including house 500,
    // which the dataset's filter excludes. House 3 is 53 m² with 4 bedrooms,
    // house 1 is 51 m² with 2.
    let keys = [json!(500), json!(3), json!(1)];
    let answer = host
        .predict(
            "House prices",
            None,
            "houses",
            PredictRows::Keys(&keys),
            false,
        )
        .await?;
    assert_eq!(answer.len(), 3, "{answer:?}");
    assert!(close(&answer[0], price(100.0, 3.0)), "{answer:?}");
    assert!(close(&answer[1], price(53.0, 4.0)), "{answer:?}");
    assert!(close(&answer[2], price(51.0, 2.0)), "{answer:?}");
    // A key given as text is the same key.
    let answer = host
        .predict(
            "House prices",
            None,
            "houses",
            PredictRows::Keys(&[json!("500")]),
            false,
        )
        .await?;
    assert!(close(&answer[0], price(100.0, 3.0)), "{answer:?}");

    // A literal row is the dataset's columns as given…
    let rows = [json!({ "area": 90.0, "bedrooms": 2 })];
    let answer = host
        .predict(
            "House prices",
            None,
            "houses",
            PredictRows::Values(&rows),
            false,
        )
        .await?;
    assert!(close(&answer[0], price(90.0, 2.0)), "{answer:?}");
    // …and one missing a feature is refused naming it.
    let err = host
        .predict(
            "House prices",
            None,
            "houses",
            PredictRows::Values(&[json!({ "area": 90.0 })]),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no value for `bedrooms`"), "{err}");

    // `detail` carries a class's probability.
    let answer = host
        .predict(
            "Size band",
            None,
            "houses",
            PredictRows::Keys(&[json!(500), json!(2)]),
            true,
        )
        .await?;
    assert_eq!(answer[0]["value"], json!("large"), "{answer:?}");
    assert_eq!(answer[1]["value"], json!("small"), "{answer:?}");
    let p = answer[0]["probability"].as_f64().expect("a probability");
    assert!(p > 0.5 && p <= 1.0, "{answer:?}");
    // A regression's detail has a value and nothing to be uncertain about.
    let answer = host
        .predict(
            "House prices",
            None,
            "houses",
            PredictRows::Keys(&[json!(1)]),
            true,
        )
        .await?;
    assert!(close(&answer[0]["value"], price(51.0, 2.0)), "{answer:?}");
    assert!(answer[0].get("probability").is_none(), "{answer:?}");

    // What is refused, by name.
    let refused = |e: sc_error::Error| e.to_string();
    let err = refused(
        host.predict(
            "House prices",
            None,
            "houses",
            PredictRows::Keys(&[json!(9999)]),
            false,
        )
        .await
        .unwrap_err(),
    );
    assert!(err.contains("no row of `houses` has the id 9999"), "{err}");
    let err = refused(
        host.predict(
            "House prices",
            None,
            "orders",
            PredictRows::Keys(&keys),
            false,
        )
        .await
        .unwrap_err(),
    );
    assert!(
        err.contains("`House prices` is a model of `houses`, and these rows are of `orders`"),
        "{err}"
    );
    let err = refused(
        host.predict("Nope", None, "houses", PredictRows::Keys(&keys), false)
            .await
            .unwrap_err(),
    );
    assert!(err.contains("no model named `Nope`"), "{err}");

    // What a save check will read.
    let described = host.describe("Size band").await?;
    assert_eq!(described.table, "houses");
    assert_eq!(described.prediction_types, vec![sc_types::BasicType::Text]);
    assert!(described.predicts());
    assert!(described.active_fit.is_some());
    Ok(())
}

#[tokio::test]
async fn a_code_trigger_predicts_the_events_row_and_writes_it_with_db() -> Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let fit = client
        .active_model(
            "House prices",
            "linear_regression",
            &["price", "area", "bedrooms"],
            "price",
        )
        .await;

    // The replacement for `predict_row`: a handle, a prediction of the event's
    // row, and an ordinary write.
    client
        .code(
            "estimate",
            "insert",
            Some("houses"),
            r#"
            const m = await models.get("House prices");
            const estimate = await m.predict(row);
            await db.houses.where({ id: row.id }).update({ estimate });
            return estimate;
            "#,
        )
        .await;
    // An unsold house, which the dataset's filter excludes.
    client
        .ok(
            "POST",
            "/api/tables/houses/rows",
            Some(json!({ "id": 501, "area": 70.0, "bedrooms": 1, "sold": false })),
        )
        .await;
    let house = client.house(501).await;
    assert!(close(&house["estimate"], price(70.0, 1.0)), "{house}");

    // The handle, from a directly-run body: what it is, a batch, a literal
    // row, and the posterior methods absent with a sentence.
    let about = client
        .code(
            "about",
            "none",
            None,
            r#"
            const m = await models.get("House prices");
            const said = {};
            try { await m.draws("alpha"); } catch (e) { said.draws = e.message; }
            try { m.variables; } catch (e) { said.variables = e.message; }
            try { await (await models.get("Nobody")); } catch (e) { said.nobody = e.message; }
            return {
                name: m.name, provider: m.provider, table: m.table, outcome: m.outcome,
                fit: m.fit.id, active: m.fit.active, status: m.fit.status,
                hasDraws: Object.keys(m).includes("draws"),
                batch: await m.predict([{ id: 500 }, { area: 90, bedrooms: 2 }, { id: 1 }]),
                said,
            };
            "#,
        )
        .await;
    let result = client.run(&about).await;
    assert_eq!(result["name"], json!("House prices"), "{result}");
    assert_eq!(result["provider"], json!("linear_regression"));
    assert_eq!(result["table"], json!("houses"));
    assert_eq!(
        result["outcome"],
        json!({ "outcome": "regression", "label": "price" })
    );
    assert_eq!(result["fit"], json!(fit));
    assert_eq!(result["active"], json!(true));
    assert_eq!(result["status"], json!("fitted"));
    assert_eq!(result["hasDraws"], json!(false));
    let batch = result["batch"].as_array().unwrap();
    assert!(close(&batch[0], price(100.0, 3.0)), "{result}");
    assert!(close(&batch[1], price(90.0, 2.0)), "{result}");
    assert!(close(&batch[2], price(51.0, 2.0)), "{result}");
    assert_eq!(
        result["said"]["draws"],
        json!("`House prices` is a linear_regression regression; `draws` is for posterior models")
    );
    assert_eq!(
        result["said"]["variables"],
        json!(
            "`House prices` is a linear_regression regression; `variables` is for posterior \
             models"
        )
    );
    assert!(
        result["said"]["nobody"]
            .as_str()
            .unwrap()
            .contains("no model named `Nobody`"),
        "{result}"
    );
    Ok(())
}
