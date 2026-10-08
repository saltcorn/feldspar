//! **The attribute table and selection** of the Map workspace (analytics TODO
//! A5.10).
//!
//! A selection is a set of **feature ids**, the same ids [`layer_data`]
//! gives the features ([`crate::layer`]): a row's key while the layer's rows
//! are a table's, and otherwise its place in the dataset's order, from 1. The
//! attribute table ([`layer_rows`]) answers each row's id beside it, so
//! selecting rows highlights the features and clicking features selects the
//! rows.
//!
//! Selecting by anything but a click is a **condition** — a formula over the
//! dataset's columns — evaluated by the database ([`select_features`]):
//!
//! | selection | condition |
//! |---|---|
//! | attribute | the formula given, `price > 100000` |
//! | lasso | `Geo.intersects(g, Geo.fromGeoJSON('…'))`, the lasso's polygon |
//! | within a distance of a point | `Geo.distance(g, Geo.point(lon, lat)) <= d` |
//! | within a distance of another layer's selected features | `Geo.distance(g, Geo.fromGeoJSON('…')) <= d`, one per feature, any of them |
//!
//! where `g` is the layer's geometry formula (`location`,
//! `Geo.point(lon, lat)`, `districtⱵoutline`). **Save selection as dataset**
//! ([`save_selection`]) makes a dataset whose base is the layer's dataset,
//! followed by the layer's filter and a Filter: the condition when the
//! selection was made by one, so the saved dataset keeps meaning "the
//! incidents within 1 km of here" as rows are added; and for a selection made
//! by clicking, the rows' identities — their keys (`id == 4 || id == 9`), or
//! the group keys of an aggregated dataset (`district == 3`).

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use sc_catalog::Catalog;
use sc_dataset::{
    Base, ColType, DatasetDef, Grain, Op, OpStatus, Operation, Options, Schema, SortKey, SortOp,
    StageColumn, compile, read_rows, value_json,
};
use sc_error::{Error, Result};
use sc_query::Value;

use crate::layer::{EXTRA_OP, LayerRequest, Prepared, prepare_with};

/// The most rows the attribute table reads: as many as a layer sends as
/// GeoJSON, so a layer drawn whole has all its rows in the table.
pub const TABLE_ROWS: u64 = crate::layer::GEOJSON_FEATURES;
/// The most rows a selection reads its ids from.
pub const SELECT_ROWS: u64 = 100_000;
/// The most clicked features a saved selection names one by one.
pub const MAX_SAVED_IDS: usize = 1_000;
/// The most features a selection by location measures the distance to.
pub const MAX_NEAR_FEATURES: usize = 200;

/// A layer's rows as the attribute table shows them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LayerRows {
    /// The columns, geometry left out.
    pub columns: Vec<StageColumn>,
    /// The rows, in column order.
    pub rows: Vec<Vec<Json>>,
    /// Each row's feature id.
    pub ids: Vec<Json>,
    /// How many rows the layer has in all.
    pub total: u64,
    /// Whether a feature's id is its row's key, so a selection survives a
    /// change of order and is found in tiles too.
    pub keyed: bool,
    /// Whether the rows are in the order asked for. A layer whose rows are
    /// not a table's is read in its dataset's order, since its ids are places
    /// in that order; the table then sorts what it has.
    pub sorted: bool,
}

/// A layer's rows for its attribute table: the first `limit` (at most
/// [`TABLE_ROWS`]), sorted by `sort` while the rows are a table's, each with
/// its feature id.
pub async fn layer_rows(
    catalog: &Catalog,
    req: &LayerRequest,
    sort: Option<&SortKey>,
    limit: u64,
) -> Result<std::result::Result<LayerRows, String>> {
    let layer = match prepare_with(catalog, req, Vec::new()).await? {
        Ok(layer) => layer,
        Err(e) => return Ok(Err(e)),
    };
    let layer = match sort {
        Some(key) if layer.keyed => {
            if layer.shape.column(&key.formula).is_none() {
                return Ok(Err(format!(
                    "`{}` is not a column of `{}` to sort by",
                    key.formula, layer.name
                )));
            }
            let op = Operation::new(
                format!("{EXTRA_OP}-sort"),
                Op::Sort(SortOp {
                    keys: vec![key.clone()],
                }),
            );
            match prepare_with(catalog, req, vec![(op, "the table's order")]).await? {
                Ok(layer) => layer,
                Err(e) => return Ok(Err(e)),
            }
        }
        _ => layer,
    };
    let sorted = sort.is_none() || layer.keyed;
    let read = read_rows(
        catalog,
        &layer.stage,
        None,
        Some(limit.clamp(1, TABLE_ROWS)),
    )
    .await?;
    let total = sc_dataset::count(catalog, &layer.stage, None).await?;
    let shown: Vec<usize> = read
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name != layer.geometry && c.ty != ColType::Geometry)
        .map(|(i, _)| i)
        .collect();
    let columns = shown.iter().map(|i| read.columns[*i].clone()).collect();
    let rows = read
        .rows
        .iter()
        .map(|r| {
            shown
                .iter()
                .map(|i| r.get(*i).map_or(Json::Null, value_json))
                .collect()
        })
        .collect();
    let ids = (0..read.rows.len())
        .map(|i| feature_id(&layer, &read.keys, i))
        .collect();
    Ok(Ok(LayerRows {
        columns,
        rows,
        ids,
        total,
        keyed: layer.keyed,
        sorted,
    }))
}

/// The feature id of the `i`th row read: its key, or its place from 1 — as
/// [`crate::layer::layer_data`] numbers features.
fn feature_id(layer: &Prepared, keys: &[Value], i: usize) -> Json {
    match keys.get(i) {
        Some(Value::Int(k)) if layer.keyed => json!(k),
        _ => json!(i + 1),
    }
}

/// How features are selected, other than by clicking them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum SelectBy {
    /// The rows a formula over the dataset's columns holds for.
    Condition {
        /// The formula.
        formula: String,
    },
    /// The features a shape drawn on the map touches: a lasso, a box.
    Shape {
        /// A GeoJSON polygon, in longitude and latitude.
        geometry: Json,
    },
    /// The features within a distance of a point.
    NearPoint {
        /// Degrees east.
        longitude: f64,
        /// Degrees north.
        latitude: f64,
        /// Metres.
        distance: f64,
    },
    /// The features within a distance of another layer's selected features.
    NearFeatures {
        /// The other layer.
        layer: LayerRequest,
        /// Its selected features' ids.
        ids: Vec<Json>,
        /// Metres.
        distance: f64,
    },
}

/// The features a selection found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Selected {
    /// Their ids, up to [`SELECT_ROWS`].
    pub ids: Vec<Json>,
    /// How many rows match.
    pub count: u64,
    /// Whether there were more than [`SELECT_ROWS`] to name.
    pub truncated: bool,
    /// The condition they were found by, over the dataset's columns: what a
    /// saved selection filters by.
    pub condition: String,
}

/// A formula's text literal: JSON's string syntax, which the formula
/// language reads.
fn text_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// A distance in metres, as a formula reads it.
fn metres(d: f64) -> std::result::Result<String, String> {
    if d.is_finite() && d > 0.0 {
        Ok(format!("{d}"))
    } else {
        Err(format!(
            "{d} metres is not a distance to select within; give a positive number"
        ))
    }
}

/// `terms` joined by `||`, bracketed as a balanced tree so a long list does
/// not nest a thousand deep.
fn any_of(terms: &[String]) -> String {
    match terms {
        [] => "false".to_owned(),
        [one] => one.clone(),
        _ => {
            let (a, b) = terms.split_at(terms.len() / 2);
            format!("({}) || ({})", any_of(a), any_of(b))
        }
    }
}

/// The condition a selection is made by.
async fn condition_of(
    catalog: &Catalog,
    req: &LayerRequest,
    by: &SelectBy,
) -> Result<std::result::Result<String, String>> {
    // The geometry as stored: a lasso picks the regions it touches, not their
    // centres.
    let g = req.geometry.formula();
    Ok(match by {
        SelectBy::Condition { formula } => {
            if formula.trim().is_empty() {
                Err("write the condition the rows are selected by".to_owned())
            } else {
                Ok(formula.trim().to_owned())
            }
        }
        SelectBy::Shape { geometry } => {
            let kind = geometry
                .get("type")
                .and_then(Json::as_str)
                .unwrap_or_default();
            if !matches!(kind, "Polygon" | "MultiPolygon") {
                Err("a shape to select by is a GeoJSON polygon".to_owned())
            } else {
                let text = serde_json::to_string(geometry)
                    .map_err(|e| Error::serde(format!("a shape does not serialise: {e}")))?;
                Ok(format!(
                    "Geo.intersects({g}, Geo.fromGeoJSON({}))",
                    text_literal(&text)
                ))
            }
        }
        SelectBy::NearPoint {
            longitude,
            latitude,
            distance,
        } => {
            if !(-180.0..=180.0).contains(longitude) || !(-90.0..=90.0).contains(latitude) {
                Err(format!(
                    "[{longitude}, {latitude}] is not a longitude and a latitude"
                ))
            } else {
                metres(*distance).map(|d| {
                    format!("Geo.distance({g}, Geo.point({longitude}, {latitude})) <= {d}")
                })
            }
        }
        SelectBy::NearFeatures {
            layer,
            ids,
            distance,
        } => {
            let d = match metres(*distance) {
                Ok(d) => d,
                Err(e) => return Ok(Err(e)),
            };
            if ids.is_empty() {
                return Ok(Err(
                    "select the features of the other layer to measure from first".to_owned(),
                ));
            }
            if ids.len() > MAX_NEAR_FEATURES {
                return Ok(Err(format!(
                    "{} features are selected in the other layer, and a selection by \
                     location measures from at most {MAX_NEAR_FEATURES}",
                    ids.len()
                )));
            }
            let shapes = match geometries_of(catalog, layer, ids).await? {
                Ok(shapes) => shapes,
                Err(e) => return Ok(Err(format!("the other layer: {e}"))),
            };
            if shapes.is_empty() {
                return Ok(Err(
                    "the other layer's selected features have no geometry".to_owned()
                ));
            }
            let terms: Vec<String> = shapes
                .iter()
                .map(|shape| {
                    format!(
                        "Geo.distance({g}, Geo.fromGeoJSON({})) <= {d}",
                        text_literal(&shape.to_string())
                    )
                })
                .collect();
            Ok(any_of(&terms))
        }
    })
}

/// The geometries, as GeoJSON, of the features of `req` with these ids.
async fn geometries_of(
    catalog: &Catalog,
    req: &LayerRequest,
    ids: &[Json],
) -> Result<std::result::Result<Vec<Json>, String>> {
    let layer = match prepare_with(catalog, req, Vec::new()).await? {
        Ok(layer) => layer,
        Err(e) => return Ok(Err(e)),
    };
    let read = read_rows(catalog, &layer.stage, None, Some(SELECT_ROWS)).await?;
    let at = read.columns.iter().position(|c| c.name == layer.geometry);
    let mut out = Vec::new();
    for i in 0..read.rows.len() {
        if !ids.contains(&feature_id(&layer, &read.keys, i)) {
            continue;
        }
        if let Some(shape) = at
            .and_then(|a| read.rows[i].get(a))
            .map(value_json)
            .filter(|j| !j.is_null())
        {
            out.push(shape);
        }
    }
    Ok(Ok(out))
}

/// The features of `req` a selection finds, and the condition it found them
/// by; or the sentence saying why it cannot.
pub async fn select_features(
    catalog: &Catalog,
    req: &LayerRequest,
    by: &SelectBy,
) -> Result<std::result::Result<Selected, String>> {
    let condition = match condition_of(catalog, req, by).await? {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    let found = ids_where(catalog, req, &condition).await?;
    Ok(found.map(|(ids, count)| Selected {
        truncated: count > ids.len() as u64,
        ids,
        count,
        condition,
    }))
}

/// The ids of the features `condition` holds for, and how many there are.
async fn ids_where(
    catalog: &Catalog,
    req: &LayerRequest,
    condition: &str,
) -> Result<std::result::Result<(Vec<Json>, u64), String>> {
    let probe = match prepare_with(catalog, req, Vec::new()).await? {
        Ok(layer) => layer,
        Err(e) => return Ok(Err(e)),
    };
    let what = "the selection's condition";
    if probe.keyed {
        // Keys survive a Filter: read the rows it keeps.
        let op = Operation::new(format!("{EXTRA_OP}-select"), Op::filter(condition));
        let layer = match prepare_with(catalog, req, vec![(op, what)]).await? {
            Ok(layer) => layer,
            Err(e) => return Ok(Err(e)),
        };
        let read = read_rows(catalog, &layer.stage, None, Some(SELECT_ROWS)).await?;
        let count = sc_dataset::count(catalog, &layer.stage, None).await?;
        let ids = (0..read.rows.len())
            .map(|i| feature_id(&layer, &read.keys, i))
            .collect();
        return Ok(Ok((ids, count)));
    }
    // A place in the order does not survive a Filter: the condition is a
    // column beside every row instead, and the rows it holds for are counted
    // in place.
    let taken: Vec<String> = probe
        .stage
        .shape()
        .columns
        .into_iter()
        .map(|c| c.name)
        .collect();
    let flag = crate::layer::unique_name("selected", &taken);
    let op = Operation::new(
        format!("{EXTRA_OP}-select"),
        Op::calculated(flag.clone(), condition),
    );
    let layer = match prepare_with(catalog, req, vec![(op, what)]).await? {
        Ok(layer) => layer,
        Err(e) => return Ok(Err(e)),
    };
    let read = read_rows(catalog, &layer.stage, None, Some(SELECT_ROWS)).await?;
    let at = read.columns.iter().position(|c| c.name == flag);
    let ids: Vec<Json> = (0..read.rows.len())
        .filter(|i| {
            matches!(
                at.and_then(|a| read.rows[*i].get(a)),
                Some(Value::Bool(true))
            )
        })
        .map(|i| feature_id(&layer, &read.keys, i))
        .collect();
    let count = ids.len() as u64;
    Ok(Ok((ids, count)))
}

/// A value as a formula's literal, when it can be written as one.
fn literal(v: &Value) -> Option<String> {
    match v {
        Value::Int(n) => Some(n.to_string()),
        Value::Float(f) if f.is_finite() => Some(format!("{f}")),
        Value::Decimal(d) => Some(d.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Text(t) => Some(text_literal(t)),
        _ => None,
    }
}

/// The condition naming the clicked features `ids` by what identifies their
/// rows: the table's key while rows are a table's, else the group keys of an
/// aggregated dataset. A sentence when the rows cannot be told apart.
async fn identity_condition(
    catalog: &Catalog,
    req: &LayerRequest,
    ids: &[Json],
) -> Result<std::result::Result<String, String>> {
    if ids.is_empty() {
        return Ok(Err("nothing is selected".to_owned()));
    }
    if ids.len() > MAX_SAVED_IDS {
        return Ok(Err(format!(
            "{} features are selected one by one, and a saved selection names at most \
             {MAX_SAVED_IDS} that way; select them by a condition instead",
            ids.len()
        )));
    }
    let layer = match prepare_with(catalog, req, Vec::new()).await? {
        Ok(layer) => layer,
        Err(e) => return Ok(Err(e)),
    };
    // While rows are a table's and its key is a column, the ids are its values.
    if layer.keyed
        && let Grain::Table { key, .. } = &layer.shape.grain
        && layer
            .shape
            .column(key)
            .is_some_and(|c| c.ty == ColType::Int)
    {
        let mut terms: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(n) = id.as_i64() else {
                return Ok(Err(format!("{id} is not a row's key")));
            };
            terms.push(format!("{key} == {n}"));
        }
        return Ok(Ok(any_of(&terms)));
    }
    let keys: Vec<String> = match &layer.shape.grain {
        Grain::Group { keys } if keys.iter().all(|k| layer.shape.column(k).is_some()) => {
            keys.clone()
        }
        Grain::Table { key, .. } if layer.shape.column(key).is_some() => vec![key.clone()],
        _ => {
            return Ok(Err(format!(
                "the rows of `{}` have nothing that tells them apart, so features picked one \
                 by one cannot be saved; select them by a condition instead",
                layer.name
            )));
        }
    };
    let read = read_rows(catalog, &layer.stage, None, Some(SELECT_ROWS)).await?;
    let index: Vec<Option<usize>> = keys
        .iter()
        .map(|k| read.columns.iter().position(|c| &c.name == k))
        .collect();
    let mut terms = Vec::with_capacity(ids.len());
    for i in 0..read.rows.len() {
        if !ids.contains(&feature_id(&layer, &read.keys, i)) {
            continue;
        }
        let mut parts = Vec::with_capacity(keys.len());
        for (k, at) in keys.iter().zip(&index) {
            let value = at.and_then(|a| read.rows[i].get(a)).unwrap_or(&Value::Null);
            let Some(lit) = literal(value) else {
                return Ok(Err(format!(
                    "the rows of `{}` are told apart by `{k}`, whose values cannot be written \
                     in a condition, so features picked one by one cannot be saved; select them \
                     by a condition instead",
                    layer.name
                )));
            };
            parts.push(format!("{k} == {lit}"));
        }
        terms.push(parts.join(" && "));
    }
    if terms.is_empty() {
        return Ok(Err(
            "none of the selected features is a row of the layer now".to_owned(),
        ));
    }
    Ok(Ok(any_of(&terms)))
}

/// **Save selection as dataset**: a dataset named `name` whose base is the
/// layer's dataset, followed by the layer's filter and a Filter keeping the
/// selection — `condition` when it was made by one, else the clicked `ids`
/// by their rows' identity. Stored, and answered.
pub async fn save_selection(
    catalog: &Catalog,
    req: &LayerRequest,
    name: &str,
    ids: &[Json],
    condition: Option<&str>,
) -> Result<DatasetDef> {
    let formula = match condition.filter(|c| !c.trim().is_empty()) {
        Some(c) => c.trim().to_owned(),
        None => identity_condition(catalog, req, ids)
            .await?
            .map_err(Error::invalid)?,
    };
    let mut def = DatasetDef::new(name.trim(), Base::dataset(req.dataset));
    if let Some(filter) = req.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        def.operations
            .push(Operation::new("layer-filter", Op::filter(filter)));
    }
    def.operations
        .push(Operation::new("selection", Op::filter(formula)));
    let schema = Schema::of_catalog(catalog)?;
    let mut library = sc_dataset::load_library(catalog).await?;
    library.insert(def.clone());
    let compiled = compile(&schema, &library, &def, Options::default());
    if let Some(report) = compiled
        .operations
        .iter()
        .find(|r| r.status == OpStatus::Invalid)
    {
        return Err(Error::invalid(format!(
            "the selection cannot be saved: {}",
            report.error.as_deref().unwrap_or_default()
        )));
    }
    sc_dataset::save_dataset(catalog, &def).await?;
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_is_a_balanced_or() {
        let terms: Vec<String> = (1..=5).map(|i| format!("id == {i}")).collect();
        assert_eq!(
            any_of(&terms),
            "((id == 1) || (id == 2)) || ((id == 3) || ((id == 4) || (id == 5)))"
        );
        assert_eq!(any_of(&terms[..1]), "id == 1");
        assert_eq!(any_of(&[]), "false");
        // A thousand terms nest about ten deep, not a thousand.
        let many: Vec<String> = (0..1000).map(|i| format!("id == {i}")).collect();
        let text = any_of(&many);
        let mut depth = 0i32;
        let mut deepest = 0;
        for c in text.chars() {
            match c {
                '(' => {
                    depth += 1;
                    deepest = deepest.max(depth);
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        assert!(deepest <= 11, "{deepest}");
    }

    #[test]
    fn literals_are_written_as_a_formula_reads_them() {
        assert_eq!(literal(&Value::Int(3)).as_deref(), Some("3"));
        assert_eq!(literal(&Value::Bool(true)).as_deref(), Some("true"));
        assert_eq!(
            literal(&Value::Text("O'Brien \"Jr\"".into())).as_deref(),
            Some("\"O'Brien \\\"Jr\\\"\"")
        );
        assert_eq!(literal(&Value::Null), None);
        assert_eq!(
            text_literal("{\"type\":\"Point\"}"),
            "\"{\\\"type\\\":\\\"Point\\\"}\""
        );
    }

    #[test]
    fn a_selection_by_how_reads_from_json() {
        let by: SelectBy = serde_json::from_value(json!({
            "by": "near_point", "longitude": 0.1, "latitude": 51.5, "distance": 1000
        }))
        .expect("reads");
        assert_eq!(
            by,
            SelectBy::NearPoint {
                longitude: 0.1,
                latitude: 51.5,
                distance: 1000.0
            }
        );
        assert!(metres(0.0).is_err());
        assert!(metres(f64::NAN).is_err());
    }
}
