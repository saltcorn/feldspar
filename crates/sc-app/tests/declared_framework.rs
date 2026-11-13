//! An application whose framework a **module** declared (§13.3, §15.1).
//!
//! `scaffold_app.rs` is the same story for `react`, and the point of this file is
//! that it is the *same story*: the application is saved, validated, scaffolded,
//! resolved to a build and given a builder agent by exactly the code that does
//! all five for the two frameworks written in Rust. Nothing downstream of
//! `app_source_from_config` can tell which it is building.
//!
//! The module itself is stubbed. What a real one does — evaluate a `frameworks`
//! export on a worker and answer files from a `scaffold` function — is
//! `sc-module`'s `tests/frameworks.rs`, on a real worker with a real npm install;
//! reproducing that here would test the worker twice and this crate's half not at
//! all. So the [`FrameworkHost`] here is three lines, and everything asserted
//! below is `sc-app`'s own.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sc_app::{
    ApiConfig, Application, BuildTemplate, CspPolicy, DeclaredFile, FilePhase, FrameworkDecl,
    FrameworkHost, FrameworkRef, FrameworkSet, InstallSpec, app_source_from_config,
    emit_app_runtime, framework_builder_agent, framework_config_spec, framework_default_csp,
    install_frameworks, registered_framework_info, save_application, scaffold_app,
};
use sc_catalog::{Catalog, FileStoreId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::Template;
use sc_files::LocalFileStore;
use sc_test_harness::TestDb;
use sc_types::{BasicType, FormField};
use serde_json::Value as Json;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<TempDir> {
        let dir = std::env::temp_dir().join(format!(
            "sc-app-declared-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
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

fn template(source: &str) -> Template {
    Template::parse(source).expect("the test's own template parses")
}

/// The declaration a module's `frameworks: { vue: … }` export becomes.
fn vue() -> FrameworkDecl {
    FrameworkDecl {
        name: "vue".to_owned(),
        module: "@feldspar/vue".to_owned(),
        label: "Vue".to_owned(),
        description: "A Vue 3 + Vite project.".to_owned(),
        config_spec: vec![
            FormField::new("store", BasicType::Text).required(),
            FormField::new("project", BasicType::Text).default_value(""),
        ],
        build: BuildTemplate {
            store: template("{{ store }}"),
            source: template("{{ project }}"),
            output: template("{{ project }}/dist"),
            command: "npm run build".to_owned(),
            install: Some(InstallSpec {
                command: "npm".to_owned(),
                args: vec!["install".to_owned()],
                marker: "node_modules".to_owned(),
            }),
            runtime: Some(template("{{ project }}/src/feldspar")),
            client_file: "client.ts".to_owned(),
            server: None,
        },
        csp: Some(CspPolicy::strict().directive("img-src", ["'self'", "data:"])),
        builder_prompt: Some(template(
            "You build the Vue application {{ app }} in {{ root }} of {{ store }}.",
        )),
        checks: vec!["typecheck".to_owned()],
        scaffolds: true,
        settings_tabs: None,
        targets: Vec::new(),
    }
}

/// A module host answering the two files a Vue project's own half is made of.
struct StubHost;

#[async_trait]
impl FrameworkHost for StubHost {
    fn frameworks(&self) -> Vec<FrameworkDecl> {
        vec![vue()]
    }

    async fn framework_files(
        &self,
        _name: &str,
        phase: FilePhase,
        context: Json,
    ) -> Result<Vec<DeclaredFile>> {
        let runtime = context["runtime"].as_str().unwrap_or_default().to_owned();
        let tables: Vec<String> = context["tables"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .filter_map(|t| t["pascal"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let mut files = vec![DeclaredFile {
            path: format!("{runtime}/composables.ts"),
            contents: tables
                .iter()
                .map(|t| format!("export function use{t}() {{}}\n"))
                .collect(),
        }];
        if phase == FilePhase::Scaffold {
            files.push(DeclaredFile {
                path: "src/App.vue".to_owned(),
                // The framework's own settings reach the generator, so a
                // framework can use one it declared (the project, here).
                contents: format!(
                    "<template>{} {}</template>\n",
                    context["name"], context["settings"]["project"]
                ),
            });
            for table in &tables {
                files.push(DeclaredFile {
                    path: format!("src/pages/{table}.vue"),
                    contents: "<template />\n".to_owned(),
                });
            }
        }
        Ok(files)
    }
}

/// Install the stub as this process's framework set.
///
/// Process-wide, like `sc-types`' rich-type registry — so every test in this file
/// installs the same set rather than racing to install different ones. Tests in
/// the other files ask for `react` and `code`, which this does not touch.
fn install_stub() {
    install_frameworks(FrameworkSet::new(Arc::new(StubHost)))
        .expect("the registry is not poisoned");
}

async fn catalog_with_tasks(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE tasks (\
               id bigint generated by default as identity primary key, \
               title text not null, \
               done boolean)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// The whole configuration of a Vue app: a store and a project name — the same
/// two a React app carries, because this framework declared the same two.
fn todo_app() -> Application {
    Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("vue")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_table("tasks".to_owned())
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"))
}

#[test]
fn a_declared_framework_joins_the_registry_the_admin_ui_reads() {
    install_stub();
    let listed = registered_framework_info();
    let names: Vec<&str> = listed.iter().map(|f| f.name.as_str()).collect();
    // The built-ins keep their order and their place at the front: `react` is
    // the path an admin should take, and a module cannot displace that.
    assert_eq!(names, ["react", "code", "none", "vue"]);
    let vue = listed.iter().find(|f| f.name == "vue").unwrap();
    assert_eq!(vue.label, "Vue");
    assert!(vue.serves_ui);

    // Its settings render like any other framework's, from the same lookup.
    let spec = framework_config_spec("vue").unwrap();
    assert_eq!(
        spec.iter().map(|f| f.name()).collect::<Vec<_>>(),
        ["store", "project"]
    );
    // And an unknown name still names what *is* registered, now including it.
    let msg = framework_config_spec("svelte").unwrap_err().to_string();
    assert!(msg.contains("svelte") && msg.contains("`vue`"), "{msg}");

    // Its default CSP is the strict baseline plus its own widenings.
    let csp = framework_default_csp("vue");
    assert_eq!(csp.directives["default-src"], ["'self'"]);
    assert_eq!(csp.directives["img-src"], ["'self'", "data:"]);
}

#[test]
fn a_declared_framework_resolves_to_the_same_app_source_the_built_ins_do() {
    install_stub();
    let source = app_source_from_config(&todo_app().framework).unwrap();
    assert_eq!(source.store.0, "apps");
    assert_eq!(source.build.command, "npm");
    assert_eq!(source.build.args, ["run", "build"]);
    assert_eq!(source.build.source_dir, "todo");
    assert_eq!(source.build.output_dir, "todo/dist");
    assert_eq!(
        source.build.install.as_ref().unwrap().marker,
        "node_modules"
    );
    assert_eq!(
        source.client_path.as_deref(),
        Some("todo/src/feldspar/client.ts")
    );

    // Blank project, store root — `react`'s rule, falling out of the templates
    // rather than being written again per framework.
    let mut root = todo_app();
    root.framework = FrameworkRef::new("vue")
        .with("store", "apps")
        .with("project", "");
    let source = app_source_from_config(&root.framework).unwrap();
    assert_eq!(source.build.source_dir, "");
    assert_eq!(source.build.output_dir, "dist");
    assert_eq!(
        source.client_path.as_deref(),
        Some("src/feldspar/client.ts")
    );
}

#[test]
fn a_declared_framework_declares_the_agent_that_builds_its_applications() {
    install_stub();
    let app = todo_app();
    let spec = framework_builder_agent(&app.framework, &app).expect("vue declares one");
    assert_eq!(spec.name, "build-todo");
    // The framework's own sentence, interpolated with the application and the
    // tree the agent is scoped to…
    assert!(
        spec.system_prompt
            .contains("You build the Vue application Todo in todo of apps."),
        "{}",
        spec.system_prompt
    );
    // …and nothing else: how to work is the `coding` trait's own prompt.
    assert!(!spec.system_prompt.contains("Work in small steps"));
    // Scoped to this application's own source tree, like a React app's.
    let coding = spec
        .traits
        .iter()
        .find(|t| t.trait_ == sc_app::TRAIT_CODING)
        .expect("a coding trait");
    assert_eq!(
        coding.config.get(sc_app::TRAIT_CFG_STORE).unwrap(),
        &serde_json::json!("apps")
    );
    assert_eq!(
        coding.config.get(sc_app::TRAIT_CFG_ROOT).unwrap(),
        &serde_json::json!("todo")
    );
    // The checks are the framework's to declare, and the build follows them.
    assert_eq!(
        coding.config.get(sc_app::TRAIT_CFG_CHECKS).unwrap(),
        &serde_json::json!(["typecheck"])
    );
    assert_eq!(
        coding.config.get(sc_app::TRAIT_CFG_APPLICATION).unwrap(),
        &serde_json::json!("todo")
    );
}

#[tokio::test]
async fn the_server_writes_the_modules_project_and_its_own_generated_half() -> Result<()> {
    install_stub();
    let db = TestDb::new().await?;
    let cat = catalog_with_tasks(&db).await?;
    sc_auth::bootstrap(&cat).await?;
    let tmp = TempDir::new("scaffold")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    sc_app::bootstrap(&cat).await?;
    let app = save_application(&cat, &todo_app()).await?;

    let report = scaffold_app(&cat, &app, None).await?;
    assert_eq!(report.project, "todo");

    // The module's half — including a page named from the *pascal* form this
    // crate worked out, not one the module re-derived from the table name.
    let files = &report.files;
    for expected in [
        "todo/src/App.vue",
        "todo/src/pages/Tasks.vue",
        "todo/src/feldspar/composables.ts",
    ] {
        assert!(files.iter().any(|f| f == expected), "{expected}: {files:?}");
    }
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("todo/src/App.vue"))?,
        "<template>\"todo\" \"todo\"</template>\n",
        "the generator sees the application's framework settings"
    );

    // And Saltcorn's half, in the directory the framework named — written for a
    // declared framework exactly as it is for `react`, because it comes from the
    // application's own API rather than from anything a module knows.
    for expected in [
        "todo/src/feldspar/client.ts",
        "todo/src/feldspar/helper.ts",
        "todo/src/feldspar/schema.sql",
        "todo/src/feldspar/SKILL.md",
    ] {
        assert!(files.iter().any(|f| f == expected), "{expected}: {files:?}");
    }

    // The client is the real generated one: it has this application's table on
    // it, typed from this application's columns.
    let client = std::fs::read_to_string(tmp.path().join("todo/src/feldspar/client.ts"))?;
    assert!(client.contains("TasksRow"), "the client is generated");
    assert!(client.contains("createClient"));
    // …and the composables the module wrote name the same table.
    let composables = std::fs::read_to_string(tmp.path().join("todo/src/feldspar/composables.ts"))?;
    assert_eq!(composables, "export function useTasks() {}\n");

    // A second scaffold into an occupied directory is refused, as it is for
    // `react`: the rule is the scaffold's, not the framework's.
    let err = scaffold_app(&cat, &app, None).await.unwrap_err();
    assert!(err.to_string().contains("not empty"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_rebuild_rewrites_both_generated_halves_and_nothing_else() -> Result<()> {
    install_stub();
    let db = TestDb::new().await?;
    let cat = catalog_with_tasks(&db).await?;
    sc_auth::bootstrap(&cat).await?;
    let tmp = TempDir::new("runtime")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    sc_app::bootstrap(&cat).await?;
    let app = save_application(&cat, &todo_app()).await?;
    scaffold_app(&cat, &app, None).await?;

    // The developer's own file, which a re-emit must not touch.
    let mine = "<template>mine</template>\n";
    std::fs::write(tmp.path().join("todo/src/App.vue"), mine)?;

    let source = app_source_from_config(&app.framework)?;
    let written = emit_app_runtime(&cat, &app, &source, None).await?;

    // The runtime phase writes the framework's generated half and Saltcorn's,
    // and no page and no shell — that is the whole difference between the two
    // phases, and it is the module that decided it by answering fewer files.
    let mut written = written;
    written.sort();
    assert_eq!(
        written,
        [
            "todo/src/feldspar/SKILL.md",
            "todo/src/feldspar/client.ts",
            "todo/src/feldspar/composables.ts",
            "todo/src/feldspar/helper.ts",
            // Framework-neutral, so a declared framework gets it unchanged and
            // writes its own provider over it (§16.1, 4.3).
            "todo/src/feldspar/messages.ts",
            "todo/src/feldspar/schema.sql",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("todo/src/App.vue"))?,
        mine
    );
    Ok(())
}
