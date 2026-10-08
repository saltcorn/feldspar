//! A **new table from a geographic file** (analytics TODO A5.2): GeoJSON, a
//! zipped Shapefile or a GeoPackage.
//!
//! Boundary files are where most GIS work starts, so they import the way a CSV
//! does ([`crate::csv::create_table_from_csv`]): the fields are deduced from the
//! file, the table is made through [`schema_edit::apply`], every feature goes in
//! through the row layer ([`rows::create_row_in`]) in one transaction, and a
//! file whose rows will not go in leaves no table behind.
//!
//! **No projection library.** A file's coordinates are in whatever coordinate
//! system it was made in — British National Grid, a UTM zone, a state plane —
//! and the table stores WGS84. PostGIS converts them: each feature's geometry
//! is sent with its source system (an EPSG code, or the `.prj`'s WKT text,
//! which PROJ reads) through `ST_Transform`, and what comes back is GeoJSON in
//! longitude and latitude, which is then written like any other value. Heights
//! and measures are dropped (`ST_Force2D`): the columns are 2D.
//!
//! The readers ([`geojson`], [`shapefile`], [`gpkg`]) are pure: bytes in,
//! [`GeoFile`] out, so each is tested on fixture files without a database.

mod geojson;
mod gpkg;
mod shapefile;

use std::collections::{BTreeMap, HashSet};

use sc_catalog::{CallerContext, Catalog, SharedTx, Table};
use sc_error::{Error, Result};
use sc_query::{Statement, Value};
use sc_types::{BasicType, GeometryKind};
use serde_json::{Map, Value as Json};

use crate::rows;
use crate::schema_edit;

/// The kinds of file this imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoFormat {
    /// A GeoJSON FeatureCollection, Feature or geometry.
    GeoJson,
    /// A zip holding a Shapefile (`.shp`, `.dbf`, `.prj`, …).
    Shapefile,
    /// A GeoPackage (a SQLite database).
    GeoPackage,
}

impl GeoFormat {
    /// The format of a file, from its first bytes and then its name.
    pub fn detect(file_name: &str, bytes: &[u8]) -> Result<GeoFormat> {
        if bytes.starts_with(b"PK\x03\x04") {
            return Ok(GeoFormat::Shapefile);
        }
        if bytes.starts_with(b"SQLite format 3\0") {
            return Ok(GeoFormat::GeoPackage);
        }
        let lower = file_name.to_ascii_lowercase();
        let first = bytes.iter().find(|b| !b.is_ascii_whitespace());
        if first == Some(&b'{') || lower.ends_with(".geojson") || lower.ends_with(".json") {
            return Ok(GeoFormat::GeoJson);
        }
        Err(Error::invalid(format!(
            "`{file_name}` is not a file this imports: a GeoJSON file, a zipped Shapefile (a \
             .zip holding the .shp, .dbf and .prj) or a GeoPackage (.gpkg)"
        )))
    }

    /// What it is called in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            GeoFormat::GeoJson => "GeoJSON",
            GeoFormat::Shapefile => "Shapefile",
            GeoFormat::GeoPackage => "GeoPackage",
        }
    }
}

/// The coordinate system a file's coordinates are in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceCrs {
    /// An EPSG code: 4326 is WGS84 longitude and latitude.
    Srid(i32),
    /// A WKT or PROJ definition, as a Shapefile's `.prj` holds one.
    Definition(String),
}

/// One feature's geometry as the file has it, in its own coordinate system.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceGeometry {
    /// A GeoJSON geometry object.
    GeoJson(Json),
    /// Well-Known Binary (ISO or extended), as a GeoPackage stores it.
    Wkb(Vec<u8>),
}

impl SourceGeometry {
    /// The kind of geometry, as far as the file says.
    fn kind(&self) -> Option<GeometryKind> {
        match self {
            SourceGeometry::GeoJson(json) => json
                .get("type")
                .and_then(Json::as_str)
                .and_then(GeometryKind::of_geojson_type),
            SourceGeometry::Wkb(bytes) => {
                let order = *bytes.first()?;
                let word: [u8; 4] = bytes.get(1..5)?.try_into().ok()?;
                let word = if order == 0 {
                    u32::from_be_bytes(word)
                } else {
                    u32::from_le_bytes(word)
                };
                Some(match (word & 0x0FFF_FFFF) % 1000 {
                    1 => GeometryKind::Point,
                    2 => GeometryKind::LineString,
                    3 => GeometryKind::Polygon,
                    4 => GeometryKind::MultiPoint,
                    5 => GeometryKind::MultiLineString,
                    6 => GeometryKind::MultiPolygon,
                    _ => GeometryKind::Any,
                })
            }
        }
    }
}

/// One feature: its attributes and its geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    /// The attributes, as JSON values.
    pub properties: Map<String, Json>,
    /// The geometry, or `None` for a feature without one.
    pub geometry: Option<SourceGeometry>,
}

/// What a reader found in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoFile {
    /// What kind of file it was.
    pub format: GeoFormat,
    /// The coordinate system of its geometries.
    pub crs: SourceCrs,
    /// The features, in the file's order.
    pub features: Vec<Feature>,
    /// Things the admin should know that did not stop the import: a Shapefile
    /// without a `.prj`, a column that could not be read.
    pub warnings: Vec<String>,
}

/// Read a geographic file (see [`GeoFormat`]); `layer` picks one of several
/// Shapefiles in a zip, or one of several tables in a GeoPackage.
pub fn read_geo_file(file_name: &str, bytes: &[u8], layer: Option<&str>) -> Result<GeoFile> {
    let format = GeoFormat::detect(file_name, bytes)?;
    let file = match format {
        GeoFormat::GeoJson => geojson::read(bytes),
        GeoFormat::Shapefile => shapefile::read(bytes, layer),
        GeoFormat::GeoPackage => gpkg::read(bytes, layer),
    }
    .map_err(|e| {
        Error::invalid(format!(
            "`{file_name}` could not be read as {}: {}",
            format.describe(),
            e.causes()
        ))
    })?;
    if file.features.is_empty() {
        return Err(Error::invalid(format!(
            "`{file_name}` has no features to import"
        )));
    }
    Ok(file)
}

// --- planning the table ---------------------------------------------------------

/// The geometry column of a planned table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryColumn {
    /// Its name: `geom`, unless an attribute already has that name.
    pub name: String,
    /// The kind every feature's geometry fits.
    pub kind: GeometryKind,
    /// Whether single geometries are made multi to fit (`ST_Multi`): a file
    /// that mixes polygons and multipolygons is a multipolygon column.
    pub promote: bool,
}

/// One attribute column of a planned table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeColumn {
    /// The property it is read from.
    pub property: String,
    /// The field name it becomes.
    pub name: String,
    /// The type deduced from its values.
    pub ty: BasicType,
    /// Whether every feature has a value for it.
    pub required: bool,
}

/// The table a file becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoPlan {
    /// The attribute columns, in the order the file first names them.
    pub columns: Vec<AttributeColumn>,
    /// The geometry column.
    pub geometry: GeometryColumn,
    /// Whether the file's own `id` attribute is the primary key (every feature
    /// has one, a whole number, and no two are the same). Otherwise the table
    /// numbers its rows itself and a file `id` is kept as `source_id`.
    pub id_from_file: bool,
}

/// The column a primary key is called.
const ID: &str = "id";

/// Decide the table's columns from the file's features.
pub fn plan(file: &GeoFile) -> Result<GeoPlan> {
    let mut order: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for f in &file.features {
        for key in f.properties.keys() {
            if seen.insert(key.clone()) {
                order.push(key.clone());
            }
        }
    }
    // The attribute that would be called `id`, whatever its case.
    let id_property = order
        .iter()
        .find(|p| crate::csv::label_to_name(p) == ID)
        .cloned();
    let id_from_file = id_property.as_ref().is_some_and(|property| {
        let mut ids = HashSet::new();
        file.features.iter().all(|f| {
            f.properties
                .get(property)
                .and_then(Json::as_i64)
                .is_some_and(|id| ids.insert(id))
        })
    });

    let mut names: HashSet<String> = HashSet::from([ID.to_owned()]);
    let mut columns = Vec::with_capacity(order.len());
    for property in &order {
        let mut name = crate::csv::label_to_name(property);
        if name.is_empty() {
            continue;
        }
        if name == ID {
            if id_from_file && id_property.as_ref() == Some(property) {
                names.remove(ID);
            } else {
                name = "source_id".to_owned();
            }
        }
        if !names.insert(name.clone()) {
            continue;
        }
        let values: Vec<&Json> = file
            .features
            .iter()
            .filter_map(|f| f.properties.get(property))
            .filter(|v| !v.is_null())
            .collect();
        columns.push(AttributeColumn {
            property: property.clone(),
            name,
            ty: json_type(&values),
            required: values.len() == file.features.len(),
        });
    }

    let geometry_name = ["geom", "geometry", "the_geom"]
        .into_iter()
        .find(|n| !names.contains(*n))
        .unwrap_or("geom_1")
        .to_owned();
    let (kind, promote) = geometry_kind(file.features.iter().filter_map(|f| {
        f.geometry
            .as_ref()
            .map(|g| g.kind().unwrap_or(GeometryKind::Any))
    }));
    Ok(GeoPlan {
        columns,
        geometry: GeometryColumn {
            name: geometry_name,
            kind,
            promote,
        },
        id_from_file,
    })
}

/// The kind of column that holds every one of `kinds`, and whether single
/// geometries have to be made multi to fit it.
fn geometry_kind(kinds: impl Iterator<Item = GeometryKind>) -> (GeometryKind, bool) {
    use GeometryKind::{
        Any, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
    };
    let found: HashSet<GeometryKind> = kinds.collect();
    if found.len() == 1 {
        return (found.into_iter().next().unwrap_or(Any), false);
    }
    for (single, multi) in [
        (Point, MultiPoint),
        (LineString, MultiLineString),
        (Polygon, MultiPolygon),
    ] {
        if found.len() == 2 && found.contains(&single) && found.contains(&multi) {
            return (multi, true);
        }
    }
    (Any, false)
}

/// The type of a column of JSON values.
fn json_type(values: &[&Json]) -> BasicType {
    if values.is_empty() {
        return BasicType::Text;
    }
    if values.iter().all(|v| v.is_boolean()) {
        return BasicType::Bool;
    }
    if values.iter().all(|v| v.as_i64().is_some()) {
        return BasicType::Int;
    }
    if values.iter().all(|v| v.is_number()) {
        return BasicType::Float;
    }
    if values.iter().all(|v| v.is_object() || v.is_array()) {
        return BasicType::Json;
    }
    let strings: Option<Vec<&str>> = values.iter().map(|v| v.as_str()).collect();
    if let Some(strings) = strings {
        if strings
            .iter()
            .all(|s| s.parse::<chrono::NaiveDate>().is_ok())
        {
            return BasicType::Date;
        }
        if strings
            .iter()
            .all(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        {
            return BasicType::Timestamp;
        }
    }
    BasicType::Text
}

/// A value of a column of `ty`, as the row layer takes it.
fn cell(ty: &BasicType, value: &Json) -> Json {
    match (ty, value) {
        (_, Json::Null) => Json::Null,
        (BasicType::Text, Json::String(_)) => value.clone(),
        // A number or a flag in a column of text is its text.
        (BasicType::Text, other) => Json::String(other.to_string()),
        _ => value.clone(),
    }
}

// --- converting the geometries ---------------------------------------------------

/// How many geometries one conversion query carries.
const CHUNK: usize = 500;

/// Every feature's geometry in WGS84 GeoJSON, converted by PostGIS; `None` for a
/// feature without one.
async fn to_wgs84(
    catalog: &Catalog,
    table: &Table,
    file: &GeoFile,
    geometry: &GeometryColumn,
) -> Result<Vec<Option<Json>>> {
    let driver = catalog.driver_for(table)?;
    let mut out: Vec<Option<Json>> = vec![None; file.features.len()];
    let with_geometry: Vec<(usize, &SourceGeometry)> = file
        .features
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.geometry.as_ref().map(|g| (i, g)))
        .collect();
    for chunk in with_geometry.chunks(CHUNK) {
        match convert(&*driver, &file.crs, geometry.promote, chunk).await {
            Ok(converted) => {
                for (i, g) in converted {
                    out[i] = Some(g);
                }
            }
            // One geometry PostGIS will not take fails the whole query; find
            // which, so the sentence can name the feature.
            Err(chunk_error) => {
                for one in chunk {
                    if let Err(e) = convert(&*driver, &file.crs, geometry.promote, &[*one]).await {
                        return Err(Error::invalid(format!(
                            "feature {}: its geometry could not be converted to WGS84: {}",
                            one.0 + 1,
                            plain_database_error(&e)
                        )));
                    }
                }
                return Err(chunk_error);
            }
        }
    }
    Ok(out)
}

/// One conversion query: `(index, geometry)` pairs in, `(index, GeoJSON)` out.
async fn convert(
    driver: &dyn sc_db::DatabaseDriver,
    crs: &SourceCrs,
    promote: bool,
    chunk: &[(usize, &SourceGeometry)],
) -> Result<Vec<(usize, Json)>> {
    let mut binds: Vec<Value> = Vec::with_capacity(chunk.len() * 2 + 1);
    let placeholder = |v: Value, binds: &mut Vec<Value>| {
        binds.push(v);
        format!("${}", binds.len())
    };
    let definition = match crs {
        SourceCrs::Definition(text) => Some(placeholder(Value::Text(text.clone()), &mut binds)),
        SourceCrs::Srid(_) => None,
    };
    let mut rows = Vec::with_capacity(chunk.len());
    let mut wkb = false;
    for (i, g) in chunk {
        let index = placeholder(Value::Int(*i as i64), &mut binds);
        let value = match g {
            SourceGeometry::GeoJson(json) => {
                placeholder(Value::Text(json.to_string()), &mut binds) + "::text"
            }
            SourceGeometry::Wkb(bytes) => {
                wkb = true;
                placeholder(Value::Bytes(bytes.clone()), &mut binds) + "::bytea"
            }
        };
        rows.push(format!("({index}::int8, {value})"));
    }
    // One file is one encoding: GeoJSON text, or WKB.
    let parsed = if wkb {
        "ST_GeomFromWKB(g.b)"
    } else {
        "ST_GeomFromGeoJSON(g.b)"
    };
    let wgs84 = match (crs, &definition) {
        (SourceCrs::Srid(4326), _) => format!("ST_SetSRID({parsed}, 4326)"),
        (SourceCrs::Srid(srid), _) => format!("ST_Transform(ST_SetSRID({parsed}, {srid}), 4326)"),
        (SourceCrs::Definition(_), Some(p)) => format!("ST_Transform({parsed}, {p}::text, 4326)"),
        (SourceCrs::Definition(_), None) => unreachable!("a definition is always bound"),
    };
    let mut expr = format!("ST_Force2D({wgs84})");
    if promote {
        expr = format!("ST_Multi({expr})");
    }
    let sql = format!(
        "SELECT g.i, {expr} FROM (VALUES {}) AS g(i, b) ORDER BY g.i",
        rows.join(", ")
    );
    let result = driver
        .query(&Statement::raw(sql, binds))
        .await?
        .try_collect()
        .await?;
    let mut out = Vec::with_capacity(result.len());
    for row in result {
        let index = match row.get_index(0) {
            Some(Value::Int(i)) => *i as usize,
            _ => return Err(Error::database("the conversion lost a feature's number")),
        };
        match row.get_index(1) {
            Some(Value::Json(g)) => out.push((index, g.clone())),
            Some(Value::Null) | None => {}
            Some(other) => {
                return Err(Error::database(format!(
                    "the conversion returned a {} rather than a geometry",
                    other.kind()
                )));
            }
        }
    }
    Ok(out)
}

/// A database error as the sentence inside it, without the statement.
fn plain_database_error(e: &Error) -> String {
    let text = e.causes();
    let text = text.split("\n  sql:").next().unwrap_or(&text);
    text.trim_start_matches("database error: ")
        .trim_start_matches("query failed: ")
        .to_owned()
}

// --- the import ----------------------------------------------------------------

/// What an import did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeoImportOutcome {
    /// How many features were inserted.
    pub inserted: usize,
    /// What the admin should know that did not stop it.
    pub warnings: Vec<String>,
}

/// Create a table from a geographic file and fill it with the file's features.
///
/// The table has an `id` key (the file's own, when every feature has a
/// different whole number for it), a column per attribute, typed by its values,
/// and a geometry column of the kind every feature fits. **All or nothing**, as
/// for a CSV: a feature that will not go in drops the table and says which.
/// `database` is empty for the primary database or names a connected one, which
/// must have PostGIS.
pub async fn create_table_from_geo_file(
    catalog: &Catalog,
    name: &str,
    database: &str,
    file_name: &str,
    bytes: &[u8],
    layer: Option<&str>,
    context: Option<&CallerContext>,
) -> Result<(Table, GeoImportOutcome)> {
    let file = read_geo_file(file_name, bytes, layer)?;
    let plan = plan(&file)?;
    let mut fields = vec![schema_edit::FieldSpec {
        name: ID.to_owned(),
        type_name: BasicType::Int.name().to_owned(),
        label: "ID".to_owned(),
        required: true,
        primary_key: true,
        ..schema_edit::FieldSpec::default()
    }];
    for c in &plan.columns {
        if c.name == ID {
            continue;
        }
        fields.push(schema_edit::FieldSpec {
            name: c.name.clone(),
            type_name: c.ty.name().to_owned(),
            label: crate::csv::header_label(&c.property),
            required: c.required,
            ..schema_edit::FieldSpec::default()
        });
    }
    fields.push(schema_edit::FieldSpec {
        name: plan.geometry.name.clone(),
        type_name: plan.geometry.kind.type_name().to_owned(),
        label: "Geometry".to_owned(),
        ..schema_edit::FieldSpec::default()
    });
    schema_edit::apply(
        catalog,
        &[schema_edit::Operation::CreateTable {
            name: name.trim().to_owned(),
            database: database.trim().to_owned(),
            settings: schema_edit::TableSettings::default(),
            fields,
        }],
        &schema_edit::ApplyOptions::default(),
    )
    .await?;
    let table = catalog.require(name.trim())?;
    match fill(catalog, &table, &file, &plan, context).await {
        Ok(inserted) => Ok((
            catalog.require(&table.name)?,
            GeoImportOutcome {
                inserted,
                warnings: file.warnings,
            },
        )),
        Err(e) => {
            // The table was made for this file; without its rows it is not a
            // table anybody asked for.
            let _ = schema_edit::apply(
                catalog,
                &[schema_edit::Operation::DropTable {
                    table: table.name.clone(),
                }],
                &schema_edit::ApplyOptions::default(),
            )
            .await;
            Err(Error::invalid(format!(
                "the features could not be imported, so `{}` was not created: {}",
                table.name,
                e.causes()
            )))
        }
    }
}

/// Convert the geometries and write every feature, in one transaction.
async fn fill(
    catalog: &Catalog,
    table: &Table,
    file: &GeoFile,
    plan: &GeoPlan,
    context: Option<&CallerContext>,
) -> Result<usize> {
    let geometries = to_wgs84(catalog, table, file, &plan.geometry).await?;
    let driver = catalog.driver_for(table)?;
    let tx = SharedTx::begin_on(&driver, table.database.clone());
    let executor = rows::Executor::Transaction(tx.clone());
    let types: BTreeMap<&str, &AttributeColumn> = plan
        .columns
        .iter()
        .map(|c| (c.property.as_str(), c))
        .collect();
    for (i, (feature, geometry)) in file.features.iter().zip(geometries).enumerate() {
        let mut body = Map::new();
        for (property, value) in &feature.properties {
            if let Some(c) = types.get(property.as_str()) {
                body.insert(c.name.clone(), cell(&c.ty, value));
            }
        }
        if let Some(g) = geometry {
            body.insert(plan.geometry.name.clone(), g);
        }
        if let Err(e) =
            rows::create_row_in(catalog, table, &Json::Object(body), context, &executor).await
        {
            let _ = tx.rollback().await;
            return Err(Error::invalid(format!("feature {}: {}", i + 1, e.causes())));
        }
    }
    tx.commit().await?;
    if plan.id_from_file {
        crate::csv::advance_identity_sequence(catalog, table, ID).await?;
    }
    Ok(file.features.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feature(properties: Json, geometry: Option<Json>) -> Feature {
        Feature {
            properties: properties.as_object().cloned().unwrap_or_default(),
            geometry: geometry.map(SourceGeometry::GeoJson),
        }
    }

    fn file(features: Vec<Feature>) -> GeoFile {
        GeoFile {
            format: GeoFormat::GeoJson,
            crs: SourceCrs::Srid(4326),
            features,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn the_formats_are_told_apart_by_their_bytes() {
        assert_eq!(
            GeoFormat::detect("a.zip", b"PK\x03\x04rest").ok(),
            Some(GeoFormat::Shapefile)
        );
        assert_eq!(
            GeoFormat::detect("a.gpkg", b"SQLite format 3\0rest").ok(),
            Some(GeoFormat::GeoPackage)
        );
        assert_eq!(
            GeoFormat::detect("x", b"  {\"type\":").ok(),
            Some(GeoFormat::GeoJson)
        );
        let e = GeoFormat::detect("roads.kml", b"<kml>")
            .unwrap_err()
            .to_string();
        assert!(e.contains("zipped Shapefile"), "{e}");
    }

    #[test]
    fn columns_are_typed_by_their_values_and_the_id_is_kept_when_it_can_be_a_key() {
        let point = || Some(json!({"type": "Point", "coordinates": [0, 0]}));
        let plan = plan(&file(vec![
            feature(json!({"id": 7, "Name": "A", "Pop": 10, "Area km2": 1.5, "opened": "2020-01-02", "geom": "x"}), point()),
            feature(json!({"id": 9, "Name": "B", "Pop": 12, "Area km2": 2, "opened": null}), point()),
        ]))
        .expect("a plan");
        assert!(plan.id_from_file);
        let by_name: BTreeMap<&str, &AttributeColumn> =
            plan.columns.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(by_name["name"].ty, BasicType::Text);
        assert_eq!(by_name["pop"].ty, BasicType::Int);
        assert_eq!(by_name["area_km2"].ty, BasicType::Float);
        assert_eq!(by_name["opened"].ty, BasicType::Date);
        assert!(!by_name["opened"].required);
        assert!(by_name["name"].required);
        // `geom` is an attribute here, so the geometry is `geometry`.
        assert_eq!(plan.geometry.name, "geometry");
        assert_eq!(plan.geometry.kind, GeometryKind::Point);

        // Two features with the same id: the table numbers its own rows.
        let plan2 = super::plan(&file(vec![
            feature(json!({"id": 1}), point()),
            feature(json!({"id": 1}), point()),
        ]))
        .expect("a plan");
        assert!(!plan2.id_from_file);
        assert_eq!(plan2.columns[0].name, "source_id");
    }

    #[test]
    fn single_and_multi_geometries_share_a_multi_column() {
        use GeometryKind::*;
        assert_eq!(
            geometry_kind([Polygon, Polygon].into_iter()),
            (Polygon, false)
        );
        assert_eq!(
            geometry_kind([Polygon, MultiPolygon].into_iter()),
            (MultiPolygon, true)
        );
        assert_eq!(geometry_kind([Point, Polygon].into_iter()), (Any, false));
    }
}
