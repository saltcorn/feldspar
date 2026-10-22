//! Reading rows: what the root fields and the row objects actually do.
//!
//! Every resolver here is a closure built at mount time over two `String`s — the
//! table and, for a row field, the column — and everything else arrives with the
//! request ([`RequestContext`]). That is what makes the schema *data*: a
//! resolver knows the name of the thing it reads, and looks the thing itself up
//! in the live catalog each time.
//!
//! **Not a second data path.** A root list is
//! [`ownership::read_row_values_as`] — the same entry point the agent's
//! `query_table` uses, the same "meets the floor OR the formula grants it", the
//! same RLS routing. `_by_pk` is that query with a primary-key equality ANDed
//! in. Nothing here decides who may read what.
//!
//! **An outgoing key is projected, not fetched.** A requested `manager { email }`
//! does not become a second query: the resolver reads the selection set, asks
//! [`sc_expr::join_path_expr`] for the Ⱶ-join's correlated subquery, and projects
//! it as another column of the *same* `SELECT`, aliased by the join path itself
//! (`managerⱵemail`). The `manager` resolver then hands its children that row
//! with the prefix stripped, so a second hop (`managerⱵbossⱵname`) composes by
//! doing the same thing again. Only the requested leaves are projected — asking
//! for one column of a related row must not cost the whole row.
//!
//! A null key is `null`, not an error: the FK column itself rides back with the
//! row, and a null there ends the walk before any leaf is looked at. That is the
//! Ⱶ operator's own contract and it is what a GraphQL caller expects of a
//! nullable object field.
//!
//! **The joined row is still a read of the other table**, so its read rule holds
//! ([`ownership::join_guard`]): a key is not a way around a floor that a list
//! field over the same table enforces. The one shape a projected join cannot
//! carry is an ownership *formula* — the subquery comes from the schema shape
//! and there is no `WHERE` here to AND a predicate into — so a caller whose
//! access to the target is a formula's is refused by name, exactly as decision
//! 5 refuses an untranslatable aggregate. A wrong row is worse than a refusal.
//!
//! **An incoming key is batched, not walked.** A child list cannot be projected
//! — it is many rows, not one value — so it is the one thing here that costs a
//! second statement. It costs exactly one: [`child_list_field`] says only which
//! rows this parent wants, and [`ChildLoader`](super::loader::ChildLoader)
//! answers every sibling that asked the same thing together.
//!
//! **An incoming key *aggregated* is one value again**, so it goes back to
//! being projected: `employees_aggregate(where: …) { count }` is a correlated
//! subquery in the same `SELECT` as the parent row, and
//! [`child_aggregate_field`] issues no query at all — it reads the column the
//! parent's read already computed. The root `X_aggregate` has no parent to ride
//! with and runs one statement of its own. Either way the *rows* being counted
//! are the ones the caller may read, because the child's rule is folded into
//! the subquery's `WHERE` and a rule that cannot be folded refuses.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_graphql::dataloader::DataLoader;
use async_graphql::dynamic::{FieldFuture, FieldValue, ResolverContext};
use async_graphql::{SelectionField, Value as GqlValue};
use base64::Engine;
use rust_decimal::prelude::ToPrimitive;
use sc_catalog::{DataFieldKind, Table};
use sc_error::{Error, Result};
use sc_expr::JOIN;
use sc_query::{Expr, Projection, Value};

use super::agg;
use super::args;
use super::context::{RequestContext, request_context};
use super::loader::{ChildKey, ChildLoader, ChildRequest};
use super::names::{self, RelationNames};
use crate::convert::value_to_json;
use crate::ownership;
use crate::rows::{self, RowQuery};

/// The shape every resolver in this module has.
pub type Resolver =
    Box<dyn for<'a> Fn(ResolverContext<'a>) -> FieldFuture<'a> + Send + Sync + 'static>;

/// One row as the resolvers pass it down: the values its `SELECT` returned,
/// keyed by column, plus whichever table they came from.
///
/// A joined row is the same type with the join prefix stripped from its keys, so
/// the row object's field resolvers are written once and work at every depth.
#[derive(Clone)]
pub struct RowValue {
    /// The table these values are a row of.
    table: String,
    /// Column (or stripped join path) → value.
    values: BTreeMap<String, Value>,
    /// Which statement produced them.
    origin: Origin,
}

/// Where a row the resolvers are walking came from — which decides what an
/// *absent* value means.
///
/// A read projects exactly what the selection set asked for, so a missing join
/// leaf there means the caller did not ask for it. A write returns the row it
/// wrote and nothing else, so a missing join leaf there means the question was
/// asked and cannot be answered — and answering `null` would be inventing an
/// absent relation. The two need different answers, so the row says which it is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A `SELECT` that was given the selection set (every read, and the row an
    /// insert or an update reads back).
    Read,
    /// A write's `RETURNING`: the row's own columns, and nothing beside them.
    Write,
}

impl RowValue {
    /// A row of `table`, read.
    pub fn new(table: impl Into<String>, values: BTreeMap<String, Value>) -> RowValue {
        RowValue {
            table: table.into(),
            values,
            origin: Origin::Read,
        }
    }

    /// A row of `table` as a write returned it — see [`Origin`].
    pub(super) fn written(table: impl Into<String>, values: BTreeMap<String, Value>) -> RowValue {
        RowValue {
            table: table.into(),
            values,
            origin: Origin::Write,
        }
    }

    /// The value of one column, if this row carries it.
    pub(super) fn get(&self, column: &str) -> Option<&Value> {
        self.values.get(column)
    }

    /// The row on the other side of a Ⱶ-join: every key that starts with
    /// `field` + Ⱶ, with that prefix removed.
    fn joined(&self, field: &str, table: &str) -> RowValue {
        RowValue {
            table: table.to_owned(),
            values: strip_prefix(&self.values, &format!("{field}{JOIN}")),
            origin: self.origin,
        }
    }

    /// The aggregate values this row's query computed under one response key —
    /// the correlated subqueries projected beside its columns.
    fn aggregate(&self, key: &str) -> AggValues {
        AggValues {
            values: strip_prefix(&self.values, &format!("{key}{}", agg::RESPONSE_SEP)),
        }
    }
}

/// The values one aggregate selection produced, keyed by response-key path
/// relative to wherever they are being read from.
///
/// The aggregate object's fields resolve out of this the way a row's fields
/// resolve out of a [`RowValue`], and for the same reason: `sum { salary }` is
/// two levels of GraphQL over one flat `SELECT`, so each level strips its own
/// response key and hands the rest down.
#[derive(Clone, Default)]
pub struct AggValues {
    values: BTreeMap<String, Value>,
}

impl AggValues {
    /// The values under one response key, with that key stripped.
    fn nested(&self, key: &str) -> AggValues {
        AggValues {
            values: strip_prefix(&self.values, &format!("{key}{}", agg::RESPONSE_SEP)),
        }
    }

    /// One value, if the selection that produced these asked for it.
    fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Whether anything at all came back under this key.
    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The entries of `values` whose keys start with `prefix`, with it removed —
/// how one flat `SELECT` answers a nested GraphQL selection at every level.
fn strip_prefix(values: &BTreeMap<String, Value>, prefix: &str) -> BTreeMap<String, Value> {
    values
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(prefix)
                .map(|rest| (rest.to_owned(), v.clone()))
        })
        .collect()
}

/// A `File` field on the wire: the stored path, and the URL the **REST**
/// provider serves the bytes at. Never the bytes themselves — one door into a
/// file is the point of having access rules on it.
pub struct FileRef {
    path: String,
    url: String,
}

/// The parent row, or the error saying a resolver was attached to something that
/// is not one — a schema-building mistake, so it says so plainly.
fn parent_row<'a>(ctx: &ResolverContext<'a>) -> async_graphql::Result<&'a RowValue> {
    ctx.parent_value
        .try_downcast_ref::<RowValue>()
        .map_err(|_| async_graphql::Error::new("this GraphQL field is not resolving over a row"))
}

/// A stored value as the GraphQL scalar it is carried by.
fn scalar(value: &Value) -> GqlValue {
    match value {
        Value::Null => GqlValue::Null,
        // The `Bytes` scalar is base64 text, as its description says; a JSON
        // array of byte numbers would be neither that nor useful.
        Value::Bytes(bytes) => {
            GqlValue::String(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        other => GqlValue::from_json(value_to_json(other)).unwrap_or(GqlValue::Null),
    }
}

/// The root list field for one table.
pub fn list_field(table: impl Into<String>) -> Resolver {
    let table = table.into();
    Box::new(move |ctx| {
        let table = table.clone();
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let table = rc.table(&table)?;
            let query = read_query(rc, &table, &ctx)?;
            Ok(Some(FieldValue::list(read(rc, &table, &query).await?)))
        })
    })
}

/// The root `_by_pk` field: the same read, with a primary-key equality ANDed in.
pub fn by_pk_field(table: impl Into<String>, pk: impl Into<String>) -> Resolver {
    let table = table.into();
    let pk = pk.into();
    Box::new(move |ctx| {
        let (table, pk) = (table.clone(), pk.clone());
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let table = rc.table(&table)?;
            let key = ctx
                .args
                .get(&pk)
                .ok_or_else(|| Error::invalid(format!("`{pk}` is required")))?;
            let value = args::key_value(&table, &pk, key.as_value())?;
            Ok(read_one(rc, &table, &pk, value, &ctx).await?)
        })
    })
}

/// One row of `table` addressed by its primary key, projected for the selection
/// set of the field being resolved.
///
/// What `_by_pk` answers with — and what a mutation reads its written row back
/// through, which is why it is shared: the row a write returns and the row a
/// query returns are then the *same* row, produced by the same rule.
pub(super) async fn read_one(
    rc: &RequestContext,
    table: &Table,
    pk: &str,
    value: Value,
    ctx: &ResolverContext<'_>,
) -> Result<Option<FieldValue<'static>>> {
    let query = read_projections(rc, table, ctx.ctx.field())?
        .onto(RowQuery::new())
        .and_filter(Expr::col(pk).eq(Expr::lit(value)))
        .limit(1);
    Ok(read(rc, table, &query).await?.into_iter().next())
}

/// One stored, calculated or unexposed-key column.
pub fn column_field(column: impl Into<String>) -> Resolver {
    let column = column.into();
    Box::new(move |ctx| {
        let column = column.clone();
        FieldFuture::new(async move {
            let row = parent_row(&ctx)?;
            Ok(row.get(&column).map(|v| FieldValue::value(scalar(v))))
        })
    })
}

/// A secret column (TODO-imap M2): `null` when it holds nothing, the redaction
/// sentinel when it holds anything — never the value, which no client reads.
pub fn secret_field(column: impl Into<String>) -> Resolver {
    let column = column.into();
    Box::new(move |ctx| {
        let column = column.clone();
        FieldFuture::new(async move {
            let row = parent_row(&ctx)?;
            Ok(row
                .get(&column)
                .map(|v| FieldValue::value(scalar(&sc_catalog::secrets::redacted(v)))))
        })
    })
}

/// An outgoing `Key` whose target this application exposes: the row on the other
/// side, built from the leaves the parent query already projected.
pub fn key_field(column: impl Into<String>, target: impl Into<String>) -> Resolver {
    let column = column.into();
    let target = target.into();
    Box::new(move |ctx| {
        let (column, target) = (column.clone(), target.clone());
        FieldFuture::new(async move {
            let row = parent_row(&ctx)?;
            // A null foreign key is a null relation, not an error — and the walk
            // stops here rather than handing down a row of nulls.
            if matches!(row.get(&column), Some(Value::Null)) {
                return Ok(None);
            }
            let joined = row.joined(&column, &target);
            if joined.values.is_empty() {
                // Nothing came back under this join. On a read that means the
                // caller asked for no leaf of it (`{ __typename }`), which is
                // not a relation to invent. On a **write's** returned row it
                // means the question was asked and the statement that answered
                // it did not project the join — a `null` there would say "no
                // related row", which is a different and wrong answer.
                if row.origin == Origin::Write && asks_for_values(&ctx.ctx.field()) {
                    return Err(async_graphql::Error::new(format!(
                        "`{column}` cannot be followed on a row a mutation returned: a write \
                         answers with the row it wrote, and the related row is not part of it — \
                         read `{target}` in a query, or ask for the key's own value"
                    )));
                }
                return Ok(None);
            }
            Ok(Some(FieldValue::owned_any(joined)))
        })
    })
}

/// An incoming key: the child rows referencing this one, **batched**.
///
/// The resolver runs once per parent, and each run only says *which* rows it
/// wants — the relation, this field's arguments, and this parent's key. The
/// [`ChildLoader`] turns the siblings that ask the same thing into one
/// `SELECT … WHERE key IN (…)`, so a level of a query costs a statement rather
/// than a statement per row.
///
/// The rules applied are the **child's**: the load reads through
/// `ownership::read_row_values_as` over the child table, so a caller who may not
/// read it is refused there. Because this field is nullable, that refusal is an
/// error on the field with the rest of the response intact — which is what
/// GraphQL's partial results are for.
pub fn child_list_field(
    child_table: impl Into<String>,
    key_field: impl Into<String>,
    parent_field: impl Into<String>,
) -> Resolver {
    let child_table = child_table.into();
    let key_field = key_field.into();
    let parent_field = parent_field.into();
    Box::new(move |ctx| {
        let (child_table, key_field, parent_field) =
            (child_table.clone(), key_field.clone(), parent_field.clone());
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let row = parent_row(&ctx)?;
            // The column the child key references has to have come back with
            // the parent row. When it did not, the parent was reached through a
            // Ⱶ-join that did not project it — a mistake in this provider, and
            // an empty list would hide it.
            let Some(parent) = row.get(&parent_field) else {
                return Err(async_graphql::Error::new(format!(
                    "`{}`.`{parent_field}` was not read, so its `{child_table}` cannot be",
                    row.table
                )));
            };
            // A parent whose referenced column is null has no children: nothing
            // can equal a null key.
            if parent.is_null() {
                return Ok(Some(FieldValue::list(Vec::<FieldValue<'static>>::new())));
            }
            let child = rc.table(&child_table)?;
            let query = args::child_row_query(&child, &ctx, &key_field, rc.limits.row_cap)?;
            let query = read_projections(rc, &child, ctx.ctx.field())?.onto(query);
            let request = Arc::new(ChildRequest::new(&child.name, &key_field, query));
            let rows = ctx
                .data::<DataLoader<ChildLoader>>()?
                .load_one(ChildKey::new(request, parent.clone()))
                .await
                .map_err(|e| async_graphql::Error::new(e.to_string()))?
                .unwrap_or_default();
            Ok(Some(FieldValue::list(
                rows.iter().cloned().map(FieldValue::owned_any),
            )))
        })
    })
}

/// The root `X_aggregate` field: one statement, answering every value the
/// selection asked for over the rows this caller may read.
///
/// Not a read of rows followed by arithmetic — the database aggregates, over
/// the caller's `where` ANDed with whatever [`ownership::aggregate_values_as`]
/// decides this caller may see. A rule that cannot be expressed as a predicate
/// refuses there, naming the table, rather than answering a number computed
/// over rows the caller cannot read.
pub fn aggregate_field(table: impl Into<String>) -> Resolver {
    let table = table.into();
    Box::new(move |ctx| {
        let table = table.clone();
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let table = rc.table(&table)?;
            let selections = agg::selections(&table, ctx.ctx.field())?;
            let projections = agg::root_projections(&table, &selections)?;
            // `{ departments_aggregate { __typename } }` asks for no value, and
            // a `SELECT` with no columns is not a statement.
            if projections.is_empty() {
                return Ok(Some(FieldValue::owned_any(AggValues::default())));
            }
            let filter = match ctx.args.get(args::ARG_WHERE) {
                Some(arg) => args::where_expr(&table, arg.as_value(), None)?,
                None => None,
            };
            // A root aggregate is a statement of its own — it has no parent
            // read to ride in — so it is charged like one.
            rc.charge(&table.name)?;
            let values = ownership::aggregate_values_as(
                &rc.catalog,
                &table,
                projections,
                filter,
                rc.role(),
                rc.user(),
            )
            .await?;
            Ok(Some(FieldValue::owned_any(AggValues { values })))
        })
    })
}

/// A child `X_aggregate` field: the values the parent's **own** `SELECT`
/// already computed for this row.
///
/// This resolver issues no query. The correlated subqueries were projected as
/// columns of the parent read (see [`collect_child_aggregate`]), so the whole of
/// `departments { name employees_aggregate(where: …) { count } }` is one
/// statement — which is the milestone's motivating case.
///
/// A row reached through a Ⱶ-join carries only what its parent's query
/// projected, and an aggregate correlated to *it* was not projected: that is a
/// refusal naming the table, because the alternative is a query per row.
pub fn child_aggregate_field(child_table: impl Into<String>) -> Resolver {
    let child_table = child_table.into();
    Box::new(move |ctx| {
        let child_table = child_table.clone();
        FieldFuture::new(async move {
            let row = parent_row(&ctx)?;
            let field = ctx.ctx.field();
            let values = row.aggregate(&agg::response_key(&field));
            if values.is_empty() && asks_for_values(&field) {
                return Err(async_graphql::Error::new(match row.origin {
                    Origin::Read => format!(
                        "the aggregate over `{child_table}` was not computed for this row: it \
                         is projected into the query that reads the parent, and a row reached \
                         through a join is not read by one — ask for it on a `{}` read directly",
                        row.table
                    ),
                    Origin::Write => format!(
                        "the aggregate over `{child_table}` cannot be computed on a row a \
                         mutation returned: it is projected into the query that *reads* the \
                         parent, and a write answers with the row it wrote — ask for it in a \
                         query over `{}`",
                        row.table
                    ),
                }));
            }
            Ok(Some(FieldValue::owned_any(values)))
        })
    })
}

/// One aggregate value: `count`, or a column of a `sum`/`avg`/`min`/`max`,
/// carried as the `wire` scalar the schema promised for it.
pub fn agg_value_field(wire: impl Into<String>) -> Resolver {
    let wire = wire.into();
    Box::new(move |ctx| {
        let wire = wire.clone();
        FieldFuture::new(async move {
            let values = parent_agg(&ctx)?;
            let key = agg::response_key(&ctx.ctx.field());
            Ok(values
                .get(&key)
                .map(|v| FieldValue::value(scalar_as(&wire, v))))
        })
    })
}

/// A value as the scalar the schema said this field is.
///
/// One narrowing, and it is Postgres' doing: `sum` over a `bigint` column is
/// `numeric`, because a sum can outgrow what it sums. The schema types that
/// field `BigInt` — a JSON number — and a caller typed against the SDL must not
/// be handed the string an exact decimal rides back as. A sum that genuinely
/// does not fit in 64 bits stays a decimal rather than being truncated into
/// one: the promise was `BigInt`, and a wrong number would be worse than an
/// unexpected shape.
fn scalar_as(wire: &str, value: &Value) -> GqlValue {
    match (wire, value) {
        (names::BIG_INT, Value::Decimal(d)) if d.is_integer() => match d.to_i64() {
            Some(n) => GqlValue::Number(n.into()),
            None => scalar(value),
        },
        _ => scalar(value),
    }
}

/// `sum` / `avg` / `min` / `max`: the values computed under this function, one
/// response key down.
pub fn agg_group_field() -> Resolver {
    Box::new(move |ctx| {
        FieldFuture::new(async move {
            let values = parent_agg(&ctx)?;
            let key = agg::response_key(&ctx.ctx.field());
            Ok(Some(FieldValue::owned_any(values.nested(&key))))
        })
    })
}

/// Whether a selection set asks for anything but introspection.
fn asks_for_values(field: &SelectionField<'_>) -> bool {
    field
        .selection_set()
        .any(|sub| !sub.name().starts_with("__"))
}

/// The parent aggregate values, or the error saying a resolver was attached to
/// something that is not one.
fn parent_agg<'a>(ctx: &ResolverContext<'a>) -> async_graphql::Result<&'a AggValues> {
    ctx.parent_value
        .try_downcast_ref::<AggValues>()
        .map_err(|_| {
            async_graphql::Error::new("this GraphQL field is not resolving over an aggregate")
        })
}

/// A `File` column: its stored path and the REST URL for its bytes.
pub fn file_field(column: impl Into<String>) -> Resolver {
    let column = column.into();
    Box::new(move |ctx| {
        let column = column.clone();
        FieldFuture::new(async move {
            let rc = request_context(&ctx)?;
            let row = parent_row(&ctx)?;
            let Some(Value::Text(path)) = row.get(&column) else {
                return Ok(None);
            };
            if path.is_empty() {
                return Ok(None);
            }
            let table = rc.table(&row.table)?;
            // The bytes are served by the REST provider, addressed by row and
            // field exactly as they are there. Without a primary key in the row
            // there is no address, and a `FileValue` with an unusable `url`
            // would be worse than none.
            let pk = rows::single_pk(&table)?;
            let Some(id) = row.get(&pk).map(value_to_json) else {
                return Ok(None);
            };
            let id = match id {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            Ok(Some(FieldValue::owned_any(FileRef {
                path: path.clone(),
                url: format!(
                    "{}/{}/{id}/{column}",
                    rc.file_mount.trim_end_matches('/'),
                    row.table
                ),
            })))
        })
    })
}

/// `FileValue.path` / `FileValue.url`.
pub fn file_part(part: &'static str) -> Resolver {
    Box::new(move |ctx| {
        FieldFuture::new(async move {
            let file = ctx
                .parent_value
                .try_downcast_ref::<FileRef>()
                .map_err(|_| {
                    async_graphql::Error::new("this field is not resolving over a file")
                })?;
            let value = match part {
                "path" => &file.path,
                _ => &file.url,
            };
            Ok(Some(FieldValue::value(GqlValue::String(value.clone()))))
        })
    })
}

/// The [`RowQuery`] behind one list (or `_by_pk`) field: the caller's arguments,
/// plus the Ⱶ-join projections their selection set implies.
fn read_query(rc: &RequestContext, table: &Table, ctx: &ResolverContext<'_>) -> Result<RowQuery> {
    let query = args::row_query(table, ctx, rc.limits.row_cap)?;
    Ok(read_projections(rc, table, ctx.ctx.field())?.onto(query))
}

/// Run one read and wrap its rows for the executor.
async fn read(
    rc: &RequestContext,
    table: &Table,
    query: &RowQuery,
) -> Result<Vec<FieldValue<'static>>> {
    rc.charge(&table.name)?;
    let rows = ownership::read_row_values_as(
        &rc.catalog,
        table,
        query,
        rc.role(),
        rc.user(),
        rc.evaluator(),
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|values| FieldValue::owned_any(RowValue::new(&table.name, values)))
        .collect())
}

/// Everything one read has to project beyond the row's own columns, collected
/// while walking the selection set.
#[derive(Default)]
struct Extras {
    /// The Ⱶ-join leaves, as join-path identifiers.
    idents: Vec<String>,
    /// The correlated aggregates, already built.
    aggregates: Vec<Projection>,
    /// How many subquery aliases the aggregates have taken, so the next one is
    /// distinct within this statement.
    aliases: usize,
    /// Whether any of them reaches an RLS-protected table, and so whether the
    /// whole statement has to be a caller-context one.
    in_caller_context: bool,
}

/// The extra projections a selection set asks for, and whether they oblige the
/// read to run in the caller's transaction.
///
/// One correlated subquery per requested leaf behind a `Key` field, aliased by
/// the join path itself, and one per requested child aggregate, aliased by its
/// response key. Both are the same trick — the answer to a question about a
/// *related* row, computed as another column of this row's `SELECT` rather than
/// as another statement.
///
/// Which is exactly why the caller context is decided here too: a subquery over
/// a table whose ownership is the database's runs *that* table's policies, and
/// they only exist inside a transaction that has set the caller GUCs (§7.3).
struct ReadPlan {
    /// The extra columns of the `SELECT`.
    extra: Vec<Projection>,
    /// Whether the statement has to run with the caller's GUCs set.
    in_caller_context: bool,
}

impl ReadPlan {
    /// This plan applied to a query: its projections, and its answer to whether
    /// the read is a caller-context one.
    fn onto(self, query: RowQuery) -> RowQuery {
        query
            .projecting(self.extra)
            .requiring_caller_context(self.in_caller_context)
    }
}

fn read_projections(
    rc: &RequestContext,
    table: &Table,
    selection: SelectionField<'_>,
) -> Result<ReadPlan> {
    let mut extras = Extras::default();
    collect_extras(rc, table, selection, None, &mut extras)?;
    let mut out = extras.aggregates;
    if !extras.idents.is_empty() {
        let shape = rc.catalog.schema_shape()?;
        for ident in extras.idents {
            let expr = sc_expr::join_path_expr(&shape, &table.name, &ident).map_err(Error::from)?;
            out.push(Projection::expr_as(expr, ident));
        }
    }
    Ok(ReadPlan {
        extra: out,
        in_caller_context: extras.in_caller_context,
    })
}

/// Walk a selection set collecting what the read has to project.
///
/// `prefix` is the join path reached so far — `None` at the row itself, where a
/// scalar needs no projection because `SELECT *` already has it.
fn collect_extras(
    rc: &RequestContext,
    table: &Table,
    selection: SelectionField<'_>,
    prefix: Option<&str>,
    out: &mut Extras,
) -> Result<()> {
    let cat = &rc.catalog;
    for sub in selection.selection_set() {
        let name = sub.name();
        // Introspection fields (`__typename`) name no column.
        if name.starts_with("__") {
            continue;
        }
        let Some(field) = table.field(name) else {
            // Not a column: an inverse relation. A child **aggregate** over a
            // row this query reads is computed by this query, as another
            // column of it (the milestone's motivating case).
            if prefix.is_none()
                && let Some(rel) = aggregate_relation(rc, table, name)
            {
                collect_child_aggregate(rc, table, &rel, sub, out)?;
                continue;
            }
            // A child **list** is many rows, not a value, so it is read by a
            // query of its own correlated against this row's key — and the key
            // is what has to ride back for that.
            push_key(table, prefix, &mut out.idents);
            continue;
        };
        let ident = match prefix {
            Some(prefix) => format!("{prefix}{JOIN}{name}"),
            None => name.to_owned(),
        };
        match &field.kind {
            DataFieldKind::Key { target_table, .. } => {
                let Ok(target) = cat.require(&target_table.0) else {
                    // A key out of the application's own tables carries its own
                    // value, which `SELECT *` (or the leaf below) already has.
                    push_leaf(prefix, &ident, &mut out.idents);
                    continue;
                };
                // The foreign key's own value, so a null relation can be
                // answered as `null` without looking at a single leaf.
                push_leaf(prefix, &ident, &mut out.idents);
                // A joined row is *read*, so the target's read rule holds — a
                // key must not become a way around the floor that a list field
                // over the same table enforces. Only when the caller actually
                // asks for a value of it: `manager { __typename }` reads
                // nothing and answers from the schema.
                if asks_for_values(&sub) {
                    match ownership::join_guard(&target, rc.role())? {
                        ownership::JoinAccess::Unrestricted => {}
                        // The target's policies decide what the subquery sees,
                        // and outside a caller transaction they see no caller
                        // and yield nothing — which would answer
                        // `manager { email }` with a silent null.
                        ownership::JoinAccess::InContext => out.in_caller_context = true,
                    }
                }
                collect_extras(rc, &target, sub, Some(&ident), out)?;
            }
            // A `File` on the far side of a join needs the *target* row's key to
            // address its bytes by, so it is projected alongside the path.
            DataFieldKind::File { .. } if prefix.is_some() => {
                push_leaf(prefix, &ident, &mut out.idents);
                push_key(table, prefix, &mut out.idents);
            }
            _ => push_leaf(prefix, &ident, &mut out.idents),
        }
    }
    Ok(())
}

/// The inverse relation one field name is the aggregate of, if it is one.
///
/// Asked of the **derived names** rather than re-derived here: which child
/// table `employees_aggregate` means, and which of its keys points back, was
/// decided once when the schema was built, and a second derivation is a second
/// answer waiting to disagree.
fn aggregate_relation(rc: &RequestContext, table: &Table, field: &str) -> Option<RelationNames> {
    rc.names
        .get(&table.name)?
        .relations
        .iter()
        .find(|r| r.aggregate_field == field)
        .cloned()
}

/// Build the correlated subqueries one child-aggregate selection asks for, as
/// columns of the parent's own read.
///
/// Two things are folded into each subquery's `WHERE` beside the correlation:
/// the caller's `where` argument, and — this is the load-bearing part — the
/// **child** table's own read rule. An aggregate must never count a row the
/// caller may not read, and the only place that can be enforced for a count is
/// inside the count.
fn collect_child_aggregate(
    rc: &RequestContext,
    parent: &Table,
    rel: &RelationNames,
    field: SelectionField<'_>,
    out: &mut Extras,
) -> Result<()> {
    let child = rc.table(&rel.child_table)?;
    let selections = agg::selections(&child, field)?;
    if selections.is_empty() {
        return Ok(());
    }
    let arguments = field
        .arguments()
        .map_err(|e| Error::invalid(format!("`{}`: {e}", rel.aggregate_field)))?;
    let filter = arguments
        .iter()
        .find(|(name, _)| name.as_str() == args::ARG_WHERE)
        .map(|(_, value)| value.clone());
    let response_key = agg::response_key(&field);
    let correlation = agg::Correlation {
        child: &child,
        key_field: &rel.key_field,
        parent: &parent.name,
        parent_field: &rel.parent_field,
        response_key: &response_key,
    };
    let mut needs_context = false;
    let projections = agg::child_projections(
        &correlation,
        &selections,
        &mut out.aliases,
        |alias| -> Result<Option<Expr>> {
            let mut predicate = match &filter {
                Some(value) => args::where_expr(&child, value, Some(alias))?,
                None => None,
            };
            match ownership::aggregate_guard(&rc.catalog, &child, alias, rc.role(), rc.user())? {
                ownership::AggregateGuard::Predicate(guard) => {
                    // Both, or whichever there is (`Option::or` on the tail).
                    predicate = match (predicate, guard) {
                        (Some(a), Some(b)) => Some(a.and(b)),
                        (a, b) => a.or(b),
                    };
                }
                // The child's rule is its RLS policies, and a policy only
                // applies inside a caller-context transaction. So the *parent's*
                // read becomes one — the subquery is a column of it, and there
                // is no other statement for the policies to run in. Without
                // that the policies would see no caller and count whatever they
                // make of it, which is a number nobody should trust.
                ownership::AggregateGuard::InContext => needs_context = true,
            }
            Ok(predicate)
        },
    )?;
    out.in_caller_context |= needs_context;
    out.aggregates.extend(projections);
    Ok(())
}

/// Record this table's primary key at the current depth: what a `File` field is
/// addressed by and what a child list correlates against, neither of which the
/// caller named. A relation keyed on some *other* unique column is not covered
/// here; the child-list resolver refuses rather than answering an empty list.
fn push_key(table: &Table, prefix: Option<&str>, out: &mut Vec<String>) {
    if let Ok(pk) = rows::single_pk(table) {
        let ident = match prefix {
            Some(prefix) => format!("{prefix}{JOIN}{pk}"),
            None => pk,
        };
        push_leaf(prefix, &ident, out);
    }
}

/// Record a leaf, but only when it is actually behind a join — a column of the
/// row itself is already in the `SELECT`.
fn push_leaf(prefix: Option<&str>, ident: &str, out: &mut Vec<String>) {
    if prefix.is_some() && !out.iter().any(|i| i == ident) {
        out.push(ident.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_query::Value;

    #[test]
    fn a_joined_row_is_the_parent_with_the_prefix_stripped() {
        // What makes a second hop free: the child resolver does to its row
        // exactly what the parent did to its own.
        let row = RowValue::new(
            "departments",
            BTreeMap::from([
                ("id".to_owned(), Value::Int(1)),
                ("manager".to_owned(), Value::Int(7)),
                (format!("manager{JOIN}email"), Value::Text("a@b".into())),
                (
                    format!("manager{JOIN}boss{JOIN}email"),
                    Value::Text("c@d".into()),
                ),
            ]),
        );
        let manager = row.joined("manager", "users");
        assert_eq!(manager.table, "users");
        assert_eq!(manager.get("email"), Some(&Value::Text("a@b".into())));
        let boss = manager.joined("boss", "users");
        assert_eq!(boss.get("email"), Some(&Value::Text("c@d".into())));
        // The parent's own columns do not leak into the joined row.
        assert_eq!(manager.get("id"), None);
    }

    #[test]
    fn bytes_reach_the_wire_as_base64_not_as_a_list_of_numbers() {
        assert_eq!(
            scalar(&Value::Bytes(vec![1, 2, 3])),
            GqlValue::String("AQID".into())
        );
    }

    #[test]
    fn a_decimal_stays_exact_on_the_wire() {
        // The reason the GraphQL read takes values rather than JSON numbers.
        let value = Value::Decimal("1.100".parse().expect("a decimal"));
        assert_eq!(scalar(&value), GqlValue::String("1.100".into()));
    }
}
