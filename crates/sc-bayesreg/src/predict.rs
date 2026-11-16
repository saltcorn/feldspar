//! Prediction in Rust from the stored draws (TODO-bayesian-regression §8,
//! R2.4): the posterior mean of E[y | row] (`posterior_epred`) for new rows,
//! without CmdStan.
//!
//! A fit keeps, in its state, a **prediction pack**: the lowered
//! configuration (whose bindings say how every data variable is computed from
//! a row), the binder's coordinates (each model matrix's fitted encoding, each
//! dimension's levels), and the draws of what the linear predictors read,
//! thinned to at most [`MAX_DRAWS`]. A prediction:
//!
//! 1. computes each data variable for the new rows from its binding, with the
//!    fit's own encodings — an unknown category or group level, or a null, is
//!    an error naming the column;
//! 2. for each draw, computes every linear predictor η = Intercept + X b +
//!    monotonic effects + Σ r[J]·Z (and the non-linear expression), then its
//!    inverse link;
//! 3. answers the mean over the draws of the family's E[y] — the same
//!    expressions the generated program's `epred_1` computes (`generate.rs`).
//!
//! A multivariate or categorical model does not predict a number per row, and
//! is refused by name.

use std::collections::{BTreeMap, HashMap};

use sc_error::{Error, Result};
use sc_model::terms::{TermsEncoding, apply_terms};
use sc_model::{
    Binding, Column, Coordinates, DERIVED_COLUMNS_KEY, DerivedColumn, DrawSeries, Frame,
};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use crate::canon::{InterceptShape, Structure};
use crate::family::Kind;
use crate::lower::{build, keys};
use crate::parse::NlExpr;
use crate::resolve::Data;

/// The most draws a prediction reads (§8): a prediction's cost is rows ×
/// draws.
pub const MAX_DRAWS: usize = 1000;

/// The most numbers a pack's draws hold: a model with many group levels keeps
/// fewer draws, so the state stays a manageable size.
pub const MAX_PACK_VALUES: usize = 250_000;

/// The key of the pack in a fit's state.
pub const PACK_KEY: &str = "prediction";

/// Why a fit has no pack, when it has none: kept in the state so the refusal
/// can say so.
pub const NO_PACK_KEY: &str = "prediction_unavailable";

/// Whether a model of `structure` predicts a number per row.
pub fn predicts(structure: &Structure) -> Result<(), String> {
    if !structure.more.is_empty() {
        return Err(
            "a multivariate model predicts several responses, and a prediction here is one \
             number per row; read its draws in a code body instead"
                .to_owned(),
        );
    }
    if structure.kind() == Kind::Categorical {
        return Err(
            "a categorical model predicts a probability per category, and a prediction here is \
             one number per row; read its draws in a code body instead"
                .to_owned(),
        );
    }
    Ok(())
}

/// The variables whose draws a prediction of `structure` reads.
fn needed(structure: &Structure) -> Vec<String> {
    let mut out = Vec::new();
    for (i, p) in structure.predictors.iter().enumerate() {
        let n = i + 1;
        out.push(format!("b_{n}"));
        out.push(format!("Intercept_{n}"));
        out.push(format!("bsp_{n}"));
        out.push(format!("bcs_{n}"));
        for j in 1..=p.mo {
            out.push(format!("simo_{n}_{j}"));
        }
    }
    for k in 1..=structure.blocks.len() {
        out.push(format!("r_{k}"));
    }
    for s in &structure.scalars {
        out.push(s.dpar.clone());
    }
    out
}

/// One element's draws, chain by chain.
type Chains<'a> = BTreeMap<u32, &'a [f64]>;

/// The pack for a fit: `None` (and why) when it cannot predict.
pub fn pack(
    config: &Attrs,
    structure: &Structure,
    coordinates: &Coordinates,
    draws: &[DrawSeries],
) -> std::result::Result<Json, String> {
    predicts(structure)?;
    if !sc_model::keeps_draws(config).unwrap_or(true) {
        return Err(
            "this fit keeps no draws (`keep_draws` is off), and a prediction is computed from \
             them; turn `keep_draws` on and fit again"
                .to_owned(),
        );
    }
    let excluded = sc_model::excluded_variables(config).unwrap_or_default();
    let wanted = needed(structure);
    // Each element's post-warmup draws, chains in order.
    let mut series: BTreeMap<(String, Vec<usize>), Chains<'_>> = BTreeMap::new();
    for s in draws.iter().filter(|s| !s.warmup) {
        if wanted.contains(&s.variable) {
            series
                .entry((s.variable.clone(), s.element.clone()))
                .or_default()
                .insert(s.chain, &s.draws);
        }
    }
    for v in &wanted {
        let present = series.keys().any(|(name, _)| name == v);
        let declared_somewhere = draws.iter().any(|s| &s.variable == v);
        if !present && (declared_somewhere || excluded.contains(v)) {
            return Err(format!(
                "this fit does not keep the draws of `{v}`, which a prediction reads; take it out \
                 of `exclude_variables` and fit again"
            ));
        }
    }
    let total = series
        .values()
        .next()
        .map_or(0, |c| c.values().map(|d| d.len()).sum::<usize>());
    let budget = (MAX_PACK_VALUES / series.len().max(1)).max(100);
    let keep = total.min(MAX_DRAWS).min(budget).max(1);
    let stride = total.div_ceil(keep).max(1);
    let mut packed = serde_json::Map::new();
    for ((v, element), chains) in &series {
        let all: Vec<f64> = chains.values().flat_map(|d| d.iter().copied()).collect();
        let thinned: Vec<Json> = all
            .iter()
            .step_by(stride)
            .map(|x| crate::lower::real_json(*x))
            .collect();
        let key = format!(
            "{v}[{}]",
            element
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        packed.insert(key, Json::Array(thinned));
    }
    let mut kept = config.clone();
    kept.remove(keys::GENERATED_PROGRAM);
    kept.remove(keys::RESOLVED_PRIORS);
    Ok(json!({
        "config": kept,
        "coordinates": coordinates,
        "draws": packed,
    }))
}

/// The thinned draws of a pack.
struct Draws {
    by: HashMap<String, Vec<f64>>,
    count: usize,
}

impl Draws {
    fn get(&self, var: &str, element: &[usize]) -> Option<&[f64]> {
        let key = format!(
            "{var}[{}]",
            element
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        self.by.get(&key).map(Vec::as_slice)
    }

    fn need(&self, var: &str, element: &[usize]) -> Result<&[f64]> {
        self.get(var, element).ok_or_else(|| {
            Error::msg(format!(
                "the fit's prediction draws have no `{var}{element:?}`"
            ))
        })
    }

    /// How many elements of a vector `var`'s first index were drawn.
    fn len(&self, var: &str) -> usize {
        let prefix = format!("{var}[");
        self.by
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter_map(|rest| rest.split([',', ']']).next()?.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
    }
}

/// The posterior mean of E[y] for each row of `frame`, from a fit's `state`.
#[allow(clippy::too_many_lines)] // the steps in order
#[allow(clippy::needless_range_loop)] // rows and columns of two matrices at once
pub fn predict(state: &Json, frame: &Frame) -> Result<Vec<f64>> {
    let Some(pack) = state.get(PACK_KEY) else {
        let why = state
            .get(NO_PACK_KEY)
            .and_then(Json::as_str)
            .unwrap_or("it was fitted before prediction from a formula existed; fit it again");
        return Err(Error::invalid(format!("this fit cannot predict: {why}")));
    };
    let config: Attrs =
        serde_json::from_value(pack.get("config").cloned().unwrap_or(Json::Null))
            .map_err(|e| Error::msg(format!("the prediction pack's configuration: {e}")))?;
    let coordinates: Coordinates =
        serde_json::from_value(pack.get("coordinates").cloned().unwrap_or(Json::Null))
            .map_err(|e| Error::msg(format!("the prediction pack's coordinates: {e}")))?;
    let structure: Structure =
        serde_json::from_value(config.get(keys::STRUCTURE).cloned().unwrap_or(Json::Null))
            .map_err(|e| Error::msg(format!("the prediction pack's structure: {e}")))?;
    predicts(&structure).map_err(Error::invalid)?;
    let mut by = HashMap::new();
    let mut count = 0;
    if let Some(Json::Object(m)) = pack.get("draws") {
        for (k, v) in m {
            let values: Vec<f64> = v
                .as_array()
                .map(|a| a.iter().map(crate::lower::json_real).collect())
                .unwrap_or_default();
            count = count.max(values.len());
            by.insert(k.clone(), values);
        }
    }
    let draws = Draws { by, count };
    let rows = Rows::new(&config, &coordinates, frame)?;
    let n_rows = frame.rows;

    // The non-linear expression, from the formula (re-parsed: it is the
    // configuration, and its canonical names are its order of appearance).
    let nl: Option<(NlExpr, Vec<String>, Vec<String>)> = if structure.nl.is_some() {
        let built = build(&config, Data::Nothing)?;
        built.resolved.responses[0].nl.clone().map(|e| {
            let (p, c) = e.names();
            (e, p, c)
        })
    } else {
        None
    };
    let covariates: Vec<Vec<f64>> = match &nl {
        Some((_, _, c)) => (1..=c.len())
            .map(|j| rows.numbers(&format!("C_{j}")))
            .collect::<Result<_>>()?,
        None => Vec::new(),
    };

    // Everything per row that does not depend on the draw.
    struct Pred {
        x: Option<sc_model::terms::ModelMatrix>,
        offset: Option<Vec<f64>>,
        mo: Vec<Vec<usize>>,
        cs: Option<sc_model::terms::ModelMatrix>,
        blocks: Vec<(usize, usize, Vec<usize>, sc_model::terms::ModelMatrix)>,
    }
    let mut preds = Vec::new();
    for (i, p) in structure.predictors.iter().enumerate() {
        let n = i + 1;
        let x = rows
            .has(&format!("X_{n}"))
            .then(|| rows.matrix(&format!("X_{n}")))
            .transpose()?;
        let offset = if p.offset {
            let o = rows.matrix(&format!("O_{n}"))?;
            let w = o.columns.len();
            Some(
                (0..n_rows)
                    .map(|r| o.values[r * w..(r + 1) * w].iter().sum())
                    .collect(),
            )
        } else {
            None
        };
        let mo = (1..=p.mo)
            .map(|j| rows.index(&format!("Xmo_{n}_{j}")))
            .collect::<Result<_>>()?;
        let cs = p.cs.then(|| rows.matrix(&format!("Xcs_{n}"))).transpose()?;
        let mut blocks = Vec::new();
        for (b, shape) in structure.blocks.iter().enumerate() {
            let k = b + 1;
            let Some(j) = shape.parts.iter().position(|&at| at == i) else {
                continue;
            };
            let mut start = 0;
            for q in 0..j {
                start += rows.matrix(&format!("Z_{k}_{}", q + 1))?.columns.len();
            }
            let z = rows.matrix(&format!("Z_{k}_{}", j + 1))?;
            blocks.push((k, start, rows.index(&format!("J_{k}"))?, z));
        }
        preds.push(Pred {
            x,
            offset,
            mo,
            cs,
            blocks,
        });
    }
    let fixed = |name: &str| rows.value(name);
    let trials = if structure.response.trials.is_some() {
        Some(rows.numbers("trials")?)
    } else {
        None
    };
    let rate = if structure.response.rate.is_some() {
        Some(rows.numbers("rate")?)
    } else {
        None
    };
    let family = structure.family.as_str();
    let nthres = draws.len(&format!(
        "Intercept_{}",
        structure.predictor_position("mu").unwrap_or(1)
    ));

    let mut sums = vec![0.0; n_rows];
    let count = draws.count.max(1);
    for d in 0..draws.count {
        // Each linear predictor's value per row, through its inverse link.
        let mut values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for (i, p) in structure.predictors.iter().enumerate() {
            let n = i + 1;
            let pr = &preds[i];
            let mut eta = vec![0.0; n_rows];
            if matches!(p.intercept, InterceptShape::Centred | InterceptShape::Plain) {
                let a = draws.need(&format!("Intercept_{n}"), &[])?[d];
                eta.iter_mut().for_each(|e| *e += a);
            }
            if let Some(x) = &pr.x {
                let w = x.columns.len();
                for c in 0..w {
                    let b = draws.need(&format!("b_{n}"), &[c + 1])?[d];
                    for r in 0..n_rows {
                        eta[r] += x.values[r * w + c] * b;
                    }
                }
            }
            if let Some(o) = &pr.offset {
                for r in 0..n_rows {
                    eta[r] += o[r];
                }
            }
            for (j, levels) in pr.mo.iter().enumerate() {
                let b = draws.need(&format!("bsp_{n}"), &[j + 1])?[d];
                let var = format!("simo_{n}_{}", j + 1);
                let steps = draws.len(&var);
                let mut cum = vec![0.0; steps + 1];
                for s in 1..=steps {
                    cum[s] = cum[s - 1] + draws.need(&var, &[s])?[d];
                }
                for r in 0..n_rows {
                    eta[r] += b * steps as f64 * cum[levels[r]];
                }
            }
            for (k, start, j, z) in &pr.blocks {
                let w = z.columns.len();
                for r in 0..n_rows {
                    for c in 0..w {
                        let effect = draws.need(&format!("r_{k}"), &[j[r] + 1, start + c + 1])?[d];
                        eta[r] += effect * z.values[r * w + c];
                    }
                }
            }
            let ordinal = structure.kind() == Kind::Ordinal && p.dpar == "mu";
            let link = if ordinal { "identity" } else { p.link.as_str() };
            values.insert(
                p.dpar.clone(),
                eta.into_iter().map(|e| inverse_link(link, e)).collect(),
            );
        }
        if let Some((expr, params, covs)) = &nl {
            let link = structure.mu_link().to_owned();
            let mu = (0..n_rows)
                .map(|r| {
                    let x = eval(
                        expr,
                        (params, covs),
                        &|i| values[&format!("nlp_{}", i + 1)][r],
                        &|j| covariates[j][r],
                    );
                    inverse_link(&link, x)
                })
                .collect();
            values.insert("mu".to_owned(), mu);
        }
        // A distributional parameter at row `r`: its formula's, a scalar's
        // draw, a fixed value.
        let dp = |name: &str, r: usize| -> Result<f64> {
            if let Some(v) = values.get(name) {
                return Ok(v[r]);
            }
            if let Some(s) = structure.scalars.iter().find(|s| s.dpar == name) {
                return if s.fixed {
                    fixed(name)
                } else {
                    Ok(draws.need(name, &[])?[d])
                };
            }
            if name == "disc" {
                return Ok(1.0);
            }
            Err(Error::msg(format!("a prediction has no `{name}`")))
        };
        for r in 0..n_rows {
            let mu = dp("mu", r)?;
            let e = match family {
                "lognormal" => (mu + dp("sigma", r)?.powi(2) / 2.0).exp(),
                "binomial" | "beta_binomial" => mu * trials.as_ref().map_or(1.0, |t| t[r]),
                "poisson" | "negbinomial" | "geometric" => mu * rate.as_ref().map_or(1.0, |t| t[r]),
                "zero_inflated_poisson" | "zero_inflated_negbinomial" | "zero_inflated_beta" => {
                    (1.0 - dp("zi", r)?) * mu
                }
                "hurdle_poisson" => (1.0 - dp("hu", r)?) * mu / (1.0 - (-mu).exp()),
                "hurdle_negbinomial" => {
                    let shape = dp("shape", r)?;
                    let p0 = (shape / (mu + shape)).powf(shape);
                    (1.0 - dp("hu", r)?) * mu / (1.0 - p0)
                }
                "hurdle_gamma" => (1.0 - dp("hu", r)?) * mu,
                "hurdle_lognormal" => {
                    (1.0 - dp("hu", r)?) * (mu + dp("sigma", r)?.powi(2) / 2.0).exp()
                }
                "zero_one_inflated_beta" => {
                    let zoi = dp("zoi", r)?;
                    zoi * dp("coi", r)? + (1.0 - zoi) * mu
                }
                "asym_laplace" => {
                    let q = dp("quantile", r)?;
                    mu + dp("sigma", r)? * (1.0 - 2.0 * q) / (q * (1.0 - q))
                }
                _ if structure.kind() == Kind::Ordinal => {
                    let n = structure.predictor_position("mu").unwrap_or(1);
                    let mut thres = Vec::with_capacity(nthres);
                    for t in 1..=nthres {
                        let mut v = draws.need(&format!("Intercept_{n}"), &[t])?[d];
                        if let Some(cs) = &preds[n - 1].cs {
                            let w = cs.columns.len();
                            for c in 0..w {
                                v -= cs.values[r * w + c]
                                    * draws.need(&format!("bcs_{n}"), &[c + 1, t])?[d];
                            }
                        }
                        thres.push(v);
                    }
                    let probs =
                        ordinal_probs(family, structure.mu_link(), mu, &thres, dp("disc", r)?);
                    probs
                        .iter()
                        .enumerate()
                        .map(|(k, p)| (k + 1) as f64 * p)
                        .sum()
                }
                _ => mu,
            };
            sums[r] += e;
        }
    }
    Ok(sums.into_iter().map(|s| s / count as f64).collect())
}

/// An ordinal family's category probabilities (as `ord_lprobs` in the
/// program).
fn ordinal_probs(family: &str, link: &str, eta: f64, thres: &[f64], disc: f64) -> Vec<f64> {
    let f = |x: f64| cdf(link, x);
    let k = thres.len() + 1;
    let mut p = vec![0.0; k];
    match family {
        "cumulative" => {
            let mut below = 0.0;
            for (j, t) in thres.iter().enumerate() {
                let c = f(disc * (t - eta));
                p[j] = c - below;
                below = c;
            }
            p[k - 1] = 1.0 - below;
        }
        "sratio" | "cratio" => {
            let mut passed = 1.0;
            for (j, t) in thres.iter().enumerate() {
                let (hit, pass) = if family == "sratio" {
                    let q = f(disc * (t - eta));
                    (q, 1.0 - q)
                } else {
                    let q = f(disc * (eta - t));
                    (1.0 - q, q)
                };
                p[j] = passed * hit;
                passed *= pass;
            }
            p[k - 1] = passed;
        }
        _ => {
            let up: Vec<f64> = thres.iter().map(|t| f(disc * (eta - t))).collect();
            for (c, slot) in p.iter_mut().enumerate() {
                *slot = up[..c].iter().product::<f64>()
                    * up[c..].iter().map(|u| 1.0 - u).product::<f64>();
            }
            let total: f64 = p.iter().sum();
            p.iter_mut().for_each(|x| *x /= total);
        }
    }
    p
}

/// The standard normal CDF, through `erfc` (W. J. Cody's rational
/// approximation as in Press et al., *Numerical Recipes*, §6.2: relative error
/// below 1.2 × 10⁻⁷).
fn phi(x: f64) -> f64 {
    let z = (x / std::f64::consts::SQRT_2).abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let erfc = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87
                                        + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 {
        1.0 - erfc / 2.0
    } else {
        erfc / 2.0
    }
}

fn inv_logit(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// A link's CDF, for the ordinal families.
fn cdf(link: &str, x: f64) -> f64 {
    match link {
        "probit" => phi(x),
        "probit_approx" => inv_logit(0.070_565_8 * x.powi(3) + 1.597_6 * x),
        "cloglog" => 1.0 - (-x.exp()).exp(),
        "cauchit" => x.atan() / std::f64::consts::PI + 0.5,
        _ => inv_logit(x),
    }
}

/// The inverse of `link` at `x` (as the program's).
fn inverse_link(link: &str, x: f64) -> f64 {
    match link {
        "log" => x.exp(),
        "logit" => inv_logit(x),
        "probit" => phi(x),
        "probit_approx" => inv_logit(0.070_565_8 * x.powi(3) + 1.597_6 * x),
        "cloglog" => 1.0 - (-x.exp()).exp(),
        "cauchit" => x.atan() / std::f64::consts::PI + 0.5,
        "inverse" => 1.0 / x,
        "sqrt" => x * x,
        "softplus" => x.exp().ln_1p(),
        "logm1" => 1.0 + x.exp(),
        "tan_half" => 2.0 * x.atan(),
        _ => x,
    }
}

/// A non-linear expression's value, its parameters and columns looked up by
/// position.
fn eval(
    e: &NlExpr,
    names: (&[String], &[String]),
    param: &dyn Fn(usize) -> f64,
    covariate: &dyn Fn(usize) -> f64,
) -> f64 {
    let go = |x: &NlExpr| eval(x, names, param, covariate);
    match e {
        NlExpr::Number(x) => *x,
        NlExpr::Param(n) => param(names.0.iter().position(|p| p == n).unwrap_or(0)),
        // Columns are numbered by first appearance, as in canonicalising.
        NlExpr::Covariate(n) => covariate(names.1.iter().position(|c| c == n).unwrap_or(0)),
        NlExpr::Neg(x) => -go(x),
        NlExpr::Binary { op, lhs, rhs } => {
            let (a, b) = (go(lhs), go(rhs));
            match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => a / b,
                _ => a.powf(b),
            }
        }
        NlExpr::Call { function, args } => {
            let a: Vec<f64> = args.iter().map(go).collect();
            match *function {
                "exp" => a[0].exp(),
                "log" => a[0].ln(),
                "log1p" => a[0].ln_1p(),
                "expm1" => a[0].exp_m1(),
                "log1p_exp" => a[0].exp().ln_1p(),
                "sqrt" => a[0].sqrt(),
                "square" => a[0] * a[0],
                "inv" => 1.0 / a[0],
                "inv_logit" => inv_logit(a[0]),
                "logit" => (a[0] / (1.0 - a[0])).ln(),
                "inv_cloglog" => 1.0 - (-a[0].exp()).exp(),
                "Phi" => phi(a[0]),
                "Phi_approx" => inv_logit(0.070_565_8 * a[0].powi(3) + 1.597_6 * a[0]),
                "abs" => a[0].abs(),
                "sin" => a[0].sin(),
                "cos" => a[0].cos(),
                "tan" => a[0].tan(),
                "tanh" => a[0].tanh(),
                "atan" => a[0].atan(),
                "fmin" => a[0].min(a[1]),
                "fmax" => a[0].max(a[1]),
                _ => a[0].powf(a[1]),
            }
        }
    }
}

/// The new rows, with the derived columns added, and the data variables of a
/// lowered configuration computed from them.
struct Rows<'a> {
    bindings: BTreeMap<String, Binding>,
    coordinates: &'a Coordinates,
    frame: Frame,
}

impl<'a> Rows<'a> {
    fn new(config: &Attrs, coordinates: &'a Coordinates, frame: &Frame) -> Result<Rows<'a>> {
        let mut bindings = BTreeMap::new();
        if let Some(Json::Object(m)) = config.get(sc_model::BINDINGS_KEY) {
            for (k, v) in m {
                if let Ok(b) = serde_json::from_value::<Binding>(v.clone()) {
                    bindings.insert(k.clone(), b);
                }
            }
        }
        let mut frame = frame.clone();
        if let Some(Json::Object(m)) = config.get(DERIVED_COLUMNS_KEY) {
            for (name, v) in m {
                let d: DerivedColumn = serde_json::from_value(v.clone())
                    .map_err(|e| Error::msg(format!("the derived column `{name}`: {e}")))?;
                let column = d.evaluate(name, &frame)?;
                frame.columns.push((name.clone(), column));
            }
        }
        Ok(Rows {
            bindings,
            coordinates,
            frame,
        })
    }

    fn has(&self, var: &str) -> bool {
        self.bindings.contains_key(var)
    }

    fn binding(&self, var: &str) -> Result<&Binding> {
        self.bindings
            .get(var)
            .ok_or_else(|| Error::msg(format!("the fit's configuration does not bind `{var}`")))
    }

    fn column(&self, name: &str) -> Result<&Column> {
        self.frame.column(name).ok_or_else(|| {
            Error::invalid(format!(
                "a prediction needs the column `{name}`, and the row has none"
            ))
        })
    }

    /// A `terms` binding's matrix, with the fit's encoding.
    fn matrix(&self, var: &str) -> Result<sc_model::terms::ModelMatrix> {
        let design = self
            .coordinates
            .designs
            .get(var)
            .ok_or_else(|| Error::msg(format!("the fit recorded no encoding for `{var}`")))?;
        let encoding = TermsEncoding::from_json(&design.encoding)?;
        apply_terms(&encoding, &self.frame)
    }

    /// A `column` or `value` binding's numbers, one per row.
    fn numbers(&self, var: &str) -> Result<Vec<f64>> {
        match self.binding(var)? {
            Binding::Value { value } => Ok(vec![value.as_f64().unwrap_or(0.0); self.frame.rows]),
            Binding::Column { column, .. } => {
                let c = self.column(column)?;
                (0..self.frame.rows)
                    .map(|i| {
                        number(c, i).ok_or_else(|| {
                            Error::invalid(format!(
                                "`{column}` is null (or not a number) in row {}, and a \
                                 prediction needs it",
                                i + 1
                            ))
                        })
                    })
                    .collect()
            }
            other => Err(Error::msg(format!(
                "`{var}` is bound as {}, not to numbers",
                other.describe()
            ))),
        }
    }

    /// A `value` binding's number.
    fn value(&self, var: &str) -> Result<f64> {
        match self.binding(var)? {
            Binding::Value { value } => crate::lower::try_real(value)
                .ok_or_else(|| Error::msg(format!("`{var}` is not a number"))),
            other => Err(Error::msg(format!(
                "`{var}` is bound as {}, not to a number",
                other.describe()
            ))),
        }
    }

    /// An `index` binding's 0-based positions: each row's level of the
    /// fit's dimension, refused by name when the fit did not have it.
    fn index(&self, var: &str) -> Result<Vec<usize>> {
        let Binding::Index {
            column, dimension, ..
        } = self.binding(var)?
        else {
            return Err(Error::msg(format!("`{var}` is not an index")));
        };
        let dim = self
            .coordinates
            .dimension(dimension)
            .ok_or_else(|| Error::msg(format!("the fit recorded no dimension `{dimension}`")))?;
        let c = self.column(column)?;
        (0..self.frame.rows)
            .map(|i| {
                let Some(text) = cell_text(c, i) else {
                    return Err(Error::invalid(format!(
                        "`{column}` is null in row {}, and a prediction needs its level",
                        i + 1
                    )));
                };
                dim.keys
                    .iter()
                    .position(|k| key_matches(k, &text))
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`{column}` is `{text}` in row {}, which was not a level of \
                             `{column}` when this was fitted, so the fit has no effect for it",
                            i + 1
                        ))
                    })
            })
            .collect()
    }
}

fn key_matches(key: &Json, text: &str) -> bool {
    match key {
        Json::String(s) => s == text,
        Json::Number(n) => match (n.as_f64(), text.parse::<f64>()) {
            (Some(a), Ok(b)) => a == b,
            _ => n.to_string() == text,
        },
        Json::Bool(b) => text == if *b { "true" } else { "false" },
        _ => false,
    }
}

fn cell_text(column: &Column, i: usize) -> Option<String> {
    match column {
        Column::Float(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Bool(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Str(v) => v.get(i).cloned().flatten(),
        Column::Date(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Null(_) => None,
    }
}

fn number(column: &Column, i: usize) -> Option<f64> {
    match column {
        Column::Float(v) => v.get(i).copied().flatten(),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x as f64),
        Column::Bool(v) => v.get(i).copied().flatten().map(|b| f64::from(u8::from(b))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_normal_cdf_is_accurate() {
        assert!((phi(0.0) - 0.5).abs() < 2e-7);
        assert!((phi(1.959_963_985) - 0.975).abs() < 1e-7);
        assert!((phi(-1.0) - 0.158_655_253_9).abs() < 1e-7);
    }

    #[test]
    fn ordinal_probabilities_sum_to_one_for_every_family() {
        for family in ["cumulative", "sratio", "cratio", "acat"] {
            for link in ["logit", "probit", "cloglog", "cauchit"] {
                let p = ordinal_probs(family, link, 0.3, &[-1.0, 0.2, 1.5], 1.2);
                let total: f64 = p.iter().sum();
                assert!((total - 1.0).abs() < 1e-12, "{family} {link}: {p:?}");
                assert!(p.iter().all(|x| *x > 0.0), "{family} {link}: {p:?}");
            }
        }
    }
}
