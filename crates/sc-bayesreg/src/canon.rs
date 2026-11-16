//! Canonicalisation (TODO-bayesian-regression §5, R1.5, R2): a [`Resolved`]
//! formula → the [`Structure`] that alone decides the generated program's text,
//! and the [`DataPlan`] that says how each of the program's data variables is
//! bound.
//!
//! The rules of §5:
//!
//! 1. **Names out.** No column name reaches the structure; each becomes a
//!    binding of a positional data variable (`X_1`, `J_2`), and the user's
//!    names survive only in the bindings and the labels.
//! 2. **Counts out.** The number of coefficients, of group-level effects, of
//!    levels, of thresholds, of categories and of rows are data: `y ~ x` and
//!    `y ~ a + b + c` are one program, and so are `(1 | g)` and `(1 + x | g)`
//!    — a correlated block's Cholesky factor is `cholesky_factor_corr[M]`
//!    whatever `M` is. (The number of `mo()` terms is structural: each has a
//!    simplex of its own.)
//! 3. **Order out.** Linear predictors are in the fixed order `mu` (or the
//!    non-linear parameters, in order of first appearance), then the family's
//!    own; blocks are sorted by their structural signature, and so are the
//!    responses of a multivariate model, ties kept in the order written (a tie
//!    is structurally identical, so its order cannot change the text).
//! 4. **Non-linear** (R2.2): the expression's parameters are renamed `nlp_1`,
//!    … and its columns `C_1`, … in order of first appearance, and its
//!    literals printed in one form. Commutative reordering is not attempted:
//!    `a*b` and `b*a` are two programs (a cache miss, never a wrong answer).
//! 5. **Versioned.** [`GENERATOR_VERSION`] is in the structure, so a change to
//!    what the generator emits for a structure changes every key.
//!
//! What *is* structural: the family and every link; per linear predictor
//! whether it has an intercept (centred or not, or thresholds), coefficients
//! (bounded or not), an offset, monotonic effects (how many) and
//! category-specific effects; per block whether it is correlated, has a
//! covariance per level of a `by` column, and which predictors it feeds; the
//! response's addition terms, and whether each argument is a column or one
//! number; the responses of a multivariate model and their residual
//! correlation; the non-linear expression; and the two structural priors
//! (§2.5), a bound on `b` and a fixed distributional parameter.
//!
//! Every field R2 adds is left out of the canonical JSON when it is unused,
//! so an R1 model's structure — and so its program and its compiled
//! executable — is what it was.

use std::collections::BTreeMap;

use sc_model::terms::VarExpr;
use sc_model::{Binding, DerivedColumn, DimensionSpec, MAIN_DATASET, Policies, Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::family::Kind;
use crate::parse::{ATerm, Bound, Intercept, NlExpr};
use crate::prior::{ClassKind, Prior, PriorFamily, PriorRow};
use crate::resolve::Resolved;

/// The generator's version: bumped by any change to the text it emits for any
/// structure (§5 rule 5), which the golden files catch.
pub const GENERATOR_VERSION: u32 = 1;

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Everything that changes the program's text, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Structure {
    /// [`GENERATOR_VERSION`].
    pub generator: u32,
    /// The family, as brms spells it.
    pub family: String,
    /// The linear predictors: per response (in canonical order), `mu` first
    /// (or the non-linear parameters), then the family's order.
    pub predictors: Vec<PredictorShape>,
    /// The first response's distributional parameters without a formula, in
    /// the family's order.
    pub scalars: Vec<ScalarShape>,
    /// The group-level blocks, sorted (rule 3).
    pub blocks: Vec<BlockShape>,
    /// The first response and its addition terms.
    pub response: ResponseShape,
    /// The other responses of a multivariate model, in canonical order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more: Vec<SubShape>,
    /// Residual correlations between the responses (gaussian).
    #[serde(default, skip_serializing_if = "is_false")]
    pub rescor: bool,
    /// The main formula, when it is non-linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nl: Option<NlShape>,
    /// Whether the program computes each row's expected response,
    /// `epred_1`, in generated quantities.
    #[serde(default, skip_serializing_if = "is_false")]
    pub epred: bool,
}

/// A later response of a multivariate model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubShape {
    /// Its distributional parameters without a formula.
    pub scalars: Vec<ScalarShape>,
    /// Its addition terms.
    pub response: ResponseShape,
}

/// A non-linear main formula.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NlShape {
    /// `mu`'s link: the expression is on its scale.
    pub link: String,
    /// The expression as Stan, over `nlp_1…` and `C_1…` (rule 4).
    pub expr: String,
    /// How many columns it reads.
    pub covariates: usize,
}

/// One linear predictor's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictorShape {
    /// `mu`, `sigma`, …; `nlp_1`, … for a non-linear parameter.
    pub dpar: String,
    /// Its link.
    pub link: String,
    /// Its intercept.
    pub intercept: InterceptShape,
    /// Its population-level coefficients.
    pub b: CoefShape,
    /// Whether it has an offset (`offset()`, or a Poisson `rate()`'s log is
    /// not one — that is the response's).
    pub offset: bool,
    /// The response it is of, by canonical position (0 for the first).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub resp: usize,
    /// How many monotonic effects (`mo()`) it has.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mo: usize,
    /// Whether it has category-specific effects (`cs()`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub cs: bool,
}

/// How a linear predictor has its intercept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterceptShape {
    /// None, or one that is a column of the model matrix (`0 + Intercept`).
    None,
    /// A parameter of its own on the centred predictors, the real intercept
    /// recovered in generated quantities (brms's documented `center = TRUE`).
    Centred,
    /// A parameter of its own, the predictors as they are.
    Plain,
    /// An ordinal model's thresholds, on the centred predictors.
    Thresholds,
    /// An ordinal model's thresholds, the predictors as they are.
    ThresholdsPlain,
}

impl InterceptShape {
    /// Whether the predictors are centred around it.
    pub fn centred(self) -> bool {
        matches!(self, InterceptShape::Centred | InterceptShape::Thresholds)
    }

    /// Whether it is an ordinal model's thresholds.
    pub fn thresholds(self) -> bool {
        matches!(
            self,
            InterceptShape::Thresholds | InterceptShape::ThresholdsPlain
        )
    }
}

/// Whether a linear predictor has population-level coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoefShape {
    /// No terms.
    None,
    /// `vector[K] b`.
    Free,
    /// `vector<lower=lb, upper=ub>[K] b`, the bounds data (±∞ where a
    /// coefficient has none).
    Bounded,
}

/// A distributional parameter with no formula of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScalarShape {
    /// Its name.
    pub dpar: String,
    /// `real`, `positive`, `above_one` or `unit`.
    pub support: String,
    /// Fixed by `constant()` or `dpar = number`: data rather than a
    /// parameter.
    pub fixed: bool,
}

/// One block of group-level effects.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockShape {
    /// Correlated (an LKJ on its Cholesky factor) or not.
    pub correlated: bool,
    /// The linear predictors it feeds, by position in
    /// [`Structure::predictors`], ascending.
    pub parts: Vec<usize>,
    /// A covariance (SDs, and correlations) per level of a `by` column.
    #[serde(default, skip_serializing_if = "is_false")]
    pub by: bool,
}

/// Where an addition term's argument comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgShape {
    /// A column: one value per row.
    Column,
    /// One number for every row.
    Constant,
}

/// The response's addition terms.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResponseShape {
    /// `weights()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<ArgShape>,
    /// `trials()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trials: Option<ArgShape>,
    /// `cens()`: whether it has the interval's upper end (`cens(c, y2)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cens: Option<bool>,
    /// `trunc(lb = )`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunc_lb: Option<ArgShape>,
    /// `trunc(ub = )`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunc_ub: Option<ArgShape>,
    /// `se()`: whether a residual SD is estimated as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub se: Option<bool>,
    /// `rate()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<ArgShape>,
    /// `subset()`: measured on some rows only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub subset: bool,
}

impl ResponseShape {
    /// Whether the likelihood is a loop over the rows rather than one
    /// vectorised statement.
    pub fn per_row(&self) -> bool {
        self.weights.is_some()
            || self.cens.is_some()
            || self.trunc_lb.is_some()
            || self.trunc_ub.is_some()
    }
}

impl Structure {
    /// Its canonical JSON: what the program's header carries and what is
    /// hashed.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The family's table entry.
    pub fn family_entry(&self) -> Option<&'static crate::Family> {
        crate::family::family(&self.family).ok()
    }

    /// Whether the response is whole numbers (`array[N] int`).
    pub fn integer_response(&self) -> bool {
        self.family_entry().is_some_and(|f| f.response.integer())
    }

    /// The family's kind.
    pub fn kind(&self) -> Kind {
        self.family_entry().map_or(Kind::Standard, |f| f.kind)
    }

    /// The position (from 1) of the linear predictor of `dpar` of the first
    /// response.
    pub fn predictor_position(&self, dpar: &str) -> Option<usize> {
        self.predictors
            .iter()
            .position(|p| p.dpar == dpar && p.resp == 0)
            .map(|i| i + 1)
    }

    /// How many responses.
    pub fn responses(&self) -> usize {
        1 + self.more.len()
    }

    /// Response `r`'s scalars and addition terms.
    pub fn sub(&self, r: usize) -> (&[ScalarShape], &ResponseShape) {
        if r == 0 {
            (&self.scalars, &self.response)
        } else {
            let s = &self.more[r - 1];
            (&s.scalars, &s.response)
        }
    }

    /// `mu`'s link of the first response.
    pub fn mu_link(&self) -> &str {
        match &self.nl {
            Some(nl) => &nl.link,
            None => self
                .predictors
                .iter()
                .find(|p| p.dpar == "mu" && p.resp == 0)
                .map_or("identity", |p| p.link.as_str()),
        }
    }
}

/// The suffix of response `r`'s variables: none for the first, `_2`, `_3`, …
/// for the others.
pub fn suffix(r: usize) -> String {
    if r == 0 {
        String::new()
    } else {
        format!("_{}", r + 1)
    }
}

/// A formula canonicalised: its structure, and how its parts map back.
#[derive(Debug, Clone, PartialEq)]
pub struct Canonical {
    /// The structure.
    pub structure: Structure,
    /// For each canonical block (in order), the index of the resolved block
    /// it is.
    pub block_order: Vec<usize>,
    /// For each canonical block, its parts' resolved predictor indices in
    /// canonical order.
    pub block_parts: Vec<Vec<usize>>,
    /// For each canonical predictor position, the resolved predictor it is.
    pub predictor_order: Vec<usize>,
    /// For each canonical response position, the resolved response it is.
    pub response_order: Vec<usize>,
    /// For each canonical predictor with a non-linear formula's columns: the
    /// columns `C_1…`, in order.
    pub covariates: Vec<String>,
}

impl Canonical {
    /// The canonical position (from 0) of resolved predictor `i`.
    pub fn position_of(&self, i: usize) -> usize {
        self.predictor_order
            .iter()
            .position(|&p| p == i)
            .unwrap_or(i)
    }

    /// The canonical position of resolved response `r`.
    pub fn response_position(&self, r: usize) -> usize {
        self.response_order
            .iter()
            .position(|&q| q == r)
            .unwrap_or(r)
    }
}

/// Options that change the program beyond what the formula says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// Compute each row's expected response in generated quantities.
    pub epred: bool,
}

/// Canonicalise `resolved`, with the admin's explicit prior `rows` (whose
/// bounds and `constant()`s are structural). A sentence when the formula uses
/// something this release cannot generate.
pub fn canonicalise(resolved: &Resolved, rows: &[PriorRow]) -> Result<Canonical, String> {
    canonicalise_with(resolved, rows, Options::default())
}

/// [`canonicalise`] with [`Options`].
#[allow(clippy::too_many_lines)] // the structure, part by part
pub fn canonicalise_with(
    resolved: &Resolved,
    rows: &[PriorRow],
    options: Options,
) -> Result<Canonical, String> {
    let family = resolved.family;
    let mv = resolved.multivariate();
    let resp_name = |r: usize| {
        if mv {
            resolved.responses[r].response.clone()
        } else {
            String::new()
        }
    };
    let row_prior = |r: &PriorRow| Prior::parse(&r.prior).ok();
    let constant_rows: Vec<&PriorRow> = rows
        .iter()
        .filter(|r| row_prior(r).is_some_and(|p| p.family == PriorFamily::Constant))
        .collect();
    for r in &constant_rows {
        let sub = resolved
            .responses
            .iter()
            .enumerate()
            .find(|(i, _)| resp_name(*i) == r.resp);
        let scalar =
            sub.is_some_and(|(_, s)| s.scalars.contains(&r.class.as_str())) && r.coef.is_empty();
        if !scalar {
            return Err(format!(
                "{}: `constant()` can fix a distributional parameter that has no formula \
                 (`sigma`, `nu`, …); to fix one, write `{} = 2` in the formula, and a \
                 coefficient cannot be fixed",
                r.describe(),
                r.class
            ));
        }
    }
    if family.kind == Kind::Categorical && mv {
        return Err("a multivariate model with a `categorical` response is not supported".into());
    }

    // Each predictor's shape, in resolved order.
    let mut shapes: Vec<PredictorShape> = Vec::new();
    for p in &resolved.predictors {
        let has_b = !p.terms.terms.is_empty() || p.intercept == Intercept::Coefficient;
        let resp = resp_name(p.resp);
        let nlpar = p.nlpar.clone().unwrap_or_default();
        let bounded = rows.iter().any(|r| {
            r.class == "b"
                && r.dpar == p.dpar_key()
                && r.nlpar == nlpar
                && r.resp == resp
                && (r.lb.is_some() || r.ub.is_some())
        });
        if bounded && family.kind == Kind::Categorical {
            return Err(
                "bounds on `b` are not supported with `categorical`, whose coefficients are a \
                 matrix"
                    .into(),
            );
        }
        let b = match (has_b, bounded) {
            (false, _) => CoefShape::None,
            (true, false) => CoefShape::Free,
            (true, true) => CoefShape::Bounded,
        };
        let ordinal = family.kind == Kind::Ordinal && p.dpar == "mu" && p.nlpar.is_none();
        let intercept = match p.intercept {
            Intercept::On if ordinal && resolved.center && has_b => InterceptShape::Thresholds,
            Intercept::On if ordinal => InterceptShape::ThresholdsPlain,
            Intercept::Off | Intercept::Coefficient => InterceptShape::None,
            Intercept::On if resolved.center && has_b => InterceptShape::Centred,
            Intercept::On => InterceptShape::Plain,
        };
        shapes.push(PredictorShape {
            dpar: p.dpar.to_owned(),
            link: p.link.name().to_owned(),
            intercept,
            b,
            offset: !p.offsets.is_empty(),
            resp: 0,
            mo: p.mo.len(),
            cs: p.cs.is_some(),
        });
    }

    // Each response's own shape.
    let mut subs: Vec<SubShape> = Vec::new();
    for (r, sub) in resolved.responses.iter().enumerate() {
        let scalars = family
            .dpars
            .iter()
            .filter(|d| sub.scalars.contains(&d.name) || sub.fixed.contains_key(d.name))
            .map(|d| ScalarShape {
                dpar: d.name.to_owned(),
                support: match d.kind {
                    ClassKind::Positive => "positive",
                    ClassKind::AboveOne => "above_one",
                    ClassKind::Unit => "unit",
                    ClassKind::Real | ClassKind::Correlation | ClassKind::Simplex => "real",
                }
                .to_owned(),
                fixed: sub.fixed.contains_key(d.name)
                    || constant_rows
                        .iter()
                        .any(|c| c.class == d.name && c.resp == resp_name(r)),
            })
            .collect();
        subs.push(SubShape {
            scalars,
            response: response_shape(resolved, r)?,
        });
    }

    // The responses in canonical order: by signature, ties as written.
    let mut response_order: Vec<usize> = (0..resolved.responses.len()).collect();
    if mv {
        let signature = |r: usize| {
            let preds: Vec<&PredictorShape> = resolved
                .predictors
                .iter()
                .zip(&shapes)
                .filter(|(p, _)| p.resp == r)
                .map(|(_, s)| s)
                .collect();
            serde_json::to_string(&(&subs[r], preds)).unwrap_or_default()
        };
        response_order.sort_by_cached_key(|&r| signature(r));
    }
    let mut predictor_order: Vec<usize> = Vec::new();
    for (pos, &r) in response_order.iter().enumerate() {
        for (i, p) in resolved.predictors.iter().enumerate() {
            if p.resp == r {
                shapes[i].resp = pos;
                predictor_order.push(i);
            }
        }
    }
    let position = |i: usize| predictor_order.iter().position(|&p| p == i).unwrap_or(i);

    // The blocks, each with its parts in predictor order, then sorted by
    // signature — a stable sort, so ties keep the order written.
    let mut blocks: Vec<(BlockShape, usize, Vec<usize>)> = Vec::new();
    for (i, b) in resolved.blocks.iter().enumerate() {
        let mut parts: Vec<(usize, usize)> = b
            .parts
            .iter()
            .map(|part| (position(part.predictor), part.predictor))
            .collect();
        parts.sort_by_key(|(at, _)| *at);
        blocks.push((
            BlockShape {
                correlated: b.correlated,
                parts: parts.iter().map(|(at, _)| *at).collect(),
                by: b.by.is_some(),
            },
            i,
            parts.iter().map(|(_, p)| *p).collect(),
        ));
    }
    blocks.sort_by(|a, b| a.0.cmp(&b.0));

    // The non-linear expression, renamed.
    let mut covariates = Vec::new();
    let nl = match &resolved.responses[0].nl {
        Some(expr) => {
            let (params, covs) = expr.names();
            covariates.clone_from(&covs);
            Some(NlShape {
                link: resolved
                    .links
                    .get("mu")
                    .map_or("identity", |l| l.name())
                    .to_owned(),
                expr: nl_stan(expr, &params, &covs).0,
                covariates: covs.len(),
            })
        }
        None => None,
    };
    if options.epred && (mv || family.kind == Kind::Categorical) {
        return Err(
            "the expected response (`epred`) is computed for one response of a number, and \
             this model is multivariate or categorical"
                .into(),
        );
    }

    let mut ordered_subs: Vec<SubShape> = response_order.iter().map(|&r| subs[r].clone()).collect();
    let first = ordered_subs.remove(0);
    let mut ordered_shapes: Vec<PredictorShape> =
        predictor_order.iter().map(|&i| shapes[i].clone()).collect();
    for s in &mut ordered_shapes {
        // A positional name, never the user's.
        if s.dpar.starts_with("nlp_") {
            continue;
        }
    }
    Ok(Canonical {
        structure: Structure {
            generator: GENERATOR_VERSION,
            family: family.name.to_owned(),
            predictors: std::mem::take(&mut ordered_shapes),
            scalars: first.scalars,
            blocks: blocks.iter().map(|(s, _, _)| s.clone()).collect(),
            response: first.response,
            more: ordered_subs,
            rescor: resolved.rescor,
            nl,
            epred: options.epred,
        },
        block_order: blocks.iter().map(|(_, i, _)| *i).collect(),
        block_parts: blocks.into_iter().map(|(_, _, d)| d).collect(),
        predictor_order,
        response_order,
        covariates,
    })
}

/// Response `r`'s addition terms as their shape.
fn response_shape(resolved: &Resolved, r: usize) -> Result<ResponseShape, String> {
    let family = resolved.family;
    let sub = &resolved.responses[r];
    let mut response = ResponseShape::default();
    let arg = |b: &Bound| match b {
        Bound::Column(_) => ArgShape::Column,
        Bound::Number(_) => ArgShape::Constant,
    };
    for a in &sub.aterms {
        match a {
            ATerm::Weights(b) => response.weights = Some(arg(b)),
            ATerm::Trials(b) => response.trials = Some(arg(b)),
            ATerm::Cens(c) => response.cens = Some(c.upper.is_some()),
            ATerm::Trunc { lb, ub } => {
                response.trunc_lb = lb.as_ref().map(arg);
                response.trunc_ub = ub.as_ref().map(arg);
            }
            ATerm::Se { sigma, .. } => response.se = Some(*sigma),
            ATerm::Rate(b) => response.rate = Some(arg(b)),
            ATerm::Subset(_) => response.subset = true,
            ATerm::Thres(_) => {}
        }
    }
    if response.subset && sub.aterms.len() > 1 {
        return Err(format!(
            "`{}` is measured on a subset of the rows, and this release takes no other addition \
             term with `subset()`",
            sub.response
        ));
    }
    if family.response.integer() {
        for (what, b) in sub.aterms.iter().flat_map(|a| match a {
            ATerm::Trunc { lb, ub } => vec![("lb", lb.as_ref()), ("ub", ub.as_ref())],
            ATerm::Trials(b) => vec![("trials", Some(b))],
            _ => Vec::new(),
        }) {
            if let Some(Bound::Number(x)) = b {
                if x.fract() != 0.0 {
                    return Err(format!(
                        "the family `{}` models whole numbers, so `{what}` must be a whole \
                         number (it is {})",
                        family.name,
                        sc_model::terms::number_text(*x)
                    ));
                }
            }
        }
    }
    Ok(response)
}

/// A non-linear expression as vectorised Stan over `nlp_i` and `C_j`, and
/// whether it is a vector (it reads a parameter or a column) or a number.
pub fn nl_stan(e: &NlExpr, params: &[String], covs: &[String]) -> (String, bool) {
    match e {
        NlExpr::Number(x) => {
            let text = stan_number(*x);
            if *x < 0.0 {
                (format!("({text})"), false)
            } else {
                (text, false)
            }
        }
        NlExpr::Param(n) => {
            let i = params.iter().position(|p| p == n).unwrap_or(0);
            (format!("nlp_{}", i + 1), true)
        }
        NlExpr::Covariate(n) => {
            let j = covs.iter().position(|c| c == n).unwrap_or(0);
            (format!("C_{}", j + 1), true)
        }
        NlExpr::Neg(x) => {
            let (t, v) = nl_stan(x, params, covs);
            (format!("(-{t})"), v)
        }
        NlExpr::Binary { op, lhs, rhs } => {
            let (l, lv) = nl_stan(lhs, params, covs);
            let (r, rv) = nl_stan(rhs, params, covs);
            let symbol = match (op, lv, rv) {
                ('+', ..) => "+",
                ('-', ..) => "-",
                ('*', true, true) => ".*",
                ('*', ..) => "*",
                ('/', true, true) | ('/', false, true) => "./",
                ('/', ..) => "/",
                ('^', false, false) => "^",
                ('^', ..) => ".^",
                _ => "+",
            };
            (format!("({l} {symbol} {r})"), lv || rv)
        }
        NlExpr::Call { function, args } => {
            let parts: Vec<(String, bool)> =
                args.iter().map(|a| nl_stan(a, params, covs)).collect();
            let vector = parts.iter().any(|(_, v)| *v);
            let text = parts
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            (format!("{function}({text})"), vector)
        }
    }
}

/// A number as a Stan literal, one form for each value: whole numbers with
/// `.0`, others in Rust's shortest round-trip form.
fn stan_number(x: f64) -> String {
    let a = x.abs();
    if a.fract() == 0.0 && a < 1e15 {
        format!("{}{a:.1}", if x < 0.0 { "-" } else { "" })
    } else {
        format!("{x:?}")
    }
}

/// How every data variable of the program is bound: the host's binding keys.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DataPlan {
    /// Each data variable's binding (prior variables excepted: they are
    /// [`prior_data`](crate::provider) values).
    pub bindings: BTreeMap<String, Binding>,
    /// The grouping factors' dimensions.
    pub dimensions: BTreeMap<String, DimensionSpec>,
    /// The main dataset's policies: a null in any column the formula reads
    /// drops the row (§2.9).
    pub policies: BTreeMap<String, Policies>,
    /// For each canonical block, the dimension its levels are.
    pub block_dimensions: Vec<String>,
    /// For each canonical block with `gr(by = )`, the dimension of `by`.
    pub by_dimensions: BTreeMap<usize, String>,
    /// The columns the binder adds: a combination of grouping columns, a
    /// response's values on its subset.
    pub derived: BTreeMap<String, DerivedColumn>,
    /// Per canonical predictor position with `mo()`, each term's dimension.
    pub mo_dimensions: BTreeMap<usize, Vec<String>>,
    /// The dimension of a categorical response's categories.
    pub category_dimension: Option<String>,
}

impl DataPlan {
    /// The binding keys as configuration JSON: the bindings, the dimensions,
    /// the policies and the derived columns.
    pub fn to_json(&self) -> (Json, Json, Json, Json) {
        fn map<T: Serialize>(m: &BTreeMap<String, T>) -> Json {
            Json::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(Json::Null)))
                    .collect(),
            )
        }
        (
            map(&self.bindings),
            map(&self.dimensions),
            map(&self.policies),
            map(&self.derived),
        )
    }
}

/// What the data plan needs to know of the model beyond the formula.
#[derive(Debug, Clone, Default)]
pub struct PlanContext {
    /// The names a dimension may not have (the related datasets').
    pub taken: Vec<String>,
    /// Grouping columns that are foreign keys to a table the model has a
    /// related dataset over, with that dataset's name: their levels are its
    /// rows (§2.8), so a level with no rows still gets a parameter.
    pub rows_dimensions: BTreeMap<String, String>,
}

impl PlanContext {
    /// The context of `model`: its related datasets' names, and each foreign
    /// key of the main dataset to a table one of them is over.
    pub fn of(model: &sc_model::Model) -> PlanContext {
        let mut out = PlanContext {
            taken: model.related.iter().map(|r| r.name.clone()).collect(),
            ..PlanContext::default()
        };
        for c in &model.dataset.columns {
            let Some(key) = &c.key else { continue };
            if let Some(r) = model.related.iter().find(|r| r.dataset.table == key.table) {
                out.rows_dimensions.insert(c.name.clone(), r.name.clone());
            }
        }
        out
    }
}

fn column(c: &str) -> Binding {
    Binding::Column {
        dataset: MAIN_DATASET.to_owned(),
        column: c.to_owned(),
        time: None,
    }
}

fn number(x: f64, integer: bool) -> Binding {
    // A whole number for an `int` declaration (canonicalisation has refused
    // a fraction there); the cast saturates, and Stan refuses a count that
    // large by its declared bounds.
    #[allow(clippy::cast_possible_truncation)]
    let value = if integer { json!(x as i64) } else { json!(x) };
    Binding::Value { value }
}

fn bound(b: &Bound, integer: bool) -> Binding {
    match b {
        Bound::Column(c) => column(c),
        Bound::Number(x) => number(*x, integer),
    }
}

fn terms(spec: &sc_model::terms::TermsSpec) -> Binding {
    Binding::Terms {
        dataset: MAIN_DATASET.to_owned(),
        terms: spec.terms.clone(),
        intercept: spec.intercept,
        intercept_column: spec.intercept_column,
    }
}

fn index(column: &str, dimension: &str) -> Binding {
    Binding::Index {
        dataset: MAIN_DATASET.to_owned(),
        column: column.to_owned(),
        dimension: dimension.to_owned(),
        match_column: None,
    }
}

/// The name of a derived column holding response `y`'s values on its subset.
pub fn subset_column(y: &str, s: &str) -> String {
    format!("{y} | subset({s})")
}

/// The data plan of `canonical` over `resolved`. `taken` are the names a
/// dimension may not have (the related datasets').
pub fn data_plan(resolved: &Resolved, canonical: &Canonical, taken: &[String]) -> DataPlan {
    data_plan_in(
        resolved,
        canonical,
        &PlanContext {
            taken: taken.to_vec(),
            ..PlanContext::default()
        },
    )
}

/// [`data_plan`] with the model's [`PlanContext`].
#[allow(clippy::too_many_lines)] // every data variable, in program order
pub fn data_plan_in(resolved: &Resolved, canonical: &Canonical, ctx: &PlanContext) -> DataPlan {
    let mut plan = DataPlan::default();
    let mut used: Vec<String> = ctx.taken.clone();
    used.push(MAIN_DATASET.to_owned());
    // A values dimension over `col`, shared, named after the column where
    // that is an identifier and free.
    let mut values_dim = |plan: &mut DataPlan, col: &str, fallback: String| -> String {
        if let Some((name, _)) = plan
            .dimensions
            .iter()
            .find(|(_, d)| matches!(d, DimensionSpec::Values { column, .. } if column == col))
        {
            return name.clone();
        }
        let mut name = col.replace(':', "_");
        if used.contains(&name) || !is_identifier(&name) {
            name = fallback;
        }
        used.push(name.clone());
        plan.dimensions.insert(
            name.clone(),
            DimensionSpec::Values {
                dataset: MAIN_DATASET.to_owned(),
                column: col.to_owned(),
            },
        );
        name
    };

    plan.bindings.insert(
        "N".into(),
        Binding::Count {
            dataset: MAIN_DATASET.to_owned(),
        },
    );
    let kind = resolved.family.kind;
    let integer = resolved.family.response.integer();
    for (pos, &r) in canonical.response_order.iter().enumerate() {
        let sub = &resolved.responses[r];
        let suf = suffix(pos);
        if let Some(s) = sub.subset() {
            let masked = subset_column(&sub.response, s);
            plan.derived.insert(
                masked.clone(),
                DerivedColumn::Mask {
                    dataset: MAIN_DATASET.to_owned(),
                    column: sub.response.clone(),
                    where_column: s.to_owned(),
                },
            );
            let b = &mut plan.bindings;
            b.insert(
                format!("N_sub{suf}"),
                Binding::CountPresent {
                    dataset: MAIN_DATASET.to_owned(),
                    column: masked.clone(),
                },
            );
            b.insert(
                format!("J_sub{suf}"),
                Binding::Present {
                    dataset: MAIN_DATASET.to_owned(),
                    column: masked.clone(),
                },
            );
            b.insert(
                format!("Y{suf}"),
                Binding::PresentValues {
                    dataset: MAIN_DATASET.to_owned(),
                    column: masked,
                },
            );
            continue;
        }
        match kind {
            Kind::Categorical => {
                let dim = values_dim(&mut plan, &sub.response, "category".to_owned());
                plan.bindings
                    .insert(format!("Y{suf}"), index(&sub.response, &dim));
                plan.bindings.insert(
                    format!("ncat{suf}"),
                    Binding::Size {
                        dimension: dim.clone(),
                    },
                );
                plan.category_dimension = Some(dim);
            }
            Kind::Ordinal => {
                plan.bindings
                    .insert(format!("Y{suf}"), column(&sub.response));
                // The number of thresholds: `thres(k)`, or from the data
                // (filled in when the rows are read).
                plan.bindings.insert(
                    format!("nthres{suf}"),
                    Binding::Value {
                        value: json!(sub.thres().unwrap_or(1)),
                    },
                );
            }
            Kind::Standard => {
                plan.bindings
                    .insert(format!("Y{suf}"), column(&sub.response));
            }
        }
        let b = &mut plan.bindings;
        for a in &sub.aterms {
            match a {
                ATerm::Weights(w) => {
                    b.insert(format!("weights{suf}"), bound(w, false));
                }
                ATerm::Trials(t) => {
                    b.insert(format!("trials{suf}"), bound(t, true));
                }
                ATerm::Cens(c) => {
                    b.insert(format!("cens{suf}"), column(&c.column));
                    if let Some(upper) = &c.upper {
                        b.insert(format!("Y2{suf}"), column(upper));
                    }
                }
                ATerm::Trunc { lb, ub } => {
                    if let Some(lb) = lb {
                        b.insert(format!("lb_Y{suf}"), bound(lb, integer));
                    }
                    if let Some(ub) = ub {
                        b.insert(format!("ub_Y{suf}"), bound(ub, integer));
                    }
                }
                ATerm::Se { column: c, .. } => {
                    b.insert(format!("se{suf}"), column(c));
                }
                ATerm::Rate(r) => {
                    b.insert(format!("rate{suf}"), bound(r, false));
                }
                ATerm::Thres(_) | ATerm::Subset(_) => {}
            }
        }
    }

    for (n0, &i) in canonical.predictor_order.iter().enumerate() {
        let n = n0 + 1;
        let p = &resolved.predictors[i];
        let shape = &canonical.structure.predictors[n0];
        if shape.b != CoefShape::None {
            // The intercept is a parameter of its own unless it was written
            // `0 + Intercept` (or is a non-linear parameter's), when it is the
            // matrix's first column.
            let mut spec = p.terms.clone();
            spec.intercept_column = p.intercept == Intercept::Coefficient;
            plan.bindings.insert(format!("X_{n}"), terms(&spec));
            plan.bindings.insert(
                format!("K_{n}"),
                Binding::Width {
                    of: format!("X_{n}"),
                },
            );
        }
        if shape.offset {
            let spec = sc_model::terms::TermsSpec {
                terms: p
                    .offsets
                    .iter()
                    .map(|o| vec![o.clone()])
                    .collect::<Vec<Vec<VarExpr>>>(),
                intercept: false,
                intercept_column: false,
            };
            plan.bindings.insert(format!("O_{n}"), terms(&spec));
            plan.bindings.insert(
                format!("K_O_{n}"),
                Binding::Width {
                    of: format!("O_{n}"),
                },
            );
        }
        if let Some(cs) = &p.cs {
            plan.bindings.insert(format!("Xcs_{n}"), terms(cs));
            plan.bindings.insert(
                format!("Kcs_{n}"),
                Binding::Width {
                    of: format!("Xcs_{n}"),
                },
            );
        }
        let mut mo_dims = Vec::new();
        for (j, m) in p.mo.iter().enumerate() {
            let j = j + 1;
            let dim = values_dim(&mut plan, m, format!("mo_{n}_{j}"));
            plan.bindings.insert(
                format!("Lmo_{n}_{j}"),
                Binding::Size {
                    dimension: dim.clone(),
                },
            );
            plan.bindings.insert(format!("Xmo_{n}_{j}"), index(m, &dim));
            mo_dims.push(dim);
        }
        if !mo_dims.is_empty() {
            plan.mo_dimensions.insert(n0, mo_dims);
        }
    }
    for (j, c) in canonical.covariates.iter().enumerate() {
        plan.bindings.insert(format!("C_{}", j + 1), column(c));
    }

    for (k, &at) in canonical.block_order.iter().enumerate() {
        let n = k + 1;
        let block = &resolved.blocks[at];
        // The levels: a foreign key's referenced rows, a combination's
        // distinct values, or a column's.
        let dim = if block.factors.len() > 1 {
            let derived = block.factors.join(":");
            plan.derived.insert(
                derived.clone(),
                DerivedColumn::Combine {
                    dataset: MAIN_DATASET.to_owned(),
                    columns: block.factors.clone(),
                },
            );
            let dim = values_dim(&mut plan, &derived, format!("group_{n}"));
            plan.bindings
                .insert(format!("J_{n}"), index(&derived, &dim));
            dim
        } else {
            let factor = block.factors.first().cloned().unwrap_or_default();
            let dim = match ctx.rows_dimensions.get(&factor) {
                Some(related) => related.clone(),
                None => values_dim(&mut plan, &factor, format!("group_{n}")),
            };
            plan.bindings.insert(format!("J_{n}"), index(&factor, &dim));
            dim
        };
        plan.bindings.insert(
            format!("N_{n}"),
            Binding::Size {
                dimension: dim.clone(),
            },
        );
        if let Some(by) = &block.by {
            let by_dim = values_dim(&mut plan, by, format!("by_{n}"));
            plan.bindings.insert(
                format!("Nby_{n}"),
                Binding::Size {
                    dimension: by_dim.clone(),
                },
            );
            plan.bindings.insert(format!("Jby_{n}"), index(by, &by_dim));
            plan.by_dimensions.insert(k, by_dim);
        }
        for (j, &pi) in canonical.block_parts[k].iter().enumerate() {
            let part = block.parts.iter().find(|p| p.predictor == pi);
            if let Some(part) = part {
                plan.bindings
                    .insert(format!("Z_{n}_{}", j + 1), terms(&part.terms));
                plan.bindings.insert(
                    format!("M_{n}_{}", j + 1),
                    Binding::Width {
                        of: format!("Z_{n}_{}", j + 1),
                    },
                );
            }
        }
        plan.block_dimensions.push(dim);
    }
    plan.policies.insert(
        MAIN_DATASET.to_owned(),
        Policies {
            nulls: Policy::Drop,
            unknown: Policy::Refuse,
        },
    );
    plan
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_has_one_literal_form() {
        assert_eq!(stan_number(1.0), "1.0");
        assert_eq!(stan_number(-2.0), "-2.0");
        assert_eq!(stan_number(0.5), "0.5");
        assert_eq!(stan_number(1e-3), "0.001");
    }

    #[test]
    fn a_non_linear_expression_is_renamed_and_vectorised() {
        let e = crate::parse::parse_formula("y ~ a * (1 - exp(-b * t))\na ~ 1\nb ~ 1")
            .unwrap()
            .lines[0]
            .nl
            .clone()
            .unwrap();
        let (params, covs) = e.names();
        assert_eq!(params, ["a", "b"]);
        assert_eq!(covs, ["t"]);
        assert_eq!(
            nl_stan(&e, &params, &covs).0,
            "(nlp_1 .* (1.0 - exp(((-nlp_2) .* C_1))))"
        );
    }
}
