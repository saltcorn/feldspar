//! The top-level statement AST ([`Statement`]) and its sub-structures.
//!
//! A `Statement` is a plain data value — serializable, inspectable, and
//! buildable by any language through the code adapters (technical design §4).
//! Each [`SqlDialect`](crate::SqlDialect) renders it to concrete SQL; a
//! `TableProvider` may interpret it directly.
//!
//! The four structured statement kinds ([`Select`], [`Insert`], [`Update`],
//! [`Delete`]) are boxed inside the [`Statement`] enum so the enum stays small
//! regardless of how large `Select` grows. A fifth, [`Raw`](Statement::Raw),
//! is the one hole in the representation — see its own documentation for the
//! rule written on it.

use serde::{Deserialize, Serialize};

use crate::{Expr, Value};

/// The top-level query AST.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Statement {
    /// A `SELECT` query.
    Select(Box<Select>),
    /// An `INSERT` statement.
    Insert(Box<Insert>),
    /// An `UPDATE` statement.
    Update(Box<Update>),
    /// A `DELETE` statement.
    Delete(Box<Delete>),
    /// SQL text, rendered by the dialect **as it stands**, with its ordered
    /// bind values.
    ///
    /// This enum is the representation of a query, and raw text is a hole in it.
    /// There is exactly one, it is named, and the rule is written here:
    ///
    /// **A `Raw` is constructed only from an admin-authored query definition,
    /// never from anything a caller sent.** A custom SQL query (§13.4) is SQL an
    /// administrator typed into the application's own configuration; the values
    /// its caller supplies arrive as `binds` and reach the database as bind
    /// parameters, so an argument spelling `'; DROP TABLE …` is a *value*. Code
    /// that assembles `sql` by formatting caller input into it has defeated the
    /// query layer's one safety guarantee.
    ///
    /// `sql` carries the dialect's own placeholders already — it is written by
    /// [`rewrite_named_params`](crate::rewrite_named_params), which turns the
    /// admin's `:name` into them — so rendering appends the binds without
    /// renumbering. That is also why a `Raw` is a **top-level** statement and
    /// never nested inside another: there is nothing to renumber against.
    Raw {
        /// The SQL text, with this dialect's placeholders already in it.
        sql: String,
        /// The bind values the placeholders refer to, in placeholder order.
        binds: Vec<Value>,
        /// The backend's own name for the type each placeholder is to be sent
        /// as, in placeholder order — empty to let the backend infer them.
        ///
        /// Stating them is what makes a custom query behave the same when it is
        /// *run* as when it was *described*: `WHERE (:q IS NULL OR name = :q)`
        /// has no inferable parameter type, so a query that prepared happily
        /// against the admin's declared types would otherwise fail the first
        /// time somebody called it. Backend type names in the AST are the one
        /// place that is not a leak, because a raw statement is written for one
        /// backend by construction.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        param_types: Vec<String>,
    },
}

impl Statement {
    /// A [`Raw`](Statement::Raw) statement whose parameter types the backend
    /// infers. Read that variant's documentation before calling this: `sql` must
    /// be admin-authored, never assembled from caller input.
    pub fn raw(sql: impl Into<String>, binds: Vec<Value>) -> Statement {
        Statement::Raw {
            sql: sql.into(),
            binds,
            param_types: Vec::new(),
        }
    }

    /// [`raw`](Statement::raw) with each placeholder's type stated — see
    /// [`param_types`](Statement::Raw::param_types).
    pub fn raw_typed(
        sql: impl Into<String>,
        binds: Vec<Value>,
        param_types: Vec<String>,
    ) -> Statement {
        Statement::Raw {
            sql: sql.into(),
            binds,
            param_types,
        }
    }
}

impl From<Select> for Statement {
    fn from(s: Select) -> Self {
        Statement::Select(Box::new(s))
    }
}

impl From<Insert> for Statement {
    fn from(s: Insert) -> Self {
        Statement::Insert(Box::new(s))
    }
}

impl From<Update> for Statement {
    fn from(s: Update) -> Self {
        Statement::Update(Box::new(s))
    }
}

impl From<Delete> for Statement {
    fn from(s: Delete) -> Self {
        Statement::Delete(Box::new(s))
    }
}

/// A source of rows in a `FROM` or `JOIN` clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A base table, optionally aliased.
    Table {
        /// Table name.
        name: String,
        /// Optional alias.
        alias: Option<String>,
    },
    /// A derived table (subquery); an alias is required.
    Subquery {
        /// The nested query.
        query: Box<Select>,
        /// The alias the subquery is exposed under.
        alias: String,
    },
    /// The rows of several queries one after another, duplicates kept:
    /// `(SELECT … UNION ALL SELECT …) AS alias` (analytics TODO A1.5).
    ///
    /// A **source** rather than a statement, because every caller so far wants
    /// to go on querying the union — filter it, number it, join it — and a
    /// derived table is the one position both dialects accept that in. The
    /// parts are matched by **position**, as SQL matches them: a caller that
    /// means "by name" lines its projections up itself.
    ///
    /// A part with an `ORDER BY`, `LIMIT` or `OFFSET` of its own is rendered
    /// wrapped in a derived table of its own, because SQLite refuses those
    /// clauses on any but the last member of a compound select and Postgres
    /// would apply the last member's to the whole union.
    UnionAll {
        /// The queries, in order; at least one.
        parts: Vec<Select>,
        /// The alias the union is exposed under.
        alias: String,
    },
    /// **No `FROM` clause at all**: `SELECT 1 AS "x"` is one row computed from
    /// nothing. Both dialects accept it; it is how a list of literal rows is
    /// spelled portably (one such `SELECT` per row, in a
    /// [`UnionAll`](Source::UnionAll)), since SQLite's `VALUES` cannot name its
    /// columns. Refused in a `JOIN`.
    Nothing,
    /// A derived table that may read the columns of the sources before it in
    /// the same `FROM`: `LATERAL (query) AS alias` (analytics TODO A5.4). What
    /// "the nearest of the other's rows to each of these" is: the subquery is
    /// ordered by distance to the outer row and limited to one.
    ///
    /// Postgres only. SQLite has no `LATERAL`, and the one caller, the Spatial
    /// join, is refused on a database without PostGIS before it gets here.
    /// Only a `JOIN`'s source; as a whole `FROM` it is a plain subquery.
    Lateral {
        /// The nested query, which may name the outer sources' aliases.
        query: Box<Select>,
        /// The alias the subquery is exposed under.
        alias: String,
    },
}

impl Source {
    /// A base table with no alias.
    pub fn table(name: impl Into<String>) -> Self {
        Source::Table {
            name: name.into(),
            alias: None,
        }
    }

    /// `(parts[0] UNION ALL parts[1] …) AS alias`.
    pub fn union_all(parts: Vec<Select>, alias: impl Into<String>) -> Self {
        Source::UnionAll {
            parts,
            alias: alias.into(),
        }
    }

    /// A derived table: `(query) AS alias`.
    pub fn subquery(query: Select, alias: impl Into<String>) -> Self {
        Source::Subquery {
            query: Box::new(query),
            alias: alias.into(),
        }
    }

    /// A derived table that reads the sources before it:
    /// `LATERAL (query) AS alias`.
    pub fn lateral(query: Select, alias: impl Into<String>) -> Self {
        Source::Lateral {
            query: Box::new(query),
            alias: alias.into(),
        }
    }

    /// A base table with an alias.
    pub fn table_as(name: impl Into<String>, alias: impl Into<String>) -> Self {
        Source::Table {
            name: name.into(),
            alias: Some(alias.into()),
        }
    }
}

/// A single item in a `SELECT` projection list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// `*` or `table.*`.
    Wildcard {
        /// Optional table qualifier for `table.*`.
        table: Option<String>,
    },
    /// An expression, optionally aliased with `AS`.
    Expr {
        /// The projected expression.
        expr: Expr,
        /// Optional output alias.
        alias: Option<String>,
    },
}

impl Projection {
    /// An unaliased expression projection.
    pub fn expr(expr: impl Into<Expr>) -> Self {
        Projection::Expr {
            expr: expr.into(),
            alias: None,
        }
    }

    /// An `expr AS alias` projection.
    pub fn expr_as(expr: impl Into<Expr>, alias: impl Into<String>) -> Self {
        Projection::Expr {
            expr: expr.into(),
            alias: Some(alias.into()),
        }
    }

    /// A bare `*`.
    pub fn all() -> Self {
        Projection::Wildcard { table: None }
    }
}

/// The kind of a `JOIN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    /// `INNER JOIN`.
    Inner,
    /// `LEFT [OUTER] JOIN`.
    Left,
    /// `RIGHT [OUTER] JOIN`.
    Right,
    /// `FULL [OUTER] JOIN`.
    Full,
    /// `CROSS JOIN` (no `ON` condition).
    Cross,
}

/// A single `JOIN` clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Join {
    /// The join kind.
    pub kind: JoinKind,
    /// The joined source.
    pub source: Source,
    /// The `ON` condition. `None` only for [`JoinKind::Cross`]; a general
    /// [`Expr`] otherwise, so joins on composite or non-primary-key columns are
    /// expressible.
    pub on: Option<Expr>,
}

/// Sort direction for an [`OrderBy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderDir {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

/// Placement of `NULL`s in an ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Nulls {
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// One `ORDER BY` key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBy {
    /// The expression to sort by.
    pub expr: Expr,
    /// Sort direction.
    pub dir: OrderDir,
    /// Optional explicit null placement.
    pub nulls: Option<Nulls>,
}

impl OrderBy {
    /// Ascending order on `expr`, with dialect-default null placement.
    pub fn asc(expr: impl Into<Expr>) -> Self {
        OrderBy {
            expr: expr.into(),
            dir: OrderDir::Asc,
            nulls: None,
        }
    }

    /// Descending order on `expr`, with dialect-default null placement.
    pub fn desc(expr: impl Into<Expr>) -> Self {
        OrderBy {
            expr: expr.into(),
            dir: OrderDir::Desc,
            nulls: None,
        }
    }
}

/// A `SELECT` query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Select {
    /// The primary `FROM` source.
    pub from: Source,
    /// The projection list (`SELECT …`).
    pub columns: Vec<Projection>,
    /// `JOIN` clauses.
    pub joins: Vec<Join>,
    /// `WHERE` condition.
    pub filter: Option<Expr>,
    /// `GROUP BY` keys.
    pub group: Vec<Expr>,
    /// `HAVING` condition.
    pub having: Option<Expr>,
    /// `ORDER BY` keys.
    pub order: Vec<OrderBy>,
    /// `LIMIT`.
    pub limit: Option<u64>,
    /// `OFFSET`.
    pub offset: Option<u64>,
}

impl Select {
    /// A `SELECT *` from a source with no other clauses set. Chain the public
    /// fields (or the helpers) to build up the rest of the query.
    pub fn from(source: Source) -> Self {
        Select {
            from: source,
            columns: vec![Projection::all()],
            joins: Vec::new(),
            filter: None,
            group: Vec::new(),
            having: None,
            order: Vec::new(),
            limit: None,
            offset: None,
        }
    }

    /// Replace the projection list.
    pub fn columns(mut self, columns: Vec<Projection>) -> Self {
        self.columns = columns;
        self
    }

    /// Set the `WHERE` filter.
    pub fn filter(mut self, expr: Expr) -> Self {
        self.filter = Some(expr);
        self
    }

    /// Append a `JOIN`.
    pub fn join(mut self, join: Join) -> Self {
        self.joins.push(join);
        self
    }

    /// Set the `LIMIT`.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Set the `OFFSET`.
    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n);
        self
    }
}

/// An `INSERT` statement. Supports multi-row inserts and `RETURNING`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Insert {
    /// Target table.
    pub table: String,
    /// The columns being written, in row order.
    pub columns: Vec<String>,
    /// One inner `Vec<Expr>` per row; each must match `columns` in length.
    pub rows: Vec<Vec<Expr>>,
    /// `RETURNING` projection (empty for none).
    pub returning: Vec<Projection>,
}

impl Insert {
    /// A single-row insert of `columns = values` into `table`.
    pub fn row(table: impl Into<String>, columns: Vec<String>, values: Vec<Expr>) -> Self {
        Insert {
            table: table.into(),
            columns,
            rows: vec![values],
            returning: Vec::new(),
        }
    }

    /// Set the `RETURNING` projection.
    pub fn returning(mut self, returning: Vec<Projection>) -> Self {
        self.returning = returning;
        self
    }
}

/// A single `SET column = value` assignment in an [`Update`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// Column being assigned.
    pub column: String,
    /// New value expression.
    pub value: Expr,
}

impl Assignment {
    /// `column = value`.
    pub fn new(column: impl Into<String>, value: impl Into<Expr>) -> Self {
        Assignment {
            column: column.into(),
            value: value.into(),
        }
    }
}

/// An `UPDATE` statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    /// Target table.
    pub table: String,
    /// `SET` assignments.
    pub assignments: Vec<Assignment>,
    /// `WHERE` condition. `None` updates every row.
    pub filter: Option<Expr>,
    /// `RETURNING` projection (empty for none).
    pub returning: Vec<Projection>,
}

impl Update {
    /// An `UPDATE table SET …` with no filter yet.
    pub fn new(table: impl Into<String>, assignments: Vec<Assignment>) -> Self {
        Update {
            table: table.into(),
            assignments,
            filter: None,
            returning: Vec::new(),
        }
    }

    /// Set the `WHERE` filter.
    pub fn filter(mut self, expr: Expr) -> Self {
        self.filter = Some(expr);
        self
    }
}

/// A `DELETE` statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delete {
    /// Target table.
    pub table: String,
    /// `WHERE` condition. `None` deletes every row.
    pub filter: Option<Expr>,
    /// `RETURNING` projection (empty for none).
    pub returning: Vec<Projection>,
}

impl Delete {
    /// A `DELETE FROM table` with no filter yet.
    pub fn from(table: impl Into<String>) -> Self {
        Delete {
            table: table.into(),
            filter: None,
            returning: Vec::new(),
        }
    }

    /// Set the `WHERE` filter.
    pub fn filter(mut self, expr: Expr) -> Self {
        self.filter = Some(expr);
        self
    }
}
