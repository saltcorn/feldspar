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
}

/// The binding kinds of Phase 6 (§12), named so a configuration using one is
/// told it is not here yet rather than that it is a typo.
const STRUCTURED_KINDS: [&str; 13] = [
    "series",
    "series_present",
    "cells",
    "cells_present",
    "edge_count",
    "edge_from",
    "edge_to",
    "adjacency",
    "components",
    "component",
    "icar_scale",
    "points",
    "distances",
];

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
        }
    }

    /// The dataset it reads, for the kinds that read one.
    pub fn dataset(&self) -> Option<&str> {
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
            | Binding::SegmentSize { dataset, .. } => Some(dataset),
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
            | Binding::CountAbsent { .. } => Some(0),
            Binding::Column { .. }
            | Binding::Index { .. }
            | Binding::Present { .. }
            | Binding::Absent { .. }
            | Binding::PresentValues { .. }
            | Binding::SegmentStart { .. }
            | Binding::SegmentSize { .. } => Some(1),
            Binding::Columns { .. } | Binding::Design { .. } => Some(2),
        }
    }

    /// Whether it can only produce reals, known without the data: a model
    /// matrix, and a date scaled to a unit.
    pub(crate) fn always_real(&self) -> bool {
        matches!(
            self,
            Binding::Design { .. } | Binding::Column { time: Some(_), .. }
        )
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

/// One binding, parsed, with the Phase 6 kinds named as such.
fn parse_binding(name: &str, json: &Json) -> Result<Binding> {
    let kind = json.get("kind").and_then(Json::as_str).unwrap_or_default();
    if STRUCTURED_KINDS.contains(&kind) {
        return Err(Error::invalid(format!(
            "the binding of `{name}`: `{kind}` is one of the time-series and spatial kinds, \
             which are not available yet"
        )));
    }
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
        let err = parse_binding("y", &json!({"kind": "series"})).unwrap_err();
        assert!(err.to_string().contains("not available yet"), "{err}");
        let err = parse_binding("y", &json!({"kind": "count"})).unwrap_err();
        assert!(err.to_string().contains("missing field `dataset`"), "{err}");
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
