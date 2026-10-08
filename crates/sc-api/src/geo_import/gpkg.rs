//! Reading a **GeoPackage**: a SQLite database whose feature tables are listed
//! in `gpkg_contents`, with each table's geometry column and coordinate system
//! in `gpkg_geometry_columns` and `gpkg_spatial_ref_sys`.
//!
//! A geometry is stored as a small GeoPackage header (`GP`, a version, flags
//! saying the byte order, whether there is an envelope and how big, and
//! whether the geometry is empty, then the SRS id and the envelope) followed by
//! ordinary WKB, which PostGIS reads as it is. The coordinate system is its EPSG
//! code where the package names one, and its WKT definition otherwise.

use sc_error::{Error, Result};
use serde_json::{Map, Number, Value as Json};

use super::{Feature, GeoFile, GeoFormat, SourceCrs, SourceGeometry};

/// Read a GeoPackage's one feature table, or the one `layer` names.
pub(super) fn read(bytes: &[u8], layer: Option<&str>) -> Result<GeoFile> {
    // SQLite opens files, so the package is written to one for as long as it
    // is read.
    let path = std::env::temp_dir().join(format!(
        "feldspar-import-{}.gpkg",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, bytes)
        .map_err(|e| Error::database(format!("writing the GeoPackage to read it: {e}")))?;
    let out = read_file(&path, layer);
    let _ = std::fs::remove_file(&path);
    out
}

fn sqlite(e: rusqlite::Error) -> Error {
    Error::invalid(format!("it is not a GeoPackage this can read ({e})"))
}

fn read_file(path: &std::path::Path, layer: Option<&str>) -> Result<GeoFile> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite)?;
    let mut tables: Vec<(String, String, i64)> = Vec::new();
    {
        let mut stmt = db
            .prepare(
                "SELECT c.table_name, g.column_name, g.srs_id FROM gpkg_contents c \
                 JOIN gpkg_geometry_columns g ON g.table_name = c.table_name \
                 WHERE c.data_type = 'features' ORDER BY c.table_name",
            )
            .map_err(sqlite)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(sqlite)?;
        for row in rows {
            tables.push(row.map_err(sqlite)?);
        }
    }
    let names = || {
        tables
            .iter()
            .map(|t| format!("`{}`", t.0))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (table, column, srs_id) = match (tables.as_slice(), layer) {
        ([], _) => return Err(Error::invalid("the GeoPackage has no feature tables")),
        ([one], None) => one.clone(),
        (_, Some(layer)) => tables
            .iter()
            .find(|t| t.0.eq_ignore_ascii_case(layer.trim()))
            .cloned()
            .ok_or_else(|| {
                Error::invalid(format!(
                    "the GeoPackage has no feature table called `{layer}`; it has {}",
                    names()
                ))
            })?,
        (_, None) => {
            return Err(Error::invalid(format!(
                "the GeoPackage holds {} feature tables ({}); say which to import",
                tables.len(),
                names()
            )));
        }
    };

    let mut warnings = Vec::new();
    let crs = srs(&db, srs_id, &mut warnings)?;

    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    let mut stmt = db
        .prepare(&format!("SELECT * FROM {quoted}"))
        .map_err(sqlite)?;
    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut skipped = Vec::new();
    let mut features = Vec::new();
    let mut rows = stmt.query([]).map_err(sqlite)?;
    while let Some(row) = rows.next().map_err(sqlite)? {
        let mut properties = Map::new();
        let mut geometry = None;
        for (i, name) in columns.iter().enumerate() {
            let value = row.get_ref(i).map_err(sqlite)?;
            use rusqlite::types::ValueRef;
            if name.eq_ignore_ascii_case(&column) {
                if let ValueRef::Blob(blob) = value {
                    geometry = wkb_of(blob).map_err(|e| {
                        Error::invalid(format!("feature {}: {e}", features.len() + 1))
                    })?;
                }
                continue;
            }
            let json = match value {
                ValueRef::Null => Json::Null,
                ValueRef::Integer(i) => Json::from(i),
                ValueRef::Real(f) => Number::from_f64(f).map_or(Json::Null, Json::Number),
                ValueRef::Text(t) => Json::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(_) => {
                    if !skipped.contains(name) {
                        skipped.push(name.clone());
                    }
                    continue;
                }
            };
            properties.insert(name.clone(), json);
        }
        features.push(Feature {
            properties,
            geometry: geometry.map(SourceGeometry::Wkb),
        });
    }
    for name in skipped {
        warnings.push(format!(
            "the column `{name}` holds binary values, which are not imported"
        ));
    }
    Ok(GeoFile {
        format: GeoFormat::GeoPackage,
        crs,
        features,
        warnings,
    })
}

/// The coordinate system an SRS id names in the package.
fn srs(db: &rusqlite::Connection, srs_id: i64, warnings: &mut Vec<String>) -> Result<SourceCrs> {
    let found: Option<(String, i64, String)> = db
        .query_row(
            "SELECT organization, organization_coordsys_id, definition \
             FROM gpkg_spatial_ref_sys WHERE srs_id = ?1",
            [srs_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    match found {
        Some((org, code, _)) if org.eq_ignore_ascii_case("EPSG") && code > 0 => i32::try_from(code)
            .map(SourceCrs::Srid)
            .map_err(|_| Error::invalid(format!("EPSG:{code} is not a coordinate system"))),
        Some((_, _, definition)) if !definition.trim().is_empty() && definition != "undefined" => {
            Ok(SourceCrs::Definition(definition))
        }
        _ => {
            warnings.push(format!(
                "the GeoPackage does not say what coordinate system SRS {srs_id} is, so its \
                 coordinates were taken to be WGS84 longitude and latitude"
            ));
            Ok(SourceCrs::Srid(4326))
        }
    }
}

/// The WKB inside a GeoPackage geometry, or `None` for an empty geometry.
fn wkb_of(blob: &[u8]) -> Result<Option<Vec<u8>>> {
    if blob.len() < 8 || &blob[..2] != b"GP" {
        return Err(Error::invalid("its geometry is not a GeoPackage geometry"));
    }
    let flags = blob[3];
    if flags & 0b0010_0000 != 0 {
        return Err(Error::invalid(
            "its geometry is an extended GeoPackage geometry, which this does not read",
        ));
    }
    if flags & 0b0001_0000 != 0 {
        return Ok(None);
    }
    let envelope = match (flags >> 1) & 0b111 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        other => return Err(Error::invalid(format!("{other} is not an envelope kind"))),
    };
    let start = 8 + envelope;
    blob.get(start..)
        .filter(|w| !w.is_empty())
        .map(|w| Some(w.to_vec()))
        .ok_or_else(|| Error::invalid("its geometry ends before its WKB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_and_envelope_are_skipped_to_the_wkb() {
        let wkb = [
            1u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0, 0, 0, 0, 0, 0, 0, 0x40,
        ];
        // No envelope.
        let mut blob = vec![b'G', b'P', 0, 0b0000_0001, 0xe6, 0x10, 0, 0];
        blob.extend_from_slice(&wkb);
        assert_eq!(wkb_of(&blob).expect("read"), Some(wkb.to_vec()));
        // A 32-byte envelope.
        let mut blob = vec![b'G', b'P', 0, 0b0000_0011, 0xe6, 0x10, 0, 0];
        blob.extend_from_slice(&[0u8; 32]);
        blob.extend_from_slice(&wkb);
        assert_eq!(wkb_of(&blob).expect("read"), Some(wkb.to_vec()));
        // Empty.
        assert_eq!(
            wkb_of(&[b'G', b'P', 0, 0b0001_0001, 0, 0, 0, 0]).expect("read"),
            None
        );
        assert!(wkb_of(b"not a geometry").is_err());
    }
}
