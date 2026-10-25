//! The normalised JavaScript rendering of a formula — what the reified
//! evaluator actually runs (TODO Phase 3).
//!
//! The evaluator does not execute the admin's source text. It renders
//! JavaScript *from the lowered [`Ast`]* with the handful of rewrites that make
//! raw JavaScript agree with the semantics Phase 2's translator specified —
//! which is what turns "the two evaluators agree" into a provable property.
//! The rewrites:
//!
//! - **Loose equality becomes strict**: `==`/`!=` render as `===`/`!==`. The
//!   coercion table is not part of the formula language.
//! - **Ordered comparisons and arithmetic are null-guarded to `null`**:
//!   `a < b` renders as `((x, y) => x === null || y === null ? null : x < y)(a, b)`,
//!   because JS would coerce `null` to `0` (`null < 5` is `true`!) where SQL
//!   yields `NULL` and grants nothing. Guarding to `null` (not `false`) keeps
//!   the value identical to SQL's in value position too (`(a < b) === (c < d)`
//!   matches `IS NOT DISTINCT FROM` exactly). The IIFE evaluates operands once
//!   and adds no scope leakage.
//! - **An ambient object's members are null-safe**: `user.x` renders as
//!   `(user === null ? null : user.x)`, because `user` *is* null when nobody is
//!   logged in and raw JS would throw. Null in, null out — the same answer both
//!   `UserEnv`s give. `row`/`old` (decision 7) get the same guard, which is what
//!   makes `old.x` on an insert null rather than a thrown `TypeError`.
//! - **`!`, `&&`, `||`, `?:`, `??` render natively.** JS logic is two-valued
//!   over truthiness (with `null` falsy), and the translator was written to
//!   that spec: `!P` is SQL `P IS DISTINCT FROM TRUE`, `AND`/`OR`/`CASE`/
//!   `COALESCE` line up as they are. No guards needed — and native rendering
//!   means untranslatable formulas (the fallback path) keep ordinary JS
//!   short-circuit behaviour.
//!
//! Everything else renders as written. The renderer is a pure function of the
//! AST, unit-testable without V8 in the loop.

use std::collections::BTreeMap;

use crate::analyze::{Ambient, JOIN, hoisted_call_key};
use crate::ast::{Ast, BinaryOp, MemberProp, UnaryOp};

/// Render the normalised JavaScript for `ast`. The result is an expression
/// (always parenthesised at the top level where it matters), suitable for
/// embedding in the evaluator's script template.
///
/// Only the tests render an AST with nothing hoisted; every caller in the
/// evaluator goes through [`render_js_hoisted`] with the §4b bindings it
/// prefetched, so this is the tests' spelling of "and nothing was hoisted".
#[cfg(test)]
pub(crate) fn render_js(ast: &Ast) -> String {
    render_js_hoisted(ast, &BTreeMap::new())
}

/// The same rendering, with the **hoisted module calls** of §4b resolved.
///
/// `hoisted` maps a call's key ([`hoisted_call_key`]) to the identifier its
/// prefetched result is bound under, and a call node whose key is in it renders
/// as that identifier instead of as a call. That is the whole of the hoist on
/// this side: the evaluator sees a value and never knows there was a module —
/// no op, no surface, no node compatibility — exactly as it never knows there
/// was a Ⱶ-join query.
pub(crate) fn render_js_hoisted(ast: &Ast, hoisted: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut locals: Vec<String> = Vec::new();
    render(ast, &mut locals, hoisted, &mut out);
    out
}

fn render(
    ast: &Ast,
    locals: &mut Vec<String>,
    hoisted: &BTreeMap<String, String>,
    out: &mut String,
) {
    // A hoisted module call: its value was fetched before the formula started
    // and bound under a generated name, so what renders here is the name.
    if !hoisted.is_empty()
        && let Some(key) = hoisted_call_key(ast)
        && let Some(binding) = hoisted.get(&key)
    {
        out.push_str(binding);
        return;
    }
    match ast {
        Ast::Ident(name) => out.push_str(name),
        Ast::Str(s) => render_str(s, out),
        Ast::Num(n) => render_num(*n, out),
        Ast::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Ast::Null => out.push_str("null"),
        Ast::Member { obj, prop, .. } if free_ambient(obj, locals).is_some() => {
            // An ambient object (`user`/`row`/`old`) is object-or-null by
            // contract; the guard makes a null one read as null rather than
            // throw. The `?.` flag is subsumed.
            let name = free_ambient(obj, locals).map_or("user", Ambient::as_str);
            out.push('(');
            out.push_str(name);
            out.push_str(" === null ? null : ");
            out.push_str(name);
            match prop {
                MemberProp::Static(p) => {
                    out.push('.');
                    out.push_str(p);
                }
                MemberProp::Computed(e) => {
                    out.push_str("[__scKey(");
                    render(e, locals, hoisted, out);
                    out.push_str(")]");
                }
            }
            out.push(')');
        }
        // Member access on a `maxBy`/`minBy` result: the row is `null` when the
        // relation is empty, and reading a field must then yield `null` (SQL's
        // empty-subquery `NULL`), not JS `?.`'s `undefined` — otherwise
        // `.field === null` would disagree with the symbolic side. A single-eval
        // IIFE folds a missing row to `null`.
        Ast::Member { obj, prop, .. } if is_ordered_selection_call(obj) => {
            out.push_str("((__o) => __o == null ? null : __o");
            match prop {
                MemberProp::Static(p) => {
                    out.push('.');
                    out.push_str(p);
                }
                MemberProp::Computed(e) => {
                    out.push_str("[__scKey(");
                    render(e, locals, hoisted, out);
                    out.push_str(")]");
                }
            }
            out.push_str(")(");
            render(obj, locals, hoisted, out);
            out.push(')');
        }
        Ast::Member {
            obj,
            prop,
            optional,
        } => {
            render(obj, locals, hoisted, out);
            match prop {
                MemberProp::Static(p) => {
                    out.push_str(if *optional { "?." } else { "." });
                    out.push_str(p);
                }
                MemberProp::Computed(e) => {
                    // Through the isolate's key guard (`eval::FREEZE_PRELUDE`):
                    // a key computed from data may not name `constructor` and
                    // its kin, which a static one is refused at parse time.
                    out.push_str(if *optional {
                        "?.[__scKey("
                    } else {
                        "[__scKey("
                    });
                    render(e, locals, hoisted, out);
                    out.push_str(")]");
                }
            }
        }
        Ast::Call {
            callee,
            args,
            optional,
        } => {
            render(callee, locals, hoisted, out);
            out.push_str(if *optional { "?.(" } else { "(" });
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                render(a, locals, hoisted, out);
            }
            out.push(')');
        }
        Ast::NewDate(args) => {
            out.push_str("new Date(");
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                render(a, locals, hoisted, out);
            }
            out.push(')');
        }
        Ast::Unary { op, expr } => match op {
            // `!` is native: JS two-valued logic is the specified semantics
            // (the translator's `IS DISTINCT FROM TRUE` side of the bargain).
            UnaryOp::Not => wrap_unary("!", expr, locals, hoisted, out),
            // Negation is arithmetic: null-guarded like the binary operators.
            UnaryOp::Neg => {
                out.push_str("((x => x === null ? null : -x)(");
                render(expr, locals, hoisted, out);
                out.push_str("))");
            }
            UnaryOp::Pos => wrap_unary("+", expr, locals, hoisted, out),
            UnaryOp::TypeOf => wrap_unary("typeof ", expr, locals, hoisted, out),
        },
        Ast::Binary { op, l, r } => match op {
            BinaryOp::Eq | BinaryOp::StrictEq => infix("===", l, r, locals, hoisted, out),
            BinaryOp::NotEq | BinaryOp::StrictNotEq => infix("!==", l, r, locals, hoisted, out),
            BinaryOp::And => infix("&&", l, r, locals, hoisted, out),
            BinaryOp::Or => infix("||", l, r, locals, hoisted, out),
            BinaryOp::Nullish => infix("??", l, r, locals, hoisted, out),
            BinaryOp::Lt => guarded("<", l, r, locals, hoisted, out),
            BinaryOp::LtEq => guarded("<=", l, r, locals, hoisted, out),
            BinaryOp::Gt => guarded(">", l, r, locals, hoisted, out),
            BinaryOp::GtEq => guarded(">=", l, r, locals, hoisted, out),
            BinaryOp::Add => guarded("+", l, r, locals, hoisted, out),
            BinaryOp::Sub => guarded("-", l, r, locals, hoisted, out),
            BinaryOp::Mul => guarded("*", l, r, locals, hoisted, out),
            BinaryOp::Div => guarded("/", l, r, locals, hoisted, out),
            BinaryOp::Mod => guarded("%", l, r, locals, hoisted, out),
        },
        Ast::Cond { test, cons, alt } => {
            out.push('(');
            render(test, locals, hoisted, out);
            out.push_str(" ? ");
            render(cons, locals, hoisted, out);
            out.push_str(" : ");
            render(alt, locals, hoisted, out);
            out.push(')');
        }
        Ast::Array(elems) => {
            out.push('[');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                render(e, locals, hoisted, out);
            }
            out.push(']');
        }
        Ast::Template { quasis, exprs } => {
            out.push('`');
            for (i, quasi) in quasis.iter().enumerate() {
                render_quasi(quasi, out);
                if let Some(e) = exprs.get(i) {
                    out.push_str("${");
                    render(e, locals, hoisted, out);
                    out.push('}');
                }
            }
            out.push('`');
        }
        Ast::Arrow { params, body } => {
            out.push_str("((");
            out.push_str(&params.join(", "));
            out.push_str(") => ");
            let depth = locals.len();
            locals.extend(params.iter().cloned());
            render(body, locals, hoisted, out);
            locals.truncate(depth);
            out.push(')');
        }
    }
}

/// A native unary operator, parenthesised.
fn wrap_unary(
    op: &str,
    expr: &Ast,
    locals: &mut Vec<String>,
    hoisted: &BTreeMap<String, String>,
    out: &mut String,
) {
    out.push('(');
    out.push_str(op);
    out.push('(');
    render(expr, locals, hoisted, out);
    out.push_str("))");
}

/// A native infix operator, parenthesised.
fn infix(
    op: &str,
    l: &Ast,
    r: &Ast,
    locals: &mut Vec<String>,
    hoisted: &BTreeMap<String, String>,
    out: &mut String,
) {
    out.push('(');
    render(l, locals, hoisted, out);
    out.push(' ');
    out.push_str(op);
    out.push(' ');
    render(r, locals, hoisted, out);
    out.push(')');
}

/// A null-guarded binary operator: SQL's null-propagation for ordered
/// comparison and arithmetic, in JS. The IIFE evaluates each operand exactly
/// once (a textual guard would re-render them and blow up nested expressions
/// exponentially).
fn guarded(
    op: &str,
    l: &Ast,
    r: &Ast,
    locals: &mut Vec<String>,
    hoisted: &BTreeMap<String, String>,
    out: &mut String,
) {
    out.push_str("(((x, y) => x === null || y === null ? null : x ");
    out.push_str(op);
    out.push_str(" y)(");
    render(l, locals, hoisted, out);
    out.push_str(", ");
    render(r, locals, hoisted, out);
    out.push_str("))");
}

/// The ambient object this expression names, unshadowed by an arrow parameter.
///
/// Every ambient object gets the same null guard, and it is applied on syntax
/// alone: the normaliser has no shape, so it cannot know whether `row` is in
/// scope here. Guarding one that is not costs nothing — a *field* called `row`
/// is a scalar, and `(row === null ? null : row.x)` is the same `null` its
/// member access would have produced anyway.
fn free_ambient(ast: &Ast, locals: &[String]) -> Option<Ambient> {
    match ast {
        Ast::Ident(name) if !locals.iter().any(|l| l == name) => Ambient::from_ident(name),
        _ => None,
    }
}

/// Whether `ast` is a `…​.maxBy(…)` / `…​.minBy(…)` call — whose result is a
/// child row or `null`, so any member access on it must optional-chain.
fn is_ordered_selection_call(ast: &Ast) -> bool {
    matches!(
        ast,
        Ast::Call { callee, .. }
            if matches!(&**callee, Ast::Member { prop: MemberProp::Static(m), .. }
                if m == "maxBy" || m == "minBy")
    )
}

/// A string literal, JSON-escaped — JSON string syntax is valid JS.
fn render_str(s: &str, out: &mut String) {
    match serde_json::to_string(s) {
        Ok(quoted) => out.push_str(&quoted),
        // serde_json cannot fail on a plain string; the fallback keeps the
        // renderer total without an unwrap.
        Err(_) => out.push_str("null"),
    }
}

/// A number literal. Integral values in the exact range print without a
/// decimal point; Rust's shortest-round-trip `Display` covers the rest.
fn render_num(n: f64, out: &mut String) {
    const MAX_EXACT_INT: f64 = 9_007_199_254_740_992.0; // 2^53
    if n.fract() == 0.0 && n.abs() <= MAX_EXACT_INT {
        out.push_str(&format!("{}", n as i64));
    } else {
        out.push_str(&format!("{n}"));
    }
}

/// A template quasi: escape backslashes, backticks and `${` so the cooked text
/// round-trips through a fresh template literal.
fn render_quasi(quasi: &str, out: &mut String) {
    let escaped = quasi
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${");
    out.push_str(&escaped);
}

/// True when `name` contains the Ⱶ join character (bound as a prefetched value
/// by the evaluator). Used by the eval module's binding assembly.
pub(crate) fn is_join_ident(name: &str) -> bool {
    name.contains(JOIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::Formula;

    fn js(src: &str) -> String {
        render_js(Formula::parse(src).unwrap().ast())
    }

    #[test]
    fn new_date_renders_as_itself() {
        assert_eq!(js("new Date()"), "new Date()");
        assert_eq!(js("new Date"), "new Date()");
        assert_eq!(js("new Date(start)"), "new Date(start)");
    }

    #[test]
    fn loose_equality_renders_strict() {
        assert_eq!(js("pages == 100"), "(pages === 100)");
        assert_eq!(js("pages != 100"), "(pages !== 100)");
    }

    #[test]
    fn ordered_comparisons_are_null_guarded_to_null() {
        assert_eq!(
            js("pages < 100"),
            "(((x, y) => x === null || y === null ? null : x < y)(pages, 100))"
        );
    }

    #[test]
    fn arithmetic_is_null_guarded() {
        assert_eq!(
            js("pages % 2 === 0"),
            "((((x, y) => x === null || y === null ? null : x % y)(pages, 2)) === 0)"
        );
        assert_eq!(
            js("-pages === 1"),
            "(((x => x === null ? null : -x)(pages)) === 1)"
        );
    }

    #[test]
    fn user_access_is_null_safe() {
        assert_eq!(
            js("owner === user.id"),
            "(owner === (user === null ? null : user.id))"
        );
        // `?.` collapses into the same guard.
        assert_eq!(
            js("owner === user?.id"),
            "(owner === (user === null ? null : user.id))"
        );
        // Computed access takes the guard too.
        assert_eq!(
            js("owner === user[title]"),
            "(owner === (user === null ? null : user[__scKey(title)]))"
        );
    }

    #[test]
    fn logic_and_conditionals_render_natively() {
        assert_eq!(
            js("user && owner === user.id"),
            "(user && (owner === (user === null ? null : user.id)))"
        );
        assert_eq!(js("!(owner === null)"), "(!((owner === null)))");
        assert_eq!(js("a ? b === 1 : c === 2"), "(a ? (b === 1) : (c === 2))");
        assert_eq!(js("title ?? 'anon'"), "(title ?? \"anon\")");
    }

    #[test]
    fn an_arrow_parameter_shadows_the_user_guard() {
        // Inside the arrow, `user` is the parameter, not the user object.
        assert_eq!(
            js("groups.some(user => user === 1)"),
            "groups.some(((user) => (user === 1)))"
        );
    }

    #[test]
    fn strings_numbers_and_templates_escape() {
        assert_eq!(js("title === 'a\"b'"), "(title === \"a\\\"b\")");
        assert_eq!(js("pages === 3.5"), "(pages === 3.5)");
        assert_eq!(js("pages === 3"), "(pages === 3)");
        assert_eq!(js("`x${title}` === title"), "(`x${title}` === title)");
        // Backticks and `${` in the source template survive escaped.
        let rendered = js("`a\\`b` === title");
        assert!(rendered.contains("\\`"), "got: {rendered}");
    }

    #[test]
    fn join_identifiers_render_verbatim() {
        assert_eq!(
            js("publisherⱵname === 'ACME'"),
            "(publisherⱵname === \"ACME\")"
        );
    }

    #[test]
    fn relation_chains_render_natively_for_the_prelude() {
        // The relation identifier and the curated methods render verbatim; the
        // prelude on Array.prototype supplies the invented ones.
        assert_eq!(
            js("linesↃorder.sum(\"qty\") === 5"),
            "(linesↃorder.sum(\"qty\") === 5)"
        );
        assert_eq!(
            js("linesↃorder.some(r => r.ok)"),
            "linesↃorder.some(((r) => r.ok))"
        );
    }

    #[test]
    fn maxby_member_access_is_folded_to_null_not_undefined() {
        // `.status` on a `maxBy` result reads null when the relation is empty
        // (an IIFE guard), matching SQL's empty-subquery NULL rather than JS
        // optional chaining's `undefined`.
        assert_eq!(
            js("linesↃorder.maxBy(\"qty\").status === null"),
            "(((__o) => __o == null ? null : __o.status)(linesↃorder.maxBy(\"qty\")) === null)"
        );
    }
}
