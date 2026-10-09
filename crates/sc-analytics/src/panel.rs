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
//! | `map` | `{ spec }` — a [`MapSpec`]: a map panel of the explorer, or a whole Map workspace (A5.13) |
//! | `stat_card` | a [`StatCard`]: one number from a dataset, compared and with a sparkline (A6.2) |
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

use crate::card::{RenderedCard, StatCard, render_card_in};
use crate::crossfilter::{self, Applied, Condition};
use crate::map::{MapSpec, RenderedMap, render_map_in};
use crate::model_outputs::{OutputView, render_one_output};
use crate::plot::render::plot_rows;
use crate::plot::{
    Channel, DataRef, PlotSpec, Rendered, RenderedTable, TableSpec, render_plot_in, render_table_in,
};
use crate::stats::{TestSpec, TestsAnswer, run_tests_in};
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
    /// A map: its layers over a base map (A5.13).
    Map {
        /// The map.
        spec: MapSpec,
    },
    /// One number from a dataset: a dashboard's stat card (A6.2).
    StatCard(StatCard),
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
            PanelBody::Map { .. } => "map",
            PanelBody::StatCard(_) => "stat_card",
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
            PanelBody::Text { .. }
            | PanelBody::FitTable { .. }
            | PanelBody::Map { .. }
            | PanelBody::StatCard(_)
            | PanelBody::Custom { .. } => vec![],
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

    /// The datasets it reads: a map's, each layer's; a stat card's.
    pub fn datasets(&self) -> BTreeSet<DatasetId> {
        let mut out: BTreeSet<DatasetId> = self
            .body
            .data()
            .into_iter()
            .filter_map(|d| match d {
                DataRef::Dataset { dataset } => Some(*dataset),
                DataRef::FitOutput { .. } => None,
            })
            .collect();
        match &self.body {
            PanelBody::Map { spec } => out.extend(spec.layers.iter().map(|l| l.dataset)),
            PanelBody::StatCard(card) => {
                out.insert(card.dataset);
            }
            _ => {}
        }
        out
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
            PanelBody::Map { spec } if spec.layers.is_empty() => {
                Err(Error::invalid("a map panel has at least one layer"))
            }
            PanelBody::Map { spec } => spec.check(false),
            PanelBody::StatCard(card) => card.check(),
            _ => Ok(()),
        }
    }
}

// --- where a workspace keeps its panels -----------------------------------------

/// The panels in a workspace's state, by where its kind keeps them: a report's
/// are the `panel` of each of its `blocks` whose `kind` is `panel` (A4.3–A4.4),
/// a dashboard's the `panel` of each of its `tiles` (A6.1).
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
        WorkspaceKind::Dashboard => state
            .get("tiles")
            .and_then(Json::as_array)
            .map(|tiles| tiles.iter().filter_map(|t| t.get("panel")).collect())
            .unwrap_or_default(),
        WorkspaceKind::DataExplorer
        | WorkspaceKind::Notebook
        | WorkspaceKind::Map
        | WorkspaceKind::Simulation => vec![],
    }
}

/// Refuse a state whose panels do not read, naming the first that does not,
/// a report whose other blocks or page are not ones it has, and a dashboard
/// whose tiles are not on its grid.
pub fn check_state(kind: WorkspaceKind, state: &Json) -> Result<()> {
    for (i, slot) in panel_slots(kind, state).into_iter().enumerate() {
        Panel::from_json(slot)
            .map_err(|e| Error::invalid(format!("panel {} of the workspace: {e}", i + 1)))?;
    }
    match kind {
        WorkspaceKind::Report => check_report(state)?,
        WorkspaceKind::Dashboard => check_dashboard(state)?,
        WorkspaceKind::Map => crate::map::spec_of_state(state)?.check(true)?,
        _ => {}
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

/// The columns of a dashboard's grid (A6.1).
pub const DASHBOARD_COLUMNS: u64 = 12;
/// The most rows of the grid a tile may span.
pub const MAX_TILE_ROWS: u64 = 40;
/// The most tiles a dashboard holds.
pub const MAX_TILES: usize = 100;

/// A dashboard's tiles (A6.1): each `{ id, panel, x, y, w, h, drill? }`,
/// placed on a grid [`DASHBOARD_COLUMNS`] wide — `x` and `w` in columns, `y`
/// and `h` in rows — without overlapping. The browser lays a narrow screen
/// out in one column from the same tiles; what is stored is the wide layout.
/// Beside the tiles, its own `filters` (conditions, A6.6) and how often it
/// `refresh`es, in seconds.
fn check_dashboard(state: &Json) -> Result<()> {
    let tiles = match state.get("tiles") {
        None => &[][..],
        Some(Json::Array(tiles)) => &tiles[..],
        Some(_) => return Err(Error::invalid("a dashboard's tiles are a list")),
    };
    if tiles.len() > MAX_TILES {
        return Err(Error::invalid(format!(
            "a dashboard holds at most {MAX_TILES} tiles"
        )));
    }
    let mut placed: Vec<(usize, [u64; 4])> = Vec::with_capacity(tiles.len());
    for (i, tile) in tiles.iter().enumerate() {
        let refuse = |why: &str| Error::invalid(format!("tile {} of the dashboard {why}", i + 1));
        if tile
            .get("id")
            .and_then(Json::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(refuse("has no id"));
        }
        if tile.get("panel").is_none() {
            return Err(refuse("has no panel"));
        }
        let at = |field: &str| tile.get(field).and_then(Json::as_u64);
        let (Some(x), Some(y), Some(w), Some(h)) = (at("x"), at("y"), at("w"), at("h")) else {
            return Err(refuse(
                "is not placed: its x, y, w and h are whole numbers, not negative",
            ));
        };
        if w == 0 || x + w > DASHBOARD_COLUMNS {
            return Err(refuse(&format!(
                "does not fit the grid's {DASHBOARD_COLUMNS} columns"
            )));
        }
        if !(1..=MAX_TILE_ROWS).contains(&h) {
            return Err(refuse(&format!(
                "is 1 to {MAX_TILE_ROWS} rows high, not {h}"
            )));
        }
        if let Some(drill) = tile.get("drill") {
            check_drill(drill).map_err(|why| refuse(&why))?;
        }
        let rect = [x, y, w, h];
        if let Some((j, _)) = placed.iter().find(|(_, o)| overlap(o, &rect)) {
            return Err(Error::invalid(format!(
                "tiles {} and {} of the dashboard overlap",
                j + 1,
                i + 1
            )));
        }
        placed.push((i, rect));
    }
    if let Some(filters) = state.get("filters") {
        let filters = Condition::list_of(filters)
            .map_err(|e| Error::invalid(format!("the dashboard's filters: {e}")))?;
        if filters.len() > MAX_DASHBOARD_FILTERS {
            return Err(Error::invalid(format!(
                "a dashboard has at most {MAX_DASHBOARD_FILTERS} filters of its own"
            )));
        }
    }
    match state.get("refresh") {
        None | Some(Json::Null) => {}
        Some(r) => match r.as_u64() {
            Some(0) => {}
            Some(s) if REFRESH_SECONDS.contains(&s) => {}
            _ => {
                return Err(Error::invalid(format!(
                    "a dashboard refreshes every {} to {} seconds, or not at all",
                    REFRESH_SECONDS.start(),
                    REFRESH_SECONDS.end()
                )));
            }
        },
    }
    Ok(())
}

/// The most filters a dashboard keeps of its own (A6.6); a panel is drawn
/// with those and the selections, at most [`crossfilter::MAX_CONDITIONS`].
pub const MAX_DASHBOARD_FILTERS: usize = 20;
/// How often a dashboard may refresh its panels, in seconds (A6.6).
pub const REFRESH_SECONDS: std::ops::RangeInclusive<u64> = 10..=86_400;
/// The most levels a drill path has (A6.5).
pub const MAX_DRILL_LEVELS: usize = 8;

/// A tile's drill path (A6.5): `{ channel, path }`, the columns the channel
/// shows at each level, outermost first — `district`, then `category`. A click
/// on a value at one level shows the next, for that value.
fn check_drill(drill: &Json) -> std::result::Result<(), String> {
    let channel = drill
        .get("channel")
        .cloned()
        .map(serde_json::from_value::<Channel>)
        .and_then(std::result::Result::ok);
    if !matches!(channel, Some(Channel::X | Channel::Y | Channel::Color)) {
        return Err("drills down along X, Y or Color".to_owned());
    }
    let Some(path) = drill.get("path").and_then(Json::as_array) else {
        return Err("has a drill path without its columns".to_owned());
    };
    let names: Vec<&str> = path.iter().filter_map(Json::as_str).collect();
    if names.len() != path.len() || names.iter().any(|n| n.trim().is_empty()) {
        return Err("has a drill path whose levels are not all columns".to_owned());
    }
    if !(2..=MAX_DRILL_LEVELS).contains(&names.len()) {
        return Err(format!(
            "has a drill path of {} levels, and one has 2 to {MAX_DRILL_LEVELS}",
            names.len()
        ));
    }
    let distinct: BTreeSet<&str> = names.iter().copied().collect();
    if distinct.len() != names.len() {
        return Err("has a drill path that names a column twice".to_owned());
    }
    Ok(())
}

/// Whether two `[x, y, w, h]` rectangles share a cell.
fn overlap(a: &[u64; 4], b: &[u64; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// The datasets a kind's state reads directly, outside any panel, each with
/// how many of its parts read it: the Data explorer's chosen dataset (none),
/// a map's layers.
fn state_datasets(kind: WorkspaceKind, state: &Json) -> BTreeMap<DatasetId, usize> {
    match kind {
        WorkspaceKind::DataExplorer => state
            .get("dataset")
            .and_then(Json::as_str)
            .and_then(|s| s.parse().ok())
            .map(|d| BTreeMap::from([(d, 0)]))
            .unwrap_or_default(),
        WorkspaceKind::Map => crate::map::spec_of_state(state)
            .map(|spec| spec.datasets())
            .unwrap_or_default(),
        _ => BTreeMap::new(),
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
    /// How many of its panels read the thing — a map's layers — none for an
    /// explorer that only has it chosen.
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
            for (d, n) in state_datasets(ws.kind, &ws.state) {
                *datasets.entry(d).or_default() += n;
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
    /// A map's layers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map: Option<RenderedMap>,
    /// A stat card's numbers, or why there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<RenderedCard>,
    /// The columns a plot reads that are categories though their values are
    /// numbers — foreign keys — so the browser draws their ids as the
    /// explorer does, as values rather than a scale.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categorical: Vec<String>,
    /// What each of a dashboard's conditions did to each dataset the panel
    /// reads: the column it filtered, or why it filtered none (A6.4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Applied>,
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
    render_panel_in(catalog, panel, &[]).await
}

/// [`render_panel`] on a dashboard (A6.3–A6.4): over the rows `conditions`
/// keep, each applied to the panel's datasets it reaches
/// ([`crossfilter::scope`]). What became of each is in the answer's
/// `filters`.
pub async fn render_panel_in(
    catalog: &Catalog,
    panel: &Panel,
    conditions: &[Condition],
) -> Result<RenderedPanel> {
    panel.check()?;
    let mut out = RenderedPanel {
        kind: panel.body.kind(),
        ..RenderedPanel::default()
    };
    if let Some(sentence) = missing_reference(catalog, panel).await? {
        out.error = Some(sentence);
        return Ok(out);
    }
    let scope = crossfilter::scope(catalog, conditions, &panel.datasets()).await?;
    out.filters = scope.applied.clone();
    match &panel.body {
        PanelBody::Plot { spec } => {
            out.plot = Some(render_plot_in(catalog, spec, &scope).await?);
            out.categorical = categorical(catalog, &spec.data).await?;
        }
        PanelBody::SummaryTable { spec } => {
            out.table = Some(render_table_in(catalog, spec, &scope).await?);
        }
        PanelBody::TestResult { tests, plot } => {
            out.tests = Some(run_tests_in(catalog, tests, &scope).await?);
            if let Some(spec) = plot {
                out.plot = Some(render_plot_in(catalog, spec, &scope).await?);
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
        PanelBody::Map { spec } => out.map = Some(render_map_in(catalog, spec, &scope).await?),
        PanelBody::StatCard(card) => {
            out.card = Some(render_card_in(catalog, card, &scope).await?);
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
                "map",
                json!({ "spec": { "layers": [{ "dataset": d,
                    "geometry": { "kind": "key", "column": "district", "geometry": "outline" },
                    "style": { "kind": "graduated", "method": "quantile", "classes": 5 },
                    "encoding": { "color": { "field": "count" } }, "opacity": 0.5 }] } }),
            ),
            (
                "stat_card",
                json!({ "dataset": d, "value": { "function": "count" },
                        "time": { "column": "at", "period": "month" },
                        "comparison": "previous_period", "sparkline": true }),
            ),
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

        // A map reads each layer's dataset.
        let (a, b) = (DatasetId::new(), DatasetId::new());
        let map = Panel::from_json(&json!({
            "id": Uuid::new_v4(), "kind": "map",
            "content": { "spec": { "layers": [
                { "dataset": a, "geometry": { "kind": "column", "column": "at" } },
                { "dataset": b, "geometry": { "kind": "column", "column": "at" } } ] } }
        }))
        .expect("panel");
        assert_eq!(map.datasets(), BTreeSet::from([a, b]));
        // A map with nothing on it, or a layer half-transparent past 1, is refused.
        assert!(
            Panel::from_json(&json!({ "id": Uuid::new_v4(), "kind": "map",
            "content": { "spec": { "layers": [] } } }))
            .is_err()
        );
        let err = Panel::from_json(&json!({ "id": Uuid::new_v4(), "kind": "map",
            "content": { "spec": { "layers": [
                { "dataset": a, "geometry": { "kind": "column", "column": "at" }, "opacity": 2 } ] } } }))
        .expect_err("opacity");
        assert!(err.to_string().contains("opacity"), "{err}");
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
        // A map reads the datasets of its layers; a report's blocks in a
        // map's state are not its.
        let mut map = Workspace::new("Map", WorkspaceKind::Map, None);
        map.state = json!({
            "blocks": [{ "kind": "panel", "panel": dataset_plot(other) }],
            "layers": [
                { "id": "a", "dataset": houses, "geometry": { "kind": "column", "column": "at" } },
                { "id": "b", "dataset": houses, "geometry": { "kind": "column", "column": "at" },
                  "filter": "price > 3" },
            ],
        });

        let index = UsageIndex::of_workspaces(&[report.clone(), explorer.clone(), map]);
        let uses = index.dataset(houses);
        assert_eq!(
            uses.iter()
                .map(|u| (u.name.as_str(), u.panels))
                .collect::<Vec<_>>(),
            vec![("Explore", 0), ("Map", 2), ("Quarterly", 2)]
        );
        assert!(index.dataset(other).is_empty());
        let by_fit = index.fits([fit, Uuid::new_v4()]);
        assert_eq!(by_fit.len(), 1);
        assert_eq!(by_fit[0].id, report.id);
        assert_eq!(by_fit[0].panels, 1);
    }

    fn tile(id: &str, panel: Json, [x, y, w, h]: [u64; 4]) -> Json {
        json!({ "id": id, "panel": panel, "x": x, "y": y, "w": w, "h": h })
    }

    #[test]
    fn a_dashboard_is_tiles_of_panels_on_a_grid_without_overlaps() {
        let houses = DatasetId::new();
        let card = json!({ "id": Uuid::new_v4(), "kind": "stat_card", "title": "Houses",
            "content": { "dataset": houses, "value": { "function": "count" } } });
        let state = json!({ "tiles": [
            tile("a", dataset_plot(houses), [0, 0, 8, 4]),
            tile("b", card.clone(), [8, 0, 4, 2]),
            tile("c", dataset_plot(DatasetId::new()), [8, 2, 4, 6]),
        ] });
        check_state(WorkspaceKind::Dashboard, &state).expect("a dashboard");
        check_state(WorkspaceKind::Dashboard, &json!({})).expect("an empty dashboard");

        // Its panels are what the usage index reads: the plot and the card.
        assert_eq!(panels_in_state(WorkspaceKind::Dashboard, &state).len(), 3);
        let mut dashboard = Workspace::new("Board", WorkspaceKind::Dashboard, None);
        dashboard.state = state;
        let index = UsageIndex::of_workspaces(&[dashboard]);
        assert_eq!(index.dataset(houses)[0].panels, 2);

        let refused = |tiles: Vec<Json>, says: &str| {
            let err =
                check_state(WorkspaceKind::Dashboard, &json!({ "tiles": tiles })).expect_err(says);
            assert!(err.to_string().contains(says), "{err} should say {says}");
        };
        refused(
            vec![
                tile("a", card.clone(), [0, 0, 4, 2]),
                tile("b", card.clone(), [3, 1, 4, 2]),
            ],
            "tiles 1 and 2 of the dashboard overlap",
        );
        refused(vec![tile("a", card.clone(), [10, 0, 4, 2])], "12 columns");
        refused(vec![tile("a", card.clone(), [0, 0, 0, 2])], "12 columns");
        refused(
            vec![tile("a", card.clone(), [0, 0, 4, 0])],
            "1 to 40 rows high",
        );
        refused(
            vec![json!({ "id": "a", "panel": card.clone(), "x": -1, "y": 0, "w": 4, "h": 2 })],
            "is not placed",
        );
        refused(vec![tile("", card.clone(), [0, 0, 4, 2])], "has no id");
        refused(
            vec![json!({ "id": "a", "x": 0, "y": 0, "w": 4, "h": 2 })],
            "has no panel",
        );
        // A card that cannot be made is refused as its panel.
        let bad = json!({ "id": Uuid::new_v4(), "kind": "stat_card",
            "content": { "dataset": houses, "value": { "function": "mean" } } });
        refused(
            vec![tile("a", bad, [0, 0, 4, 2])],
            "panel 1 of the workspace",
        );
    }

    #[test]
    fn a_dashboard_keeps_drill_paths_filters_and_a_refresh_interval() {
        let incidents = DatasetId::new();
        let mut bar = tile("a", dataset_plot(incidents), [0, 0, 6, 4]);
        bar["drill"] = json!({ "channel": "x", "path": ["district", "category"] });
        let state = json!({
            "tiles": [bar.clone()],
            "filters": [{ "id": "f1", "dataset": incidents, "column": "category",
                          "values": ["burglary"] },
                        { "id": "f2", "dataset": incidents, "column": "occurred_on",
                          "range": { "min": "2025-01-01" } }],
            "refresh": 300,
        });
        check_state(WorkspaceKind::Dashboard, &state).expect("a dashboard");
        check_state(
            WorkspaceKind::Dashboard,
            &json!({ "tiles": [], "refresh": 0 }),
        )
        .expect("no refresh");

        let refused = |change: &dyn Fn(&mut Json), says: &str| {
            let mut s = state.clone();
            change(&mut s);
            let err = check_state(WorkspaceKind::Dashboard, &s).expect_err(says);
            assert!(err.to_string().contains(says), "{err} should say {says}");
        };
        refused(
            &|s| s["tiles"][0]["drill"]["path"] = json!(["district"]),
            "tile 1 of the dashboard has a drill path of 1 levels",
        );
        refused(
            &|s| s["tiles"][0]["drill"]["path"] = json!(["district", "district"]),
            "names a column twice",
        );
        refused(
            &|s| s["tiles"][0]["drill"]["channel"] = json!("size"),
            "drills down along X, Y or Color",
        );
        refused(
            &|s| s["tiles"][0]["drill"]["path"] = json!(["district", 3]),
            "not all columns",
        );
        refused(
            &|s| s["filters"][1]["id"] = json!("f1"),
            "two filters have the id `f1`",
        );
        refused(
            &|s| s["filters"][0]["values"] = json!([]),
            "the dashboard's filters: filter 1: a filter keeps at least one value",
        );
        refused(&|s| s["refresh"] = json!(5), "every 10 to 86400 seconds");
        refused(&|s| s["refresh"] = json!("often"), "every 10 to 86400");
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
