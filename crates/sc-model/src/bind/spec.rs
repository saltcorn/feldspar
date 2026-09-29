//! What the configuration says: the bindings, the declared dimensions and the
//! per-dataset policies, as types (Stan TODO §§8–10).
//!
//! Parsed from the model's configuration JSON with a sentence per mistake that
//! names the variable (or the dimension, or the dataset) it is about, because
//! the admin reading it is looking at a table of bindings, one row per
//! variable.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use super::{BINDINGS_KEY, DIMENSIONS_KEY, POLICIES_KEY};

/// The rule computing one `data` variable (§9). One variable, one binding.
///
/// Every `dataset` is a dataset's name as bindings know it —
/// [`MAIN_DATASET`](crate::MAIN_DATASET) or a related dataset's — and every
/// `column` is one of that dataset's **columns** by name. The binder never
/// learns a second way to reach across a key: a join path or an aggregation is
/// a formula on the dataset, and the binding names the column it computes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Binding {
    /// A JSON literal, as written — `2.5`, `[1, 0, 0]`, `[[1, 0], [0, 1]]`.
    Value {
        /// The literal. `"NaN"`, `"Inf"` and `"-Inf"` are reals.
        value: Json,
    },
    /// A dataset's row count.
    Count {
        /// The dataset.
        dataset: String,
    },
    /// A dimension's number of positions.
    Size {
        /// The dimension: a dataset's name (its rows), a declared dimension, or
        /// a time grid's `name.future`.
        dimension: String,
    },
    /// One value per row.
    Column {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
        /// How a date becomes a number. Required for a date column.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<TimeScale>,
    },
    /// One row per dataset row, one column per listed column.
    Columns {
        /// The dataset.
        dataset: String,
        /// Its columns, in order.
        columns: Vec<String>,
    },
    /// A model matrix through [`sc_model::encode`](crate::fit_encoding):
    /// categoricals one-hot and reference-coded, numbers as they are or
    /// standardised.
    Design {
        /// The dataset.
        dataset: String,
        /// Its columns, in order.
        columns: Vec<String>,
        /// Centre and scale the numeric columns.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        standardise: bool,
    },
    /// The number of columns of a `design`-bound variable.
    Width {
        /// The `design`-bound variable.
        of: String,
    },
    /// The 1-based position of each row's value in a dimension.
    Index {
        /// The dataset.
        dataset: String,
        /// Its column holding the values.
        column: String,
        /// The dimension indexed into.
        dimension: String,
        /// Which column of the target dataset the values are compared with —
        /// its primary key when absent. Only for a rows dimension.
        #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
        match_column: Option<String>,
    },
    /// The 1-based row positions where a column is not null.
    Present {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
    },
    /// The 1-based row positions where a column is null.
    Absent {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
    },
    /// How many rows a column is not null in.
    CountPresent {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
    },
    /// How many rows a column is null in.
    CountAbsent {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
    },
    /// A column's non-null values.
    PresentValues {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
    },
    /// For each position of an `index`-bound variable's dimension, the 1-based
    /// row its rows start at. The dataset is sorted by that index.
    SegmentStart {
        /// The dataset.
        dataset: String,
        /// The `index`-bound variable, over the same dataset.
        index: String,
    },
    /// For each position of an `index`-bound variable's dimension, how many
    /// rows it has.
    SegmentSize {
        /// The dataset.
        dataset: String,
        /// The `index`-bound variable, over the same dataset.
        index: String,
    },
    /// One value per position of a dimension — a time grid's steps, usually —
    /// aggregating the rows that fall in each (§12).
    Series {
        /// The dataset.
        dataset: String,
        /// Its column holding the values; absent to count rows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
        /// Where each row falls.
        over: Along,
        /// How the rows of one position combine: `count` when there is no
        /// `column`, `refuse` when there is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregate: Option<Aggregate>,
        /// The value of a position no row falls in (a number, or `"NaN"`).
        /// A count needs none: it is 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fill: Option<Json>,
    },
    /// 1 for each position of a dimension some row falls in (with a non-null
    /// value, when there is a `column`), 0 for the others — `series`'s mask.
    SeriesPresent {
        /// The dataset.
        dataset: String,
        /// Its column; absent to ask only whether a row falls there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
        /// Where each row falls.
        over: Along,
    },
    /// `series` over two dimensions at once: one value per cell of a
    /// `matrix[R, C]`.
    Cells {
        /// The dataset.
        dataset: String,
        /// Its column holding the values; absent to count rows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
        /// Which row of the matrix each row falls in.
        rows: Along,
        /// Which column of the matrix each row falls in.
        cols: Along,
        /// As for `series`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregate: Option<Aggregate>,
        /// As for `series`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fill: Option<Json>,
    },
    /// `series_present` over two dimensions.
    CellsPresent {
        /// The dataset.
        dataset: String,
        /// Its column.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
        /// Which row of the matrix each row falls in.
        rows: Along,
        /// Which column of the matrix each row falls in.
        cols: Along,
    },
    /// The number of edges of a graph — the ICAR formulation's `N_edges`.
    EdgeCount(Edges),
    /// Each edge's first node — `node1`.
    EdgeFrom(Edges),
    /// Each edge's second node — `node2`.
    EdgeTo(Edges),
    /// The dense 0/1 `matrix[R, R]`, symmetric.
    Adjacency(Edges),
    /// How many connected components the graph has.
    Components(Edges),
    /// Each node's connected component, numbered `1..` in the order of their
    /// first node.
    Component(Edges),
    /// BYM2's scaling factor: the geometric mean of the marginal variances of
    /// the ICAR precision's generalised inverse, per connected component.
    IcarScale(Edges),
    /// Each row's position as `[x, y]`: longitude and latitude in degrees, or
    /// kilometres east and north of the centroid when projected.
    Points(Points),
    /// The `matrix[N, N]` of great-circle distances between the rows, in km.
    Distances(Points),
}

/// Where each row of a dataset falls in a dimension: its value of `column`,
/// looked up as an `index` looks one up.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Along {
    /// The dimension.
    pub dimension: String,
    /// The dataset's column holding each row's value; when absent, the
    /// dimension's own column, which needs the dimension to be declared over
    /// the same dataset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// For a rows dimension, which column of its dataset the values are
    /// compared with — the primary key when absent.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_column: Option<String>,
}

/// How the rows falling in one position of a `series` combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// How many (with a non-null value, when there is a column).
    Count,
    /// Their sum.
    Sum,
    /// Their mean — a real.
    Mean,
    /// The least.
    Min,
    /// The greatest.
    Max,
    /// The first in the dataset's order.
    First,
    /// The last in the dataset's order.
    Last,
    /// None of them: two rows in one position is refused, naming both.
    Refuse,
}

impl Aggregate {
    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            Aggregate::Count => "count",
            Aggregate::Sum => "sum",
            Aggregate::Mean => "mean",
            Aggregate::Min => "min",
            Aggregate::Max => "max",
            Aggregate::First => "first",
            Aggregate::Last => "last",
            Aggregate::Refuse => "refuse",
        }
    }
}

/// A graph over a dimension, stored as a junction table: one row per edge,
/// two columns naming its ends (§12).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edges {
    /// The junction dataset.
    pub dataset: String,
    /// Its column naming one end.
    pub from: String,
    /// Its column naming the other.
    pub to: String,
    /// The dimension the ends are positions of — `regions`.
    pub dimension: String,
    /// For a rows dimension, which column of its dataset the ends are
    /// compared with — the primary key when absent.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_column: Option<String>,
    /// For `edge_count`, `edge_from` and `edge_to`: `dedupe` (each unordered
    /// pair once, the lesser position first, sorted) or `keep` (every row as
    /// stored, in the dataset's order).
    #[serde(default, skip_serializing_if = "Symmetric::is_default")]
    pub symmetric: Symmetric,
}

impl Edges {
    fn describe(&self, kind: &str) -> String {
        let matching = match &self.match_column {
            Some(m) => format!(", match: {m}"),
            None => String::new(),
        };
        format!(
            "{kind}({}: {} — {} → {}{matching})",
            self.dataset, self.from, self.to, self.dimension
        )
    }
}

/// Whether an edge list keeps each unordered pair once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Symmetric {
    /// Each unordered pair once, `node1 < node2`, sorted.
    #[default]
    Dedupe,
    /// Every row as stored.
    Keep,
}

impl Symmetric {
    fn is_default(&self) -> bool {
        *self == Symmetric::Dedupe
    }
}

/// Sites with a latitude and a longitude, in degrees.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Points {
    /// The dataset.
    pub dataset: String,
    /// Its latitude column, degrees north.
    pub lat: String,
    /// Its longitude column, degrees east.
    pub lon: String,
    /// For `points`: kilometres east and north of the centroid
    /// (equirectangular — adequate at city-to-country scale) rather than
    /// degrees.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project: bool,
}

impl Binding {
    /// The kind's wire name.
    pub fn kind(&self) -> &'static str {
        match self {
            Binding::Value { .. } => "value",
            Binding::Count { .. } => "count",
            Binding::Size { .. } => "size",
            Binding::Column { .. } => "column",
            Binding::Columns { .. } => "columns",
            Binding::Design { .. } => "design",
            Binding::Width { .. } => "width",
            Binding::Index { .. } => "index",
            Binding::Present { .. } => "present",
            Binding::Absent { .. } => "absent",
            Binding::CountPresent { .. } => "count_present",
            Binding::CountAbsent { .. } => "count_absent",
            Binding::PresentValues { .. } => "present_values",
            Binding::SegmentStart { .. } => "segment_start",
            Binding::SegmentSize { .. } => "segment_size",
            Binding::Series { .. } => "series",
            Binding::SeriesPresent { .. } => "series_present",
            Binding::Cells { .. } => "cells",
            Binding::CellsPresent { .. } => "cells_present",
            Binding::EdgeCount(_) => "edge_count",
            Binding::EdgeFrom(_) => "edge_from",
            Binding::EdgeTo(_) => "edge_to",
            Binding::Adjacency(_) => "adjacency",
            Binding::Components(_) => "components",
            Binding::Component(_) => "component",
            Binding::IcarScale(_) => "icar_scale",
            Binding::Points(_) => "points",
            Binding::Distances(_) => "distances",
        }
    }

    /// The graph, for the edge kinds.
    pub fn edges(&self) -> Option<&Edges> {
        match self {
            Binding::EdgeCount(e)
            | Binding::EdgeFrom(e)
            | Binding::EdgeTo(e)
            | Binding::Adjacency(e)
            | Binding::Components(e)
            | Binding::Component(e)
            | Binding::IcarScale(e) => Some(e),
            _ => None,
        }
    }

    /// The sites, for `points` and `distances`.
    pub fn points(&self) -> Option<&Points> {
        match self {
            Binding::Points(p) | Binding::Distances(p) => Some(p),
            _ => None,
        }
    }

    /// The dimensions each row of a `series` or `cells` falls along.
    pub fn along(&self) -> Vec<&Along> {
        match self {
            Binding::Series { over, .. } | Binding::SeriesPresent { over, .. } => vec![over],
            Binding::Cells { rows, cols, .. } | Binding::CellsPresent { rows, cols, .. } => {
                vec![rows, cols]
            }
            _ => Vec::new(),
        }
    }

    /// The dataset it reads, for the kinds that read one.
    pub fn dataset(&self) -> Option<&str> {
        if let Some(e) = self.edges() {
            return Some(&e.dataset);
        }
        if let Some(p) = self.points() {
            return Some(&p.dataset);
        }
        match self {
            Binding::Value { .. } | Binding::Size { .. } | Binding::Width { .. } => None,
            Binding::Count { dataset }
            | Binding::Column { dataset, .. }
            | Binding::Columns { dataset, .. }
            | Binding::Design { dataset, .. }
            | Binding::Index { dataset, .. }
            | Binding::Present { dataset, .. }
            | Binding::Absent { dataset, .. }
            | Binding::CountPresent { dataset, .. }
            | Binding::CountAbsent { dataset, .. }
            | Binding::PresentValues { dataset, .. }
            | Binding::SegmentStart { dataset, .. }
            | Binding::SegmentSize { dataset, .. }
            | Binding::Series { dataset, .. }
            | Binding::SeriesPresent { dataset, .. }
            | Binding::Cells { dataset, .. }
            | Binding::CellsPresent { dataset, .. } => Some(dataset),
            _ => None,
        }
    }

    /// The columns whose nulls follow the dataset's `nulls` policy (§10):
    /// those of a `column`, `columns`, `design` or `index`. The others handle
    /// nulls themselves.
    pub fn policed_columns(&self) -> Vec<&str> {
        match self {
            Binding::Column { column, .. } | Binding::Index { column, .. } => vec![column],
            Binding::Columns { columns, .. } | Binding::Design { columns, .. } => {
                columns.iter().map(String::as_str).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Every column of its dataset it reads.
    pub fn columns(&self) -> Vec<&str> {
        match self {
            Binding::Present { column, .. }
            | Binding::Absent { column, .. }
            | Binding::CountPresent { column, .. }
            | Binding::CountAbsent { column, .. }
            | Binding::PresentValues { column, .. } => vec![column],
            Binding::Series { column, .. }
            | Binding::SeriesPresent { column, .. }
            | Binding::Cells { column, .. }
            | Binding::CellsPresent { column, .. } => column
                .iter()
                .map(String::as_str)
                .chain(self.along().into_iter().filter_map(|a| a.column.as_deref()))
                .collect(),
            Binding::EdgeCount(e)
            | Binding::EdgeFrom(e)
            | Binding::EdgeTo(e)
            | Binding::Adjacency(e)
            | Binding::Components(e)
            | Binding::Component(e)
            | Binding::IcarScale(e) => vec![&e.from, &e.to],
            Binding::Points(p) | Binding::Distances(p) => vec![&p.lat, &p.lon],
            other => other.policed_columns(),
        }
    }

    /// How it reads in a sentence: `counties.log_uranium`,
    /// `index(main.county → counties)`.
    pub fn describe(&self) -> String {
        match self {
            Binding::Value { value } => {
                let text = value.to_string();
                if text.chars().count() > 40 {
                    "value(…)".to_owned()
                } else {
                    format!("value({text})")
                }
            }
            Binding::Count { dataset } => format!("count({dataset})"),
            Binding::Size { dimension } => format!("size({dimension})"),
            Binding::Column {
                dataset, column, ..
            } => format!("{dataset}.{column}"),
            Binding::Columns { dataset, columns } => {
                format!("columns({dataset}: {})", columns.join(", "))
            }
            Binding::Design {
                dataset, columns, ..
            } => format!("design({dataset}: {})", columns.join(", ")),
            Binding::Width { of } => format!("width({of})"),
            Binding::Index {
                dataset,
                column,
                dimension,
                match_column,
            } => match match_column {
                Some(m) => format!("index({dataset}.{column} → {dimension}, match: {m})"),
                None => format!("index({dataset}.{column} → {dimension})"),
            },
            Binding::Present { dataset, column } => format!("present({dataset}.{column})"),
            Binding::Absent { dataset, column } => format!("absent({dataset}.{column})"),
            Binding::CountPresent { dataset, column } => {
                format!("count_present({dataset}.{column})")
            }
            Binding::CountAbsent { dataset, column } => {
                format!("count_absent({dataset}.{column})")
            }
            Binding::PresentValues { dataset, column } => {
                format!("present_values({dataset}.{column})")
            }
            Binding::SegmentStart { dataset, index } => {
                format!("segment_start({dataset} by {index})")
            }
            Binding::SegmentSize { dataset, index } => {
                format!("segment_size({dataset} by {index})")
            }
            Binding::Series {
                dataset,
                column,
                over,
                aggregate,
                fill,
            } => {
                let mut text = format!("series({} over {}", what(dataset, column), over.describe());
                if let Some(a) = aggregate {
                    text.push_str(&format!(", {}", a.name()));
                }
                if let Some(f) = fill {
                    text.push_str(&format!(", fill {f}"));
                }
                text + ")"
            }
            Binding::SeriesPresent {
                dataset,
                column,
                over,
            } => format!(
                "series_present({} over {})",
                what(dataset, column),
                over.describe()
            ),
            Binding::Cells {
                dataset,
                column,
                rows,
                cols,
                aggregate,
                fill,
            } => {
                let mut text = format!(
                    "cells({}, rows: {}, cols: {}",
                    what(dataset, column),
                    rows.describe(),
                    cols.describe()
                );
                if let Some(a) = aggregate {
                    text.push_str(&format!(", {}", a.name()));
                }
                if let Some(f) = fill {
                    text.push_str(&format!(", fill {f}"));
                }
                text + ")"
            }
            Binding::CellsPresent {
                dataset,
                column,
                rows,
                cols,
            } => format!(
                "cells_present({}, rows: {}, cols: {})",
                what(dataset, column),
                rows.describe(),
                cols.describe()
            ),
            Binding::EdgeCount(e)
            | Binding::EdgeFrom(e)
            | Binding::EdgeTo(e)
            | Binding::Adjacency(e)
            | Binding::Components(e)
            | Binding::Component(e)
            | Binding::IcarScale(e) => e.describe(self.kind()),
            Binding::Points(p) | Binding::Distances(p) => format!(
                "{}({}: {}, {}{})",
                self.kind(),
                p.dataset,
                p.lat,
                p.lon,
                if p.project { ", projected" } else { "" }
            ),
        }
    }

    /// The rank it produces where the kind alone decides it — `None` for a
    /// `value`, whose literal decides.
    pub(crate) fn rank(&self) -> Option<usize> {
        match self {
            Binding::Value { .. } => None,
            Binding::Count { .. }
            | Binding::Size { .. }
            | Binding::Width { .. }
            | Binding::CountPresent { .. }
            | Binding::CountAbsent { .. }
            | Binding::EdgeCount(_)
            | Binding::Components(_)
            | Binding::IcarScale(_) => Some(0),
            Binding::Column { .. }
            | Binding::Index { .. }
            | Binding::Present { .. }
            | Binding::Absent { .. }
            | Binding::PresentValues { .. }
            | Binding::SegmentStart { .. }
            | Binding::SegmentSize { .. }
            | Binding::Series { .. }
            | Binding::SeriesPresent { .. }
            | Binding::EdgeFrom(_)
            | Binding::EdgeTo(_)
            | Binding::Component(_) => Some(1),
            Binding::Columns { .. }
            | Binding::Design { .. }
            | Binding::Cells { .. }
            | Binding::CellsPresent { .. }
            | Binding::Adjacency(_)
            | Binding::Points(_)
            | Binding::Distances(_) => Some(2),
        }
    }

    /// Whether it can only produce reals, known without the data: a model
    /// matrix, a date scaled to a unit, a mean, a fraction to fill with, a
    /// scaling factor, and coordinates and distances.
    pub(crate) fn always_real(&self) -> bool {
        match self {
            Binding::Series {
                aggregate, fill, ..
            }
            | Binding::Cells {
                aggregate, fill, ..
            } => {
                *aggregate == Some(Aggregate::Mean)
                    || fill.as_ref().is_some_and(|f| f.as_i64().is_none())
            }
            _ => matches!(
                self,
                Binding::Design { .. }
                    | Binding::Column { time: Some(_), .. }
                    | Binding::IcarScale(_)
                    | Binding::Points(_)
                    | Binding::Distances(_)
            ),
        }
    }

    /// For a `series` or `cells`, how the rows of one position combine: as
    /// given, else `count` with no column and `refuse` with one.
    pub(crate) fn aggregate(&self) -> Aggregate {
        match self {
            Binding::Series {
                aggregate, column, ..
            }
            | Binding::Cells {
                aggregate, column, ..
            } => aggregate.unwrap_or(if column.is_some() {
                Aggregate::Refuse
            } else {
                Aggregate::Count
            }),
            _ => Aggregate::Count,
        }
    }
}

/// `main.amount`, or `main` when rows are counted.
fn what(dataset: &str, column: &Option<String>) -> String {
    match column {
        Some(c) => format!("{dataset}.{c}"),
        None => format!("count of {dataset}"),
    }
}

impl Along {
    /// `day`, `at → hour`, `fips → counties (match: fips)`.
    pub fn describe(&self) -> String {
        let mut text = match &self.column {
            Some(c) => format!("{c} → {}", self.dimension),
            None => self.dimension.clone(),
        };
        if let Some(m) = &self.match_column {
            text.push_str(&format!(" (match: {m})"));
        }
        text
    }
}

/// How a date becomes a real number: in which unit, counted from where.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeScale {
    /// `seconds`, `minutes`, `hours`, `days` or `weeks` — fixed lengths only,
    /// because a month is not a number of seconds.
    pub unit: String,
    /// `min` (the earliest value bound), `epoch` (1970-01-01 UTC) or a date or
    /// timestamp.
    pub origin: String,
}

impl TimeScale {
    /// The unit's length in seconds.
    pub(crate) fn seconds(&self) -> Result<i64> {
        match self.unit.trim().trim_end_matches('s') {
            "second" => Ok(1),
            "minute" => Ok(60),
            "hour" => Ok(3_600),
            "day" => Ok(86_400),
            "week" => Ok(604_800),
            "month" | "quarter" | "year" => Err(Error::invalid(format!(
                "`time.unit` `{}` is a calendar unit with no fixed length; use `days` (or a \
                 time grid, whose steps follow the calendar)",
                self.unit
            ))),
            _ => Err(Error::invalid(format!(
                "`time.unit` `{}` is not one of `seconds`, `minutes`, `hours`, `days`, `weeks`",
                self.unit
            ))),
        }
    }

    /// The origin, given the bound values' earliest.
    pub(crate) fn origin(&self, earliest: Option<i64>) -> Result<i64> {
        match self.origin.trim() {
            "min" => Ok(earliest.unwrap_or(0)),
            "epoch" => Ok(0),
            other => parse_instant(other).ok_or_else(|| {
                Error::invalid(format!(
                    "`time.origin` `{other}` is not `min`, `epoch`, a date (`2024-01-31`) or a \
                     timestamp (`2024-01-31T12:00:00Z`)"
                ))
            }),
        }
    }
}

/// A date or a timestamp as epoch seconds, UTC: `2024-01-31`,
/// `2024-01-31T12:00:00Z`, `2024-01-31 12:00:00` (read as UTC).
pub(crate) fn parse_instant(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(t.timestamp());
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(t.and_utc().timestamp());
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc().timestamp())
}

/// A dimension the configuration declares (§8). Every dataset is a rows
/// dimension without being declared.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DimensionSpec {
    /// The distinct non-null values of a column, sorted.
    Values {
        /// The dataset.
        dataset: String,
        /// Its column.
        column: String,
    },
    /// The steps of a regular calendar grid over a date column, plus a
    /// horizon. Also exposes `name.future`, the horizon alone.
    TimeGrid {
        /// The dataset.
        dataset: String,
        /// Its date column.
        column: String,
        /// `day`, `3 hours`, `1 month` — a count (1 when absent) and one of
        /// `minutes`, `hours`, `days`, `weeks`, `months`, `quarters`, `years`.
        step: String,
        /// The first step; the first value floored to the step when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<String>,
        /// The last observed instant; the last value when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<String>,
        /// Extra steps after the end, for a forecast.
        #[serde(default, skip_serializing_if = "is_zero")]
        horizon: u32,
    },
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl DimensionSpec {
    /// The dataset it is over.
    pub fn dataset(&self) -> &str {
        match self {
            DimensionSpec::Values { dataset, .. } | DimensionSpec::TimeGrid { dataset, .. } => {
                dataset
            }
        }
    }

    /// The column it is over.
    pub fn column(&self) -> &str {
        match self {
            DimensionSpec::Values { column, .. } | DimensionSpec::TimeGrid { column, .. } => column,
        }
    }
}

/// A time grid's step: so many of a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Step {
    pub count: u32,
    pub unit: StepUnit,
}

/// The units a grid steps in. The last three step by the calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Step {
    /// `day`, `3 hours`, `1 month`.
    pub(crate) fn parse(text: &str) -> Result<Step> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let (count, unit) = match words.as_slice() {
            [unit] => (1, *unit),
            [count, unit] => (
                count
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`step` `{text}`: `{count}` is not a positive whole number"
                        ))
                    })?,
                *unit,
            ),
            _ => {
                return Err(Error::invalid(format!(
                    "`step` `{text}` must be a unit (`day`) or a count and a unit (`3 hours`)"
                )));
            }
        };
        let unit = match unit.to_ascii_lowercase().trim_end_matches('s') {
            "minute" => StepUnit::Minute,
            "hour" => StepUnit::Hour,
            "day" => StepUnit::Day,
            "week" => StepUnit::Week,
            "month" => StepUnit::Month,
            "quarter" => StepUnit::Quarter,
            "year" => StepUnit::Year,
            _ => {
                return Err(Error::invalid(format!(
                    "`step` `{text}`: `{unit}` is not one of minutes, hours, days, weeks, \
                     months, quarters, years"
                )));
            }
        };
        Ok(Step { count, unit })
    }
}

/// What happens to a row that cannot be bound as it stands (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Refuse the fit, naming the column, the count and the first row.
    #[default]
    Refuse,
    /// Drop the row before anything of the dataset is counted, indexed or
    /// bound, and report how many.
    Drop,
}

/// One dataset's two policies: for a null in a bound column, and for an
/// `index` value that is not a position of its dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    /// A null in a `column`, `columns`, `design` or `index` column.
    #[serde(default)]
    pub nulls: Policy,
    /// An `index` value that is not in its dimension.
    #[serde(default)]
    pub unknown: Policy,
}

/// Everything the configuration says about binding, parsed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Spec {
    /// Each `data` variable's binding.
    pub bindings: BTreeMap<String, Binding>,
    /// The declared dimensions.
    pub dimensions: BTreeMap<String, DimensionSpec>,
    /// Each dataset's policies; absent means both `refuse`.
    pub policies: BTreeMap<String, Policies>,
}

impl Spec {
    /// The configuration's binding keys, parsed — each mistake a sentence
    /// naming what it is about.
    pub(crate) fn parse(config: &Attrs) -> Result<Spec> {
        let mut spec = Spec::default();
        for (name, binding) in object(config, BINDINGS_KEY)? {
            spec.bindings
                .insert(name.clone(), parse_binding(name, binding)?);
        }
        for (name, dimension) in object(config, DIMENSIONS_KEY)? {
            let parsed = serde_json::from_value(dimension.clone()).map_err(|e| {
                Error::invalid(format!("dimension `{name}`: {}", serde_sentence(&e)))
            })?;
            spec.dimensions.insert(name.clone(), parsed);
        }
        for (name, policies) in object(config, POLICIES_KEY)? {
            let parsed = serde_json::from_value(policies.clone()).map_err(|e| {
                Error::invalid(format!(
                    "the policies of dataset `{name}`: {} (each of `nulls` and `unknown` is \
                     `refuse` or `drop`)",
                    serde_sentence(&e)
                ))
            })?;
            spec.policies.insert(name.clone(), parsed);
        }
        Ok(spec)
    }

    /// The policies of `dataset`.
    pub(crate) fn policies(&self, dataset: &str) -> Policies {
        self.policies.get(dataset).copied().unwrap_or_default()
    }
}

/// One binding, parsed.
pub(crate) fn parse_binding(name: &str, json: &Json) -> Result<Binding> {
    serde_json::from_value(json.clone())
        .map_err(|e| Error::invalid(format!("the binding of `{name}`: {}", serde_sentence(&e))))
}

/// A serde error without its "at line 1 column 40", which means nothing on a
/// form.
fn serde_sentence(e: &serde_json::Error) -> String {
    let text = e.to_string();
    match text.find(" at line ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// The object under `key`, or an empty one.
fn object<'a>(config: &'a Attrs, key: &str) -> Result<&'a serde_json::Map<String, Json>> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, Json>> = std::sync::OnceLock::new();
    match config.get(key) {
        None | Some(Json::Null) => Ok(EMPTY.get_or_init(serde_json::Map::new)),
        Some(Json::Object(map)) => Ok(map),
        Some(_) => Err(Error::invalid(format!("`{key}` must be an object"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_binding_parses_from_the_json_the_form_writes() {
        let b: Binding = serde_json::from_value(json!({
            "kind": "index", "dataset": "main", "column": "fips",
            "dimension": "counties", "match": "fips"
        }))
        .expect("parse");
        assert_eq!(b.describe(), "index(main.fips → counties, match: fips)");
        assert_eq!(b.policed_columns(), ["fips"]);
        let err = parse_binding("y", &json!({"kind": "colum", "dataset": "main"})).unwrap_err();
        assert!(
            err.to_string()
                .contains("the binding of `y`: unknown variant `colum`"),
            "{err}"
        );
        let err = parse_binding("y", &json!({"kind": "series", "dataset": "main"})).unwrap_err();
        assert!(err.to_string().contains("missing field `over`"), "{err}");
        let err = parse_binding("y", &json!({"kind": "count"})).unwrap_err();
        assert!(err.to_string().contains("missing field `dataset`"), "{err}");
    }

    #[test]
    fn the_structured_kinds_parse_and_describe_themselves() {
        let b = parse_binding(
            "y",
            &json!({"kind": "series", "dataset": "main", "column": "amount",
                    "over": {"dimension": "day"}, "fill": 0}),
        )
        .unwrap();
        assert_eq!(b.describe(), "series(main.amount over day, fill 0)");
        assert_eq!(b.aggregate(), Aggregate::Refuse);
        assert_eq!(b.rank(), Some(1));
        let b = parse_binding(
            "Y",
            &json!({"kind": "cells", "dataset": "main", "column": "value",
                    "rows": {"dimension": "sensors", "column": "sensor"},
                    "cols": {"dimension": "hour", "column": "at"}, "aggregate": "mean"}),
        )
        .unwrap();
        assert_eq!(
            b.describe(),
            "cells(main.value, rows: sensor → sensors, cols: at → hour, mean)"
        );
        assert!(b.always_real());
        assert_eq!(b.columns(), ["value", "sensor", "at"]);
        let edges = json!({"kind": "edge_from", "dataset": "adjacency", "from": "a", "to": "b",
                           "dimension": "regions"});
        let b = parse_binding("node1", &edges).unwrap();
        assert_eq!(b.describe(), "edge_from(adjacency: a — b → regions)");
        assert_eq!(b.dataset(), Some("adjacency"));
        assert_eq!(b.edges().map(|e| e.symmetric), Some(Symmetric::Dedupe));
        let mut typo = edges.clone();
        typo["symetric"] = json!("keep");
        let err = parse_binding("node1", &typo).unwrap_err();
        assert!(
            err.to_string().contains("unknown field `symetric`"),
            "{err}"
        );
        let b = parse_binding(
            "xy",
            &json!({"kind": "points", "dataset": "sites", "lat": "lat", "lon": "lon",
                    "project": true}),
        )
        .unwrap();
        assert_eq!(b.describe(), "points(sites: lat, lon, projected)");
        assert_eq!(serde_json::to_value(&b).unwrap()["kind"], json!("points"));
    }

    #[test]
    fn a_step_is_a_count_and_a_unit() {
        assert_eq!(
            Step::parse("day").unwrap(),
            Step {
                count: 1,
                unit: StepUnit::Day
            }
        );
        assert_eq!(
            Step::parse("3 Hours").unwrap(),
            Step {
                count: 3,
                unit: StepUnit::Hour
            }
        );
        assert!(Step::parse("0 days").is_err());
        assert!(Step::parse("fortnight").is_err());
    }

    #[test]
    fn an_instant_is_a_date_or_a_timestamp_in_utc() {
        assert_eq!(parse_instant("1970-01-02"), Some(86_400));
        assert_eq!(parse_instant("1970-01-01T01:00:00Z"), Some(3_600));
        assert_eq!(parse_instant("1970-01-01 00:01:00"), Some(60));
        assert_eq!(parse_instant("yesterday"), None);
    }
}
