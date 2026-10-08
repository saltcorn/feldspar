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

/// Map panels (analytics TODO A5.6–A5.7) through the API: the explorer's
/// suggestion and its geometry sources, a map drawn with the domains of its
/// encoded columns, as GeoJSON and as tiles, and the base map setting — what
/// `mapSettings` answers and the hosts the Analytics UI's policy names.
#[tokio::test]
async fn a_map_panel_is_suggested_drawn_and_its_base_map_allowed() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let mut h = setup(db, "maps").await?;
    sc_dataset::bootstrap_datasets(&h.catalog).await?;
    sc_config::bootstrap(&h.catalog).await?;
    let (status, body) = h
        .admin
        .send("POST", "/api/tables", Some(json!({ "name": "sightings" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "species", "type": "text" }),
        json!({ "name": "count", "type": "int" }),
        json!({ "name": "lng", "type": "float" }),
        json!({ "name": "lat", "type": "float" }),
    ] {
        let (status, body) = h
            .admin
            .send("POST", "/api/tables/sightings/fields", Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    for (id, species, count, lng) in [(1, "heron", 2, -0.12), (2, "egret", 5, -0.11), (3, "heron", 9, -0.10)] {
        let (status, body) = h
            .admin
            .send(
                "POST",
                "/api/tables/sightings/rows",
                Some(json!({ "id": id, "species": species, "count": count, "lng": lng, "lat": 51.5 })),
            )
            .await;
        assert!(status.is_success(), "{body}");
    }
    let (status, dataset) = h
        .admin
        .send(
            "POST",
            "/api/datasets",
            Some(json!({ "name": "Sightings", "base": { "kind": "table", "table": "sightings" } })),
        )
        .await;
    assert!(status.is_success(), "{dataset}");
    let id = dataset["dataset"]["id"].clone();

    // No geometry column: the points are made from `lng` and `lat`.
    let (status, suggested) = h
        .admin
        .send(
            "POST",
            "/api/maps/suggest",
            Some(json!({
                "dataset": id,
                "assignment": { "color": { "field": "species" }, "size": { "field": "count" } },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{suggested}");
    assert_eq!(
        suggested["sources"],
        json!([{ "source": { "kind": "lon_lat", "longitude": "lng", "latitude": "lat" },
                 "label": "`lng` and `lat`" }])
    );
    let spec = suggested["spec"].clone();
    assert_eq!(spec["layers"][0]["geometry"]["kind"], "lon_lat", "{spec}");

    let (status, drawn) = h
        .admin
        .send("POST", "/api/maps/render", Some(json!({ "spec": spec })))
        .await;
    assert_eq!(status, StatusCode::OK, "{drawn}");
    let layer = &drawn["layers"][0];
    assert_eq!(layer["data"]["delivery"], "geojson", "{layer}");
    assert_eq!(layer["data"]["geometry"], json!(["point"]));
    assert_eq!(layer["domains"]["color"]["values"], json!(["egret", "heron"]));
    assert_eq!(layer["domains"]["size"]["min"], json!(2.0));
    assert_eq!(layer["domains"]["size"]["max"], json!(9.0));

    // Five thousand more: tiles, with the same domains over every feature.
    let client = h._db.client().await?;
    client
        .execute(
            "INSERT INTO sightings (id, species, count, lng, lat) SELECT g, 'gull', 20, \
             -0.2 + g * 0.00002, 51.45 FROM generate_series(10, 5010) AS g",
            &[],
        )
        .await
        .unwrap();
    let (_, drawn) = h
        .admin
        .send("POST", "/api/maps/render", Some(json!({ "spec": spec })))
        .await;
    let layer = &drawn["layers"][0];
    assert_eq!(layer["data"]["delivery"], "tiles", "{layer}");
    let template = layer["data"]["tiles"].as_str().expect("a template");
    let (status, _, _) = h
        .admin
        .raw("GET", &template.replace("{z}/{x}/{y}", "0/0/0"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        layer["domains"]["color"]["values"],
        json!(["egret", "gull", "heron"])
    );
    assert_eq!(layer["domains"]["size"]["max"], json!(20.0));

    // The base map: OpenFreeMap until it is set, and its host in the policy.
    let policy = |h: &mut Harness| {
        let router = h.router.clone();
        let cookies = h.admin.cookies.clone();
        async move {
            let jar = cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            let request = Request::get("/analytics/")
                .header(header::HOST, BASE_DOMAIN)
                .header(header::COOKIE, jar)
                .body(Body::empty())
                .unwrap();
            let response = router.oneshot(request).await.unwrap();
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .expect("a policy")
                .to_str()
                .unwrap()
                .to_owned()
        }
    };
    let (_, settings) = h.admin.send("GET", "/api/maps/settings", None).await;
    assert_eq!(
        settings,
        json!({ "style": sc_config::DEFAULT_MAP_STYLE, "style_dark": sc_config::DEFAULT_MAP_STYLE_DARK,
                "hosts": ["https://tiles.openfreemap.org"] })
    );
    assert!(
        policy(&mut h)
            .await
            .contains("connect-src 'self' https://tiles.openfreemap.org;")
    );
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                "map_style": "https://maps.example.org/light.json",
                "map_style_dark": "",
                "map_hosts": "https://glyphs.example.net",
            } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, settings) = h.admin.send("GET", "/api/maps/settings", None).await;
    assert_eq!(
        settings,
        json!({ "style": "https://maps.example.org/light.json",
                "style_dark": "https://maps.example.org/light.json",
                "hosts": ["https://maps.example.org", "https://glyphs.example.net"] })
    );
    let served = policy(&mut h).await;
    assert!(
        served.contains("connect-src 'self' https://maps.example.org https://glyphs.example.net;"),
        "{served}"
    );
    assert!(
        served.contains("img-src 'self' data: blob: https://maps.example.org https://glyphs.example.net;"),
        "{served}"
    );
    assert!(!served.contains("openfreemap"), "{served}");
    // A host that is not one is refused where it was typed, and nothing of it
    // reaches the policy.
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "map_hosts": "https://ok.org; script-src *" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("map host"), "{body}");
    assert!(!policy(&mut h).await.contains("script-src *"));
    Ok(())
}

/// The CSP the Analytics UI is served under, for the signed-in admin.
async fn analytics_policy(h: &Harness) -> String {
    let jar = h
        .admin
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let request = Request::get("/analytics/")
        .header(header::HOST, BASE_DOMAIN)
        .header(header::COOKIE, jar)
        .body(Body::empty())
        .unwrap();
    let response = h.router.clone().oneshot(request).await.unwrap();
    response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("a policy")
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn the_map_workspace_selects_saves_runs_tools_and_is_a_panel() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let mut h = setup(db, "workspace").await?;
    sc_dataset::bootstrap_datasets(&h.catalog).await?;
    sc_config::bootstrap(&h.catalog).await?;
    sc_analytics::bootstrap_workspaces(&h.catalog).await?;
    sc_model::bootstrap_models(&h.catalog).await?;
    for (table, fields) in [
        (
            "zones",
            vec![
                json!({ "name": "id", "type": "int", "primary_key": true }),
                json!({ "name": "name", "type": "text" }),
                json!({ "name": "outline", "type": "geometry_polygon" }),
            ],
        ),
        (
            "calls",
            vec![
                json!({ "name": "id", "type": "int", "primary_key": true }),
                json!({ "name": "kind", "type": "text" }),
                json!({ "name": "location", "type": "geometry_point" }),
            ],
        ),
    ] {
        let (status, body) = h
            .admin
            .send("POST", "/api/tables", Some(json!({ "name": table })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        for field in fields {
            let (status, body) = h
                .admin
                .send("POST", &format!("/api/tables/{table}/fields"), Some(field))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
    }
    let square = |w: f64| {
        json!({ "type": "Polygon", "coordinates": [[
            [w, 51.0], [w + 0.1, 51.0], [w + 0.1, 51.1], [w, 51.1], [w, 51.0] ]] })
    };
    for (id, name, west) in [(1, "west", 0.0), (2, "middle", 0.1), (3, "east", 0.2)] {
        let (status, body) = h
            .admin
            .send(
                "POST",
                "/api/tables/zones/rows",
                Some(json!({ "id": id, "name": name, "outline": square(west) })),
            )
            .await;
        assert!(status.is_success(), "{body}");
    }
    // Three calls in the west, one in the middle, none in the east.
    for (id, kind, lon) in [(1, "fire", 0.01), (2, "flood", 0.02), (3, "fire", 0.03), (4, "fire", 0.15)] {
        let (status, body) = h
            .admin
            .send(
                "POST",
                "/api/tables/calls/rows",
                Some(json!({ "id": id, "kind": kind,
                             "location": { "type": "Point", "coordinates": [lon, 51.05] } })),
            )
            .await;
        assert!(status.is_success(), "{body}");
    }
    let mut ids = HashMap::new();
    for (name, table) in [("Calls", "calls"), ("Zones", "zones")] {
        let (status, made) = h
            .admin
            .send(
                "POST",
                "/api/datasets",
                Some(json!({ "name": name, "base": { "kind": "table", "table": table } })),
            )
            .await;
        assert!(status.is_success(), "{made}");
        ids.insert(name, made["dataset"]["id"].clone());
    }
    let calls = json!({ "id": "calls", "dataset": ids["Calls"],
                        "geometry": { "kind": "column", "column": "location" } });
    let zones = json!({ "id": "zones", "dataset": ids["Zones"],
                        "geometry": { "kind": "column", "column": "outline" } });

    // The Map kind can be created now.
    let (_, kinds) = h.admin.send("GET", "/api/workspace-kinds", None).await;
    let map_kind = kinds
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kind"] == "map")
        .expect("a map kind")
        .clone();
    assert_eq!(map_kind["available"], json!(true), "{map_kind}");
    let (status, ws) = h
        .admin
        .send("POST", "/api/workspaces", Some(json!({ "name": "Calls map", "kind": "map" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ws}");
    let ws_id = ws["id"].as_str().unwrap().to_owned();

    // The toolbox: count per zone, every zone, the east 0.
    let (_, tools) = h.admin.send("GET", "/api/maps/tools", None).await;
    let count = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "count_per_region")
        .expect("count per region")
        .clone();
    assert_eq!(count["group"], "Aggregate");
    assert_eq!(count["params"][0]["kind"], "layer");
    let (status, run) = h
        .admin
        .send(
            "POST",
            "/api/maps/tools/run",
            Some(json!({ "tool": "count_per_region",
                         "params": { "layer": calls, "regions": zones } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    assert_eq!(run["dataset"]["name"], "Calls per Zones");
    let kinds: Vec<&str> = run["report"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["spatial_join", "aggregate", "complete"]);
    let per_zone = run["layer"].clone();
    assert_eq!(
        per_zone["geometry"],
        json!({ "kind": "key", "column": "zone", "geometry": "outline" })
    );
    let (_, page) = h
        .admin
        .send("POST", "/api/datasets/stage", Some(json!({ "dataset": run["dataset"] })))
        .await;
    assert_eq!(page["rows"], json!([[1, 3], [2, 1], [3, 0]]), "{page}");
    // A tool that cannot make a dataset says why, and stores nothing.
    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/maps/tools/run",
            Some(json!({ "tool": "buffer", "params": { "layer": calls, "distance": -3 } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("positive"), "{body}");

    // Drawn with graduated colours: the classes come with it.
    let mut per_zone_layer = per_zone.clone();
    per_zone_layer["id"] = json!("per-zone");
    let (status, drawn) = h
        .admin
        .send(
            "POST",
            "/api/maps/render",
            Some(json!({ "spec": { "layers": [zones, per_zone_layer] } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{drawn}");
    assert_eq!(drawn["layers"][1]["data"]["count"], 3, "{drawn}");
    assert_eq!(drawn["layers"][1]["classes"], json!([0.0, 1.0, 3.0, 3.0]));
    assert!(drawn["layers"][0].get("classes").is_none());

    // The workspace keeps its layers and reference layers; one that does not
    // read is refused.
    let state = json!({
        "layers": [zones, calls, per_zone_layer],
        "reference": [{ "id": "osm", "name": "OpenStreetMap", "kind": "tiles",
                        "url": "https://tile.openstreetmap.org/{z}/{x}/{y}.png", "opacity": 0.6 }],
        "selection": { "layer": "calls", "ids": [1] },
    });
    let (status, saved) = h
        .admin
        .send(
            "PUT",
            &format!("/api/workspaces/{ws_id}/state"),
            Some(json!({ "state": state })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let mut twice = state.clone();
    twice["layers"][1]["id"] = json!("zones");
    let (status, body) = h
        .admin
        .send(
            "PUT",
            &format!("/api/workspaces/{ws_id}/state"),
            Some(json!({ "state": twice })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("which another layer has"), "{body}");
    // What uses the calls: the map, by one layer.
    let (_, usage) = h
        .admin
        .send("GET", &format!("/api/datasets/{}/usage", ids["Calls"].as_str().unwrap()), None)
        .await;
    assert_eq!(
        usage["workspaces"],
        json!([{ "id": ws_id, "name": "Calls map", "kind": "map", "panels": 1 }])
    );

    // The attribute table: each row with its feature's id.
    let (status, rows) = h
        .admin
        .send(
            "POST",
            "/api/layers/rows",
            Some(json!({ "layer": calls, "sort": { "formula": "id", "descending": true } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows["ids"], json!([4, 3, 2, 1]));
    assert_eq!(rows["columns"].as_array().unwrap().len(), 2, "geometry left out: {rows}");
    assert_eq!((rows["keyed"].clone(), rows["sorted"].clone()), (json!(true), json!(true)));

    // Selection: within 3 km of a point in the west.
    let (status, found) = h
        .admin
        .send(
            "POST",
            "/api/layers/select",
            Some(json!({ "layer": calls,
                         "by": { "by": "near_point", "longitude": 0.02, "latitude": 51.05, "distance": 1000 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["ids"], json!([1, 2, 3]));
    // By the calls selected in the zones layer's terms: the middle zone.
    let (_, found_zone) = h
        .admin
        .send(
            "POST",
            "/api/layers/select",
            Some(json!({ "layer": zones,
                         "by": { "by": "near_features", "layer": calls, "ids": [4], "distance": 1 } })),
        )
        .await;
    assert_eq!(found_zone["ids"], json!([2]), "{found_zone}");
    let (_, bad) = h
        .admin
        .send(
            "POST",
            "/api/layers/select",
            Some(json!({ "layer": calls, "by": { "by": "condition", "formula": "colour == 2" } })),
        )
        .await;
    assert!(bad["error"].as_str().unwrap().contains("condition does not work"), "{bad}");

    // Save selection as dataset: by the condition, and by clicked ids.
    let (status, near) = h
        .admin
        .send(
            "POST",
            "/api/layers/selection",
            Some(json!({ "layer": calls, "name": "Calls near the west",
                         "condition": found["condition"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{near}");
    assert_eq!(near["dataset"]["base"], json!({ "kind": "dataset", "dataset": ids["Calls"] }));
    let (_, page) = h
        .admin
        .send("POST", "/api/datasets/stage", Some(json!({ "dataset": near["dataset"] })))
        .await;
    assert_eq!(page["total"], 3, "{page}");
    let (status, picked) = h
        .admin
        .send(
            "POST",
            "/api/layers/selection",
            Some(json!({ "layer": per_zone, "name": "Busy zones", "ids": [1] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{picked}");
    let (_, page) = h
        .admin
        .send("POST", "/api/datasets/stage", Some(json!({ "dataset": picked["dataset"] })))
        .await;
    assert_eq!(page["rows"], json!([[1, 3]]), "{page}");

    // The whole map as a panel, drawn for a report.
    let (status, panel) = h
        .admin
        .send(
            "POST",
            "/api/panels/render",
            Some(json!({ "panel": { "id": "6f1c0f9e-3a43-4d55-9f43-1e2f7d0c8a11", "kind": "map",
                                    "title": "Calls map", "content": { "spec": state } } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{panel}");
    assert_eq!(panel["kind"], "map");
    assert_eq!(panel["map"]["layers"].as_array().unwrap().len(), 3, "{panel}");
    assert_eq!(panel["map"]["layers"][2]["classes"], json!([0.0, 1.0, 3.0, 3.0]));

    // A reference layer's host, allowed: in Settings → Maps and the policy.
    assert!(!analytics_policy(&h).await.contains("tile.openstreetmap.org"));
    let (status, settings) = h
        .admin
        .send(
            "POST",
            "/api/maps/hosts",
            Some(json!({ "url": "https://tile.openstreetmap.org/{z}/{x}/{y}.png" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert!(
        settings["hosts"]
            .as_array()
            .unwrap()
            .contains(&json!("https://tile.openstreetmap.org")),
        "{settings}"
    );
    let served = analytics_policy(&h).await;
    assert!(
        served.contains("img-src 'self' data: blob: https://tiles.openfreemap.org https://tile.openstreetmap.org;"),
        "{served}"
    );
    let (status, body) = h
        .admin
        .send("POST", "/api/maps/hosts", Some(json!({ "url": "data:text/html,x" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}
