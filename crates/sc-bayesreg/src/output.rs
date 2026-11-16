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

    /// How many elements of a vector `variable` were drawn (its first index's
    /// largest value).
    fn len(&self, variable: &str) -> usize {
        self.by
            .keys()
            .filter(|(v, _)| *v == variable)
            .filter_map(|(_, e)| e.first().copied())
            .max()
            .unwrap_or(0)
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

/// The text of a layout field.
fn text(v: &Json, key: &str) -> String {
    v.get(key).and_then(Json::as_str).unwrap_or("").to_owned()
}

/// A layout list of texts.
fn texts(v: &Json, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The tables, from the draws, the binder's coordinates (the model matrices'
/// column names and the dimensions' labels) and the lowering's layout.
#[allow(clippy::too_many_lines)] // one table after another
pub fn tables(
    draws: &[DrawSeries],
    coordinates: &Coordinates,
    layout: &Json,
    structure: &crate::Structure,
) -> Result<Vec<ParameterBlock>> {
    use crate::canon::{CoefShape, InterceptShape};
    use crate::family::Kind;
    let d = Draws::new(draws);
    let columns_of = |var: &str| -> Vec<String> {
        coordinates
            .designs
            .get(var)
            .map(|c| c.columns.clone())
            .unwrap_or_default()
    };
    let labels_of = |dim: &str| -> Vec<String> {
        coordinates
            .dimension(dim)
            .map(|c| c.labels.clone())
            .unwrap_or_default()
    };
    let categorical = structure.kind() == Kind::Categorical;
    // A categorical response's categories but the first, by label.
    let categories: Vec<String> = layout
        .get("category_dimension")
        .and_then(Json::as_str)
        .map(|dim| labels_of(dim).into_iter().skip(1).collect())
        .unwrap_or_default();
    let category = |c: usize| {
        categories
            .get(c)
            .map_or_else(|| format!("mu{}", c + 2), |l| format!("mu{l}"))
    };
    let predictors = layout
        .get("predictors")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();

    // Population-level.
    let mut rows = Vec::new();
    for (i, p) in structure.predictors.iter().enumerate() {
        let n = i + 1;
        let entry = predictors.get(i).cloned().unwrap_or(Json::Null);
        let prefix = text(&entry, "prefix");
        let intercept = format!("Intercept_{n}");
        match p.intercept {
            InterceptShape::None => {}
            _ if p.intercept.thresholds() => {
                for t in 1..=d.len(&intercept) {
                    if let Some(r) = d.row(&format!("{prefix}Intercept[{t}]"), &intercept, &[t]) {
                        rows.push(r);
                    }
                }
            }
            _ if categorical => {
                for c in 0..d.len(&intercept) {
                    if let Some(r) =
                        d.row(&format!("{}_Intercept", category(c)), &intercept, &[c + 1])
                    {
                        rows.push(r);
                    }
                }
            }
            _ => {
                if let Some(r) = d.row(&format!("{prefix}Intercept"), &intercept, &[]) {
                    rows.push(r);
                }
            }
        }
        if p.b != CoefShape::None {
            for (j, name) in columns_of(&format!("X_{n}")).iter().enumerate() {
                if categorical {
                    for c in 0..categories.len().max(1) {
                        if let Some(r) = d.row(
                            &format!("{}_{name}", category(c)),
                            &format!("b_{n}"),
                            &[j + 1, c + 1],
                        ) {
                            rows.push(r);
                        }
                    }
                } else if let Some(r) =
                    d.row(&format!("{prefix}{name}"), &format!("b_{n}"), &[j + 1])
                {
                    rows.push(r);
                }
            }
        }
        for (j, m) in texts(&entry, "mo").iter().enumerate() {
            if let Some(r) = d.row(&format!("{prefix}mo{m}"), &format!("bsp_{n}"), &[j + 1]) {
                rows.push(r);
            }
        }
        if p.cs {
            let thresholds = d.len(&intercept).max(1);
            for (j, name) in columns_of(&format!("Xcs_{n}")).iter().enumerate() {
                for t in 1..=thresholds {
                    if let Some(r) = d.row(
                        &format!("{prefix}{name}[{t}]"),
                        &format!("bcs_{n}"),
                        &[j + 1, t],
                    ) {
                        rows.push(r);
                    }
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

    // Monotonic effects' simplexes.
    let mut rows = Vec::new();
    for (i, p) in structure.predictors.iter().enumerate() {
        let entry = predictors.get(i).cloned().unwrap_or(Json::Null);
        let prefix = text(&entry, "prefix");
        for (j, m) in texts(&entry, "mo").iter().enumerate().take(p.mo) {
            let var = format!("simo_{}_{}", i + 1, j + 1);
            for e in 1..=d.len(&var) {
                if let Some(r) = d.row(&format!("{prefix}mo{m}1[{e}]"), &var, &[e]) {
                    rows.push(r);
                }
            }
        }
    }
    if !rows.is_empty() {
        out.push(ParameterBlock::table(
            "Monotonic simplex parameters",
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
        let group = text(b, "group");
        let parts = texts(b, "parts");
        let mut names = Vec::new();
        for (j, prefix) in parts.iter().enumerate() {
            let cols = columns_of(&format!("Z_{k}_{}", j + 1));
            if categorical {
                for c in 0..categories.len().max(1) {
                    for col in &cols {
                        names.push(format!("{}_{col}", category(c)));
                    }
                }
            } else {
                for col in cols {
                    names.push(format!("{prefix}{col}"));
                }
            }
        }
        let shape = structure.blocks.get(i);
        let by_levels: Option<Vec<String>> = shape.filter(|s| s.by).map(|_| {
            b.get("by_dimension")
                .and_then(Json::as_str)
                .map(labels_of)
                .unwrap_or_default()
        });
        let mut rows = Vec::new();
        let levels: Vec<(Option<usize>, String)> = match &by_levels {
            Some(l) => l
                .iter()
                .enumerate()
                .map(|(x, name)| (Some(x + 1), format!(":{name}")))
                .collect(),
            None => vec![(None, String::new())],
        };
        for (level, suffix) in &levels {
            for (m, name) in names.iter().enumerate() {
                let element: Vec<usize> = match level {
                    Some(l) => vec![m + 1, *l],
                    None => vec![m + 1],
                };
                if let Some(r) = d.row(&format!("sd({name}{suffix})"), &format!("sd_{k}"), &element)
                {
                    rows.push(r);
                }
            }
            if shape.is_some_and(|s| s.correlated) {
                for a in 0..names.len() {
                    for c in (a + 1)..names.len() {
                        let label = format!("cor({}{suffix},{}{suffix})", names[a], names[c]);
                        let element: Vec<usize> = match level {
                            Some(l) => vec![*l, c + 1, a + 1],
                            None => vec![c + 1, a + 1],
                        };
                        if let Some(r) = d.row(&label, &format!("Cor_{k}"), &element) {
                            rows.push(r);
                        }
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
    let mut rows: Vec<ParameterRow> = layout
        .get("scalars")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| d.row(&text(x, "label"), &text(x, "variable"), &[]))
                .collect()
        })
        .unwrap_or_default();
    if structure.rescor {
        let responses = texts(layout, "responses");
        for a in 0..responses.len() {
            for c in (a + 1)..responses.len() {
                let label = format!("rescor({},{})", responses[a], responses[c]);
                if let Some(r) = d.row(&label, "Rescor", &[c + 1, a + 1]) {
                    rows.push(r);
                }
            }
        }
    }
    if !rows.is_empty() {
        out.push(ParameterBlock::table(
            "Family-specific parameters",
            COLUMNS,
            rows,
        )?);
    }
    Ok(out)
}
