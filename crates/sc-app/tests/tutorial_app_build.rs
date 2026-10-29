//! The tutorial, executed: `docs/tutorial-react-todo.md` from its data model to a
//! built bundle, with nothing stubbed.
//!
//! Every other test around the React framework either stops at the generated text
//! or stubs the toolchain, which is exactly the seam a real failure hides in: the
//! scaffold can be word-perfect and still not compile. So this one creates the
//! `tasks` table **through the catalog API the admin UI calls**, scaffolds the
//! app, and then runs the real `npm install` + `tsc --noEmit` + `vite build`.
//!
//! It **skips when `npm` is not on `PATH`**, so a Rust-only checkout stays green,
//! and it runs by default everywhere else — deliberately not behind an opt-in
//! variable. An opt-in test is one nobody runs, and the whole point of this one is
//! to fail when the generated project stops building.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_app::{
    ApiConfig, AppRequest, Application, CodeFramework, FrameworkRef, app_source_from_config,
    build_application, build_application_if_changed, scaffold_app,
};
use sc_catalog::{Catalog, DataField, FileStoreId, TableId};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> sc_error::Result<TempDir> {
        sc_test_harness::sweep_stale_scratch("sc-app-tutorial");
        let dir = std::env::temp_dir().join(format!(
            "sc-app-tutorial-{}-{tag}-{:?}",
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

/// Whether a Node toolchain is available to build with.
fn npm_available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join("npm").is_file() || dir.join("npm.cmd").is_file())
        })
        .unwrap_or(false)
}

/// **Tutorial step 2**, through the same calls the admin UI makes: create the
/// `tasks` table, then add `title` and `done`.
///
/// Not raw SQL, because the point is to build what an admin building it in a
/// browser would get — including whatever the catalog decides a column's type is
/// on the way back out of `information_schema`.
async fn tutorial_catalog(db: &TestDb) -> sc_error::Result<Arc<Catalog>> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);

    // The key the admin UI gives a new table: an integer that numbers itself,
    // because nothing invents an `id` and a key nobody can supply is a table no
    // form can insert into.
    let id = DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
        .generated(ColumnGenerator::Identity);
    catalog.create_table("tasks", &[id]).await?;
    catalog
        .create_field(
            "tasks",
            &DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
        )
        .await?;
    catalog
        .create_field(
            "tasks",
            &DataField::plain("done", TypeRef::Basic(BasicType::Bool)),
        )
        .await?;
    Ok(catalog)
}

/// **Tutorial step 3**: the application, as the create form posts it — a store, a
/// project name, the one table, and REST at `/api`.
fn todo_app() -> Application {
    Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"))
}

#[tokio::test]
async fn the_tutorial_app_scaffolds_installs_type_checks_builds_and_serves() -> sc_error::Result<()>
{
    if !npm_available() {
        eprintln!("skipping: `npm` is not on PATH, so the tutorial build cannot run");
        return Ok(());
    }

    let db = TestDb::new().await?;
    let cat = tutorial_catalog(&db).await?;
    let tmp = TempDir::new("build")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // Step 3: creating the app scaffolds its project.
    let app = todo_app();
    let scaffold = scaffold_app(&cat, &app, None).await?;
    assert!(scaffold.files.iter().any(|f| f.ends_with("package.json")));

    // Step 4: Build. Installs dependencies, regenerates the runtime, type-checks
    // and bundles. A failure here carries the tool's own diagnostics, which is
    // what makes this test worth having: `tsc` names the file and line in the
    // *generated* code that no longer compiles.
    let source = app_source_from_config(&app.framework)?;
    let report = build_application(&cat, &app, &source, None).await?;
    assert!(report.installed, "the first build installs dependencies");

    // Step 5: what is served is a real Vite bundle — an entry point plus hashed
    // assets, not the scaffold's own `index.html` copied through.
    let fw = CodeFramework::new("react", report.bundle.clone());
    let index = fw.serve(&AppRequest::get("/"));
    assert_eq!(index.status, 200);
    let html = String::from_utf8_lossy(&index.body);
    assert!(html.contains("<div id=\"root\">"), "{html}");
    assert!(
        html.contains("<script type=\"module\""),
        "the bundler's entry script should be in the served index: {html}"
    );
    assert!(!html.contains("/src/main.tsx"), "unbundled source: {html}");

    // Deep links resolve to the entry point, so the app's own routes work.
    assert_eq!(fw.serve(&AppRequest::get("/tasks/42")).status, 200);

    // A second build reuses the installed dependencies.
    let again = build_application(&cat, &app, &source, None).await?;
    assert!(!again.installed);

    // The boot path builds once and then reuses. The scaffold made the project
    // a git repository and committed nothing, so this is the key of an entirely
    // untracked tree: the regenerated runtime must come out byte-identical, the
    // scaffold's `.gitignore` must keep `node_modules` and `dist` out, and the
    // build must write nothing else into the project.
    if tmp.path().join("todo").join(".git").is_dir() {
        let first = build_application_if_changed(&cat, &app, &source, None).await?;
        assert!(!first.reused, "no stamp yet");
        let second = build_application_if_changed(&cat, &app, &source, None).await?;
        assert!(second.reused, "an unchanged project is not rebuilt");
        assert_eq!(second.bundle.len(), report.bundle.len());
    }
    Ok(())
}

/// **The IDE's Problems panel, from the server's end** (design §12.1).
///
/// The IDE has no type-checker of its own until the language server lands, so a
/// type error reaches the admin only if a failed build's error carries where it
/// is. `tsc` writes that to stdout while npm reports the failure on stderr, and
/// the browser parses `src/App.tsx(12,15): error TS…` out of the message — so
/// this test puts a deliberate type error in the scaffolded `src/App.tsx` and
/// asserts the position survives the whole path, through npm and out of the API.
#[tokio::test]
async fn a_type_error_fails_the_build_naming_the_file_and_the_line() -> sc_error::Result<()> {
    if !npm_available() {
        eprintln!("skipping: `npm` is not on PATH, so the tutorial build cannot run");
        return Ok(());
    }

    let db = TestDb::new().await?;
    let cat = tutorial_catalog(&db).await?;
    let tmp = TempDir::new("typeerror")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    let app = todo_app();
    scaffold_app(&cat, &app, None).await?;

    // What an admin does in the editor and gets wrong: a string where the
    // component wants a number. Appended, so the line it is on is known.
    let app_tsx = tmp.path().join("todo/src/App.tsx");
    let source = std::fs::read_to_string(&app_tsx)?;
    let bad_line = source.lines().count() + 2;
    // Exported, so the only thing wrong with it is its type.
    std::fs::write(
        &app_tsx,
        format!("{source}\nexport const pageSize: number = \"twenty\";\n"),
    )?;

    let err = build_application(&cat, &app, &app_source_from_config(&app.framework)?, None)
        .await
        .expect_err("a type error must fail the build")
        .to_string();

    // The position, in the form the IDE parses: `<file>(<line>,<col>): error TS…`.
    assert!(
        err.contains(&format!("src/App.tsx({bad_line},")),
        "the error must name the file and the line: {err}"
    );
    assert!(
        err.contains("error TS2322"),
        "and the type error itself: {err}"
    );
    Ok(())
}

/// An application that declares tables but **enables no API provider**.
///
/// Reported from a real run: the app's endpoint set is then empty, so the
/// generated client has no methods — while the hooks and the login screen call
/// them. `tsc` produced eleven errors in generated files, none of which named the
/// actual mistake (an empty APIs list in the create form), and all of which
/// pointed at code the admin never wrote.
///
/// A React app with no API cannot reach data at all, which is the whole point of
/// this framework, so the build must say *that* — early, before npm runs.
#[tokio::test]
async fn an_app_with_no_api_provider_is_told_so_not_shown_typescript_errors() -> sc_error::Result<()>
{
    let db = TestDb::new().await?;
    let cat = tutorial_catalog(&db).await?;
    let tmp = TempDir::new("noapi")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // Exactly the shape that failed: a table, a store, and no `apis` row.
    let app = Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()));

    let err = scaffold_app(&cat, &app, None)
        .await
        .expect_err("a react app with no API cannot work")
        .to_string();
    // The message names the mistake and the fix, in the admin's vocabulary.
    assert!(err.contains("API"), "{err}");
    assert!(err.contains("rest"), "should name a provider to add: {err}");
    assert!(!err.contains("TS2339"), "{err}");
    // Nothing was written: a project that cannot work is not left behind to be
    // scaffolded *around* later.
    assert!(!tmp.path().join("todo/package.json").exists());

    // The same refusal from the build path, which is where an app saved before
    // this check existed arrives.
    let source = app_source_from_config(&app.framework)?;
    let err = build_application(&cat, &app, &source, None)
        .await
        .expect_err("the build cannot succeed either")
        .to_string();
    assert!(err.contains("API"), "{err}");
    Ok(())
}

/// The same path, for the app shapes an admin produces that the tutorial does
/// not: no tables at all, a table whose rows are not addressable, and column
/// types beyond text and boolean.
///
/// These are not exotic. "Create the app, then add the tables" is an ordinary
/// order to work in, and a table without a single-column primary key is whatever
/// the admin's legacy schema happens to contain. Each generates a *different*
/// project, and a project that does not compile is a build failure with a
/// TypeScript error in code the admin never wrote — the worst error this
/// framework can produce.
#[tokio::test]
async fn projects_for_unusual_table_sets_also_build() -> sc_error::Result<()> {
    if !npm_available() {
        eprintln!("skipping: `npm` is not on PATH, so the tutorial build cannot run");
        return Ok(());
    }

    let db = TestDb::new().await?;
    let cat = tutorial_catalog(&db).await?;
    // A table with no primary key (so no row-addressed endpoints) and a spread of
    // column types: a date, a number and a nullable text.
    //
    // And beside it the `users` shape: a **UUID key with no default**, which the
    // caller issues rather than the database. Its insert type requires the `id`
    // the page must therefore supply — the scaffold that left it out did not
    // compile (TS2345), which is a failure only a real `tsc` sees.
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE events (name text not null, starts date, seats int, note text);\n\
             CREATE TABLE people (id uuid primary key, name text not null, note text)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    cat.reload().await?;

    let tmp = TempDir::new("shapes")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // (a) An application with **no tables yet** — created before its data model,
    //     which is an ordinary order to work in.
    let bare = Application::new(
        "Bare",
        "bare",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "bare"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    scaffold_app(&cat, &bare, None).await?;
    let source = app_source_from_config(&bare.framework)?;
    build_application(&cat, &bare, &source, None).await?;

    // (b) Three tables: one keyless and carrying date/int columns, and one whose
    //     key the caller issues.
    let mixed = Application::new(
        "Mixed",
        "mixed",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "mixed"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_table(TableId("events".to_owned()))
    .with_table(TableId("people".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    scaffold_app(&cat, &mixed, None).await?;
    let source = app_source_from_config(&mixed.framework)?;
    let report = build_application(&cat, &mixed, &source, None).await?;

    // Every table has a page, each reachable at its own path.
    let fw = CodeFramework::new("react", report.bundle);
    assert_eq!(fw.serve(&AppRequest::get("/events")).status, 200);
    assert_eq!(fw.serve(&AppRequest::get("/people")).status, 200);
    // The page mints the key its insert requires, rather than omitting it.
    let page = std::fs::read_to_string(tmp.path().join("mixed/src/pages/People.tsx"))?;
    assert!(page.contains("crypto.randomUUID()"), "{page}");
    Ok(())
}
