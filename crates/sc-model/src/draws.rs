//! The `_fd_model_draws` table: a posterior's draws, one row per element per
//! chain (Stan TODO §14).
//!
//! A fit of the radon model is 4 chains × 1 000 draws × ~1 100 elements: 4.4
//! million numbers. Too many for the instance's JSON columns, and not too many
//! for a table of their own — and the database is where they belong. It is the
//! one place every node of an installation already shares; it is written **in
//! the same transaction that marks the instance fitted**, so a fitted instance
//! always has all of its draws and a failed one has none; it is deleted with the
//! instance; and it is in every backup without a second mechanism.
//!
//! **One row per element per chain** is the granularity, because it is the one
//! whose every read is cheap: "the chains of `alpha`" is one indexed read on
//! `(instance, variable)`, and one element's chain is one JSON array that parses
//! straight into a `Vec<f64>`. A row per *draw* would be 4.4 million rows for
//! one fit; a row per *variable* would make a 50 000-element `y_rep` one 60 MB
//! value read whole to plot one element of it.
//!
//! JSON and not `bytea`, for the reason design §14.2 gives for `state`: a system
//! table with a binary column would be the only one, and the numbers would be
//! unreadable to every tool that is not this one. NaN and the infinities — real
//! values a generated quantity can take — are stored as the strings `"NaN"`,
//! `"Inf"` and `"-Inf"`, which is CmdStan's own JSON convention.

use std::collections::BTreeSet;

use sc_catalog::{Catalog, DataField, SchemaStep, Table};
use sc_db::{Row, Transaction};
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, InSet, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::instance::InstanceId;
use crate::model::ModelId;
use crate::posterior::DrawSeries;
use crate::store::{bad_column, rows, text};

/// Name of the draws table in the primary database.
pub const DRAWS_TABLE: &str = "_fd_model_draws";

/// The index every read goes through.
const DRAWS_INDEX: &str = "sc_ix__fd_model_draws_instance_variable";

/// The UUID primary-key column.
pub const COL_ID: &str = "id";
/// The instance the draws are of.
pub const COL_INSTANCE: &str = "instance";
/// The variable — `alpha`, `lp__`.
pub const COL_VARIABLE: &str = "variable";
/// The 1-based index array, `[]` for a scalar (JSON).
pub const COL_ELEMENT: &str = "element";
/// The chain, from 1.
pub const COL_CHAIN: &str = "chain";
/// Whether these are warmup iterations.
pub const COL_WARMUP: &str = "warmup";
/// The values, one per iteration in order (JSON).
pub const COL_DRAWS: &str = "draws";

/// Rows per `INSERT`. Seven binds a row keeps a batch far under every
/// backend's parameter limit, and a batch of a thousand-draw series is a few
/// megabytes of statement — big enough that 4 400 rows are 18 round trips, not
/// 4 400.
const INSERT_BATCH: usize = 250;

/// Elements per `IN (…)` when a read selects some — the same bound, for the
/// same reason.
const ELEMENT_BATCH: usize = 500;

/// The fields of the `_fd_model_draws` table, in declaration order.
fn draws_fields() -> Vec<DataField> {
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        // Not a foreign key, for the reason `_fd_model_instances.model` is not:
        // the deletion rule is stated in code, in the transaction that deletes
        // the instance, rather than in a constraint to reconcile onto every
        // existing database.
        DataField::plain(COL_INSTANCE, TypeRef::Basic(BasicType::Uuid)).required(),
        DataField::plain(COL_VARIABLE, TypeRef::Basic(BasicType::Text)).required(),
        DataField::plain(COL_ELEMENT, json()).required(),
        DataField::plain(COL_CHAIN, TypeRef::Basic(BasicType::Int)).required(),
        DataField::plain(COL_WARMUP, TypeRef::Basic(BasicType::Bool)).required(),
        DataField::plain(COL_DRAWS, json()).required(),
    ]
}

/// Ensure `_fd_model_draws` and its `(instance, variable)` index exist.
///
/// Idempotent: the index is `CREATE INDEX IF NOT EXISTS`, which both backends
/// speak identically, so a second boot is a no-op and needs no introspection to
/// know it.
pub async fn bootstrap_model_draws(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(DRAWS_TABLE, &draws_fields())
        .await?;
    let dialect = catalog.primary().dialect();
    catalog
        .apply_schema_batch(&[SchemaStep::Sql(format!(
            "CREATE INDEX IF NOT EXISTS {} ON {} ({}, {})",
            dialect.quote_ident(DRAWS_INDEX),
            dialect.quote_ident(DRAWS_TABLE),
            dialect.quote_ident(COL_INSTANCE),
            dialect.quote_ident(COL_VARIABLE),
        ))])
        .await?;
    Ok(table)
}

/// Write `draws` for `instance` on `tx`, replacing any it already had.
///
/// The caller's transaction, never one of its own: this is half of "the
/// instance is fitted", and the other half is the instance row
/// ([`save_fitted_instance`](crate::save_fitted_instance)). The draws are checked
/// before anything is written, because a series the table cannot hold — an
/// index of 0, a chain of 0, the same element twice — is the provider's bug and
/// should be refused whole rather than half-written and rolled back.
pub(crate) async fn write_draws(
    tx: &mut dyn Transaction,
    instance: InstanceId,
    draws: &[DrawSeries],
) -> Result<()> {
    check_draws(draws)?;
    delete_on(tx, Expr::col(COL_INSTANCE).eq(Expr::lit(instance.0))).await?;
    let columns: Vec<String> = [
        COL_ID,
        COL_INSTANCE,
        COL_VARIABLE,
        COL_ELEMENT,
        COL_CHAIN,
        COL_WARMUP,
        COL_DRAWS,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect();
    for batch in draws.chunks(INSERT_BATCH) {
        let rows = batch
            .iter()
            .map(|series| {
                vec![
                    Expr::lit(Uuid::new_v4()),
                    Expr::lit(instance.0),
                    Expr::lit(series.variable.clone()),
                    Expr::lit(Value::Json(element_json(&series.element))),
                    Expr::lit(i64::from(series.chain)),
                    Expr::lit(series.warmup),
                    Expr::lit(Value::Json(values_json(&series.draws))),
                ]
            })
            .collect();
        let insert = Insert {
            table: DRAWS_TABLE.to_owned(),
            columns: columns.clone(),
            rows,
            returning: Vec::new(),
        };
        tx.query(&Statement::from(insert))
            .await?
            .try_collect()
            .await?;
    }
    Ok(())
}

/// Delete the draws of one instance on `tx`.
pub(crate) async fn delete_instance_draws(
    tx: &mut dyn Transaction,
    instance: InstanceId,
) -> Result<()> {
    delete_on(tx, Expr::col(COL_INSTANCE).eq(Expr::lit(instance.0))).await
}

/// Delete the draws of every instance of `model` on `tx` — before the instances
/// themselves, since the instance rows are how they are found.
pub(crate) async fn delete_model_draws(tx: &mut dyn Transaction, model: ModelId) -> Result<()> {
    let mut ids = Select::from(Source::table(crate::INSTANCES_TABLE))
        .filter(Expr::col(crate::instance_store::COL_MODEL).eq(Expr::lit(model.0)));
    ids.columns = vec![Projection::expr(Expr::col(crate::instance_store::COL_ID))];
    delete_on(
        tx,
        Expr::In {
            e: Box::new(Expr::col(COL_INSTANCE)),
            set: InSet::Subquery(Box::new(ids)),
        },
    )
    .await
}

/// `DELETE FROM _fd_model_draws WHERE …` on `tx`. The caller has checked the
/// table exists: an error inside a Postgres transaction poisons the rest of it.
async fn delete_on(tx: &mut dyn Transaction, filter: Expr) -> Result<()> {
    let delete = Delete::from(DRAWS_TABLE).filter(filter);
    tx.query(&Statement::from(delete))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Refuse draws the table cannot hold faithfully, naming the first offender.
fn check_draws(draws: &[DrawSeries]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for series in draws {
        let at = |why: &str| {
            Error::invalid(format!(
                "the draws of `{}` in chain {} {why}",
                series.label(),
                series.chain
            ))
        };
        if series.variable.trim().is_empty() {
            return Err(Error::invalid(
                "a series of draws names no variable".to_owned(),
            ));
        }
        if series.chain == 0 {
            return Err(at("are numbered chain 0; chains count from 1"));
        }
        if series.element.contains(&0) {
            return Err(at("have an index of 0; Stan indices count from 1"));
        }
        if !seen.insert((
            series.variable.as_str(),
            series.element.as_slice(),
            series.chain,
            series.warmup,
        )) {
            return Err(at("appear twice"));
        }
    }
    Ok(())
}

/// The element index as the JSON the column holds.
fn element_json(element: &[usize]) -> Json {
    Json::Array(element.iter().map(|i| Json::from(*i)).collect())
}

/// The values as the JSON the column holds: numbers, and CmdStan's strings for
/// the three values JSON has no number for.
fn values_json(values: &[f64]) -> Json {
    Json::Array(
        values
            .iter()
            .map(|v| match serde_json::Number::from_f64(*v) {
                Some(n) => Json::Number(n),
                None if v.is_nan() => Json::from("NaN"),
                None if *v > 0.0 => Json::from("Inf"),
                None => Json::from("-Inf"),
            })
            .collect(),
    )
}

/// Which draws of one instance a read wants.
///
/// One variable per read, because that is what the index is on and what every
/// caller asks — a trace plot, a summary row, a write-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawsQuery {
    /// The variable.
    pub variable: String,
    /// Only these elements (1-based index arrays); `None` for all of them.
    pub elements: Option<Vec<Vec<usize>>>,
    /// Only these chains; `None` for all of them.
    pub chains: Option<Vec<u32>>,
    /// Whether warmup series come back too (they exist only when the fit saved
    /// them).
    pub warmup: bool,
}

impl DrawsQuery {
    /// Every post-warmup draw of `variable`.
    pub fn variable(variable: impl Into<String>) -> DrawsQuery {
        DrawsQuery {
            variable: variable.into(),
            elements: None,
            chains: None,
            warmup: false,
        }
    }

    /// Only these elements.
    pub fn elements(mut self, elements: Vec<Vec<usize>>) -> DrawsQuery {
        self.elements = Some(elements);
        self
    }

    /// Only these chains.
    pub fn chains(mut self, chains: Vec<u32>) -> DrawsQuery {
        self.chains = Some(chains);
        self
    }

    /// Warmup series too.
    pub fn with_warmup(mut self) -> DrawsQuery {
        self.warmup = true;
        self
    }
}

/// Reads one instance's draws back out of `_fd_model_draws`.
pub struct DrawsReader<'a> {
    catalog: &'a Catalog,
    instance: InstanceId,
}

impl<'a> DrawsReader<'a> {
    /// A reader of `instance`'s draws.
    pub fn new(catalog: &'a Catalog, instance: InstanceId) -> DrawsReader<'a> {
        DrawsReader { catalog, instance }
    }

    /// The series `query` selects, ordered by element, then chain, then warmup
    /// before the iterations that follow it.
    ///
    /// An instance with no such variable answers an empty list rather than an
    /// error: whether the variable *should* exist is a question about the
    /// program, and the caller that knows the program asks it.
    pub async fn read(&self, query: &DrawsQuery) -> Result<Vec<DrawSeries>> {
        let mut base = Expr::col(COL_INSTANCE)
            .eq(Expr::lit(self.instance.0))
            .and(Expr::col(COL_VARIABLE).eq(Expr::lit(query.variable.clone())));
        if !query.warmup {
            base = base.and(Expr::col(COL_WARMUP).eq(Expr::lit(false)));
        }
        if let Some(chains) = &query.chains {
            if chains.is_empty() {
                return Ok(Vec::new());
            }
            base = base.and(Expr::In {
                e: Box::new(Expr::col(COL_CHAIN)),
                set: InSet::List(chains.iter().map(|c| Expr::lit(i64::from(*c))).collect()),
            });
        }
        let filters: Vec<Expr> = match &query.elements {
            None => vec![base],
            Some(elements) if elements.is_empty() => return Ok(Vec::new()),
            // The element is compared as JSON: both backends hold what
            // `element_json` wrote, and a literal built by the same function
            // compares equal to it — as `jsonb` on Postgres, and as the same
            // compact text on SQLite.
            Some(elements) => elements
                .chunks(ELEMENT_BATCH)
                .map(|chunk| {
                    base.clone().and(Expr::In {
                        e: Box::new(Expr::col(COL_ELEMENT)),
                        set: InSet::List(
                            chunk
                                .iter()
                                .map(|e| Expr::lit(Value::Json(element_json(e))))
                                .collect(),
                        ),
                    })
                })
                .collect(),
        };
        let mut out = Vec::new();
        for filter in filters {
            let select = Select::from(Source::table(DRAWS_TABLE)).filter(filter);
            for row in rows(self.catalog, select).await? {
                out.push(series_from_row(&row)?);
            }
        }
        out.sort_by(|a, b| (&a.element, a.chain, !a.warmup).cmp(&(&b.element, b.chain, !b.warmup)));
        Ok(out)
    }

    /// How many series the instance has stored — every variable, element, chain
    /// and warmup flag.
    pub async fn count(&self) -> Result<usize> {
        if self.catalog.get(DRAWS_TABLE)?.is_none() {
            return Ok(0);
        }
        let mut select = Select::from(Source::table(DRAWS_TABLE))
            .filter(Expr::col(COL_INSTANCE).eq(Expr::lit(self.instance.0)));
        select.columns = vec![Projection::expr(Expr::col(COL_ID))];
        Ok(rows(self.catalog, select).await?.len())
    }
}

/// One stored row as the series it was written from. Strict, as every store in
/// this crate is: a row that does not read is an error naming the column, never
/// a series quietly missing its values.
fn series_from_row(row: &Row) -> Result<DrawSeries> {
    let variable = text(row, COL_VARIABLE)?;
    let at = |e: String| Error::invalid(format!("{DRAWS_TABLE} row for `{variable}`: {e}"));
    let element = match row.get(COL_ELEMENT) {
        Some(Value::Json(Json::Array(items))) => items
            .iter()
            .map(|i| {
                i.as_u64()
                    .filter(|i| *i > 0)
                    .and_then(|i| usize::try_from(i).ok())
                    .ok_or_else(|| at(format!("{COL_ELEMENT} holds `{i}`, not a 1-based index")))
            })
            .collect::<Result<Vec<usize>>>()?,
        other => {
            return Err(at(
                bad_column(COL_ELEMENT, "a json array", other).to_string()
            ));
        }
    };
    let chain = match row.get(COL_CHAIN) {
        Some(Value::Int(c)) => u32::try_from(*c).map_err(|_| at(format!("{COL_CHAIN} is {c}")))?,
        other => return Err(at(bad_column(COL_CHAIN, "an integer", other).to_string())),
    };
    let warmup = match row.get(COL_WARMUP) {
        Some(Value::Bool(w)) => *w,
        other => return Err(at(bad_column(COL_WARMUP, "a boolean", other).to_string())),
    };
    let draws = match row.get(COL_DRAWS) {
        Some(Value::Json(Json::Array(values))) => values
            .iter()
            .map(|v| match v {
                Json::Number(n) => n.as_f64().ok_or_else(|| at(format!("`{n}` is not a draw"))),
                Json::String(s) if s == "NaN" => Ok(f64::NAN),
                Json::String(s) if s == "Inf" => Ok(f64::INFINITY),
                Json::String(s) if s == "-Inf" => Ok(f64::NEG_INFINITY),
                other => Err(at(format!(
                    "{COL_DRAWS} holds `{other}`, which is not a draw"
                ))),
            })
            .collect::<Result<Vec<f64>>>()?,
        other => return Err(at(bad_column(COL_DRAWS, "a json array", other).to_string())),
    };
    Ok(DrawSeries {
        variable: variable.clone(),
        element,
        chain,
        warmup,
        draws,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_values_json_has_no_number_for_are_cmdstans_strings() {
        assert_eq!(
            values_json(&[1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
            serde_json::json!([1.5, "NaN", "Inf", "-Inf"])
        );
        assert_eq!(element_json(&[2, 1]), serde_json::json!([2, 1]));
        assert_eq!(element_json(&[]), serde_json::json!([]));
    }

    #[test]
    fn a_series_the_table_cannot_hold_is_refused_before_anything_is_written() {
        let ok = DrawSeries::new("alpha", vec![1], 1, vec![0.0]);
        check_draws(std::slice::from_ref(&ok)).expect("fine");
        let err = check_draws(&[DrawSeries::new("alpha", vec![0], 1, vec![])]).unwrap_err();
        assert!(err.to_string().contains("count from 1"), "{err}");
        let err = check_draws(&[DrawSeries::new("alpha", vec![1], 0, vec![])]).unwrap_err();
        assert!(err.to_string().contains("chain 0"), "{err}");
        let err = check_draws(&[ok.clone(), ok.clone()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("`alpha[1]` in chain 1 appear twice"),
            "{err}"
        );
        // The same element as warmup is a different series.
        check_draws(&[ok.clone(), ok.warmup()]).expect("warmup is its own series");
    }

    #[test]
    fn the_schema_is_one_row_per_element_per_chain() {
        let fields = draws_fields();
        let names: Vec<&str> = fields.iter().map(|f| f.base.name.as_str()).collect();
        assert_eq!(
            names,
            [
                COL_ID,
                COL_INSTANCE,
                COL_VARIABLE,
                COL_ELEMENT,
                COL_CHAIN,
                COL_WARMUP,
                COL_DRAWS
            ]
        );
        assert!(fields.iter().all(|f| f.required));
    }
}
