//! The comparison vocabulary every filtering syntax lowers through.
//!
//! `eq`/`ne`/`gt`/`gte`/`lt`/`lte`/`in`/`nin`/`like`/`ilike`/`is_null` are what a
//! caller may say about one column, and this module is the **one** place that
//! decides what each of them means. A GraphQL `where: { due: { lt: "2026-01-01" } }`
//! and a REST `?due=lt.2026-01-01` are two spellings of one question, and they
//! reach the row layer as the same [`Expr`] — so a Date filter binds a date in
//! both surfaces, `is_null` means SQL's nullness in both, and a comparison added
//! here appears in both at once. A second lowering would be a second answer, and
//! the one that is wrong is the one nobody is reading.
//!
//! Nothing here reaches SQL as text: the column name comes from the catalog, the
//! operator comes from the `match` below, and every literal the caller wrote
//! becomes an [`Expr::Lit`] the query layer parameterises on render.
//!
//! **Literals are coerced against the column, not against JSON.** A filter value
//! goes through the row layer's own [`rows::column_value`], the same function an
//! insert's body goes through, so `"2026-01-01"` binds a *date* and not a string
//! — the comparison the database performs is then the one the column's type
//! means. The one deliberate exception is `like`/`ilike`, whose argument is a
//! **pattern** rather than a value of the column: `"%urgent%"` is not a legal
//! value for a `String` field with an `options` attribute, and holding a pattern
//! to the column's value rules would refuse a query that is perfectly well
//! formed.
//!
//! Null semantics are SQL's, not JavaScript's: `eq`/`ne` are `=` and `<>`, so
//! neither matches a null, and `is_null` is how nullness is asked about. That is
//! what a caller who knows Hasura or PostgREST expects, and inventing a
//! null-safe `eq` here would make the same filter mean different things in a
//! GraphQL `where`, in a REST query string and in an ownership formula.
//!
//! # The filter **object**
//!
//! Above the vocabulary sits the object every non-URL surface speaks —
//! `{ status: "draft", pages: { gte: 100 }, or: [ … ] }` — and it lives here for
//! the same reason the comparisons do. It was an agent trait's `where` argument
//! first (§11.3); a code body's `db.books.where({ … })` (§10.1) is the same
//! object, and a second walk over it would be a second answer to "what does this
//! filter mean" in a place nobody is reading. [`where_expr`] is that walk, and
//! [`required_where`] is it where the filter is not optional.

use sc_catalog::{DataField, Table};
use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, InSet, UnOp, Value};
use serde_json::Value as Json;

use crate::rows;

/// The comparisons a filter may name, in the order an error message lists them.
///
/// Public so a syntax that has to *describe* the vocabulary — a query-string
/// parser refusing an operator by name — quotes this list rather than a second
/// copy of it.
pub const OPERATORS: [&str; 11] = [
    "eq", "ne", "gt", "gte", "lt", "lte", "in", "nin", "like", "ilike", "is_null",
];

/// The name the filter object goes by wherever one is written — an agent tool's
/// argument, a code body's `.where()`. Quoted in this module's messages so they
/// read as the thing the caller typed.
pub const WHERE: &str = "where";

/// The boolean combinators a filter object may name, which are not columns.
const AND: &str = "and";
const OR: &str = "or";
const NOT: &str = "not";

/// One comparison operator applied to one column of `table`.
///
/// `col` is the column *expression* rather than the column name, because the
/// same comparison is built against a bare column (`"due"`) at the root of a
/// query and against a qualified one (`"_fd_a1"."due"`) inside a correlated
/// subquery. `column` is still the catalog name the operand is coerced against.
///
/// The operand is JSON: it is what a GraphQL input value converts to and what a
/// query-string token parses as, and [`rows::column_value`] is the coercion
/// both need anyway.
pub fn comparison(
    table: &Table,
    column: &str,
    col: Expr,
    op: &str,
    operand: &Json,
) -> Result<Expr> {
    comparison_on(&Operand::Column { table, column }, col, op, operand)
}

/// What a filter's operand is coerced against, and what the comparison calls
/// itself when it refuses one.
///
/// Almost always a **column**, which is the rule stated in this module's docs: a
/// literal beside one is coerced through [`rows::column_value`], so `"2026-01-01"`
/// binds a date and the comparison the database performs is the one the column's
/// type means.
///
/// The exception is a `HAVING` over a grouped aggregate (§10.1's `db`): `count()`
/// and `sum(price * qty)` stand behind no column, so there is nothing to coerce
/// against and the operand is read by its own JSON shape. It is a variant here
/// rather than a second walk over the filter object, because "what does
/// `{ gt: 3 }` mean" must have one answer wherever the object is written.
pub enum Operand<'a> {
    /// A column of a table: the operand is a value of that column.
    Column {
        /// The table it belongs to.
        table: &'a Table,
        /// Its name there.
        column: &'a str,
    },
    /// An expression standing behind no column — an aggregate in a `having` —
    /// named by the alias it was asked for under.
    Untyped {
        /// The alias, for the refusal to name.
        alias: &'a str,
    },
}

impl Operand<'_> {
    /// The value one JSON operand binds as.
    fn value(&self, operand: &Json) -> Result<Value> {
        match self {
            Operand::Column { table, column } => rows::column_value(table, column, operand),
            Operand::Untyped { .. } => Ok(sc_expr::value_from_json(operand)),
        }
    }

    /// How a refusal names what was being compared.
    fn qualified(&self) -> String {
        match self {
            Operand::Column { table, column } => format!("`{}`.`{column}`", table.name),
            Operand::Untyped { alias } => format!("`{alias}`"),
        }
    }

    /// Its short name, for a message that has already named the table.
    fn name(&self) -> &str {
        match self {
            Operand::Column { column, .. } => column,
            Operand::Untyped { alias } => alias,
        }
    }
}

/// [`comparison`] against whatever the operand is coerced by.
pub(crate) fn comparison_on(on: &Operand, col: Expr, op: &str, operand: &Json) -> Result<Expr> {
    let lit = |operand: &Json| -> Result<Expr> { Ok(Expr::lit(on.value(operand)?)) };
    let binary = |op: BinOp, operand: &Json| -> Result<Expr> {
        Ok(Expr::binary(op, col.clone(), lit(operand)?))
    };
    match op {
        "eq" => binary(BinOp::Eq, operand),
        "ne" => binary(BinOp::Ne, operand),
        "gt" => binary(BinOp::Gt, operand),
        "gte" => binary(BinOp::Ge, operand),
        "lt" => binary(BinOp::Lt, operand),
        "lte" => binary(BinOp::Le, operand),
        // A pattern is not a value of the column (see the module docs), so it is
        // bound as the text it is.
        "like" => Ok(Expr::binary(BinOp::Like, col, pattern(op, operand)?)),
        "ilike" => Ok(Expr::binary(BinOp::ILike, col, pattern(op, operand)?)),
        "in" | "nin" => {
            let Json::Array(items) = operand else {
                return Err(Error::invalid(format!(
                    "`{op}` on {} takes a list",
                    on.qualified()
                )));
            };
            let set = InSet::List(items.iter().map(&lit).collect::<Result<Vec<_>>>()?);
            let member = Expr::In {
                e: Box::new(col),
                set,
            };
            Ok(match op {
                "in" => member,
                _ => Expr::unary(UnOp::Not, member),
            })
        }
        "is_null" => match operand {
            Json::Bool(true) => Ok(Expr::unary(UnOp::IsNull, col)),
            Json::Bool(false) => Ok(Expr::unary(UnOp::IsNotNull, col)),
            _ => Err(Error::invalid(format!(
                "`is_null` on {} takes a boolean",
                on.qualified()
            ))),
        },
        other => Err(Error::invalid(format!(
            "`{other}` is not a comparison on {} — the comparisons are {}",
            on.qualified(),
            OPERATORS.join(", ")
        ))),
    }
}

/// A `like`/`ilike` pattern, bound as text.
fn pattern(op: &str, operand: &Json) -> Result<Expr> {
    match operand {
        Json::String(s) => Ok(Expr::lit(Value::Text(s.clone()))),
        _ => Err(Error::invalid(format!("`{op}` takes a string pattern"))),
    }
}

// --- the filter object ------------------------------------------------------

/// The predicate a `where` object translates to — every entry ANDed.
///
/// `None` for an absent or empty object, which is "every row" rather than "no
/// rows": a caller who wants a count of everything sends `{}`, and reading that
/// as an unsatisfiable filter would answer zero. A surface for which "every row"
/// is not an acceptable request refuses the absence itself (see
/// [`required_where`]) rather than making this function lie.
///
/// `fields` is the names the caller may filter on — an agent trait's allow-list,
/// or every field of the table for a surface that has none. A name outside it is
/// refused listing the ones that exist, because a caller that cannot read what
/// it got wrong cannot fix it.
pub fn where_expr(table: &Table, fields: &[String], where_: Option<&Json>) -> Result<Option<Expr>> {
    where_resolved(table, fields, where_, &|_, _| Ok(None))
}

/// The fields an application's caller may filter `table` on: every column
/// its REST read returns ([`visible_field`](crate::user_rows::visible_field)),
/// less the calculated ones, which are returned but not filtered on.
pub fn api_filter_fields(table: &Table) -> Vec<String> {
    table
        .fields
        .iter()
        .filter(|field| !field.is_calc())
        .filter(|field| crate::user_rows::visible_field(table, &field.base.name).is_some())
        .map(|field| field.base.name.clone())
        .collect()
}

/// [`where_expr`] over [`api_filter_fields`]: the filter object as an
/// application's caller may write it against `table` — what a live
/// subscription to a `table_changes` stream is filtered by (TODO.md "Live
/// updates" L3.6), so it can name exactly the fields a REST read could and the
/// password hash is as unknown here as there.
pub fn api_where(table: &Table, where_: Option<&Json>) -> Result<Option<Expr>> {
    where_expr(table, &api_filter_fields(table), where_)
}

/// What a filter key that is **not** a column of the table being read may turn
/// out to be, in a surface that has such keys.
pub(crate) enum FilterKey {
    /// A Ⱶ-path: the column the comparison is against, in the table it belongs
    /// to — the *target* table, since that is the value being compared, and the
    /// literal is coerced against it. Boxed because it carries a whole [`Table`]
    /// and the other variant is a pointer.
    Joined(Box<JoinedColumn>),
    /// A whole predicate rather than a comparison: what the **formula** spelling
    /// of a filter is (§3), which is one expression and not a column at all.
    Predicate(Expr),
}

/// A Ⱶ-path, resolved to the correlated subquery it stands for.
pub(crate) struct JoinedColumn {
    /// The table the compared column belongs to.
    pub(crate) table: Table,
    /// Its name there.
    pub(crate) column: String,
    /// The expression that reads it from a row of the table being filtered.
    pub(crate) expr: Expr,
}

/// [`where_expr`] where a key the table does not declare may still resolve — a
/// Ⱶ-path or a formula in a code body's `.where(…)`.
///
/// The resolver is consulted **at every depth**, not only at the top, because a
/// filter object nests: two `.where()` calls that mix the two spellings arrive as
/// `{ and: [ { … }, { formula: "…" } ] }`, and a resolver that only saw the outer
/// object would refuse the inner one as a missing column.
///
/// `Ok(None)` means "not a key I know", and the ordinary unknown-field refusal
/// follows; an `Err` is a key that *was* one and was malformed, which is a better
/// message than "no such field".
pub(crate) fn where_resolved(
    table: &Table,
    fields: &[String],
    where_: Option<&Json>,
    resolve: &dyn Fn(&str, &Json) -> Result<Option<FilterKey>>,
) -> Result<Option<Expr>> {
    let Some(where_) = where_.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let obj = where_.as_object().ok_or_else(|| {
        Error::invalid(format!(
            "`{WHERE}` should be an object of field conditions, got {where_}"
        ))
    })?;
    let mut conjuncts: Vec<Expr> = Vec::new();
    for (name, condition) in obj {
        // A **field wins over a combinator**: an application really may have a
        // column called `or`, and a filter on it must keep meaning what it says.
        // The REST query string makes the same trade for the same reason.
        let expr = if fields.contains(name) {
            let field = queryable_field(table, fields, name, WHERE)?;
            let column = &field.base.name;
            Some(condition_on(
                &Operand::Column { table, column },
                Expr::col(column),
                condition,
            )?)
        } else {
            match name.as_str() {
                AND => combined(table, fields, condition, BinOp::And, name, resolve)?,
                OR => combined(table, fields, condition, BinOp::Or, name, resolve)?,
                NOT => where_resolved(table, fields, Some(condition), resolve)?
                    .map(|inner| Expr::unary(UnOp::Not, inner)),
                // Not a field this caller may use, and not a combinator: a
                // surface with paths and formulas gets to resolve it, and
                // otherwise `queryable_field` refuses it naming the alternatives.
                _ => match resolve(name, condition)? {
                    Some(FilterKey::Joined(joined)) => Some(condition_on(
                        &Operand::Column {
                            table: &joined.table,
                            column: &joined.column,
                        },
                        joined.expr,
                        condition,
                    )?),
                    Some(FilterKey::Predicate(expr)) => Some(expr),
                    None => {
                        let field = queryable_field(table, fields, name, WHERE)?;
                        let column = &field.base.name;
                        Some(condition_on(
                            &Operand::Column { table, column },
                            Expr::col(column),
                            condition,
                        )?)
                    }
                },
            }
        };
        if let Some(expr) = expr {
            conjuncts.push(expr);
        }
    }
    Ok(conjuncts.into_iter().reduce(Expr::and))
}

/// [`where_expr`] where the filter is **not optional** — what a surface that
/// changes rows takes.
///
/// An `update_rows` or a `delete_rows` whose `where` was left out is "every row
/// in the table", and a whole table rewritten or emptied is not something an
/// omitted argument should be able to cause (§10.1 refuses the same thing on the
/// actions, at save time, and a code body's `.update()`/`.delete()` refuses it in
/// the prelude *and* here). A caller that means every row says so with a
/// condition that matches every row.
pub fn required_where(table: &Table, fields: &[String], where_: Option<&Json>) -> Result<Expr> {
    where_expr(table, fields, where_)?.ok_or_else(|| {
        Error::invalid(format!(
            "`{WHERE}` is required and must name at least one condition; \
             this tool will not change every row of `{}` at once",
            table.name
        ))
    })
}

/// `and` / `or`: a list of filter objects folded with one connective.
///
/// An empty list constrains nothing rather than matching nothing, for the reason
/// an empty object does.
fn combined(
    table: &Table,
    fields: &[String],
    value: &Json,
    op: BinOp,
    key: &str,
    resolve: &dyn Fn(&str, &Json) -> Result<Option<FilterKey>>,
) -> Result<Option<Expr>> {
    let Json::Array(items) = value else {
        return match value {
            Json::Null => Ok(None),
            _ => Err(Error::invalid(format!(
                "`{key}` on `{}` takes a list of filters, got {value}",
                table.name
            ))),
        };
    };
    let mut parts = Vec::new();
    for item in items {
        if let Some(expr) = where_resolved(table, fields, Some(item), resolve)? {
            parts.push(expr);
        }
    }
    Ok(parts.into_iter().reduce(|a, b| Expr::binary(op, a, b)))
}

/// One condition, on the column or aggregate the [`Operand`] names.
///
/// A JSON object whose single key is one of [`OPERATORS`] is that comparison;
/// **anything else is a literal to match exactly**, including an object destined
/// for a `Json` column. The rule is stated that way round — and in the agent
/// tools' own descriptions — because the alternative (an object is always an
/// operator) makes a `Json` column unfilterable, and a caller that means equality
/// can always say `{"eq": …}`.
///
/// Two readings are this function's own rather than [`comparison`]'s, and both
/// are about a caller writing JSON rather than a URL: `eq`/`ne` against **null**
/// mean the null tests (`{"eq": null}` is "unset", and SQL's `=` is never true of
/// one), and an **empty** `in` list is refused rather than lowered to a
/// membership test nothing can satisfy.
pub(crate) fn condition_on(on: &Operand, column: Expr, condition: &Json) -> Result<Expr> {
    let col = || column.clone();
    let name = on.name();
    if let Json::Object(map) = condition
        && map.len() == 1
        && let Some((op, operand)) = map.iter().next()
        && OPERATORS.contains(&op.as_str())
    {
        return match (op.as_str(), operand) {
            ("eq", Json::Null) => Ok(Expr::unary(UnOp::IsNull, col())),
            ("ne", Json::Null) => Ok(Expr::unary(UnOp::IsNotNull, col())),
            ("in" | "nin", Json::Array(items)) if items.is_empty() => Err(Error::invalid(format!(
                "`{name}`: `{op}` needs at least one value"
            ))),
            (op, operand) => comparison_on(on, col(), op, operand),
        };
    }
    Ok(match condition {
        Json::Null => Expr::unary(UnOp::IsNull, col()),
        other => Expr::binary(BinOp::Eq, col(), Expr::lit(on.value(other)?)),
    })
}

/// A field the caller may filter on or order by: named in `fields`, real, and
/// backed by a column.
///
/// The error names the alternatives, because a caller that guessed a column name
/// can only recover if it is told the ones that exist — and being told is
/// cheaper than a second round trip through a failed query.
pub fn queryable_field<'a>(
    table: &'a Table,
    fields: &[String],
    name: &str,
    what: &str,
) -> Result<&'a DataField> {
    if !fields.contains(&name.to_owned()) {
        return Err(Error::invalid(format!(
            "`{what}`: `{}` has no field `{name}` you may use; the fields are {}",
            table.name,
            fields.join(", ")
        )));
    }
    let field = table
        .field(name)
        .ok_or_else(|| Error::invalid(format!("`{}` has no field `{name}`", table.name)))?;
    if field.is_calc() {
        return Err(Error::invalid(format!(
            "`{what}`: `{name}` is a calculated field; it is returned with each row \
             but cannot be filtered, ordered on or written"
        )));
    }
    if field.is_collab_doc() {
        return Err(Error::invalid(format!(
            "`{what}`: `{name}` is a collaborative document; it cannot be filtered or \
             ordered on (filter on its text mirror instead, if its stream keeps one)"
        )));
    }
    if field.is_secret() {
        return Err(Error::invalid(format!(
            "`{what}`: `{name}` is a secret; it cannot be filtered or ordered on"
        )));
    }
    Ok(field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, plain_field, table_of, typed_field};
    use sc_query::SqlDialect;
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

    /// The rendered SQL and binds of one comparison.
    fn rendered(expr: &Expr) -> (String, Vec<Value>) {
        let stmt: sc_query::Statement = sc_query::Select::from(sc_query::Source::table("tasks"))
            .filter(expr.clone())
            .into();
        let (sql, binds) = Pg.render(&stmt).expect("renders");
        let where_ = sql
            .split_once(" WHERE ")
            .map(|(_, w)| w.to_owned())
            .unwrap_or_else(|| panic!("no WHERE in {sql}"));
        (where_, binds)
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
            let expr = comparison(
                &tasks(),
                "priority",
                Expr::col("priority"),
                op,
                &serde_json::json!(3),
            )
            .expect("lowers");
            let (sql, binds) = rendered(&expr);
            assert_eq!(sql, format!("(\"priority\" {sql_op} $1)"));
            assert_eq!(binds, vec![Value::Int(3)]);
        }
    }

    #[test]
    fn a_date_filter_binds_a_date_and_not_a_string() {
        // The reason the coercion goes through the row layer: `"2026-01-01"` is
        // a string in JSON and a *date* in the column, and only the second one
        // compares the way the caller meant. Both syntaxes get this for free
        // precisely because they share this function.
        let expr = comparison(
            &tasks(),
            "due",
            Expr::col("due"),
            "lt",
            &serde_json::json!("2026-01-01"),
        )
        .expect("lowers");
        let (_, binds) = rendered(&expr);
        assert!(
            matches!(binds.as_slice(), [Value::Date(_)]),
            "expected a bound date, got {binds:?}"
        );
    }

    #[test]
    fn an_unknown_operator_is_refused_naming_it_and_the_vocabulary() {
        // The message is read by whoever typed the query string, so it says what
        // they could have typed instead.
        let err = comparison(
            &tasks(),
            "priority",
            Expr::col("priority"),
            "between",
            &serde_json::json!(3),
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("`between` is not a comparison"), "{text}");
        assert!(text.contains("gte"), "{text}");
    }
}
