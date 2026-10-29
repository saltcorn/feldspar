//! Target builds on a build host (`TODO-build-hosts.md` B2): the build button
//! queues a job when this server cannot build the target and an online build
//! host can, and a fake agent here does what `feldspar build-agent` does —
//! claims the job, fetches the project, sends a log and an artifact — so the
//! file ends up in the store where a local build leaves it.
//!
//! The target requires an operating system no test runs on, so this server is
//! never ready for it, as a Linux server is not for iOS. The framework is
//! stubbed as in `build_updates_client.rs`; the registry is process-wide, which
//! is why this is its own test binary.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{
    ApiConfig, Application, BuildTemplate, DeclaredFile, FilePhase, FrameworkDecl, FrameworkHost,
    FrameworkRef, FrameworkSet, TargetRequirement, TargetRequirementKind, TargetTemplate,
    add_build_host, bootstrap, install_frameworks, save_application,
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
use sc_types::{BasicType, FormField};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";

fn template(source: &str) -> Template {
    Template::parse(source).expect("the test's own template parses")
}

/// A framework with one target, `remote`, that builds only on Plan 9, and
/// whose environment carries a secret setting, as the iOS one carries the App
/// Store Connect key.
fn remote_framework() -> FrameworkDecl {
    FrameworkDecl {
        name: "remote-shell".to_owned(),
        module: "@feldspar/remote-shell".to_owned(),
        label: "Remote shell".to_owned(),
        description: "A project whose target builds elsewhere.".to_owned(),
        config_spec: vec![
            FormField::new("store", BasicType::Text).required(),
            FormField::new("project", BasicType::Text).default_value(""),
            FormField::new("token", BasicType::Text).secret(),
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
        scaffolds: false,
        targets: vec![TargetTemplate {
            name: "remote".to_owned(),
            label: "Remote bundle".to_owned(),
            command: template("sh bundle.sh"),
            artifact: template("{{ project }}/remote-output/app.bin"),
            env: BTreeMap::from([("TOKEN".to_owned(), template("{{ token }}"))]),
            requires: vec![TargetRequirement {
                kind: TargetRequirementKind::Os {
                    name: "plan9".to_owned(),
                },
                hint: String::new(),
            }],
            options: Vec::new(),
            operations: Vec::new(),
        }],
    }
}

struct StubHost;

#[async_trait]
impl FrameworkHost for StubHost {
    fn frameworks(&self) -> Vec<FrameworkDecl> {
        vec![remote_framework()]
    }

    async fn framework_files(
        &self,
        _name: &str,
        _phase: FilePhase,
        context: Value,
    ) -> sc_error::Result<Vec<DeclaredFile>> {
        let runtime = context["runtime"].as_str().unwrap_or_default().to_owned();
        Ok(vec![DeclaredFile {
            path: format!("{runtime}/composables.ts"),
            contents: "export {};\n".to_owned(),
        }])
    }
}

#[derive(Clone)]
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    /// An admin's request: the session cookie and the CSRF token.
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

    /// A build agent's request: its bearer token and nothing else.
    async fn agent(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, BASE_DOMAIN)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    /// A heartbeat saying this agent's machine has everything `remote` needs.
    async fn heartbeat_ready(&self, token: &str) {
        let report = json!({
            "protocol": sc_app::BUILD_AGENT_PROTOCOL,
            "version": "test",
            "os": "plan9",
            "arch": "aarch64",
            "readiness": { "remote": { "missing": [] } },
        });
        let (status, body) = self
            .agent(
                token,
                "POST",
                "/build-agent/heartbeat",
                report.to_string().into_bytes(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A server with the stub framework, an application `site` whose project is in
/// the `apps` store, a signed-in admin, and a build host `Mac mini` for `remote`.
async fn serve(tag: &str) -> sc_error::Result<(Client, Application, String, TempDir, TestDb)> {
    install_frameworks(FrameworkSet::new(Arc::new(StubHost)))?;

    let db = TestDb::new().await?;
    let dir = TempDir(
        std::env::temp_dir().join(format!("sc-server-build-jobs-{}-{tag}", std::process::id())),
    );
    std::fs::remove_dir_all(&dir.0).ok();
    std::fs::create_dir_all(dir.0.join("site/node_modules"))?;
    std::fs::write(dir.0.join("site/bundle.sh"), "echo bundling\n")?;
    std::fs::write(dir.0.join("site/node_modules/big.js"), "installed")?;
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

    let app = Application::new(
        "Site",
        "site",
        FrameworkRef::new("remote-shell")
            .with("store", "apps")
            .with("project", "site")
            .with("token", "s3cret"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    let app = save_application(&catalog, &app).await?;
    let (_, token) = add_build_host(&catalog, "Mac mini", &["remote".to_owned()]).await?;

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
    Ok((client, app, token, dir, db))
}

#[tokio::test]
async fn a_build_host_takes_the_job_and_its_artifact_lands_in_the_store() -> sc_error::Result<()> {
    let (mut client, app, token, dir, _db) = serve("artifact").await?;
    let build = format!("/api/applications/{}/targets/remote/build", app.id);

    // The host has not been heard from: nothing can build the target.
    let (status, body) = client.send("POST", &build, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("plan9"),
        "{body}"
    );

    // Once it says it is ready, the list says the target builds there…
    client.heartbeat_ready(&token).await;
    let (_, list) = client.send("GET", "/api/applications", None).await;
    let readiness = &list[0]["targets"][0]["readiness"];
    assert_eq!(readiness["ready"], json!(true), "{list}");
    assert_eq!(readiness["build_host"], json!("Mac mini"), "{list}");

    // …and the button queues a job, which a second press joins, even one
    // made at the same moment.
    let (mut first, mut second) = (client.clone(), client.clone());
    let ((status, queued), (_, again)) = tokio::join!(
        first.send("POST", &build, None),
        second.send("POST", &build, None)
    );
    assert_eq!(status, StatusCode::OK, "{queued}");
    assert_eq!(queued["status"], json!("queued"), "{queued}");
    assert_eq!(again["started_at"], queued["started_at"], "{again}");
    // The generated code was written before the job was queued.
    let project = dir.0.join("site");
    assert!(project.join("src/feldspar/composables.ts").is_file());

    // A wrong token claims nothing, and neither does the admin's session.
    let (status, _) = client
        .agent("fsbh_nope", "POST", "/build-agent/claim", Vec::new())
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = client.send("POST", "/build-agent/claim", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The agent claims it: the target rendered for the app, secret included.
    let (status, claimed) = client
        .agent(&token, "POST", "/build-agent/claim", Vec::new())
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&claimed)
    );
    let claimed = json_of(&claimed);
    assert_eq!(claimed["target"]["name"], json!("remote"), "{claimed}");
    assert_eq!(
        claimed["target"]["env"]["TOKEN"],
        json!("s3cret"),
        "{claimed}"
    );
    assert_eq!(
        claimed["target"]["artifact"],
        json!("site/remote-output/app.bin")
    );
    let job = claimed["job"].as_str().unwrap().to_owned();
    let (_, running) = client.send("GET", &build, None).await;
    assert_eq!(running["status"], json!("running"), "{running}");
    assert_eq!(running["build_host"], json!("Mac mini"), "{running}");

    // The project comes as a zip, with the generated code and without what
    // the build installs again.
    let (status, zip) = client
        .agent(
            &token,
            "GET",
            &format!("/build-agent/jobs/{job}/project"),
            Vec::new(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    assert!(names.contains(&"site/bundle.sh".to_owned()), "{names:?}");
    assert!(
        names.contains(&"site/src/feldspar/composables.ts".to_owned()),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("node_modules")),
        "{names:?}"
    );
    let mut script = String::new();
    archive
        .by_name("site/bundle.sh")
        .unwrap()
        .read_to_string(&mut script)
        .unwrap();
    assert_eq!(script, "echo bundling\n");

    // Another host's token opens none of this job's routes.
    let catalog_token = {
        let (status, created) = client
            .send(
                "POST",
                "/api/build-hosts",
                Some(json!({ "name": "Other Mac", "targets": ["remote"] })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        created["token"].as_str().unwrap().to_owned()
    };
    let (status, _) = client
        .agent(
            &catalog_token,
            "POST",
            &format!("/build-agent/jobs/{job}/log"),
            b"x".to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The log arrives as the build runs, in the store beside a local build's.
    let (status, _) = client
        .agent(
            &token,
            "POST",
            &format!("/build-agent/jobs/{job}/log"),
            b"step 1\n".to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let log_path = running["log_path"].as_str().unwrap();
    assert!(
        log_path.starts_with("site/build-logs/remote-"),
        "{log_path}"
    );
    let log = std::fs::read_to_string(dir.0.join(log_path))?;
    assert!(
        log.contains("Building on the build host `Mac mini`."),
        "{log}"
    );
    assert!(log.ends_with("step 1\n"), "{log}");

    // The artifact lands at the target's path in the store.
    let (status, _) = client
        .agent(
            &token,
            "POST",
            &format!("/build-agent/jobs/{job}/artifact"),
            b"APP".to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        std::fs::read(project.join("remote-output/app.bin"))?,
        b"APP"
    );
    let (_, done) = client.send("GET", &build, None).await;
    assert_eq!(done["status"], json!("succeeded"), "{done}");
    assert_eq!(
        done["artifact"],
        json!("site/remote-output/app.bin"),
        "{done}"
    );
    assert_eq!(done["size"], json!(3), "{done}");
    assert!(
        done["log"].as_str().unwrap_or_default().contains("step 1"),
        "{done}"
    );

    // A finished job takes nothing more.
    let (status, _) = client
        .agent(
            &token,
            "POST",
            &format!("/build-agent/jobs/{job}/log"),
            b"late".to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_job_fails_with_the_hosts_reason_or_when_the_host_stops_answering() -> sc_error::Result<()>
{
    let (mut client, app, token, dir, db) = serve("lost").await?;
    let build = format!("/api/applications/{}/targets/remote/build", app.id);
    client.heartbeat_ready(&token).await;

    // The agent says why the build failed; the admin reads it on the job and
    // at the end of the log.
    client.send("POST", &build, None).await;
    let (_, claimed) = client
        .agent(&token, "POST", "/build-agent/claim", Vec::new())
        .await;
    let job = json_of(&claimed)["job"].as_str().unwrap().to_owned();
    let (status, _) = client
        .agent(
            &token,
            "POST",
            &format!("/build-agent/jobs/{job}/failed"),
            json!({ "error": "xcodebuild exited with 65" })
                .to_string()
                .into_bytes(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, failed) = client.send("GET", &build, None).await;
    assert_eq!(failed["status"], json!("failed"), "{failed}");
    assert_eq!(
        failed["error"],
        json!("xcodebuild exited with 65"),
        "{failed}"
    );
    let log = std::fs::read_to_string(dir.0.join(failed["log_path"].as_str().unwrap()))?;
    assert!(log.ends_with("xcodebuild exited with 65\n"), "{log}");

    // A host that stops answering mid-build loses the job when its lease runs
    // out, and the job says so.
    let (_, queued) = client.send("POST", &build, None).await;
    assert_eq!(queued["status"], json!("queued"), "{queued}");
    let (status, _) = client
        .agent(&token, "POST", "/build-agent/claim", Vec::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    db.client()
        .await?
        .batch_execute(
            "UPDATE _fd_build_jobs SET lease_until = now() - interval '1 minute' \
             WHERE state = 'running'",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let (_, lost) = client.send("GET", &build, None).await;
    assert_eq!(lost["status"], json!("failed"), "{lost}");
    assert!(
        lost["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`Mac mini` stopped answering"),
        "{lost}"
    );

    // An artifact that finishes arriving after the lease ran out is not
    // stored: the job keeps its end, and no part file is left behind.
    client.send("POST", &build, None).await;
    let (_, claimed) = client
        .agent(&token, "POST", "/build-agent/claim", Vec::new())
        .await;
    let job = json_of(&claimed)["job"].as_str().unwrap().to_owned();
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let upload = Request::builder()
        .method("POST")
        .uri(format!("/build-agent/jobs/{job}/artifact"))
        .header(header::HOST, BASE_DOMAIN)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from_stream(rx))
        .unwrap();
    let uploading = tokio::spawn(client.router.clone().oneshot(upload));
    tx.unbounded_send(Ok(b"half".to_vec())).unwrap();
    let artifact = dir.0.join("site/remote-output/app.bin");
    let partial = dir.0.join("site/remote-output/app.bin.part");
    for _ in 0..500 {
        if partial.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(partial.exists(), "the upload never started");
    db.client()
        .await?
        .batch_execute(
            "UPDATE _fd_build_jobs SET lease_until = now() - interval '1 minute' \
             WHERE state = 'running'",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    tx.unbounded_send(Ok(b" more".to_vec())).unwrap();
    drop(tx);
    let response = uploading.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!artifact.exists() && !partial.exists());
    let (_, late) = client.send("GET", &build, None).await;
    assert_eq!(late["status"], json!("failed"), "{late}");

    // An agent that asks for a new job while one is still running for it lost
    // that one: it fails at once, though the heartbeats keep the lease fresh.
    client.send("POST", &build, None).await;
    client
        .agent(&token, "POST", "/build-agent/claim", Vec::new())
        .await;
    client.heartbeat_ready(&token).await;
    let (status, _) = client
        .agent(&token, "POST", "/build-agent/claim", Vec::new())
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, dropped) = client.send("GET", &build, None).await;
    assert_eq!(dropped["status"], json!("failed"), "{dropped}");
    assert!(
        dropped["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`Mac mini` dropped the build"),
        "{dropped}"
    );
    Ok(())
}
