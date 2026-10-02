//! Plots (analytics TODO A2.1–A2.6; the goals document's "Plots and the
//! grammar of graphics").
//!
//! - [`spec`](PlotSpec) — the plot spec: data, layers (mark, encoding, stat),
//!   scales, coordinates, facets, reference lines and selections.
//! - [`validate`] — the spec checked against a dataset's shape, each refusal a
//!   sentence.
//! - [`show_me`] and [`preset`] — the plot chosen from the column types on the
//!   drop zones, and the gallery's presets.
//! - [`render_plot`] — the stats compiled to SQL over the dataset's query and
//!   finished in memory: each layer's data and the scale domains.
//! - [`render_table`] — a summary table (A2.8) by the same machinery: the
//!   cells for each combination of the row and column dimensions, and the
//!   totals.

mod math;
pub(crate) mod render;
mod show_me;
mod spec;
pub(crate) mod validate;

pub use math::{BinParams, CurvePoint, LinearFit, LinearSums};
pub use render::{
    DEFAULT_SAMPLE, DOMAIN_VALUES, Domain, EXACT_DENSITY, LOESS_SAMPLE, LayerData, MAX_BOXES,
    MAX_CURVES, MAX_FACETS, MAX_GROUP_ROWS, MAX_OUTLIERS, MAX_SAMPLE, MAX_TABLE_ROWS, PlotData,
    Rendered, RenderedTable, Table, TableData, render_plot, render_table,
};
pub use show_me::{Assignment, GalleryItem, Preset, gallery, preset, show_me};
pub use spec::{
    AggregateFn, Bin, Cell, Channel, Coord, DataRef, Encoding, Facet, FacetScales, FieldDef, Fold,
    Layer, Mark, Pairs, PlotSpec, Reference, Scale, ScaleKind, Selection, SelectionKind,
    SmoothMethod, Stat, TableSpec,
};
pub use validate::{folded_shape, validate, validate_table};
