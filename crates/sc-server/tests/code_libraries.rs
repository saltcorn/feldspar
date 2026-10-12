//! **Code libraries** (TODO F3.1–F3.4): named JavaScript shared by code
//! queries and triggers, imported with `lib(name)` — through the real router,
//! against a real database (SQLite, a file per test).
//!
//! - F3.2: two applications' queries share an instance library, and an edit
//!   reaches both on their next call; an application's own library wins over
//!   the instance's; a library's `fs` under `asUser` cannot read what that user
//!   cannot; a cycle names its chain; a trigger (which has no application)
//!   cannot see an application's library but can see the instance's;
//! - F3.1/F3.3: the admin API lists, creates, updates and deletes them, says
//!   which queries and triggers use each, and refuses a syntax error with its
//!   line; deleting an application deletes its libraries; a backup carries
//!   them to a second installation;
//! - F3.4: the MCP tools are refused without their grants, and a save with a
//!   syntax error is refused with the line.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use sc_action::{EventKind, Trigger, TriggerDispatcher};
use sc_api::mcp::ToolContext;
use sc_api::schema_edit::Grants;
use sc_api::{CustomQuery, Method, QueryLanguage};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, CodeLibrary, FileStoreId};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_server::{
    AppMounts, BackupSelection, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_MEMBER: u8 = 80;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-code-libraries-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A cookie-carrying browser on one host, sending the CSRF header a page would.
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

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
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
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let body = match body {
            Some(json) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&json).unwrap())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get(&mut self, path: &str) -> (StatusCode, Value) {
        self.send("GET", path, None).await
    }

    async fn login(&mut self, email: &str) {
        self.get("/").await;
        let (status, body) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

/// A JavaScript GET query anyone may call.
fn js(name: &str, path: &str, code: &str) -> CustomQuery {
    CustomQuery::new(name, Method::Get, path, code)
        .language(QueryLanguage::Javascript)
        .min_role(sc_auth::ROLE_PUBLIC)
}

/// The two applications' queries: the same instance helper from both, a name
/// one of them overrides, a file read through a library, and a cycle.
fn app(name: &str, subdomain: &str) -> Application {
    let mut api = ApiConfig::new("rest", "/api");
    let queries = vec![
        js(
            "greetMe",
            "/me",
            r#"const { requireUser } = lib("auth");
               return { email: requireUser().email, app: lib("greet").hello() };"#,
        ),
        js(
            "readPrivate",
            "/read",
            r#"return { text: await lib("files").readAsCaller("alice/notes.txt") };"#,
        ),
        js("loop", "/loop", r#"return lib("ping");"#),
    ];
    sc_api::set_custom_queries(&mut api.config, &queries).unwrap();
    // Never built: the framework's settings are only what a save requires.
    let framework = FrameworkRef::new("code")
        .with("store", "files")
        .with("source", subdomain)
        .with("output", format!("{subdomain}/dist"))
        .with("command", "true");
    Application::new(name, subdomain, framework)
        .with_file_store(FileStoreId("files".to_owned()))
        .with_api(api)
}

const AUTH_V1: &str = r#"exports.requireUser = () => {
  if (!user) throw httpError(401, "sign in first");
  return user;
};"#;

struct Server {
    catalog: Arc<Catalog>,
    router: Router,
    apps: Arc<AppMounts>,
    dispatcher: Arc<TriggerDispatcher>,
    one: Application,
    two: Application,
}

async fn setup(tmp: &TempDir, tag: &str) -> sc_error::Result<Server> {
    let db_dir = tmp.path().join(format!("db-{tag}"));
    std::fs::create_dir_all(&db_dir).map_err(sc_error::Error::from)?;
    let driver = Arc::new(SqliteDriver::open(db_dir.join("libs.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::bootstrap_code_libraries(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    let mut ids = HashMap::new();
    for who in ["alice", "bob"] {
        let user = create_user(
            &catalog,
            &format!("{who}@example.com"),
            PASSWORD,
            ROLE_MEMBER,
        )
        .await?;
        ids.insert(who, user.id);
    }
    create_user(&catalog, "admin@example.com", PASSWORD, sc_auth::ROLE_ADMIN).await?;

    // Alice's own file, readable by her alone (the owner rule, §2.1).
    let files_dir = tmp.path().join(format!("files-{tag}"));
    std::fs::create_dir_all(&files_dir).map_err(sc_error::Error::from)?;
    sc_catalog::save_file_store(
        &catalog,
        &sc_files::FileStoreDef::local("files", files_dir.to_string_lossy()),
    )
    .await?;
    let store = Arc::new(LocalFileStore::new("files", &files_dir)?);
    catalog.connect_file_store(store.clone())?;
    store
        .write("alice/notes.txt", Bytes::from_static(b"alice's notes"))
        .await?;
    store
        .set_meta(
            "alice/notes.txt",
            &FileMeta {
                min_role: Some(sc_auth::ROLE_ADMIN),
                owner: Some(ids["alice"].to_string()),
                ..FileMeta::default()
            },
        )
        .await?;

    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;

    let one = sc_app::save_application(&catalog, &app("One", "one")).await?;
    let two = sc_app::save_application(&catalog, &app("Two", "two")).await?;

    // The instance's libraries, and `two`'s own `greet`, which wins there.
    for library in [
        CodeLibrary::new("auth", AUTH_V1),
        CodeLibrary::new("greet", r#"exports.hello = () => "instance";"#),
        CodeLibrary::new(
            "files",
            r#"exports.readAsCaller = (path) => fs("files").asUser(user).open(path).text();"#,
        ),
        CodeLibrary::new("ping", r#"module.exports = lib("pong");"#),
        CodeLibrary::new("pong", r#"module.exports = lib("ping");"#),
        CodeLibrary::new("greet", r#"exports.hello = () => "two's own";"#).in_application(two.id.0),
    ] {
        sc_app::save_code_library_checked(&catalog, &library).await?;
    }

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone()),
    );
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    for application in [&one, &two] {
        apps.mount(MountedApp::new_with(
            application.clone(),
            framework.clone(),
            &catalog,
            apps.evaluator(),
            apps.triggers(),
        )?)?;
    }
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    Ok(Server {
        catalog,
        router,
        apps,
        dispatcher,
        one,
        two,
    })
}

async fn library_named(catalog: &Catalog, name: &str) -> CodeLibrary {
    sc_catalog::load_code_library_by_name(catalog, name, None)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn two_applications_share_a_library_and_an_edit_reaches_both() -> sc_error::Result<()> {
    let tmp = TempDir::new("share");
    let server = setup(&tmp, "a").await?;
    for (host, greeting) in [
        ("one.example.com", "instance"),
        ("two.example.com", "two's own"),
    ] {
        // Anonymous: the library's `httpError(401)` is the query's answer.
        let mut anon = Client::new(&server.router, host);
        let (status, body) = anon.get("/api/me").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{host}: {body}");
        assert_eq!(body["error"], json!("sign in first"), "{host}");
        // Signed in: `user` inside the library is the caller, and `greet`
        // resolves to the application's own where it has one.
        let mut alice = Client::new(&server.router, host);
        alice.login("alice@example.com").await;
        let (status, body) = alice.get("/api/me").await;
        assert_eq!(status, StatusCode::OK, "{host}: {body}");
        assert_eq!(body["email"], json!("alice@example.com"));
        assert_eq!(body["app"], json!(greeting), "{host}");
    }

    // One edit to the instance library, and both applications answer from it.
    let mut auth = library_named(&server.catalog, "auth").await;
    auth.code = AUTH_V1.replace("sign in first", "please sign in");
    sc_app::save_code_library_checked(&server.catalog, &auth).await?;
    for host in ["one.example.com", "two.example.com"] {
        let (status, body) = Client::new(&server.router, host).get("/api/me").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], json!("please sign in"), "{host}");
    }
    Ok(())
}

#[tokio::test]
async fn a_librarys_fs_has_the_callers_authority_and_no_more() -> sc_error::Result<()> {
    let tmp = TempDir::new("authority");
    let server = setup(&tmp, "a").await?;
    let mut alice = Client::new(&server.router, "one.example.com");
    alice.login("alice@example.com").await;
    let (status, body) = alice.get("/api/read").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], json!("alice's notes"));

    // Bob runs the same query, so the library's `asUser(user)` is Bob: the file
    // is not his, and to him it does not exist.
    let mut bob = Client::new(&server.router, "one.example.com");
    bob.login("bob@example.com").await;
    let (status, body) = bob.get("/api/read").await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "{status}: {body}"
    );
    assert!(!body.to_string().contains("alice's notes"), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_cycle_names_its_chain_to_an_admin() -> sc_error::Result<()> {
    let tmp = TempDir::new("cycle");
    let server = setup(&tmp, "a").await?;
    let mut admin = Client::new(&server.router, "one.example.com");
    admin.login("admin@example.com").await;
    let (status, body) = admin.get("/api/loop").await;
    assert!(!status.is_success(), "{body}");
    assert!(
        body.to_string().contains("ping → pong → ping"),
        "the admin reads the chain: {body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_trigger_sees_the_instances_libraries_and_no_applications() -> sc_error::Result<()> {
    let tmp = TempDir::new("trigger");
    let server = setup(&tmp, "a").await?;
    let registry = server.dispatcher.registry();
    let code = |code: &str| -> sc_types::Attrs {
        [("code".to_owned(), json!(code))].into_iter().collect()
    };
    for (name, body) in [
        ("instanceGreet", r#"return lib("greet").hello();"#),
        ("appOnly", r#"lib("greet"); return lib("nothere");"#),
    ] {
        let trigger =
            Trigger::new(name, EventKind::None, "run_js_code").with_configuration(code(body));
        sc_action::save_trigger(&server.catalog, &registry, &trigger).await?;
    }
    server.dispatcher.reload(&server.catalog).await?;
    // `greet` exists twice — the instance's and `two`'s — and a trigger, which
    // belongs to no application, gets the instance's.
    let greeting = server
        .dispatcher
        .run_trigger(&server.catalog, "instanceGreet", Value::Null, None)
        .await?;
    assert_eq!(greeting, json!("instance"));

    // An application-only library is not there to import.
    sc_app::save_code_library_checked(
        &server.catalog,
        &CodeLibrary::new("nothere", "exports.x = 1;").in_application(server.one.id.0),
    )
    .await?;
    let err = server
        .dispatcher
        .run_trigger(&server.catalog, "appOnly", Value::Null, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no code library named `nothere`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn the_admin_api_manages_libraries_and_says_who_uses_them() -> sc_error::Result<()> {
    let tmp = TempDir::new("admin");
    let server = setup(&tmp, "a").await?;
    let mut admin = Client::new(&server.router, BASE_DOMAIN);
    admin.login("admin@example.com").await;

    let (status, list) = admin.get("/api/code-libraries").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let find = |list: &Value, name: &str, app: Option<&str>| -> Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == json!(name) && l["application"] == json!(app))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let auth = find(&list, "auth", None);
    let users: Vec<String> = auth["used_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            format!(
                "{}/{}",
                u["application"].as_str().unwrap_or("-"),
                u["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(users, vec!["one/greetMe", "two/greetMe"], "{auth}");
    // `two`'s `greet` is used by `two`, and the instance's by `one` only.
    assert_eq!(
        find(&list, "greet", Some("two"))["used_by"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        find(&list, "greet", None)["used_by"][0]["application"],
        json!("one")
    );

    // A syntax error is refused, naming its line, and nothing is saved.
    let (status, body) = admin
        .send(
            "POST",
            "/api/code-libraries",
            Some(json!({ "name": "broken", "code": "exports.a = 1;\nexports.b = (;" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("(line 2)"), "{body}");
    assert!(
        sc_catalog::load_code_library_by_name(&server.catalog, "broken", None)
            .await?
            .is_none()
    );
    // A name taken in the same scope is refused; the same name in an
    // application is a different library.
    let (status, body) = admin
        .send(
            "POST",
            "/api/code-libraries",
            Some(json!({ "name": "auth", "code": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string()
            .contains("already has a code library named `auth`"),
        "{body}"
    );

    // Create, update, delete.
    let (status, created) = admin
        .send(
            "POST",
            "/api/code-libraries",
            Some(json!({
                "name": "money", "description": "cents",
                "code": "exports.cents = (x) => Math.round(x * 100);",
                "application_id": server.one.id.0,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["application"], json!("one"));
    let id = created["id"].as_str().unwrap().to_owned();
    let (status, updated) = admin
        .send(
            "PUT",
            &format!("/api/code-libraries/{id}"),
            Some(json!({ "name": "money", "code": "exports.cents = (x) => x * 100;" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(
        updated["application_id"],
        Value::Null,
        "the body is the whole definition"
    );
    let (status, _) = admin
        .send("DELETE", &format!("/api/code-libraries/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = admin
        .send("DELETE", &format!("/api/code-libraries/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Not for anyone else.
    let (status, _) = Client::new(&server.router, BASE_DOMAIN)
        .get("/api/code-libraries")
        .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED,
        "{status}"
    );

    // Deleting an application deletes its libraries.
    assert!(sc_app::delete_application(&server.catalog, server.two.id).await?);
    assert!(
        sc_catalog::load_code_library_by_name(&server.catalog, "greet", Some(server.two.id.0))
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn a_backup_carries_the_libraries_to_a_second_installation() -> sc_error::Result<()> {
    let tmp = TempDir::new("backup");
    let first = setup(&tmp, "first").await?;
    let available = sc_server::backup_available(&first.catalog).await?;
    assert_eq!(available.code_libraries, 6);
    let mut selection = BackupSelection::everything(&available);
    selection.code_libraries = true;
    let archive = sc_server::write_backup(&first.catalog, &selection).await?;
    let (contents, _) = sc_server::inspect_backup(&archive)?;
    assert_eq!(contents.code_libraries, 6);

    // A second installation with its own `auth`: restore adds and keeps.
    let second = setup(&tmp, "second").await?;
    let mut mine = library_named(&second.catalog, "auth").await;
    mine.code = "exports.mine = true;".to_owned();
    // A fresh id, so it is not the same library under the backup's id.
    mine.id = uuid::Uuid::new_v4();
    sc_catalog::delete_code_library(
        &second.catalog,
        library_named(&second.catalog, "auth").await.id,
    )
    .await?;
    sc_app::save_code_library_checked(&second.catalog, &mine).await?;
    for library in sc_catalog::list_code_libraries(&second.catalog).await? {
        if library.name != "auth" {
            sc_catalog::delete_code_library(&second.catalog, library.id).await?;
        }
    }
    // Restore just the libraries: `two` has a different id on this installation,
    // so its own `greet` is refused by name; the rest arrive.
    let only = BackupSelection {
        code_libraries: true,
        ..BackupSelection::default()
    };
    let report = sc_server::restore_backup(&second.catalog, &second.apps, &archive, &only).await?;
    let text = format!("{report:?}");
    assert!(text.contains("its code was kept"), "{text}");
    let restored = sc_catalog::list_code_libraries(&second.catalog).await?;
    let names: Vec<&str> = restored
        .iter()
        .filter(|l| l.application_id.is_none())
        .map(|l| l.name.as_str())
        .collect();
    assert_eq!(names, vec!["auth", "files", "greet", "ping", "pong"]);
    assert_eq!(
        library_named(&second.catalog, "auth").await.code,
        "exports.mine = true;"
    );
    // What arrived is imported straight away: `readPrivate` uses the restored
    // `files`.
    let mut alice = Client::new(&second.router, "one.example.com");
    alice.login("alice@example.com").await;
    let (status, body) = alice.get("/api/read").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], json!("alice's notes"));
    Ok(())
}

#[tokio::test]
async fn the_mcp_tools_need_their_grants_and_refuse_a_syntax_error() -> sc_error::Result<()> {
    let tmp = TempDir::new("mcp");
    let server = setup(&tmp, "a").await?;
    let tools = sc_app::mcp::app_tools();
    let tool = |name: &str| tools.iter().find(|t| t.name() == name).unwrap().clone();
    let ctx = ToolContext {
        catalog: &server.catalog,
        user: None,
        role: sc_auth::ROLE_ADMIN,
        triggers: Some(&server.dispatcher),
        actor: "test",
    };
    let none = Grants {
        create: false,
        edit: false,
        drop: false,
        access_changes: false,
    };
    let all = Grants {
        create: true,
        edit: true,
        drop: true,
        access_changes: true,
    };
    let save = tool("save_code_library");
    let delete = tool("delete_code_library");
    let describe = tool("describe_code_libraries");

    let err = save
        .call(
            &ctx,
            &none,
            &json!({ "name": "x", "code": "exports.x = 1;" }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("allow_edit"), "{err}");
    let err = delete
        .call(
            &ctx,
            &Grants { drop: false, ..all },
            &json!({ "name": "auth" }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("allow_drop"), "{err}");
    assert!(library_named(&server.catalog, "auth").await.code == AUTH_V1);

    let err = save
        .call(
            &ctx,
            &all,
            &json!({ "name": "x", "code": "exports.a = 1;\n\nexports.b = = 2;" }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("(line 3)") && err.contains("Nothing was saved"),
        "{err}"
    );

    // With the grants: an application's own library, described in its scope,
    // then deleted.
    let saved = save
        .call(
            &ctx,
            &all,
            &json!({ "name": "util", "code": "exports.x = 1;", "application": "one" }),
        )
        .await?;
    assert_eq!(saved["created"], json!(true));
    let listed = describe
        .call(&ctx, &none, &json!({ "application": "two" }))
        .await?;
    let names: Vec<&str> = listed["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"util"),
        "two cannot see one's library: {names:?}"
    );
    delete
        .call(&ctx, &all, &json!({ "name": "util", "application": "one" }))
        .await?;
    assert!(
        sc_catalog::load_code_library_by_name(&server.catalog, "util", Some(server.one.id.0))
            .await?
            .is_none()
    );
    Ok(())
}
