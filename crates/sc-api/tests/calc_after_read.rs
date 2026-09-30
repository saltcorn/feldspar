//! Calculated fields computed **after the read** (milestone 31 §4, tasks 3.5
//! and 3.6): a field whose formula does not translate to SQL — a
//! `predict("…")`, a module function call — used to be silently absent from
//! every row, and is now evaluated over the fetched page.
//!
//! A fake [`ModelHost`] counts its calls, which is the assertion worth making
//! here: a page of rows is **one** prediction, not one per row. The real
//! models are `sc-server`'s `model_formulas.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method, rows};
use sc_auth::User;
use sc_catalog::{
    Catalog, DataFieldKind, FieldMeta, ModelHost, ModelSummary, PredictRows, bootstrap_field_meta,
    save_field_meta,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, ModuleFnHost, ModuleFunction};
use sc_test_harness::TestDb;
use sc_types::BasicType;
use serde_json::{Value as Json, json};

/// Prices a house at ten times its id — except the ones in `unseen`, whose
/// style the fit never saw.
#[derive(Default)]
struct FakeModels {
    calls: AtomicUsize,
    asked: Mutex<Vec<usize>>,
    unseen: Vec<i64>,
}

#[async_trait]
impl ModelHost for FakeModels {
    async fn predict(
        &self,
        model: &str,
        _fit: Option<&str>,
        table: &str,
        rows: PredictRows<'_>,
        _detail: bool,
    ) -> Result<Vec<Json>> {
        assert_eq!((model, table), ("House prices", "houses"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let PredictRows::Keys(keys) = rows else {
            panic!("a read predicts its rows by key");
        };
        self.asked.lock().unwrap().push(keys.len());
        keys.iter()
            .map(|k| {
                let id = k.as_i64().unwrap();
                if self.unseen.contains(&id) {
                    return Err(Error::invalid(
                        "`style` has a category the fit never saw: `brutalist`",
                    ));
                }
                Ok(json!(id * 10))
            })
            .collect()
    }

    async fn describe(&self, model: &str) -> Result<ModelSummary> {
        Ok(ModelSummary {
            name: model.to_owned(),
            table: "houses".to_owned(),
            provider: "linear_regression".to_owned(),
            prediction_types: vec![BasicType::Float],
            no_prediction: None,
            active_fit: Some("fit".to_owned()),
            not_rows_of_table: None,
        })
    }
}

/// `shout`, from `@test/text`: its argument in capitals.
#[derive(Default)]
struct Shout {
    calls: AtomicUsize,
}

#[async_trait]
impl ModuleFnHost for Shout {
    async fn call(&self, request: Json) -> Result<Json> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!(
            request["args"][0].as_str().unwrap_or("").to_uppercase()
        ))
    }

    fn functions(&self) -> Vec<ModuleFunction> {
        vec![ModuleFunction {
            module: "@test/text".to_owned(),
            name: "shout".to_owned(),
            description: "Capitals".to_owned(),
            is_async: false,
            arguments: Vec::new(),
        }]
    }
}

/// Sixty houses, a model host and a module installed, an evaluator on the
/// catalog, and four calculated fields: one SQL projects, and three computed
/// after the read.
async fn setup(db: &TestDb, models: Arc<FakeModels>) -> Result<(Arc<Catalog>, Arc<Shout>)> {
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE houses (id bigint primary key, area bigint, title text);
             INSERT INTO houses SELECT i, 50 + i, 'house ' || i FROM generate_series(1, 60) i;",
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    bootstrap_field_meta(&catalog).await?;
    let shout = Arc::new(Shout::default());
    catalog.set_module_functions(Arc::clone(&shout) as Arc<dyn ModuleFnHost>)?;
    catalog.set_model_host(models as Arc<dyn ModelHost>)?;
    catalog.set_formula_evaluator(Arc::new(DenoEvaluator::new()))?;
    catalog.reload().await?;
    for (name, expression) in [
        ("double_area", "area * 2"),
        // Declared before the field it reads, so the order is the plan's.
        ("premium", "estimated_price * 2"),
        ("estimated_price", "predict(\"House prices\")"),
        ("loud", "shout(title)"),
    ] {
        let meta = FieldMeta::new("houses", name).kind(DataFieldKind::Calc {
            expression: expression.into(),
        });
        save_field_meta(&catalog, &meta).await?;
    }
    assert!(
        catalog.field_overlay_issues()?.is_empty(),
        "{:?}",
        catalog.field_overlay_issues()?
    );
    Ok((catalog, shout))
}

#[tokio::test]
async fn a_page_is_one_prediction_and_every_field_is_computed() -> Result<()> {
    let db = TestDb::new().await?;
    let models = Arc::new(FakeModels::default());
    let (catalog, shout) = setup(&db, Arc::clone(&models)).await?;
    let houses = catalog.require("houses")?;

    let query = rows::RowQuery::new().limit(50);
    let page = rows::list_rows_query(&catalog, &houses, &query, None).await?;
    let page = page.as_array().unwrap();
    assert_eq!(page.len(), 50);
    // One provider call for the page, with all fifty keys in it.
    assert_eq!(models.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*models.asked.lock().unwrap(), vec![50]);
    for row in page {
        let id = row["id"].as_f64().unwrap();
        assert_eq!(row["estimated_price"].as_f64(), Some(id * 10.0), "{row}");
        // A field reading the predicting field sees its value.
        assert_eq!(row["premium"].as_f64(), Some(id * 20.0), "{row}");
        // A module-function field, which the read used to skip.
        assert_eq!(row["loud"], json!(format!("HOUSE {id}")), "{row}");
        // And the one SQL computes is still SQL's.
        assert_eq!(
            row["double_area"].as_f64(),
            Some((50.0 + id) * 2.0),
            "{row}"
        );
    }
    // Module calls are per row, as on the write path.
    assert_eq!(shout.calls.load(Ordering::SeqCst), 50);

    // The typed read — what GraphQL and a code body use — is completed the
    // same way.
    let values = rows::list_row_values(&catalog, &houses, &query, None).await?;
    assert_eq!(
        values[0]
            .get("estimated_price")
            .map(sc_types::value_to_json),
        Some(json!(10))
    );
    assert_eq!(models.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn a_prediction_that_fails_fails_the_read_naming_the_field_the_model_and_the_row()
-> Result<()> {
    let db = TestDb::new().await?;
    let models = Arc::new(FakeModels {
        unseen: vec![13],
        ..FakeModels::default()
    });
    let (catalog, _) = setup(&db, Arc::clone(&models)).await?;
    let houses = catalog.require("houses")?;

    let err = rows::list_rows(&catalog, &houses)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`estimated_price` of `houses`"), "{err}");
    assert!(err.contains("predict(\"House prices\")"), "{err}");
    assert!(err.contains("the row whose id is 13"), "{err}");
    assert!(err.contains("never saw: `brutalist`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_field_computed_after_the_read_cannot_be_filtered_or_sorted_on() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, _) = setup(&db, Arc::new(FakeModels::default())).await?;
    let houses = catalog.require("houses")?;

    // REST's query string.
    let pairs = |k: &str, v: &str| vec![(k.to_owned(), v.to_owned())];
    let query = sc_api::query_string::row_query(&houses, &pairs("estimated_price", "gt.100"), 50)?;
    let err = rows::list_rows_query(&catalog, &houses, &query, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "cannot filter on `estimated_price`: `estimated_price` is computed after the rows \
             are read, because it calls `predict`"
        ),
        "{err}"
    );
    let query = sc_api::query_string::row_query(&houses, &pairs("order", "premium.desc"), 50)?;
    let err = rows::list_rows_query(&catalog, &houses, &query, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot sort by `premium`")
            && err.contains("it reads `estimated_price`, which is computed after the read too"),
        "{err}"
    );
    // A count over the same filter says the same, so a grid's count and its
    // rows cannot disagree about why.
    let filter = sc_api::query_string::filter_predicate(&houses, &pairs("loud", "eq.X"))?;
    let err = rows::count_rows_where(&catalog, &houses, filter, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("because it calls the module function `shout`"),
        "{err}"
    );
    // The field SQL computes is still fine to filter and sort on.
    let query = sc_api::query_string::row_query(&houses, &pairs("order", "double_area.desc"), 1)?;
    let top = rows::list_rows_query(&catalog, &houses, &query, None).await?;
    assert_eq!(top[0]["id"], json!(60));

    // GraphQL's `where` and `order_by`.
    let api = GraphqlProvider::project("/graphql", std::slice::from_ref(&houses))?;
    let user = User::new(uuid::Uuid::new_v4(), sc_auth::ROLE_ADMIN)?;
    for document in [
        "{ houses(where: { estimated_price: { gt: \"100\" } }) { id } }",
        "{ houses(order_by: { estimated_price: desc }) { id } }",
    ] {
        let resp = api
            .handle(
                ApiRequest::new(Method::Post, "/graphql").body(json!({ "query": document })),
                &catalog,
                Some(&user),
            )
            .await?;
        let body = resp.body.to_string();
        assert!(
            body.contains("`estimated_price` is computed after the rows are read"),
            "{document}: {body}"
        );
    }
    // And selecting it is fine.
    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": "{ houses(limit: 2) { id estimated_price } }" })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(
        resp.body["data"]["houses"],
        json!([{ "id": 1, "estimated_price": 10 }, { "id": 2, "estimated_price": 20 }]),
        "{}",
        resp.body
    );
    Ok(())
}
