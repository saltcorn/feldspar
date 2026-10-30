//! Interpolated templates: a string with `{{ }}` tokens, each token a
//! [`Formula`] (TODO "Email" Phase 1, decision 1).
//!
//! A template is **not a second expression language**. `Template::parse` splits
//! a string into literal runs and tokens and hands every token to
//! [`Formula::parse`], so a subject line, an ownership rule and a trigger's
//! `only_if` are parsed by the same parser, validated against the same
//! [`SchemaShape`], and evaluated by the same [`JsEvaluator`]. The consequences
//! are the point: a template that names a field which does not exist is refused
//! **on save**, in front of the admin; its free variables come out of the
//! ordinary [`Analysis`], so `{{ customerⱵemail }}` is prefetched by the same
//! `prefetch_bindings` that resolves a Ⱶ-path anywhere else.
//!
//! # The three sigils
//!
//! Saltcorn 1's, kept exactly, because a v1 application's templates are the
//! corpus this has to accept:
//!
//! - `{{ x }}` — the value, HTML-escaped when the template renders as HTML.
//! - `{{! x }}` — the value, never escaped: the token that holds markup.
//! - `{{= x }}` — the value, **re-interpolated** in the same scope, so a field
//!   holding `"Hello {{ firstName }}!"` renders as a template of its own.
//!
//! The sigil is the character immediately after `{{`, which is what makes
//! `{{ !x }}` the negation it looks like rather than a raw token.
//!
//! # Two deliberate departures from v1
//!
//! - **Re-interpolation is bounded** ([`MAX_PASSES`]). v1's is not, so a row
//!   whose field holds `{{= self }}` renders forever; here the bound is hit and
//!   the error names the template.
//! - **A missing binding is an error, not an empty string.** v1 evaluates a
//!   token in a sandbox where an unknown name is `undefined`; here every name
//!   was classified against the schema when the template was saved, so an
//!   unresolvable name at render time means the schema changed underneath —
//!   which this system reports rather than absorbs. A **null column value** is
//!   still the empty string: that is data, not a mistake.
//!
//! # Escaping is a property of the render
//!
//! [`render_html`](Template::render_html) escapes bare tokens;
//! [`render_text`](Template::render_text) does not. A subject line put through
//! the HTML rule turns `Tea & Coffee` into `Tea &amp; Coffee`, and a
//! `text/plain` body becomes a page of entities — so a recipient list, a subject
//! and a text body render as text while an HTML body renders as HTML. `{{! }}`
//! and `{{= }}` mean the same in both.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::analyze::Analysis;
use crate::ast::Ast;
use crate::eval::{FormulaCall, JsEvaluator};
use crate::formula::Formula;
use crate::shape::SchemaShape;

/// How many times a `{{= }}` token's result may be interpolated again before the
/// render is refused. Five is more nesting than any real template has and short
/// enough that a self-referential value is reported rather than hung on.
pub const MAX_PASSES: usize = 5;

/// What a token does with the value it evaluates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// `{{ x }}` — escaped where the render escapes (HTML), verbatim in text.
    Html,
    /// `{{! x }}` — never escaped.
    Raw,
    /// `{{= x }}` — escaped as [`Html`](Escape::Html) would be, then rendered
    /// again as a template in the same scope.
    Reinterpolate,
}

/// Whether a render escapes its bare tokens.
///
/// The whole difference between the two rendering rules, passed as data so a
/// caller with a subject line and a body renders both through one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Bare tokens are HTML-escaped: an HTML body, where a `<` from a row must
    /// not become markup.
    Html,
    /// Nothing is escaped: a subject line, a recipient list, a `text/plain`
    /// body.
    Text,
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq)]
enum Part {
    /// Text that is copied out as it stands.
    Literal(String),
    /// A `{{ }}` token.
    Token(Token),
}

/// A parsed `{{ }}` token: what it does with its value, the formula, and the
/// source it was written as (which is what error messages quote).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    escape: Escape,
    formula: Formula,
    source: String,
}

impl Token {
    /// What this token does with its value.
    pub fn escape(&self) -> Escape {
        self.escape
    }

    /// The formula this token evaluates.
    pub fn formula(&self) -> &Formula {
        &self.formula
    }

    /// The token as it was written, sigil and all (`{{= greeting }}`) — what an
    /// error message names, so an admin can find it in the field they typed.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A string with `{{ }}` tokens in it: the literal runs, and one [`Formula`]
/// per token.
///
/// Owned data throughout, like a [`Formula`]: a template lives on a cached
/// trigger configuration shared across threads.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    source: String,
    parts: Vec<Part>,
}

impl Template {
    /// Split `source` into literal runs and tokens, parsing each token's
    /// formula.
    ///
    /// A string with no `{{` is one literal and costs one search plus one clone
    /// — the common case (most subject lines are constants) must not pay for
    /// the facility.
    ///
    /// An unclosed `{{` is an error naming the fragment it starts, because the
    /// admin's actual mistake is almost always a missing brace and the message
    /// has to point at *which* one.
    pub fn parse(source: &str) -> Result<Template> {
        let mut parts = Vec::new();
        let mut rest = source;
        while let Some(open) = rest.find("{{") {
            if open > 0 {
                parts.push(Part::Literal(rest[..open].to_owned()));
            }
            let after = &rest[open + 2..];
            let Some(close) = after.find("}}") else {
                return Err(Error::invalid(format!(
                    "template: unclosed `{{{{` at `{}`",
                    fragment(&rest[open..])
                )));
            };
            let inner = &after[..close];
            parts.push(Part::Token(token(inner)?));
            rest = &after[close + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_owned()));
        }
        Ok(Template {
            source: source.to_owned(),
            parts,
        })
    }

    /// The source text this template was parsed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether this template has no tokens — the constant string, which renders
    /// to itself without an evaluator.
    pub fn is_literal(&self) -> bool {
        !self.parts.iter().any(|p| matches!(p, Part::Token(_)))
    }

    /// The tokens, in the order they appear.
    pub fn tokens(&self) -> impl Iterator<Item = &Token> {
        self.parts.iter().filter_map(|part| match part {
            Part::Token(token) => Some(token),
            Part::Literal(_) => None,
        })
    }

    /// Validate every token against `shape` in `table`'s scope, one
    /// [`Analysis`] per token in order.
    ///
    /// The caller gets exactly what it must prefetch — every Ⱶ-path and
    /// Ↄ-relation the whole template reads — and an unknown identifier is an
    /// error naming both the identifier and **the token it is in**, since a
    /// template is usually several tokens and "unknown identifier `custmer`"
    /// alone leaves the admin to find which one.
    pub fn validate(&self, shape: &SchemaShape, table: &str) -> Result<Vec<Analysis>> {
        self.tokens()
            .map(|token| {
                token
                    .formula
                    .validate(shape, table)
                    .map_err(|e| Error::invalid(format!("`{}`: {e}", token.source)))
            })
            .collect()
    }

    /// Render as HTML: bare tokens escaped, `{{! }}` verbatim.
    pub async fn render_html(
        &self,
        evaluator: &dyn JsEvaluator,
        call: impl Fn(&Formula) -> FormulaCall + Sync,
    ) -> Result<String> {
        self.render(RenderMode::Html, evaluator, call).await
    }

    /// Render as text: nothing escaped — a subject line, a recipient list, a
    /// `text/plain` body.
    pub async fn render_text(
        &self,
        evaluator: &dyn JsEvaluator,
        call: impl Fn(&Formula) -> FormulaCall + Sync,
    ) -> Result<String> {
        self.render(RenderMode::Text, evaluator, call).await
    }

    /// The identifier every token of this template is — `Err` naming the first
    /// token that is anything else.
    ///
    /// A template whose scope is a **flat set of names** rather than a row: a
    /// framework's path templates (`{{ project }}/dist`), where the names in
    /// scope are that framework's own settings and there is no evaluator to
    /// call. Asking for them up front is what lets a caller check, once, that
    /// every name it will be asked for is one it can answer — and refuse the
    /// declaration where its author can still fix it, rather than at the moment
    /// a path is needed.
    ///
    /// This is not a second expression language: the template is parsed by
    /// [`Template::parse`] exactly as any other, and this is a **restriction**
    /// of what it may contain, stated as an error rather than by a second
    /// grammar.
    pub fn identifiers(&self) -> Result<Vec<&str>> {
        self.tokens()
            .map(|token| match token.formula.ast() {
                Ast::Ident(name) => Ok(name.as_str()),
                _ => Err(Error::invalid(format!(
                    "`{}`: this template interpolates names only, so a token has to be \
                     one name and nothing else",
                    token.source
                ))),
            })
            .collect()
    }

    /// Render with every token replaced by its value in `bindings` — no
    /// evaluator, and therefore only the tokens [`identifiers`](Template::
    /// identifiers) accepts.
    ///
    /// Nothing is escaped ([`RenderMode::Text`]'s rule): what this renders is a
    /// path or a prompt, never markup, so `{{ x }}` and `{{! x }}` mean the same
    /// thing. `{{= x }}` is refused rather than re-interpolated — there is no
    /// second pass over a path, and silently not making one would be worse than
    /// saying so.
    ///
    /// An unbound name is an error naming it and what *is* bound, for the reason
    /// the async render gives: reaching here with a name nothing answers means
    /// the declaration and the scope disagree, and an empty string in the middle
    /// of a path would turn that into a directory nobody meant.
    pub fn render_static(&self, bindings: &BTreeMap<String, String>) -> Result<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Token(token) => {
                    if token.escape == Escape::Reinterpolate {
                        return Err(Error::invalid(format!(
                            "`{}`: `{{{{= }}}}` re-interpolates its own result, which this \
                             template is not rendered often enough to do",
                            token.source
                        )));
                    }
                    let Ast::Ident(name) = token.formula.ast() else {
                        return Err(Error::invalid(format!(
                            "`{}`: this template interpolates names only, so a token has to \
                             be one name and nothing else",
                            token.source
                        )));
                    };
                    let value = bindings.get(name.as_str()).ok_or_else(|| {
                        Error::invalid(format!(
                            "`{}`: there is no `{name}` here; the names in scope are {}",
                            token.source,
                            if bindings.is_empty() {
                                "none".to_owned()
                            } else {
                                bindings.keys().cloned().collect::<Vec<_>>().join(", ")
                            }
                        ))
                    })?;
                    out.push_str(value);
                }
            }
        }
        Ok(out)
    }

    /// Render with every token replaced by the value its **path** names in
    /// `scope` — a name (`{{ order_id }}`) or a chain of static members on one
    /// (`{{ run.id }}`, `{{ order.customer.name }}`) — and no evaluator.
    ///
    /// [`render_static`](Template::render_static) for a scope that is a JSON
    /// object rather than a flat map: a checklist's variables are what earlier
    /// steps captured, and a captured row or response is an object a later step
    /// reaches into. A string is copied as it stands, `null` is the empty
    /// string, and any other value is written as JSON. A template that is one
    /// token and nothing else is better served by [`sole_path`](Template::
    /// sole_path), which keeps the value's type.
    ///
    /// The refusals are `render_static`'s: a token that is anything but a path,
    /// `{{= }}`, and a name the scope does not have — and a member the value
    /// does not have, which is the same mistake one level down.
    pub fn render_paths(&self, scope: &Json) -> Result<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Token(token) => match self::path_value(token, scope)? {
                    Json::String(s) => out.push_str(&s),
                    Json::Null => {}
                    other => out.push_str(&other.to_string()),
                },
            }
        }
        Ok(out)
    }

    /// The value of the one path this template is, when it is exactly one
    /// token with nothing around it (`"{{ order_id }}"`), with its JSON type
    /// kept — so a captured id that is a number is substituted as a number.
    /// `None` for any other template.
    pub fn sole_path(&self, scope: &Json) -> Option<Result<Json>> {
        match self.parts.as_slice() {
            [Part::Token(token)] => Some(path_value(token, scope)),
            _ => None,
        }
    }

    /// Render in `mode`, evaluating each token through `evaluator`.
    ///
    /// `call` builds the evaluation request for one token's formula — the
    /// bindings are the caller's, because what is in scope is a property of the
    /// *event* (or the row, or the caller) and this crate sits below everything
    /// that has one.
    pub async fn render(
        &self,
        mode: RenderMode,
        evaluator: &dyn JsEvaluator,
        call: impl Fn(&Formula) -> FormulaCall + Sync,
    ) -> Result<String> {
        if self.is_literal() {
            return Ok(self.source.clone());
        }
        render_parts(&self.parts, mode, evaluator, &call, 0, &self.source).await
    }
}

/// Parse one token's interior: the sigil, then the formula.
fn token(inner: &str) -> Result<Token> {
    let source = format!("{{{{{inner}}}}}");
    // The sigil is the character straight after `{{`, untrimmed: that is v1's
    // rule, and it is what leaves `{{ !x }}` the negation it reads as.
    let (escape, expr) = match inner.as_bytes().first() {
        Some(b'!') => (Escape::Raw, &inner[1..]),
        Some(b'=') => (Escape::Reinterpolate, &inner[1..]),
        _ => (Escape::Html, inner),
    };
    let formula = Formula::parse(expr).map_err(|e| Error::invalid(format!("`{source}`: {e}")))?;
    Ok(Token {
        escape,
        formula,
        source,
    })
}

/// Render a run of parts, appending each token's value.
///
/// Boxed rather than plainly recursive because a `{{= }}` token renders its own
/// result as a template: the recursion is real, and [`MAX_PASSES`] is what keeps
/// it finite. `root` is the outermost template's source, so the message a
/// runaway produces names what the admin wrote rather than what a row held.
fn render_parts<'a>(
    parts: &'a [Part],
    mode: RenderMode,
    evaluator: &'a dyn JsEvaluator,
    call: &'a (dyn Fn(&Formula) -> FormulaCall + Sync + 'a),
    depth: usize,
    root: &'a str,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        let mut out = String::new();
        for part in parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Token(token) => {
                    let value = evaluator
                        .eval_value(call(&token.formula))
                        .await
                        .map_err(|e| Error::invalid(format!("`{}`: {e}", token.source)))?;
                    let text = render_value(&value);
                    match token.escape {
                        Escape::Raw => out.push_str(&text),
                        Escape::Html => match mode {
                            RenderMode::Html => out.push_str(&escape_html(&text)),
                            RenderMode::Text => out.push_str(&text),
                        },
                        Escape::Reinterpolate => {
                            // Escape *first*, then interpolate: the value's own
                            // literal text is escaped once, and the tokens it
                            // carries escape their own values — the other order
                            // would escape those twice.
                            let text = match mode {
                                RenderMode::Html => escape_html(&text),
                                RenderMode::Text => text,
                            };
                            if !text.contains("{{") {
                                out.push_str(&text);
                                continue;
                            }
                            if depth >= MAX_PASSES {
                                return Err(Error::invalid(format!(
                                    "template `{}`: `{}` is still interpolating after \
                                     {MAX_PASSES} passes — a value that renders itself",
                                    fragment(root),
                                    token.source
                                )));
                            }
                            let mut nested = Template::parse(&text)?;
                            from_data(&mut nested, &token.source)?;
                            out.push_str(
                                &render_parts(
                                    &nested.parts,
                                    mode,
                                    evaluator,
                                    call,
                                    depth + 1,
                                    root,
                                )
                                .await?,
                            );
                        }
                    }
                }
            }
        }
        Ok(out)
    })
}

/// Hold a template parsed from a `{{= }}` token's **value** — row data, which
/// whoever wrote the row chose — to what data may ask for: each token only
/// names a value (`firstName`, `user.name`, `customerⱵemail`), and is escaped
/// as a bare token is. An operator, a call or a computed member is code, and
/// the formula isolate is shared by every evaluation (security review #23); a
/// `{{! }}` would let the row put any field's markup on the page unescaped.
fn from_data(nested: &mut Template, outer: &str) -> Result<()> {
    for part in &mut nested.parts {
        let Part::Token(token) = part else { continue };
        if !names_a_value(token.formula.ast()) {
            return Err(Error::invalid(format!(
                "`{outer}` holds the token `{}`; a value re-interpolated with `{{{{= }}}}` \
                 may only name fields, as in `{{{{ firstName }}}}`",
                token.source
            )));
        }
        if token.escape == Escape::Raw {
            token.escape = Escape::Html;
        }
    }
    Ok(())
}

/// Whether `ast` is a name, or a name's static members: `a`, `user.name`.
fn names_a_value(ast: &Ast) -> bool {
    match ast {
        Ast::Ident(_) => true,
        Ast::Member {
            obj,
            prop: crate::ast::MemberProp::Static(_),
            ..
        } => names_a_value(obj),
        _ => false,
    }
}

/// One evaluated token as text.
///
/// `null` (and the `undefined` that reaches here as one) is the **empty
/// string**: a null column is data, and an email that says "null" where the
/// customer has no middle name is worse than one that says nothing. A string is
/// itself — not its JSON spelling, which would quote it. An array or an object
/// renders as JSON, which is the one departure from v1: JavaScript's own
/// `String({})` is `[object Object]`, and a template that reached that has told
/// the reader nothing.
fn render_value(value: &Json) -> String {
    match value {
        Json::Null => String::new(),
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// HTML-escape the five characters that can end an element, an attribute or a
/// string in markup.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The first few characters of `s`, for an error message that must quote
/// something without reprinting a whole HTML body.
fn fragment(s: &str) -> String {
    const LIMIT: usize = 40;
    let taken: String = s.chars().take(LIMIT).collect();
    if taken.chars().count() < s.chars().count() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// The value a path token names in `scope` (see [`Template::render_paths`]).
fn path_value(token: &Token, scope: &Json) -> Result<Json> {
    if token.escape == Escape::Reinterpolate {
        return Err(Error::invalid(format!(
            "`{}`: `{{{{= }}}}` re-interpolates its own result, which this template is \
             not rendered often enough to do",
            token.source
        )));
    }
    let mut names = Vec::new();
    let mut ast = token.formula.ast();
    loop {
        match ast {
            Ast::Ident(name) => {
                names.push(name.clone());
                break;
            }
            Ast::Member { obj, prop, .. } => {
                let name = match prop {
                    crate::ast::MemberProp::Static(prop) => prop.clone(),
                    crate::ast::MemberProp::Computed(index) => match index.as_ref() {
                        Ast::Str(key) => key.clone(),
                        Ast::Num(n) if n.fract() == 0.0 && *n >= 0.0 => format!("{n}"),
                        _ => {
                            return Err(Error::invalid(format!(
                                "`{}`: an index here is a number or a quoted name",
                                token.source
                            )));
                        }
                    },
                };
                names.push(name);
                ast = obj;
            }
            _ => {
                return Err(Error::invalid(format!(
                    "`{}`: this template interpolates names only (`x`, `x.y`), so a token \
                     has to be one of those and nothing else",
                    token.source
                )));
            }
        }
    }
    names.reverse();
    let mut value = scope;
    for (depth, name) in names.iter().enumerate() {
        let next = match value {
            Json::Object(map) => map.get(name.as_str()),
            Json::Array(items) => name.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        };
        value = next.ok_or_else(|| {
            let what = if depth == 0 {
                "here".to_owned()
            } else {
                format!("in `{}`", names[..depth].join("."))
            };
            let known = match value {
                Json::Object(map) if !map.is_empty() => {
                    map.keys().cloned().collect::<Vec<_>>().join(", ")
                }
                _ => "none".to_owned(),
            };
            Error::invalid(format!(
                "`{}`: there is no `{name}` {what}; the names there are {known}",
                token.source
            ))
        })?;
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ast;

    fn parts(source: &str) -> Vec<Part> {
        Template::parse(source).unwrap().parts
    }

    #[test]
    fn a_string_with_no_token_is_one_literal() {
        let t = Template::parse("Receipt for your order").unwrap();
        assert!(t.is_literal());
        assert_eq!(t.parts.len(), 1);
        assert_eq!(t.tokens().count(), 0);
        // The empty template is a template, not an error: an unset optional
        // setting must not have to be special-cased by every caller.
        assert!(Template::parse("").unwrap().is_literal());
        assert!(Template::parse("").unwrap().parts.is_empty());
    }

    #[test]
    fn literals_and_tokens_alternate_in_order() {
        let parts = parts("Hello {{ name }}, order {{ id }}!");
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0], Part::Literal("Hello ".into()));
        assert_eq!(parts[2], Part::Literal(", order ".into()));
        assert_eq!(parts[4], Part::Literal("!".into()));
        let Part::Token(token) = &parts[1] else {
            panic!("expected a token, got {:?}", parts[1]);
        };
        assert_eq!(token.escape(), Escape::Html);
        assert_eq!(*token.formula().ast(), Ast::Ident("name".into()));
        assert_eq!(token.source(), "{{ name }}");
    }

    #[test]
    fn the_sigil_is_the_character_after_the_braces() {
        let sigils = |source: &str| -> Vec<Escape> {
            Template::parse(source)
                .unwrap()
                .tokens()
                .map(Token::escape)
                .collect()
        };
        assert_eq!(
            sigils("{{ x }}{{! x }}{{= x }}"),
            [Escape::Html, Escape::Raw, Escape::Reinterpolate]
        );
        // `{{ !x }}` is the negation it reads as — the sigil is not trimmed to.
        let t = Template::parse("{{ !x }}").unwrap();
        let token = t.tokens().next().unwrap();
        assert_eq!(token.escape(), Escape::Html);
        assert!(matches!(token.formula().ast(), Ast::Unary { .. }));
    }

    #[test]
    fn an_unclosed_token_names_the_fragment_it_starts() {
        let err = Template::parse("Hi {{ name, your order is ready").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unclosed"), "{msg}");
        assert!(msg.contains("{{ name"), "{msg}");
        // A very long tail is elided rather than reprinted whole.
        let long = format!("{{{{ {}", "x".repeat(200));
        let msg = Template::parse(&long).unwrap_err().to_string();
        assert!(msg.contains('…'), "{msg}");
        assert!(msg.len() < 120, "{msg}");
    }

    #[test]
    fn a_path_render_reaches_into_the_scope_and_keeps_a_sole_tokens_type() {
        let scope = serde_json::json!({
            "run": { "id": "r1" },
            "order": { "id": 42, "lines": [{ "sku": "A" }] },
            "note": null,
        });
        let t = Template::parse("/orders/{{ order.id }}?run={{ run.id }}{{ note }}").unwrap();
        assert_eq!(t.render_paths(&scope).unwrap(), "/orders/42?run=r1");
        assert_eq!(
            Template::parse("{{ order.lines[0].sku }}")
                .unwrap()
                .render_paths(&scope)
                .unwrap(),
            "A"
        );
        // One token alone keeps its JSON type; anything around it does not.
        let sole = Template::parse("{{ order.id }}").unwrap();
        assert_eq!(
            sole.sole_path(&scope).unwrap().unwrap(),
            serde_json::json!(42)
        );
        assert!(t.sole_path(&scope).is_none());
        // A missing name or member is named, with what is there.
        let msg = Template::parse("{{ order.total }}")
            .unwrap()
            .render_paths(&scope)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no `total` in `order`"), "{msg}");
        assert!(msg.contains("id, lines"), "{msg}");
        // A formula is not a path.
        let msg = Template::parse("{{ order.id + 1 }}")
            .unwrap()
            .render_paths(&scope)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("names only"), "{msg}");
    }

    #[test]
    fn a_tokens_formula_is_parsed_by_the_formula_parser() {
        // An empty token and a broken one are refused, with the token quoted so
        // the admin can find which of five it is.
        for source in ["{{ }}", "{{ x + }}", "{{ let x = 1 }}"] {
            let msg = Template::parse(source).unwrap_err().to_string();
            assert!(msg.contains(source), "{source}: {msg}");
        }
        // And a Ⱶ-path is one identifier here exactly as it is in a formula.
        let t = Template::parse("{{ customerⱵemail }}").unwrap();
        assert_eq!(
            *t.tokens().next().unwrap().formula().ast(),
            Ast::Ident("customerⱵemail".into())
        );
    }

    #[test]
    fn validation_names_the_identifier_and_the_token_it_is_in() {
        use crate::shape::{SchemaShape, TableShape};
        let shape = SchemaShape::new().table(
            "orders",
            TableShape::new()
                .field("id")
                .field("total")
                .primary_key("id"),
        );
        let t = Template::parse("Order {{ id }} for {{ total }}").unwrap();
        let analyses = t.validate(&shape, "orders").unwrap();
        assert_eq!(analyses.len(), 2);
        assert!(analyses[0].fields.contains("id"));
        assert!(analyses[1].fields.contains("total"));

        let t = Template::parse("Order {{ id }} for {{ totl }}").unwrap();
        let msg = t.validate(&shape, "orders").unwrap_err().to_string();
        assert!(msg.contains("totl"), "the identifier: {msg}");
        assert!(msg.contains("{{ totl }}"), "the token it is in: {msg}");
    }

    #[test]
    fn a_value_renders_as_text_and_null_renders_as_nothing() {
        assert_eq!(render_value(&Json::Null), "");
        assert_eq!(
            render_value(&serde_json::json!("Tea & Coffee")),
            "Tea & Coffee"
        );
        assert_eq!(render_value(&serde_json::json!(42)), "42");
        assert_eq!(render_value(&serde_json::json!(2.5)), "2.5");
        assert_eq!(render_value(&serde_json::json!(true)), "true");
        // Compound values are JSON, not `[object Object]`.
        assert_eq!(render_value(&serde_json::json!([1, 2])), "[1,2]");
        assert_eq!(render_value(&serde_json::json!({ "a": 1 })), r#"{"a":1}"#);
    }

    #[test]
    fn a_flat_scope_renders_without_an_evaluator() {
        let bindings = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let t = Template::parse("{{ project }}/dist").unwrap();
        assert_eq!(t.identifiers().unwrap(), ["project"]);
        assert_eq!(
            t.render_static(&bindings(&[("project", "todo")])).unwrap(),
            "todo/dist"
        );
        // The blank value is a blank run, not a refusal: what to do with the
        // `/` it leaves behind is the caller's rule, not this one's.
        assert_eq!(
            t.render_static(&bindings(&[("project", "")])).unwrap(),
            "/dist"
        );
        // Nothing is escaped, and `{{! }}` says the same thing here as `{{ }}`.
        let t = Template::parse("{{! app }} & {{ app }}").unwrap();
        assert_eq!(
            t.render_static(&bindings(&[("app", "Tea & Coffee")]))
                .unwrap(),
            "Tea & Coffee & Tea & Coffee"
        );
    }

    #[test]
    fn a_flat_scope_refuses_what_it_cannot_answer() {
        let empty = std::collections::BTreeMap::new();
        // An expression: one name and nothing else is the whole vocabulary.
        let t = Template::parse("{{ project + 1 }}").unwrap();
        let msg = t.identifiers().unwrap_err().to_string();
        assert!(msg.contains("{{ project + 1 }}"), "{msg}");
        assert!(t.render_static(&empty).is_err());
        // Re-interpolation, which there is no second pass for.
        let t = Template::parse("{{= project }}").unwrap();
        let msg = t.render_static(&empty).unwrap_err().to_string();
        assert!(msg.contains("{{= project }}"), "{msg}");
        // And an unbound name says what is in scope.
        let t = Template::parse("{{ projekt }}").unwrap();
        let msg = t
            .render_static(&[("project".to_owned(), "todo".to_owned())].into())
            .unwrap_err()
            .to_string();
        assert!(msg.contains("projekt") && msg.contains("project"), "{msg}");
    }

    #[test]
    fn escaping_covers_the_five_markup_characters() {
        assert_eq!(
            escape_html(r#"<a href="x">&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }
}

/// Rendering, in the real V8 the production path uses.
///
/// The cases are Saltcorn 1's own interpolation tests
/// (`packages/saltcorn-data/tests/calc.test.ts`), transcribed — this facility's
/// job is to accept the corpus v1 accepts — plus the three things v1 does not
/// answer: what text mode does with `&`, what an unknown identifier does, and
/// what happens when a value renders itself.
#[cfg(feature = "eval")]
#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::eval::DenoEvaluator;
    use crate::translate::{AmbientValues, Operation};
    use sc_query::Value;
    use std::collections::BTreeMap;

    /// Bind a bare row, the scope a template over a table row is rendered in.
    // `use<>` because the closure captures nothing borrowed — the row is built
    // here and moved in — and edition 2024 would otherwise tie it to `fields`.
    fn bind(fields: &[(&str, Value)]) -> impl Fn(&Formula) -> FormulaCall + Sync + use<> {
        let row: BTreeMap<String, Value> = fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        move |formula: &Formula| FormulaCall {
            formula: formula.clone(),
            op: Operation::Read,
            row: row.clone(),
            user: None,
            ambient: AmbientValues::new(),
        }
    }

    #[tokio::test]
    async fn a_token_renders_its_value_and_a_formula_is_an_expression() {
        let ev = DenoEvaluator::new();
        let bindings = bind(&[("x", Value::Int(1))]);
        let t = Template::parse("hello {{ x }}").unwrap();
        assert_eq!(t.render_html(&ev, &bindings).await.unwrap(), "hello 1");
        let t = Template::parse("{{ x+1 }}").unwrap();
        assert_eq!(t.render_html(&ev, &bindings).await.unwrap(), "2");
        // A literal never reaches the evaluator, and renders to itself.
        let t = Template::parse("hello").unwrap();
        assert_eq!(t.render_html(&ev, &bindings).await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn a_null_renders_as_nothing_and_a_row_value_as_itself() {
        let ev = DenoEvaluator::new();
        let bindings = bind(&[
            ("middle_name", Value::Null),
            ("title", Value::Text("Dune".into())),
        ]);
        let t = Template::parse("[{{ middle_name }}] {{ title }}").unwrap();
        assert_eq!(t.render_text(&ev, &bindings).await.unwrap(), "[] Dune");
    }

    #[tokio::test]
    async fn a_bare_token_is_escaped_as_html_and_a_bang_token_is_not() {
        let ev = DenoEvaluator::new();
        let bindings = bind(&[("x", Value::Text("<script>".into()))]);
        let t = Template::parse("{{ x }}").unwrap();
        assert_eq!(
            t.render_html(&ev, &bindings).await.unwrap(),
            "&lt;script&gt;"
        );
        let t = Template::parse("{{! x }}").unwrap();
        assert_eq!(t.render_html(&ev, &bindings).await.unwrap(), "<script>");
    }

    #[tokio::test]
    async fn text_mode_escapes_nothing() {
        // The departure from v1, which has one rendering rule: a subject line
        // put through the HTML rule reads `Tea &amp; Coffee` in the mail client.
        let ev = DenoEvaluator::new();
        let bindings = bind(&[("name", Value::Text("Tea & Coffee".into()))]);
        let t = Template::parse("Receipt from {{ name }}").unwrap();
        assert_eq!(
            t.render_text(&ev, &bindings).await.unwrap(),
            "Receipt from Tea & Coffee"
        );
        assert_eq!(
            t.render_html(&ev, &bindings).await.unwrap(),
            "Receipt from Tea &amp; Coffee"
        );
    }

    #[tokio::test]
    async fn an_equals_token_interpolates_its_own_result() {
        let ev = DenoEvaluator::new();
        let bindings = bind(&[
            ("greeter", Value::Text("Hello {{ firstName }}!".into())),
            ("firstName", Value::Text("John".into())),
        ]);
        let t = Template::parse("{{= greeter }}").unwrap();
        assert_eq!(t.render_html(&ev, &bindings).await.unwrap(), "Hello John!");
        // The value's own literal text is escaped once, and the token it
        // carries escapes its value — not twice.
        let bindings = bind(&[
            ("greeter", Value::Text("Tea & {{ firstName }}".into())),
            ("firstName", Value::Text("<b>".into())),
        ]);
        let t = Template::parse("{{= greeter }}").unwrap();
        assert_eq!(
            t.render_html(&ev, &bindings).await.unwrap(),
            "Tea &amp; &lt;b&gt;"
        );
    }

    #[tokio::test]
    async fn a_value_from_data_may_only_name_fields() {
        // Row data re-interpolated is somebody's text, not the admin's
        // template: a token in it names a value, and nothing more.
        let ev = DenoEvaluator::new();
        for code in [
            "{{ `x`.constructor.constructor(`return 1`)() }}",
            "{{ firstName + 1 }}",
            "{{ firstName.toUpperCase() }}",
            "{{ user[firstName] }}",
        ] {
            let bindings = bind(&[
                ("greeter", Value::Text(code.into())),
                ("firstName", Value::Text("John".into())),
            ]);
            let t = Template::parse("{{= greeter }}").unwrap();
            let msg = t.render_text(&ev, &bindings).await.unwrap_err().to_string();
            assert!(
                msg.contains("may only name fields") || msg.contains("may not read the member"),
                "{code}: {msg}"
            );
        }
        // And a raw token in it is escaped as a bare one is.
        let bindings = bind(&[
            ("greeter", Value::Text("Hi {{! bio }}".into())),
            ("bio", Value::Text("<img src=x onerror=alert(1)>".into())),
        ]);
        let t = Template::parse("{{= greeter }}").unwrap();
        assert_eq!(
            t.render_html(&ev, &bindings).await.unwrap(),
            "Hi &lt;img src=x onerror=alert(1)&gt;"
        );
    }

    #[tokio::test]
    async fn a_self_referential_value_hits_the_bound_and_names_the_template() {
        let ev = DenoEvaluator::new();
        let bindings = bind(&[("self_", Value::Text("{{= self_ }}".into()))]);
        let t = Template::parse("Body: {{= self_ }}").unwrap();
        let msg = t.render_html(&ev, &bindings).await.unwrap_err().to_string();
        assert!(msg.contains("passes"), "{msg}");
        assert!(msg.contains("Body: {{= self_ }}"), "the template: {msg}");
    }

    #[tokio::test]
    async fn an_unbound_name_is_an_error_naming_its_token() {
        // Decision 4: the only way here is a schema that changed under a saved
        // template, and this system reports that rather than sending an email
        // addressed to nobody.
        let ev = DenoEvaluator::new();
        let bindings = bind(&[("x", Value::Int(1))]);
        let t = Template::parse("to: {{ email }}").unwrap();
        let msg = t.render_text(&ev, &bindings).await.unwrap_err().to_string();
        assert!(msg.contains("{{ email }}"), "{msg}");
    }
}
