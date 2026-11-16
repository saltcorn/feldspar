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
use sc_model::{BINDINGS_KEY, DIMENSIONS_KEY, EXCLUDE_VARIABLES_KEY, Frame, POLICIES_KEY};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use crate::canon::{
    Canonical, CoefShape, DataPlan, InterceptShape, Structure, canonicalise, data_plan,
};
use crate::generate::{PriorVariable, generate, prior_variables};
use crate::parse::{ATerm, Formula, parse_formula};
use crate::prior::{self, Prior, PriorRow, PriorTable, merge_rows};
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
    let canonical = canonicalise(&resolved, &rows).map_err(Error::invalid)?;
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
        let (names, scale) = match frame {
            Some(f) => (Some(r.names(f)?), Some(r.data_scale(f))),
            None => (None, None),
        };
        let slots = r.prior_slots(names.as_ref());
        let table = prior::resolve(&slots, &self.rows, scale, &r.response, r.center);
        Ok((table, names))
    }

    /// The lowered configuration (§7): `config` plus the generated program,
    /// the bindings and the prior data. With `frame` (a preview, a fit) every
    /// prior is resolved to its numbers and the rows are checked against the
    /// coefficients by name; without, the prior data are placeholders of the
    /// right shapes, enough for the save-time checks.
    pub fn lower(&self, config: &Attrs, frame: Option<&Frame>, taken: &[String]) -> Result<Attrs> {
        let plan = data_plan(&self.resolved, &self.canonical, taken);
        let (table, names) = self.prior_table(frame)?;
        table.require_matched()?;
        if let Some(f) = frame {
            self.resolved.check_response(f)?;
            check_censoring(&self.resolved, f)?;
        }
        let priors = match &names {
            Some(names) => prior_data(self, &table, names)?,
            None => placeholder_data(self.structure()),
        };
        let (mut bindings, dimensions, policies) = plan.to_json();
        if let Json::Object(b) = &mut bindings {
            for (k, v) in priors {
                b.insert(k, json!({ "kind": "value", "value": v }));
            }
        }
        let mut out = config.clone();
        out.insert(BINDINGS_KEY.to_owned(), bindings);
        out.insert(DIMENSIONS_KEY.to_owned(), dimensions);
        out.insert(POLICIES_KEY.to_owned(), policies);
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

/// What each positional name is, for the parameter table: per linear
/// predictor its dpar, per block its grouping factor, its dimension and its
/// parts' dpars.
fn layout(built: &Built, plan: &DataPlan) -> Json {
    let s = built.structure();
    let blocks: Vec<Json> = built
        .canonical
        .block_order
        .iter()
        .enumerate()
        .map(|(k, &at)| {
            json!({
                "group": built.resolved.blocks[at].group,
                "dimension": plan.block_dimensions.get(k),
                "parts": built.canonical.block_parts[k],
            })
        })
        .collect();
    json!({
        "predictors": s.predictors.iter().map(|p| p.dpar.clone()).collect::<Vec<_>>(),
        "blocks": blocks,
        "scalars": s.scalars.iter().filter(|x| !x.fixed).map(|x| x.dpar.clone()).collect::<Vec<_>>(),
    })
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
    for x in &s.scalars {
        if x.fixed {
            out.insert(x.dpar.clone(), json!(1.0));
        }
    }
    out
}

fn insert_placeholder(out: &mut BTreeMap<String, Json>, v: &PriorVariable) {
    let p = &v.parameter;
    if let Some(k) = p.strip_prefix("cor_") {
        out.insert(format!("prior_cor_{k}_eta"), json!(1.0));
    } else if v.vector {
        out.insert(format!("prior_{p}_code"), json!([0]));
        out.insert(format!("prior_{p}_args"), json!([[0.0, 0.0, 0.0]]));
    } else {
        out.insert(format!("prior_{p}_code"), json!(0));
        out.insert(format!("prior_{p}_args"), json!([0.0, 0.0, 0.0]));
    }
}

/// The prior data: every parameter's code and numbers, from the resolved
/// table, element by element in the program's order.
fn prior_data(built: &Built, table: &PriorTable, names: &Names) -> Result<BTreeMap<String, Json>> {
    let params = table.parameters();
    let find = |class: &str, dpar: &str, group: &str, coef: &str| -> Result<Prior> {
        let key = (
            class.to_owned(),
            dpar.to_owned(),
            group.to_owned(),
            coef.to_owned(),
        );
        let row = params.get(&key).ok_or_else(|| {
            Error::msg(format!(
                "no resolved prior for `{class}` `{coef}` (group `{group}`, dpar `{dpar}`)"
            ))
        })?;
        Prior::parse(&row.prior).map_err(Error::invalid)
    };
    let bounds_of = |dpar: &str, coef: &str| -> (f64, f64) {
        let key = (
            "b".to_owned(),
            dpar.to_owned(),
            String::new(),
            coef.to_owned(),
        );
        params
            .get(&key)
            .map_or((f64::NEG_INFINITY, f64::INFINITY), |r| {
                (
                    r.lb.unwrap_or(f64::NEG_INFINITY),
                    r.ub.unwrap_or(f64::INFINITY),
                )
            })
    };
    let s = built.structure();
    let mut out = BTreeMap::new();
    let vector = |priors: &[Prior]| -> (Json, Json) {
        let (codes, args): (Vec<u8>, Vec<[f64; 3]>) = priors.iter().map(code_args).unzip();
        (json!(codes), json!(args))
    };
    for (i, p) in s.predictors.iter().enumerate() {
        let n = i + 1;
        let dk = if p.dpar == "mu" { "" } else { p.dpar.as_str() };
        if p.b != CoefShape::None {
            let coefs = names.b.get(&p.dpar).cloned().unwrap_or_default();
            let priors = coefs
                .iter()
                .map(|c| find("b", dk, "", c))
                .collect::<Result<Vec<_>>>()?;
            let (codes, args) = vector(&priors);
            out.insert(format!("prior_b_{n}_code"), codes);
            out.insert(format!("prior_b_{n}_args"), args);
            if p.b == CoefShape::Bounded {
                let (lb, ub): (Vec<Json>, Vec<Json>) = coefs
                    .iter()
                    .map(|c| {
                        let (lb, ub) = bounds_of(dk, c);
                        (real(lb), real(ub))
                    })
                    .unzip();
                out.insert(format!("lb_b_{n}"), Json::Array(lb));
                out.insert(format!("ub_b_{n}"), Json::Array(ub));
            }
        }
        if p.intercept != InterceptShape::None {
            let (code, args) = code_args(&find("Intercept", dk, "", "")?);
            out.insert(format!("prior_Intercept_{n}_code"), json!(code));
            out.insert(format!("prior_Intercept_{n}_args"), json!(args));
        }
    }
    for (k, &at) in built.canonical.block_order.iter().enumerate() {
        let n = k + 1;
        let block = &built.resolved.blocks[at];
        let mut priors = Vec::new();
        for dpar in &built.canonical.block_parts[k] {
            let dk = if *dpar == "mu" { "" } else { dpar };
            let coefs = names
                .blocks
                .get(&(at, (*dpar).to_owned()))
                .cloned()
                .unwrap_or_default();
            for c in &coefs {
                priors.push(find("sd", dk, &block.group, c)?);
            }
        }
        let (codes, args) = vector(&priors);
        out.insert(format!("prior_sd_{n}_code"), codes);
        out.insert(format!("prior_sd_{n}_args"), args);
        if s.blocks[k].correlated {
            // A block of one effect has no correlation and no row for one;
            // its LKJ term is a constant, so any η will do.
            let eta = find("cor", "", &block.group, "")
                .ok()
                .and_then(|p| p.args.first().copied())
                .unwrap_or(1.0);
            out.insert(format!("prior_cor_{n}_eta"), json!(eta));
        }
    }
    for x in &s.scalars {
        let prior = find(&x.dpar, "", "", "")?;
        if x.fixed {
            out.insert(
                x.dpar.clone(),
                json!(prior.args.first().copied().unwrap_or(0.0)),
            );
        } else {
            let (code, args) = code_args(&prior);
            out.insert(format!("prior_{}_code", x.dpar), json!(code));
            out.insert(format!("prior_{}_args", x.dpar), json!(args));
        }
    }
    Ok(out)
}
