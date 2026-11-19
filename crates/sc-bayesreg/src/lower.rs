//! The configuration → the program and its bindings (TODO-bayesian-regression
//! §7): what [`BayesRegProvider::lower`](crate::BayesRegProvider) answers, and
//! what the editor's API shows (the resolved priors, the generated program).
//!
//! The pipeline is §5's: the formula text is parsed, resolved against what is
//! known of the dataset, canonicalised into a [`Structure`] and generated into
//! Stan; the [`DataPlan`] binds every data variable, and the resolved prior
//! table fills in the prior data. Nothing here reads a database or a file.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_model::{
    BINDINGS_KEY, DERIVED_COLUMNS_KEY, DIMENSIONS_KEY, EXCLUDE_VARIABLES_KEY, Frame, POLICIES_KEY,
};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use crate::canon::{
    Canonical, CoefShape, DataPlan, InterceptShape, Options, PlanContext, Structure,
    canonicalise_with, data_plan_in, suffix,
};
use crate::family::Kind;
use crate::generate::{PriorVariable, generate, prior_variables};
use crate::parse::{ATerm, Formula, parse_formula};
use crate::prior::{self, Prior, PriorRow, PriorTable, Target, merge_rows};
use crate::resolve::{Data, Names, Resolved, Settings, resolve};

/// The provider's configuration keys.
pub mod keys {
    /// The formula text.
    pub const FORMULA: &str = "formula";
    /// The family's name.
    pub const FAMILY: &str = "family";
    /// Links by distributional parameter: `{"mu": "probit"}`.
    pub const LINKS: &str = "links";
    /// The explicit prior rows (§3.7): only what the admin set.
    pub const PRIORS: &str = "priors";
    /// Centre the population-level predictors (default on).
    pub const CENTER: &str = "center";
    /// Lowered only: the generated program's text.
    pub const GENERATED_PROGRAM: &str = "generated_program";
    /// Lowered only: the canonical structure.
    pub const STRUCTURE: &str = "structure";
    /// Lowered only: what each positional block and predictor is, for the
    /// parameter table (§6).
    pub const LAYOUT: &str = "layout";
    /// Lowered only: every parameter's resolved prior.
    pub const RESOLVED_PRIORS: &str = "resolved_priors";
    /// Compute each row's expected response (`epred_1`) in the program's
    /// generated quantities (default off): structural, so another program.
    pub const EPRED: &str = "epred";
}

/// The form's settings in `config`.
pub fn settings(config: &Attrs) -> Result<Settings> {
    let mut s = Settings::default();
    if let Some(f) = config.get(keys::FAMILY).and_then(Json::as_str) {
        if !f.trim().is_empty() {
            f.trim().clone_into(&mut s.family);
        }
    }
    match config.get(keys::LINKS) {
        None | Some(Json::Null) => {}
        Some(Json::Object(m)) => {
            for (dpar, link) in m {
                match link {
                    Json::String(l) if !l.trim().is_empty() => {
                        s.links.insert(dpar.clone(), l.trim().to_owned());
                    }
                    Json::String(_) | Json::Null => {}
                    other => {
                        return Err(Error::invalid(format!(
                            "`{}`: the link of `{dpar}` must be a name, not {other}",
                            keys::LINKS
                        )));
                    }
                }
            }
        }
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{}` must be an object of links by parameter, not {other}",
                keys::LINKS
            )));
        }
    }
    if let Some(c) = config.get(keys::CENTER).filter(|v| !v.is_null()) {
        s.center = c
            .as_bool()
            .ok_or_else(|| Error::invalid(format!("`{}` must be true or false", keys::CENTER)))?;
    }
    Ok(s)
}

/// The options in `config` that change the program beyond the formula.
pub fn options(config: &Attrs) -> Result<Options> {
    let epred = match config.get(keys::EPRED) {
        None | Some(Json::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| Error::invalid(format!("`{}` must be true or false", keys::EPRED)))?,
    };
    Ok(Options { epred })
}

/// The explicit prior rows in `config`.
pub fn rows(config: &Attrs) -> Result<Vec<PriorRow>> {
    match config.get(keys::PRIORS) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            Error::invalid(format!(
                "`{}` must be a list of prior rows ({{class, coef, group, dpar, prior, lb, \
                 ub}}): {e}",
                keys::PRIORS
            ))
        }),
    }
}

/// The formula text in `config`.
pub fn formula_text(config: &Attrs) -> Result<&str> {
    config
        .get(keys::FORMULA)
        .and_then(Json::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| Error::invalid(format!("`{}`: write the model's formula", keys::FORMULA)))
}

/// A configuration built as far as the program: the formula, resolved and
/// canonicalised, and the program generated.
#[derive(Debug, Clone)]
pub struct Built {
    /// The formula as parsed.
    pub formula: Formula,
    /// The explicit rows: the configuration's, with any pasted into the
    /// formula merged in.
    pub rows: Vec<PriorRow>,
    /// Resolved.
    pub resolved: Resolved,
    /// Canonicalised.
    pub canonical: Canonical,
    /// The program.
    pub program: String,
}

impl Built {
    /// The structure.
    pub fn structure(&self) -> &Structure {
        &self.canonical.structure
    }
}

/// Build `config` against `data`.
pub fn build(config: &Attrs, data: Data<'_>) -> Result<Built> {
    let text = formula_text(config)?;
    let settings = settings(config)?;
    let formula = parse_formula(text).map_err(Error::from)?;
    let pasted: Vec<PriorRow> = formula.priors.iter().map(|p| p.row.clone()).collect();
    let (rows, _) = merge_rows(&rows(config)?, &pasted);
    let resolved = resolve(text, &formula, &settings, data).map_err(Error::from)?;
    let canonical =
        canonicalise_with(&resolved, &rows, options(config)?).map_err(Error::invalid)?;
    let program = generate(&canonical.structure);
    Ok(Built {
        formula,
        rows,
        resolved,
        canonical,
        program,
    })
}

impl Built {
    /// The prior table: with `frame`, every coefficient by name and the
    /// data-dependent defaults computed; without, the templates and the rules.
    pub fn prior_table(&self, frame: Option<&Frame>) -> Result<(PriorTable, Option<Names>)> {
        let r = &self.resolved;
        let (names, scales) = match frame {
            Some(f) => (Some(r.names(f)?), Some(r.data_scales(f))),
            None => (None, None),
        };
        let slots = r.prior_slots(names.as_ref());
        let responses: Vec<String> = r.responses.iter().map(|s| s.response.clone()).collect();
        let table =
            prior::resolve_each(&slots, &self.rows, scales.as_deref(), &responses, r.center);
        Ok((table, names))
    }

    /// The lowered configuration (§7): `config` plus the generated program,
    /// the bindings and the prior data. With `frame` (a preview, a fit) every
    /// prior is resolved to its numbers and the rows are checked against the
    /// coefficients by name; without, the prior data are placeholders of the
    /// right shapes, enough for the save-time checks.
    pub fn lower(&self, config: &Attrs, frame: Option<&Frame>, taken: &[String]) -> Result<Attrs> {
        self.lower_in(
            config,
            frame,
            &PlanContext {
                taken: taken.to_vec(),
                ..PlanContext::default()
            },
        )
    }

    /// [`lower`](Built::lower) with the model's [`PlanContext`].
    pub fn lower_in(
        &self,
        config: &Attrs,
        frame: Option<&Frame>,
        ctx: &PlanContext,
    ) -> Result<Attrs> {
        let plan = data_plan_in(&self.resolved, &self.canonical, ctx);
        let (table, names) = self.prior_table(frame)?;
        table.require_matched()?;
        require_nl_priors(&table)?;
        if let Some(f) = frame {
            self.resolved.check_response(f)?;
            check_censoring(&self.resolved, f)?;
            check_by(&self.resolved, f)?;
        }
        let priors = match (&names, frame) {
            (Some(names), Some(f)) => {
                let mut data = prior_data(self, &table, names, f)?;
                data.extend(frame_data(self, f)?);
                data
            }
            _ => placeholder_data(self.structure()),
        };
        let (mut bindings, dimensions, policies, derived) = plan.to_json();
        if let Json::Object(b) = &mut bindings {
            for (k, v) in priors {
                b.insert(k, json!({ "kind": "value", "value": v }));
            }
        }
        let mut out = config.clone();
        out.insert(BINDINGS_KEY.to_owned(), bindings);
        out.insert(DIMENSIONS_KEY.to_owned(), dimensions);
        out.insert(POLICIES_KEY.to_owned(), policies);
        if derived.as_object().is_some_and(|m| !m.is_empty()) {
            out.insert(DERIVED_COLUMNS_KEY.to_owned(), derived);
        }
        out.insert(
            keys::GENERATED_PROGRAM.to_owned(),
            Json::from(self.program.clone()),
        );
        out.insert(
            keys::STRUCTURE.to_owned(),
            serde_json::to_value(self.structure()).unwrap_or(Json::Null),
        );
        out.insert(keys::LAYOUT.to_owned(), layout(self, &plan));
        out.insert(
            keys::RESOLVED_PRIORS.to_owned(),
            serde_json::to_value(&table).unwrap_or(Json::Null),
        );
        out.insert(
            keys::PRIORS.to_owned(),
            serde_json::to_value(&self.rows).unwrap_or(Json::Null),
        );
        // The raw effects `z` are summarised but their draws are not kept:
        // `r`, which they make, is what anybody reads.
        let mut excluded: Vec<Json> = config
            .get(EXCLUDE_VARIABLES_KEY)
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        for k in 1..=self.structure().blocks.len() {
            let z = Json::from(format!("z_{k}"));
            if !excluded.contains(&z) {
                excluded.push(z);
            }
        }
        if !excluded.is_empty() {
            out.insert(EXCLUDE_VARIABLES_KEY.to_owned(), Json::Array(excluded));
        }
        Ok(out)
    }
}

/// The brms-style prefix of a linear predictor's coefficients: its dpar (not
/// `mu`), its non-linear parameter, and its response in a multivariate model,
/// each followed by `_` — `sigma_`, `a_`, `y2_`, `sigma_y2_`.
pub fn prefix(built: &Built, predictor: usize) -> String {
    let r = &built.resolved;
    let p = &r.predictors[predictor];
    let mut parts: Vec<String> = Vec::new();
    if p.dpar != "mu" && p.nlpar.is_none() {
        parts.push(p.dpar.to_owned());
    }
    if r.multivariate() {
        parts.push(r.responses[p.resp].response.clone());
    }
    if let Some(n) = &p.nlpar {
        parts.push(n.clone());
    }
    parts.iter().map(|x| format!("{x}_")).collect()
}

/// What each positional name is, for the parameter table and a prediction:
/// per linear predictor its dpar and prefix, per block its grouping factor,
/// its dimension and its parts' prefixes, the scalars' labels, and the
/// dimensions of `by`, of `mo()` terms and of a categorical response.
fn layout(built: &Built, plan: &DataPlan) -> Json {
    let s = built.structure();
    let c = &built.canonical;
    let r = &built.resolved;
    let predictors: Vec<Json> = c
        .predictor_order
        .iter()
        .enumerate()
        .map(|(n0, &i)| {
            let p = &r.predictors[i];
            json!({
                "dpar": p.dpar,
                "prefix": prefix(built, i),
                "key": p.key(&r.responses),
                "mo": p.mo,
                "mo_dimensions": plan.mo_dimensions.get(&n0),
            })
        })
        .collect();
    let blocks: Vec<Json> = c
        .block_order
        .iter()
        .enumerate()
        .map(|(k, &at)| {
            json!({
                "group": r.blocks[at].group,
                "dimension": plan.block_dimensions.get(k),
                "by_dimension": plan.by_dimensions.get(&k),
                "parts": c.block_parts[k].iter().map(|&i| prefix(built, i)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mv = r.multivariate();
    let mut scalars: Vec<Json> = Vec::new();
    for (pos, &q) in c.response_order.iter().enumerate() {
        for x in s.sub(pos).0.iter().filter(|x| !x.fixed) {
            let label = if mv {
                format!("{}_{}", x.dpar, r.responses[q].response)
            } else {
                x.dpar.clone()
            };
            scalars
                .push(json!({ "variable": format!("{}{}", x.dpar, suffix(pos)), "label": label }));
        }
    }
    json!({
        "predictors": predictors,
        "blocks": blocks,
        "scalars": scalars,
        "responses": c.response_order.iter().map(|&q| r.responses[q].response.clone()).collect::<Vec<_>>(),
        "category_dimension": plan.category_dimension,
    })
}

/// A non-linear parameter's coefficients need proper priors: with a flat
/// prior the posterior of a non-linear model is easily improper, which is
/// why brms requires them too (`?brmsformula`, "non-linear models").
pub fn require_nl_priors(table: &PriorTable) -> Result<()> {
    match table
        .rows
        .iter()
        .find(|r| !r.nlpar.is_empty() && r.class == "b" && r.prior == "flat")
    {
        None => Ok(()),
        Some(r) => Err(Error::invalid(format!(
            "the non-linear parameter `{}` needs a proper prior on `b`{}: set one in the prior \
             table (`prior(normal(0, 5), nlpar = {})`)",
            r.nlpar,
            if r.coef.is_empty() {
                String::new()
            } else {
                format!(" (it is flat on `{}`)", r.coef)
            },
            r.nlpar
        ))),
    }
}

/// With `gr(by = h)`, each level of the grouping factor is within one level
/// of `h` (`?gr`): refused, naming the first level that is not.
fn check_by(r: &Resolved, frame: &Frame) -> Result<()> {
    let kept = r.kept(frame)?;
    for b in &r.blocks {
        let Some(by) = &b.by else { continue };
        let (Some(g), Some(h)) = (
            b.factors.first().and_then(|f| kept.column(f)),
            kept.column(by),
        ) else {
            continue;
        };
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for i in 0..kept.rows {
            let (Some(gv), Some(hv)) = (cell(g, i), cell(h, i)) else {
                continue;
            };
            if let Some(first) = seen.get(&gv) {
                if *first != hv {
                    return Err(Error::invalid(format!(
                        "`gr({}, by = {by})`: `{gv}` of `{}` is in two levels of `{by}` (`{first}` \
                         and `{hv}`); each level of the grouping factor must be within one level \
                         of `by`",
                        b.group, b.group
                    )));
                }
            } else {
                seen.insert(gv, hv);
            }
        }
    }
    Ok(())
}

fn cell(column: &sc_model::Column, i: usize) -> Option<String> {
    use sc_model::Column;
    match column {
        Column::Float(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Bool(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Str(v) => v.get(i).cloned().flatten(),
        Column::Date(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Null(_) => None,
    }
}

/// The data a fit reads from its rows that no binding computes: an ordinal
/// response's number of thresholds (from its largest category, unless
/// `thres(k)` gives it).
fn frame_data(built: &Built, frame: &Frame) -> Result<BTreeMap<String, Json>> {
    let mut out = BTreeMap::new();
    let r = &built.resolved;
    if r.family.kind != Kind::Ordinal {
        return Ok(out);
    }
    let kept = r.kept(frame)?;
    for (pos, &q) in built.canonical.response_order.iter().enumerate() {
        let sub = &r.responses[q];
        let n = match sub.thres() {
            Some(k) => k,
            None => {
                let max = kept
                    .column(&sub.response)
                    .map(|c| {
                        r.response_rows(q, &kept)
                            .into_iter()
                            .filter_map(|i| cell(c, i).and_then(|t| t.parse::<f64>().ok()))
                            .fold(0.0_f64, f64::max)
                    })
                    .unwrap_or(0.0);
                if max < 2.0 {
                    return Err(Error::invalid(format!(
                        "`{}` has one category only, and an ordinal model needs two or more",
                        sub.response
                    )));
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let k = max as usize - 1;
                k
            }
        };
        out.insert(format!("nthres{}", suffix(pos)), json!(n));
    }
    Ok(out)
}

/// The cens column's values are −1, 0, 1 or 2 (or yes/no: yes is right
/// censored), refused at the first row that is not.
fn check_censoring(r: &Resolved, frame: &Frame) -> Result<()> {
    for a in &r.aterms {
        let ATerm::Cens(c) = a else { continue };
        let Some(column) = frame.column(&c.column) else {
            continue;
        };
        use sc_model::Column;
        match column {
            Column::Bool(_) => {}
            Column::Int(v) => {
                if let Some((i, x)) = v
                    .iter()
                    .enumerate()
                    .find_map(|(i, x)| x.filter(|x| !(-1..=2).contains(x)).map(|x| (i, x)))
                {
                    return Err(Error::invalid(format!(
                        "`cens({})`: row {} is {x}; a censoring column holds -1 (left), 0 \
                         (none), 1 (right) or 2 (interval)",
                        c.column,
                        i + 1
                    )));
                }
                if c.upper.is_none() {
                    if let Some(i) = v.iter().position(|x| *x == Some(2)) {
                        return Err(Error::invalid(format!(
                            "`cens({})`: row {} is interval-censored (2), so write the \
                             interval's upper end too: `cens({}, upper_column)`",
                            c.column,
                            i + 1,
                            c.column
                        )));
                    }
                }
            }
            other => {
                return Err(Error::invalid(format!(
                    "`cens({})` is a {} column; this release reads censoring as the numbers \
                     -1 (left), 0 (none), 1 (right) and 2 (interval), or yes/no for right \
                     censoring — make it one of those in the dataset",
                    c.column,
                    other.kind().name()
                )));
            }
        }
    }
    Ok(())
}

/// A prior as the program reads it: its code and three numbers.
fn code_args(prior: &Prior) -> (u8, [f64; 3]) {
    let mut args = [0.0; 3];
    for (slot, x) in args.iter_mut().zip(&prior.args) {
        *slot = *x;
    }
    (prior.family.code(), args)
}

fn real(x: f64) -> Json {
    if x == f64::INFINITY {
        Json::from("Inf")
    } else if x == f64::NEG_INFINITY {
        Json::from("-Inf")
    } else {
        json!(x)
    }
}

/// A real as the JSON a draw or a binding holds it: `"Inf"`, `"-Inf"` and
/// `"NaN"` for the values JSON has no number for.
pub(crate) fn real_json(x: f64) -> Json {
    if x.is_nan() {
        Json::from("NaN")
    } else {
        real(x)
    }
}

/// The real a JSON value holds, as [`real_json`] writes it.
pub(crate) fn try_real(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => match s.as_str() {
            "Inf" => Some(f64::INFINITY),
            "-Inf" => Some(f64::NEG_INFINITY),
            "NaN" => Some(f64::NAN),
            _ => None,
        },
        _ => None,
    }
}

/// [`try_real`], NaN for anything else.
pub(crate) fn json_real(v: &Json) -> f64 {
    try_real(v).unwrap_or(f64::NAN)
}

/// Placeholders of the shapes the program declares, for the save-time checks
/// (ranks and types; the lengths are only known with the data).
fn placeholder_data(s: &Structure) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    for v in prior_variables(s) {
        insert_placeholder(&mut out, &v);
    }
    for (i, p) in s.predictors.iter().enumerate() {
        if p.b == CoefShape::Bounded {
            out.insert(format!("lb_b_{}", i + 1), json!([0.0]));
            out.insert(format!("ub_b_{}", i + 1), json!([0.0]));
        }
    }
    for r in 0..s.responses() {
        for x in s.sub(r).0 {
            if x.fixed {
                out.insert(format!("{}{}", x.dpar, suffix(r)), json!(1.5));
            }
        }
    }
    for (i, p) in s.predictors.iter().enumerate() {
        for j in 1..=p.mo {
            out.insert(format!("con_simo_{}_{j}", i + 1), json!([1.0]));
        }
    }
    out
}

fn insert_placeholder(out: &mut BTreeMap<String, Json>, v: &PriorVariable) {
    let p = &v.parameter;
    if let Some(k) = p.strip_prefix("cor_") {
        out.insert(format!("prior_cor_{k}_eta"), json!(1.0));
    } else if p == "rescor" {
        out.insert("prior_rescor_eta".to_owned(), json!(1.0));
    } else if v.vector {
        out.insert(format!("prior_{p}_code"), json!([0]));
        out.insert(format!("prior_{p}_args"), json!([[0.0, 0.0, 0.0]]));
    } else {
        out.insert(format!("prior_{p}_code"), json!(0));
        out.insert(format!("prior_{p}_args"), json!([0.0, 0.0, 0.0]));
    }
}

/// The prior data: every parameter's code and numbers, from the resolved
/// table, element by element in the program's order; and a monotonic
/// effect's Dirichlet concentrations, sized by its levels in `frame`.
#[allow(clippy::too_many_lines)] // every class of parameter in turn
fn prior_data(
    built: &Built,
    table: &PriorTable,
    names: &Names,
    frame: &Frame,
) -> Result<BTreeMap<String, Json>> {
    let params = table.parameters();
    let r = &built.resolved;
    let mv = r.multivariate();
    let resp_of = |q: usize| {
        if mv {
            r.responses[q].response.clone()
        } else {
            String::new()
        }
    };
    let find = |t: Target| -> Result<Prior> {
        let row = params.get(&t).ok_or_else(|| {
            Error::msg(format!(
                "no resolved prior for `{}` `{}` (group `{}`, dpar `{}`, nlpar `{}`, resp `{}`)",
                t.class, t.coef, t.group, t.dpar, t.nlpar, t.resp
            ))
        })?;
        Prior::parse(&row.prior).map_err(Error::invalid)
    };
    let target = |class: &str, p: &crate::resolve::Predictor, group: &str, coef: &str| Target {
        class: class.to_owned(),
        dpar: p.dpar_key().to_owned(),
        nlpar: p.nlpar.clone().unwrap_or_default(),
        resp: resp_of(p.resp),
        group: group.to_owned(),
        coef: coef.to_owned(),
    };
    let s = built.structure();
    let c = &built.canonical;
    let mut out = BTreeMap::new();
    let vector = |priors: &[Prior]| -> (Json, Json) {
        let (codes, args): (Vec<u8>, Vec<[f64; 3]>) = priors.iter().map(code_args).unzip();
        (json!(codes), json!(args))
    };
    let kept = r.kept(frame)?;
    for (n0, &i) in c.predictor_order.iter().enumerate() {
        let n = n0 + 1;
        let p = &r.predictors[i];
        let shape = &s.predictors[n0];
        let key = p.key(&r.responses);
        let all = names.b.get(&key).cloned().unwrap_or_default();
        let (coefs, mo_coefs) = all.split_at(all.len().saturating_sub(p.mo.len()));
        if shape.b != CoefShape::None {
            let priors = coefs
                .iter()
                .map(|co| find(target("b", p, "", co)))
                .collect::<Result<Vec<_>>>()?;
            let (codes, args) = vector(&priors);
            out.insert(format!("prior_b_{n}_code"), codes);
            out.insert(format!("prior_b_{n}_args"), args);
            if shape.b == CoefShape::Bounded {
                let (lb, ub): (Vec<Json>, Vec<Json>) = coefs
                    .iter()
                    .map(|co| {
                        let row = params.get(&target("b", p, "", co));
                        (
                            real(row.and_then(|r| r.lb).unwrap_or(f64::NEG_INFINITY)),
                            real(row.and_then(|r| r.ub).unwrap_or(f64::INFINITY)),
                        )
                    })
                    .unzip();
                out.insert(format!("lb_b_{n}"), Json::Array(lb));
                out.insert(format!("ub_b_{n}"), Json::Array(ub));
            }
        }
        if shape.intercept != InterceptShape::None {
            let (code, args) = code_args(&find(target("Intercept", p, "", ""))?);
            out.insert(format!("prior_Intercept_{n}_code"), json!(code));
            out.insert(format!("prior_Intercept_{n}_args"), json!(args));
        }
        if !p.mo.is_empty() {
            let priors = mo_coefs
                .iter()
                .map(|co| find(target("b", p, "", co)))
                .collect::<Result<Vec<_>>>()?;
            let (codes, args) = vector(&priors);
            out.insert(format!("prior_bsp_{n}_code"), codes);
            out.insert(format!("prior_bsp_{n}_args"), args);
            for (j, m) in p.mo.iter().enumerate() {
                let levels = kept.column(m).map_or(0, |col| {
                    let mut seen = std::collections::BTreeSet::new();
                    for row in 0..kept.rows {
                        if let Some(t) = cell(col, row) {
                            seen.insert(t);
                        }
                    }
                    seen.len()
                });
                if levels < 2 {
                    return Err(Error::invalid(format!(
                        "`mo({m})`: `{m}` has {levels} level{} in the rows a fit keeps, and a \
                         monotonic effect needs two or more",
                        if levels == 1 { "" } else { "s" }
                    )));
                }
                let alpha = find(target("simo", p, "", &format!("mo{m}1")))?
                    .args
                    .first()
                    .copied()
                    .unwrap_or(1.0);
                out.insert(
                    format!("con_simo_{n}_{}", j + 1),
                    json!(vec![alpha; levels - 1]),
                );
            }
        }
        if shape.cs {
            let cs = names.cs.get(&key).cloned().unwrap_or_default();
            let priors = cs
                .iter()
                .map(|co| find(target("b", p, "", co)))
                .collect::<Result<Vec<_>>>()?;
            let (codes, args) = vector(&priors);
            out.insert(format!("prior_bcs_{n}_code"), codes);
            out.insert(format!("prior_bcs_{n}_args"), args);
        }
    }
    for (k, &at) in c.block_order.iter().enumerate() {
        let n = k + 1;
        let block = &r.blocks[at];
        let mut priors = Vec::new();
        for &pi in &c.block_parts[k] {
            let p = &r.predictors[pi];
            let coefs = names
                .blocks
                .get(&(at, p.key(&r.responses)))
                .cloned()
                .unwrap_or_default();
            for co in &coefs {
                priors.push(find(target("sd", p, &block.group, co))?);
            }
        }
        let (codes, args) = vector(&priors);
        out.insert(format!("prior_sd_{n}_code"), codes);
        out.insert(format!("prior_sd_{n}_args"), args);
        if s.blocks[k].correlated {
            // A block of one effect has no correlation and no row for one;
            // its LKJ term is a constant, so any η will do.
            let eta = find(Target {
                class: "cor".to_owned(),
                group: block.group.clone(),
                ..Target::default()
            })
            .ok()
            .and_then(|p| p.args.first().copied())
            .unwrap_or(1.0);
            out.insert(format!("prior_cor_{n}_eta"), json!(eta));
        }
    }
    for (pos, &q) in c.response_order.iter().enumerate() {
        let suf = suffix(pos);
        for x in s.sub(pos).0 {
            let name = format!("{}{suf}", x.dpar);
            if let Some(v) = r.responses[q].fixed.get(x.dpar.as_str()) {
                out.insert(name, json!(v));
                continue;
            }
            let prior = find(Target {
                class: x.dpar.clone(),
                resp: resp_of(q),
                ..Target::default()
            })?;
            if x.fixed {
                out.insert(name, json!(prior.args.first().copied().unwrap_or(0.0)));
            } else {
                let (code, args) = code_args(&prior);
                out.insert(format!("prior_{name}_code"), json!(code));
                out.insert(format!("prior_{name}_args"), json!(args));
            }
        }
    }
    if s.rescor {
        let eta = find(Target {
            class: "rescor".to_owned(),
            ..Target::default()
        })
        .ok()
        .and_then(|p| p.args.first().copied())
        .unwrap_or(1.0);
        out.insert("prior_rescor_eta".to_owned(), json!(eta));
    }
    Ok(out)
}
