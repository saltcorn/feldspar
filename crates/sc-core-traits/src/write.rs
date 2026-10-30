//! Selecting the rows a write trait is about to change (§11.3).
//!
//! `edit_table_rows`' update and delete both start the same way: find the rows the
//! filter selects, refuse the call outright if there are too many, and then
//! write them one at a time by primary key. That shape is here once because each
//! of its three decisions is a promise, and two of them are safety properties
//! that must not be able to differ between the two.
//!
//! **The rows are selected through [`sc_api::read_rows_as`]** — the caller's own
//! read. So a tool can only change rows the same caller could have been shown,
//! and an agent chatting with a user whose ownership formula hides half the
//! table cannot update the half it cannot see. That is a stronger rule than the
//! write path alone would give (each row is *also* checked for write access as
//! it is written), and it is the right way round: an agent that could change
//! rows it cannot read would be a way to edit data by guessing at it.
//!
//! **Too many rows is a refusal, not a truncation.** One row more than the
//! ceiling and nothing is written at all, with a message saying how many matched
//! and that the filter should be narrowed. A partially applied bulk change is
//! the one outcome from which neither the model nor the person watching can tell
//! what happened.
//!
//! **One row at a time, by primary key.** Not one bulk statement: that is what
//! gives each changed row its own event with its own payload (§10.2's decision
//! 2, which the actions of the same names already take), and what lets §7.3's
//! per-row check apply at all. The consequence — inherited from those actions,
//! and stated here rather than hidden — is that a row that fails *while* the
//! loop is running stops it with the rows before it already written. There is no
//! transaction around the set, because the events are dispatched after commit by
//! design; the ceiling above is what keeps the set small enough for the error to
//! name what happened.

use sc_agent::TraitContext;
use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_query::Expr;
use serde_json::Value as Json;

use sc_api::rows::{self, RowQuery};

use crate::table::row_id;

/// The primary keys of the rows `filter` selects, as both the string the row
/// layer addresses a row by and the JSON a tool result reports — or the refusal
/// that says there were too many.
pub async fn matching_ids(
    ctx: &TraitContext<'_>,
    table: &Table,
    filter: Option<Expr>,
    ceiling: u64,
    verb: &str,
) -> Result<Vec<(String, Json)>> {
    let pk = rows::single_pk(table)?;
    // One more than the ceiling, so "too many" can be reported as a fact rather
    // than inferred from a full page.
    let query = RowQuery::new()
        .where_(filter)
        .limit(ceiling.saturating_add(1));
    let found = sc_api::read_rows_as(
        ctx.catalog,
        table,
        &query,
        ctx.caller.role,
        ctx.caller.user.as_ref(),
        ctx.evaluator,
    )
    .await?;
    let rows: Vec<Json> = found.as_array().cloned().unwrap_or_default();
    if rows.len() as u64 > ceiling {
        return Err(Error::invalid(format!(
            "that filter matches more than {ceiling} rows of `{}`, which is the \
             most this tool may {verb} in one call; nothing was changed — \
             narrow the filter and try again",
            table.name
        )));
    }
    // Nothing matched is **not** an error: it is a count of zero, which is a
    // true answer to what was asked and one a model can act on. Raising it as a
    // failure would make an idempotent call ("delete the done items") look
    // broken every second time it is made.
    rows.iter().map(|row| row_id(table, &pk, row)).collect()
}
