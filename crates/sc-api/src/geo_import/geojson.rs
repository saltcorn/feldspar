//! Reading a **GeoJSON** file: a FeatureCollection, a single Feature, or a bare
//! geometry.
//!
//! RFC 7946 GeoJSON is WGS84 by definition. Files written before it may carry a
//! `crs` member naming another system (`"urn:ogc:def:crs:EPSG::27700"`), which
//! is honoured, since those are exactly the files whose coordinates would
//! otherwise be read as degrees and refused. A Feature's own `id` becomes an
//! `id` attribute when its properties have none.

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

use super::{Feature, GeoFile, GeoFormat, SourceCrs, SourceGeometry};

/// Read a GeoJSON document.
pub(super) fn read(bytes: &[u8]) -> Result<GeoFile> {
    let doc: Json = serde_json::from_slice(bytes)
        .map_err(|e| Error::invalid(format!("it is not JSON ({e})")))?;
    let crs = crs(&doc)?;
    let type_ = doc
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid("it has no `type`, so it is not GeoJSON"))?;
    let features = match type_ {
        "FeatureCollection" => doc
            .get("features")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::invalid("a FeatureCollection needs a `features` array"))?
            .iter()
            .enumerate()
            .map(|(i, f)| feature(f).map_err(|e| Error::invalid(format!("feature {}: {e}", i + 1))))
            .collect::<Result<Vec<_>>>()?,
        "Feature" => vec![feature(&doc)?],
        _ if sc_types::GeometryKind::of_geojson_type(type_).is_some() => vec![Feature {
            properties: Map::new(),
            geometry: Some(SourceGeometry::GeoJson(doc.clone())),
        }],
        other => {
            return Err(Error::invalid(format!(
                "`{other}` is not a GeoJSON type; a file holds a FeatureCollection, a Feature \
                 or a geometry"
            )));
        }
    };
    Ok(GeoFile {
        format: GeoFormat::GeoJson,
        crs,
        features,
        warnings: Vec::new(),
    })
}

fn feature(json: &Json) -> Result<Feature> {
    if json.get("type").and_then(Json::as_str) != Some("Feature") {
        return Err(Error::invalid("a FeatureCollection holds Features"));
    }
    let mut properties = match json.get("properties") {
        Some(Json::Object(p)) => p.clone(),
        None | Some(Json::Null) => Map::new(),
        Some(_) => return Err(Error::invalid("`properties` is an object")),
    };
    if let Some(id) = json.get("id").filter(|id| !id.is_null())
        && !properties.contains_key("id")
    {
        properties.insert("id".to_owned(), id.clone());
    }
    let geometry = match json.get("geometry") {
        None | Some(Json::Null) => None,
        Some(g @ Json::Object(_)) => Some(SourceGeometry::GeoJson(g.clone())),
        Some(_) => return Err(Error::invalid("`geometry` is an object or null")),
    };
    Ok(Feature {
        properties,
        geometry,
    })
}

/// The coordinate system a pre-RFC 7946 `crs` member names, WGS84 without one.
fn crs(doc: &Json) -> Result<SourceCrs> {
    let Some(name) = doc
        .get("crs")
        .and_then(|c| c.get("properties"))
        .and_then(|p| p.get("name"))
        .and_then(Json::as_str)
    else {
        return Ok(SourceCrs::Srid(4326));
    };
    epsg_of(name).map(SourceCrs::Srid).ok_or_else(|| {
        Error::invalid(format!(
            "its `crs` is `{name}`, which is not an EPSG code; save the file as WGS84 GeoJSON, \
             or as a Shapefile with its .prj"
        ))
    })
}

/// The EPSG code a CRS name means: `EPSG:27700`, `urn:ogc:def:crs:EPSG::27700`,
/// `urn:ogc:def:crs:EPSG:6.6:27700`, and OGC's `CRS84` for WGS84.
pub(super) fn epsg_of(name: &str) -> Option<i32> {
    let upper = name.trim().to_ascii_uppercase();
    if upper.ends_with("CRS84") {
        return Some(4326);
    }
    if !upper.contains("EPSG") {
        return None;
    }
    upper.rsplit(':').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_collection_a_feature_and_a_geometry_are_all_files() {
        let collection = json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "id": 3, "properties": {"name": "a"},
             "geometry": {"type": "Point", "coordinates": [1, 2]}},
            {"type": "Feature", "properties": null, "geometry": null}
        ]});
        let file = read(collection.to_string().as_bytes()).expect("read");
        assert_eq!(file.crs, SourceCrs::Srid(4326));
        assert_eq!(file.features.len(), 2);
        assert_eq!(file.features[0].properties["id"], json!(3));
        assert!(file.features[1].geometry.is_none());

        let bare = json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        assert_eq!(
            read(bare.to_string().as_bytes())
                .expect("read")
                .features
                .len(),
            1
        );
    }

    #[test]
    fn an_old_crs_member_is_honoured_and_an_unknown_one_refused() {
        assert_eq!(epsg_of("urn:ogc:def:crs:EPSG::27700"), Some(27700));
        assert_eq!(epsg_of("EPSG:3857"), Some(3857));
        assert_eq!(epsg_of("urn:ogc:def:crs:OGC:1.3:CRS84"), Some(4326));
        assert_eq!(epsg_of("local"), None);
        let doc = json!({"type": "FeatureCollection",
            "crs": {"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::27700"}},
            "features": []});
        assert_eq!(
            read(doc.to_string().as_bytes()).expect("read").crs,
            SourceCrs::Srid(27700)
        );
        let doc = json!({"type": "FeatureCollection",
            "crs": {"type": "name", "properties": {"name": "my grid"}}, "features": []});
        let e = read(doc.to_string().as_bytes()).unwrap_err().to_string();
        assert!(e.contains("not an EPSG code"), "{e}");
    }
}
