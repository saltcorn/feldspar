//! Dimensions: where a 1-based index comes from (Stan TODO §8).
//!
//! Every Stan index is a position in `1..n`; a database has keys. A
//! [`Dimension`] is the mapping between the two, and its [`Coordinates`] — the
//! key and the label of every position — are stored on the instance, because
//! positions are the instance's private business: a county inserted between two
//! fits may shift every position after it, and everything that leaves the host
//! speaks keys.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Timelike, Utc};
use sc_error::{Error, Result};
use serde_json::{Number, Value as Json};

use super::spec::{Step, StepUnit};
use crate::frame::Column;

/// The most steps a time grid may have: a grid of minutes over fifty years is
/// a mistake, not a model.
pub const MAX_GRID_STEPS: usize = 1_000_000;

/// What a dimension's positions are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionKind {
    /// A dataset's rows, in its order.
    Rows,
    /// A column's distinct non-null values, sorted.
    Values,
    /// The steps of a time grid (or of its horizon, for `name.future`).
    TimeGrid,
}

/// One dimension's coordinates: a key and a label per position, in order.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DimensionCoordinates {
    /// The dimension's name — a dataset's, a declared one's, or `day.future`.
    pub name: String,
    /// What its positions are.
    pub kind: DimensionKind,
    /// The dataset it is over.
    pub dataset: String,
    /// For a values dimension or a time grid, the column it is over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// Position `i + 1`'s key: a row's primary key, a value, or a step's start
    /// instant (RFC 3339, UTC).
    pub keys: Vec<Json>,
    /// Position `i + 1`'s label: the related dataset's label formula, else the
    /// key as text; a value's text; a step's date (or instant).
    pub labels: Vec<String>,
}

impl DimensionCoordinates {
    /// How many positions.
    pub fn size(&self) -> usize {
        self.keys.len()
    }
}

/// A `design` binding's recorded encoding: the column names are what label
/// whatever the program sizes by its `width` (§9).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DesignCoordinates {
    /// The model matrix's column names — `floor`, `region=north`.
    pub columns: Vec<String>,
    /// The fitted [`Encoding`](crate::Encoding), as stored.
    pub encoding: Json,
}

/// Every dimension's coordinates and every design's columns — what an instance
/// stores so its positions can be spoken of by key and label.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Coordinates {
    /// The dimensions, datasets first in the model's order, then the declared
    /// ones by name.
    pub dimensions: Vec<DimensionCoordinates>,
    /// Each `design`-bound variable's columns, by variable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub designs: BTreeMap<String, DesignCoordinates>,
}

impl Coordinates {
    /// The dimension called `name`.
    pub fn dimension(&self, name: &str) -> Option<&DimensionCoordinates> {
        self.dimensions.iter().find(|d| d.name == name)
    }
}

/// A dimension while binding: its coordinates and how a value finds its
/// position.
#[derive(Debug, Clone)]
pub(crate) struct Dimension {
    pub coords: DimensionCoordinates,
    lookup: Lookup,
}

#[derive(Debug, Clone)]
enum Lookup {
    /// A value's text → its 0-based position.
    Text(HashMap<String, usize>),
    /// A time grid's arithmetic, with the positions it covers.
    Grid { grid: Grid, from: usize, to: usize },
}

impl Dimension {
    /// How many positions.
    pub(crate) fn size(&self) -> usize {
        self.coords.size()
    }

    /// The 1-based position of row `i`'s value of `column`, or `None` when it
    /// is not one of this dimension's — the `unknown` policy's case. A null
    /// is `None` too; the caller has already applied the `nulls` policy.
    pub(crate) fn position(&self, column: &Column, i: usize) -> Option<usize> {
        match &self.lookup {
            Lookup::Text(map) => cell_text(column, i).and_then(|t| map.get(&t).map(|p| p + 1)),
            Lookup::Grid { grid, from, to } => {
                let Column::Date(v) = column else {
                    return None;
                };
                let k = grid.step_of(v.get(i).copied().flatten()?)?;
                (*from..*to).contains(&k).then(|| k - from + 1)
            }
        }
    }

    /// A rows dimension over a dataset's final rows: each row's key (from the
    /// frame's canonical keys, or its position when the table has no single
    /// primary key) and its label — the label formula's value where the
    /// dataset has one, the key's text otherwise.
    pub(crate) fn rows(
        name: &str,
        keys: &[String],
        labels: Option<&[String]>,
        rows: usize,
    ) -> Dimension {
        let texts: Vec<String> = if keys.is_empty() {
            (1..=rows).map(|i| i.to_string()).collect()
        } else {
            keys.iter().map(|k| key_text(k).to_owned()).collect()
        };
        let json_keys: Vec<Json> = if keys.is_empty() {
            (1..=rows).map(|i| Json::Number(Number::from(i))).collect()
        } else {
            keys.iter().map(|k| key_json(k)).collect()
        };
        let lookup = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (t.clone(), i))
            .collect();
        Dimension {
            coords: DimensionCoordinates {
                name: name.to_owned(),
                kind: DimensionKind::Rows,
                dataset: name.to_owned(),
                column: None,
                keys: json_keys,
                labels: labels.map_or(texts, <[String]>::to_vec),
            },
            lookup: Lookup::Text(lookup),
        }
    }

    /// The lookup matching a rows dimension's positions by another of its
    /// dataset's columns (`match: fips`), refused when a value is on two rows.
    pub(crate) fn matching(
        &self,
        dataset: &str,
        column_name: &str,
        column: &Column,
    ) -> Result<Dimension> {
        let mut map = HashMap::new();
        for i in 0..column.len() {
            if let Some(text) = cell_text(column, i) {
                if let Some(first) = map.insert(text.clone(), i) {
                    return Err(Error::invalid(format!(
                        "`{column_name}` of `{dataset}` cannot be matched on, because `{text}` \
                         is on two of its rows ({} and {})",
                        self.coords.labels[first], self.coords.labels[i]
                    )));
                }
            }
        }
        Ok(Dimension {
            coords: self.coords.clone(),
            lookup: Lookup::Text(map),
        })
    }

    /// A values dimension: the distinct non-null values of `column`, sorted —
    /// numbers numerically, text by code point (never by locale: the order
    /// must not change with the server's environment), `false < true`.
    pub(crate) fn values(
        name: &str,
        dataset: &str,
        column_name: &str,
        column: &Column,
    ) -> Dimension {
        let mut keys: Vec<(Sort, Json, String)> = Vec::new();
        let mut seen = HashMap::new();
        for i in 0..column.len() {
            let Some(text) = cell_text(column, i) else {
                continue;
            };
            if seen.insert(text.clone(), ()).is_some() {
                continue;
            }
            let (sort, json) = match column {
                Column::Int(v) => {
                    let n = v[i].unwrap_or_default();
                    (Sort::Number(n as f64), Json::Number(Number::from(n)))
                }
                Column::Date(v) => {
                    let n = v[i].unwrap_or_default();
                    (Sort::Number(n as f64), Json::String(instant_text(n)))
                }
                Column::Float(v) => {
                    let x = v[i].unwrap_or_default();
                    (Sort::Number(x), super::tensor::real(x))
                }
                Column::Bool(v) => {
                    let b = v[i].unwrap_or_default();
                    (Sort::Number(f64::from(u8::from(b))), Json::Bool(b))
                }
                _ => (Sort::Text(text.clone()), Json::String(text.clone())),
            };
            keys.push((sort, json, text));
        }
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        let lookup = keys
            .iter()
            .enumerate()
            .map(|(i, (_, _, t))| (t.clone(), i))
            .collect();
        let labels = keys
            .iter()
            .map(|(_, json, text)| match json {
                Json::String(s) => s.clone(),
                _ => text.clone(),
            })
            .collect();
        Dimension {
            coords: DimensionCoordinates {
                name: name.to_owned(),
                kind: DimensionKind::Values,
                dataset: dataset.to_owned(),
                column: Some(column_name.to_owned()),
                keys: keys.into_iter().map(|(_, json, _)| json).collect(),
                labels,
            },
            lookup: Lookup::Text(lookup),
        }
    }

    /// A time grid over `column`, and its `.future` slice.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn time_grid(
        name: &str,
        dataset: &str,
        column_name: &str,
        column: &Column,
        step: Step,
        start: Option<i64>,
        end: Option<i64>,
        horizon: u32,
    ) -> Result<(Dimension, Dimension)> {
        let values: Vec<i64> = match column {
            Column::Date(v) => v.iter().flatten().copied().collect(),
            Column::Null(_) => Vec::new(),
            other => {
                return Err(Error::invalid(format!(
                    "dimension `{name}`: a time grid is over a date or timestamp column, and \
                     `{column_name}` of `{dataset}` is {}",
                    other.kind().name()
                )));
            }
        };
        let first = values.iter().min().copied();
        let last = values.iter().max().copied();
        let start = match (start, first) {
            (Some(s), _) => s,
            (None, Some(f)) => floor(f, step.unit),
            (None, None) => {
                return Err(Error::invalid(format!(
                    "dimension `{name}`: `{column_name}` of `{dataset}` has no values, so the \
                     time grid has no start; give `start` and `end`"
                )));
            }
        };
        let end = end.or(last).unwrap_or(start);
        if end < start {
            return Err(Error::invalid(format!(
                "dimension `{name}`: its end ({}) is before its start ({})",
                instant_text(end),
                instant_text(start)
            )));
        }
        let grid = Grid { start, step };
        let observed = grid
            .step_of(end)
            .map(|k| k + 1)
            .ok_or_else(|| Error::invalid(format!("dimension `{name}`: its grid overflows")))?;
        let total = observed + horizon as usize;
        if total > MAX_GRID_STEPS {
            return Err(Error::invalid(format!(
                "dimension `{name}` would have {total} steps, more than the {MAX_GRID_STEPS} \
                 allowed: use a longer step or a shorter range"
            )));
        }
        let mut keys = Vec::with_capacity(total);
        let mut labels = Vec::with_capacity(total);
        for k in 0..total {
            let at = grid
                .start_of(k)
                .ok_or_else(|| Error::invalid(format!("dimension `{name}`: its grid overflows")))?;
            keys.push(Json::String(instant_text(at)));
            labels.push(step_label(at, step.unit));
        }
        let make = |dim_name: String, from: usize, to: usize| Dimension {
            coords: DimensionCoordinates {
                name: dim_name,
                kind: DimensionKind::TimeGrid,
                dataset: dataset.to_owned(),
                column: Some(column_name.to_owned()),
                keys: keys[from..to].to_vec(),
                labels: labels[from..to].to_vec(),
            },
            lookup: Lookup::Grid { grid, from, to },
        };
        Ok((
            make(name.to_owned(), 0, total),
            make(format!("{name}.future"), observed, total),
        ))
    }
}

/// How values of a values dimension sort.
#[derive(Debug, Clone, PartialEq)]
enum Sort {
    Number(f64),
    Text(String),
}

impl Eq for Sort {}

impl PartialOrd for Sort {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Sort {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Sort::Number(a), Sort::Number(b)) => a.total_cmp(b),
            // Rust orders strings by their UTF-8 bytes, which is code-point
            // order.
            (Sort::Text(a), Sort::Text(b)) => a.cmp(b),
            (Sort::Number(_), Sort::Text(_)) => std::cmp::Ordering::Less,
            (Sort::Text(_), Sort::Number(_)) => std::cmp::Ordering::Greater,
        }
    }
}

/// A regular grid: its first step's start, and its step.
#[derive(Debug, Clone, Copy)]
struct Grid {
    start: i64,
    step: Step,
}

impl Grid {
    /// Which step `t` falls in, counting from 0; `None` before the start.
    fn step_of(&self, t: i64) -> Option<usize> {
        if t < self.start {
            return None;
        }
        let k = match fixed_seconds(self.step.unit) {
            Some(secs) => (t - self.start) / (secs * i64::from(self.step.count)),
            None => {
                let months = i64::from(self.step.count) * months_per(self.step.unit);
                let (from, to) = (utc(self.start)?, utc(t)?);
                let between = (i64::from(to.year()) * 12 + i64::from(to.month0()))
                    - (i64::from(from.year()) * 12 + i64::from(from.month0()));
                let mut k = between.div_euclid(months);
                // A start mid-month: the step that month begins may still be
                // ahead of `t`.
                while k > 0 && self.start_of(k as usize)? > t {
                    k -= 1;
                }
                k
            }
        };
        usize::try_from(k).ok()
    }

    /// The start of step `k`.
    fn start_of(&self, k: usize) -> Option<i64> {
        match fixed_seconds(self.step.unit) {
            Some(secs) => {
                let k = i64::try_from(k).ok()?;
                k.checked_mul(secs * i64::from(self.step.count))?
                    .checked_add(self.start)
            }
            None => {
                let months = u32::try_from(k)
                    .ok()?
                    .checked_mul(self.step.count)?
                    .checked_mul(u32::try_from(months_per(self.step.unit)).ok()?)?;
                Some(
                    utc(self.start)?
                        .checked_add_months(Months::new(months))?
                        .timestamp(),
                )
            }
        }
    }
}

fn fixed_seconds(unit: StepUnit) -> Option<i64> {
    match unit {
        StepUnit::Minute => Some(60),
        StepUnit::Hour => Some(3_600),
        StepUnit::Day => Some(86_400),
        StepUnit::Week => Some(604_800),
        StepUnit::Month | StepUnit::Quarter | StepUnit::Year => None,
    }
}

fn months_per(unit: StepUnit) -> i64 {
    match unit {
        StepUnit::Quarter => 3,
        StepUnit::Year => 12,
        _ => 1,
    }
}

fn utc(t: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(t, 0).single()
}

/// `t` floored to the start of its unit, in UTC: the minute, the hour, the
/// day, the ISO week (Monday), the month, the quarter, the year.
fn floor(t: i64, unit: StepUnit) -> i64 {
    match unit {
        StepUnit::Minute => t - t.rem_euclid(60),
        StepUnit::Hour => t - t.rem_euclid(3_600),
        StepUnit::Day => t - t.rem_euclid(86_400),
        // 1970-01-05 was a Monday.
        StepUnit::Week => t - (t - 4 * 86_400).rem_euclid(604_800),
        StepUnit::Month | StepUnit::Quarter | StepUnit::Year => {
            let Some(at) = utc(t) else { return t };
            let month = match unit {
                StepUnit::Month => at.month(),
                StepUnit::Quarter => (at.month0() / 3) * 3 + 1,
                _ => 1,
            };
            NaiveDate::from_ymd_opt(at.year(), month, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map_or(t, |d| d.and_utc().timestamp())
        }
    }
}

/// An instant as RFC 3339 in UTC, `Z`-suffixed.
pub(crate) fn instant_text(t: i64) -> String {
    utc(t).map_or_else(
        || t.to_string(),
        |d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// A step's label: its date when the grid steps in days or longer and the
/// step starts at midnight, its instant otherwise.
fn step_label(t: i64, unit: StepUnit) -> String {
    match utc(t) {
        Some(d)
            if !matches!(unit, StepUnit::Minute | StepUnit::Hour)
                && d.hour() == 0
                && d.minute() == 0
                && d.second() == 0 =>
        {
            d.format("%Y-%m-%d").to_string()
        }
        _ => instant_text(t),
    }
}

/// Row `i`'s value as the text a lookup compares — the rendering a primary
/// key's canonical form carries after its kind, so `27001` in a foreign-key
/// column finds the row whose key is `int:27001`. `None` for a null.
pub(crate) fn cell_text(column: &Column, i: usize) -> Option<String> {
    match column {
        Column::Float(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Bool(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Str(v) => v.get(i).cloned().flatten(),
        Column::Date(v) => v.get(i).copied().flatten().map(instant_text),
        Column::Null(_) => None,
    }
}

/// A canonical key's text after its kind: `int:27001` → `27001`.
pub(crate) fn key_text(key: &str) -> &str {
    key.split_once(':').map_or(key, |(_, text)| text)
}

/// A canonical key as the JSON value it was: `int:27001` → `27001`,
/// `text:north` → `"north"`.
pub(crate) fn key_json(key: &str) -> Json {
    let (kind, text) = key.split_once(':').unwrap_or(("text", key));
    match kind {
        "int" => text.parse::<i64>().map_or_else(
            |_| Json::String(text.to_owned()),
            |n| Json::Number(Number::from(n)),
        ),
        "float" | "decimal" => text
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map_or_else(|| Json::String(text.to_owned()), Json::Number),
        "bool" => text
            .parse::<bool>()
            .map_or_else(|_| Json::String(text.to_owned()), Json::Bool),
        _ => Json::String(text.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bind::spec::parse_instant;

    fn at(text: &str) -> i64 {
        parse_instant(text).expect(text)
    }

    fn dates(values: &[&str]) -> Column {
        Column::Date(values.iter().map(|v| Some(at(v))).collect())
    }

    #[test]
    fn values_sort_numerically_by_code_point_and_false_first() {
        let numbers = Column::Int(vec![Some(10), Some(9), None, Some(10), Some(-1)]);
        let d = Dimension::values("n", "main", "n", &numbers);
        assert_eq!(d.coords.labels, ["-1", "9", "10"]);
        assert_eq!(d.position(&numbers, 0), Some(3));
        assert_eq!(d.position(&numbers, 2), None);
        let text = Column::Str(vec![
            Some("b".into()),
            Some("B".into()),
            Some("é".into()),
            Some("a".into()),
        ]);
        // Code point order: uppercase before lowercase, accents after both.
        assert_eq!(
            Dimension::values("t", "main", "t", &text).coords.labels,
            ["B", "a", "b", "é"]
        );
        let flags = Column::Bool(vec![Some(true), Some(false)]);
        let d = Dimension::values("f", "main", "f", &flags);
        assert_eq!(d.coords.keys, [Json::Bool(false), Json::Bool(true)]);
    }

    #[test]
    fn a_rows_dimension_is_found_by_the_keys_text() {
        let keys = vec!["int:27001".to_owned(), "int:27003".to_owned()];
        let labels = vec!["Aitkin".to_owned(), "Anoka".to_owned()];
        let d = Dimension::rows("counties", &keys, Some(&labels), 2);
        assert_eq!(
            d.coords.keys,
            [serde_json::json!(27001), serde_json::json!(27003)]
        );
        let fk = Column::Int(vec![Some(27003), Some(99)]);
        assert_eq!(d.position(&fk, 0), Some(2));
        assert_eq!(d.position(&fk, 1), None);
        // A float foreign key renders as the integer it is.
        assert_eq!(d.position(&Column::Float(vec![Some(27001.0)]), 0), Some(1));
    }

    #[test]
    fn a_daily_grid_floors_its_start_and_slices_its_future() {
        let column = dates(&["2024-01-01T10:00:00Z", "2024-01-03T23:59:59Z"]);
        let (day, future) = Dimension::time_grid(
            "day",
            "main",
            "at",
            &column,
            Step::parse("day").unwrap(),
            None,
            None,
            2,
        )
        .unwrap();
        assert_eq!(
            day.coords.labels,
            [
                "2024-01-01",
                "2024-01-02",
                "2024-01-03",
                "2024-01-04",
                "2024-01-05"
            ]
        );
        assert_eq!(future.coords.labels, ["2024-01-04", "2024-01-05"]);
        assert_eq!(day.position(&column, 0), Some(1));
        assert_eq!(day.position(&column, 1), Some(3));
        let later = dates(&["2024-01-05T12:00:00Z", "2024-01-06T00:00:00Z"]);
        assert_eq!(future.position(&later, 0), Some(2));
        assert_eq!(day.position(&later, 1), None);
    }

    #[test]
    fn calendar_steps_follow_the_calendar() {
        let column = dates(&["2024-01-31", "2024-02-29", "2024-05-15"]);
        let (month, _) = Dimension::time_grid(
            "month",
            "main",
            "at",
            &column,
            Step::parse("month").unwrap(),
            None,
            None,
            0,
        )
        .unwrap();
        assert_eq!(
            month.coords.labels,
            [
                "2024-01-01",
                "2024-02-01",
                "2024-03-01",
                "2024-04-01",
                "2024-05-01"
            ]
        );
        assert_eq!(month.position(&column, 1), Some(2));
        assert_eq!(month.position(&column, 2), Some(5));
        let (quarter, _) = Dimension::time_grid(
            "q",
            "main",
            "at",
            &column,
            Step::parse("quarter").unwrap(),
            None,
            None,
            1,
        )
        .unwrap();
        assert_eq!(
            quarter.coords.labels,
            ["2024-01-01", "2024-04-01", "2024-07-01"]
        );
        // Weeks start on Monday: 2024-01-31 was a Wednesday.
        let (week, _) = Dimension::time_grid(
            "w",
            "main",
            "at",
            &column,
            Step::parse("week").unwrap(),
            None,
            None,
            0,
        )
        .unwrap();
        assert_eq!(
            week.coords.labels.first().map(String::as_str),
            Some("2024-01-29")
        );
    }

    #[test]
    fn a_given_start_and_end_bound_the_grid() {
        let column = dates(&["2024-01-01", "2024-01-10"]);
        let (day, _) = Dimension::time_grid(
            "day",
            "main",
            "at",
            &column,
            Step::parse("day").unwrap(),
            Some(at("2024-01-02")),
            Some(at("2024-01-04")),
            0,
        )
        .unwrap();
        assert_eq!(day.size(), 3);
        assert_eq!(day.position(&column, 0), None);
        assert_eq!(day.position(&column, 1), None);
        let err = Dimension::time_grid(
            "m",
            "main",
            "at",
            &column,
            Step::parse("minute").unwrap(),
            Some(0),
            Some(at("2024-01-01")),
            0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("more than the 1000000 allowed"),
            "{err}"
        );
    }

    #[test]
    fn a_canonical_key_is_read_back_as_the_value_it_was() {
        assert_eq!(key_json("int:5"), serde_json::json!(5));
        assert_eq!(key_json("text:a:b"), serde_json::json!("a:b"));
        assert_eq!(key_json("uuid:0f"), serde_json::json!("0f"));
        assert_eq!(key_text("text:a:b"), "a:b");
    }
}
