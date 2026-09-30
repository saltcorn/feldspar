//! Milestone 31 task 1.2: `fit_model`, generic across providers — fired from a
//! trigger over a `linear_regression` and over a stub provider that warns, with
//! each of `activate`'s three settings, waiting and not.
//!
//! "Clean" means the same thing for every provider: fitted, and nothing warned.
//! The stub is what makes that observable without a posterior — its fit is
//! fine and it says so with a warning, the way a Python provider passes on
//! scikit-learn's `ConvergenceWarning`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_model::{FitResult, Frame, ModelProvider, OutcomeSpec, Prediction};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, FormField};
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// Sixty houses whose price is an exact linear function of two columns, so a
/// linear regression's held-out R² is 1.
const SCHEMA: &str = "
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        bedrooms bigint,
        price double precision
    );
    INSERT INTO houses (id, area, bedrooms, price)
      SELECT i, 50 + i, 1 + (i % 5), 1000 * (50 + i) + 20000 * (1 + (i % 5))
      FROM generate_series(1, 60) AS i;
";

/// What the stub says about every fit it makes.
pub(crate) const WARNING: &str = "the solver stopped before it converged: raise `max_iter`";

/// A regression that always predicts 1 and always warns.
pub(crate) struct Warns;

#[async_trait::async_trait]
impl ModelProvider for Warns {
    fn name(&self) -> &str {
        "warns"
    }
    fn description(&self) -> &str {
        "a regression that warns about every fit"
    }
    fn config_declaration(&self) -> Vec<FormField> {
        vec![sc_model::numeric_column_field("label", "Label")]
    }
    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Regression {
            label: "label".to_owned(),
        }
    }
    async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
        Ok(FitResult::new(json!({ "value": 1.0 })).warning(WARNING))
    }
    async fn predict(&self, _s: &Value, frame: &Frame) -> Result<Vec<Prediction>> {
        Ok(vec![Prediction::number(1.0); frame.rows])
    }
}

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

    /// Save a model, answering its id.
    async fn model(&mut self, name: &str, provider: &str) -> String {
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
                        "columns": [
                            { "name": "price", "expr": "price" },
                            { "name": "area", "expr": "area" },
                            { "name": "bedrooms", "expr": "bedrooms" },
                        ],
                    },
                    "configuration": { "label": "price" },
                    "hyperparameters": {},
                    "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
                    "attributes": {},
                })),
            )
            .await;
        saved["id"].as_str().unwrap().to_owned()
    }

    /// A `fit_model` trigger with this configuration, answering its id.
    async fn refit(&mut self, name: &str, configuration: Value) -> String {
        let created = self
            .ok(
                "POST",
                "/api/triggers",
                Some(json!({
                    "name": name, "description": "", "when": "none", "channel": null,
                    "only_if": null, "action": "fit_model", "configuration": configuration,
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

    /// Poll one instance until `done` says it is.
    async fn instance_until(&mut self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..800 {
            let body = self
                .ok("GET", &format!("/api/model-instances/{id}"), None)
                .await;
            if done(&body) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("instance {id} never got there");
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
    // The stub joins the registry before the action is built over it, as a
    // module's provider does on a reload.
    let mut registry = models.base_registry()?;
    registry.register(Arc::new(Warns))?;
    models.set_registry(Arc::new(registry));
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

#[tokio::test]
async fn fit_model_refits_a_regression_and_answers_its_metrics() -> Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client.model("House prices", "linear_regression").await;

    // The description no longer speaks of posteriors, and `activate` is the
    // three words.
    let actions = client.ok("GET", "/api/actions", None).await;
    let fit = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("fit_model"))
        .unwrap();
    assert_eq!(
        fit["description"],
        json!("Fit a model again, and optionally make the new fit active")
    );
    let activate = fit["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("activate"))
        .unwrap();
    assert_eq!(
        activate["options"],
        json!(["never", "if_clean", "always"]),
        "{activate}"
    );

    let nightly = client
        .refit(
            "nightly",
            json!({ "model": "House prices", "activate": "if_clean" }),
        )
        .await;
    let result = client.run(&nightly).await;
    assert_eq!(result["status"], json!("fitted"), "{result}");
    assert_eq!(result["active"], json!(true), "{result}");
    assert_eq!(result["warnings"], json!([]), "{result}");
    assert_eq!(result["error"], Value::Null, "{result}");
    // `metrics` is in the answer, so a later workflow step can branch on it.
    let r2 = result["metrics"]["test"]["r2"].as_f64().unwrap();
    assert!((r2 - 1.0).abs() < 1e-6, "{result}");

    // `never` (the default) keeps the new fit beside the active one.
    let kept = client
        .refit("keep", json!({ "model": "House prices" }))
        .await;
    let second = client.run(&kept).await;
    assert_eq!(second["status"], json!("fitted"), "{second}");
    assert_eq!(second["active"], json!(false), "{second}");
    let first = client
        .ok(
            "GET",
            &format!(
                "/api/model-instances/{}",
                result["instance"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(first["active"], json!(true), "{first}");

    // A setting that is not one of the three is refused on save.
    let (status, refused) = client
        .send(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "wrong", "description": "", "when": "none", "channel": null,
                "only_if": null, "action": "fit_model",
                "configuration": { "model": "House prices", "activate": "sometimes" },
                "min_role": null, "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let refused = refused.to_string();
    assert!(
        refused.contains("`activate`") && refused.contains("if_clean"),
        "{refused}"
    );
    Ok(())
}

#[tokio::test]
async fn a_fit_that_warns_is_not_clean_and_always_activates_it_anyway() -> Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client.model("Warned", "warns").await;

    let if_clean = client
        .refit(
            "if_clean",
            json!({ "model": "Warned", "activate": "if_clean" }),
        )
        .await;
    let result = client.run(&if_clean).await;
    assert_eq!(result["status"], json!("fitted"), "{result}");
    assert_eq!(result["active"], json!(false), "{result}");
    assert_eq!(result["warnings"], json!([WARNING]), "{result}");
    // The instance carries the sentence where a posterior's diagnostics go.
    let instance = client
        .ok(
            "GET",
            &format!(
                "/api/model-instances/{}",
                result["instance"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(instance["warnings"], json!([WARNING]), "{instance}");

    let always = client
        .refit("always", json!({ "model": "Warned", "activate": "always" }))
        .await;
    let result = client.run(&always).await;
    assert_eq!(result["status"], json!("fitted"), "{result}");
    assert_eq!(result["active"], json!(true), "{result}");
    assert_eq!(result["warnings"], json!([WARNING]), "{result}");
    Ok(())
}

#[tokio::test]
async fn not_waiting_the_job_applies_if_clean_when_the_fit_finishes() -> Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client.model("House prices", "linear_regression").await;
    client.model("Warned", "warns").await;

    // Answered at once, and activated by the job once it is fitted.
    let background = client
        .refit(
            "background",
            json!({ "model": "House prices", "activate": "if_clean", "wait": false }),
        )
        .await;
    let result = client.run(&background).await;
    assert_eq!(result["status"], json!("fitting"), "{result}");
    assert_eq!(result["active"], json!(false), "{result}");
    let id = result["instance"].as_str().unwrap().to_owned();
    let finished = client
        .instance_until(&id, |i| i["active"] == json!(true))
        .await;
    assert_eq!(finished["status"], json!("fitted"), "{finished}");

    // And one that warns stays inactive, the job reading the same rule.
    let background = client
        .refit(
            "background_warned",
            json!({ "model": "Warned", "activate": "if_clean", "wait": false }),
        )
        .await;
    let result = client.run(&background).await;
    let id = result["instance"].as_str().unwrap().to_owned();
    let finished = client
        .instance_until(&id, |i| i["status"] != json!("fitting"))
        .await;
    assert_eq!(finished["status"], json!("fitted"), "{finished}");
    // The job's activation is a second write after the fit's own; give it the
    // moment it needs before asserting it did not happen.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let finished = client
        .ok("GET", &format!("/api/model-instances/{id}"), None)
        .await;
    assert_eq!(finished["active"], json!(false), "{finished}");
    assert_eq!(finished["warnings"], json!([WARNING]), "{finished}");
    Ok(())
}
