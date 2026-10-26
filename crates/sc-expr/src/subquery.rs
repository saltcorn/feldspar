//! The correlated-aggregate subquery builder — one implementation of "an
//! aggregate over a child table", shared by every caller that needs one.
//!
//! An aggregate over an incoming key is always the same SQL: a scalar subquery
//! over the child table, correlated back to the parent row by the key, with the
//! aggregate as its projection and any further constraint ANDed into its
//! `WHERE`. `employeesↃdepartment.filter(r => r.salary < 50000).length` and the
//! GraphQL selection `employees_aggregate(where: { salary: { lt: 50000 } })
//! { count }` are the *same question*, so they must be the same
//! [`sc_query::Expr`]:
//!
//! ```sql
//! (SELECT count(*) FROM "employees" "_fd_a1"
//!    WHERE "_fd_a1"."department" = "departments"."id"
//!      AND "_fd_a1"."salary" < $1)
//! ```
//!
//! The reason this is a module rather than a private helper is drift. The empty
//! relation's answer differs per function — `sum` is `0`, `avg`/`min`/`max` are
//! `null` (docs/AGG_EXPRS.md's semantics table *is* the spec, and both
//! evaluators are held to it) — and a second implementation of that table would
//! eventually disagree with the first. [`correlated_aggregate`] carries it once;
//! the formula translator calls it, and so does the GraphQL provider.
//!
//! Everything structural (the function name, the identifiers) comes from code
//! or the catalogue; every value the caller supplies rides in an [`Expr`] and is
//! parameterised on render, as it is everywhere else in the query layer.

use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, Projection, Select, Source};

/// Which aggregate a [`AggregateSpec`] computes.
///
/// The empty-relation answer is a property of the function, so it lives here
/// rather than with the caller: [`Sum`](AggFunc::Sum) and
/// [`StringAgg`](AggFunc::StringAgg) and [`JsonAgg`](AggFunc::JsonAgg)
/// coalesce, the rest do not.
#[derive(Debug, Clone, PartialEq)]
pub enum AggFunc {
    /// `count(*)` with no value expression — rows, not values — or
    /// `count(v)` / `count(DISTINCT v)` with one. Never null: a scalar
    /// aggregate subquery over no rows still counts `0`.
    Count,
    /// `coalesce(sum(v), 0)`. Empty is `0`, not SQL `NULL`: the JS-programmer
    /// expectation (`[].reduce((a, b) => a + b, 0)`), and it keeps a later
    /// `total * rate` from being poisoned by a null.
    Sum,
    /// Bare `avg(v)` — null over no rows.
    Avg,
    /// Bare `min(v)` — null over no rows.
    Min,
    /// Bare `max(v)` — null over no rows.
    Max,
    /// `coalesce(string_agg(v, separator), '')`.
    StringAgg {
        /// The separator expression (a literal, bound on render).
        separator: Expr,
    },
    /// `coalesce(jsonb_agg(v), jsonb_build_array())`: the values as a JSON
    /// array, `[]` over no rows like the JS `map` it translates.
    JsonAgg,
}

impl AggFunc {
    /// The SQL function name. Structural — chosen here, never user data.
    fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::StringAgg { .. } => "string_agg",
            AggFunc::JsonAgg => "jsonb_agg",
        }
    }
}

/// One aggregate over an incoming key: what to aggregate, over which child
/// rows, correlated to which parent.
///
/// The two "parent" fields are separate because the parent is not always a bare
/// table: a formula correlates to the table it is written on, while a GraphQL
/// child aggregate correlates to whatever alias the parent query gave it.
#[derive(Debug, Clone)]
pub struct AggregateSpec {
    /// The child table being aggregated.
    pub child_table: String,
    /// The child's key column — the one referencing the parent.
    pub key_field: String,
    /// The parent's table name **or alias** as it appears in the enclosing
    /// query; the correlation's left-hand qualifier.
    pub parent: String,
    /// The parent column the child key references.
    pub parent_field: String,
    /// The alias given to the child table inside the subquery. The caller owns
    /// uniqueness: two aggregates in one statement need two aliases.
    pub alias: String,
    /// Which aggregate to compute.
    pub func: AggFunc,
    /// Whether the aggregated values are de-duplicated (`DISTINCT`).
    pub distinct: bool,
    /// The expression being aggregated, qualified with [`alias`](Self::alias).
    /// `None` is only valid for [`AggFunc::Count`], where it means `count(*)`.
    pub value: Option<Expr>,
    /// A further constraint on the child rows, **already qualified** with
    /// [`alias`](Self::alias) — a GraphQL `where`, a formula's `filter`, or a
    /// child table's translated ownership predicate. ANDed into the subquery's
    /// `WHERE`, never applied afterwards, so the database does the counting.
    pub filter: Option<Expr>,
}

/// The correlation predicate a child subquery is bound to its parent by:
/// `<alias>.<key_field> = <parent>.<parent_field>`.
///
/// Exposed beside the builder because the paths that select from the child
/// table without aggregating it (a membership test, an ordered pick of one row)
/// must correlate identically.
pub fn correlation(alias: &str, key_field: &str, parent: &str, parent_field: &str) -> Expr {
    Expr::binary(
        BinOp::Eq,
        Expr::qcol(alias.to_string(), key_field.to_string()),
        Expr::qcol(parent.to_string(), parent_field.to_string()),
    )
}

/// Build the correlated scalar subquery for `spec`, with the empty-relation
/// semantics of its [`AggFunc`].
///
/// Fails only when the spec is impossible — a value-aggregate with no value, or
/// `count(DISTINCT *)` — which is a mistake by the caller constructing it, not
/// something a user can provoke.
pub fn correlated_aggregate(spec: AggregateSpec) -> Result<Expr> {
    let AggregateSpec {
        child_table,
        key_field,
        parent,
        parent_field,
        alias,
        func,
        distinct,
        value,
        filter,
    } = spec;

    let mut where_clause = correlation(&alias, &key_field, &parent, &parent_field);
    if let Some(p) = filter {
        where_clause = where_clause.and(p);
    }

    let (args, empty) = agg_parts(&func, distinct, value, &child_table)?;

    let sub = Select::from(Source::table_as(child_table, alias))
        .columns(vec![Projection::expr(Expr::Agg {
            func: func.name().to_string(),
            distinct,
            args,
        })])
        .filter(where_clause);
    let sub = Expr::Subquery(Box::new(sub));

    Ok(match empty {
        Some(default) => coalesce(sub, default),
        None => sub,
    })
}

/// The same aggregate over rows the **enclosing** query already selects: no
/// subquery and no correlation, because there is nothing to correlate to.
///
/// A GraphQL root aggregate (`employees_aggregate(where: …) { sum { salary } }`)
/// is this one: the rows are the ones the caller's filter and the table's
/// ownership predicate leave, and the aggregate is a projection of that same
/// `SELECT`. It shares its decision table with [`correlated_aggregate`], so the
/// empty-relation answers of AGG_EXPRS.md's table — `sum` is `0`, `avg`/`min`/
/// `max` are null — are the same answers whichever way the question is asked.
///
/// `subject` names the table being aggregated, for the error an impossible spec
/// produces.
pub fn aggregate_expr(
    func: &AggFunc,
    distinct: bool,
    value: Option<Expr>,
    subject: &str,
) -> Result<Expr> {
    let (args, empty) = agg_parts(func, distinct, value, subject)?;
    let agg = Expr::Agg {
        func: func.name().to_string(),
        distinct,
        args,
    };
    Ok(match empty {
        Some(default) => coalesce(agg, default),
        None => agg,
    })
}

/// The aggregate's arguments, and what an empty relation must yield in its
/// place — the semantics table of docs/AGG_EXPRS.md, in one place.
fn agg_parts(
    func: &AggFunc,
    distinct: bool,
    value: Option<Expr>,
    subject: &str,
) -> Result<(Vec<Expr>, Option<Expr>)> {
    let needs_value = |v: Option<Expr>| -> Result<Expr> {
        v.ok_or_else(|| {
            Error::invalid(format!(
                "aggregate `{}` over `{subject}` needs a value expression",
                func.name()
            ))
        })
    };
    Ok(match func {
        AggFunc::Count => match value {
            None if distinct => {
                return Err(Error::invalid(format!(
                    "`count(DISTINCT …)` over `{subject}` needs a value expression"
                )));
            }
            None => (vec![], None),
            Some(v) => (vec![v], None),
        },
        AggFunc::Sum => (vec![needs_value(value)?], Some(Expr::lit(0_i64))),
        AggFunc::Avg | AggFunc::Min | AggFunc::Max => (vec![needs_value(value)?], None),
        AggFunc::StringAgg { separator } => (
            vec![needs_value(value)?, separator.clone()],
            Some(Expr::lit("")),
        ),
        AggFunc::JsonAgg => (
            vec![needs_value(value)?],
            Some(Expr::Func {
                name: "jsonb_build_array".into(),
                args: vec![],
            }),
        ),
    })
}

/// `COALESCE(expr, default)`.
fn coalesce(expr: Expr, default: Expr) -> Expr {
    Expr::Func {
        name: "COALESCE".into(),
        args: vec![expr, default],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_query::{SqlDialect, Statement, Value};

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

    /// employees(department → departments.id), aggregated for a departments row.
    fn spec(func: AggFunc, value: Option<Expr>, filter: Option<Expr>) -> AggregateSpec {
        AggregateSpec {
            child_table: "employees".into(),
            key_field: "department".into(),
            parent: "departments".into(),
            parent_field: "id".into(),
            alias: "_fd_a1".into(),
            func,
            distinct: false,
            value,
            filter,
        }
    }

    /// Render a built aggregate as the sole projection of a parent select,
    /// returning the projected expression's SQL and the statement's binds.
    fn projected(spec: AggregateSpec) -> (String, Vec<Value>) {
        let expr = correlated_aggregate(spec).expect("build");
        let stmt: Statement = Select::from(Source::table("departments"))
            .columns(vec![Projection::expr(expr)])
            .into();
        let (sql, binds) = Pg.render(&stmt).unwrap();
        let projection = sql
            .strip_prefix("SELECT ")
            .and_then(|s| s.strip_suffix(" FROM \"departments\""))
            .unwrap_or_else(|| panic!("unexpected statement shape: {sql}"))
            .to_string();
        (projection, binds)
    }

    fn salary_below(n: i64) -> Expr {
        Expr::binary(BinOp::Lt, Expr::qcol("_fd_a1", "salary"), Expr::lit(n))
    }

    #[test]
    fn count_is_count_star_correlated_to_the_parent() {
        let (sql, binds) = projected(spec(AggFunc::Count, None, None));
        assert_eq!(
            sql,
            "(SELECT count(*) FROM \"employees\" AS \"_fd_a1\" \
             WHERE (\"_fd_a1\".\"department\" = \"departments\".\"id\"))"
        );
        assert!(binds.is_empty(), "a bare count binds nothing: {binds:?}");
    }

    #[test]
    fn count_distinct_takes_the_value_and_refuses_without_one() {
        let mut s = spec(AggFunc::Count, Some(Expr::qcol("_fd_a1", "manager")), None);
        s.distinct = true;
        let (sql, _) = projected(s);
        assert!(
            sql.contains("count(DISTINCT \"_fd_a1\".\"manager\")"),
            "got: {sql}"
        );

        // `count(DISTINCT *)` is not a thing; the builder says so rather than
        // quietly counting rows.
        let mut s = spec(AggFunc::Count, None, None);
        s.distinct = true;
        assert!(correlated_aggregate(s).is_err());
    }

    #[test]
    fn sum_coalesces_to_zero_and_the_others_do_not() {
        // The semantics table (docs/AGG_EXPRS.md): sum over no rows is 0, and
        // avg/min/max over no rows are null.
        let (sql, binds) = projected(spec(
            AggFunc::Sum,
            Some(Expr::qcol("_fd_a1", "salary")),
            None,
        ));
        assert_eq!(
            sql,
            "COALESCE((SELECT sum(\"_fd_a1\".\"salary\") FROM \"employees\" AS \"_fd_a1\" \
             WHERE (\"_fd_a1\".\"department\" = \"departments\".\"id\")), $1)"
        );
        assert_eq!(binds, vec![Value::Int(0)]);

        for func in [AggFunc::Avg, AggFunc::Min, AggFunc::Max] {
            let name = func.name();
            let (sql, binds) = projected(spec(func, Some(Expr::qcol("_fd_a1", "salary")), None));
            assert_eq!(
                sql,
                format!(
                    "(SELECT {name}(\"_fd_a1\".\"salary\") FROM \"employees\" AS \"_fd_a1\" \
                     WHERE (\"_fd_a1\".\"department\" = \"departments\".\"id\"))"
                )
            );
            assert!(binds.is_empty(), "{name} should not coalesce: {sql}");
        }
    }

    #[test]
    fn string_agg_coalesces_to_the_empty_string() {
        let (sql, binds) = projected(spec(
            AggFunc::StringAgg {
                separator: Expr::lit(", "),
            },
            Some(Expr::qcol("_fd_a1", "name")),
            None,
        ));
        assert!(
            sql.starts_with("COALESCE((SELECT string_agg(\"_fd_a1\".\"name\", $1)"),
            "got: {sql}"
        );
        assert_eq!(
            binds,
            vec![Value::Text(", ".into()), Value::Text(String::new())]
        );
    }

    #[test]
    fn a_value_aggregate_without_a_value_is_refused() {
        for func in [
            AggFunc::Sum,
            AggFunc::Avg,
            AggFunc::Min,
            AggFunc::Max,
            AggFunc::StringAgg {
                separator: Expr::lit(","),
            },
        ] {
            assert!(
                correlated_aggregate(spec(func.clone(), None, None)).is_err(),
                "{} must require a value",
                func.name()
            );
        }
    }

    #[test]
    fn an_extra_predicate_folds_into_the_subquerys_where() {
        // The milestone's motivating query: employees per department below a
        // salary. One correlated count(*), the bound as a parameter — not
        // inlined, and not a second round trip.
        let (sql, binds) = projected(spec(AggFunc::Count, None, Some(salary_below(50_000))));
        assert_eq!(
            sql,
            "(SELECT count(*) FROM \"employees\" AS \"_fd_a1\" \
             WHERE ((\"_fd_a1\".\"department\" = \"departments\".\"id\") AND \
             (\"_fd_a1\".\"salary\" < $1)))"
        );
        assert_eq!(binds, vec![Value::Int(50_000)]);
        assert!(
            !sql.contains("50000"),
            "the bound must not be inlined: {sql}"
        );
        assert_eq!(sql.matches("SELECT").count(), 1, "one subquery: {sql}");

        // Every function carries the predicate, not just count.
        let (sql, _) = projected(spec(
            AggFunc::Avg,
            Some(Expr::qcol("_fd_a1", "salary")),
            Some(salary_below(50_000)),
        ));
        assert!(
            sql.contains("AND (\"_fd_a1\".\"salary\" < $1)"),
            "got: {sql}"
        );
    }

    /// The bare aggregate as the sole projection of a select over the child.
    fn bare(func: AggFunc, distinct: bool, value: Option<Expr>) -> (String, Vec<Value>) {
        let expr = aggregate_expr(&func, distinct, value, "employees").expect("build");
        let stmt: Statement = Select::from(Source::table("employees"))
            .columns(vec![Projection::expr(expr)])
            .into();
        let (sql, binds) = Pg.render(&stmt).unwrap();
        let projection = sql
            .strip_prefix("SELECT ")
            .and_then(|s| s.strip_suffix(" FROM \"employees\""))
            .unwrap_or_else(|| panic!("unexpected statement shape: {sql}"))
            .to_string();
        (projection, binds)
    }

    #[test]
    fn an_uncorrelated_aggregate_carries_the_same_empty_relation_answers() {
        // The GraphQL root aggregate's form: no subquery, because the rows are
        // the enclosing statement's own — but the same semantics table, since
        // both spellings go through `agg_parts`.
        assert_eq!(bare(AggFunc::Count, false, None).0, "count(*)");
        assert_eq!(
            bare(AggFunc::Count, true, Some(Expr::col("department"))).0,
            "count(DISTINCT \"department\")"
        );
        let (sql, binds) = bare(AggFunc::Sum, false, Some(Expr::col("salary")));
        assert_eq!(sql, "COALESCE(sum(\"salary\"), $1)");
        assert_eq!(binds, vec![Value::Int(0)]);
        // …and the three that answer null over no rows do not coalesce.
        for func in [AggFunc::Avg, AggFunc::Min, AggFunc::Max] {
            let name = match func {
                AggFunc::Avg => "avg",
                AggFunc::Min => "min",
                _ => "max",
            };
            assert_eq!(
                bare(func, false, Some(Expr::col("salary"))).0,
                format!("{name}(\"salary\")")
            );
        }
    }

    #[test]
    fn an_uncorrelated_aggregate_refuses_the_same_impossible_specs() {
        // One decision table, so a value-less `sum` is a mistake wherever it is
        // made — and the message names the table it was about.
        let err = aggregate_expr(&AggFunc::Sum, false, None, "employees").unwrap_err();
        assert!(format!("{err}").contains("employees"), "{err}");
        assert!(aggregate_expr(&AggFunc::Count, true, None, "employees").is_err());
    }

    #[test]
    fn the_parent_qualifier_may_be_an_alias() {
        // A GraphQL child aggregate correlates to whatever the parent query
        // called the parent, which is not always the table's own name.
        let mut s = spec(AggFunc::Count, None, None);
        s.parent = "d".into();
        let expr = correlated_aggregate(s).expect("build");
        let stmt: Statement = Select::from(Source::table_as("departments", "d"))
            .columns(vec![Projection::expr(expr)])
            .into();
        let (sql, _) = Pg.render(&stmt).unwrap();
        assert!(
            sql.contains("\"_fd_a1\".\"department\" = \"d\".\"id\""),
            "got: {sql}"
        );
    }
}
