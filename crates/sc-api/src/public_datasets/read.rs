//! Reading a downloaded file into rows: the records of a CSV, a JSON array or a
//! GeoJSON FeatureCollection, and each field's value from a record, typed the
//! way the row layer takes it.
//!
//! Pure — bytes in, JSON values out — so every quirk of the catalogue's files
//! (a header the file does not have, columns lined up with spaces, `?` for a
//! missing value, a quarter written `1998 Q1`, a point with a depth) is tested
//! here without a database or a network.
//!
//! Records are visited one at a time rather than collected: the largest file
//! has 336,776 rows of 20 columns, and a parsed copy of all of them as JSON
//! would cost far more memory than the file.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use sc_error::{Error, Result};
use sc_types::{BasicType, GeometryKind};
use serde_json::{Map, Value as Json, json};

use super::{FieldDef, Format, Parse, SourceDef};

/// One record of a file: a CSV row, a JSON object, or a GeoJSON feature.
pub(super) struct Record<'a> {
    kind: Kind<'a>,
    constants: &'a BTreeMap<String, String>,
}

enum Kind<'a> {
    Csv {
        index: &'a HashMap<String, usize>,
        row: &'a ::csv::StringRecord,
    },
    Json(&'a Map<String, Json>),
    Feature {
        properties: &'a Map<String, Json>,
        geometry: Option<&'a Json>,
        id: Option<&'a Json>,
    },
}

/// A cell as the file has it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Raw<'a> {
    /// Text from a CSV.
    Text(&'a str),
    /// A JSON value.
    Json(Cow<'a, Json>),
    /// No such column, or JSON `null`.
    Missing,
}

impl Record<'_> {
    /// The cell called `name`; `$geometry`, `$id` and `$z` reach into a feature.
    pub(super) fn get(&self, name: &str) -> Raw<'_> {
        if let Some(constant) = self.constants.get(name) {
            return Raw::Text(constant);
        }
        let found = match &self.kind {
            Kind::Csv { index, row } => {
                return index
                    .get(name)
                    .and_then(|i| row.get(*i))
                    .map_or(Raw::Missing, Raw::Text);
            }
            Kind::Json(object) => object.get(name),
            Kind::Feature {
                properties,
                geometry,
                id,
            } => match name {
                "$geometry" => *geometry,
                "$id" => *id,
                "$z" => {
                    return geometry
                        .and_then(|g| g.get("coordinates"))
                        .and_then(|c| c.get(2))
                        .map_or(Raw::Missing, |z| Raw::Json(Cow::Owned(z.clone())));
                }
                _ => properties.get(name),
            },
        };
        match found {
            None | Some(Json::Null) => Raw::Missing,
            Some(value) => Raw::Json(Cow::Borrowed(value)),
        }
    }
}

/// A file's records, one at a time: [`next_with`](Reader::next_with) hands
/// each to a closure, so the caller can write a row (asynchronously) before the
/// next is read.
pub(super) struct Reader<'a> {
    constants: &'a BTreeMap<String, String>,
    state: State<'a>,
    n: u64,
}

enum State<'a> {
    Csv {
        reader: ::csv::Reader<&'a [u8]>,
        index: HashMap<String, usize>,
        row: ::csv::StringRecord,
    },
    Aligned {
        lines: std::vec::IntoIter<&'a str>,
        index: HashMap<String, usize>,
        row: ::csv::StringRecord,
    },
    Json {
        items: std::vec::IntoIter<Map<String, Json>>,
    },
    Geojson {
        features: std::vec::IntoIter<Json>,
    },
}

impl<'a> Reader<'a> {
    /// Read `text` — the file, through [`decode`] — as `source` says.
    pub(super) fn new(source: &'a SourceDef, text: &'a str) -> Result<Reader<'a>> {
        let state = match source.format {
            Format::Csv if source.delimiter.as_deref() == Some("whitespace") => {
                let mut lines = text.lines().filter(|l| !l.trim().is_empty()).filter(|l| {
                    source
                        .comment
                        .as_deref()
                        .is_none_or(|c| !l.trim_start().starts_with(c))
                });
                let names: Vec<String> = match &source.header {
                    Some(given) => given.clone(),
                    None => lines.next().map(split_aligned).unwrap_or_default(),
                };
                State::Aligned {
                    lines: lines.collect::<Vec<_>>().into_iter(),
                    index: column_index(&names),
                    row: ::csv::StringRecord::new(),
                }
            }
            Format::Csv => {
                let delimiter = match source.delimiter.as_deref() {
                    Some(";") => b';',
                    Some("\t") => b'\t',
                    _ => b',',
                };
                let mut reader = ::csv::ReaderBuilder::new()
                    .delimiter(delimiter)
                    .has_headers(source.header.is_none())
                    .flexible(true)
                    .trim(::csv::Trim::All)
                    .comment(source.comment.as_deref().and_then(|c| c.bytes().next()))
                    .from_reader(text.as_bytes());
                let names: Vec<String> = match &source.header {
                    Some(given) => given.clone(),
                    None => reader
                        .headers()
                        .map_err(|e| Error::invalid(format!("the header could not be read: {e}")))?
                        .iter()
                        .map(str::to_owned)
                        .collect(),
                };
                State::Csv {
                    reader,
                    index: column_index(&names),
                    row: ::csv::StringRecord::new(),
                }
            }
            Format::Json => {
                let items: Vec<Map<String, Json>> = serde_json::from_str(text)
                    .map_err(|e| Error::invalid(format!("not a JSON array of objects: {e}")))?;
                State::Json {
                    items: items.into_iter(),
                }
            }
            Format::Geojson => {
                let mut document: Json = serde_json::from_str(text)
                    .map_err(|e| Error::invalid(format!("not GeoJSON: {e}")))?;
                let features = match document.get_mut("features").map(Json::take) {
                    Some(Json::Array(features)) => features,
                    _ => return Err(Error::invalid("not a GeoJSON FeatureCollection")),
                };
                State::Geojson {
                    features: features.into_iter(),
                }
            }
        };
        Ok(Reader {
            constants: &source.constants,
            state,
            n: 0,
        })
    }

    /// Hand the next record and its number (from 1, data rows only) to
    /// `visit`; `None` at the end of the file.
    pub(super) fn next_with<T>(
        &mut self,
        visit: impl FnOnce(u64, &Record<'_>) -> Result<T>,
    ) -> Option<Result<T>> {
        let constants = self.constants;
        match &mut self.state {
            State::Csv { reader, index, row } => loop {
                match reader.read_record(row) {
                    Ok(false) => return None,
                    // A line of nothing but separators is no row.
                    Ok(true) if row.iter().all(str::is_empty) => continue,
                    Ok(true) => {
                        self.n += 1;
                        let record = Record {
                            kind: Kind::Csv { index, row },
                            constants,
                        };
                        return Some(visit(self.n, &record));
                    }
                    Err(e) => {
                        return Some(Err(Error::invalid(format!("row {}: {e}", self.n + 1))));
                    }
                }
            },
            State::Aligned { lines, index, row } => {
                let line = lines.next()?;
                *row = ::csv::StringRecord::from(split_aligned(line));
                self.n += 1;
                let record = Record {
                    kind: Kind::Csv { index, row },
                    constants,
                };
                Some(visit(self.n, &record))
            }
            State::Json { items } => {
                let item = items.next()?;
                self.n += 1;
                let record = Record {
                    kind: Kind::Json(&item),
                    constants,
                };
                Some(visit(self.n, &record))
            }
            State::Geojson { features } => {
                let feature = features.next()?;
                self.n += 1;
                let empty = Map::new();
                let record = Record {
                    kind: Kind::Feature {
                        properties: feature
                            .get("properties")
                            .and_then(Json::as_object)
                            .unwrap_or(&empty),
                        geometry: feature.get("geometry").filter(|g| !g.is_null()),
                        id: feature.get("id").filter(|g| !g.is_null()),
                    },
                    constants,
                };
                Some(visit(self.n, &record))
            }
        }
    }
}

/// Visit every record of `bytes`, read as `source` says.
#[cfg(test)]
fn each_record(
    source: &SourceDef,
    bytes: &[u8],
    mut visit: impl FnMut(u64, &Record<'_>) -> Result<()>,
) -> Result<()> {
    let text = decode(bytes);
    let mut reader = Reader::new(source, &text)?;
    while let Some(outcome) = reader.next_with(&mut visit) {
        outcome?;
    }
    Ok(())
}

/// The text of a file: UTF-8 without a byte-order mark, or — for an older file
/// that is not UTF-8 — Latin-1, which every byte sequence is.
pub(super) fn decode(bytes: &[u8]) -> Cow<'_, str> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(bytes.iter().map(|b| char::from(*b)).collect()),
    }
}

/// The position of each column name.
fn column_index(names: &[String]) -> HashMap<String, usize> {
    let mut index = HashMap::new();
    for (i, name) in names.iter().enumerate() {
        index.entry(name.trim().to_owned()).or_insert(i);
    }
    index
}

/// One line of a file whose columns are separated by runs of spaces or tabs,
/// with double quotes around a cell that has spaces in it.
fn split_aligned(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(first) = chars.next() else { break };
        let mut cell = String::new();
        if first == '"' {
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                cell.push(c);
            }
        } else {
            cell.push(first);
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                cell.push(c);
            }
        }
        cells.push(cell);
    }
    cells
}

// --- a field's value -----------------------------------------------------------

/// The value of `field` in `record`, as the row layer takes it, or `null` for
/// none. `ty` is the column's type: a `key`'s is its target's.
pub(super) fn value(
    field: &FieldDef,
    ty: &BasicType,
    record: &Record<'_>,
    source_na: &[String],
) -> Result<Json> {
    let in_field = |what: String| Error::invalid(format!("`{}`: {what}", field.name));
    let raws: Vec<Raw<'_>> = field.sources().iter().map(|s| record.get(s)).collect();
    let is_na = |raw: &Raw<'_>| match raw {
        Raw::Missing => true,
        Raw::Text(t) => {
            t.trim().is_empty() || source_na.iter().chain(&field.na).any(|n| n == t.trim())
        }
        Raw::Json(j) => {
            j.is_null()
                || j.as_str().is_some_and(|s| s.trim().is_empty())
                || source_na
                    .iter()
                    .chain(&field.na)
                    .any(|n| *n == plain_text(j))
        }
    };
    if raws.iter().any(is_na) {
        return Ok(Json::Null);
    }

    // Several columns make one value.
    if raws.len() > 1 {
        return match ty {
            BasicType::Geometry(GeometryKind::Point) => {
                let lon = float(&raws[0]).map_err(in_field)?;
                let lat = float(&raws[1]).map_err(in_field)?;
                Ok(json!({ "type": "Point", "coordinates": [wrap_longitude(lon), lat] }))
            }
            _ => {
                let year = int(&raws[0]).map_err(in_field)?;
                let month = int(&raws[1]).map_err(in_field)?;
                let day = raws
                    .get(2)
                    .map(int)
                    .transpose()
                    .map_err(in_field)?
                    .unwrap_or(1);
                // Converted, not cast: `year as i32` would wrap a year past the
                // range into a different, valid one.
                let date = match (
                    i32::try_from(year),
                    u32::try_from(month),
                    u32::try_from(day),
                ) {
                    (Ok(y), Ok(m), Ok(d)) => NaiveDate::from_ymd_opt(y, m, d),
                    _ => None,
                }
                .ok_or_else(|| in_field(format!("{year}-{month}-{day} is not a date")))?;
                Ok(Json::String(date.to_string()))
            }
        };
    }
    let raw = &raws[0];

    if field.parse == Some(Parse::ParentCode) {
        let code = text(raw);
        let parent: String = code
            .chars()
            .take(code.chars().count().saturating_sub(1))
            .collect();
        return Ok(if parent.chars().count() < 2 {
            Json::Null
        } else {
            Json::String(parent)
        });
    }

    match ty {
        BasicType::Int => Ok(json!(int(raw).map_err(in_field)?)),
        BasicType::Float => Ok(json!(float(raw).map_err(in_field)?)),
        BasicType::Bool => Ok(json!(boolean(raw).map_err(in_field)?)),
        BasicType::Date => Ok(Json::String(
            date(raw, field.parse).map_err(in_field)?.to_string(),
        )),
        BasicType::Timestamp => Ok(Json::String(
            timestamp(raw, field.parse).map_err(in_field)?.to_rfc3339(),
        )),
        BasicType::Geometry(kind) => geometry(raw, *kind).map_err(in_field),
        _ => Ok(Json::String(text(raw).into_owned())),
    }
}

/// A JSON value as a cell would spell it.
fn plain_text(j: &Json) -> String {
    match j {
        Json::String(s) => s.trim().to_owned(),
        other => other.to_string(),
    }
}

fn text<'a>(raw: &'a Raw<'_>) -> Cow<'a, str> {
    match raw {
        Raw::Text(t) => Cow::Borrowed(t.trim()),
        Raw::Json(j) => match j.as_ref() {
            Json::String(s) => Cow::Borrowed(s.trim()),
            other => Cow::Owned(other.to_string()),
        },
        Raw::Missing => Cow::Borrowed(""),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "integral and well inside the i64 range, so the cast is exact"
)]
fn int(raw: &Raw<'_>) -> std::result::Result<i64, String> {
    if let Raw::Json(j) = raw
        && let Some(i) = j.as_i64()
    {
        return Ok(i);
    }
    let t = text(raw);
    if let Ok(i) = t.parse::<i64>() {
        return Ok(i);
    }
    // `3504.` and `41.0` are whole numbers written as decimals.
    match t.parse::<f64>() {
        Ok(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(f as i64),
        _ => Err(format!("`{t}` is not a whole number")),
    }
}

fn float(raw: &Raw<'_>) -> std::result::Result<f64, String> {
    if let Raw::Json(j) = raw
        && let Some(f) = j.as_f64()
    {
        return Ok(f);
    }
    let t = text(raw);
    t.parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .ok_or_else(|| format!("`{t}` is not a number"))
}

fn boolean(raw: &Raw<'_>) -> std::result::Result<bool, String> {
    if let Raw::Json(j) = raw
        && let Some(b) = j.as_bool()
    {
        return Ok(b);
    }
    let t = text(raw);
    match t.to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" => Ok(true),
        "0" | "false" | "f" | "no" | "n" => Ok(false),
        _ => Err(format!("`{t}` is not yes or no")),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "a decimal year's floor and month fraction: float-to-integer as saturates, and from_ymd_opt refuses what is out of range"
)]
fn date(raw: &Raw<'_>, parse: Option<Parse>) -> std::result::Result<NaiveDate, String> {
    let t = text(raw);
    let bad = || format!("`{t}` is not a date");
    match parse {
        Some(Parse::YearQuarter) => {
            let (year, quarter) = t.split_once(" Q").ok_or_else(bad)?;
            let year: i32 = year.trim().parse().map_err(|_| bad())?;
            let quarter: u32 = quarter.trim().parse().map_err(|_| bad())?;
            if !(1..=4).contains(&quarter) {
                return Err(bad());
            }
            NaiveDate::from_ymd_opt(year, (quarter - 1) * 3 + 1, 1).ok_or_else(bad)
        }
        Some(Parse::YearMonth) => {
            NaiveDate::parse_from_str(&format!("{t}-01"), "%Y-%m-%d").map_err(|_| bad())
        }
        Some(Parse::DecimalYearMonth) => {
            let f = float(raw)?;
            let year = f.floor();
            let month = ((f - year) * 12.0).round() as u32 + 1;
            NaiveDate::from_ymd_opt(year as i32, month.min(12), 1).ok_or_else(bad)
        }
        // A date is the first ten characters of a date or a timestamp:
        // `1996-07-04 00:00:00.000`, `2000-01-01T08:00:00.000Z`.
        _ => t
            .get(..10)
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .ok_or_else(bad),
    }
}

fn timestamp(raw: &Raw<'_>, parse: Option<Parse>) -> std::result::Result<DateTime<Utc>, String> {
    if parse == Some(Parse::EpochMs) {
        let ms = int(raw)?;
        return Utc
            .timestamp_millis_opt(ms)
            .single()
            .ok_or_else(|| format!("{ms} is not a time"));
    }
    let t = text(raw);
    if let Ok(at) = DateTime::parse_from_rfc3339(&t) {
        return Ok(at.with_timezone(&Utc));
    }
    // A time with no zone is taken as UTC.
    NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%.f"))
        .map(|naive| naive.and_utc())
        .map_err(|_| format!("`{t}` is not a time"))
}

/// A longitude in -180..=180: Fiji's earthquakes are recorded east of 180°.
fn wrap_longitude(lon: f64) -> f64 {
    if lon > 180.0 {
        lon - 360.0
    } else if lon < -180.0 {
        lon + 360.0
    } else {
        lon
    }
}

/// A GeoJSON geometry for a column of `kind`: flattened to two dimensions (the
/// columns are 2D, and an earthquake's depth is a column of its own), and a
/// single polygon or line made multi to fit a multi column.
fn geometry(raw: &Raw<'_>, kind: GeometryKind) -> std::result::Result<Json, String> {
    let Raw::Json(g) = raw else {
        return Err("is not a geometry".to_owned());
    };
    let found = g.get("type").and_then(Json::as_str).unwrap_or_default();
    let coordinates = flatten(
        g.get("coordinates")
            .ok_or("a geometry without coordinates")?,
    );
    let (expected, single) = match kind {
        GeometryKind::Point => ("Point", None),
        GeometryKind::MultiPolygon => ("MultiPolygon", Some("Polygon")),
        GeometryKind::MultiLineString => ("MultiLineString", Some("LineString")),
        other => {
            return Err(format!(
                "a {} column is not one this fills",
                other.type_name()
            ));
        }
    };
    if found == expected {
        Ok(json!({ "type": expected, "coordinates": coordinates }))
    } else if Some(found) == single {
        Ok(json!({ "type": expected, "coordinates": [coordinates] }))
    } else {
        Err(format!("a {found} does not fit a {expected} column"))
    }
}

/// Coordinates without a third (or fourth) number in any position.
fn flatten(coordinates: &Json) -> Json {
    match coordinates.as_array() {
        Some(items) if items.first().is_some_and(Json::is_number) => {
            Json::Array(items.iter().take(2).cloned().collect())
        }
        Some(items) => Json::Array(items.iter().map(flatten).collect()),
        None => coordinates.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(format: Format) -> SourceDef {
        SourceDef {
            url: "https://example.com/f".into(),
            format,
            bytes: 0,
            delimiter: None,
            header: None,
            comment: None,
            na: Vec::new(),
            constants: BTreeMap::new(),
        }
    }

    fn field(name: &str, ty: &str) -> FieldDef {
        FieldDef {
            name: name.into(),
            type_name: ty.into(),
            from: None,
            label: None,
            required: false,
            primary_key: false,
            references: None,
            if_missing: None,
            na: Vec::new(),
            parse: None,
        }
    }

    /// Every record's value of each field, as JSON.
    fn read(source: &SourceDef, bytes: &[u8], fields: &[FieldDef]) -> Vec<Vec<Json>> {
        let mut out = Vec::new();
        each_record(source, bytes, |_, record| {
            let row = fields
                .iter()
                .map(|f| {
                    let ty = f.basic_type().unwrap_or(BasicType::Text);
                    value(f, &ty, record, &source.na)
                })
                .collect::<Result<Vec<_>>>()?;
            out.push(row);
            Ok(())
        })
        .expect("the file reads");
        out
    }

    #[test]
    fn a_headerless_file_with_question_marks_and_padding_reads_by_the_given_names() {
        let mut s = source(Format::Csv);
        s.header = Some(vec!["age".into(), "work".into(), "income".into()]);
        s.na = vec!["?".into()];
        let rows = read(
            &s,
            b"39, State-gov, <=50K\n50, ?, >50K\n\n",
            &[
                field("age", "int"),
                field("work", "text"),
                field("income", "text"),
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![json!(39), json!("State-gov"), json!("<=50K")],
                vec![json!(50), Json::Null, json!(">50K")],
            ]
        );
    }

    #[test]
    fn aligned_columns_split_on_spaces_and_keep_a_quoted_name_whole() {
        let mut s = source(Format::Csv);
        s.delimiter = Some("whitespace".into());
        s.header = Some(vec![
            "mpg".into(),
            "hp".into(),
            "weight".into(),
            "name".into(),
        ]);
        s.na = vec!["?".into()];
        let rows = read(
            &s,
            b"18.0   130.0      3504.  \t\"chevrolet chevelle malibu\"\n25.0   ?   2046.\t\"ford pinto\"\n",
            &[field("mpg", "float"), field("hp", "float"), field("weight", "int"), field("name", "text")],
        );
        assert_eq!(
            rows[0],
            vec![
                json!(18.0),
                json!(130.0),
                json!(3504),
                json!("chevrolet chevelle malibu")
            ]
        );
        assert_eq!(rows[1][1], Json::Null);
    }

    #[test]
    fn comments_semicolons_constants_and_combined_dates() {
        let mut s = source(Format::Csv);
        s.comment = Some("#".into());
        s.delimiter = Some(";".into());
        s.constants = BTreeMap::from([("colour".into(), "red".into())]);
        let mut month = field("month", "date");
        month.from = Some(super::super::Columns::Many(vec![
            "year".into(),
            "month".into(),
        ]));
        let mut days = field("days", "int");
        days.from = Some(super::super::Columns::One("ndays".into()));
        days.na = vec!["-1".into()];
        let rows = read(
            &s,
            b"# NOAA header\n# more\nyear;month;ndays\n1958;3;-1\n2024;12;27\n",
            &[month, days, field("colour", "text")],
        );
        assert_eq!(
            rows,
            vec![
                vec![json!("1958-03-01"), Json::Null, json!("red")],
                vec![json!("2024-12-01"), json!(27), json!("red")],
            ]
        );
    }

    /// A year past `i32` is refused, not wrapped: `year as i32` turned
    /// 4294969296 (2³² + 2000) into the year 2000 (security review 2026-11, §6).
    #[test]
    fn a_combined_date_out_of_range_is_refused_rather_than_wrapped() {
        let s = source(Format::Csv);
        let mut date = field("d", "date");
        date.from = Some(super::super::Columns::Many(vec![
            "year".into(),
            "month".into(),
        ]));
        let mut seen = Vec::new();
        each_record(&s, b"year,month\n4294969296,1\n2000,1\n", |_, record| {
            seen.push(value(&date, &BasicType::Date, record, &[]).map_err(|e| e.to_string()));
            Ok(())
        })
        .expect("the file reads");
        assert!(
            matches!(&seen[0], Err(e) if e.contains("is not a date")),
            "{seen:?}"
        );
        assert_eq!(seen[1], Ok(json!("2000-01-01")));
    }

    #[test]
    fn dates_are_read_from_quarters_months_decimal_years_and_timestamps() {
        let s = source(Format::Csv);
        let mut quarter = field("q", "date");
        quarter.parse = Some(Parse::YearQuarter);
        let mut month = field("m", "date");
        month.parse = Some(Parse::YearMonth);
        let mut decimal = field("d", "date");
        decimal.parse = Some(Parse::DecimalYearMonth);
        let rows = read(
            &s,
            b"q,m,d,t,ts\n1998 Q3,1850-01,1949.08333333333,1996-07-04 00:00:00.000,2013-01-01T10:00:00Z\n1998 Q1,2016-12,1960.91666666667,2000-01-01T08:00:00.000Z,2013-06-01 12:30:00\n",
            &[quarter, month, decimal, field("t", "date"), field("ts", "timestamp")],
        );
        assert_eq!(
            rows[0],
            vec![
                json!("1998-07-01"),
                json!("1850-01-01"),
                json!("1949-02-01"),
                json!("1996-07-04"),
                json!("2013-01-01T10:00:00+00:00"),
            ]
        );
        assert_eq!(rows[1][2], json!("1960-12-01"));
        assert_eq!(rows[1][3], json!("2000-01-01"));
        assert_eq!(rows[1][4], json!("2013-06-01T12:30:00+00:00"));
    }

    #[test]
    fn a_feature_gives_its_id_depth_and_a_flat_geometry_made_multi_where_the_column_is() {
        let s = source(Format::Geojson);
        let bytes = br#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"us1","properties":{"mag":4.5,"time":1700000000000,"tsunami":0,"code":-99},
             "geometry":{"type":"Point","coordinates":[-122.5,38.1,7.25]}},
            {"type":"Feature","properties":{"code":"DE149"},
             "geometry":{"type":"Polygon","coordinates":[[[0,0,1],[1,0,1],[1,1,1],[0,0,1]]]}}
        ]}"#;
        let mut id = field("id", "text");
        id.from = Some(super::super::Columns::One("$id".into()));
        let mut depth = field("depth", "float");
        depth.from = Some(super::super::Columns::One("$z".into()));
        let mut time = field("time", "timestamp");
        time.parse = Some(Parse::EpochMs);
        let mut code = field("code", "text");
        code.na = vec!["-99".into()];
        let mut parent = field("parent", "text");
        parent.from = Some(super::super::Columns::One("code".into()));
        parent.parse = Some(Parse::ParentCode);
        let rows = read(
            &s,
            bytes,
            &[id, depth, time, field("tsunami", "bool"), code, parent],
        );
        assert_eq!(rows[0][0], json!("us1"));
        assert_eq!(rows[0][1], json!(7.25));
        assert_eq!(rows[0][2], json!("2023-11-14T22:13:20+00:00"));
        assert_eq!(rows[0][3], json!(false));
        assert_eq!(rows[0][4], Json::Null, "-99 means none here");
        assert_eq!(rows[1][5], json!("DE14"));

        let mut geom = field("geom", "geometry_point");
        geom.from = Some(super::super::Columns::One("$geometry".into()));
        let mut outline = geom.clone();
        outline.type_name = "geometry_multipolygon".into();
        let mut geometries = Vec::new();
        each_record(&s, bytes, |n, record| {
            let field = if n == 1 { &geom } else { &outline };
            geometries.push(value(field, &field.basic_type().unwrap(), record, &[])?);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            geometries[0],
            json!({"type": "Point", "coordinates": [-122.5, 38.1]})
        );
        assert_eq!(
            geometries[1],
            json!({"type": "MultiPolygon", "coordinates": [[[[0,0],[1,0],[1,1],[0,0]]]]})
        );
        // A polygon is not a point.
        let e = each_record(&s, bytes, |_, record| {
            value(&geom, &geom.basic_type().unwrap(), record, &[]).map(|_| ())
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("a Polygon does not fit a Point column"), "{e}");
    }

    #[test]
    fn a_point_is_made_from_two_columns_with_its_longitude_wrapped() {
        let s = source(Format::Csv);
        let mut point = field("location", "geometry_point");
        point.from = Some(super::super::Columns::Many(vec![
            "long".into(),
            "lat".into(),
        ]));
        let rows = read(&s, b"lat,long\n-20.42,181.62\n", &[point]);
        let coordinates = rows[0][0]["coordinates"].as_array().unwrap();
        assert!((coordinates[0].as_f64().unwrap() - -178.38).abs() < 1e-9);
    }

    #[test]
    fn a_two_letter_code_has_no_parent_and_a_bad_number_names_its_field() {
        let s = source(Format::Csv);
        let mut parent = field("parent", "text");
        parent.from = Some(super::super::Columns::One("code".into()));
        parent.parse = Some(Parse::ParentCode);
        assert_eq!(read(&s, b"code\nDE\n", &[parent]), vec![vec![Json::Null]]);

        let e = each_record(&s, b"n\nlots\n", |_, record| {
            value(&field("n", "int"), &BasicType::Int, record, &[]).map(|_| ())
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("`n`") && e.contains("not a whole number"), "{e}");
    }

    #[test]
    fn a_latin1_file_is_read_as_latin1() {
        let s = source(Format::Csv);
        let rows = read(&s, b"name\nM\xfcnster\n", &[field("name", "text")]);
        assert_eq!(rows[0][0], json!("Münster"));
    }
}
