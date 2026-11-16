//! TODO-bayesian-regression R1.8: a `bayesian_regression` model through the
//! assembled router, without CmdStan — the provider listed, the formula
//! checked as the editor checks it (its mistakes located, pasted priors moved
//! into rows, the program and the derived bindings), the prior table with its
//! data-dependent numbers, the data preview of the lowered bindings, and a
//! save that validates the formula against the dataset.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_error::Result;
use sc_server::{AppMounts, ServerConfig, StanSettings, admin_handlers, build_router_with_apps};
use sc_test_harness::TestServer;
use serde_json::{Value, json};
use tower::ServiceExt;

const SCHEMA: &str = "
    CREATE TABLE homes (
        id bigint primary key, county text, floor double precision, log_radon double precision
    );
    INSERT INTO homes VALUES
        (1, 'Aitkin', 0, 0.8), (2, 'Aitkin', 1, 1.2), (3, 'Anoka', 0, 1.9),
        (4, 'Anoka', null, 2.1), (5, 'Becker', 1, 2.8), (6, 'Becker', 0, 3.3),
        (7, 'Becker', 0, 9.3), (8, 'Becker', 1, 4.0);
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
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }
}

async fn setup() -> Result<(Client, sc_test_harness::TestDb)> {
    let TestServer { db, catalog, .. } = TestServer::builder().sql(SCHEMA).build().await?;
    sc_auth::bootstrap(&catalog).await?;
    let models = sc_server::install_models_with(
        &catalog,
        sc_model::DEFAULT_MAX_ROWS,
        &StanSettings {
            // Not looked for: everything here is without CmdStan.
            cmdstan: Some("/nonexistent/cmdstan-2.40.0".into()),
            ..StanSettings::default()
        },
    )
    .await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_models(models));
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

fn radon(formula: &str, priors: Value) -> Value {
    json!({
        "name": "Radon",
        "description": "",
        "provider": "bayesian_regression",
        "dataset": {
            "table": "homes",
            "columns": [
                { "name": "county", "expr": "county" },
                { "name": "floor", "expr": "floor" },
                { "name": "log_radon", "expr": "log_radon" },
            ],
        },
        "configuration": { "formula": formula, "family": "gaussian", "priors": priors },
        "hyperparameters": {},
        "attributes": {},
    })
}

#[tokio::test]
async fn a_formula_model_is_checked_previewed_and_saved_through_the_api() -> Result<()> {
    let (mut client, _db) = setup().await?;

    // Listed beside `stan`, saying CmdStan is missing.
    let providers = client.ok("GET", "/api/model-providers", None).await;
    let ours = providers["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "bayesian_regression")
        .expect("the provider is registered");
    assert_eq!(ours["binds_data"], true);
    assert!(
        ours["unavailable"]
            .as_str()
            .unwrap()
            .contains("CmdStan was not found")
    );

    // A typo: located for the underline, with the near column named.
    let check = client
        .ok(
            "POST",
            "/api/model-formula/check",
            Some(radon("log_radon ~ flor + (1 | county)", json!([]))),
        )
        .await;
    let error = &check["errors"][0];
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("did you mean `floor`?"),
        "{check}"
    );
    assert_eq!(
        (error["line"].as_u64(), error["column"].as_u64()),
        (Some(1), Some(13))
    );
    assert_eq!(error["end_column"], 17);
    assert!(
        check["completions"]["columns"]
            .as_array()
            .unwrap()
            .contains(&json!("floor"))
    );

    // A pasted prior moves into the rows; the program and the bindings come back.
    let check = client
        .ok(
            "POST",
            "/api/model-formula/check",
            Some(radon(
                "log_radon ~ floor + (1 | county)\nprior(normal(0, 1), class = b)",
                json!([{"class": "b", "prior": "normal(0, 5)"}]),
            )),
        )
        .await;
    assert_eq!(check["errors"], json!([]), "{check}");
    assert_eq!(
        check["formula"].as_str().unwrap().trim(),
        "log_radon ~ floor + (1 | county)"
    );
    assert_eq!(
        check["priors"],
        json!([{"class": "b", "prior": "normal(0, 1)"}])
    );
    assert_eq!(check["replaced"][0]["prior"], "normal(0, 5)");
    assert!(
        check["program"]
            .as_str()
            .unwrap()
            .contains("cholesky_factor_corr")
    );
    assert_eq!(check["data"]["bindings"]["J_1"]["dimension"], "county");
    assert_eq!(check["from_data"], false);
    let intercept = check["table"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["class"] == "Intercept")
        .unwrap()
        .clone();
    assert_eq!(
        intercept["rule"],
        "student_t(3, median, scale) of `log_radon`"
    );

    // After the data: numbers, over the seven homes a fit keeps (the one with
    // no `floor` is dropped, or the median would be 2.45): the median reading
    // is 2.8, and the MAD (1.8) under the floor of 2.5.
    let priors = client
        .ok(
            "POST",
            "/api/model-formula/priors",
            Some(radon("log_radon ~ floor + (1 | county)", json!([]))),
        )
        .await;
    assert_eq!(priors["from_data"], true);
    let intercept = priors["table"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["class"] == "Intercept")
        .unwrap()
        .clone();
    assert_eq!(intercept["prior"], "student_t(3, 2.8, 2.5)", "{priors}");

    // The preview binds the lowered bindings, the null `floor` dropped.
    let preview = client
        .ok(
            "POST",
            "/api/model-data/preview",
            Some(radon("log_radon ~ floor + (1 | county)", json!([]))),
        )
        .await;
    assert_eq!(preview["errors"], json!([]), "{preview}");
    let n = preview["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "N")
        .unwrap()
        .clone();
    assert!(n.to_string().contains('7'), "{n}");

    // A save validates the formula against the dataset.
    let (status, refused) = client
        .send(
            "POST",
            "/api/models",
            Some(radon("log_radon ~ flor", json!([]))),
        )
        .await;
    assert!(!status.is_success(), "{refused}");
    assert!(
        refused.to_string().contains("did you mean `floor`?"),
        "{refused}"
    );
    client
        .ok(
            "POST",
            "/api/models",
            Some(radon("log_radon ~ floor + (1 | county)", json!([]))),
        )
        .await;
    Ok(())
}
