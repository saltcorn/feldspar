//! Parsing a formula's source into a [`Formula`].
//!
//! Parsing runs swc over the source, requires the result to be a **single
//! expression statement** (so trailing statements — `a === b; somethingElse()`
//! — are refused rather than silently ignored), lowers the swc tree into the
//! crate's own [`Ast`], and collects the free variables once. The swc types are
//! gone by the time this function returns.

use sc_error::{Error, Result};
use swc_common::input::StringInput;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast::{EsVersion, Stmt};
use swc_ecma_parser::error::Error as SwcError;
use swc_ecma_parser::lexer::Lexer;
use swc_ecma_parser::{EsSyntax, Parser, Syntax};

use crate::analyze::{FreeVars, collect_free_vars};
use crate::ast::{Ast, lower};

/// A parsed ownership (or, later, calculated-field) formula: the source it came
/// from, the lowered [`Ast`] both evaluators consume, and its free variables.
///
/// Owned data throughout — a `Formula` lives on cached catalog entries shared
/// across threads.
#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    source: String,
    ast: Ast,
    free: FreeVars,
}

impl Formula {
    /// Parse `source` as a single JavaScript expression.
    ///
    /// Errors are [`Invalid`](sc_error::Repr::Invalid) — an admin typed this in
    /// the table editor — and carry the 1-based line and column of the failure.
    pub fn parse(source: &str) -> Result<Formula> {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return Err(Error::invalid("a formula must not be empty"));
        }
        let input = StringInput::new(source, BytePos(0), BytePos(source.len() as u32));
        let lexer = Lexer::new(
            Syntax::Es(EsSyntax::default()),
            EsVersion::latest(),
            input,
            None,
        );
        let mut parser = Parser::new_from(lexer);
        let script = parser.parse_script().map_err(|e| parse_error(source, &e))?;
        // swc recovers from some syntax errors to keep parsing; a recovered
        // error is still an error here (principle 5 — no silent failures).
        if let Some(e) = parser.take_errors().into_iter().next() {
            return Err(parse_error(source, &e));
        }
        let mut body = script.body;
        if body.len() != 1 {
            return Err(Error::invalid(
                "a formula must be a single expression, not multiple statements",
            ));
        }
        let expr = match body.pop() {
            Some(Stmt::Expr(stmt)) => stmt.expr,
            _ => {
                return Err(Error::invalid(
                    "a formula must be a single expression, not a statement",
                ));
            }
        };
        let ast = lower(&expr)?;
        let free = collect_free_vars(&ast);
        Ok(Formula {
            source: source.to_string(),
            ast,
            free,
        })
    }

    /// The source text this formula was parsed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The lowered expression tree.
    pub fn ast(&self) -> &Ast {
        &self.ast
    }

    /// The formula's free variables, collected at parse time.
    pub fn free_vars(&self) -> &FreeVars {
        &self.free
    }
}

/// Render a swc parse error with a 1-based line/column computed from the source
/// (swc's own positions are byte offsets into the input we handed it).
fn parse_error(source: &str, e: &SwcError) -> Error {
    let offset = (e.span().lo.0 as usize).min(source.len());
    let (line, column) = position(source, offset);
    Error::invalid(format!(
        "formula parse error at line {line}, column {column}: {}",
        e.kind().msg()
    ))
}

/// 1-based (line, column) of a byte offset. Column counts characters, not
/// bytes — the message points at what the admin sees, not at UTF-8 internals.
fn position(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let col_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = source[col_start..offset].chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, MemberProp};

    #[test]
    fn parses_a_comparison() {
        let f = Formula::parse("owner === user.id").unwrap();
        let Ast::Binary { op, l, r } = f.ast() else {
            panic!("expected a binary node, got {:?}", f.ast());
        };
        assert_eq!(*op, BinaryOp::StrictEq);
        assert_eq!(**l, Ast::Ident("owner".into()));
        let Ast::Member {
            obj,
            prop,
            optional,
        } = &**r
        else {
            panic!("expected member access, got {r:?}");
        };
        assert_eq!(**obj, Ast::Ident("user".into()));
        assert_eq!(*prop, MemberProp::Static("id".into()));
        assert!(!optional);
    }

    #[test]
    fn a_half_h_join_path_is_one_identifier() {
        // The design premise stated in the TODO, tested against the real
        // parser: Ⱶ (U+2C75, category Lu) is an identifier character, so a join
        // path lexes as a single identifier — no preprocessing anywhere.
        let f = Formula::parse("publisherⱵname").unwrap();
        assert_eq!(*f.ast(), Ast::Ident("publisherⱵname".into()));
        // Chained to any depth, still one identifier.
        let f = Formula::parse("publisherⱵcountryⱵname === 'DK'").unwrap();
        let Ast::Binary { l, .. } = f.ast() else {
            panic!("expected a binary node");
        };
        assert_eq!(**l, Ast::Ident("publisherⱵcountryⱵname".into()));
    }

    #[test]
    fn parse_errors_carry_line_and_column() {
        let err = Formula::parse("owner ===").unwrap_err().to_string();
        assert!(err.contains("line 1"), "got: {err}");
        // A multi-line formula reports the failing line, not line 1.
        let err = Formula::parse("owner ===\n  user.id &&&").unwrap_err();
        assert!(err.to_string().contains("line 2"), "got: {err}");
    }

    #[test]
    fn multiple_statements_are_refused() {
        let err = Formula::parse("a === b; c()").unwrap_err().to_string();
        assert!(err.contains("single expression"), "got: {err}");
    }

    #[test]
    fn non_expression_statements_are_refused() {
        for src in ["if (a) b", "return a", "let x = 1"] {
            let err = Formula::parse(src).unwrap_err();
            assert!(
                matches!(err.repr(), sc_error::Repr::Invalid(_)),
                "{src}: {err}"
            );
        }
    }

    #[test]
    fn effectful_constructs_are_refused_by_name() {
        for (src, named) in [
            ("owner = user.id", "assignment"),
            ("counter++", "increment"),
            ("new Map()", "`new`"),
            ("new globalThis.Date()", "`new`"),
            ("this.owner", "`this`"),
            ("(a, b)", "comma"),
            ("[...groups]", "spread"),
            ("owner in user", "`in`"),
            ("~owner", "bitwise"),
            ("owner ** 2", "exponentiation"),
        ] {
            let err = Formula::parse(src).unwrap_err().to_string();
            assert!(err.contains(named), "{src}: expected `{named}` in: {err}");
        }
    }

    #[test]
    fn the_empty_formula_is_refused() {
        for src in ["", "   ", "\n"] {
            assert!(Formula::parse(src).is_err(), "{src:?} should be refused");
        }
    }

    #[test]
    fn optional_chaining_lowers_with_the_flag() {
        let f = Formula::parse("user?.id === owner").unwrap();
        let Ast::Binary { l, .. } = f.ast() else {
            panic!("expected a binary node");
        };
        let Ast::Member { optional, .. } = &**l else {
            panic!("expected member access, got {l:?}");
        };
        assert!(*optional);
    }

    #[test]
    fn arrows_bind_simple_parameters_only() {
        assert!(Formula::parse("groups.some(g => g === dept)").is_ok());
        let err = Formula::parse("groups.some(({ g }) => g)").unwrap_err();
        assert!(err.to_string().contains("destructuring"), "got: {err}");
        let err = Formula::parse("groups.some(g => { return g })").unwrap_err();
        assert!(err.to_string().contains("block body"), "got: {err}");
    }

    #[test]
    fn a_formula_is_send_and_sync() {
        // A Formula lives on cached catalog entries shared across threads; the
        // lowered AST must not smuggle swc's interned types along.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Formula>();
    }
}
