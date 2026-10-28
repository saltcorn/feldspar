//! The `data.*` step types (docs/CHECKLISTS.md §2, TODO C1.8).
//!
//! Every one of them goes through the row layer **as `run_as`** —
//! `sc_api::read_rows_as` and its siblings, the functions an agent's tools use
//! — so a checklist tests permissions as well as data: the ownership formula,
//! the role floors and a table's RLS policies all apply exactly as they do to
//! the application's own REST API.
//!
//! `data.expect_denied` writes nothing at all: it asks the row layer for its
//! verdict (`row_insertable_as` and siblings) instead of attempting the write,
//! so a check that passes leaves no trace and one that fails has not just
//! deleted the row it was asking about.

use sc_api::rows::{self, Executor, RowQuery};
use sc_catalog::Table;
use sc_error::{Error, Repr, Result};
use sc_types::{BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use crate::registry::{InsertedRow, Needs, StepContext, StepOutcome, StepType};

/// The most rows `data.expect_rows` reads.
const MAX_ROWS: u64 = 1000;

fn table_field() -> FormField {
    FormField::new("table", BasicType::Text)
        .label("Table")
        .required()
}

fn id_field() -> FormField {
    FormField::new("id", BasicType::Text)
        .label("Row id")
        .required()
        .sublabel("The row's primary key, e.g. `{{ order.id }}`")
}

fn row_field(label: &str) -> FormField {
    FormField::new("row", BasicType::Json).label(label)
}

fn where_field() -> FormField {
    FormField::new("where", BasicType::Json)
        .label("Filter")
        .sublabel("A filter object: {\"status\": \"open\", \"total\": {\"gte\": 10}}")
}

/// The table a step names.
fn table(cx: &StepContext<'_>, params: &Json) -> Result<Table> {
    let name = text(params, "table")?;
    cx.catalog
        .get(&name)?
        .ok_or_else(|| Error::not_found(format!("there is no table `{name}`")))
}

/// A required string parameter.
fn text(params: &Json, key: &str) -> Result<String> {
    match params.get(key) {
        Some(Json::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        Some(Json::Number(n)) => Ok(n.to_string()),
        _ => Err(Error::invalid(format!("`{key}` is required"))),
    }
}

/// An object parameter, empty when absent.
fn object(params: &Json, key: &str) -> Result<Json> {
    match params.get(key) {
        None | Some(Json::Null) => Ok(Json::Object(Map::new())),
        Some(Json::Object(map)) => Ok(Json::Object(map.clone())),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be an object, got {other}"
        ))),
    }
}

/// The key a row is addressed by, out of the row the row layer returned.
fn key_of(table: &Table, row: &Json) -> Result<String> {
    let pk = rows::single_pk(table)?;
    match row.get(&pk) {
        Some(Json::String(s)) => Ok(s.clone()),
        Some(Json::Null) | None => Err(Error::msg(format!(
            "the row written to `{}` came back without its `{pk}`",
            table.name
        ))),
        Some(other) => Ok(other.to_string()),
    }
}

/// Whether the row layer refused, rather than failed: an authorization error,
/// or the not-found a withheld row is indistinguishable from.
fn is_denial(e: &Error) -> bool {
    matches!(e.repr(), Repr::Auth(_) | Repr::NotFound(_))
}

/// The rows `run_as` can read that `filter` selects.
async fn read(
    cx: &StepContext<'_>,
    table: &Table,
    filter: Option<sc_query::Expr>,
    limit: u64,
) -> Result<Vec<Json>> {
    let query = RowQuery::new().where_(filter).limit(limit);
    let found = sc_api::read_rows_as(
        cx.catalog,
        table,
        &query,
        cx.role(),
        cx.run_as,
        cx.evaluator()?,
    )
    .await?;
    Ok(found.as_array().cloned().unwrap_or_default())
}

/// The one row with this key that `run_as` can read.
async fn read_one(cx: &StepContext<'_>, table: &Table, id: &str) -> Result<Option<Json>> {
    let pk = rows::single_pk(table)?;
    let filter = rows::pk_filter(table, &pk, id)?;
    Ok(read(cx, table, Some(filter), 1).await?.into_iter().next())
}

/// Whether an expected value matches an actual one: equal, or the same
/// number, or the same text once a number or a boolean is written out — a
/// template renders to text, and a date comes back from the row layer as one.
pub(crate) fn matches(expected: &Json, actual: &Json) -> bool {
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64(),
        (Json::String(s), Json::Number(_) | Json::Bool(_)) => s.as_str() == actual.to_string(),
        (Json::Number(_) | Json::Bool(_), Json::String(s)) => s.as_str() == expected.to_string(),
        (Json::Object(want), Json::Object(have)) => want
            .iter()
            .all(|(k, v)| have.get(k).is_some_and(|h| matches(v, h))),
        _ => false,
    }
}

/// `data.insert`: write a row as `run_as`. It is deleted again when the run
/// ends.
pub struct Insert;

#[async_trait::async_trait]
impl StepType for Insert {
    fn name(&self) -> &str {
        "data.insert"
    }

    fn description(&self) -> &str {
        "Insert a row into a table as the checklist's user, to set up what a later step needs. \
         Its value is the row as written (with its id), and it is deleted again when the run \
         ends."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![table_field(), row_field("Row").required()]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let table = table(cx, params)?;
            let row = object(params, "row")?;
            let written = match sc_api::insert_row_as(
                cx.catalog,
                &table,
                &row,
                cx.role(),
                cx.run_as,
                cx.evaluator()?,
                &[],
                &Executor::Pooled,
            )
            .await
            {
                Ok(written) => written,
                Err(e) if is_denial(&e) => {
                    return Ok(StepOutcome::fail(format!(
                        "inserting into `{}` was refused: {e}",
                        table.name
                    )));
                }
                Err(e) => return Err(e),
            };
            let key = key_of(&table, &written)?;
            cx.inserted(InsertedRow {
                table: table.name.clone(),
                key: key.clone(),
            });
            Ok(StepOutcome::pass(format!("inserted row {key} into `{}`", table.name))
                .with_value(written))
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

/// `data.update`: change a row as `run_as`.
pub struct Update;

#[async_trait::async_trait]
impl StepType for Update {
    fn name(&self) -> &str {
        "data.update"
    }

    fn description(&self) -> &str {
        "Update one row of a table, by its id, as the checklist's user. Fails when the update is \
         refused. Its value is the row as it now is."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![table_field(), id_field(), row_field("Changes").required()]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let table = table(cx, params)?;
            let id = text(params, "id")?;
            let changes = object(params, "row")?;
            match sc_api::update_row_as(
                cx.catalog,
                &table,
                &id,
                &changes,
                cx.role(),
                cx.run_as,
                cx.evaluator()?,
                &[],
                &Executor::Pooled,
            )
            .await
            {
                Ok(row) => Ok(StepOutcome::pass(format!(
                    "updated row {id} of `{}`",
                    table.name
                ))
                .with_value(row)),
                Err(e) if is_denial(&e) => Ok(StepOutcome::fail(format!(
                    "updating row {id} of `{}` was refused: {e}",
                    table.name
                ))),
                Err(e) => Err(e),
            }
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

/// `data.delete`: delete a row as `run_as`.
pub struct Delete;

#[async_trait::async_trait]
impl StepType for Delete {
    fn name(&self) -> &str {
        "data.delete"
    }

    fn description(&self) -> &str {
        "Delete one row of a table, by its id, as the checklist's user. Fails when the delete is \
         refused."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![table_field(), id_field()]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let table = table(cx, params)?;
            let id = text(params, "id")?;
            match sc_api::delete_row_as(
                cx.catalog,
                &table,
                &id,
                cx.role(),
                cx.run_as,
                cx.evaluator()?,
                &[],
                &Executor::Pooled,
            )
            .await
            {
                Ok(_) => Ok(StepOutcome::pass(format!(
                    "deleted row {id} of `{}`",
                    table.name
                ))),
                Err(e) if is_denial(&e) => Ok(StepOutcome::fail(format!(
                    "deleting row {id} of `{}` was refused: {e}",
                    table.name
                ))),
                Err(e) => Err(e),
            }
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

/// `data.expect_rows`: the rows `run_as` can read that a filter selects —
/// how many, and what they hold.
pub struct ExpectRows;

#[async_trait::async_trait]
impl StepType for ExpectRows {
    fn name(&self) -> &str {
        "data.expect_rows"
    }

    fn description(&self) -> &str {
        "Assert on the rows of a table the checklist's user can read that a filter selects: \
         exactly `count` of them (at least one when `count` is not given), each holding the \
         values in `fields`. Its value is the rows."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            table_field(),
            where_field(),
            FormField::new("count", BasicType::Int).label("Exactly this many rows"),
            FormField::new("fields", BasicType::Json)
                .label("Every row has these values")
                .sublabel("An object of field values, e.g. {\"status\": \"paid\"}"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let table = table(cx, params)?;
            let filter = sc_api::filter::api_where(&table, params.get("where").filter(|w| !w.is_null()))?;
            let found = match read(cx, &table, filter, MAX_ROWS).await {
                Ok(found) => found,
                Err(e) if is_denial(&e) => {
                    return Ok(StepOutcome::fail(format!(
                        "reading `{}` was refused: {e}",
                        table.name
                    )));
                }
                Err(e) => return Err(e),
            };
            let n = found.len();
            match params.get("count").and_then(Json::as_u64) {
                Some(count) if n as u64 != count => {
                    return Ok(StepOutcome::fail(format!(
                        "expected {count} row{} of `{}`, found {n}",
                        if count == 1 { "" } else { "s" },
                        table.name
                    ))
                    .with_value(Json::Array(found)));
                }
                None if n == 0 => {
                    return Ok(StepOutcome::fail(format!(
                        "expected rows of `{}`, found none",
                        table.name
                    )));
                }
                _ => {}
            }
            if let Some(fields) = params.get("fields").filter(|f| !f.is_null()) {
                if !fields.is_object() {
                    return Err(Error::invalid("`fields` should be an object of values"));
                }
                if let Some(row) = found.iter().find(|row| !matches(fields, row)) {
                    return Ok(StepOutcome::fail(format!(
                        "a row of `{}` does not hold {fields}: {row}",
                        table.name
                    ))
                    .with_value(Json::Array(found)));
                }
            }
            Ok(StepOutcome::pass(format!(
                "{n} row{} of `{}` as expected",
                if n == 1 { "" } else { "s" },
                table.name
            ))
            .with_value(Json::Array(found)))
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

/// `data.expect_denied`: `run_as` must **not** be able to read, insert,
/// update or delete. Nothing is written.
pub struct ExpectDenied;

#[async_trait::async_trait]
impl StepType for ExpectDenied {
    fn name(&self) -> &str {
        "data.expect_denied"
    }

    fn description(&self) -> &str {
        "Assert that the checklist's user may NOT read, insert, update or delete: `read` with \
         an `id` or a filter passes when no such row is visible; `insert` takes the `row`; \
         `update` and `delete` take the `id`. Nothing is written — the access rules are asked \
         for their verdict. Catches a role or ownership-formula edit that let too much through."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            table_field(),
            FormField::new("operation", BasicType::Text)
                .label("Operation")
                .required()
                .options(["read", "insert", "update", "delete"]),
            FormField::new("id", BasicType::Text).label("Row id"),
            row_field("Row (for insert)"),
            where_field(),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let table = table(cx, params)?;
            let operation = text(params, "operation")?;
            let who = cx
                .run_as
                .and_then(|u| u.extra.get("email"))
                .map(|e| match e {
                    sc_query::Value::Text(t) => t.clone(),
                    other => format!("{other:?}"),
                })
                .unwrap_or_else(|| "an anonymous visitor".to_owned());
            let evaluator = cx.evaluator()?;
            let allowed = match operation.as_str() {
                "read" => {
                    let filter = match params.get("id").filter(|v| !v.is_null()) {
                        Some(_) => {
                            let pk = rows::single_pk(&table)?;
                            Some(rows::pk_filter(&table, &pk, &text(params, "id")?)?)
                        }
                        None => sc_api::filter::api_where(
                            &table,
                            params.get("where").filter(|w| !w.is_null()),
                        )?,
                    };
                    match read(cx, &table, filter, 1).await {
                        Ok(found) => !found.is_empty(),
                        Err(e) if is_denial(&e) => false,
                        Err(e) => return Err(e),
                    }
                }
                "insert" => {
                    sc_api::row_insertable_as(
                        cx.catalog,
                        &table,
                        &object(params, "row")?,
                        cx.role(),
                        cx.run_as,
                        evaluator,
                    )
                    .await?
                }
                "update" => {
                    sc_api::row_updatable_as(
                        cx.catalog,
                        &table,
                        &text(params, "id")?,
                        cx.role(),
                        cx.run_as,
                        evaluator,
                    )
                    .await?
                }
                "delete" => {
                    sc_api::row_deletable_as(
                        cx.catalog,
                        &table,
                        &text(params, "id")?,
                        cx.role(),
                        cx.run_as,
                        evaluator,
                    )
                    .await?
                }
                other => {
                    return Err(Error::invalid(format!(
                        "`operation` should be read, insert, update or delete, not `{other}`"
                    )));
                }
            };
            Ok(if allowed {
                StepOutcome::fail(format!(
                    "{who} may {operation} `{}`, and should not",
                    table.name
                ))
            } else {
                StepOutcome::pass(format!("{who} may not {operation} `{}`", table.name))
            })
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

/// `data.run_trigger`: run a trigger with a payload as `run_as`, expecting it
/// to succeed or to fail with a matching message.
pub struct RunTrigger;

#[async_trait::async_trait]
impl StepType for RunTrigger {
    fn name(&self) -> &str {
        "data.run_trigger"
    }

    fn description(&self) -> &str {
        "Run a trigger (an action, a workflow or an agent) with a payload, as the checklist's \
         user — who must meet its `min_role`. Expect `success` (the default), or `error` with an \
         optional pattern its message must match. Its value is what the trigger returned. Assert \
         its effects with data.expect_rows."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("trigger", BasicType::Text)
                .label("Trigger")
                .required(),
            FormField::new("payload", BasicType::Json)
                .label("Payload")
                .sublabel("The row or payload the trigger runs with"),
            FormField::new("expect", BasicType::Text)
                .label("Expect")
                .options(["success", "error"])
                .default_value("success"),
            FormField::new("error_matches", BasicType::Text)
                .label("Error matches (regular expression)")
                .show_if("expect", vec![json!("error")]),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let name = text(params, "trigger")?;
            let expect_error = params.get("expect").and_then(Json::as_str) == Some("error");
            let pattern = match params.get("error_matches").and_then(Json::as_str) {
                Some(p) if !p.is_empty() => Some(regex_lite::Regex::new(p).map_err(|e| {
                    Error::invalid(format!("`error_matches` is not a regular expression: {e}"))
                })?),
                _ => None,
            };
            let dispatcher = cx.triggers()?;
            let floor = dispatcher.triggers()?.require(&name)?.min_role.unwrap_or(1);
            let result = if cx.role() > floor {
                Err(Error::auth(format!(
                    "trigger `{name}` needs role {floor} or better, and the checklist runs at \
                     role {}",
                    cx.role()
                )))
            } else {
                let payload = params.get("payload").cloned().unwrap_or(Json::Null);
                let caller = sc_api::caller_context(cx.run_as);
                dispatcher
                    .run_trigger(cx.catalog, &name, payload, Some(&caller))
                    .await
            };
            Ok(match (result, expect_error) {
                (Ok(value), false) => {
                    StepOutcome::pass(format!("trigger `{name}` ran")).with_value(value)
                }
                (Ok(value), true) => StepOutcome::fail(format!(
                    "trigger `{name}` succeeded, and was expected to fail"
                ))
                .with_value(value),
                (Err(e), false) => StepOutcome::fail(format!(
                    "trigger `{name}` failed: {}",
                    sc_error::format_chain(&e)
                )),
                (Err(e), true) => {
                    let message = sc_error::format_chain(&e);
                    match &pattern {
                        Some(re) if !re.is_match(&message) => StepOutcome::fail(format!(
                            "trigger `{name}` failed, but not as expected: {message}"
                        )),
                        _ => StepOutcome::pass(format!("trigger `{name}` failed: {message}")),
                    }
                }
            })
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

/// `data.expect_calculated`: a calculated field's value on a row, as `run_as`
/// reads it — what catches a formula broken by a field rename.
pub struct ExpectCalculated;

#[async_trait::async_trait]
impl StepType for ExpectCalculated {
    fn name(&self) -> &str {
        "data.expect_calculated"
    }

    fn description(&self) -> &str {
        "Assert the value of a calculated field on one row, by its id, as the checklist's user \
         reads it. Catches a formula broken by a field rename."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            table_field(),
            id_field(),
            FormField::new("field", BasicType::Text)
                .label("Calculated field")
                .required(),
            FormField::new("value", BasicType::Json)
                .label("Expected value")
                .required(),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::DATA
    }

    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let outcome: Result<StepOutcome> = async {
            let table = table(cx, params)?;
            let id = text(params, "id")?;
            let field = text(params, "field")?;
            let Some(def) = table.field(&field) else {
                return Ok(StepOutcome::fail(format!(
                    "`{}` has no field `{field}`",
                    table.name
                )));
            };
            if !def.is_calc() {
                return Err(Error::invalid(format!(
                    "`{}.{field}` is not a calculated field; check it with data.expect_rows",
                    table.name
                )));
            }
            let expected = params.get("value").cloned().unwrap_or(Json::Null);
            let row = match read_one(cx, &table, &id).await {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return Ok(StepOutcome::fail(format!(
                        "no row {id} of `{}` is visible",
                        table.name
                    )));
                }
                Err(e) if is_denial(&e) => {
                    return Ok(StepOutcome::fail(format!(
                        "reading `{}` was refused: {e}",
                        table.name
                    )));
                }
                Err(e) => {
                    // A formula that no longer evaluates fails the read; that
                    // is exactly what this step is for.
                    return Ok(StepOutcome::fail(format!(
                        "`{}.{field}` could not be calculated: {}",
                        table.name,
                        sc_error::format_chain(&e)
                    )));
                }
            };
            let actual = row.get(&field).cloned().unwrap_or(Json::Null);
            Ok(if matches(&expected, &actual) {
                StepOutcome::pass(format!("`{}.{field}` is {actual}", table.name))
                    .with_value(actual)
            } else {
                StepOutcome::fail(format!(
                    "`{}.{field}` is {actual}, expected {expected}",
                    table.name
                ))
                .with_value(actual)
            })
        }
        .await;
        outcome.unwrap_or_else(StepOutcome::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_match_across_their_spellings() {
        assert!(matches(&json!(3), &json!(3.0)));
        assert!(matches(&json!("3"), &json!(3)));
        assert!(matches(&json!(true), &json!("true")));
        assert!(matches(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
        assert!(!matches(&json!({"a": 1, "c": 1}), &json!({"a": 1})));
        assert!(!matches(&json!("x"), &json!("y")));
        assert!(!matches(&json!(null), &json!(0)));
    }
}
