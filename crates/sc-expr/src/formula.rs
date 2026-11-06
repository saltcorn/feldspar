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
use crate::ast::{Ast, MAX_FORMULA_DEPTH, lower};

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

/// The longest formula source [`Formula::parse`] accepts, in bytes. Row data
/// can reach the parser (a `{{= }}` template over a user-written field), so
/// the source is bounded before swc builds a tree of it (security review
/// 2026-11, D1).
pub const MAX_FORMULA_BYTES: usize = 64 * 1024;

/// Stack one nesting level may take while swc parses it and the tree is
/// lowered and dropped. swc's parser recurses through a dozen functions per
/// parenthesis, and a debug build's frames are large: about 10 KiB a level.
const STACK_PER_LEVEL: usize = 32 * 1024;

/// Stack per byte of source for the levels [`nesting_bound`] does not count
/// (binary operators, `?:`, member chains: a loop in swc's parser, but a
/// level of the tree that lowering and `Drop` recurse through), and for
/// lowering the tree up to [`MAX_FORMULA_DEPTH`].
const STACK_PER_BYTE: usize = 2 * 1024;

impl Formula {
    /// Parse `source` as a single JavaScript expression.
    ///
    /// Errors are [`Invalid`](sc_error::Repr::Invalid) — an admin typed this in
    /// the table editor — and carry the 1-based line and column of the failure.
    /// A source longer than [`MAX_FORMULA_BYTES`] or nested deeper than
    /// [`MAX_FORMULA_DEPTH`] is refused.
    pub fn parse(source: &str) -> Result<Formula> {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return Err(Error::invalid("a formula must not be empty"));
        }
        if source.len() > MAX_FORMULA_BYTES {
            return Err(Error::invalid(format!(
                "a formula may be at most {MAX_FORMULA_BYTES} bytes long"
            )));
        }
        // swc's parser recurses once per nesting level, as do lowering and
        // dropping its tree, so the depth is bounded before swc sees the
        // source (iteratively, see `nesting_bound`), and the parse runs on a
        // stack big enough for that depth. The lowered `Ast` is bounded by
        // the same limit (`lower` counts), so nothing deep outlives this call.
        let depth = nesting_bound(source);
        if depth > MAX_FORMULA_DEPTH {
            return Err(too_deep());
        }
        let need = depth * STACK_PER_LEVEL + source.len() * STACK_PER_BYTE + 512 * 1024;
        stacker::maybe_grow(need, need, || Self::parse_bounded(source))
    }

    fn parse_bounded(source: &str) -> Result<Formula> {
        // `parse` has already refused anything over `MAX_FORMULA_BYTES`.
        let end = u32::try_from(source.len()).map_err(|_| {
            Error::invalid(format!(
                "a formula may be at most {MAX_FORMULA_BYTES} bytes long"
            ))
        })?;
        let input = StringInput::new(source, BytePos(0), BytePos(end));
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

    /// `self || other`: a formula that grants whatever either one grants.
    ///
    /// Built from the two trees rather than by parsing their sources joined,
    /// so it costs a clone and cannot fail. The source reads
    /// `(self) || (other)`, which parses back to the same tree.
    pub fn or(&self, other: &Formula) -> Formula {
        let ast = Ast::Binary {
            op: crate::ast::BinaryOp::Or,
            l: Box::new(self.ast.clone()),
            r: Box::new(other.ast.clone()),
        };
        let free = collect_free_vars(&ast);
        Formula {
            source: format!("({}) || ({})", self.source, other.source),
            ast,
            free,
        }
    }
}

/// The error for a formula nested deeper than [`MAX_FORMULA_DEPTH`].
pub(crate) fn too_deep() -> Error {
    Error::invalid(format!(
        "a formula may not be nested more than {MAX_FORMULA_DEPTH} levels deep"
    ))
}

/// An upper bound on how deeply swc's parser will recurse over `source`,
/// found without recursion: open brackets (`(`, `[`, `{`, a template's
/// `${`) plus prefix operators (`!`, `-`, `typeof`, …) and arrows, each counted
/// until the bracket it sits in closes. Strings, template text, comments and
/// regular expression literals are skipped as JavaScript's lexer skips them,
/// so a bracket inside one cannot close a real one.
///
/// Binary operators, `?:` and member chains are not counted: swc parses those
/// in a loop, and `lower` counts the depth of the tree they build.
#[allow(clippy::too_many_lines)]
fn nesting_bound(source: &str) -> usize {
    let b = source.as_bytes();
    // Per open bracket: whether it is a template's `${`, and how many prefix
    // operators were counted inside it.
    let mut stack: Vec<(bool, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut max = 0usize;
    // After an operand, `/` divides and `-` is binary; otherwise they start
    // a regular expression and a prefix operator.
    let mut after_operand = false;
    let mut line_start = true;
    let mut i = 0;
    // Skip a template's text from `i` (just past a backtick or a `}` that
    // closed a `${`); true when it stopped at a `${`.
    let template_text = |i: &mut usize| -> bool {
        while *i < b.len() {
            match b[*i] {
                b'\\' => *i += 2,
                b'`' => {
                    *i += 1;
                    return false;
                }
                b'$' if b.get(*i + 1) == Some(&b'{') => {
                    *i += 2;
                    return true;
                }
                _ => *i += 1,
            }
        }
        false
    };
    // A prefix operator stays counted until its bracket closes.
    let count_prefix = |stack: &mut Vec<(bool, usize)>, depth: &mut usize| {
        if let Some((_, n)) = stack.last_mut() {
            *n += 1;
        }
        *depth += 1;
    };
    while i < b.len() {
        let c = b[i];
        let was_line_start = line_start;
        line_start = false;
        match c {
            b'\n' | b'\r' => {
                line_start = true;
                i += 1;
            }
            b' ' | b'\t' | 0x0b | 0x0c => {
                line_start = was_line_start;
                i += 1;
            }
            b'\'' | b'"' => {
                i += 1;
                while i < b.len() && b[i] != c && b[i] != b'\n' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                after_operand = true;
            }
            b'`' => {
                i += 1;
                if template_text(&mut i) {
                    stack.push((true, 0));
                    depth += 1;
                    after_operand = false;
                } else {
                    after_operand = true;
                }
            }
            // Comments, including the HTML-like ones a script allows.
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'<' if b[i..].starts_with(b"<!--") => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'-' if was_line_start && b[i..].starts_with(b"-->") => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !b[i..].starts_with(b"*/") {
                    line_start |= b[i] == b'\n';
                    i += 1;
                }
                i += 2;
                line_start |= was_line_start;
            }
            // A regular expression literal: up to an unescaped `/` outside a
            // character class.
            b'/' if !after_operand => {
                i += 1;
                let mut class = false;
                while i < b.len() && b[i] != b'\n' {
                    match b[i] {
                        b'\\' => i += 1,
                        b'[' => class = true,
                        b']' => class = false,
                        b'/' if !class => break,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                after_operand = true;
            }
            b'(' | b'[' | b'{' => {
                stack.push((false, 0));
                depth += 1;
                after_operand = false;
                i += 1;
            }
            b')' | b']' | b'}' => {
                i += 1;
                after_operand = true;
                if let Some((template, n)) = stack.pop() {
                    depth -= 1 + n;
                    if template && template_text(&mut i) {
                        stack.push((true, 0));
                        depth += 1;
                        after_operand = false;
                    }
                }
            }
            b'=' if b.get(i + 1) == Some(&b'>') => {
                count_prefix(&mut stack, &mut depth);
                after_operand = false;
                i += 2;
            }
            b'!' | b'-' | b'+' | b'~' if !after_operand => {
                count_prefix(&mut stack, &mut depth);
                i += 1;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'\\' || c >= 0x80 => {
                let start = i;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || matches!(b[i], b'_' | b'$' | b'\\')
                        || b[i] >= 0x80)
                {
                    i += 1;
                }
                match &b[start..i] {
                    b"typeof" | b"void" | b"delete" | b"new" | b"await" | b"yield" => {
                        count_prefix(&mut stack, &mut depth);
                        after_operand = false;
                    }
                    b"in" | b"instanceof" | b"of" if after_operand => after_operand = false,
                    _ => after_operand = true,
                }
            }
            _ => {
                // Any other punctuation: an operator, `.`, `,`, `?`, `:`.
                after_operand = false;
                i += 1;
            }
        }
        max = max.max(depth);
    }
    max
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
    fn or_is_the_parse_of_its_source() {
        let a = Formula::parse("_read && id === user.id").unwrap();
        let b = Formula::parse("manager === user.id").unwrap();
        let both = a.or(&b);
        assert_eq!(
            both.source(),
            "(_read && id === user.id) || (manager === user.id)"
        );
        let reparsed = Formula::parse(both.source()).unwrap();
        assert_eq!(both.ast(), reparsed.ast());
        assert_eq!(both.free_vars(), reparsed.free_vars());
    }

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

    /// Row data can reach the parser, so an over-long or over-deep source is
    /// an error, never a stack overflow that aborts the process (security
    /// review 2026-11, D1). Run on a thread with the default 2 MiB stack.
    #[test]
    fn deep_or_long_formulas_are_refused_not_a_crash() {
        let on_small_stack = |src: String| {
            std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || Formula::parse(&src).map(|_| ()).map_err(|e| e.to_string()))
                .unwrap()
                .join()
                .expect("parsing must not crash the thread")
        };
        // 100k nested parentheses: over the length cap.
        let parens = format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000));
        let err = on_small_stack(parens).unwrap_err();
        assert!(err.contains("at most"), "{err}");
        // Under the length cap, but far deeper than a formula may nest.
        for deep in [
            format!("{}1{}", "(".repeat(30_000), ")".repeat(30_000)),
            format!("{}x", "!".repeat(60_000)),
            format!("1{}", "+1".repeat(30_000)),
            format!("{}0{}", "[".repeat(30_000), "]".repeat(30_000)),
            format!("{}1", "a ? b : ".repeat(8_000)),
        ] {
            let err = on_small_stack(deep).unwrap_err();
            assert!(err.contains("levels deep"), "{err}");
        }
        // The deepest formula allowed still parses.
        let ok = format!("1{}", " || 1".repeat(crate::MAX_FORMULA_DEPTH - 1));
        on_small_stack(ok).unwrap();
        let too_deep = format!("1{}", " || 1".repeat(crate::MAX_FORMULA_DEPTH));
        assert!(
            on_small_stack(too_deep)
                .unwrap_err()
                .contains("levels deep")
        );
    }

    /// The pre-scan skips what JavaScript's lexer skips, and nothing hidden
    /// in a regular expression, string or comment can close a real bracket
    /// (and so hide nesting from the bound).
    #[test]
    fn the_nesting_bound_is_an_upper_bound() {
        assert_eq!(nesting_bound("a === b"), 0);
        assert_eq!(nesting_bound("f(g(x))"), 2);
        assert_eq!(nesting_bound("!(-x)"), 3);
        assert_eq!(nesting_bound("a - b"), 0);
        assert_eq!(nesting_bound("'(((' + \"[[[\" + `{{{`"), 0);
        assert_eq!(nesting_bound("`a${ f(`b${ x }`) }`"), 3);
        assert_eq!(nesting_bound("a /* ((( */ + b // (((\n + c"), 0);
        assert_eq!(nesting_bound("x => y => x + y"), 2);
        // A closing bracket in a regular expression, a string, a comment or
        // template text does not pop an open one.
        let open = "(".repeat(300);
        for hidden in [
            "/)))))/",
            "/[/)]/",
            "')))'",
            "/* ))) */",
            "// )))\n",
            "<!-- )))\n",
            "`)))`",
        ] {
            let src = format!("{open}{hidden}{open}");
            assert!(
                nesting_bound(&src) >= 600,
                "{hidden}: {}",
                nesting_bound(&src)
            );
        }
    }

    #[test]
    fn a_formula_is_send_and_sync() {
        // A Formula lives on cached catalog entries shared across threads; the
        // lowered AST must not smuggle swc's interned types along.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Formula>();
    }
}
