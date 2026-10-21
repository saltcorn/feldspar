//! Analytics applications (analytics TODO A9.1–A9.4), over HTTP against a real
//! Postgres: the Analytics UI published to a role on its own subdomain.
//!
//! What is asserted:
//!
//! - **Authority** (A9.1): a user reads through their table permissions and
//!   ownership formulas — a summary's count, a stage's rows and a `Ⱶ` column
//!   are the rows of theirs only — and a table they may not read is refused,
//!   naming it. Datasets and workspaces have an owner, and are shared with a
//!   role.
//! - **The framework** (A9.2): offered in the picker with its settings, and
//!   its settings are checked on save — a fixed application naming a
//!   workspace that does not exist is refused.
//! - **Mounting** (A9.3): the bundle on the application's host, under the
//!   Analytics UI's policy; the application's own sign-in; a role below the
//!   floor answered nothing; nothing of the admin's (models) reachable.
//! - **Enforcement** (A9.4): the base picker offers the application's tables
//!   only, a dataset on another is refused however it is asked for, a fixed
//!   application reads only what its workspaces read and changes nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const PASSWORD: &str = "hunter2pass";
const SAM: &str = "sam@example.com";
const KIM: &str = "kim@example.com";
const PAT: &str = "pat@example.com";

/// Twelve houses in two neighbourhoods; Sam is the agent of the first four
/// (two in each neighbourhood), Kim of the rest. And incidents, which no
/// application here lets anybody build on.
const SCHEMA: &str = "
    CREATE TABLE neighbourhoods (id bigint primary key, name text);
    CREATE TABLE houses (
        id bigint primary key,
        price double precision,
        neighbourhood bigint references neighbourhoods(id),
        agent text
    );
    CREATE TABLE incidents (id bigint primary key, kind text);
    INSERT INTO neighbourhoods VALUES (1, 'North'), (2, 'South');
    INSERT INTO houses (id, price, neighbourhood, agent)
      SELECT i, 1000 * i, 1 + (i % 2),
             CASE WHEN i <= 4 THEN 'sam@example.com' ELSE 'kim@example.com' END
      FROM generate_series(1, 12) AS i;
    INSERT INTO incidents VALUES (1, 'burglary');
";

/// A cookie-carrying client addressing one host, echoing CSRF on mutations.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: &Router, host: &str) -> Client {
        Client {
            router: router.clone(),
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
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
        if method != "GET"
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
        let headers = response.headers().clone();
        for raw in headers.get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
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

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(
            status.is_success(),
            "{} {method} {path}: {status} {value}",
            self.host
        );
        value
    }

    /// A refusal: its status and its sentence.
    async fn refused(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let (status, value) = self.send(method, path, body).await;
        assert!(
            status.is_client_error(),
            "{} {method} {path}: {status} {value}",
            self.host
        );
        (
            status,
            value["error"].as_str().unwrap_or_default().to_owned(),
        )
    }

    /// Sign in as `email` on this client's host.
    async fn sign_in(&mut self, email: &str) {
        self.send("GET", "/api/auth/status", None).await;
        self.ok(
            "POST",
            "/api/login",
            Some(json!({ "email": email, "password": PASSWORD })),
        )
        .await;
    }
}

/// The server: the schema above, roles Staff (40) and Member (80), Sam and
/// Kim on Staff and Pat a Member, an admin signed in on the base domain, and
/// a stand-in Analytics UI bundle. `houses` is read by its agent alone
/// (`agent === user.email`), the others by Staff.
async fn setup() -> sc_error::Result<(Client, Router, TestDb, PathBuf)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;

    let bundle = std::env::temp_dir().join(format!("sc-analytics-apps-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(bundle.join("assets")).unwrap();
    std::fs::write(
        bundle.join("index.html"),
        "<!doctype html><div id=\"root\"></div>",
    )
    .unwrap();
    std::fs::write(bundle.join("assets/index-1a2b3c4d.js"), "console.log(1)").unwrap();

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_triggers(dispatcher)
            .with_models(models)
            .with_analytics_dir(Some(bundle.clone())),
    );
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;

    let mut admin = Client::new(&router, BASE_DOMAIN);
    admin.send("GET", "/api/auth/status", None).await;
    admin
        .ok(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": PASSWORD })),
        )
        .await;
    for (role, name) in [(40, "Staff"), (80, "Member")] {
        admin
            .ok(
                "POST",
                "/api/roles",
                Some(json!({ "role": role, "name": name, "description": "" })),
            )
            .await;
    }
    for (email, role) in [(SAM, 40), (KIM, 40), (PAT, 80)] {
        admin
            .ok(
                "POST",
                "/api/users",
                Some(json!({ "email": email, "password": PASSWORD, "role": role })),
            )
            .await;
    }
    let settings = |read: u8, formula: &str| {
        json!({
            "label": "", "description": "",
            "min_role_read": read, "min_role_write": 1,
            "ownership_formula": formula, "rls_enabled": false, "sync_enabled": false,
        })
    };
    admin
        .ok(
            "PUT",
            "/api/tables/houses",
            Some(settings(1, "agent === user.email")),
        )
        .await;
    admin
        .ok("PUT", "/api/tables/neighbourhoods", Some(settings(40, "")))
        .await;
    admin
        .ok("PUT", "/api/tables/incidents", Some(settings(40, "")))
        .await;
    Ok((admin, router, db, bundle))
}

fn op(id: &str, kind: &str, params: Value) -> Value {
    json!({ "id": id, "enabled": true, "kind": kind, "params": params })
}

fn on_table(name: &str, table: &str, operations: Value) -> Value {
    json!({ "name": name, "base": { "kind": "table", "table": table }, "operations": operations })
}

fn panel(kind: &str, content: Value) -> Value {
    json!({ "id": uuid::Uuid::new_v4(), "kind": kind, "content": content })
}

/// How many rows a summary table over `dataset` counts.
fn count_spec(dataset: &str) -> Value {
    json!({ "spec": {
        "data": { "kind": "dataset", "dataset": dataset },
        "cells": [{ "function": "count" }], "totals": false,
    } })
}

/// An application of the framework `analytics` with these settings and tables.
fn application(subdomain: &str, config: Value, tables: &[&str]) -> Value {
    json!({
        "name": format!("{subdomain} insights"),
        "description": "",
        "subdomain": subdomain,
        "framework": { "name": "analytics", "config": config },
        "extra_frameworks": [],
        "tables": tables,
        "file_stores": [],
        "triggers": [],
        "apis": [],
        "static_dirs": [],
        "attributes": {}
    })
}

#[tokio::test]
async fn a_self_serve_application_reads_as_its_user_and_only_its_tables() -> sc_error::Result<()> {
    let (mut admin, router, _db, bundle) = setup().await?;

    // --- the framework is offered, with its settings ---------------------------
    let frameworks = admin.ok("GET", "/api/frameworks", None).await;
    let analytics = frameworks
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("analytics"))
        .unwrap_or_else(|| panic!("`analytics` is not offered: {frameworks}"))
        .clone();
    let settings: Vec<&str> = analytics["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        settings,
        [
            "mode",
            "min_role",
            "workspaces",
            "dataset_editor",
            "workspace_kinds",
            "create_workspaces"
        ]
    );
    let (_, why) = admin
        .refused(
            "POST",
            "/api/applications",
            Some(application(
                "explore",
                json!({ "mode": "self_serve", "min_role": 40, "workspace_kinds": ["simulation"] }),
                &["houses"],
            )),
        )
        .await;
    assert!(why.contains("milestone A7"), "{why}");

    let created = admin
        .ok(
            "POST",
            "/api/applications",
            Some(application(
                "explore",
                json!({
                    "mode": "self_serve", "min_role": 40, "dataset_editor": true,
                    "workspace_kinds": ["data_explorer"],
                }),
                &["houses", "neighbourhoods"],
            )),
        )
        .await;
    assert_eq!(created["mounted"], json!(true), "{created}");
    let app_id = created["id"].as_str().unwrap().to_owned();

    // --- the bundle, on the application's host ----------------------------------
    let mut sam = Client::new(&router, "explore.example.com");
    let (status, headers, body) = sam.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("id=\"root\""));
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("worker-src 'self'"), "{csp}");
    let (status, _, _) = sam
        .raw("GET", "/analytics/assets/index-1a2b3c4d.js", None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // Nobody signed in: who they are is answered, and nothing else.
    let status = sam.ok("GET", "/api/auth/status", None).await;
    assert_eq!(status["current_user"], Value::Null, "{status}");
    let (code, _) = sam.refused("GET", "/api/datasets", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);

    // A Member is below the application's floor.
    let mut pat = Client::new(&router, "explore.example.com");
    pat.sign_in(PAT).await;
    let (code, _) = pat.refused("GET", "/api/datasets", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    sam.sign_in(SAM).await;
    let shell = sam.ok("GET", "/api/analytics/shell", None).await;
    assert_eq!(shell["application"]["mode"], json!("self_serve"), "{shell}");
    assert_eq!(shell["application"]["dataset_editor"], json!(true));
    assert_eq!(
        shell["application"]["workspace_kinds"],
        json!(["data_explorer"])
    );
    assert!(
        shell["roles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == &json!({ "role": 40, "name": "Staff" })),
        "{shell}"
    );
    // The admin's own endpoints are not here at all.
    let (code, _) = sam.refused("GET", "/api/models", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // --- the base picker offers the application's tables ------------------------
    let tables = sam.ok("GET", "/api/datasets/tables", None).await;
    let mut names: Vec<&str> = tables
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["houses", "neighbourhoods"]);
    let (code, why) = sam
        .refused(
            "POST",
            "/api/datasets",
            Some(on_table("Incidents", "incidents", json!([]))),
        )
        .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{why}");
    assert!(
        why.contains("`incidents` is not one of the tables"),
        "{why}"
    );
    // Nor by an unsaved definition sent to be read.
    let (_, why) = sam
        .refused(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": on_table("x", "incidents", json!([])) })),
        )
        .await;
    assert!(
        why.contains("`incidents` is not one of the tables"),
        "{why}"
    );

    // --- a dataset of Sam's, read as Sam ----------------------------------------
    let mine = sam
        .ok(
            "POST",
            "/api/datasets",
            Some(on_table(
                "My houses",
                "houses",
                json!([op(
                    "h",
                    "calculated",
                    json!({ "name": "hood", "formula": "neighbourhoodⱵname" })
                )]),
            )),
        )
        .await;
    let mine_id = mine["dataset"]["id"].as_str().unwrap().to_owned();
    // The editor's completions name the application's tables only.
    let mut completions: Vec<&str> = mine["report"]["tables"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    completions.sort_unstable();
    assert_eq!(completions, ["houses", "neighbourhoods"]);
    // Four of the twelve houses are Sam's: a summary counts four, a stage reads
    // four, and the joined neighbourhood names are there.
    let table = sam
        .ok("POST", "/api/plots/table", Some(count_spec(&mine_id)))
        .await;
    assert_eq!(table["total"], json!(4), "{table}");
    let stage = sam
        .ok(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": mine["dataset"] })),
        )
        .await;
    assert_eq!(stage["total"], json!(4), "{stage}");
    let hood = stage["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["name"] == json!("hood"))
        .unwrap();
    let mut hoods: Vec<&str> = stage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[hood].as_str().unwrap())
        .collect();
    hoods.sort_unstable();
    assert_eq!(hoods, ["North", "North", "South", "South"]);
    // A histogram's bars count the same four.
    let bars = sam
        .ok(
            "POST",
            "/api/plots/render",
            Some(json!({ "spec": {
                "data": { "kind": "dataset", "dataset": mine_id },
                "layers": [{ "mark": "bar", "stat": { "kind": "count" },
                             "encoding": { "x": { "field": "neighbourhood" } } }],
            } })),
        )
        .await;
    let counted: i64 = bars["layers"][0]["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("{bars}"))
        .iter()
        .map(|r| r.as_array().unwrap().last().unwrap().as_i64().unwrap())
        .sum();
    assert_eq!(counted, 4, "{bars}");
    // The admin reads all twelve of the same dataset.
    let table = admin
        .ok("POST", "/api/plots/table", Some(count_spec(&mine_id)))
        .await;
    assert_eq!(table["total"], json!(12), "{table}");

    // A join to a table outside the application is refused where it is read.
    let joined = on_table(
        "With incidents",
        "houses",
        json!([op(
            "j",
            "join",
            json!({
                "with": { "kind": "table", "table": "incidents" },
                "kind": "left", "on": [{ "left": "id", "right": "id" }],
            })
        )]),
    );
    let (_, why) = sam
        .refused(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": joined })),
        )
        .await;
    assert!(
        why.contains("`incidents` is not one of the tables"),
        "{why}"
    );

    // A table Staff may not read at all: the join reaches it, and says so.
    admin
        .ok(
            "PUT",
            "/api/tables/neighbourhoods",
            Some(json!({
                "label": "", "description": "", "min_role_read": 1, "min_role_write": 1,
                "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
            })),
        )
        .await;
    let (_, why) = sam
        .refused("POST", "/api/plots/table", Some(count_spec(&mine_id)))
        .await;
    assert!(why.contains("you may not read `neighbourhoods`"), "{why}");
    admin
        .ok(
            "PUT",
            "/api/tables/neighbourhoods",
            Some(json!({
                "label": "", "description": "", "min_role_read": 40, "min_role_write": 1,
                "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
            })),
        )
        .await;

    // --- owners and sharing ------------------------------------------------------
    let mut kim = Client::new(&router, "explore.example.com");
    kim.sign_in(KIM).await;
    let listed = |list: &Value| -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(listed(&kim.ok("GET", "/api/datasets", None).await).is_empty());
    let (code, _) = kim
        .refused("GET", &format!("/api/datasets/{mine_id}"), None)
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // Sam shares it with Staff: Kim sees it, and reads Kim's own rows of it.
    sam.ok(
        "PUT",
        &format!("/api/datasets/{mine_id}/share"),
        Some(json!({ "share_role": 40 })),
    )
    .await;
    let list = kim.ok("GET", "/api/datasets", None).await;
    assert_eq!(listed(&list), ["My houses"]);
    assert_eq!(list[0]["may_change"], json!(false), "{list}");
    assert_eq!(list[0]["share_role"], json!(40), "{list}");
    let table = kim
        .ok("POST", "/api/plots/table", Some(count_spec(&mine_id)))
        .await;
    assert_eq!(table["total"], json!(8), "{table}");
    // It is still Sam's to change.
    let (code, why) = kim
        .refused(
            "PUT",
            &format!("/api/datasets/{mine_id}"),
            Some(on_table("Renamed", "houses", json!([]))),
        )
        .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert!(why.contains("not yours to change"), "{why}");
    // An admin's dataset on `houses` is seen only once it is shared.
    let admins = admin
        .ok(
            "POST",
            "/api/datasets",
            Some(on_table("Prices", "houses", json!([]))),
        )
        .await;
    let admins_id = admins["dataset"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        listed(&sam.ok("GET", "/api/datasets", None).await),
        ["My houses"]
    );
    admin
        .ok(
            "PUT",
            &format!("/api/datasets/{admins_id}/share"),
            Some(json!({ "share_role": 40 })),
        )
        .await;
    assert_eq!(
        listed(&sam.ok("GET", "/api/datasets", None).await),
        ["My houses", "Prices"]
    );

    // --- workspaces belong to the application ------------------------------------
    let ws = sam
        .ok(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Prices by area", "kind": "data_explorer" })),
        )
        .await;
    assert_eq!(ws["application"], json!(app_id), "{ws}");
    assert_eq!(ws["may_change"], json!(true), "{ws}");
    let ws_id = ws["id"].as_str().unwrap().to_owned();
    let (code, why) = sam
        .refused(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Board", "kind": "dashboard" })),
        )
        .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert!(
        why.contains("cannot be opened in this application"),
        "{why}"
    );
    let kinds = sam.ok("GET", "/api/workspace-kinds", None).await;
    assert_eq!(kinds.as_array().unwrap().len(), 1, "{kinds}");
    // Kim does not see it until it is shared, and may not change it after.
    assert!(
        kim.ok("GET", "/api/workspaces", None)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    sam.ok(
        "PUT",
        &format!("/api/workspaces/{ws_id}/share"),
        Some(json!({ "share_role": 40 })),
    )
    .await;
    let theirs = kim.ok("GET", "/api/workspaces", None).await;
    assert_eq!(theirs[0]["may_change"], json!(false), "{theirs}");
    let (code, _) = kim
        .refused(
            "PUT",
            &format!("/api/workspaces/{ws_id}/state"),
            Some(json!({ "state": {} })),
        )
        .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    // The unrestricted UI lists its own workspaces, not the application's.
    let unrestricted = admin.ok("GET", "/api/workspaces", None).await;
    assert!(
        unrestricted
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["id"] != json!(ws_id)),
        "{unrestricted}"
    );

    std::fs::remove_dir_all(bundle).ok();
    Ok(())
}

#[tokio::test]
async fn a_fixed_application_shows_its_dashboard_and_nothing_else() -> sc_error::Result<()> {
    let (mut admin, router, _db, bundle) = setup().await?;

    // The admin's dashboard, on a dataset of all the houses.
    let houses = admin
        .ok(
            "POST",
            "/api/datasets",
            Some(on_table("All houses", "houses", json!([]))),
        )
        .await;
    let houses_id = houses["dataset"]["id"].as_str().unwrap().to_owned();
    let other = admin
        .ok(
            "POST",
            "/api/datasets",
            Some(on_table("Areas", "neighbourhoods", json!([]))),
        )
        .await;
    let other_id = other["dataset"]["id"].as_str().unwrap().to_owned();
    let board = admin
        .ok(
            "POST",
            "/api/workspaces",
            Some(json!({ "name": "Board", "kind": "dashboard" })),
        )
        .await;
    let board_id = board["id"].as_str().unwrap().to_owned();
    let counts = panel(
        "summary_table",
        json!({ "spec": { "data": { "kind": "dataset", "dataset": houses_id },
                          "cells": [{ "function": "count" }], "totals": false } }),
    );
    admin
        .ok(
            "PUT",
            &format!("/api/workspaces/{board_id}/state"),
            Some(json!({ "state": { "tiles": [
                { "id": "a", "panel": counts, "x": 0, "y": 0, "w": 6, "h": 4 },
            ] } })),
        )
        .await;

    // A workspace that does not exist is refused on save, by name.
    let (_, why) = admin
        .refused(
            "POST",
            "/api/applications",
            Some(application(
                "board",
                json!({ "mode": "fixed", "min_role": 40, "workspaces": ["Bored"] }),
                &[],
            )),
        )
        .await;
    assert!(why.contains("no workspace `Bored`"), "{why}");
    admin
        .ok(
            "POST",
            "/api/applications",
            Some(application(
                "board",
                json!({ "mode": "fixed", "min_role": 40, "workspaces": ["Board"] }),
                &[],
            )),
        )
        .await;

    let mut sam = Client::new(&router, "board.example.com");
    sam.sign_in(SAM).await;
    let shell = sam.ok("GET", "/api/analytics/shell", None).await;
    assert_eq!(shell["application"]["mode"], json!("fixed"), "{shell}");
    assert_eq!(
        shell["application"]["workspaces"],
        json!([{ "id": board_id, "name": "Board", "kind": "dashboard" }])
    );
    assert_eq!(shell["application"]["dataset_editor"], json!(false));

    // The dashboard, and nothing to change on it.
    let listed = sam.ok("GET", "/api/workspaces", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["may_change"], json!(false));
    let opened = sam
        .ok("GET", &format!("/api/workspaces/{board_id}"), None)
        .await;
    let tile = opened["state"]["tiles"][0]["panel"].clone();
    for (method, path, body) in [
        (
            "PUT",
            format!("/api/workspaces/{board_id}/state"),
            json!({ "state": {} }),
        ),
        (
            "PUT",
            format!("/api/workspaces/{board_id}"),
            json!({ "name": "Mine" }),
        ),
        (
            "POST",
            "/api/workspaces".to_owned(),
            json!({ "name": "New", "kind": "dashboard" }),
        ),
        (
            "POST",
            "/api/datasets".to_owned(),
            on_table("Mine", "houses", json!([])),
        ),
    ] {
        let (code, _) = sam.refused(method, &path, Some(body)).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{method} {path}");
    }

    // Its tile draws Sam's four houses of the twelve.
    let drawn = sam
        .ok("POST", "/api/panels/render", Some(json!({ "panel": tile })))
        .await;
    assert_eq!(drawn["table"]["total"], json!(4), "{drawn}");
    // The datasets it reads are the dashboard's, and no other.
    let datasets = sam.ok("GET", "/api/datasets", None).await;
    assert_eq!(datasets.as_array().unwrap().len(), 1, "{datasets}");
    assert_eq!(datasets[0]["id"], json!(houses_id));
    let (code, _) = sam
        .refused("POST", "/api/plots/table", Some(count_spec(&other_id)))
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(
        sam.ok("GET", "/api/datasets/tables", None)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Whatever operations are sent with it, the dashboard's dataset is read as
    // it is stored.
    let mut sent = houses["dataset"].clone();
    sent["operations"] = json!([
        op("f", "filter", json!({ "formula": "true" })),
        op(
            "j",
            "join",
            json!({
                                     "with": { "kind": "table", "table": "incidents" },
                                     "kind": "left", "on": [{ "left": "id", "right": "id" }] })
        )
    ]);
    let stage = sam
        .ok(
            "POST",
            "/api/datasets/stage",
            Some(json!({ "dataset": sent })),
        )
        .await;
    assert_eq!(stage["total"], json!(4), "{stage}");
    assert!(
        stage["columns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != json!("kind")),
        "{stage}"
    );

    std::fs::remove_dir_all(bundle).ok();
    Ok(())
}

#[tokio::test]
async fn row_level_security_decides_inside_a_transaction_that_carries_the_caller()
-> sc_error::Result<()> {
    let (mut admin, router, _db, bundle) = setup().await?;
    // The same rule, kept by the database's own policies.
    admin
        .ok(
            "PUT",
            "/api/tables/houses",
            Some(json!({
                "label": "", "description": "", "min_role_read": 1, "min_role_write": 1,
                "ownership_formula": "agent === user.email", "rls_enabled": true, "sync_enabled": false,
            })),
        )
        .await;
    admin
        .ok(
            "POST",
            "/api/applications",
            Some(application(
                "explore",
                json!({ "mode": "self_serve", "min_role": 40, "dataset_editor": true }),
                &["houses"],
            )),
        )
        .await;
    let mut sam = Client::new(&router, "explore.example.com");
    sam.sign_in(SAM).await;
    let mine = sam
        .ok(
            "POST",
            "/api/datasets",
            Some(on_table("Mine", "houses", json!([]))),
        )
        .await;
    let id = mine["dataset"]["id"].as_str().unwrap().to_owned();
    let table = sam
        .ok("POST", "/api/plots/table", Some(count_spec(&id)))
        .await;
    assert_eq!(table["total"], json!(4), "{table}");
    // The admin is read as the admin by the policies too: every row, where a
    // read outside a caller's transaction would see none on a FORCE'd table.
    let table = admin
        .ok("POST", "/api/plots/table", Some(count_spec(&id)))
        .await;
    assert_eq!(table["total"], json!(12), "{table}");
    std::fs::remove_dir_all(bundle).ok();
    Ok(())
}
