//! Prefetching the values a **reified** formula evaluation needs (§7.3, Phase 7).
//!
//! The JavaScript evaluator does no I/O — it is handed a flat map of bindings —
//! so whoever calls it must first fetch the two kinds of value a formula can read
//! beyond the row's own columns: a **Ⱶ-join path** (`publisherⱵname`, walked link
//! by link through Key fields) and a **Ↄ-relation** (`order_linesↃorder`, the
//! child rows an aggregation ranges over).
//!
//! This lives in `sc-catalog` because it has two callers in different crates and
//! only a `Catalog` between them: ownership enforcement in `sc-api` (layer 8) and
//! a trigger's `only_if` in `sc-action` (layer 6), which cannot see `sc-api`. It
//! was written for the first and moved down here for the second — one
//! implementation, because two would drift and the drift would be *silent*: a
//! path that resolves in one and binds null in the other is a formula that grants
//! in one place and denies in the other.
//!
//! The read path does not use this: a `SELECT` can project a join path as a
//! correlated subselect alongside the row (`sc_expr::join_path_expr`), which is
//! one query instead of one per row. This is for the rows a query cannot project
//! from — a **proposed** row on the write path, and an event's row after the fact.

use std::collections::BTreeMap;

use sc_db::Row;
use sc_error::{Error, Result};
use sc_expr::{
    AggUse, Analysis, INVERSE, ModelCall, ModuleArg, ModuleCall, SchemaShape, value_from_json,
    value_to_json,
};
use sc_query::{Expr, Projection, Select, Source, Value};
use serde_json::{Map, Value as Json};

use crate::catalog::Catalog;
use crate::field::DataFieldKind;
use crate::model_host::PredictRows;
use crate::table::Table;

/// Add one binding per Ⱶ-join path and per Ↄ-relation the `analysis` names to
/// `values`, leaving any the caller already supplied untouched.
///
/// After this, `values` is what a [`FormulaCall`](sc_expr::FormulaCall)'s `row`
/// wants: the row's own fields plus every derived value the formula reads, keyed
/// by the identifier it reads them under. An already-present key is *not*
/// refetched — the read path projects join values into the row it fetched, and
/// this must not undo that.
pub async fn prefetch_bindings(
    cat: &Catalog,
    table: &Table,
    analysis: &Analysis,
    shape: &SchemaShape,
    values: &mut BTreeMap<String, Value>,
) -> Result<()> {
    for path in &analysis.join_paths {
        if !values.contains_key(&path.ident) {
            let value = resolve_join_value(cat, table, &path.segments, values).await?;
            values.insert(path.ident.clone(), value);
        }
    }
    // Module function calls (§4b), resolved **after** the join paths because an
    // argument may be one: `md_to_html(publisherⱵblurb)` reads a value the loop
    // above just fetched. The same rule the whole of this function follows —
    // the evaluator does no I/O, so whoever calls it does the I/O first.
    for call in &analysis.module_calls {
        if !values.contains_key(&call.key) {
            let value = resolve_module_call(cat, call, values).await?;
            values.insert(call.key.clone(), value);
        }
    }
    // Predictions (milestone 31 §4), hoisted like module calls. The read path
    // batches a page's predictions and binds them before it gets here, so a key
    // already present is not asked again.
    for call in &analysis.model_calls {
        if !values.contains_key(&call.key) {
            let value = resolve_model_call(cat, table, call, values).await?;
            values.insert(call.key.clone(), value);
        }
    }
    // Aggregations over incoming keys (Phase 7): the reified evaluator does no
    // I/O, so prefetch each relation's child rows and bind them under the
    // relation identifier (`childↃkey`) — the array the prelude aggregates.
    for agg in &analysis.agg_uses {
        let ident = format!("{}{}{}", agg.child_table, INVERSE, agg.key_field);
        if !values.contains_key(&ident) {
            let rows = resolve_agg_relation(cat, shape, agg, values).await?;
            values.insert(ident, rows);
        }
    }
    Ok(())
}

/// How long a formula's hoisted module call may take.
///
/// **Not** the module pool's own 120 s, which is the bound a Proxmox snapshot
/// fired from a trigger needs, and not the formula's 250 ms, which is the
/// evaluator's alone and is not spent here. This is the bound on the *hoist*:
/// what waits on it is a row being read or written, and a formula that holds a
/// write open for two minutes because a geocoder is down is worse than one that
/// fails saying so.
const MODULE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Resolve one hoisted module function call to the value the evaluator will see.
///
/// The arguments are read, never computed — that is what
/// [`ModuleArg`](sc_expr::ModuleArg) is narrow for, and the classification
/// happened at parse time. A binding the caller did not supply reads as null,
/// exactly as an unresolvable Ⱶ-join link does.
///
/// A server with no modules loaded is an **error naming the call**, not a null:
/// a formula whose module went away computed a different answer, and the whole
/// of this system's position on silent failure is that a different answer is
/// worse than a failure.
async fn resolve_module_call(
    cat: &Catalog,
    call: &ModuleCall,
    values: &BTreeMap<String, Value>,
) -> Result<Value> {
    let Some(host) = cat.module_functions() else {
        return Err(Error::invalid(format!(
            "this formula calls the module function `{}`, and no module is loaded on this \
             server to answer it",
            call.function
        )));
    };
    let mut args = Vec::with_capacity(call.args.len());
    for arg in &call.args {
        args.push(match arg {
            ModuleArg::Literal(text) => serde_json::from_str(text).unwrap_or(Json::Null),
            ModuleArg::Binding(name) => values.get(name).map_or(Json::Null, value_to_json),
        });
    }
    let plan = serde_json::json!({
        "module": call.module,
        "function": call.function,
        "args": args,
        "timeout_ms": MODULE_CALL_TIMEOUT.as_millis() as u64,
    });
    let answered = tokio::time::timeout(MODULE_CALL_TIMEOUT, host.call(plan))
        .await
        .map_err(|_| {
            Error::invalid(format!(
                "the module function `{}` of `{}` took longer than {:?} and this formula was \
                 not evaluated",
                call.function, call.module, MODULE_CALL_TIMEOUT
            ))
        })??;
    Ok(value_from_json(&answered))
}

/// Resolve one `predict("…")` for the row in `values`, through the catalog's
/// [`ModelHost`](crate::ModelHost).
///
/// The row is the one the formula is evaluated over. **By its key** when it
/// has one — a single-column primary key with a value — so it is read through
/// the model's dataset, as it was at fit time, and a row the dataset's filter
/// excludes is still answered. Otherwise (a row not inserted yet, a table with
/// no single key) **its values** are taken as the dataset's columns, and a
/// feature it does not supply is refused by name.
///
/// No host is an error naming the call, never a null: the module functions'
/// rule, for the same reason.
async fn resolve_model_call(
    cat: &Catalog,
    table: &Table,
    call: &ModelCall,
    values: &BTreeMap<String, Value>,
) -> Result<Value> {
    let Some(host) = cat.model_host() else {
        return Err(Error::invalid(format!(
            "this formula calls `{}`, and this server has no model support to answer it",
            call.key
        )));
    };
    let key = match table.primary_key.as_slice() {
        [pk] => values.get(pk).filter(|v| !v.is_null()).map(value_to_json),
        _ => None,
    };
    let answered = match key {
        Some(key) => {
            let keys = [key];
            host.predict(
                &call.model,
                None,
                &table.name,
                PredictRows::Keys(&keys),
                false,
            )
            .await
        }
        None => {
            // The row's own columns and the Ⱶ-join values fetched above; not
            // the hoisted calls' keys, which are no dataset's columns.
            let row: Map<String, Json> = values
                .iter()
                .filter(|(name, _)| !name.contains('('))
                .map(|(name, value)| (name.clone(), value_to_json(value)))
                .collect();
            let rows = [Json::Object(row)];
            host.predict(
                &call.model,
                None,
                &table.name,
                PredictRows::Values(&rows),
                false,
            )
            .await
        }
    }
    .map_err(|e| Error::invalid(format!("`{}` failed: {e}", call.key)))?;
    let value = answered
        .into_iter()
        .next()
        .ok_or_else(|| Error::invalid(format!("`{}` answered nothing for this row", call.key)))?;
    Ok(value_from_json(&value))
}

/// Fetch the child rows an aggregation ranges over, as a JSON array bound under
/// the relation identifier for the reified evaluator. The correlation is the
/// parent's own value in the column the child key targets; a null there (or no
/// matching children) is the empty relation.
async fn resolve_agg_relation(
    cat: &Catalog,
    shape: &SchemaShape,
    agg: &AggUse,
    values: &BTreeMap<String, Value>,
) -> Result<Value> {
    let empty = Value::Json(Json::Array(Vec::new()));
    // The parent column the child key references (the correlation target).
    let Some(parent_field) = shape
        .tables
        .get(&agg.child_table)
        .and_then(|t| t.fields.get(&agg.key_field))
        .and_then(|f| f.key.as_ref())
        .map(|k| k.target_field.clone())
    else {
        return Ok(empty);
    };
    let parent_value = values.get(&parent_field).cloned().unwrap_or(Value::Null);
    if parent_value.is_null() {
        return Ok(empty);
    }
    let child = cat.require(&agg.child_table)?;
    let select = Select::from(Source::table(child.name.clone()))
        .columns(vec![Projection::all()])
        .filter(Expr::col(agg.key_field.clone()).eq(Expr::lit(parent_value)));
    let fetched: Vec<Row> = cat
        .provider(&child)?
        .query(&select)
        .await?
        .try_collect()
        .await?;
    let rows: Vec<Json> = fetched
        .iter()
        .map(|row| {
            let obj: Map<String, Json> = row
                .columns()
                .iter()
                .zip(row.values().iter())
                .map(|(name, value)| (name.clone(), value_to_json(value)))
                .collect();
            Json::Object(obj)
        })
        .collect();
    Ok(Value::Json(Json::Array(rows)))
}

/// Resolve one Ⱶ-join path from a row's values by walking the Key links — the
/// path for *proposed* rows, which are not in the database to be projected
/// from. A null anywhere propagates (the Ⱶ optional-chaining contract).
async fn resolve_join_value(
    cat: &Catalog,
    table: &Table,
    segments: &[String],
    values: &BTreeMap<String, Value>,
) -> Result<Value> {
    let Some(first) = segments.first() else {
        return Ok(Value::Null);
    };
    let mut current = table.clone();
    let mut value = values.get(first).cloned().unwrap_or(Value::Null);
    for i in 1..segments.len() {
        if value.is_null() {
            return Ok(Value::Null);
        }
        let link = &segments[i - 1];
        let field = current
            .field(link)
            .ok_or_else(|| Error::invalid(format!("`{}` has no field `{link}`", current.name)))?;
        let DataFieldKind::Key {
            target_table,
            target_field,
            ..
        } = &field.kind
        else {
            return Err(Error::invalid(format!(
                "`{}`.`{link}` is not a Key field",
                current.name
            )));
        };
        let target = cat.require(&target_table.0)?;
        let next = &segments[i];
        let select = Select::from(Source::table(target.name.clone()))
            .columns(vec![Projection::expr(Expr::col(next.clone()))])
            .filter(Expr::col(target_field.0.clone()).eq(Expr::lit(value)))
            .limit(1);
        let fetched: Vec<Row> = cat
            .provider(&target)?
            .query(&select)
            .await?
            .try_collect()
            .await?;
        value = fetched
            .first()
            .and_then(|r| r.values().first().cloned())
            .unwrap_or(Value::Null);
        current = target;
    }
    Ok(value)
}
