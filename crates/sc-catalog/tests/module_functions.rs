//! A formula's **module function call**, hoisted (TODO "Modules in-process",
//! §4b).
//!
//! The formula evaluator does no I/O, so a call to `md_to_html(notes)` cannot
//! happen inside it — the same problem a Ⱶ-join has, and the same answer:
//! collected at parse time, resolved by [`prefetch_bindings`] before the formula
//! runs, and bound into the row as an ordinary scope entry. This is the
//! `sc-catalog` half of that, against a real catalog and a fake module host.
//!
//! What a fake host buys is the assertion actually worth making here: that the
//! *plan* the catalog sends is the one a module can answer, and that its result
//! lands under the key the evaluator will look for. The module worker's own half
//! is `sc-module`'s `deno_host` suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_catalog::{
    Catalog, DataField, TableMeta, bootstrap_table_meta, prefetch_bindings, save_table_meta,
};
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{Formula, ModuleFnHost, ModuleFunction, SchemaShape, TableShape};
use sc_query::Value;
use sc_test_harness::TestDb;
use sc_types::value_to_json;
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;

/// A module host that records the plans it was asked for and renders markdown
/// the way `@saltcorn/markdown` does — badly, but deterministically.
struct FakeMarkdown {
    asked: Mutex<Vec<Json>>,
    /// What to answer with, or `None` to fail as a module that is not loaded.
    loaded: bool,
}

#[async_trait]
impl ModuleFnHost for FakeMarkdown {
    async fn call(&self, request: Json) -> Result<Json> {
        self.asked.lock().unwrap().push(request.clone());
        if !self.loaded {
            return Err(Error::invalid(
                "the module @saltcorn/markdown is not loaded in this host",
            ));
        }
        let text = request["args"][0].as_str().unwrap_or("");
        Ok(Json::String(format!("<p>{text}</p>")))
    }

    fn functions(&self) -> Vec<ModuleFunction> {
        vec![ModuleFunction {
            module: "@saltcorn/markdown".to_owned(),
            name: "md_to_html".to_owned(),
            description: "Turn markdown into HTML".to_owned(),
            is_async: false,
            arguments: Vec::new(),
        }]
    }
}

/// A catalog over a throwaway database with one `notes` table in it.
async fn catalog_with_notes(db: &TestDb) -> Result<(Catalog, sc_catalog::Table)> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver).await?;
    catalog
        .create_table(
            "article",
            &[DataField::plain("notes", TypeRef::Basic(BasicType::Text))],
        )
        .await?;
    let table = catalog.require("article")?;
    Ok((catalog, table))
}

/// The shape a formula on `article` is validated against, with the module
/// installed.
fn shape() -> SchemaShape {
    SchemaShape::new()
        .table(
            "article",
            TableShape::new()
                .field("id")
                .field("notes")
                .primary_key("id"),
        )
        .module_function("md_to_html", "@saltcorn/markdown")
}

#[tokio::test]
async fn a_calc_fields_module_call_is_resolved_before_the_formula_runs() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, table) = catalog_with_notes(&db).await?;
    let host = Arc::new(FakeMarkdown {
        asked: Mutex::new(Vec::new()),
        loaded: true,
    });
    catalog.set_module_functions(Arc::clone(&host) as Arc<dyn ModuleFnHost>)?;

    let formula = Formula::parse("md_to_html(notes)").unwrap();
    let analysis = formula.validate(&shape(), "article").unwrap();
    let call = analysis.module_calls.first().unwrap();
    assert_eq!(call.module, "@saltcorn/markdown");
    assert_eq!(call.key, "md_to_html(notes)");

    let mut values: BTreeMap<String, Value> =
        BTreeMap::from([("notes".to_owned(), Value::Text("hello".to_owned()))]);
    prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values).await?;

    // The result is in the row, under the key the evaluator computes from the
    // same AST node — which is the whole of the hand-off.
    assert_eq!(
        values.get("md_to_html(notes)").map(value_to_json),
        Some(Json::String("<p>hello</p>".to_owned()))
    );

    // And the plan the module was handed names the module, the function and
    // v1's positional arguments, with a clock on it.
    let asked = host.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        asked[0]["module"],
        Json::String("@saltcorn/markdown".into())
    );
    assert_eq!(asked[0]["function"], Json::String("md_to_html".into()));
    assert_eq!(asked[0]["args"], serde_json::json!(["hello"]));
    assert!(asked[0]["timeout_ms"].as_u64().unwrap() > 0);
    Ok(())
}

#[tokio::test]
async fn a_hoisted_call_whose_module_is_not_loaded_fails_the_formula() -> Result<()> {
    let db = TestDb::new().await?;
    let (catalog, table) = catalog_with_notes(&db).await?;

    let formula = Formula::parse("md_to_html(notes)").unwrap();
    let analysis = formula.validate(&shape(), "article").unwrap();
    let mut values: BTreeMap<String, Value> =
        BTreeMap::from([("notes".to_owned(), Value::Text("hello".to_owned()))]);

    // No host at all: a server whose module support never started. The formula
    // is **not evaluated**, rather than evaluated against a null — a formula
    // that computed a different answer because a module went away is the silent
    // failure this whole system refuses.
    let err = prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("md_to_html"), "{err}");
    assert!(!values.contains_key("md_to_html(notes)"));

    // And a host that has the function but cannot answer fails the same way,
    // carrying the module's own words.
    catalog.set_module_functions(Arc::new(FakeMarkdown {
        asked: Mutex::new(Vec::new()),
        loaded: false,
    }))?;
    let err = prefetch_bindings(&catalog, &table, &analysis, &shape(), &mut values)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not loaded"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_that_calls_a_module_grants_nothing() -> Result<()> {
    // The catalog's own reload is the second place this is refused (the first is
    // the schema editor, on save): a module can be *installed* after a formula
    // was stored, and the same source that validated yesterday would start
    // calling a module today.
    let db = TestDb::new().await?;
    let (catalog, _) = catalog_with_notes(&db).await?;
    catalog.set_module_functions(Arc::new(FakeMarkdown {
        asked: Mutex::new(Vec::new()),
        loaded: true,
    }))?;

    bootstrap_table_meta(&catalog).await?;
    let mut meta = TableMeta::new("article");
    meta.set_ownership_formula(Some("md_to_html(notes) !== ''"));
    save_table_meta(&catalog, &meta).await?;

    let table = catalog.require("article")?;
    assert!(table.ownership.is_none(), "the rule must grant nothing");
    let reason = table.ownership_error.clone().unwrap_or_default();
    assert!(reason.contains("md_to_html"), "{reason}");
    assert!(reason.contains("fail closed"), "{reason}");
    Ok(())
}
