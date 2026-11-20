//! A notebook's JavaScript **cell**, rewritten into the code body that runs it
//! (analytics TODO A10.3–A10.4).
//!
//! A cell is written as a Jupyter cell is: its top-level declarations are seen
//! by the cells after it, its last expression is its output, and an error says
//! which line it came from. A code body is none of those — it is the inside of
//! an `async function`, whose declarations end with it — so the cell is
//! rewritten, by span, leaving every character the author wrote where it was:
//!
//! - each top-level declaration (`const`, `let`, `var`, a `function`, a
//!   `class`, destructuring included) is copied onto `globalThis` when the cell
//!   ends, which is where the next cell finds it. A notebook's session runs on
//!   an isolate of its own, so its `globalThis` is its own;
//! - a marker before each top-level statement records its line, so a throw is
//!   reported with the line of the statement it came from — V8's own stack
//!   does not survive the trip;
//! - the last statement, when it is an expression, is the cell's value.
//!
//! What the cell is run *with* is the notebook's business, not this crate's:
//! the rewritten body expects `nb` to be made by `globalThis.__nbMake(db)`,
//! and to answer `__console`, `__finish(value)` and `__fail(error, line)`.
//! The whole cell runs inside one block, so `console` there is the notebook's,
//! whose lines become outputs in the order they were printed.

use sc_error::{Error, Result};
use swc_common::input::StringInput;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as swc;
use swc_ecma_ast::{EsVersion, Stmt};
use swc_ecma_parser::lexer::Lexer;
use swc_ecma_parser::{EsSyntax, Parser, Syntax};

/// What the cell is parsed inside: the function a code body is.
const OPEN: &str = "(async function () {\n";
const CLOSE: &str = "\n})";

/// The code body that runs one notebook cell, made from its source.
///
/// A syntax error is refused here, with the cell's own line: the body is
/// never sent to the engine.
pub fn notebook_cell(source: &str) -> Result<String> {
    let wrapped = format!("{OPEN}{source}{CLOSE}");
    let end = u32::try_from(wrapped.len())
        .map_err(|_| Error::invalid("a cell this long cannot be read"))?;
    let input = StringInput::new(&wrapped, BytePos(0), BytePos(end));
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::latest(),
        input,
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let invalid = |e: &swc_ecma_parser::error::Error| {
        let at = (e.span().lo.0 as usize).saturating_sub(OPEN.len());
        Error::invalid(format!("line {}: {}", line_of(source, at), e.kind().msg()))
    };
    let script = parser.parse_script().map_err(|e| invalid(&e))?;
    if let Some(e) = parser.take_errors().into_iter().next() {
        return Err(invalid(&e));
    }
    let stmts = body_of(script)
        .ok_or_else(|| Error::invalid("a cell must be statements, not the end of a function"))?;

    let offset = |pos: BytePos| (pos.0 as usize).saturating_sub(OPEN.len());
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let last = stmts.len().checked_sub(1);
    for (i, stmt) in stmts.iter().enumerate() {
        let span = stmt.span();
        let (lo, hi) = (offset(span.lo), offset(span.hi));
        let marker = format!(";__nbAt = {};", line_of(source, lo));
        if Some(i) == last
            && let Stmt::Expr(e) = stmt
        {
            let expr = &source[offset(e.expr.span().lo)..offset(e.expr.span().hi)];
            edits.push((lo, hi, format!("{marker} __nbValue = ({expr});")));
            continue;
        }
        edits.push((lo, lo, marker));
        declared(stmt, &mut names);
    }

    let mut code = source.to_owned();
    edits.sort_by_key(|(lo, _, _)| std::cmp::Reverse(*lo));
    for (lo, hi, text) in edits {
        code.replace_range(lo..hi, &text);
    }
    names.retain(|n| !n.starts_with("__nb"));
    names.dedup();
    let exports: String = names
        .iter()
        .map(|n| format!("globalThis.{n} = {n};"))
        .collect();
    // On one line, so the author's first line is the body's second and the
    // markers — not V8 — are what name a line anyway.
    Ok(format!(
        "const nb = globalThis.__nbMake(db); let __nbAt = 1, __nbValue; try {{ \
         const console = nb.__console;\n{code}\n;{exports}\n}} catch (__nbE) {{ \
         return nb.__fail(__nbE, __nbAt); }}\nreturn nb.__finish(__nbValue, __nbAt);"
    ))
}

/// The function's statements, out of the script the cell was parsed as.
fn body_of(script: swc::Script) -> Option<Vec<Stmt>> {
    let mut body = script.body;
    let (Some(Stmt::Expr(stmt)), true) = (body.pop(), body.is_empty()) else {
        return None;
    };
    let swc::Expr::Paren(paren) = *stmt.expr else {
        return None;
    };
    let swc::Expr::Fn(function) = *paren.expr else {
        return None;
    };
    function.function.body.map(|b| b.stmts)
}

/// The 1-based line of byte `at` of `source`.
fn line_of(source: &str, at: usize) -> usize {
    source
        .get(..at.min(source.len()))
        .map_or(0, |s| s.matches('\n').count())
        + 1
}

/// The names a top-level statement declares.
fn declared(stmt: &Stmt, out: &mut Vec<String>) {
    let Stmt::Decl(decl) = stmt else {
        return;
    };
    match decl {
        swc::Decl::Var(var) => {
            for d in &var.decls {
                bound(&d.name, out);
            }
        }
        swc::Decl::Fn(f) => out.push(f.ident.sym.to_string()),
        swc::Decl::Class(c) => out.push(c.ident.sym.to_string()),
        _ => {}
    }
}

/// The names a binding pattern binds.
fn bound(pat: &swc::Pat, out: &mut Vec<String>) {
    match pat {
        swc::Pat::Ident(ident) => out.push(ident.id.sym.to_string()),
        swc::Pat::Array(array) => {
            for elem in array.elems.iter().flatten() {
                bound(elem, out);
            }
        }
        swc::Pat::Rest(rest) => bound(&rest.arg, out),
        swc::Pat::Assign(assign) => bound(&assign.left, out),
        swc::Pat::Object(object) => {
            for prop in &object.props {
                match prop {
                    swc::ObjectPatProp::KeyValue(kv) => bound(&kv.value, out),
                    swc::ObjectPatProp::Assign(a) => out.push(a.key.id.sym.to_string()),
                    swc::ObjectPatProp::Rest(rest) => bound(&rest.arg, out),
                }
            }
        }
        swc::Pat::Invalid(_) | swc::Pat::Expr(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_kept_for_the_next_cell_and_the_last_expression_is_the_value() {
        let code = notebook_cell(
            "const d = 1;\nlet { a, b: [c, ...e] } = x;\nfunction f() {}\nclass K {}\nd + 1",
        )
        .unwrap();
        for name in ["d", "a", "c", "e", "f", "K"] {
            assert!(
                code.contains(&format!("globalThis.{name} = {name};")),
                "{name}: {code}"
            );
        }
        assert!(!code.contains("globalThis.b ="), "{code}");
        assert!(code.contains(";__nbAt = 5; __nbValue = (d + 1);"), "{code}");
        assert!(code.contains(";__nbAt = 2;let {"), "{code}");
    }

    #[test]
    fn a_cell_ending_in_a_statement_has_no_value_and_await_is_allowed() {
        let code = notebook_cell("const t = await nb.table(d);\nif (t) { t; }").unwrap();
        assert!(!code.contains("__nbValue = ("), "{code}");
        assert!(code.contains("globalThis.t = t;"), "{code}");
    }

    #[test]
    fn a_syntax_error_names_the_cells_own_line() {
        let err = notebook_cell("const a = 1;\n\nconst b = = 2;").unwrap_err();
        assert!(err.to_string().contains("line 3"), "{err}");
    }

    #[test]
    fn a_comment_on_the_last_line_does_not_swallow_the_end() {
        let code = notebook_cell("1 + 2 // three").unwrap();
        assert!(code.contains("__nbValue = (1 + 2);"), "{code}");
    }
}
