//! Milestone 31 Phase 3: `predict("…")` in formulas — the save checks (3.3),
//! action formulas (3.4), non-stored calculated fields on the read path (3.5)
//! and the filter/sort refusal on them (3.6), over the built-in
//! `linear_regression`.
//!
//! The model is fitted on the **sold** houses (the dataset's filter), and
//! house 500 is not sold: a formula on it is still answered, because a
//! dataset's filter says which rows a fit was computed from, not which rows it
//! may be asked about.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, ModelHost, ModelSummary, PredictRows};
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
/// house 500 (not sold), and an `orders` table that is not the model's.
const SCHEMA: &str = "
    CREATE TABLE houses (
        id bigint primary key,
        area double precision,
        bedrooms bigint,
        price double precision,
        sold boolean not null default true,
        estimate double precision
    );
    INSERT INTO houses (id, area, bedrooms, price)
      SELECT i, 50 + i, 1 + (i % 5), 1000 * (50 + i) + 20000 * (1 + (i % 5))
      FROM generate_series(1, 60) AS i;
    INSERT INTO houses (id, area, bedrooms, price, sold)
      VALUES (500, 100, 3, NULL, false);
    CREATE TABLE orders (id bigint primary key, total double precision);
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
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    /// A request that must be refused, answering the refusal's text.
    async fn refused(&mut self, method: &str, path: &str, body: Option<Value>) -> String {
        let (status, value) = self.send(method, path, body).await;
        assert!(
            status.is_client_error(),
            "{method} {path} should be refused: {status} {value}"
        );
        value.to_string()
    }

    /// Save a model over the sold houses, answering its id.
    async fn model(&mut self, name: &str, provider: &str, configuration: Value) -> String {
        let columns: Vec<Value> = ["price", "area", "bedrooms"]
            .iter()
            .map(|c| json!({ "name": c, "expr": c }))
            .collect();
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
                        "columns": columns,
                        "filter": "sold === true",
                    },
                    "configuration": configuration,
                    "hyperparameters": {},
                    "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
                    "attributes": {},
                })),
            )
            .await;
        saved["id"].as_str().unwrap().to_owned()
    }

    /// Fit model `id` and make the fit active, answering the fit's id.
    async fn fit_and_activate(&mut self, id: &str) -> String {
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
        panic!("the fit of {id} never finished");
    }

    /// `House prices`, fitted and active.
    async fn house_prices(&mut self) -> String {
        let id = self
            .model(
                "House prices",
                "linear_regression",
                json!({ "label": "price" }),
            )
            .await;
        self.fit_and_activate(&id).await
    }

    fn trigger(
        name: &str,
        when: &str,
        channel: Option<&str>,
        only_if: Option<&str>,
        action: &str,
        configuration: Value,
    ) -> Value {
        json!({
            "name": name, "description": "", "when": when, "channel": channel,
            "only_if": only_if, "action": action, "configuration": configuration,
            "min_role": null, "enabled": true,
        })
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
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
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

/// A calculated field, as the admin UI adds one.
fn calc(name: &str, type_name: &str, expression: &str) -> Value {
    json!({
        "name": name,
        "type": type_name,
        "kind": { "type": "calc", "expression": expression },
    })
}

// --- 3.3: the save checks ---------------------------------------------------

#[tokio::test]
async fn a_calculated_field_that_predicts_is_checked_against_the_model_on_save() -> Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    client.house_prices().await;
    client
        .model(
            "Price test",
            "t_test",
            json!({ "test": "one_sample", "value": "price", "mu": 0.0 }),
        )
        .await;
    let fields = "/api/tables/houses/fields";

    // No such model.
    let err = client
        .refused(
            "POST",
            fields,
            Some(calc("guess", "float8", "predict(\"Nope\")")),
        )
        .await;
    assert!(err.contains("Nope"), "{err}");

    // A model of another table.
    let err = client
        .refused(
            "POST",
            "/api/tables/orders/fields",
            Some(calc("guess", "float8", "predict(\"House prices\")")),
        )
        .await;
    assert!(
        err.contains("`House prices` is a model of `houses`, and this formula is on `orders`"),
        "{err}"
    );

    // A model that answers nothing per row.
    let err = client
        .refused(
            "POST",
            fields,
            Some(calc("guess", "float8", "predict(\"Price test\")")),
        )
        .await;
    assert!(err.contains("`Price test` is a hypothesis test"), "{err}");

    // A field whose value is the prediction must be able to hold it.
    let err = client
        .refused(
            "POST",
            fields,
            Some(calc("guess", "text", "predict(\"House prices\")")),
        )
        .await;
    assert!(
        err.contains("the field is text and `House prices` predicts float"),
        "{err}"
    );

    // The shape of the call is the syntax check's, in the module calls' style.
    let err = client
        .refused(
            "POST",
            fields,
            Some(calc(
                "guess",
                "float8",
                "predict(\"House prices\", \"fit\")",
            )),
        )
        .await;
    assert!(err.contains("`predict` takes one argument"), "{err}");

    // What is accepted: the prediction itself as a number, and a formula that
    // computes something else from it, whose type is its own.
    client
        .ok(
            "POST",
            fields,
            Some(calc(
                "estimated_price",
                "float8",
                "predict(\"House prices\")",
            )),
        )
        .await;
    client
        .ok(
            "POST",
            fields,
            Some(calc(
                "expensive",
                "bool",
                "predict(\"House prices\") > 150000",
            )),
        )
        .await;

    // A model with no active fit is accepted, and the batch says what that
    // means for the table's reads.
    client
        .model("Unfitted", "linear_regression", json!({ "label": "price" }))
        .await;
    let applied = sc_api::schema_edit::apply(
        &catalog,
        &[sc_api::schema_edit::Operation::AddField {
            table: "houses".to_owned(),
            field: sc_api::schema_edit::FieldSpec {
                name: "later".to_owned(),
                type_name: "float8".to_owned(),
                kind: sc_catalog::DataFieldKind::Calc {
                    expression: "predict(\"Unfitted\")".to_owned(),
                },
                ..Default::default()
            },
        }],
        &sc_api::schema_edit::ApplyOptions::default(),
    )
    .await?;
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.contains("`Unfitted` has no active fit yet")
                && n.contains("every read of `houses` fails")),
        "{:?}",
        applied.notes
    );
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_may_not_predict() -> Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client.house_prices().await;
    let err = client
        .refused(
            "PUT",
            "/api/tables/houses",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": 100, "min_role_write": 1,
                "ownership_formula": "predict(\"House prices\") > 0",
                "rls_enabled": false,
            })),
        )
        .await;
    assert!(
        err.contains("an ownership formula may not call `predict(\\\"House prices\\\")`"),
        "{err}"
    );
    assert!(err.contains("fail closed"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_trigger_that_predicts_is_checked_against_the_model_on_save() -> Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client.house_prices().await;

    // An `update_rows` over a table the model is not of.
    let err = client
        .refused(
            "POST",
            "/api/triggers",
            Some(Client::trigger(
                "orders guess",
                "insert",
                Some("orders"),
                None,
                "update_rows",
                json!({
                    "table": "orders", "where": "id === row.id",
                    "assignments": { "total": "predict(\"House prices\")" },
                }),
            )),
        )
        .await;
    assert!(
        err.contains("`House prices` is a model of `houses`, and this formula is on `orders`"),
        "{err}"
    );
    assert!(err.contains("`total`"), "{err}");

    // An `insert_row` value ranges over no row, so there is nothing to predict.
    let err = client
        .refused(
            "POST",
            "/api/triggers",
            Some(Client::trigger(
                "copy guess",
                "insert",
                Some("houses"),
                None,
                "insert_row",
                json!({
                    "table": "orders",
                    "values": { "id": "row.id", "total": "predict(\"House prices\")" },
                }),
            )),
        )
        .await;
    assert!(err.contains("ranges over none"), "{err}");

    // An `only if` naming a model that does not exist.
    let err = client
        .refused(
            "POST",
            "/api/triggers",
            Some(Client::trigger(
                "cheap only",
                "insert",
                Some("houses"),
                Some("predict(\"Nope\") < 100000"),
                "update_rows",
                json!({
                    "table": "houses", "where": "id === row.id",
                    "assignments": { "estimate": "0" },
                }),
            )),
        )
        .await;
    assert!(err.contains("`only if`"), "{err}");
    assert!(err.contains("Nope"), "{err}");
    Ok(())
}

// --- 3.4: action formulas ---------------------------------------------------

#[tokio::test]
async fn an_insert_trigger_writes_the_prediction_and_a_template_renders_it() -> Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    client.house_prices().await;
    // The direct replacement for `predict_row`: an insert trigger whose
    // `update_rows` sets the estimate on the row that was inserted, guarded
    // by an `only if` that predicts too.
    client
        .ok(
            "POST",
            "/api/triggers",
            Some(Client::trigger(
                "estimate",
                "insert",
                Some("houses"),
                Some("predict(\"House prices\") > 0"),
                "update_rows",
                json!({
                    "table": "houses", "where": "id === row.id",
                    "assignments": { "estimate": "predict(\"House prices\")" },
                }),
            )),
        )
        .await;
    // A new house, not sold, so the dataset's filter excludes it.
    client
        .ok(
            "POST",
            "/api/tables/houses/rows",
            Some(json!({ "id": 600, "area": 90.0, "bedrooms": 2, "sold": false })),
        )
        .await;
    let house = client.house(600).await;
    assert!(close(&house["estimate"], price(90.0, 2.0)), "{house}");

    // A `{{ }}` template renders the prediction for the event's row.
    let event = sc_action::Event::new(sc_action::EventKind::None)
        .on("houses")
        .row(json!({ "id": 500, "area": 100.0, "bedrooms": 3 }));
    let config = sc_types::Attrs::new();
    let evaluator = default_js_evaluator();
    let ctx = sc_action::ActionContext::new(&catalog, &event, &config, "quote")
        .with_evaluator(&evaluator);
    let subject =
        sc_expr::Template::parse("House {{ id }} is worth {{ predict(\"House prices\") }}")
            .unwrap();
    let rendered =
        sc_action::render_event_template(&ctx, &subject, "`subject`", sc_expr::RenderMode::Text)
            .await?;
    let worth: f64 = rendered
        .strip_prefix("House 500 is worth ")
        .unwrap_or_else(|| panic!("{rendered}"))
        .parse()
        .unwrap();
    assert!((worth - price(100.0, 3.0)).abs() < 1.0, "{rendered}");
    Ok(())
}

// --- 3.5 and 3.6: a calculated field that predicts, on the read path ---------

/// The installed model host, with a count of the calls made through it.
pub(crate) struct Counting {
    inner: Arc<dyn ModelHost>,
    pub(crate) calls: AtomicUsize,
}

#[async_trait]
impl ModelHost for Counting {
    async fn predict(
        &self,
        model: &str,
        fit: Option<&str>,
        table: &str,
        rows: PredictRows<'_>,
        detail: bool,
    ) -> Result<Vec<Value>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.predict(model, fit, table, rows, detail).await
    }

    async fn describe(&self, model: &str) -> Result<ModelSummary> {
        self.inner.describe(model).await
    }
}

/// Count the model calls from here on.
pub(crate) fn count_calls(catalog: &Catalog) -> Result<Arc<Counting>> {
    let counting = Arc::new(Counting {
        inner: catalog.model_host().expect("the model host is installed"),
        calls: AtomicUsize::new(0),
    });
    catalog.set_model_host(Arc::clone(&counting) as Arc<dyn ModelHost>)?;
    Ok(counting)
}

#[tokio::test]
async fn listing_houses_predicts_every_row_in_one_call_and_refuses_filtering_on_it() -> Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    client.house_prices().await;
    client
        .ok(
            "POST",
            "/api/tables/houses/fields",
            Some(calc(
                "estimated_price",
                "float8",
                "predict(\"House prices\")",
            )),
        )
        .await;
    let counting = count_calls(&catalog)?;

    // Every row has its prediction — the unsold house 500 included — and the
    // page was one provider call.
    let rows = client.ok("GET", "/api/tables/houses/rows", None).await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 61);
    for row in rows {
        let (area, bedrooms) = (
            row["area"].as_f64().unwrap(),
            row["bedrooms"].as_f64().unwrap(),
        );
        assert!(
            close(&row["estimated_price"], price(area, bedrooms)),
            "{row}"
        );
    }
    assert_eq!(counting.calls.load(Ordering::SeqCst), 1);

    // Filtering and sorting on it are refused, naming the field and saying why.
    let err = client
        .refused(
            "GET",
            "/api/tables/houses/rows?estimated_price=gt.150000",
            None,
        )
        .await;
    assert!(
        err.contains(
            "cannot filter on `estimated_price`: `estimated_price` is computed after the rows \
             are read, because it calls `predict`"
        ),
        "{err}"
    );
    let err = client
        .refused(
            "GET",
            "/api/tables/houses/rows?order=estimated_price.desc",
            None,
        )
        .await;
    assert!(err.contains("cannot sort by `estimated_price`"), "{err}");

    // A code body reads it with the row, and cannot sort or filter on it.
    let trigger = client
        .ok(
            "POST",
            "/api/triggers",
            Some(Client::trigger(
                "read estimates",
                "none",
                None,
                None,
                "run_js_code",
                json!({ "code": "
                    const rows = await db.houses.where({ id: 500 }).rows();
                    const refused = [];
                    for (const attempt of [
                        () => db.houses.orderBy('estimated_price').rows(),
                        () => db.houses.where({ estimated_price: { gt: 1 } }).rows(),
                        () => db.houses.where('estimated_price > 1').rows(),
                    ]) {
                        try { await attempt(); refused.push(null); }
                        catch (e) { refused.push(String(e.message ?? e)); }
                    }
                    return { estimate: rows[0].estimated_price, refused };
                " }),
            )),
        )
        .await;
    let id = trigger["id"].as_str().unwrap();
    let ran = client
        .ok("POST", &format!("/api/triggers/{id}/run"), Some(json!({})))
        .await;
    let result = &ran["result"];
    assert!(close(&result["estimate"], price(100.0, 3.0)), "{ran}");
    for refusal in result["refused"].as_array().unwrap() {
        let refusal = refusal
            .as_str()
            .unwrap_or_else(|| panic!("not refused: {ran}"));
        assert!(
            refusal.contains("`estimated_price` is computed after the rows are read"),
            "{refusal}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_read_the_model_cannot_answer_fails_naming_the_field_the_model_and_the_row() -> Result<()>
{
    let (mut client, catalog, db) = setup().await?;
    // A style the sold houses have two of, and house 500 a third.
    db.client()
        .await?
        .batch_execute(
            "ALTER TABLE houses ADD COLUMN style text;
             UPDATE houses SET style = CASE WHEN id % 2 = 0 THEN 'modern' ELSE 'classic' END;
             UPDATE houses SET style = 'brutalist' WHERE id = 500;",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    catalog.reload().await?;
    let columns: Vec<Value> = ["price", "area", "style"]
        .iter()
        .map(|c| json!({ "name": c, "expr": c }))
        .collect();
    let saved = client
        .ok(
            "POST",
            "/api/models",
            Some(json!({
                "name": "Styled prices", "description": "", "provider": "linear_regression",
                "dataset": { "table": "houses", "columns": columns, "filter": "sold === true" },
                "configuration": { "label": "price" }, "hyperparameters": {},
                "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
                "attributes": {},
            })),
        )
        .await;
    client.fit_and_activate(saved["id"].as_str().unwrap()).await;
    client
        .ok(
            "POST",
            "/api/tables/houses/fields",
            Some(calc("styled", "float8", "predict(\"Styled prices\")")),
        )
        .await;

    let err = client.refused("GET", "/api/tables/houses/rows", None).await;
    assert!(err.contains("`styled` of `houses`"), "{err}");
    assert!(err.contains("Styled prices"), "{err}");
    assert!(err.contains("the row whose id is 500"), "{err}");
    assert!(err.contains("brutalist"), "{err}");
    Ok(())
}
