//! Lowering a list field's arguments onto the row layer's own
//! [`RowQuery`](crate::rows::RowQuery).
//!
//! `where`, `order_by`, `limit` and `offset` are the four things a caller may
//! say about *which* rows they want, and each becomes exactly what the row layer
//! already understands: a `sc_query::Expr`, a `Vec<OrderBy>`, and two bounds.
//! Nothing here reaches SQL as text — a column name comes from the catalog, an
//! operator comes from [`crate::filter`], and every literal the caller wrote
//! becomes an [`Expr::Lit`] that the query layer parameterises on render.
//!
//! What one **comparison** means is not decided here: `eq`/`lt`/`in`/`is_null`/…
//! lower through [`crate::filter`], which the REST query string lowers through
//! too, so the two syntaxes ask the database the same question. This module owns
//! the part that is GraphQL's own — the shape of a `BoolExp`, its `_and`/`_or`/
//! `_not` connectives, `order_by`'s input objects, and how a `limit` is bounded.

use async_graphql::Value as GqlValue;
use async_graphql::dynamic::ResolverContext;
use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, OrderBy, UnOp, Value};
use serde_json::Value as Json;

use crate::filter;
use crate::rows::{self, Partition, RowQuery};

/// The list-field argument names, shared with the schema builder so the two
/// cannot drift.
pub const ARG_WHERE: &str = "where";
pub const ARG_ORDER_BY: &str = "order_by";
pub const ARG_LIMIT: &str = "limit";
pub const ARG_OFFSET: &str = "offset";
/// A full-text search phrase (TODO-agent-skills AS1.4). Without an `order_by`,
/// the best match comes first.
pub const ARG_SEARCH: &str = "search";
/// The rows nearest a vector or text, `{ field, to }` (TODO-agent-skills
/// AS2.4); only on a table with a vector field.
pub const ARG_NEAREST: &str = "nearest";
/// `count(distinct: …)` — one column, because `count(DISTINCT a, b)` is not what
/// `sc_query::Expr::Agg` spells.
pub const ARG_DISTINCT: &str = "distinct";

/// The boolean connectives of a `BoolExp`, which are not column names.
const AND: &str = "_and";
const OR: &str = "_or";
const NOT: &str = "_not";

/// A list field's four arguments as one [`RowQuery`].
///
/// `row_cap` is the application's ceiling: an absent `limit` becomes it, and a
/// present one is **clamped** to it. A caller's number is a request, not a
/// permission — a list field with no bound is how a table ends up streamed into
/// a response by accident.
pub fn row_query(table: &Table, ctx: &ResolverContext<'_>, row_cap: u64) -> Result<RowQuery> {
    let mut query = filtered_and_ordered(table, ctx)?;
    let (limit, offset) = bounds(ctx)?;
    query = query.limit(limit.map_or(row_cap, |n| n.min(row_cap)));
    if let Some(n) = offset {
        query = query.offset(n);
    }
    Ok(query)
}

/// A **child** list field's arguments, for a read that answers one parent per
/// group in a single statement.
///
/// The difference from [`row_query`] is what `limit` means. On a root list it
/// bounds the read; on a child list it bounds *each parent's* children, so it
/// becomes a [`Partition`] over the child's key column and the read itself
/// stays unbounded — one `SELECT … WHERE key IN (…)` still answers every parent.
///
/// When the caller names no bound at all there is nothing to partition by, so
/// the read takes the application's row cap as a whole. That cap is shared
/// between the parents, which is why hitting it is an **error** here where the
/// root list simply truncates: nobody can tell which parent's list was cut
/// short, so answering would be answering wrongly.
pub fn child_row_query(
    table: &Table,
    ctx: &ResolverContext<'_>,
    key_field: &str,
    row_cap: u64,
) -> Result<RowQuery> {
    let query = filtered_and_ordered(table, ctx)?;
    let (limit, offset) = bounds(ctx)?;
    if limit.is_none() && offset.is_none() {
        return Ok(query.limit(row_cap));
    }
    Ok(query.per_partition(Partition {
        by: key_field.to_owned(),
        // A per-parent bound is still held to the cap, and an `offset` with no
        // `limit` takes the cap as its bound — the same rule the root list's
        // absent `limit` follows.
        limit: Some(limit.unwrap_or(row_cap).min(row_cap)),
        offset,
    }))
}

/// The `where` and `order_by` halves of a list field's arguments — the part
/// that means the same thing wherever the rows come from.
fn filtered_and_ordered(table: &Table, ctx: &ResolverContext<'_>) -> Result<RowQuery> {
    let mut query = RowQuery::new();
    if let Some(arg) = ctx.args.get(ARG_WHERE)
        && let Some(expr) = where_expr(table, arg.as_value(), None)?
    {
        query = query.where_(Some(expr));
    }
    let search = match ctx.args.get(ARG_SEARCH).map(|a| a.as_value().clone()) {
        None | Some(GqlValue::Null) => None,
        Some(GqlValue::String(phrase)) => Some(crate::search::text_search(table, &phrase, None)?),
        Some(_) => {
            return Err(Error::invalid(format!(
                "`{ARG_SEARCH}` on `{}` must be a string",
                table.name
            )));
        }
    };
    if let Some(arg) = ctx.args.get(ARG_ORDER_BY) {
        query = query.order_by(order_by(table, arg.as_value())?);
    }
    if let Some(search) = search {
        if query.order.is_empty() {
            query = query.order_by(vec![crate::search::relevance(&search)]);
        }
        query = query.searching(search);
    }
    match ctx.args.get(ARG_NEAREST).map(|a| a.as_value().clone()) {
        None | Some(GqlValue::Null) => {}
        Some(value) => {
            let nearest = crate::vector::Nearest::from_json(&to_json(&value)?)?;
            crate::vector::vector_field(table, &nearest.field)?;
            query = query.nearest(nearest);
        }
    }
    Ok(query)
}

/// The `limit` and `offset` a caller named, unclamped: what they *asked* for,
/// which the two callers above bound differently.
fn bounds(ctx: &ResolverContext<'_>) -> Result<(Option<u64>, Option<u64>)> {
    let limit = match ctx.args.get(ARG_LIMIT) {
        Some(arg) => bound(arg.as_value(), ARG_LIMIT)?,
        None => None,
    };
    let offset = match ctx.args.get(ARG_OFFSET) {
        Some(arg) => bound(arg.as_value(), ARG_OFFSET)?,
        None => None,
    };
    Ok((limit, offset))
}

/// A `limit`/`offset` argument: absent or null is `None`, a negative one is an
/// error naming the argument rather than a silently ignored intent.
fn bound(value: &GqlValue, arg: &str) -> Result<Option<u64>> {
    match value {
        GqlValue::Null => Ok(None),
        GqlValue::Number(n) => match n.as_i64() {
            Some(n) if n >= 0 => Ok(Some(n as u64)),
            _ => Err(Error::invalid(format!("`{arg}` must not be negative"))),
        },
        _ => Err(Error::invalid(format!("`{arg}` must be an integer"))),
    }
}

/// A `BoolExp` as a predicate over `table`, or `None` when it constrains nothing.
///
/// `qualifier` is the table alias the columns belong to — `None` for the root
/// query, whose only source needs no qualifier, and `Some(alias)` for a filter
/// evaluated inside a child subquery, where the same expression has to name the
/// subquery's alias instead.
pub fn where_expr(
    table: &Table,
    value: &GqlValue,
    qualifier: Option<&str>,
) -> Result<Option<Expr>> {
    let GqlValue::Object(obj) = value else {
        return match value {
            GqlValue::Null => Ok(None),
            _ => Err(Error::invalid(format!(
                "`{ARG_WHERE}` on `{}` must be an object",
                table.name
            ))),
        };
    };
    let mut conjuncts: Vec<Expr> = Vec::new();
    for (key, sub) in obj.iter() {
        let key = key.as_str();
        let expr = match key {
            AND => all_of(table, sub, qualifier, BinOp::And, key)?,
            OR => all_of(table, sub, qualifier, BinOp::Or, key)?,
            NOT => where_expr(table, sub, qualifier)?.map(|inner| Expr::unary(UnOp::Not, inner)),
            column => column_predicate(table, column, sub, qualifier)?,
        };
        if let Some(expr) = expr {
            conjuncts.push(expr);
        }
    }
    Ok(combine(conjuncts, BinOp::And))
}

/// `_and` / `_or`: a list of `BoolExp`s folded with one connective.
fn all_of(
    table: &Table,
    value: &GqlValue,
    qualifier: Option<&str>,
    op: BinOp,
    key: &str,
) -> Result<Option<Expr>> {
    let GqlValue::List(items) = value else {
        return match value {
            GqlValue::Null => Ok(None),
            _ => Err(Error::invalid(format!(
                "`{key}` on `{}` must be a list of filters",
                table.name
            ))),
        };
    };
    let mut parts = Vec::new();
    for item in items {
        if let Some(expr) = where_expr(table, item, qualifier)? {
            parts.push(expr);
        }
    }
    Ok(combine(parts, op))
}

/// Fold a list of predicates with one connective; `None` for an empty list,
/// which is a filter that constrains nothing rather than one that matches
/// nothing.
fn combine(parts: Vec<Expr>, op: BinOp) -> Option<Expr> {
    parts.into_iter().reduce(|a, b| Expr::binary(op, a, b))
}

/// The comparisons named against one column, ANDed together.
fn column_predicate(
    table: &Table,
    column: &str,
    value: &GqlValue,
    qualifier: Option<&str>,
) -> Result<Option<Expr>> {
    if table.field(column).is_none() {
        return Err(Error::invalid(format!(
            "`{}` has no field `{column}`",
            table.name
        )));
    }
    let GqlValue::Object(ops) = value else {
        return match value {
            GqlValue::Null => Ok(None),
            _ => Err(Error::invalid(format!(
                "the filter on `{}`.`{column}` must be a comparison object",
                table.name
            ))),
        };
    };
    let col = match qualifier {
        Some(alias) => Expr::qcol(alias, column),
        None => Expr::col(column),
    };
    let mut parts = Vec::new();
    for (op, operand) in ops.iter() {
        parts.push(comparison(
            table,
            column,
            col.clone(),
            op.as_str(),
            operand,
        )?);
    }
    Ok(combine(parts, BinOp::And))
}

/// One comparison operator applied to one column — the GraphQL value converted
/// to JSON and handed to the shared lowering.
///
/// The conversion is the whole of GraphQL's part in a comparison: the custom
/// scalars carry themselves (a `Date` arrives as the string a date is written
/// as, a `Decimal` as a string), so what the vocabulary meets is the same JSON a
/// query string's token parses to.
fn comparison(
    table: &Table,
    column: &str,
    col: Expr,
    op: &str,
    operand: &GqlValue,
) -> Result<Expr> {
    filter::comparison(table, column, col, op, &to_json(operand)?)
}

/// An `order_by` argument: a list of single-direction-per-column objects (or one
/// such object), in the precedence the caller wrote them.
pub fn order_by(table: &Table, value: &GqlValue) -> Result<Vec<OrderBy>> {
    let mut keys = Vec::new();
    match value {
        GqlValue::Null => {}
        GqlValue::List(items) => {
            for item in items {
                order_keys(table, item, &mut keys)?;
            }
        }
        single => order_keys(table, single, &mut keys)?,
    }
    Ok(keys)
}

/// The keys one `OrderBy` input object contributes.
fn order_keys(table: &Table, value: &GqlValue, out: &mut Vec<OrderBy>) -> Result<()> {
    let GqlValue::Object(obj) = value else {
        return match value {
            GqlValue::Null => Ok(()),
            _ => Err(Error::invalid(format!(
                "`{ARG_ORDER_BY}` on `{}` takes objects of column: asc|desc",
                table.name
            ))),
        };
    };
    for (column, dir) in obj.iter() {
        if table.field(column.as_str()).is_none() {
            return Err(Error::invalid(format!(
                "`{}` has no field `{column}` to order by",
                table.name
            )));
        }
        let expr = Expr::col(column.as_str());
        // An enum literal in a document arrives as `Enum`; the same enum
        // supplied through a variable arrives as `String`. They mean the same
        // thing, so they are read the same way — this is exactly what
        // `ValueAccessor::enum_name` does.
        match direction(dir) {
            Some("asc") => out.push(OrderBy::asc(expr)),
            Some("desc") => out.push(OrderBy::desc(expr)),
            None => {}
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{}`.`{column}` must be ordered `asc` or `desc`, not `{other}`",
                    table.name
                )));
            }
        }
    }
    Ok(())
}

/// A sort direction as its name, whether it arrived as an enum literal or as a
/// string through a variable. `None` is an explicit null — no ordering asked for
/// on that column.
fn direction(value: &GqlValue) -> Option<&str> {
    match value {
        GqlValue::Enum(name) => Some(name.as_str()),
        GqlValue::String(name) => Some(name.as_str()),
        GqlValue::Null => None,
        _ => Some("(not a direction)"),
    }
}

/// A GraphQL input value as the JSON the row layer's coercion consumes.
///
/// The custom scalars carry themselves: `Date` arrives as the string a date is
/// written as, `BigInt` as a JSON number or a string, `Decimal` as a string —
/// and [`rows::column_value`] parses each against the column it is destined for,
/// which is where a malformed one is named.
fn to_json(value: &GqlValue) -> Result<Json> {
    value
        .clone()
        .into_json()
        .map_err(|e| Error::invalid(format!("this filter value is not a JSON value: {e}")))
}

/// The stored value of one column, for a `_by_pk` key argument: the same
/// coercion a filter literal gets, exposed for the resolver that builds
/// `pk = <argument>` itself.
pub fn key_value(table: &Table, column: &str, value: &GqlValue) -> Result<Value> {
    rows::column_value(table, column, &to_json(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, plain_field, table_of, typed_field};
    use sc_query::{OrderDir, SqlDialect};
    use sc_types::BasicType;

    /// Postgres-flavoured rendering, as in `sc-query`'s own tests.
    struct Pg;

    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }

    fn tasks() -> Table {
        table_of(
            "tasks",
            vec![
                id_field(),
                plain_field("title"),
                typed_field("priority", BasicType::Int),
                typed_field("due", BasicType::Date),
            ],
        )
    }

    fn gql(json: serde_json::Value) -> GqlValue {
        GqlValue::from_json(json).expect("a GraphQL value")
    }

    /// The rendered `WHERE` of a filter, plus its bound parameters.
    fn rendered(value: GqlValue) -> (String, Vec<Value>) {
        let expr = where_expr(&tasks(), &value, None)
            .expect("lowers")
            .expect("a predicate");
        let stmt: sc_query::Statement = sc_query::Select::from(sc_query::Source::table("tasks"))
            .filter(expr)
            .into();
        let (sql, binds) = Pg.render(&stmt).expect("renders");
        let where_ = sql
            .split_once(" WHERE ")
            .map(|(_, w)| w.to_owned())
            .unwrap_or_else(|| panic!("no WHERE in {sql}"));
        (where_, binds)
    }

    #[test]
    fn a_date_filter_binds_a_date_and_not_a_string() {
        // The reason the coercion goes through the row layer: `"2026-01-01"` is
        // a string in JSON and a *date* in the column, and only the second one
        // compares the way the caller meant.
        let (sql, binds) = rendered(gql(serde_json::json!({ "due": { "lt": "2026-01-01" } })));
        assert_eq!(sql, "(\"due\" < $1)");
        assert!(
            matches!(binds.as_slice(), [Value::Date(_)]),
            "expected a bound date, got {binds:?}"
        );
    }

    #[test]
    fn every_comparison_lowers_to_its_operator() {
        for (op, sql_op) in [
            ("eq", "="),
            ("ne", "<>"),
            ("gt", ">"),
            ("gte", ">="),
            ("lt", "<"),
            ("lte", "<="),
        ] {
            let (sql, binds) = rendered(gql(serde_json::json!({ "priority": { op: 3 } })));
            assert_eq!(sql, format!("(\"priority\" {sql_op} $1)"));
            assert_eq!(binds, vec![Value::Int(3)]);
        }
    }

    #[test]
    fn in_and_nin_are_one_membership_test_and_its_negation() {
        let (sql, binds) = rendered(gql(serde_json::json!({ "priority": { "in": [1, 2] } })));
        assert_eq!(sql, "(\"priority\" IN ($1, $2))");
        assert_eq!(binds, vec![Value::Int(1), Value::Int(2)]);

        let (sql, _) = rendered(gql(serde_json::json!({ "priority": { "nin": [1] } })));
        assert_eq!(sql, "(NOT (\"priority\" IN ($1)))");
    }

    #[test]
    fn is_null_asks_about_nullness_and_ne_does_not() {
        let (sql, binds) = rendered(gql(serde_json::json!({ "title": { "is_null": true } })));
        assert_eq!(sql, "(\"title\" IS NULL)");
        assert!(binds.is_empty());
        let (sql, _) = rendered(gql(serde_json::json!({ "title": { "is_null": false } })));
        assert_eq!(sql, "(\"title\" IS NOT NULL)");
    }

    #[test]
    fn a_pattern_is_not_held_to_the_columns_value_rules() {
        // `%urgent%` is a pattern, not a title. Binding it through the column's
        // validation would refuse a perfectly well-formed query on any field
        // with an `options` or `regex` attribute.
        let (sql, binds) = rendered(gql(serde_json::json!({ "title": { "ilike": "%urgent%" } })));
        assert_eq!(sql, "(\"title\" ILIKE $1)");
        assert_eq!(binds, vec![Value::Text("%urgent%".into())]);
    }

    #[test]
    fn the_connectives_nest() {
        let (sql, _) = rendered(gql(serde_json::json!({
            "_or": [
                { "priority": { "gt": 5 } },
                { "_not": { "title": { "is_null": true } } }
            ]
        })));
        assert_eq!(sql, "((\"priority\" > $1) OR (NOT (\"title\" IS NULL)))");
    }

    #[test]
    fn an_empty_filter_constrains_nothing() {
        // `{}` is "no opinion", not "match nothing" — a filter that silently
        // emptied a list would be the worst possible default.
        assert!(
            where_expr(&tasks(), &gql(serde_json::json!({})), None)
                .expect("lowers")
                .is_none()
        );
        assert!(
            where_expr(&tasks(), &GqlValue::Null, None)
                .expect("lowers")
                .is_none()
        );
    }

    #[test]
    fn a_filter_on_an_unknown_column_is_refused_by_name() {
        let err = where_expr(
            &tasks(),
            &gql(serde_json::json!({ "nope": { "eq": 1 } })),
            None,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("has no field `nope`"), "{err}");
    }

    #[test]
    fn a_child_filter_is_rooted_at_the_subquerys_alias() {
        // What a child list's or a child aggregate's `where` needs: the same
        // predicate, qualified with the alias the subquery gave the table.
        let expr = where_expr(
            &tasks(),
            &gql(serde_json::json!({ "priority": { "gt": 1 } })),
            Some("_fd_a1"),
        )
        .expect("lowers")
        .expect("a predicate");
        let stmt: sc_query::Statement = sc_query::Select::from(sc_query::Source::table("tasks"))
            .filter(expr)
            .into();
        let (sql, _) = Pg.render(&stmt).expect("renders");
        assert!(sql.contains("\"_fd_a1\".\"priority\" > $1"), "{sql}");
    }

    #[test]
    fn order_by_keeps_the_callers_precedence() {
        let keys = order_by(
            &tasks(),
            &gql(serde_json::json!([{ "priority": "desc" }, { "title": "asc" }])),
        )
        .expect("lowers");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].dir, OrderDir::Desc);
        assert_eq!(keys[0].expr, Expr::col("priority"));
        assert_eq!(keys[1].dir, OrderDir::Asc);
        assert_eq!(keys[1].expr, Expr::col("title"));
    }

    #[test]
    fn ordering_by_an_unknown_column_is_refused_by_name() {
        let err = order_by(&tasks(), &gql(serde_json::json!([{ "nope": "asc" }]))).unwrap_err();
        assert!(format!("{err}").contains("has no field `nope`"), "{err}");
    }
}
