//! The formula AST — Saltcorn's own, lowered from swc's at the parse boundary.
//!
//! swc's AST never escapes this crate, for two reasons. First, ownership: a
//! [`Formula`](crate::Formula) is stored on cached catalog entries shared across
//! threads, so its AST must be plain owned data (`Send + Sync`), free of interned
//! atoms and spans. Second — and this is the design point — both evaluators
//! consume *this* AST: the symbolic translator (Phase 2) walks it into
//! `sc_query::Expr`, and the reified evaluator (Phase 3) renders normalised
//! JavaScript from it. One tree with one specified semantics is what makes
//! evaluator parity a provable property rather than a hope.
//!
//! Lowering is also where the language is bounded: a formula is a single pure
//! expression. Assignment, `new` (bar `new Date(…)`), function/class
//! definitions, `await` — anything with an effect or a binding beyond an arrow
//! parameter — is refused here by name, so neither evaluator ever has to decide
//! what such a construct means.

use sc_error::{Error, Result};
use swc_ecma_ast as swc;

/// A node in a formula's expression tree.
///
/// The variant set is exactly the subset of JavaScript a formula may use. It is
/// wider than what translates to SQL — a method call such as
/// `user.groups.some(g => g === dept)` lowers fine and simply comes back
/// `Untranslatable` from Phase 2, falling to the reified evaluator — but
/// everything here has a defined meaning in *some* evaluator.
#[derive(Debug, Clone, PartialEq)]
pub enum Ast {
    /// An identifier: a field name, `user`, an operation flag, a whitelisted
    /// global — or a Ⱶ-join path, which is a *single identifier* because Ⱶ
    /// (U+2C75) is Unicode category Lu and therefore a valid JavaScript
    /// identifier character. Classification is the analyser's job, not the
    /// parser's.
    Ident(String),
    /// A string literal.
    Str(String),
    /// A numeric literal (JS numbers are f64).
    Num(f64),
    /// A boolean literal.
    Bool(bool),
    /// The `null` literal.
    Null,
    /// Member access: `obj.prop` or `obj[expr]`; `optional` marks `?.`.
    Member {
        /// The object being accessed.
        obj: Box<Ast>,
        /// The property: static name or computed expression.
        prop: MemberProp,
        /// True for optional chaining (`obj?.prop`).
        optional: bool,
    },
    /// A call: `callee(args…)`; `optional` marks `?.()`.
    Call {
        /// The function or method being called.
        callee: Box<Ast>,
        /// Argument expressions, in order.
        args: Vec<Ast>,
        /// True for optional call (`f?.()`).
        optional: bool,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Ast>,
    },
    /// A binary operation (including `&&`/`||`/`??`, which swc also folds into
    /// its binary node).
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        l: Box<Ast>,
        /// Right operand.
        r: Box<Ast>,
    },
    /// The conditional operator `test ? cons : alt`.
    Cond {
        /// The condition.
        test: Box<Ast>,
        /// Value when the condition is truthy.
        cons: Box<Ast>,
        /// Value when the condition is falsy.
        alt: Box<Ast>,
    },
    /// An array literal (no holes, no spread).
    Array(Vec<Ast>),
    /// A template literal: `quasis` has one more element than `exprs`, and the
    /// rendering interleaves them starting with a quasi.
    Template {
        /// The literal text runs.
        quasis: Vec<String>,
        /// The interpolated expressions.
        exprs: Vec<Ast>,
    },
    /// `new Date(args…)` — the one constructor a formula may call, so that a
    /// date default (`new Date()`) or date arithmetic reads as JavaScript does.
    /// Any other `new` is refused at lowering.
    NewDate(Vec<Ast>),
    /// An arrow function with simple named parameters and an expression body —
    /// the only binding form a formula has, and only so that array methods
    /// (`some`, `every`, …) are usable on the reified path.
    Arrow {
        /// Parameter names, bound within `body`.
        params: Vec<String>,
        /// The body expression.
        body: Box<Ast>,
    },
}

/// The property side of a [`Ast::Member`] access.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberProp {
    /// A static property name: `obj.name`.
    Static(String),
    /// A computed property: `obj[expr]`.
    Computed(Box<Ast>),
}

/// Unary operators a formula may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Logical not (`!`).
    Not,
    /// Numeric negation (`-`).
    Neg,
    /// Unary plus (`+`).
    Pos,
    /// `typeof`.
    TypeOf,
}

/// Binary operators a formula may use. `&&`/`||`/`??` are here rather than in a
/// separate logical node, mirroring swc; the translator gives them their own
/// treatment regardless of where they sit in the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// Loose equality (`==`).
    Eq,
    /// Loose inequality (`!=`).
    NotEq,
    /// Strict equality (`===`).
    StrictEq,
    /// Strict inequality (`!==`).
    StrictNotEq,
    /// Less than (`<`).
    Lt,
    /// Less than or equal (`<=`).
    LtEq,
    /// Greater than (`>`).
    Gt,
    /// Greater than or equal (`>=`).
    GtEq,
    /// Addition / string concatenation (`+`).
    Add,
    /// Subtraction (`-`).
    Sub,
    /// Multiplication (`*`).
    Mul,
    /// Division (`/`).
    Div,
    /// Remainder (`%`).
    Mod,
    /// Logical and (`&&`).
    And,
    /// Logical or (`||`).
    Or,
    /// Nullish coalescing (`??`).
    Nullish,
}

/// Lower a swc expression into an [`Ast`], refusing — by name — everything
/// outside the formula language.
pub(crate) fn lower(e: &swc::Expr) -> Result<Ast> {
    match e {
        swc::Expr::Paren(p) => lower(&p.expr),
        swc::Expr::Ident(i) => Ok(Ast::Ident(i.sym.to_string())),
        swc::Expr::Lit(lit) => lower_lit(lit),
        swc::Expr::Bin(b) => Ok(Ast::Binary {
            op: lower_binop(b.op)?,
            l: Box::new(lower(&b.left)?),
            r: Box::new(lower(&b.right)?),
        }),
        swc::Expr::Unary(u) => Ok(Ast::Unary {
            op: lower_unop(u.op)?,
            expr: Box::new(lower(&u.arg)?),
        }),
        swc::Expr::Cond(c) => Ok(Ast::Cond {
            test: Box::new(lower(&c.test)?),
            cons: Box::new(lower(&c.cons)?),
            alt: Box::new(lower(&c.alt)?),
        }),
        swc::Expr::Member(m) => lower_member(m, false),
        swc::Expr::OptChain(o) => match &*o.base {
            swc::OptChainBase::Member(m) => lower_member(m, true),
            swc::OptChainBase::Call(c) => Ok(Ast::Call {
                callee: Box::new(lower(&c.callee)?),
                args: lower_args(&c.args)?,
                optional: true,
            }),
        },
        swc::Expr::Call(c) => {
            let callee = match &c.callee {
                swc::Callee::Expr(e) => lower(e)?,
                swc::Callee::Super(_) | swc::Callee::Import(_) => {
                    return Err(unsupported("`super`/`import` calls"));
                }
            };
            Ok(Ast::Call {
                callee: Box::new(callee),
                args: lower_args(&c.args)?,
                optional: false,
            })
        }
        swc::Expr::Arrow(a) => lower_arrow(a),
        swc::Expr::Array(a) => {
            let mut elems = Vec::with_capacity(a.elems.len());
            for elem in &a.elems {
                let Some(elem) = elem else {
                    return Err(unsupported("array holes (elision)"));
                };
                if elem.spread.is_some() {
                    return Err(unsupported("spread (`...`)"));
                }
                elems.push(lower(&elem.expr)?);
            }
            Ok(Ast::Array(elems))
        }
        swc::Expr::Tpl(t) => {
            let mut quasis = Vec::with_capacity(t.quasis.len());
            for q in &t.quasis {
                match q.cooked.as_ref().and_then(|c| c.as_atom()) {
                    Some(text) => quasis.push(text.to_string()),
                    None => return Err(unsupported("template literals with invalid escapes")),
                }
            }
            let exprs = t.exprs.iter().map(|e| lower(e)).collect::<Result<_>>()?;
            Ok(Ast::Template { quasis, exprs })
        }
        // Everything below is refused by name: a formula is one pure expression.
        swc::Expr::Assign(_) => Err(unsupported("assignment")),
        swc::Expr::Update(_) => Err(unsupported("increment/decrement")),
        swc::Expr::Seq(_) => Err(unsupported("comma expressions")),
        swc::Expr::New(n) if matches!(&*n.callee, swc::Expr::Ident(i) if &*i.sym == "Date") => {
            Ok(Ast::NewDate(match &n.args {
                Some(args) => lower_args(args)?,
                None => Vec::new(),
            }))
        }
        swc::Expr::New(_) => Err(unsupported("`new` (other than `new Date(…)`)")),
        swc::Expr::Object(_) => Err(unsupported("object literals")),
        swc::Expr::Fn(_) => Err(unsupported("`function` expressions (use an arrow)")),
        swc::Expr::Class(_) => Err(unsupported("class expressions")),
        swc::Expr::Await(_) => Err(unsupported("`await`")),
        swc::Expr::Yield(_) => Err(unsupported("`yield`")),
        swc::Expr::This(_) => Err(unsupported("`this`")),
        swc::Expr::TaggedTpl(_) => Err(unsupported("tagged templates")),
        _ => Err(unsupported("this syntax")),
    }
}

fn lower_lit(lit: &swc::Lit) -> Result<Ast> {
    match lit {
        // A JS string may be WTF-8 (lone surrogates, e.g. '\uD800'); a formula
        // string must be real text — lossy replacement would silently corrupt
        // the value being compared against (principle 5).
        swc::Lit::Str(s) => match s.value.as_atom() {
            Some(text) => Ok(Ast::Str(text.to_string())),
            None => Err(unsupported("string literals with unpaired surrogates")),
        },
        swc::Lit::Num(n) => Ok(Ast::Num(n.value)),
        swc::Lit::Bool(b) => Ok(Ast::Bool(b.value)),
        swc::Lit::Null(_) => Ok(Ast::Null),
        swc::Lit::BigInt(_) => Err(unsupported("BigInt literals")),
        swc::Lit::Regex(_) => Err(unsupported("regular expression literals")),
        swc::Lit::JSXText(_) => Err(unsupported("JSX")),
    }
}

fn lower_member(m: &swc::MemberExpr, optional: bool) -> Result<Ast> {
    let prop = match &m.prop {
        swc::MemberProp::Ident(i) => MemberProp::Static(i.sym.to_string()),
        swc::MemberProp::Computed(c) => MemberProp::Computed(Box::new(lower(&c.expr)?)),
        swc::MemberProp::PrivateName(_) => return Err(unsupported("private names (`#x`)")),
    };
    Ok(Ast::Member {
        obj: Box::new(lower(&m.obj)?),
        prop,
        optional,
    })
}

fn lower_args(args: &[swc::ExprOrSpread]) -> Result<Vec<Ast>> {
    args.iter()
        .map(|a| {
            if a.spread.is_some() {
                return Err(unsupported("spread (`...`)"));
            }
            lower(&a.expr)
        })
        .collect()
}

fn lower_arrow(a: &swc::ArrowExpr) -> Result<Ast> {
    if a.is_async || a.is_generator {
        return Err(unsupported("async/generator arrows"));
    }
    let params = a
        .params
        .iter()
        .map(|p| match p {
            swc::Pat::Ident(i) => Ok(i.id.sym.to_string()),
            _ => Err(unsupported("destructuring/default parameters")),
        })
        .collect::<Result<Vec<_>>>()?;
    let body = match &*a.body {
        swc::BlockStmtOrExpr::Expr(e) => lower(e)?,
        swc::BlockStmtOrExpr::BlockStmt(_) => {
            return Err(unsupported("arrow functions with a block body"));
        }
    };
    Ok(Ast::Arrow {
        params,
        body: Box::new(body),
    })
}

fn lower_binop(op: swc::BinaryOp) -> Result<BinaryOp> {
    use swc::BinaryOp as S;
    Ok(match op {
        S::EqEq => BinaryOp::Eq,
        S::NotEq => BinaryOp::NotEq,
        S::EqEqEq => BinaryOp::StrictEq,
        S::NotEqEq => BinaryOp::StrictNotEq,
        S::Lt => BinaryOp::Lt,
        S::LtEq => BinaryOp::LtEq,
        S::Gt => BinaryOp::Gt,
        S::GtEq => BinaryOp::GtEq,
        S::Add => BinaryOp::Add,
        S::Sub => BinaryOp::Sub,
        S::Mul => BinaryOp::Mul,
        S::Div => BinaryOp::Div,
        S::Mod => BinaryOp::Mod,
        S::LogicalAnd => BinaryOp::And,
        S::LogicalOr => BinaryOp::Or,
        S::NullishCoalescing => BinaryOp::Nullish,
        S::In => return Err(unsupported("`in`")),
        S::InstanceOf => return Err(unsupported("`instanceof`")),
        S::Exp => return Err(unsupported("exponentiation (`**`)")),
        S::LShift | S::RShift | S::ZeroFillRShift | S::BitAnd | S::BitOr | S::BitXor => {
            return Err(unsupported("bitwise operators"));
        }
    })
}

fn lower_unop(op: swc::UnaryOp) -> Result<UnaryOp> {
    use swc::UnaryOp as S;
    Ok(match op {
        S::Bang => UnaryOp::Not,
        S::Minus => UnaryOp::Neg,
        S::Plus => UnaryOp::Pos,
        S::TypeOf => UnaryOp::TypeOf,
        S::Tilde => return Err(unsupported("bitwise not (`~`)")),
        S::Void => return Err(unsupported("`void`")),
        S::Delete => return Err(unsupported("`delete`")),
    })
}

/// An `Invalid` error for a construct outside the formula language. The message
/// names the construct — the admin reads this in the table editor.
fn unsupported(what: &str) -> Error {
    Error::invalid(format!("{what} cannot be used in a formula"))
}
