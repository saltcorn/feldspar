//! "Get datasets" through the admin API: the catalogue listed with what can be
//! got here, an install started as a background job and polled until it has
//! made its table and dataset, a download that fails reported on the job, and
//! the 404s for a dataset there is no such thing as or that was never started.
//!
//! The files come from a fake downloader handed to `admin_handlers_with`, so
//! nothing here touches the network.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::public_datasets::Fetch;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers_with, build_router,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const PENGUINS: &str = "https://raw.githubusercontent.com/allisonhorst/palmerpenguins/8957207b78d6ccd1b4654a9dd9c9041b657478ab/inst/extdata/penguins.csv";

/// Serves a synthetic penguins file; anything else is a 404.
struct Fixtures;

#[async_trait]
impl Fetch for Fixtures {
    async fn fetch(&self, url: &str) -> sc_error::Result<Vec<u8>> {
        if url == PENGUINS {
            Ok(b"species,island,bill_length_mm,bill_depth_mm,flipper_length_mm,body_mass_g,sex,year\n\
                 Adelie,Torgersen,39.1,18.7,181,3750,male,2007\n\
                 Gentoo,Biscoe,NA,NA,NA,NA,NA,2009\n"
                .to_vec())
        } else {
            Err(sc_error::Error::msg("HTTP status 404 Not Found"))
        }
    }
}

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

    /// Poll an install until it is no longer running.
    async fn finished(&mut self, key: &str) -> Value {
        for _ in 0..200 {
            let (status, job) = self
                .send("GET", &format!("/api/public-datasets/{key}/install"), None)
                .await;
            assert_eq!(status, StatusCode::OK, "{job}");
            if job["status"] != "running" {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the install of {key} did not finish");
    }
}

async fn setup(db: &TestDb) -> sc_error::Result<Client> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_spatial(&catalog).await?;
    sc_dataset::bootstrap_datasets(&catalog).await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers_with(catalog, apps, Arc::new(Fixtures)),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
    )?;
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok(client)
}

#[tokio::test]
async fn a_public_dataset_is_got_as_a_job_and_opens_as_a_dataset() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let mut client = setup(&db).await?;

    // The catalogue, with its credit and its terms, and nothing got yet.
    let (status, listing) = client.send("GET", "/api/public-datasets", None).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let entries = listing.as_array().unwrap();
    assert!(entries.len() >= 30);
    let penguins = entries.iter().find(|e| e["key"] == "penguins").unwrap();
    assert_eq!(penguins["licence"], "CC0 1.0");
    assert_eq!(penguins["tables"], json!(["penguins"]));
    assert_eq!(penguins["installed"], false);
    assert!(penguins["job"].is_null() && penguins["dataset_id"].is_null());
    // No PostGIS in a plain test database: a spatial dataset says so.
    let countries = entries
        .iter()
        .find(|e| e["key"] == "world_countries")
        .unwrap();
    assert!(
        countries["unavailable"]
            .as_str()
            .unwrap_or_default()
            .contains("PostGIS"),
        "{countries}"
    );

    // Started, answered at once, polled to the end.
    let (status, job) = client
        .send("POST", "/api/public-datasets/penguins/install", None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["key"], "penguins");
    let job = client.finished("penguins").await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["rows"], 2);
    assert_eq!(job["dataset_name"], "Palmer penguins");
    let dataset_id = job["dataset_id"].as_str().unwrap().to_owned();

    // The dataset is there, on the table, and reads.
    let (status, datasets) = client.send("GET", "/api/datasets", None).await;
    assert_eq!(status, StatusCode::OK);
    let dataset = datasets
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == dataset_id.as_str())
        .unwrap();
    assert_eq!(dataset["table"], "penguins");
    assert!(dataset["error"].is_null(), "{dataset}");
    assert!(
        dataset["description"]
            .as_str()
            .unwrap()
            .contains("Credit: Horst AM")
    );

    // `NA` is no value.
    let (_, rows) = client
        .send("GET", "/api/tables/penguins/rows?order=id", None)
        .await;
    assert_eq!(rows[1]["species"], "Gentoo");
    assert!(rows[1]["bill_length_mm"].is_null());

    let (_, listing) = client.send("GET", "/api/public-datasets", None).await;
    let penguins = listing
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "penguins")
        .unwrap();
    assert_eq!(penguins["installed"], true);
    assert_eq!(penguins["dataset_id"], dataset_id.as_str());
    assert_eq!(penguins["job"]["status"], "succeeded");
    Ok(())
}

#[tokio::test]
async fn a_failed_download_is_reported_on_its_job_and_unknown_keys_are_404s() -> sc_error::Result<()>
{
    let db = TestDb::new().await?;
    let mut client = setup(&db).await?;

    let (status, _) = client
        .send("POST", "/api/public-datasets/iris/install", None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job = client.finished("iris").await;
    assert_eq!(job["status"], "failed");
    let error = job["error"].as_str().unwrap();
    assert!(
        error.contains("iris.data could not be downloaded") && error.contains("404"),
        "{error}"
    );
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert!(
        !tables.to_string().contains("\"iris\""),
        "a failed install leaves no table"
    );

    let (status, _) = client
        .send("POST", "/api/public-datasets/no-such-thing/install", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = client
        .send("GET", "/api/public-datasets/tips/install", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}
