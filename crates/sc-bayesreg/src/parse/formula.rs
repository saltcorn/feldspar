//! What a parsed statement means as a formula.
//!
//! The population-level expansion follows R's documented formula semantics
//! (`?formula`; Chambers & Hastie 1992, §2.3): `a + b` is the union of the
//! terms, `a:b` their pairwise products, `a * b` = `a + b + a:b`, `a / b` =
//! `a + (all of a):b`, `a %in% b` each term of `a` with all of `b`, `(…)^k`
//! the products up to order *k*, `- t` removes `t`, and `0`/`-1`/`1` turn the
//! intercept off and on. The terms are ordered by their order (main effects,
//! then two-way interactions, …), ties kept in the order written, and the
//! variables of a term in the order they first appear in the formula — so the
//! term labels and column names are R's.
//!
//! The group-level syntax is lme4's (Bates et al. 2015, *JSS* 67(1), §2.1):
//! `(lhs | g)`, `(lhs || g)` for uncorrelated effects, `g1:g2` and `g1/g2`;
//! with brms's documented extensions `(lhs | ID | g)` and `gr(g, cor, id)`
//! (`?brmsformula`, `?gr`). The response's addition terms are brms's
//! (`?addition-terms`).

use std::collections::BTreeSet;

use sc_model::terms::{Function, Op, VarExpr};

use super::expr::{Arg, BinOp, Expr, expression, statements};
use super::lex::lex;
use super::{ParseError, ParseErrors, Span};
use crate::prior::{Prior, PriorRow};

/// Everything the formula text says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Formula {
    /// The formula lines, in the order written.
    pub lines: Vec<Line>,
    /// `prior(…)`/`set_prior(…)` calls pasted into the text, as rows.
    pub priors: Vec<PastedPrior>,
    /// What a `bf(…)` wrapper's arguments set.
    pub overrides: Overrides,
    /// The spans of the statements that were prior calls, for
    /// [`strip_priors`].
    pub prior_spans: Vec<Span>,
    /// Distributional parameters fixed to a number: `nu = 4` (R2.1).
    pub fixed: Vec<FixedDpar>,
}

/// `nu = 4`: a distributional parameter fixed rather than estimated.
#[derive(Debug, Clone, PartialEq)]
pub struct FixedDpar {
    /// The parameter.
    pub dpar: String,
    /// Its value.
    pub value: f64,
    /// Where it is written.
    pub span: Span,
    /// The `bf(…)` it was written in, if any.
    pub group: Option<usize>,
}

impl Formula {
    /// The line whose response is the model's (the first response line).
    pub fn main(&self) -> Option<&Line> {
        self.lines
            .iter()
            .find(|l| matches!(l.lhs, Lhs::Response { .. }))
    }
}

/// What a `bf(…)` wrapper's named arguments set, which override the form.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overrides {
    /// `family = …`.
    pub family: Option<FamilySpec>,
    /// `center = …`.
    pub center: Option<bool>,
    /// `nl = …`: whether the main formula is non-linear (inferred when
    /// absent: it is when another line is the formula of a name it uses).
    pub nl: Option<bool>,
    /// `rescor = …` / `set_rescor(…)`: residual correlations between the
    /// responses of a multivariate model.
    pub rescor: Option<bool>,
    /// The sentences the editor shows: "`bf()` sets the family to `student`".
    pub notes: Vec<String>,
}

/// A family as written: `gaussian`, `"student"`, `binomial("probit")`,
/// `student(link_sigma = "identity")`, `brmsfamily("gaussian")`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FamilySpec {
    /// The family's name as written.
    pub name: String,
    /// Links set with it: `("mu", "probit")`, `("sigma", "identity")`.
    pub links: Vec<(String, String)>,
    /// Where it is written, when it came from the text.
    pub span: Span,
}

/// One formula line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// What it is the formula of.
    pub lhs: Lhs,
    /// The right-hand side.
    pub rhs: Rhs,
    /// The whole line.
    pub span: Span,
    /// Its left-hand side.
    pub lhs_span: Span,
    /// The main formula's right-hand side when it is non-linear (then `rhs`
    /// is empty): an expression over its non-linear parameters and columns.
    pub nl: Option<NlExpr>,
    /// The `bf(…)` it was written in, if any: what ties a distributional
    /// parameter's line to its response in a multivariate model.
    pub group: Option<usize>,
}

/// What a line is the formula of.
#[derive(Debug, Clone, PartialEq)]
pub enum Lhs {
    /// The response, with its addition terms.
    Response {
        /// The response column.
        column: String,
        /// Its addition terms, in the order written.
        aterms: Vec<(ATerm, Span)>,
    },
    /// A distributional parameter: `sigma ~ x`.
    Dpar(String),
    /// A non-linear parameter of the main formula: `a ~ 1 + (1 | g)`.
    Nlpar(String),
}

/// A non-linear formula's right-hand side (R2.2): arithmetic over numbers,
/// non-linear parameters and columns, and a whitelist of functions. Parsed
/// here and re-emitted by the generator; no text of it reaches Stan as
/// written.
#[derive(Debug, Clone, PartialEq)]
pub enum NlExpr {
    /// A number.
    Number(f64),
    /// A non-linear parameter, by name.
    Param(String),
    /// A column, by name.
    Covariate(String),
    /// `-x`.
    Neg(Box<NlExpr>),
    /// `a + b`, `a - b`, `a * b`, `a / b`, `a ^ b`.
    Binary {
        /// `+ - * / ^`.
        op: char,
        /// The left operand.
        lhs: Box<NlExpr>,
        /// The right operand.
        rhs: Box<NlExpr>,
    },
    /// A whitelisted function.
    Call {
        /// Its name, one of [`NL_FUNCTIONS`].
        function: &'static str,
        /// Its arguments.
        args: Vec<NlExpr>,
    },
}

/// The functions a non-linear formula may call, with their arities: Stan's
/// own names, so each is emitted as itself.
pub const NL_FUNCTIONS: &[(&str, usize)] = &[
    ("exp", 1),
    ("log", 1),
    ("log1p", 1),
    ("expm1", 1),
    ("log1p_exp", 1),
    ("sqrt", 1),
    ("square", 1),
    ("inv", 1),
    ("inv_logit", 1),
    ("logit", 1),
    ("inv_cloglog", 1),
    ("Phi", 1),
    ("Phi_approx", 1),
    ("abs", 1),
    ("sin", 1),
    ("cos", 1),
    ("tan", 1),
    ("tanh", 1),
    ("atan", 1),
    ("fmin", 2),
    ("fmax", 2),
    ("pow", 2),
];

impl NlExpr {
    /// The names of its non-linear parameters and its columns, each in order
    /// of first appearance.
    pub fn names(&self) -> (Vec<String>, Vec<String>) {
        fn go(e: &NlExpr, params: &mut Vec<String>, covs: &mut Vec<String>) {
            match e {
                NlExpr::Number(_) => {}
                NlExpr::Param(n) => {
                    if !params.contains(n) {
                        params.push(n.clone());
                    }
                }
                NlExpr::Covariate(n) => {
                    if !covs.contains(n) {
                        covs.push(n.clone());
                    }
                }
                NlExpr::Neg(x) => go(x, params, covs),
                NlExpr::Binary { lhs, rhs, .. } => {
                    go(lhs, params, covs);
                    go(rhs, params, covs);
                }
                NlExpr::Call { args, .. } => {
                    for a in args {
                        go(a, params, covs);
                    }
                }
            }
        }
        let (mut p, mut c) = (Vec::new(), Vec::new());
        go(self, &mut p, &mut c);
        (p, c)
    }
}

/// The names that are a distributional parameter on a line's left-hand side,
/// across brms's families (`?brmsfamily`). Whether the model's family has it is
/// the resolver's question.
pub const DPAR_NAMES: &[&str] = &[
    "sigma", "shape", "nu", "phi", "kappa", "beta", "alpha", "zi", "hu", "zoi", "coi", "disc",
    "bs", "ndt", "bias", "quantile", "xi", "ndt",
];

/// A response addition term (`?addition-terms`).
#[derive(Debug, Clone, PartialEq)]
pub enum ATerm {
    /// `weights(w)`.
    Weights(Bound),
    /// `trials(n)`.
    Trials(Bound),
    /// `cens(c)` or `cens(c, y2)`.
    Cens(Censoring),
    /// `trunc(lb = , ub = )`.
    Trunc {
        /// The lower bound.
        lb: Option<Bound>,
        /// The upper bound.
        ub: Option<Bound>,
    },
    /// `se(s, sigma = FALSE)`.
    Se {
        /// The column of known standard errors.
        column: String,
        /// Whether a residual SD is estimated as well.
        sigma: bool,
    },
    /// `rate(d)`.
    Rate(Bound),
    /// `thres(k)`: the number of thresholds of an ordinal response (R2.1).
    Thres(usize),
    /// `subset(s)`: the rows (where the yes/no column `s` is true) this
    /// response is measured on, in a multivariate model (R2.3).
    Subset(String),
}

impl ATerm {
    /// Its name: `weights`, `cens`.
    pub fn name(&self) -> &'static str {
        match self {
            ATerm::Weights(_) => "weights",
            ATerm::Trials(_) => "trials",
            ATerm::Cens(_) => "cens",
            ATerm::Trunc { .. } => "trunc",
            ATerm::Se { .. } => "se",
            ATerm::Rate(_) => "rate",
            ATerm::Thres(_) => "thres",
            ATerm::Subset(_) => "subset",
        }
    }

    /// The columns it reads.
    pub fn columns(&self) -> Vec<&str> {
        fn bound(b: &Bound) -> Option<&str> {
            match b {
                Bound::Column(c) => Some(c.as_str()),
                Bound::Number(_) => None,
            }
        }
        match self {
            ATerm::Weights(b) | ATerm::Trials(b) | ATerm::Rate(b) => bound(b).into_iter().collect(),
            ATerm::Cens(c) => std::iter::once(c.column.as_str())
                .chain(c.upper.as_deref())
                .collect(),
            ATerm::Trunc { lb, ub } => lb.iter().chain(ub).filter_map(bound).collect(),
            ATerm::Se { column, .. } | ATerm::Subset(column) => vec![column],
            ATerm::Thres(_) => Vec::new(),
        }
    }
}

/// An addition term's argument: a column, or a number.
#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    /// A column of the dataset.
    Column(String),
    /// The same number for every row.
    Number(f64),
}

/// `cens(c, y2)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Censoring {
    /// The column saying how each row is censored.
    pub column: String,
    /// For interval censoring, the upper end.
    pub upper: Option<String>,
}

/// A right-hand side.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rhs {
    /// The intercept.
    pub intercept: Intercept,
    /// The population-level terms, in R's order.
    pub terms: Vec<Term>,
    /// `offset(…)`s.
    pub offsets: Vec<VarExpr>,
    /// The group-level terms, in the order written.
    pub groups: Vec<GroupTerm>,
    /// `mo(x)`: monotonic effects of ordinal columns (R2.1), in the order
    /// written.
    pub mo: Vec<String>,
    /// `cs(…)`: category-specific effects (R2.1, ordinal families).
    pub cs: Vec<Term>,
}

/// Whether a right-hand side has an intercept, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Intercept {
    /// The default: an intercept (centred, for brms).
    #[default]
    On,
    /// `0 +` or `- 1`.
    Off,
    /// `0 + Intercept`: an intercept that is an ordinary coefficient, not
    /// centred.
    Coefficient,
}

/// A population-level term: a product of variables.
#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    /// Its variables, in the order they first appear in the formula.
    pub variables: Vec<VarExpr>,
}

impl Term {
    /// `area`, `area:region`.
    pub fn label(&self) -> String {
        self.variables
            .iter()
            .map(VarExpr::label)
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// A group-level term: `(1 + x | g)`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupTerm {
    /// The grouping columns: one, or several for `g1:g2`.
    pub factors: Vec<String>,
    /// The intercept of the varying part.
    pub intercept: Intercept,
    /// Its terms.
    pub terms: Vec<Term>,
    /// Correlated (`|`) or not (`||`, `gr(cor = FALSE)`).
    pub correlated: bool,
    /// `|ID|` or `gr(id = )`.
    pub id: Option<String>,
    /// `gr(by = h)`: one covariance matrix per level of `h` (R2.1).
    pub by: Option<String>,
    /// Where it is written.
    pub span: Span,
}

impl GroupTerm {
    /// The grouping factor's label: `county`, `school:class`.
    pub fn group(&self) -> String {
        self.factors.join(":")
    }
}

/// A prior pasted into the formula text.
#[derive(Debug, Clone, PartialEq)]
pub struct PastedPrior {
    /// The row it sets.
    pub row: PriorRow,
    /// Where it was written.
    pub span: Span,
}

/// Parse formula text. Every statement is parsed, and every mistake is
/// reported, located.
pub fn parse_formula(text: &str) -> Result<Formula, ParseErrors> {
    let tokens = lex(text).map_err(|e| ParseErrors(vec![e.locate(text)]))?;
    let mut formula = Formula::default();
    let mut errors = Vec::new();
    let mut raw: Vec<RawLine> = Vec::new();
    let reader = Reader {
        text,
        groups: std::cell::Cell::new(0),
    };
    for statement in statements(&tokens) {
        let result = statement.and_then(|e| reader.statement(&e, &mut formula, &mut raw));
        if let Err(e) = result {
            errors.push(e.locate(text));
        }
    }
    let nlpars = reader.nlpars(&raw, &mut formula);
    for r in &raw {
        match reader.line(&r.lhs, &r.rhs, r.span, r.group, &nlpars) {
            Ok(lines) => formula.lines.extend(lines),
            Err(e) => errors.push(e.locate(text)),
        }
    }
    if errors.is_empty() && formula.main().is_none() {
        let span = Span::new(0, text.len());
        errors.push(
            ParseError::new(
                if formula.lines.is_empty() {
                    "write a formula: the response, `~`, and what it depends on (`y ~ x + (1 | g)`)"
                } else {
                    "no line names the response: every line is a distributional parameter's"
                },
                span,
            )
            .locate(text),
        );
    }
    if errors.is_empty() {
        Ok(formula)
    } else {
        Err(ParseErrors(errors))
    }
}

/// The formula text without its pasted prior calls — what the field keeps once
/// they have moved into the prior table (§3.7) — with the blank lines they
/// leave collapsed.
pub fn strip_priors(text: &str, formula: &Formula) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut spans = formula.prior_spans.clone();
    spans.sort_by_key(|s| s.start);
    for span in spans {
        if span.start >= at {
            out.push_str(&text[at..span.start]);
            at = span.end;
            // A `;` or `+` joining it to the next statement goes with it.
            let rest = &text[at..];
            let trimmed = rest.trim_start_matches([' ', '\t']);
            if trimmed.starts_with(';') {
                at += rest.len() - trimmed.len() + 1;
            }
        }
    }
    out.push_str(&text[at..]);
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines() {
        let blank = line.trim().is_empty();
        if blank && lines.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        lines.push(line.trim_end());
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn err<T>(message: impl Into<String>, span: Span) -> Result<T, ParseError> {
    Err(ParseError::new(message, span))
}

/// The functions that are formula *features* rather than variables, each with
/// what to say about it.
fn unsupported_special(name: &str) -> Option<&'static str> {
    Some(match name {
        "s" | "t2" => "a smoothing spline, which is not supported yet",
        "gp" => "a Gaussian process, which is not supported yet",
        "me" | "mi" => "measurement error and imputation, which are not supported",
        "mm" | "mmc" => "a multi-membership term, which is not supported yet",
        "ar" | "ma" | "arma" | "cosy" | "unstr" | "car" | "sar" | "fcor" => {
            "an autocorrelation term, which is not supported"
        }
        "lf" | "nlf" | "acformula" => "a formula helper that is not supported yet",
        "mvbind" => "a multivariate response, which belongs on the left of `~`",
        "factor" | "as.factor" | "ordered" | "as.numeric" | "as.integer" | "cut" | "ifelse" => {
            "a conversion inside the formula; compute it as a calculated column of the dataset"
        }
        _ => return None,
    })
}

struct Reader<'t> {
    text: &'t str,
    /// How many `bf(…)`s have been read: each groups its lines.
    groups: std::cell::Cell<usize>,
}

/// A formula statement before its meaning is decided: whether the main
/// formula is non-linear depends on every line.
struct RawLine {
    lhs: Expr,
    rhs: Expr,
    span: Span,
    group: Option<usize>,
}

/// The free names of an expression: every name not in a function's place.
fn free_names(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Name(n, _) => {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
        Expr::Num(..) | Expr::Str(..) => {}
        Expr::Call { args, .. } => {
            for a in args {
                free_names(&a.value, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            free_names(lhs, out);
            free_names(rhs, out);
        }
        Expr::Unary { operand, .. } => free_names(operand, out),
        Expr::Paren(inner, _) => free_names(inner, out),
    }
}

impl Reader<'_> {
    fn source(&self, span: Span) -> &str {
        &self.text[span.start.min(self.text.len())..span.end.min(self.text.len())]
    }

    fn statement(
        &self,
        e: &Expr,
        formula: &mut Formula,
        raw: &mut Vec<RawLine>,
    ) -> Result<(), ParseError> {
        match e.unparen() {
            Expr::Binary {
                op: BinOp::Tilde,
                lhs,
                rhs,
                span,
            } => {
                raw.push(RawLine {
                    lhs: (**lhs).clone(),
                    rhs: (**rhs).clone(),
                    span: *span,
                    group: None,
                });
                Ok(())
            }
            Expr::Unary { op: "~", span, .. } => err(
                "this formula has no left-hand side: write the response, or the distributional \
                 parameter, before `~`",
                *span,
            ),
            Expr::Binary {
                op: BinOp::Assign,
                lhs,
                rhs,
                span,
            } => {
                self.fixed(lhs, rhs, *span, None, formula)?;
                Ok(())
            }
            Expr::Call { name, .. } if is_prior_call(name) || name == "c" => {
                self.priors(e, formula)?;
                formula.prior_spans.push(e.span());
                Ok(())
            }
            Expr::Binary {
                op: BinOp::Plus, ..
            } if is_prior_sum(e) => {
                self.priors(e, formula)?;
                formula.prior_spans.push(e.span());
                Ok(())
            }
            Expr::Call {
                name, args, span, ..
            } if name == "bf" || name == "brmsformula" => self.bf(args, *span, formula, raw),
            Expr::Call { name, args, .. } if name == "set_rescor" => {
                self.set_rescor(args, e.span(), formula)
            }
            Expr::Call {
                name, args, span, ..
            } if name == "mvbf" || name == "mvbrmsformula" => {
                for a in args {
                    match (&a.name, a.value.unparen()) {
                        (Some((n, _)), v) if n == "rescor" => {
                            formula.overrides.rescor = Some(logical(v)?);
                        }
                        (None, inner) => self.statement(inner, formula, raw)?,
                        (Some((n, s)), _) => {
                            return err(format!("`mvbf()` has no argument `{n}`"), *s);
                        }
                    }
                }
                let _ = span;
                Ok(())
            }
            Expr::Binary {
                op: BinOp::Plus,
                lhs,
                rhs,
                ..
            } if is_bf_sum(e) => {
                self.statement(lhs, formula, raw)?;
                self.statement(rhs, formula, raw)
            }
            other => err(
                "a line is a formula (`y ~ x`), a `bf(…)`, or a `prior(…)`; this is none of \
                 them",
                other.span(),
            ),
        }
    }

    fn bf(
        &self,
        args: &[Arg],
        span: Span,
        formula: &mut Formula,
        raw: &mut Vec<RawLine>,
    ) -> Result<(), ParseError> {
        let mut any = false;
        let group = self.groups.get();
        self.groups.set(group + 1);
        for arg in args {
            match &arg.name {
                None => match arg.value.unparen() {
                    Expr::Binary {
                        op: BinOp::Tilde,
                        lhs,
                        rhs,
                        span,
                    } => {
                        raw.push(RawLine {
                            lhs: (**lhs).clone(),
                            rhs: (**rhs).clone(),
                            span: *span,
                            group: Some(group),
                        });
                        any = true;
                    }
                    Expr::Binary {
                        op: BinOp::Assign,
                        lhs,
                        rhs,
                        span,
                    } => self.fixed(lhs, rhs, *span, Some(group), formula)?,
                    other => {
                        return err(
                            "each unnamed argument of `bf(…)` is a formula",
                            other.span(),
                        );
                    }
                },
                Some((name, name_span)) => match name.as_str() {
                    "family" => {
                        let family = family_spec(&arg.value)?;
                        formula
                            .overrides
                            .notes
                            .push(format!("`bf()` sets the family to `{}`", family.name));
                        formula.overrides.family = Some(family);
                    }
                    "center" => {
                        let value = logical(&arg.value)?;
                        formula.overrides.notes.push(format!(
                            "`bf()` sets `center` to {}",
                            if value { "TRUE" } else { "FALSE" }
                        ));
                        formula.overrides.center = Some(value);
                    }
                    "nl" => {
                        formula.overrides.nl = Some(logical(&arg.value)?);
                    }
                    "rescor" => {
                        formula.overrides.rescor = Some(logical(&arg.value)?);
                    }
                    // `bf(sigma ~ x, nu = 4)`: R lets a fixed parameter be a
                    // named argument too.
                    d if DPAR_NAMES.contains(&d) => {
                        let value = arg.value.as_number().ok_or_else(|| {
                            ParseError::new(
                                format!("`{d} = …` fixes `{d}` to a number"),
                                arg.value.span(),
                            )
                        })?;
                        formula.fixed.push(FixedDpar {
                            dpar: d.to_owned(),
                            value,
                            span: name_span.to(arg.value.span()),
                            group: Some(group),
                        });
                    }
                    other => {
                        return err(
                            format!("`bf()`'s `{other}` argument is not supported"),
                            *name_span,
                        );
                    }
                },
            }
        }
        if any {
            Ok(())
        } else {
            err("`bf(…)` holds no formula", span)
        }
    }

    /// `dpar = number`.
    fn fixed(
        &self,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        group: Option<usize>,
        formula: &mut Formula,
    ) -> Result<(), ParseError> {
        match lhs.as_name() {
            Some(name) if DPAR_NAMES.contains(&name) => {
                let Some(value) = rhs.as_number() else {
                    return err(
                        format!(
                            "`{name} = …` fixes `{name}` to a number; for a formula, write \
                             `{name} ~ …`"
                        ),
                        rhs.span(),
                    );
                };
                formula.fixed.push(FixedDpar {
                    dpar: name.to_owned(),
                    value,
                    span,
                    group,
                });
                Ok(())
            }
            _ => err(
                "an assignment is not a formula; write `response ~ terms`, or fix a \
                 distributional parameter with `nu = 4`",
                span,
            ),
        }
    }

    fn set_rescor(
        &self,
        args: &[Arg],
        span: Span,
        formula: &mut Formula,
    ) -> Result<(), ParseError> {
        match args {
            [a] if a.name.as_ref().is_none_or(|(n, _)| n == "rescor") => {
                formula.overrides.rescor = Some(logical(&a.value)?);
                Ok(())
            }
            _ => err("`set_rescor()` takes `TRUE` or `FALSE`", span),
        }
    }

    /// The non-linear parameters: the names the main formula uses that are
    /// the left-hand side of another line (R2.2). With `nl = FALSE` none; with
    /// `nl = TRUE` every such line must be one.
    fn nlpars(&self, raw: &[RawLine], formula: &mut Formula) -> Vec<String> {
        if formula.overrides.nl == Some(false) {
            return Vec::new();
        }
        let is_response = |r: &RawLine| match r.lhs.unparen() {
            Expr::Name(n, _) => !DPAR_NAMES.contains(&n.as_str()),
            _ => true,
        };
        let Some(main_at) = raw.iter().position(is_response) else {
            return Vec::new();
        };
        let mut used = Vec::new();
        free_names(&raw[main_at].rhs, &mut used);
        let nlpars: Vec<String> = raw
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != main_at)
            .filter_map(|(_, r)| r.lhs.as_name())
            .filter(|n| !DPAR_NAMES.contains(n) && used.iter().any(|u| u == n))
            .map(str::to_owned)
            .collect();
        if !nlpars.is_empty() && formula.overrides.nl.is_none() {
            formula.overrides.notes.push(format!(
                "the formula is non-linear: {} {} its parameters",
                nlpars
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if nlpars.len() == 1 { "is" } else { "are" }
            ));
        }
        nlpars
    }

    /// One statement `lhs ~ rhs` as its lines: one, or one per response of
    /// `mvbind(…)`.
    fn line(
        &self,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        group: Option<usize>,
        nlpars: &[String],
    ) -> Result<Vec<Line>, ParseError> {
        let lhs_span = lhs.span();
        let lhs = match lhs.unparen() {
            Expr::Name(name, _) if DPAR_NAMES.contains(&name.as_str()) => Lhs::Dpar(name.clone()),
            Expr::Name(name, _) if nlpars.contains(name) => Lhs::Nlpar(name.clone()),
            Expr::Name(name, _) => Lhs::Response {
                column: name.clone(),
                aterms: Vec::new(),
            },
            Expr::Call {
                name, args, span, ..
            } if name == "mvbind" => {
                if !nlpars.is_empty() {
                    return err(
                        "a non-linear formula has one response; write it without `mvbind`",
                        *span,
                    );
                }
                let rhs_parsed = self.rhs(rhs, true)?;
                let mut out = Vec::new();
                for a in args {
                    if a.name.is_some() {
                        return err("`mvbind()` takes the response columns", a.value.span());
                    }
                    out.push(Line {
                        lhs: Lhs::Response {
                            column: response_column(&a.value)?,
                            aterms: Vec::new(),
                        },
                        rhs: rhs_parsed.clone(),
                        span: *span,
                        lhs_span: a.value.span(),
                        nl: None,
                        group,
                    });
                }
                if out.len() < 2 {
                    return err("`mvbind()` binds two responses or more", *span);
                }
                return Ok(out);
            }
            Expr::Binary {
                op: BinOp::Bar,
                lhs: response,
                rhs: aterms,
                ..
            } => {
                let column = response_column(response)?;
                let mut list = Vec::new();
                for (call, span) in plus_items(aterms) {
                    list.push((aterm(call)?, span));
                }
                let mut seen = BTreeSet::new();
                for (a, span) in &list {
                    if !seen.insert(a.name()) {
                        return err(format!("`{}()` is given twice", a.name()), *span);
                    }
                }
                Lhs::Response {
                    column,
                    aterms: list,
                }
            }
            other => Lhs::Response {
                column: response_column(other)?,
                aterms: Vec::new(),
            },
        };
        // The main formula is non-linear when it uses a non-linear parameter.
        let main_nl = matches!(lhs, Lhs::Response { .. }) && !nlpars.is_empty() && {
            let mut used = Vec::new();
            free_names(rhs, &mut used);
            used.iter().any(|u| nlpars.contains(u))
        };
        let (rhs, nl) = if main_nl {
            (Rhs::default(), Some(self.nl_expr(rhs, nlpars)?))
        } else {
            (self.rhs(rhs, true)?, None)
        };
        Ok(vec![Line {
            lhs,
            rhs,
            span,
            lhs_span,
            nl,
            group,
        }])
    }

    /// A non-linear right-hand side as an [`NlExpr`].
    fn nl_expr(&self, e: &Expr, nlpars: &[String]) -> Result<NlExpr, ParseError> {
        Ok(match e {
            Expr::Paren(inner, _) => self.nl_expr(inner, nlpars)?,
            Expr::Num(x, _) => NlExpr::Number(*x),
            Expr::Name(n, span) => {
                if matches!(
                    n.as_str(),
                    "TRUE" | "FALSE" | "T" | "F" | "NA" | "NULL" | "."
                ) {
                    return err(format!("`{n}` is not a column"), *span);
                }
                if nlpars.contains(n) {
                    NlExpr::Param(n.clone())
                } else {
                    NlExpr::Covariate(n.clone())
                }
            }
            Expr::Str(_, span) => return err("a string is not part of a formula", *span),
            Expr::Unary {
                op: "-", operand, ..
            } => match self.nl_expr(operand, nlpars)? {
                NlExpr::Number(x) => NlExpr::Number(-x),
                x => NlExpr::Neg(Box::new(x)),
            },
            Expr::Unary {
                op: "+", operand, ..
            } => self.nl_expr(operand, nlpars)?,
            Expr::Unary { span, .. } => return err("a second `~` in one formula", *span),
            Expr::Binary { op, lhs, rhs, span } => {
                let op = match op {
                    BinOp::Plus => '+',
                    BinOp::Minus => '-',
                    BinOp::Times => '*',
                    BinOp::Divide => '/',
                    BinOp::Power => '^',
                    BinOp::Bar | BinOp::DoubleBar => {
                        return err(
                            "a non-linear formula has no group-level terms of its own; put them \
                             in a parameter's formula (`a ~ 1 + (1 | g)`)",
                            *span,
                        );
                    }
                    other => {
                        return err(
                            format!(
                                "`{}` is not arithmetic, and a non-linear formula is arithmetic",
                                other.symbol()
                            ),
                            *span,
                        );
                    }
                };
                NlExpr::Binary {
                    op,
                    lhs: Box::new(self.nl_expr(lhs, nlpars)?),
                    rhs: Box::new(self.nl_expr(rhs, nlpars)?),
                }
            }
            Expr::Call {
                name,
                args,
                span,
                name_span,
            } => {
                let name = match name.as_str() {
                    "fabs" => "abs",
                    "plogis" => "inv_logit",
                    "qlogis" => "logit",
                    "pnorm" => "Phi",
                    other => other,
                };
                let Some(&(function, arity)) = NL_FUNCTIONS.iter().find(|(f, _)| *f == name) else {
                    return err(
                        format!(
                            "`{name}()` is not a function a non-linear formula can use (they are \
                             {})",
                            NL_FUNCTIONS
                                .iter()
                                .map(|(f, _)| format!("`{f}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        *name_span,
                    );
                };
                if args.len() != arity || args.iter().any(|a| a.name.is_some()) {
                    return err(
                        format!(
                            "`{function}()` takes {arity} argument{}, without names",
                            if arity == 1 { "" } else { "s" }
                        ),
                        *span,
                    );
                }
                NlExpr::Call {
                    function,
                    args: args
                        .iter()
                        .map(|a| self.nl_expr(&a.value, nlpars))
                        .collect::<Result<_, _>>()?,
                }
            }
        })
    }

    fn priors(&self, e: &Expr, formula: &mut Formula) -> Result<(), ParseError> {
        match e.unparen() {
            Expr::Call { name, args, .. } if name == "c" => {
                for a in args {
                    if a.name.is_some() {
                        return err("`c(…)` of priors takes no names", a.value.span());
                    }
                    self.priors(&a.value, formula)?;
                }
                Ok(())
            }
            Expr::Binary {
                op: BinOp::Plus,
                lhs,
                rhs,
                ..
            } => {
                self.priors(lhs, formula)?;
                self.priors(rhs, formula)
            }
            Expr::Call {
                name, args, span, ..
            } if is_prior_call(name) => {
                let row = prior_call(name, args, *span)?;
                formula.priors.push(PastedPrior { row, span: *span });
                Ok(())
            }
            other => err(
                format!(
                    "`{}` is not a prior; a prior is `prior(…)` or `set_prior(\"…\", …)`",
                    self.source(other.span())
                ),
                other.span(),
            ),
        }
    }

    /// A right-hand side. `top` is false for a group-level term's own terms,
    /// where `offset`, a nested group and `Intercept` are refused.
    fn rhs(&self, e: &Expr, top: bool) -> Result<Rhs, ParseError> {
        let mut reg = Registry::default();
        let x = self.expand(e, &mut reg, top)?;
        let mut terms = x.terms;
        // R's order: by the number of variables, ties as written.
        terms.sort_by_key(BTreeSet::len);
        let intercept = match (x.intercept, x.intercept_coef) {
            (_, true) => {
                if x.intercept == Some(true) {
                    return err(
                        "`Intercept` is the intercept as a coefficient, which needs `0 +` (`0 + \
                         Intercept + x`)",
                        e.span(),
                    );
                }
                Intercept::Coefficient
            }
            (Some(false), false) => Intercept::Off,
            _ => Intercept::On,
        };
        Ok(Rhs {
            intercept,
            terms: terms
                .into_iter()
                .map(|set| Term {
                    variables: set.into_iter().map(|i| reg.vars[i].clone()).collect(),
                })
                .collect(),
            offsets: x.offsets,
            groups: x.groups,
            mo: x.mo,
            cs: x.cs,
        })
    }

    #[allow(clippy::too_many_lines)] // one arm per formula operator, as `?formula` lists them
    fn expand(&self, e: &Expr, reg: &mut Registry, top: bool) -> Result<Expansion, ParseError> {
        match e {
            Expr::Paren(inner, span) => match inner.unparen() {
                Expr::Binary {
                    op: BinOp::Bar | BinOp::DoubleBar,
                    ..
                } => {
                    if !top {
                        return err("a group-level term cannot be inside another one", *span);
                    }
                    Ok(Expansion {
                        groups: self.group(inner.unparen(), *span)?,
                        ..Expansion::default()
                    })
                }
                _ => self.expand(inner, reg, top),
            },
            Expr::Num(x, span) => match x {
                x if *x == 1.0 => Ok(Expansion::intercept(true)),
                x if *x == 0.0 => Ok(Expansion::intercept(false)),
                _ => err(
                    format!(
                        "`{}` is not a term: only `1` (an intercept) and `0` (none) are numbers \
                         in a formula; for arithmetic, use `I(…)`",
                        self.source(*span)
                    ),
                    *span,
                ),
            },
            Expr::Str(_, span) => err("a string is not a term", *span),
            Expr::Name(name, span) => {
                if name == "." {
                    return err(
                        "`.` (every other column) is not supported; list the columns",
                        *span,
                    );
                }
                if name == "Intercept" && top {
                    return Ok(Expansion {
                        intercept_coef: true,
                        ..Expansion::default()
                    });
                }
                if matches!(name.as_str(), "TRUE" | "FALSE" | "T" | "F" | "NA" | "NULL") {
                    return err(format!("`{name}` is not a column"), *span);
                }
                Ok(Expansion::variable(reg.add(VarExpr::column(name.clone()))))
            }
            Expr::Call {
                name,
                args,
                span,
                name_span,
            } => {
                if name == "offset" {
                    if !top {
                        return err("an `offset()` belongs to the population-level part", *span);
                    }
                    let [arg] = args.as_slice() else {
                        return err("`offset()` takes one argument", *span);
                    };
                    let v = self.arithmetic(&arg.value)?;
                    return Ok(Expansion {
                        offsets: vec![v],
                        ..Expansion::default()
                    });
                }
                if name == "mo" || name == "cs" {
                    if !top {
                        return err(
                            format!(
                                "`{name}()` belongs to the population-level part, not inside a \
                                 group-level term"
                            ),
                            *span,
                        );
                    }
                    if name == "mo" {
                        let col = match args.as_slice() {
                            [a] if a.name.is_none() => a.value.as_name().map(str::to_owned),
                            _ => None,
                        };
                        let Some(col) = col else {
                            return err("`mo()` takes one ordinal column: `mo(education)`", *span);
                        };
                        return Ok(Expansion {
                            mo: vec![col],
                            ..Expansion::default()
                        });
                    }
                    let [arg] = args.as_slice() else {
                        return err("`cs()` takes the terms that vary by category", *span);
                    };
                    let inner = self.rhs(&arg.value, false)?;
                    return Ok(Expansion {
                        cs: inner.terms,
                        ..Expansion::default()
                    });
                }
                if name == "gr" || name == "mm" {
                    return err(
                        format!("`{name}(…)` belongs on the right of a `|`: `(1 | {name}(…))`"),
                        *span,
                    );
                }
                if let Some(what) = unsupported_special(name) {
                    return err(format!("`{name}()` is {what}"), *name_span);
                }
                let v = self.variable(e)?;
                Ok(Expansion::variable(reg.add(v)))
            }
            Expr::Unary {
                op: "-", operand, ..
            } => {
                let removed = self.expand(operand, reg, top)?;
                Ok(Expansion::default().minus(&removed))
            }
            Expr::Unary {
                op: "+", operand, ..
            } => self.expand(operand, reg, top),
            Expr::Unary { span, .. } => err("a second `~` in one formula", *span),
            Expr::Binary { op, lhs, rhs, span } => match op {
                BinOp::Plus => {
                    let l = self.expand(lhs, reg, top)?;
                    let r = self.expand(rhs, reg, top)?;
                    Ok(l.plus(r))
                }
                BinOp::Minus => {
                    let l = self.expand(lhs, reg, top)?;
                    let r = self.expand(rhs, reg, top)?;
                    if !r.groups.is_empty() || !r.offsets.is_empty() {
                        return err(
                            "a group-level term or an offset cannot be removed with `-`",
                            rhs.span(),
                        );
                    }
                    Ok(l.minus(&r))
                }
                BinOp::Times => {
                    let l = self.plain(lhs, reg, top)?;
                    let r = self.plain(rhs, reg, top)?;
                    let product = l.product(&r);
                    Ok(l.plus(r).plus(product))
                }
                BinOp::Colon => {
                    let l = self.plain(lhs, reg, top)?;
                    let r = self.plain(rhs, reg, top)?;
                    Ok(l.product(&r))
                }
                BinOp::Divide => {
                    let l = self.plain(lhs, reg, top)?;
                    let r = self.plain(rhs, reg, top)?;
                    let all = Expansion {
                        terms: vec![l.all_variables()],
                        ..Expansion::default()
                    };
                    let nested = all.product(&r);
                    Ok(l.plus(nested))
                }
                BinOp::In => {
                    let l = self.plain(lhs, reg, top)?;
                    let r = self.plain(rhs, reg, top)?;
                    let all = Expansion {
                        terms: vec![r.all_variables()],
                        ..Expansion::default()
                    };
                    Ok(l.product(&all))
                }
                BinOp::Power => {
                    let l = self.plain(lhs, reg, top)?;
                    let k = rhs.as_number().filter(|k| k.fract() == 0.0 && *k >= 1.0);
                    let Some(k) = k else {
                        return err(
                            "the power of terms (`(a + b)^2`) must be a whole number; for \
                             arithmetic, use `I(x^2)`",
                            rhs.span(),
                        );
                    };
                    let mut out = l.clone();
                    let mut i = 1.0;
                    while i < k {
                        let product = out.product(&l);
                        out = out.plus(l.clone()).plus(product);
                        i += 1.0;
                    }
                    Ok(out)
                }
                BinOp::Bar | BinOp::DoubleBar => err(
                    "a group-level term needs parentheses of its own: `(1 | g)`",
                    *span,
                ),
                BinOp::Tilde => err("a second `~` in one formula", *span),
                BinOp::Assign => err("`=` is not part of a formula", *span),
            },
        }
    }

    /// An expansion that may only hold terms: the operands of `:`, `*`, `/`,
    /// `%in%` and `^`.
    fn plain(&self, e: &Expr, reg: &mut Registry, top: bool) -> Result<Expansion, ParseError> {
        let x = self.expand(e, reg, top)?;
        if !x.groups.is_empty() || !x.offsets.is_empty() {
            return err(
                "a group-level term or an offset cannot be part of an interaction",
                e.span(),
            );
        }
        if !x.mo.is_empty() || !x.cs.is_empty() {
            return err(
                "`mo()` and `cs()` cannot be part of an interaction",
                e.span(),
            );
        }
        if x.intercept_coef {
            return err("`Intercept` cannot be part of an interaction", e.span());
        }
        Ok(x)
    }

    /// A variable: a column or a whitelisted transform.
    fn variable(&self, e: &Expr) -> Result<VarExpr, ParseError> {
        let v = match e.unparen() {
            Expr::Name(n, _) => VarExpr::column(n.clone()),
            Expr::Call {
                name,
                args,
                span,
                name_span,
            } => {
                let Some(function) = Function::parse(name) else {
                    return err(
                        format!(
                            "`{name}()` is not a transform a formula can use (they are `log`, \
                             `log1p`, `exp`, `sqrt`, `abs`, `inv`, `scale`, `poly` and `I`); \
                             compute it as a calculated column of the dataset"
                        ),
                        *name_span,
                    );
                };
                match function {
                    Function::Poly => {
                        let mut x = None;
                        let mut degree = None;
                        for (i, a) in args.iter().enumerate() {
                            match (a.name.as_ref().map(|(n, _)| n.as_str()), i) {
                                (Some("x") | None, 0) => x = Some(self.arithmetic(&a.value)?),
                                (Some("degree") | None, 1) | (Some("degree"), _) => {
                                    degree = Some(a.value.as_number().ok_or_else(|| {
                                        ParseError::new(
                                            "the degree of `poly` is a number",
                                            a.value.span(),
                                        )
                                    })?);
                                }
                                (Some("raw"), _) => {
                                    return err(
                                        "`raw` polynomials are not supported; `poly` is \
                                         orthogonal, or write `x + I(x^2)`",
                                        a.value.span(),
                                    );
                                }
                                _ => {
                                    return err(
                                        "`poly` takes a variable and its degree",
                                        a.value.span(),
                                    );
                                }
                            }
                        }
                        let (Some(x), Some(k)) = (x, degree) else {
                            return err(
                                "`poly` takes a variable and its degree: `poly(x, 2)`",
                                *span,
                            );
                        };
                        VarExpr::call(Function::Poly, vec![x, VarExpr::Number(k)])
                    }
                    _ => {
                        let [arg] = args.as_slice() else {
                            return err(format!("`{name}()` takes one argument"), *span);
                        };
                        if arg.name.is_some() {
                            return err(
                                format!("`{name}()` takes its argument without a name"),
                                arg.value.span(),
                            );
                        }
                        VarExpr::call(function, vec![self.arithmetic(&arg.value)?])
                    }
                }
            }
            other => return self.arithmetic(other),
        };
        v.check()
            .map_err(|problem| ParseError::new(problem.to_string(), e.span()))?;
        Ok(v)
    }

    /// Arithmetic inside a transform: numbers, columns, `+ - * / ^`, and the
    /// whitelisted functions.
    fn arithmetic(&self, e: &Expr) -> Result<VarExpr, ParseError> {
        Ok(match e.unparen() {
            Expr::Name(n, _) => VarExpr::column(n.clone()),
            Expr::Num(x, _) => VarExpr::Number(*x),
            Expr::Call { .. } => self.variable(e)?,
            Expr::Unary {
                op: "-", operand, ..
            } => match self.arithmetic(operand)? {
                VarExpr::Number(x) => VarExpr::Number(-x),
                v => VarExpr::Op {
                    op: Op::Neg,
                    args: vec![v],
                },
            },
            Expr::Unary {
                op: "+", operand, ..
            } => self.arithmetic(operand)?,
            Expr::Binary { op, lhs, rhs, span } => {
                let op = match op {
                    BinOp::Plus => Op::Add,
                    BinOp::Minus => Op::Sub,
                    BinOp::Times => Op::Mul,
                    BinOp::Divide => Op::Div,
                    BinOp::Power => Op::Pow,
                    other => {
                        return err(format!("`{}` is not arithmetic", other.symbol()), *span);
                    }
                };
                VarExpr::Op {
                    op,
                    args: vec![self.arithmetic(lhs)?, self.arithmetic(rhs)?],
                }
            }
            other => {
                return err(
                    format!(
                        "`{}` is not arithmetic over columns",
                        self.source(other.span())
                    ),
                    other.span(),
                );
            }
        })
    }

    /// `(lhs | g)` and its kin, as group-level terms (`g1/g2` gives two).
    fn group(&self, e: &Expr, span: Span) -> Result<Vec<GroupTerm>, ParseError> {
        let Expr::Binary { op, lhs, rhs, .. } = e else {
            return err("a group-level term is `(terms | group)`", span);
        };
        let mut correlated = *op == BinOp::Bar;
        let mut by: Option<String> = None;
        let (lhs, mut id) = match lhs.unparen() {
            Expr::Binary {
                op: BinOp::Bar,
                lhs: inner,
                rhs: id,
                ..
            } if *op == BinOp::Bar => {
                let id = match id.unparen() {
                    Expr::Name(n, _) => n.clone(),
                    Expr::Num(x, _) => sc_model::terms::number_text(*x),
                    other => {
                        return err("an `|ID|` is a name or a number", other.span());
                    }
                };
                (inner.as_ref(), Some(id))
            }
            Expr::Binary {
                op: BinOp::Bar | BinOp::DoubleBar,
                span,
                ..
            } => {
                return err("`|` and `||` cannot be mixed in one term", *span);
            }
            _ => (lhs.as_ref(), None),
        };
        let part = self.rhs(lhs, false)?;
        if part.intercept == Intercept::Off && part.terms.is_empty() {
            return err(
                "this group-level term has no effects: `0` removes its intercept and nothing \
                 else varies",
                span,
            );
        }
        // The grouping factor(s).
        let factors: Vec<Vec<String>> = match rhs.unparen() {
            Expr::Call { name, args, .. } if name == "gr" => {
                let mut factor = None;
                for (i, a) in args.iter().enumerate() {
                    match a.name.as_ref().map(|(n, s)| (n.as_str(), *s)) {
                        None if i == 0 => factor = Some(group_columns(&a.value)?),
                        Some(("cor", _)) => correlated = correlated && logical(&a.value)?,
                        Some(("id", _)) => {
                            id = Some(match a.value.unparen() {
                                Expr::Str(s, _) => s.clone(),
                                Expr::Name(n, _) => n.clone(),
                                other => return err("`id` is a name", other.span()),
                            });
                        }
                        Some(("by", s)) => {
                            by = Some(a.value.as_name().map(str::to_owned).ok_or_else(|| {
                                ParseError::new("`by` is a column", s.to(a.value.span()))
                            })?);
                        }
                        Some(("cov", s)) => {
                            return err("`gr(cov = )` (a known covariance) is not supported", s);
                        }
                        Some(("dist", s)) => {
                            return err("`gr(dist = )` is not supported; it is normal", s);
                        }
                        Some((other, s)) => {
                            return err(format!("`gr()` has no argument `{other}`"), s);
                        }
                        None => return err("`gr()` takes one grouping factor", a.value.span()),
                    }
                }
                vec![
                    factor.ok_or_else(|| {
                        ParseError::new("`gr()` needs a grouping factor", rhs.span())
                    })?,
                ]
            }
            Expr::Call {
                name, name_span, ..
            } if name == "mm" || name == "mmc" => {
                return err(
                    "a multi-membership term (`mm()`) is not supported yet",
                    *name_span,
                );
            }
            Expr::Binary {
                op: BinOp::Divide,
                lhs: outer,
                rhs: inner,
                ..
            } => {
                // `g1/g2` = `(…| g1) + (…| g1:g2)`.
                let outer = group_columns(outer)?;
                let mut both = outer.clone();
                both.extend(group_columns(inner)?);
                vec![outer, both]
            }
            other => vec![group_columns(other)?],
        };
        Ok(factors
            .into_iter()
            .map(|factors| GroupTerm {
                factors,
                intercept: part.intercept,
                terms: part.terms.clone(),
                correlated,
                id: id.clone(),
                by: by.clone(),
                span,
            })
            .collect())
    }
}

/// The columns of a grouping factor: `g`, or `g1:g2`.
fn group_columns(e: &Expr) -> Result<Vec<String>, ParseError> {
    match e.unparen() {
        Expr::Name(n, _) => Ok(vec![n.clone()]),
        Expr::Binary {
            op: BinOp::Colon,
            lhs,
            rhs,
            ..
        } => {
            let mut out = group_columns(lhs)?;
            out.extend(group_columns(rhs)?);
            Ok(out)
        }
        other => err(
            "a grouping factor is a column, or columns joined by `:`; compute anything else as \
             a column of the dataset",
            other.span(),
        ),
    }
}

/// The response's column: a bare name.
fn response_column(e: &Expr) -> Result<String, ParseError> {
    match e.unparen() {
        Expr::Name(n, _) => Ok(n.clone()),
        Expr::Call {
            name, name_span, ..
        } if name == "mvbind" => err(
            "`mvbind(…)` takes no addition terms; give each response its own line to add them",
            *name_span,
        ),
        Expr::Call { span, .. } | Expr::Binary { span, .. } | Expr::Unary { span, .. } => err(
            "the response is a column; compute a transformed response (`log(price)`) as a \
             calculated column of the dataset",
            *span,
        ),
        other => err("the response is a column", other.span()),
    }
}

/// The items of `a + b + c`, each with its span.
fn plus_items(e: &Expr) -> Vec<(&Expr, Span)> {
    match e.unparen() {
        Expr::Binary {
            op: BinOp::Plus,
            lhs,
            rhs,
            ..
        } => {
            let mut out = plus_items(lhs);
            out.extend(plus_items(rhs));
            out
        }
        other => vec![(other, other.span())],
    }
}

fn bound(e: &Expr) -> Result<Bound, ParseError> {
    if let Some(x) = e.as_number() {
        return Ok(Bound::Number(x));
    }
    match e.unparen() {
        Expr::Name(n, _) => Ok(Bound::Column(n.clone())),
        other => err(
            "this is a column or a number; compute anything else as a column of the dataset",
            other.span(),
        ),
    }
}

fn column(e: &Expr) -> Result<String, ParseError> {
    match e.unparen() {
        Expr::Name(n, _) => Ok(n.clone()),
        other => err("this is a column", other.span()),
    }
}

fn logical(e: &Expr) -> Result<bool, ParseError> {
    match e.unparen() {
        Expr::Name(n, _) if n == "TRUE" || n == "T" => Ok(true),
        Expr::Name(n, _) if n == "FALSE" || n == "F" => Ok(false),
        other => err("this is `TRUE` or `FALSE`", other.span()),
    }
}

/// One addition term.
fn aterm(e: &Expr) -> Result<ATerm, ParseError> {
    let Expr::Call {
        name,
        args,
        span,
        name_span,
    } = e
    else {
        return err(
            "after `|` come the response's addition terms: `weights(w)`, `trials(n)`, \
             `cens(c)`, `trunc(lb = 0)`, `se(s)`, `rate(d)`",
            e.span(),
        );
    };
    let positional = |i: usize| args.get(i).filter(|a| a.name.is_none()).map(|a| &a.value);
    let named = |n: &str| {
        args.iter()
            .find(|a| a.name.as_ref().is_some_and(|(m, _)| m == n))
            .map(|a| &a.value)
    };
    let one = |what: &str| -> Result<&Expr, ParseError> {
        match args.as_slice() {
            [a] if a.name.is_none() => Ok(&a.value),
            _ => err(format!("`{what}()` takes one column or number"), *span),
        }
    };
    Ok(match name.as_str() {
        "weights" => ATerm::Weights(bound(one("weights")?)?),
        "trials" => ATerm::Trials(bound(one("trials")?)?),
        "rate" => ATerm::Rate(bound(one("rate")?)?),
        "cens" => {
            let c =
                positional(0).ok_or_else(|| ParseError::new("`cens()` needs its column", *span))?;
            let upper = match positional(1).or_else(|| named("y2")) {
                Some(e) => Some(column(e)?),
                None => None,
            };
            if args.len() > 1 + usize::from(upper.is_some()) {
                return err(
                    "`cens()` takes the censoring column and, for intervals, `y2`",
                    *span,
                );
            }
            ATerm::Cens(Censoring {
                column: column(c)?,
                upper,
            })
        }
        "trunc" => {
            let lb = match named("lb").or_else(|| positional(0)) {
                Some(e) => Some(bound(e)?),
                None => None,
            };
            let ub = match named("ub").or_else(|| positional(1)) {
                Some(e) => Some(bound(e)?),
                None => None,
            };
            if lb.is_none() && ub.is_none() {
                return err("`trunc()` needs `lb`, `ub`, or both", *span);
            }
            ATerm::Trunc { lb, ub }
        }
        "se" => {
            let c =
                positional(0).ok_or_else(|| ParseError::new("`se()` needs its column", *span))?;
            let sigma = match named("sigma") {
                Some(e) => logical(e)?,
                None => false,
            };
            ATerm::Se {
                column: column(c)?,
                sigma,
            }
        }
        "thres" => {
            if named("gr").is_some() {
                return err(
                    "grouped thresholds (`thres(gr = )`) are not supported",
                    *name_span,
                );
            }
            let k = one("thres")?
                .as_number()
                .filter(|k| *k >= 1.0 && k.fract() == 0.0);
            let Some(k) = k else {
                return err(
                    "`thres()` takes the number of thresholds: `thres(4)`",
                    *span,
                );
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            ATerm::Thres(k as usize)
        }
        "subset" => ATerm::Subset(column(one("subset")?)?),
        "cat" => {
            return err(
                "`cat()` is not supported; the categories are the data's",
                *name_span,
            );
        }
        "mi" | "dec" | "vreal" | "vint" | "index" | "resp_mi" => {
            return err(format!("`{name}()` is not supported"), *name_span);
        }
        other => {
            return err(
                format!(
                    "`{other}()` is not an addition term (they are `weights`, `trials`, `cens`, \
                     `trunc`, `se`, `rate`, `thres` and `subset`)"
                ),
                *name_span,
            );
        }
    })
}

/// `gaussian`, `"student"`, `binomial("probit")`, `student(link_sigma = …)`.
pub(crate) fn family_spec(e: &Expr) -> Result<FamilySpec, ParseError> {
    let span = e.span();
    match e.unparen() {
        Expr::Name(n, _) | Expr::Str(n, _) => Ok(FamilySpec {
            name: n.clone(),
            links: Vec::new(),
            span,
        }),
        Expr::Call { name, args, .. } => {
            let mut spec = FamilySpec {
                name: name.clone(),
                links: Vec::new(),
                span,
            };
            let mut rest = args.as_slice();
            if name == "brmsfamily" {
                let Some(first) = rest.first().filter(|a| a.name.is_none()) else {
                    return err("`brmsfamily()` names a family first", span);
                };
                spec.name = match first.value.unparen() {
                    Expr::Str(s, _) | Expr::Name(s, _) => s.clone(),
                    other => return err("a family's name", other.span()),
                };
                rest = &rest[1..];
            }
            for (i, a) in rest.iter().enumerate() {
                let value = match a.value.unparen() {
                    Expr::Str(s, _) | Expr::Name(s, _) => s.clone(),
                    other => return err("a link is a name: `\"log\"`", other.span()),
                };
                match &a.name {
                    None if i == 0 => spec.links.push(("mu".to_owned(), value)),
                    Some((n, _)) if n == "link" => spec.links.push(("mu".to_owned(), value)),
                    Some((n, s)) => match n.strip_prefix("link_") {
                        Some(dpar) => spec.links.push((dpar.to_owned(), value)),
                        None => {
                            return err(
                                format!("`{n}` is not a family argument this supports"),
                                *s,
                            );
                        }
                    },
                    None => {
                        return err(
                            "a family's links are named: `link_sigma = …`",
                            a.value.span(),
                        );
                    }
                }
            }
            Ok(spec)
        }
        other => err("a family is a name: `gaussian`, `student()`", other.span()),
    }
}

/// `bf(…) + bf(…) + set_rescor(…)`.
fn is_bf_sum(e: &Expr) -> bool {
    match e.unparen() {
        Expr::Binary {
            op: BinOp::Plus,
            lhs,
            rhs,
            ..
        } => is_bf_sum(lhs) && is_bf_sum(rhs),
        Expr::Call { name, .. } => {
            matches!(name.as_str(), "bf" | "brmsformula" | "set_rescor" | "mvbf")
        }
        _ => false,
    }
}

fn is_prior_call(name: &str) -> bool {
    matches!(name, "prior" | "set_prior" | "prior_string")
}

fn is_prior_sum(e: &Expr) -> bool {
    match e.unparen() {
        Expr::Binary {
            op: BinOp::Plus,
            lhs,
            rhs,
            ..
        } => is_prior_sum(lhs) && is_prior_sum(rhs),
        Expr::Call { name, .. } => is_prior_call(name) || name == "c",
        _ => false,
    }
}

/// The order of `set_prior`'s and `prior`'s positional arguments
/// (`?set_prior`).
const PRIOR_ARGS: &[&str] = &[
    "prior", "class", "coef", "group", "resp", "dpar", "nlpar", "lb", "ub", "check",
];

/// `prior(normal(0, 1), class = b, coef = x)` or `set_prior("normal(0, 1)",
/// class = "b")`, as a row.
fn prior_call(name: &str, args: &[Arg], span: Span) -> Result<PriorRow, ParseError> {
    let mut row = PriorRow::default();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut prior: Option<Prior> = None;
    for (i, a) in args.iter().enumerate() {
        let key = match &a.name {
            Some((n, s)) => {
                if !PRIOR_ARGS.contains(&n.as_str()) {
                    return err(format!("`{name}()` has no argument `{n}`"), *s);
                }
                n.as_str()
            }
            None => match PRIOR_ARGS.get(i) {
                Some(k) => k,
                None => return err(format!("too many arguments to `{name}()`"), a.value.span()),
            },
        };
        if !seen.insert(key) {
            return err(format!("`{key}` is given twice"), a.value.span());
        }
        let text_value = |a: &Arg| -> Result<String, ParseError> {
            match a.value.unparen() {
                Expr::Str(s, _) | Expr::Name(s, _) => Ok(s.clone()),
                other => err(format!("`{key}` is a name"), other.span()),
            }
        };
        match key {
            "prior" => {
                prior = Some(match (name, a.value.unparen()) {
                    // `set_prior("normal(0, 1)")`: the prior is the string's text.
                    (_, Expr::Str(s, sspan)) => {
                        let offset = sspan.start + 1;
                        if s.trim().is_empty() {
                            Prior::flat()
                        } else {
                            let tokens = lex(s)
                                .map_err(|e| ParseError::new(e.message, e.span.shift(offset)))?;
                            let e = expression(&tokens)
                                .map_err(|e| ParseError::new(e.message, e.span.shift(offset)))?;
                            Prior::from_expr(&e).map_err(|m| {
                                ParseError::new(m, Span::new(sspan.start, sspan.end))
                            })?
                        }
                    }
                    (_, e) => Prior::from_expr(e).map_err(|m| ParseError::new(m, e.span()))?,
                });
            }
            "class" => row.class = text_value(a)?,
            "coef" => row.coef = text_value(a)?,
            "group" => row.group = text_value(a)?,
            "resp" => row.resp = text_value(a)?,
            "dpar" => row.dpar = text_value(a)?,
            "nlpar" => row.nlpar = text_value(a)?,
            "lb" | "ub" => {
                let x = a.value.as_number().ok_or_else(|| {
                    ParseError::new(format!("`{key}` is a number"), a.value.span())
                })?;
                if key == "lb" {
                    row.lb = Some(x);
                } else {
                    row.ub = Some(x);
                }
            }
            "check" if !logical(&a.value)? => {
                return err(
                    "`check = FALSE` is not supported: every prior here is checked; a model \
                     that needs arbitrary Stan can be converted to a Stan model",
                    a.value.span(),
                );
            }
            _ => {}
        }
    }
    let prior = prior.ok_or_else(|| ParseError::new(format!("`{name}()` needs a prior"), span))?;
    row.prior = prior.to_string();
    if row.class.is_empty() {
        row.class = "b".to_owned();
    }
    Ok(row)
}

/// The variables of one right-hand side, by label, in order of first
/// appearance.
#[derive(Default)]
struct Registry {
    vars: Vec<VarExpr>,
    labels: Vec<String>,
}

impl Registry {
    fn add(&mut self, v: VarExpr) -> usize {
        let label = v.label();
        match self.labels.iter().position(|l| *l == label) {
            Some(i) => i,
            None => {
                self.labels.push(label);
                self.vars.push(v);
                self.labels.len() - 1
            }
        }
    }
}

/// A right-hand side while it is expanded: its terms as sets of variable
/// indices, in order.
#[derive(Debug, Clone, Default)]
struct Expansion {
    terms: Vec<BTreeSet<usize>>,
    intercept: Option<bool>,
    intercept_coef: bool,
    offsets: Vec<VarExpr>,
    groups: Vec<GroupTerm>,
    mo: Vec<String>,
    cs: Vec<Term>,
}

impl Expansion {
    fn intercept(on: bool) -> Expansion {
        Expansion {
            intercept: Some(on),
            ..Expansion::default()
        }
    }

    fn variable(i: usize) -> Expansion {
        Expansion {
            terms: vec![BTreeSet::from([i])],
            ..Expansion::default()
        }
    }

    fn plus(mut self, other: Expansion) -> Expansion {
        for t in other.terms {
            if !self.terms.contains(&t) {
                self.terms.push(t);
            }
        }
        if other.intercept.is_some() {
            self.intercept = other.intercept;
        }
        self.intercept_coef |= other.intercept_coef;
        self.offsets.extend(other.offsets);
        self.groups.extend(other.groups);
        for m in other.mo {
            if !self.mo.contains(&m) {
                self.mo.push(m);
            }
        }
        self.cs.extend(other.cs);
        self
    }

    fn minus(mut self, other: &Expansion) -> Expansion {
        self.terms.retain(|t| !other.terms.contains(t));
        if other.intercept == Some(true) {
            self.intercept = Some(false);
        }
        if other.intercept_coef {
            self.intercept_coef = false;
        }
        self
    }

    fn product(&self, other: &Expansion) -> Expansion {
        let mut out = Expansion::default();
        for a in &self.terms {
            for b in &other.terms {
                let t: BTreeSet<usize> = a.union(b).copied().collect();
                if !out.terms.contains(&t) {
                    out.terms.push(t);
                }
            }
        }
        out
    }

    fn all_variables(&self) -> BTreeSet<usize> {
        self.terms.iter().flatten().copied().collect()
    }
}
