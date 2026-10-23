//! A row write **as a caller**: the REST API's create, update and delete rules
//! (§7.3), shared with offline sync's uploads so a device can never do more than
//! its user could through the API.
//!
//! - At or above the table's write floor: the write goes through.
//! - Below it, with an ownership formula: an insert is checked against the
//!   proposed row; an update against the row as it is *and* as it would be, so
//!   it cannot move a row out of the caller's ownership; a delete against the
//!   row as it is. A translatable formula also rides in the write's `WHERE`.
//! - Below it, without one: refused.
//! - An RLS table's policies decide everything; nothing is checked here.
//!
//! The write runs on the caller's [`Executor`]. The ownership checks read on the
//! pool, so a row written earlier in the same open transaction is not seen by
//! them: callers that need that commit first (offline sync commits each change).

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_auth::User;
use sc_catalog::{CallerContext, Catalog, Table};
use sc_error::{Error, Result};
use sc_expr::{Formula, JsEvaluator, Operation};
use sc_query::Value;
use serde_json::Value as Json;

use crate::ownership;
use crate::rows::{self, Executor};

/// Why a write was refused before it reached the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Denied {
    /// Below the table's write floor, with no ownership formula to extend it.
    Floor,
    /// The ownership formula refused the row; the sentence says how.
    Ownership(&'static str),
}

impl Denied {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Denied::Floor => "insufficient privilege",
            Denied::Ownership(why) => why,
        }
    }
}

/// Everything a write as somebody needs besides the row.
pub(crate) struct Caller<'a> {
    pub user: Option<&'a User>,
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// Who the write's event reports as its cause (§10.2), and an RLS table's
    /// policies' caller.
    pub context: &'a CallerContext,
    pub executor: &'a Executor,
}

/// The ownership formula that decides for this caller, `Ok(None)` when the
/// floor already allows, or [`Denied::Floor`].
///
/// The floor is met by the user's role or by the role of the context the write
/// runs under. Those are the same for a request; they differ for a code body,
/// which writes with the admin's authority on behalf of the event's caller
/// (or of nobody, for a scheduled trigger).
fn formula<'t>(
    table: &'t Table,
    caller: &Caller<'_>,
) -> std::result::Result<Option<&'t Formula>, Denied> {
    if ownership::meets(caller.user, table.access.min_role_write)
        || caller.context.role <= table.access.min_role_write
    {
        return Ok(None);
    }
    table.ownership.as_ref().map(Some).ok_or(Denied::Floor)
}

/// Insert `body`, answering the inserted row.
pub(crate) async fn create(
    cat: &Catalog,
    table: &Table,
    body: &Json,
    caller: &Caller<'_>,
) -> Result<std::result::Result<Json, Denied>> {
    let context = Some(caller.context);
    if table.rls_enabled {
        return Ok(Ok(rows::create_row_in(
            cat,
            table,
            body,
            context,
            caller.executor,
        )
        .await?));
    }
    let formula = match formula(table, caller) {
        Ok(formula) => formula,
        Err(denied) => return Ok(Err(denied)),
    };
    // The defaults first, so the ownership check sees the row that would be
    // written (`rows::fill_defaults`).
    let body = &rows::fill_defaults(cat, table, body, caller.context.user.as_ref()).await?;
    if let Some(f) = formula {
        // An insert is checked against the proposed row, `_insert` folded true
        // (§7.3) — WITH CHECK semantics at runtime.
        let proposed = ownership::proposed_insert_row(table, body)?;
        if !ownership::row_allowed(
            cat,
            table,
            f,
            Operation::Insert,
            caller.user,
            caller.evaluator,
            &proposed,
        )
        .await?
        {
            return Ok(Err(Denied::Ownership("this row is outside your ownership")));
        }
    }
    Ok(Ok(rows::create_row_in(
        cat,
        table,
        body,
        context,
        caller.executor,
    )
    .await?))
}

/// Update the row keyed `id` with `body`, answering the updated row.
pub(crate) async fn update(
    cat: &Catalog,
    table: &Table,
    id: &str,
    body: &Json,
    caller: &Caller<'_>,
) -> Result<std::result::Result<Json, Denied>> {
    let context = Some(caller.context);
    if table.rls_enabled {
        return Ok(Ok(rows::update_row_guarded_in(
            cat,
            table,
            id,
            body,
            None,
            context,
            caller.executor,
        )
        .await?));
    }
    let f = match formula(table, caller) {
        Ok(None) => {
            return Ok(Ok(rows::update_row_guarded_in(
                cat,
                table,
                id,
                body,
                None,
                context,
                caller.executor,
            )
            .await?));
        }
        Ok(Some(f)) => f,
        Err(denied) => return Ok(Err(denied)),
    };
    // USING: granted on the existing row…
    let existing = owned_row(cat, table, f, Operation::Update, caller, id).await?;
    // …and WITH CHECK: granted on the proposed row too, so an update cannot move
    // the row out of the caller's ownership.
    let changes = rows::coerce_row_values(table, body)?;
    let merged = ownership::merged_row(table, &existing, &changes);
    if !ownership::row_allowed(
        cat,
        table,
        f,
        Operation::Update,
        caller.user,
        caller.evaluator,
        &merged,
    )
    .await?
    {
        return Ok(Err(Denied::Ownership(
            "the update would move this row out of your ownership",
        )));
    }
    // The translated predicate rides in the UPDATE's WHERE when it can, closing
    // the check-to-write race.
    let guard = ownership::write_guard(cat, table, f, Operation::Update, caller.user)?;
    Ok(Ok(rows::update_row_guarded_in(
        cat,
        table,
        id,
        body,
        guard,
        context,
        caller.executor,
    )
    .await?))
}

/// Delete the row keyed `id`.
pub(crate) async fn delete(
    cat: &Catalog,
    table: &Table,
    id: &str,
    caller: &Caller<'_>,
) -> Result<std::result::Result<(), Denied>> {
    let context = Some(caller.context);
    let guard = if table.rls_enabled {
        None
    } else {
        match formula(table, caller) {
            Ok(None) => None,
            Ok(Some(f)) => {
                owned_row(cat, table, f, Operation::Delete, caller, id).await?;
                ownership::write_guard(cat, table, f, Operation::Delete, caller.user)?
            }
            Err(denied) => return Ok(Err(denied)),
        }
    };
    rows::delete_row_guarded_in(cat, table, id, guard, context, caller.executor).await?;
    Ok(Ok(()))
}

/// The row keyed `id` with the values `formula` needs, when the formula grants
/// `op` on it; otherwise the same not-found a missing row gets, so a refusal
/// does not tell the caller the row exists.
pub(crate) async fn owned_row(
    cat: &Catalog,
    table: &Table,
    formula: &Formula,
    op: Operation,
    caller: &Caller<'_>,
    id: &str,
) -> Result<BTreeMap<String, Value>> {
    owned_row_as(cat, table, formula, op, caller.user, caller.evaluator, id).await
}

/// [`owned_row`] for a caller with no write in hand (a file download).
pub(crate) async fn owned_row_as(
    cat: &Catalog,
    table: &Table,
    formula: &Formula,
    op: Operation,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    id: &str,
) -> Result<BTreeMap<String, Value>> {
    let not_found = || {
        let pk = rows::single_pk(table)?;
        Err(Error::not_found(format!("no row with {pk} = {id}")))
    };
    let Some(existing) = ownership::fetch_row_values(cat, table, formula, id).await? else {
        return not_found();
    };
    if !ownership::row_allowed(cat, table, formula, op, user, evaluator, &existing).await? {
        return not_found();
    }
    Ok(existing)
}
