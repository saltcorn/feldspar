//! Canonicalisation (TODO-bayesian-regression §5, R1.5): a [`Resolved`]
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
//!    levels and of rows are data: `y ~ x` and `y ~ a + b + c` are one
//!    program, and so are `(1 | g)` and `(1 + x | g)` — a correlated block's
//!    Cholesky factor is `cholesky_factor_corr[M]` whatever `M` is.
//! 3. **Order out.** Linear predictors are in the fixed order `mu`, then the
//!    family's own; blocks are sorted by their structural signature, ties
//!    kept in the order written (a tie is structurally identical, so its order
//!    cannot change the text).
//! 4. **Versioned.** [`GENERATOR_VERSION`] is in the structure, so a change to
//!    what the generator emits for a structure changes every key.
//!
//! What *is* structural: the family and every link; per linear predictor
//! whether it has an intercept (centred or not), coefficients (bounded or not)
//! and an offset; per block whether it is correlated and which predictors it
//! feeds; the response's addition terms, and whether each argument is a column
//! or one number; and the two structural priors (§2.5), a bound on `b` and
//! `constant()` on a distributional parameter.

use std::collections::BTreeMap;

use sc_model::terms::VarExpr;
use sc_model::{Binding, DimensionSpec, MAIN_DATASET, Policies, Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::parse::{ATerm, Bound, Intercept};
use crate::prior::{ClassKind, Prior, PriorFamily, PriorRow};
use crate::resolve::Resolved;

/// The generator's version: bumped by any change to the text it emits for any
/// structure (§5 rule 5), which the golden files catch.
pub const GENERATOR_VERSION: u32 = 1;

/// Everything that changes the program's text, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Structure {
    /// [`GENERATOR_VERSION`].
    pub generator: u32,
    /// The family, as brms spells it.
    pub family: String,
    /// The linear predictors: `mu` first, then the family's order.
    pub predictors: Vec<PredictorShape>,
    /// The distributional parameters without a formula, in the family's order.
    pub scalars: Vec<ScalarShape>,
    /// The group-level blocks, sorted (rule 3).
    pub blocks: Vec<BlockShape>,
    /// The response and its addition terms.
    pub response: ResponseShape,
}

/// One linear predictor's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictorShape {
    /// `mu`, `sigma`, ….
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
    /// `real`, `positive` or `above_one`.
    pub support: String,
    /// Fixed by `constant()`: data rather than a parameter.
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

    /// Whether the response is whole numbers (`array[N] int`).
    pub fn integer_response(&self) -> bool {
        crate::family::family(&self.family).is_ok_and(|f| f.response.integer())
    }

    /// The position (from 1) of the linear predictor of `dpar`.
    pub fn predictor_position(&self, dpar: &str) -> Option<usize> {
        self.predictors
            .iter()
            .position(|p| p.dpar == dpar)
            .map(|i| i + 1)
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
    /// For each canonical block, its parts' dpars in canonical order.
    pub block_parts: Vec<Vec<&'static str>>,
}

/// Canonicalise `resolved`, with the admin's explicit prior `rows` (whose
/// bounds and `constant()`s are structural). A sentence when the formula uses
/// something this release cannot generate.
pub fn canonicalise(resolved: &Resolved, rows: &[PriorRow]) -> Result<Canonical, String> {
    let family = resolved.family;
    let row_prior = |r: &PriorRow| Prior::parse(&r.prior).ok();
    let constant_rows: Vec<&PriorRow> = rows
        .iter()
        .filter(|r| row_prior(r).is_some_and(|p| p.family == PriorFamily::Constant))
        .collect();
    for r in &constant_rows {
        let scalar = resolved.scalars.contains(&r.class.as_str()) && r.coef.is_empty();
        if !scalar {
            return Err(format!(
                "{}: `constant()` can fix a distributional parameter that has no formula \
                 (`sigma`, `nu`, …); fixing `{}` arrives in a later release (R2)",
                r.describe(),
                r.class
            ));
        }
    }

    let mut predictors = Vec::new();
    for p in &resolved.predictors {
        let has_b = !p.terms.terms.is_empty() || p.intercept == Intercept::Coefficient;
        let dpar_key = if p.dpar == "mu" { "" } else { p.dpar };
        let bounded = rows
            .iter()
            .any(|r| r.class == "b" && r.dpar == dpar_key && (r.lb.is_some() || r.ub.is_some()));
        let b = match (has_b, bounded) {
            (false, _) => CoefShape::None,
            (true, false) => CoefShape::Free,
            (true, true) => CoefShape::Bounded,
        };
        let intercept = match p.intercept {
            Intercept::Off | Intercept::Coefficient => InterceptShape::None,
            Intercept::On if resolved.center && has_b => InterceptShape::Centred,
            Intercept::On => InterceptShape::Plain,
        };
        predictors.push(PredictorShape {
            dpar: p.dpar.to_owned(),
            link: p.link.name().to_owned(),
            intercept,
            b,
            offset: !p.offsets.is_empty(),
        });
    }

    let scalars = resolved
        .scalars
        .iter()
        .filter_map(|name| family.dpar(name))
        .map(|d| ScalarShape {
            dpar: d.name.to_owned(),
            support: match d.kind {
                ClassKind::Positive => "positive",
                ClassKind::AboveOne => "above_one",
                ClassKind::Unit => "unit",
                ClassKind::Real | ClassKind::Correlation => "real",
            }
            .to_owned(),
            fixed: constant_rows.iter().any(|r| r.class == d.name),
        })
        .collect();

    // The blocks, each with its parts in predictor order, then sorted by
    // signature — a stable sort, so ties keep the order written.
    let mut blocks: Vec<(BlockShape, usize, Vec<&'static str>)> = Vec::new();
    for (i, b) in resolved.blocks.iter().enumerate() {
        if b.factors.len() > 1 {
            return Err(format!(
                "grouping by a combination of columns (`{}`) arrives in a later release (R2); \
                 for now, add a Calculated column to the dataset holding the combination and \
                 group by it",
                b.group
            ));
        }
        let mut parts: Vec<(usize, &'static str)> = b
            .parts
            .iter()
            .filter_map(|part| {
                resolved
                    .predictors
                    .iter()
                    .position(|p| p.dpar == part.dpar)
                    .map(|at| (at, part.dpar))
            })
            .collect();
        parts.sort_by_key(|(at, _)| *at);
        blocks.push((
            BlockShape {
                correlated: b.correlated,
                parts: parts.iter().map(|(at, _)| *at).collect(),
            },
            i,
            parts.iter().map(|(_, d)| *d).collect(),
        ));
    }
    blocks.sort_by(|a, b| a.0.cmp(&b.0));

    let mut response = ResponseShape::default();
    let arg = |b: &Bound| match b {
        Bound::Column(_) => ArgShape::Column,
        Bound::Number(_) => ArgShape::Constant,
    };
    for a in &resolved.aterms {
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
        }
    }
    if family.response.integer() {
        for (what, b) in resolved.aterms.iter().flat_map(|a| match a {
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

    Ok(Canonical {
        structure: Structure {
            generator: GENERATOR_VERSION,
            family: family.name.to_owned(),
            predictors,
            scalars,
            blocks: blocks.iter().map(|(s, _, _)| s.clone()).collect(),
            response,
        },
        block_order: blocks.iter().map(|(_, i, _)| *i).collect(),
        block_parts: blocks.into_iter().map(|(_, _, d)| d).collect(),
    })
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
}

impl DataPlan {
    /// The binding keys as configuration JSON.
    pub fn to_json(&self) -> (Json, Json, Json) {
        let bindings = self
            .bindings
            .iter()
            .map(|(k, b)| (k.clone(), serde_json::to_value(b).unwrap_or(Json::Null)))
            .collect::<serde_json::Map<_, _>>();
        let dimensions = self
            .dimensions
            .iter()
            .map(|(k, d)| (k.clone(), serde_json::to_value(d).unwrap_or(Json::Null)))
            .collect::<serde_json::Map<_, _>>();
        let policies = self
            .policies
            .iter()
            .map(|(k, p)| (k.clone(), serde_json::to_value(p).unwrap_or(Json::Null)))
            .collect::<serde_json::Map<_, _>>();
        (
            Json::Object(bindings),
            Json::Object(dimensions),
            Json::Object(policies),
        )
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

/// The data plan of `canonical` over `resolved`. `taken` are the names a
/// dimension may not have (the related datasets').
pub fn data_plan(resolved: &Resolved, canonical: &Canonical, taken: &[String]) -> DataPlan {
    let mut plan = DataPlan::default();
    let b = &mut plan.bindings;
    b.insert(
        "N".into(),
        Binding::Count {
            dataset: MAIN_DATASET.to_owned(),
        },
    );
    b.insert("Y".into(), column(&resolved.response));
    let integer = resolved.family.response.integer();
    for a in &resolved.aterms {
        match a {
            ATerm::Weights(w) => {
                b.insert("weights".into(), bound(w, false));
            }
            ATerm::Trials(t) => {
                b.insert("trials".into(), bound(t, true));
            }
            ATerm::Cens(c) => {
                b.insert("cens".into(), column(&c.column));
                if let Some(upper) = &c.upper {
                    b.insert("Y2".into(), column(upper));
                }
            }
            ATerm::Trunc { lb, ub } => {
                if let Some(lb) = lb {
                    b.insert("lb_Y".into(), bound(lb, integer));
                }
                if let Some(ub) = ub {
                    b.insert("ub_Y".into(), bound(ub, integer));
                }
            }
            ATerm::Se { column: c, .. } => {
                b.insert("se".into(), column(c));
            }
            ATerm::Rate(r) => {
                b.insert("rate".into(), bound(r, false));
            }
        }
    }

    for (i, p) in resolved.predictors.iter().enumerate() {
        let n = i + 1;
        let shape = &canonical.structure.predictors[i];
        if shape.b != CoefShape::None {
            // The intercept is a parameter of its own unless it was written
            // `0 + Intercept`, when it is the matrix's first column.
            let mut spec = p.terms.clone();
            spec.intercept_column = p.intercept == Intercept::Coefficient;
            b.insert(format!("X_{n}"), terms(&spec));
            b.insert(
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
            b.insert(format!("O_{n}"), terms(&spec));
            b.insert(
                format!("K_O_{n}"),
                Binding::Width {
                    of: format!("O_{n}"),
                },
            );
        }
    }

    let mut used: Vec<String> = taken.to_vec();
    used.push(MAIN_DATASET.to_owned());
    for (k, &at) in canonical.block_order.iter().enumerate() {
        let n = k + 1;
        let block = &resolved.blocks[at];
        let factor = block.factors.first().cloned().unwrap_or_default();
        // The dimension is named after the grouping column, so a summary's
        // heading reads `county`; one dimension per column, shared.
        let dim =
            match plan.dimensions.iter().find(
                |(_, d)| matches!(d, DimensionSpec::Values { column, .. } if *column == factor),
            ) {
                Some((name, _)) => name.clone(),
                None => {
                    let mut name = factor.clone();
                    if used.contains(&name) || !is_identifier(&name) {
                        name = format!("group_{n}");
                    }
                    used.push(name.clone());
                    plan.dimensions.insert(
                        name.clone(),
                        DimensionSpec::Values {
                            dataset: MAIN_DATASET.to_owned(),
                            column: factor.clone(),
                        },
                    );
                    name
                }
            };
        b.insert(
            format!("N_{n}"),
            Binding::Size {
                dimension: dim.clone(),
            },
        );
        b.insert(
            format!("J_{n}"),
            Binding::Index {
                dataset: MAIN_DATASET.to_owned(),
                column: factor,
                dimension: dim.clone(),
                match_column: None,
            },
        );
        for (j, dpar) in canonical.block_parts[k].iter().enumerate() {
            let part = block.parts.iter().find(|p| p.dpar == *dpar);
            if let Some(part) = part {
                b.insert(format!("Z_{n}_{}", j + 1), terms(&part.terms));
                b.insert(
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
