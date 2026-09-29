//! The **plan**: what one terminal of a code body's `db` chain sends, and how it
//! becomes a statement.
//!
//! A plan is a plain JSON object (§7 of the milestone) — `op`, `table`,
//! `authority`, `where`, `select`, `order`, `limit`, `offset` — and that is the
//! whole seam. The fluent surface that produces it is JavaScript living in
//! `sc-expr`'s prelude; a Python adapter (§15) will produce the same objects, and
//! neither of them decides anything: **every** name in a plan is resolved here,
//! against the catalog, and anything else is refused naming it.
//!
//! Two spellings of a filter (§3), one predicate: the object DSL every other
//! surface already speaks — lowered by [`crate::filter`], the same walk an agent's
//! `where` argument goes through — or a formula string, parsed by
//! [`Formula::parse`] and translated by the one symbolic translator. A projection
//! is the same choice: a field name, a Ⱶ-path, or an `{ alias, formula }` object.
//! There is one expression language and this is it, which is why a formula the
//! translator refuses is an error naming it and saying to compute it in the code
//! body — where the author already has JavaScript.
//!
//! Nothing here reaches SQL as text. A column name comes from the catalog, a
//! join path from [`join_path_expr`], an operator from the shared vocabulary, and
//! every literal a body wrote becomes an `Expr::Lit` the query layer
//! parameterises on render.

use std::cell::Cell;
use std::collections::BTreeMap;

use sc_catalog::{Catalog, DataFieldKind, Table};
use sc_error::{Error, Result};
use sc_expr::{
    AggFunc, CalcFields, Env, Formula, JOIN, Operation, TranslateError, UserEnv, aggregate_expr,
    join_path_expr, translate, translate_value, value_from_json,
};
use sc_query::{
    BinOp, Expr, Nulls, OrderBy, OrderDir, Projection, Statement, UnOp, Value, rewrite_named_params,
};
use serde::Deserialize;
use serde_json::Value as Json;

use crate::convert::value_to_json;
use crate::filter::{self, FilterKey, JoinedColumn};
use crate::ownership::{self, AggregateGuard, JoinAccess};
use crate::rows::{self, RowQuery};

use super::HostLimits;

/// The key that spells the **formula** form of a `where` (§3).
///
/// A **column of this name wins**, as it does against the `and`/`or`/`not`
/// combinators and for the same reason: an application really may have a column
/// called `formula`, and a filter on it must keep meaning what it says. A table
/// that has one simply cannot use the string spelling — it still has the object
/// DSL, which can say everything the plan needs.
const FORMULA: &str = "formula";

// ---------------------------------------------------------------------------
// The wire shape
// ---------------------------------------------------------------------------

/// One terminal of a `db` chain, as it crosses from the guest.
///
/// `deny_unknown_fields` on purpose: a plan carrying something this server does
/// not understand is a guest and a host that disagree about the seam, and
/// answering it anyway would answer a question nobody asked.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Which operation. `select` and `aggregate` read; the rest write.
    pub op: Op,
    /// The table, resolved against the catalog and nowhere else.
    pub table: String,
    /// Whose authority this runs under (§5). Admin unless the body delegated.
    #[serde(default)]
    pub authority: Authority,
    /// The filter: the object DSL, or `{ "formula": "…" }`.
    #[serde(default, rename = "where")]
    pub filter: Option<Json>,
    /// The projections. Empty is the whole row.
    #[serde(default)]
    pub select: Vec<Selection>,
    /// `ORDER BY` keys, in precedence order.
    #[serde(default)]
    pub order: Vec<OrderKey>,
    /// Grouping keys: a field, a Ⱶ-path or a formula, one answer row per
    /// distinct combination of them.
    #[serde(default)]
    pub group: Vec<String>,
    /// The bound on the **groups** — the object DSL again, but keyed by this
    /// aggregate's own aliases rather than by columns.
    #[serde(default)]
    pub having: Option<Json>,
    /// The bound the body asked for.
    #[serde(default)]
    pub limit: Option<u64>,
    /// How many rows to skip first.
    #[serde(default)]
    pub offset: Option<u64>,
    /// The primary key `.get(pk)` named.
    #[serde(default)]
    pub pk: Option<Json>,
    /// Whether this read is **one batch of an `.iter()`**.
    ///
    /// It changes three things about the read and nothing else: the ordering is
    /// made **total** (the primary key is appended to it, so no two rows tie),
    /// null placement is stated rather than left to the dialect, and the answer
    /// carries a [`Cursor`] beside the rows. `limit` stops being an answer the
    /// body computes on and becomes the size of one batch — see
    /// [`batch_limit`].
    #[serde(default)]
    pub cursor: bool,
    /// Where the previous batch stopped: one value per `ORDER BY` key of the
    /// cursor read, in the order the read sorts by, ending with the primary
    /// key's.
    ///
    /// It is the value the host itself answered with, handed back verbatim —
    /// and **nothing in it is trusted** for all that: each value is coerced
    /// against the column its key sorts on, exactly as a `.where()` literal is,
    /// and a cursor of the wrong length is refused rather than padded.
    #[serde(default)]
    pub after: Option<Vec<Json>>,
    /// What an `aggregate` asks for, each aliased by the key it rides back under.
    #[serde(default)]
    pub aggregate: Vec<AggSpec>,
    /// Whether this read wants the **statement** rather than the rows — v1's
    /// `getJoinedQuery`, which answers `{ sql, values }` (TODO "the v1 `Table`
    /// API" §3.5).
    ///
    /// A field of the read rather than a sixth [`Op`] because it *is* the read:
    /// the same plan, the same lowering, the same ownership rule — only the last
    /// step differs, and rendering a statement this server then declines to run
    /// is not an operation of its own. Nothing runs, so the row cap has nothing
    /// to count and the call budget is what bounds it.
    #[serde(default)]
    pub render: bool,
    /// An insert's row(s), or an update's assignments (phase 3).
    #[serde(default)]
    pub values: Option<Json>,
}

/// The **other** thing a terminal may send: SQL the code body wrote itself
/// (`db.sql("select …", [args])`).
///
/// A separate shape rather than a sixth [`Op`], because it shares nothing with a
/// plan but the authority: there is no table to resolve, no column to check and
/// no filter to lower — the whole statement is the author's text, and the only
/// thing this server puts into it is the bound values. [`super::TableHost`]
/// tells the two apart by the `op` field before it deserialises either, so a malformed
/// request is refused in the words of the shape it was trying to be.
///
/// The rule written on [`Statement::Raw`](sc_query::Statement::Raw) — raw SQL is
/// **admin-authored, never assembled from what a caller sent** — is what makes
/// this admissible at all: a `run_js_code` body is server-side configuration,
/// written by the same administrator who writes §13.4's custom SQL queries. The
/// values a run has in hand (a row, a payload, a user) reach the statement as
/// **binds** and nowhere else, which is why the parameters are an array rather
/// than something the body interpolates.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlPlan {
    /// Always `sql` — the field the two shapes are told apart by.
    pub op: SqlOp,
    /// Whose authority it runs under (§5). Admin unless the body delegated.
    #[serde(default)]
    pub authority: Authority,
    /// The statement, with the database's own placeholders in it.
    pub sql: String,
    /// The values those placeholders stand for, in placeholder order.
    #[serde(default)]
    pub params: Vec<Json>,
}

/// The one operation a [`SqlPlan`] may be — a unit enum rather than an ignored
/// field, so a request that says `sql` is the only thing that deserialises as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlOp {
    /// `db.sql(…)`.
    Sql,
}

/// The five operations a plan may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Rows.
    Select,
    /// One row of aggregate values.
    Aggregate,
    /// Write new rows.
    Insert,
    /// Change matched rows.
    Update,
    /// Remove matched rows.
    Delete,
}

/// Whose authority a plan runs under (§5): the trigger's own by default,
/// the event's caller when the body said `asUser()`, and — since v1's `Table`
/// says whose view of the data it wants with an *argument* — a **named user**.
///
/// The three that are not `Admin` are one thing in the end: [`Actor::Caller`],
/// a role and a user, checked through `sc_api::ownership`'s `*_as` functions.
/// There is no second implementation of "meets the floor OR the formula grants
/// it" here and there must never be one; what these forms differ in is only
/// *which* role and user those functions are handed.
///
/// None of them can be an escalation. A body already runs as admin and could
/// read everything by saying nothing at all, so naming somebody smaller is a
/// body volunteering to be treated as them (TODO "the v1 `Table` API" §4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Authority {
    /// The trigger's: `ROLE_ADMIN`, carrying the event's user. The default,
    /// because a trigger is server-side configuration and the audit row a caller
    /// may not insert is the archetype of what a trigger exists to write.
    #[default]
    Admin,
    /// The event's caller, through `sc_api::ownership`'s `*_as` functions.
    User,
    /// The **public** role and nobody — v1's `forPublic: true`, and the honest
    /// reading of "what would an anonymous visitor see".
    Public,
    /// One **named** user, loaded from the users table and then treated exactly
    /// as [`Authority::User`]'s caller is: v1's `forUser` and its `user`
    /// argument. The id is carried as it arrived and resolved where users live,
    /// so a value that is no user id is refused naming it rather than
    /// deserialised into something plausible.
    Named(Json),
}

impl<'de> Deserialize<'de> for Authority {
    /// `"admin"`, `"user"`, `"public"` — or `{ "user": <id> }` for a named one.
    ///
    /// Hand-written rather than untagged, for the reason every other shape on
    /// this seam is `deny_unknown_fields`: an authority this server does not
    /// understand must be a sentence naming the four spellings, not a silent
    /// fall back to the default — which is admin, and therefore the one wrong
    /// answer that would matter.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Authority, D::Error> {
        use serde::de::Error as _;
        let json = Json::deserialize(d)?;
        match &json {
            Json::String(s) if s == "admin" => Ok(Authority::Admin),
            Json::String(s) if s == "user" => Ok(Authority::User),
            Json::String(s) if s == "public" => Ok(Authority::Public),
            Json::Object(map) if map.len() == 1 && map.contains_key("user") => {
                Ok(Authority::Named(map["user"].clone()))
            }
            _ => Err(D::Error::custom(format!(
                "`{json}` is not an authority: it is \"admin\", \"user\", \"public\", \
                 or {{ \"user\": id }} for one particular user"
            ))),
        }
    }
}

impl Authority {
    /// Whether this authority is the admin's — the one form that clears every
    /// rule that can be written, and therefore the one worth asking about by
    /// name rather than by matching three others.
    pub fn is_admin(&self) -> bool {
        matches!(self, Authority::Admin)
    }

    /// The name a refusal calls this authority, for a surface that carries only
    /// two of the four.
    pub fn spelling(&self) -> &'static str {
        match self {
            Authority::Admin => "admin",
            Authority::User => "user",
            Authority::Public => "public",
            Authority::Named(_) => "a named user",
        }
    }
}

/// One projection: a name (a column, a calculated field or a Ⱶ-path), or a
/// formula under an alias.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Selection {
    /// `"title"`, `"customerⱵemail"`.
    Name(String),
    /// `{ alias: "spend", formula: "ordersↃcustomer.sum(o => o.total)" }`.
    Formula {
        /// The key the value rides back under.
        alias: String,
        /// The expression, in the one formula language.
        formula: String,
    },
}

/// One `ORDER BY` key.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderKey {
    /// A column, a calculated field, a Ⱶ-path or a formula.
    pub field: String,
    /// Which way. Ascending unless the body said otherwise.
    #[serde(default)]
    pub dir: Dir,
}

/// A sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Ascending — what `.orderBy(field)` with no direction means.
    #[default]
    Asc,
    /// Descending.
    Desc,
}

/// One aggregate value a plan asks for.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggSpec {
    /// The key it rides back under (`"value"` for the scalar terminals).
    pub alias: String,
    /// `count`, `sum`, `avg`, `min` or `max`.
    #[serde(rename = "fn")]
    pub func: String,
    /// The field or formula aggregated; absent for `count`.
    #[serde(default)]
    pub arg: Option<String>,
}

// ---------------------------------------------------------------------------
// A validated read
// ---------------------------------------------------------------------------

/// A `select` plan, resolved: the read the row layer runs, and the shape its
/// values are answered in.
pub(crate) struct Read {
    /// The table being read.
    pub(crate) table: Table,
    /// The read itself.
    pub(crate) query: RowQuery,
    /// The keys the answer carries, or `None` for the whole row.
    keys: Option<Vec<String>>,
    /// How the **next** batch resumes, for a read that is one batch of an
    /// `.iter()`; `None` for every ordinary read.
    pub(crate) cursor: Option<Cursor>,
}

/// How a streamed read resumes: the aliases its sort values ride back under, and
/// the batch it asked for.
///
/// It exists because the cursor cannot be read off the *answer*. A body that
/// `.select()`ed two columns still sorts by whatever it named, and a sort key may
/// be a Ⱶ-path that is no column of this table at all — so the sort values are
/// projected into the same `SELECT` under reserved aliases, read back here, and
/// dropped before the rows reach the guest ([`Read::row`] answers the keys the
/// body asked for, and these are not among them).
pub(crate) struct Cursor {
    /// The alias each sort value is projected under, in the order the read sorts
    /// by — the primary key's last.
    aliases: Vec<String>,
    /// The batch this read asked the database for. A short answer means the rows
    /// ran out, which is how the guest learns to stop.
    batch: u64,
}

impl Cursor {
    /// The cursor the next batch resumes from, or `Json::Null` when there is no
    /// next batch.
    ///
    /// **The last row read, not the last row returned** — they are the same
    /// thing here and it is worth saying why, because the difference is a class
    /// of silent bug: every rule that narrows a streamed read (an RLS policy, a
    /// translatable ownership formula) narrows it *inside* the statement, ahead
    /// of the `LIMIT`, so a row the caller may not see is never a row the batch
    /// counted. The one rule that cannot — an ownership formula only the
    /// evaluator can decide — is refused a cursor outright, in
    /// [`super::TableHost::delegable_stream`].
    pub(crate) fn next(&self, values: &[BTreeMap<String, Value>]) -> Json {
        // A batch the database could not fill is the last one. Asking again
        // would cost a round trip to be told nothing; a body that iterates a
        // table whose row count is an exact multiple of the batch pays that one
        // extra call, which is the cheaper half of the trade.
        if (values.len() as u64) < self.batch {
            return Json::Null;
        }
        let Some(last) = values.last() else {
            return Json::Null;
        };
        Json::Array(
            self.aliases
                .iter()
                .map(|alias| last.get(alias).map_or(Json::Null, value_to_json))
                .collect(),
        )
    }
}

impl Read {
    /// One read row as the **REST wire shape** — `value_to_json`, so a Decimal is
    /// exact and a Date is ISO, and a row means the same thing in
    /// `db.books.rows()` as it does over HTTP.
    pub(crate) fn row(&self, values: &BTreeMap<String, Value>) -> Json {
        let Some(keys) = &self.keys else {
            return ownership::table_row_json(&self.table, values);
        };
        let mut map = serde_json::Map::with_capacity(keys.len());
        for key in keys {
            map.insert(
                key.clone(),
                values.get(key).map_or(Json::Null, value_to_json),
            );
        }
        Json::Object(map)
    }
}

/// An `insert` plan, resolved: the table, and the row(s) the row layer will
/// coerce, validate and raise events for.
pub(crate) struct Insertion {
    /// The table being written.
    pub(crate) table: Table,
    /// The rows, each a JSON object, in the order the body gave them.
    pub(crate) rows: Vec<Json>,
    /// Whether the body passed an **array**. The answer's shape follows the
    /// call's — one row in, one row out — so `.insert(v)` never has to be unwrapped.
    pub(crate) many: bool,
}

/// An `update` or `delete` plan, resolved: the read that finds the rows it
/// touches, the key it addresses each one by, and what an update assigns.
pub(crate) struct Write {
    /// The read whose rows are then written **one at a time** — which is what
    /// makes each one its own event (§4).
    pub(crate) matched: Read,
    /// The primary key each matched row is addressed by.
    pub(crate) pk: String,
    /// An update's assignments; `None` for a delete.
    pub(crate) values: Option<Json>,
}

/// An `aggregate` plan, resolved — grouped or not, which is one field of it
/// rather than two structures (§the phase 6 rule: the scalar terminals are sugar
/// for the grouped path with nothing to group by).
pub(crate) struct Aggregate {
    /// The table being aggregated.
    pub(crate) table: Table,
    /// One projection per group key and one per requested value, each aliased by
    /// the key it rides back under.
    pub(crate) projections: Vec<Projection>,
    /// Which rows it ranges over, grouped how, ordered and bounded how.
    pub(crate) query: RowQuery,
    /// The answer's keys — the group keys first, then the values — in the order
    /// the plan named them.
    pub(crate) keys: Vec<String>,
    /// Whether the plan grouped. An ungrouped aggregate is **one object** (the
    /// shape `.count()` has always answered); a grouped one is a row per group.
    pub(crate) grouped: bool,
}

/// Resolve a `select` plan against the catalog.
pub(crate) fn read(cat: &Catalog, plan: &Plan, limits: &HostLimits, role: u8) -> Result<Read> {
    let table = cat.require(&plan.table)?;
    let low = Lowering::new(cat, &table, role)?;
    let mut filter = low.filter(plan.filter.as_ref())?;
    if let Some(pk) = &plan.pk {
        let expr = pk_expr(&table, pk)?;
        filter = Some(match filter {
            Some(existing) => existing.and(expr),
            None => expr,
        });
    }

    let mut keys = Vec::with_capacity(plan.select.len());
    let mut extra = Vec::new();
    for selection in &plan.select {
        match selection {
            Selection::Name(name) => {
                keys.push(name.clone());
                // A column of the table (or a calculated field) is already in the
                // `SELECT` the row layer builds; only a path or an expression is
                // a projection this read has to add.
                if let Some(expr) = low.projected(name)? {
                    extra.push(Projection::expr_as(expr, name.clone()));
                }
            }
            Selection::Formula { alias, formula } => {
                keys.push(alias.clone());
                extra.push(Projection::expr_as(
                    low.formula_value(formula)?,
                    alias.clone(),
                ));
            }
        }
    }
    refuse_grouping(plan, "a row read")?;

    // The ordering, and — for a batch of an `.iter()` — the two things that
    // follow from it: where the batch resumes, and where the next one will.
    let (order, cursor) = match plan.cursor {
        false => {
            let mut order = Vec::with_capacity(plan.order.len());
            for key in &plan.order {
                let expr = low.value_expr(&key.field)?;
                order.push(match key.dir {
                    Dir::Asc => OrderBy::asc(expr),
                    Dir::Desc => OrderBy::desc(expr),
                });
            }
            (order, None)
        }
        true => {
            let sort = low.sort_keys(plan)?;
            if let Some(after) = &plan.after {
                // An `.offset()` belongs to the *iteration*, so it is spent on
                // the batch that starts it. Applied again to a resumed batch it
                // would skip rows in the middle of the stream, which is a wrong
                // answer that looks like a right one.
                if plan.offset.is_some() {
                    return Err(Error::invalid(format!(
                        "this `.iter()` of `{}` resumed from a cursor *and* asked to skip \
                         rows; an `.offset()` is skipped once, at the start of the iteration",
                        plan.table
                    )));
                }
                let resumed = after_predicate(&sort, after, &plan.table)?;
                filter = Some(match filter {
                    Some(existing) => existing.and(resumed),
                    None => resumed,
                });
            }
            let mut aliases = Vec::with_capacity(sort.len());
            let mut order = Vec::with_capacity(sort.len());
            for (i, key) in sort.iter().enumerate() {
                // Projected even when the key is a column the read already
                // carries: the alias is what the cursor is read back by, and one
                // rule for every key is one thing to be right about rather than
                // two. A duplicated column costs the database nothing.
                let alias = format!("{CURSOR_ALIAS}{i}");
                extra.push(Projection::expr_as(key.expr.clone(), alias.clone()));
                aliases.push(alias);
                order.push(key.order_by());
            }
            let batch = batch_limit(plan, limits);
            (order, Some(Cursor { aliases, batch }))
        }
    };

    let mut query = RowQuery::new().where_(filter).order_by(order);
    if !extra.is_empty() {
        query = query.projecting(extra);
    }
    query = query.limit(match &cursor {
        Some(cursor) => cursor.batch,
        None => bounded_limit(plan, limits, &table)?,
    });
    if let Some(offset) = plan.offset {
        query = query.offset(offset);
    }
    let query = query.requiring_caller_context(low.in_caller_context.get());
    Ok(Read {
        table,
        query,
        keys: (!plan.select.is_empty()).then_some(keys),
        cursor,
    })
}

/// The prefix the sort values of a streamed read are projected under. Reserved:
/// a body cannot ask for a column by this name, because `.select()` resolves its
/// names against the catalog and no catalog field starts with it.
const CURSOR_ALIAS: &str = "__sc_cursor_";

/// One `ORDER BY` key of a streamed read: what it sorts by, which way, whether
/// it can be null, and where its cursor value is typed.
struct SortKey {
    /// The expression sorted by — the very same one an unstreamed `.orderBy()`
    /// produces, so a body that switches from `.rows()` to `.iter()` sorts by
    /// the same thing.
    expr: Expr,
    /// Which way it sorts.
    dir: Dir,
    /// Whether a row's value for it can be null. A `NOT NULL` column cannot, and
    /// saying so keeps the resume predicate free of a disjunct that could only
    /// ever be false.
    nullable: bool,
    /// The table and column one cursor value is coerced against. A key that is a
    /// Ⱶ-path types against the column at the far end, which is exactly what a
    /// filter on that path already does.
    typed: (Table, String),
}

impl SortKey {
    /// The `ORDER BY` this key contributes, with null placement **stated**.
    ///
    /// Left to the dialect it would be a default that the resume predicate has
    /// to agree with in order to be correct — and a default that differs between
    /// two backends is a silently skipped row on one of them. Postgres' own
    /// defaults are what these are; the point is that they are now written down
    /// in the one place the predicate is built from.
    fn order_by(&self) -> OrderBy {
        match self.dir {
            Dir::Asc => OrderBy {
                expr: self.expr.clone(),
                dir: OrderDir::Asc,
                nulls: Some(Nulls::Last),
            },
            Dir::Desc => OrderBy {
                expr: self.expr.clone(),
                dir: OrderDir::Desc,
                nulls: Some(Nulls::First),
            },
        }
    }
}

/// "Sorts strictly after the cursor", as one predicate.
///
/// The lexicographic rule, which is the whole of streaming an ordered read: a row
/// comes after the cursor if its first key does, **or** if that key ties and its
/// second does, and so on down to the primary key — which is why the primary key
/// is there at all. Ties are not a corner case: `.orderBy("due")` over a table
/// where fifty invoices share a date will straddle a batch boundary, and without
/// a total order those rows are skipped or served twice with nothing to show for
/// it.
///
/// Nulls are the other half. They sort last ascending and first descending (which
/// is what [`SortKey::order_by`] now states), and `NULL > x` is neither true nor
/// false, so each direction needs its own answer:
///
/// | direction | cursor value | rows after it |
/// |---|---|---|
/// | ascending | a value | `k > v OR k IS NULL` — the nulls are still to come |
/// | ascending | null | *none by this key* — the nulls are last, so only a later key can break the tie |
/// | descending | a value | `k < v` — the nulls came first and are behind us |
/// | descending | null | `k IS NOT NULL` — everything else is still to come |
fn after_predicate(sort: &[SortKey], after: &[Json], table: &str) -> Result<Expr> {
    if after.len() != sort.len() {
        return Err(Error::invalid(format!(
            "this `.iter()` of `{table}` resumed from a cursor of {} value(s) where its \
             ordering has {} — the cursor a batch answers with is the one the next batch \
             sends, unchanged",
            after.len(),
            sort.len()
        )));
    }
    // Every value typed before any of them is built into a predicate, so a
    // cursor that names a value the column cannot hold is refused whole rather
    // than half-lowered.
    let mut values = Vec::with_capacity(after.len());
    for (key, json) in sort.iter().zip(after) {
        let (table, column) = &key.typed;
        values.push(match json.is_null() {
            // A null is never bound: every branch that reads one asks `IS NULL`
            // or `IS NOT NULL`, which take no operand — so a null cursor value
            // never has to survive being coerced against a column that would
            // refuse it for being one.
            true => None,
            false => Some(rows::column_value(table, column, json)?),
        });
    }

    let mut terms: Vec<Expr> = Vec::with_capacity(sort.len());
    for (i, key) in sort.iter().enumerate() {
        let Some(after) = strictly_after(key, values[i].as_ref()) else {
            continue;
        };
        // The keys before this one tie; this one breaks the tie. Chained in the
        // order the read sorts by, so the predicate can be read against the
        // `ORDER BY` it belongs to.
        let ties = (0..i)
            .map(|j| tied(&sort[j], values[j].as_ref()))
            .reduce(Expr::and);
        terms.push(match ties {
            Some(ties) => ties.and(after),
            None => after,
        });
    }
    // Unreachable while the primary key is the last sort key: it is `NOT NULL`
    // and its cursor value is one, so its own term is always built.
    let mut predicate = terms.pop().ok_or_else(|| {
        Error::invalid(format!(
            "this `.iter()` of `{table}` has no ordering that can be resumed from"
        ))
    })?;
    while let Some(term) = terms.pop() {
        predicate = term.or(predicate);
    }
    Ok(predicate)
}

/// "This key ties with the cursor" — null-safely, since a tie on a null is a tie.
fn tied(key: &SortKey, value: Option<&Value>) -> Expr {
    match value {
        Some(value) => key.expr.clone().eq(Expr::lit(value.clone())),
        None => Expr::unary(UnOp::IsNull, key.expr.clone()),
    }
}

/// "This key sorts strictly after the cursor", or `None` when nothing can —
/// which is the ascending null: the nulls sort last, so a row that ties on it
/// can only be told apart by a later key.
fn strictly_after(key: &SortKey, value: Option<&Value>) -> Option<Expr> {
    let expr = || key.expr.clone();
    match (key.dir, value) {
        (Dir::Asc, Some(value)) => {
            let after = Expr::binary(BinOp::Gt, expr(), Expr::lit(value.clone()));
            Some(match key.nullable {
                false => after,
                true => after.or(Expr::unary(UnOp::IsNull, expr())),
            })
        }
        (Dir::Asc, None) => None,
        (Dir::Desc, Some(value)) => Some(Expr::binary(BinOp::Lt, expr(), Expr::lit(value.clone()))),
        (Dir::Desc, None) => Some(Expr::unary(UnOp::IsNotNull, expr())),
    }
}

/// Resolve an `aggregate` plan against the catalog — `.count()` and
/// `.groupBy(…).aggregate({ … })` alike, which is one function because they are
/// one statement with and without a `GROUP BY`.
pub(crate) fn aggregate(
    cat: &Catalog,
    plan: &Plan,
    limits: &HostLimits,
    role: u8,
) -> Result<Aggregate> {
    let table = cat.require(&plan.table)?;
    let low = Lowering::new(cat, &table, role)?;
    let filter = low.filter(plan.filter.as_ref())?;
    // A streamed read resumes from the last row's place in a total order, and
    // the rows a grouped aggregate answers are groups: they have no primary key
    // to break a tie on, and two runs of the same query need not even produce
    // the same number of them.
    if plan.cursor {
        return Err(Error::invalid(format!(
            "`.iter()` streams the **rows** of `{}` and this aggregates them; ask for the \
             groups with `.rows()`, which answers them all at once",
            table.name
        )));
    }
    if plan.aggregate.is_empty() {
        return Err(Error::invalid(format!(
            "this aggregate names no value to compute over `{}`",
            table.name
        )));
    }
    let grouped = !plan.group.is_empty();
    let mut projections = Vec::with_capacity(plan.group.len() + plan.aggregate.len());
    let mut keys: Vec<String> = Vec::with_capacity(projections.capacity());
    let mut group = Vec::with_capacity(plan.group.len());
    // The group keys are projected as well as grouped by: a body reading back
    // `{ n: 3 }` with no idea which author it counted would have to ask again.
    for name in &plan.group {
        let expr = low.value_expr(name)?;
        group.push(expr.clone());
        projections.push(Projection::expr_as(expr, name.clone()));
        keys.push(name.clone());
    }
    // Alias → the aggregate it stands for, which is what a `having` compares:
    // Postgres will not read an output alias in a `HAVING`, so the expression is
    // repeated there rather than named.
    let mut aggregates: BTreeMap<String, Expr> = BTreeMap::new();
    for spec in &plan.aggregate {
        let func = agg_func(&spec.func, &table)?;
        let value = match &spec.arg {
            Some(arg) => Some(low.value_expr(arg)?),
            None if matches!(func, AggFunc::Count) => None,
            None => {
                return Err(Error::invalid(format!(
                    "`{}` over `{}` needs a field or a formula to compute over",
                    spec.func, table.name
                )));
            }
        };
        let expr = aggregate_expr(&func, false, value, &table.name)?;
        // Two values under one key is one value lost on the way back through
        // JSON, so it is refused here rather than silently answered.
        if keys.contains(&spec.alias) {
            return Err(Error::invalid(format!(
                "`{}` is asked for twice in this aggregate over `{}`; give each value its \
                 own name",
                spec.alias, table.name
            )));
        }
        projections.push(Projection::expr_as(expr.clone(), spec.alias.clone()));
        keys.push(spec.alias.clone());
        aggregates.insert(spec.alias.clone(), expr);
    }
    // An aggregate is one `SELECT` with no room for a per-row decision, so a
    // projection that would need the caller's GUCs set cannot be honoured here
    // the way a row read's can. Refused rather than answered with a number the
    // policies never saw.
    if low.in_caller_context.get() && !table.rls_enabled {
        return Err(Error::invalid(format!(
            "this aggregate over `{}` reaches a table with row-level security, which an \
             aggregate cannot carry the caller into — read the rows and aggregate them in \
             your code body instead",
            table.name
        )));
    }

    let mut query = RowQuery::new()
        .where_(filter)
        .group_by(group)
        .having(low.having(plan.having.as_ref(), &aggregates)?);
    // An ordering and a bound belong to a **grouped** aggregate, which answers
    // many rows; over the single row of a scalar aggregate they say nothing, and
    // an `ORDER BY` on a column that is in no `GROUP BY` is not even a statement.
    if grouped {
        let mut order = Vec::with_capacity(plan.order.len());
        for key in &plan.order {
            order.push(match key.dir {
                Dir::Asc => OrderBy::asc(low.grouped_expr(&key.field, &aggregates)?),
                Dir::Desc => OrderBy::desc(low.grouped_expr(&key.field, &aggregates)?),
            });
        }
        query = query
            .order_by(order)
            .limit(bounded_limit(plan, limits, &table)?);
        if let Some(offset) = plan.offset {
            query = query.offset(offset);
        }
    }
    Ok(Aggregate {
        table,
        projections,
        query,
        keys,
        grouped,
    })
}

/// Resolve an `insert` plan against the catalog.
///
/// The rows are checked here — every key a field of the table, not a calculated
/// one, and every value coercible to its column — **before any of them is
/// written**. The row layer would refuse the same things one row at a time, but
/// this milestone has no transactions (§6): a bad third row found on the third
/// `INSERT` would leave the first two written and their events already out.
pub(crate) fn insert(cat: &Catalog, plan: &Plan, limits: &HostLimits) -> Result<Insertion> {
    let table = cat.require(&plan.table)?;
    refuse_read_shaping(plan, "insert", false)?;
    let values = plan.values.as_ref().ok_or_else(|| {
        Error::invalid(format!(
            "`db.{}.insert()` was given no row to write",
            table.name
        ))
    })?;
    let (rows, many) = match values {
        Json::Array(items) => (items.clone(), true),
        one => (vec![one.clone()], false),
    };
    if rows.is_empty() {
        return Err(Error::invalid(format!(
            "`db.{}.insert([])` was given no rows to write",
            table.name
        )));
    }
    if rows.len() as u64 > limits.max_rows {
        return Err(Error::invalid(format!(
            "this insert carries {} rows into `{}`, more than the {} a code body may write \
             in one call",
            rows.len(),
            table.name,
            limits.max_rows
        )));
    }
    for row in &rows {
        writable_values(&table, row, "insert")?;
    }
    Ok(Insertion { table, rows, many })
}

/// Resolve an `update` or `delete` plan against the catalog.
pub(crate) fn write(cat: &Catalog, plan: &Plan, limits: &HostLimits, role: u8) -> Result<Write> {
    let op = match plan.op {
        Op::Update => "update",
        _ => "delete",
    };
    refuse_read_shaping(plan, op, true)?;
    // §4: a whole table rewritten or emptied is not something an *omitted* call
    // should be able to cause. The prelude refuses this too, in front of the
    // author — but the prelude is a convenience and this is the rule, so it is
    // checked again where a guest cannot reach it.
    if plan.filter.is_none() {
        return Err(Error::invalid(format!(
            "`db.{table}.{op}()` with no `.where()` would touch every row of `{table}`; \
             add a `.where()`, or filter on the primary key to name one row",
            table = plan.table
        )));
    }
    let matched = read(cat, plan, limits, role)?;
    let pk = rows::single_pk(&matched.table)?;
    let values = match plan.op {
        Op::Update => Some(assignments(&matched.table, plan)?),
        _ => None,
    };
    Ok(Write {
        matched,
        pk,
        values,
    })
}

/// An update's assignments: an object of writable fields, checked the way an
/// insert's row is.
fn assignments(table: &Table, plan: &Plan) -> Result<Json> {
    let values = plan.values.as_ref().ok_or_else(|| {
        Error::invalid(format!(
            "`db.{}.update()` was given no values to assign",
            table.name
        ))
    })?;
    writable_values(table, values, "update")?;
    Ok(values.clone())
}

/// One row-shaped argument to a write: an object of the table's own writable
/// fields, each value coercible to its column.
fn writable_values(table: &Table, row: &Json, op: &str) -> Result<()> {
    let Json::Object(obj) = row else {
        return Err(Error::invalid(format!(
            "`db.{}.{op}()` takes an object of field values{}",
            table.name,
            match op {
                "insert" => ", or an array of them",
                _ => "",
            }
        )));
    };
    if obj.is_empty() {
        return Err(Error::invalid(format!(
            "this {op} of `{}` names no field to write",
            table.name
        )));
    }
    for (key, json) in obj {
        match table.field(key) {
            None => {
                return Err(Error::invalid(format!(
                    "`{}` has no field `{key}` to {op}",
                    table.name
                )));
            }
            // A calculated field has no column: it is computed from the ones
            // being written, so writing it is a contradiction rather than a
            // permission question.
            Some(field) if field.is_calc() => {
                return Err(Error::invalid(format!(
                    "`{key}` of `{}` is a calculated field and cannot be written",
                    table.name
                )));
            }
            Some(_) => {}
        }
        // The same coercion the row layer will do, done early so a bad value in
        // the last row of a bulk insert is found before the first one is written.
        rows::column_value(table, key, json)?;
    }
    Ok(())
}

/// The chain methods that only shape a **read**, refused on a write rather than
/// ignored.
///
/// A body that wrote `db.books.select("id").update({ … })` meant something an
/// update cannot do, and answering it as though the call were not there is how a
/// write nobody intended gets made quietly. `matches_rows` is true for the two
/// operations that resolve their rows first — an `.orderBy()` and a `.limit()`
/// are meaningful there ("the oldest ten") and meaningless on an insert.
fn refuse_read_shaping(plan: &Plan, op: &str, matches_rows: bool) -> Result<()> {
    let unwanted = [
        (!plan.select.is_empty(), ".select()"),
        (!plan.aggregate.is_empty(), "an aggregate"),
        (plan.pk.is_some(), ".get()"),
        (plan.cursor, ".iter()"),
        (!matches_rows && plan.filter.is_some(), ".where()"),
        (!matches_rows && !plan.order.is_empty(), ".orderBy()"),
        (!matches_rows && plan.limit.is_some(), ".limit()"),
        (!matches_rows && plan.offset.is_some(), ".offset()"),
    ];
    if let Some((_, what)) = unwanted.into_iter().find(|(present, _)| *present) {
        return Err(Error::invalid(format!(
            "`{what}` has no meaning in an `{op}` of `{}` — remove it",
            plan.table
        )));
    }
    refuse_grouping(plan, &format!("an `{op}`"))
}

/// The row cap, applied to the plan's own bound.
///
/// A read is materialised into the isolate, so an unbounded `.rows()` on a large
/// table is an out-of-memory rather than a slow query. The bound the body asked
/// for is honoured up to the cap and **refused** above it — never silently
/// lowered, because a body that asked for 5000 rows and got 1000 would go on to
/// compute a wrong answer from a right-looking one. The read itself asks for one
/// row more than the cap so that an unbounded read can tell "exactly the cap"
/// from "more than we may return".
fn bounded_limit(plan: &Plan, limits: &HostLimits, table: &Table) -> Result<u64> {
    match plan.limit {
        Some(n) if n > limits.max_rows => Err(Error::invalid(format!(
            "a code body may read {} rows at once, and this asked `{}` for {n}; \
             narrow the `.where()` or lower the `.limit()`",
            limits.max_rows, table.name
        ))),
        Some(n) => Ok(n),
        None => Ok(limits.max_rows.saturating_add(1)),
    }
}

/// The size of one batch of an `.iter()`.
///
/// **Clamped where [`bounded_limit`] refuses**, and the difference is the whole
/// distinction between the two calls. A `.limit()` on a `.rows()` is the answer:
/// a body that asked for 5000 rows and was handed 1000 would compute a wrong
/// answer out of a right-looking one, so it is told. A batch size is a hint about
/// round trips: iterating in batches of 1000 rather than the 5000 asked for
/// yields the very same rows in the very same order, and only the number of
/// database calls differs. Refusing that would make `.limit(5000).iter()` — a
/// bound on the *iteration*, which the guest applies itself — an error for no
/// reason a body's author could act on.
fn batch_limit(plan: &Plan, limits: &HostLimits) -> u64 {
    match plan.limit {
        Some(n) => n.clamp(1, limits.max_rows),
        None => limits.max_rows,
    }
}

/// `.groupBy()` and `.having()` shape a **grouped aggregate** and mean nothing
/// anywhere else, so they are refused rather than quietly ignored — a row read
/// that dropped a `.groupBy()` would answer every row where the body expected one
/// per group, which is a wrong answer that looks right.
fn refuse_grouping(plan: &Plan, what: &str) -> Result<()> {
    if !plan.group.is_empty() {
        return Err(Error::invalid(format!(
            "`.groupBy()` has no meaning in {what} of `{}`: a group answers aggregate \
             values, so say which — `.groupBy(\"author\").aggregate({{ n: \"count()\" }}).rows()`",
            plan.table
        )));
    }
    if plan.having.is_some() {
        return Err(Error::invalid(format!(
            "`.having()` bounds the groups of an aggregate and has no meaning in {what} of \
             `{}` — filter the rows with `.where()` instead",
            plan.table
        )));
    }
    Ok(())
}

/// The aggregate a plan named, by the names the chain's terminals use.
fn agg_func(name: &str, table: &Table) -> Result<AggFunc> {
    match name {
        "count" => Ok(AggFunc::Count),
        "sum" => Ok(AggFunc::Sum),
        "avg" => Ok(AggFunc::Avg),
        "min" => Ok(AggFunc::Min),
        "max" => Ok(AggFunc::Max),
        other => Err(Error::invalid(format!(
            "`{other}` is not an aggregate over `{}` — they are count, sum, avg, min, max",
            table.name
        ))),
    }
}

/// `pk = <value>` for a `.get(pk)`, coerced against the key column.
fn pk_expr(table: &Table, pk: &Json) -> Result<Expr> {
    let key = rows::single_pk(table)?;
    let value = rows::column_value(table, &key, pk)?;
    Ok(Expr::col(key).eq(Expr::lit(value)))
}

// ---------------------------------------------------------------------------
// The body's own SQL
// ---------------------------------------------------------------------------

/// The statement one `db.sql(…)` runs: the body's text as it stands, and its
/// arguments as bind values.
///
/// Two things are read out of the text, and only two — both because getting them
/// wrong is silent rather than loud:
///
/// - **One statement.** A `Raw` is prepared and bound, and a backend that is
///   handed `a; b` either refuses the pair or runs the second one unbound. Either
///   way "the code body ran one query" stops being true, so a second statement is
///   refused here, naming the way to run two: two calls.
/// - **No `:name` parameters.** They are §13.4's spelling, where an admin also
///   declares each one's type; here the arguments are positional, so a `:name`
///   is an author expecting the other surface's rewriting. Left alone it would
///   reach the database as a syntax error pointing at a colon, which says
///   nothing about why.
///
/// The scan that answers both is `rewrite_named_params` — the one reader of
/// admin SQL in the codebase, which knows that a `;` inside a literal or a
/// comment separates nothing and that `x::text` is a cast. Its rewritten text is
/// thrown away: what runs is what the body wrote.
pub(crate) fn statement(cat: &Catalog, plan: &SqlPlan) -> Result<Statement> {
    let dialect = cat.primary().dialect();
    let named = rewrite_named_params(dialect, &plan.sql)
        .map_err(|e| Error::invalid(format!("this code body's SQL {e}")))?;
    if named.statements == 0 {
        return Err(Error::invalid(
            "`db.sql()` was given no SQL to run".to_owned(),
        ));
    }
    if named.statements > 1 {
        return Err(Error::invalid(format!(
            "`db.sql()` was given {} statements; it runs one — call it twice, and note \
             that the two do not share a transaction",
            named.statements
        )));
    }
    if let Some(name) = named.params.first() {
        return Err(Error::invalid(format!(
            "`db.sql()` binds its parameters by position, so `:{name}` is not one of them; \
             write `{}` and pass the value in the array (a custom SQL query is the surface \
             that takes `:{name}`)",
            dialect.placeholder(1)
        )));
    }
    let binds = plan.params.iter().map(value_from_json).collect();
    Ok(Statement::raw(plan.sql.clone(), binds))
}

// ---------------------------------------------------------------------------
// Resolving names
// ---------------------------------------------------------------------------

/// What every name in a plan is resolved through: the catalog, the schema shape
/// and this caller's role.
///
/// Its methods take `&self` and record their one side effect — that the
/// statement has to run inside a caller-context transaction, because something
/// it reaches has row-level security — in a [`Cell`], so a resolver can be
/// handed to the shared filter walk as a plain closure.
struct Lowering<'a> {
    catalog: &'a Catalog,
    /// The table being read. Owned (a `Table` is cheap next to a query) so that
    /// the caller may hand its own copy on to the read it is building.
    table: Table,
    /// Every field of the table: the code host has no allow-list, unlike an
    /// agent's tool, because a code body is the admin's own configuration.
    fields: Vec<String>,
    shape: sc_expr::SchemaShape,
    calc: CalcFields,
    /// Which calculated fields the database never sees (milestone 31 §4), so
    /// a filter or an ordering naming one is refused saying why.
    calc_plan: crate::calc_read::CalcPlan,
    user_env: UserEnv,
    role: u8,
    in_caller_context: Cell<bool>,
}

impl<'a> Lowering<'a> {
    fn new(catalog: &'a Catalog, table: &Table, role: u8) -> Result<Lowering<'a>> {
        Ok(Lowering {
            catalog,
            fields: table.fields.iter().map(|f| f.base.name.clone()).collect(),
            shape: catalog.schema_shape()?,
            calc: table.calc_formulas(),
            calc_plan: crate::calc_read::CalcPlan::of(catalog, table)?,
            table: table.clone(),
            // A formula in a plan reads the row and nothing else — see
            // `guard`, which refuses `user` by name rather than inlining a null.
            user_env: UserEnv::Inline(None),
            role,
            in_caller_context: Cell::new(false),
        })
    }

    /// The predicate a plan's `where` means — the object DSL and the formula
    /// spelling, which the shared walk reaches through one resolver so that the
    /// two can be **mixed**: repeated `.where()` calls arrive as
    /// `{ and: [ { … }, { formula: "…" } ] }`.
    fn filter(&self, where_: Option<&Json>) -> Result<Option<Expr>> {
        if let Some(where_) = where_ {
            let mut keys = Vec::new();
            object_keys(where_, &mut keys);
            self.calc_plan
                .refuse_in_query(keys.iter().map(String::as_str), "filter on")?;
        }
        filter::where_resolved(&self.table, &self.fields, where_, &|key, condition| {
            self.filter_key(key, condition)
        })
    }

    /// A filter key the table does not declare: the formula spelling, or a
    /// Ⱶ-path as the column it compares.
    fn filter_key(&self, key: &str, condition: &Json) -> Result<Option<FilterKey>> {
        if key == FORMULA
            && let Some(source) = condition.as_str()
        {
            return Ok(Some(FilterKey::Predicate(self.formula_predicate(source)?)));
        }
        if !key.contains(JOIN) {
            return Ok(None);
        }
        let (target, column) = self.walk(key)?;
        Ok(Some(FilterKey::Joined(Box::new(JoinedColumn {
            expr: join_path_expr(&self.shape, &self.table.name, key).map_err(Error::from)?,
            column,
            table: target,
        }))))
    }

    /// The projection a selected **name** needs, or `None` when the row already
    /// carries it (a column of the table, or a calculated field the row layer
    /// projects itself).
    fn projected(&self, name: &str) -> Result<Option<Expr>> {
        match self.table.field(name) {
            Some(_) => Ok(None),
            None if is_plain_name(name) => Err(Error::invalid(format!(
                "`{}` has no field `{name}` to select",
                self.table.name
            ))),
            None => Ok(Some(self.value_expr(name)?)),
        }
    }

    /// One name in **value** position — a column, a calculated field, a Ⱶ-path or
    /// an expression. What `.orderBy(…)` and `.sum(…)` take, resolved the one
    /// way, so a formula means the same thing wherever a plan puts one.
    fn value_expr(&self, source: &str) -> Result<Expr> {
        match self.table.field(source) {
            Some(field) if !field.is_calc() => Ok(Expr::col(source)),
            // A calculated field has no column; its own expression is what the
            // database computes, inlined by the translator's calc environment.
            Some(_) => self.formula_value(source),
            None if is_plain_name(source) => Err(Error::invalid(format!(
                "`{}` has no field `{source}`",
                self.table.name
            ))),
            None => self.formula_value(source),
        }
    }

    /// The ordering a streamed read sorts and resumes by: the keys the body
    /// named, and the **primary key** after them.
    ///
    /// The primary key is what makes the order total, and a total order is what
    /// makes streaming correct rather than approximately correct: two rows that
    /// tie on every key the body named have no order between them, so a batch
    /// boundary that falls among them serves some twice and skips the rest. It is
    /// appended rather than required of the author, because it is not their
    /// concern — `.orderBy("due")` means "by due date" whether or not the reader
    /// knows why a tie-break is needed. A body that already ended its ordering
    /// with the primary key ascending gets no second copy.
    ///
    /// A key that is an **expression** is refused, and that is the one thing
    /// `.iter()` cannot do that `.rows()` can. Resuming means binding the last
    /// row's value back into a `WHERE`, and a value needs a type: a column has
    /// one, the far end of a Ⱶ-path has one, `qty * price` has none that this
    /// server may assume.
    fn sort_keys(&self, plan: &Plan) -> Result<Vec<SortKey>> {
        let pk = rows::single_pk(&self.table)?;
        let mut keys = Vec::with_capacity(plan.order.len() + 1);
        for key in &plan.order {
            keys.push(self.sort_key(&key.field, key.dir)?);
        }
        let ends_at_pk = plan
            .order
            .last()
            .is_some_and(|key| key.field == pk && key.dir == Dir::Asc);
        if !ends_at_pk {
            keys.push(self.sort_key(&pk, Dir::Asc)?);
        }
        Ok(keys)
    }

    /// One key of that ordering: the expression an unstreamed `.orderBy()` would
    /// sort by, plus what the cursor needs — where its value is typed, and
    /// whether it can be null.
    fn sort_key(&self, field: &str, dir: Dir) -> Result<SortKey> {
        let expr = self.value_expr(field)?;
        // A calculated field has a declared type like any other field, so its
        // value can be typed — but it is an expression over the row, so nothing
        // says it cannot be null even when every column it reads is required.
        if let Some(f) = self.table.field(field) {
            return Ok(SortKey {
                expr,
                dir,
                nullable: f.is_calc() || !f.required,
                typed: (self.table.clone(), field.to_owned()),
            });
        }
        if field.contains(JOIN) {
            let (target, column) = self.walk(field)?;
            return Ok(SortKey {
                expr,
                dir,
                // The far column may be `NOT NULL` and the value still absent:
                // the row this one points at may not exist, or the key may be
                // null, and either way the join answers nothing.
                nullable: true,
                typed: (target, column),
            });
        }
        Err(Error::invalid(format!(
            "`.iter()` of `{}` cannot resume from `{field}`: it orders by an expression, and \
             resuming means comparing against the last row's value, which needs the type of a \
             column. Order by a field or a Ⱶ-path, or read the rows with `.rows()`",
            self.table.name
        )))
    }

    /// One `ORDER BY` key of a **grouped** aggregate.
    ///
    /// An alias of the aggregate wins over a field of the table, which is the
    /// opposite of the rule a filter key follows and right for the same reason it
    /// is right there: the rows a grouped aggregate answers *are* its aliases, so
    /// `.orderBy("n")` beside `{ n: "count()" }` can only mean the count — and a
    /// bare column that is in no `GROUP BY` would not be a statement at all. The
    /// aggregate is repeated rather than named, because the alias is not in scope
    /// in the clause the ordering compiles to on every database.
    fn grouped_expr(&self, name: &str, aggregates: &BTreeMap<String, Expr>) -> Result<Expr> {
        match aggregates.get(name) {
            Some(expr) => Ok(expr.clone()),
            None => self.value_expr(name),
        }
    }

    /// The `HAVING` a plan asked for: the same filter object, over this
    /// aggregate's own values.
    ///
    /// The keys are the aliases and **only** the aliases. A condition on a group
    /// key is a condition on the rows, which is what `.where()` says, and saying
    /// it here would compute the groups before throwing most of them away; a
    /// condition on anything else is a name that is not in the answer. Both are
    /// refused naming the values that exist.
    ///
    /// The operand has no column behind it — `count()` is a number, not a value
    /// of anything — so it is read by its own JSON shape through
    /// [`filter::Operand::Untyped`], while everything else about the condition
    /// (the operators, the null tests, the combinators) is the one shared walk.
    fn having(
        &self,
        having: Option<&Json>,
        aggregates: &BTreeMap<String, Expr>,
    ) -> Result<Option<Expr>> {
        filter::where_resolved(&self.table, &[], having, &|key, condition| {
            let Some(expr) = aggregates.get(key) else {
                return Err(Error::invalid(format!(
                    "`{key}` is not one of this aggregate's values ({}) — a condition on a \
                     group key or a column belongs in `.where()`",
                    aggregates
                        .keys()
                        .map(|a| format!("`{a}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            Ok(Some(FilterKey::Predicate(filter::condition_on(
                &filter::Operand::Untyped { alias: key },
                expr.clone(),
                condition,
            )?)))
        })
    }

    /// A formula in value position: parsed, validated against the schema shape
    /// with this table's row as the bare scope, guarded, and translated.
    fn formula_value(&self, source: &str) -> Result<Expr> {
        let formula = self.parse(source)?;
        self.calc_plan
            .refuse_in_query(formula.free_vars().idents.iter().map(String::as_str), "use")?;
        match translate_value(&formula, &self.env(), &self.shape, &self.table.name) {
            Ok(expr) => Ok(expr),
            Err(e) => Err(self.translation_failed(source, e)),
        }
    }

    /// A formula in predicate position — the string spelling of a `where`.
    fn formula_predicate(&self, source: &str) -> Result<Expr> {
        let formula = self.parse(source)?;
        self.calc_plan.refuse_in_query(
            formula.free_vars().idents.iter().map(String::as_str),
            "filter on",
        )?;
        match translate(
            &formula,
            Operation::Read,
            &self.env(),
            &self.shape,
            &self.table.name,
        ) {
            Ok(expr) => Ok(expr),
            Err(e) => Err(self.translation_failed(source, e)),
        }
    }

    /// Parse and validate one formula, and check what it reaches.
    fn parse(&self, source: &str) -> Result<Formula> {
        let formula = Formula::parse(source)
            .map_err(|e| Error::invalid(format!("`{source}` is not a formula: {e}")))?;
        let analysis = formula.validate(&self.shape, &self.table.name)?;
        // The scope is the row's own fields. `user`, `row`, `old` and `payload`
        // are bindings of the *code body*, which is JavaScript and can splice
        // whatever it likes into the plan — a formula that named one would be a
        // second, weaker way to say something the body already says better.
        if let Some(ambient) = analysis.ambient_outside(&[]) {
            return Err(Error::invalid(format!(
                "`{source}` reads `{}`, which a formula in a code body has no scope for: \
                 its scope is the row of `{}`. Read `{}` in the code body and pass the value \
                 into the filter instead",
                ambient.as_str(),
                self.table.name,
                ambient.as_str(),
            )));
        }
        for path in &analysis.join_paths {
            self.walk(&path.ident)?;
        }
        for use_ in &analysis.agg_uses {
            self.guard_child(&use_.child_table)?;
        }
        Ok(formula)
    }

    /// The environment a plan's formulas translate in.
    fn env(&self) -> Env<'_> {
        Env::new(&self.user_env).with_calc(&self.calc)
    }

    /// A translation failure, said in terms of the plan that caused it.
    ///
    /// The untranslatable case is the interesting one: there is exactly one
    /// expression language, and the part of it the database cannot do is the part
    /// the *code body* is for. So the message names the formula and says where to
    /// put it, which costs the author nothing.
    fn translation_failed(&self, source: &str, e: TranslateError) -> Error {
        match e {
            TranslateError::Untranslatable(what) => Error::invalid(format!(
                "`{source}` cannot be computed by the database ({what}) — compute it in your \
                 code body instead, which is JavaScript and can"
            )),
            TranslateError::Error(e) => e,
        }
    }

    /// Follow a Ⱶ-path from this table, checking each hop the way a read of the
    /// target table would be checked. Answers the final table and column, which
    /// is what a filter literal is coerced against.
    ///
    /// [`ownership::join_guard`] is the check, and it is the same one the REST
    /// `select` embeds and the GraphQL key fields go through: a joined row is
    /// *read*, so the target's read rule holds, and a caller whose access comes
    /// from an ownership formula is refused by name rather than handed a withheld
    /// row one column at a time.
    fn walk(&self, ident: &str) -> Result<(Table, String)> {
        let mut table = self.table.clone();
        let mut segments = ident.split(JOIN).peekable();
        while let Some(segment) = segments.next() {
            let field = table.field(segment).ok_or_else(|| {
                Error::invalid(format!(
                    "`{}` has no field `{segment}` (in `{ident}`)",
                    table.name
                ))
            })?;
            if segments.peek().is_none() {
                return Ok((table.clone(), segment.to_owned()));
            }
            let DataFieldKind::Key { target_table, .. } = &field.kind else {
                return Err(Error::invalid(format!(
                    "`{}`.`{segment}` is not a key to another table, so `{ident}` joins nothing",
                    table.name
                )));
            };
            let target = self.catalog.require(&target_table.0)?;
            match ownership::join_guard(&target, self.role)? {
                JoinAccess::Unrestricted => {}
                JoinAccess::InContext => self.in_caller_context.set(true),
            }
            table = target;
        }
        // Unreachable: a path with no Ⱶ has one segment, which returns above.
        Err(Error::invalid(format!("`{ident}` is not a join path")))
    }

    /// Whether a Ↄ-aggregation over `child` may be computed for this caller —
    /// [`ownership::aggregate_guard`]'s question, asked for the same reason: the
    /// aggregate is a correlated subquery with no room for a per-row decision.
    fn guard_child(&self, child: &str) -> Result<()> {
        let child = self.catalog.require(child)?;
        match ownership::aggregate_guard(self.catalog, &child, &child.name, self.role, None)? {
            AggregateGuard::InContext => self.in_caller_context.set(true),
            AggregateGuard::Predicate(None) => {}
            AggregateGuard::Predicate(Some(_)) => {
                return Err(Error::invalid(format!(
                    "`{}` cannot be aggregated for you here: your access to it comes from its \
                     ownership formula, and the correlated subquery this becomes has no room \
                     for that decision — read `{}` directly instead",
                    child.name, child.name
                )));
            }
        }
        Ok(())
    }
}

/// Whether a name is an ordinary identifier — no Ⱶ, no Ↄ, no operators — and so
/// something that should have been a field of the table rather than an
/// expression to parse.
///
/// The distinction is only about the **message**: `nope` is a typo and gets
/// "`books` has no field `nope`", while `qty * price` is an expression and gets
/// whatever the formula language says about it.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}
/// Every object key anywhere in a `where` document — the names it filters on,
/// under `and`/`or`/`not` as much as at the top.
fn object_keys(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::Object(map) => {
            for (key, sub) in map {
                out.push(key.clone());
                object_keys(sub, out);
            }
        }
        Json::Array(items) => {
            for item in items {
                object_keys(item, out);
            }
        }
        _ => {}
    }
}
