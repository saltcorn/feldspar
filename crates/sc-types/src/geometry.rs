//! The **geometry** field type (analytics TODO A5.1).
//!
//! A geometry is a point, a line, a polygon or one of their multi variants, in
//! WGS84 longitude and latitude. It is stored as a PostGIS
//! `geometry(<kind>, 4326)` column, and everywhere above the database driver it
//! is a **GeoJSON geometry object** carried as a [`Value::Json`]: the REST and
//! GraphQL wire shape, the value a formula reads, and what a map draws. The
//! Postgres driver converts between GeoJSON and PostGIS's binary form at the
//! wire, so no query has to wrap a geometry column in `ST_AsGeoJSON`.
//!
//! This module holds the kinds and the check that a GeoJSON value is one: the
//! right `type` for the column, positions of two or three numbers inside the
//! WGS84 range, lines of at least two positions and closed polygon rings. The
//! sentences say which of those failed, because "not a valid geometry" names
//! nothing to fix.

use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::Value as Json;

/// The coordinate system every geometry is stored in: WGS84 longitude and
/// latitude (EPSG:4326).
pub const WGS84: i32 = 4326;

/// Which geometries a geometry field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GeometryKind {
    /// Any geometry, including a collection.
    Any,
    /// A single point.
    Point,
    /// A single line.
    LineString,
    /// A single polygon, possibly with holes.
    Polygon,
    /// Several points.
    MultiPoint,
    /// Several lines.
    MultiLineString,
    /// Several polygons.
    MultiPolygon,
}

impl GeometryKind {
    /// Every kind, in the order a type picker offers them.
    pub const ALL: [GeometryKind; 7] = [
        GeometryKind::Any,
        GeometryKind::Point,
        GeometryKind::LineString,
        GeometryKind::Polygon,
        GeometryKind::MultiPoint,
        GeometryKind::MultiLineString,
        GeometryKind::MultiPolygon,
    ];

    /// The GeoJSON `type` of a geometry of this kind, or `None` for
    /// [`Any`](GeometryKind::Any).
    pub fn geojson_type(self) -> Option<&'static str> {
        Some(match self {
            GeometryKind::Any => return None,
            GeometryKind::Point => "Point",
            GeometryKind::LineString => "LineString",
            GeometryKind::Polygon => "Polygon",
            GeometryKind::MultiPoint => "MultiPoint",
            GeometryKind::MultiLineString => "MultiLineString",
            GeometryKind::MultiPolygon => "MultiPolygon",
        })
    }

    /// The kind a GeoJSON `type` names (`GeometryCollection` is
    /// [`Any`](GeometryKind::Any)'s), or `None` for anything else.
    pub fn of_geojson_type(type_: &str) -> Option<GeometryKind> {
        if type_ == "GeometryCollection" {
            return Some(GeometryKind::Any);
        }
        GeometryKind::ALL
            .into_iter()
            .find(|k| k.geojson_type() == Some(type_))
    }

    /// The field type name: `geometry` for any geometry, `geometry_point`,
    /// `geometry_polygon`, … for one kind. Not `point` or `polygon`, which are
    /// Postgres's own (non-geographic) types and must keep meaning those.
    pub fn type_name(self) -> &'static str {
        match self {
            GeometryKind::Any => "geometry",
            GeometryKind::Point => "geometry_point",
            GeometryKind::LineString => "geometry_linestring",
            GeometryKind::Polygon => "geometry_polygon",
            GeometryKind::MultiPoint => "geometry_multipoint",
            GeometryKind::MultiLineString => "geometry_multilinestring",
            GeometryKind::MultiPolygon => "geometry_multipolygon",
        }
    }

    /// The kind a field type name means.
    pub fn of_type_name(name: &str) -> Option<GeometryKind> {
        GeometryKind::ALL
            .into_iter()
            .find(|k| k.type_name() == name)
    }

    /// The column type for DDL: `geometry(Point,4326)` and so on.
    pub fn sql_type(self) -> &'static str {
        match self {
            GeometryKind::Any => "geometry(Geometry,4326)",
            GeometryKind::Point => "geometry(Point,4326)",
            GeometryKind::LineString => "geometry(LineString,4326)",
            GeometryKind::Polygon => "geometry(Polygon,4326)",
            GeometryKind::MultiPoint => "geometry(MultiPoint,4326)",
            GeometryKind::MultiLineString => "geometry(MultiLineString,4326)",
            GeometryKind::MultiPolygon => "geometry(MultiPolygon,4326)",
        }
    }

    /// The kind a column's SQL type means, lower-cased and without spaces:
    /// `geometry` (no type modifier, as `information_schema` reports it) or
    /// `geometry(<kind>,4326)` (as `format_type` does). A geometry column in
    /// another coordinate system is not one of these, and stays a basic type
    /// the catch-all path shows.
    pub fn of_sql_type(sql_type: &str) -> Option<GeometryKind> {
        let compact: String = sql_type
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if compact == "geometry" {
            return Some(GeometryKind::Any);
        }
        let inner = compact.strip_prefix("geometry(")?.strip_suffix(",4326)")?;
        if inner == "geometry" {
            return Some(GeometryKind::Any);
        }
        GeometryKind::ALL.into_iter().find(|k| {
            k.geojson_type()
                .is_some_and(|t| t.eq_ignore_ascii_case(inner))
        })
    }

    /// The words a sentence uses for a geometry of this kind.
    pub fn describe(self) -> &'static str {
        match self {
            GeometryKind::Any => "a geometry",
            GeometryKind::Point => "a point",
            GeometryKind::LineString => "a line",
            GeometryKind::Polygon => "a polygon",
            GeometryKind::MultiPoint => "a multipoint",
            GeometryKind::MultiLineString => "a multiline",
            GeometryKind::MultiPolygon => "a multipolygon",
        }
    }
}

/// Check that `json` is a GeoJSON geometry a field of `kind` can hold.
pub fn check_geojson(kind: GeometryKind, json: &Json) -> Result<()> {
    let Json::Object(obj) = json else {
        return Err(Error::invalid(format!(
            "{} is written as a GeoJSON geometry object, such as \
             {{\"type\": \"Point\", \"coordinates\": [-0.12, 51.5]}}",
            kind.describe()
        )));
    };
    let type_ = obj.get("type").and_then(Json::as_str).ok_or_else(|| {
        Error::invalid("a GeoJSON geometry needs a `type`, such as \"Point\" or \"Polygon\"")
    })?;
    if matches!(type_, "Feature" | "FeatureCollection") {
        return Err(Error::invalid(format!(
            "this is a GeoJSON {type_}; a field holds the geometry itself, the Feature's \
             `geometry` member"
        )));
    }
    let Some(found) = GeometryKind::of_geojson_type(type_) else {
        return Err(Error::invalid(format!(
            "`{type_}` is not a GeoJSON geometry type; the types are Point, LineString, \
             Polygon, MultiPoint, MultiLineString, MultiPolygon and GeometryCollection"
        )));
    };
    if kind != GeometryKind::Any && found != kind {
        return Err(Error::invalid(format!(
            "this field holds {}, and the value is {} ({type_})",
            kind.describe(),
            if type_ == "GeometryCollection" {
                "a geometry collection"
            } else {
                found.describe()
            }
        )));
    }
    check_shape(type_, obj)
}

/// Check the members of a geometry whose `type` is known to be valid.
fn check_shape(type_: &str, obj: &serde_json::Map<String, Json>) -> Result<()> {
    if type_ == "GeometryCollection" {
        let geometries = obj
            .get("geometries")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::invalid("a GeometryCollection needs a `geometries` array"))?;
        for g in geometries {
            check_geojson(GeometryKind::Any, g)?;
        }
        return Ok(());
    }
    let coordinates = obj
        .get("coordinates")
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid(format!("a {type_} needs a `coordinates` array")))?;
    // An empty geometry (`POINT EMPTY`) is a geometry with no coordinates.
    if coordinates.is_empty() {
        return Ok(());
    }
    match type_ {
        "Point" => position(&Json::Array(coordinates.clone())),
        "LineString" | "MultiPoint" => {
            for p in coordinates {
                position(p)?;
            }
            if type_ == "LineString" && coordinates.len() < 2 {
                return Err(Error::invalid("a line needs at least two positions"));
            }
            Ok(())
        }
        "Polygon" => polygon(coordinates),
        "MultiLineString" => {
            for line in coordinates {
                let line = array(line, "each line of a MultiLineString")?;
                if line.len() < 2 {
                    return Err(Error::invalid("a line needs at least two positions"));
                }
                for p in line {
                    position(p)?;
                }
            }
            Ok(())
        }
        "MultiPolygon" => {
            for p in coordinates {
                polygon(array(p, "each polygon of a MultiPolygon")?)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn polygon(rings: &[Json]) -> Result<()> {
    for ring in rings {
        let ring = array(ring, "each ring of a polygon")?;
        for p in ring {
            position(p)?;
        }
        if ring.len() < 4 {
            return Err(Error::invalid(
                "a polygon's ring needs at least four positions, the last the same as the first",
            ));
        }
        if ring.first() != ring.last() {
            return Err(Error::invalid(
                "a polygon's ring must be closed: its last position the same as its first",
            ));
        }
    }
    Ok(())
}

fn array<'a>(json: &'a Json, what: &str) -> Result<&'a [Json]> {
    json.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| Error::invalid(format!("{what} is an array of positions")))
}

/// A position: longitude, latitude and an optional height, inside WGS84.
fn position(json: &Json) -> Result<()> {
    let numbers: Option<Vec<f64>> = json
        .as_array()
        .map(|a| a.iter().map(Json::as_f64).collect::<Option<Vec<f64>>>())
        .unwrap_or(None);
    let Some(numbers) = numbers.filter(|n| (2..=3).contains(&n.len())) else {
        return Err(Error::invalid(format!(
            "{json} is not a position; a position is [longitude, latitude] or [longitude, \
             latitude, height]"
        )));
    };
    let (lon, lat) = (numbers[0], numbers[1]);
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return Err(Error::invalid(format!(
            "[{lon}, {lat}] is outside the range of longitude (-180 to 180) and latitude \
             (-90 to 90); geometry is stored in WGS84 longitude and latitude, so a file in \
             another coordinate system is imported rather than typed in"
        )));
    }
    Ok(())
}

/// The value a geometry field holds for `json`, checked.
pub fn geometry_value(kind: GeometryKind, json: &Json) -> Result<Value> {
    check_geojson(kind, json)?;
    Ok(Value::Json(json.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_and_sql_types_round_trip() {
        for kind in GeometryKind::ALL {
            assert_eq!(GeometryKind::of_type_name(kind.type_name()), Some(kind));
            assert_eq!(GeometryKind::of_sql_type(kind.sql_type()), Some(kind));
        }
        // What `information_schema` and `format_type` report.
        assert_eq!(
            GeometryKind::of_sql_type("geometry"),
            Some(GeometryKind::Any)
        );
        assert_eq!(
            GeometryKind::of_sql_type("geometry(MultiPolygon, 4326)"),
            Some(GeometryKind::MultiPolygon)
        );
        // Another coordinate system, or Postgres's own `point`, is not ours.
        assert_eq!(GeometryKind::of_sql_type("geometry(Point,27700)"), None);
        assert_eq!(GeometryKind::of_sql_type("point"), None);
    }

    #[test]
    fn a_geometry_of_the_right_kind_passes() {
        let point = json!({"type": "Point", "coordinates": [-0.1276, 51.5072]});
        assert!(check_geojson(GeometryKind::Point, &point).is_ok());
        assert!(check_geojson(GeometryKind::Any, &point).is_ok());
        let square = json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        assert!(check_geojson(GeometryKind::Polygon, &square).is_ok());
        let multi =
            json!({"type": "MultiPolygon", "coordinates": [[[[0, 0], [1, 0], [1, 1], [0, 0]]]]});
        assert!(check_geojson(GeometryKind::MultiPolygon, &multi).is_ok());
        let collection = json!({"type": "GeometryCollection", "geometries": [point, square]});
        assert!(check_geojson(GeometryKind::Any, &collection).is_ok());
        assert!(
            check_geojson(
                GeometryKind::Point,
                &json!({"type": "Point", "coordinates": []})
            )
            .is_ok()
        );
    }

    fn refusal(kind: GeometryKind, json: Json) -> String {
        check_geojson(kind, &json).unwrap_err().to_string()
    }

    #[test]
    fn each_refusal_says_what_is_wrong() {
        let polygon = json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        let e = refusal(GeometryKind::MultiPolygon, polygon);
        assert!(
            e.contains("holds a multipolygon") && e.contains("a polygon"),
            "{e}"
        );
        let e = refusal(
            GeometryKind::Point,
            json!({"type": "Point", "coordinates": [530000, 180000]}),
        );
        assert!(e.contains("WGS84"), "{e}");
        let e = refusal(
            GeometryKind::Polygon,
            json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1]]]}),
        );
        assert!(e.contains("closed"), "{e}");
        let e = refusal(
            GeometryKind::LineString,
            json!({"type": "LineString", "coordinates": [[0, 0]]}),
        );
        assert!(e.contains("two positions"), "{e}");
        let e = refusal(
            GeometryKind::Any,
            json!({"type": "Feature", "geometry": null}),
        );
        assert!(e.contains("Feature's `geometry`"), "{e}");
        let e = refusal(GeometryKind::Any, json!("POINT(1 2)"));
        assert!(e.contains("GeoJSON geometry object"), "{e}");
        let e = refusal(
            GeometryKind::Any,
            json!({"type": "Circle", "coordinates": [0, 0]}),
        );
        assert!(e.contains("not a GeoJSON geometry type"), "{e}");
    }
}
