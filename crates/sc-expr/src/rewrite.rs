//! Rewriting JavaScript expression **text** by its free references — what the
//! Saltcorn 1 importer carries v1's formulas across with.
//!
//! A v1 formula is JavaScript too, but it is read in a different scope: an
//! aggregation's `where` names the *child* row's fields bare (`gift === true`),
//! a workflow names its context variables bare (`orders`), and a calculated
//! field may reach through a key with a dot (`publisher.name`). Each of those has
//! one spelling here (`r.gift`, `context.step1`, `publisherⱵname`), and the
//! difference is only ever in the free references — so that is all this
//! rewrites, by span, leaving every other character the admin wrote as it was.
//!
//! Unlike [`Formula::parse`](crate::Formula::parse) this accepts **object
//! literals**, because a v1 row expression (`{discount: 0.15}`) and a v1 query
//! (`{id}`) are one, and [`object_literal_entries`] is how the importer takes
//! them apart into a field → formula map.

use sc_error::{Error, Result};
use swc_common::input::StringInput;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as swc;
use swc_ecma_ast::{EsVersion, Stmt};
use swc_ecma_parser::lexer::Lexer;
use swc_ecma_parser::{EsSyntax, Parser, Syntax};

/// What a rewrite callback answers for one free reference: how many of its
/// leading segments to replace, and with what text. `None` leaves it as written.
pub type Replacement = Option<(usize, String)>;

/// `source` with each free reference rewritten by `rewrite`.
///
/// A free reference is an identifier that no arrow parameter binds, together
/// with the static member accesses that follow it — `order.customer.name` is the
/// one reference `["order", "customer", "name"]`. The callback may replace any
/// leading part of it: answering `(1, "context.orders")` for `["orders",
/// "length"]` gives `context.orders.length`.
///
/// Object keys are not references; a shorthand property (`{ id }`) is one, and
/// is written out as `id: <replacement>` when it is replaced.
pub fn rewrite_free_refs(
    source: &str,
    rewrite: &mut dyn FnMut(&[String]) -> Replacement,
) -> Result<String> {
    let parsed = Parsed::new(source)?;
    let mut edits = Vec::new();
    walk(&parsed.expr, &mut Vec::new(), &mut edits, rewrite)?;
    Ok(parsed.apply(edits))
}

/// The entries of an object literal, each value as its source text: `{a: x + 1,
/// b}` is `[("a", "x + 1"), ("b", "b")]`.
///
/// Anything but a plain object of named properties — a spread, a method, a
/// computed key, or not an object at all — is an error naming it.
pub fn object_literal_entries(source: &str) -> Result<Vec<(String, String)>> {
    let parsed = Parsed::new(source)?;
    let swc::Expr::Object(object) = unparen(&parsed.expr) else {
        return Err(Error::invalid(format!(
            "`{}` is not an object literal",
            source.trim()
        )));
    };
    let mut out = Vec::new();
    for prop in &object.props {
        let swc::PropOrSpread::Prop(prop) = prop else {
            return Err(Error::invalid("an object literal with a spread (`...`)"));
        };
        match &**prop {
            swc::Prop::Shorthand(ident) => {
                out.push((ident.sym.to_string(), ident.sym.to_string()));
            }
            swc::Prop::KeyValue(kv) => {
                let key = match &kv.key {
                    swc::PropName::Ident(name) => name.sym.to_string(),
                    swc::PropName::Str(s) => s.value.to_string_lossy().into_owned(),
                    _ => return Err(Error::invalid("an object literal with a computed key")),
                };
                out.push((key, parsed.text(kv.value.span()).trim().to_owned()));
            }
            _ => {
                return Err(Error::invalid(
                    "an object literal with a method or accessor",
                ));
            }
        }
    }
    Ok(out)
}

/// One expression, parsed out of `(` source `)` — the parentheses are what let
/// an object literal be read as an expression rather than as a block.
struct Parsed {
    wrapped: String,
    expr: Box<swc::Expr>,
}

/// Where the source starts within [`Parsed::wrapped`].
const OPEN: usize = 1;

impl Parsed {
    fn new(source: &str) -> Result<Parsed> {
        if source.trim().is_empty() {
            return Err(Error::invalid("an expression must not be empty"));
        }
        // The newline keeps a trailing `// comment` from swallowing the `)`.
        let wrapped = format!("({source}\n)");
        let end = u32::try_from(wrapped.len())
            .map_err(|_| Error::invalid("an expression this long cannot be read"))?;
        let input = StringInput::new(&wrapped, BytePos(0), BytePos(end));
        let lexer = Lexer::new(
            Syntax::Es(EsSyntax::default()),
            EsVersion::latest(),
            input,
            None,
        );
        let mut parser = Parser::new_from(lexer);
        let invalid = |e: &swc_ecma_parser::error::Error| {
            Error::invalid(format!(
                "`{}` is not a JavaScript expression: {}",
                source.trim(),
                e.kind().msg()
            ))
        };
        let script = parser.parse_script().map_err(|e| invalid(&e))?;
        if let Some(e) = parser.take_errors().into_iter().next() {
            return Err(invalid(&e));
        }
        let mut body = script.body;
        let expr = match (body.pop(), body.is_empty()) {
            (Some(Stmt::Expr(stmt)), true) => match *stmt.expr {
                swc::Expr::Paren(paren) => paren.expr,
                _ => return Err(Error::invalid("not a single expression")),
            },
            _ => {
                return Err(Error::invalid(format!(
                    "`{}` is not a single JavaScript expression",
                    source.trim()
                )));
            }
        };
        Ok(Parsed { wrapped, expr })
    }

    fn text(&self, span: swc_common::Span) -> &str {
        &self.wrapped[span.lo.0 as usize..span.hi.0 as usize]
    }

    /// The source with `edits` (wrapped offsets) applied, unwrapped again.
    fn apply(&self, mut edits: Vec<(u32, u32, String)>) -> String {
        edits.sort_by_key(|(lo, _, _)| std::cmp::Reverse(*lo));
        let mut out = self.wrapped.clone();
        for (lo, hi, text) in edits {
            out.replace_range(lo as usize..hi as usize, &text);
        }
        // Drop the `(` and the `\n)` the source was wrapped in.
        out[OPEN..out.len() - 2].to_owned()
    }
}

fn unparen(expr: &swc::Expr) -> &swc::Expr {
    match expr {
        swc::Expr::Paren(p) => unparen(&p.expr),
        other => other,
    }
}

/// A static member chain rooted at an identifier — `a.b.c` — as its root and
/// each segment's name with the offset it ends at.
fn chain(expr: &swc::Expr) -> Option<(&swc::Ident, Vec<(String, u32)>)> {
    match expr {
        swc::Expr::Ident(ident) => Some((ident, vec![(ident.sym.to_string(), ident.span.hi.0)])),
        swc::Expr::Member(member) => {
            let swc::MemberProp::Ident(prop) = &member.prop else {
                return None;
            };
            let (root, mut segments) = chain(&member.obj)?;
            segments.push((prop.sym.to_string(), prop.span.hi.0));
            Some((root, segments))
        }
        _ => None,
    }
}

fn walk(
    expr: &swc::Expr,
    bound: &mut Vec<String>,
    edits: &mut Vec<(u32, u32, String)>,
    rewrite: &mut dyn FnMut(&[String]) -> Replacement,
) -> Result<()> {
    if let Some((root, segments)) = chain(expr) {
        if bound.iter().any(|b| *b == *root.sym) {
            return Ok(());
        }
        let names: Vec<String> = segments.iter().map(|(n, _)| n.clone()).collect();
        if let Some((count, text)) = rewrite(&names)
            && count >= 1
            && count <= segments.len()
        {
            edits.push((root.span.lo.0, segments[count - 1].1, text));
        }
        return Ok(());
    }
    macro_rules! each {
        ($e:expr, $bound:expr) => {
            walk($e, $bound, edits, rewrite)
        };
    }
    match expr {
        swc::Expr::Lit(_) | swc::Expr::This(_) => Ok(()),
        swc::Expr::Paren(p) => each!(&p.expr, bound),
        swc::Expr::Bin(b) => {
            each!(&b.left, bound)?;
            each!(&b.right, bound)
        }
        swc::Expr::Unary(u) => each!(&u.arg, bound),
        swc::Expr::Cond(c) => {
            each!(&c.test, bound)?;
            each!(&c.cons, bound)?;
            each!(&c.alt, bound)
        }
        swc::Expr::Seq(s) => s.exprs.iter().try_for_each(|e| each!(e, bound)),
        swc::Expr::Member(m) => {
            each!(&m.obj, bound)?;
            match &m.prop {
                swc::MemberProp::Computed(c) => each!(&c.expr, bound),
                _ => Ok(()),
            }
        }
        swc::Expr::OptChain(o) => match &*o.base {
            swc::OptChainBase::Member(m) => {
                each!(&m.obj, bound)?;
                match &m.prop {
                    swc::MemberProp::Computed(c) => each!(&c.expr, bound),
                    _ => Ok(()),
                }
            }
            swc::OptChainBase::Call(c) => {
                each!(&c.callee, bound)?;
                c.args.iter().try_for_each(|a| each!(&a.expr, bound))
            }
        },
        swc::Expr::Call(c) => {
            if let swc::Callee::Expr(callee) = &c.callee {
                each!(callee, bound)?;
            }
            c.args.iter().try_for_each(|a| each!(&a.expr, bound))
        }
        swc::Expr::New(n) => {
            each!(&n.callee, bound)?;
            n.args
                .iter()
                .flatten()
                .try_for_each(|a| each!(&a.expr, bound))
        }
        swc::Expr::Array(a) => a
            .elems
            .iter()
            .flatten()
            .try_for_each(|e| each!(&e.expr, bound)),
        swc::Expr::Tpl(t) => t.exprs.iter().try_for_each(|e| each!(e, bound)),
        swc::Expr::Object(o) => {
            for prop in &o.props {
                match prop {
                    swc::PropOrSpread::Spread(s) => each!(&s.expr, bound)?,
                    swc::PropOrSpread::Prop(prop) => match &**prop {
                        swc::Prop::KeyValue(kv) => {
                            if let swc::PropName::Computed(c) = &kv.key {
                                each!(&c.expr, bound)?;
                            }
                            each!(&kv.value, bound)?;
                        }
                        swc::Prop::Shorthand(ident) => {
                            if bound.iter().any(|b| *b == *ident.sym) {
                                continue;
                            }
                            if let Some((1, text)) = rewrite(&[ident.sym.to_string()]) {
                                edits.push((
                                    ident.span.lo.0,
                                    ident.span.hi.0,
                                    format!("{}: {text}", ident.sym),
                                ));
                            }
                        }
                        _ => return Err(Error::invalid("an object literal with a method")),
                    },
                }
            }
            Ok(())
        }
        swc::Expr::Arrow(a) => {
            let before = bound.len();
            for param in &a.params {
                let swc::Pat::Ident(ident) = param else {
                    return Err(Error::invalid(
                        "an arrow function with destructuring or default parameters",
                    ));
                };
                bound.push(ident.id.sym.to_string());
            }
            let result = match &*a.body {
                swc::BlockStmtOrExpr::Expr(body) => each!(body, bound),
                swc::BlockStmtOrExpr::BlockStmt(_) => {
                    Err(Error::invalid("an arrow function with a block body"))
                }
            };
            bound.truncate(before);
            result
        }
        _ => Err(Error::invalid(format!(
            "`{}` is not an expression that can be carried across",
            expr_text(expr)
        ))),
    }
}

fn expr_text(expr: &swc::Expr) -> &'static str {
    match expr {
        swc::Expr::Assign(_) => "an assignment",
        swc::Expr::Update(_) => "an increment",
        swc::Expr::Fn(_) => "a function",
        swc::Expr::Await(_) => "an `await`",
        _ => "this construct",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(path: &[String], name: &str) -> Replacement {
        (path[0] == name).then(|| (1, format!("r.{name}")))
    }

    #[test]
    fn free_references_are_rewritten_and_everything_else_is_kept() {
        let out = rewrite_free_refs("gift === true &&  qty>2", &mut |p| {
            Some((1, format!("r.{}", p[0])))
        })
        .unwrap();
        assert_eq!(out, "r.gift === true &&  r.qty>2");
    }

    #[test]
    fn a_chain_is_one_reference_and_may_be_replaced_in_part() {
        let mut seen = Vec::new();
        let out = rewrite_free_refs("orders.length + order.customer.name", &mut |p| {
            seen.push(p.to_vec());
            match p[0].as_str() {
                "orders" => Some((1, "context.step1".to_owned())),
                "order" => Some((3, "orderⱵcustomerⱵname".to_owned())),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(out, "context.step1.length + orderⱵcustomerⱵname");
        assert_eq!(seen[0], vec!["orders", "length"]);
    }

    #[test]
    fn arrow_parameters_and_object_keys_are_not_references() {
        let out = rewrite_free_refs(
            "xs.filter(x => x.a === a).map(y => ({a: y, b}))",
            &mut |p| {
                prefix(p, "a")
                    .or_else(|| prefix(p, "b"))
                    .or_else(|| prefix(p, "xs"))
            },
        )
        .unwrap();
        assert_eq!(
            out,
            "r.xs.filter(x => x.a === r.a).map(y => ({a: y, b: r.b}))"
        );
    }

    #[test]
    fn an_object_literal_comes_apart_into_its_entries() {
        assert_eq!(
            object_literal_entries("{discount: 0.15, \"x\": a+1, id}").unwrap(),
            vec![
                ("discount".to_owned(), "0.15".to_owned()),
                ("x".to_owned(), "a+1".to_owned()),
                ("id".to_owned(), "id".to_owned()),
            ]
        );
        assert!(object_literal_entries("{}").unwrap().is_empty());
        assert!(object_literal_entries("[1]").is_err());
        assert!(object_literal_entries("{...a}").is_err());
    }

    #[test]
    fn what_is_not_one_expression_is_refused() {
        assert!(rewrite_free_refs("a = 1", &mut |_| None).is_err());
        assert!(rewrite_free_refs("a; b", &mut |_| None).is_err());
        assert!(rewrite_free_refs("", &mut |_| None).is_err());
    }
}
