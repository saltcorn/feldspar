//! `feldspar build-agent` building a job (`TODO-build-hosts.md` B2.5): against
//! a real server on a real port, the agent claims a queued job, unpacks the
//! project into its workspace, runs the target with the code a local build
//! runs, sends the log, and sends the artifact, which lands in the store.
//!
//! The framework is stubbed, and the registry it goes into is process-wide,
//! which is why this is its own test binary.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sc_app::{
    Application, BuildTemplate, DeclaredFile, FilePhase, FrameworkDecl, FrameworkHost,
    FrameworkRef, FrameworkSet, JobState, QueuedJob, TargetTemplate, install_frameworks,
};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, FileStoreId};
use sc_cli::build_agent::{Agent, Built, Worker};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_expr::Template;
use sc_files::LocalFileStore;
use sc_server::{AppMounts, ServerConfig, admin_handlers, build_router_with_apps};
use sc_test_harness::TestDb;
use sc_types::{BasicType, FormField};
use serde_json::Value;

fn template(source: &str) -> Template {
    Template::parse(source).expect("the test's own template parses")
}

/// A target built by `sh bundle.sh`, which writes the secret it is given into
/// the artifact, so the test sees the environment arrived.
fn framework() -> FrameworkDecl {
    FrameworkDecl {
        name: "shell".to_owned(),
        module: "@feldspar/shell".to_owned(),
        label: "Shell".to_owned(),
        description: String::new(),
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
            runtime: None,
            client_file: "client.ts".to_owned(),
            server: None,
        },
        csp: None,
        builder_prompt: None,
        checks: Vec::new(),
        scaffolds: false,
        targets: vec![TargetTemplate {
            name: "bundle".to_owned(),
            label: "Bundle".to_owned(),
            command: template("sh bundle.sh"),
            artifact: template("{{ project }}/bundle-output/app.bin"),
            env: BTreeMap::from([("TOKEN".to_owned(), template("{{ token }}"))]),
            requires: Vec::new(),
            options: Vec::new(),
            operations: Vec::new(),
        }],
    }
}

struct StubHost;

#[async_trait]
impl FrameworkHost for StubHost {
    fn frameworks(&self) -> Vec<FrameworkDecl> {
        vec![framework()]
    }

    async fn framework_files(
        &self,
        _name: &str,
        _phase: FilePhase,
        _context: Value,
    ) -> sc_error::Result<Vec<DeclaredFile>> {
        Ok(Vec::new())
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch(tag: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!("sc-cli-agent-jobs-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

#[tokio::test]
async fn the_agent_builds_a_claimed_job_and_sends_the_artifact_home() -> sc_error::Result<()> {
    install_frameworks(FrameworkSet::new(Arc::new(StubHost)))?;
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;

    let store = scratch("store");
    std::fs::create_dir_all(store.0.join("site"))?;
    std::fs::write(
        store.0.join("site/bundle.sh"),
        "echo bundling\nmkdir -p bundle-output\nprintf %s \"$TOKEN\" > bundle-output/app.bin\n",
    )?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &store.0)?))?;
    let app = sc_app::save_application(
        &catalog,
        &Application::new(
            "Site",
            "site",
            FrameworkRef::new("shell")
                .with("store", "apps")
                .with("project", "site")
                .with("token", "s3cret"),
        )
        .with_file_store(FileStoreId("apps".to_owned())),
    )
    .await?;

    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| sc_error::Error::msg(e.to_string()))?;
    let server = format!(
        "http://{}",
        listener
            .local_addr()
            .map_err(|e| sc_error::Error::msg(e.to_string()))?
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let (_, token) = sc_app::add_build_host(&catalog, "Mac mini", &["bundle".into()]).await?;
    // Online, so the job waits for it rather than being given up.
    Agent::new(&server, token.clone())?
        .heartbeat()
        .await
        .expect("the server answers");
    let queued = sc_app::queue_build_job(
        &catalog,
        QueuedJob {
            application: app.id.0,
            target: "bundle",
            label: "Bundle",
            store: "apps",
            log_path: "site/build-logs/bundle-20260101-000000.log",
        },
    )
    .await?;

    let workspace = scratch("workspace");
    let worker = Worker::new(&server, token, workspace.0.clone())?;
    let job = worker
        .claim(0)
        .await
        .expect("the server answers")
        .expect("there is a job");
    assert_eq!(job.spec.name, "bundle");
    assert_eq!(job.application, "Site");
    let built = worker.build(&job).await.expect("the server answers");
    assert_eq!(built, Built::Artifact(6));

    // Built in the application's own workspace, and the artifact went home.
    assert!(
        workspace
            .0
            .join(&job.application_id)
            .join("site/bundle.sh")
            .is_file()
    );
    assert_eq!(
        std::fs::read(store.0.join("site/bundle-output/app.bin"))?,
        b"s3cret"
    );
    let done = sc_app::latest_build_job(&catalog, app.id.0, "bundle")
        .await?
        .expect("the job is kept");
    assert_eq!(done.id, queued.id);
    assert_eq!(done.state, JobState::Succeeded, "{:?}", done.error);
    let log = std::fs::read_to_string(store.0.join("site/build-logs/bundle-20260101-000000.log"))?;
    assert!(
        log.contains("$ sh bundle.sh") && log.contains("bundling"),
        "{log}"
    );

    // Nothing more to do.
    assert!(worker.claim(0).await.expect("the server answers").is_none());
    Ok(())
}
