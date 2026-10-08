//! Reading a **zipped Shapefile**: the `.shp` (geometries), the `.dbf`
//! (attributes), the `.prj` (coordinate system, as WKT) and the `.cpg` (the
//! attributes' text encoding), found in the zip by their shared name.
//!
//! The geometries are turned into GeoJSON objects **in the file's own
//! coordinates** — PostGIS reprojects them afterwards (see the module above).
//! A Shapefile polygon is a list of rings, outer rings clockwise and holes
//! anticlockwise, with nothing saying which hole belongs to which outer ring,
//! so each hole goes to the outer ring that contains it; several outer rings
//! make a MultiPolygon. Heights and measures are skipped.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use sc_error::{Error, Result};
use serde_json::{Map, Number, Value as Json, json};

use super::{Feature, GeoFile, GeoFormat, SourceCrs, SourceGeometry};

/// Read a zip holding one Shapefile, or several with `layer` naming one.
pub(super) fn read(bytes: &[u8], layer: Option<&str>) -> Result<GeoFile> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| Error::invalid(format!("it is not a zip file ({e})")))?;
    // Every member by lower-cased name, ignoring the folders macOS adds.
    let mut members: BTreeMap<String, String> = BTreeMap::new();
    for name in zip.file_names() {
        if name.starts_with("__MACOSX/") || name.ends_with('/') {
            continue;
        }
        members.insert(name.to_ascii_lowercase(), name.to_owned());
    }
    let stems: Vec<String> = members
        .keys()
        .filter_map(|n| n.strip_suffix(".shp").map(str::to_owned))
        .collect();
    let stem = match (stems.as_slice(), layer) {
        ([], _) => {
            return Err(Error::invalid(
                "the zip holds no .shp file; a Shapefile is a .shp with its .dbf and .prj beside it",
            ));
        }
        ([one], None) => one.clone(),
        (_, Some(layer)) => {
            let wanted = layer.trim().to_ascii_lowercase();
            stems
                .iter()
                .find(|s| *s == &wanted || s.rsplit('/').next() == Some(wanted.as_str()))
                .cloned()
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "the zip has no Shapefile called `{layer}`; it has {}",
                        names(&stems)
                    ))
                })?
        }
        (_, None) => {
            return Err(Error::invalid(format!(
                "the zip holds {} Shapefiles ({}); say which to import",
                stems.len(),
                names(&stems)
            )));
        }
    };
    let mut member = |ext: &str| -> Result<Option<Vec<u8>>> {
        let Some(name) = members.get(&format!("{stem}.{ext}")) else {
            return Ok(None);
        };
        let mut file = zip
            .by_name(name)
            .map_err(|e| Error::invalid(format!("{name}: {e}")))?;
        let mut out = Vec::new();
        file.read_to_end(&mut out)
            .map_err(|e| Error::invalid(format!("{name}: {e}")))?;
        Ok(Some(out))
    };
    let shp = member("shp")?.unwrap_or_default();
    let dbf = member("dbf")?;
    let prj = member("prj")?;
    let cpg = member("cpg")?;

    let mut warnings = Vec::new();
    let geometries = read_shp(&shp)?;
    let records = match &dbf {
        Some(dbf) => read_dbf(dbf, cpg.as_deref(), &mut warnings)?,
        None => {
            warnings.push("the Shapefile has no .dbf, so its features have no attributes".into());
            vec![Map::new(); geometries.len()]
        }
    };
    if records.len() != geometries.len() {
        return Err(Error::invalid(format!(
            "the .shp has {} shapes and the .dbf {} records; they should be one each",
            geometries.len(),
            records.len()
        )));
    }
    let crs = match prj {
        Some(text) => SourceCrs::Definition(String::from_utf8_lossy(&text).trim().to_owned()),
        None => {
            warnings.push(
                "the Shapefile has no .prj, so its coordinates were taken to be WGS84 longitude \
                 and latitude"
                    .into(),
            );
            SourceCrs::Srid(4326)
        }
    };
    Ok(GeoFile {
        format: GeoFormat::Shapefile,
        crs,
        features: records
            .into_iter()
            .zip(geometries)
            .map(|(properties, geometry)| Feature {
                properties,
                geometry: geometry.map(SourceGeometry::GeoJson),
            })
            .collect(),
        warnings,
    })
}

fn names(stems: &[String]) -> String {
    stems
        .iter()
        .map(|s| format!("`{}`", s.rsplit('/').next().unwrap_or(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

// --- the .shp ------------------------------------------------------------------

struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bytes<'_> {
    fn slice(&mut self, n: usize) -> Result<&[u8]> {
        let out = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or_else(|| Error::invalid("the .shp ends in the middle of a shape"))?;
        self.pos += n;
        Ok(out)
    }

    fn le_i32(&mut self) -> Result<i32> {
        let b = self.slice(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn be_i32(&mut self) -> Result<i32> {
        let b = self.slice(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64> {
        let b = self.slice(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(f64::from_le_bytes(a))
    }

    fn count(&mut self, what: &str) -> Result<usize> {
        let n = self.le_i32()?;
        usize::try_from(n).map_err(|_| Error::invalid(format!("a shape has {n} {what}")))
    }
}

/// Every shape of a `.shp`, as a GeoJSON geometry in the file's coordinates,
/// or `None` for a null shape.
fn read_shp(shp: &[u8]) -> Result<Vec<Option<Json>>> {
    let mut b = Bytes { data: shp, pos: 0 };
    if b.be_i32()? != 9994 {
        return Err(Error::invalid(
            "the .shp does not start as a Shapefile does",
        ));
    }
    b.pos = 100;
    let mut out = Vec::new();
    while b.pos + 8 <= shp.len() {
        let _number = b.be_i32()?;
        let words = b.be_i32()?;
        let length =
            usize::try_from(words).map_err(|_| Error::invalid("a shape of negative length"))? * 2;
        let start = b.pos;
        let mut rec = Bytes {
            data: b.slice(length)?,
            pos: 0,
        };
        out.push(read_shape(&mut rec).map_err(|e| {
            Error::invalid(format!("shape {} (at byte {start}): {e}", out.len() + 1))
        })?);
    }
    Ok(out)
}

fn read_shape(b: &mut Bytes<'_>) -> Result<Option<Json>> {
    let shape_type = b.le_i32()?;
    Ok(Some(match shape_type {
        0 => return Ok(None),
        // Point, PointZ, PointM: x and y first.
        1 | 11 | 21 => json!({"type": "Point", "coordinates": position(b.f64()?, b.f64()?)?}),
        // MultiPoint and its Z and M forms: a box, then the points.
        8 | 18 | 28 => {
            b.slice(32)?;
            let n = b.count("points")?;
            let points = (0..n)
                .map(|_| position(b.f64()?, b.f64()?))
                .collect::<Result<Vec<_>>>()?;
            json!({"type": "MultiPoint", "coordinates": points})
        }
        // PolyLine and Polygon and their Z and M forms: a box, the part
        // starts, then every point; heights and measures follow, unread.
        3 | 13 | 23 | 5 | 15 | 25 => {
            b.slice(32)?;
            let parts = b.count("parts")?;
            let points = b.count("points")?;
            let mut starts = (0..parts)
                .map(|_| b.count("as a part start"))
                .collect::<Result<Vec<_>>>()?;
            starts.push(points);
            let mut xy = Vec::with_capacity(points);
            for _ in 0..points {
                xy.push((b.f64()?, b.f64()?));
            }
            let mut rings: Vec<Vec<(f64, f64)>> = Vec::with_capacity(parts);
            for w in starts.windows(2) {
                let part = xy
                    .get(w[0]..w[1])
                    .ok_or_else(|| Error::invalid("a part starts past the shape's points"))?;
                rings.push(part.to_vec());
            }
            if matches!(shape_type, 3 | 13 | 23) {
                lines(&rings)?
            } else {
                polygons(rings)?
            }
        }
        31 => {
            return Err(Error::invalid(
                "it is a MultiPatch (a 3D surface), which has no 2D geometry",
            ));
        }
        other => return Err(Error::invalid(format!("{other} is not a shape type"))),
    }))
}

fn position(x: f64, y: f64) -> Result<Json> {
    match (Number::from_f64(x), Number::from_f64(y)) {
        (Some(x), Some(y)) => Ok(Json::Array(vec![Json::Number(x), Json::Number(y)])),
        _ => Err(Error::invalid(format!("({x}, {y}) is not a position"))),
    }
}

fn ring_json(ring: &[(f64, f64)]) -> Result<Json> {
    ring.iter()
        .map(|(x, y)| position(*x, *y))
        .collect::<Result<Vec<_>>>()
        .map(Json::Array)
}

fn lines(parts: &[Vec<(f64, f64)>]) -> Result<Json> {
    let lines = parts
        .iter()
        .map(|p| ring_json(p))
        .collect::<Result<Vec<_>>>()?;
    Ok(if lines.len() == 1 {
        json!({"type": "LineString", "coordinates": lines[0]})
    } else {
        json!({"type": "MultiLineString", "coordinates": lines})
    })
}

/// Twice the signed area of a ring: negative when it runs clockwise.
fn signed_area(ring: &[(f64, f64)]) -> f64 {
    ring.windows(2)
        .map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1)
        .sum()
}

/// Whether `p` is inside `ring` (crossing number).
fn contains(ring: &[(f64, f64)], (px, py): (f64, f64)) -> bool {
    let mut inside = false;
    for w in ring.windows(2) {
        let ((x1, y1), (x2, y2)) = (w[0], w[1]);
        if (y1 > py) != (y2 > py) && px < (x2 - x1) * (py - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    inside
}

/// A Shapefile polygon's rings as a Polygon, or a MultiPolygon when it has
/// several outer rings.
fn polygons(rings: Vec<Vec<(f64, f64)>>) -> Result<Json> {
    // Outer rings run clockwise. A writer that wound every ring the other way
    // round is read the other way round too, rather than as all holes.
    let clockwise: Vec<bool> = rings.iter().map(|r| signed_area(r) < 0.0).collect();
    let outer_is_clockwise = clockwise.iter().any(|c| *c);
    let mut shells: Vec<Vec<Vec<(f64, f64)>>> = Vec::new();
    let mut holes = Vec::new();
    for (ring, cw) in rings.into_iter().zip(clockwise) {
        if cw == outer_is_clockwise {
            shells.push(vec![ring]);
        } else {
            holes.push(ring);
        }
    }
    for hole in holes {
        let first = hole.first().copied().unwrap_or_default();
        match shells.iter_mut().find(|s| contains(&s[0], first)) {
            Some(shell) => shell.push(hole),
            // A hole in no shell is a shell of its own.
            None => shells.push(vec![hole]),
        }
    }
    let polygons = shells
        .iter()
        .map(|rings| {
            rings
                .iter()
                .map(|r| ring_json(r))
                .collect::<Result<Vec<_>>>()
                .map(Json::Array)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(if polygons.len() == 1 {
        json!({"type": "Polygon", "coordinates": polygons[0]})
    } else {
        json!({"type": "MultiPolygon", "coordinates": polygons})
    })
}

// --- the .dbf ------------------------------------------------------------------

/// One column of a `.dbf`.
struct DbfField {
    name: String,
    kind: u8,
    length: usize,
    decimals: u8,
}

/// Every record of a `.dbf` as JSON attributes. Text is UTF-8 when the `.cpg`
/// says so or does not say, falling back to Latin-1 for bytes that are not.
fn read_dbf(
    dbf: &[u8],
    cpg: Option<&[u8]>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Map<String, Json>>> {
    let short = || Error::invalid("the .dbf is shorter than its header says");
    if dbf.len() < 32 {
        return Err(short());
    }
    let count = u32::from_le_bytes([dbf[4], dbf[5], dbf[6], dbf[7]]) as usize;
    let header = u16::from_le_bytes([dbf[8], dbf[9]]) as usize;
    let record = u16::from_le_bytes([dbf[10], dbf[11]]) as usize;
    let latin1 = cpg.is_some_and(|c| {
        let c = String::from_utf8_lossy(c).to_ascii_uppercase();
        !c.contains("UTF")
    });

    let mut fields = Vec::new();
    let mut pos = 32;
    while pos + 32 <= header && dbf.get(pos) != Some(&0x0D) {
        let d = &dbf[pos..pos + 32];
        let name_end = d[..11].iter().position(|b| *b == 0).unwrap_or(11);
        fields.push(DbfField {
            name: String::from_utf8_lossy(&d[..name_end]).trim().to_owned(),
            kind: d[11],
            length: d[16] as usize,
            decimals: d[17],
        });
        pos += 32;
    }
    for f in &fields {
        if !matches!(f.kind, b'C' | b'N' | b'F' | b'L' | b'D') {
            warnings.push(format!(
                "the .dbf column `{}` is of type `{}`, which is not read",
                f.name, f.kind as char
            ));
        }
    }

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = header + i * record;
        let row = dbf.get(start..start + record).ok_or_else(short)?;
        // `*` marks a deleted record, which still has its place in the .shp.
        let mut attrs = Map::new();
        let mut at = 1;
        for f in &fields {
            let raw = row.get(at..at + f.length).ok_or_else(short)?;
            at += f.length;
            let text = decode(raw, latin1);
            let text = text.trim_matches(|c: char| c == ' ' || c == '\0');
            let value = match f.kind {
                b'C' if text.is_empty() => Json::Null,
                b'C' => Json::String(text.to_owned()),
                b'N' | b'F' => number(text, f.decimals),
                b'L' => match text {
                    "Y" | "y" | "T" | "t" => Json::Bool(true),
                    "N" | "n" | "F" | "f" => Json::Bool(false),
                    _ => Json::Null,
                },
                b'D' if text.len() == 8 && text.bytes().all(|b| b.is_ascii_digit()) => {
                    Json::String(format!("{}-{}-{}", &text[..4], &text[4..6], &text[6..]))
                }
                b'D' => Json::Null,
                _ => continue,
            };
            attrs.insert(f.name.clone(), value);
        }
        out.push(attrs);
    }
    Ok(out)
}

fn decode(raw: &[u8], latin1: bool) -> String {
    if !latin1 && let Ok(s) = std::str::from_utf8(raw) {
        return s.to_owned();
    }
    raw.iter().map(|b| *b as char).collect()
}

fn number(text: &str, decimals: u8) -> Json {
    if text.is_empty() || text.starts_with('*') {
        return Json::Null;
    }
    if decimals == 0
        && let Ok(i) = text.parse::<i64>()
    {
        return Json::from(i);
    }
    text.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map_or(Json::Null, Json::Number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holes_go_to_the_outer_ring_that_holds_them() {
        // Clockwise squares (outer rings) and an anticlockwise hole in the first.
        let outer_a = vec![(0.0, 0.0), (0.0, 4.0), (4.0, 4.0), (4.0, 0.0), (0.0, 0.0)];
        let hole = vec![(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0), (1.0, 1.0)];
        let outer_b = vec![
            (10.0, 0.0),
            (10.0, 1.0),
            (11.0, 1.0),
            (11.0, 0.0),
            (10.0, 0.0),
        ];
        let one = polygons(vec![outer_a.clone(), hole.clone()]).expect("a polygon");
        assert_eq!(one["type"], "Polygon");
        assert_eq!(one["coordinates"].as_array().map(Vec::len), Some(2));
        let two = polygons(vec![outer_a, outer_b, hole]).expect("polygons");
        assert_eq!(two["type"], "MultiPolygon");
        assert_eq!(two["coordinates"][0].as_array().map(Vec::len), Some(2));
        assert_eq!(two["coordinates"][1].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn dbf_numbers_dates_and_flags_are_typed() {
        assert_eq!(number("42", 0), json!(42));
        assert_eq!(number("4.50", 2), json!(4.5));
        assert_eq!(number("", 0), Json::Null);
        assert_eq!(number("*****", 0), Json::Null);
        assert_eq!(decode(&[0x43, 0x61, 0x66, 0xe9], true), "Café");
        assert_eq!(decode("Café".as_bytes(), false), "Café");
    }
}
