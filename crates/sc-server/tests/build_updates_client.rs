//! Build brings an application's generated code up to date **before** it runs
//! the bundler — all of it, the way "Update client" did, including the case
//! where the project directory is empty and has to be scaffolded.
//!
//! That is what lets the admin sidebar offer one button instead of two: an
//! admin who emptied a project (or whose store was unreachable when the app was
//! created) presses Build and gets a project and a build, rather than a bundler
//! failing in a directory with nothing in it.
//!
//! The framework is a module-declared one, stubbed as in `sc-app`'s
//! `declared_framework.rs`, because a `react` build would need a real
//! `npm install`: its scaffold writes a `build.sh` that the build then runs, so
//! the build succeeding is itself the proof the scaffold came first. The
//! registry is process-wide, which is why this is its own test binary.
//!
//! **Deep clean** is here too, for the same reason: it ends in the same build,
//! and a second stub framework — one with an install step — is what it needs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{
    ApiConfig, Application, BuildTemplate, DeclaredFile, FilePhase, FrameworkDecl, FrameworkHost,
    FrameworkRef, FrameworkSet, InstallSpec, OperationAnswer, TargetOperation, TargetTemplate,
    bootstrap, install_frameworks, load_application, save_application,
};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_expr::Template;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use sc_types::{BasicType, FormField, SECRET_SENTINEL};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";

fn template(source: &str) -> Template {
    Template::parse(source).expect("the test's own template parses")
}

/// A framework whose project is a shell script: scaffolded by the module, built
/// by `sh build.sh`, with a generated runtime directory like React's.
fn shell_framework() -> FrameworkDecl {
    FrameworkDecl {
        name: "shell".to_owned(),
        module: "@feldspar/shell".to_owned(),
        label: "Shell".to_owned(),
        description: "A project built by a shell script.".to_owned(),
        config_spec: vec![
            FormField::new("store", BasicType::Text).required(),
            FormField::new("project", BasicType::Text).default_value(""),
            // A secret, as a keystore password is: shown masked, kept on save.
            FormField::new("token", BasicType::Text).secret(),
            // The `bundle` target's own settings.
            FormField::new("key_file", BasicType::Text),
            FormField::new("key_password", BasicType::Text).secret(),
        ],
        build: BuildTemplate {
            store: template("{{ store }}"),
            source: template("{{ project }}"),
            output: template("{{ project }}/dist"),
            command: "sh build.sh".to_owned(),
            install: None,
            runtime: Some(template("{{ project }}/src/feldspar")),
            client_file: "client.ts".to_owned(),
            server: None,
        },
        csp: None,
        builder_prompt: None,
        checks: Vec::new(),
        scaffolds: true,
        // A target whose operation makes a key, as the Android one makes a
        // keystore — and one that misbehaves.
        targets: vec![TargetTemplate {
            name: "bundle".to_owned(),
            label: "Bundle".to_owned(),
            command: template("sh bundle.sh"),
            artifact: template("{{ project }}/bundle.zip"),
            env: Default::default(),
            requires: Vec::new(),
            options: vec!["key_file".to_owned(), "key_password".to_owned()],
            operations: ["make_key", "stray"]
                .into_iter()
                .map(|name| TargetOperation {
                    name: name.to_owned(),
                    label: name.to_owned(),
                    description: String::new(),
                    show_if: Vec::new(),
                })
                .collect(),
        }],
    }
}

/// [`shell_framework`], installing its dependencies as `react` does: an
/// `install.sh` that creates `node_modules` and counts how often it ran.
fn installing_framework() -> FrameworkDecl {
    let mut decl = shell_framework();
    decl.name = "shell-installs".to_owned();
    decl.build.install = Some(InstallSpec {
        command: "sh".to_owned(),
        args: vec!["install.sh".to_owned()],
        marker: "node_modules".to_owned(),
    });
    decl
}

struct StubHost;

#[async_trait]
impl FrameworkHost for StubHost {
    fn frameworks(&self) -> Vec<FrameworkDecl> {
        vec![shell_framework(), installing_framework()]
    }

    async fn framework_files(
        &self,
        _name: &str,
        phase: FilePhase,
        context: Value,
    ) -> sc_error::Result<Vec<DeclaredFile>> {
        let runtime = context["runtime"].as_str().unwrap_or_default().to_owned();
        let mut files = vec![DeclaredFile {
            path: format!("{runtime}/composables.ts"),
            contents: "export {};\n".to_owned(),
        }];
        if phase == FilePhase::Scaffold {
            files.push(DeclaredFile {
                path: "build.sh".to_owned(),
                contents: "mkdir -p dist && echo '<!doctype html>built' > dist/index.html\n"
                    .to_owned(),
            });
            files.push(DeclaredFile {
                path: "install.sh".to_owned(),
                contents: "mkdir -p node_modules && echo install >> installs.log\n".to_owned(),
            });
        }
        Ok(files)
    }

    async fn call_target_operation(
        &self,
        _name: &str,
        _target: &str,
        operation: &str,
        context: Value,
    ) -> sc_error::Result<OperationAnswer> {
        let mut answer = OperationAnswer {
            files: vec![(
                format!(
                    "keys/{}.key",
                    context["app"]["subdomain"].as_str().unwrap_or("")
                ),
                b"KEY".to_vec(),
            )],
            message: "made".to_owned(),
            ..OperationAnswer::default()
        };
        answer.settings.insert(
            "key_file".to_owned(),
            json!(format!(
                "keys/{}.key",
                context["app"]["subdomain"].as_str().unwrap_or("")
            )),
        );
        // The password the form holds (typed, or the stored one behind the mask).
        answer.settings.insert(
            "key_password".to_owned(),
            context["settings"]["key_password"].clone(),
        );
        if operation == "stray" {
            // Not one of the target's settings.
            answer
                .settings
                .insert("project".to_owned(), json!("elsewhere"));
            answer.files[0].0 = "keys/stray.key".to_owned();
        }
        Ok(answer)
    }
}

struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, BASE_DOMAIN);
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
        for value in response.headers().get_all(header::SET_COOKIE) {
            let raw = value.to_str().unwrap();
            let pair = raw.split(';').next().unwrap();
            if let Some((name, val)) = pair.split_once('=') {
                self.cookies.insert(name.to_owned(), val.to_owned());
            }
        }
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A server with the stub frameworks installed, a `tasks` table, an `apps`
/// store in a scratch directory, and a signed-in admin.
async fn serve(tag: &str) -> sc_error::Result<(Client, Arc<Catalog>, TempDir, TestDb)> {
    install_frameworks(FrameworkSet::new(Arc::new(StubHost)))?;

    let db = TestDb::new().await?;
    let dir = TempDir(std::env::temp_dir().join(format!(
        "sc-server-build-updates-client-{}-{tag}",
        std::process::id()
    )));
    std::fs::remove_dir_all(&dir.0).ok();
    std::fs::create_dir_all(&dir.0)?;
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE tasks (\
               id bigint generated by default as identity primary key, \
               title text not null)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
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
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    Ok((client, catalog, dir, db))
}

#[tokio::test]
async fn build_scaffolds_an_empty_project_and_regenerates_its_code_before_building()
-> sc_error::Result<()> {
    let (mut client, catalog, dir, _db) = serve("build").await?;

    // Saved, but never scaffolded: the project directory does not exist.
    let app = Application::new(
        "Site",
        "site",
        FrameworkRef::new("shell")
            .with("store", "apps")
            .with("project", "site"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    save_application(&catalog, &app).await?;
    let project = dir.0.join("site");
    assert!(!project.join("build.sh").exists());

    let (status, body) = client
        .send("POST", &format!("/api/applications/{}/build", app.id), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["built"], json!(true), "{body}");
    // The scaffold is news, so the log leads with it.
    assert!(
        body["log"]
            .as_str()
            .unwrap_or_default()
            .contains("scaffolded"),
        "the log should say the project was scaffolded: {body}"
    );
    // The scaffold's own build script ran, so it was written first…
    assert!(project.join("dist/index.html").is_file());
    // …along with the generated client and runtime.
    assert!(project.join("src/feldspar/client.ts").is_file());
    assert!(project.join("src/feldspar/composables.ts").is_file());

    // A second build finds a project there: the generated code is rewritten,
    // nothing is scaffolded over the admin's work, and the log does not say
    // otherwise.
    std::fs::write(project.join("mine.txt"), "mine")?;
    std::fs::remove_file(project.join("src/feldspar/client.ts"))?;
    let (status, body) = client
        .send("POST", &format!("/api/applications/{}/build", app.id), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body["log"]
            .as_str()
            .unwrap_or_default()
            .contains("scaffolded"),
        "{body}"
    );
    assert!(project.join("src/feldspar/client.ts").is_file());
    assert_eq!(std::fs::read_to_string(project.join("mine.txt"))?, "mine");
    Ok(())
}

/// Deep clean deletes the installed dependencies and builds, which installs
/// them from scratch — the remedy for a `node_modules` no build can fix.
#[tokio::test]
async fn deep_clean_reinstalls_the_dependencies_and_builds() -> sc_error::Result<()> {
    let (mut client, catalog, dir, _db) = serve("deep-clean").await?;
    let app = Application::new(
        "Shop",
        "shop",
        FrameworkRef::new("shell-installs")
            .with("store", "apps")
            .with("project", "shop"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    save_application(&catalog, &app).await?;
    let project = dir.0.join("shop");

    // The list says which applications a deep clean applies to.
    let (_, list) = client.send("GET", "/api/applications", None).await;
    let listed = list
        .as_array()
        .and_then(|apps| apps.iter().find(|a| a["subdomain"] == json!("shop")))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(listed["installs"], json!(true), "{list}");

    let (status, body) = client
        .send("POST", &format!("/api/applications/{}/build", app.id), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    std::fs::write(project.join("node_modules/stale.js"), "broken")?;

    let (status, body) = client
        .send(
            "POST",
            &format!("/api/applications/{}/deep-clean", app.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["built"], json!(true), "{body}");
    assert!(
        body["log"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Deleted node_modules; reinstalling."),
        "{body}"
    );
    assert!(!project.join("node_modules/stale.js").exists());
    assert!(project.join("node_modules").is_dir());
    assert_eq!(
        std::fs::read_to_string(project.join("installs.log"))?,
        "install\ninstall\n",
        "installed by the first build, and again by the deep clean"
    );
    assert!(project.join("dist/index.html").is_file());

    // A framework that installs nothing has nothing to deep clean, and says so.
    let plain = Application::new(
        "Plain",
        "plain",
        FrameworkRef::new("shell")
            .with("store", "apps")
            .with("project", "plain"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    save_application(&catalog, &plain).await?;
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/applications/{}/deep-clean", plain.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

/// A framework's secret setting (a keystore password) is never sent back to the
/// admin, and an edit that hands back the mask keeps the stored value.
#[tokio::test]
async fn a_framework_secret_is_masked_and_survives_an_edit() -> sc_error::Result<()> {
    let (mut client, catalog, _dir, _db) = serve("secret").await?;
    let app = Application::new(
        "Vault",
        "vault",
        FrameworkRef::new("shell")
            .with("store", "apps")
            .with("project", "vault")
            .with("token", "s3cret"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    save_application(&catalog, &app).await?;

    let (status, list) = client.send("GET", "/api/applications", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let mut row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["subdomain"] == json!("vault"))
        .unwrap()
        .clone();
    assert_eq!(row["framework"]["config"]["token"], json!(SECRET_SENTINEL));
    assert!(!list.to_string().contains("s3cret"));

    // Save the row as the form would: renamed, the mask handed back.
    row["name"] = json!("Vault 2");
    let (status, saved) = client
        .send("PUT", &format!("/api/applications/{}", app.id), Some(row))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["framework"]["config"]["token"],
        json!(SECRET_SENTINEL)
    );
    let stored = load_application(&catalog, app.id).await?.unwrap();
    assert_eq!(stored.name, "Vault 2");
    assert_eq!(stored.framework.config["token"], json!("s3cret"));
    Ok(())
}

/// A target's operation (a keystore generator): the module's file lands in the
/// store and its settings are saved, a second run does not replace the file,
/// and an answer reaching beyond the target's settings changes nothing.
#[tokio::test]
async fn a_target_operation_writes_its_file_and_saves_its_settings() -> sc_error::Result<()> {
    let (mut client, catalog, dir, _db) = serve("operation").await?;
    let app = Application::new(
        "Keys",
        "keys",
        FrameworkRef::new("shell")
            .with("store", "apps")
            .with("project", "keys"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    save_application(&catalog, &app).await?;
    let url = |op: &str| {
        format!(
            "/api/applications/{}/targets/bundle/operations/{op}",
            app.id
        )
    };

    // The form's unsaved password is what the module is given.
    let (status, body) = client
        .send(
            "POST",
            &url("make_key"),
            Some(json!({ "config": { "store": "apps", "project": "keys", "key_password": "typed" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("made"));
    assert_eq!(body["files"], json!(["keys/keys.key"]));
    assert_eq!(body["git_repo"], json!(false));
    // Settings come back as the form shows them: the secret masked.
    assert_eq!(body["settings"]["key_file"], json!("keys/keys.key"));
    assert_eq!(body["settings"]["key_password"], json!(SECRET_SENTINEL));
    assert_eq!(std::fs::read(dir.0.join("keys/keys.key"))?, b"KEY");
    let stored = load_application(&catalog, app.id).await?.unwrap();
    assert_eq!(stored.framework.config["key_file"], json!("keys/keys.key"));
    assert_eq!(stored.framework.config["key_password"], json!("typed"));

    // Again: the key is not replaced, and nothing is changed.
    std::fs::write(dir.0.join("keys/keys.key"), "SIGNED-WITH")?;
    let (status, body) = client
        .send(
            "POST",
            &url("make_key"),
            Some(json!({ "config": { "store": "apps", "key_password": SECRET_SENTINEL } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("already exists"), "{body}");
    assert_eq!(
        std::fs::read_to_string(dir.0.join("keys/keys.key"))?,
        "SIGNED-WITH"
    );

    // An answer naming a setting that is not the target's writes nothing.
    let (status, body) = client.send("POST", &url("stray"), Some(json!({}))).await;
    assert!(
        status.is_server_error() || status.is_client_error(),
        "{body}"
    );
    assert!(body.to_string().contains("project"), "{body}");
    assert!(!dir.0.join("keys/stray.key").exists());
    assert_eq!(
        load_application(&catalog, app.id)
            .await?
            .unwrap()
            .framework
            .config["project"],
        json!("keys")
    );

    // An operation the target does not declare is not run at all.
    let (status, _) = client.send("POST", &url("nope"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}
