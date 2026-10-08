//! PostGIS geometry at the wire (analytics TODO A5.1): **EWKB** in and out, a
//! GeoJSON geometry object in a [`Value::Json`] everywhere above the driver.
//!
//! PostGIS sends and receives `geometry` (and `geography`) in its binary form,
//! Extended Well-Known Binary: a byte-order byte, a type word whose high bits
//! flag a Z, an M and an SRID, the SRID when flagged, then the coordinates. ISO
//! WKB's `+1000`/`+2000`/`+3000` dimension codes (what a GeoPackage stores) are
//! read too. Converting here means every read of a geometry column — a REST
//! list, a dataset stage, the admin grid — gets GeoJSON without a query having
//! to wrap the column in `ST_AsGeoJSON`, and every write binds GeoJSON without
//! one wrapping the placeholder in `ST_GeomFromGeoJSON`.
//!
//! Writing produces 2D little-endian EWKB with SRID 4326, which is what a
//! `geometry(<kind>,4326)` column takes. A GeoJSON position's optional height
//! is accepted and not stored: the columns are 2D, and refusing every file
//! that carries elevations would refuse most of them.

use serde_json::{Map, Number, Value as Json, json};

/// The SRID written into every geometry this module encodes.
const WGS84: u32 = 4326;

const FLAG_Z: u32 = 0x8000_0000;
const FLAG_M: u32 = 0x4000_0000;
const FLAG_SRID: u32 = 0x2000_0000;

/// What went wrong reading or writing a geometry.
pub type GeomError = String;

/// Whether a Postgres type name is one this module converts.
pub fn is_geometry_type(name: &str) -> bool {
    matches!(name, "geometry" | "geography")
}

// --- reading -----------------------------------------------------------------

/// A GeoJSON geometry from (E)WKB bytes.
pub fn ewkb_to_geojson(bytes: &[u8]) -> Result<Json, GeomError> {
    let mut reader = Reader {
        bytes,
        pos: 0,
        little: true,
    };
    let geometry = reader.geometry()?;
    if reader.pos != bytes.len() {
        return Err(format!(
            "{} bytes left over after a geometry",
            bytes.len() - reader.pos
        ));
    }
    Ok(geometry)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    little: bool,
}

/// How many numbers each position has, and which of them to keep.
#[derive(Clone, Copy)]
struct Dims {
    z: bool,
    m: bool,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], GeomError> {
        let end = self.pos + N;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| "the geometry ends early".to_owned())?;
        self.pos = end;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, GeomError> {
        let b = self.take::<4>()?;
        Ok(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self) -> Result<f64, GeomError> {
        let b = self.take::<8>()?;
        Ok(if self.little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    /// One whole geometry, header included.
    fn geometry(&mut self) -> Result<Json, GeomError> {
        let [order] = self.take::<1>()?;
        self.little = match order {
            0 => false,
            1 => true,
            other => return Err(format!("{other} is not a WKB byte order")),
        };
        let word = self.u32()?;
        let mut dims = Dims {
            z: word & FLAG_Z != 0,
            m: word & FLAG_M != 0,
        };
        if word & FLAG_SRID != 0 {
            self.u32()?;
        }
        let mut base = word & 0x0FFF_FFFF;
        // ISO WKB spells the dimensions as thousands.
        match base / 1000 {
            0 => {}
            1 => dims.z = true,
            2 => dims.m = true,
            3 => {
                dims.z = true;
                dims.m = true;
            }
            _ => return Err(format!("{base} is not a WKB geometry type")),
        }
        base %= 1000;
        Ok(match base {
            1 => {
                let p = self.position(dims)?;
                // `POINT EMPTY` is a point of NaNs.
                let coordinates = if p.iter().all(|c| c.is_nan()) {
                    Json::Array(Vec::new())
                } else {
                    numbers(&p)?
                };
                geojson("Point", coordinates)
            }
            2 => geojson("LineString", self.positions(dims)?),
            3 => geojson("Polygon", self.rings(dims)?),
            4..=7 => {
                let n = self.u32()?;
                let mut parts = Vec::with_capacity(n.min(1 << 16) as usize);
                for _ in 0..n {
                    let saved = self.little;
                    parts.push(self.geometry()?);
                    self.little = saved;
                }
                if base == 7 {
                    json!({"type": "GeometryCollection", "geometries": parts})
                } else {
                    let (type_, part_type) = match base {
                        4 => ("MultiPoint", "Point"),
                        5 => ("MultiLineString", "LineString"),
                        _ => ("MultiPolygon", "Polygon"),
                    };
                    let mut coordinates = Vec::with_capacity(parts.len());
                    for part in parts {
                        if part["type"] != part_type {
                            return Err(format!("a {type_} holds a {}", part["type"]));
                        }
                        coordinates.push(part["coordinates"].clone());
                    }
                    geojson(type_, Json::Array(coordinates))
                }
            }
            other => {
                return Err(format!(
                    "WKB geometry type {other} (a curve or a surface) has no GeoJSON form"
                ));
            }
        })
    }

    fn position(&mut self, dims: Dims) -> Result<Vec<f64>, GeomError> {
        let x = self.f64()?;
        let y = self.f64()?;
        let mut p = vec![x, y];
        if dims.z {
            p.push(self.f64()?);
        }
        if dims.m {
            // A measure has no GeoJSON form.
            self.f64()?;
        }
        Ok(p)
    }

    fn positions(&mut self, dims: Dims) -> Result<Json, GeomError> {
        let n = self.u32()?;
        let mut out = Vec::with_capacity(n.min(1 << 20) as usize);
        for _ in 0..n {
            out.push(numbers(&self.position(dims)?)?);
        }
        Ok(Json::Array(out))
    }

    fn rings(&mut self, dims: Dims) -> Result<Json, GeomError> {
        let n = self.u32()?;
        let mut out = Vec::with_capacity(n.min(1 << 16) as usize);
        for _ in 0..n {
            out.push(self.positions(dims)?);
        }
        Ok(Json::Array(out))
    }
}

fn numbers(p: &[f64]) -> Result<Json, GeomError> {
    p.iter()
        .map(|c| {
            Number::from_f64(*c)
                .map(Json::Number)
                .ok_or_else(|| format!("{c} is not a coordinate"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Json::Array)
}

fn geojson(type_: &str, coordinates: Json) -> Json {
    let mut obj = Map::new();
    obj.insert("type".into(), Json::String(type_.into()));
    obj.insert("coordinates".into(), coordinates);
    Json::Object(obj)
}

// --- writing -----------------------------------------------------------------

/// Little-endian 2D EWKB with SRID 4326 for a GeoJSON geometry.
pub fn geojson_to_ewkb(json: &Json) -> Result<Vec<u8>, GeomError> {
    let mut out = Vec::with_capacity(64);
    write_geometry(json, true, &mut out)?;
    Ok(out)
}

fn write_header(base: u32, srid: bool, out: &mut Vec<u8>) {
    out.push(1);
    if srid {
        out.extend_from_slice(&(base | FLAG_SRID).to_le_bytes());
        out.extend_from_slice(&WGS84.to_le_bytes());
    } else {
        out.extend_from_slice(&base.to_le_bytes());
    }
}

fn write_geometry(json: &Json, srid: bool, out: &mut Vec<u8>) -> Result<(), GeomError> {
    let type_ = json
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| "a GeoJSON geometry needs a `type`".to_owned())?;
    if type_ == "GeometryCollection" {
        let parts = json
            .get("geometries")
            .and_then(Json::as_array)
            .ok_or_else(|| "a GeometryCollection needs `geometries`".to_owned())?;
        write_header(7, srid, out);
        write_count(parts.len(), out)?;
        for part in parts {
            write_geometry(part, false, out)?;
        }
        return Ok(());
    }
    let coordinates = json
        .get("coordinates")
        .and_then(Json::as_array)
        .ok_or_else(|| format!("a {type_} needs `coordinates`"))?;
    match type_ {
        "Point" => {
            write_header(1, srid, out);
            if coordinates.is_empty() {
                write_f64(f64::NAN, out);
                write_f64(f64::NAN, out);
            } else {
                write_position(&Json::Array(coordinates.clone()), out)?;
            }
        }
        "LineString" => {
            write_header(2, srid, out);
            write_positions(coordinates, out)?;
        }
        "Polygon" => {
            write_header(3, srid, out);
            write_rings(coordinates, out)?;
        }
        "MultiPoint" | "MultiLineString" | "MultiPolygon" => {
            let base = match type_ {
                "MultiPoint" => 4,
                "MultiLineString" => 5,
                _ => 6,
            };
            write_header(base, srid, out);
            write_count(coordinates.len(), out)?;
            for part in coordinates {
                let items = part.as_array();
                match base {
                    4 => {
                        write_header(1, false, out);
                        write_position(part, out)?;
                    }
                    5 => {
                        write_header(2, false, out);
                        write_positions(items.ok_or("a line is an array")?, out)?;
                    }
                    _ => {
                        write_header(3, false, out);
                        write_rings(items.ok_or("a polygon is an array")?, out)?;
                    }
                }
            }
        }
        other => return Err(format!("`{other}` is not a GeoJSON geometry type")),
    }
    Ok(())
}

fn write_count(n: usize, out: &mut Vec<u8>) -> Result<(), GeomError> {
    let n = u32::try_from(n).map_err(|_| "a geometry with too many parts".to_owned())?;
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

fn write_f64(v: f64, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_position(p: &Json, out: &mut Vec<u8>) -> Result<(), GeomError> {
    let numbers = p
        .as_array()
        .filter(|a| a.len() >= 2)
        .ok_or_else(|| format!("{p} is not a position"))?;
    for n in &numbers[..2] {
        write_f64(
            n.as_f64().ok_or_else(|| format!("{p} is not a position"))?,
            out,
        );
    }
    Ok(())
}

fn write_positions(ps: &[Json], out: &mut Vec<u8>) -> Result<(), GeomError> {
    write_count(ps.len(), out)?;
    for p in ps {
        write_position(p, out)?;
    }
    Ok(())
}

fn write_rings(rings: &[Json], out: &mut Vec<u8>) -> Result<(), GeomError> {
    write_count(rings.len(), out)?;
    for ring in rings {
        write_positions(ring.as_array().ok_or("a ring is an array")?, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(g: Json) {
        let bytes = geojson_to_ewkb(&g).expect("encode");
        assert_eq!(ewkb_to_geojson(&bytes).expect("decode"), g, "{g}");
    }

    #[test]
    fn every_geojson_type_round_trips() {
        round_trip(json!({"type": "Point", "coordinates": [1.0, 2.0]}));
        round_trip(json!({"type": "Point", "coordinates": []}));
        round_trip(json!({"type": "LineString", "coordinates": [[0.0, 0.0], [1.5, -2.25]]}));
        round_trip(json!({"type": "Polygon", "coordinates": [
            [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 0.0]],
            [[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 1.0]]
        ]}));
        round_trip(json!({"type": "MultiPoint", "coordinates": [[0.0, 0.0], [1.0, 1.0]]}));
        round_trip(json!({"type": "MultiLineString", "coordinates": [[[0.0, 0.0], [1.0, 1.0]]]}));
        round_trip(json!({"type": "MultiPolygon", "coordinates": [
            [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]
        ]}));
        round_trip(json!({"type": "GeometryCollection", "geometries": [
            {"type": "Point", "coordinates": [1.0, 2.0]},
            {"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]}
        ]}));
    }

    #[test]
    fn what_postgis_sends_is_read() {
        // `SELECT ST_AsEWKB(ST_GeomFromGeoJSON('{"type":"Point","coordinates":[1,2]}'))`
        let bytes = [
            0x01, 0x01, 0x00, 0x00, 0x20, 0xe6, 0x10, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0,
            0, 0, 0, 0, 0, 0, 0x40,
        ];
        assert_eq!(
            ewkb_to_geojson(&bytes).expect("decode"),
            json!({"type": "Point", "coordinates": [1.0, 2.0]})
        );
        // And what this module writes is that, byte for byte.
        assert_eq!(
            geojson_to_ewkb(&json!({"type": "Point", "coordinates": [1, 2]})).expect("encode"),
            bytes
        );
    }

    #[test]
    fn big_endian_iso_z_wkb_reads_with_its_height() {
        // A GeoPackage's ISO WKB: big-endian `Point Z` (1001).
        let mut bytes = vec![0x00];
        bytes.extend_from_slice(&1001u32.to_be_bytes());
        for v in [3.0f64, 4.0, 5.0] {
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(
            ewkb_to_geojson(&bytes).expect("decode"),
            json!({"type": "Point", "coordinates": [3.0, 4.0, 5.0]})
        );
    }

    #[test]
    fn a_height_is_not_written_and_garbage_is_refused() {
        let with_height =
            geojson_to_ewkb(&json!({"type": "Point", "coordinates": [1, 2, 99]})).expect("encode");
        assert_eq!(
            ewkb_to_geojson(&with_height).expect("decode"),
            json!({"type": "Point", "coordinates": [1.0, 2.0]})
        );
        assert!(ewkb_to_geojson(&[1, 1, 0]).is_err());
        assert!(geojson_to_ewkb(&json!({"type": "Circle", "coordinates": [0, 0]})).is_err());
    }
}
