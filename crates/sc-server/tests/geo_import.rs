//! A new table from a geographic file (analytics TODO A5.2), through the admin
//! API: GeoJSON, a zipped Shapefile in British National Grid, a Shapefile with
//! no `.prj`, and a GeoPackage with two layers — each with its attributes typed
//! and its geometry reprojected to WGS84 by PostGIS — plus the refusals: a
//! package with two layers and none chosen, a feature PostGIS will not take
//! (which leaves no table behind), and a database without PostGIS.
//!
//! The fixtures are written by `fixtures/geo/make_geo_fixtures.py`. The PostGIS
//! tests skip with a message on a machine without `feldspar_postgis_template`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        if method != "GET"
            && method != "HEAD"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if value.is_empty() {
                        self.cookies.remove(name);
                    } else {
                        self.cookies.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// An admin router over `db`, booted with PostGIS looked for, and the admin
/// logged in.
async fn setup(db: &TestDb) -> sc_error::Result<Client> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_spatial(&catalog).await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
    )?;
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok(client)
}

impl Client {
    async fn import(
        &mut self,
        name: &str,
        file_name: &str,
        bytes: &[u8],
        layer: Option<&str>,
    ) -> (StatusCode, Value) {
        let content = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.send(
            "POST",
            "/api/tables/geo",
            Some(json!({
                "name": name, "file_name": file_name, "content_base64": content, "layer": layer
            })),
        )
        .await
    }

    async fn rows(&mut self, table: &str) -> Vec<Value> {
        let (status, body) = self
            .send("GET", &format!("/api/tables/{table}/rows?order=id"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array().cloned().unwrap()
    }

    async fn field_types(&mut self, table: &str) -> HashMap<String, String> {
        let (_, fields) = self
            .send("GET", &format!("/api/tables/{table}/fields"), None)
            .await;
        fields
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().to_owned(),
                    f["type"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }
}

const PARKS: &[u8] = include_bytes!("fixtures/geo/parks.geojson");
const STATIONS_BNG: &[u8] = include_bytes!("fixtures/geo/stations_bng.zip");
const DISTRICTS: &[u8] = include_bytes!("fixtures/geo/districts.zip");
const NETWORK: &[u8] = include_bytes!("fixtures/geo/network.gpkg");

/// Caister Water Tower: Ordnance Survey's worked example, (651409.903,
/// 313177.270) in British National Grid. Its ETRS89 position is
/// (1.7179215, 52.6575703); without a datum grid PROJ lands about 130 m west of
/// it, so the tolerance is 0.003° — enough to tell a reprojected point from one
/// left in metres, or from one in the wrong zone, by orders of magnitude.
const CAISTER: (f64, f64) = (1.7179215, 52.6575703);

#[track_caller]
fn near(point: &Value, (lon, lat): (f64, f64)) {
    assert_eq!(point["type"], "Point", "{point}");
    let c = point["coordinates"].as_array().unwrap();
    let (x, y) = (c[0].as_f64().unwrap(), c[1].as_f64().unwrap());
    assert!(
        (x - lon).abs() < 0.003 && (y - lat).abs() < 0.003,
        "{point} is not near ({lon}, {lat})"
    );
}

#[tokio::test]
async fn geojson_shapefiles_and_geopackages_become_tables_in_wgs84() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let mut admin = setup(&db).await?;

    // --- GeoJSON: typed attributes, the file's ids, polygons made multi -----
    let (status, body) = admin.import("parks", "parks.geojson", PARKS, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["inserted"], 3, "{body}");
    let types = admin.field_types("parks").await;
    assert_eq!(types["name"], "text", "{types:?}");
    assert_eq!(types["area_ha"], "float", "{types:?}");
    assert_eq!(types["gates"], "int", "{types:?}");
    assert_eq!(types["opened"], "date", "{types:?}");
    assert_eq!(types["geom"], "geometry_multipolygon", "{types:?}");
    let parks = admin.rows("parks").await;
    assert_eq!(parks.len(), 3);
    assert_eq!(parks[0]["id"], 1);
    assert_eq!(parks[0]["name"], "Regent's Park");
    assert_eq!(parks[0]["opened"], "1835-01-01");
    // The polygon is a multipolygon of one, with the file's coordinates.
    assert_eq!(parks[0]["geom"]["type"], "MultiPolygon");
    assert_eq!(
        parks[0]["geom"]["coordinates"][0][0][0],
        json!([-0.16, 51.52])
    );
    assert_eq!(
        parks[1]["geom"]["coordinates"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(parks[2]["geom"], Value::Null);

    // --- A Shapefile in British National Grid, with its .prj ---------------
    let (status, body) = admin
        .import("stations", "stations_bng.zip", STATIONS_BNG, None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["warnings"], json!([]), "{body}");
    let types = admin.field_types("stations").await;
    assert_eq!(types["name"], "text", "{types:?}");
    assert_eq!(types["zone"], "int", "{types:?}");
    assert_eq!(types["riders"], "float", "{types:?}");
    assert_eq!(types["step_free"], "bool", "{types:?}");
    assert_eq!(types["opened"], "date", "{types:?}");
    assert_eq!(types["geom"], "geometry_point", "{types:?}");
    let stations = admin.rows("stations").await;
    near(&stations[0]["geom"], CAISTER);
    // 1 km east and north is about 0.0148° of longitude and 0.009° of latitude.
    near(
        &stations[1]["geom"],
        (CAISTER.0 + 0.0148, CAISTER.1 + 0.009),
    );
    assert_eq!(stations[0]["riders"], 1234.5);
    assert_eq!(stations[0]["step_free"], true);
    assert_eq!(stations[0]["opened"], "1932-06-01");
    assert_eq!(stations[1]["name"], "Café on the Green");
    assert_eq!(stations[1]["zone"], Value::Null);

    // --- A Shapefile without a .prj: WGS84, said so ------------------------
    let (status, body) = admin
        .import("districts", "districts.zip", DISTRICTS, None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["warnings"].to_string().contains("no .prj"), "{body}");
    let districts = admin.rows("districts").await;
    assert_eq!(districts[0]["name"], "West");
    // The polygon keeps its hole; the two-part one is two polygons.
    assert_eq!(districts[0]["geom"]["type"], "MultiPolygon");
    assert_eq!(
        districts[0]["geom"]["coordinates"][0]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(
        districts[1]["geom"]["coordinates"].as_array().map(Vec::len),
        Some(2)
    );

    // --- A GeoPackage: two layers, so one is chosen -------------------------
    let (status, body) = admin.import("network", "network.gpkg", NETWORK, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("`stations`") && text.contains("`tracks`"),
        "{text}"
    );
    let (status, body) = admin
        .import("gp_stations", "network.gpkg", NETWORK, Some("stations"))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let types = admin.field_types("gp_stations").await;
    assert_eq!(types["platforms"], "int", "{types:?}");
    assert_eq!(types["opened"], "date", "{types:?}");
    let rows = admin.rows("gp_stations").await;
    near(&rows[0]["geom"], CAISTER);
    assert_eq!(rows[0]["name"], "Caister");
    let (status, body) = admin
        .import("gp_tracks", "network.gpkg", NETWORK, Some("tracks"))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rows = admin.rows("gp_tracks").await;
    assert_eq!(
        rows[0]["geom"],
        json!({"type": "LineString", "coordinates": [[-0.1, 51.5], [-0.12, 51.51]]})
    );

    // --- A feature PostGIS will not take: no table is left behind -----------
    let broken = json!({"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {"name": "fine"},
         "geometry": {"type": "Point", "coordinates": [0, 51]}},
        {"type": "Feature", "properties": {"name": "broken"},
         "geometry": {"type": "LineString", "coordinates": [[0, 51]]}}
    ]});
    let (status, body) = admin
        .import(
            "broken",
            "broken.geojson",
            broken.to_string().as_bytes(),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("feature 2") && text.contains("was not created"),
        "{text}"
    );
    let (status, _) = admin.send("GET", "/api/tables/broken/fields", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_geographic_file_is_refused_where_there_is_no_postgis() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let mut admin = setup(&db).await?;
    let (status, body) = admin.import("parks", "parks.geojson", PARKS, None).await;
    if status == StatusCode::CREATED {
        eprintln!("skipped: PostGIS was installed into a plain test database");
        return Ok(());
    }
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("cannot be a geometry") && text.contains("PostGIS"),
        "{text}"
    );
    Ok(())
}
