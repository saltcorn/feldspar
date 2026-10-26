//! Scalar expressions ([`Expr`]) used inside a [`Statement`](crate::Statement).
//!
//! An `Expr` is the tree of column references, literals, operators, function
//! calls, `IN` sets, JSON accesses, and `CASE` branches that appear in a
//! `WHERE`/`ON`/projection/`SET` position. Everything is plain, serializable
//! data (technical design §4), so a query can be inspected or reconstructed by a
//! code adapter.
//!
//! Two design requirements shape the tree:
//!
//! - **Composite keys and foreign keys to non-primary-key columns** — a join or
//!   filter condition is an arbitrary [`Expr`] over general [`ColRef`]s, never
//!   "the id column".
//! - **JSON is built in** — [`Expr::Json`] (paired with [`Value::Json`]) is a
//!   first-class access path, not an add-on.
//!
//! Literals live in [`Expr::Lit`] and are **always parameterised on render**;
//! constructing a literal here never interpolates it into SQL text.

use serde::{Deserialize, Serialize};

use crate::{OrderBy, Value};

/// A table-qualified column reference.
///
/// `table` is optional because a column may be unambiguous in a single-source
/// query; when a join is present it should be set. The reference is a plain
/// name pair so it can point at any column — including a non-primary-key column
/// that a foreign key targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColRef {
    /// Optional table (or alias) qualifier.
    pub table: Option<String>,
    /// Column name.
    pub column: String,
}

impl ColRef {
    /// A column with no table qualifier.
    pub fn bare(column: impl Into<String>) -> Self {
        ColRef {
            table: None,
            column: column.into(),
        }
    }

    /// A `table.column` reference.
    pub fn qualified(table: impl Into<String>, column: impl Into<String>) -> Self {
        ColRef {
            table: Some(table.into()),
            column: column.into(),
        }
    }
}

/// Binary operators supported by the MVP AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// Logical `AND`.
    And,
    /// Logical `OR`.
    Or,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `LIKE`
    Like,
    /// `ILIKE`
    ILike,
    /// String concatenation (`||`).
    Concat,
    /// `IS NOT DISTINCT FROM` — null-safe equality: `NULL` on either side
    /// compares as a value instead of poisoning the result. Added for ownership
    /// formulas (§7.3), whose JavaScript `===` is two-valued: `owner === id`
    /// with a null `owner` must be *false*, and its negation *true*, which
    /// plain `=` cannot express.
    IsNotDistinct,
    /// `IS DISTINCT FROM` — null-safe inequality (the negation of
    /// [`IsNotDistinct`](BinOp::IsNotDistinct)).
    IsDistinct,
}

/// Unary operators supported by the MVP AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnOp {
    /// Logical `NOT`.
    Not,
    /// Arithmetic negation (`-`).
    Neg,
    /// `IS NULL`.
    IsNull,
    /// `IS NOT NULL`.
    IsNotNull,
}

/// One step of a JSON access path (`Expr::Json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonStep {
    /// Object field access by key.
    Field(String),
    /// Array element access by index.
    Index(i64),
}

/// The right-hand side of an `IN` expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InSet {
    /// A literal list of expressions: `IN (a, b, c)`.
    List(Vec<Expr>),
    /// A scalar subquery: `IN (SELECT …)`.
    Subquery(Box<crate::Select>),
}

/// One `WHEN … THEN …` arm of a [`Expr::Case`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseArm {
    /// The `WHEN` condition (a boolean expression, or a value to match against
    /// the case operand when one is given).
    pub when: Expr,
    /// The `THEN` result.
    pub then: Expr,
}

/// A scalar expression tree.
///
/// Kept intentionally minimal — extended only as real queries demand it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    /// A (possibly table-qualified) column reference.
    Col(ColRef),
    /// A literal value; always parameterised on render.
    Lit(Value),
    /// A positional bind parameter (0-based index into the caller's binds).
    Param(usize),
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinOp,
        /// Left operand.
        l: Box<Expr>,
        /// Right operand.
        r: Box<Expr>,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnOp,
        /// Operand.
        e: Box<Expr>,
    },
    /// A function call: `name(args…)`.
    Func {
        /// Function name.
        name: String,
        /// Argument expressions.
        args: Vec<Expr>,
    },
    /// An aggregate call: `func([DISTINCT] args…)`, or `func(*)` when `args` is
    /// empty. The one shape a plain [`Func`](Expr::Func) cannot spell, added for
    /// formula aggregations over incoming keys (§7.3, Phase 7): `count(*)`,
    /// `count(DISTINCT x)`, `sum(x)`, `string_agg(x, sep)`. `func` is
    /// structural (chosen by code, like a `Func` name), never user data.
    Agg {
        /// Aggregate function name (`count`, `sum`, `avg`, `min`, `max`,
        /// `string_agg`).
        func: String,
        /// Whether the arguments are de-duplicated (`DISTINCT`).
        distinct: bool,
        /// Argument expressions; empty renders as `*` (only valid for `count`).
        args: Vec<Expr>,
    },
    /// A window function call: `func(args…) OVER (PARTITION BY … ORDER BY …)`.
    ///
    /// The narrowest form that gives a *child list* a per-parent `limit` in one
    /// round trip: `row_number() OVER (PARTITION BY key ORDER BY …)` numbers
    /// each parent's children, and the outer query keeps the first *k*.
    /// [`Agg`](Expr::Agg) has no `OVER` and there is no `LATERAL`, so without
    /// this a nested list costs a query per parent.
    ///
    /// `func` is **structural** — chosen by code, exactly as [`Agg::func`] and
    /// [`Func::name`] are — never user data. Both frames are omitted when
    /// empty, so `func() OVER ()` is expressible (the whole result as one
    /// partition).
    ///
    /// [`Agg::func`]: Expr::Agg
    /// [`Func::name`]: Expr::Func
    Window {
        /// Window function name (`row_number`, `rank`, `sum`, …).
        func: String,
        /// Argument expressions; empty for `row_number()`.
        args: Vec<Expr>,
        /// `PARTITION BY` keys; empty for one partition over everything.
        partition: Vec<Expr>,
        /// The window's `ORDER BY`; empty for an unordered frame.
        order: Vec<OrderBy>,
    },
    /// An `IN` test.
    In {
        /// The expression being tested.
        e: Box<Expr>,
        /// The set to test membership against.
        set: InSet,
    },
    /// A JSON access path into `target`.
    Json {
        /// The JSON-typed expression to index into.
        target: Box<Expr>,
        /// The access path.
        path: Vec<JsonStep>,
    },
    /// A `CASE` expression.
    Case {
        /// Optional operand for the simple `CASE operand WHEN v …` form; `None`
        /// for the searched `CASE WHEN cond …` form.
        operand: Option<Box<Expr>>,
        /// The `WHEN … THEN …` arms.
        arms: Vec<CaseArm>,
        /// Optional `ELSE` result.
        else_result: Option<Box<Expr>>,
    },
    /// A scalar subquery: `(SELECT …)` used as a value. How an ownership
    /// formula's Ⱶ-joinfield reaches the target table — a correlated subselect
    /// whose empty result is SQL `NULL`, which is exactly the Ⱶ operator's
    /// optional-chaining contract.
    Subquery(Box<crate::Select>),
    /// `CAST(expr AS type)`. The type name is structural (chosen by code from
    /// a column's declared type, like a `Func` name), never user data; the
    /// renderer still refuses a type name that could not be one.
    Cast {
        /// The expression being cast.
        expr: Box<Expr>,
        /// The SQL type name to cast to (e.g. `uuid`, `timestamp with time
        /// zone`, `numeric(10,2)`).
        type_name: String,
    },
    /// Whether the row matches a full-text search (TODO-agent-skills AS1.1).
    /// What it renders to depends on the backend — see [`crate::search`].
    TextMatch(Box<crate::TextSearch>),
    /// How well the row matches a full-text search, **higher is better** on
    /// every backend (AS1.2): what a read ordered by relevance orders by,
    /// descending.
    TextRank(Box<crate::TextSearch>),
}

impl Expr {
    /// A bare (unqualified) column reference.
    pub fn col(column: impl Into<String>) -> Self {
        Expr::Col(ColRef::bare(column))
    }

    /// A `table.column` reference.
    pub fn qcol(table: impl Into<String>, column: impl Into<String>) -> Self {
        Expr::Col(ColRef::qualified(table, column))
    }

    /// A literal value.
    pub fn lit(v: impl Into<Value>) -> Self {
        Expr::Lit(v.into())
    }

    /// Build a binary expression from two operands.
    pub fn binary(op: BinOp, l: Expr, r: Expr) -> Self {
        Expr::Binary {
            op,
            l: Box::new(l),
            r: Box::new(r),
        }
    }

    /// Build a unary expression.
    pub fn unary(op: UnOp, e: Expr) -> Self {
        Expr::Unary { op, e: Box::new(e) }
    }

    /// `row_number() OVER (PARTITION BY … ORDER BY …)` — the per-parent
    /// numbering a batched child list is paged by.
    pub fn row_number(partition: Vec<Expr>, order: Vec<OrderBy>) -> Self {
        Expr::Window {
            func: "row_number".into(),
            args: Vec::new(),
            partition,
            order,
        }
    }

    /// `self = other`.
    pub fn eq(self, other: Expr) -> Self {
        Expr::binary(BinOp::Eq, self, other)
    }

    /// `self AND other`.
    pub fn and(self, other: Expr) -> Self {
        Expr::binary(BinOp::And, self, other)
    }

    /// `self OR other`.
    pub fn or(self, other: Expr) -> Self {
        Expr::binary(BinOp::Or, self, other)
    }
}

/// A literal value becomes an [`Expr::Lit`].
impl From<Value> for Expr {
    fn from(v: Value) -> Self {
        Expr::Lit(v)
    }
}

/// A column reference becomes an [`Expr::Col`].
impl From<ColRef> for Expr {
    fn from(c: ColRef) -> Self {
        Expr::Col(c)
    }
}
