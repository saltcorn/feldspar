//! Writing rows: `insert_X`, `update_X_by_pk` and `delete_X_by_pk`.
//!
//! Each is a **thin call** into the row layer's own write path — that is the
//! whole design. `ownership::insert_row_as` / `update_row_as` / `delete_row_as`
//! are the same entry points the agent's write tools use (§11.3), and they in
//! turn go through `rows::create_row_ctx` / `update_row_guarded` /
//! `delete_row_guarded`, so a rich type's attribute rule, a `File` field's store
//! and folder checks, §7.3's "granted on the existing row *and* on the row as it
//! would become", RLS's `WITH CHECK` and the table's triggers all fire for a
//! GraphQL caller **because they are the same code**. Nothing in this module
//! decides who may write what, and nothing here coerces a value: it turns a
//! GraphQL input object into the JSON body the row layer already consumes and
//! hands it over.
//!
//! Two consequences worth stating, because they are visible on the wire:
//!
//! **The refusal is the row layer's, labelled.** A mutation is run by an
//! application on behalf of a person filling in a form, so its errors carry
//! `extensions.code` and — for a validation failure — `extensions.field`
//! ([`errors`](super::errors)). The message itself is unchanged.
//!
//! **An insert and an update answer with the row *read back*.** The write
//! returns the row it wrote, and that row carries its own columns and nothing
//! else — no Ⱶ-join leaves, no correlated aggregates, because a write's
//! `RETURNING` was never given the selection set. So the response is projected
//! by [`resolve::read_one`], the same read `_by_pk` performs: the row a mutation
//! returns is then *the row a query would have returned*, joinfields and child
//! aggregates included, and the caller's read rules apply to it. A delete has no
//! row left to read, so it answers with the columns the row layer removed and
//! refuses a selection that reaches past them, rather than answering `null` for
//! a relation that was never looked at.
//!
//! **Every table gets its mutations** (decision 4). One schema per application,
//! not per role: a table whose `min_role_write` this caller does not meet is in
//! the schema and refuses at resolve time, naming itself — because the schema
//! describes what the *application* exposes, and a caller who gains a role
//! should not need a different schema.

use async_graphql::Value as GqlValue;
use async_graphql::dynamic::{FieldFuture, FieldValue, ResolverContext};
use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::Value as Json;

use super::args;
use super::context::{RequestContext, request_context};
use super::errors;
use super::resolve::{self, Resolver, RowValue};
use crate::convert::value_to_json;
use crate::ownership;
use crate::rows;

/// `insert_X(object: …)` — the row to write.
pub const ARG_OBJECT: &str = "object";
/// `update_X_by_pk(pk_columns: …)` — which row to write it to.
///
/// An input object rather than a bare key, following Hasura, because that is the
/// shape a composite key would need and a caller should not have to relearn the
/// field when a table gains one.
pub const ARG_PK_COLUMNS: &str = "pk_columns";
/// `update_X_by_pk(set: …)` — the columns to change.
pub const ARG_SET: &str = "set";

/// `insert_X(object: XInsertInput!): X`.
pub fn insert_field(table: impl Into<String>) -> Resolver {
    let name = table.into();
    Box::new(move |ctx| {
        let name = name.clone();
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let table = rc.table(&name).map_err(|e| errors::table_error(&name, e))?;
            insert(rc, &table, &ctx)
                .await
                .map_err(|e| errors::mutation_error(&table, e))
        })
    })
}

/// `update_X_by_pk(pk_columns: XPkColumns!, set: XSetInput!): X`.
pub fn update_by_pk_field(table: impl Into<String>, pk: impl Into<String>) -> Resolver {
    let name = table.into();
    let pk = pk.into();
    Box::new(move |ctx| {
        let (name, pk) = (name.clone(), pk.clone());
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let table = rc.table(&name).map_err(|e| errors::table_error(&name, e))?;
            update(rc, &table, &pk, &ctx)
                .await
                .map_err(|e| errors::mutation_error(&table, e))
        })
    })
}

/// `delete_X_by_pk(id: …): X`.
pub fn delete_by_pk_field(table: impl Into<String>, pk: impl Into<String>) -> Resolver {
    let name = table.into();
    let pk = pk.into();
    Box::new(move |ctx| {
        let (name, pk) = (name.clone(), pk.clone());
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let table = rc.table(&name).map_err(|e| errors::table_error(&name, e))?;
            delete(rc, &table, &pk, &ctx)
                .await
                .map_err(|e| errors::mutation_error(&table, e))
        })
    })
}

/// The insert, through the row layer.
async fn insert(
    rc: &RequestContext,
    table: &Table,
    ctx: &ResolverContext<'_>,
) -> Result<Option<FieldValue<'static>>> {
    let body = object_arg(table, ctx, ARG_OBJECT)?;
    rc.charge(&table.name)?;
    let written = ownership::insert_row_as(
        &rc.catalog,
        table,
        &body,
        rc.role(),
        rc.user(),
        rc.evaluator(),
        // A GraphQL mutation is a request, not a trigger firing: nothing led
        // here, so the event this write raises is at depth 0.
        &[],
        &rows::Executor::Pooled,
    )
    .await?;
    read_back(rc, table, &written, ctx).await
}

/// The update, through the row layer: granted on the existing row and on the
/// row as it would become, both decided there.
async fn update(
    rc: &RequestContext,
    table: &Table,
    pk: &str,
    ctx: &ResolverContext<'_>,
) -> Result<Option<FieldValue<'static>>> {
    let keys = object_arg(table, ctx, ARG_PK_COLUMNS)?;
    let id = pk_text(table, pk, &keys)?;
    let body = object_arg(table, ctx, ARG_SET)?;
    rc.charge(&table.name)?;
    let written = ownership::update_row_as(
        &rc.catalog,
        table,
        &id,
        &body,
        rc.role(),
        rc.user(),
        rc.evaluator(),
        &[],
        &rows::Executor::Pooled,
    )
    .await?;
    read_back(rc, table, &written, ctx).await
}

/// The delete, through the row layer, answering with the row it removed.
async fn delete(
    rc: &RequestContext,
    table: &Table,
    pk: &str,
    ctx: &ResolverContext<'_>,
) -> Result<Option<FieldValue<'static>>> {
    let key = ctx
        .args
        .get(pk)
        .ok_or_else(|| Error::invalid(format!("`{pk}` is required")))?;
    let id = key_text(table, pk, key.as_value())?;
    rc.charge(&table.name)?;
    let removed = ownership::delete_row_as(
        &rc.catalog,
        table,
        &id,
        rc.role(),
        rc.user(),
        rc.evaluator(),
        &[],
        &rows::Executor::Pooled,
    )
    .await?;
    // The row is gone, so it cannot be read back as an insert's is: its last
    // image is judged by the read rule instead (security review #28). A caller
    // who may delete but not read is told the delete happened, not the row.
    let readable = ownership::RowImage::new(table, &removed)
        .readable(&rc.catalog, rc.role(), rc.user(), rc.evaluator())
        .await
        .unwrap_or(false);
    if !readable {
        return Err(Error::auth(format!(
            "the row was deleted from `{}`, but you may not read it",
            table.name
        )));
    }
    Ok(Some(FieldValue::owned_any(RowValue::written(
        &table.name,
        rows::json_row_values(table, &removed),
    ))))
}

/// One input-object argument as the JSON body the row layer's write path takes.
///
/// The values are *not* coerced here. `rows::column_value` is what turns
/// `"2026-01-01"` into a date and refuses `999` for an `Integer` with a `max` of
/// 120, and it runs inside the write — the same call an inserted REST body goes
/// through, naming the same field in the same words.
fn object_arg(table: &Table, ctx: &ResolverContext<'_>, arg: &str) -> Result<Json> {
    let value = ctx
        .args
        .get(arg)
        .ok_or_else(|| Error::invalid(format!("`{arg}` is required")))?;
    match value
        .as_value()
        .clone()
        .into_json()
        .map_err(|e| Error::invalid(format!("`{arg}` is not a JSON value: {e}")))?
    {
        Json::Object(map) if map.is_empty() => Err(Error::invalid(format!(
            "`{arg}` names no column of `{}` to write",
            table.name
        ))),
        Json::Object(map) => Ok(Json::Object(map)),
        _ => Err(Error::invalid(format!(
            "`{arg}` on `{}` must be an object",
            table.name
        ))),
    }
}

/// The primary key out of a `pk_columns` argument.
fn pk_text(table: &Table, pk: &str, keys: &Json) -> Result<String> {
    let key = keys
        .get(pk)
        .ok_or_else(|| Error::invalid(format!("`{ARG_PK_COLUMNS}` must name `{pk}`")))?;
    text_of(rows::column_value(table, pk, key)?)
}

/// The primary key out of a bare argument (`delete_X_by_pk(id: 7)`).
fn key_text(table: &Table, pk: &str, key: &GqlValue) -> Result<String> {
    text_of(args::key_value(table, pk, key)?)
}

/// A key as the text the row layer addresses a row by.
///
/// The row layer takes an id as a `&str` — that is what a REST path segment is —
/// and coerces it back through the column with `rows::column_value`. Going out
/// through the *typed* value first rather than rendering the caller's input is
/// what makes `id: 7` and `id: "7"` the same row and a malformed key an error
/// naming the column, instead of a string the database will make its own mind up
/// about.
fn text_of(value: Value) -> Result<String> {
    match value_to_json(&value) {
        Json::String(s) => Ok(s),
        Json::Null => Err(Error::invalid("a primary key cannot be null")),
        other => Ok(other.to_string()),
    }
}

/// The row a write answers with: the written row, **read back** through the same
/// path `_by_pk` uses, so the selection set is answered in full.
///
/// A table with no single primary key has no row to address, so its insert
/// answers with what the write returned — the row's own columns, and an error
/// rather than a `null` for anything reaching past them.
async fn read_back(
    rc: &RequestContext,
    table: &Table,
    written: &Json,
    ctx: &ResolverContext<'_>,
) -> Result<Option<FieldValue<'static>>> {
    let values = rows::json_row_values(table, written);
    let key = rows::single_pk(table)
        .ok()
        .and_then(|pk| values.get(&pk).cloned().map(|value| (pk, value)));
    let Some((pk, value)) = key else {
        return Ok(Some(FieldValue::owned_any(RowValue::written(
            &table.name,
            values,
        ))));
    };
    match resolve::read_one(rc, table, &pk, value, ctx).await? {
        Some(row) => Ok(Some(row)),
        // The write happened — this is not a rollback, and saying so is the
        // point. A caller who may write a table they may not read gets the
        // refusal rather than a `null` that reads as "nothing was written".
        None => Err(Error::auth(format!(
            "the write to `{}` was applied, but you may not read the row back",
            table.name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, plain_field, table_of, typed_field};
    use sc_types::BasicType;

    #[test]
    fn a_key_reaches_the_row_layer_as_the_text_it_addresses_rows_by() {
        let table = table_of("tasks", vec![id_field(), plain_field("title")]);
        // A JSON number and the same key as a string are one row.
        assert_eq!(
            pk_text(&table, "id", &serde_json::json!({ "id": 7 })).expect("a key"),
            "7"
        );
        assert_eq!(
            pk_text(&table, "id", &serde_json::json!({ "id": "7" })).expect("a key"),
            "7"
        );
        // A date key comes back as the date it is, not as whatever the caller
        // wrote it as.
        let events = table_of(
            "events",
            vec![typed_field("day", BasicType::Date).required().primary_key()],
        );
        assert_eq!(
            pk_text(&events, "day", &serde_json::json!({ "day": "2026-01-02" })).expect("a key"),
            "2026-01-02"
        );
    }

    #[test]
    fn a_malformed_key_is_refused_by_the_column_not_passed_through() {
        let table = table_of("tasks", vec![id_field()]);
        let err = pk_text(&table, "id", &serde_json::json!({ "id": "seven" })).unwrap_err();
        assert!(format!("{err}").contains("id"), "{err}");
        // …and one that names no key at all says which key it needed.
        let err = pk_text(&table, "id", &serde_json::json!({})).unwrap_err();
        assert!(format!("{err}").contains("`id`"), "{err}");
    }
}
