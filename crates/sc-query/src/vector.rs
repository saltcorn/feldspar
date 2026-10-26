//! Vector distance: what a `nearest` read orders by (TODO-agent-skills AS2.3).
//!
//! [`Expr::VectorDistance`](crate::Expr::VectorDistance) is the **cosine
//! distance** between a vector column and a given vector — 0 for the same
//! direction, 2 for the opposite one, **lower is nearer** on both backends:
//!
//! - **Postgres** (pgvector): `col <=> CAST($1 AS vector)`, the operator an HNSW
//!   index with `vector_cosine_ops` answers, so `ORDER BY` it with a `LIMIT` is
//!   an index scan.
//! - **SQLite**: `_fd_cosine_distance(col, ?1)`, a function the driver
//!   registers, over the little-endian `f32` blobs vectors are stored as. There
//!   is no index: a nearest read is a scan of every row, which the row layer
//!   bounds.
//!
//! A vector **written** to a column goes through `CAST(value AS vector(n))`
//! ([`Expr::Cast`](crate::Expr::Cast)): Postgres casts, and the SQLite dialect
//! renders it as `_fd_vector(value, n)`, which turns the JSON array into the
//! blob and refuses the wrong length ([`vector_cast_dimensions`]).

use serde::{Deserialize, Serialize};

use crate::Expr;

/// The cosine distance from `column` to `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorDistance {
    /// The vector column (or any expression of a vector).
    pub column: Expr,
    /// The vector distances are measured to.
    pub to: Vec<f32>,
}

/// The dimensions of a `CAST` to `vector(n)`, which SQLite renders as a
/// function rather than a cast: `Some(n)` for `vector(n)` (any case and
/// spacing), `None` for every other type name.
pub fn vector_cast_dimensions(type_name: &str) -> Option<u32> {
    let compact: String = type_name
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    compact
        .strip_prefix("vector(")?
        .strip_suffix(')')?
        .parse()
        .ok()
}

/// A vector as the little-endian `f32` blob SQLite stores it as.
pub fn vector_blob(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A vector as the JSON array of numbers it is above the driver.
pub fn vector_json(values: &[f32]) -> serde_json::Value {
    serde_json::Value::Array(
        values
            .iter()
            .map(|x| {
                serde_json::Number::from_f64(f64::from(*x))
                    .map_or(serde_json::Value::Null, serde_json::Value::Number)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Insert, OrderBy, Projection, Select, Source, SqlDialect, Statement, Value};

    struct Pg;
    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{ident}\"")
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }
    struct Lite;
    impl SqlDialect for Lite {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{ident}\"")
        }
        fn placeholder(&self, position: usize) -> String {
            format!("?{position}")
        }
        fn is_sqlite(&self) -> bool {
            true
        }
    }

    fn nearest() -> Statement {
        let distance = Expr::VectorDistance(Box::new(VectorDistance {
            column: Expr::qcol("doc", "embedding"),
            to: vec![1.0, 0.5],
        }));
        let mut select = Select::from(Source::table("doc"))
            .columns(vec![Projection::expr_as(distance.clone(), "d")])
            .limit(3);
        select.order = vec![OrderBy::asc(distance)];
        select.into()
    }

    #[test]
    fn postgres_orders_by_pgvectors_cosine_operator() {
        let (sql, binds) = Pg.render(&nearest()).expect("renders");
        assert!(
            sql.contains("(\"doc\".\"embedding\" <=> CAST($1 AS vector)) AS \"d\""),
            "{sql}"
        );
        assert!(
            sql.contains("ORDER BY (\"doc\".\"embedding\" <=> CAST($2 AS vector))"),
            "{sql}"
        );
        assert_eq!(binds[0], Value::Json(serde_json::json!([1.0, 0.5])));
    }

    #[test]
    fn sqlite_calls_the_drivers_function_on_the_blob() {
        let (sql, binds) = Lite.render(&nearest()).expect("renders");
        assert!(
            sql.contains("_fd_cosine_distance(\"doc\".\"embedding\", ?1)"),
            "{sql}"
        );
        assert_eq!(binds[0], Value::Bytes(vector_blob(&[1.0, 0.5])));
    }

    #[test]
    fn a_vector_write_is_a_cast_on_postgres_and_a_function_on_sqlite() {
        let insert: Statement = Insert::row(
            "doc",
            vec!["embedding".into()],
            vec![Expr::Cast {
                expr: Box::new(Expr::lit(Value::Json(serde_json::json!([1, 2])))),
                type_name: "vector(2)".into(),
            }],
        )
        .into();
        let (pg, _) = Pg.render(&insert).expect("renders");
        assert!(pg.contains("CAST($1 AS vector(2))"), "{pg}");
        let (lite, _) = Lite.render(&insert).expect("renders");
        assert!(lite.contains("_fd_vector(?1, 2)"), "{lite}");
        assert_eq!(vector_cast_dimensions("Vector( 2 )"), Some(2));
        assert_eq!(vector_cast_dimensions("uuid"), None);
    }
}
