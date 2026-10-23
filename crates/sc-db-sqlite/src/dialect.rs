//! The SQLite SQL dialect.
//!
//! Rendering a [`Statement`](sc_query::Statement) is almost entirely shared: the
//! [`SqlDialect`] trait's default `render` walks the AST and emits clause order,
//! `RETURNING`, `CASE`, `IN` and the JSON access path (`->`), all of which a
//! modern SQLite parses. What a concrete dialect supplies is the handful of
//! things that genuinely differ between backends (technical design §4, §5).
//!
//! For SQLite that is:
//!
//! - **Quoting**: double quotes with any embedded double quote doubled — the
//!   same as Postgres, and the form SQLite documents for identifiers that are
//!   keywords or carry unusual characters. (SQLite also accepts `[…]` and
//!   backticks; it does not need them.)
//! - **Placeholders**: the *numbered* `?1`, `?2`, … form rather than bare `?`.
//!   Numbered is what makes a re-used parameter work: a named custom query whose
//!   `:q` appears twice renders the same number twice and is bound once, exactly
//!   as `$1` twice is under Postgres.
//! - **`ILIKE`**: SQLite has no such operator, because it does not need one —
//!   its `LIKE` is already case-insensitive for ASCII. Spelling it `LIKE` is the
//!   whole of the divergence, which is why [`SqlDialect::binary_op`] is a hook
//!   rather than a reason to copy the renderer.
//!
//! Everything a `Value` literal carries leaves through the bind vector the
//! shared renderer builds, never interpolated — so this dialect is
//! injection-safe by construction, exactly as the query layer guarantees.

use sc_query::{BinOp, SqlDialect, default_bin_op};

/// The SQLite [`SqlDialect`]. Zero-sized: a dialect holds no state, it only
/// spells identifiers, placeholders and the one operator SQLite spells
/// differently.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDialect;

impl SqliteDialect {
    /// Construct the SQLite dialect.
    pub const fn new() -> Self {
        SqliteDialect
    }
}

impl SqlDialect for SqliteDialect {
    fn quote_ident(&self, ident: &str) -> String {
        let mut out = String::with_capacity(ident.len() + 2);
        out.push('"');
        for ch in ident.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
        out
    }

    fn placeholder(&self, position: usize) -> String {
        format!("?{position}")
    }

    fn is_sqlite(&self) -> bool {
        true
    }

    /// SQLite has no row locks: a write transaction (`BEGIN IMMEDIATE`) holds
    /// the whole database already.
    fn row_lock(&self) -> Option<&'static str> {
        None
    }

    fn binary_op(&self, op: BinOp) -> &'static str {
        match op {
            // SQLite's LIKE is case-insensitive for ASCII already; there is no
            // ILIKE to render, and rendering one would be a syntax error.
            BinOp::ILike => "LIKE",
            other => default_bin_op(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use sc_query::{
        Assignment, Delete, Expr, InSet, Insert, JsonStep, Projection, Select, Source, SqlDialect,
        Statement, Update, Value,
    };

    use super::*;

    fn render(stmt: impl Into<Statement>) -> (String, Vec<Value>) {
        SqliteDialect.render(&stmt.into()).expect("render")
    }

    #[test]
    fn a_locking_select_renders_no_lock_clause() {
        // SQLite's write transaction already holds the database.
        let (sql, _) = render(Select::from(Source::table("notes")).for_update());
        assert_eq!(sql, "SELECT * FROM \"notes\"");
    }

    #[test]
    fn quotes_identifiers_and_escapes_embedded_quotes() {
        assert_eq!(SqliteDialect.quote_ident("email"), "\"email\"");
        assert_eq!(SqliteDialect.quote_ident("select"), "\"select\"");
        assert_eq!(SqliteDialect.quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn placeholders_are_numbered_question_marks() {
        assert_eq!(SqliteDialect.placeholder(1), "?1");
        assert_eq!(SqliteDialect.placeholder(42), "?42");
    }

    #[test]
    fn case_insensitive_match_renders_as_like() {
        let (sql, binds) = render(Select::from(Source::table("t")).filter(Expr::Binary {
            op: BinOp::ILike,
            l: Box::new(Expr::col("title")),
            r: Box::new(Expr::lit("%woolf%")),
        }));
        assert_eq!(sql, "SELECT * FROM \"t\" WHERE (\"title\" LIKE ?1)");
        assert_eq!(binds, vec![Value::Text("%woolf%".into())]);
    }

    #[test]
    fn select_insert_update_delete_render_with_numbered_binds() {
        let (sql, binds) = render(
            Select::from(Source::table("users"))
                .columns(vec![Projection::expr(Expr::col("id"))])
                .filter(Expr::col("email").eq(Expr::lit("a@b.c")))
                .limit(10),
        );
        assert_eq!(
            sql,
            "SELECT \"id\" FROM \"users\" WHERE (\"email\" = ?1) LIMIT ?2"
        );
        assert_eq!(binds, vec![Value::Text("a@b.c".into()), Value::Int(10)]);

        let (sql, _) = render(Insert {
            table: "users".into(),
            columns: vec!["email".into()],
            rows: vec![vec![Expr::lit("a@b.c")]],
            returning: vec![Projection::expr(Expr::col("id"))],
        });
        assert_eq!(
            sql,
            "INSERT INTO \"users\" (\"email\") VALUES (?1) RETURNING \"id\""
        );

        let (sql, _) = render(
            Update::new("users", vec![Assignment::new("role", Expr::lit(3_i64))])
                .filter(Expr::col("id").eq(Expr::lit(7_i64))),
        );
        assert_eq!(
            sql,
            "UPDATE \"users\" SET \"role\" = ?1 WHERE (\"id\" = ?2)"
        );

        let (sql, _) = render(Delete::from("users").filter(Expr::col("id").eq(Expr::lit(7_i64))));
        assert_eq!(sql, "DELETE FROM \"users\" WHERE (\"id\" = ?1)");
    }

    #[test]
    fn json_access_uses_the_arrow_operator_and_parameterises_the_path() {
        let json = Expr::Json {
            target: Box::new(Expr::col("data")),
            path: vec![JsonStep::Field("a".into()), JsonStep::Index(0)],
        };
        let (sql, binds) = render(Select::from(Source::table("t")).filter(Expr::In {
            e: Box::new(json),
            set: InSet::List(vec![Expr::lit("x")]),
        }));
        assert_eq!(
            sql,
            "SELECT * FROM \"t\" WHERE (\"data\" -> ?1 -> ?2 IN (?3))"
        );
        assert_eq!(
            binds,
            vec![
                Value::Text("a".into()),
                Value::Int(0),
                Value::Text("x".into())
            ]
        );
    }

    #[test]
    fn injection_payload_stays_a_single_bind() {
        let payload = "'; DROP TABLE users; --";
        let (sql, binds) = render(
            Select::from(Source::table("t")).filter(Expr::col("name").eq(Expr::lit(payload))),
        );
        assert!(
            !sql.contains("DROP TABLE"),
            "payload leaked into SQL: {sql}"
        );
        assert_eq!(binds, vec![Value::Text(payload.into())]);
    }
}
