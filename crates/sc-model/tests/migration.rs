//! `TABLES_RENAME.sql` sections 7 and 8 (analytics TODO A1.10): models stored
//! with their dataset written on them become models of named datasets, running
//! it twice is running it once, and the models then fit.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_model::{
    CompiledSource, FitStatus, ModelInstance, bootstrap_model_instances, bootstrap_models,
    builtin_registry, fit_model, list_models, save_model_instance,
};
use sc_query::{Expr, Insert, Select, Source, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

/// The migration file, from the repository root.
const SQL: &str = include_str!("../../../TABLES_RENAME.sql");

/// The text of section `n`: from its heading to the next one's.
fn section(n: u32) -> String {
    let start = SQL
        .find(&format!("\n-- {n}. "))
        .unwrap_or_else(|| panic!("no section {n}"));
    let rest = &SQL[start..];
    let end = rest
        .find(&format!("\n-- {}. ", n + 1))
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// Section 8's statements: SQLite's half is commented out so the file runs
/// on Postgres as it stands, indented under `--   `.
fn sqlite_statements() -> Vec<String> {
    let text: String = section(8)
        .lines()
        .filter_map(|l| l.strip_prefix("--   "))
        .map(|l| format!("{l}\n"))
        .collect();
    text.split(";\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A `houses` table whose price is an exact function of area and bedrooms, the
/// model tables, and two models written the old way.
async fn old_installation(cat: &Catalog) -> Result<()> {
    bootstrap_models(cat).await?;
    bootstrap_model_instances(cat).await?;
    cat.create_table(
        "houses",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("price", TypeRef::Basic(BasicType::Float)),
            DataField::plain("area", TypeRef::Basic(BasicType::Float)),
            DataField::plain("bedrooms", TypeRef::Basic(BasicType::Int)),
        ],
    )
    .await?;
    let rows: Vec<Vec<Expr>> = (1..=40_i64)
        .map(|i| {
            vec![
                Expr::lit(i),
                Expr::lit((1000 * (50 + i) + 20000 * (1 + i % 5)) as f64),
                Expr::lit((50 + i) as f64),
                Expr::lit(1 + i % 5),
            ]
        })
        .collect();
    run(
        cat,
        Insert {
            table: "houses".into(),
            columns: vec![
                "id".into(),
                "price".into(),
                "area".into(),
                "bedrooms".into(),
            ],
            rows,
            returning: Vec::new(),
        }
        .into(),
    )
    .await?;
    // Before datasets had a table of their own.
    run(
        cat,
        Statement::raw(r#"DROP TABLE "_fd_datasets""#, Vec::new()),
    )
    .await?;
    for (name, dataset, related) in [
        (
            "House prices",
            json!({
                "table": "houses",
                "columns": [
                    { "name": "price", "expr": "price" },
                    { "name": "area", "expr": "area" },
                    { "name": "rooms", "expr": "bedrooms" },
                ],
                "filter": "bedrooms > 1",
                "order": [{ "expr": "price", "descending": true }],
            }),
            Json::Null,
        ),
        (
            "Plain",
            json!({ "table": "houses", "columns": [
                { "name": "price", "expr": "price" }, { "name": "area", "expr": "area" }
            ]}),
            json!([{
                "name": "sizes",
                "dataset": { "table": "houses", "columns": [{ "name": "b", "expr": "bedrooms" }] },
                "label": "id",
            }]),
        ),
    ] {
        run(
            cat,
            Insert::row(
                sc_model::MODELS_TABLE,
                [
                    "id",
                    "name",
                    "description",
                    "table_name",
                    "provider",
                    "dataset",
                    "configuration",
                    "hyperparameters",
                    "split",
                    "attributes",
                    "related",
                ]
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
                vec![
                    Expr::lit(uuid::Uuid::new_v4()),
                    Expr::lit(name),
                    Expr::lit(""),
                    Expr::lit("houses"),
                    Expr::lit("linear_regression"),
                    Expr::Lit(Value::Json(dataset)),
                    Expr::Lit(Value::Json(json!({ "label": "price" }))),
                    Expr::Lit(Value::Json(json!({}))),
                    Expr::Lit(Value::Json(
                        json!({ "train": 1.0, "validation": 0.0, "test": 0.0, "seed": 1 }),
                    )),
                    Expr::Lit(Value::Json(json!({}))),
                    if related.is_null() {
                        Expr::Lit(Value::Null)
                    } else {
                        Expr::Lit(Value::Json(related))
                    },
                ],
            )
            .into(),
        )
        .await?;
    }
    Ok(())
}

async fn run(cat: &Catalog, statement: Statement) -> Result<Vec<sc_db::Row>> {
    cat.primary().query(&statement).await?.try_collect().await
}

/// What the migration leaves: every dataset and every model's references,
/// as text that does not depend on the order rows come back in.
async fn state(cat: &Catalog) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for table in ["_fd_datasets", sc_model::MODELS_TABLE] {
        for row in run(cat, Select::from(Source::table(table)).into()).await? {
            let pick = |c: &str| format!("{:?}", row.get(c));
            out.push(match table {
                "_fd_datasets" => {
                    format!("{} {} {}", pick("name"), pick("base"), pick("operations"))
                }
                _ => format!("{} {} {}", pick("name"), pick("dataset"), pick("related")),
            });
        }
    }
    out.sort();
    Ok(out)
}

/// After the migration: each model reads its named dataset, as the old one
/// read, and fits.
async fn the_models_fit(cat: Arc<Catalog>) -> Result<()> {
    cat.reload().await?;
    let models = list_models(&cat).await?;
    assert_eq!(models.len(), 2);
    let prices = &models[0];
    assert_eq!(prices.name, "House prices");
    assert_eq!(prices.dataset.error, None);
    assert_eq!(prices.dataset.name, "House prices — data");
    let names: Vec<&str> = prices
        .dataset
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["price", "area", "rooms"]);
    let plain = &models[1];
    assert_eq!(plain.related.len(), 1);
    assert_eq!(plain.related[0].dataset.name, "Plain — sizes");
    assert_eq!(plain.related[0].label.as_deref(), Some("id"));

    let registry = builtin_registry()?;
    let source = CompiledSource::new(Arc::clone(&cat));
    for model in &models {
        let started = ModelInstance::starting(model.id);
        save_model_instance(&cat, &started).await?;
        let fit = fit_model(&cat, &registry, &source, model, started.id, 1000).await?;
        assert_eq!(fit.status, FitStatus::Fitted, "{:?}", fit.error());
    }
    // The filter came across: 32 of the 40 houses have more than one bedroom.
    let fit = sc_model::list_model_instances(&cat, prices.id).await?;
    assert_eq!(fit[0].attributes["rows"]["selected"], json!(32));
    Ok(())
}

#[tokio::test]
async fn section_7_names_the_datasets_of_old_models_on_postgres_and_twice_is_once() -> Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    old_installation(&cat).await?;
    let client = db.client().await?;
    let sql = section(7);
    client
        .batch_execute(&sql)
        .await
        .map_err(|e| sc_error::Error::database(format!("section 7: {e}")))?;
    let once = state(&cat).await?;
    client
        .batch_execute(&sql)
        .await
        .map_err(|e| sc_error::Error::database(format!("section 7 again: {e}")))?;
    assert_eq!(state(&cat).await?, once);
    assert_eq!(once.len(), 5, "three datasets and two models: {once:#?}");
    the_models_fit(cat).await
}

#[tokio::test]
async fn section_8_names_the_datasets_of_old_models_on_sqlite_and_twice_is_once() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let cat = Arc::new(Catalog::init(driver).await?);
    old_installation(&cat).await?;
    let statements = sqlite_statements();
    assert_eq!(statements.len(), 6, "{statements:#?}");
    for statement in &statements {
        run(&cat, Statement::raw(statement.clone(), Vec::new())).await?;
    }
    let once = state(&cat).await?;
    for statement in &statements {
        run(&cat, Statement::raw(statement.clone(), Vec::new())).await?;
    }
    assert_eq!(state(&cat).await?, once);
    assert_eq!(once.len(), 5, "three datasets and two models: {once:#?}");
    the_models_fit(cat).await
}
