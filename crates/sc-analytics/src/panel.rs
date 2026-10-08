//! Panels (analytics TODO A4.2; the goals document's "Panels").
//!
//! A **panel** is an elementary output — a plot, a summary table, the
//! explorer's hypothesis tests beside their plot, a block of text, a table of
//! a fit's outputs, or a plugin's own kind — that can be dragged from where it
//! was made into a report (A4) or a dashboard (A6). It is stored as what makes
//! it, never as what it drew: a plot panel is its plot spec, so a panel
//! **renders live** from its dataset every time it is shown, and dragging one
//! copies the spec.
//!
//! On the wire and in a workspace's state a panel is
//! `{ "id", "title"?, "kind", "content" }`, `content` being the kind's own:
//!
//! | kind | content |
//! |---|---|
//! | `plot` | `{ spec }` — a [`PlotSpec`] |
//! | `summary_table` | `{ spec }` — a [`TableSpec`] |
//! | `test_result` | `{ tests, plot? }` — a [`TestSpec`] and the plot beside it |
//! | `text` | `{ markdown }` |
//! | `fit_table` | `{ fit, output }` — a table output of a model fit (A3.2) |
//! | `custom` | `{ renderer, config }` — a plugin's panel kind |
//!
//! `fit_table` is a sixth kind beside the plan's five: a coefficient table is
//! not a summary table of a dataset but a table a fit recorded, so it names
//! the fit and the output rather than a spec.
//!
//! Panels live in workspaces' states, which this crate otherwise does not look
//! into. [`panels_in_state`] is the one place that knows where each kind keeps
//! them, so the [`UsageIndex`] can answer "what uses this dataset" for the
//! delete warning, and [`check_state`] can refuse a state whose panels do not
//! read. A panel whose dataset or fit is gone renders as a sentence saying so
//! ([`render_panel`]), never as a failure.

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::Catalog;
use sc_dataset::DatasetId;
use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::model_outputs::{OutputView, render_one_output};
use crate::plot::render::plot_rows;
use crate::plot::{
    DataRef, PlotSpec, Rendered, RenderedTable, TableSpec, render_plot, render_table,
};
use crate::stats::{TestSpec, TestsAnswer, run_tests};
use crate::workspace::{Workspace, WorkspaceId, WorkspaceKind, list_workspaces};

/// The longest a text panel may be, in bytes.
pub const MAX_TEXT: usize = 100_000;

/// One panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    /// Its identity within the workspace that holds it. A copy has a new one.
    pub id: Uuid,
    /// A caption shown above it, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Its kind and the kind's content.
    #[serde(flatten)]
    pub body: PanelBody,
}

/// A panel's kind and what makes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
pub enum PanelBody {
    /// A plot, drawn from its spec.
    Plot {
        /// The plot.
        spec: PlotSpec,
    },
    /// A summary table, made from its spec.
    SummaryTable {
        /// The table.
        spec: TableSpec,
    },
    /// The explorer's hypothesis tests, and the plot they sit beside: one
    /// panel, so they are dragged together (goals document, "Hypothesis tests
    /// in the data explorer").
    TestResult {
        /// The roles the tests are chosen from.
        tests: TestSpec,
        /// The plot beside them, if there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plot: Option<PlotSpec>,
    },
    /// Text, in Markdown.
    Text {
        /// The text.
        markdown: String,
    },
    /// A table output of a model fit — a coefficient table, cluster sizes, a
    /// posterior summary.
    FitTable {
        /// The fit (a model instance).
        fit: Uuid,
        /// The output's name, as the provider declared it.
        output: String,
    },
    /// A panel kind a plugin provides, drawn by its own renderer.
    Custom {
        /// The renderer's name.
        renderer: String,
        /// The renderer's own configuration.
        #[serde(default)]
        config: Json,
    },
}

impl PanelBody {
    /// The kind's name as it is stored and sent.
    pub fn kind(&self) -> &'static str {
        match self {
            PanelBody::Plot { .. } => "plot",
            PanelBody::SummaryTable { .. } => "summary_table",
            PanelBody::TestResult { .. } => "test_result",
            PanelBody::Text { .. } => "text",
            PanelBody::FitTable { .. } => "fit_table",
            PanelBody::Custom { .. } => "custom",
        }
    }

    /// Every place it reads rows from.
    fn data(&self) -> Vec<&DataRef> {
        match self {
            PanelBody::Plot { spec } => vec![&spec.data],
            PanelBody::SummaryTable { spec } => vec![&spec.data],
            PanelBody::TestResult { tests, plot } => std::iter::once(&tests.data)
                .chain(plot.iter().map(|p| &p.data))
                .collect(),
            PanelBody::Text { .. } | PanelBody::FitTable { .. } | PanelBody::Custom { .. } => {
                vec![]
            }
        }
    }
}

impl Panel {
    /// A new panel.
    pub fn new(body: PanelBody) -> Panel {
        Panel {
            id: Uuid::new_v4(),
            title: None,
            body,
        }
    }

    /// The panel read from JSON, or a sentence saying why it is not one.
    pub fn from_json(value: &Json) -> Result<Panel> {
        let panel: Panel = serde_json::from_value(value.clone())
            .map_err(|e| Error::invalid(format!("this is not a panel: {e}")))?;
        panel.check()?;
        Ok(panel)
    }

    /// A copy with an identity of its own: what a drop makes, since a drag
    /// always copies and never moves.
    #[must_use]
    pub fn copied(&self) -> Panel {
        Panel {
            id: Uuid::new_v4(),
            ..self.clone()
        }
    }

    /// The datasets it reads.
    pub fn datasets(&self) -> BTreeSet<DatasetId> {
        self.body
            .data()
            .into_iter()
            .filter_map(|d| match d {
                DataRef::Dataset { dataset } => Some(*dataset),
                DataRef::FitOutput { .. } => None,
            })
            .collect()
    }

    /// The fits it reads: a fit's output data under a plot, or a fit's table.
    pub fn fits(&self) -> BTreeSet<Uuid> {
        let mut fits: BTreeSet<Uuid> = self
            .body
            .data()
            .into_iter()
            .filter_map(|d| match d {
                DataRef::FitOutput { instance, .. } => Some(*instance),
                DataRef::Dataset { .. } => None,
            })
            .collect();
        if let PanelBody::FitTable { fit, .. } = &self.body {
            fits.insert(*fit);
        }
        fits
    }

    /// What is wrong with it on its own, before anything is read.
    pub fn check(&self) -> Result<()> {
        match &self.body {
            PanelBody::Text { markdown } if markdown.len() > MAX_TEXT => Err(Error::invalid(
                format!("a text panel holds at most {MAX_TEXT} bytes"),
            )),
            PanelBody::FitTable { output, .. } if output.trim().is_empty() => Err(Error::invalid(
                "a fit's table panel names the output it shows",
            )),
            PanelBody::Custom { renderer, .. } if renderer.trim().is_empty() => Err(
                Error::invalid("a custom panel names the renderer that draws it"),
            ),
            _ => Ok(()),
        }
    }
}

// --- where a workspace keeps its panels -----------------------------------------

/// The panels in a workspace's state, by where its kind keeps them: a report's
/// are the `panel` of each of its `blocks` whose `kind` is `panel` (A4.3–A4.4).
/// The kinds that hold no panels yet answer none. Anything in the place a
/// panel goes that is not one is reported by [`check_state`], and skipped
/// here.
pub fn panels_in_state(kind: WorkspaceKind, state: &Json) -> Vec<Panel> {
    panel_slots(kind, state)
        .into_iter()
        .filter_map(|v| Panel::from_json(v).ok())
        .collect()
}

/// The JSON values in the places `kind` keeps its panels.
fn panel_slots(kind: WorkspaceKind, state: &Json) -> Vec<&Json> {
    match kind {
        WorkspaceKind::Report => state
            .get("blocks")
            .and_then(Json::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("kind").and_then(Json::as_str) == Some("panel"))
                    .filter_map(|b| b.get("panel"))
                    .collect()
            })
            .unwrap_or_default(),
        WorkspaceKind::DataExplorer
        | WorkspaceKind::Dashboard
        | WorkspaceKind::Notebook
        | WorkspaceKind::Map
        | WorkspaceKind::Simulation => vec![],
    }
}

/// Refuse a state whose panels do not read, naming the first that does not,
/// and a report whose other blocks or page are not ones it has.
pub fn check_state(kind: WorkspaceKind, state: &Json) -> Result<()> {
    for (i, slot) in panel_slots(kind, state).into_iter().enumerate() {
        Panel::from_json(slot)
            .map_err(|e| Error::invalid(format!("panel {} of the workspace: {e}", i + 1)))?;
    }
    if kind == WorkspaceKind::Report {
        check_report(state)?;
    }
    Ok(())
}

/// The longest a report heading may be, in bytes.
pub const MAX_HEADING: usize = 1_000;

/// The page sizes a report can be printed on (A4.4).
pub const PAGE_SIZES: [&str; 4] = ["A4", "A3", "Letter", "Legal"];

/// A report's blocks other than its panels (A4.4), and its page: a block is
/// `{ id, kind }` with `kind` one of `panel`, `heading` (`text`, `level`
/// 1–3), `text` (`markdown`) and `page_break`; the page is `{ size,
/// orientation }`.
fn check_report(state: &Json) -> Result<()> {
    let blocks = match state.get("blocks") {
        None => &[][..],
        Some(Json::Array(blocks)) => &blocks[..],
        Some(_) => return Err(Error::invalid("a report's blocks are a list")),
    };
    for (i, block) in blocks.iter().enumerate() {
        let refuse = |why: &str| Error::invalid(format!("block {} of the report {why}", i + 1));
        if block
            .get("id")
            .and_then(Json::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(refuse("has no id"));
        }
        let text = |field: &str| block.get(field).and_then(Json::as_str);
        match block.get("kind").and_then(Json::as_str) {
            Some("panel") | Some("page_break") => {}
            Some("heading") => match text("text") {
                None => return Err(refuse("is a heading without text")),
                Some(t) if t.len() > MAX_HEADING => {
                    return Err(refuse(&format!(
                        "is a heading longer than {MAX_HEADING} bytes"
                    )));
                }
                Some(_) => {
                    if !matches!(block.get("level").and_then(Json::as_u64), Some(1..=3)) {
                        return Err(refuse("is a heading whose level is not 1, 2 or 3"));
                    }
                }
            },
            Some("text") => match text("markdown") {
                None => return Err(refuse("is a text block without its Markdown")),
                Some(t) if t.len() > MAX_TEXT => {
                    return Err(refuse(&format!("holds more than {MAX_TEXT} bytes of text")));
                }
                Some(_) => {}
            },
            Some(other) => {
                return Err(refuse(&format!(
                    "is a \"{other}\", which is not a kind of block: a report has panels, headings, text and page breaks"
                )));
            }
            None => return Err(refuse("does not say what kind of block it is")),
        }
    }
    if let Some(page) = state.get("page") {
        let size = page.get("size").and_then(Json::as_str).unwrap_or_default();
        if !PAGE_SIZES.contains(&size) {
            return Err(Error::invalid(format!(
                "a report's page size is one of {}, not \"{size}\"",
                PAGE_SIZES.join(", ")
            )));
        }
        let orientation = page.get("orientation").and_then(Json::as_str);
        if !matches!(orientation, Some("portrait") | Some("landscape")) {
            return Err(Error::invalid(
                "a report's page is either portrait or landscape",
            ));
        }
    }
    Ok(())
}

/// The dataset a kind's state reads directly, outside any panel: the Data
/// explorer's chosen dataset.
fn state_dataset(kind: WorkspaceKind, state: &Json) -> Option<DatasetId> {
    match kind {
        WorkspaceKind::DataExplorer => state
            .get("dataset")
            .and_then(Json::as_str)
            .and_then(|s| s.parse().ok()),
        _ => None,
    }
}

// --- the usage index -------------------------------------------------------------

/// A workspace that uses a dataset or a fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceUse {
    /// The workspace.
    pub id: WorkspaceId,
    /// Its name.
    pub name: String,
    /// Its kind.
    pub kind: WorkspaceKind,
    /// How many of its panels read the thing: none for an explorer that only
    /// has it chosen.
    pub panels: usize,
}

/// What uses what: for each dataset and each fit, the workspaces whose state
/// reads it, through a panel or (the explorer) directly. Built from the stored
/// states when it is asked, so it is never out of date; there are a handful of
/// workspaces, not thousands.
#[derive(Debug, Clone, Default)]
pub struct UsageIndex {
    datasets: BTreeMap<DatasetId, Vec<WorkspaceUse>>,
    fits: BTreeMap<Uuid, Vec<WorkspaceUse>>,
}

impl UsageIndex {
    /// The index of every stored workspace.
    pub async fn build(catalog: &Catalog) -> Result<UsageIndex> {
        Ok(UsageIndex::of_workspaces(&list_workspaces(catalog).await?))
    }

    /// The index of these workspaces.
    pub fn of_workspaces(workspaces: &[Workspace]) -> UsageIndex {
        let mut index = UsageIndex::default();
        for ws in workspaces {
            let panels = panels_in_state(ws.kind, &ws.state);
            let mut datasets: BTreeMap<DatasetId, usize> = BTreeMap::new();
            let mut fits: BTreeMap<Uuid, usize> = BTreeMap::new();
            if let Some(d) = state_dataset(ws.kind, &ws.state) {
                datasets.entry(d).or_default();
            }
            for panel in &panels {
                for d in panel.datasets() {
                    *datasets.entry(d).or_default() += 1;
                }
                for f in panel.fits() {
                    *fits.entry(f).or_default() += 1;
                }
            }
            let entry = |panels: usize| WorkspaceUse {
                id: ws.id,
                name: ws.name.clone(),
                kind: ws.kind,
                panels,
            };
            for (d, n) in datasets {
                index.datasets.entry(d).or_default().push(entry(n));
            }
            for (f, n) in fits {
                index.fits.entry(f).or_default().push(entry(n));
            }
        }
        for uses in index.datasets.values_mut().chain(index.fits.values_mut()) {
            uses.sort_by(|a, b| a.name.cmp(&b.name));
        }
        index
    }

    /// The workspaces that use the dataset.
    pub fn dataset(&self, id: DatasetId) -> &[WorkspaceUse] {
        self.datasets.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The workspaces that use any of these fits — a model's, for its delete
    /// warning — each once, its panels counted over all of them.
    pub fn fits(&self, ids: impl IntoIterator<Item = Uuid>) -> Vec<WorkspaceUse> {
        let mut out: Vec<WorkspaceUse> = Vec::new();
        for id in ids {
            for found in self.fits.get(&id).map_or(&[][..], Vec::as_slice) {
                match out.iter_mut().find(|u| u.id == found.id) {
                    Some(seen) => seen.panels += found.panels,
                    None => out.push(found.clone()),
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

// --- rendering --------------------------------------------------------------------

/// What a panel shows, made from what it is stored as, now.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RenderedPanel {
    /// The panel's kind.
    pub kind: &'static str,
    /// Why it shows nothing: what it reads is gone, or its kind cannot be
    /// drawn here. A plot that cannot be drawn says so in `plot` instead,
    /// as `renderPlot` does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A plot's data, or the plot beside a test result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plot: Option<Rendered>,
    /// A summary table's data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<RenderedTable>,
    /// A test result's tests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestsAnswer>,
    /// A fit's table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputView>,
    /// The columns a plot reads that are categories though their values are
    /// numbers — foreign keys — so the browser draws their ids as the
    /// explorer does, as values rather than a scale.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categorical: Vec<String>,
}

/// The foreign key columns of the rows `data` names.
async fn categorical(catalog: &Catalog, data: &DataRef) -> Result<Vec<String>> {
    Ok(match plot_rows(catalog, data).await? {
        Ok(rows) => rows
            .shape
            .columns
            .iter()
            .filter(|c| c.key.is_some())
            .map(|c| c.name.clone())
            .collect(),
        Err(_) => vec![],
    })
}

/// The sentence for the first thing `panel` reads that is gone, if any.
pub async fn missing_reference(catalog: &Catalog, panel: &Panel) -> Result<Option<String>> {
    for id in panel.datasets() {
        if sc_dataset::load_dataset(catalog, id).await?.is_none() {
            return Ok(Some(
                "The dataset this panel shows has been deleted.".to_owned(),
            ));
        }
    }
    for id in panel.fits() {
        if sc_model::load_model_instance(catalog, sc_model::InstanceId(id))
            .await?
            .is_none()
        {
            return Ok(Some(
                "The model fit this panel shows has been deleted.".to_owned(),
            ));
        }
    }
    Ok(None)
}

/// Draw `panel` from its datasets and fits as they are now. A panel whose
/// dataset or fit is gone answers the sentence saying so in `error`.
pub async fn render_panel(catalog: &Catalog, panel: &Panel) -> Result<RenderedPanel> {
    panel.check()?;
    let mut out = RenderedPanel {
        kind: panel.body.kind(),
        ..RenderedPanel::default()
    };
    if let Some(sentence) = missing_reference(catalog, panel).await? {
        out.error = Some(sentence);
        return Ok(out);
    }
    match &panel.body {
        PanelBody::Plot { spec } => {
            out.plot = Some(render_plot(catalog, spec).await?);
            out.categorical = categorical(catalog, &spec.data).await?;
        }
        PanelBody::SummaryTable { spec } => out.table = Some(render_table(catalog, spec).await?),
        PanelBody::TestResult { tests, plot } => {
            out.tests = Some(run_tests(catalog, tests).await?);
            if let Some(spec) = plot {
                out.plot = Some(render_plot(catalog, spec).await?);
                out.categorical = categorical(catalog, &spec.data).await?;
            }
        }
        // The browser renders the Markdown.
        PanelBody::Text { .. } => {}
        PanelBody::FitTable { fit, output } => {
            let instance =
                sc_model::require_model_instance(catalog, sc_model::InstanceId(*fit)).await?;
            match render_one_output(catalog, &instance, output).await? {
                Some(view) if view.kind == "table" => out.output = Some(view),
                Some(_) => {
                    out.error = Some(format!(
                        "The output `{output}` of this fit is a plot, not a table."
                    ));
                }
                None => {
                    out.error = Some(format!("This fit has no output `{output}`."));
                }
            }
        }
        PanelBody::Custom { renderer, .. } => {
            out.error = Some(format!(
                "This panel is drawn by `{renderer}`, which is not installed."
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dataset_plot(dataset: DatasetId) -> Json {
        json!({
            "id": Uuid::new_v4(),
            "kind": "plot",
            "content": { "spec": {
                "data": { "kind": "dataset", "dataset": dataset },
                "layers": [{ "mark": "point", "encoding": {
                    "x": { "field": "area" }, "y": { "field": "price" } } }]
            } }
        })
    }

    #[test]
    fn every_kind_round_trips_as_id_kind_and_content() {
        let d = DatasetId::new();
        let fit = Uuid::new_v4();
        let spec = json!({
            "data": { "kind": "dataset", "dataset": d },
            "layers": [{ "mark": "bar", "stat": { "kind": "count" },
                         "encoding": { "x": { "field": "neighbourhood" } } }]
        });
        let kinds = [
            ("plot", json!({ "spec": spec })),
            (
                "summary_table",
                json!({ "spec": { "data": { "kind": "dataset", "dataset": d },
                                  "rows": [{ "field": "neighbourhood" }],
                                  "cells": [{ "field": "price", "function": "mean" }] } }),
            ),
            (
                "test_result",
                json!({ "tests": { "data": { "kind": "dataset", "dataset": d },
                                   "y": [{ "field": "price" }], "x": { "field": "neighbourhood" } },
                        "plot": spec }),
            ),
            ("text", json!({ "markdown": "# Houses" })),
            ("fit_table", json!({ "fit": fit, "output": "coefficients" })),
            (
                "custom",
                json!({ "renderer": "gauge", "config": { "max": 10 } }),
            ),
        ];
        for (kind, content) in kinds {
            let raw =
                json!({ "id": Uuid::new_v4(), "title": "T", "kind": kind, "content": content });
            let panel = Panel::from_json(&raw).unwrap_or_else(|e| panic!("{kind}: {e}"));
            assert_eq!(panel.body.kind(), kind);
            let back = serde_json::to_value(&panel).expect("serialises");
            assert_eq!(back["kind"], json!(kind));
            assert_eq!(Panel::from_json(&back).expect("reads back"), panel);
        }
        let err = Panel::from_json(&json!({ "id": Uuid::new_v4(), "kind": "pie", "content": {} }))
            .expect_err("no such kind");
        assert!(err.to_string().contains("not a panel"), "{err}");
    }

    #[test]
    fn a_panel_names_what_it_reads_and_a_copy_is_a_new_panel() {
        let d = DatasetId::new();
        let panel = Panel::from_json(&dataset_plot(d)).expect("panel");
        assert_eq!(panel.datasets(), BTreeSet::from([d]));
        assert!(panel.fits().is_empty());
        let copy = panel.copied();
        assert_ne!(copy.id, panel.id);
        assert_eq!(copy.body, panel.body);

        let fit = Uuid::new_v4();
        let residuals = Panel::from_json(&json!({
            "id": Uuid::new_v4(), "kind": "plot",
            "content": { "spec": {
                "data": { "kind": "fit_output", "instance": fit, "name": "rows" },
                "layers": [{ "mark": "point", "encoding": {
                    "x": { "field": "fitted" }, "y": { "field": "residual" } } }]
            } }
        }))
        .expect("panel");
        assert!(residuals.datasets().is_empty());
        assert_eq!(residuals.fits(), BTreeSet::from([fit]));
        let table = Panel::new(PanelBody::FitTable {
            fit,
            output: "coefficients".into(),
        });
        assert_eq!(table.fits(), BTreeSet::from([fit]));
    }

    #[test]
    fn the_usage_index_finds_reports_through_their_panels_and_explorers_by_their_dataset() {
        let houses = DatasetId::new();
        let other = DatasetId::new();
        let fit = Uuid::new_v4();
        let mut report = Workspace::new("Quarterly", WorkspaceKind::Report, None);
        report.state = json!({ "blocks": [
            { "id": "a", "kind": "heading", "text": "Houses" },
            { "id": "b", "kind": "panel", "panel": dataset_plot(houses) },
            { "id": "c", "kind": "panel", "panel": dataset_plot(houses) },
            { "id": "d", "kind": "panel", "panel": {
                "id": Uuid::new_v4(), "kind": "fit_table",
                "content": { "fit": fit, "output": "coefficients" } } },
        ] });
        let mut explorer = Workspace::new("Explore", WorkspaceKind::DataExplorer, None);
        explorer.state = json!({ "dataset": houses.to_string(), "assignment": {} });
        // A map's state is not read for panels until A5.
        let mut map = Workspace::new("Map", WorkspaceKind::Map, None);
        map.state = json!({ "blocks": [{ "kind": "panel", "panel": dataset_plot(houses) }] });

        let index = UsageIndex::of_workspaces(&[report.clone(), explorer.clone(), map]);
        let uses = index.dataset(houses);
        assert_eq!(
            uses.iter()
                .map(|u| (u.name.as_str(), u.panels))
                .collect::<Vec<_>>(),
            vec![("Explore", 0), ("Quarterly", 2)]
        );
        assert!(index.dataset(other).is_empty());
        let by_fit = index.fits([fit, Uuid::new_v4()]);
        assert_eq!(by_fit.len(), 1);
        assert_eq!(by_fit[0].id, report.id);
        assert_eq!(by_fit[0].panels, 1);
    }

    #[test]
    fn a_report_state_with_a_broken_panel_is_refused_naming_it() {
        let ok = json!({ "blocks": [{ "id": "a", "kind": "panel", "panel": dataset_plot(DatasetId::new()) }] });
        check_state(WorkspaceKind::Report, &ok).expect("reads");
        let bad = json!({ "blocks": [
            { "id": "a", "kind": "panel", "panel": dataset_plot(DatasetId::new()) },
            { "id": "b", "kind": "panel", "panel": { "id": Uuid::new_v4(), "kind": "plot", "content": {} } },
        ] });
        let err = check_state(WorkspaceKind::Report, &bad).expect_err("panel 2 is not a panel");
        assert!(err.to_string().contains("panel 2"), "{err}");
        // An explorer's state holds no panels, so nothing in it is checked.
        check_state(WorkspaceKind::DataExplorer, &bad).expect("not a report");
        let long = json!({ "blocks": [{ "id": "a", "kind": "panel", "panel": {
            "id": Uuid::new_v4(), "kind": "text",
            "content": { "markdown": "x".repeat(MAX_TEXT + 1) } } }] });
        assert!(check_state(WorkspaceKind::Report, &long).is_err());
    }

    #[test]
    fn a_report_is_a_document_of_panels_headings_text_and_page_breaks_on_a_page() {
        let document = json!({
            "page": { "size": "A4", "orientation": "landscape" },
            "blocks": [
                { "id": "h", "kind": "heading", "text": "House prices", "level": 1 },
                { "id": "t", "kind": "text", "markdown": "Prices *rose*." },
                { "id": "p", "kind": "panel", "panel": dataset_plot(DatasetId::new()) },
                { "id": "b", "kind": "page_break" },
            ],
        });
        check_state(WorkspaceKind::Report, &document).expect("a document");
        check_state(WorkspaceKind::Report, &json!({})).expect("an empty report");

        let refused = |state: Json, says: &str| {
            let err = check_state(WorkspaceKind::Report, &state).expect_err(says);
            assert!(err.to_string().contains(says), "{err} should say {says}");
        };
        refused(
            json!({ "blocks": [{ "id": "a", "kind": "page_break" }, { "id": "x", "kind": "chart" }] }),
            "block 2 of the report is a \"chart\"",
        );
        refused(
            json!({ "blocks": [{ "id": "a", "kind": "heading", "text": "H", "level": 4 }] }),
            "level is not 1, 2 or 3",
        );
        refused(
            json!({ "blocks": [{ "id": "a", "kind": "text" }] }),
            "without its Markdown",
        );
        refused(json!({ "blocks": [{ "kind": "page_break" }] }), "has no id");
        refused(
            json!({ "page": { "size": "B5", "orientation": "portrait" } }),
            "one of A4, A3, Letter, Legal",
        );
        refused(
            json!({ "page": { "size": "A4", "orientation": "sideways" } }),
            "portrait or landscape",
        );
        // The panels in it are still what the usage index reads.
        assert_eq!(panels_in_state(WorkspaceKind::Report, &document).len(), 1);
    }
}
