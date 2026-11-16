//! The brms-style parameter table (TODO-bayesian-regression §6): the fit's
//! positional draws read back in the formula's terms — "Population-level
//! effects" (`Intercept`, `area`, `sigma_Intercept`), one "Group-level
//! effects" table per grouping factor (`sd(Intercept)`,
//! `cor(Intercept,area)`), and "Family-specific parameters" (`sigma`, `nu`).
//!
//! The columns are those `summary()` of a brms fit documents: the posterior
//! mean and SD, the 95 % central interval, R̂ and the bulk and tail effective
//! sample sizes — computed with the host's functions (`sc_model::rhat`,
//! `ess_bulk`, `ess_tail`), so they agree with the rest of the page.

use std::collections::BTreeMap;

use sc_error::Result;
use sc_model::{Coordinates, DrawSeries, ParameterBlock, ParameterRow};
use serde_json::{Value as Json, json};

/// The column headings.
pub const COLUMNS: [&str; 8] = [
    "Parameter",
    "Estimate",
    "Est.Error",
    "l-95% CI",
    "u-95% CI",
    "Rhat",
    "Bulk_ESS",
    "Tail_ESS",
];

/// Each element's draws, chain by chain: `(variable, element) → chain → draws`.
type ByElement<'a> = BTreeMap<(&'a str, &'a [usize]), BTreeMap<u32, &'a [f64]>>;

/// The draws of one element, chain by chain.
struct Draws<'a> {
    by: ByElement<'a>,
}

impl<'a> Draws<'a> {
    fn new(series: &'a [DrawSeries]) -> Draws<'a> {
        let mut by: ByElement<'a> = BTreeMap::new();
        for s in series.iter().filter(|s| !s.warmup) {
            by.entry((s.variable.as_str(), s.element.as_slice()))
                .or_default()
                .insert(s.chain, s.draws.as_slice());
        }
        Draws { by }
    }

    /// The row for `variable[element]`, named `label`, if it was drawn.
    fn row(&self, label: &str, variable: &str, element: &[usize]) -> Option<ParameterRow> {
        let chains: Vec<&[f64]> = self
            .by
            .get(&(variable, element))?
            .values()
            .copied()
            .collect();
        let mut all: Vec<f64> = chains.iter().flat_map(|c| c.iter().copied()).collect();
        if all.is_empty() {
            return None;
        }
        let n = all.len() as f64;
        let mean = all.iter().sum::<f64>() / n;
        let sd = if all.len() > 1 {
            (all.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
        } else {
            f64::NAN
        };
        all.sort_by(f64::total_cmp);
        let cell = |x: f64| if x.is_finite() { json!(x) } else { Json::Null };
        Some(ParameterRow::new([
            Json::from(label),
            cell(mean),
            cell(sd),
            cell(sc_model::quantile(&all, 0.025)),
            cell(sc_model::quantile(&all, 0.975)),
            cell(sc_model::rhat(&chains)),
            cell(sc_model::ess_bulk(&chains).round()),
            cell(sc_model::ess_tail(&chains).round()),
        ]))
    }
}

/// `sigma_area` for a coefficient of `sigma`'s formula, `area` for `mu`'s.
fn prefixed(dpar: &str, name: &str) -> String {
    if dpar == "mu" {
        name.to_owned()
    } else {
        format!("{dpar}_{name}")
    }
}

/// The tables, from the draws, the binder's coordinates (the model matrices'
/// column names) and the lowering's layout.
pub fn tables(
    draws: &[DrawSeries],
    coordinates: &Coordinates,
    layout: &Json,
    structure: &crate::Structure,
) -> Result<Vec<ParameterBlock>> {
    let d = Draws::new(draws);
    let columns_of = |var: &str| -> Vec<String> {
        coordinates
            .designs
            .get(var)
            .map(|c| c.columns.clone())
            .unwrap_or_default()
    };
    let mut out = Vec::new();

    // Population-level.
    let mut rows = Vec::new();
    for (i, p) in structure.predictors.iter().enumerate() {
        let n = i + 1;
        if p.intercept != crate::canon::InterceptShape::None {
            if let Some(r) = d.row(
                &prefixed(&p.dpar, "Intercept"),
                &format!("Intercept_{n}"),
                &[],
            ) {
                rows.push(r);
            }
        }
        if p.b != crate::canon::CoefShape::None {
            for (j, name) in columns_of(&format!("X_{n}")).iter().enumerate() {
                if let Some(r) = d.row(&prefixed(&p.dpar, name), &format!("b_{n}"), &[j + 1]) {
                    rows.push(r);
                }
            }
        }
    }
    if !rows.is_empty() {
        out.push(ParameterBlock::table(
            "Population-level effects",
            COLUMNS,
            rows,
        )?);
    }

    // Group-level, one table per grouping factor.
    let blocks = layout
        .get("blocks")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let mut by_group: Vec<(String, Vec<ParameterRow>)> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let k = i + 1;
        let group = b
            .get("group")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let parts: Vec<String> = b
            .get("parts")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let mut names = Vec::new();
        for (j, dpar) in parts.iter().enumerate() {
            for c in columns_of(&format!("Z_{k}_{}", j + 1)) {
                names.push(prefixed(dpar, &c));
            }
        }
        let mut rows = Vec::new();
        for (m, name) in names.iter().enumerate() {
            if let Some(r) = d.row(&format!("sd({name})"), &format!("sd_{k}"), &[m + 1]) {
                rows.push(r);
            }
        }
        if structure.blocks.get(i).is_some_and(|s| s.correlated) {
            for a in 0..names.len() {
                for c in (a + 1)..names.len() {
                    let label = format!("cor({},{})", names[a], names[c]);
                    if let Some(r) = d.row(&label, &format!("Cor_{k}"), &[c + 1, a + 1]) {
                        rows.push(r);
                    }
                }
            }
        }
        match by_group.iter_mut().find(|(g, _)| *g == group) {
            Some((_, existing)) => existing.extend(rows),
            None => by_group.push((group, rows)),
        }
    }
    for (group, rows) in by_group {
        if !rows.is_empty() {
            out.push(ParameterBlock::table(
                format!("Group-level effects ~{group}"),
                COLUMNS,
                rows,
            )?);
        }
    }

    // Family-specific.
    let rows: Vec<ParameterRow> = structure
        .scalars
        .iter()
        .filter(|s| !s.fixed)
        .filter_map(|s| d.row(&s.dpar, &s.dpar, &[]))
        .collect();
    if !rows.is_empty() {
        out.push(ParameterBlock::table(
            "Family-specific parameters",
            COLUMNS,
            rows,
        )?);
    }
    Ok(out)
}
