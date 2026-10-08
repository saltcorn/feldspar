#!/usr/bin/env python3
"""Write the small geographic files `geo_import.rs` imports (analytics TODO A5.2).

Run from this directory: `python3 make_geo_fixtures.py`. Standard library only,
so the files are reproducible without GDAL; the formats are written by hand,
which is also what makes them small enough to read in a hex dump.

- parks.geojson       WGS84 FeatureCollection: a Polygon and a MultiPolygon
                      (so the column is a multipolygon), typed attributes, one
                      feature without a geometry.
- stations_bng.zip    A point Shapefile in British National Grid (EPSG:27700)
                      with an ESRI-style .prj, a UTF-8 .cpg and a .dbf with
                      text, integer, decimal, logical and date columns.
- districts.zip       A polygon Shapefile with no .prj (so WGS84 is assumed,
                      with a warning): one polygon with a hole, one of two
                      parts.
- network.gpkg        A GeoPackage with two feature tables: `stations` (points
                      in EPSG:27700) and `tracks` (a line in WGS84).
"""

import io
import json
import os
import sqlite3
import struct
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))

# Ordnance Survey's worked example, Caister Water Tower, in British National
# Grid; and a second point 1 km east and north of it.
CAISTER = (651409.903, 313177.270)
NEARBY = (652409.903, 314177.270)

BNG_PRJ = (
    'PROJCS["British_National_Grid",GEOGCS["GCS_OSGB_1936",DATUM["D_OSGB_1936",'
    'SPHEROID["Airy_1830",6377563.396,299.3249646]],PRIMEM["Greenwich",0.0],'
    'UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
    'PARAMETER["False_Easting",400000.0],PARAMETER["False_Northing",-100000.0],'
    'PARAMETER["Central_Meridian",-2.0],PARAMETER["Scale_Factor",0.9996012717],'
    'PARAMETER["Latitude_Of_Origin",49.0],UNIT["Meter",1.0]]'
)


def write(name, data):
    mode = "w" if isinstance(data, str) else "wb"
    with open(os.path.join(HERE, name), mode) as f:
        f.write(data)


# --- GeoJSON -----------------------------------------------------------------

def square(lon, lat, size):
    return [[lon, lat], [lon + size, lat], [lon + size, lat + size], [lon, lat + size], [lon, lat]]


def parks():
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "id": 1,
                "properties": {"name": "Regent's Park", "area_ha": 166.0, "opened": "1835-01-01", "gates": 8},
                "geometry": {"type": "Polygon", "coordinates": [square(-0.16, 51.52, 0.01)]},
            },
            {
                "type": "Feature",
                "id": 2,
                "properties": {"name": "The Royal Parks", "area_ha": 2.5, "opened": None, "gates": 3},
                "geometry": {
                    "type": "MultiPolygon",
                    "coordinates": [[square(-0.18, 51.50, 0.005)], [square(-0.15, 51.50, 0.005)]],
                },
            },
            {
                "type": "Feature",
                "id": 3,
                "properties": {"name": "Unmapped garden", "area_ha": 0.1, "opened": "2001-05-04", "gates": 1},
                "geometry": None,
            },
        ],
    }


# --- Shapefiles --------------------------------------------------------------

def bbox(points):
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    return min(xs), min(ys), max(xs), max(ys)


def shp_and_shx(shape_type, contents, all_points):
    """The .shp and .shx for records whose contents are already encoded."""
    def header(length_words):
        xmin, ymin, xmax, ymax = bbox(all_points)
        return (
            struct.pack(">7i", 9994, 0, 0, 0, 0, 0, length_words)
            + struct.pack("<2i", 1000, shape_type)
            + struct.pack("<8d", xmin, ymin, xmax, ymax, 0, 0, 0, 0)
        )

    body = b""
    index = b""
    offset = 50  # in 16-bit words, after the 100-byte header
    for n, content in enumerate(contents, start=1):
        words = len(content) // 2
        body += struct.pack(">2i", n, words) + content
        index += struct.pack(">2i", offset, words)
        offset += 4 + words
    shp = header((100 + len(body)) // 2) + body
    shx = header((100 + len(index)) // 2) + index
    return shp, shx


def point_record(x, y):
    return struct.pack("<i2d", 1, x, y)


def polygon_record(rings):
    points = [p for ring in rings for p in ring]
    xmin, ymin, xmax, ymax = bbox(points)
    out = struct.pack("<i4d2i", 5, xmin, ymin, xmax, ymax, len(rings), len(points))
    start = 0
    for ring in rings:
        out += struct.pack("<i", start)
        start += len(ring)
    for x, y in points:
        out += struct.pack("<2d", x, y)
    return out


def dbf(fields, records):
    """fields: (name, type, length, decimals); records: lists of Python values."""
    header_len = 32 + 32 * len(fields) + 1
    record_len = 1 + sum(f[2] for f in fields)
    out = struct.pack("<B3BIHH20x", 3, 126, 10, 8, len(records), header_len, record_len)
    for name, kind, length, decimals in fields:
        out += struct.pack("<11sc4xBB14x", name.encode("ascii"), kind.encode("ascii"), length, decimals)
    out += b"\x0d"
    for rec in records:
        out += b" "
        for (name, kind, length, decimals), value in zip(fields, rec):
            if value is None:
                text = b""
            elif kind == "C":
                text = value.encode("utf-8")
            elif kind == "N":
                text = (f"{value:.{decimals}f}" if decimals else str(value)).encode("ascii")
            elif kind == "L":
                text = b"T" if value else b"F"
            elif kind == "D":
                text = value.replace("-", "").encode("ascii")
            text = text[:length]
            out += text.rjust(length) if kind == "N" else text.ljust(length)
    return out + b"\x1a"


def zip_of(files):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in files:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)
    return buf.getvalue()


def stations_bng():
    points = [CAISTER, NEARBY]
    shp, shx = shp_and_shx(1, [point_record(*p) for p in points], points)
    fields = [
        ("NAME", "C", 30, 0),
        ("ZONE", "N", 4, 0),
        ("RIDERS", "N", 10, 2),
        ("STEP_FREE", "L", 1, 0),
        ("OPENED", "D", 8, 0),
    ]
    records = [
        ["Caister Water Tower", 1, 1234.5, True, "1932-06-01"],
        ["Café on the Green", None, 99.25, False, None],
    ]
    return zip_of([
        ("stations_bng/stations_bng.shp", shp),
        ("stations_bng/stations_bng.shx", shx),
        ("stations_bng/stations_bng.dbf", dbf(fields, records)),
        ("stations_bng/stations_bng.prj", BNG_PRJ),
        ("stations_bng/stations_bng.cpg", "UTF-8"),
    ])


def clockwise(ring):
    """Shapefile outer rings run clockwise: the squares above run the other way."""
    return list(reversed(ring))


def districts():
    outer = clockwise(square(-0.20, 51.40, 0.10))
    hole = square(-0.17, 51.43, 0.02)  # anticlockwise: a hole
    part_a = clockwise(square(0.00, 51.40, 0.05))
    part_b = clockwise(square(0.10, 51.40, 0.05))
    contents = [polygon_record([outer, hole]), polygon_record([part_a, part_b])]
    shp, shx = shp_and_shx(5, contents, outer + part_a + part_b)
    fields = [("NAME", "C", 20, 0), ("CODE", "C", 4, 0)]
    records = [["West", "W"], ["East", "E"]]
    return zip_of([
        ("districts.shp", shp),
        ("districts.shx", shx),
        ("districts.dbf", dbf(fields, records)),
    ])


# --- GeoPackage ----------------------------------------------------------------

def gpkg_geometry(srs_id, wkb):
    # Magic, version 0, flags: little-endian header, no envelope.
    return b"GP" + bytes([0, 0b00000001]) + struct.pack("<i", srs_id) + wkb


def network():
    path = os.path.join(HERE, "network.gpkg")
    if os.path.exists(path):
        os.remove(path)
    db = sqlite3.connect(path)
    db.executescript(
        """
        PRAGMA application_id = 1196444487;
        CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER PRIMARY KEY,
          organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL,
          definition TEXT NOT NULL, description TEXT);
        CREATE TABLE gpkg_contents (table_name TEXT PRIMARY KEY, data_type TEXT NOT NULL,
          identifier TEXT, description TEXT DEFAULT '', last_change TEXT, min_x DOUBLE,
          min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER);
        CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL,
          geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL,
          m TINYINT NOT NULL);
        CREATE TABLE stations (fid INTEGER PRIMARY KEY AUTOINCREMENT, geom BLOB, name TEXT,
          opened TEXT, platforms INTEGER);
        CREATE TABLE tracks (fid INTEGER PRIMARY KEY AUTOINCREMENT, geom BLOB, name TEXT);
        """
    )
    db.executemany(
        "INSERT INTO gpkg_spatial_ref_sys VALUES (?, ?, ?, ?, ?, ?)",
        [
            ("WGS 84", 4326, "EPSG", 4326, "undefined", None),
            ("OSGB36 / British National Grid", 27700, "EPSG", 27700, BNG_PRJ, None),
        ],
    )
    db.executemany(
        "INSERT INTO gpkg_contents (table_name, data_type, identifier, srs_id) VALUES (?, 'features', ?, ?)",
        [("stations", "stations", 27700), ("tracks", "tracks", 4326)],
    )
    db.executemany(
        "INSERT INTO gpkg_geometry_columns VALUES (?, 'geom', ?, ?, 0, 0)",
        [("stations", "POINT", 27700), ("tracks", "LINESTRING", 4326)],
    )
    for name, (x, y), opened, platforms in [
        ("Caister", CAISTER, "1932-06-01", 2),
        ("Nearby", NEARBY, None, 1),
    ]:
        wkb = struct.pack("<BI2d", 1, 1, x, y)
        db.execute(
            "INSERT INTO stations (geom, name, opened, platforms) VALUES (?, ?, ?, ?)",
            (gpkg_geometry(27700, wkb), name, opened, platforms),
        )
    line = struct.pack("<BII4d", 1, 2, 2, -0.1, 51.5, -0.12, 51.51)
    db.execute("INSERT INTO tracks (geom, name) VALUES (?, ?)", (gpkg_geometry(4326, line), "Spur"))
    db.commit()
    db.execute("VACUUM")
    db.close()


if __name__ == "__main__":
    write("parks.geojson", json.dumps(parks(), indent=1) + "\n")
    write("stations_bng.zip", stations_bng())
    write("districts.zip", districts())
    network()
