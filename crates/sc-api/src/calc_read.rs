//! Calculated fields on the read path: projected in SQL where they translate,
//! and **computed after the `SELECT`** where they do not (milestone 31 §4).
//!
//! A non-stored calculated field is ordinarily one more projection of the
//! read — `pages * 2`, a Ⱶ-join, an aggregation all translate. One that calls
//! `predict("…")` or a module function cannot: its value comes from outside
//! the database. Such a field used to be **skipped**, silently absent from
//! every row. It is now evaluated by the reified evaluator over the fetched
//! page, in the fields' dependency order (a field that reads a predicting field
//! sees its value), with every hoisted value resolved first:
//!
//! - **predictions are batched per page**: one [`ModelHost::predict`] per model,
//!   with every row's key, so a 50-row page is one dataset read and one provider
//!   call rather than fifty;
//! - the other hoisted values (Ⱶ-joins, Ↄ-relations, module calls) are resolved
//!   per row by [`prefetch_bindings`], as they are on the write path.
//!
//! An error **fails the read**, naming the field, the model and the row, which
//! is the module functions' rule and the system's position on silent failure.
//! And such a field cannot be filtered or sorted on, because the database never
//! sees it: [`CalcPlan::refuse_in_query`] is the one sentence every place that
//! lowers a filter says so with.
//!
//! [`ModelHost::predict`]: sc_catalog::ModelHost::predict

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sc_catalog::{Catalog, PredictRows, Table, prefetch_bindings};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_expr::{
    AmbientValues, Analysis, CalcFields, Env, Formula, FormulaCall, Operation, SchemaShape,
    TranslateError, UserEnv, translate_value, value_from_json,
};
use sc_query::{Expr, InSet, OrderBy, Projection, Value};
use serde_json::Value as Json;

use crate::convert::value_to_json;

/// How a read computes `table`'s non-stored calculated fields: the ones SQL
/// projects, and the ones computed after the rows are fetched.
pub(crate) struct CalcPlan {
    /// The translatable fields, each aliased to its name.
    pub(crate) projections: Vec<Projection>,
    /// The rest, in dependency order.
    after: Vec<AfterRead>,
    /// The shape the after-read formulas were validated against, which their
    /// prefetch needs again.
    shape: Option<SchemaShape>,
}

/// One calculated field computed after the read.
struct AfterRead {
    field: String,
    formula: Formula,
    analysis: Analysis,
    /// Why it is computed after the read, as the end of "… because …".
    because: String,
}

impl CalcPlan {
    /// The plan for `table`'s calculated fields.
    pub(crate) fn of(catalog: &Catalog, table: &Table) -> Result<CalcPlan> {
        let calc = calc_map(table);
        let mut plan = CalcPlan {
            projections: Vec::new(),
            after: Vec::new(),
            shape: None,
        };
        if calc.is_empty() {
            return Ok(plan);
        }
        let shape = catalog.schema_shape()?;
        let env = UserEnv::Inline(None);
        let mut untranslatable = Vec::new();
        for field in table.calc_fields() {
            let Some(formula) = calc.get(&field.base.name) else {
                continue;
            };
            match translate_value(
                formula,
                &Env::new(&env).with_calc(&calc),
                &shape,
                &table.name,
            ) {
                Ok(expr) => plan
                    .projections
                    .push(Projection::expr_as(expr, field.base.name.clone())),
                Err(TranslateError::Untranslatable(_)) => {
                    let analysis = formula.validate(&shape, &table.name).map_err(|e| {
                        Error::invalid(format!(
                            "the calculated field `{}` of `{}`: {e}",
                            field.base.name, table.name
                        ))
                    })?;
                    untranslatable.push(AfterRead {
                        field: field.base.name.clone(),
                        formula: formula.clone(),
                        analysis,
                        because: String::new(),
                    });
                }
                Err(TranslateError::Error(e)) => return Err(e),
            }
        }
        plan.after = in_dependency_order(untranslatable);
        plan.shape = Some(shape);
        Ok(plan)
    }

    /// Whether every calculated field is in the `SELECT`.
    pub(crate) fn is_complete(&self) -> bool {
        self.after.is_empty()
    }

    /// Refuse a filter or an ordering that names a field computed after the
    /// read, with the sentence saying why. `doing` is what the caller was
    /// doing with it: "filter on", "sort by".
    pub(crate) fn refuse_in_query<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
        doing: &str,
    ) -> Result<()> {
        if self.after.is_empty() {
            return Ok(());
        }
        for name in names {
            if let Some(after) = self.after.iter().find(|a| a.field == name) {
                return Err(Error::invalid(format!(
                    "cannot {doing} `{name}`: `{name}` is computed after the rows are read, \
                     because {}, so the database never sees it",
                    after.because
                )));
            }
        }
        Ok(())
    }

    /// Refuse a read whose filter or ordering names a field computed after
    /// the read — the check every read path shares, because each of them
    /// (REST's query string, GraphQL's `where` and `order_by`, a code body's
    /// plan) lowers its filter to an [`Expr`] that ends up here.
    ///
    /// A column of this table only: a subquery ranges over another table,
    /// whose calculated fields are not this plan's.
    pub(crate) fn refuse_query(
        &self,
        table: &Table,
        filter: Option<&Expr>,
        order: &[OrderBy],
    ) -> Result<()> {
        if self.after.is_empty() {
            return Ok(());
        }
        let mut named = Vec::new();
        if let Some(filter) = filter {
            columns_of(filter, &table.name, &mut named);
        }
        self.refuse_in_query(named.iter().map(String::as_str), "filter on")?;
        let mut named = Vec::new();
        for key in order {
            columns_of(&key.expr, &table.name, &mut named);
        }
        self.refuse_in_query(named.iter().map(String::as_str), "sort by")
    }

    /// `rows`, with every field computed after the read appended to each —
    /// `rows` unchanged when there are none.
    pub(crate) async fn complete(
        &self,
        catalog: &Catalog,
        table: &Table,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.after.is_empty() || rows.is_empty() {
            return Ok(rows);
        }
        let Some(shape) = &self.shape else {
            return Ok(rows);
        };
        let first = &self.after[0].field;
        let evaluator = catalog.formula_evaluator().ok_or_else(|| {
            Error::invalid(format!(
                "`{first}` of `{}` is computed after the rows are read, and this server has no \
                 formula engine to compute it",
                table.name
            ))
        })?;
        let mut values: Vec<BTreeMap<String, Value>> =
            rows.iter().map(crate::rows::row_values).collect();
        self.predict_page(catalog, table, &mut values).await?;
        for row in &mut values {
            for after in &self.after {
                let value = compute(catalog, table, shape, &evaluator, after, row).await?;
                row.insert(after.field.clone(), value);
            }
        }
        // The same columns, in the same order, with the computed fields after
        // them: a caller reading the row as JSON or as values sees one shape.
        let mut columns: Vec<String> = rows
            .first()
            .map(|r| r.columns().to_vec())
            .unwrap_or_default();
        for after in &self.after {
            if !columns.contains(&after.field) {
                columns.push(after.field.clone());
            }
        }
        let columns = Arc::new(columns);
        values
            .into_iter()
            .map(|mut row| {
                let ordered = columns
                    .iter()
                    .map(|c| row.remove(c).unwrap_or(Value::Null))
                    .collect();
                Row::new(Arc::clone(&columns), ordered)
            })
            .collect()
    }

    /// Every prediction the page needs, **one provider call per model**:
    /// each keyed row's key in one [`PredictRows::Keys`], answered in order
    /// and bound under the call's key. A row with no key is left for
    /// [`prefetch_bindings`], which predicts it from its values.
    async fn predict_page(
        &self,
        catalog: &Catalog,
        table: &Table,
        values: &mut [BTreeMap<String, Value>],
    ) -> Result<()> {
        // Each call once, named by the first field that makes it.
        let mut calls: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
        for after in &self.after {
            for call in &after.analysis.model_calls {
                calls
                    .entry(call.key.as_str())
                    .or_insert((call.model.as_str(), after.field.as_str()));
            }
        }
        let [pk] = table.primary_key.as_slice() else {
            return Ok(());
        };
        if calls.is_empty() {
            return Ok(());
        }
        let keyed: Vec<(usize, Json)> = values
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                row.get(pk)
                    .filter(|v| !v.is_null())
                    .map(|v| (i, value_to_json(v)))
            })
            .collect();
        if keyed.is_empty() {
            return Ok(());
        }
        let Some(host) = catalog.model_host() else {
            // `calls` is not empty (checked above), so this names one.
            let named = calls
                .first_key_value()
                .map(|(call, (_, field))| format!("`{field}` of `{}` calls `{call}`", table.name))
                .unwrap_or_else(|| format!("a field of `{}` calls `predict`", table.name));
            return Err(Error::invalid(format!(
                "{named}, and this server has no model support to answer it"
            )));
        };
        let keys: Vec<Json> = keyed.iter().map(|(_, k)| k.clone()).collect();
        for (call, (model, field)) in calls {
            let answered = match host
                .predict(model, None, &table.name, PredictRows::Keys(&keys), false)
                .await
            {
                Ok(answered) if answered.len() == keys.len() => answered,
                Ok(answered) => {
                    return Err(Error::invalid(format!(
                        "`{call}` answered {} values for {} rows of `{}`",
                        answered.len(),
                        keys.len(),
                        table.name
                    )));
                }
                Err(e) => {
                    return Err(blame_row(&*host, table, pk, &keys, call, model, field, e).await);
                }
            };
            for ((index, _), value) in keyed.iter().zip(answered) {
                values[*index].insert(call.to_owned(), value_from_json(&value));
            }
        }
        Ok(())
    }
}

/// A batch prediction failed: find the row it failed for, by asking again one
/// row at a time, so the error names the field, the model **and the row**.
/// Only on the error path, where the cost is worth the sentence.
#[allow(clippy::too_many_arguments)]
async fn blame_row(
    host: &dyn sc_catalog::ModelHost,
    table: &Table,
    pk: &str,
    keys: &[Json],
    call: &str,
    model: &str,
    field: &str,
    batch: Error,
) -> Error {
    for key in keys {
        if let Err(e) = host
            .predict(
                model,
                None,
                &table.name,
                PredictRows::Keys(std::slice::from_ref(key)),
                false,
            )
            .await
        {
            return Error::invalid(format!(
                "`{field}` of `{}` could not be computed for the row whose {pk} is {}: `{call}`: \
                 {e}",
                table.name,
                key_text(key)
            ));
        }
    }
    Error::invalid(format!(
        "`{field}` of `{}` could not be computed: `{call}`: {batch}",
        table.name
    ))
}

/// Compute one field for one row: its other hoisted values, then the formula.
async fn compute(
    catalog: &Catalog,
    table: &Table,
    shape: &SchemaShape,
    evaluator: &Arc<dyn sc_expr::JsEvaluator>,
    after: &AfterRead,
    row: &mut BTreeMap<String, Value>,
) -> Result<Value> {
    let failed = |row: &BTreeMap<String, Value>, e: Error| {
        let which = match table.primary_key.as_slice() {
            [pk] => row
                .get(pk)
                .map(|v| format!(" for the row whose {pk} is {}", key_text(&value_to_json(v))))
                .unwrap_or_default(),
            _ => String::new(),
        };
        Error::invalid(format!(
            "`{}` of `{}` could not be computed{which}: {e}",
            after.field, table.name
        ))
    };
    if let Err(e) = prefetch_bindings(catalog, table, &after.analysis, shape, row).await {
        return Err(failed(row, e));
    }
    let call = FormulaCall {
        formula: after.formula.clone(),
        op: Operation::Read,
        row: row.clone(),
        user: None,
        ambient: AmbientValues::new(),
    };
    let json = evaluator
        .eval_value(call)
        .await
        .map_err(|e| failed(row, e))?;
    // The value as the formula produced it, which is what SQL does for a
    // field it projects: a calculated field's declared type says how it is
    // shown, not what it is coerced to.
    Ok(value_from_json(&json))
}

/// Every column of `table` that `expr` names, bare or qualified by the
/// table's name, not looking inside subqueries.
fn columns_of(expr: &Expr, table: &str, out: &mut Vec<String>) {
    match expr {
        Expr::Col(col) => {
            if col.table.as_deref().is_none_or(|t| t == table) {
                out.push(col.column.clone());
            }
        }
        Expr::Lit(_) | Expr::Param(_) | Expr::Subquery(_) => {}
        Expr::Binary { l, r, .. } => {
            columns_of(l, table, out);
            columns_of(r, table, out);
        }
        Expr::Unary { e, .. } => columns_of(e, table, out),
        Expr::Func { args, .. } | Expr::Agg { args, .. } => {
            for arg in args {
                columns_of(arg, table, out);
            }
        }
        Expr::Window {
            args,
            partition,
            order,
            ..
        } => {
            for e in args.iter().chain(partition) {
                columns_of(e, table, out);
            }
            for key in order {
                columns_of(&key.expr, table, out);
            }
        }
        Expr::In { e, set } => {
            columns_of(e, table, out);
            if let InSet::List(items) = set {
                for item in items {
                    columns_of(item, table, out);
                }
            }
        }
        Expr::Json { target, .. } => columns_of(target, table, out),
        Expr::Case {
            operand,
            arms,
            else_result,
        } => {
            for e in operand.iter().chain(else_result) {
                columns_of(e, table, out);
            }
            for arm in arms {
                columns_of(&arm.when, table, out);
                columns_of(&arm.then, table, out);
            }
        }
        Expr::Cast { expr, .. } => columns_of(expr, table, out),
    }
}

/// A key as a person reads it: a string without its quotes.
fn key_text(key: &Json) -> String {
    match key {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The after-read fields ordered so each comes after every after-read field
/// it reads, each with the reason it is computed after the read.
fn in_dependency_order(mut pending: Vec<AfterRead>) -> Vec<AfterRead> {
    let names: BTreeSet<String> = pending.iter().map(|a| a.field.clone()).collect();
    let mut placed: Vec<AfterRead> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending.iter().position(|a| {
            a.analysis
                .fields
                .iter()
                .filter(|f| names.contains(*f) && **f != a.field)
                .all(|f| placed.iter().any(|p| &p.field == f))
        });
        // A cycle cannot be saved; if one is stored anyway, the rest keep
        // their declaration order and the evaluator reports the unbound name.
        let mut next = pending.remove(ready.unwrap_or(0));
        next.because = because(&next, &placed);
        placed.push(next);
    }
    placed
}

/// Why `after` is computed after the read, given the fields placed before it.
fn because(after: &AfterRead, placed: &[AfterRead]) -> String {
    if after.analysis.first_model_call().is_some() {
        return "it calls `predict`".to_owned();
    }
    if let Some(call) = after.analysis.first_module_call() {
        return format!("it calls the module function `{}`", call.function);
    }
    if let Some(reads) = placed
        .iter()
        .find(|p| after.analysis.fields.contains(&p.field))
    {
        return format!(
            "it reads `{}`, which is computed after the read too",
            reads.field
        );
    }
    "its formula does not translate to SQL".to_owned()
}

/// The parsed calc-field expressions of `table` (Phase 8), keyed by field name.
/// Re-parses the stored source, which validated cleanly at merge time.
pub(crate) fn calc_map(table: &Table) -> CalcFields {
    table
        .calc_fields()
        .filter_map(|f| {
            let expr = f.calc_expression()?;
            Formula::parse(expr)
                .ok()
                .map(|fm| (f.base.name.clone(), fm))
        })
        .collect()
}
