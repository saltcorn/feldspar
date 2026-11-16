//! A parsed [`Formula`] against the model's dataset and form (R1.2–R1.3): the
//! family and its links, the response and its addition terms, each linear
//! predictor's terms as the binder's [`TermsSpec`], the group-level terms
//! merged into **blocks**, and the prior classes all of that creates.
//!
//! What can be checked depends on what is known. With the column names only
//! (a model listed at load), every name is checked to be a column; with the
//! dataset's shape (a save from the editor), also the types — the response
//! against the family, no date or text inside a transform; with the frames (a
//! preview, a fit), also the values and the categorical levels, so a prior row
//! naming a level can be matched exactly.

use std::collections::BTreeMap;

use sc_model::terms::{Coding, TermsSpec, VarExpr, code_terms, fit_terms};
use sc_model::{ColumnType, DatasetShape, Frame};

use crate::family::{Family, Link, Response, family};
use crate::parse::{
    ATerm, Bound, Formula, GroupTerm, Intercept, Lhs, ParseError, ParseErrors, Rhs, Span,
};
use crate::prior::{ClassKind, ClassSlot, Coef, CoefPart, CoefTemplate, DataScale, DefaultPrior};

/// The main line, its response and its addition terms.
type MainLine<'f> = (&'f crate::parse::Line, &'f String, &'f Vec<(ATerm, Span)>);

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
            Data::Names(n) => n.iter().any(|c| c == column),
            Data::Shape(s) => s.column(column).is_some(),
            Data::Frame(f) => f.column(column).is_some(),
        }
    }

    fn names(&self) -> Vec<String> {
        match self {
            Data::Names(n) => n.to_vec(),
            Data::Shape(s) => s.columns.iter().map(|c| c.name.clone()).collect(),
            Data::Frame(f) => f.columns.iter().map(|(n, _)| n.clone()).collect(),
        }
    }

    fn ty(&self, column: &str) -> Option<ColumnType> {
        match self {
            Data::Names(_) => None,
            Data::Shape(s) => s.column(column),
            Data::Frame(f) => f.column(column).map(sc_model::Column::kind),
        }
    }

    fn typed(&self) -> bool {
        !matches!(self, Data::Names(_))
    }
}

/// One linear predictor: `mu`'s, or a distributional parameter's.
#[derive(Debug, Clone, PartialEq)]
pub struct Predictor {
    /// `mu`, `sigma`, ….
    pub dpar: &'static str,
    /// Its link.
    pub link: Link,
    /// Its intercept.
    pub intercept: Intercept,
    /// Its population-level terms, for a `terms` binding (no intercept
    /// column: the intercept is a parameter of its own, centred or not —
    /// except `0 + Intercept`, where it is the matrix's first column).
    pub terms: TermsSpec,
    /// Its offsets.
    pub offsets: Vec<VarExpr>,
    /// Its `b` coefficients, as templates until the levels are known.
    pub coefs: Vec<CoefTemplate>,
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
    /// Its parts, one per linear predictor it varies.
    pub parts: Vec<BlockPart>,
}

/// The effects one block varies in one linear predictor.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockPart {
    /// The linear predictor.
    pub dpar: &'static str,
    /// The effects' model matrix (`Z`): an `Intercept` column when the term
    /// has one, then the terms.
    pub terms: TermsSpec,
    /// The effects' names, as templates until the levels are known.
    pub coefs: Vec<CoefTemplate>,
}

/// A formula resolved against the dataset and the form.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The family.
    pub family: &'static Family,
    /// Every distributional parameter's link, `mu` included.
    pub links: BTreeMap<&'static str, Link>,
    /// Whether the population-level predictors are centred.
    pub center: bool,
    /// The response column.
    pub response: String,
    /// Its addition terms.
    pub aterms: Vec<ATerm>,
    /// The linear predictors: `mu` first, then the distributional
    /// parameters with a formula, in the family's order.
    pub predictors: Vec<Predictor>,
    /// The family's distributional parameters without a formula: one
    /// parameter each.
    pub scalars: Vec<&'static str>,
    /// The group-level blocks.
    pub blocks: Vec<Block>,
    /// What the editor should say: overrides from `bf()`.
    pub notes: Vec<String>,
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

    let (main, dpar_lines) = sort_lines(text, formula, family, &mut |e| located(e, &mut errors));
    let Some((main_line, response, aterms)) = main else {
        located(
            problem("no line names the response", Some(Span::new(0, text.len()))),
            &mut errors,
        );
        return Err(ParseErrors(errors));
    };

    let mut checker = Checker {
        text,
        data,
        errors: Vec::new(),
    };

    let aterm_list = checker.response(family, response, main_line.lhs_span, aterms);

    // The linear predictors, `mu` first, then in the family's order.
    let mut predictors = Vec::new();
    let mut parts: Vec<(&'static str, &GroupTerm)> = Vec::new();
    let mut lines: Vec<(&'static str, &Rhs, Span)> = vec![("mu", &main_line.rhs, main_line.span)];
    for d in family.dpars {
        if let Some(line) = dpar_lines.get(d.name) {
            lines.push((d.name, &line.rhs, line.span));
        }
    }
    for (dpar, rhs, span) in &lines {
        for t in &rhs.terms {
            for v in &t.variables {
                checker.variable(v, *span);
            }
        }
        for o in &rhs.offsets {
            checker.variable(o, *span);
        }
        for g in &rhs.groups {
            for f in &g.factors {
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
            parts.push((dpar, g));
        }
        let terms = population_terms(rhs);
        let coefs = templates(&terms, &data);
        predictors.push(Predictor {
            dpar,
            link: links.get(dpar).copied().unwrap_or(Link::Identity),
            intercept: rhs.intercept,
            terms,
            offsets: rhs.offsets.clone(),
            coefs,
        });
    }

    let blocks = merge_blocks(parts, &data, &mut checker);

    for e in checker.errors {
        located(e, &mut errors);
    }
    if !errors.is_empty() {
        return Err(ParseErrors(errors));
    }
    let scalars = family
        .dpars
        .iter()
        .filter(|d| !dpar_lines.contains_key(d.name))
        .map(|d| d.name)
        .collect();
    Ok(Resolved {
        family,
        links,
        center,
        response: response.clone(),
        aterms: aterm_list,
        predictors,
        scalars,
        blocks,
        notes: formula.overrides.notes.clone(),
    })
}

/// The blocks: one per group-level term, except that terms sharing an `|ID|`
/// on the same grouping factor are one (across linear predictors too).
fn merge_blocks(
    parts: Vec<(&'static str, &GroupTerm)>,
    data: &Data<'_>,
    checker: &mut Checker<'_>,
) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for (dpar, g) in parts {
        let group = g.group();
        let terms = group_terms(g);
        let coefs = templates(&terms, data);
        let part = BlockPart { dpar, terms, coefs };
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
                } else if b.parts.iter().any(|p| p.dpar == dpar) {
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
        blocks.push(Block {
            group,
            factors: g.factors.clone(),
            id: g.id.clone(),
            correlated: g.correlated,
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
    /// The linear predictor of `dpar`.
    pub fn predictor(&self, dpar: &str) -> Option<&Predictor> {
        self.predictors.iter().find(|p| p.dpar == dpar)
    }

    /// The coefficient names of every linear predictor and block part over
    /// `frame`: the model matrices' columns, with the levels the rows have.
    pub fn names(&self, frame: &Frame) -> sc_error::Result<Names> {
        let mut names = Names::default();
        for p in &self.predictors {
            let (_, m) = fit_terms(&p.terms, frame).map_err(|e| {
                sc_error::Error::invalid(format!("the formula of `{}`: {e}", p.dpar))
            })?;
            names.b.insert(p.dpar.to_owned(), m.columns);
        }
        for (k, b) in self.blocks.iter().enumerate() {
            for part in &b.parts {
                let (_, m) = fit_terms(&part.terms, frame).map_err(|e| {
                    sc_error::Error::invalid(format!(
                        "the group-level term over `{}`: {e}",
                        b.group
                    ))
                })?;
                names.blocks.insert((k, part.dpar.to_owned()), m.columns);
            }
        }
        Ok(names)
    }

    /// The prior classes the formula creates (§3.7): what a row can target
    /// and what the table lists. With `names` (after the data is read) the
    /// coefficients are the matrices' columns; without, their templates.
    pub fn prior_slots(&self, names: Option<&Names>) -> Vec<ClassSlot> {
        let mut slots = Vec::new();
        for p in &self.predictors {
            let dpar = if p.dpar == "mu" { "" } else { p.dpar };
            if p.intercept == Intercept::On {
                slots.push(ClassSlot {
                    class: "Intercept".to_owned(),
                    dpar: dpar.to_owned(),
                    group: String::new(),
                    coefs: Vec::new(),
                    kind: ClassKind::Real,
                    default: if p.dpar == "mu" {
                        DefaultPrior::InterceptScale
                    } else {
                        DefaultPrior::Fixed("student_t", &[3.0, 0.0, 2.5])
                    },
                });
            }
            let coefs: Vec<Coef> = match names.and_then(|n| n.b.get(p.dpar)) {
                Some(cols) => cols.iter().cloned().map(Coef::Named).collect(),
                None => p.coefs.iter().cloned().map(Coef::Template).collect(),
            };
            if !coefs.is_empty() {
                slots.push(ClassSlot {
                    class: "b".to_owned(),
                    dpar: dpar.to_owned(),
                    group: String::new(),
                    coefs,
                    kind: ClassKind::Real,
                    default: DefaultPrior::Flat,
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
                let dpar = if part.dpar == "mu" { "" } else { part.dpar };
                let coefs: Vec<Coef> =
                    match names.and_then(|n| n.blocks.get(&(k, part.dpar.to_owned()))) {
                        Some(cols) => cols.iter().cloned().map(Coef::Named).collect(),
                        None => part.coefs.iter().cloned().map(Coef::Template).collect(),
                    };
                size += coefs.len();
                match sd.iter_mut().find(|s| s.dpar == dpar && s.group == b.group) {
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
                    }),
                }
            }
            // A correlation needs two effects — or possibly two, before the
            // levels are known.
            let levels_unknown = names.is_none()
                && b.parts.iter().any(|p| {
                    p.coefs
                        .iter()
                        .any(|c| c.parts.iter().any(|q| !matches!(q, CoefPart::Exact(_))))
                });
            let maybe_two = size > 1 || levels_unknown;
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
            });
        }
        for name in &self.scalars {
            if let Some(d) = self.family.dpar(name) {
                slots.push(ClassSlot {
                    class: d.name.to_owned(),
                    dpar: String::new(),
                    group: String::new(),
                    coefs: Vec::new(),
                    kind: d.kind,
                    default: d.prior,
                });
            }
        }
        slots
    }

    /// The scale the data-dependent defaults are computed with, over the
    /// response in `frame` (§3.7): the median and the floored MAD of the
    /// response on `mu`'s link scale.
    pub fn data_scale(&self, frame: &Frame) -> DataScale {
        let binary =
            matches!(self.family.response, Response::Binary) || self.family.name == "binomial";
        if binary {
            return DataScale::default();
        }
        let Some(column) = frame.column(&self.response) else {
            return DataScale::default();
        };
        let link = self.links.get("mu").copied().unwrap_or(Link::Identity);
        let lognormal = self.family.name == "lognormal";
        let values: Vec<f64> = (0..frame.rows)
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

    /// The response's values against the family (§3.6), refused with the
    /// first row that is not one.
    pub fn check_response(&self, frame: &Frame) -> sc_error::Result<()> {
        let Some(column) = frame.column(&self.response) else {
            return Err(sc_error::Error::invalid(format!(
                "the dataset has no column `{}`",
                self.response
            )));
        };
        for i in 0..frame.rows {
            if column.is_null_at(i) {
                continue;
            }
            let Some(y) = number(column, i) else { continue };
            if !self.family.response.accepts(y) {
                return Err(sc_error::Error::invalid(format!(
                    "the family `{}` models {}, and `{}` is {} in row {}",
                    self.family.name,
                    self.family.response.describe(),
                    self.response,
                    sc_model::terms::number_text(y),
                    i + 1
                )));
            }
        }
        Ok(())
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
/// linear predictor).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Names {
    /// `b` of each linear predictor, by its dpar (`mu`, `sigma`).
    pub b: BTreeMap<String, Vec<String>>,
    /// The effects of each block's part, by (block index, dpar).
    pub blocks: BTreeMap<(usize, String), Vec<String>>,
}

/// The lines: one response, and at most one formula per distributional
/// parameter, each the family's.
fn sort_lines<'f>(
    text: &str,
    formula: &'f Formula,
    family: &'static Family,
    say: &mut dyn FnMut(ParseError),
) -> (
    Option<MainLine<'f>>,
    BTreeMap<&'static str, &'f crate::parse::Line>,
) {
    let mut main: Option<MainLine<'_>> = None;
    let mut dpar_lines: BTreeMap<&'static str, &crate::parse::Line> = BTreeMap::new();
    for line in &formula.lines {
        match &line.lhs {
            Lhs::Response { column, aterms } => {
                if let Some((first, first_column, _)) = main {
                    say(problem(
                        if first_column == column {
                            format!(
                                "`{column}` has two formulas (the first is at the start of \
                                     `{}`)",
                                &text[first.span.start..first.span.end.min(first.span.start + 30)]
                            )
                        } else {
                            format!(
                                "a second response, `{column}`, makes a multivariate model, \
                                     which arrives in a later release (R2); if `{column}` is a \
                                     distributional parameter, the family `{}` does not have it \
                                     (it has {})",
                                family.name,
                                list(&family.dpar_names())
                            )
                        },
                        Some(line.lhs_span),
                    ));
                    continue;
                }
                main = Some((line, column, aterms));
            }
            Lhs::Dpar(name) => match family.dpar(name) {
                Some(d) => {
                    if dpar_lines.insert(d.name, line).is_some() {
                        say(problem(
                            format!("`{name}` has two formulas"),
                            Some(line.lhs_span),
                        ));
                    }
                }
                None => say(problem(
                    if family.dpars.is_empty() {
                        format!(
                            "the family `{}` has no distributional parameters besides the \
                                 mean, so `{name}` cannot have a formula",
                            family.name
                        )
                    } else {
                        format!(
                            "the family `{}` has no `{name}` (it has {})",
                            family.name,
                            list(&family.dpars.iter().map(|d| d.name).collect::<Vec<_>>())
                        )
                    },
                    Some(line.lhs_span),
                )),
            },
        }
    }
    (main, dpar_lines)
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
    fn response(
        &mut self,
        family: &'static Family,
        response: &str,
        lhs_span: Span,
        aterms: &[(ATerm, Span)],
    ) -> Vec<ATerm> {
        let main_line_span = lhs_span;
        // The response.
        self.column(response, main_line_span);
        if let Some(ty) = self.data.ty(response) {
            let integer = family.response.integer();
            let ok = match ty {
                ColumnType::Float => !integer,
                ColumnType::Int => true,
                ColumnType::Bool => family.response == Response::Binary,
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
            };
            if !allowed {
                self.say(
                    format!(
                        "the family `{}` does not take `{}()`{}",
                        family.name,
                        a.name(),
                        match a {
                            ATerm::Trials(_) => " (it is for `binomial`)",
                            ATerm::Se { .. } => " (it is for `gaussian` and `student`)",
                            ATerm::Rate(_) => " (it is for the count families)",
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
                    let cens_column = matches!(a, ATerm::Cens(cc) if cc.column == c);
                    if !numeric
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
        if family.name == "binomial" && !aterms.iter().any(|(a, _)| matches!(a, ATerm::Trials(_))) {
            self.say(
                format!(
                    "a `binomial` response needs its number of trials: `{response} | trials(n) ~ …` \
                     (for a 0/1 response, use `bernoulli`)"
                ),
                main_line_span,
            );
        }

        aterm_list
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
