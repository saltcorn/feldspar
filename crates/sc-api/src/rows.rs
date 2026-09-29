//! Table row CRUD, shared by every API surface (design §13.1, §13.4).
//!
//! Reading and writing a table's rows as JSON is the substance of *both* the
//! admin API's row endpoints and an application's REST API: same tables, same
//! query layer, same wire shape. So it lives here once, below both, rather than
//! in either. `sc-server`'s admin handlers call these functions, and so does
//! [`RestProvider`](crate::RestProvider) — which is what makes "the app's API and
//! the admin API are the same machinery" true in the code and not just in the
//! docs.
//!
//! Rows cross the wire as plain JSON objects keyed by column name;
//! [`crate::convert`] bridges those to the query layer's [`Value`]. A row is
//! addressed by its **single-column primary key** (composite keys are post-MVP).
//! These functions do no authorization of their own — the caller enforces the
//! endpoint's [`AuthRequirement`](crate::AuthRequirement) first, so every API
//! surface goes through the same §7 layer.

use crate::calc_read::CalcPlan;
use sc_catalog::{CallerContext, Catalog, DataFieldKind, SharedTx, Table, TableWrite, WriteOp};
use sc_db::Row;
use sc_error::{Error, Repr, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, OrderBy, Projection, Select, Source, Statement,
    Update, Value,
};
use sc_types::{BasicType, TypeRef};
use serde_json::{Map, Value as Json};

use crate::convert::{json_to_value, value_to_json};
use crate::user_rows;

/// Where a row operation's statement actually runs.
///
/// Almost always [`Pooled`](Executor::Pooled): the table's provider takes a
/// connection from the pool, or — on a table with row-level security — a
/// one-statement transaction that sets the caller GUCs first. The exceptions are
/// the two callers that hold **one transaction open across many writes**: a CSV
/// import (§13.1), whose foreign-key checks are deferred to its commit and whose
/// reads must see the rows it has already written, and a **workflow step**
/// (§10.3, decision 6), whose writes commit with the run's advance or not at all.
///
/// It is an explicit parameter rather than something hidden in the context
/// because it changes when a write becomes visible and when it becomes
/// permanent. Two consequences the caller owns: **events fire as the rows are
/// written**, before the commit (a trigger reading the table on another
/// connection will not see them yet), and a rollback undoes rows whose events
/// have already gone out.
#[derive(Clone, Default)]
pub enum Executor {
    /// The ordinary path: the table's provider, or an RLS caller-context
    /// transaction when the table has policies.
    #[default]
    Pooled,
    /// A transaction the caller opened and will finish. Every statement for a
    /// table that transaction [`serves`](sc_catalog::SharedTx::serves) joins it,
    /// carrying its own caller — the GUCs are re-applied per writer rather than
    /// set once, because a shared transaction has more than one (§10.3).
    ///
    /// A table it does **not** serve — one on another database connection, or one
    /// a module provides — takes the pooled path instead: those rows are not
    /// reachable from this transaction, and pretending otherwise would write them
    /// somewhere else entirely.
    Transaction(SharedTx),
}

impl Executor {
    /// The executor for an optional transaction: the caller's when it has one,
    /// [`Pooled`](Executor::Pooled) when it does not.
    ///
    /// Every caller that *may* be inside a step transaction spells the choice
    /// this way, so "am I in one?" is asked once, here, instead of at each of the
    /// row layer's entry points.
    pub fn of(tx: Option<SharedTx>) -> Executor {
        match tx {
            Some(tx) => Executor::Transaction(tx),
            None => Executor::Pooled,
        }
    }

    /// The transaction this executor runs `table`'s statements in, if any: `None`
    /// for the pooled path, and `None` for a table the transaction does not
    /// serve.
    fn serving(&self, table: &Table) -> Option<&SharedTx> {
        match self {
            Executor::Pooled => None,
            Executor::Transaction(tx) => tx.serves(table).then_some(tx),
        }
    }
}

/// Every row of `table`, as a JSON array of objects.
pub async fn list_rows(catalog: &Catalog, table: &Table) -> Result<Json> {
    list_rows_where(catalog, table, None, None).await
}

/// Every row of `table`, optionally through an RLS caller context — the entry
/// point the admin API uses so its own row viewer works on an RLS-enabled
/// (FORCE'd) table: role 1 clears every policy's role floor, so an admin sees
/// and edits everything, while a non-RLS table takes the ordinary path.
pub async fn list_rows_ctx(
    catalog: &Catalog,
    table: &Table,
    context: Option<&CallerContext>,
) -> Result<Json> {
    list_rows_where(catalog, table, None, context).await
}

/// [`update_row`] optionally through an RLS caller context (admin API).
pub async fn update_row_ctx(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    body: &Json,
    context: Option<&CallerContext>,
) -> Result<Json> {
    update_row_guarded(catalog, table, id, body, None, context).await
}

/// [`delete_row`] optionally through an RLS caller context (admin API).
pub async fn delete_row_ctx(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    context: Option<&CallerContext>,
) -> Result<Json> {
    delete_row_guarded(catalog, table, id, None, context).await
}

/// The rows of `table` matching `filter` (all of them for `None`), as a JSON
/// array. The filter is how ownership enforcement (§7.3) narrows a read to the
/// rows a formula grants — ANDed in by the caller as a translated predicate.
///
/// `context` routes the read through an RLS caller-context transaction (§7.3)
/// when the table's ownership is enforced by the database; `None` runs it on a
/// pooled connection through the provider, as every non-RLS read does.
pub async fn list_rows_where(
    catalog: &Catalog,
    table: &Table,
    filter: Option<Expr>,
    context: Option<&CallerContext>,
) -> Result<Json> {
    list_rows_query(catalog, table, &RowQuery::new().where_(filter), context).await
}

/// Which rows of a table to read, in what order, and how many at most.
///
/// The shape of a read that is not "everything": a filter, an ordering and a
/// bound. It exists because an agent's `query_table` tool (§11.3) asks for
/// exactly those three and must not be able to ask for anything else — a tool
/// that could name its own projection or its own SQL would be a second row layer
/// with none of this one's rules. Every field is optional, so
/// [`RowQuery::new`] is "every row, in no particular order".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowQuery {
    /// `WHERE`, if any. Ownership enforcement ANDs its own predicate into this.
    pub filter: Option<Expr>,
    /// `ORDER BY` keys, in precedence order.
    pub order: Vec<OrderBy>,
    /// The most rows to return.
    pub limit: Option<u64>,
    /// How many rows to skip first.
    pub offset: Option<u64>,
    /// Extra projections computed in the **same** `SELECT` as the row, each
    /// aliased to the name the caller will read the value back by.
    ///
    /// What a GraphQL `manager { email }` lowers to (§13.4): the Ⱶ-join's
    /// correlated subquery, projected as another column of this query rather
    /// than fetched in a second one. It is the same trick ownership's reified
    /// path already uses for the join values a formula reads — one query serves
    /// the row and everything derived from it.
    pub extra: Vec<Projection>,
    /// A bound applied **within each group** of rows rather than to the read as
    /// a whole. `None` for every ordinary read.
    pub partition: Option<Partition>,
    /// `GROUP BY` keys, for an **aggregate** read ([`aggregate_grouped`]): one
    /// answer row per distinct combination of these.
    ///
    /// Empty for every row read, and necessarily so — a row read projects the
    /// table's own columns, and `SELECT *, count(*) … GROUP BY x` is not a
    /// statement. Grouping lives on this struct rather than beside the
    /// aggregate's projections because the rest of the question is the same one
    /// a row read asks — which rows, in what order, how many at most — and a
    /// second struct for it would be that question answered twice.
    pub group: Vec<Expr>,
    /// `HAVING`: the bound on the **groups** rather than on the rows, so it is
    /// written in terms of the aggregates and not of the columns. `None` for
    /// every read that is not a grouped aggregate.
    pub having: Option<Expr>,
    /// Whether this read must run inside a caller-context transaction even
    /// though its own table is not protected by row-level security.
    ///
    /// Ordinarily the **table** decides (see `in_context`): its policies are the
    /// only thing a `SET LOCAL` here would be read by. But an [`extra`](Self::extra)
    /// projection can be a correlated subquery over a *different* table — a
    /// GraphQL `employees_aggregate { count }` inside a `departments` read
    /// (§13.4) — and if that table is RLS-protected, its policies apply to the
    /// subquery and see whatever an unset GUC makes of them: no caller, so no
    /// rows, so a count of zero nobody would investigate. Setting this makes the
    /// statement a caller-context one so the child's policies decide as they
    /// would in a read of their own.
    ///
    /// It only ever *adds* a transaction; it never removes a table's own.
    pub in_caller_context: bool,
    /// Leave out the calculated fields computed **after** the read
    /// (milestone 31 §4): only what the `SELECT` itself computes comes back.
    ///
    /// For one caller, a model's dataset read, and for a reason that is not
    /// an optimisation. A dataset reads its own columns, not the table's
    /// calculated fields — and a field of that table that calls
    /// `predict("…")` of a model over it would read that model's dataset to
    /// compute itself, which would compute the field again, forever.
    pub sql_only: bool,
}

/// At most `limit` rows for each distinct value of `by`, after skipping
/// `offset` of them, in the query's own order.
///
/// The one thing a batched child list needs that `LIMIT` cannot express: one
/// `SELECT` answers `employees(limit: 3)` for *every* department at once, and
/// "three each" is not "three". It lowers to `row_number() OVER (PARTITION BY
/// … ORDER BY …)` numbered inside the read and filtered outside it — after the
/// ownership predicate has been ANDed in, so the numbering never counts a row
/// the caller may not see.
#[derive(Debug, Clone, PartialEq)]
pub struct Partition {
    /// The column the rows are grouped by — a child table's key, in the one
    /// case that needs this.
    pub by: String,
    /// The most rows to return per group.
    pub limit: Option<u64>,
    /// How many rows to skip in each group first.
    pub offset: Option<u64>,
}

/// The alias the per-partition row number rides back under. Structural — chosen
/// here, never user data — and prefixed so it cannot be a column of a table an
/// admin declared.
const PARTITION_ROW_NUMBER: &str = "_fd_rn";

/// The alias the partitioned read's inner query is exposed under. Same rule.
const PARTITION_SOURCE: &str = "_fd_part";

impl RowQuery {
    /// Every row, unordered and unbounded.
    pub fn new() -> RowQuery {
        RowQuery::default()
    }

    /// Restrict to the rows matching `filter` (`None` leaves it unrestricted).
    pub fn where_(mut self, filter: Option<Expr>) -> RowQuery {
        self.filter = filter;
        self
    }

    /// Order by these keys.
    pub fn order_by(mut self, order: Vec<OrderBy>) -> RowQuery {
        self.order = order;
        self
    }

    /// Return at most `n` rows.
    pub fn limit(mut self, n: u64) -> RowQuery {
        self.limit = Some(n);
        self
    }

    /// Skip the first `n` rows.
    pub fn offset(mut self, n: u64) -> RowQuery {
        self.offset = Some(n);
        self
    }

    /// Project `extra` alongside the row's own columns.
    pub fn projecting(mut self, extra: Vec<Projection>) -> RowQuery {
        self.extra = extra;
        self
    }

    /// Bound the rows *within each group* of `partition` rather than as a whole.
    pub fn per_partition(mut self, partition: Partition) -> RowQuery {
        self.partition = Some(partition);
        self
    }

    /// Group an [`aggregate_grouped`] read by these keys — one answer row per
    /// distinct combination.
    pub fn group_by(mut self, group: Vec<Expr>) -> RowQuery {
        self.group = group;
        self
    }

    /// Keep only the groups this predicate admits (`HAVING`).
    pub fn having(mut self, having: Option<Expr>) -> RowQuery {
        self.having = having;
        self
    }

    /// Require this read to run inside a caller-context transaction — see
    /// [`in_caller_context`](Self::in_caller_context). `false` leaves the
    /// decision where it normally lives, with the table.
    pub fn requiring_caller_context(mut self, required: bool) -> RowQuery {
        self.in_caller_context = required;
        self
    }

    /// Leave out the calculated fields computed after the read — see
    /// [`sql_only`](Self::sql_only).
    pub fn sql_only(mut self) -> RowQuery {
        self.sql_only = true;
        self
    }

    /// This query with `extra` ANDed into its filter — how an ownership
    /// predicate joins a caller's own one without either being able to drop the
    /// other.
    pub fn and_filter(mut self, extra: Expr) -> RowQuery {
        self.filter = Some(match self.filter {
            Some(existing) => existing.and(extra),
            None => extra,
        });
        self
    }
}

/// [`list_rows_where`] with an ordering and a bound: the general read.
pub async fn list_rows_query(
    catalog: &Catalog,
    table: &Table,
    query: &RowQuery,
    context: Option<&CallerContext>,
) -> Result<Json> {
    let plan = CalcPlan::of(catalog, table)?;
    let rows = run_read(
        catalog,
        table,
        &planned_select(table, &plan, query)?,
        context,
        query.in_caller_context,
    )
    .await?;
    let rows = match query.sql_only {
        true => rows,
        false => plan.complete(catalog, table, rows).await?,
    };
    Ok(Json::Array(rows.iter().map(row_to_json).collect()))
}

/// [`list_rows_query`] as **query values** keyed by column rather than as the
/// JSON wire shape — the same statement, the same RLS routing, read by a caller
/// that needs the values typed.
///
/// A GraphQL read is that caller: `Decimal` must reach the wire exact rather
/// than through a JSON number, and the [`RowQuery::extra`] projections it adds
/// are not columns of the table, so the JSON renderer would drop them.
pub async fn list_row_values(
    catalog: &Catalog,
    table: &Table,
    query: &RowQuery,
    context: Option<&CallerContext>,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    list_row_values_in(catalog, table, query, context, &Executor::Pooled).await
}

/// [`list_row_values`] on a caller's [`Executor`] — the same read, inside the
/// caller's transaction, which is the only way to see what that transaction has
/// written but not yet committed.
pub async fn list_row_values_in(
    catalog: &Catalog,
    table: &Table,
    query: &RowQuery,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    let plan = CalcPlan::of(catalog, table)?;
    let rows = run_read_in(
        catalog,
        table,
        &planned_select(table, &plan, query)?,
        context,
        query.in_caller_context,
        executor,
    )
    .await?;
    let rows = match query.sql_only {
        true => rows,
        false => plan.complete(catalog, table, rows).await?,
    };
    Ok(rows.iter().map(row_values).collect())
}

/// The one row an **aggregate** read returns: `aggregates` projected over the
/// rows of `table` that `filter` leaves, with no `GROUP BY`.
///
/// The counterpart of [`list_row_values`] for a question about rows rather than
/// about a row. It is a separate function because an aggregate `SELECT` is not a
/// row read with extra columns: `SELECT *, count(*)` is not a legal statement,
/// and the calculated fields have nothing to compute over here. Every projection
/// is the caller's, aliased by the key it will be read back under — the
/// expressions themselves come from `sc_expr`'s shared builder, so the wire and
/// a formula answer the same question the same way.
///
/// A scalar aggregate always has exactly one row — it is
/// [`aggregate_grouped`] with nothing to group by — and an empty result would
/// mean the database answered something else; the empty map that comes back then
/// resolves as nulls rather than as invented zeroes.
pub async fn aggregate_values(
    catalog: &Catalog,
    table: &Table,
    aggregates: Vec<Projection>,
    filter: Option<Expr>,
    context: Option<&CallerContext>,
) -> Result<std::collections::BTreeMap<String, Value>> {
    let query = RowQuery::new().where_(filter);
    let grouped = aggregate_grouped(catalog, table, aggregates, &query, context).await?;
    Ok(grouped.into_iter().next().unwrap_or_default())
}

/// [`aggregate_values`] **per group**: `projections` computed over the rows
/// `query` leaves, once for each distinct combination of its
/// [`group`](RowQuery::group) keys, in the query's own order and bound.
///
/// One row per group rather than one row full stop, which is the only difference
/// — and the reason it is the general function and the scalar one is sugar for
/// it: `db.orders.groupBy("customer").aggregate({ n: "count()" }).rows()` and
/// `db.orders.count()` are the same statement with and without a `GROUP BY`, and
/// two builders for that would be two places for the filter, the ordering or the
/// caller context to be applied differently.
///
/// The grouping keys are `query.group`, and the caller projects them too if it
/// wants them in the answer: a group key is an expression, and only the caller
/// knows the name it wants to read the value back under.
pub async fn aggregate_grouped(
    catalog: &Catalog,
    table: &Table,
    projections: Vec<Projection>,
    query: &RowQuery,
    context: Option<&CallerContext>,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    // A count or a sum over a filter the row read would refuse is refused the
    // same way, so a grid's count and its rows cannot disagree about why.
    CalcPlan::of(catalog, table)?.refuse_query(table, query.filter.as_ref(), &query.order)?;
    let mut select = Select::from(Source::table(table.name.clone())).columns(projections);
    select.filter = query.filter.clone();
    select.group = query.group.clone();
    select.having = query.having.clone();
    select.order = query.order.clone();
    select.limit = query.limit;
    select.offset = query.offset;
    let rows = run_read(catalog, table, &select, context, query.in_caller_context).await?;
    Ok(rows.iter().map(row_values).collect())
}

/// The alias `count_rows` reads its one value back under. Structural — chosen
/// here, never user data — so it cannot collide with a column an admin declared.
const ROW_COUNT_KEY: &str = "_fd_count";

/// How many rows `table` has, as [`aggregate_values`] answers it.
///
/// A count rather than the length of a listing: the admin's table page shows
/// this beside the link to the rows, and reading every row of a large table to
/// find out how many there are would make the page cost what the data costs.
/// It goes through the same read path, so an RLS-enforced table counts the rows
/// the caller may see rather than the rows that exist.
pub async fn count_rows(
    catalog: &Catalog,
    table: &Table,
    context: Option<&CallerContext>,
) -> Result<i64> {
    count_rows_where(catalog, table, None, context).await
}

/// [`count_rows`] over the rows a filter leaves.
///
/// The admin grid's scrollbar is as long as this number, and the rows it scrolls
/// are what `list_rows_query` returns for the same filter — so the two take the
/// same predicate, from the same parse of the same query string, rather than
/// counting one population and paging another.
pub async fn count_rows_where(
    catalog: &Catalog,
    table: &Table,
    filter: Option<Expr>,
    context: Option<&CallerContext>,
) -> Result<i64> {
    let count = sc_expr::aggregate_expr(&sc_expr::AggFunc::Count, false, None, &table.name)?;
    let values = aggregate_values(
        catalog,
        table,
        vec![Projection::expr_as(count, ROW_COUNT_KEY)],
        filter,
        context,
    )
    .await?;
    match values.get(ROW_COUNT_KEY) {
        Some(Value::Int(n)) => Ok(*n),
        // `count(*)` is `bigint` in Postgres and an integer everywhere else, so
        // anything but an int here means the driver mapped it to something this
        // does not know about rather than that the table is empty.
        other => Err(Error::msg(format!(
            "`count(*)` over `{}` came back as {other:?}",
            table.name
        ))),
    }
}

/// The `SELECT` one [`RowQuery`] renders to: every column plus the calculated
/// fields SQL can compute and the query's own extra projections, filtered,
/// ordered and bounded.
///
/// A calculated field computed **after** the read is not in it, and the
/// caller that runs it completes the rows with [`CalcPlan::complete`]; one
/// that only renders it (a code body's query plan) gets the statement the
/// database would run.
pub(crate) fn read_select(catalog: &Catalog, table: &Table, query: &RowQuery) -> Result<Select> {
    planned_select(table, &CalcPlan::of(catalog, table)?, query)
}

/// [`read_select`] with the calculated fields already planned.
fn planned_select(table: &Table, plan: &CalcPlan, query: &RowQuery) -> Result<Select> {
    plan.refuse_query(table, query.filter.as_ref(), &query.order)?;
    let mut columns = user_rows::projection(table);
    columns.extend(plan.projections.iter().cloned());
    columns.extend(query.extra.iter().cloned());
    let Some(partition) = &query.partition else {
        let mut select = Select::from(Source::table(table.name.clone())).columns(columns);
        if let Some(filter) = query.filter.clone() {
            select = select.filter(filter);
        }
        select.order = query.order.clone();
        select.limit = query.limit;
        select.offset = query.offset;
        return Ok(select);
    };

    // A per-group bound is not a `LIMIT`: the rows have to be *numbered* first,
    // inside the same read that the filter (and so the ownership predicate)
    // applies to, and the numbering compared afterwards. Hence the wrap.
    columns.push(Projection::expr_as(
        Expr::row_number(vec![Expr::col(partition.by.clone())], query.order.clone()),
        PARTITION_ROW_NUMBER,
    ));
    let mut inner = Select::from(Source::table(table.name.clone())).columns(columns);
    if let Some(filter) = query.filter.clone() {
        inner = inner.filter(filter);
    }
    let mut select = Select::from(Source::Subquery {
        query: Box::new(inner),
        alias: PARTITION_SOURCE.to_owned(),
    });
    let rn = || Expr::col(PARTITION_ROW_NUMBER);
    let skip = partition.offset.unwrap_or(0);
    let mut bound = (skip > 0).then(|| Expr::binary(BinOp::Gt, rn(), Expr::lit(skip as i64)));
    if let Some(take) = partition.limit {
        let last = skip.saturating_add(take);
        let within = Expr::binary(BinOp::Le, rn(), Expr::lit(last as i64));
        bound = Some(match bound {
            Some(b) => b.and(within),
            None => within,
        });
    }
    select.filter = bound;
    select.order = query.order.clone();
    select.limit = query.limit;
    select.offset = query.offset;
    Ok(select)
}

/// The text a row (or a caller looking one up) is grouped by on one column.
///
/// [`Value`] is not `Hash` — it carries floats and decimals — and a key column
/// is small, so grouping goes through its `Debug` rendering, which distinguishes
/// `Int(1)` from `Text("1")` and so cannot collapse two different keys into one
/// bucket. An absent column groups as `NULL`, which is where it belongs: a child
/// row whose key is null belongs to no parent.
pub(crate) fn group_key(value: Option<&Value>) -> String {
    format!("{:?}", value.unwrap_or(&Value::Null))
}

/// One fetched row as a map from column name to its value.
pub(crate) fn row_values(row: &Row) -> std::collections::BTreeMap<String, Value> {
    row.columns()
        .iter()
        .cloned()
        .zip(row.values().iter().cloned())
        .collect()
}

/// The rows of `table` matching `filter` as **query values** keyed by column,
/// including any non-stored calculated field.
///
/// The evaluation-side counterpart of [`list_rows_where`]: same rows, same RLS
/// routing, but as the `Value` map a formula is evaluated against rather than the
/// JSON wire shape. This is what an action's `where` predicate selects with — it
/// needs the values *typed* to bind them, and it needs the calculated fields
/// because a formula may read one.
pub async fn select_values(
    catalog: &Catalog,
    table: &Table,
    filter: Option<Expr>,
    context: Option<&CallerContext>,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    select_values_in(catalog, table, filter, context, &Executor::Pooled).await
}

/// [`select_values`] on a caller's [`Executor`] — a read inside the caller's
/// transaction, which is the only way to see the rows that transaction has
/// written but not yet committed.
pub async fn select_values_in(
    catalog: &Catalog,
    table: &Table,
    filter: Option<Expr>,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    let plan = CalcPlan::of(catalog, table)?;
    plan.refuse_query(table, filter.as_ref(), &[])?;
    let mut columns = user_rows::projection(table);
    columns.extend(plan.projections.iter().cloned());
    let mut select = Select::from(Source::table(table.name.clone())).columns(columns);
    if let Some(filter) = filter {
        select = select.filter(filter);
    }
    let fetched = run_read_in(catalog, table, &select, context, false, executor).await?;
    let fetched = plan.complete(catalog, table, fetched).await?;
    Ok(fetched.iter().map(row_values).collect())
}

/// Insert a row from a JSON object, returning the inserted row (with any
/// database-generated columns filled in).
pub async fn create_row(catalog: &Catalog, table: &Table, body: &Json) -> Result<Json> {
    create_row_ctx(catalog, table, body, None).await
}

/// [`create_row`] routed through an RLS caller context (§7.3) when one is
/// given — the insert runs in a transaction with the caller's role/identity
/// set, so an `INSERT` a policy's `WITH CHECK` rejects surfaces as the same
/// not-found a missing row gets.
pub async fn create_row_ctx(
    catalog: &Catalog,
    table: &Table,
    body: &Json,
    context: Option<&CallerContext>,
) -> Result<Json> {
    create_row_in(catalog, table, body, context, &Executor::Pooled).await
}

/// [`create_row_ctx`] on a caller's [`Executor`] — the same insert, the same
/// coercion, the same event, run wherever the caller says.
pub async fn create_row_in(
    catalog: &Catalog,
    table: &Table,
    body: &Json,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Json> {
    let obj = require_object(body)?;
    reject_calc_writes(table, obj)?;
    user_rows::check_insert(table, obj, context)?;
    let mut columns = Vec::with_capacity(obj.len());
    let mut values = Vec::with_capacity(obj.len());
    for (key, json) in obj {
        let value = column_value(table, key, json)?;
        validate_file_write(catalog, table, key, &value)?;
        columns.push(key.clone());
        values.push(Expr::lit(value));
    }
    if columns.is_empty() {
        return Err(Error::invalid("no fields to insert"));
    }
    let plan = CalcPlan::of(catalog, table)?;
    let mut returning = user_rows::projection(table);
    returning.extend(plan.projections.iter().cloned());
    let insert = Insert::row(table.name.clone(), columns, values).returning(returning);
    let rows = run_write_in(catalog, table, Statement::from(insert), context, executor).await?;
    let rows = complete_written(catalog, table, &plan, rows, executor).await?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found("the insert was refused"))?;
    let row = row_to_json(&row);
    emit(
        catalog,
        table,
        WriteOp::Insert,
        &row,
        None,
        context,
        executor,
    )
    .await;
    Ok(row)
}

/// Update the row of `table` whose primary key is `id`, returning the updated
/// row. The primary key addresses the row and is not reassignable through the
/// body.
pub async fn update_row(catalog: &Catalog, table: &Table, id: &str, body: &Json) -> Result<Json> {
    update_row_guarded(catalog, table, id, body, None, None).await
}

/// [`update_row`] with an extra `guard` predicate ANDed into the WHERE —
/// ownership enforcement's translated formula (§7.3) — and an optional RLS
/// caller `context`. A row the guard (or a policy) excludes produces the
/// **same** not-found as a row that is not there: a caller must not be able to
/// probe which rows exist beyond the ones they may reach.
pub(crate) async fn update_row_guarded(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    body: &Json,
    guard: Option<Expr>,
    context: Option<&CallerContext>,
) -> Result<Json> {
    update_row_guarded_in(catalog, table, id, body, guard, context, &Executor::Pooled).await
}

/// [`update_row_ctx`] on a caller's [`Executor`] — see [`create_row_in`].
pub async fn update_row_in(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    body: &Json,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Json> {
    update_row_guarded_in(catalog, table, id, body, None, context, executor).await
}

pub(crate) async fn update_row_guarded_in(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    body: &Json,
    guard: Option<Expr>,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Json> {
    let obj = require_object(body)?;
    reject_calc_writes(table, obj)?;
    let guard = user_rows::and_guard(guard, user_rows::update_guard(table, id, obj, context)?);
    let pk = single_pk(table)?;
    let mut assignments = Vec::with_capacity(obj.len());
    for (key, json) in obj {
        if key == &pk {
            continue;
        }
        let value = column_value(table, key, json)?;
        validate_file_write(catalog, table, key, &value)?;
        assignments.push(Assignment::new(key.clone(), Expr::lit(value)));
    }
    if assignments.is_empty() {
        return Err(Error::invalid("no fields to update"));
    }
    // The pre-image, for the event's `old_row` — read **only when something
    // listens** (§10.2's emit seam), so an update on a table with no update
    // trigger costs exactly what it always did. There is no transaction around
    // the pair: dispatch is after-commit by design (decision 1), and a row that
    // changed in between is a race the event reports rather than prevents.
    let old_row = match catalog.observes_writes(&table.name, WriteOp::Update) {
        true => read_row_in(catalog, table, &pk, id, context, executor).await?,
        false => None,
    };
    let plan = CalcPlan::of(catalog, table)?;
    let mut returning = user_rows::projection(table);
    returning.extend(plan.projections.iter().cloned());
    let update = Update {
        table: table.name.clone(),
        assignments,
        filter: Some(guarded_filter(table, &pk, id, guard)?),
        returning,
    };
    let rows = run_write_in(catalog, table, Statement::from(update), context, executor).await?;
    let rows = complete_written(catalog, table, &plan, rows, executor).await?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found(format!("no row with {pk} = {id}")))?;
    let row = row_to_json(&row);
    emit(
        catalog,
        table,
        WriteOp::Update,
        &row,
        old_row,
        context,
        executor,
    )
    .await;
    Ok(row)
}

/// Delete the row of `table` whose primary key is `id`, returning **the row as
/// it was**. Deleting a row that is not there is a
/// [`NotFound`](Error::NotFound), not a silent success.
///
/// The statement has to read the row back anyway — a delete event carries the
/// only copy of it anyone will ever get — so returning it costs nothing and is
/// the one moment it can be had. What a *caller* does with it is theirs to
/// decide: the REST projection answers `{"deleted": true}`, because that is its
/// wire contract, and the GraphQL one answers with the row, because that is
/// what `delete_X_by_pk: X` promised. Neither shape belongs here.
pub async fn delete_row(catalog: &Catalog, table: &Table, id: &str) -> Result<Json> {
    delete_row_guarded(catalog, table, id, None, None).await
}

/// [`delete_row`] with an extra `guard` predicate and optional RLS `context`;
/// same probe-free rule as [`update_row_guarded`].
pub(crate) async fn delete_row_guarded(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    guard: Option<Expr>,
    context: Option<&CallerContext>,
) -> Result<Json> {
    delete_row_guarded_in(catalog, table, id, guard, context, &Executor::Pooled).await
}

/// [`delete_row_ctx`] on a caller's [`Executor`] — see [`create_row_in`].
pub async fn delete_row_in(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Json> {
    delete_row_guarded_in(catalog, table, id, None, context, executor).await
}

pub(crate) async fn delete_row_guarded_in(
    catalog: &Catalog,
    table: &Table,
    id: &str,
    guard: Option<Expr>,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Json> {
    let guard = user_rows::and_guard(guard, user_rows::delete_guard(table, context)?);
    let pk = single_pk(table)?;
    let delete = Delete {
        table: table.name.clone(),
        filter: Some(guarded_filter(table, &pk, id, guard)?),
        // The whole row, not just the key: a delete event carries the row **as it
        // was**, which is the only copy of it anyone will ever get. No calc
        // projections — those are correlated subqueries, and correlating them
        // against a row being deleted in the same statement is a question with no
        // good answer.
        returning: user_rows::projection(table),
    };
    let rows = run_write_in(catalog, table, Statement::from(delete), context, executor).await?;
    let Some(row) = rows.first() else {
        return Err(Error::not_found(format!("no row with {pk} = {id}")));
    };
    let row = row_to_json(row);
    emit(
        catalog,
        table,
        WriteOp::Delete,
        &row,
        None,
        context,
        executor,
    )
    .await;
    Ok(row)
}

/// Delete every row of `table` the caller may delete, returning how many went.
///
/// One statement, not a loop over [`delete_row`]: it has no key to address rows
/// by, so a table with no primary key can be emptied too. The same guards
/// apply, and a delete event is still raised for each row (§10.2), because a
/// trigger watching deletes is watching these as well. The whole rows are read
/// back only when something is listening: otherwise a constant is all the
/// count needs, and emptying a large table does not ship it to the server.
pub async fn delete_all_rows_ctx(
    catalog: &Catalog,
    table: &Table,
    context: Option<&CallerContext>,
) -> Result<u64> {
    let executor = Executor::Pooled;
    let observed = catalog.observes_writes(&table.name, WriteOp::Delete);
    let delete = Delete {
        table: table.name.clone(),
        filter: user_rows::delete_guard(table, context)?,
        returning: if observed {
            user_rows::projection(table)
        } else {
            // Text, because a literal goes out as a bind parameter and a
            // parameter in a `RETURNING` list has no column to take a type from:
            // Postgres calls it `text` and would refuse an integer.
            vec![Projection::expr_as(Expr::lit(""), "deleted")]
        },
    };
    let rows = run_write_in(catalog, table, Statement::from(delete), context, &executor).await?;
    if observed {
        for row in &rows {
            let row = row_to_json(row);
            emit(
                catalog,
                table,
                WriteOp::Delete,
                &row,
                None,
                context,
                &executor,
            )
            .await;
        }
    }
    Ok(rows.len() as u64)
}

/// Raise the event one committed write is (§10.2), if anything is listening.
///
/// Two properties this function exists to hold, both of them in the TODO's words:
///
/// - **A write nobody observes pays nothing.** The `observes_writes` lookup comes
///   first, so the row is not even cloned for a table with no trigger on it.
/// - **A failing trigger does not fail the request or lose the write.** The write
///   has already committed by the time this runs, so an error here is *reported*
///   and the row still goes back to the caller. The dispatcher reports each
///   trigger's own failure; what reaches here is dispatch itself failing.
async fn emit(
    catalog: &Catalog,
    table: &Table,
    op: WriteOp,
    row: &Json,
    old_row: Option<Json>,
    caller: Option<&CallerContext>,
    executor: &Executor,
) {
    if !catalog.observes_writes(&table.name, op) {
        return;
    }
    let write = TableWrite {
        table,
        op,
        row: row.clone(),
        old_row,
        caller,
        // The transaction the write was made in travels with the event, so what
        // a listening trigger writes about it lands in the same transaction and
        // shares its fate (§10.3, decision 6). `None` on the pooled path, which
        // is every write outside a step or an import.
        tx: executor.serving(table).cloned(),
    };
    if let Err(e) = catalog.emit_write(write).await {
        eprintln!(
            "feldspar: dispatching the {op} event for `{}`: {}",
            table.name,
            sc_error::format_chain(&e)
        );
    }
}

/// One row by primary key, as the JSON an event carries — the pre-image an
/// update's `old_row` needs. `None` when no such row (or none the caller may
/// see, which for an event is the same thing: the update will not match either).
///
/// Read on the caller's [`Executor`], so a pre-image taken inside a transaction
/// sees what that transaction has written.
async fn read_row_in(
    catalog: &Catalog,
    table: &Table,
    pk: &str,
    id: &str,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Option<Json>> {
    let filter = pk_filter(table, pk, id)?;
    let select = read_select(catalog, table, &RowQuery::new().where_(Some(filter)))?;
    let rows = run_read_in(catalog, table, &select, context, false, executor).await?;
    Ok(rows.first().map(row_to_json))
}

/// `pk = id`, ANDed with the ownership guard when one applies.
fn guarded_filter(table: &Table, pk: &str, id: &str, guard: Option<Expr>) -> Result<Expr> {
    let base = pk_filter(table, pk, id)?;
    Ok(match guard {
        Some(guard) => base.and(guard),
        None => base,
    })
}

/// Coerce a whole JSON row body through [`column_value`], keyed by column — the
/// values an ownership formula is checked against before an insert or update
/// (§7.3) sees the database. Unknown columns are refused by name, exactly as
/// the write itself would.
pub(crate) fn coerce_row_values(
    table: &Table,
    body: &Json,
) -> Result<std::collections::BTreeMap<String, Value>> {
    let obj = require_object(body)?;
    let mut values = std::collections::BTreeMap::new();
    for (key, json) in obj {
        values.insert(key.clone(), column_value(table, key, json)?);
    }
    Ok(values)
}

/// The stored value of one column of the row addressed by `id` — a single cell,
/// for a caller that needs it for something other than serving the row (an
/// application's file endpoints resolve a `File` field's stored path this way,
/// §4). A missing row is a [`NotFound`](Error::not_found), exactly as
/// [`update_row`] reports one; an unknown column is refused by name.
pub async fn read_field(catalog: &Catalog, table: &Table, column: &str, id: &str) -> Result<Value> {
    read_field_ctx(catalog, table, column, id, None).await
}

/// [`read_field`] routed through an RLS caller context (§7.3) when one is
/// given, so a cell a policy withholds is a not-found rather than a leak.
pub(crate) async fn read_field_ctx(
    catalog: &Catalog,
    table: &Table,
    column: &str,
    id: &str,
    context: Option<&CallerContext>,
) -> Result<Value> {
    if table.field(column).is_none() {
        return Err(Error::invalid(format!(
            "`{}` has no field `{column}`",
            table.name
        )));
    }
    let pk = single_pk(table)?;
    let select = Select::from(Source::table(table.name.clone()))
        .columns(vec![Projection::expr(Expr::col(column))])
        .filter(pk_filter(table, &pk, id)?);
    let rows = run_read(catalog, table, &select, context, false).await?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found(format!("no row with {pk} = {id}")))?;
    row.values()
        .first()
        .cloned()
        .ok_or_else(|| Error::msg("single-column select returned no column"))
}

/// A row as a JSON object keyed by column name, values in natural JSON.
pub fn row_to_json(row: &Row) -> Json {
    let mut map = Map::with_capacity(row.len());
    for (name, value) in row.columns().iter().zip(row.values()) {
        map.insert(name.clone(), value_to_json(value));
    }
    Json::Object(map)
}

/// A written row's calculated fields computed after the read
/// ([`CalcPlan::complete`]), once the write has committed.
///
/// Two differences from a read, both because the row has already been
/// written. A failure says so — "the row was saved, but …" — rather than
/// reading as a write that did not happen. And inside a caller's
/// **transaction** the fields are left out: the row is not committed, and a
/// prediction reads it through the model's dataset on another connection,
/// where it is not there yet (or is still its old self).
async fn complete_written(
    catalog: &Catalog,
    table: &Table,
    plan: &CalcPlan,
    rows: Vec<Row>,
    executor: &Executor,
) -> Result<Vec<Row>> {
    if plan.is_complete() || executor.serving(table).is_some() {
        return Ok(rows);
    }
    plan.complete(catalog, table, rows).await.map_err(|e| {
        Error::invalid(format!(
            "the row was saved, but a calculated field of `{}` could not be computed for it: \
             {e}",
            table.name
        ))
    })
}

/// Refuse a write that names a non-stored calculated field — it has no column
/// (Phase 8). Called by insert and update before building the statement.
fn reject_calc_writes(table: &Table, columns: &Map<String, Json>) -> Result<()> {
    for key in columns.keys() {
        if table.field(key).is_some_and(|f| f.is_calc()) {
            return Err(Error::invalid(format!(
                "`{key}` is a calculated field and cannot be written"
            )));
        }
    }
    Ok(())
}

/// Coerce a JSON value for a named column of `table`, validating it against the
/// field's type and attributes, and rejecting unknown columns.
///
/// Two steps (§2.3). First the JSON is coerced to a [`Value`] of the column's
/// **storage** type — the SQL type the column actually has, which a rich type
/// sits on. Then the value is checked against the field's full
/// [`TypeRef`](sc_types::TypeRef) *and* its configured attributes via
/// [`validate_with`](sc_types::TypeRef::validate_with): a basic type checks the
/// value family; a rich type also enforces its attributes (a `String`'s
/// `max_length`/`options`/`regex`, an `Integer`'s `min`/`max`).
///
/// Both failures are reported as an [`Error::invalid`] (an HTTP 400) **naming the
/// field**, because this message is shown to a user of an application, not only
/// to the admin — "`age`: must be at most 120, got 999" lands them on the input
/// to fix.
pub fn column_value(table: &Table, column: &str, json: &Json) -> Result<Value> {
    let field = table
        .field(column)
        .ok_or_else(|| Error::invalid(format!("`{}` has no field `{column}`", table.name)))?;
    let type_ = &field.base.type_;

    let value = json_to_value(&storage_type(type_), json).map_err(|e| field_error(column, e))?;
    type_
        .validate_with(&value, &field.base.attributes)
        .map_err(|e| field_error(column, e))?;
    Ok(value)
}

/// Validate a value written to a `File` field against its store, folder and MIME
/// rules (§3.5). A no-op for any field that is not a `File` kind, or a null/empty
/// value (nullability is a separate check).
///
/// The path must resolve to a **connected** store — an unresolvable store is an
/// error naming the store and the field, because a reference into a store that is
/// not there points at nothing — and its shape must satisfy the field's folder
/// and MIME constraints ([`sc_files::validate_file_path`]). The error names the
/// field, as it is shown to a user of an application, not only the admin.
fn validate_file_write(
    catalog: &Catalog,
    table: &Table,
    column: &str,
    value: &Value,
) -> Result<()> {
    let Some(field) = table.field(column) else {
        return Ok(());
    };
    let DataFieldKind::File {
        store,
        folder,
        mime_allow,
    } = &field.kind
    else {
        return Ok(());
    };
    // A null or empty path is absence, governed by the column's nullability, not
    // by the file rules.
    let path = match value {
        Value::Text(path) if !path.is_empty() => path.as_str(),
        _ => return Ok(()),
    };

    if catalog.file_store(&store.0)?.is_none() {
        return Err(Error::invalid(format!(
            "`{column}`: file store `{}` is not resolvable",
            store.0
        )));
    }
    sc_files::validate_file_path(path, folder.as_deref(), mime_allow)
        .map_err(|e| field_error(column, e))
}

/// A row the database **returned** — an insert's or a delete's `RETURNING` —
/// back as the typed values a reader works in.
///
/// The inverse of [`row_to_json`], and deliberately not [`column_value`]: these
/// values came out of the column, so there is nothing to validate them against.
/// Holding a returned row to the field's attribute rules would refuse a row the
/// database already holds — a `max_length` tightened after the row was written
/// is the admin's problem to fix, not a reason a delete cannot say what it
/// removed.
///
/// A column the table does not declare, and one whose text will not parse as
/// its declared type, is carried as the JSON it arrived as rather than dropped:
/// a value nobody asked about must not silently disappear on the way back.
pub(crate) fn json_row_values(
    table: &Table,
    row: &Json,
) -> std::collections::BTreeMap<String, Value> {
    let mut values = std::collections::BTreeMap::new();
    let Json::Object(obj) = row else {
        return values;
    };
    for (name, json) in obj {
        let typed = table
            .field(name)
            .map(|field| storage_type(&field.base.type_))
            .and_then(|basic| sc_types::json_to_value(&basic, json).ok())
            .unwrap_or_else(|| match json {
                Json::Null => Value::Null,
                other => Value::Json(other.clone()),
            });
        values.insert(name.clone(), typed);
    }
    values
}

/// The basic (storage) type a JSON value is coerced through: the type itself for
/// a basic field, or the SQL type a rich field sits on (a `String` stores as
/// `text`, an `Integer` as `int8`).
fn storage_type(type_: &TypeRef) -> BasicType {
    match type_.as_basic() {
        Some(basic) => basic.clone(),
        None => BasicType::from_sql_type(type_.sql_type()),
    }
}

/// Re-raise a value error against a named field, preserving the `Invalid` kind
/// (so it stays a 400) and prefixing the field name (§2.3). The specific
/// violation — from either coercion or validation, both `Invalid` — is kept; any
/// other kind falls back to its full display.
pub(crate) fn field_error(column: &str, e: Error) -> Error {
    let detail = match e.repr() {
        Repr::Invalid(message) => message.clone(),
        _ => e.to_string(),
    };
    Error::invalid(format!("`{column}`: {detail}"))
}

/// The single primary-key column of `table`, or an error when the table has a
/// composite or absent key (row addressing needs exactly one — composite keys
/// are post-MVP).
pub fn single_pk(table: &Table) -> Result<String> {
    match table.primary_key.as_slice() {
        [pk] => Ok(pk.clone()),
        [] => Err(Error::invalid(format!(
            "table `{}` has no primary key to address rows by",
            table.name
        ))),
        _ => Err(Error::invalid(format!(
            "table `{}` has a composite primary key (unsupported for row addressing)",
            table.name
        ))),
    }
}

/// The primary key of a **read** row, in the two forms a caller writing that row
/// back needs: the string [`update_row_ctx`] and [`delete_row_ctx`] address it by,
/// and the JSON an answer reports it as.
///
/// One function because there is one rule and it is easy to get subtly wrong: the
/// value goes out through the same rendering an API response uses, and the
/// *string* form is that rendering unquoted — a uuid key is its own text, an
/// integer key is its digits — which is exactly what [`pk_filter`] coerces back.
/// Both callers that resolve rows by predicate and then write them one at a time
/// (a row action's `update_rows`, a code body's `db.…update()`) share it.
pub fn row_key(
    table: &Table,
    pk: &str,
    values: &std::collections::BTreeMap<String, Value>,
) -> Result<(String, Json)> {
    let value = values.get(pk).filter(|v| !v.is_null()).ok_or_else(|| {
        Error::msg(format!(
            "a row selected from `{}` carries no `{pk}` to address it by",
            table.name
        ))
    })?;
    let json = value_to_json(value);
    let id = match &json {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    Ok((id, json))
}

/// `pk = <id>`, coercing the path-parameter string to the key column's type.
pub(crate) fn pk_filter(table: &Table, pk: &str, id: &str) -> Result<Expr> {
    let value = column_value(table, pk, &Json::String(id.to_owned()))?;
    Ok(Expr::col(pk).eq(Expr::lit(value)))
}

/// Whether this statement runs inside a caller-context transaction: **the table
/// decides**, not the caller — unless the statement itself reaches a table that
/// does.
///
/// It used to be "whenever a context was given", which worked only while the
/// context existed for RLS alone. Now the caller travels with every write (an
/// event has to say who caused it), so a `Some` on an ordinary table must not
/// silently wrap it in a transaction and a `SET LOCAL` no policy will ever read.
/// A context is still *required* to reach the policies: without one they see
/// `NULL` and deny, which is the fail-closed shape §7.3 depends on.
///
/// `reaches_rls` is the second half of that rule, and it is a property of the
/// *statement*: a read of an ordinary table that projects a correlated subquery
/// over an RLS-protected one runs that table's policies, so it needs the GUCs
/// too (see [`RowQuery::in_caller_context`]).
fn in_context<'a>(
    table: &Table,
    context: Option<&'a CallerContext>,
    reaches_rls: bool,
) -> Option<&'a CallerContext> {
    context.filter(|_| table.rls_enabled || reaches_rls)
}

/// Run a `SELECT`, collecting its rows — through an RLS caller-context
/// transaction on an RLS table (§7.3) or one whose subqueries reach one, else on
/// a pooled connection via the table's provider.
pub(crate) async fn run_read(
    catalog: &Catalog,
    table: &Table,
    select: &Select,
    context: Option<&CallerContext>,
    reaches_rls: bool,
) -> Result<Vec<Row>> {
    run_read_in(
        catalog,
        table,
        select,
        context,
        reaches_rls,
        &Executor::Pooled,
    )
    .await
}

/// [`run_read`] on a caller's executor: unchanged when it is
/// [`Pooled`](Executor::Pooled), and otherwise the caller's transaction, whose
/// GUCs (and whose deferred constraints) are already set.
pub(crate) async fn run_read_in(
    catalog: &Catalog,
    table: &Table,
    select: &Select,
    context: Option<&CallerContext>,
    reaches_rls: bool,
    executor: &Executor,
) -> Result<Vec<Row>> {
    if let Some(tx) = executor.serving(table) {
        // The caller travels with the statement here, not with the transaction:
        // a step transaction has several writers, and each one's statements are
        // decided by the policies as *itself* (`SharedTx::run`).
        return tx
            .run(context, &Statement::Select(Box::new(select.clone())))
            .await;
    }
    match in_context(table, context, reaches_rls) {
        Some(context) => {
            sc_catalog::run_in_context(
                catalog,
                context,
                &Statement::Select(Box::new(select.clone())),
            )
            .await
        }
        None => {
            catalog
                .provider(table)?
                .query(select)
                .await?
                .try_collect()
                .await
        }
    }
}

/// Run a write statement, collecting `RETURNING` rows — on the caller's
/// executor: their transaction when it serves this table, else an RLS
/// caller-context transaction on an RLS table, else the provider (see
/// [`run_read_in`]).
async fn run_write_in(
    catalog: &Catalog,
    table: &Table,
    statement: Statement,
    context: Option<&CallerContext>,
    executor: &Executor,
) -> Result<Vec<Row>> {
    let outcome = match executor.serving(table) {
        Some(tx) => tx.run(context, &statement).await,
        None => match in_context(table, context, false) {
            Some(context) => sc_catalog::run_in_context(catalog, context, &statement).await,
            None => match catalog.provider(table)?.write(&statement).await {
                Ok(stream) => stream.try_collect().await,
                Err(e) => Err(e),
            },
        },
    };
    outcome.map_err(|e| constraint_message(table, e))
}

/// A constraint violation, said in the admin's own words.
///
/// A jointly-unique constraint is enforced by the database, so what a caller
/// sees when they break it is Postgres's `duplicate key value violates unique
/// constraint "sc_uq_member_org_email"` — which names an object they have never
/// heard of and does not say what to do. The admin wrote a sentence for exactly
/// this moment (it is why the constraint form asks for one); this is where it is
/// substituted, on **one** funnel, so every write path gets it: the admin UI,
/// the REST API, an action, a CSV import.
///
/// A row constraint arrives the same way — its trigger names itself in the
/// error's `CONSTRAINT` field, exactly as Postgres does for a unique violation,
/// so one lookup serves both. What the constraint changes is not only the
/// wording: a rule the *caller* broke is a 400 naming it, not the 500 an
/// unexplained database error deserves.
///
/// An error naming no constraint of this table passes through untouched, because
/// inventing a friendly message for a fault nobody anticipated is how a real one
/// gets hidden.
fn constraint_message(table: &Table, error: Error) -> Error {
    let chain = sc_error::format_chain(&error);
    match sc_catalog::violated_constraint(&table.constraints, &chain) {
        Some(constraint) => Error::invalid(
            constraint
                .error_message
                .clone()
                // No message of the admin's: the database's own sentence, which
                // for a row constraint is the generated one naming the rule and
                // for a unique violation is Postgres's. Better than the whole
                // chain, which carries the statement and its bind count.
                .unwrap_or_else(|| database_message(&chain)),
        ),
        None => error,
    }
}

/// The server's own sentence out of a driver error chain.
///
/// `sc-db-postgres` formats a database failure as `<message> [<sqlstate>]`
/// (optionally ` (constraint "…")`) followed by the statement, so the message is
/// everything before the SQLSTATE. A chain in any other shape is returned whole:
/// a half-parsed error is worse than a verbose one.
fn database_message(chain: &str) -> String {
    let start = chain.find(": ").map_or(0, |i| i + 2);
    match chain[start..].find(" [") {
        Some(end) => chain[start..start + end].trim().to_owned(),
        None => chain.to_owned(),
    }
}

/// A JSON body that must be an object, e.g. a row.
pub fn require_object(body: &Json) -> Result<&Map<String, Json>> {
    body.as_object()
        .ok_or_else(|| Error::invalid("expected a JSON object body"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_catalog::DbId;
    use sc_db::PhysicalTable;

    fn table(rls_enabled: bool) -> Table {
        let mut table = Table::from_physical(
            DbId::primary(),
            &PhysicalTable {
                name: "books".into(),
                schema: None,
                columns: Vec::new(),
                primary_key: vec!["id".into()],
                foreign_keys: Vec::new(),
                constraints: Vec::new(),
            },
        );
        table.rls_enabled = rls_enabled;
        table
    }

    /// The rule the caller context changed meaning under: it is *who is writing*
    /// (every write has one, so an event can say who caused it), and whether the
    /// statement runs in a GUC transaction is the **table's** business.
    ///
    /// Worth its own test because both halves are silent failures. Routing an
    /// ordinary write through a policy transaction costs a transaction and a
    /// `SET LOCAL` no policy will ever read; *not* routing an RLS one leaves the
    /// GUCs unset, which every generated policy reads as no access — the
    /// fail-closed shape §7.3 depends on.
    #[test]
    fn the_table_decides_the_caller_context_transaction_not_the_caller() {
        let caller = CallerContext::anonymous(1);
        assert!(in_context(&table(true), Some(&caller), false).is_some());
        assert!(in_context(&table(false), Some(&caller), false).is_none());
        assert!(in_context(&table(true), None, false).is_none());
    }

    /// …and the one thing that is *not* the table's business: a statement whose
    /// own subqueries reach a protected table.
    ///
    /// A GraphQL read of an ordinary `departments` that projects
    /// `employees_aggregate { count }` over an RLS-protected `employees` runs
    /// the employees' policies inside the departments' statement. Without the
    /// GUCs those policies see no caller and grant nothing, and the aggregate
    /// comes back `0` — a number that looks like an answer. So the *statement*
    /// gets to ask for the transaction the table did not need.
    #[test]
    fn a_statement_reaching_a_protected_table_asks_for_the_transaction_itself() {
        let caller = CallerContext::anonymous(1);
        assert!(in_context(&table(false), Some(&caller), true).is_some());
        // Still fail-closed on the other half: no context is no transaction,
        // whatever the statement reaches.
        assert!(in_context(&table(false), None, true).is_none());
    }
}
