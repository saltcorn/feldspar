//! The materialised dataset: a **columnar**, **bounded** table of values
//! (TODO §9).
//!
//! Columnar because every consumer wants a column: the encoder standardises
//! one, the splitter indexes rows across all of them, and a numeric matrix is
//! built column-major anyway. It is also what crosses a module seam — a
//! 50 000 × 12 dataset is twelve JSON arrays and not 50 000 JSON objects with
//! the same twelve keys repeated, and on the Python side it lands as something
//! `numpy.asarray` takes directly.
//!
//! Two shapes here are wider than the sketch in §9, and both are deliberate:
//!
//! - **[`Column::Date`]** is its own variant rather than an `Int`. A date is
//!   already epoch seconds by the time it is in a frame, so the cast is not what
//!   the variant buys — what it buys is that the encoder can *tell*, and record
//!   on the instance, that this column was a date. Without it a date would
//!   arrive as an integer and be standardised or one-hot encoded by whatever
//!   rule integers get, which is a silent difference between fit and predict.
//! - **[`Column::Null`] carries its length**, so a frame whose columns are all
//!   the same height stays checkable. A column of nothing but nulls is a real
//!   answer (an outer-joined path that matched nothing, an aggregation over no
//!   rows) and it has to keep the frame rectangular.
//!
//! A frame also carries its **keys**: the canonical rendering of each row's
//! primary key, which is what [`Split`](crate::Split) hashes. They are the
//! host's business and never cross a provider seam — a provider is handed
//! numbers, and the question "was this row in the training set" is the host's to
//! answer.

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::{Map, Number, Value as Json};

/// One column of a [`Frame`], as the values a provider will be given.
///
/// The variant is the column's *type*: it is decided once, when the frame is
/// built from query values, and everything downstream reads it rather than
/// re-deciding per row.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Floating point — `float`, `numeric`, and any integer column that shares
    /// its column with a float.
    Float(Vec<Option<f64>>),
    /// Signed integer.
    Int(Vec<Option<i64>>),
    /// Boolean.
    Bool(Vec<Option<bool>>),
    /// Text — and every value with no numeric reading (uuid, json, bytes, time),
    /// rendered rather than dropped, because a categorical feature is exactly
    /// what those usually are.
    Str(Vec<Option<String>>),
    /// A date or timestamp as **epoch seconds**, UTC.
    Date(Vec<Option<i64>>),
    /// Every value in this column was null. The length is carried so the frame
    /// stays rectangular.
    Null(usize),
}

/// A column's type, without its values — what a provider's `config_spec` is
/// handed to build its form against (a label picker offers the numeric columns;
/// a classification target offers the rest).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    /// [`Column::Float`].
    Float,
    /// [`Column::Int`].
    Int,
    /// [`Column::Bool`].
    Bool,
    /// [`Column::Str`].
    Str,
    /// [`Column::Date`].
    Date,
    /// [`Column::Null`] — nothing but nulls, so nothing is known.
    Null,
}

impl ColumnType {
    /// The stable wire name, matching the serde tag and the frame's JSON.
    pub fn name(self) -> &'static str {
        match self {
            ColumnType::Float => "float",
            ColumnType::Int => "int",
            ColumnType::Bool => "bool",
            ColumnType::Str => "str",
            ColumnType::Date => "date",
            ColumnType::Null => "null",
        }
    }

    /// Whether this column is a number a provider can use as-is — the test a
    /// regression's label picker applies.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            ColumnType::Float | ColumnType::Int | ColumnType::Bool | ColumnType::Date
        )
    }

    /// The type this wire name denotes.
    fn parse(name: &str) -> Result<ColumnType> {
        match name {
            "float" => Ok(ColumnType::Float),
            "int" => Ok(ColumnType::Int),
            "bool" => Ok(ColumnType::Bool),
            "str" => Ok(ColumnType::Str),
            "date" => Ok(ColumnType::Date),
            "null" => Ok(ColumnType::Null),
            other => Err(Error::invalid(format!(
                "unknown frame column type `{other}`"
            ))),
        }
    }
}

impl Column {
    /// How many rows this column holds.
    pub fn len(&self) -> usize {
        match self {
            Column::Float(v) => v.len(),
            Column::Int(v) => v.len(),
            Column::Bool(v) => v.len(),
            Column::Str(v) => v.len(),
            Column::Date(v) => v.len(),
            Column::Null(n) => *n,
        }
    }

    /// Whether this column holds no rows at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// This column's type.
    pub fn kind(&self) -> ColumnType {
        match self {
            Column::Float(_) => ColumnType::Float,
            Column::Int(_) => ColumnType::Int,
            Column::Bool(_) => ColumnType::Bool,
            Column::Str(_) => ColumnType::Str,
            Column::Date(_) => ColumnType::Date,
            Column::Null(_) => ColumnType::Null,
        }
    }

    /// Whether row `i` is null here.
    pub fn is_null_at(&self, i: usize) -> bool {
        match self {
            Column::Float(v) => v.get(i).is_none_or(Option::is_none),
            Column::Int(v) => v.get(i).is_none_or(Option::is_none),
            Column::Bool(v) => v.get(i).is_none_or(Option::is_none),
            Column::Str(v) => v.get(i).is_none_or(Option::is_none),
            Column::Date(v) => v.get(i).is_none_or(Option::is_none),
            Column::Null(_) => true,
        }
    }

    /// Build a column from one query column's values, choosing the variant the
    /// values fit.
    ///
    /// The rule is deliberately simple and stated rather than inferred per row:
    /// booleans are boolean, integers are integer **unless** something in the
    /// column is a float or decimal (then the whole column is float), dates and
    /// timestamps are epoch seconds, and everything else is text. A column that
    /// mixes text with numbers becomes text, because that is the only reading
    /// that keeps every row.
    pub fn from_values(values: Vec<Value>) -> Column {
        let mut has_float = false;
        let mut has_int = false;
        let mut has_bool = false;
        let mut has_date = false;
        let mut has_other = false;
        for v in &values {
            match v {
                Value::Null => {}
                Value::Bool(_) => has_bool = true,
                Value::Int(_) => has_int = true,
                Value::Float(_) | Value::Decimal(_) => has_float = true,
                Value::Date(_) | Value::Timestamp(_) => has_date = true,
                _ => has_other = true,
            }
        }
        let kinds =
            usize::from(has_float || has_int) + usize::from(has_bool) + usize::from(has_date);
        if has_other || kinds > 1 {
            // Mixed, or nothing numeric: render every value. A `Str` column of
            // categories is a first-class feature, so this is a reading and not
            // a fallback.
            if !has_other && kinds == 0 {
                return Column::Null(values.len());
            }
            return Column::Str(values.into_iter().map(as_text).collect());
        }
        if has_bool {
            return Column::Bool(values.into_iter().map(as_bool).collect());
        }
        if has_date {
            return Column::Date(values.into_iter().map(as_epoch).collect());
        }
        if has_float {
            return Column::Float(values.into_iter().map(as_float).collect());
        }
        if has_int {
            return Column::Int(values.into_iter().map(as_int).collect());
        }
        Column::Null(values.len())
    }

    /// `parts` one after another, as one column. Parts of one type stay that
    /// type, and an all-null part takes the type of the others; parts of
    /// different types become text, as [`from_values`](Column::from_values)
    /// makes a mixed column.
    pub(crate) fn concat(parts: Vec<Column>) -> Column {
        let len: usize = parts.iter().map(Column::len).sum();
        let kinds: BTreeSet<ColumnType> = parts
            .iter()
            .map(Column::kind)
            .filter(|k| *k != ColumnType::Null)
            .collect();
        fn join<T: Clone>(
            parts: &[Column],
            get: impl Fn(&Column) -> Option<&Vec<Option<T>>>,
        ) -> Vec<Option<T>> {
            let mut out = Vec::new();
            for part in parts {
                match get(part) {
                    Some(values) => out.extend(values.iter().cloned()),
                    None => out.extend(std::iter::repeat_n(None, part.len())),
                }
            }
            out
        }
        match kinds.into_iter().collect::<Vec<_>>().as_slice() {
            [] => Column::Null(len),
            [ColumnType::Float] => Column::Float(join(&parts, |c| match c {
                Column::Float(v) => Some(v),
                _ => None,
            })),
            [ColumnType::Int] => Column::Int(join(&parts, |c| match c {
                Column::Int(v) => Some(v),
                _ => None,
            })),
            [ColumnType::Bool] => Column::Bool(join(&parts, |c| match c {
                Column::Bool(v) => Some(v),
                _ => None,
            })),
            [ColumnType::Str] => Column::Str(join(&parts, |c| match c {
                Column::Str(v) => Some(v),
                _ => None,
            })),
            [ColumnType::Date] => Column::Date(join(&parts, |c| match c {
                Column::Date(v) => Some(v),
                _ => None,
            })),
            _ => Column::Str(
                parts
                    .iter()
                    .flat_map(|c| match c.to_json() {
                        Json::Array(values) => values,
                        _ => Vec::new(),
                    })
                    .map(|v| match v {
                        Json::Null => None,
                        Json::String(s) => Some(s),
                        other => Some(other.to_string()),
                    })
                    .collect(),
            ),
        }
    }

    /// This column restricted to `rows`, in the order given.
    pub(crate) fn take(&self, rows: &[usize]) -> Column {
        fn pick<T: Clone>(v: &[Option<T>], rows: &[usize]) -> Vec<Option<T>> {
            rows.iter().map(|i| v.get(*i).cloned().flatten()).collect()
        }
        match self {
            Column::Float(v) => Column::Float(pick(v, rows)),
            Column::Int(v) => Column::Int(pick(v, rows)),
            Column::Bool(v) => Column::Bool(pick(v, rows)),
            Column::Str(v) => Column::Str(pick(v, rows)),
            Column::Date(v) => Column::Date(pick(v, rows)),
            Column::Null(_) => Column::Null(rows.len()),
        }
    }

    /// This column's values as JSON — one array, nulls preserved.
    fn to_json(&self) -> Json {
        fn arr<T>(v: &[Option<T>], f: impl Fn(&T) -> Json) -> Json {
            Json::Array(
                v.iter()
                    .map(|x| x.as_ref().map_or(Json::Null, &f))
                    .collect(),
            )
        }
        match self {
            Column::Float(v) => arr(v, |x| {
                // A non-finite float has no JSON spelling. It is null on the
                // wire rather than a silent 0: a provider that is handed NaN as
                // a number would fit against it.
                Number::from_f64(*x).map_or(Json::Null, Json::Number)
            }),
            Column::Int(v) => arr(v, |x| Json::Number(Number::from(*x))),
            Column::Bool(v) => arr(v, |x| Json::Bool(*x)),
            Column::Str(v) => arr(v, |x| Json::String(x.clone())),
            Column::Date(v) => arr(v, |x| Json::Number(Number::from(*x))),
            Column::Null(n) => Json::Array(vec![Json::Null; *n]),
        }
    }

    /// One column of a [`Frame::from_rows`] read: the same types as the wire
    /// path, read the way a **form** produces them.
    ///
    /// Two lenienecs, and no others. A text column takes any scalar, because a
    /// category whose values happen to look like numbers is still a category and
    /// a form has no way to say so. A date column takes an ISO-8601 string,
    /// because epoch seconds is the frame's *internal* spelling and nobody types
    /// one. Everything else is refused by name — a string where the fit saw a
    /// number is a prediction about a different column.
    fn from_row_json(name: &str, ty: ColumnType, values: &[Json]) -> Result<Column> {
        let refuse = |i: usize, v: &Json, want: &str| {
            Error::invalid(format!(
                "`{name}` is {want} in this model, but row {} has {v}",
                i + 1
            ))
        };
        Ok(match ty {
            ColumnType::Str => Column::Str(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| match v {
                        Json::Null => Ok(None),
                        Json::String(s) => Ok(Some(s.clone())),
                        Json::Bool(b) => Ok(Some(b.to_string())),
                        Json::Number(n) => Ok(Some(n.to_string())),
                        other => Err(refuse(i, other, "a category")),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            ColumnType::Date => Column::Date(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| match v {
                        Json::Null => Ok(None),
                        Json::Number(n) => {
                            n.as_i64().map(Some).ok_or_else(|| refuse(i, v, "a date"))
                        }
                        Json::String(s) => parse_epoch(s)
                            .map(Some)
                            .ok_or_else(|| refuse(i, v, "a date")),
                        other => Err(refuse(i, other, "a date")),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => Column::from_json(name, ty, values)?,
        })
    }

    /// One column back off the wire, of the type its `type` field names.
    fn from_json(name: &str, ty: ColumnType, values: &[Json]) -> Result<Column> {
        fn map<T>(
            name: &str,
            ty: ColumnType,
            values: &[Json],
            f: impl Fn(&Json) -> Option<T>,
        ) -> Result<Vec<Option<T>>> {
            values
                .iter()
                .map(|v| match v {
                    Json::Null => Ok(None),
                    other => f(other).map(Some).ok_or_else(|| {
                        Error::invalid(format!(
                            "frame column `{name}` is `{}`, but a value is {other}",
                            ty.name()
                        ))
                    }),
                })
                .collect()
        }
        Ok(match ty {
            ColumnType::Float => Column::Float(map(name, ty, values, Json::as_f64)?),
            ColumnType::Int => Column::Int(map(name, ty, values, Json::as_i64)?),
            ColumnType::Bool => Column::Bool(map(name, ty, values, Json::as_bool)?),
            ColumnType::Str => {
                Column::Str(map(name, ty, values, |v| v.as_str().map(str::to_owned))?)
            }
            ColumnType::Date => Column::Date(map(name, ty, values, Json::as_i64)?),
            ColumnType::Null => {
                if let Some(bad) = values.iter().find(|v| !v.is_null()) {
                    return Err(Error::invalid(format!(
                        "frame column `{name}` is `null`, but a value is {bad}"
                    )));
                }
                Column::Null(values.len())
            }
        })
    }
}

/// A materialised dataset: named columns of equal height, and one key per row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Frame {
    /// The columns, in the dataset's own order.
    pub columns: Vec<(String, Column)>,
    /// How many rows every column holds.
    pub rows: usize,
    /// The canonical primary key of each row, for the split's hash (TODO §5).
    ///
    /// Empty when the source could not supply one — a table with a composite or
    /// absent primary key. Reads are unaffected by that; only the split refuses.
    pub keys: Vec<String>,
}

impl Frame {
    /// A frame over `columns`, with `keys` per row.
    ///
    /// Checked rather than trusted: a ragged frame would put one column's row 7
    /// against another's row 8, which is a wrong answer rather than a crash.
    pub fn new(columns: Vec<(String, Column)>, keys: Vec<String>) -> Result<Frame> {
        let rows = columns.first().map_or(keys.len(), |(_, c)| c.len());
        for (name, column) in &columns {
            if column.len() != rows {
                return Err(Error::msg(format!(
                    "frame column `{name}` has {} rows, but the frame has {rows}",
                    column.len()
                )));
            }
        }
        if !keys.is_empty() && keys.len() != rows {
            return Err(Error::msg(format!(
                "frame has {rows} rows but {} keys",
                keys.len()
            )));
        }
        Ok(Frame {
            columns,
            rows,
            keys,
        })
    }

    /// The column under this name, or `None`.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }

    /// The column names, in order.
    pub fn names(&self) -> Vec<&str> {
        self.columns.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// This frame restricted to `rows`, in the order given — what the split and
    /// the metric pass slice with.
    ///
    /// An index past the end is an error rather than a dropped row: every caller
    /// of this builds its index list from this frame's own height, so an
    /// out-of-range index is a bug and silently shortening the answer would hide
    /// it.
    pub fn take_rows(&self, rows: &[usize]) -> Result<Frame> {
        if let Some(bad) = rows.iter().find(|i| **i >= self.rows) {
            return Err(Error::msg(format!(
                "row {bad} is out of range in a frame of {} rows",
                self.rows
            )));
        }
        Ok(Frame {
            columns: self
                .columns
                .iter()
                .map(|(n, c)| (n.clone(), c.take(rows)))
                .collect(),
            rows: rows.len(),
            keys: if self.keys.is_empty() {
                Vec::new()
            } else {
                rows.iter()
                    .map(|i| self.keys.get(*i).cloned().unwrap_or_default())
                    .collect()
            },
        })
    }

    /// The frame as the JSON that crosses a module seam: one array per column.
    ///
    /// ```json
    /// { "rows": 2, "columns": [ { "name": "price", "type": "float",
    ///                             "values": [1.0, null] } ] }
    /// ```
    ///
    /// The keys are **not** on the wire. A provider is handed numbers; which row
    /// was which is the host's question, and sending 50 000 primary keys to
    /// answer nothing would be the frame's largest column.
    pub fn to_json(&self) -> Json {
        let columns = self
            .columns
            .iter()
            .map(|(name, column)| {
                let mut obj = Map::new();
                obj.insert("name".to_owned(), Json::String(name.clone()));
                obj.insert(
                    "type".to_owned(),
                    Json::String(column.kind().name().to_owned()),
                );
                obj.insert("values".to_owned(), column.to_json());
                Json::Object(obj)
            })
            .collect();
        let mut obj = Map::new();
        obj.insert("rows".to_owned(), Json::Number(Number::from(self.rows)));
        obj.insert("columns".to_owned(), Json::Array(columns));
        Json::Object(obj)
    }

    /// The frame as **rows**: one JSON object per row, keyed by column name.
    ///
    /// The inverse of [`from_rows`](Frame::from_rows), and the shape a screen
    /// wants — the dataset preview shows a table of rows, not a table of
    /// columns. Everything inside this crate reads columns, which is why this is
    /// a rendering rather than the representation.
    pub fn to_rows(&self) -> Vec<Json> {
        let columns: Vec<(&str, Json)> = self
            .columns
            .iter()
            .map(|(name, column)| (name.as_str(), column.to_json()))
            .collect();
        (0..self.rows)
            .map(|i| {
                let mut row = Map::with_capacity(columns.len());
                for (name, values) in &columns {
                    let cell = values.get(i).cloned().unwrap_or(Json::Null);
                    row.insert((*name).to_owned(), cell);
                }
                Json::Object(row)
            })
            .collect()
    }

    /// A frame built from **rows** — one JSON object per row, keyed by column
    /// name — of the columns and types given.
    ///
    /// The other direction from every frame in this crate, and it exists for one
    /// caller: `predictRows` with literal rows, which is the admin screen's "try
    /// a row" box and an API caller asking about a row that is not in the table
    /// at all. Everything else materialises a dataset, where the columns arrive
    /// as columns and their types are the data's.
    ///
    /// The types are therefore **not** inferred from what arrived: they are the
    /// ones the instance was fitted with, so a `bedrooms` typed by hand as
    /// `"3"` is read as the number the fit saw rather than as a new category.
    /// Conversion is deliberately forgiving in the two directions a form makes
    /// unavoidable — a number written for a text column, a date written as a
    /// string — and refuses everything else by naming the row and the column.
    pub fn from_rows(rows: &[Json], columns: &[(String, ColumnType)]) -> Result<Frame> {
        let mut out = Vec::with_capacity(columns.len());
        for (name, ty) in columns {
            let mut values = Vec::with_capacity(rows.len());
            for (i, row) in rows.iter().enumerate() {
                let cell = row.get(name).ok_or_else(|| {
                    Error::invalid(format!("row {} has no value for `{name}`", i + 1))
                })?;
                values.push(cell.clone());
            }
            out.push((name.clone(), Column::from_row_json(name, *ty, &values)?));
        }
        Frame::new(out, Vec::new())
    }

    /// A frame back off the wire — [`to_json`](Frame::to_json)'s inverse, and
    /// what a provider's prediction request is read from on the other side.
    pub fn from_json(json: &Json) -> Result<Frame> {
        let bad = |what: &str| Error::invalid(format!("frame JSON: {what}"));
        let columns = json
            .get("columns")
            .and_then(Json::as_array)
            .ok_or_else(|| bad("`columns` must be an array"))?;
        let mut out = Vec::with_capacity(columns.len());
        for column in columns {
            let name = column
                .get("name")
                .and_then(Json::as_str)
                .ok_or_else(|| bad("every column needs a string `name`"))?;
            let ty = column
                .get("type")
                .and_then(Json::as_str)
                .ok_or_else(|| bad("every column needs a string `type`"))?;
            let values = column
                .get("values")
                .and_then(Json::as_array)
                .ok_or_else(|| bad("every column needs an array of `values`"))?;
            out.push((
                name.to_owned(),
                Column::from_json(name, ColumnType::parse(ty)?, values)?,
            ));
        }
        Frame::new(out, Vec::new())
    }
}

/// A row's primary key as the **stable text the split hashes**.
///
/// Prefixed with the value's kind so `Int(1)` and `Text("1")` are different
/// rows: two tables' keys never collide into one bucket, and — much more to the
/// point — a key that changed its rendering between two fits of one model would
/// move rows between train and test, which is exactly what §5 exists to prevent.
pub fn canonical_key(value: &Value) -> String {
    format!("{}:{}", value.kind(), render(value))
}

/// A value's text, with no kind prefix — what a `Str` column holds and what
/// [`canonical_key`] prefixes.
fn render(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Uuid(u) => u.to_string(),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Timestamp(t) => t.to_rfc3339(),
        Value::Decimal(d) => d.to_string(),
        Value::Json(j) => j.to_string(),
        Value::Bytes(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

/// A value as a float, where it has one.
fn as_float(value: Value) -> Option<f64> {
    match value {
        Value::Float(f) => Some(f),
        Value::Int(i) => Some(i as f64),
        Value::Decimal(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// A value as an integer, where it has one.
fn as_int(value: Value) -> Option<i64> {
    match value {
        Value::Int(i) => Some(i),
        _ => None,
    }
}

/// A value as a boolean, where it has one.
fn as_bool(value: Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(b),
        _ => None,
    }
}

/// A date or timestamp as epoch seconds, UTC. A bare date is midnight UTC —
/// there is no zone in the column to say otherwise.
fn as_epoch(value: Value) -> Option<i64> {
    match value {
        Value::Timestamp(t) => Some(t.timestamp()),
        Value::Date(d) => Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp()),
        _ => None,
    }
}

/// An ISO-8601 date or timestamp as epoch seconds — how a date arrives from a
/// form, since epoch seconds is the frame's internal spelling and nobody types
/// one.
///
/// A bare date is midnight UTC, which is the reading `Column::from_values` gives
/// a `Value::Date` — the two paths have to agree or a date fitted from the table
/// and a date typed into the box would encode to different numbers.
fn parse_epoch(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(t) = text.parse::<DateTime<Utc>>() {
        return Some(t.timestamp());
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S") {
        return Some(t.and_utc().timestamp());
    }
    if let Ok(d) = text.parse::<NaiveDate>() {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    None
}

/// A value rendered as text — the reading a mixed or non-numeric column takes.
fn as_text(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(s) => Some(s),
        other => Some(render(&other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_of_ints_and_floats_is_a_float_column() {
        let column = Column::from_values(vec![Value::Int(1), Value::Float(2.5), Value::Null]);
        assert_eq!(column, Column::Float(vec![Some(1.0), Some(2.5), None]));
        assert_eq!(column.kind(), ColumnType::Float);
    }

    #[test]
    fn a_column_of_nothing_but_nulls_keeps_its_height() {
        let column = Column::from_values(vec![Value::Null, Value::Null]);
        assert_eq!(column, Column::Null(2));
        assert_eq!(column.len(), 2);
    }

    #[test]
    fn a_column_mixing_text_and_numbers_becomes_text_rather_than_losing_rows() {
        let column = Column::from_values(vec![Value::Text("a".into()), Value::Int(2)]);
        assert_eq!(
            column,
            Column::Str(vec![Some("a".to_owned()), Some("2".to_owned())])
        );
    }

    #[test]
    fn a_ragged_frame_is_refused() {
        let err = Frame::new(
            vec![
                ("a".to_owned(), Column::Int(vec![Some(1), Some(2)])),
                ("b".to_owned(), Column::Int(vec![Some(1)])),
            ],
            Vec::new(),
        )
        .expect_err("ragged");
        assert!(err.to_string().contains("`b` has 1 rows"), "{err}");
    }

    #[test]
    fn take_rows_carries_the_keys_and_refuses_an_index_past_the_end() {
        let frame = Frame::new(
            vec![(
                "a".to_owned(),
                Column::Str(vec![Some("x".into()), Some("y".into()), None]),
            )],
            vec!["int:1".into(), "int:2".into(), "int:3".into()],
        )
        .expect("frame");
        let taken = frame.take_rows(&[2, 0]).expect("take");
        assert_eq!(taken.rows, 2);
        assert_eq!(taken.keys, vec!["int:3".to_owned(), "int:1".to_owned()]);
        assert_eq!(
            taken.column("a"),
            Some(&Column::Str(vec![None, Some("x".into())]))
        );
        assert!(frame.take_rows(&[3]).is_err());
    }

    #[test]
    fn the_seam_json_is_one_array_per_column_and_round_trips() {
        let frame = Frame::new(
            vec![
                ("price".to_owned(), Column::Float(vec![Some(1.5), None])),
                (
                    "region".to_owned(),
                    Column::Str(vec![Some("n".into()), Some("s".into())]),
                ),
                ("sold".to_owned(), Column::Bool(vec![Some(true), None])),
                ("when".to_owned(), Column::Date(vec![Some(0), Some(86_400)])),
                ("nothing".to_owned(), Column::Null(2)),
            ],
            vec!["int:1".into(), "int:2".into()],
        )
        .expect("frame");
        let json = frame.to_json();
        assert_eq!(json["rows"], 2);
        assert_eq!(json["columns"][0]["name"], "price");
        assert_eq!(json["columns"][0]["type"], "float");
        assert_eq!(json["columns"][0]["values"][1], Json::Null);
        // The keys are the host's, and are not on the wire.
        assert!(json.get("keys").is_none());
        let back = Frame::from_json(&json).expect("round trip");
        assert_eq!(back.columns, frame.columns);
        assert!(back.keys.is_empty());
    }

    #[test]
    fn a_value_of_the_wrong_type_for_its_column_is_refused_by_name() {
        let json = serde_json::json!({
            "rows": 1,
            "columns": [{ "name": "price", "type": "float", "values": ["nope"] }]
        });
        let err = Frame::from_json(&json).expect_err("wrong type");
        assert!(err.to_string().contains("`price` is `float`"), "{err}");
    }

    #[test]
    fn a_literal_row_is_read_with_the_types_the_fit_saw_and_not_the_ones_typed() {
        // Task 5.4: the "try a row" box. `bedrooms` typed as a string would be a
        // new category if the types were inferred; they are the instance's, so
        // it is the number the fit saw.
        let frame = Frame::from_rows(
            &[serde_json::json!({
                "area": 100,
                "region": 3,
                "sold_on": "2024-01-02",
            })],
            &[
                ("area".to_owned(), ColumnType::Float),
                ("region".to_owned(), ColumnType::Str),
                ("sold_on".to_owned(), ColumnType::Date),
            ],
        )
        .expect("frame");
        assert_eq!(frame.rows, 1);
        assert_eq!(
            frame.column("area"),
            Some(&Column::Float(vec![Some(100.0)]))
        );
        // A number written for a category is the category it renders as: a form
        // has no way to say "this 3 is a label".
        assert_eq!(
            frame.column("region"),
            Some(&Column::Str(vec![Some("3".to_owned())]))
        );
        // And a date is typed, not epoch seconds — which is the frame's internal
        // spelling and nobody types one.
        assert_eq!(
            frame.column("sold_on"),
            Some(&Column::Date(vec![Some(1_704_153_600)]))
        );
    }

    #[test]
    fn a_literal_row_missing_a_feature_says_which_one_and_which_row() {
        let err = Frame::from_rows(
            &[serde_json::json!({ "area": 1.0 })],
            &[
                ("area".to_owned(), ColumnType::Float),
                ("region".to_owned(), ColumnType::Str),
            ],
        )
        .expect_err("missing");
        assert!(err.to_string().contains("`region`"), "{err}");
        assert!(err.to_string().contains("row 1"), "{err}");
    }

    #[test]
    fn a_string_where_the_fit_saw_a_number_is_refused_rather_than_reinterpreted() {
        let err = Frame::from_rows(
            &[serde_json::json!({ "area": "biggish" })],
            &[("area".to_owned(), ColumnType::Float)],
        )
        .expect_err("not a number");
        assert!(err.to_string().contains("`area`"), "{err}");
    }

    #[test]
    fn rows_and_columns_are_the_same_frame_read_two_ways() {
        let frame = Frame::new(
            vec![
                ("price".to_owned(), Column::Float(vec![Some(1.0), None])),
                (
                    "region".to_owned(),
                    Column::Str(vec![Some("north".into()), Some("south".into())]),
                ),
            ],
            Vec::new(),
        )
        .expect("frame");
        assert_eq!(
            frame.to_rows(),
            vec![
                serde_json::json!({ "price": 1.0, "region": "north" }),
                serde_json::json!({ "price": null, "region": "south" }),
            ]
        );
    }

    #[test]
    fn a_key_carries_its_kind_so_one_and_the_text_one_are_different_rows() {
        assert_ne!(
            canonical_key(&Value::Int(1)),
            canonical_key(&Value::Text("1".into()))
        );
    }
}
