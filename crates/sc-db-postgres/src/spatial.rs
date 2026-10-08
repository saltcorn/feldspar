//! Finding PostGIS in a database, installing it where the role may, and the
//! two SQL functions Feldspar adds beside it (analytics TODO A5.1, A5.3).
//!
//! The grid-cell functions answer "which square or hexagonal cell of `size`
//! metres is this geometry in" (`Geo.squareCell`, `Geo.hexCell`). A cell is laid
//! out in the UTM zone of the geometry's point on surface, so its size is metres
//! on the ground and the cells of one zone tile; PostGIS's `ST_Square` and
//! `ST_Hexagon` draw it, and it comes back in WGS84. They are SQL functions
//! rather than expressions the formula translator spells out, because the
//! hexagon's cube rounding refers to each coordinate a dozen times.

use sc_db::SpatialSupport;
use sc_error::{Error, Result};
use tokio_postgres::Client;

/// The functions installed beside PostGIS. `CREATE OR REPLACE`, so a newer
/// definition replaces an older one on the next start.
pub const CELL_FUNCTIONS: &str = r#"
CREATE OR REPLACE FUNCTION _fd_utm_srid(g geometry) RETURNS integer
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  SELECT (CASE WHEN ST_Y(c) < 0 THEN 32700 ELSE 32600 END)
         + LEAST(60, GREATEST(1, floor((ST_X(c) + 180) / 6)::integer + 1))
  FROM (SELECT ST_PointOnSurface(g) AS c) AS s
$$;
CREATE OR REPLACE FUNCTION _fd_square_cell(g geometry, size float8) RETURNS geometry
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  SELECT CASE WHEN size > 0 THEN ST_Transform(ST_Square(size,
           floor(ST_X(p) / size)::integer, floor(ST_Y(p) / size)::integer,
           ST_SetSRID(ST_MakePoint(0, 0), srid)), 4326) END
  FROM (SELECT _fd_utm_srid(g) AS srid) AS z,
       LATERAL (SELECT ST_Transform(ST_PointOnSurface(g), z.srid) AS p) AS q
$$;
CREATE OR REPLACE FUNCTION _fd_hex_cell(g geometry, size float8) RETURNS geometry
LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE
  srid integer; p geometry; q float8; r float8; s float8;
  rq float8; rr float8; rs float8; i integer; j integer;
BEGIN
  IF size <= 0 THEN RETURN NULL; END IF;
  srid := _fd_utm_srid(g);
  p := ST_Transform(ST_PointOnSurface(g), srid);
  -- Axial coordinates of a flat-topped hexagon with edge `size`, rounded in
  -- cube coordinates, then the odd-column offset ST_Hexagon indexes by.
  q := (2.0 / 3.0 * ST_X(p)) / size;
  r := (-1.0 / 3.0 * ST_X(p) + sqrt(3.0) / 3.0 * ST_Y(p)) / size;
  s := -q - r;
  rq := round(q); rr := round(r); rs := round(s);
  IF abs(rq - q) > abs(rr - r) AND abs(rq - q) > abs(rs - s) THEN
    rq := -rr - rs;
  ELSIF abs(rr - r) > abs(rs - s) THEN
    rr := -rq - rs;
  END IF;
  i := rq::integer;
  j := rr::integer + (i - (i & 1)) / 2;
  RETURN ST_Transform(ST_Hexagon(size, i, j, ST_SetSRID(ST_MakePoint(0, 0), srid)), 4326);
END
$$;
"#;

/// Whether PostGIS is installed in the database `client` is connected to; when
/// it is, the cell functions are (re)installed beside it.
pub async fn detect(client: &Client) -> Result<SpatialSupport> {
    let rows = client
        .query(
            "SELECT extversion FROM pg_extension WHERE extname = 'postgis'",
            &[],
        )
        .await
        .map_err(|e| Error::database(format!("looking for PostGIS: {e}")))?;
    if let Some(row) = rows.first() {
        let version: String = row.get(0);
        // Best effort: a role that may not create functions here still has
        // PostGIS, and only the two grid-cell functions are missing.
        let _ = client.batch_execute(CELL_FUNCTIONS).await;
        return Ok(SpatialSupport::Available { version });
    }
    let on_server = client
        .query(
            "SELECT 1 FROM pg_available_extensions WHERE name = 'postgis'",
            &[],
        )
        .await
        .map_err(|e| Error::database(format!("looking for PostGIS: {e}")))?;
    Ok(SpatialSupport::Unavailable {
        reason: if on_server.is_empty() {
            "geometry needs the PostGIS extension, which is not installed on this PostgreSQL \
             server (see OPERATIONS.md §10, \"Geometry with PostGIS\")"
                .to_owned()
        } else {
            "geometry needs the PostGIS extension, which this PostgreSQL server has but this \
             database does not; a superuser can add it with CREATE EXTENSION postgis"
                .to_owned()
        },
    })
}

/// Install PostGIS where the role may, then [`detect`].
pub async fn enable(client: &Client) -> Result<SpatialSupport> {
    let found = detect(client).await?;
    if found.is_available() {
        return Ok(found);
    }
    match client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
    {
        Ok(()) => detect(client).await,
        // Not permitted, or not on the server: `detect`'s sentence says which.
        Err(_) => Ok(found),
    }
}
