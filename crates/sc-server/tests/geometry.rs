//! The geometry field type (analytics TODO A5.1), end to end over a database
//! with PostGIS: a table with geometry fields made through the admin API, rows
//! written and read as **GeoJSON** through the admin's row endpoints, an
//! application's REST provider and its GraphQL provider, and the refusals —
//! a polygon in a point field, a position outside WGS84, and a geometry field
//! on a database without PostGIS — each with the sentence saying why.
//!
//! The PostGIS half skips with a message on a machine without the
//! `feldspar_postgis_template` database (`OPERATIONS.md` §10).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{
    ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, bootstrap, save_application,
};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use sc_types::{BasicType, GeometryKind, TypeRef};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "maps.example.com";

/// A cookie-jar client over the router (session + CSRF), as a browser would be.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
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
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
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
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        (status, content_type, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, _, bytes) = self.raw(method, path, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-geometry-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

struct Harness {
    admin: Client,
    router: Router,
    apps: Arc<AppMounts>,
    catalog: Arc<Catalog>,
    _db: TestDb,
    _dir: TempDir,
}

/// A server over `db`, booted as `feldspar serve` boots one, with the admin
/// logged in.
async fn setup(db: TestDb, tag: &str) -> sc_error::Result<Harness> {
    let dir = TempDir::new(tag);
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_spatial(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.send("GET", "/api/auth/status", None).await;
    admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    Ok(Harness {
        admin,
        router,
        apps,
        catalog,
        _db: db,
        _dir: dir,
    })
}

const STATION: &str = r#"{"type": "Point", "coordinates": [-0.1246, 51.5308]}"#;

fn park() -> Value {
    json!({"type": "Polygon", "coordinates": [[
        [-0.17, 51.5], [-0.15, 51.5], [-0.15, 51.51], [-0.17, 51.51], [-0.17, 51.5]
    ]]})
}

#[tokio::test]
async fn a_geometry_field_is_geojson_through_rest_and_graphql() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let mut h = setup(db, "roundtrip").await?;

    // The field types offered include the geometry kinds.
    let (_, types) = h.admin.send("GET", "/api/field-types", None).await;
    let names: Vec<&str> = types
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    for kind in GeometryKind::ALL {
        assert!(
            names.contains(&kind.type_name()),
            "{} in {names:?}",
            kind.type_name()
        );
    }

    // A table with a point and a polygon, made the way the admin makes one.
    let (status, body) = h
        .admin
        .send("POST", "/api/tables", Some(json!({ "name": "places" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "name", "type": "text" }),
        json!({ "name": "location", "type": "geometry_point" }),
        json!({ "name": "outline", "type": "geometry_polygon" }),
    ] {
        let (status, body) = h
            .admin
            .send("POST", "/api/tables/places/fields", Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // The column is PostGIS's, in WGS84, and reads back as the same type.
    let client = h._db.client().await?;
    let row = client
        .query_one(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'places'::regclass AND attname = 'location'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "geometry(Point,4326)");
    let table = h.catalog.require("places")?;
    assert_eq!(
        table.field("location").unwrap().base.type_,
        TypeRef::Basic(BasicType::Geometry(GeometryKind::Point))
    );
    // PostGIS's own table is not one of the user's.
    assert!(h.catalog.get("spatial_ref_sys")?.is_none());

    // A row written as GeoJSON — the point as an object, the polygon as an
    // object too — comes back as the same GeoJSON.
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/tables/places/rows",
            Some(json!({
                "id": 1, "name": "King's Cross",
                "location": serde_json::from_str::<Value>(STATION).unwrap(),
                "outline": park(),
            })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    let (_, rows) = h.admin.send("GET", "/api/tables/places/rows", None).await;
    let row = &rows[0];
    assert_eq!(
        row["location"],
        serde_json::from_str::<Value>(STATION).unwrap(),
        "{rows}"
    );
    assert_eq!(row["outline"], park(), "{rows}");
    // PostGIS agrees about what was stored.
    let stored = client
        .query_one(
            "SELECT ST_AsText(location), ST_SRID(location) FROM places",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), "POINT(-0.1246 51.5308)");
    assert_eq!(stored.get::<_, i32>(1), 4326);

    // The refusals say what is wrong.
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/tables/places/rows",
            Some(json!({ "id": 2, "name": "wrong kind", "location": park() })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("holds a point"), "{body}");
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/tables/places/rows",
            Some(json!({ "id": 3, "name": "British National Grid",
                         "location": {"type": "Point", "coordinates": [530000, 180000]} })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("WGS84"), "{body}");

    // An application serving the table over REST and GraphQL.
    let app = Application::new(
        "Maps",
        "maps",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_table(TableId("places".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"))
    .with_api(ApiConfig::new("graphql", "/graphql"));
    save_application(&h.catalog, &app).await?;
    let framework = Arc::new(CodeFramework::new("code", AssetBundle::new()));
    h.apps.mount(MountedApp::new(app, framework, &h.catalog)?)?;

    let mut user = Client::new(h.router.clone(), APP_HOST);
    user.raw("GET", "/graphql/schema.graphql", None).await;
    let (status, body) = user
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // REST: written as GeoJSON, read as GeoJSON.
    let (status, body) = user
        .send(
            "POST",
            "/api/places",
            Some(json!({ "id": 4, "name": "Tower", "location": {"type": "Point", "coordinates": [-0.0761, 51.5081]} })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = user.send("GET", "/api/places?id=eq.4", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = body
        .as_array()
        .cloned()
        .or_else(|| body["rows"].as_array().cloned())
        .unwrap();
    assert_eq!(
        listed[0]["location"],
        json!({"type": "Point", "coordinates": [-0.0761, 51.5081]}),
        "{body}"
    );

    // GraphQL: a `GeoJSON` scalar, carrying the same object.
    let (_, _, sdl) = user.raw("GET", "/graphql/schema.graphql", None).await;
    let sdl = String::from_utf8(sdl).unwrap();
    assert!(sdl.contains("scalar GeoJSON"), "{sdl}");
    assert!(sdl.contains("location: GeoJSON"), "{sdl}");
    let (status, body) = user
        .send(
            "POST",
            "/graphql",
            Some(json!({ "query": "query { places(order_by: [{ id: asc }]) { name location outline } }" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.get("errors"), None, "{body}");
    assert_eq!(body["data"]["places"][0]["outline"], park(), "{body}");
    assert_eq!(
        body["data"]["places"][1]["location"],
        json!({"type": "Point", "coordinates": [-0.0761, 51.5081]}),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_geometry_field_is_refused_where_there_is_no_postgis() -> sc_error::Result<()> {
    // The ordinary test database: Postgres, without the extension installed in
    // it — and a role that may not install it.
    let db = TestDb::new().await?;
    let mut h = setup(db, "refused").await?;
    if h.catalog.primary().spatial().is_available() {
        // A machine whose test role may install PostGIS into every database.
        eprintln!("skipped: PostGIS was installed into a plain test database");
        return Ok(());
    }
    let (status, body) = h
        .admin
        .send("POST", "/api/tables", Some(json!({ "name": "places" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/tables/places/fields",
            Some(json!({ "name": "location", "type": "geometry_point" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("field `location` cannot be a geometry") && text.contains("PostGIS"),
        "{text}"
    );
    Ok(())
}

/// A dataset drawn as a map layer (analytics TODO A5.5): small, its features
/// come back as GeoJSON from `layerData`; large, `layerData` answers the URL
/// template of its vector tiles, and the URL it gives serves them.
#[tokio::test]
async fn a_dataset_is_a_map_layer_as_geojson_or_as_tiles() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let mut h = setup(db, "layers").await?;
    sc_dataset::bootstrap_datasets(&h.catalog).await?;
    let (status, body) = h
        .admin
        .send("POST", "/api/tables", Some(json!({ "name": "places" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "name", "type": "text" }),
        json!({ "name": "location", "type": "geometry_point" }),
    ] {
        let (status, body) = h
            .admin
            .send("POST", "/api/tables/places/fields", Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    for (id, name, lon) in [(1, "a", -0.12), (2, "b", -0.11), (3, "c", -0.10)] {
        let (status, body) = h
            .admin
            .send(
                "POST",
                "/api/tables/places/rows",
                Some(json!({
                    "id": id, "name": name,
                    "location": { "type": "Point", "coordinates": [lon, 51.5] },
                })),
            )
            .await;
        assert!(status.is_success(), "{body}");
    }
    let (status, dataset) = h
        .admin
        .send(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "Places", "base": { "kind": "table", "table": "places" } })),
        )
        .await;
    assert!(status.is_success(), "{dataset}");
    let layer = json!({
        "dataset": dataset["dataset"]["id"],
        "geometry": { "kind": "column", "column": "location" },
    });

    // Three places: GeoJSON, each feature keyed by its row.
    let (status, small) = h
        .admin
        .send("POST", "/api/layers", Some(json!({ "layer": layer })))
        .await;
    assert_eq!(status, StatusCode::OK, "{small}");
    assert_eq!(small["delivery"], "geojson", "{small}");
    assert_eq!(small["count"], 3);
    let features = small["data"]["features"].as_array().expect("features");
    assert_eq!(
        features.iter().map(|f| f["id"].clone()).collect::<Vec<_>>(),
        [json!(1), json!(2), json!(3)]
    );
    assert_eq!(features[0]["properties"], json!({ "id": 1, "name": "a" }));
    assert!(small.get("tiles").is_none_or(Value::is_null), "{small}");

    // Five thousand more: tiles, from a URL the map fills in.
    let client = h._db.client().await?;
    client
        .execute(
            "INSERT INTO places (id, name, location) SELECT g, 'p' || g, \
             ST_SetSRID(ST_MakePoint(-0.2 + g * 0.00002, 51.45 + (g % 100) * 0.001), 4326) \
             FROM generate_series(10, 5010) AS g",
            &[],
        )
        .await
        .unwrap();
    let (status, large) = h
        .admin
        .send("POST", "/api/layers", Some(json!({ "layer": layer })))
        .await;
    assert_eq!(status, StatusCode::OK, "{large}");
    assert_eq!(large["delivery"], "tiles", "{large}");
    assert_eq!(large["count"], 5004);
    assert_eq!(large["source_layer"], "features");
    assert!(large.get("data").is_none_or(Value::is_null));
    let template = large["tiles"].as_str().expect("a template");
    assert!(
        template.starts_with("/api/layers/tiles/{z}/{x}/{y}?layer="),
        "{template}"
    );
    let url = template
        .replace("{z}", "0")
        .replace("{x}", "0")
        .replace("{y}", "0");
    let (status, content_type, bytes) = h.admin.raw("GET", &url, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(content_type, "application/vnd.mapbox-vector-tile");
    assert!(
        bytes.windows(b"features".len()).any(|w| w == b"features"),
        "the tile names its layer"
    );
    // A tile outside the grid is refused with the sentence.
    let (status, refused) = h
        .admin
        .send("GET", &template.replace("{z}/{x}/{y}", "1/2/0"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused.to_string().contains("no tile 2/0 at zoom 1"),
        "{refused}"
    );

    // A layer whose geometry is not one says so, as an answer.
    let (status, refused) = h
        .admin
        .send(
            "POST",
            "/api/layers",
            Some(json!({ "layer": {
                "dataset": dataset["dataset"]["id"],
                "geometry": { "kind": "column", "column": "name" },
            } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(refused["delivery"], "none");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("`name` is text, not a geometry")),
        "{refused}"
    );
    Ok(())
}
