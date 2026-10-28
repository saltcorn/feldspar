//! Framework and module step types against a real server (TODO-checklists
//! C3.3, C3.5, C3.7):
//!
//! - `saltcorn_ui.views_render` over a restored Saltcorn 1 backup's views
//!   passes, and fails — naming the views — once a field they show is
//!   deleted. Needs the built Saltcorn UI bundle, and skips without it.
//! - A step type a module declares runs in the module's worker, reads rows
//!   and runs a core step through the host, and is refused what it did not
//!   declare. Needs `npm`, which installs the fixture module.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use sc_auth::SessionStore;
use sc_check::{
    CheckEnv, ChecklistInput, Editor, Outcome, RunStatus, StepDraft, StepRegistry, edit,
    run_checklist,
};
use sc_server::{ServerConfig, admin_handlers, install_app_requests};
use serde_json::json;

fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures")
        .join(name)
}

#[tokio::test]
async fn views_render_fails_on_a_view_whose_field_was_deleted() -> sc_error::Result<()> {
    let Some(bundle) = crate::saltcorn_ui_render::bundle_dir() else {
        eprintln!("skipping: the Saltcorn UI bundle is not built");
        return Ok(());
    };
    let server = crate::saltcorn_ui_render::setup("checklist-views", bundle).await?;
    let catalog = server.catalog().clone();
    sc_check::bootstrap_checklists(&catalog).await?;
    let config = ServerConfig {
        base_domain: Some("example.com".to_owned()),
        ..ServerConfig::default()
    };
    let requests = install_app_requests(
        &config,
        &sc_api::admin_endpoints(),
        &admin_handlers(catalog.clone(), server.apps.clone()),
        &Arc::new(SessionStore::default()),
        &server.apps,
    )?
    .expect("a base domain and agents, so requests are sent");
    let admin = sc_auth::load_user_by_email(&catalog, "admin@example.com")
        .await?
        .expect("the first user");
    let app = sc_app::load_application_by_subdomain(&catalog, "booksdb")
        .await?
        .expect("the restored application");

    let registry = StepRegistry::installed();
    let mut input = ChecklistInput::new("BooksDB views render");
    input.application = Some(app.id.0);
    input.run_as = Some(admin.id);
    let checklist = edit::create_checklist(
        &catalog,
        &registry,
        &Editor::Human(admin.id),
        input,
        vec![StepDraft::new("saltcorn_ui.views_render").label("Every view renders")],
    )
    .await?;
    let env = CheckEnv::new(&catalog, &registry).with_requests(Some(requests.as_ref()));

    let run = run_checklist(&env, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    assert!(
        run.steps[0].message.contains("views rendered"),
        "{}",
        run.steps[0].message
    );

    // `pages` is shown by List, Show, Edit and Filter Books. Gone from the
    // table, v1's patterns still answer 200 — an empty cell, a missing column
    // — and the step names each view that shows it.
    catalog.drop_field("Books", "pages").await?;
    let run = run_checklist(&env, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Failed, "{:#?}", run.steps);
    let message = &run.steps[0].message;
    assert_eq!(run.steps[0].outcome, Outcome::Fail);
    assert!(message.starts_with("4 of 9 views"), "{message}");
    for view in ["List Books", "Show Books", "Edit Books", "Filter books"] {
        assert!(message.contains(&format!("`{view}`")), "{view}: {message}");
    }
    assert!(message.contains("no longer has: `pages`"), "{message}");
    assert!(!message.contains("`List Authors`"), "{message}");
    Ok(())
}

#[tokio::test]
async fn a_module_step_type_runs_in_its_worker_through_host_calls() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    // This test reads the step types its server installed: no other server
    // in this binary may install its own set meanwhile.
    let _registries = crate::module_registries().write().await;
    let mut server = crate::module_routes::setup_unlocked().await?;
    let installed = server
        .admin
        .send(
            "POST",
            "/api/modules",
            &[],
            Some(json!({
                "source": "local",
                "location": fixture("check-step-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(installed.status, StatusCode::CREATED, "{}", installed.text);
    let card = installed.json();
    assert_eq!(card["loaded"], json!(true), "{card}");
    assert!(card["issues"].as_array().unwrap().is_empty(), "{card}");

    let catalog = server.catalog.clone();
    sc_check::bootstrap_checklists(&catalog).await?;
    let registry = StepRegistry::installed();
    let declared = registry
        .get("checkmod.enough_rows")
        .expect("the module's step type is installed");
    assert!(declared.needs().data && !declared.needs().browser);
    assert_eq!(declared.config_spec().len(), 2);
    // Offered to a data-only checklist, like any type that needs no application.
    assert!(
        registry
            .offered(None)
            .iter()
            .any(|t| t.name() == "checkmod.expect_title")
    );

    catalog
        .create_table(
            "notes",
            &[sc_catalog::DataField::plain(
                "id",
                sc_types::TypeRef::Basic(sc_types::BasicType::Int),
            )
            .required()
            .primary_key()
            .generated(sc_db::ColumnGenerator::Identity)],
        )
        .await?;
    catalog
        .create_field(
            "notes",
            &sc_catalog::DataField::plain(
                "title",
                sc_types::TypeRef::Basic(sc_types::BasicType::Text),
            ),
        )
        .await?;
    let notes = catalog.get("notes")?.expect("the table");
    for title in ["first", "second"] {
        sc_api::rows::create_row(&catalog, &notes, &json!({ "title": title })).await?;
    }
    let admin = sc_auth::load_user_by_email(&catalog, "admin@example.com")
        .await?
        .expect("the first user");
    let human = Editor::Human(admin.id);
    let env = CheckEnv::new(&catalog, &registry);
    let checklist = |name: &str, steps: Vec<StepDraft>| {
        let mut input = ChecklistInput::new(name);
        input.run_as = Some(admin.id);
        edit::create_checklist(&catalog, &registry, &human, input, steps)
    };

    // Reads rows and runs a core step, both as the checklist's user.
    let good = checklist(
        "module steps",
        vec![
            StepDraft::new("checkmod.enough_rows")
                .param("table", "notes")
                .param("at_least", 2)
                .capture("counted"),
            StepDraft::new("checkmod.expect_title").param("title", "second"),
        ],
    )
    .await?;
    assert!(!good.mutates);
    let run = run_checklist(&env, &good, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    assert!(
        run.steps[0]
            .message
            .contains(&format!("2 rows of notes for run {}", run.id)),
        "{}",
        run.steps[0].message
    );
    assert_eq!(run.steps[0].captured, Some(json!({ "count": 2 })));
    assert!(
        run.steps[1].message.starts_with("core said:"),
        "{}",
        run.steps[1].message
    );

    // Asking for one more than there are fails the check, as the module says.
    let short = checklist(
        "too few",
        vec![
            StepDraft::new("checkmod.enough_rows")
                .param("table", "notes")
                .param("at_least", 3),
        ],
    )
    .await?;
    let run = run_checklist(&env, &short, "manual").await?;
    assert_eq!(run.status, RunStatus::Failed, "{:#?}", run.steps);

    // A write it did not declare, and a browser it did not ask for, are refused
    // by the host — and nothing was written.
    let sneaky = checklist("sneaky", vec![StepDraft::new("checkmod.writes_quietly")]).await?;
    let run = run_checklist(&env, &sneaky, "manual").await?;
    assert_eq!(run.steps[0].outcome, Outcome::Error, "{:#?}", run.steps);
    assert!(
        run.steps[0].message.contains("`mutates`"),
        "{}",
        run.steps[0].message
    );
    let peeks = checklist("peeks", vec![StepDraft::new("checkmod.peeks")]).await?;
    let run = run_checklist(&env, &peeks, "manual").await?;
    assert_eq!(run.steps[0].outcome, Outcome::Error, "{:#?}", run.steps);
    assert!(
        run.steps[0]
            .message
            .contains("`browser` capability without declaring it"),
        "{}",
        run.steps[0].message
    );
    let rows = sc_api::rows::list_rows(&catalog, &notes).await?;
    assert_eq!(rows.as_array().map(Vec::len), Some(2), "{rows}");
    Ok(())
}
