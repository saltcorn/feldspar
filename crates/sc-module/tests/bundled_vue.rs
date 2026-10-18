//! `plugins/vue` — the bundled Vue module — installed from the directory it
//! ships in, and asked for a project.
//!
//! This is the other half of the framework seam's proof. `frameworks.rs` drives
//! the seam with a fixture that writes two files; this drives it with the module
//! an admin actually installs, and asks the question that matters about a
//! *scaffold*: is what it wrote a project?
//!
//! The strongest thing asserted here is that **every relative import in the
//! generated project resolves** — to another generated file, or to one of the
//! four Saltcorn writes into the runtime directory for every framework. That is
//! the class of mistake a generator makes: a page importing a composable that is
//! only written when the table has a key, a shell importing an auth layer that
//! only exists when the app can sign someone in. Each is a project that installs
//! and then fails to build, and each is caught here without a Node toolchain.
//!
//! It needs npm to *install* the module (which downloads nothing — the module has
//! no dependencies), and skips without it. It does not build the project: that
//! needs Vue, Vite and a network, and is the same opt-in the React scaffold's
//! end-to-end test is behind.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{have_npm, temp_root};
use sc_app::{DeclaredFile, FilePhase, FrameworkHost};
use sc_module::{
    BundledModules, Installer, LoadedModule, Module, ModuleFrameworks, ModuleHost,
    ModulePermissions, ModuleSet, ModuleSource,
};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// The files Saltcorn writes into the runtime directory for **every**
/// framework, which the module's project may import and must not write.
const SALTCORN_WRITES: [&str; 7] = [
    "client.ts",
    "helper.ts",
    "offline-types.ts",
    "offline-backends.ts",
    "offline-copy.ts",
    "schema.sql",
    "SKILL.md",
];

/// The bundled Vue module, installed into a throwaway root from the directory it
/// ships in — which is what the Modules tab's Install button does, minus the row
/// and the HTTP — then loaded on a worker.
async fn frameworks(tag: &str) -> ModuleFrameworks {
    let catalog = BundledModules::discover(None);
    let entry = catalog.get("vue").expect("the Vue module ships");
    let root = temp_root(tag);
    let installer = Installer::new(&root);
    let package = installer
        .install(ModuleSource::Local, &entry.directory.display().to_string())
        .await
        .unwrap_or_else(|e| panic!("installing plugins/vue: {e}"));
    assert_eq!(package.name, entry.name);

    let host = Arc::new(ModuleHost::new(&root));
    let manifest = host
        .load(
            &package.name,
            &installer.package_dir(&package.name),
            &json!({}),
            &ModulePermissions::closed(),
        )
        .await
        .expect("the bundled Vue module loads");
    assert!(
        manifest.issues.is_empty(),
        "the module this server ships should declare nothing it has to apologise for: {:?}",
        manifest.issues
    );

    let module = Module::new(
        &package.name,
        ModuleSource::Bundled,
        entry.directory.display().to_string(),
    );
    let set = ModuleSet::empty().merged(vec![LoadedModule {
        module,
        manifest: Some(manifest),
        config_spec: Vec::new(),
        issues: Vec::new(),
    }]);
    let frameworks = ModuleFrameworks::new(&host, &set);
    assert!(
        frameworks.issues().is_empty(),
        "the declaration translates cleanly: {:?}",
        frameworks.issues()
    );
    frameworks
}

/// One table, as `sc-app`'s context describes it: the names, the key, what the
/// API lets a page do, and the create form this server worked out.
fn tasks_table() -> Json {
    json!({
        "name": "tasks",
        "pascal": "Tasks",
        "title": "Tasks",
        "client": "tasks",
        "pk": "id",
        "pk_ts_type": "number",
        "storable": true,
        "ops": { "list": true, "get": true, "create": true, "update": true, "delete": true },
        "fields": [
            { "name": "id", "label": "Id", "control": "number", "ts_type": "number",
              "empty": "0", "required": true, "primary_key": true },
            { "name": "title", "label": "Title", "control": "text", "ts_type": "string",
              "empty": "\"\"", "required": true, "primary_key": false },
            { "name": "done", "label": "Done", "control": "checkbox", "ts_type": "boolean",
              "empty": "false", "required": false, "primary_key": false }
        ],
        "form": {
            "inputs": [
                { "name": "title", "label": "Title", "control": "text", "ts_type": "string",
                  "empty": "\"\"", "required": true, "primary_key": false },
                { "name": "done", "label": "Done", "control": "checkbox", "ts_type": "boolean",
                  "empty": "false", "required": false, "primary_key": false }
            ],
            "minted": []
        }
    })
}

/// A read-only table with no addressable row: the shape that catches a generator
/// writing a delete button for an endpoint that does not exist.
fn readings_table() -> Json {
    json!({
        "name": "readings",
        "pascal": "Readings",
        "title": "Readings",
        "client": "readings",
        "pk": null,
        "pk_ts_type": "string",
        "storable": false,
        "ops": { "list": true, "get": false, "create": false, "update": false, "delete": false },
        "fields": [
            { "name": "at", "label": "At", "control": "text", "ts_type": "string",
              "empty": "\"\"", "required": true, "primary_key": false }
        ],
        "form": { "inputs": [], "minted": [] }
    })
}

fn context(auth: bool, tables: Vec<Json>) -> Json {
    json!({
        "project": "todo",
        "name": "todo",
        "runtime": "src/feldspar",
        "client": "src/feldspar/client.ts",
        "auth": auth,
        "app": { "name": "My Todo", "subdomain": "todo", "description": "",
                 "url": "https://todo.example.com" },
        "tables": tables,
        "graphql": null,
        "schema_sql": "CREATE TABLE tasks ();",
        "roles": [{ "name": "Admin", "role": 1, "description": "" }],
        "skill": "# SKILL\n"
    })
}

/// Every relative import in every generated file, resolved against what the
/// project will actually contain.
fn assert_imports_resolve(files: &[DeclaredFile]) {
    let mut present: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
    for name in SALTCORN_WRITES {
        present.insert(format!("src/feldspar/{name}"));
    }

    for file in files {
        let dir = match file.path.rsplit_once('/') {
            Some((dir, _)) => dir.to_owned(),
            None => String::new(),
        };
        for spec in relative_imports(&file.contents) {
            let resolved = resolve(&dir, &spec);
            let found = present.contains(&resolved)
                || present.contains(&format!("{resolved}.ts"))
                || present.contains(&format!("{resolved}.vue"));
            assert!(
                found,
                "{} imports `{spec}`, which resolves to `{resolved}` and nothing writes it. \
                 The project has: {present:?}",
                file.path
            );
        }
    }
}

/// The `./x` and `../x` specifiers of every `import`/`export … from` in a file.
fn relative_imports(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, _) in contents.match_indices("from \"") {
        let rest = &contents[index + 6..];
        let Some(end) = rest.find('"') else { continue };
        let spec = &rest[..end];
        if spec.starts_with("./") || spec.starts_with("../") {
            out.push(spec.to_owned());
        }
    }
    out
}

/// `src/pages` + `../feldspar/composables` → `src/feldspar/composables`.
fn resolve(dir: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in spec.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[tokio::test]
async fn the_bundled_vue_module_declares_the_framework_an_admin_picks() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let frameworks = frameworks("bundled-vue-decl").await;
    let declared = frameworks.frameworks();
    assert_eq!(declared.len(), 1);
    let vue = &declared[0];
    assert_eq!(vue.name, "vue");
    assert_eq!(vue.info().label, "Vue");
    assert!(vue.info().description.contains("Vue 3"));
    assert!(vue.scaffolds);
    // Its builder agent checks with the project's own type check.
    assert_eq!(vue.checks, ["typecheck"]);

    // The same two settings `react` asks for, so an admin moving between them is
    // filling in the same form.
    assert_eq!(
        vue.config_spec.iter().map(|f| f.name()).collect::<Vec<_>>(),
        ["store", "project"]
    );

    let config: Attrs = [
        ("store".to_owned(), json!("apps")),
        ("project".to_owned(), json!("todo")),
    ]
    .into_iter()
    .collect();
    let spec = vue.build_spec(&config).unwrap();
    assert_eq!(spec.command, "npm");
    assert_eq!(spec.args, ["run", "build"]);
    assert_eq!(spec.source_dir, "todo");
    assert_eq!(spec.output_dir, "todo/dist");
    assert_eq!(spec.install.as_ref().unwrap().marker, "node_modules");
    assert_eq!(
        vue.client_path(&config).unwrap().unwrap(),
        "todo/src/feldspar/client.ts"
    );

    // No `'unsafe-inline'` and no `'unsafe-eval'` anywhere: Vue's SFC compiler
    // emits real module scripts, and the styling is a CSS file rather than
    // runtime `<style>` injection.
    let csp = vue.default_csp();
    let rendered = csp.header_value();
    assert!(!rendered.contains("unsafe"), "{rendered}");
    assert_eq!(csp.directives["connect-src"], ["'self'"]);
    assert_eq!(csp.directives["frame-ancestors"], ["'none'"]);
}

#[tokio::test]
async fn the_scaffold_is_a_vue_project_whose_every_import_resolves() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let frameworks = frameworks("bundled-vue-scaffold").await;
    let files = frameworks
        .framework_files(
            "vue",
            FilePhase::Scaffold,
            context(true, vec![tasks_table(), readings_table()]),
        )
        .await
        .expect("the scaffold runs");

    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    for expected in [
        "package.json",
        "vite.config.ts",
        "tsconfig.json",
        "index.html",
        "AGENTS.md",
        "src/main.ts",
        "src/App.vue",
        "src/router.ts",
        "src/app.css",
        "src/auth.ts",
        "src/Login.vue",
        "src/pages/Home.vue",
        "src/pages/Tasks.vue",
        "src/pages/Readings.vue",
        "src/feldspar/composables.ts",
        "src/feldspar/README.md",
    ] {
        assert!(paths.contains(&expected), "missing {expected}: {paths:?}");
    }
    // It writes none of the four that are Saltcorn's, in any phase.
    for name in SALTCORN_WRITES {
        let path = format!("src/feldspar/{name}");
        assert!(!paths.contains(&path.as_str()), "the module wrote {path}");
    }

    assert_imports_resolve(&files);

    let file = |path: &str| {
        files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path}"))
            .contents
            .clone()
    };

    // A Vue project, not a React one wearing a hat.
    let package = file("package.json");
    assert!(package.contains("\"vue\":"), "{package}");
    assert!(package.contains("\"vue-router\":"), "{package}");
    assert!(package.contains("@vitejs/plugin-vue"), "{package}");
    assert!(
        package.contains("vue-tsc --noEmit && vite build"),
        "the build type-checks the project against the generated client: {package}"
    );
    assert!(
        package.contains("\"typecheck\": \"vue-tsc --noEmit\""),
        "the check the framework declares is a script the project has: {package}"
    );
    assert!(!package.contains("react"), "{package}");
    assert!(file("index.html").contains("/src/main.ts"));

    // The composables are typed per table, over that table's own client object.
    let composables = file("src/feldspar/composables.ts");
    assert!(composables.starts_with("// Code generated by Saltcorn. DO NOT EDIT."));
    assert!(composables.contains("export function useTasks(): Query<TasksRow[]>"));
    assert!(composables.contains("api.tasks.list()"));
    assert!(composables.contains("export function useDeleteTasks(): Mutation<number>"));
    // …and a keyless, read-only table gets the read and nothing else: the
    // endpoints decide, not the table's shape.
    assert!(composables.contains("export function useReadings()"));
    assert!(!composables.contains("useDeleteReadings"));
    assert!(!composables.contains("useCreateReadings"));

    // The page for the writable table has the form and the delete button; the
    // read-only one has neither, and imports nothing it does not call.
    let tasks = file("src/pages/Tasks.vue");
    assert!(tasks.contains("useCreateTasks") && tasks.contains("useDeleteTasks"));
    assert!(tasks.contains("v-model=\"form.title\""));
    assert!(tasks.contains("v-model=\"form.done\" type=\"checkbox\""));
    let readings = file("src/pages/Readings.vue");
    assert!(readings.contains("useReadings"));
    assert!(
        !readings.contains("useCreateReadings") && !readings.contains("useDeleteReadings"),
        "{readings}"
    );
    assert!(
        !readings.contains("import { ref }"),
        "a page with no form must not import what it does not use: {readings}"
    );

    // The shell signs people in, and the router guards the routes.
    assert!(file("src/App.vue").contains("signOut"));
    let router = file("src/router.ts");
    assert!(router.contains("router.beforeEach"));
    assert!(router.contains("path: \"/tasks\""));
    assert!(router.contains("path: \"/login\""));
}

#[tokio::test]
async fn an_app_that_cannot_sign_anyone_in_gets_no_auth_layer() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let frameworks = frameworks("bundled-vue-anon").await;
    let files = frameworks
        .framework_files(
            "vue",
            FilePhase::Scaffold,
            context(false, vec![tasks_table()]),
        )
        .await
        .expect("the scaffold runs");

    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(!paths.contains(&"src/auth.ts"));
    assert!(!paths.contains(&"src/Login.vue"));

    // Which is the point of the import check: the shell and the router must stop
    // naming the auth layer, not merely stop shipping it.
    assert_imports_resolve(&files);
    let router = files.iter().find(|f| f.path == "src/router.ts").unwrap();
    assert!(
        !router.contents.contains("beforeEach"),
        "{}",
        router.contents
    );
    assert!(!router.contents.contains("Login"), "{}", router.contents);
}

#[tokio::test]
async fn a_rebuild_rewrites_the_generated_directory_and_nothing_else() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let frameworks = frameworks("bundled-vue-runtime").await;
    let files = frameworks
        .framework_files(
            "vue",
            FilePhase::Runtime,
            context(true, vec![tasks_table()]),
        )
        .await
        .expect("the runtime runs");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["src/feldspar/composables.ts", "src/feldspar/README.md"],
        "a build must not rewrite a page the developer owns"
    );
}

/// Install → scaffold → `npm install` → `vue-tsc` → `vite build`, with a real
/// database, a real file store and a real Node toolchain.
///
/// Opt-in (`SC_TEST_NPM=1`) because it needs npm and a network; it is slow and
/// would make the whole suite depend on both. What it buys, and nothing else
/// does: the generated **composables and pages are type-checked against the
/// generated client** — the project's build script is `vue-tsc --noEmit && vite
/// build` — so a drift between what `sc-api` emits and what this module's
/// generator calls fails here. That is the one class of mistake a scaffold in a
/// guest language can make that no amount of asserting on strings will catch.
///
/// It is also the whole feature end to end: an admin installs a module, picks
/// the framework it declared, and gets a built application — with no Rust in the
/// path that knows what Vue is.
#[tokio::test]
async fn the_bundled_vue_module_scaffolds_an_application_that_builds() {
    if std::env::var("SC_TEST_NPM").as_deref() != Ok("1") {
        eprintln!("skipping: set SC_TEST_NPM=1 to run the real npm build");
        return;
    }
    skip_without!(have_npm(), "npm is not on the PATH");

    let frameworks = frameworks("bundled-vue-e2e").await;
    // What the server does on every module change: the declaration goes into the
    // registry `sc-app` resolves an application's framework through.
    sc_app::install_frameworks(sc_app::FrameworkSet::new(Arc::new(frameworks)))
        .expect("the registry is not poisoned");

    let db = sc_test_harness::TestDb::new()
        .await
        .expect("a test database");
    db.client()
        .await
        .expect("a client")
        .batch_execute(
            "CREATE TABLE tasks (\
               id bigint generated by default as identity primary key, \
               title text not null, \
               done boolean)",
        )
        .await
        .expect("the app's table");
    let driver = Arc::new(sc_db_postgres::PgDriver::from_pool(db.pool().clone()));
    let cat = sc_catalog::Catalog::init(driver as Arc<dyn sc_db::DatabaseDriver>)
        .await
        .expect("a catalog");

    let dir = temp_root("bundled-vue-project");
    std::fs::create_dir_all(&dir).expect("the store's directory");
    cat.connect_file_store(Arc::new(
        sc_files::LocalFileStore::new("apps", &dir).expect("a store"),
    ))
    .expect("the store connects");
    sc_app::bootstrap(&cat)
        .await
        .expect("the applications table");

    // An ordinary application, differing from the tutorial's React one in one
    // word: the framework it names.
    let app = sc_app::Application::new(
        "Todo",
        "todo",
        sc_app::FrameworkRef::new("vue")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_table(sc_catalog::TableId("tasks".to_owned()))
    .with_file_store(sc_catalog::FileStoreId("apps".to_owned()))
    .with_api(sc_app::ApiConfig::new("rest", "/api"));
    let app = sc_app::save_application(&cat, &app)
        .await
        .expect("a `vue` application saves like any other");

    let report = sc_app::scaffold_app(&cat, &app, None)
        .await
        .expect("the module's scaffold runs");
    assert!(report.files.iter().any(|f| f == "todo/src/pages/Tasks.vue"));
    assert!(
        report
            .files
            .iter()
            .any(|f| f == "todo/src/feldspar/client.ts")
    );

    // No shell step between scaffolding and a served bundle: the build installs
    // the dependencies the module's `package.json` declared, then runs Vite.
    let source = sc_app::app_source_from_config(&app.framework).expect("a source");
    let built = sc_app::build_application(&cat, &app, &source, None)
        .await
        .expect("the scaffolded project type-checks and builds");
    assert!(built.installed, "the first build installs dependencies");

    let fw = sc_app::CodeFramework::new("vue", built.bundle);
    let index = fw.serve(&sc_app::AppRequest::get("/"));
    assert_eq!(index.status, 200);
    assert!(String::from_utf8_lossy(&index.body).contains("<div id=\"app\">"));
    // A client-routed deep link resolves to the entry point, as it does for a
    // React app: the serving path is shared, not reimplemented.
    assert_eq!(fw.serve(&sc_app::AppRequest::get("/tasks")).status, 200);

    std::fs::remove_dir_all(&dir).ok();
}
