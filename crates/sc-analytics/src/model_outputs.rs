//! A fit's outputs, ready to show (analytics TODO A3.1–A3.3): each table
//! filled from the instance, and each plot's spec pointed at the fit's output
//! data and drawn with [`render_plot`].
//!
//! What a fit shows is declared by its provider and recorded on the instance
//! (`sc_model::OutputDecl`); this is the half that reads the declarations
//! back. Optional plots — the model editor's "More plots" — come with their
//! spec but are drawn only when asked for, so opening a model draws what is
//! on the screen and nothing else.

use std::collections::BTreeSet;

use sc_catalog::Catalog;
use sc_error::Result;
use sc_model::{
    InstanceId, ModelInstance, OutputDecl, OutputKind, ParameterBlock, SCALARS, SplitMetrics,
};
use serde::Serialize;
use serde_json::{Map, Value as Json};
use uuid::Uuid;

use crate::plot::{DataRef, PlotSpec, Rendered, render_plot};

/// The most rows a table of output data shows.
pub const MAX_TABLE_ROWS: usize = 1_000;

/// One output of a fit, as the model editor shows it.
#[derive(Debug, Clone, Serialize)]
pub struct OutputView {
    /// Its key.
    pub name: String,
    /// What it is called.
    pub label: String,
    /// Not shown until asked for.
    pub optional: bool,
    /// `table` or `plot`.
    pub kind: &'static str,
    /// A table's contents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<OutputTable>,
    /// A plot's spec, its data pointed at the fit: what a later `renderPlot`
    /// draws, and what a report will copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<PlotSpec>,
    /// A plot's data, when it was drawn: every plot that is not optional, and
    /// the optional ones asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plot: Option<Rendered>,
    /// Why it cannot be shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A table output's contents.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputTable {
    /// Its columns.
    pub columns: Vec<String>,
    /// Its rows, one cell per column.
    pub rows: Vec<Vec<Json>>,
    /// A parameter block of text: the provider's own summary, shown as it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether the rows are some of more.
    pub truncated: bool,
}

/// `spec` (a plot spec without its data) over the output data `data` of
/// `instance`, or why it is not a plot spec.
pub fn output_spec(
    instance: InstanceId,
    data: &str,
    spec: &Json,
) -> std::result::Result<PlotSpec, String> {
    let mut spec = match spec {
        Json::Object(map) => map.clone(),
        _ => return Err("its plot spec is not an object".to_owned()),
    };
    spec.insert(
        "data".to_owned(),
        serde_json::to_value(DataRef::FitOutput {
            instance: instance.0,
            name: data.to_owned(),
        })
        .map_err(|e| e.to_string())?,
    );
    serde_json::from_value(Json::Object(spec))
        .map_err(|e| format!("its plot spec does not read: {e}"))
}

/// Every output `instance` recorded, in order: tables filled, plot specs made,
/// and the plots drawn that are not optional or are named in `include`.
pub async fn render_outputs(
    catalog: &Catalog,
    instance: &ModelInstance,
    include: &BTreeSet<String>,
) -> Result<Vec<OutputView>> {
    let mut out = Vec::new();
    for decl in sc_model::instance_outputs(instance)? {
        out.push(render_output(catalog, instance, &decl, include).await?);
    }
    Ok(out)
}

/// The output of `instance` called `name`, drawn whether it is optional or
/// not; `None` when the fit recorded no such output. What a panel copied from
/// the model editor (A4.3) shows.
pub async fn render_one_output(
    catalog: &Catalog,
    instance: &ModelInstance,
    name: &str,
) -> Result<Option<OutputView>> {
    let Some(decl) = sc_model::instance_outputs(instance)?
        .into_iter()
        .find(|d| d.name == name)
    else {
        return Ok(None);
    };
    let include = BTreeSet::from([decl.name.clone()]);
    render_output(catalog, instance, &decl, &include)
        .await
        .map(Some)
}

async fn render_output(
    catalog: &Catalog,
    instance: &ModelInstance,
    decl: &OutputDecl,
    include: &BTreeSet<String>,
) -> Result<OutputView> {
    let mut view = OutputView {
        name: decl.name.clone(),
        label: decl.label.clone(),
        optional: decl.optional,
        kind: "table",
        table: None,
        spec: None,
        plot: None,
        error: None,
    };
    match &decl.kind {
        OutputKind::Parameters { block } => match parameters_table(&instance.parameters, block) {
            Some(table) => view.table = Some(table),
            None => {
                view.error = Some(format!("this fit has no parameter block `{block}`"));
            }
        },
        OutputKind::Metrics => match metrics_table(&instance.metrics) {
            Some(table) => view.table = Some(table),
            None => view.error = Some("this fit has no metrics".to_owned()),
        },
        OutputKind::Table { data } => {
            match sc_model::load_output_data(catalog, instance.id, data).await? {
                Some(frame) => view.table = Some(frame_table(&frame.frame)),
                None => view.error = Some(format!("this fit has no output data `{data}`")),
            }
        }
        OutputKind::Plot { data, spec } => {
            view.kind = "plot";
            match output_spec(instance.id, data, spec) {
                Ok(spec) => {
                    if !decl.optional || include.contains(&decl.name) {
                        view.plot = Some(render_plot(catalog, &spec).await?);
                    }
                    view.spec = Some(spec);
                }
                Err(e) => view.error = Some(e),
            }
        }
    }
    Ok(view)
}

/// The parameter block `block` as a table — or, for [`SCALARS`], every scalar
/// as a row of name and value.
pub fn parameters_table(parameters: &[ParameterBlock], block: &str) -> Option<OutputTable> {
    if block == SCALARS {
        let rows: Vec<Vec<Json>> = parameters
            .iter()
            .filter_map(|p| match p {
                ParameterBlock::Scalar { name, value } => {
                    Some(vec![Json::from(name.clone()), number(*value)])
                }
                _ => None,
            })
            .collect();
        return (!rows.is_empty()).then(|| OutputTable {
            columns: vec!["statistic".to_owned(), "value".to_owned()],
            rows,
            text: None,
            truncated: false,
        });
    }
    parameters
        .iter()
        .find(|p| p.name() == block)
        .map(|p| match p {
            ParameterBlock::Table { columns, rows, .. } => OutputTable {
                columns: columns.clone(),
                rows: rows.iter().map(|r| r.cells.clone()).collect(),
                text: None,
                truncated: false,
            },
            ParameterBlock::Scalar { name, value } => OutputTable {
                columns: vec!["statistic".to_owned(), "value".to_owned()],
                rows: vec![vec![Json::from(name.clone()), number(*value)]],
                text: None,
                truncated: false,
            },
            ParameterBlock::Text { body, .. } => OutputTable {
                columns: Vec::new(),
                rows: Vec::new(),
                text: Some(body.clone()),
                truncated: false,
            },
        })
}

/// The metrics as a table: a row per number a split's metrics have, a column
/// per split that has any.
pub fn metrics_table(metrics: &Json) -> Option<OutputTable> {
    let split = SplitMetrics::from_json(metrics).ok()?;
    let parts: Vec<(&str, Map<String, Json>)> = [
        ("train", &split.train),
        ("validation", &split.validation),
        ("test", &split.test),
    ]
    .into_iter()
    .filter_map(|(name, m)| {
        let json = serde_json::to_value(m.as_ref()?).ok()?;
        match json {
            Json::Object(map) => Some((name, map)),
            _ => None,
        }
    })
    .collect();
    let mut names: Vec<String> = Vec::new();
    for (_, map) in &parts {
        for (key, value) in map {
            if (value.is_number() || value.is_null()) && key != "metrics" && !names.contains(key) {
                names.push(key.clone());
            }
        }
    }
    if names.is_empty() {
        return None;
    }
    let mut columns = vec!["metric".to_owned()];
    columns.extend(parts.iter().map(|(n, _)| (*n).to_owned()));
    let rows = names
        .iter()
        .map(|name| {
            let mut row = vec![Json::from(name.clone())];
            row.extend(
                parts
                    .iter()
                    .map(|(_, map)| map.get(name).cloned().unwrap_or(Json::Null)),
            );
            row
        })
        .collect();
    Some(OutputTable {
        columns,
        rows,
        text: None,
        truncated: false,
    })
}

/// An output frame as a table, at most [`MAX_TABLE_ROWS`] rows of it.
fn frame_table(frame: &sc_model::Frame) -> OutputTable {
    let rows = frame.to_rows();
    let truncated = rows.len() > MAX_TABLE_ROWS;
    let names: Vec<String> = frame.names().into_iter().map(str::to_owned).collect();
    OutputTable {
        rows: rows
            .into_iter()
            .take(MAX_TABLE_ROWS)
            .map(|row| {
                names
                    .iter()
                    .map(|n| row.get(n).cloned().unwrap_or(Json::Null))
                    .collect()
            })
            .collect(),
        columns: names,
        text: None,
        truncated,
    }
}

fn number(x: f64) -> Json {
    serde_json::Number::from_f64(x).map_or(Json::Null, Json::Number)
}

/// The instance id a fit-output spec reads, if it reads one.
pub fn spec_instance(spec: &PlotSpec) -> Option<Uuid> {
    match &spec.data {
        DataRef::FitOutput { instance, .. } => Some(*instance),
        DataRef::Dataset { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::ParameterRow;
    use serde_json::json;

    #[test]
    fn a_spec_is_pointed_at_the_fit_whatever_data_it_said() {
        let id = InstanceId::new();
        let spec = output_spec(
            id,
            "rows",
            &json!({ "data": { "kind": "dataset", "dataset": Uuid::nil() },
                     "layers": [ { "mark": "point" } ] }),
        )
        .unwrap();
        assert_eq!(
            spec.data,
            DataRef::FitOutput {
                instance: id.0,
                name: "rows".to_owned()
            }
        );
        assert_eq!(spec_instance(&spec), Some(id.0));
        assert!(output_spec(id, "rows", &json!([])).is_err());
        let err = output_spec(id, "rows", &json!({ "layers": "none" })).unwrap_err();
        assert!(err.contains("does not read"), "{err}");
    }

    #[test]
    fn parameter_tables_are_read_from_the_blocks() {
        let blocks = vec![
            ParameterBlock::table(
                "Coefficients",
                ["term", "estimate"],
                vec![ParameterRow::new([json!("area"), json!(2.0)])],
            )
            .unwrap(),
            ParameterBlock::scalar("R²", 0.5),
            ParameterBlock::scalar("observations", 10.0),
            ParameterBlock::text("Summary", "all good"),
        ];
        let table = parameters_table(&blocks, "Coefficients").unwrap();
        assert_eq!(table.rows, [[json!("area"), json!(2.0)]]);
        let scalars = parameters_table(&blocks, SCALARS).unwrap();
        assert_eq!(scalars.rows.len(), 2);
        assert_eq!(scalars.rows[0], [json!("R²"), json!(0.5)]);
        assert_eq!(
            parameters_table(&blocks, "Summary")
                .unwrap()
                .text
                .as_deref(),
            Some("all good")
        );
        assert!(parameters_table(&blocks, "Nothing").is_none());
    }

    #[test]
    fn metrics_become_a_row_per_number_and_a_column_per_split() {
        let metrics = json!({
            "train": { "metrics": "regression", "r2": 0.9, "rmse": 1.0, "mae": 0.5, "rows": 80 },
            "test": { "metrics": "regression", "r2": 0.8, "rmse": 1.5, "mae": 0.7, "rows": 20 }
        });
        let table = metrics_table(&metrics).unwrap();
        assert_eq!(table.columns, ["metric", "train", "test"]);
        let r2 = table.rows.iter().find(|r| r[0] == "r2").unwrap();
        assert_eq!(r2[1..], [json!(0.9), json!(0.8)]);
        assert!(metrics_table(&Json::Null).is_none());
    }
}
