//! Running a [`Select`] over rows that are **already in memory** (design §8.3).
//!
//! A database table answers a `Select` by sending it to a database. A **provided**
//! table cannot: what it has is a module that hands back a list of JSON objects,
//! and the filter, the ordering, the bound and the projection still have to be
//! applied to them. So they are applied here, in Rust, over the values the
//! provider answered.
//!
//! v1 does the same thing in JavaScript — `json_list_to_external_table` passes
//! the plugin its `where` object and then, unless the plugin sets
//! `disableFiltering`, re-filters, re-sorts and re-slices the answer itself. The
//! reason is the same in both versions and it is not distrust: a v1 provider is
//! *allowed* to ignore everything it was passed (`@saltcorn/rss` does — it
//! answers the whole feed whatever you ask), so the caller cannot treat the
//! answer as already-filtered. What differs is the language: this system's reads
//! are a [`Select`], not v1's `where` object, so the interpreter is here.
//!
//! ## What it refuses
//!
//! A `JOIN`, a `GROUP BY`, a `HAVING`, a subquery source and a correlated
//! subquery inside an expression are **errors naming themselves**, not
//! best-effort answers. A provided table's rows are not in a database, so there
//! is nothing for a join to reach; a query that silently dropped the join would
//! answer a different question from the one it was asked, in somebody's view.
//!
//! Un-grouped aggregates *are* supported, and are not an extra: `count(*)` over
//! a filter is what the tables page shows beside every table's name, so a
//! provided table without it is one whose row count reads as an error.

use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    BinOp, CaseArm, ColRef, Expr, InSet, JsonStep, Nulls, OrderBy, OrderDir, Projection, Select,
    Source, UnOp, Value,
};
use serde_json::Value as Json;
use std::sync::Arc;

use crate::field::DataField;

/// One source row: its columns, **in the table's declaration order**.
///
/// A `Vec` rather than a map, because the order is the answer to `SELECT *`: a
/// database returns a table's columns in the order the table declares them, and
/// a provided table has to as well or its data grid puts the columns somewhere
/// different from its field list. Lookup is linear over a handful of columns,
/// which is what a hash would cost to build anyway.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValueRow(Vec<(String, Value)>);

impl ValueRow {
    /// The value of one column, or `None` when the table has no such column.
    pub fn get(&self, column: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, value)| value)
    }

    /// The columns and their values, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(name, value)| (name, value))
    }

    /// How many columns.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the row has no columns at all — a table whose provider could not
    /// say what it presents.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, Value)> for ValueRow {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> ValueRow {
        ValueRow(iter.into_iter().collect())
    }
}

/// Run `select` over `rows`, returning the rows it projects.
///
/// `table` names the table in every refusal, because "a provided table cannot be
/// joined" is only actionable if it says which one.
pub fn run_select_over(select: &Select, table: &str, rows: Vec<ValueRow>) -> Result<Vec<Row>> {
    refuse_unsupported(select, table)?;

    // WHERE, then ORDER BY, then OFFSET/LIMIT — SQL's own order, which is the
    // only one that gives the same answer as the database would.
    let mut kept = filter_rows(select.filter.as_ref(), table, rows)?;

    if !select.order.is_empty() {
        sort_rows(&mut kept, &select.order, table)?;
    }

    let offset = select.offset.unwrap_or(0) as usize;
    if offset > 0 {
        kept = kept.split_off(offset.min(kept.len()));
    }
    if let Some(limit) = select.limit {
        kept.truncate(limit as usize);
    }

    // An aggregate projection is a different shape of answer — one row over the
    // whole set — so it is decided once, for the projection list as a whole,
    // rather than per column. A list mixing `count(*)` with a bare column is
    // exactly what SQL refuses without a `GROUP BY`, and refusing it here keeps
    // the two engines answering the same questions.
    if select.columns.iter().any(projection_aggregates) {
        return aggregate_projection(select, table, &kept);
    }

    let mut out = Vec::with_capacity(kept.len());
    let mut columns: Option<Arc<Vec<String>>> = None;
    for row in &kept {
        let (names, values) = project(select, table, row)?;
        let columns = columns.get_or_insert_with(|| Arc::new(names)).clone();
        out.push(Row::new(columns, values)?);
    }
    Ok(out)
}

/// The rows one `WHERE` keeps, in the order they arrived.
///
/// Split out of [`run_select_over`] because the **write** path needs exactly
/// this and nothing else: v1's `updateRow`/`deleteRows` address rows by primary
/// key, so a provided-table write begins by asking which rows the statement's
/// filter matches (`crate::provider`). A `None` filter keeps everything, which
/// is what an unfiltered `UPDATE` or `DELETE` means.
pub fn filter_rows(
    filter: Option<&Expr>,
    table: &str,
    rows: Vec<ValueRow>,
) -> Result<Vec<ValueRow>> {
    let Some(filter) = filter else {
        return Ok(rows);
    };
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        if truthy(&eval(filter, &row, table)?) {
            kept.push(row);
        }
    }
    Ok(kept)
}

/// The values of one JSON object as the table's fields type them.
///
/// Only declared fields are read. A provider may answer far more than it
/// declared — `@saltcorn/rss` answers every element `rss-parser` found — and a
/// column nobody declared has no type, no header and no place in a `Row`, so it
/// is left where it is rather than being smuggled through as text.
pub fn value_row(fields: &[DataField], json: &Json) -> ValueRow {
    let object = json.as_object();
    fields
        .iter()
        .map(|field| {
            let raw = object.and_then(|o| o.get(&field.base.name));
            let value = raw.map_or(Value::Null, |json| coerce(json, field));
            (field.base.name.clone(), value)
        })
        .collect()
}

/// One JSON value as the [`Value`] a declared field's type calls for.
///
/// `sc-api`'s `json_to_value` is the same idea against a column an admin
/// declared, and this cannot call it (it is two layers up). The difference from
/// `sc_expr::value_from_json`, which this falls back to, is that a field's type
/// is known here: a date the provider answered as a string comes back as a
/// [`Value::Date`], so ordering by it sorts by date rather than by spelling.
fn coerce(json: &Json, field: &DataField) -> Value {
    use sc_types::BasicType;
    let Some(basic) = field.base.type_.as_basic() else {
        return sc_expr::value_from_json(json);
    };
    if json.is_null() {
        return Value::Null;
    }
    match basic {
        BasicType::Text => match json {
            Json::String(s) => Value::Text(s.clone()),
            other => Value::Text(other.to_string()),
        },
        BasicType::Int => match json {
            Json::Number(n) => n.as_i64().map_or_else(
                || n.as_f64().map_or(Value::Null, |f| Value::Int(f as i64)),
                Value::Int,
            ),
            Json::String(s) => s.trim().parse().map_or(Value::Null, Value::Int),
            Json::Bool(b) => Value::Int(i64::from(*b)),
            _ => Value::Null,
        },
        BasicType::Float => match json {
            Json::Number(n) => n.as_f64().map_or(Value::Null, Value::Float),
            Json::String(s) => s.trim().parse().map_or(Value::Null, Value::Float),
            _ => Value::Null,
        },
        BasicType::Bool => match json {
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Bool(n.as_f64().unwrap_or(0.0) != 0.0),
            Json::String(s) => Value::Bool(!matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "" | "false" | "0" | "no" | "off"
            )),
            _ => Value::Null,
        },
        BasicType::Json => Value::Json(json.clone()),
        BasicType::Date | BasicType::Timestamp => match json {
            Json::String(s) => parse_time(s),
            Json::Number(n) => n
                .as_i64()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map_or(Value::Null, Value::Timestamp),
            _ => Value::Null,
        },
        _ => sc_expr::value_from_json(json),
    }
}

/// A date or timestamp a provider wrote as a string, in the three spellings the
/// feeds and APIs behind a provider actually use: RFC 3339, RFC 2822 (which is
/// what an RSS `pubDate` is), and a bare date.
fn parse_time(s: &str) -> Value {
    let s = s.trim();
    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
        return Value::Timestamp(ts.with_timezone(&chrono::Utc));
    }
    if let Ok(ts) = chrono::DateTime::parse_from_rfc2822(s) {
        return Value::Timestamp(ts.with_timezone(&chrono::Utc));
    }
    if let Ok(date) = s.parse::<chrono::NaiveDate>() {
        return Value::Date(date);
    }
    // Not a time this knows how to read: the string itself, rather than a null
    // that would look like "the provider sent nothing".
    Value::Text(s.to_owned())
}

// --- what cannot be answered here --------------------------------------------

fn refuse_unsupported(select: &Select, table: &str) -> Result<()> {
    let refuse = |what: &str| -> Error {
        Error::invalid(format!(
            "`{table}` is served by a table provider, and this query needs {what}, which a \
             provided table cannot answer: its rows are not in a database, so there is nothing \
             for the query planner to reach"
        ))
    };
    if !select.joins.is_empty() {
        return Err(refuse("a JOIN"));
    }
    if !select.group.is_empty() {
        return Err(refuse("a GROUP BY"));
    }
    if select.having.is_some() {
        return Err(refuse("a HAVING clause"));
    }
    if matches!(select.from, Source::Subquery { .. }) {
        return Err(refuse("a subquery in its FROM clause"));
    }
    Ok(())
}

// --- projection ---------------------------------------------------------------

/// Whether a projection is an aggregate over the whole result.
fn projection_aggregates(projection: &Projection) -> bool {
    match projection {
        Projection::Wildcard { .. } => false,
        Projection::Expr { expr, .. } => contains_agg(expr),
    }
}

fn contains_agg(expr: &Expr) -> bool {
    match expr {
        Expr::Agg { .. } => true,
        Expr::Binary { l, r, .. } => contains_agg(l) || contains_agg(r),
        Expr::Unary { e, .. } => contains_agg(e),
        Expr::Func { args, .. } => args.iter().any(contains_agg),
        Expr::Json { target, .. } => contains_agg(target),
        Expr::Cast { expr, .. } => contains_agg(expr),
        Expr::Case {
            operand,
            arms,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_agg)
                || arms
                    .iter()
                    .any(|arm| contains_agg(&arm.when) || contains_agg(&arm.then))
                || else_result.as_deref().is_some_and(contains_agg)
        }
        _ => false,
    }
}

/// The output name and value of every projected column of one row.
fn project(select: &Select, table: &str, row: &ValueRow) -> Result<(Vec<String>, Vec<Value>)> {
    let mut names = Vec::new();
    let mut values = Vec::new();
    for (index, projection) in select.columns.iter().enumerate() {
        match projection {
            Projection::Wildcard { .. } => {
                for (name, value) in row.iter() {
                    names.push(name.clone());
                    values.push(value.clone());
                }
            }
            Projection::Expr { expr, alias } => {
                names.push(output_name(expr, alias.as_deref(), index));
                values.push(eval(expr, row, table)?);
            }
        }
    }
    Ok((names, values))
}

/// What a projected expression is called in the answer: its alias, else the
/// column it is, else a positional name — which is what a database does with an
/// unaliased expression, and what a caller reading by position expects.
fn output_name(expr: &Expr, alias: Option<&str>, index: usize) -> String {
    match (alias, expr) {
        (Some(alias), _) => alias.to_owned(),
        (None, Expr::Col(ColRef { column, .. })) => column.clone(),
        (None, Expr::Agg { func, .. }) => func.clone(),
        (None, Expr::Func { name, .. }) => name.clone(),
        (None, _) => format!("column{}", index + 1),
    }
}

/// The single row an un-grouped aggregate projection answers.
fn aggregate_projection(select: &Select, table: &str, rows: &[ValueRow]) -> Result<Vec<Row>> {
    let mut names = Vec::new();
    let mut values = Vec::new();
    for (index, projection) in select.columns.iter().enumerate() {
        let Projection::Expr { expr, alias } = projection else {
            return Err(Error::invalid(format!(
                "`{table}` is served by a table provider, and this query mixes `*` with an \
                 aggregate, which needs a GROUP BY a provided table cannot answer"
            )));
        };
        names.push(output_name(expr, alias.as_deref(), index));
        values.push(eval_agg(expr, table, rows)?);
    }
    Ok(vec![Row::new(Arc::new(names), values)?])
}

/// An expression in an aggregate projection: the aggregate itself, or something
/// built around one.
fn eval_agg(expr: &Expr, table: &str, rows: &[ValueRow]) -> Result<Value> {
    match expr {
        Expr::Agg {
            func,
            distinct,
            args,
        } => aggregate(func, *distinct, args, table, rows),
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Binary { op, l, r } => {
            let l = eval_agg(l, table, rows)?;
            let r = eval_agg(r, table, rows)?;
            binary(*op, &l, &r)
        }
        Expr::Unary { op, e } => {
            let v = eval_agg(e, table, rows)?;
            unary(*op, &v)
        }
        Expr::Cast { expr, .. } => eval_agg(expr, table, rows),
        other => Err(Error::invalid(format!(
            "`{table}` is served by a table provider, and `{}` cannot appear beside an aggregate \
             without a GROUP BY",
            describe(other)
        ))),
    }
}

/// One aggregate over the whole (already filtered) set.
fn aggregate(
    func: &str,
    distinct: bool,
    args: &[Expr],
    table: &str,
    rows: &[ValueRow],
) -> Result<Value> {
    // `count(*)` — the one with no argument, and the one the tables page asks.
    if args.is_empty() {
        if func.eq_ignore_ascii_case("count") {
            return Ok(Value::Int(rows.len() as i64));
        }
        return Err(Error::invalid(format!(
            "`{table}` is served by a table provider, and `{func}()` has no argument to aggregate"
        )));
    }
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        values.push(eval(&args[0], row, table)?);
    }
    if distinct {
        let mut seen: Vec<Value> = Vec::new();
        values.retain(|v| {
            if seen.iter().any(|s| s == v) {
                false
            } else {
                seen.push(v.clone());
                true
            }
        });
    }
    let present: Vec<&Value> = values.iter().filter(|v| !v.is_null()).collect();
    let numbers = || present.iter().filter_map(|v| number(v)).collect::<Vec<_>>();
    Ok(match func.to_ascii_lowercase().as_str() {
        "count" => Value::Int(present.len() as i64),
        "sum" => {
            let ns = numbers();
            if ns.is_empty() {
                Value::Null
            } else {
                number_value(ns.iter().sum())
            }
        }
        "avg" => {
            let ns = numbers();
            if ns.is_empty() {
                Value::Null
            } else {
                Value::Float(ns.iter().sum::<f64>() / ns.len() as f64)
            }
        }
        "min" | "max" => {
            let wanted = if func.eq_ignore_ascii_case("min") {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
            let mut best: Option<&Value> = None;
            for value in &present {
                best = Some(match best {
                    None => value,
                    Some(current) => match compare(value, current) {
                        Some(ord) if ord == wanted => value,
                        _ => current,
                    },
                });
            }
            best.cloned().unwrap_or(Value::Null)
        }
        "string_agg" => {
            let separator = match args.get(1) {
                Some(Expr::Lit(Value::Text(s))) => s.clone(),
                _ => ",".to_owned(),
            };
            Value::Text(
                present
                    .iter()
                    .map(|v| text(v))
                    .collect::<Vec<_>>()
                    .join(&separator),
            )
        }
        other => {
            return Err(Error::invalid(format!(
                "`{table}` is served by a table provider, which cannot compute the aggregate \
                 `{other}`"
            )));
        }
    })
}

/// An integral sum stays integral: a count of pennies that came back as `3.0`
/// would be a surprising answer to `sum(price)` on an integer column.
fn number_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::Int(n as i64)
    } else {
        Value::Float(n)
    }
}

// --- ordering -----------------------------------------------------------------

fn sort_rows(rows: &mut [ValueRow], order: &[OrderBy], table: &str) -> Result<()> {
    // Every key of every row is evaluated once, up front, rather than inside the
    // comparator: evaluation can fail, and a comparator has nowhere to report it.
    let mut keyed: Vec<(Vec<Value>, usize)> = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let mut keys = Vec::with_capacity(order.len());
        for key in order {
            keys.push(eval(&key.expr, row, table)?);
        }
        keyed.push((keys, index));
    }
    keyed.sort_by(|(a, ai), (b, bi)| {
        for (index, key) in order.iter().enumerate() {
            let ordering = order_values(&a[index], &b[index], key);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        // A stable tie-break on the original position, so a re-read of an
        // unchanged list gives the same page rather than a shuffled one.
        ai.cmp(bi)
    });
    let permutation: Vec<usize> = keyed.into_iter().map(|(_, index)| index).collect();
    apply_permutation(rows, &permutation);
    Ok(())
}

/// Reorder `rows` so that position *i* holds what was at `permutation[i]`.
fn apply_permutation(rows: &mut [ValueRow], permutation: &[usize]) {
    let mut taken: Vec<Option<ValueRow>> = rows.iter().map(|r| Some(r.clone())).collect();
    for (target, source) in permutation.iter().enumerate() {
        if let Some(row) = taken[*source].take() {
            rows[target] = row;
        }
    }
}

fn order_values(a: &Value, b: &Value, key: &OrderBy) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Null placement is the dialect's default when the key does not say, and
    // Postgres's default is NULLS LAST for ASC and NULLS FIRST for DESC — which
    // is the same rule as "nulls are the largest value", so it is written once.
    let nulls = key.nulls.unwrap_or(match key.dir {
        OrderDir::Asc => Nulls::Last,
        OrderDir::Desc => Nulls::First,
    });
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => {
            return match nulls {
                Nulls::First => Ordering::Less,
                Nulls::Last => Ordering::Greater,
            };
        }
        (false, true) => {
            return match nulls {
                Nulls::First => Ordering::Greater,
                Nulls::Last => Ordering::Less,
            };
        }
        (false, false) => {}
    }
    let ordering = compare(a, b).unwrap_or(Ordering::Equal);
    match key.dir {
        OrderDir::Asc => ordering,
        OrderDir::Desc => ordering.reverse(),
    }
}

// --- expression evaluation ------------------------------------------------------

/// Evaluate one expression against one row.
fn eval(expr: &Expr, row: &ValueRow, table: &str) -> Result<Value> {
    match expr {
        Expr::Col(ColRef { column, .. }) => Ok(row.get(column).cloned().unwrap_or(Value::Null)),
        Expr::Lit(value) => Ok(value.clone()),
        Expr::Binary { op, l, r } => {
            // `AND`/`OR` short-circuit, because SQL's do: `x IS NOT NULL AND
            // length(x) > 0` must not evaluate the right half when the left is
            // false.
            match op {
                BinOp::And => {
                    let l = eval(l, row, table)?;
                    if !truthy(&l) {
                        return Ok(Value::Bool(false));
                    }
                    Ok(Value::Bool(truthy(&eval(r, row, table)?)))
                }
                BinOp::Or => {
                    let l = eval(l, row, table)?;
                    if truthy(&l) {
                        return Ok(Value::Bool(true));
                    }
                    Ok(Value::Bool(truthy(&eval(r, row, table)?)))
                }
                _ => {
                    let l = eval(l, row, table)?;
                    let r = eval(r, row, table)?;
                    binary(*op, &l, &r)
                }
            }
        }
        Expr::Unary { op, e } => {
            let value = eval(e, row, table)?;
            unary(*op, &value)
        }
        Expr::Func { name, args } => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval(arg, row, table)?);
            }
            func(name, &values, table)
        }
        Expr::In { e, set } => {
            let value = eval(e, row, table)?;
            match set {
                InSet::List(list) => {
                    for candidate in list {
                        let candidate = eval(candidate, row, table)?;
                        if equal(&value, &candidate) {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Ok(Value::Bool(false))
                }
                InSet::Subquery(_) => Err(unsupported(table, "an IN over a subquery")),
            }
        }
        Expr::Json { target, path } => {
            let value = eval(target, row, table)?;
            Ok(json_path(&value, path))
        }
        Expr::Case {
            operand,
            arms,
            else_result,
        } => case(operand.as_deref(), arms, else_result.as_deref(), row, table),
        Expr::Cast { expr, .. } => eval(expr, row, table),
        Expr::Agg { .. } => Err(unsupported(
            table,
            "an aggregate outside the projection list",
        )),
        Expr::Window { .. } => Err(unsupported(table, "a window function")),
        Expr::Subquery(_) => Err(unsupported(table, "a subquery inside an expression")),
        Expr::TextMatch(_) | Expr::TextRank(_) => Err(unsupported(table, "a full-text search")),
        Expr::Param(index) => Err(unsupported(
            table,
            &format!("the unbound parameter ${}", index + 1),
        )),
    }
}

fn unsupported(table: &str, what: &str) -> Error {
    Error::invalid(format!(
        "`{table}` is served by a table provider, and this query uses {what}, which a provided \
         table cannot answer: its rows are not in a database"
    ))
}

fn describe(expr: &Expr) -> &'static str {
    match expr {
        Expr::Col(_) => "a column",
        Expr::Lit(_) => "a literal",
        Expr::Subquery(_) => "a subquery",
        Expr::Window { .. } => "a window function",
        _ => "that expression",
    }
}

fn case(
    operand: Option<&Expr>,
    arms: &[CaseArm],
    else_result: Option<&Expr>,
    row: &ValueRow,
    table: &str,
) -> Result<Value> {
    let subject = match operand {
        Some(expr) => Some(eval(expr, row, table)?),
        None => None,
    };
    for arm in arms {
        let when = eval(&arm.when, row, table)?;
        let hit = match &subject {
            Some(subject) => equal(subject, &when),
            None => truthy(&when),
        };
        if hit {
            return eval(&arm.then, row, table);
        }
    }
    match else_result {
        Some(expr) => eval(expr, row, table),
        None => Ok(Value::Null),
    }
}

fn json_path(value: &Value, path: &[JsonStep]) -> Value {
    let mut current = match value {
        Value::Json(json) => json.clone(),
        Value::Null => return Value::Null,
        other => Json::String(text(other)),
    };
    for step in path {
        current = match (step, &current) {
            (JsonStep::Field(key), Json::Object(map)) => {
                map.get(key).cloned().unwrap_or(Json::Null)
            }
            (JsonStep::Index(i), Json::Array(items)) => usize::try_from(*i)
                .ok()
                .and_then(|i| items.get(i).cloned())
                .unwrap_or(Json::Null),
            _ => Json::Null,
        };
    }
    sc_expr::value_from_json(&current)
}

fn binary(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    use std::cmp::Ordering;
    Ok(match op {
        // Three-valued logic, as SQL has it: a comparison with a null is null,
        // and a null in a `WHERE` is not true. `truthy` is what turns that into
        // "the row is dropped", in one place.
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            if l.is_null() || r.is_null() =>
        {
            Value::Null
        }
        BinOp::Eq => Value::Bool(equal(l, r)),
        BinOp::Ne => Value::Bool(!equal(l, r)),
        BinOp::Lt => Value::Bool(compare(l, r) == Some(Ordering::Less)),
        BinOp::Le => Value::Bool(matches!(
            compare(l, r),
            Some(Ordering::Less | Ordering::Equal)
        )),
        BinOp::Gt => Value::Bool(compare(l, r) == Some(Ordering::Greater)),
        BinOp::Ge => Value::Bool(matches!(
            compare(l, r),
            Some(Ordering::Greater | Ordering::Equal)
        )),
        // Handled by the caller, which short-circuits; reached only if an
        // aggregate expression contains one.
        BinOp::And => Value::Bool(truthy(l) && truthy(r)),
        BinOp::Or => Value::Bool(truthy(l) || truthy(r)),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            match (number(l), number(r)) {
                (Some(a), Some(b)) => match op {
                    BinOp::Add => number_value(a + b),
                    BinOp::Sub => number_value(a - b),
                    BinOp::Mul => number_value(a * b),
                    BinOp::Div if b == 0.0 => Value::Null,
                    BinOp::Div => number_value(a / b),
                    BinOp::Mod if b == 0.0 => Value::Null,
                    _ => number_value(a % b),
                },
                _ => Value::Null,
            }
        }
        BinOp::Like | BinOp::ILike => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Bool(like(&text(l), &text(r), op == BinOp::ILike))
            }
        }
        BinOp::Concat => {
            if l.is_null() && r.is_null() {
                Value::Null
            } else {
                Value::Text(format!("{}{}", text(l), text(r)))
            }
        }
        BinOp::IsNotDistinct => Value::Bool(match (l.is_null(), r.is_null()) {
            (true, true) => true,
            (false, false) => equal(l, r),
            _ => false,
        }),
        BinOp::IsDistinct => Value::Bool(match (l.is_null(), r.is_null()) {
            (true, true) => false,
            (false, false) => !equal(l, r),
            _ => true,
        }),
    })
}

fn unary(op: UnOp, value: &Value) -> Result<Value> {
    Ok(match op {
        UnOp::Not => {
            if value.is_null() {
                Value::Null
            } else {
                Value::Bool(!truthy(value))
            }
        }
        UnOp::Neg => number(value).map_or(Value::Null, |n| number_value(-n)),
        UnOp::IsNull => Value::Bool(value.is_null()),
        UnOp::IsNotNull => Value::Bool(!value.is_null()),
    })
}

/// The scalar functions a read of a provided table can actually contain.
///
/// Deliberately a short list rather than an open one: every name here is a
/// function this system's own query builders emit, and a name that is not is an
/// error saying so — a provider that answered `lower(x)` as `x` would be
/// answering a different question.
fn func(name: &str, args: &[Value], table: &str) -> Result<Value> {
    let first = || args.first().cloned().unwrap_or(Value::Null);
    Ok(match name.to_ascii_lowercase().as_str() {
        "lower" => Value::Text(text(&first()).to_lowercase()),
        "upper" => Value::Text(text(&first()).to_uppercase()),
        "trim" => Value::Text(text(&first()).trim().to_owned()),
        "length" => Value::Int(text(&first()).chars().count() as i64),
        "abs" => number(&first()).map_or(Value::Null, |n| number_value(n.abs())),
        "coalesce" => args
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null),
        "nullif" => match (args.first(), args.get(1)) {
            (Some(a), Some(b)) if equal(a, b) => Value::Null,
            (Some(a), _) => a.clone(),
            _ => Value::Null,
        },
        other => {
            return Err(unsupported(table, &format!("the SQL function `{other}`")));
        }
    })
}

/// SQL `LIKE`, with `%` and `_` and no other metacharacter.
fn like(haystack: &str, pattern: &str, insensitive: bool) -> bool {
    let (haystack, pattern) = if insensitive {
        (haystack.to_lowercase(), pattern.to_lowercase())
    } else {
        (haystack.to_owned(), pattern.to_owned())
    };
    let h: Vec<char> = haystack.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    // The classic linear wildcard match: one pass, with a remembered star
    // position to backtrack to, so a pattern full of `%` cannot go quadratic.
    let (mut hi, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while hi < h.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == h[hi]) {
            hi += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi);
            mark = hi;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            hi = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether a value counts as true in a `WHERE` — which is what makes SQL's
/// three-valued logic come out right: `NULL` is not true, so the row is dropped.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::Text(s) => !s.is_empty() && s != "false" && s != "f",
        _ => true,
    }
}

/// A value as a number, when it is one.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Decimal(d) => (*d).try_into().ok(),
        Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A value as the text a comparison or a concatenation reads it as.
fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Text(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Uuid(u) => u.to_string(),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Timestamp(ts) => ts.to_rfc3339(),
        Value::Json(json) => json
            .as_str()
            .map_or_else(|| json.to_string(), str::to_owned),
        Value::Bytes(_) => String::new(),
    }
}

/// Equality across the type boundaries a provider produces.
///
/// A module answers JSON, so a key that is `4` in one row and `"4"` in the next
/// is an ordinary thing for a provider to do — and a filter that matched the
/// first and not the second would look like data loss. Numbers therefore compare
/// as numbers whichever side spells them as text, and everything else compares
/// as its own text.
fn equal(a: &Value, b: &Value) -> bool {
    compare(a, b) == Some(std::cmp::Ordering::Equal)
}

/// The ordering of two values, or `None` when they are not comparable.
fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    if let (Some(x), Some(y)) = (number(a), number(b))
        && !matches!(a, Value::Text(_) if matches!(b, Value::Text(_)))
    {
        return x.partial_cmp(&y);
    }
    match (a, b) {
        (Value::Timestamp(x), Value::Timestamp(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::Time(x), Value::Time(y)) => Some(x.cmp(y)),
        _ => Some(text(a).cmp(&text(b))),
    }
}

// --- pushing the query down to the provider -----------------------------------

/// The v1 `where` object and `options` object a `Select` translates into — the
/// **hint** a provider is given before the query is applied to its answer.
///
/// v1's providers take exactly these two arguments (`getRows(where, opts)`), so
/// this is not a vocabulary invented here; it is the one every existing plugin
/// already reads. `@saltcorn/postgres-tables` turns it into SQL against a remote
/// database, which is the difference between fetching a page and fetching a
/// table.
///
/// **A hint is all it is, and the translation is all-or-nothing for that
/// reason.** If any part of the filter cannot be said in v1's vocabulary the
/// whole `where` is `{}` — a *partial* filter would be a lie, because a provider
/// that honoured it would return the rows matching half the condition and the
/// caller has no way to tell that from a provider that ignored it. The ordering
/// and the bound travel only when the filter did, on the same grounds: a `LIMIT`
/// applied before a filter that was not pushed is the wrong ten rows.
///
/// [`run_select_over`] runs regardless, so a provider that ignores all of this
/// still gets the right answer, and one that honours it does the same work
/// twice on a smaller list.
pub fn pushdown(select: &Select) -> (Json, Json) {
    let empty = || {
        (
            Json::Object(serde_json::Map::new()),
            Json::Object(serde_json::Map::new()),
        )
    };
    if !select.joins.is_empty() || !select.group.is_empty() || select.having.is_some() {
        return empty();
    }
    let filter = match &select.filter {
        None => serde_json::Map::new(),
        Some(expr) => match where_object(expr) {
            Some(map) => map,
            None => return empty(),
        },
    };

    let mut options = serde_json::Map::new();
    // v1's ordering vocabulary is one column and a direction — `orderBy` and
    // `orderDesc` — so a multi-key order is not expressible and travels as
    // nothing rather than as its first key.
    if let [
        OrderBy {
            expr: Expr::Col(col),
            dir,
            ..
        },
    ] = select.order.as_slice()
    {
        options.insert("orderBy".into(), Json::String(col.column.clone()));
        if *dir == OrderDir::Desc {
            options.insert("orderDesc".into(), Json::Bool(true));
        }
    } else if !select.order.is_empty() {
        return (Json::Object(filter), Json::Object(options));
    }
    if let Some(limit) = select.limit {
        options.insert("limit".into(), Json::Number(limit.into()));
    }
    if let Some(offset) = select.offset {
        options.insert("offset".into(), Json::Number(offset.into()));
    }
    (Json::Object(filter), Json::Object(options))
}

/// One expression as v1's `where` object, or `None` when it cannot be said in
/// it.
///
/// v1's vocabulary, from `json_list_to_external_table`'s own matcher: `{ col:
/// value }`, `{ col: { gt } }` / `{ lt }` (with `equal` for the inclusive
/// forms), `{ col: { in: [...] } }`, `{ col: { ilike } }`, and `{ or: [...] }`.
/// A conjunction is the object itself, which is why `AND` merges and a repeated
/// column is refused rather than silently overwritten.
fn where_object(expr: &Expr) -> Option<serde_json::Map<String, Json>> {
    match expr {
        Expr::Binary {
            op: BinOp::And,
            l,
            r,
        } => {
            let left = where_object(l)?;
            let mut right = where_object(r)?;
            let mut merged = left;
            for (key, value) in right.iter_mut() {
                if merged.contains_key(key) {
                    // Two conditions on one column: v1's object has one slot per
                    // key, so this cannot be said. (`{a: [x, y]}` means "both" in
                    // v1's matcher, but only for scalars, and guessing which
                    // shape was meant is exactly the kind of partial translation
                    // this refuses.)
                    return None;
                }
                merged.insert(key.clone(), value.take());
            }
            Some(merged)
        }
        Expr::Binary {
            op: BinOp::Or,
            l,
            r,
        } => {
            let left = where_object(l)?;
            let right = where_object(r)?;
            let mut map = serde_json::Map::new();
            map.insert(
                "or".into(),
                Json::Array(vec![Json::Object(left), Json::Object(right)]),
            );
            Some(map)
        }
        Expr::Binary { op, l, r } => {
            let (column, value) = column_and_literal(l, r)?;
            let condition = match op {
                BinOp::Eq | BinOp::IsNotDistinct => value,
                BinOp::Lt => json_pair("lt", value, false),
                BinOp::Le => json_pair("lt", value, true),
                BinOp::Gt => json_pair("gt", value, false),
                BinOp::Ge => json_pair("gt", value, true),
                BinOp::ILike | BinOp::Like => {
                    // v1's `ilike` is a *containment* test on the bare term, not
                    // a pattern: only the `%term%` shape means the same thing.
                    let text = value.as_str()?;
                    let inner = text.strip_prefix('%')?.strip_suffix('%')?;
                    if inner.contains('%') || inner.contains('_') {
                        return None;
                    }
                    let mut map = serde_json::Map::new();
                    map.insert("ilike".into(), Json::String(inner.to_owned()));
                    Json::Object(map)
                }
                _ => return None,
            };
            let mut map = serde_json::Map::new();
            map.insert(column, condition);
            Some(map)
        }
        Expr::In { e, set } => {
            let Expr::Col(col) = e.as_ref() else {
                return None;
            };
            let InSet::List(list) = set else { return None };
            let mut values = Vec::with_capacity(list.len());
            for item in list {
                let Expr::Lit(value) = item else { return None };
                values.push(sc_expr::value_to_json(value));
            }
            let mut inner = serde_json::Map::new();
            inner.insert("in".into(), Json::Array(values));
            let mut map = serde_json::Map::new();
            map.insert(col.column.clone(), Json::Object(inner));
            Some(map)
        }
        _ => None,
    }
}

/// v1's `{ gt: n }` / `{ gt: n, equal: true }`.
fn json_pair(key: &str, value: Json, equal: bool) -> Json {
    let mut map = serde_json::Map::new();
    map.insert(key.into(), value);
    if equal {
        map.insert("equal".into(), Json::Bool(true));
    }
    Json::Object(map)
}

/// A `column op literal` pair, whichever way round the caller wrote it.
fn column_and_literal(l: &Expr, r: &Expr) -> Option<(String, Json)> {
    match (l, r) {
        (Expr::Col(col), Expr::Lit(value)) | (Expr::Lit(value), Expr::Col(col)) => {
            Some((col.column.clone(), sc_expr::value_to_json(value)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_query::{Projection, Source};
    use sc_types::{BasicType, TypeRef};
    use serde_json::json;

    fn fields() -> Vec<DataField> {
        vec![
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            DataField::plain("votes", TypeRef::Basic(BasicType::Int)),
        ]
    }

    fn rows() -> Vec<ValueRow> {
        let fields = fields();
        [
            json!({ "title": "beta", "votes": 2, "extra": "ignored" }),
            json!({ "title": "alpha", "votes": "10" }),
            json!({ "title": "gamma" }),
        ]
        .iter()
        .map(|json| value_row(&fields, json))
        .collect()
    }

    fn select() -> Select {
        Select::from(Source::table("feed"))
    }

    fn run(select: &Select) -> Vec<Row> {
        run_select_over(select, "feed", rows()).unwrap()
    }

    #[test]
    fn only_declared_fields_are_read_and_they_are_typed_by_the_field() {
        let row = &rows()[0];
        // The provider answered a third key; it is not a column of this table,
        // so it is not in the row at all.
        assert_eq!(row.len(), 2);
        assert_eq!(row.get("title"), Some(&Value::Text("beta".into())));
        // And a number the provider spelled as text is an integer, because the
        // field says so.
        assert_eq!(rows()[1].get("votes"), Some(&Value::Int(10)));
        // A key the provider omitted is null, never missing.
        assert_eq!(rows()[2].get("votes"), Some(&Value::Null));
    }

    #[test]
    fn a_wildcard_select_answers_every_declared_column() {
        let out = run(&select());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].columns(), ["title", "votes"]);
        assert_eq!(out[0].get("title"), Some(&Value::Text("beta".into())));
    }

    #[test]
    fn a_filter_drops_rows_and_a_null_is_not_true() {
        let mut select = select();
        select.filter = Some(Expr::binary(
            BinOp::Gt,
            Expr::col("votes"),
            Expr::lit(5_i64),
        ));
        let out = run(&select);
        // `gamma` has no votes at all: `NULL > 5` is null, and a null in a WHERE
        // drops the row rather than keeping it.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("title"), Some(&Value::Text("alpha".into())));
    }

    #[test]
    fn ordering_sorts_by_the_typed_value_and_places_nulls_as_postgres_does() {
        let mut select = select();
        select.order = vec![OrderBy::asc(Expr::col("votes"))];
        let out = run(&select);
        let titles: Vec<_> = out.iter().map(|r| r.get("title").cloned()).collect();
        // 2, then 10 — numerically, not "10" before "2" — and the null last.
        assert_eq!(
            titles,
            vec![
                Some(Value::Text("beta".into())),
                Some(Value::Text("alpha".into())),
                Some(Value::Text("gamma".into())),
            ]
        );
        select.order = vec![OrderBy::desc(Expr::col("votes"))];
        let out = run(&select);
        assert_eq!(out[0].get("title"), Some(&Value::Text("gamma".into())));
    }

    #[test]
    fn limit_and_offset_page_the_answer() {
        let mut select = select();
        select.order = vec![OrderBy::asc(Expr::col("title"))];
        select.limit = Some(1);
        select.offset = Some(1);
        let out = run(&select);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("title"), Some(&Value::Text("beta".into())));
    }

    #[test]
    fn count_star_is_answered_because_the_tables_page_asks_it() {
        let mut select = select();
        select.columns = vec![Projection::expr_as(
            Expr::Agg {
                func: "count".into(),
                distinct: false,
                args: Vec::new(),
            },
            "_fd_count",
        )];
        let out = run(&select);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("_fd_count"), Some(&Value::Int(3)));

        // And with a filter, over what the filter left.
        select.filter = Some(Expr::binary(
            BinOp::Eq,
            Expr::col("title"),
            Expr::lit("beta"),
        ));
        let out = run(&select);
        assert_eq!(out[0].get("_fd_count"), Some(&Value::Int(1)));
    }

    #[test]
    fn sum_min_and_max_read_the_typed_values() {
        let agg = |func: &str| {
            let mut select = select();
            select.columns = vec![Projection::expr_as(
                Expr::Agg {
                    func: func.into(),
                    distinct: false,
                    args: vec![Expr::col("votes")],
                },
                "v",
            )];
            run(&select)[0].get("v").cloned().unwrap()
        };
        assert_eq!(agg("sum"), Value::Int(12));
        assert_eq!(agg("min"), Value::Int(2));
        assert_eq!(agg("max"), Value::Int(10));
        // `count(column)` counts the rows where it is not null, as SQL's does.
        assert_eq!(agg("count"), Value::Int(2));
    }

    #[test]
    fn a_join_is_refused_by_name_rather_than_answered_wrongly() {
        let mut select = select();
        select.joins = vec![sc_query::Join {
            kind: sc_query::JoinKind::Inner,
            source: Source::table("authors"),
            on: None,
        }];
        let err = run_select_over(&select, "feed", rows())
            .unwrap_err()
            .to_string();
        assert!(err.contains("JOIN"), "{err}");
        assert!(err.contains("feed"), "{err}");
        assert!(err.contains("table provider"), "{err}");
    }

    #[test]
    fn a_group_by_and_a_subquery_are_refused_too() {
        let mut grouped = select();
        grouped.group = vec![Expr::col("title")];
        assert!(run_select_over(&grouped, "feed", rows()).is_err());

        let mut correlated = select();
        correlated.filter = Some(Expr::binary(
            BinOp::Eq,
            Expr::col("title"),
            Expr::Subquery(Box::new(Select::from(Source::table("x")))),
        ));
        let err = run_select_over(&correlated, "feed", rows())
            .unwrap_err()
            .to_string();
        assert!(err.contains("subquery"), "{err}");
    }

    #[test]
    fn like_matches_the_way_sql_does() {
        assert!(like("hello world", "hello%", false));
        assert!(like("hello world", "%world", false));
        assert!(like("hello world", "h_llo%", false));
        assert!(!like("hello world", "H%", false));
        assert!(like("hello world", "H%", true));
        assert!(like("abc", "%", false));
        assert!(!like("abc", "ab", false));
    }

    #[test]
    fn a_filter_v1_can_say_is_pushed_down_whole() {
        let mut select = select();
        select.filter = Some(
            Expr::binary(BinOp::Eq, Expr::col("title"), Expr::lit("beta")).and(Expr::binary(
                BinOp::Ge,
                Expr::col("votes"),
                Expr::lit(2_i64),
            )),
        );
        select.order = vec![OrderBy::desc(Expr::col("votes"))];
        select.limit = Some(5);
        let (filter, options) = pushdown(&select);
        assert_eq!(
            filter,
            json!({ "title": "beta", "votes": { "gt": 2, "equal": true } })
        );
        assert_eq!(
            options,
            json!({ "orderBy": "votes", "orderDesc": true, "limit": 5 })
        );
    }

    #[test]
    fn a_filter_v1_cannot_say_is_pushed_down_as_nothing_rather_than_as_half() {
        // A provider that honoured half a condition would answer the rows
        // matching half a condition, and nothing downstream could tell that from
        // a provider that ignored the hint. So it is all or nothing — and the
        // ordering and the bound go with it, because a LIMIT applied before a
        // filter that was not pushed is the wrong ten rows.
        let mut select = select();
        select.filter = Some(Expr::binary(
            BinOp::Eq,
            Expr::Func {
                name: "lower".into(),
                args: vec![Expr::col("title")],
            },
            Expr::lit("beta"),
        ));
        select.limit = Some(5);
        let (filter, options) = pushdown(&select);
        assert_eq!(filter, json!({}));
        assert_eq!(options, json!({}));
        // But the answer is still right, because the query runs here regardless.
        let out = run(&select);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn an_in_list_and_an_or_reach_v1_in_v1s_own_vocabulary() {
        let mut among = select();
        among.filter = Some(Expr::In {
            e: Box::new(Expr::col("title")),
            set: InSet::List(vec![Expr::lit("beta"), Expr::lit("gamma")]),
        });
        assert_eq!(
            pushdown(&among).0,
            json!({ "title": { "in": ["beta", "gamma"] } })
        );

        let mut either = select();
        either.filter = Some(
            Expr::binary(BinOp::Eq, Expr::col("title"), Expr::lit("beta")).or(Expr::binary(
                BinOp::Eq,
                Expr::col("title"),
                Expr::lit("gamma"),
            )),
        );
        assert_eq!(
            pushdown(&either).0,
            json!({ "or": [{ "title": "beta" }, { "title": "gamma" }] })
        );
    }

    #[test]
    fn a_date_a_provider_wrote_as_a_string_sorts_as_a_date() {
        let fields = vec![DataField::plain("at", TypeRef::Basic(BasicType::Timestamp))];
        // An RSS `pubDate` is RFC 2822, which is not what anything else in this
        // system writes — and comparing the two as text would put "Mon" before
        // "Sun".
        let rows: Vec<ValueRow> = [
            json!({ "at": "Tue, 03 Jun 2025 09:00:00 GMT" }),
            json!({ "at": "Mon, 02 Jun 2025 09:00:00 GMT" }),
        ]
        .iter()
        .map(|json| value_row(&fields, json))
        .collect();
        let mut select = select();
        select.order = vec![OrderBy::asc(Expr::col("at"))];
        let out = run_select_over(&select, "feed", rows).unwrap();
        assert_eq!(
            out[0].get("at").map(text),
            Some("2025-06-02T09:00:00+00:00".to_owned())
        );
    }
}
