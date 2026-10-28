//! Framework step types and starter checklists (TODO-checklists C3.1, C3.2,
//! C3.6, C3.7): a `react.*` step is offered and accepted only for a React
//! application; renaming a field breaks `react.typecheck` on a scaffolded
//! React app; and a framework's starter checklist is offered, created on
//! request, and not offered again.
//!
//! The type check runs the real `npm install` and `tsc`, and **skips when `npm`
//! is not on `PATH`**, as `sc-app`'s tutorial build does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_app::{ApiConfig, Application, FrameworkRef, save_application, scaffold_app};
use sc_auth::{ROLE_ADMIN, User, create_user};
use sc_catalog::{Catalog, DataField, FileStoreId, TableId};
use sc_check::{
    CheckEnv, ChecklistInput, Editor, Outcome, RunStatus, StepDraft, StepRegistry, edit,
    run_checklist,
};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir =
            std::env::temp_dir().join(format!("sc-check-framework-{tag}-{}", std::process::id()));
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

fn npm_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("npm").is_file()))
}

struct Env {
    catalog: Arc<Catalog>,
    registry: StepRegistry,
    admin: User,
    db: TestDb,
    _tmp: TempDir,
}

/// A catalog with the tutorial's `tasks` table (`id`, `title`, `done`) and an
/// `apps` file store.
async fn setup(tag: &str) -> Result<Env> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_check::bootstrap_checklists(&catalog).await?;
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
    let tmp = TempDir::new(tag);
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    let admin = create_user(&catalog, "admin@example.com", "pw-admin", ROLE_ADMIN).await?;
    Ok(Env {
        catalog,
        registry: StepRegistry::builtin(),
        admin,
        db,
        _tmp: tmp,
    })
}

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

fn site_app() -> Application {
    Application::new(
        "Site",
        "site",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "site")
            .with("output", "site/dist")
            .with("command", "sh build.sh"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
}

impl Env {
    fn human(&self) -> Editor {
        Editor::Human(self.admin.id)
    }

    async fn checklist(
        &self,
        name: &str,
        app: &Application,
        steps: Vec<StepDraft>,
    ) -> Result<sc_check::Checklist> {
        let mut input = ChecklistInput::new(name);
        input.application = Some(app.id.0);
        edit::create_checklist(&self.catalog, &self.registry, &self.human(), input, steps).await
    }
}

#[tokio::test]
async fn a_frameworks_step_is_accepted_only_on_its_frameworks_checklist() -> Result<()> {
    let env = setup("scope").await?;
    let react = save_application(&env.catalog, &todo_app()).await?;
    let site = save_application(&env.catalog, &site_app()).await?;

    let err = env
        .checklist("site types", &site, vec![StepDraft::new("react.typecheck")])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`react` framework"), "{err}");
    assert!(err.contains("`code` application"), "{err}");

    // A data-only checklist cannot have one either.
    let err = edit::create_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        ChecklistInput::new("no app"),
        vec![StepDraft::new("react.no_error_boundary")],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("checks no application") || err.contains("has none"),
        "{err}"
    );

    // On the React app's checklist it is fine — and moving that checklist to
    // the `code` app is refused while the step is on it.
    let checklist = env
        .checklist(
            "todo types",
            &react,
            vec![StepDraft::new("react.typecheck")],
        )
        .await?;
    let mut moved = ChecklistInput::new("todo types");
    moved.application = Some(site.id.0);
    let err = edit::update_checklist(
        &env.catalog,
        &env.registry,
        &env.human(),
        checklist.id,
        moved,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("`react` framework"), "{err}");
    Ok(())
}

#[tokio::test]
async fn renaming_a_field_breaks_react_typecheck_on_a_scaffolded_app() -> Result<()> {
    if !npm_available() {
        eprintln!("skipping: `npm` is not on PATH, so the type check cannot run");
        return Ok(());
    }
    let env = setup("typecheck").await?;
    let app = save_application(&env.catalog, &todo_app()).await?;
    scaffold_app(&env.catalog, &app, None).await?;
    let checklist = env
        .checklist(
            "todo compiles",
            &app,
            vec![StepDraft::new("react.typecheck").label("The app type-checks")],
        )
        .await?;
    let env_check = CheckEnv::new(&env.catalog, &env.registry);

    // As scaffolded, the pages use `title` and the client has it.
    let run = run_checklist(&env_check, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);

    // Rename the column under the app: the regenerated client calls it `name`,
    // and the scaffolded page still reads `title`.
    env.db
        .client()
        .await?
        .batch_execute("ALTER TABLE tasks RENAME COLUMN title TO name")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    env.catalog.reload().await?;
    let run = run_checklist(&env_check, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Failed, "{:#?}", run.steps);
    let step = &run.steps[0];
    assert_eq!(step.outcome, Outcome::Fail);
    assert!(step.message.contains("title"), "{}", step.message);
    // The client the check regenerated is what the page no longer matches.
    assert!(
        step.message.contains("src/pages/Tasks.tsx"),
        "{}",
        step.message
    );
    Ok(())
}

#[tokio::test]
async fn a_starter_checklist_is_offered_created_on_request_and_not_offered_again() -> Result<()> {
    let env = setup("starter").await?;
    let app = save_application(&env.catalog, &todo_app()).await?;

    let offered = sc_check::starters(&env.catalog, &app).await?;
    assert_eq!(offered.len(), 1, "{offered:?}");
    let name = offered[0].input.name.clone();
    assert_eq!(name, "Todo: builds and renders");
    // Offering creates nothing.
    assert!(
        sc_check::store::list_checklists(&env.catalog, sc_check::ChecklistScope::All)
            .await?
            .is_empty()
    );

    let created =
        sc_check::create_starter(&env.catalog, &env.registry, &env.human(), &app, &name).await?;
    assert_eq!(created.application, Some(app.id.0));
    let steps = sc_check::store::current_steps(&env.catalog, created.id).await?;
    let types: Vec<&str> = steps.iter().map(|s| s.draft.type_.as_str()).collect();
    assert_eq!(
        types,
        [
            "app.builds",
            "react.typecheck",
            "react.open_route",
            "react.no_error_boundary"
        ]
    );
    assert!(steps.iter().all(|s| s.accepted), "a person's, accepted");

    assert!(sc_check::starters(&env.catalog, &app).await?.is_empty());
    let err = sc_check::create_starter(&env.catalog, &env.registry, &env.human(), &app, &name)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no starter checklist"), "{err}");
    Ok(())
}
