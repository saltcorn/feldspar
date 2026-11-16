//! What the model editor asks as the admin types (TODO-bayesian-regression §7,
//! R1.8): the formula checked against what is known of the dataset, with every
//! mistake located for an underline, the pasted prior calls lifted into rows,
//! the resolved prior table, the generated program (the `make_stancode`
//! equivalent) and the derived bindings (the `make_standata` equivalent).
//!
//! Unlike [`build`](crate::build), nothing here fails: a formula with a
//! mistake is the answer the editor is waiting for, so the mistakes are fields
//! of it, and everything that could still be worked out is answered beside
//! them.

use sc_model::{Frame, LowerInput};
use sc_types::Attrs;
use serde::Serialize;
use serde_json::Value as Json;

use crate::canon::{Structure, canonicalise, data_plan};
use crate::family::{FAMILIES, family};
use crate::generate::generate;
use crate::lower::{keys, rows, settings};
use crate::parse::{ParseError, parse_formula, strip_priors};
use crate::prior::{self, PriorFamily, PriorRow, PriorTable, merge_rows};
use crate::resolve::{Data, resolve};

/// The editor's answer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Check {
    /// The formula's mistakes, each with its span, line and columns.
    pub errors: Vec<ParseError>,
    /// A mistake that is not about one place in the text — a setting of the
    /// form, a prior row, something this release cannot generate.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// What the editor should say: a `bf()` that overrode a setting.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The formula with any pasted prior calls taken out — what the field
    /// holds once it is committed.
    pub formula: String,
    /// The explicit prior rows, with the pasted ones merged in.
    pub priors: Vec<PriorRow>,
    /// The rows a pasted call replaced, so the editor can name each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replaced: Vec<PriorRow>,
    /// How many prior calls were pasted.
    pub pasted: usize,
    /// Every parameter's resolved prior, and the rows that match nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<PriorTable>,
    /// Whether the table's data-dependent defaults are numbers (read from the
    /// rows) or still rules.
    pub from_data: bool,
    /// The generated program.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Its structure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<Structure>,
    /// The derived bindings, dimensions and policies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Json>,
    /// What the editor completes.
    pub completions: Completions,
}

/// The names the formula editor and the prior cells complete.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Completions {
    /// The dataset's columns.
    pub columns: Vec<String>,
    /// The family's distributional parameters (`mu` first).
    pub dpars: Vec<String>,
    /// The families.
    pub families: Vec<String>,
    /// The prior menu.
    pub priors: Vec<String>,
    /// The links each distributional parameter of the family takes, default
    /// first.
    pub links: std::collections::BTreeMap<String, Vec<String>>,
}

/// Check `config` against `input` (the dataset's shape while editing; the
/// rows after a data preview, which turns the default priors' rules into
/// numbers).
pub fn check(config: &Attrs, input: LowerInput<'_>) -> Check {
    let (data, frame): (Data<'_>, Option<&Frame>) = match &input {
        LowerInput::Shape { shape: Some(s), .. } => (Data::Shape(s), None),
        LowerInput::Shape { shape: None, .. } => (Data::Nothing, None),
        LowerInput::Frames { .. } => match input.main_frame() {
            Some(f) => (Data::Frame(f), Some(f)),
            None => (Data::Nothing, None),
        },
    };
    let mut out = Check {
        completions: completions(config, &data),
        from_data: frame.is_some(),
        ..Check::default()
    };
    let text = config
        .get(keys::FORMULA)
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    out.formula.clone_from(&text);
    let existing = match rows(config) {
        Ok(r) => r,
        Err(e) => {
            out.problems.push(e.to_string());
            Vec::new()
        }
    };
    out.priors.clone_from(&existing);
    if text.trim().is_empty() {
        out.problems
            .push("write the model's formula: `response ~ predictors`".to_owned());
        return out;
    }
    let settings = match settings(config) {
        Ok(s) => s,
        Err(e) => {
            out.problems.push(e.to_string());
            return out;
        }
    };
    let formula = match parse_formula(&text) {
        Ok(f) => f,
        Err(e) => {
            out.errors = e.0;
            return out;
        }
    };
    let pasted: Vec<PriorRow> = formula.priors.iter().map(|p| p.row.clone()).collect();
    out.pasted = pasted.len();
    if !pasted.is_empty() {
        let (merged, replaced) = merge_rows(&existing, &pasted);
        out.priors = merged;
        out.replaced = replaced;
        out.formula = strip_priors(&text, &formula);
    }
    out.notes.clone_from(&formula.overrides.notes);
    let resolved = match resolve(&text, &formula, &settings, data) {
        Ok(r) => r,
        Err(e) => {
            out.errors = e.0;
            return out;
        }
    };
    out.completions.dpars = resolved
        .family
        .dpar_names()
        .into_iter()
        .map(str::to_owned)
        .collect();

    // The table, even when the program cannot be generated: the admin is
    // fixing one or the other.
    let names = match frame.map(|f| resolved.names(f)).transpose() {
        Ok(n) => n,
        Err(e) => {
            out.problems.push(e.to_string());
            None
        }
    };
    let scale = frame.map(|f| resolved.data_scale(f));
    let slots = resolved.prior_slots(names.as_ref());
    out.table = Some(prior::resolve(
        &slots,
        &out.priors,
        scale,
        &resolved.response,
        resolved.center,
    ));

    match canonicalise(&resolved, &out.priors) {
        Ok(canonical) => {
            let taken: Vec<String> = input
                .model()
                .related
                .iter()
                .map(|r| r.name.clone())
                .collect();
            let plan = data_plan(&resolved, &canonical, &taken);
            let (bindings, dimensions, policies) = plan.to_json();
            out.data = Some(serde_json::json!({
                "bindings": bindings,
                "dimensions": dimensions,
                "policies": policies,
            }));
            out.program = Some(generate(&canonical.structure));
            out.structure = Some(canonical.structure);
        }
        Err(e) => out.problems.push(e),
    }
    if let Some(f) = frame {
        if let Err(e) = resolved.check_response(f) {
            out.problems.push(e.to_string());
        }
    }
    out
}

fn completions(config: &Attrs, data: &Data<'_>) -> Completions {
    let columns = match data {
        Data::Shape(s) => s.columns.iter().map(|c| c.name.clone()).collect(),
        Data::Frame(f) => f.columns.iter().map(|(n, _)| n.clone()).collect(),
        Data::Names(n) => n.to_vec(),
        Data::Nothing => Vec::new(),
    };
    let fam = config
        .get(keys::FAMILY)
        .and_then(Json::as_str)
        .and_then(|f| family(f).ok())
        .unwrap_or(&FAMILIES[0]);
    Completions {
        columns,
        dpars: fam.dpar_names().into_iter().map(str::to_owned).collect(),
        families: FAMILIES.iter().map(|f| f.name.to_owned()).collect(),
        priors: PriorFamily::names()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        links: fam
            .dpar_names()
            .into_iter()
            .filter_map(|d| {
                fam.links(d).map(|l| {
                    (
                        d.to_owned(),
                        l.iter().map(|x| x.name().to_owned()).collect(),
                    )
                })
            })
            .collect(),
    }
}
