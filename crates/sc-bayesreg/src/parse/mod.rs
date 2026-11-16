//! Formula text → [`Formula`] (TODO-bayesian-regression §3, R1.2).
//!
//! Three layers: [`lex`] turns the text into tokens and statements, [`expr`]
//! parses each statement as an R expression with R's precedence, and
//! [`formula`] gives the expression its formula meaning — the response and its
//! addition terms, the population-level terms expanded by R's `?formula`
//! rules, the group-level terms, the distributional-parameter lines, `bf()`
//! unwrapped, and pasted `prior(…)` calls lifted out.
//!
//! Nothing here knows the dataset. Every error is a sentence with the span of
//! the text it is about, so the editor can underline it.

pub(crate) mod expr;
mod formula;
pub(crate) mod lex;

pub use formula::{
    ATerm, Bound, Censoring, FamilySpec, FixedDpar, Formula, GroupTerm, Intercept, Lhs, Line,
    NL_FUNCTIONS, NlExpr, Overrides, PastedPrior, Rhs, Term, parse_formula, strip_priors,
};

/// A range of the formula text: byte offsets, `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Span {
    /// The first byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
}

impl Span {
    /// `start..end`.
    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    /// From the start of `self` to the end of `other`.
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// Shifted by `by` bytes — a span inside a string literal, made relative
    /// to the whole text.
    pub fn shift(self, by: usize) -> Span {
        Span::new(self.start + by, self.end + by)
    }
}

/// A mistake in the formula text: a sentence, and what it is about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParseError {
    /// The sentence.
    pub message: String,
    /// The text it is about (bytes).
    pub span: Span,
    /// Where that is for an editor: 1-based line, and 1-based columns in
    /// characters (Unicode scalar values) on that line. Filled in by
    /// [`ParseError::locate`].
    #[serde(default)]
    pub line: usize,
    /// The first column.
    #[serde(default)]
    pub column: usize,
    /// One past the last column (on the first line, for a span over several).
    #[serde(default)]
    pub end_column: usize,
}

impl ParseError {
    pub(crate) fn new(message: impl Into<String>, span: Span) -> ParseError {
        ParseError {
            message: message.into(),
            span,
            line: 0,
            column: 0,
            end_column: 0,
        }
    }

    /// The line and columns of the span in `text`.
    pub fn locate(mut self, text: &str) -> ParseError {
        let start = floor_boundary(text, self.span.start.min(text.len()));
        let end = floor_boundary(text, self.span.end.min(text.len()).max(start));
        let before = &text[..start];
        self.line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        self.column = text[line_start..start].chars().count() + 1;
        let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
        let end = end.min(line_end).max(start);
        self.end_column = self.column + text[start..end].chars().count().max(1);
        self
    }
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(
                f,
                "line {}, column {}: {}",
                self.line, self.column, self.message
            )
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for ParseError {}

/// Every mistake found, each located. A formula with one bad line still
/// reports the others.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParseErrors(pub Vec<ParseError>);

impl std::fmt::Display for ParseErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        f.write_str(&parts.join("; "))
    }
}

impl std::error::Error for ParseErrors {}

impl From<ParseErrors> for sc_error::Error {
    fn from(e: ParseErrors) -> sc_error::Error {
        sc_error::Error::invalid(format!("the formula: {e}"))
    }
}
