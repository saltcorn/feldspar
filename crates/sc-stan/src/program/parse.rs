//! The block splitter and the declaration parser (TODO §5).
//!
//! Narrow on purpose. It finds the seven blocks by brace matching, and in
//! `data`, `parameters`, `transformed parameters` and `generated quantities` it
//! reads the **top-level declarations** — recognised by a leading type keyword —
//! and skips every statement, to its `;` or its closing brace, without trying
//! to understand it. Only the modern type grammar is spoken (Stan 2.33 removed
//! the old array syntax, and CmdStan older than that is refused anyway, §20).
//!
//! What comes out is [`Interface`]: for each variable its element type, its
//! **full shape outer to inner** — `array[N] vector[K]` and `matrix[N, K]` are
//! both `[N, K]`, the way CmdStan's JSON nests them — its type as written, and
//! its `lower=`/`upper=` bounds as text. A size is kept as text and, where it is
//! only identifiers, integer literals, `+ - * %/% %` and parentheses, as a
//! [`SizeTree`] that evaluates.
//!
//! `stanc` remains the authority on whether the program is valid (§5); what
//! this refuses is only what it cannot read, and what the binder could not
//! bind (a tuple or a complex number in `data`).

use sc_error::{Error, Result};
use sc_model::{Declaration, Element, Interface, SizeExpr, SizeOp, SizeTree};

use super::ProgramFile;
use super::lex::{Tok, Token};

/// The blocks, in the order Stan requires them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Block {
    Functions,
    Data,
    TransformedData,
    Parameters,
    TransformedParameters,
    Model,
    GeneratedQuantities,
}

impl Block {
    fn name(self) -> &'static str {
        match self {
            Block::Functions => "functions",
            Block::Data => "data",
            Block::TransformedData => "transformed data",
            Block::Parameters => "parameters",
            Block::TransformedParameters => "transformed parameters",
            Block::Model => "model",
            Block::GeneratedQuantities => "generated quantities",
        }
    }
}

/// Every type keyword a declaration can start with.
const TYPE_KEYWORDS: &[&str] = &[
    "int",
    "real",
    "complex",
    "vector",
    "row_vector",
    "matrix",
    "complex_vector",
    "complex_row_vector",
    "complex_matrix",
    "simplex",
    "unit_vector",
    "sum_to_zero_vector",
    "sum_to_zero_matrix",
    "ordered",
    "positive_ordered",
    "cov_matrix",
    "corr_matrix",
    "cholesky_factor_cov",
    "cholesky_factor_corr",
    "column_stochastic_matrix",
    "row_stochastic_matrix",
    "array",
    "tuple",
];

/// The bound keys a type's `<…>` may carry.
const BOUND_KEYS: &[&str] = &["lower", "upper", "offset", "multiplier"];

/// Parse the spliced tokens of a program into its interface.
pub(super) fn interface(tokens: &[Token], files: &[ProgramFile]) -> Result<Interface> {
    let cx = Cx { tokens, files };
    let mut interface = Interface::default();
    for (block, range) in cx.blocks()? {
        let into = match block {
            Block::Data => &mut interface.data,
            Block::Parameters => &mut interface.parameters,
            Block::TransformedParameters => &mut interface.transformed,
            Block::GeneratedQuantities => &mut interface.generated,
            _ => continue,
        };
        let declared = cx.declarations(range.0, range.1)?;
        if block == Block::Data {
            for (decl, at) in &declared {
                refuse_unbindable(&cx, decl, *at)?;
            }
        }
        into.extend(declared.into_iter().map(|(decl, _)| decl));
    }
    Ok(interface)
}

/// A tuple or a complex number in `data` has no binding that produces it.
fn refuse_unbindable(cx: &Cx<'_>, decl: &Declaration, at: usize) -> Result<()> {
    let why = match decl.element {
        Element::Tuple => {
            "is a tuple, which no binding produces: bind a tuple's parts as \
                           separate variables"
        }
        Element::Complex => {
            "is complex, which no binding produces: bind its real and \
                             imaginary parts as separate variables"
        }
        _ => return Ok(()),
    };
    Err(cx.error(
        at,
        format!(
            "the data variable `{}` ({}) {why}",
            decl.name, decl.stan_type
        ),
    ))
}

/// The tokens and the files they came from, for slicing text and placing
/// sentences.
struct Cx<'a> {
    tokens: &'a [Token],
    files: &'a [ProgramFile],
}

impl Cx<'_> {
    fn at(&self, i: usize) -> Option<&Token> {
        self.tokens.get(i)
    }

    fn place(&self, i: usize) -> String {
        match self.tokens.get(i).or(self.tokens.last()) {
            Some(t) => format!("{}:{}:{}", self.files[t.file].path, t.line, t.col),
            None => self.files[0].path.clone(),
        }
    }

    fn error(&self, i: usize, msg: impl std::fmt::Display) -> Error {
        Error::invalid(format!("{}: {msg}", self.place(i)))
    }

    /// What token `i` looks like in a sentence.
    fn show(&self, i: usize) -> String {
        match self.at(i) {
            None => "the end of the program".to_owned(),
            Some(_) => format!("`{}`", self.text(i, i + 1)),
        }
    }

    /// The source text of tokens `from..to`, as written: a space wherever the
    /// source had whitespace (or a comment) between two tokens, none elsewhere.
    fn text(&self, from: usize, to: usize) -> String {
        let mut out = String::new();
        for i in from..to.min(self.tokens.len()) {
            let t = &self.tokens[i];
            if i > from {
                let prev = &self.tokens[i - 1];
                if prev.file != t.file || prev.end < t.start {
                    out.push(' ');
                }
            }
            out.push_str(&self.files[t.file].text[t.start..t.end]);
        }
        out
    }

    fn expect(&self, i: usize, p: &str, what: &str) -> Result<usize> {
        match self.at(i) {
            Some(t) if t.is(p) => Ok(i + 1),
            _ => Err(self.error(i, format!("expected `{p}` {what}, found {}", self.show(i)))),
        }
    }

    /// The index just past the bracket that closes the one at `open`, counting
    /// `()`, `[]` and `{}` together.
    fn close(&self, open: usize) -> Result<usize> {
        let mut depth = 0usize;
        for i in open..self.tokens.len() {
            let t = &self.tokens[i];
            if t.is("(") || t.is("[") || t.is("{") {
                depth += 1;
            } else if t.is(")") || t.is("]") || t.is("}") {
                depth -= 1;
                if depth == 0 {
                    return Ok(i + 1);
                }
            }
        }
        Err(self.error(open, format!("this {} is never closed", self.show(open))))
    }

    // ---- blocks ----

    /// Each block and the token range inside its braces, in program order.
    fn blocks(&self) -> Result<Vec<(Block, (usize, usize))>> {
        let mut out: Vec<(Block, (usize, usize))> = Vec::new();
        let mut i = 0;
        while i < self.tokens.len() {
            let (block, words) = match self.at(i).and_then(Token::ident) {
                Some("functions") => (Block::Functions, 1),
                Some("data") => (Block::Data, 1),
                Some("parameters") => (Block::Parameters, 1),
                Some("model") => (Block::Model, 1),
                Some("transformed") => match self.at(i + 1).and_then(Token::ident) {
                    Some("data") => (Block::TransformedData, 2),
                    Some("parameters") => (Block::TransformedParameters, 2),
                    _ => {
                        return Err(self.error(
                            i + 1,
                            format!(
                                "expected `data` or `parameters` after `transformed`, found {}",
                                self.show(i + 1)
                            ),
                        ));
                    }
                },
                Some("generated") if self.at(i + 1).is_some_and(|t| t.is_word("quantities")) => {
                    (Block::GeneratedQuantities, 2)
                }
                _ => {
                    return Err(self.error(
                        i,
                        format!(
                            "expected a block — `functions`, `data`, `transformed data`, \
                             `parameters`, `transformed parameters`, `model` or `generated \
                             quantities` — found {}",
                            self.show(i)
                        ),
                    ));
                }
            };
            if let Some((last, _)) = out.last() {
                if *last == block {
                    return Err(self.error(i, format!("a second `{}` block", block.name())));
                }
                if *last > block {
                    return Err(self.error(
                        i,
                        format!(
                            "the `{}` block must come before the `{}` block",
                            block.name(),
                            last.name()
                        ),
                    ));
                }
            }
            let open = self.expect(i + words, "{", &format!("after `{}`", block.name()))? - 1;
            let end = self.close(open)?;
            out.push((block, (open + 1, end - 1)));
            i = end;
        }
        Ok(out)
    }

    // ---- statements ----

    /// The top-level declarations of the block between `from` and `to`, each
    /// with the index of its first token.
    fn declarations(&self, from: usize, to: usize) -> Result<Vec<(Declaration, usize)>> {
        let mut out = Vec::new();
        let mut i = from;
        while i < to {
            let starts_type = self
                .at(i)
                .and_then(Token::ident)
                .is_some_and(|w| TYPE_KEYWORDS.contains(&w));
            if starts_type {
                i = self.declaration(i, to, &mut out)?;
            } else {
                i = self.skip_statement(i, to)?;
            }
        }
        Ok(out)
    }

    /// Skip one statement starting at `i`, returning the index after it. Only
    /// its extent is understood: a braced block, a control statement's header
    /// and body, or everything up to a `;` outside brackets.
    fn skip_statement(&self, i: usize, to: usize) -> Result<usize> {
        let Some(t) = self.at(i) else {
            return Ok(to);
        };
        if t.is("{") {
            return self.close(i);
        }
        if t.is(";") {
            return Ok(i + 1);
        }
        if let Some(word @ ("for" | "while" | "if" | "profile")) = t.ident() {
            let open = self.expect(i + 1, "(", &format!("after `{word}`"))? - 1;
            let after = self.skip_statement(self.close(open)?, to)?;
            if word == "if" && self.at(after).is_some_and(|t| t.is_word("else")) {
                return self.skip_statement(after + 1, to);
            }
            return Ok(after);
        }
        let mut j = i;
        while j < to {
            let t = &self.tokens[j];
            if t.is(";") {
                return Ok(j + 1);
            }
            if t.is("(") || t.is("[") || t.is("{") {
                j = self.close(j)?;
            } else {
                j += 1;
            }
        }
        Err(self.error(
            i,
            "this statement does not end with `;` before the end of its block",
        ))
    }

    /// One declaration statement — a type and one or more names, each maybe
    /// with an initial value — pushed onto `out`.
    fn declaration(
        &self,
        start: usize,
        to: usize,
        out: &mut Vec<(Declaration, usize)>,
    ) -> Result<usize> {
        let (ty, mut i) = self.ty(start)?;
        let stan_type = self.text(start, i);
        loop {
            let name = match self.at(i).and_then(Token::ident) {
                Some(name) if !TYPE_KEYWORDS.contains(&name) => name.to_owned(),
                _ => {
                    return Err(self.error(
                        i,
                        format!(
                            "expected a variable name after `{stan_type}`, found {}",
                            self.show(i)
                        ),
                    ));
                }
            };
            i += 1;
            if self.at(i).is_some_and(|t| t.is("[")) {
                return Err(self.error(
                    i,
                    format!(
                        "`{stan_type} {name}[…]` is the array syntax Stan 2.33 removed; write \
                         `array[…] {stan_type} {name}`"
                    ),
                ));
            }
            out.push((
                Declaration {
                    name,
                    element: ty.element,
                    dims: ty.dims.clone(),
                    stan_type: stan_type.clone(),
                    lower: ty.lower.clone(),
                    upper: ty.upper.clone(),
                },
                start,
            ));
            if self.at(i).is_some_and(|t| t.is("=")) {
                i = self.skip_expression(i + 1, to);
            }
            match self.at(i) {
                Some(t) if t.is(";") => return Ok(i + 1),
                Some(t) if t.is(",") => i += 1,
                _ => {
                    return Err(self.error(
                        i,
                        format!(
                            "expected `;` after the declaration of `{}`, found {}",
                            out.last().map(|(d, _)| d.name.as_str()).unwrap_or(""),
                            self.show(i)
                        ),
                    ));
                }
            }
        }
    }

    /// Past an initial value, to the `,` or `;` that ends it.
    fn skip_expression(&self, mut i: usize, to: usize) -> usize {
        while i < to {
            let t = &self.tokens[i];
            if t.is(";") || t.is(",") {
                return i;
            }
            if t.is("(") || t.is("[") || t.is("{") {
                i = self.close(i).unwrap_or(to);
            } else {
                i += 1;
            }
        }
        i
    }

    // ---- types ----

    /// A type starting at `i`, and the index after it.
    fn ty(&self, i: usize) -> Result<(Type, usize)> {
        if self.at(i).is_some_and(|t| t.is_word("array")) {
            let (mut dims, after) = self.sizes(i + 1, "after `array`")?;
            if self.at(after).is_some_and(|t| t.is_word("array")) {
                return Err(self.error(
                    after,
                    "an array of arrays is written with one `array[…, …]`",
                ));
            }
            let (inner, end) = self.element_type(after)?;
            dims.extend(inner.dims);
            return Ok((Type { dims, ..inner }, end));
        }
        self.element_type(i)
    }

    /// A type that is not an array.
    fn element_type(&self, i: usize) -> Result<(Type, usize)> {
        let word = self.at(i).and_then(Token::ident).unwrap_or("");
        let scalar = |element| Type {
            element,
            dims: Vec::new(),
            lower: None,
            upper: None,
        };
        match word {
            "int" | "real" => {
                let element = if word == "int" {
                    Element::Int
                } else {
                    Element::Real
                };
                let (bounds, after) = self.constraint(i + 1)?;
                Ok((bounds.apply(scalar(element)), after))
            }
            "complex" => Ok((scalar(Element::Complex), i + 1)),
            "tuple" => {
                let mut j = self.expect(i + 1, "(", "after `tuple`")?;
                loop {
                    let (_, after) = self.ty(j)?;
                    match self.at(after) {
                        Some(t) if t.is(",") => j = after + 1,
                        Some(t) if t.is(")") => return Ok((scalar(Element::Tuple), after + 1)),
                        _ => {
                            return Err(self.error(
                                after,
                                format!(
                                    "expected `,` or `)` in a tuple type, found {}",
                                    self.show(after)
                                ),
                            ));
                        }
                    }
                }
            }
            "vector" | "row_vector" | "simplex" | "unit_vector" | "sum_to_zero_vector"
            | "ordered" | "positive_ordered" | "complex_vector" | "complex_row_vector" => {
                let (bounds, after) = self.constraint(i + 1)?;
                let (dims, end) = self.sizes(after, &format!("after `{word}`"))?;
                self.arity(i, word, &dims, 1, 1)?;
                Ok((bounds.apply(container(word, dims)), end))
            }
            "matrix"
            | "complex_matrix"
            | "sum_to_zero_matrix"
            | "column_stochastic_matrix"
            | "row_stochastic_matrix" => {
                let (bounds, after) = self.constraint(i + 1)?;
                let (dims, end) = self.sizes(after, &format!("after `{word}`"))?;
                self.arity(i, word, &dims, 2, 2)?;
                Ok((bounds.apply(container(word, dims)), end))
            }
            // Square: one size is both.
            "cov_matrix" | "corr_matrix" | "cholesky_factor_corr" => {
                let (dims, end) = self.sizes(i + 1, &format!("after `{word}`"))?;
                self.arity(i, word, &dims, 1, 1)?;
                let side = dims[0].clone();
                Ok((container(word, vec![side.clone(), side]), end))
            }
            // `[M]` is square; `[M, N]` is M rows by N columns.
            "cholesky_factor_cov" => {
                let (mut dims, end) = self.sizes(i + 1, "after `cholesky_factor_cov`")?;
                self.arity(i, word, &dims, 1, 2)?;
                if dims.len() == 1 {
                    dims.push(dims[0].clone());
                }
                Ok((container(word, dims), end))
            }
            _ => Err(self.error(i, format!("expected a type, found {}", self.show(i)))),
        }
    }

    fn arity(&self, i: usize, word: &str, dims: &[SizeExpr], min: usize, max: usize) -> Result<()> {
        if (min..=max).contains(&dims.len()) {
            return Ok(());
        }
        let wanted = match (min, max) {
            (1, 1) => "one size".to_owned(),
            (2, 2) => "two sizes".to_owned(),
            _ => format!("{min} or {max} sizes"),
        };
        Err(self.error(
            i,
            format!("`{word}` takes {wanted}, and has {}", dims.len()),
        ))
    }

    /// An optional `<lower=…, upper=…, offset=…, multiplier=…>` at `i`.
    fn constraint(&self, i: usize) -> Result<(Bounds, usize)> {
        let mut bounds = Bounds::default();
        if !self.at(i).is_some_and(|t| t.is("<")) {
            return Ok((bounds, i));
        }
        // The closing `>` is the first one outside brackets: a comparison in a
        // bound has to be parenthesised in Stan for exactly this reason.
        let mut j = i + 1;
        let mut part = j;
        loop {
            let Some(t) = self.at(j) else {
                return Err(self.error(i, "this `<` of a type's bounds is never closed with `>`"));
            };
            if t.is(",") || t.is(">") {
                self.bound(part, j, &mut bounds)?;
                if t.is(">") {
                    return Ok((bounds, j + 1));
                }
                part = j + 1;
                j += 1;
            } else if t.is("(") || t.is("[") || t.is("{") {
                j = self.close(j)?;
            } else {
                j += 1;
            }
        }
    }

    /// One `key=expr` of a constraint, the tokens `from..to`.
    fn bound(&self, from: usize, to: usize, bounds: &mut Bounds) -> Result<()> {
        let key = self.at(from).and_then(Token::ident).unwrap_or("");
        if !BOUND_KEYS.contains(&key)
            || !self.at(from + 1).is_some_and(|t| t.is("="))
            || to <= from + 2
        {
            return Err(self.error(
                from,
                format!(
                    "expected `lower=`, `upper=`, `offset=` or `multiplier=` in a type's bounds, \
                     found `{}`",
                    self.text(from, to)
                ),
            ));
        }
        let expr = self.text(from + 2, to);
        match key {
            "lower" => bounds.lower = Some(expr),
            "upper" => bounds.upper = Some(expr),
            _ => {}
        }
        Ok(())
    }

    /// `[size, size, …]` at `i`, and the index after the `]`.
    fn sizes(&self, i: usize, what: &str) -> Result<(Vec<SizeExpr>, usize)> {
        let open = self.expect(i, "[", what)? - 1;
        let close = self.close(open)? - 1;
        if !self.tokens[close].is("]") {
            return Err(self.error(open, "this `[` is closed by something other than `]`"));
        }
        let mut out = Vec::new();
        let mut part = open + 1;
        let mut j = part;
        while j <= close {
            let t = &self.tokens[j];
            if t.is(",") || j == close {
                if j == part {
                    return Err(self.error(j, "a size is missing between the brackets"));
                }
                out.push(self.size(part, j));
                part = j + 1;
                j += 1;
            } else if t.is("(") || t.is("[") || t.is("{") {
                j = self.close(j)?;
            } else {
                j += 1;
            }
        }
        Ok((out, close + 1))
    }

    /// One size expression, the tokens `from..to`: its text, and its tree when
    /// it has one.
    fn size(&self, from: usize, to: usize) -> SizeExpr {
        let text = self.text(from, to);
        let mut parser = SizeParser {
            tokens: &self.tokens[from..to],
            at: 0,
        };
        match parser.sum() {
            Some(tree) if parser.at == parser.tokens.len() => SizeExpr::parsed(text, tree),
            _ => SizeExpr::opaque(text),
        }
    }
}

/// A parsed type, before it has a name.
struct Type {
    element: Element,
    dims: Vec<SizeExpr>,
    lower: Option<String>,
    upper: Option<String>,
}

fn container(word: &str, dims: Vec<SizeExpr>) -> Type {
    Type {
        element: if word.starts_with("complex") {
            Element::Complex
        } else {
            Element::Real
        },
        dims,
        lower: None,
        upper: None,
    }
}

#[derive(Default)]
struct Bounds {
    lower: Option<String>,
    upper: Option<String>,
}

impl Bounds {
    fn apply(self, ty: Type) -> Type {
        Type {
            lower: self.lower,
            upper: self.upper,
            ..ty
        }
    }
}

/// Precedence climbing over the integer size grammar. Anything outside it —
/// a call, a real, an index — makes the whole size opaque (`None`).
struct SizeParser<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl SizeParser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn sum(&mut self) -> Option<SizeTree> {
        let mut left = self.product()?;
        while let Some(op) = self.peek().and_then(|t| match &t.kind {
            Tok::Punct("+") => Some(SizeOp::Add),
            Tok::Punct("-") => Some(SizeOp::Sub),
            _ => None,
        }) {
            self.at += 1;
            left = SizeTree::binary(op, left, self.product()?);
        }
        Some(left)
    }

    fn product(&mut self) -> Option<SizeTree> {
        let mut left = self.unary()?;
        while let Some(op) = self.peek().and_then(|t| match &t.kind {
            Tok::Punct("*") => Some(SizeOp::Mul),
            Tok::Punct("%/%") => Some(SizeOp::IntDiv),
            Tok::Punct("%") => Some(SizeOp::Mod),
            _ => None,
        }) {
            self.at += 1;
            left = SizeTree::binary(op, left, self.unary()?);
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<SizeTree> {
        match &self.peek()?.kind {
            Tok::Punct("-") => {
                self.at += 1;
                Some(SizeTree::Neg(Box::new(self.unary()?)))
            }
            Tok::Punct("+") => {
                self.at += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Option<SizeTree> {
        let token = self.peek()?.clone();
        self.at += 1;
        match &token.kind {
            Tok::Int(digits) => digits.parse().ok().map(SizeTree::Int),
            // A call is not an identifier: `num_elements(y)` is opaque.
            Tok::Ident(name) if !self.peek().is_some_and(|t| t.is("(")) => {
                Some(SizeTree::Var(name.clone()))
            }
            Tok::Punct("(") => {
                let inner = self.sum()?;
                if !self.peek()?.is(")") {
                    return None;
                }
                self.at += 1;
                Some(inner)
            }
            _ => None,
        }
    }
}
