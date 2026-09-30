//! A formula's `predict("…")`, hoisted (milestone 31 §4, task 3.2).
//!
//! The formula evaluator does no I/O, so a prediction is resolved by
//! [`prefetch_bindings`] before the formula runs, through the catalog's
//! [`ModelHost`], and bound under the call's own key — the module calls'
//! arrangement. These tests hold a real catalog and a fake host, and assert
//! what the catalog hands the host: the row's **key** when it has one, its
//! **values** when it does not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_catalog::{
    Catalog, DataField, ModelHost, ModelSummary, PredictRows, TableMeta, bootstrap_table_meta,
    prefetch_bindings, save_table_meta,
};
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{Formula, SchemaShape, TableShape, value_to_json};
use sc_query::Value;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

/// What the fake was asked: the model, the table, and the rows as keys or
/// values.
#[derive(Debug, Clone, PartialEq)]
enum Asked {
    Keys(String, String, Vec<Json>),
    Values(String, String, Vec<Json>),
}

/// A model host that prices a house at 1000 per square metre, and records
/// what it was asked.
#[derive(Default)]
struct FakeModels {
    asked: Mutex<Vec<Asked>>,
    /// Fail every prediction with this sentence.
    fail: Option<&'static str>,
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
        let asked = match rows {
            PredictRows::Keys(keys) => Asked::Keys(model.into(), table.into(), keys.to_vec()),
            PredictRows::Values(values) => {
                Asked::Values(model.into(), table.into(), values.to_vec())
            }
        };
        self.asked.lock().unwrap().push(asked);
        if let Some(fail) = self.fail {
            return Err(Error::invalid(fail));
        }
        Ok(match rows {
            PredictRows::Keys(keys) => keys
                .iter()
                .map(|k| json!(k.as_i64().unwrap() * 10))
                .collect(),
            PredictRows::Values(values) => values
                .iter()
                .map(|v| {
                    v["area"]
                        .as_f64()
                        .map(|a| json!(a * 1000.0))
                        .ok_or_else(|| Error::invalid("the row has no `area`, a feature"))
                })
                .collect::<Result<_>>()?,
        })
    }

    async fn describe(&self, model: &str) -> Result<ModelSummary> {
        Ok(ModelSummary {
            name: model.to_owned(),
            table: "house".to_owned(),
            provider: "linear_regression".to_owned(),
            prediction_types: vec![BasicType::Float],
            no_prediction: None,
            active_fit: Some("fit-1".to_owned()),
            not_rows_of_table: None,
        })
    }
}

async fn catalog_with_houses(db: &TestDb) -> Result<(Catalog, sc_catalog::Table)> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver).await?;
    catalog
        .create_table(
            "house",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key(),
                DataField::plain("area", TypeRef::Basic(BasicType::Float)),
            ],
        )
        .await?;
    let table = catalog.require("house")?;
    Ok((catalog, table))
}

fn shape() -> SchemaShape {
    SchemaShape::new().table(
        "house",
        TableShape::new()
            .field("id")
            .field("area")
            .primary_key("id"),
    )
}

const KEY: &str = "predict(\"House prices\")";

#[tokio::test]
async fn a_prediction_is_asked_by_key_when_the_row_has_one_and_by_values_when_not() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, table) = catalog_with_houses(&db).await?;
    let host = Arc::new(FakeModels::default());
    catalog.set_model_host(Arc::clone(&host) as Arc<dyn ModelHost>)?;

    let analysis = Formula::parse("predict(\"House prices\") + 1")
        .unwrap()
        .validate(&shape(), "house")
        .unwrap();
    assert_eq!(analysis.model_calls.first().unwrap().key, KEY);

    // A row with its key: read through the model's dataset, by key.
    let mut values = BTreeMap::from([
        ("id".to_owned(), Value::Int(7)),
        ("area".to_owned(), Value::Float(80.0)),
    ]);
    prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values).await?;
    assert_eq!(values.get(KEY).map(value_to_json), Some(json!(70)));

    // A proposed row, not inserted yet: its values are the dataset's columns.
    let mut values = BTreeMap::from([("area".to_owned(), Value::Float(80.0))]);
    prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values).await?;
    assert_eq!(values.get(KEY).map(value_to_json), Some(json!(80000.0)));

    let asked = host.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec![
            Asked::Keys("House prices".into(), "house".into(), vec![json!(7)]),
            Asked::Values(
                "House prices".into(),
                "house".into(),
                vec![json!({ "area": 80.0 })]
            ),
        ]
    );

    // A value the caller already bound (the read path's batch) is not asked
    // again.
    let mut values = BTreeMap::from([
        ("id".to_owned(), Value::Int(7)),
        (KEY.to_owned(), Value::Float(1.0)),
    ]);
    prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values).await?;
    assert_eq!(host.asked.lock().unwrap().len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_prediction_with_no_host_or_a_failing_one_fails_naming_the_call() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, table) = catalog_with_houses(&db).await?;
    let analysis = Formula::parse("predict(\"House prices\")")
        .unwrap()
        .validate(&shape(), "house")
        .unwrap();

    // No model support at all: an error naming the call, never a null.
    let mut values = BTreeMap::from([("id".to_owned(), Value::Int(7))]);
    let err = prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(KEY), "{err}");
    assert!(err.contains("no model support"), "{err}");
    assert!(!values.contains_key(KEY));

    // A host that cannot answer: its sentence, under the call's name.
    catalog.set_model_host(Arc::new(FakeModels {
        fail: Some("model `House prices` has no active fit"),
        ..FakeModels::default()
    }))?;
    let err = prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(KEY), "{err}");
    assert!(err.contains("has no active fit"), "{err}");

    // A literal row missing a feature is refused by the host, by name.
    catalog.set_model_host(Arc::new(FakeModels::default()))?;
    let mut values = BTreeMap::new();
    let err = prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`area`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_that_predicts_grants_nothing() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, _) = catalog_with_houses(&db).await?;
    bootstrap_table_meta(&catalog).await?;
    let mut meta = TableMeta::new("house");
    meta.set_ownership_formula(Some("predict(\"House prices\") > 0"));
    save_table_meta(&catalog, &meta).await?;

    let table = catalog.require("house")?;
    assert!(table.ownership.is_none(), "the rule must grant nothing");
    let reason = table.ownership_error.clone().unwrap_or_default();
    assert!(reason.contains(KEY), "{reason}");
    assert!(reason.contains("fail closed"), "{reason}");
    Ok(())
}
