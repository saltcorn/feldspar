//! Runtime enforcement of ownership formulas (§7.3, TODO Phase 5).
//!
//! The access rule this module implements, stated once: **allowed = the caller
//! meets the operation's `min_role` OR the table's ownership formula grants
//! this row** for this user and operation. Ownership *extends* access below
//! the role floor; it never narrows what a role already has — a caller at or
//! above the floor takes the ordinary unfiltered path and this module never
//! runs for them.
//!
//! Two evaluation strategies, chosen per formula:
//!
//! - **Symbolic** where possible: reads AND the translated predicate
//!   (`UserEnv::Inline`, flags folded) into the SELECT, and guarded writes AND
//!   it into the UPDATE/DELETE's WHERE — the database does the filtering and a
//!   denied row is indistinguishable from an absent one.
//! - **Reified** where not ([`TranslateError::Untranslatable`]): rows are
//!   fetched *with their Ⱶ-join values projected alongside* (one query, not a
//!   round trip per row) and the formula runs in the [`JsEvaluator`]. An
//!   evaluation error **denies** — the evaluator contract — and a missing
//!   evaluator is a configuration error, never an open door.
//!
//! Writes check the row **twice** where it matters (§6's USING/WITH CHECK
//! semantics, here at runtime): an update must be granted on the existing row
//! *and* on the proposed row, so a user cannot move a row out of their own
//! ownership; an insert is checked against the proposed row with `_insert`.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_auth::{COL_ID, COL_ROLE, ROLE_PUBLIC, User};
use sc_catalog::{CallerContext, Catalog, Table, prefetch_bindings};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_expr::{
    Env, Formula, FormulaCall, JsEvaluator, Operation, TranslateError, UserEnv, join_path_expr,
    translate, translate_rooted,
};
use sc_query::{Expr, OrderBy, Projection, Select, Source, Value};
use serde_json::{Map, Value as Json};

use crate::convert::value_to_json;
use crate::rows;

/// The caller's role: their own, or public when nobody is logged in.
pub(crate) fn caller_role(user: Option<&User>) -> u8 {
    user.map_or(ROLE_PUBLIC, |u| u.role)
}

/// Whether the caller meets a `min_role` floor (lower role = more privileged).
pub(crate) fn meets(user: Option<&User>, min_role: u8) -> bool {
    caller_role(user) <= min_role
}

/// The [`CallerContext`] a row operation runs under: the caller's role, and —
/// when logged in — their fields as the JSON object the `sc.user` GUC carries,
/// exactly the shape `UserEnv::Guc`'s
/// `current_setting('sc.user', …)::jsonb ->> 'x'` reads. Anonymous callers
/// carry only the role, so the policies' `current_setting('sc.user', true)`
/// reads `NULL` and `user === null` decides.
///
/// It travels with **every** write, not only with the RLS ones it was built for:
/// off an RLS table it sets nothing and decides nothing, and is simply who the
/// table event reports as the cause (§10.2).
pub fn caller_context(user: Option<&User>) -> CallerContext {
    caller_context_at(caller_role(user), user)
}

/// [`caller_context`] at an **explicit** role — what the admin API's row
/// endpoints use: [`ROLE_ADMIN`](sc_auth::ROLE_ADMIN) clears every policy's role
/// floor, so the admin's own row editor works on a FORCE'd table, while the user
/// object still says which admin did it.
pub fn caller_context_at(role: u8, user: Option<&User>) -> CallerContext {
    let fields = user_values(user).map(|map| {
        let obj: serde_json::Map<String, Json> = map
            .iter()
            .map(|(k, v)| (k.clone(), value_to_json(v)))
            .collect();
        Json::Object(obj)
    });
    CallerContext::new(role, fields)
}

/// The user object as the formula sees it: `id`, `role`, and every extra field
/// — the same map both the `Inline` translation env and the reified
/// [`FormulaCall`] consume, so the two evaluators see one user by construction.
fn user_values(user: Option<&User>) -> Option<BTreeMap<String, Value>> {
    user.map(|u| {
        let mut map = u.extra.clone();
        map.insert(COL_ID.to_owned(), Value::Uuid(u.id));
        map.insert(COL_ROLE.to_owned(), Value::Int(i64::from(u.role)));
        map
    })
}

/// The rows of `table` a caller may read, applying §7.3's rule to a
/// [`RowQuery`](rows::RowQuery) rather than to "everything".
///
/// The one entry point for a reader that is **not** an API surface — today an
/// agent's `query_table` tool (§11.3), which must see exactly what the person
/// chatting would see through the REST API and not one row more. Sharing this
/// function rather than the rule is the point: a second implementation of "meets
/// the floor OR the formula grants it" is a second place for it to be subtly
/// wrong, and the one that is wrong is the one nobody is looking at.
///
/// `role` is passed explicitly rather than read off `user` because a caller is
/// not always a user: a trigger-started agent run carries the trigger's
/// authority (admin) with no user attached, and an anonymous reader carries
/// public with the same. A caller the floor does not admit and no formula
/// extends gets an [`Error::auth`] naming the table — the reader here is a tool
/// result a model has to be able to act on, not an HTTP status.
pub async fn read_rows_as(
    cat: &Catalog,
    table: &Table,
    query: &rows::RowQuery,
    role: u8,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
) -> Result<Json> {
    let granted = read_row_values_as(cat, table, query, role, user, evaluator).await?;
    Ok(Json::Array(
        granted
            .iter()
            .map(|values| table_row_json(table, values))
            .collect(),
    ))
}

/// [`read_rows_as`] as **query values** rather than as the JSON wire shape —
/// the same rule, the same statements, read by a caller that needs the values
/// typed and needs the [`RowQuery::extra`](rows::RowQuery::extra) projections
/// it asked for.
///
/// The GraphQL provider is that caller (§13.4): a `Decimal` must reach the wire
/// exact rather than through a JSON number, and a requested `manager { email }`
/// rides back as an aliased extra column that the row's JSON rendering would
/// drop. Sharing the *function* rather than the rule is the whole point — this
/// is where "meets the floor OR the formula grants it" lives, and
/// [`read_rows_as`] is now a rendering of it.
pub async fn read_row_values_as(
    cat: &Catalog,
    table: &Table,
    query: &rows::RowQuery,
    role: u8,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
) -> Result<Vec<BTreeMap<String, Value>>> {
    // The database enforces this table's ownership: run the read in a
    // caller-context transaction and let the policies decide, exactly as the
    // REST provider's RLS path does.
    //
    // A read of an *unprotected* table needs the same transaction when one of
    // its own projections reaches a protected one — a correlated aggregate over
    // an RLS child (§13.4). The context is built either way and the row layer
    // decides whether the statement is wrapped, so the two rules stay in the one
    // place that knows them.
    let ctx = (table.rls_enabled || query.in_caller_context).then(|| caller_context_at(role, user));
    if table.rls_enabled {
        return rows::list_row_values(cat, table, query, ctx.as_ref()).await;
    }
    if role <= table.access.min_role_read {
        return rows::list_row_values(cat, table, query, ctx.as_ref()).await;
    }
    let Some(formula) = &table.ownership else {
        return Err(Error::auth(format!("you may not read `{}`", table.name)));
    };

    let shape = cat.schema_shape()?;
    let env = UserEnv::Inline(user_values(user));
    let calc = table.calc_formulas();
    match translate(
        formula,
        Operation::Read,
        &Env::new(&env).with_calc(&calc),
        &shape,
        &table.name,
    ) {
        // The database filters, and the caller's own filter, ordering and bound
        // ride along in the same statement.
        Ok(pred) => {
            rows::list_row_values(cat, table, &query.clone().and_filter(pred), ctx.as_ref()).await
        }
        // The formula needs JavaScript. The filter and the ordering still go to
        // the database — the ordering survives because filtering preserves it —
        // but the **bound does not**: a `LIMIT`/`OFFSET` applied before the
        // evaluator has spoken would count rows the caller may not see, and
        // answer "10 rows" with three (or page past rows that were never
        // theirs). So both are applied here, after.
        Err(TranslateError::Untranslatable(_)) => {
            let evaluator = require_evaluator(evaluator)?;
            let fetched = fetch_rows_with_joins(
                cat,
                table,
                formula,
                &shape,
                FetchShape {
                    filter: query.filter.clone(),
                    order: &query.order,
                    extra: &query.extra,
                    context: ctx.as_ref(),
                },
            )
            .await?;
            let limit = query.limit.unwrap_or(u64::MAX);
            let offset = query.offset.unwrap_or(0);
            // A per-partition bound is a bound, so it waits for the evaluator
            // for exactly the same reason: "three employees each" counted over
            // rows the caller may not see is three of somebody else's.
            let mut counted: BTreeMap<String, u64> = BTreeMap::new();
            let mut passed = 0_u64;
            let mut granted = Vec::new();
            for values in fetched {
                if granted.len() as u64 >= limit {
                    break;
                }
                if !allowed(evaluator, formula, Operation::Read, user, &values).await {
                    continue;
                }
                if let Some(part) = &query.partition {
                    let n = counted
                        .entry(rows::group_key(values.get(&part.by)))
                        .or_insert(0);
                    *n += 1;
                    let skip = part.offset.unwrap_or(0);
                    let last = skip.saturating_add(part.limit.unwrap_or(u64::MAX));
                    if *n <= skip || *n > last {
                        continue;
                    }
                }
                passed += 1;
                if passed > offset {
                    granted.push(values);
                }
            }
            Ok(granted)
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether the caller's read of the one row of `table` whose key is `key`
/// returns it — the verdict a live `row` topic is subscribed and re-checked by
/// (TODO.md "Live updates" §3).
///
/// [`read_row_values_as`] with `pk = key`, so it is **the** read rule and not a
/// copy of it: the floor, the ownership formula (translated or reified), and a
/// table's RLS policies in a caller-context transaction, all exactly as the
/// application's REST read applies them. A row that does not exist and a row
/// the caller may not read are both `false` — deliberately one answer — and so
/// is a key that could not be a key of this table at all (`abc` for an integer
/// key names no row). A caller below the floor of a table with no formula is
/// `false` rather than an error, for the same reason.
pub async fn row_readable_as(
    cat: &Catalog,
    table: &Table,
    key: &str,
    role: u8,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
) -> Result<bool> {
    let pk = rows::single_pk(table)?;
    let Ok(filter) = rows::pk_filter(table, &pk, key) else {
        return Ok(false);
    };
    let query = rows::RowQuery {
        filter: Some(filter),
        limit: Some(1),
        ..rows::RowQuery::new()
    };
    match read_row_values_as(cat, table, &query, role, user, evaluator).await {
        Ok(found) => Ok(!found.is_empty()),
        // No floor met and no formula to extend it: nothing here is theirs.
        Err(e) if matches!(e.repr(), sc_error::Repr::Auth(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// One image of a row — as a write left it, or as it was before — held so its
/// read access can be decided for any number of callers (TODO.md "Live
/// updates" §3 rule 5, L3.8).
///
/// The live hub's question about a `table_changes` element is "may *this*
/// subscriber read the row this change carries?", asked once per distinct
/// subscriber and about a row that a read can no longer return: the old image
/// of an update or a delete is gone from the table. So this is §7.3's rule
/// applied to the image itself, the way [`row_allowed`] applies it to a
/// proposed row:
///
/// - a caller who meets `min_role_read` reads it, and nothing is evaluated;
/// - a table with no ownership formula grants nobody else anything;
/// - the formula `owner === user.id` (either way round, `==` too, on a plain
///   column) is a comparison of the column with the caller's id, decided here
///   with no isolate and no I/O — the commonest formula there is;
/// - any other formula is evaluated **reified** in the [`JsEvaluator`] — the
///   reference evaluator, whose verdicts an RLS table's policies and the
///   translated predicate are specified to equal — against the image with its
///   Ⱶ-join and Ↄ-relation values prefetched by
///   [`prefetch_bindings`](sc_catalog::prefetch_bindings). The prefetch is made
///   **once per image**, on the first caller who needs it, and shared by every
///   caller after: the bindings are a function of the row, not of who asks.
///
/// Everything that goes wrong denies, as [`allowed`] does: a formula that no
/// longer validates, a prefetch that fails, a missing evaluator. The error is
/// handed back once so the caller can say so; the verdict is `false` either
/// way, and a failed prefetch is not retried for the next caller.
pub struct RowImage<'a> {
    table: &'a Table,
    values: BTreeMap<String, Value>,
    /// `None` until a caller below the floor needed the formula's bindings;
    /// then whether fetching them worked.
    prepared: Option<std::result::Result<(), String>>,
}

impl<'a> RowImage<'a> {
    /// The image `row` (an event's JSON row) of a row of `table`.
    pub fn new(table: &'a Table, row: &Json) -> RowImage<'a> {
        RowImage {
            table,
            values: rows::row_json_values(table, row),
            prepared: None,
        }
    }

    /// The image's column values, typed by the table's columns.
    pub fn values(&self) -> &BTreeMap<String, Value> {
        &self.values
    }

    /// Whether the formula is one this image is decided for without the
    /// evaluator — the role floor, no formula, or `owner === user.id`.
    pub fn needs_evaluator(&self, role: u8) -> bool {
        role > self.table.access.min_role_read
            && self
                .table
                .ownership
                .as_ref()
                .is_some_and(|formula| owner_column(self.table, formula).is_none())
    }

    /// Whether a caller with `role`, signed in as `user` (or not), may read
    /// this image. `Err` only to say why a denial was not a verdict — the
    /// caller treats it as `false`.
    pub async fn readable(
        &mut self,
        cat: &Catalog,
        role: u8,
        user: Option<&User>,
        evaluator: Option<&Arc<dyn JsEvaluator>>,
    ) -> Result<bool> {
        let table = self.table;
        if role <= table.access.min_role_read {
            return Ok(true);
        }
        let Some(formula) = &table.ownership else {
            return Ok(false);
        };
        if let Some(column) = owner_column(table, formula) {
            return Ok(match user {
                // `user.id` of nobody throws in the evaluator, which denies.
                None => false,
                Some(user) => is_user_id(self.values.get(column), user),
            });
        }
        if self.prepared.is_none() {
            let fetched = async {
                let shape = cat.schema_shape()?;
                let analysis = formula.validate(&shape, &table.name)?;
                prefetch_bindings(cat, table, &analysis, &shape, &mut self.values).await
            }
            .await;
            self.prepared = Some(fetched.map_err(|e| sc_error::format_chain(&e)));
            if let Some(Err(reason)) = &self.prepared {
                return Err(Error::msg(format!(
                    "the values `{}`'s ownership formula reads could not be fetched for a \
                     changed row, so nobody below its read role is sent the change: {reason}",
                    table.name
                )));
            }
        }
        if !matches!(self.prepared, Some(Ok(()))) {
            return Ok(false);
        }
        let evaluator = require_evaluator(evaluator)?;
        Ok(allowed(evaluator, formula, Operation::Read, user, &self.values).await)
    }
}

/// The column `formula` compares with the caller's id when it is exactly
/// `column === user.id` (or `user.id === column`, or `==`), on a plain, stored
/// column of `table` — `None` for every other formula.
fn owner_column<'f>(table: &Table, formula: &'f Formula) -> Option<&'f str> {
    use sc_expr::{Ast, BinaryOp, MemberProp};
    let Ast::Binary { op, l, r } = formula.ast() else {
        return None;
    };
    if !matches!(op, BinaryOp::StrictEq | BinaryOp::Eq) {
        return None;
    }
    let is_user_id = |ast: &Ast| {
        matches!(
            ast,
            Ast::Member { obj, prop: MemberProp::Static(prop), optional: false }
                if prop == COL_ID && matches!(obj.as_ref(), Ast::Ident(name) if name == "user")
        )
    };
    let column = match (l.as_ref(), r.as_ref()) {
        (Ast::Ident(column), other) | (other, Ast::Ident(column)) if is_user_id(other) => column,
        _ => return None,
    };
    let field = table.field(column)?;
    (!field.is_calc()).then_some(column.as_str())
}

/// Whether a column's value is the caller's id, as the evaluator's `===`
/// would find it: the id itself, or text spelling it exactly as the user
/// object does (the hyphenated lower-case form). Null, absent and any other
/// type are not equal to a string.
fn is_user_id(value: Option<&Value>, user: &User) -> bool {
    match value {
        Some(Value::Uuid(id)) => *id == user.id,
        Some(Value::Text(text)) => *text == user.id.hyphenated().to_string(),
        _ => false,
    }
}

/// How this caller's permission to read `table` can be expressed **inside** a
/// statement — the form an aggregate needs, because an aggregate observes rows
/// without returning them.
///
/// A read that returns rows can decide row by row (the reified path); an
/// aggregate cannot. `count(*)` is computed by the database over whatever the
/// `WHERE` leaves, so either the rule is a predicate the `WHERE` can carry or
/// there is no honest answer at all.
pub enum AggregateGuard {
    /// The rule is the database's: `table` has RLS, so its policies decide —
    /// but only inside a caller-context transaction, which the statement
    /// carrying the aggregate therefore has to run in.
    InContext,
    /// The rule is this predicate (`None` for a caller at or above the floor,
    /// who is restricted by nothing).
    Predicate(Option<Expr>),
}

/// The guard restricting an aggregate over `table` to the rows this caller may
/// read, with the table's columns named through `root` — its own name for an
/// aggregate over the table itself, or a subquery alias for a correlated one.
///
/// The two refusals are the point (docs/GRAPHQL_API.md §5):
///
/// - A caller the floor does not admit and no formula extends may not read the
///   table, so they may not count it either.
/// - An **untranslatable** formula refuses the aggregate, naming the table. A
///   formula only the evaluator can decide cannot filter rows inside a
///   statement, and a count over rows the caller cannot see is a leak that a
///   plausible number hides. A refusal is loud; a wrong number is not.
pub fn aggregate_guard(
    cat: &Catalog,
    table: &Table,
    root: &str,
    role: u8,
    user: Option<&User>,
) -> Result<AggregateGuard> {
    if table.rls_enabled {
        return Ok(AggregateGuard::InContext);
    }
    if role <= table.access.min_role_read {
        return Ok(AggregateGuard::Predicate(None));
    }
    let Some(formula) = &table.ownership else {
        return Err(Error::auth(format!("you may not read `{}`", table.name)));
    };
    let shape = cat.schema_shape()?;
    let env = UserEnv::Inline(user_values(user));
    let calc = table.calc_formulas();
    match translate_rooted(
        formula,
        Operation::Read,
        &Env::new(&env).with_calc(&calc),
        &shape,
        &table.name,
        root,
    ) {
        Ok(pred) => Ok(AggregateGuard::Predicate(Some(pred))),
        Err(TranslateError::Untranslatable(what)) => Err(Error::invalid(format!(
            "`{}` cannot be aggregated for you: its ownership formula ({what}) has to be \
             decided row by row, and an aggregate over rows you may not read would be a \
             number nobody can check",
            table.name
        ))),
        Err(e) => Err(e.into()),
    }
}

/// How this caller's permission to read `table` can be carried by a **Ⱶ-join**
/// — a correlated subquery over `table` projected as a column of some *other*
/// table's read.
///
/// The sibling of [`AggregateGuard`], for the other thing a read does inside
/// somebody else's statement. It has one fewer option, and that is the whole
/// point: a join subquery is built by [`join_path_expr`] out of the schema
/// shape, and there is nowhere in it to AND a predicate. So the rule can be
/// carried by the database's own policies or it cannot be carried at all.
pub enum JoinAccess {
    /// The caller meets the table's read floor, so nothing restricts the join.
    Unrestricted,
    /// The table's rule is the database's: its policies decide, inside a
    /// caller-context transaction the enclosing statement therefore has to run
    /// in.
    InContext,
}

/// Whether a Ⱶ-join into `table` may be projected for this caller.
///
/// A joined row is *read*, and the values it yields are the target table's, so
/// the target table's read rule has to hold — the same argument
/// [`read_row_values_as`] makes for a list and [`aggregate_guard`] makes for a
/// count. Two refusals, both naming the table:
///
/// - A caller the floor does not admit and no formula extends may not read the
///   table, so they may not read it through a key either.
/// - A caller whose access comes from an **ownership formula** is refused too,
///   translatable or not. A formula decides *which rows*, and a join subquery
///   has no `WHERE` this provider owns to put that decision in; answering
///   anyway would hand the caller a row they do not own, one column at a time,
///   which is the quietest possible leak. Reading the table directly still
///   works and still applies the formula.
pub fn join_guard(table: &Table, role: u8) -> Result<JoinAccess> {
    if table.rls_enabled {
        return Ok(JoinAccess::InContext);
    }
    if role <= table.access.min_role_read {
        return Ok(JoinAccess::Unrestricted);
    }
    match &table.ownership {
        Some(_) => Err(Error::invalid(format!(
            "`{}` cannot be read through a key by you: your access to it comes from its \
             ownership formula, which decides row by row, and a related row is read as a \
             subquery of another table's query with no room for that decision — query \
             `{}` directly instead",
            table.name, table.name
        ))),
        None => Err(Error::auth(format!("you may not read `{}`", table.name))),
    }
}

/// One aggregate read over `table` — the caller's `aggregates`, over the rows
/// `filter` leaves that this caller may read.
///
/// [`read_row_values_as`]'s sibling for a question about rows rather than about
/// a row, and the same rule: the floor, the formula, or the database's own
/// policies, chosen by [`aggregate_guard`]. What it will not do is fall back to
/// the evaluator — see there.
pub async fn aggregate_values_as(
    cat: &Catalog,
    table: &Table,
    aggregates: Vec<Projection>,
    filter: Option<Expr>,
    role: u8,
    user: Option<&User>,
) -> Result<BTreeMap<String, Value>> {
    let query = rows::RowQuery::new().where_(filter);
    let grouped = aggregate_grouped_as(cat, table, aggregates, &query, role, user).await?;
    Ok(grouped.into_iter().next().unwrap_or_default())
}

/// [`aggregate_values_as`] **per group**: the same rule over
/// [`rows::aggregate_grouped`], answering one row per group.
///
/// The rule does not change with the grouping, and that is worth saying rather
/// than assuming: [`aggregate_guard`] narrows the *rows* the aggregate ranges
/// over — in the `WHERE`, or through the database's own policies — and the
/// `GROUP BY` then partitions whatever is left. So a caller whose access comes
/// from an untranslatable ownership formula is refused here for exactly the
/// reason an ungrouped count is: a per-group number computed over rows it could
/// not filter would be several numbers nobody can check instead of one.
pub async fn aggregate_grouped_as(
    cat: &Catalog,
    table: &Table,
    projections: Vec<Projection>,
    query: &rows::RowQuery,
    role: u8,
    user: Option<&User>,
) -> Result<Vec<BTreeMap<String, Value>>> {
    match aggregate_guard(cat, table, &table.name, role, user)? {
        AggregateGuard::InContext => {
            let ctx = caller_context_at(role, user);
            rows::aggregate_grouped(cat, table, projections, query, Some(&ctx)).await
        }
        AggregateGuard::Predicate(pred) => {
            let query = match pred {
                Some(pred) => query.clone().and_filter(pred),
                None => query.clone(),
            };
            rows::aggregate_grouped(cat, table, projections, &query, None).await
        }
    }
}

/// Insert `body` into `table` as a caller who is **not** an API surface — an
/// agent's `insert_row` tool (§11.3).
///
/// The write half of [`read_rows_as`], and the same argument for its existence:
/// §7.3's rule for a write is *meets `min_role_write` OR the formula grants the
/// proposed row*, checked with `_insert` folded true (§6's WITH CHECK, at
/// runtime), and a second implementation of that is a second place for it to be
/// subtly wrong. The write itself goes through [`rows::create_row_ctx`], so it is
/// coerced, validated, `File`-field-checked and **observed by triggers** exactly
/// like an API caller's.
///
/// A caller the floor does not admit and no formula extends gets an
/// [`Error::auth`] naming the table, rather than an HTTP status: the reader is a
/// tool result a model has to be able to act on.
///
/// `chain` is what led here — the trigger names whose actions caused this write,
/// so the event it raises knows how deep in a cascade it is. A request is not a
/// firing and passes `&[]`; a delegated write from a code body passes the chain
/// of the trigger running it, which is what keeps `Event::firing`'s cascade
/// bound applying to `db.asUser().t.insert(…)` exactly as it does to
/// `db.t.insert(…)`.
///
/// `executor` is where the **write** lands: a workflow step's transaction when
/// this is reached from inside one (§10.3, decision 6), so a delegated write is
/// as much part of the step's atomic advance as an undelegated one, and
/// [`Pooled`](rows::Executor::Pooled) everywhere else. The ownership *checks*
/// above it read through the pool either way — they decide permission and write
/// nothing, and a check that cannot see an uncommitted row denies rather than
/// grants.
#[allow(clippy::too_many_arguments)]
pub async fn insert_row_as(
    cat: &Catalog,
    table: &Table,
    body: &Json,
    role: u8,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    chain: &[String],
    executor: &rows::Executor,
) -> Result<Json> {
    let caller = caller_context_at(role, user).chained(chain.to_vec());
    // The database enforces this table's ownership: the policies decide, and a
    // `WITH CHECK` they refuse surfaces from `run_in_context`.
    if table.rls_enabled {
        return rows::create_row_in(cat, table, body, Some(&caller), executor).await;
    }
    if let Some(formula) = write_formula(table, role, "write")? {
        let proposed = rows::coerce_row_values(table, body)?;
        if !row_allowed(
            cat,
            table,
            formula,
            Operation::Insert,
            user,
            evaluator,
            &proposed,
        )
        .await?
        {
            return Err(Error::auth(format!(
                "this row is outside what you may write to `{}`",
                table.name
            )));
        }
    }
    rows::create_row_in(cat, table, body, Some(&caller), executor).await
}

/// Update the row of `table` addressed by `id`, as a caller who is not an API
/// surface (§11.3). [`insert_row_as`]'s sibling.
///
/// Checked **twice** where it matters, exactly as the REST path is: granted on
/// the existing row (USING) *and* on the row as it would become (WITH CHECK), so
/// an update cannot move a row out of the caller's own ownership. A row the
/// formula withholds is the **same** not-found an absent row gets — a tool must
/// not become a way to probe which rows exist.
///
/// `chain` is what led here, and `executor` where the write lands, as in
/// [`insert_row_as`].
#[allow(clippy::too_many_arguments)]
pub async fn update_row_as(
    cat: &Catalog,
    table: &Table,
    id: &str,
    body: &Json,
    role: u8,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    chain: &[String],
    executor: &rows::Executor,
) -> Result<Json> {
    let caller = caller_context_at(role, user).chained(chain.to_vec());
    if table.rls_enabled {
        return rows::update_row_in(cat, table, id, body, Some(&caller), executor).await;
    }
    let Some(formula) = write_formula(table, role, "write")? else {
        return rows::update_row_in(cat, table, id, body, Some(&caller), executor).await;
    };
    let existing = owned_row(cat, table, formula, Operation::Update, user, evaluator, id).await?;
    let changes = rows::coerce_row_values(table, body)?;
    let merged = merged_row(table, &existing, &changes);
    if !row_allowed(
        cat,
        table,
        formula,
        Operation::Update,
        user,
        evaluator,
        &merged,
    )
    .await?
    {
        return Err(Error::auth(format!(
            "that update would move the row out of what you may reach in `{}`",
            table.name
        )));
    }
    // The translated predicate rides in the UPDATE's WHERE where it can, closing
    // the gap between the check and the write.
    let guard = write_guard(cat, table, formula, Operation::Update, user)?;
    rows::update_row_guarded_in(cat, table, id, body, guard, Some(&caller), executor).await
}

/// Delete the row of `table` addressed by `id`, as a caller who is not an API
/// surface (§11.3). [`insert_row_as`]'s sibling, and the same not-found rule.
///
/// `chain` is what led here, and `executor` where the write lands, as in
/// [`insert_row_as`].
#[allow(clippy::too_many_arguments)]
pub async fn delete_row_as(
    cat: &Catalog,
    table: &Table,
    id: &str,
    role: u8,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    chain: &[String],
    executor: &rows::Executor,
) -> Result<Json> {
    let caller = caller_context_at(role, user).chained(chain.to_vec());
    if table.rls_enabled {
        return rows::delete_row_in(cat, table, id, Some(&caller), executor).await;
    }
    let Some(formula) = write_formula(table, role, "delete from")? else {
        return rows::delete_row_in(cat, table, id, Some(&caller), executor).await;
    };
    owned_row(cat, table, formula, Operation::Delete, user, evaluator, id).await?;
    let guard = write_guard(cat, table, formula, Operation::Delete, user)?;
    rows::delete_row_guarded_in(cat, table, id, guard, Some(&caller), executor).await
}

/// Which formula a sub-floor writer is judged by: `None` when the caller meets
/// `min_role_write` and takes the ordinary path, `Some` when they are below it
/// and the table has one, and an error when they are below it and it has none —
/// which is the same denial the `MinRole` gate would have produced, said in words
/// a tool result can carry.
fn write_formula<'a>(table: &'a Table, role: u8, verb: &str) -> Result<Option<&'a Formula>> {
    if role <= table.access.min_role_write {
        return Ok(None);
    }
    match &table.ownership {
        Some(formula) => Ok(Some(formula)),
        None => Err(Error::auth(format!("you may not {verb} `{}`", table.name))),
    }
}

/// The existing row `id`, granted to the caller by the formula for `op` — or the
/// **same not-found a missing row gets**, which is what makes a denial
/// indistinguishable from an absence. The twin of `RestProvider::owned_row`.
async fn owned_row(
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
    let Some(existing) = fetch_row_values(cat, table, formula, id).await? else {
        return not_found();
    };
    if !row_allowed(cat, table, formula, op, user, evaluator, &existing).await? {
        return not_found();
    }
    Ok(existing)
}

/// Whether the formula grants `op` on the row `values` — the single-row check
/// every sub-floor write (and file access) goes through. Join values missing
/// from `values` (a proposed row that has not been stored) are resolved
/// link-by-link first.
pub(crate) async fn row_allowed(
    cat: &Catalog,
    table: &Table,
    formula: &Formula,
    op: Operation,
    user: Option<&User>,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    values: &BTreeMap<String, Value>,
) -> Result<bool> {
    let evaluator = require_evaluator(evaluator)?;
    let shape = cat.schema_shape()?;
    let analysis = formula.validate(&shape, &table.name)?;
    let mut values = values.clone();
    // The join/relation values the evaluator needs but the row does not carry —
    // resolved by `sc_catalog::prefetch_bindings`, which a trigger's `only_if`
    // (Phase 2) shares, so there is one implementation of "what does a reified
    // formula get bound".
    prefetch_bindings(cat, table, &analysis, &shape, &mut values).await?;
    Ok(allowed(evaluator, formula, op, user, &values).await)
}

/// The translated guard predicate for a write, when the formula is
/// translatable — ANDed into the UPDATE/DELETE WHERE so the row cannot change
/// hands between the check and the write. `None` for an untranslatable
/// formula, whose single-row reified check already ran.
pub(crate) fn write_guard(
    cat: &Catalog,
    table: &Table,
    formula: &Formula,
    op: Operation,
    user: Option<&User>,
) -> Result<Option<Expr>> {
    let shape = cat.schema_shape()?;
    let env = UserEnv::Inline(user_values(user));
    let calc = table.calc_formulas();
    match translate(
        formula,
        op,
        &Env::new(&env).with_calc(&calc),
        &shape,
        &table.name,
    ) {
        Ok(pred) => Ok(Some(pred)),
        Err(TranslateError::Untranslatable(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Fetch the full row addressed by `id` — its columns plus every Ⱶ-join value
/// the formula reads, projected in the same query. `None` when no such row.
pub(crate) async fn fetch_row_values(
    cat: &Catalog,
    table: &Table,
    formula: &Formula,
    id: &str,
) -> Result<Option<BTreeMap<String, Value>>> {
    let shape = cat.schema_shape()?;
    let pk = rows::single_pk(table)?;
    let filter = rows::pk_filter(table, &pk, id)?;
    let mut fetched = fetch_rows_with_joins(
        cat,
        table,
        formula,
        &shape,
        FetchShape {
            filter: Some(filter),
            ..FetchShape::default()
        },
    )
    .await?;
    Ok(fetched.drain(..).next())
}

/// The proposed row of an update: the existing row with the body's coerced
/// values written over it — what §6's WITH CHECK sees, computed at runtime.
/// Stale join values are dropped so [`row_allowed`] re-resolves them against
/// the (possibly changed) foreign keys.
pub(crate) fn merged_row(
    table: &Table,
    existing: &BTreeMap<String, Value>,
    changes: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let mut merged: BTreeMap<String, Value> = existing
        .iter()
        .filter(|(name, _)| table.field(name).is_some())
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    for (name, value) in changes {
        merged.insert(name.clone(), value.clone());
    }
    merged
}

/// Strip a fetched values map back to the table's own columns and render it as
/// the wire row — the projected join values are enforcement inputs, not
/// response payload.
pub(crate) fn table_row_json(table: &Table, values: &BTreeMap<String, Value>) -> Json {
    let mut map = Map::new();
    for field in &table.fields {
        if let Some(value) = values.get(&field.base.name) {
            map.insert(field.base.name.clone(), value_to_json(value));
        }
    }
    Json::Object(map)
}

/// Evaluate the formula on one row. An `Err` from the evaluator **denies**, per
/// its contract — a throwing or timed-out formula must never grant. The reason
/// is not surfaced per row (a filtered list cannot carry one); the formula
/// author sees the behaviour, and the §16 error log is where the reason will
/// land once it exists.
async fn allowed(
    evaluator: &Arc<dyn JsEvaluator>,
    formula: &Formula,
    op: Operation,
    user: Option<&User>,
    values: &BTreeMap<String, Value>,
) -> bool {
    let call = FormulaCall {
        formula: formula.clone(),
        op,
        row: values.clone(),
        user: user_values(user),
        // An ownership formula has no triggering event, so no `row`/`old` is in
        // scope — validation refuses naming one (§7.3's scope is the row's own
        // fields, `user` and the flags).
        ambient: Default::default(),
    };
    evaluator.eval(call).await.unwrap_or(false)
}

/// Rows of `table` (optionally filtered) with one extra projected column per
/// Ⱶ-join path the formula uses, aliased as the join identifier itself — the
/// correlated subselect from the symbolic translator, reused as a projection.
/// One query serves both the row data and the evaluator's bindings.
/// The rest of the read [`fetch_rows_with_joins`] performs, beside the formula's
/// own join projections — grouped because they travel together and because a
/// list of eight positional arguments is a list nobody reads.
#[derive(Default)]
struct FetchShape<'a> {
    /// `WHERE`, if the caller has one.
    filter: Option<Expr>,
    /// `ORDER BY`. The bound is deliberately *not* here: it cannot be applied
    /// before the evaluator has spoken (see the caller).
    order: &'a [OrderBy],
    /// Extra projections of the caller's own, in the same statement.
    extra: &'a [Projection],
    /// The caller context, when one of `extra` reaches an RLS-protected table.
    context: Option<&'a CallerContext>,
}

async fn fetch_rows_with_joins(
    cat: &Catalog,
    table: &Table,
    formula: &Formula,
    shape: &sc_expr::SchemaShape,
    read: FetchShape<'_>,
) -> Result<Vec<BTreeMap<String, Value>>> {
    let FetchShape {
        filter,
        order,
        extra,
        context,
    } = read;
    let analysis = formula.validate(shape, &table.name)?;
    let mut columns = vec![Projection::all()];
    for path in &analysis.join_paths {
        let expr = join_path_expr(shape, &table.name, &path.ident).map_err(Error::from)?;
        columns.push(Projection::expr_as(expr, path.ident.clone()));
    }
    // The caller's own extra projections ride in the same statement as the
    // formula's; a GraphQL Ⱶ-join and an ownership Ⱶ-join are the same kind of
    // column and there is no reason for the reified path to cost two queries.
    columns.extend(extra.iter().cloned());
    let mut select = Select::from(Source::table(table.name.clone())).columns(columns);
    if let Some(filter) = filter {
        select = select.filter(filter);
    }
    select.order = order.to_vec();
    // `context` is `Some` only when one of `extra`'s projections reaches an
    // RLS-protected table; the row layer decides what to do with it, so this
    // path cannot disagree with the symbolic one about when a statement is a
    // caller-context statement.
    let fetched: Vec<Row> = rows::run_read(cat, table, &select, context, context.is_some()).await?;
    Ok(fetched.iter().map(crate::rows::row_values).collect())
}

/// The evaluator, or the loud configuration error. Fail closed: a deployment
/// without an engine cannot run formulas, and the answer is "fix the server",
/// never "let the request through".
fn require_evaluator(evaluator: Option<&Arc<dyn JsEvaluator>>) -> Result<&Arc<dyn JsEvaluator>> {
    evaluator.ok_or_else(|| {
        Error::config(
            "this table's ownership formula needs the JavaScript evaluator, \
             and none is configured on this server",
        )
    })
}
