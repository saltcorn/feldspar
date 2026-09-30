//! Symbolic evaluation: translating a [`Formula`] into a `sc_query::Expr`
//! predicate (TODO Phase 2).
//!
//! One translator serves both consumers. The runtime-check path (Phase 5)
//! translates with [`UserEnv::Inline`], where the current user's values become
//! literals; the RLS path (Phase 6) translates with [`UserEnv::Guc`], where
//! `user.x` becomes a `current_setting('sc.user', true)` JSON extraction. The
//! operation flags never reach SQL: [`translate`] is called *per operation* and
//! the flags are folded to constants first, so `_read || owner === user.id`
//! simply becomes `TRUE` for a select.
//!
//! # The specified semantics
//!
//! The translation *is* the specification of what a formula means; Phase 3's
//! reified evaluator renders normalised JavaScript from the same AST to match
//! it, which is what makes parity between the two provable.
//!
//! - **Equality is JavaScript's, expressed in SQL**: `===`/`==` render as
//!   `IS NOT DISTINCT FROM` (and their negations as `IS DISTINCT FROM`), so
//!   `owner === user.id` with a null `owner` is *false* and its negation *true*
//!   — two-valued, exactly as JS. `x === null` is `x IS NULL`. Loose `==` is
//!   translated as strict: the type-coercion table is not part of the formula
//!   language, and the normalised rendering makes the reified side agree.
//! - **Ordered comparisons keep SQL semantics**: a null operand grants nothing.
//!   JS's `null < 5 === true` coercion is specified away; the normalised
//!   rendering wraps ordered comparisons in null guards to match.
//! - **The logic is two-valued, as JS's is.** `&&`/`||`/`?:` line up with
//!   `AND`/`OR`/`CASE` as they are (a null predicate is falsy on both sides),
//!   but `!` does not: JS `!null` is `true` while SQL `NOT NULL` is `NULL`. So
//!   `!P` translates as `P IS DISTINCT FROM TRUE` — "not provenly true" — which
//!   is exactly JS `!` over a two-valued-with-null predicate. In a *value*
//!   position (`x === (a && b)`) JS returns an operand, not a boolean, so that
//!   is refused as untranslatable rather than silently wrong.
//! - **A bare value as a condition** (`vip && …` where `vip` is a field) needs
//!   the field's type to decide truthiness, which the translator does not have —
//!   untranslatable, with a message suggesting the explicit comparison. The two
//!   knowable cases are translated: bare `user` (object-or-null) and `user.x`
//!   where the env knows `x` is boolean.
//!
//! Anything outside the subset returns [`TranslateError::Untranslatable`]
//! naming the construct — Phase 5 catches that and falls back to the reified
//! evaluator; Phase 6 surfaces it as the reason RLS cannot be enabled.

use std::collections::BTreeMap;
use std::fmt;

use sc_error::Error;
use sc_query::{
    BinOp as QBinOp, Expr as QExpr, InSet, OrderBy, Projection, Select, Source, UnOp as QUnOp,
    Value,
};

use crate::agg::{self, Arrow, Chain, Relation, Selector, Terminal};
use crate::analyze::{Ambient, GLOBALS, JOIN, OpFlag};
use crate::ast::{Ast, BinaryOp, MemberProp, UnaryOp};
use crate::formula::Formula;
use crate::shape::SchemaShape;
use crate::subquery::{AggFunc, AggregateSpec, correlated_aggregate, correlation};

/// The GUC that carries the logged-in user as a JSON object in
/// [`UserEnv::Guc`] mode. Phase 6 issues `SET LOCAL sc.user = '<json>'` per
/// transaction; a missing setting reads as SQL `NULL` via
/// `current_setting(…, true)`, so an unset context **fails closed** by
/// construction.
pub const USER_GUC: &str = "sc.user";

/// The access operation a formula is being translated for. Chooses the value
/// of each operation flag (`_write` is true for everything but [`Read`]).
///
/// [`Read`]: Operation::Read
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// A select.
    Read,
    /// An insert (checked against the proposed row).
    Insert,
    /// An update.
    Update,
    /// A delete.
    Delete,
}

impl Operation {
    /// The constant this operation folds `flag` to.
    fn flag_value(self, flag: OpFlag) -> bool {
        match flag {
            OpFlag::Read => self == Operation::Read,
            OpFlag::Insert => self == Operation::Insert,
            OpFlag::Update => self == Operation::Update,
            OpFlag::Delete => self == Operation::Delete,
            OpFlag::Write => self != Operation::Read,
        }
    }
}

/// How `user` renders into SQL — the one point where the two consumers differ.
#[derive(Debug, Clone, PartialEq)]
pub enum UserEnv {
    /// The current user's values are inlined as literals (always
    /// parameterised on render). `None` is "not logged in": `user` is null,
    /// every `user.x` is null. The map is the user's fields by name —
    /// `sc-auth`'s `User.extra` plus `id`/`role`, projected by the caller.
    Inline(Option<BTreeMap<String, Value>>),
    /// `user.x` renders as a cast JSON extraction from the [`USER_GUC`]
    /// setting, for RLS policies that outlive any one request. `field_types`
    /// maps a user field to the SQL type its extracted text is cast to (a
    /// missing entry or `text` means no cast); it is also what lets a bare
    /// `user.x` condition translate when `x` is boolean.
    Guc {
        /// User field name → SQL type name for the cast.
        field_types: BTreeMap<String, String>,
    },
}

/// Why a formula did not become SQL. The two variants are different verdicts:
/// [`Untranslatable`](TranslateError::Untranslatable) is a property of the
/// formula's *shape* — fall back to the reified evaluator (Phase 5) or refuse
/// to enable RLS naming the construct (Phase 6) — while
/// [`Error`](TranslateError::Error) is a real mistake (an unknown identifier,
/// a broken join path) that validation would also have caught.
#[derive(Debug)]
pub enum TranslateError {
    /// The construct has no SQL counterpart with matching semantics.
    Untranslatable(String),
    /// The formula is wrong regardless of evaluator.
    Error(Error),
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranslateError::Untranslatable(what) => {
                write!(f, "cannot be translated to SQL: {what}")
            }
            TranslateError::Error(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TranslateError {}

impl From<TranslateError> for Error {
    fn from(e: TranslateError) -> Error {
        match e {
            TranslateError::Untranslatable(_) => Error::invalid(e.to_string()),
            TranslateError::Error(err) => err,
        }
    }
}

fn untranslatable<T>(what: impl Into<String>) -> Result<T, TranslateError> {
    Err(TranslateError::Untranslatable(what.into()))
}

/// A table's non-stored calculated fields (Phase 8), by name — what the
/// translator **inlines** wherever an ownership formula (or another calc field)
/// names one. A calc field has no column, so a reference to it must expand to
/// its defining expression; the expansion is transitive and terminates because
/// the catalog orders calc fields acyclically.
pub type CalcFields = BTreeMap<String, Formula>;

/// The empty calc map, for the translation entry points that inline nothing.
static EMPTY_CALC: CalcFields = BTreeMap::new();

/// The ambient objects *other than* `user`, inlined as literals: `row` and `old`
/// (decision 7 — a trigger's formula sees the triggering row ambiently).
///
/// The key's presence is scope, and the `Option` inside is null-or-values —
/// exactly the distinction [`UserEnv::Inline`] draws for an anonymous caller. So
/// `old` present-and-`None` on an insert makes `old.x` null and `old === null`
/// true, while `old` *absent* means the formula was never allowed to name it.
pub type AmbientValues = BTreeMap<Ambient, Option<BTreeMap<String, Value>>>;

/// The empty ambient map, for the ownership/calc translations that have no
/// triggering row.
static EMPTY_AMBIENT: AmbientValues = BTreeMap::new();

/// Everything a translation resolves against beyond the schema: the caller, the
/// ambient objects, and the calc fields to inline.
///
/// One env rather than three trailing parameters, because the three grew
/// together and every caller that needs one of the extras still has to spell the
/// others. `Env::new(&user_env)` is the ownership/RLS case unchanged; a trigger
/// adds `.with_ambient(…)`, a table with calc fields `.with_calc(…)`.
#[derive(Debug, Clone, Copy)]
pub struct Env<'a> {
    user: &'a UserEnv,
    ambient: &'a AmbientValues,
    calc: &'a CalcFields,
}

impl<'a> Env<'a> {
    /// The caller's environment, with no ambient row and nothing to inline.
    pub fn new(user: &'a UserEnv) -> Env<'a> {
        Env {
            user,
            ambient: &EMPTY_AMBIENT,
            calc: &EMPTY_CALC,
        }
    }

    /// Put the triggering event's `row`/`old` in scope, inlined as literals.
    pub fn with_ambient(mut self, ambient: &'a AmbientValues) -> Env<'a> {
        self.ambient = ambient;
        self
    }

    /// Make the table's calculated fields available to inline (Phase 8).
    pub fn with_calc(mut self, calc: &'a CalcFields) -> Env<'a> {
        self.calc = calc;
        self
    }

    /// Whether an ambient object is in scope here: `user` always is, `row`/`old`
    /// exactly when the caller put them in the ambient map (even with a null
    /// value — `old` on an insert is in scope *and* null).
    fn in_scope(&self, ambient: Ambient) -> bool {
        ambient == Ambient::User || self.ambient.contains_key(&ambient)
    }

    /// The inlined values of an ambient object — `user`'s from [`UserEnv`],
    /// `row`/`old`'s from the ambient map.
    ///
    /// `None` is null, and covers three cases the translator treats alike: an
    /// anonymous caller, an object that is null for this event (`old` on an
    /// insert), and an object out of scope entirely. The third cannot reach here
    /// on a validated formula — validation refuses the name — and treating it as
    /// null rather than panicking keeps the translator total.
    fn values(&self, ambient: Ambient) -> Option<&'a BTreeMap<String, Value>> {
        match ambient {
            Ambient::User => match self.user {
                UserEnv::Inline(values) => values.as_ref(),
                // GUC mode has no inlined values; callers route through
                // `Translator::guc_types` before asking.
                UserEnv::Guc { .. } => None,
            },
            other => self.ambient.get(&other)?.as_ref(),
        }
    }
}

impl<'a> From<&'a UserEnv> for Env<'a> {
    fn from(user: &'a UserEnv) -> Env<'a> {
        Env::new(user)
    }
}

/// Translate `formula` into a boolean `sc_query::Expr` over `table`, for one
/// operation, in one environment. The result is what Phase 5 ANDs into a
/// statement's WHERE and Phase 6 renders into an RLS policy.
///
/// The caller is expected to have run [`Formula::validate`]; an invalid formula
/// still fails here (as [`TranslateError::Error`]), just with less polish.
pub fn translate(
    formula: &Formula,
    op: Operation,
    env: &Env<'_>,
    shape: &SchemaShape,
    table: &str,
) -> Result<QExpr, TranslateError> {
    translate_rooted(formula, op, env, shape, table, table)
}

/// [`translate`] with the formula's own row read through `root` — an **alias**
/// — rather than through the table's name.
///
/// A predicate evaluated *inside* a subquery that aliased the table cannot name
/// the table: `FROM "employees" "_fd_a1"` hides `employees`, so a translated
/// ownership formula that says `"employees"."owner"` is an error rather than a
/// filter. The GraphQL provider needs exactly this — a child table's ownership
/// predicate ANDed into the `WHERE` of a correlated aggregate over it — and it
/// is the same rooting [`join_path_expr_rooted`] already exposes for a join
/// path, applied to a whole formula.
///
/// `table` is still the shape's key: an alias is not a table, and the fields
/// are looked up by the real name.
pub fn translate_rooted(
    formula: &Formula,
    op: Operation,
    env: &Env<'_>,
    shape: &SchemaShape,
    table: &str,
    root: &str,
) -> Result<QExpr, TranslateError> {
    let mut tr = translator(env, shape, table)?;
    tr.root = root;
    let folded = fold(formula.ast(), op, &mut Vec::new());
    tr.predicate(&folded)
}

/// Translate a calc field's `formula` in **value** position to an
/// `sc_query::Expr` — the column expression the read path projects into a
/// `SELECT` (Phase 8), and the value an action's configuration formula computes
/// (Phase 3). Value position carries no operation flags, so no folding and no
/// operation is needed.
pub fn translate_value(
    formula: &Formula,
    env: &Env<'_>,
    shape: &SchemaShape,
    table: &str,
) -> Result<QExpr, TranslateError> {
    let mut tr = translator(env, shape, table)?;
    tr.value(formula.ast())
}

/// [`translate_value`] with the formula's own row read through `root` — an
/// alias — rather than through the table's name: the value-position twin of
/// [`translate_rooted`].
///
/// A dataset stage (analytics TODO A1.3) is a subquery, so its formulas read
/// `"_fd_s2"."price"` rather than `"houses"."price"`; `table` is still the
/// shape's key, because an alias is not a table.
pub fn translate_value_rooted(
    formula: &Formula,
    env: &Env<'_>,
    shape: &SchemaShape,
    table: &str,
    root: &str,
) -> Result<QExpr, TranslateError> {
    let mut tr = translator(env, shape, table)?;
    tr.root = root;
    tr.value(formula.ast())
}

/// The translator both entry points build, with the one check they share.
fn translator<'a>(
    env: &'a Env<'a>,
    shape: &'a SchemaShape,
    table: &'a str,
) -> Result<Translator<'a>, TranslateError> {
    if !shape.tables.contains_key(table) {
        return Err(TranslateError::Error(Error::invalid(format!(
            "formula on `{table}`: unknown table `{table}`"
        ))));
    }
    Ok(Translator {
        env,
        shape,
        table,
        root: table,
        aliases: 0,
        child_scopes: Vec::new(),
    })
}

/// The correlated-subselect `sc_query::Expr` for one Ⱶ-join identifier on
/// `table` — the same expression [`translate`] builds inside a predicate,
/// exposed on its own so the runtime-check path (Phase 5) can *project* join
/// values: `SELECT *, (…) AS "publisherⱵname" FROM books` fetches the rows and
/// the values the reified evaluator needs in one query, instead of a round
/// trip per row per path.
pub fn join_path_expr(
    shape: &SchemaShape,
    table: &str,
    ident: &str,
) -> Result<QExpr, TranslateError> {
    join_path_expr_rooted(shape, table, table, ident)
}

/// [`join_path_expr`] correlated from an arbitrary **alias** rather than from
/// the table's own name.
///
/// A join path is rooted somewhere: `publisherⱵname` on `books` reads
/// `books.publisher`, but the same path evaluated for a row of a *subquery* —
/// a child list's alias, an aggregate's alias — has to read
/// `<alias>.publisher` instead. The translator has always needed both (an
/// aggregation arrow's `r.publisherⱵname` is the aliased case); this is that
/// same rooting, exposed, because a GraphQL filter over a child table is
/// written against the subquery's alias too.
///
/// `root_table` is the table `root_alias` names — the schema shape is consulted
/// by table, and an alias is not one.
///
/// Aliases inside the returned expression are numbered from `_fd_j1` per call,
/// so a caller composing two of these into *nested* positions must keep them
/// apart itself; siblings in one statement are separate scopes and are fine.
pub fn join_path_expr_rooted(
    shape: &SchemaShape,
    root_alias: &str,
    root_table: &str,
    ident: &str,
) -> Result<QExpr, TranslateError> {
    // The env is irrelevant to a join path (no ambient object inside it);
    // anonymous-and-empty is the cheapest to construct.
    let user = UserEnv::Inline(None);
    let mut tr = Translator {
        env: &Env::new(&user),
        shape,
        table: root_table,
        root: root_alias,
        aliases: 0,
        child_scopes: Vec::new(),
    };
    tr.join_value_rooted(root_alias, root_table, ident)
}

/// Fold the operation flags to constants and simplify what the constants
/// decide. Only **value-exact** simplifications are applied — ones where the
/// JS value, not just its truthiness, is preserved — because folding runs
/// before we know whether a subtree sits in value or predicate position:
/// `false && x` → `false`, `true && x` → `x`, `true || x` → `true`,
/// `false || x` → `x`, `!literal`, and a conditional with a literal test.
/// (`x && true` is *not* folded: JS returns `x` when `x` is falsy.)
///
/// `locals` tracks arrow parameters, which shadow the flag names.
fn fold(ast: &Ast, op: Operation, locals: &mut Vec<String>) -> Ast {
    match ast {
        Ast::Ident(name) if !locals.iter().any(|l| l == name) => match OpFlag::from_ident(name) {
            Some(flag) => Ast::Bool(op.flag_value(flag)),
            None => ast.clone(),
        },
        Ast::Ident(_) | Ast::Str(_) | Ast::Num(_) | Ast::Bool(_) | Ast::Null => ast.clone(),
        Ast::Member {
            obj,
            prop,
            optional,
        } => Ast::Member {
            obj: Box::new(fold(obj, op, locals)),
            prop: match prop {
                MemberProp::Static(s) => MemberProp::Static(s.clone()),
                MemberProp::Computed(e) => MemberProp::Computed(Box::new(fold(e, op, locals))),
            },
            optional: *optional,
        },
        Ast::Call {
            callee,
            args,
            optional,
        } => Ast::Call {
            callee: Box::new(fold(callee, op, locals)),
            args: args.iter().map(|a| fold(a, op, locals)).collect(),
            optional: *optional,
        },
        Ast::Unary { op: uop, expr } => {
            let expr = fold(expr, op, locals);
            match (uop, &expr) {
                (UnaryOp::Not, Ast::Bool(b)) => Ast::Bool(!b),
                _ => Ast::Unary {
                    op: *uop,
                    expr: Box::new(expr),
                },
            }
        }
        Ast::Binary { op: bop, l, r } => {
            let l = fold(l, op, locals);
            let r = fold(r, op, locals);
            match (bop, &l) {
                (BinaryOp::And, Ast::Bool(false)) => Ast::Bool(false),
                (BinaryOp::And, Ast::Bool(true)) => r,
                (BinaryOp::Or, Ast::Bool(true)) => Ast::Bool(true),
                (BinaryOp::Or, Ast::Bool(false)) => r,
                _ => Ast::Binary {
                    op: *bop,
                    l: Box::new(l),
                    r: Box::new(r),
                },
            }
        }
        Ast::Cond { test, cons, alt } => {
            let test = fold(test, op, locals);
            match test {
                Ast::Bool(true) => fold(cons, op, locals),
                Ast::Bool(false) => fold(alt, op, locals),
                _ => Ast::Cond {
                    test: Box::new(test),
                    cons: Box::new(fold(cons, op, locals)),
                    alt: Box::new(fold(alt, op, locals)),
                },
            }
        }
        Ast::Array(elems) => Ast::Array(elems.iter().map(|e| fold(e, op, locals)).collect()),
        Ast::Template { quasis, exprs } => Ast::Template {
            quasis: quasis.clone(),
            exprs: exprs.iter().map(|e| fold(e, op, locals)).collect(),
        },
        Ast::Arrow { params, body } => {
            let depth = locals.len();
            locals.extend(params.iter().cloned());
            let body = fold(body, op, locals);
            locals.truncate(depth);
            Ast::Arrow {
                params: params.clone(),
                body: Box::new(body),
            }
        }
    }
}

struct Translator<'a> {
    env: &'a Env<'a>,
    shape: &'a SchemaShape,
    table: &'a str,
    /// How the formula's own row is **named** in SQL: the table itself, or the
    /// alias it was given by the query the predicate is being embedded in (see
    /// [`translate_rooted`]). Every reference to a column of this row — a bare
    /// field, the root of a Ⱶ-join path, the parent side of a Ↄ-aggregation's
    /// correlation — goes through this rather than through
    /// [`table`](Self::table), which stays the shape's key.
    root: &'a str,
    /// Counter for join-subquery aliases. Prefixed `_fd_` because user tables
    /// cannot start with it (§9 reserves the prefix), so an alias can never
    /// shadow a real table a correlated column reference points at.
    aliases: usize,
    /// Active child-row scopes, innermost last: an aggregation arrow's parameter
    /// bound to the aliased child table it ranges over (Phase 7). While one is
    /// on the stack, `param.field` resolves to a child column instead of failing.
    child_scopes: Vec<ChildScope>,
}

/// One active aggregation arrow scope: its parameter name, the child-table alias
/// it stands for, and that table's name (for resolving `param.fkⱵx` join paths).
struct ChildScope {
    param: String,
    alias: String,
    table: String,
}

impl<'a> Translator<'a> {
    // ---- predicates --------------------------------------------------------

    /// Translate `ast` in boolean (predicate) position.
    fn predicate(&mut self, ast: &Ast) -> Result<QExpr, TranslateError> {
        if let Some(chain) = agg::parse_chain(ast) {
            let chain = chain.map_err(TranslateError::Error)?;
            return if chain.terminal.is_boolean() {
                self.aggregation(&chain)
            } else {
                untranslatable("a bare aggregate value as a condition (compare it, e.g. `… > 0`)")
            };
        }
        match ast {
            Ast::Bool(b) => Ok(QExpr::lit(*b)),
            Ast::Unary {
                op: UnaryOp::Not,
                expr,
            } => Ok(not_pred(self.predicate(expr)?)),
            Ast::Binary { op, l, r } => match op {
                BinaryOp::And => Ok(self.predicate(l)?.and(self.predicate(r)?)),
                BinaryOp::Or => Ok(self.predicate(l)?.or(self.predicate(r)?)),
                BinaryOp::Eq | BinaryOp::StrictEq => self.equality(l, r, false),
                BinaryOp::NotEq | BinaryOp::StrictNotEq => self.equality(l, r, true),
                BinaryOp::Lt => self.comparison(QBinOp::Lt, l, r),
                BinaryOp::LtEq => self.comparison(QBinOp::Le, l, r),
                BinaryOp::Gt => self.comparison(QBinOp::Gt, l, r),
                BinaryOp::GtEq => self.comparison(QBinOp::Ge, l, r),
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    untranslatable("arithmetic as a condition")
                }
                BinaryOp::Nullish => untranslatable("`??` as a condition"),
            },
            Ast::Cond { test, cons, alt } => Ok(QExpr::Case {
                operand: None,
                arms: vec![sc_query::CaseArm {
                    when: self.predicate(test)?,
                    then: self.predicate(cons)?,
                }],
                else_result: Some(Box::new(self.predicate(alt)?)),
            }),
            Ast::Ident(name) if self.ambient_named(name).is_some() => {
                Ok(self.ambient_truthy(self.ambient_named(name).unwrap_or(Ambient::User)))
            }
            // A bare calc field as a condition inlines to its expression in
            // predicate position (Phase 8) — a boolean calc field is usable
            // where any bare boolean field is not, because its definition is
            // known.
            Ast::Ident(name) if self.env.calc.contains_key(name) => {
                let ast = self.env.calc[name].ast();
                self.predicate(ast)
            }
            Ast::Member { obj, prop, .. } if self.ambient_of(obj).is_some() => {
                let amb = self.ambient_of(obj).unwrap_or(Ambient::User);
                match prop {
                    MemberProp::Static(field) => self.ambient_field_truthy(amb, field),
                    MemberProp::Computed(_) => {
                        untranslatable(format!("computed access on `{amb}`"))
                    }
                }
            }
            Ast::Ident(_) => untranslatable(
                "a bare value as a condition (write an explicit comparison, \
                 e.g. `x === true` or `x !== null`)",
            ),
            other => untranslatable(format!("{} as a condition", describe(other))),
        }
    }

    /// `a === b` / `a !== b` (and their loose spellings) — JavaScript's
    /// two-valued equality, which in SQL is `IS [NOT] DISTINCT FROM`, with the
    /// null-literal and `user` cases given their direct forms.
    fn equality(&mut self, l: &Ast, r: &Ast, negated: bool) -> Result<QExpr, TranslateError> {
        let (null_op, distinct_op) = if negated {
            (QUnOp::IsNotNull, QBinOp::IsDistinct)
        } else {
            (QUnOp::IsNull, QBinOp::IsNotDistinct)
        };
        match (l, r) {
            // `null === null` is true; folded rather than sent to SQL.
            (Ast::Null, Ast::Null) => Ok(QExpr::lit(!negated)),
            (Ast::Null, other) | (other, Ast::Null) => {
                if let Some(amb) = self.ambient_of(other) {
                    // `user === null`: "not logged in" — and `old === null`:
                    // "this is not an update", in each env's terms.
                    let is_null = self.ambient_is_null(amb);
                    return Ok(if negated {
                        QExpr::unary(QUnOp::Not, is_null)
                    } else {
                        is_null
                    });
                }
                Ok(QExpr::unary(null_op, self.value(other)?))
            }
            _ if self.ambient_of(l).or(self.ambient_of(r)).is_some() => {
                let amb = self
                    .ambient_of(l)
                    .or(self.ambient_of(r))
                    .unwrap_or(Ambient::User);
                untranslatable(format!(
                    "comparing the `{amb}` object itself (compare one of its fields)"
                ))
            }
            _ => Ok(QExpr::binary(distinct_op, self.value(l)?, self.value(r)?)),
        }
    }

    /// Ordered comparisons translate operand-for-operand and keep SQL's null
    /// semantics: a null operand yields SQL `NULL`, which grants nothing.
    fn comparison(&mut self, op: QBinOp, l: &Ast, r: &Ast) -> Result<QExpr, TranslateError> {
        Ok(QExpr::binary(op, self.value(l)?, self.value(r)?))
    }

    // ---- values ------------------------------------------------------------

    /// Translate `ast` in value position.
    fn value(&mut self, ast: &Ast) -> Result<QExpr, TranslateError> {
        if let Some(chain) = agg::parse_chain(ast) {
            let chain = chain.map_err(TranslateError::Error)?;
            return self.aggregation(&chain);
        }
        match ast {
            Ast::Str(s) => Ok(QExpr::lit(s.as_str())),
            Ast::Num(n) => Ok(QExpr::Lit(num_value(*n))),
            Ast::Bool(b) => Ok(QExpr::lit(*b)),
            Ast::Null => Ok(QExpr::Lit(Value::Null)),
            Ast::Ident(name) => self.ident_value(name),
            Ast::Member { obj, prop, .. } if self.ambient_of(obj).is_some() => {
                let amb = self.ambient_of(obj).unwrap_or(Ambient::User);
                match prop {
                    MemberProp::Static(field) => Ok(self.ambient_field(amb, field)),
                    MemberProp::Computed(_) => {
                        untranslatable(format!("computed access on `{amb}`"))
                    }
                }
            }
            Ast::Member { obj, prop, .. } => match self.child_alias_of(obj) {
                Some((alias, table)) => match prop {
                    MemberProp::Static(field) if field.contains(JOIN) => {
                        self.join_value_rooted(&alias, &table, field)
                    }
                    MemberProp::Static(field) => Ok(QExpr::qcol(alias, field.clone())),
                    MemberProp::Computed(_) => untranslatable("computed access on a child row"),
                },
                None => untranslatable("property access on something other than an ambient object"),
            },
            Ast::Unary { op, expr } => match op {
                UnaryOp::Neg => Ok(QExpr::unary(QUnOp::Neg, self.value(expr)?)),
                UnaryOp::Not => Ok(not_pred(self.predicate(expr)?)),
                UnaryOp::Pos => untranslatable("unary `+` (numeric coercion)"),
                UnaryOp::TypeOf => untranslatable("`typeof`"),
            },
            Ast::Binary { op, l, r } => match op {
                BinaryOp::Add => self.arithmetic(QBinOp::Add, l, r),
                BinaryOp::Sub => self.arithmetic(QBinOp::Sub, l, r),
                BinaryOp::Mul => self.arithmetic(QBinOp::Mul, l, r),
                BinaryOp::Div => self.arithmetic(QBinOp::Div, l, r),
                BinaryOp::Mod => self.arithmetic(QBinOp::Mod, l, r),
                // JS `??` on SQL values is COALESCE exactly (there is no
                // `undefined` on this side of the boundary).
                BinaryOp::Nullish => Ok(QExpr::Func {
                    name: "COALESCE".into(),
                    args: vec![self.value(l)?, self.value(r)?],
                }),
                // A comparison used as a value is a boolean value in both
                // worlds; `&&`/`||` are not (JS returns an operand).
                BinaryOp::Eq
                | BinaryOp::StrictEq
                | BinaryOp::NotEq
                | BinaryOp::StrictNotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq => self.predicate(ast),
                BinaryOp::And | BinaryOp::Or => untranslatable(
                    "`&&`/`||` as a value (JavaScript yields an operand, SQL a boolean)",
                ),
            },
            Ast::Cond { test, cons, alt } => Ok(QExpr::Case {
                operand: None,
                arms: vec![sc_query::CaseArm {
                    when: self.predicate(test)?,
                    then: self.value(cons)?,
                }],
                else_result: Some(Box::new(self.value(alt)?)),
            }),
            other => untranslatable(describe(other)),
        }
    }

    /// An identifier in value position: a field, a Ⱶ-join path, or a refusal
    /// that says which kind.
    fn ident_value(&mut self, name: &str) -> Result<QExpr, TranslateError> {
        if name == "user" {
            return untranslatable(
                "the `user` object itself has no SQL value (compare one of its fields)",
            );
        }
        // A calc field has no column: inline its defining expression in value
        // position (Phase 8), transitively. Checked before the plain-field case
        // because a calc field *is* present in the shape as a field.
        if let Some(formula) = self.env.calc.get(name) {
            let ast = formula.ast();
            return self.value(ast);
        }
        if let Some(table_shape) = self.shape.tables.get(self.table)
            && table_shape.fields.contains_key(name)
        {
            return Ok(QExpr::qcol(self.root, name));
        }
        if name.contains(JOIN) {
            return self.join_value(name);
        }
        if GLOBALS.contains(&name) {
            return untranslatable(format!("the JavaScript global `{name}`"));
        }
        Err(TranslateError::Error(Error::invalid(format!(
            "formula on `{}`: unknown identifier `{name}`",
            self.table
        ))))
    }

    fn arithmetic(&mut self, op: QBinOp, l: &Ast, r: &Ast) -> Result<QExpr, TranslateError> {
        Ok(QExpr::binary(op, self.value(l)?, self.value(r)?))
    }

    // ---- Ⱶ-join paths ------------------------------------------------------

    /// A Ⱶ-identifier as a value: nested correlated scalar subselects, one per
    /// link. `publisherⱵname` on `books` becomes
    /// `(SELECT _fd_j1.name FROM publishers AS _fd_j1
    ///    WHERE _fd_j1.id = books.publisher)`;
    /// a null foreign key selects no row, the subquery yields SQL `NULL`, and
    /// nothing is granted — which *is* the Ⱶ optional-chaining contract, for
    /// free.
    fn join_value(&mut self, ident: &str) -> Result<QExpr, TranslateError> {
        let (root_alias, root_table) = (self.root.to_string(), self.table.to_string());
        self.join_value_rooted(&root_alias, &root_table, ident)
    }

    /// A Ⱶ-identifier as a value, correlated from an arbitrary root — the
    /// formula's own table for a top-level join path, or a child-table alias
    /// when the path sits inside an aggregation arrow (`r.publisherⱵname`).
    fn join_value_rooted(
        &mut self,
        root_alias: &str,
        root_table: &str,
        ident: &str,
    ) -> Result<QExpr, TranslateError> {
        let segments: Vec<&str> = ident.split(JOIN).collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(self.path_error(ident, "empty segment around Ⱶ"));
        }
        let mut current = root_table.to_string();
        let mut expr: Option<QExpr> = None;
        for (i, segment) in segments.iter().enumerate() {
            let last = i == segments.len() - 1;
            let table_shape = self.shape.tables.get(&current).ok_or_else(|| {
                self.path_error(
                    ident,
                    &format!("table `{current}` is not in the schema shape"),
                )
            })?;
            let field = table_shape.fields.get(*segment).ok_or_else(|| {
                self.path_error(
                    ident,
                    &format!("`{segment}` is not a field of table `{current}`"),
                )
            })?;
            if last {
                break;
            }
            let Some(key) = &field.key else {
                return Err(self.path_error(
                    ident,
                    &format!("`{segment}` is not a Key field on `{current}`"),
                ));
            };
            // The correlation value: the root's own FK column for the first
            // link, the previous subquery for every later one.
            let corr = match expr.take() {
                None => QExpr::qcol(root_alias.to_string(), *segment),
                Some(prev) => prev,
            };
            self.aliases += 1;
            let alias = format!("_fd_j{}", self.aliases);
            let next = segments[i + 1];
            let sub = Select::from(Source::table_as(key.target_table.clone(), alias.clone()))
                .columns(vec![Projection::expr(QExpr::qcol(alias.clone(), next))])
                .filter(QExpr::binary(
                    QBinOp::Eq,
                    QExpr::qcol(alias, key.target_field.clone()),
                    corr,
                ));
            expr = Some(QExpr::Subquery(Box::new(sub)));
            current = key.target_table.clone();
        }
        expr.ok_or_else(|| {
            // A Ⱶ-identifier with a single segment cannot exist (`split` on a
            // contained char yields ≥ 2 parts), so this is unreachable — but
            // an error beats a panic if that invariant ever shifts.
            self.path_error(ident, "join path has no links")
        })
    }

    fn path_error(&self, ident: &str, msg: &str) -> TranslateError {
        TranslateError::Error(Error::invalid(format!(
            "formula on `{}`: `{ident}`: {msg}",
            self.table
        )))
    }

    // ---- Ↄ-aggregations ----------------------------------------------------

    /// The child-table alias and name a member's object stands for, if it is an
    /// aggregation arrow's parameter (innermost scope wins).
    fn child_alias_of(&self, obj: &Ast) -> Option<(String, String)> {
        let Ast::Ident(name) = obj else { return None };
        self.child_scopes
            .iter()
            .rev()
            .find(|s| s.param == *name)
            .map(|s| (s.alias.clone(), s.table.clone()))
    }

    /// Translate an aggregation chain into one correlated subquery over the
    /// child table (§7.3, Phase 7). The terminal picks the SQL form; `filter`
    /// predicates fold into the subquery's `WHERE`, and the value expression
    /// comes from the terminal's selector or a preceding `map`.
    ///
    /// The subquery itself is [`subquery::correlated_aggregate`]'s to build —
    /// the same function the GraphQL provider calls for the same question, so
    /// the two paths cannot drift apart on the empty-relation semantics.
    fn aggregation(&mut self, chain: &Chain) -> Result<QExpr, TranslateError> {
        let rel = chain
            .resolve(self.shape, self.table)
            .map_err(TranslateError::Error)?;
        self.aliases += 1;
        let alias = format!("_fd_a{}", self.aliases);
        // What the chain's `filter` steps constrain the child rows by; the
        // correlation back to the parent is the builder's own business.
        let mut extra: Option<QExpr> = None;
        for f in &chain.filters {
            let p = self.child_predicate(&alias, &rel.child_table, f)?;
            extra = and_opt(extra, p);
        }
        match &chain.terminal {
            Terminal::Length => {
                let value = if chain.distinct {
                    Some(self.require_value(chain, &rel, &alias)?)
                } else {
                    None
                };
                self.agg(&rel, &alias, AggFunc::Count, chain.distinct, value, extra)
            }
            Terminal::Sum(_) => {
                let v = self.require_value(chain, &rel, &alias)?;
                self.agg(&rel, &alias, AggFunc::Sum, chain.distinct, Some(v), extra)
            }
            Terminal::Avg(_) => self.bare_agg(AggFunc::Avg, chain, &rel, &alias, extra),
            Terminal::Min(_) => self.bare_agg(AggFunc::Min, chain, &rel, &alias, extra),
            Terminal::Max(_) => self.bare_agg(AggFunc::Max, chain, &rel, &alias, extra),
            Terminal::Some(pred) => {
                let p = self.child_predicate(&alias, &rel.child_table, pred)?;
                let count =
                    self.agg(&rel, &alias, AggFunc::Count, false, None, and_opt(extra, p))?;
                Ok(QExpr::binary(QBinOp::Gt, count, QExpr::lit(0_i64)))
            }
            Terminal::Every(pred) => {
                // No child fails the predicate: count of rows where the
                // predicate is not provenly true is zero.
                let p = self.child_predicate(&alias, &rel.child_table, pred)?;
                let count = self.agg(
                    &rel,
                    &alias,
                    AggFunc::Count,
                    false,
                    None,
                    and_opt(extra, not_pred(p)),
                )?;
                Ok(QExpr::binary(QBinOp::Eq, count, QExpr::lit(0_i64)))
            }
            Terminal::Includes(x) => {
                // Not an aggregate — a membership test over the child values —
                // but correlated by the same predicate the builder uses.
                let value = self.mapped_value(chain, &rel, &alias)?;
                let needle = self.value(x)?;
                let sub = Select::from(Source::table_as(rel.child_table.clone(), alias.clone()))
                    .columns(vec![Projection::expr(value)])
                    .filter(self.correlated(&rel, &alias, extra));
                Ok(QExpr::In {
                    e: Box::new(needle),
                    set: InSet::Subquery(Box::new(sub)),
                })
            }
            Terminal::Join(sep) => {
                let value = self.mapped_value(chain, &rel, &alias)?;
                let separator = self.value(sep)?;
                self.agg(
                    &rel,
                    &alias,
                    AggFunc::StringAgg { separator },
                    chain.distinct,
                    Some(value),
                    extra,
                )
            }
            Terminal::MaxBy(sel) | Terminal::MinBy(sel) => {
                self.ordered_selection(chain, &rel, &alias, sel, extra)
            }
        }
    }

    /// Build one correlated aggregate over `rel` through the shared builder,
    /// correlated to the table this formula is written on.
    fn agg(
        &self,
        rel: &Relation,
        alias: &str,
        func: AggFunc,
        distinct: bool,
        value: Option<QExpr>,
        filter: Option<QExpr>,
    ) -> Result<QExpr, TranslateError> {
        correlated_aggregate(AggregateSpec {
            child_table: rel.child_table.clone(),
            key_field: rel.key_field.clone(),
            parent: self.root.to_string(),
            parent_field: rel.parent_field.clone(),
            alias: alias.to_string(),
            func,
            distinct,
            value,
            filter,
        })
        .map_err(TranslateError::Error)
    }

    /// The `WHERE` for a child subquery the builder does not build: the
    /// correlation, plus whatever else constrains the child rows.
    fn correlated(&self, rel: &Relation, alias: &str, filter: Option<QExpr>) -> QExpr {
        let corr = correlation(alias, &rel.key_field, self.root, &rel.parent_field);
        match filter {
            Some(p) => corr.and(p),
            None => corr,
        }
    }

    /// `avg`/`min`/`max`: a bare aggregate (empty → SQL NULL, matching the
    /// semantics table).
    fn bare_agg(
        &mut self,
        func: AggFunc,
        chain: &Chain,
        rel: &Relation,
        alias: &str,
        filter: Option<QExpr>,
    ) -> Result<QExpr, TranslateError> {
        let v = self.require_value(chain, rel, alias)?;
        self.agg(rel, alias, func, chain.distinct, Some(v), filter)
    }

    /// The ordered-selection subquery for `maxBy`/`minBy`: the accessed member,
    /// from the row with the greatest/least selector value, ties broken by the
    /// child primary key so both evaluators pick the same row.
    fn ordered_selection(
        &mut self,
        chain: &Chain,
        rel: &Relation,
        alias: &str,
        sel: &Selector,
        filter: Option<QExpr>,
    ) -> Result<QExpr, TranslateError> {
        let Some(pk) = &rel.child_pk else {
            return untranslatable(
                "`maxBy`/`minBy` need the child table's primary key for a deterministic tie-break",
            );
        };
        let Some(member) = chain.member else {
            return untranslatable("`maxBy`/`minBy` must have a field read from the row");
        };
        let key = self.selector_value(alias, &rel.child_table, sel)?;
        let proj = if member.contains(JOIN) {
            self.join_value_rooted(alias, &rel.child_table, member)?
        } else {
            QExpr::qcol(alias.to_string(), member.to_string())
        };
        // Ignore rows with a null key, as every other aggregate does.
        let filter = self
            .correlated(rel, alias, filter)
            .and(QExpr::unary(QUnOp::IsNotNull, key.clone()));
        let desc = matches!(chain.terminal, Terminal::MaxBy(_));
        let ob = |e: QExpr| {
            if desc {
                OrderBy::desc(e)
            } else {
                OrderBy::asc(e)
            }
        };
        let mut sub = Select::from(Source::table_as(rel.child_table.clone(), alias.to_string()))
            .columns(vec![Projection::expr(proj)])
            .filter(filter)
            .limit(1);
        sub.order = vec![ob(key), ob(QExpr::qcol(alias.to_string(), pk.clone()))];
        Ok(QExpr::Subquery(Box::new(sub)))
    }

    /// The value a value-aggregate (`sum`/`avg`/`min`/`max`, or `count(DISTINCT
    /// …)`) aggregates: the terminal's selector, else a preceding `map`, else
    /// the distinct selector.
    fn require_value(
        &mut self,
        chain: &Chain,
        rel: &Relation,
        alias: &str,
    ) -> Result<QExpr, TranslateError> {
        if let Some(sel) = terminal_selector(&chain.terminal) {
            return self.selector_value(alias, &rel.child_table, sel);
        }
        if let Some(map) = &chain.map {
            return self.child_value(alias, &rel.child_table, map);
        }
        if let Some(sel) = &chain.distinct_selector {
            return self.selector_value(alias, &rel.child_table, sel);
        }
        untranslatable("this aggregate needs a selector or a preceding `map`")
    }

    /// The mapped value for `includes`/`join` (a preceding `map` is required by
    /// validation).
    fn mapped_value(
        &mut self,
        chain: &Chain,
        rel: &Relation,
        alias: &str,
    ) -> Result<QExpr, TranslateError> {
        match &chain.map {
            Some(map) => self.child_value(alias, &rel.child_table, map),
            None => untranslatable("`includes`/`join` need a preceding `map`"),
        }
    }

    /// A selector as a child value: a field name is a child column; an arrow is
    /// its body translated in the child scope.
    fn selector_value(
        &mut self,
        alias: &str,
        child_table: &str,
        sel: &Selector,
    ) -> Result<QExpr, TranslateError> {
        match sel {
            Selector::Field(f) => Ok(QExpr::qcol(alias.to_string(), (*f).to_string())),
            Selector::Arrow(a) => self.child_value(alias, child_table, a),
        }
    }

    /// Translate an arrow body in child scope, in value position.
    fn child_value(
        &mut self,
        alias: &str,
        child_table: &str,
        arrow: &Arrow,
    ) -> Result<QExpr, TranslateError> {
        self.push_child(alias, child_table, arrow.param);
        let r = self.value(arrow.body);
        self.child_scopes.pop();
        r
    }

    /// Translate an arrow body in child scope, in predicate position.
    fn child_predicate(
        &mut self,
        alias: &str,
        child_table: &str,
        arrow: &Arrow,
    ) -> Result<QExpr, TranslateError> {
        self.push_child(alias, child_table, arrow.param);
        let r = self.predicate(arrow.body);
        self.child_scopes.pop();
        r
    }

    fn push_child(&mut self, alias: &str, table: &str, param: &str) {
        self.child_scopes.push(ChildScope {
            param: param.to_string(),
            alias: alias.to_string(),
            table: table.to_string(),
        });
    }

    // ---- the ambient objects ----------------------------------------------
    //
    // `user`, `row` and `old` share one set of semantics (object-or-null, members
    // inlined as literals), so they share one set of methods. Only `user` has a
    // second rendering — the GUC read an RLS policy needs, because a policy
    // outlives the request that set it; `row`/`old` are inline-only, since a
    // policy has no triggering event.

    /// The in-scope ambient object an *expression* names, if it names one.
    ///
    /// Scope is what makes this safe on a table that happens to have a field
    /// called `row`: outside a trigger's formula no such object is in scope, so
    /// the identifier stays the field it always was.
    fn ambient_of(&self, ast: &Ast) -> Option<Ambient> {
        match ast {
            Ast::Ident(name) => self.ambient_named(name),
            _ => None,
        }
    }

    /// [`ambient_of`](Translator::ambient_of) for a bare identifier.
    fn ambient_named(&self, name: &str) -> Option<Ambient> {
        Ambient::from_ident(name).filter(|amb| self.env.in_scope(*amb))
    }

    /// The user field→type map when this object renders as a **GUC read** rather
    /// than as literals: `user` under [`UserEnv::Guc`], and nothing else.
    fn guc_types(&self, ambient: Ambient) -> Option<&'a BTreeMap<String, String>> {
        match (ambient, self.env.user) {
            (Ambient::User, UserEnv::Guc { field_types }) => Some(field_types),
            _ => None,
        }
    }

    /// Bare `user`/`row`/`old` as a condition: object-or-null, so truthy ⇔
    /// present (for `user`, ⇔ logged in).
    fn ambient_truthy(&self, ambient: Ambient) -> QExpr {
        match self.guc_types(ambient) {
            Some(_) => QExpr::unary(QUnOp::IsNotNull, guc_raw()),
            None => QExpr::lit(self.env.values(ambient).is_some()),
        }
    }

    /// `user === null` (and the same for `row`/`old`).
    fn ambient_is_null(&self, ambient: Ambient) -> QExpr {
        match self.guc_types(ambient) {
            Some(_) => QExpr::unary(QUnOp::IsNull, guc_raw()),
            None => QExpr::lit(self.env.values(ambient).is_none()),
        }
    }

    /// `user.x` / `row.x` / `old.x` as a value: a literal from the inlined
    /// values (null when the object is null or has no such field), or the cast
    /// JSON extraction from the GUC for an RLS policy's `user`.
    fn ambient_field(&self, ambient: Ambient, field: &str) -> QExpr {
        let Some(field_types) = self.guc_types(ambient) else {
            let value = self
                .env
                .values(ambient)
                .and_then(|vals| vals.get(field).cloned())
                .unwrap_or(Value::Null);
            return QExpr::Lit(value);
        };
        let text = QExpr::Func {
            name: "jsonb_extract_path_text".into(),
            args: vec![
                QExpr::Cast {
                    expr: Box::new(guc_raw()),
                    type_name: "jsonb".into(),
                },
                QExpr::lit(field),
            ],
        };
        match field_types.get(field).map(String::as_str) {
            None | Some("text") => text,
            Some(type_name) => QExpr::Cast {
                expr: Box::new(text),
                type_name: type_name.into(),
            },
        }
    }

    /// Bare `user.x` as a condition. Translatable exactly when the truthiness
    /// is knowable: inline, the actual value decides; in GUC mode only a field
    /// the env declares boolean (null-safe `IS NOT DISTINCT FROM TRUE`, so a
    /// missing user or field is false, as JS truthiness of `undefined` is).
    fn ambient_field_truthy(&self, ambient: Ambient, field: &str) -> Result<QExpr, TranslateError> {
        let Some(field_types) = self.guc_types(ambient) else {
            let truthy = self
                .env
                .values(ambient)
                .and_then(|vals| vals.get(field))
                .is_some_and(js_truthy);
            return Ok(QExpr::lit(truthy));
        };
        match field_types.get(field).map(String::as_str) {
            Some("boolean") | Some("bool") => Ok(QExpr::binary(
                QBinOp::IsNotDistinct,
                self.ambient_field(ambient, field),
                QExpr::lit(true),
            )),
            _ => untranslatable(format!(
                "`{ambient}.{field}` as a condition (only boolean user fields \
                 are; write an explicit comparison)"
            )),
        }
    }
}

/// `NULLIF(current_setting('sc.user', true), '')` — the caller's user JSON, or
/// `NULL` when anonymous.
///
/// `true` is `missing_ok`, so a GUC that was never set reads as `NULL`. The
/// `NULLIF` is the load-bearing part: a *custom* GUC (`sc.user`) that has been
/// set once on a pooled connection keeps an **empty-string** default on later
/// transactions that do not set it, and `''::jsonb` is a hard error, not NULL.
/// Folding `''` to `NULL` makes an unset user read the same whether the
/// connection is fresh or reused — so the policy fails closed either way.
fn guc_raw() -> QExpr {
    QExpr::Func {
        name: "NULLIF".into(),
        args: vec![
            QExpr::Func {
                name: "current_setting".into(),
                args: vec![QExpr::lit(USER_GUC), QExpr::lit(true)],
            },
            QExpr::lit(""),
        ],
    }
}

/// Accumulate a child predicate onto whatever was already there.
fn and_opt(acc: Option<QExpr>, p: QExpr) -> Option<QExpr> {
    Some(match acc {
        None => p,
        Some(e) => e.and(p),
    })
}

/// The selector a value-aggregating terminal carries, if any.
fn terminal_selector<'a, 'b>(terminal: &'a Terminal<'b>) -> Option<&'a Selector<'b>> {
    match terminal {
        Terminal::Sum(s) | Terminal::Avg(s) | Terminal::Min(s) | Terminal::Max(s) => s.as_ref(),
        Terminal::MaxBy(s) | Terminal::MinBy(s) => Some(s),
        _ => None,
    }
}

/// JS `!` over a predicate that may be SQL `NULL`: `P IS DISTINCT FROM TRUE`.
/// Plain `NOT NULL` would be `NULL` (deny), where JS `!null` is `true` (grant)
/// — the one spot where SQL's three-valued logic and JS's two-valued logic
/// part ways, resolved in JS's favour because the formula language is JS.
fn not_pred(p: QExpr) -> QExpr {
    QExpr::binary(QBinOp::IsDistinct, p, QExpr::lit(true))
}

/// A JS number literal as a SQL value: integral f64s (the common case — row
/// ids, role numbers) become `Int` so they compare cleanly with integer
/// columns; everything else stays `Float`.
fn num_value(n: f64) -> Value {
    const MAX_EXACT_INT: f64 = 9_007_199_254_740_992.0; // 2^53
    if n.fract() == 0.0 && n.abs() <= MAX_EXACT_INT {
        Value::Int(n as i64)
    } else {
        Value::Float(n)
    }
}

/// JavaScript truthiness of a SQL value — used only in [`UserEnv::Inline`],
/// where the actual value is at hand.
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0 && !f.is_nan(),
        Value::Text(s) => !s.is_empty(),
        Value::Decimal(d) => !d.is_zero(),
        Value::Json(j) => match j {
            serde_json::Value::Null => false,
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            serde_json::Value::String(s) => !s.is_empty(),
            // Arrays and objects are objects: truthy, even when empty.
            _ => true,
        },
        // Bytes, UUIDs and temporal values reach JS as objects or non-empty
        // strings: truthy.
        Value::Bytes(_)
        | Value::Uuid(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_) => true,
    }
}

/// A human name for an AST shape, for untranslatable-messages.
fn describe(ast: &Ast) -> &'static str {
    match ast {
        Ast::Call { .. } => "a function call",
        Ast::Array(_) => "an array literal",
        Ast::Template { .. } => "a template literal",
        Ast::Arrow { .. } => "an arrow function",
        Ast::Member { .. } => "property access",
        Ast::Ident(_) => "an identifier",
        Ast::Str(_) | Ast::Num(_) | Ast::Bool(_) | Ast::Null => "a literal",
        Ast::Unary { .. } => "a unary operation",
        Ast::Binary { .. } => "a binary operation",
        Ast::Cond { .. } => "a conditional",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::TableShape;
    use sc_query::{SqlDialect, Statement};

    /// Postgres-flavoured rendering, as in `sc-query`'s own tests: the golden
    /// strings below are the exact WHERE clauses Phase 5 injects and Phase 6
    /// bakes into policies.
    struct Pg;

    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }

    /// books(id, title, pages, owner, publisher→publishers.id);
    /// publishers(id, name, country→countries.code); countries(code, name);
    /// reviews(id, book→books.id, rating, reviewer, approved).
    fn shape() -> SchemaShape {
        SchemaShape::new()
            .table(
                "books",
                TableShape::new()
                    .primary_key("id")
                    .field("id")
                    .field("title")
                    .field("pages")
                    .field("owner")
                    .key_field("publisher", "publishers", "id"),
            )
            .table(
                "publishers",
                TableShape::new().field("id").field("name").key_field(
                    "country",
                    "countries",
                    "code",
                ),
            )
            .table("countries", TableShape::new().field("code").field("name"))
            .table(
                "reviews",
                TableShape::new()
                    .primary_key("id")
                    .field("id")
                    .key_field("book", "books", "id")
                    .field("rating")
                    .field("reviewer")
                    .field("approved"),
            )
            .user_fields(["id", "role", "email", "is_admin"])
    }

    fn inline_user(fields: &[(&str, Value)]) -> UserEnv {
        UserEnv::Inline(Some(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ))
    }

    fn guc(types: &[(&str, &str)]) -> UserEnv {
        UserEnv::Guc {
            field_types: types
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// One ambient object's test values: its fields, or `None` for an object
    /// that is in scope but null (`old` on an insert).
    type AmbientCase<'a> = (Ambient, Option<&'a [(&'a str, Value)]>);

    /// An ambient `row`/`old` map from a list of [`AmbientCase`]s.
    fn ambient(entries: &[AmbientCase<'_>]) -> AmbientValues {
        entries
            .iter()
            .map(|(amb, fields)| {
                (
                    *amb,
                    fields.map(|fs| fs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()),
                )
            })
            .collect()
    }

    /// [`where_sql`] with the triggering event's ambient objects in scope.
    fn where_sql_ambient(
        src: &str,
        env: &UserEnv,
        ambient: &AmbientValues,
    ) -> (String, Vec<Value>) {
        let formula = Formula::parse(src).unwrap();
        let pred = translate(
            &formula,
            Operation::Read,
            &Env::new(env).with_ambient(ambient),
            &shape(),
            "books",
        )
        .unwrap();
        let stmt: Statement = Select::from(Source::table("books")).filter(pred).into();
        let (sql, binds) = Pg.render(&stmt).unwrap();
        let clause = sql
            .strip_prefix("SELECT * FROM \"books\" WHERE ")
            .unwrap_or_else(|| panic!("unexpected statement shape: {sql}"))
            .to_string();
        (clause, binds)
    }

    #[test]
    fn an_ambient_row_member_inlines_as_a_literal() {
        // The event's row is *not* the table being filtered: `owner` is a column
        // of `books`, `row.owner` is a value from the triggering row (decision 7).
        let anon = UserEnv::Inline(None);
        let amb = ambient(&[(
            Ambient::Row,
            Some(&[("owner", Value::Text("alice".into()))]),
        )]);
        let (sql, binds) = where_sql_ambient("owner === row.owner", &anon, &amb);
        assert_eq!(sql, "(\"books\".\"owner\" IS NOT DISTINCT FROM $1)");
        assert_eq!(binds, vec![Value::Text("alice".into())]);
    }

    #[test]
    fn an_ambient_object_that_is_null_reads_as_null_and_compares_as_such() {
        let anon = UserEnv::Inline(None);
        // `old` in scope but null — an insert. `old.x` is NULL, and `old === null`
        // is true, exactly as an anonymous `user` behaves.
        let amb = ambient(&[(Ambient::Old, None)]);
        let (sql, binds) = where_sql_ambient("title === old.title", &anon, &amb);
        assert_eq!(sql, "(\"books\".\"title\" IS NOT DISTINCT FROM $1)");
        assert_eq!(binds, vec![Value::Null]);
        // Constants bind rather than render as keywords (every literal is
        // parameterised), so the verdict is in the bind.
        let (_, binds) = where_sql_ambient("old === null", &anon, &amb);
        assert_eq!(binds, vec![Value::Bool(true)]);
        let (_, binds) = where_sql_ambient("old", &anon, &amb);
        assert_eq!(binds, vec![Value::Bool(false)]);
        // With values, the same formulas invert.
        let amb = ambient(&[(Ambient::Old, Some(&[("title", Value::Text("a".into()))]))]);
        let (sql, binds) = where_sql_ambient("title === old.title", &anon, &amb);
        assert_eq!(sql, "(\"books\".\"title\" IS NOT DISTINCT FROM $1)");
        assert_eq!(binds, vec![Value::Text("a".into())]);
        let (_, binds) = where_sql_ambient("old === null", &anon, &amb);
        assert_eq!(binds, vec![Value::Bool(false)]);
    }

    #[test]
    fn an_out_of_scope_ambient_name_stays_a_field_reference() {
        // No ambient map: `row` is not in scope, so a formula naming it is
        // translated as an identifier — and on a table with no such field that is
        // the unknown-identifier *error*, never a silent null.
        let anon = UserEnv::Inline(None);
        let err = err_of("owner === row.owner", Operation::Read, &anon);
        assert!(
            matches!(&err, TranslateError::Untranslatable(w) if w.contains("other than an ambient object")),
            "got: {err:?}"
        );
    }

    /// Translate `src` on `books` and render it as the WHERE clause of a
    /// `SELECT * FROM books`, returning the clause text and its binds.
    fn where_sql(src: &str, op: Operation, env: &UserEnv) -> (String, Vec<Value>) {
        let formula = Formula::parse(src).unwrap();
        let pred = translate(&formula, op, &Env::new(env), &shape(), "books").unwrap();
        let stmt: Statement = Select::from(Source::table("books")).filter(pred).into();
        let (sql, binds) = Pg.render(&stmt).unwrap();
        let clause = sql
            .strip_prefix("SELECT * FROM \"books\" WHERE ")
            .unwrap_or_else(|| panic!("unexpected statement shape: {sql}"))
            .to_string();
        (clause, binds)
    }

    #[test]
    fn a_rooted_translation_names_the_alias_everywhere_the_row_is_read() {
        // What a child table's ownership predicate needs to be usable inside a
        // correlated aggregate over it: `FROM "books" "_fd_g1"` hides `books`,
        // so every reference to the row — a column, the root of a Ⱶ-join, the
        // parent side of a Ↄ-correlation — has to say `_fd_g1` instead.
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let formula = Formula::parse(
            "owner === user.id && publisherⱵname === 'Acme' && reviewsↃbook.length > 0",
        )
        .unwrap();
        let pred = translate_rooted(
            &formula,
            Operation::Read,
            &Env::new(&env),
            &shape(),
            "books",
            "_fd_g1",
        )
        .unwrap();
        let stmt: Statement = Select::from(Source::table_as("books", "_fd_g1"))
            .filter(pred)
            .into();
        let (sql, _) = Pg.render(&stmt).unwrap();
        assert!(sql.contains("\"_fd_g1\".\"owner\""), "{sql}");
        assert!(sql.contains("\"_fd_g1\".\"publisher\""), "{sql}");
        // (`_fd_a2`: the join path took the first alias of this translation.)
        assert!(
            sql.contains("\"_fd_a2\".\"book\" = \"_fd_g1\".\"id\""),
            "{sql}"
        );
        // The table's own name is nowhere but in the `FROM`.
        assert_eq!(sql.matches("\"books\"").count(), 1, "{sql}");
    }

    #[test]
    fn an_unrooted_translation_is_the_rooted_one_at_the_tables_own_name() {
        // `translate` is `translate_rooted` with the table as its own root, so
        // there is one translator and not two.
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let formula = Formula::parse("owner === user.id").unwrap();
        let plain = translate(
            &formula,
            Operation::Read,
            &Env::new(&env),
            &shape(),
            "books",
        );
        let rooted = translate_rooted(
            &formula,
            Operation::Read,
            &Env::new(&env),
            &shape(),
            "books",
            "books",
        );
        assert_eq!(plain.unwrap(), rooted.unwrap());
    }

    fn err_of(src: &str, op: Operation, env: &UserEnv) -> TranslateError {
        let formula = Formula::parse(src).unwrap();
        translate(&formula, op, &Env::new(env), &shape(), "books").unwrap_err()
    }

    #[test]
    fn inline_equality_is_null_safe_and_binds_the_users_value() {
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let (sql, binds) = where_sql("owner === user.id", Operation::Read, &env);
        assert_eq!(sql, "(\"books\".\"owner\" IS NOT DISTINCT FROM $1)");
        assert_eq!(binds, vec![Value::Text("u1".into())]);
    }

    #[test]
    fn guc_equality_extracts_from_the_setting_and_casts() {
        let env = guc(&[("id", "uuid")]);
        let (sql, binds) = where_sql("owner === user.id", Operation::Read, &env);
        assert_eq!(
            sql,
            "(\"books\".\"owner\" IS NOT DISTINCT FROM \
             CAST(jsonb_extract_path_text(\
             CAST(NULLIF(current_setting($1, $2), $3) AS jsonb), $4) AS uuid))"
        );
        assert_eq!(
            binds,
            vec![
                Value::Text(USER_GUC.into()),
                Value::Bool(true),
                Value::Text(String::new()),
                Value::Text("id".into()),
            ]
        );
    }

    #[test]
    fn a_text_user_field_is_not_cast() {
        let env = guc(&[("email", "text")]);
        let (sql, _) = where_sql("owner === user.email", Operation::Read, &env);
        assert!(!sql.contains("AS text"), "needless cast in: {sql}");
        assert!(sql.contains("jsonb_extract_path_text"), "got: {sql}");
    }

    #[test]
    fn flags_fold_per_operation() {
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        // `_read || …` is TRUE for a select — the whole formula folds away…
        let (sql, binds) = where_sql("_read || owner === user.id", Operation::Read, &env);
        assert_eq!(sql, "$1");
        assert_eq!(binds, vec![Value::Bool(true)]);
        // …and exactly the ownership clause for a write.
        let (sql, _) = where_sql("_read || owner === user.id", Operation::Update, &env);
        assert_eq!(sql, "(\"books\".\"owner\" IS NOT DISTINCT FROM $1)");
        // `_write && …` is FALSE for a select.
        let (sql, binds) = where_sql("_write && owner === user.id", Operation::Read, &env);
        assert_eq!(sql, "$1");
        assert_eq!(binds, vec![Value::Bool(false)]);
        // Each write flag reaches its own operation.
        for (op, expect) in [
            (Operation::Insert, true),
            (Operation::Update, false),
            (Operation::Delete, false),
        ] {
            let (_, binds) = where_sql("_insert", op, &env);
            assert_eq!(binds, vec![Value::Bool(expect)], "{op:?}");
        }
    }

    #[test]
    fn a_join_path_becomes_a_correlated_scalar_subselect() {
        let env = UserEnv::Inline(None);
        let (sql, binds) = where_sql("publisherⱵname === 'ACME'", Operation::Read, &env);
        assert_eq!(
            sql,
            "((SELECT \"_fd_j1\".\"name\" FROM \"publishers\" AS \"_fd_j1\" \
             WHERE (\"_fd_j1\".\"id\" = \"books\".\"publisher\")) IS NOT DISTINCT FROM $1)"
        );
        assert_eq!(binds, vec![Value::Text("ACME".into())]);
    }

    #[test]
    fn a_chained_join_path_nests_one_subselect_per_link() {
        let env = UserEnv::Inline(None);
        let (sql, _) = where_sql("publisherⱵcountryⱵname === 'DK'", Operation::Read, &env);
        assert_eq!(
            sql,
            "((SELECT \"_fd_j2\".\"name\" FROM \"countries\" AS \"_fd_j2\" WHERE \
             (\"_fd_j2\".\"code\" = \
             (SELECT \"_fd_j1\".\"country\" FROM \"publishers\" AS \"_fd_j1\" \
             WHERE (\"_fd_j1\".\"id\" = \"books\".\"publisher\")))) IS NOT DISTINCT FROM $1)"
        );
    }

    #[test]
    fn null_literal_comparisons_use_is_null() {
        let env = UserEnv::Inline(None);
        let (sql, _) = where_sql("owner === null", Operation::Read, &env);
        assert_eq!(sql, "(\"books\".\"owner\" IS NULL)");
        let (sql, _) = where_sql("owner !== null", Operation::Read, &env);
        assert_eq!(sql, "(\"books\".\"owner\" IS NOT NULL)");
        let (sql, binds) = where_sql("null === null", Operation::Read, &env);
        assert_eq!(sql, "$1");
        assert_eq!(binds, vec![Value::Bool(true)]);
    }

    #[test]
    fn user_null_checks_translate_per_env() {
        let (sql, binds) = where_sql("user === null", Operation::Read, &UserEnv::Inline(None));
        assert_eq!((sql.as_str(), binds), ("$1", vec![Value::Bool(true)]));
        let logged_in = inline_user(&[]);
        let (_, binds) = where_sql("user === null", Operation::Read, &logged_in);
        assert_eq!(binds, vec![Value::Bool(false)]);
        let (sql, _) = where_sql("user === null", Operation::Read, &guc(&[]));
        assert_eq!(sql, "(NULLIF(current_setting($1, $2), $3) IS NULL)");
        // Bare `user` as a condition is the logged-in test.
        let (sql, _) = where_sql(
            "user && owner === user.id",
            Operation::Read,
            &guc(&[("id", "uuid")]),
        );
        assert!(
            sql.starts_with("((NULLIF(current_setting($1, $2), $3) IS NOT NULL) AND "),
            "got: {sql}"
        );
    }

    #[test]
    fn an_anonymous_users_field_is_null_which_matches_null() {
        // The documented corner of the two-valued semantics: with no user,
        // `user.id` is null, and `owner === user.id` therefore *matches rows
        // whose owner is null* (`null === null` is true in JS and in
        // IS NOT DISTINCT FROM alike). A formula that must not grant
        // anonymously writes `user && …` — bare `user` is object-or-null, so
        // its truthiness is exactly the logged-in test.
        let (sql, binds) = where_sql("owner === user.id", Operation::Read, &UserEnv::Inline(None));
        assert_eq!(sql, "(\"books\".\"owner\" IS NOT DISTINCT FROM $1)");
        assert_eq!(binds, vec![Value::Null]);
        // The guard in action: anonymously, the whole formula folds to FALSE.
        let (sql, binds) = where_sql(
            "user && owner === user.id",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert_eq!(
            (sql.as_str(), binds),
            (
                "($1 AND (\"books\".\"owner\" IS NOT DISTINCT FROM $2))",
                vec![Value::Bool(false), Value::Null]
            )
        );
    }

    #[test]
    fn ordered_comparisons_keep_sql_null_semantics() {
        let env = UserEnv::Inline(None);
        let (sql, binds) = where_sql("pages >= 100", Operation::Read, &env);
        assert_eq!(sql, "(\"books\".\"pages\" >= $1)");
        assert_eq!(binds, vec![Value::Int(100)]);
    }

    #[test]
    fn negation_is_two_valued_not_three_valued() {
        // JS `!null` is `true`; SQL `NOT NULL` is `NULL`. `!P` therefore
        // renders as `P IS DISTINCT FROM TRUE`, so a null-paged row *is*
        // granted by `!(pages < 100)` — on both evaluators.
        let env = UserEnv::Inline(None);
        let (sql, binds) = where_sql("!(pages < 100)", Operation::Read, &env);
        assert_eq!(sql, "((\"books\".\"pages\" < $1) IS DISTINCT FROM $2)");
        assert_eq!(binds, vec![Value::Int(100), Value::Bool(true)]);
    }

    #[test]
    fn integral_numbers_bind_as_int_and_fractional_as_float() {
        let env = UserEnv::Inline(None);
        let (_, binds) = where_sql("pages === 3", Operation::Read, &env);
        assert_eq!(binds, vec![Value::Int(3)]);
        let (_, binds) = where_sql("pages === 3.5", Operation::Read, &env);
        assert_eq!(binds, vec![Value::Float(3.5)]);
    }

    #[test]
    fn nullish_coalescing_is_coalesce() {
        let env = UserEnv::Inline(None);
        let (sql, binds) = where_sql("(title ?? 'anon') === 'x'", Operation::Read, &env);
        assert_eq!(
            sql,
            "(COALESCE(\"books\".\"title\", $1) IS NOT DISTINCT FROM $2)"
        );
        assert_eq!(
            binds,
            vec![Value::Text("anon".into()), Value::Text("x".into())]
        );
    }

    #[test]
    fn a_conditional_translates_to_a_searched_case() {
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let (sql, _) = where_sql(
            "title === 'wiki' ? true : owner === user.id",
            Operation::Read,
            &env,
        );
        assert_eq!(
            sql,
            "CASE WHEN (\"books\".\"title\" IS NOT DISTINCT FROM $1) THEN $2 \
             ELSE (\"books\".\"owner\" IS NOT DISTINCT FROM $3) END"
        );
    }

    #[test]
    fn a_boolean_user_field_is_a_condition_in_both_envs() {
        // Inline: the actual value decides.
        let env = inline_user(&[("is_admin", Value::Bool(true))]);
        let (_, binds) = where_sql("user.is_admin", Operation::Read, &env);
        assert_eq!(binds, vec![Value::Bool(true)]);
        // GUC: declared boolean → null-safe IS NOT DISTINCT FROM TRUE.
        let env = guc(&[("is_admin", "boolean")]);
        let (sql, _) = where_sql("user.is_admin", Operation::Read, &env);
        assert!(sql.contains("IS NOT DISTINCT FROM"), "got: {sql}");
        // GUC: a non-boolean user field's truthiness is not knowable.
        let env = guc(&[("email", "text")]);
        let err = err_of("user.email && owner === user.id", Operation::Read, &env);
        assert!(
            matches!(err, TranslateError::Untranslatable(_)),
            "got: {err}"
        );
    }

    #[test]
    fn untranslatable_constructs_are_named() {
        let env = UserEnv::Inline(None);
        for (src, names) in [
            ("title.includes('x')", "a function call"),
            ("`${title}!` === title", "a template literal"),
            ("owner && true", "a bare value as a condition"),
            ("Math.random() > 0.5", "a function call"),
            // A prediction is hoisted, never translated (milestone 31 §4).
            ("predict('House prices') > 1", "a function call"),
            (
                "publisher.name === 'x'",
                "property access on something other",
            ),
            ("owner === user[title]", "computed access on `user`"),
            ("title === undefined", "the JavaScript global `undefined`"),
            ("owner === (title && owner)", "`&&`/`||` as a value"),
            ("pages + 1", "arithmetic as a condition"),
            ("owner === user", "comparing the `user` object itself"),
        ] {
            let err = err_of(src, Operation::Read, &env);
            let TranslateError::Untranslatable(msg) = &err else {
                panic!("{src}: expected Untranslatable, got {err:?}");
            };
            assert!(msg.contains(names), "{src}: expected `{names}` in: {msg}");
        }
    }

    #[test]
    fn arithmetic_translates_in_value_position() {
        let env = UserEnv::Inline(None);
        let (sql, binds) = where_sql("pages % 2 === 0", Operation::Read, &env);
        assert_eq!(sql, "((\"books\".\"pages\" % $1) IS NOT DISTINCT FROM $2)");
        assert_eq!(binds, vec![Value::Int(2), Value::Int(0)]);
    }

    #[test]
    fn aggregation_length_becomes_a_correlated_count() {
        let (sql, _) = where_sql(
            "reviewsↃbook.length > 0",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert_eq!(
            sql,
            "((SELECT count(*) FROM \"reviews\" AS \"_fd_a1\" \
             WHERE (\"_fd_a1\".\"book\" = \"books\".\"id\")) > $1)"
        );
    }

    #[test]
    fn aggregation_sum_coalesces_and_takes_a_selector() {
        let (sql, binds) = where_sql(
            "reviewsↃbook.sum(\"rating\") >= 10",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert_eq!(
            sql,
            "(COALESCE((SELECT sum(\"_fd_a1\".\"rating\") FROM \"reviews\" AS \"_fd_a1\" \
             WHERE (\"_fd_a1\".\"book\" = \"books\".\"id\")), $1) >= $2)"
        );
        assert_eq!(binds, vec![Value::Int(0), Value::Int(10)]);
    }

    #[test]
    fn aggregation_filter_folds_into_the_where() {
        let (sql, _) = where_sql(
            "reviewsↃbook.filter(r => r.approved === true).avg(\"rating\") > 3",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert!(
            sql.contains(
                "SELECT avg(\"_fd_a1\".\"rating\") FROM \"reviews\" AS \"_fd_a1\" \
                 WHERE ((\"_fd_a1\".\"book\" = \"books\".\"id\") AND \
                 (\"_fd_a1\".\"approved\" IS NOT DISTINCT FROM"
            ),
            "got: {sql}"
        );
    }

    #[test]
    fn aggregation_some_is_an_exists_style_count() {
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let (sql, _) = where_sql(
            "reviewsↃbook.some(r => r.reviewer === user.id)",
            Operation::Read,
            &env,
        );
        assert_eq!(
            sql,
            "((SELECT count(*) FROM \"reviews\" AS \"_fd_a1\" \
             WHERE ((\"_fd_a1\".\"book\" = \"books\".\"id\") AND \
             (\"_fd_a1\".\"reviewer\" IS NOT DISTINCT FROM $1))) > $2)"
        );
    }

    #[test]
    fn aggregation_every_is_a_not_exists_style_count() {
        let (sql, _) = where_sql(
            "reviewsↃbook.every(r => r.rating >= 3)",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        // A row fails when `rating >= 3` is not provenly true.
        assert_eq!(
            sql,
            "((SELECT count(*) FROM \"reviews\" AS \"_fd_a1\" \
             WHERE ((\"_fd_a1\".\"book\" = \"books\".\"id\") AND \
             ((\"_fd_a1\".\"rating\" >= $1) IS DISTINCT FROM $2))) = $3)"
        );
    }

    #[test]
    fn aggregation_includes_is_a_subquery_membership() {
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let (sql, _) = where_sql(
            "reviewsↃbook.map(r => r.reviewer).includes(user.id)",
            Operation::Read,
            &env,
        );
        assert_eq!(
            sql,
            "($1 IN (SELECT \"_fd_a1\".\"reviewer\" FROM \"reviews\" AS \"_fd_a1\" \
             WHERE (\"_fd_a1\".\"book\" = \"books\".\"id\")))"
        );
    }

    #[test]
    fn aggregation_distinct_count() {
        let (sql, _) = where_sql(
            "reviewsↃbook.distinct(\"reviewer\").length > 1",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert_eq!(
            sql,
            "((SELECT count(DISTINCT \"_fd_a1\".\"reviewer\") FROM \"reviews\" AS \"_fd_a1\" \
             WHERE (\"_fd_a1\".\"book\" = \"books\".\"id\")) > $1)"
        );
    }

    #[test]
    fn aggregation_maxby_orders_and_limits_with_a_pk_tiebreak() {
        let (sql, _) = where_sql(
            "reviewsↃbook.maxBy(\"rating\").reviewer === 'u1'",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert_eq!(
            sql,
            "((SELECT \"_fd_a1\".\"reviewer\" FROM \"reviews\" AS \"_fd_a1\" \
             WHERE ((\"_fd_a1\".\"book\" = \"books\".\"id\") AND \
             (\"_fd_a1\".\"rating\" IS NOT NULL)) \
             ORDER BY \"_fd_a1\".\"rating\" DESC, \"_fd_a1\".\"id\" DESC LIMIT $1) \
             IS NOT DISTINCT FROM $2)"
        );
    }

    #[test]
    fn aggregation_join_uses_string_agg() {
        let (sql, _) = where_sql(
            "reviewsↃbook.map(r => r.reviewer).join(', ') === ''",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        assert!(
            sql.contains("COALESCE((SELECT string_agg(\"_fd_a1\".\"reviewer\", $1)"),
            "got: {sql}"
        );
    }

    #[test]
    fn aggregation_in_guc_mode_renders_the_user_extraction() {
        // The RLS path: `user.id` inside a `some` predicate becomes the GUC
        // extraction, correlated inside the subquery.
        let env = guc(&[("id", "uuid")]);
        let (sql, _) = where_sql(
            "reviewsↃbook.some(r => r.reviewer === user.id)",
            Operation::Read,
            &env,
        );
        assert!(sql.contains("jsonb_extract_path_text"), "got: {sql}");
        assert!(
            sql.contains("SELECT count(*) FROM \"reviews\""),
            "got: {sql}"
        );
    }

    /// The `sc_query::Expr` a value-position formula translates to on `books`.
    fn value_expr(src: &str) -> QExpr {
        let formula = Formula::parse(src).unwrap();
        let anon = UserEnv::Inline(None);
        translate_value(&formula, &Env::new(&anon), &shape(), "books").unwrap()
    }

    /// The same aggregate asked for through the builder: `reviews` correlated
    /// to `books`, at the alias the translator numbers first.
    fn built(func: AggFunc, value: Option<QExpr>, filter: Option<QExpr>) -> QExpr {
        correlated_aggregate(AggregateSpec {
            child_table: "reviews".into(),
            key_field: "book".into(),
            parent: "books".into(),
            parent_field: "id".into(),
            alias: "_fd_a1".into(),
            func,
            distinct: false,
            value,
            filter,
        })
        .unwrap()
    }

    #[test]
    fn the_formula_path_and_the_builder_produce_the_identical_expr() {
        // The point of the refactor: a Ↄ chain and a caller that asks the
        // builder the same question get the *same tree*, not merely SQL that
        // happens to match today. If the two ever drift, this fails.
        assert_eq!(
            value_expr("reviewsↃbook.length"),
            built(AggFunc::Count, None, None)
        );
        assert_eq!(
            value_expr("reviewsↃbook.sum(\"rating\")"),
            built(AggFunc::Sum, Some(QExpr::qcol("_fd_a1", "rating")), None)
        );
        assert_eq!(
            value_expr("reviewsↃbook.avg(\"rating\")"),
            built(AggFunc::Avg, Some(QExpr::qcol("_fd_a1", "rating")), None)
        );
        // …and with a child predicate, which is the GraphQL `where` argument's
        // shape: the constraint is the subquery's, not the caller's.
        assert_eq!(
            value_expr("reviewsↃbook.filter(r => r.rating >= 3).length"),
            built(
                AggFunc::Count,
                None,
                Some(QExpr::binary(
                    QBinOp::Ge,
                    QExpr::qcol("_fd_a1", "rating"),
                    QExpr::lit(3_i64),
                )),
            )
        );
    }

    #[test]
    fn a_join_path_can_be_rooted_at_an_alias() {
        // What a GraphQL filter over a *child* table needs: the same join
        // path, correlated from the subquery's alias rather than from the
        // table's own name.
        let expr = join_path_expr_rooted(&shape(), "_fd_a1", "books", "publisherⱵname").unwrap();
        let stmt: Statement = Select::from(Source::table("books"))
            .columns(vec![Projection::expr(expr)])
            .into();
        let (sql, _) = Pg.render(&stmt).unwrap();
        assert_eq!(
            sql,
            "SELECT (SELECT \"_fd_j1\".\"name\" FROM \"publishers\" AS \"_fd_j1\" \
             WHERE (\"_fd_j1\".\"id\" = \"_fd_a1\".\"publisher\")) FROM \"books\""
        );

        // Rooting at the table's own name is what the table-rooted entry point
        // already did — one implementation, so they cannot disagree.
        assert_eq!(
            join_path_expr(&shape(), "books", "publisherⱵname").unwrap(),
            join_path_expr_rooted(&shape(), "books", "books", "publisherⱵname").unwrap()
        );
    }

    #[test]
    fn a_stage_whose_rows_are_another_tables_aggregates_over_its_children() {
        // A dataset stage aggregated by `customer` is not a table, but each of
        // its rows is a customer (analytics TODO A1.2): `rows_of` says so, and
        // the child's key then correlates on the stage's `customer` column.
        let shape = SchemaShape::new()
            .table(
                "orders",
                TableShape::new()
                    .field("id")
                    .field("total")
                    .key_field("customer", "customers", "id")
                    .primary_key("id"),
            )
            .table("customers", TableShape::new().field("id").primary_key("id"))
            .table(
                "_fd_stage",
                TableShape::new()
                    .key_field("customer", "customers", "id")
                    .field("n")
                    .rows_of("customers", "id", "customer"),
            );
        let formula = Formula::parse("ordersↃcustomer.length").unwrap();
        formula.validate(&shape, "_fd_stage").unwrap();
        let user = UserEnv::Inline(None);
        let expr =
            translate_value_rooted(&formula, &Env::new(&user), &shape, "_fd_stage", "_fd_s1")
                .unwrap();
        let stmt: Statement = Select::from(Source::table("x"))
            .columns(vec![Projection::expr(expr)])
            .into();
        let (sql, _) = Pg.render(&stmt).unwrap();
        assert!(
            sql.contains("(\"_fd_a1\".\"customer\" = \"_fd_s1\".\"customer\")"),
            "{sql}"
        );

        // Without `rows_of` the same stage is refused, naming the table the
        // key points at.
        let mut bare = shape.clone();
        if let Some(stage) = bare.tables.get_mut("_fd_stage") {
            stage.rows_of = None;
        }
        let err = formula.validate(&bare, "_fd_stage").unwrap_err();
        assert!(err.to_string().contains("points at `customers`"), "{err}");
    }

    fn calc_of(pairs: &[(&str, &str)]) -> CalcFields {
        pairs
            .iter()
            .map(|(name, src)| (name.to_string(), Formula::parse(src).unwrap()))
            .collect()
    }

    #[test]
    fn a_calc_field_reference_is_inlined_into_an_ownership_formula() {
        // The formula names calc field `big`, which has no column: it must
        // expand to its defining expression, not become `books.big`.
        let calc = calc_of(&[("big", "pages > 100")]);
        let formula = Formula::parse("owner === user.id || big").unwrap();
        let env = inline_user(&[("id", Value::Text("u1".into()))]);
        let pred = translate(
            &formula,
            Operation::Read,
            &Env::new(&env).with_calc(&calc),
            &shape(),
            "books",
        )
        .unwrap();
        let stmt: Statement = Select::from(Source::table("books")).filter(pred).into();
        let (sql, _) = Pg.render(&stmt).unwrap();
        assert!(sql.contains("\"books\".\"pages\" > "), "not inlined: {sql}");
        assert!(!sql.contains("\"big\""), "calc column leaked: {sql}");
    }

    #[test]
    fn translate_value_inlines_calc_to_calc_transitively() {
        // gross = pages + tax, tax = pages * 2 → gross expands to
        // pages + (pages * 2), with no reference to `tax` or `gross`.
        let calc = calc_of(&[("tax", "pages * 2"), ("gross", "pages + tax")]);
        let gross = Formula::parse("pages + tax").unwrap();
        let anon = UserEnv::Inline(None);
        let value =
            translate_value(&gross, &Env::new(&anon).with_calc(&calc), &shape(), "books").unwrap();
        let stmt: Statement = Select::from(Source::table("books"))
            .columns(vec![Projection::expr(value)])
            .into();
        let (sql, _) = Pg.render(&stmt).unwrap();
        assert!(
            sql.contains("(\"books\".\"pages\" + (\"books\".\"pages\" * "),
            "not transitively inlined: {sql}"
        );
        assert!(!sql.contains("\"tax\""), "calc column leaked: {sql}");
    }

    #[test]
    fn an_unknown_identifier_is_an_error_not_a_fallback() {
        // Phase 5 falls back to the reified evaluator on Untranslatable; a
        // typo must not ride that path into V8 and "work" by throwing.
        let err = err_of("writer === 1", Operation::Read, &UserEnv::Inline(None));
        let TranslateError::Error(e) = err else {
            panic!("expected Error, got {err:?}");
        };
        assert!(
            e.to_string().contains("unknown identifier `writer`"),
            "got: {e}"
        );
    }

    #[test]
    fn translate_error_converts_to_workspace_error() {
        let err = err_of(
            "title.includes('x')",
            Operation::Read,
            &UserEnv::Inline(None),
        );
        let e: Error = err.into();
        assert!(matches!(e.repr(), sc_error::Repr::Invalid(_)));
        assert!(e.to_string().contains("cannot be translated"), "got: {e}");
    }
}
