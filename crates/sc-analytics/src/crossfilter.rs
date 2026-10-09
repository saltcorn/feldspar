//! **Cross-filtering** (analytics TODO A6.3–A6.4): what a click or a brush on
//! one of a dashboard's panels, a drill-down, or one of its dashboard-wide
//! filters does to the others.
//!
//! A selection on a panel becomes a [`Condition`] on the column it encodes, a
//! value list for a click (`category ∈ {burglary}`) and a range for a brush
//! (`occurred_on ∈ [2025-03-01, 2025-06-30]`). The browser makes them — it
//! knows what was under the pointer — and sends the ones that apply to a panel
//! with it when it is drawn ([`render_panel_in`](crate::panel::render_panel_in)).
//!
//! **Propagation** ([`scope`]). A condition is made on a column of one dataset;
//! it applies to a panel on
//!
//! - **the same dataset**, through the column itself;
//! - **another dataset**, through a column that refers to the same table: the
//!   condition's column is a foreign key to `districts` (or is the key of a
//!   dataset whose rows are rows of `districts`), and the other dataset has a
//!   foreign key to `districts` too (or its rows are rows of `districts`, when
//!   it filters by its key). Two such columns are a choice this does not make
//!   for the person, unless one has the condition's column's name.
//!
//! Everything else is left alone, with a sentence saying why, so the dashboard
//! can say which tiles a filter does not reach.
//!
//! **Execution.** The conditions that apply to a dataset are one formula,
//! appended to the dataset's operations as a Filter ([`FILTER_OP`]) — compiled,
//! checked and translated as any Filter is, so a stat card, a summary table, a
//! map layer (whose tiles carry it in their URL) and the hypothesis tests are
//! filtered exactly as a plot is. The values are written as the formula
//! literals of the column's type: a date as `"2025-03-01"`, which both
//! databases compare with a date.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use sc_catalog::Catalog;
use sc_dataset::{
    ColType, DatasetDef, DatasetId, Grain, Op, OpStatus, Operation, Options, Schema, StageShape,
    compile,
};
use sc_error::{Error, Result};

use crate::selection::{any_of, text_literal};

/// The id of the Filter a dashboard's conditions are appended as.
pub const FILTER_OP: &str = "_fd_dashboard_filter";
/// The most conditions one panel is drawn with.
pub const MAX_CONDITIONS: usize = 50;
/// The most values one condition lists.
pub const MAX_VALUES: usize = 1_000;
/// The longest a condition's id may be.
const MAX_ID: usize = 200;

/// A condition on one column of a dataset: some of its values, or a range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// Its identity on the dashboard, so it can be removed.
    pub id: String,
    /// The dataset whose column it is.
    pub dataset: DatasetId,
    /// The column; none for the rows themselves, by their key — a feature
    /// clicked on a map of a table's rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The values kept, for a click: `null` is a missing value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Json>>,
    /// The range kept, for a brush or a bin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

/// A range of a column's values. Either end may be open.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Range {
    /// The smallest value kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<Json>,
    /// The largest value kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<Json>,
    /// Whether `max` itself is left out: a bin's end is the next bin's start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub max_exclusive: bool,
}

/// Whether a JSON value is one a column holds: not a list or an object.
fn scalar(v: &Json) -> bool {
    !matches!(v, Json::Array(_) | Json::Object(_))
}

impl Condition {
    /// What is wrong with it on its own, before any dataset is read.
    pub fn check(&self) -> Result<()> {
        self.problem()
            .map_or(Ok(()), |why| Err(Error::invalid(why)))
    }

    /// The sentence saying what is wrong with it on its own, if anything is.
    pub fn problem(&self) -> Option<String> {
        let refuse = |why: &str| Some(format!("a filter {why}"));
        if self.id.trim().is_empty() || self.id.len() > MAX_ID {
            return refuse(&format!("has an id of 1 to {MAX_ID} characters"));
        }
        if self.column.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return refuse("names its column");
        }
        match (&self.values, &self.range) {
            (Some(values), None) => {
                if values.is_empty() {
                    return refuse("keeps at least one value");
                }
                if values.len() > MAX_VALUES {
                    return refuse(&format!("keeps at most {MAX_VALUES} values"));
                }
                if !values.iter().all(scalar) {
                    return refuse("keeps values, not lists or objects");
                }
            }
            (None, Some(range)) => {
                if range.min.is_none() && range.max.is_none() {
                    return refuse("on a range has at least one end");
                }
                if [&range.min, &range.max]
                    .into_iter()
                    .flatten()
                    .any(|v| v.is_null() || !scalar(v))
                {
                    return refuse("on a range has values at its ends");
                }
            }
            (Some(_), Some(_)) => return refuse("keeps either values or a range, not both"),
            (None, None) => return refuse("keeps some values or a range"),
        }
        None
    }

    /// The conditions in `json` (a list), each checked, with different ids.
    pub fn read_list(json: &Json) -> Result<Vec<Condition>> {
        Condition::list_of(json).map_err(Error::invalid)
    }

    /// [`Condition::read_list`], or the sentence saying why not.
    pub fn list_of(json: &Json) -> std::result::Result<Vec<Condition>, String> {
        let list: Vec<Condition> = serde_json::from_value(json.clone())
            .map_err(|e| format!("these are not filters: {e}"))?;
        if list.len() > MAX_CONDITIONS {
            return Err(format!(
                "a panel is drawn with at most {MAX_CONDITIONS} filters"
            ));
        }
        let mut ids = BTreeSet::new();
        for (i, c) in list.iter().enumerate() {
            if let Some(why) = c.problem() {
                return Err(format!("filter {}: {why}", i + 1));
            }
            if !ids.insert(c.id.as_str()) {
                return Err(format!("two filters have the id `{}`", c.id));
            }
        }
        Ok(list)
    }
}

/// What one condition does to one dataset a panel reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Applied {
    /// The condition.
    pub id: String,
    /// The dataset.
    pub dataset: DatasetId,
    /// The column it filters, when it applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// Why it does not, when it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// The conditions as each dataset of a panel takes them: a formula per
/// dataset, and what became of each condition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    filters: BTreeMap<DatasetId, String>,
    /// For each dataset and condition, the column it filters or why not.
    pub applied: Vec<Applied>,
}

impl Scope {
    /// No conditions.
    pub fn none() -> Scope {
        Scope::default()
    }

    /// The formula the conditions on `dataset` make, if any apply.
    pub fn formula(&self, dataset: DatasetId) -> Option<&str> {
        self.filters.get(&dataset).map(String::as_str)
    }

    /// The operations to append to `dataset`'s: the Filter, if any apply.
    pub fn operations(&self, dataset: DatasetId) -> Vec<Operation> {
        self.formula(dataset)
            .map(|f| vec![Operation::new(FILTER_OP, Op::filter(f))])
            .into_iter()
            .flatten()
            .collect()
    }

    /// `def` with the conditions on it appended.
    pub fn extend(&self, def: &DatasetDef) -> DatasetDef {
        let mut def = def.clone();
        def.operations.extend(self.operations(def.id));
        def
    }

    /// `filter` and the conditions on `dataset`, both holding.
    pub fn and_filter(&self, dataset: DatasetId, filter: Option<&str>) -> Option<String> {
        let own = filter.map(str::trim).filter(|f| !f.is_empty());
        match (own, self.formula(dataset)) {
            (None, None) => None,
            (Some(f), None) | (None, Some(f)) => Some(f.to_owned()),
            (Some(a), Some(b)) => Some(format!("({a}) && ({b})")),
        }
    }
}

/// The sentence for a dataset's rows that do not read because of the
/// dashboard's Filter, if that is why: compiled with `def`'s operations, the
/// Filter is the first that fails.
pub(crate) fn filter_failure(
    schema: &Schema,
    library: &sc_dataset::Library,
    def: &DatasetDef,
) -> Option<String> {
    let compiled = compile(schema, library, def, Options::default());
    def.operations
        .iter()
        .zip(&compiled.operations)
        .find(|(_, r)| r.status == OpStatus::Invalid)
        .filter(|(op, _)| op.id == FILTER_OP)
        .map(|(_, r)| {
            format!(
                "the dashboard's filters do not work on `{}`: {}",
                def.name,
                r.error.as_deref().unwrap_or_default()
            )
        })
}

/// What `conditions` do to each of `datasets` (A6.4): the formula each takes,
/// and for each pair the column filtered or why not.
pub async fn scope(
    catalog: &Catalog,
    conditions: &[Condition],
    datasets: &BTreeSet<DatasetId>,
) -> Result<Scope> {
    if conditions.is_empty() || datasets.is_empty() {
        return Ok(Scope::none());
    }
    let schema = Schema::of_catalog(catalog)?;
    let library = sc_dataset::load_library(catalog).await?;
    let mut shapes: BTreeMap<DatasetId, Option<(String, StageShape)>> = BTreeMap::new();
    for id in conditions
        .iter()
        .map(|c| c.dataset)
        .chain(datasets.iter().copied())
    {
        shapes.entry(id).or_insert_with(|| {
            library.get(id).and_then(|def| {
                compile(&schema, &library, def, Options::default())
                    .last()
                    .ok()
                    .map(|stage| (def.name.clone(), stage.shape()))
            })
        });
    }
    let named = |id: DatasetId| -> Option<Named<'_>> {
        shapes
            .get(&id)
            .and_then(Option::as_ref)
            .map(|(name, shape)| Named { id, name, shape })
    };
    let mut out = Scope::none();
    for &target in datasets {
        let Some(to) = named(target) else {
            // The panel says its dataset is gone; nothing to filter.
            continue;
        };
        let mut terms = Vec::new();
        for c in conditions {
            let outcome = target_column(c, named(c.dataset).as_ref(), &to)
                .and_then(|(column, ty)| term(c, &column, ty).map(|t| (column, t)));
            let (column, skipped) = match outcome {
                Ok((column, t)) => {
                    terms.push(t);
                    (Some(column), None)
                }
                Err(why) => (None, Some(why)),
            };
            out.applied.push(Applied {
                id: c.id.clone(),
                dataset: target,
                column,
                skipped,
            });
        }
        if !terms.is_empty() {
            let formula = if terms.len() == 1 {
                terms.remove(0)
            } else {
                terms
                    .iter()
                    .map(|t| format!("({t})"))
                    .collect::<Vec<_>>()
                    .join(" && ")
            };
            out.filters.insert(target, formula);
        }
    }
    Ok(out)
}

/// A dataset, its name and its last stage's shape.
struct Named<'a> {
    id: DatasetId,
    name: &'a str,
    shape: &'a StageShape,
}

/// The table a column's values identify rows of: the table a foreign key
/// refers to, or the table whose rows a dataset's rows are, for its key.
fn identifies(shape: &StageShape, column: &str) -> Option<String> {
    let col = shape.column(column)?;
    if let Some(key) = &col.key {
        return Some(key.table.clone());
    }
    match &shape.grain {
        Grain::Table { table, key } if key == column => Some(table.clone()),
        _ => None,
    }
}

/// The column of `to` that `c` filters, and its type, or the sentence saying
/// why it filters none.
fn target_column(
    c: &Condition,
    from: Option<&Named<'_>>,
    to: &Named<'_>,
) -> std::result::Result<(String, ColType), String> {
    let Some(from) = from else {
        return Err("the dataset this filter was made on has been deleted".to_owned());
    };
    let column = match &c.column {
        Some(name) => {
            if from.shape.column(name).is_none() {
                return Err(format!("`{name}` is no longer a column of `{}`", from.name));
            }
            name.clone()
        }
        None => match &from.shape.grain {
            Grain::Table { key, .. } if from.shape.column(key).is_some() => key.clone(),
            _ => {
                return Err(format!(
                    "the rows of `{}` are not a table's rows, so the ones picked cannot be told \
                     apart",
                    from.name
                ));
            }
        },
    };
    let ty_of = |name: &str| to.shape.column(name).map_or(ColType::Unknown, |c| c.ty);
    if from.id == to.id {
        return Ok((column.clone(), ty_of(&column)));
    }
    let Some(table) = identifies(from.shape, &column) else {
        return Err(format!(
            "`{column}` is a column of `{}`, and neither refers to the rows of a table that \
             `{}` refers to",
            from.name, to.name
        ));
    };
    if let Grain::Table { table: own, key } = &to.shape.grain
        && *own == table
        && to.shape.column(key).is_some()
    {
        return Ok((key.clone(), ty_of(key)));
    }
    let referring: Vec<&str> = to
        .shape
        .columns
        .iter()
        .filter(|c| c.key.as_ref().is_some_and(|k| k.table == table))
        .map(|c| c.name.as_str())
        .collect();
    match referring[..] {
        [] => Err(format!(
            "`{}` has no column that refers to `{table}`",
            to.name
        )),
        [one] => Ok((one.to_owned(), ty_of(one))),
        _ => match referring.iter().find(|n| **n == column) {
            Some(same) => Ok(((*same).to_owned(), ty_of(same))),
            None => Err(format!(
                "`{}` refers to `{table}` by {}, and a filter on `{column}` does not say which",
                to.name,
                referring
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )),
        },
    }
}

/// The formula keeping the rows `c` keeps, on `column` of type `ty`.
fn term(c: &Condition, column: &str, ty: ColType) -> std::result::Result<String, String> {
    if let Some(values) = &c.values {
        let mut terms = Vec::with_capacity(values.len());
        for v in values {
            terms.push(if v.is_null() {
                format!("{column} == null")
            } else {
                format!("{column} == {}", literal(v, ty, column)?)
            });
        }
        return Ok(any_of(&terms));
    }
    let range = c.range.as_ref().cloned().unwrap_or_default();
    let mut parts = Vec::with_capacity(2);
    if let Some(min) = &range.min {
        parts.push(format!("{column} >= {}", literal(min, ty, column)?));
    }
    if let Some(max) = &range.max {
        let op = if range.max_exclusive { "<" } else { "<=" };
        parts.push(format!("{column} {op} {}", literal(max, ty, column)?));
    }
    Ok(parts.join(" && "))
}

/// `v` as a formula literal of a column of type `ty`, or why it cannot be.
fn literal(v: &Json, ty: ColType, column: &str) -> std::result::Result<String, String> {
    let wrong = || format!("{v} is not a value of `{column}`, {}", a_column(ty));
    let number = || {
        match v {
            Json::Number(n) => n.as_f64(),
            Json::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
        .filter(|f| f.is_finite())
    };
    let text = || match v {
        Json::String(s) => Some(s.trim().to_owned()),
        _ => None,
    };
    match ty {
        ColType::Int => match v {
            Json::Number(n) if n.as_i64().is_some() => Ok(n.to_string()),
            _ => number()
                .filter(|f| f.fract() == 0.0)
                .map(|f| format!("{}", f as i64))
                .ok_or_else(wrong),
        },
        ColType::Float | ColType::Decimal => number().map(|f| format!("{f}")).ok_or_else(wrong),
        ColType::Text => Ok(text_literal(&match v {
            Json::String(s) => s.clone(),
            other => other.to_string(),
        })),
        ColType::Bool => match v {
            Json::Bool(b) => Ok(b.to_string()),
            Json::String(s) if s == "true" || s == "false" => Ok(s.clone()),
            _ => Err(wrong()),
        },
        ColType::Date => instant(v)
            .map(|t| t.date_naive())
            .or_else(|| text().and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()))
            .map(|d| text_literal(&d.format("%Y-%m-%d").to_string()))
            .ok_or_else(wrong),
        ColType::Timestamp => instant(v)
            .map(|t| text_literal(&t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()))
            .ok_or_else(wrong),
        ColType::Time => text()
            .and_then(|s| NaiveTime::parse_from_str(&s, "%H:%M:%S%.f").ok())
            .map(|t| text_literal(&t.format("%H:%M:%S").to_string()))
            .ok_or_else(wrong),
        ColType::Uuid => text()
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .map(|u| text_literal(&u.to_string()))
            .ok_or_else(wrong),
        // Whatever the value looks like.
        ColType::Unknown => match v {
            Json::Number(_) | Json::Bool(_) => Ok(v.to_string()),
            Json::String(s) => Ok(text_literal(s)),
            _ => Err(wrong()),
        },
        ColType::Json | ColType::Bytes | ColType::Geometry => Err(format!(
            "`{column}` is {}, which a filter does not compare",
            a_column(ty)
        )),
    }
}

/// "a date column", "an integer column".
fn a_column(ty: ColType) -> String {
    let name = ty.name();
    let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {name} column")
}

/// An instant from a JSON value: milliseconds since 1970 (what a time axis
/// brushes in), RFC 3339, a date and time without a zone (UTC), or a date.
fn instant(v: &Json) -> Option<DateTime<Utc>> {
    match v {
        Json::Number(n) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .and_then(|ms| DateTime::from_timestamp_millis(ms.round() as i64)),
        Json::String(s) => {
            let s = s.trim();
            if let Ok(t) = DateTime::parse_from_rfc3339(s) {
                return Some(t.with_timezone(&Utc));
            }
            for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
                if let Ok(naive) = NaiveDateTime::parse_from_str(s, format) {
                    return Some(naive.and_utc());
                }
            }
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|naive| naive.and_utc())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use sc_dataset::{ForeignKey, StageColumn};
    use serde_json::json;

    use super::*;

    fn col(name: &str, ty: ColType) -> StageColumn {
        StageColumn {
            name: name.into(),
            ty,
            key: None,
        }
    }

    fn fk(name: &str, table: &str) -> StageColumn {
        StageColumn {
            name: name.into(),
            ty: ColType::Int,
            key: Some(ForeignKey {
                table: table.into(),
                field: "id".into(),
            }),
        }
    }

    fn values(dataset: DatasetId, column: &str, values: Json) -> Condition {
        serde_json::from_value(json!({
            "id": "c1", "dataset": dataset, "column": column, "values": values
        }))
        .expect("a condition")
    }

    #[test]
    fn a_condition_reads_and_is_checked() {
        let d = DatasetId::new();
        let c = values(d, "category", json!(["burglary", null]));
        c.check().expect("fine");
        let range: Condition = serde_json::from_value(json!({
            "id": "r", "dataset": d, "column": "at",
            "range": { "min": "2025-01-01", "max": 5, "max_exclusive": true }
        }))
        .expect("reads");
        range.check().expect("fine");
        assert_eq!(
            serde_json::to_value(&range).expect("writes")["range"],
            json!({ "min": "2025-01-01", "max": 5, "max_exclusive": true })
        );
        let refused = |raw: Json, says: &str| {
            let c: Condition = serde_json::from_value(raw).expect("reads");
            let err = c.check().expect_err("refused").to_string();
            assert!(err.contains(says), "{err}");
        };
        refused(
            json!({ "id": "", "dataset": d, "values": [1] }),
            "has an id",
        );
        refused(
            json!({ "id": "x", "dataset": d, "values": [] }),
            "at least one value",
        );
        refused(
            json!({ "id": "x", "dataset": d, "values": [[1]] }),
            "not lists",
        );
        refused(
            json!({ "id": "x", "dataset": d, "range": {} }),
            "at least one end",
        );
        refused(
            json!({ "id": "x", "dataset": d, "values": [1], "range": { "min": 1 } }),
            "not both",
        );
        refused(json!({ "id": "x", "dataset": d }), "some values or a range");
        let twice = json!([
            { "id": "a", "dataset": d, "values": [1] },
            { "id": "a", "dataset": d, "values": [2] }
        ]);
        let err = Condition::read_list(&twice)
            .expect_err("same id")
            .to_string();
        assert!(err.contains("two filters have the id `a`"), "{err}");
    }

    #[test]
    fn values_are_written_as_the_columns_literals() {
        let lit = |v: Json, ty| literal(&v, ty, "c");
        assert_eq!(lit(json!(3), ColType::Int).as_deref(), Ok("3"));
        assert_eq!(lit(json!(3.0), ColType::Int).as_deref(), Ok("3"));
        assert_eq!(lit(json!("4"), ColType::Int).as_deref(), Ok("4"));
        assert!(lit(json!(3.5), ColType::Int).is_err());
        assert_eq!(lit(json!(2.5), ColType::Float).as_deref(), Ok("2.5"));
        assert_eq!(
            lit(json!("O'Brien \"Jr\""), ColType::Text).as_deref(),
            Ok("\"O'Brien \\\"Jr\\\"\"")
        );
        assert_eq!(lit(json!(7), ColType::Text).as_deref(), Ok("\"7\""));
        assert_eq!(lit(json!(true), ColType::Bool).as_deref(), Ok("true"));
        assert_eq!(
            lit(json!("2025-03-01T13:00:00Z"), ColType::Date).as_deref(),
            Ok("\"2025-03-01\"")
        );
        // A time axis brushes in milliseconds.
        assert_eq!(
            lit(json!(1_740_787_200_000_i64), ColType::Date).as_deref(),
            Ok("\"2025-03-01\"")
        );
        assert_eq!(
            lit(json!("2025-03-01"), ColType::Timestamp).as_deref(),
            Ok("\"2025-03-01T00:00:00.000Z\"")
        );
        assert_eq!(lit(json!("12:30"), ColType::Time).ok(), None);
        assert_eq!(
            lit(json!("12:30:05"), ColType::Time).as_deref(),
            Ok("\"12:30:05\"")
        );
        let err = lit(json!("soon"), ColType::Date).expect_err("not a date");
        assert_eq!(err, "\"soon\" is not a value of `c`, a date column");
        assert!(lit(json!({}), ColType::Geometry).is_err());
    }

    #[test]
    fn a_click_and_a_brush_become_formulas() {
        let d = DatasetId::new();
        let c = values(d, "category", json!(["burglary", "theft", null]));
        assert_eq!(
            term(&c, "category", ColType::Text).as_deref(),
            Ok("(category == \"burglary\") || ((category == \"theft\") || (category == null))")
        );
        let bin: Condition = serde_json::from_value(json!({
            "id": "b", "dataset": d, "column": "price",
            "range": { "min": 100, "max": 200, "max_exclusive": true }
        }))
        .expect("reads");
        assert_eq!(
            term(&bin, "price", ColType::Float).as_deref(),
            Ok("price >= 100 && price < 200")
        );
        let from: Condition = serde_json::from_value(json!({
            "id": "b", "dataset": d, "column": "on", "range": { "min": "2025-03-01" }
        }))
        .expect("reads");
        assert_eq!(
            term(&from, "on", ColType::Date).as_deref(),
            Ok("on >= \"2025-03-01\"")
        );
    }

    #[test]
    fn a_condition_propagates_through_foreign_keys() {
        let (incidents, districts, by_month, people) = (
            DatasetId::new(),
            DatasetId::new(),
            DatasetId::new(),
            DatasetId::new(),
        );
        let incidents_shape = StageShape {
            columns: vec![
                col("id", ColType::Int),
                fk("district", "districts"),
                col("category", ColType::Text),
            ],
            grain: Grain::Table {
                table: "incidents".into(),
                key: "id".into(),
            },
        };
        let districts_shape = StageShape {
            columns: vec![col("id", ColType::Int), col("name", ColType::Text)],
            grain: Grain::Table {
                table: "districts".into(),
                key: "id".into(),
            },
        };
        let by_month_shape = StageShape {
            columns: vec![col("month", ColType::Date), col("n", ColType::Int)],
            grain: Grain::Group {
                keys: vec!["month".into()],
            },
        };
        let people_shape = StageShape {
            columns: vec![
                col("id", ColType::Int),
                fk("home", "districts"),
                fk("work", "districts"),
            ],
            grain: Grain::Derived,
        };
        let n = |id, name, shape| Named { id, name, shape };
        let inc = n(incidents, "incidents", &incidents_shape);
        let dis = n(districts, "districts", &districts_shape);
        let mon = n(by_month, "by month", &by_month_shape);
        let ppl = n(people, "people", &people_shape);

        // A district clicked on a map of the districts table (its key) filters
        // the incidents by their foreign key.
        let clicked: Condition = serde_json::from_value(json!({
            "id": "map", "dataset": districts, "values": [3]
        }))
        .expect("reads");
        assert_eq!(
            target_column(&clicked, Some(&dis), &inc),
            Ok(("district".to_owned(), ColType::Int))
        );
        // …and the districts themselves by their key.
        assert_eq!(
            target_column(&clicked, Some(&dis), &dis),
            Ok(("id".to_owned(), ColType::Int))
        );
        // A district picked on the incidents filters the districts by key.
        let picked = values(incidents, "district", json!([3]));
        assert_eq!(
            target_column(&picked, Some(&inc), &dis),
            Ok(("id".to_owned(), ColType::Int))
        );
        // Not through a column that refers to nothing.
        let category = values(incidents, "category", json!(["burglary"]));
        assert_eq!(
            target_column(&category, Some(&inc), &inc),
            Ok(("category".to_owned(), ColType::Text))
        );
        assert_eq!(
            target_column(&category, Some(&inc), &dis),
            Err(
                "`category` is a column of `incidents`, and neither refers to the rows of a \
                 table that `districts` refers to"
                    .to_owned()
            )
        );
        assert_eq!(
            target_column(&picked, Some(&inc), &mon),
            Err("`by month` has no column that refers to `districts`".to_owned())
        );
        // Two columns that refer to districts: not a choice made for anyone.
        assert_eq!(
            target_column(&picked, Some(&inc), &ppl),
            Err(
                "`people` refers to `districts` by `home` and `work`, and a filter on \
                 `district` does not say which"
                    .to_owned()
            )
        );
        // The rows themselves need rows of a table.
        let rows: Condition = serde_json::from_value(json!({
            "id": "rows", "dataset": by_month, "values": [1]
        }))
        .expect("reads");
        assert!(
            target_column(&rows, Some(&mon), &mon)
                .expect_err("no key")
                .contains("not a table's rows")
        );
        assert_eq!(
            target_column(&category, None, &inc),
            Err("the dataset this filter was made on has been deleted".to_owned())
        );
        let gone = values(incidents, "kind", json!(["x"]));
        assert_eq!(
            target_column(&gone, Some(&inc), &inc),
            Err("`kind` is no longer a column of `incidents`".to_owned())
        );
    }

    #[test]
    fn a_scope_joins_its_filters_with_a_layers_own() {
        let d = DatasetId::new();
        let mut scope = Scope::none();
        assert_eq!(scope.and_filter(d, Some(" ")), None);
        assert_eq!(scope.and_filter(d, Some("a > 1")).as_deref(), Some("a > 1"));
        scope.filters.insert(d, "b == 2".into());
        assert_eq!(scope.and_filter(d, None).as_deref(), Some("b == 2"));
        assert_eq!(
            scope.and_filter(d, Some("a > 1")).as_deref(),
            Some("(a > 1) && (b == 2)")
        );
        let ops = scope.operations(d);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, FILTER_OP);
        assert!(scope.operations(DatasetId::new()).is_empty());
    }
}
