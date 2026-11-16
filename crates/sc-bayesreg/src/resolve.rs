//! A parsed [`Formula`] against the model's dataset and form (R1.2–R1.3,
//! R2.1–R2.3): the family and its links, each response and its addition
//! terms, each linear predictor's terms as the binder's [`TermsSpec`], the
//! group-level terms merged into **blocks**, and the prior classes all of that
//! creates.
//!
//! What can be checked depends on what is known. With the column names only
//! (a model listed at load), every name is checked to be a column; with the
//! dataset's shape (a save from the editor), also the types — the response
//! against the family, no date or text inside a transform; with the frames (a
//! preview, a fit), also the values and the categorical levels, so a prior row
//! naming a level can be matched exactly.
//!
//! R2 adds three shapes of model to the one R1 resolves:
//!
//! - **multivariate** (R2.3): several response lines, or `mvbind(y1, y2)`,
//!   each its own submodel; a distributional parameter's line belongs to the
//!   response written in the same `bf(…)`; terms sharing an `|ID|` are one
//!   block across responses (brms's documented `?mvbrmsformula`);
//! - **non-linear** (R2.2): the main formula an expression over non-linear
//!   parameters, each of which has a formula of its own (`?brmsformula`,
//!   "Parameterization of the population-level intercept" and `nl`);
//! - **ordinal** and **categorical** families (R2.1): thresholds in place of
//!   an intercept, and a linear predictor per category but the first.

use std::collections::BTreeMap;

use sc_model::terms::{Coding, TermsSpec, VarExpr, code_terms, fit_terms};
use sc_model::{ColumnType, DatasetShape, Frame};

use crate::family::{Family, Kind, Link, Response, family};
use crate::parse::{
    ATerm, Bound, Formula, GroupTerm, Intercept, Lhs, Line, NlExpr, ParseError, ParseErrors, Rhs,
    Span,
};
use crate::prior::{ClassKind, ClassSlot, Coef, CoefPart, CoefTemplate, DataScale, DefaultPrior};

/// The names a non-linear parameter's predictor is known by in the
/// structure: positional, in order of first appearance (§5 rule 4).
pub const NLP: [&str; 16] = [
    "nlp_1", "nlp_2", "nlp_3", "nlp_4", "nlp_5", "nlp_6", "nlp_7", "nlp_8", "nlp_9", "nlp_10",
    "nlp_11", "nlp_12", "nlp_13", "nlp_14", "nlp_15", "nlp_16",
];

/// The form's settings that a formula's `bf()` may override.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The family's name.
    pub family: String,
    /// Links set on the form, by distributional parameter (`mu`, `sigma`).
    pub links: BTreeMap<String, String>,
    /// Centre the population-level predictors (brms's `center`, default on).
    pub center: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            family: "gaussian".to_owned(),
            links: BTreeMap::new(),
            center: true,
        }
    }
}

/// What is known of the dataset.
#[derive(Debug, Clone, Copy)]
pub enum Data<'a> {
    /// Nothing: no column is checked (a model listed before anything has read
    /// its dataset).
    Nothing,
    /// The column names only.
    Names(&'a [String]),
    /// The shape: names and types.
    Shape(&'a DatasetShape),
    /// The rows themselves.
    Frame(&'a Frame),
}

impl Data<'_> {
    fn has(&self, column: &str) -> bool {
        match self {
            Data::Nothing => true,
            Data::Names(n) => n.iter().any(|c| c == column),
            Data::Shape(s) => s.column(column).is_some(),
            Data::Frame(f) => f.column(column).is_some(),
        }
    }

    fn names(&self) -> Vec<String> {
        match self {
            Data::Nothing => Vec::new(),
            Data::Names(n) => n.to_vec(),
            Data::Shape(s) => s.columns.iter().map(|c| c.name.clone()).collect(),
            Data::Frame(f) => f.columns.iter().map(|(n, _)| n.clone()).collect(),
        }
    }

    fn ty(&self, column: &str) -> Option<ColumnType> {
        match self {
            Data::Nothing | Data::Names(_) => None,
            Data::Shape(s) => s.column(column),
            Data::Frame(f) => f.column(column).map(sc_model::Column::kind),
        }
    }

    fn typed(&self) -> bool {
        !matches!(self, Data::Nothing | Data::Names(_))
    }
}

/// One linear predictor: `mu`'s, a distributional parameter's, or a
/// non-linear parameter's.
#[derive(Debug, Clone, PartialEq)]
pub struct Predictor {
    /// The response it belongs to: an index into [`Resolved::responses`].
    pub resp: usize,
    /// `mu`, `sigma`, …; for a non-linear parameter, its positional name
    /// `nlp_1`, … ([`NLP`]).
    pub dpar: &'static str,
    /// A non-linear parameter's own name (`a`), or `None`.
    pub nlpar: Option<String>,
    /// Its link (a non-linear parameter's is the identity).
    pub link: Link,
    /// Its intercept.
    pub intercept: Intercept,
    /// Its population-level terms, for a `terms` binding (no intercept
    /// column: the intercept is a parameter of its own, centred or not —
    /// except `0 + Intercept` and a non-linear parameter's, where it is the
    /// matrix's first column).
    pub terms: TermsSpec,
    /// Its offsets.
    pub offsets: Vec<VarExpr>,
    /// Its `b` coefficients, as templates until the levels are known.
    pub coefs: Vec<CoefTemplate>,
    /// `mo(x)`: the ordinal columns with a monotonic effect.
    pub mo: Vec<String>,
    /// `cs(…)`: the category-specific terms (ordinal families).
    pub cs: Option<TermsSpec>,
    /// Their coefficients' templates.
    pub cs_coefs: Vec<CoefTemplate>,
}

impl Predictor {
    /// What it is called in [`Names`], the prior rows and the parameter
    /// table: `mu`, `sigma`; `nlpar:a`; for a later response `y2/mu`.
    pub fn key(&self, responses: &[Sub]) -> String {
        let own = match &self.nlpar {
            Some(n) => format!("nlpar:{n}"),
            None => self.dpar.to_owned(),
        };
        if self.resp == 0 {
            own
        } else {
            format!("{}/{own}", responses[self.resp].response)
        }
    }

    /// The prior rows' `dpar` for its classes: empty for `mu`.
    pub fn dpar_key(&self) -> &'static str {
        if self.dpar == "mu" || self.nlpar.is_some() {
            ""
        } else {
            self.dpar
        }
    }
}

/// One block of group-level effects: a grouping factor, the effects that
/// vary over it (in one or more linear predictors), correlated or not.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// The grouping factor's label: `county`, `school:class`.
    pub group: String,
    /// Its columns.
    pub factors: Vec<String>,
    /// The `|ID|` that merged its parts, if any.
    pub id: Option<String>,
    /// Whether its effects are correlated.
    pub correlated: bool,
    /// `gr(by = h)`: a covariance per level of this column.
    pub by: Option<String>,
    /// Its parts, one per linear predictor it varies.
    pub parts: Vec<BlockPart>,
}

/// The effects one block varies in one linear predictor.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockPart {
    /// The linear predictor: an index into [`Resolved::predictors`].
    pub predictor: usize,
    /// Its dpar (`mu`, `sigma`, `nlp_1`).
    pub dpar: &'static str,
    /// The effects' model matrix (`Z`): an `Intercept` column when the term
    /// has one, then the terms.
    pub terms: TermsSpec,
    /// The effects' names, as templates until the levels are known.
    pub coefs: Vec<CoefTemplate>,
}

/// One response and what is its own: in a univariate model, the only one.
#[derive(Debug, Clone, PartialEq)]
pub struct Sub {
    /// The response column.
    pub response: String,
    /// Its addition terms (`subset` and `thres` among them).
    pub aterms: Vec<ATerm>,
    /// The family's distributional parameters it estimates as one parameter
    /// each (no formula, not fixed).
    pub scalars: Vec<&'static str>,
    /// The distributional parameters fixed to a number (`nu = 4`).
    pub fixed: BTreeMap<&'static str, f64>,
    /// The main formula's expression, when it is non-linear.
    pub nl: Option<NlExpr>,
}

impl Sub {
    /// `subset(s)`'s column.
    pub fn subset(&self) -> Option<&str> {
        self.aterms.iter().find_map(|a| match a {
            ATerm::Subset(c) => Some(c.as_str()),
            _ => None,
        })
    }

    /// `thres(k)`.
    pub fn thres(&self) -> Option<usize> {
        self.aterms.iter().find_map(|a| match a {
            ATerm::Thres(k) => Some(*k),
            _ => None,
        })
    }
}

/// A formula resolved against the dataset and the form.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The family (every response's).
    pub family: &'static Family,
    /// Every distributional parameter's link, `mu` included.
    pub links: BTreeMap<&'static str, Link>,
    /// Whether the population-level predictors are centred.
    pub center: bool,
    /// The (first) response column — [`responses`](Resolved::responses)`[0]`'s.
    pub response: String,
    /// Its addition terms.
    pub aterms: Vec<ATerm>,
    /// The linear predictors: per response (in the order written), `mu` (or
    /// the non-linear parameters, in order of first appearance) first, then
    /// the distributional parameters with a formula, in the family's order.
    pub predictors: Vec<Predictor>,
    /// The first response's scalar distributional parameters.
    pub scalars: Vec<&'static str>,
    /// The group-level blocks.
    pub blocks: Vec<Block>,
    /// What the editor should say: overrides from `bf()`, an inferred `nl`.
    pub notes: Vec<String>,
    /// Every response, in the order written (more than one: multivariate).
    pub responses: Vec<Sub>,
    /// Residual correlations between the responses (gaussian and student).
    pub rescor: bool,
}

fn problem(message: impl Into<String>, span: Option<Span>) -> ParseError {
    let mut e = ParseError::new(message, span.unwrap_or_default());
    if span.is_none() {
        // A problem with the form, not the text: no line to point at.
        e.line = 0;
    }
    e
}

/// Resolve `formula` (parsed from `text`) with the form's `settings` against
/// what is known of the dataset. Every problem found is reported, located in
/// the text where it is about the text.
#[allow(clippy::too_many_lines)] // the steps in order, each short
pub fn resolve(
    text: &str,
    formula: &Formula,
    settings: &Settings,
    data: Data<'_>,
) -> Result<Resolved, ParseErrors> {
    let mut errors: Vec<ParseError> = Vec::new();
    let located = |e: ParseError, errors: &mut Vec<ParseError>| {
        if e.span == Span::default() && e.line == 0 {
            errors.push(e);
        } else {
            errors.push(e.locate(text));
        }
    };

    let Some((family, links)) =
        family_and_links(formula, settings, &mut |e| located(e, &mut errors))
    else {
        return Err(ParseErrors(errors));
    };
    let center = formula.overrides.center.unwrap_or(settings.center);

    let sorted = sort_lines(text, formula, family, &mut |e| located(e, &mut errors));
    if sorted.responses.is_empty() {
        located(
            problem("no line names the response", Some(Span::new(0, text.len()))),
            &mut errors,
        );
        return Err(ParseErrors(errors));
    }
    let multivariate = sorted.responses.len() > 1;

    let mut checker = Checker {
        text,
        data,
        errors: Vec::new(),
    };

    let mut responses: Vec<Sub> = Vec::new();
    let mut predictors: Vec<Predictor> = Vec::new();
    let mut parts: Vec<(usize, &GroupTerm)> = Vec::new();
    for (r, resp) in sorted.responses.iter().enumerate() {
        let line = resp.line;
        let aterm_list = checker.response(
            family,
            &resp.column,
            line.lhs_span,
            &resp.aterms,
            multivariate,
        );
        checker.kind_rules(family, line, &sorted, r);

        // The linear predictors of this response: `mu`'s (or the non-linear
        // parameters'), then the distributional parameters', in the family's
        // order.
        let mut lines: Vec<(&'static str, Option<String>, &Rhs, Span)> = Vec::new();
        match &line.nl {
            Some(expr) => {
                let (params, covariates) = expr.names();
                for c in &covariates {
                    checker.numeric_column(c, line.span);
                }
                if params.len() > NLP.len() {
                    checker.say(
                        format!(
                            "a non-linear formula can have at most {} parameters",
                            NLP.len()
                        ),
                        line.span,
                    );
                }
                for (i, p) in params.iter().enumerate().take(NLP.len()) {
                    if let Some(l) = sorted.nlpars.get(p.as_str()) {
                        lines.push((NLP[i], Some(p.clone()), &l.rhs, l.span));
                    }
                }
            }
            None => lines.push(("mu", None, &line.rhs, line.span)),
        }
        for d in family.dpars {
            if let Some(l) = sorted.dpars.get(&(r, d.name)) {
                lines.push((d.name, None, &l.rhs, l.span));
            }
        }
        for (dpar, nlpar, rhs, span) in &lines {
            for t in &rhs.terms {
                for v in &t.variables {
                    checker.variable(v, *span);
                }
            }
            for o in &rhs.offsets {
                checker.variable(o, *span);
            }
            for m in &rhs.mo {
                checker.mo_column(m, *span);
            }
            for t in &rhs.cs {
                for v in &t.variables {
                    checker.variable(v, *span);
                }
            }
            for g in &rhs.groups {
                for f in g.factors.iter().chain(&g.by) {
                    if checker.column(f, g.span) && matches!(data.ty(f), Some(ColumnType::Null)) {
                        checker.say(
                            format!("`{f}` is null in every row, so it groups nothing"),
                            g.span,
                        );
                    }
                }
                for t in &g.terms {
                    for v in &t.variables {
                        checker.variable(v, g.span);
                    }
                }
                parts.push((predictors.len(), g));
            }
            let mut terms = population_terms(rhs);
            if nlpar.is_some() {
                // A non-linear parameter's intercept is an ordinary
                // coefficient, never centred (brms's `center = FALSE` for
                // non-linear parameters).
                terms.intercept_column = rhs.intercept != Intercept::Off;
            }
            let coefs = templates(&terms, &data);
            let cs = (!rhs.cs.is_empty()).then(|| TermsSpec {
                terms: rhs.cs.iter().map(|t| t.variables.clone()).collect(),
                intercept: false,
                intercept_column: false,
            });
            let cs_coefs = cs.as_ref().map(|c| templates(c, &data)).unwrap_or_default();
            predictors.push(Predictor {
                resp: r,
                dpar,
                nlpar: nlpar.clone(),
                link: if nlpar.is_some() {
                    Link::Identity
                } else {
                    links.get(dpar).copied().unwrap_or(Link::Identity)
                },
                intercept: if nlpar.is_some() && rhs.intercept != Intercept::Off {
                    Intercept::Coefficient
                } else {
                    rhs.intercept
                },
                terms,
                offsets: rhs.offsets.clone(),
                coefs,
                mo: rhs.mo.clone(),
                cs,
                cs_coefs,
            });
        }

        // `se(s, sigma = FALSE)`: the known SEs are the whole residual SD, so
        // there is no `sigma` to estimate (`?addition-terms`).
        let no_sigma = aterm_list
            .iter()
            .any(|a| matches!(a, ATerm::Se { sigma: false, .. }));
        if no_sigma {
            if let Some(l) = sorted.dpars.get(&(r, "sigma")) {
                checker.say(
                    "`se(…, sigma = FALSE)` takes the residual SD as known, so `sigma` cannot \
                     have a formula; write `se(…, sigma = TRUE)` to estimate one as well",
                    l.lhs_span,
                );
            }
        }
        let mut fixed: BTreeMap<&'static str, f64> = BTreeMap::new();
        for f in sorted.fixed.iter().filter(|f| f.0 == r) {
            let (_, d, value, span) = *f;
            if sorted.dpars.contains_key(&(r, d.name)) {
                checker.say(
                    format!("`{}` has a formula and is fixed to a number too", d.name),
                    span,
                );
            }
            let ok = match d.kind {
                ClassKind::Positive => value > 0.0,
                ClassKind::AboveOne => value > 1.0,
                ClassKind::Unit => value > 0.0 && value < 1.0,
                _ => value.is_finite(),
            };
            if !ok {
                checker.say(
                    format!(
                        "`{} = {}` is outside what `{}` can be ({})",
                        d.name,
                        sc_model::terms::number_text(value),
                        d.name,
                        d.about
                    ),
                    span,
                );
            }
            fixed.insert(d.name, value);
        }
        let scalars = family
            .dpars
            .iter()
            .filter(|d| !sorted.dpars.contains_key(&(r, d.name)))
            .filter(|d| !(no_sigma && d.name == "sigma"))
            .filter(|d| !fixed.contains_key(d.name))
            .filter(|d| d.fixed.is_none())
            .map(|d| d.name)
            .collect();
        responses.push(Sub {
            response: resp.column.clone(),
            aterms: aterm_list,
            scalars,
            fixed,
            nl: line.nl.clone(),
        });
    }
    for (name, l) in &sorted.unused_nlpars {
        checker.say(
            format!("`{name}` has a formula, but the non-linear formula does not use it"),
            l.lhs_span,
        );
    }

    let blocks = merge_blocks(parts, &predictors, &data, &mut checker);
    let rescor = checker.rescor(family, formula, &responses);

    for e in checker.errors {
        located(e, &mut errors);
    }
    if !errors.is_empty() {
        return Err(ParseErrors(errors));
    }
    let main = &responses[0];
    let mut notes = formula.overrides.notes.clone();
    if multivariate {
        notes.push(format!(
            "the model is multivariate: {} are its responses{}",
            responses
                .iter()
                .map(|r| format!("`{}`", r.response))
                .collect::<Vec<_>>()
                .join(", "),
            if rescor {
                ", with residual correlations"
            } else {
                ""
            }
        ));
    }
    Ok(Resolved {
        family,
        links,
        center,
        response: main.response.clone(),
        aterms: main.aterms.clone(),
        predictors,
        scalars: main.scalars.clone(),
        blocks,
        notes,
        responses,
        rescor,
    })
}

/// The blocks: one per group-level term, except that terms sharing an `|ID|`
/// on the same grouping factor are one (across linear predictors and
/// responses too).
fn merge_blocks(
    parts: Vec<(usize, &GroupTerm)>,
    predictors: &[Predictor],
    data: &Data<'_>,
    checker: &mut Checker<'_>,
) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for (at, g) in parts {
        let group = g.group();
        let terms = group_terms(g);
        let coefs = templates(&terms, data);
        let part = BlockPart {
            predictor: at,
            dpar: predictors[at].dpar,
            terms,
            coefs,
        };
        if let Some(id) = &g.id {
            if let Some(b) = blocks.iter_mut().find(|b| b.id.as_ref() == Some(id)) {
                if b.group != group {
                    checker.say(
                        format!(
                            "the ID `{id}` joins terms on `{}` and on `{group}`; terms sharing \
                             an ID must have the same grouping factor",
                            b.group
                        ),
                        g.span,
                    );
                } else if b.correlated != g.correlated {
                    checker.say(
                        format!("the terms sharing the ID `{id}` are correlated in one and not in another"),
                        g.span,
                    );
                } else if b.by != g.by {
                    checker.say(
                        format!("the terms sharing the ID `{id}` have different `gr(by = )`"),
                        g.span,
                    );
                } else if b.parts.iter().any(|p| p.predictor == at) {
                    checker.say(
                        format!(
                            "two terms of the same formula share the ID `{id}`; write them as one \
                             term"
                        ),
                        g.span,
                    );
                } else {
                    b.parts.push(part);
                }
                continue;
            }
        }
        if g.by.is_some() && g.factors.len() > 1 {
            checker.say(
                "`gr(by = )` takes one grouping column; add a Calculated column for the \
                 combination",
                g.span,
            );
        }
        blocks.push(Block {
            group,
            factors: g.factors.clone(),
            id: g.id.clone(),
            correlated: g.correlated,
            by: g.by.clone(),
            parts: vec![part],
        });
    }

    blocks
}

/// A right-hand side's population-level terms as the binder takes them.
fn population_terms(rhs: &Rhs) -> TermsSpec {
    TermsSpec {
        terms: rhs.terms.iter().map(|t| t.variables.clone()).collect(),
        intercept: rhs.intercept != Intercept::Off,
        intercept_column: rhs.intercept == Intercept::Coefficient,
    }
}

/// A group-level term's effects: `Intercept` first when it has one.
fn group_terms(g: &GroupTerm) -> TermsSpec {
    let has = g.intercept != Intercept::Off;
    TermsSpec {
        terms: g.terms.iter().map(|t| t.variables.clone()).collect(),
        intercept: has,
        intercept_column: has,
    }
}

/// The coefficient templates of `spec`, by R's coding, with the types where
/// they are known.
fn templates(spec: &TermsSpec, data: &Data<'_>) -> Vec<CoefTemplate> {
    let labels: Vec<Vec<String>> = spec
        .terms
        .iter()
        .map(|t| t.iter().map(VarExpr::label).collect())
        .collect();
    let categorical = |label: &str| {
        spec.terms
            .iter()
            .flatten()
            .find(|v| v.label() == label)
            .is_some_and(|v| v.is_categorical(&|c| data.ty(c)))
    };
    let codes = code_terms(
        &labels,
        spec.intercept || spec.intercept_column,
        &categorical,
    );
    let mut out = Vec::new();
    if spec.intercept_column {
        out.push(CoefTemplate::exact(sc_model::terms::INTERCEPT_COLUMN));
    }
    for (term, codes) in spec.terms.iter().zip(codes) {
        let parts = term
            .iter()
            .zip(codes)
            .map(|(v, code)| {
                let label = v.label();
                let poly = matches!(
                    v,
                    VarExpr::Call {
                        function: sc_model::terms::Function::Poly,
                        ..
                    }
                );
                if poly || code != Coding::Numeric {
                    CoefPart::Level(label)
                } else if !data.typed() && v.as_column().is_some() {
                    CoefPart::Unknown(label)
                } else {
                    CoefPart::Exact(label)
                }
            })
            .collect();
        out.push(CoefTemplate { parts });
    }
    out
}

impl Resolved {
    /// Whether it is multivariate.
    pub fn multivariate(&self) -> bool {
        self.responses.len() > 1
    }

    /// Whether the main formula is non-linear.
    pub fn nonlinear(&self) -> bool {
        self.responses[0].nl.is_some()
    }

    /// Every column the formula reads: the responses, their addition terms,
    /// the terms and offsets of every linear predictor, the non-linear
    /// formula's columns, and the grouping factors.
    pub fn columns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut add = |c: &str| {
            if !out.iter().any(|o| o == c) {
                out.push(c.to_owned());
            }
        };
        for r in &self.responses {
            add(&r.response);
            for a in &r.aterms {
                for c in a.columns() {
                    add(c);
                }
            }
            if let Some(nl) = &r.nl {
                for c in nl.names().1 {
                    add(&c);
                }
            }
        }
        for p in &self.predictors {
            for v in p.terms.terms.iter().flatten().chain(&p.offsets) {
                for c in v.columns() {
                    add(c);
                }
            }
            for m in &p.mo {
                add(m);
            }
            if let Some(cs) = &p.cs {
                for v in cs.terms.iter().flatten() {
                    for c in v.columns() {
                        add(c);
                    }
                }
            }
        }
        for b in &self.blocks {
            for f in b.factors.iter().chain(&b.by) {
                add(f);
            }
            for part in &b.parts {
                for v in part.terms.terms.iter().flatten() {
                    for c in v.columns() {
                        add(c);
                    }
                }
            }
        }
        out
    }

    /// The columns a row may be null in without being dropped: a response
    /// measured on a subset of the rows only, and its addition terms' columns
    /// — outside the subset they are not read.
    pub fn subset_only_columns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in self.responses.iter().filter(|r| r.subset().is_some()) {
            for c in std::iter::once(r.response.as_str()).chain(
                r.aterms
                    .iter()
                    .filter(|a| !matches!(a, ATerm::Subset(_)))
                    .flat_map(ATerm::columns),
            ) {
                if !out.iter().any(|o| o == c) {
                    out.push(c.to_owned());
                }
            }
        }
        // A column another part of the model reads is policed all the same.
        let elsewhere = self.columns_without_subset_responses();
        out.retain(|c| !elsewhere.contains(c));
        out
    }

    fn columns_without_subset_responses(&self) -> Vec<String> {
        let mut copy = self.clone();
        for r in &mut copy.responses {
            if r.subset().is_some() {
                r.response = String::new();
                r.aterms.retain(|a| matches!(a, ATerm::Subset(_)));
            }
        }
        copy.columns()
            .into_iter()
            .filter(|c| !c.is_empty())
            .collect()
    }

    /// The rows of `frame` a fit keeps: those with no null in any column the
    /// formula reads (the main dataset's `nulls` policy is `drop`, §2.9) —
    /// except a subset response's own columns.
    pub fn kept(&self, frame: &Frame) -> sc_error::Result<Frame> {
        let free = self.subset_only_columns();
        let read: Vec<&sc_model::Column> = self
            .columns()
            .iter()
            .filter(|c| !free.contains(c))
            .filter_map(|c| frame.column(c))
            .collect();
        let rows: Vec<usize> = (0..frame.rows)
            .filter(|&i| read.iter().all(|c| !c.is_null_at(i)))
            .collect();
        if rows.len() == frame.rows {
            return Ok(frame.clone());
        }
        frame.take_rows(&rows)
    }

    /// The rows of `frame` (already [`kept`](Resolved::kept)) response `r` is
    /// measured on: all of them, or its subset's where its own values are
    /// present.
    pub fn response_rows(&self, r: usize, frame: &Frame) -> Vec<usize> {
        let sub = &self.responses[r];
        let Some(s) = sub.subset() else {
            return (0..frame.rows).collect();
        };
        let flags = frame.column(s);
        let own: Vec<&sc_model::Column> = std::iter::once(sub.response.as_str())
            .chain(
                sub.aterms
                    .iter()
                    .filter(|a| !matches!(a, ATerm::Subset(_)))
                    .flat_map(ATerm::columns),
            )
            .filter_map(|c| frame.column(c))
            .collect();
        (0..frame.rows)
            .filter(|&i| flags.is_some_and(|f| truthy(f, i)))
            .filter(|&i| own.iter().all(|c| !c.is_null_at(i)))
            .collect()
    }

    /// The linear predictor of `dpar` of the first response.
    pub fn predictor(&self, dpar: &str) -> Option<&Predictor> {
        self.predictors
            .iter()
            .find(|p| p.resp == 0 && p.dpar == dpar && p.nlpar.is_none())
    }

    /// Each predictor's key ([`Predictor::key`]), in order.
    pub fn keys(&self) -> Vec<String> {
        self.predictors
            .iter()
            .map(|p| p.key(&self.responses))
            .collect()
    }

    /// The coefficient names of every linear predictor and block part over
    /// `frame`: the model matrices' columns, with the levels the rows have.
    pub fn names(&self, frame: &Frame) -> sc_error::Result<Names> {
        let kept = self.kept(frame)?;
        let frame = &kept;
        let mut names = Names::default();
        for p in &self.predictors {
            let key = p.key(&self.responses);
            let (_, m) = fit_terms(&p.terms, frame)
                .map_err(|e| sc_error::Error::invalid(format!("the formula of `{key}`: {e}")))?;
            let mut cols = m.columns;
            if let Some(cs) = &p.cs {
                let (_, m) = fit_terms(cs, frame).map_err(|e| {
                    sc_error::Error::invalid(format!("the `cs()` terms of `{key}`: {e}"))
                })?;
                names.cs.insert(key.clone(), m.columns);
            }
            for mo in &p.mo {
                cols.push(format!("mo{mo}"));
            }
            names.b.insert(key, cols);
        }
        for (k, b) in self.blocks.iter().enumerate() {
            for part in &b.parts {
                let (_, m) = fit_terms(&part.terms, frame).map_err(|e| {
                    sc_error::Error::invalid(format!(
                        "the group-level term over `{}`: {e}",
                        b.group
                    ))
                })?;
                names.blocks.insert(
                    (k, self.predictors[part.predictor].key(&self.responses)),
                    m.columns,
                );
            }
        }
        Ok(names)
    }

    /// The prior classes the formula creates (§3.7): what a row can target
    /// and what the table lists. With `names` (after the data is read) the
    /// coefficients are the matrices' columns; without, their templates.
    #[allow(clippy::too_many_lines)] // one class after another
    pub fn prior_slots(&self, names: Option<&Names>) -> Vec<ClassSlot> {
        let mv = self.multivariate();
        let resp_of = |r: usize| {
            if mv {
                self.responses[r].response.clone()
            } else {
                String::new()
            }
        };
        let ordinal = self.family.kind == Kind::Ordinal;
        let mut slots = Vec::new();
        for p in &self.predictors {
            let dpar = p.dpar_key().to_owned();
            let nlpar = p.nlpar.clone().unwrap_or_default();
            let key = p.key(&self.responses);
            let base = ClassSlot {
                class: String::new(),
                dpar: dpar.clone(),
                group: String::new(),
                coefs: Vec::new(),
                kind: ClassKind::Real,
                default: DefaultPrior::Flat,
                nlpar: nlpar.clone(),
                resp: resp_of(p.resp),
                response: p.resp,
            };
            if p.intercept == Intercept::On {
                slots.push(ClassSlot {
                    class: "Intercept".to_owned(),
                    default: if p.dpar == "mu" && !ordinal {
                        DefaultPrior::InterceptScale
                    } else {
                        DefaultPrior::Fixed("student_t", &[3.0, 0.0, 2.5])
                    },
                    ..base.clone()
                });
            }
            let mut coefs: Vec<Coef> = match names.and_then(|n| n.b.get(&key)) {
                Some(cols) => cols.iter().cloned().map(Coef::Named).collect(),
                None => p
                    .coefs
                    .iter()
                    .cloned()
                    .map(Coef::Template)
                    .chain(p.mo.iter().map(|m| Coef::Named(format!("mo{m}"))))
                    .collect(),
            };
            match names.and_then(|n| n.cs.get(&key)) {
                Some(cols) => coefs.extend(cols.iter().cloned().map(Coef::Named)),
                None => coefs.extend(p.cs_coefs.iter().cloned().map(Coef::Template)),
            }
            if !coefs.is_empty() {
                slots.push(ClassSlot {
                    class: "b".to_owned(),
                    coefs,
                    ..base.clone()
                });
            }
            for m in &p.mo {
                slots.push(ClassSlot {
                    class: "simo".to_owned(),
                    coefs: vec![Coef::Named(format!("mo{m}1"))],
                    kind: ClassKind::Simplex,
                    default: DefaultPrior::Fixed("dirichlet", &[1.0]),
                    ..base.clone()
                });
            }
        }
        // `sd` per (linear predictor, grouping factor); `cor` per grouping
        // factor with a correlated block.
        let mut sd: Vec<ClassSlot> = Vec::new();
        let mut cor: Vec<String> = Vec::new();
        for (k, b) in self.blocks.iter().enumerate() {
            let mut size = 0usize;
            for part in &b.parts {
                let p = &self.predictors[part.predictor];
                let dpar = p.dpar_key();
                let nlpar = p.nlpar.clone().unwrap_or_default();
                let resp = resp_of(p.resp);
                let coefs: Vec<Coef> =
                    match names.and_then(|n| n.blocks.get(&(k, p.key(&self.responses)))) {
                        Some(cols) => cols.iter().cloned().map(Coef::Named).collect(),
                        None => part.coefs.iter().cloned().map(Coef::Template).collect(),
                    };
                size += coefs.len();
                match sd.iter_mut().find(|s| {
                    s.dpar == dpar && s.group == b.group && s.nlpar == nlpar && s.resp == resp
                }) {
                    Some(slot) => {
                        for c in coefs {
                            if !slot.coefs.contains(&c) {
                                slot.coefs.push(c);
                            }
                        }
                    }
                    None => sd.push(ClassSlot {
                        class: "sd".to_owned(),
                        dpar: dpar.to_owned(),
                        group: b.group.clone(),
                        coefs,
                        kind: ClassKind::Positive,
                        default: DefaultPrior::ResponseScale,
                        nlpar,
                        resp,
                        response: p.resp,
                    }),
                }
            }
            // A correlation needs two effects — or possibly two, before the
            // levels are known (a categorical response has an effect per
            // category).
            let levels_unknown = names.is_none()
                && b.parts.iter().any(|p| {
                    p.coefs
                        .iter()
                        .any(|c| c.parts.iter().any(|q| !matches!(q, CoefPart::Exact(_))))
                });
            let categorical = self.family.kind == Kind::Categorical;
            let maybe_two = size > 1 || levels_unknown || categorical;
            if b.correlated && maybe_two && !cor.contains(&b.group) {
                cor.push(b.group.clone());
            }
        }
        slots.extend(sd);
        for group in cor {
            slots.push(ClassSlot {
                class: "cor".to_owned(),
                dpar: String::new(),
                group,
                coefs: Vec::new(),
                kind: ClassKind::Correlation,
                default: DefaultPrior::Lkj,
                nlpar: String::new(),
                resp: String::new(),
                response: 0,
            });
        }
        if self.rescor {
            slots.push(ClassSlot {
                class: "rescor".to_owned(),
                dpar: String::new(),
                group: String::new(),
                coefs: Vec::new(),
                kind: ClassKind::Correlation,
                default: DefaultPrior::Lkj,
                nlpar: String::new(),
                resp: String::new(),
                response: 0,
            });
        }
        for (r, sub) in self.responses.iter().enumerate() {
            for name in &sub.scalars {
                if let Some(d) = self.family.dpar(name) {
                    slots.push(ClassSlot {
                        class: d.name.to_owned(),
                        dpar: String::new(),
                        group: String::new(),
                        coefs: Vec::new(),
                        kind: d.kind,
                        default: d.prior,
                        nlpar: String::new(),
                        resp: resp_of(r),
                        response: r,
                    });
                }
            }
        }
        slots
    }

    /// The scale the data-dependent defaults are computed with, over the
    /// first response in `frame` (§3.7).
    pub fn data_scale(&self, frame: &Frame) -> DataScale {
        self.data_scale_of(0, frame)
    }

    /// The data scale of response `r`: the median and the floored MAD of its
    /// values on `mu`'s link scale, over the rows a fit keeps.
    pub fn data_scale_of(&self, r: usize, frame: &Frame) -> DataScale {
        let binary = matches!(
            self.family.response,
            Response::Binary | Response::Category | Response::Ordinal
        ) || matches!(self.family.name, "binomial" | "beta_binomial");
        if binary {
            return DataScale::default();
        }
        // Over the rows a fit keeps.
        let Ok(kept) = self.kept(frame) else {
            return DataScale::default();
        };
        let frame = &kept;
        let sub = &self.responses[r];
        let Some(column) = frame.column(&sub.response) else {
            return DataScale::default();
        };
        let link = self.links.get("mu").copied().unwrap_or(Link::Identity);
        let lognormal = matches!(self.family.name, "lognormal" | "hurdle_lognormal");
        let values: Vec<f64> = self
            .response_rows(r, frame)
            .into_iter()
            .filter_map(|i| number(column, i))
            .filter_map(|y| {
                let y = if lognormal { y.ln() } else { y };
                if !y.is_finite() {
                    return None;
                }
                link.apply(y)
            })
            .collect();
        DataScale::of(&values)
    }

    /// Every response's data scale, in order.
    pub fn data_scales(&self, frame: &Frame) -> Vec<DataScale> {
        (0..self.responses.len())
            .map(|r| self.data_scale_of(r, frame))
            .collect()
    }

    /// The responses' values against the family (§3.6), refused with the
    /// first row that is not one.
    pub fn check_response(&self, frame: &Frame) -> sc_error::Result<()> {
        for (r, sub) in self.responses.iter().enumerate() {
            let Some(column) = frame.column(&sub.response) else {
                return Err(sc_error::Error::invalid(format!(
                    "the dataset has no column `{}`",
                    sub.response
                )));
            };
            if self.family.response == Response::Category {
                continue;
            }
            let rows: Vec<usize> = if sub.subset().is_some() {
                self.response_rows(r, frame)
            } else {
                (0..frame.rows).collect()
            };
            for i in rows {
                if column.is_null_at(i) {
                    continue;
                }
                let Some(y) = number(column, i) else { continue };
                if !self.family.response.accepts(y) {
                    return Err(sc_error::Error::invalid(format!(
                        "the family `{}` models {}, and `{}` is {} in row {}",
                        self.family.name,
                        self.family.response.describe(),
                        sub.response,
                        sc_model::terms::number_text(y),
                        i + 1
                    )));
                }
            }
        }
        Ok(())
    }
}

fn truthy(column: &sc_model::Column, i: usize) -> bool {
    use sc_model::Column;
    match column {
        Column::Bool(v) => v.get(i).copied().flatten().unwrap_or(false),
        Column::Int(v) => v.get(i).copied().flatten().is_some_and(|x| x != 0),
        Column::Float(v) => v.get(i).copied().flatten().is_some_and(|x| x != 0.0),
        Column::Str(v) => v
            .get(i)
            .cloned()
            .flatten()
            .is_some_and(|s| matches!(s.trim(), "true" | "TRUE" | "yes" | "1")),
        _ => false,
    }
}

fn number(column: &sc_model::Column, i: usize) -> Option<f64> {
    use sc_model::Column;
    match column {
        Column::Float(v) => v.get(i).copied().flatten(),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x as f64),
        Column::Bool(v) => v.get(i).copied().flatten().map(|b| f64::from(u8::from(b))),
        _ => None,
    }
}

/// The coefficient names of a fit: per linear predictor, and per (block,
/// linear predictor), each keyed by [`Predictor::key`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Names {
    /// `b` of each linear predictor (its model matrix's columns, then its
    /// monotonic effects `mo<x>`).
    pub b: BTreeMap<String, Vec<String>>,
    /// The effects of each block's part, by (block index, predictor key).
    pub blocks: BTreeMap<(usize, String), Vec<String>>,
    /// The category-specific effects of a predictor, by its key.
    pub cs: BTreeMap<String, Vec<String>>,
}

/// One response line, as sorted.
struct ResponseLine<'f> {
    line: &'f Line,
    column: String,
    aterms: Vec<(ATerm, Span)>,
}

/// The lines, sorted: the responses, each distributional parameter's line
/// by (response, dpar), the non-linear parameters' lines by name, and the
/// fixed dpars by response.
struct Sorted<'f> {
    responses: Vec<ResponseLine<'f>>,
    dpars: BTreeMap<(usize, &'static str), &'f Line>,
    nlpars: BTreeMap<&'f str, &'f Line>,
    unused_nlpars: Vec<(String, &'f Line)>,
    fixed: Vec<(usize, &'static crate::family::Dpar, f64, Span)>,
}

/// The lines: the responses (several make a multivariate model), at most one
/// formula per distributional parameter of each, each the family's, and the
/// non-linear parameters' formulas.
#[allow(clippy::too_many_lines)]
fn sort_lines<'f>(
    text: &str,
    formula: &'f Formula,
    family: &'static Family,
    say: &mut dyn FnMut(ParseError),
) -> Sorted<'f> {
    let mut out = Sorted {
        responses: Vec::new(),
        dpars: BTreeMap::new(),
        nlpars: BTreeMap::new(),
        unused_nlpars: Vec::new(),
        fixed: Vec::new(),
    };
    for line in &formula.lines {
        if let Lhs::Response { column, aterms } = &line.lhs {
            if let Some(first) = out.responses.iter().find(|r| r.column == *column) {
                say(problem(
                    format!(
                        "`{column}` has two formulas (the first is at the start of `{}`)",
                        &text[first.line.span.start
                            ..first.line.span.end.min(first.line.span.start + 30)]
                    ),
                    Some(line.lhs_span),
                ));
                continue;
            }
            out.responses.push(ResponseLine {
                line,
                column: column.clone(),
                aterms: aterms.clone(),
            });
        }
    }
    let mv = out.responses.len() > 1;
    // The response a dpar line (or a fixed dpar) in `group` belongs to.
    let owner = |group: Option<usize>, responses: &[ResponseLine<'_>]| -> Option<usize> {
        if !mv {
            return Some(0);
        }
        let g = group?;
        let mut found = responses
            .iter()
            .enumerate()
            .filter(|(_, r)| r.line.group == Some(g));
        let first = found.next().map(|(i, _)| i);
        if found.next().is_some() { None } else { first }
    };
    let dpar_list = |family: &Family| {
        if family.dpars.is_empty() {
            None
        } else {
            Some(list(
                &family.dpars.iter().map(|d| d.name).collect::<Vec<_>>(),
            ))
        }
    };
    for line in &formula.lines {
        match &line.lhs {
            Lhs::Response { .. } => {}
            Lhs::Nlpar(name) => {
                let used = out
                    .responses
                    .first()
                    .and_then(|r| r.line.nl.as_ref())
                    .is_some_and(|nl| nl.names().0.contains(name));
                if out.nlpars.insert(name.as_str(), line).is_some() {
                    say(problem(
                        format!("`{name}` has two formulas"),
                        Some(line.lhs_span),
                    ));
                }
                if !used {
                    out.unused_nlpars.push((name.clone(), line));
                }
            }
            Lhs::Dpar(name) => match family.dpar(name) {
                Some(d) => {
                    let Some(r) = owner(line.group, &out.responses) else {
                        say(problem(
                            format!(
                                "in a multivariate model, `{name} ~ …` belongs inside the `bf(…)` \
                                 of its response: `bf(y2 ~ x, {name} ~ z)`"
                            ),
                            Some(line.lhs_span),
                        ));
                        continue;
                    };
                    if out.dpars.insert((r, d.name), line).is_some() {
                        say(problem(
                            format!("`{name}` has two formulas"),
                            Some(line.lhs_span),
                        ));
                    }
                }
                None => say(problem(
                    match dpar_list(family) {
                        None => format!(
                            "the family `{}` has no distributional parameters besides the \
                             mean, so `{name}` cannot have a formula",
                            family.name
                        ),
                        Some(l) => {
                            format!("the family `{}` has no `{name}` (it has {l})", family.name)
                        }
                    },
                    Some(line.lhs_span),
                )),
            },
        }
    }
    for f in &formula.fixed {
        match family.dpar(&f.dpar) {
            Some(d) => match owner(f.group, &out.responses) {
                Some(r) => {
                    if out
                        .fixed
                        .iter()
                        .any(|(q, e, _, _)| *q == r && e.name == d.name)
                    {
                        say(problem(
                            format!("`{}` is fixed twice", d.name),
                            Some(f.span),
                        ));
                    }
                    out.fixed.push((r, d, f.value, f.span));
                }
                None => say(problem(
                    format!(
                        "in a multivariate model, `{} = …` belongs inside the `bf(…)` of its \
                         response",
                        f.dpar
                    ),
                    Some(f.span),
                )),
            },
            None => say(problem(
                match dpar_list(family) {
                    None => format!(
                        "the family `{}` has no `{}` to fix (it has no distributional \
                         parameters besides the mean)",
                        family.name, f.dpar
                    ),
                    Some(l) => format!(
                        "the family `{}` has no `{}` to fix (it has {l})",
                        family.name, f.dpar
                    ),
                },
                Some(f.span),
            )),
        }
    }
    out
}

/// The family and every distributional parameter's link: a `bf(family = …)`
/// overrides the form, and a link set in the text overrides the form's.
fn family_and_links(
    formula: &Formula,
    settings: &Settings,
    say: &mut dyn FnMut(ParseError),
) -> Option<(&'static Family, BTreeMap<&'static str, Link>)> {
    let (spec_name, spec_links, spec_span) = match &formula.overrides.family {
        Some(f) => (f.name.clone(), f.links.clone(), Some(f.span)),
        None => (settings.family.clone(), Vec::new(), None),
    };
    let family = match family(&spec_name) {
        Ok(f) => f,
        Err(m) => {
            say(problem(m, spec_span));
            return None;
        }
    };
    let mut links: BTreeMap<&'static str, Link> = BTreeMap::new();
    for dpar in family.dpar_names() {
        if let Some(l) = family.default_link(dpar) {
            links.insert(dpar, l);
        }
    }
    let form_links = settings
        .links
        .iter()
        .map(|(d, l)| (d.clone(), l.clone(), None));
    let text_links = spec_links
        .iter()
        .map(|(d, l)| (d.clone(), l.clone(), spec_span));
    for (dpar, name, span) in form_links.chain(text_links) {
        let Some(allowed) = family.links(&dpar) else {
            say(problem(
                format!(
                    "the family `{}` has no `{dpar}` to set a link for (it has {})",
                    family.name,
                    list(&family.dpar_names())
                ),
                span,
            ));
            continue;
        };
        match Link::parse(&name).filter(|l| allowed.contains(l)) {
            Some(l) => {
                let key = family
                    .dpar_names()
                    .into_iter()
                    .find(|d| *d == dpar)
                    .unwrap_or("mu");
                links.insert(key, l);
            }
            None => say(problem(
                format!(
                    "`{name}` is not a link `{dpar}` of `{}` can take (it takes {})",
                    family.name,
                    list(&allowed.iter().map(|l| l.name()).collect::<Vec<_>>())
                ),
                span,
            )),
        }
    }
    Some((family, links))
}

impl Checker<'_> {
    /// The response against the family, and the addition terms the family
    /// takes, each over a column that is one.
    #[allow(clippy::too_many_lines)]
    fn response(
        &mut self,
        family: &'static Family,
        response: &str,
        lhs_span: Span,
        aterms: &[(ATerm, Span)],
        multivariate: bool,
    ) -> Vec<ATerm> {
        let main_line_span = lhs_span;
        // The response.
        self.column(response, main_line_span);
        if let Some(ty) = self.data.ty(response) {
            let integer = family.response.integer();
            let ok = match (family.response, ty) {
                (Response::Category, ColumnType::Str | ColumnType::Int | ColumnType::Bool) => true,
                (_, ColumnType::Float) => !integer,
                (_, ColumnType::Int) => true,
                (r, ColumnType::Bool) => r == Response::Binary,
                _ => false,
            };
            if !ok {
                self.say(
                    format!(
                        "the family `{}` models {}, and `{response}` is a {} column",
                        family.name,
                        family.response.describe(),
                        type_name(ty)
                    ),
                    main_line_span,
                );
            }
        }

        // The addition terms the family takes.
        let mut aterm_list = Vec::new();
        for (a, span) in aterms {
            let allowed = match a {
                ATerm::Weights(_) => true,
                ATerm::Trials(_) => family.aterms.trials,
                ATerm::Cens(_) | ATerm::Trunc { .. } => family.aterms.cdf,
                ATerm::Se { .. } => family.aterms.se,
                ATerm::Rate(_) => family.aterms.rate,
                ATerm::Thres(_) => family.aterms.thres,
                ATerm::Subset(_) => multivariate,
            };
            if !allowed {
                self.say(
                    format!(
                        "the family `{}` does not take `{}()`{}",
                        family.name,
                        a.name(),
                        match a {
                            ATerm::Trials(_) => " (it is for `binomial` and `beta_binomial`)",
                            ATerm::Se { .. } => " (it is for `gaussian` and `student`)",
                            ATerm::Rate(_) => " (it is for the count families)",
                            ATerm::Thres(_) => " (it is for the ordinal families)",
                            ATerm::Subset(_) =>
                                " here: `subset()` is for a response of a multivariate model",
                            _ => "",
                        }
                    ),
                    *span,
                );
            }
            for c in a.columns() {
                if self.column(c, *span) {
                    let ty = self.data.ty(c);
                    let numeric = matches!(ty, None | Some(ColumnType::Int | ColumnType::Float));
                    let flag_ok = matches!(a, ATerm::Subset(_))
                        && matches!(ty, Some(ColumnType::Bool | ColumnType::Str));
                    let cens_column = matches!(a, ATerm::Cens(cc) if cc.column == c);
                    if !numeric
                        && !flag_ok
                        && !(cens_column && matches!(ty, Some(ColumnType::Str | ColumnType::Bool)))
                    {
                        self.say(
                            format!(
                                "`{}()` needs numbers, and `{c}` is a {} column",
                                a.name(),
                                ty.map_or("", type_name)
                            ),
                            *span,
                        );
                    }
                }
            }
            if let ATerm::Trials(Bound::Number(n)) | ATerm::Weights(Bound::Number(n)) = a {
                if n.is_nan() || *n <= 0.0 {
                    self.say(format!("`{}()` must be positive", a.name()), *span);
                }
            }
            aterm_list.push(a.clone());
        }
        let needs_trials = matches!(family.name, "binomial" | "beta_binomial");
        if needs_trials && !aterms.iter().any(|(a, _)| matches!(a, ATerm::Trials(_))) {
            self.say(
                format!(
                    "a `{}` response needs its number of trials: `{response} | trials(n) ~ …` \
                     (for a 0/1 response, use `bernoulli`)",
                    family.name
                ),
                main_line_span,
            );
        }

        aterm_list
    }

    /// What the family's kind allows of the main formula's line.
    fn kind_rules(&mut self, family: &'static Family, line: &Line, sorted: &Sorted<'_>, r: usize) {
        let rhs = &line.rhs;
        let all_rhs: Vec<&Rhs> = std::iter::once(rhs)
            .chain(
                sorted
                    .dpars
                    .iter()
                    .filter(|((q, _), _)| *q == r)
                    .map(|(_, l)| &l.rhs),
            )
            .chain(sorted.nlpars.values().map(|l| &l.rhs))
            .collect();
        match family.kind {
            Kind::Ordinal => {
                if rhs.intercept != Intercept::On && line.nl.is_none() {
                    self.say(
                        format!(
                            "an ordinal model has thresholds in place of an intercept, so \
                             `{}`'s formula keeps its intercept (remove the `0 +`)",
                            family.name
                        ),
                        line.span,
                    );
                }
                if line.nl.is_some() {
                    self.say(
                        "a non-linear formula is not supported with an ordinal family",
                        line.span,
                    );
                }
                if !rhs.cs.is_empty() && family.name == "cumulative" {
                    self.say(
                        "category-specific effects (`cs()`) are not possible with `cumulative`, \
                         where they could make the probabilities negative; use `sratio`, \
                         `cratio` or `acat`",
                        line.span,
                    );
                }
            }
            Kind::Categorical => {
                if !rhs.cs.is_empty() || !rhs.mo.is_empty() {
                    self.say(
                        "`cs()` and `mo()` are not supported with `categorical`",
                        line.span,
                    );
                }
                if line.nl.is_some() {
                    self.say(
                        "a non-linear formula is not supported with `categorical`",
                        line.span,
                    );
                }
                if rhs.groups.iter().any(|g| g.id.is_some() || g.by.is_some()) {
                    self.say(
                        "with `categorical`, a group-level term takes no `|ID|` or `gr(by = )`",
                        line.span,
                    );
                }
                if !rhs.offsets.is_empty() {
                    self.say(
                        "an `offset()` is not supported with `categorical`",
                        line.span,
                    );
                }
            }
            Kind::Standard => {
                if !rhs.cs.is_empty() {
                    self.say(
                        format!(
                            "category-specific effects (`cs()`) are for the ordinal families, and \
                             `{}` is not one",
                            family.name
                        ),
                        line.span,
                    );
                }
            }
        }
        for other in &all_rhs[1..] {
            if !other.cs.is_empty() {
                self.say(
                    "`cs()` belongs in the main formula of an ordinal model",
                    line.span,
                );
            }
        }
    }

    /// Whether the responses' residuals are correlated: asked with
    /// `rescor`, or by default where it is possible (§R2.3): several gaussian
    /// or student responses with no addition terms.
    fn rescor(&mut self, family: &'static Family, formula: &Formula, subs: &[Sub]) -> bool {
        let possible_family = matches!(family.name, "gaussian" | "student");
        let plain = subs.iter().all(|s| s.aterms.is_empty() && s.nl.is_none());
        let mv = subs.len() > 1;
        match formula.overrides.rescor {
            Some(true) if !mv => {
                self.say(
                    "`rescor = TRUE` correlates the residuals of several responses, and this \
                     model has one",
                    Span::default(),
                );
                false
            }
            Some(true) if !possible_family => {
                self.say(
                    format!(
                        "residual correlations are for `gaussian` and `student` responses, and \
                         the family is `{}`; write `rescor = FALSE`",
                        family.name
                    ),
                    Span::default(),
                );
                false
            }
            Some(true) if !plain => {
                self.say(
                    "residual correlations need responses without addition terms (`weights`, \
                     `cens`, `trunc`, `se`, `subset`); write `rescor = FALSE`",
                    Span::default(),
                );
                false
            }
            Some(on) => on,
            None => mv && possible_family && plain,
        }
    }
}

struct Checker<'a> {
    text: &'a str,
    data: Data<'a>,
    errors: Vec<ParseError>,
}

impl Checker<'_> {
    fn say(&mut self, message: impl Into<String>, span: Span) {
        self.errors.push(ParseError::new(message, span));
    }

    /// The first place `name` is written as a whole name inside `span`, or
    /// `span` itself — so an underline is under the column, not the line.
    fn narrow(&self, span: Span, name: &str) -> Span {
        let end = span.end.min(self.text.len());
        let Some(within) = self.text.get(span.start..end) else {
            return span;
        };
        let word = |c: char| c.is_alphanumeric() || c == '.' || c == '_';
        let mut from = 0;
        while let Some(at) = within[from..].find(name) {
            let start = from + at;
            let stop = start + name.len();
            let before = within[..start].chars().next_back();
            let after = within[stop..].chars().next();
            if !before.is_some_and(word) && !after.is_some_and(word) {
                return Span::new(span.start + start, span.start + stop);
            }
            from = stop;
        }
        span
    }

    /// Whether `column` is one; says so where it is not.
    fn column(&mut self, column: &str, span: Span) -> bool {
        if self.data.has(column) {
            return true;
        }
        let names = self.data.names();
        let near = closest(column, &names);
        let span = self.narrow(span, column);
        self.say(
            match near {
                Some(n) => {
                    format!("`{column}` is not a column of the dataset; did you mean `{n}`?")
                }
                None => format!("`{column}` is not a column of the dataset"),
            },
            span,
        );
        false
    }

    /// A column a non-linear formula reads: a number.
    fn numeric_column(&mut self, column: &str, span: Span) {
        if !self.column(column, span) {
            return;
        }
        if let Some(ty) = self.data.ty(column) {
            if !matches!(ty, ColumnType::Int | ColumnType::Float | ColumnType::Bool) {
                let span = self.narrow(span, column);
                self.say(
                    format!(
                        "`{column}` is a {} column, and a non-linear formula is arithmetic over \
                         numbers",
                        type_name(ty)
                    ),
                    span,
                );
            }
        }
    }

    /// The column of `mo(x)`: ordered — whole numbers (or yes/no); text has
    /// no order this release can trust.
    fn mo_column(&mut self, column: &str, span: Span) {
        if !self.column(column, span) {
            return;
        }
        if let Some(ty) = self.data.ty(column) {
            if !matches!(ty, ColumnType::Int | ColumnType::Bool) {
                let span = self.narrow(span, column);
                self.say(
                    format!(
                        "`mo({column})` needs an ordinal column of whole numbers (its levels in \
                         order), and `{column}` is a {} column",
                        type_name(ty)
                    ),
                    span,
                );
            }
        }
    }

    fn variable(&mut self, v: &VarExpr, span: Span) {
        let bare = v.as_column().is_some();
        for c in v.columns() {
            if !self.column(c, span) {
                continue;
            }
            match self.data.ty(c) {
                Some(ColumnType::Date) => self.say(
                    format!(
                        "`{c}` is a date; make it a number in the dataset (days since a start, a \
                         year) to use it in a formula"
                    ),
                    span,
                ),
                Some(ColumnType::Null) => {
                    self.say(format!("`{c}` is null in every row"), span);
                }
                Some(ColumnType::Str | ColumnType::Bool) if !bare => self.say(
                    format!(
                        "`{c}` is categorical, so `{}` has no numeric value; a categorical \
                         column can only appear on its own",
                        v.label()
                    ),
                    span,
                ),
                _ => {}
            }
        }
    }
}

fn type_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Float => "decimal-number",
        ColumnType::Int => "whole-number",
        ColumnType::Bool => "yes/no",
        ColumnType::Str => "text",
        ColumnType::Date => "date",
        ColumnType::Null => "always-null",
    }
}

fn list(items: &[&str]) -> String {
    items
        .iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The column nearest `name` by edit distance, when it is near enough to be a
/// typo.
fn closest<'a>(name: &str, names: &'a [String]) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    names
        .iter()
        .map(|n| (distance(name, n), n))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n.as_str())
}

fn distance(a: &str, b: &str) -> usize {
    // Optimal string alignment: insertions, deletions, substitutions and
    // transpositions of neighbours each cost one (`arae` is one from `area`).
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}
