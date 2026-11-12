//! Saving a construction's phases (TODO-app-constructor AC2.1, AC2.3, AC2.4)
//! against a real Postgres: a list arrives as rows in its order and makes the
//! construction `planned`; a phase named by id is updated in place and one
//! left out is deleted, unless it has tasks; an invalid list is refused whole,
//! with the reason. A task inserted while the phases are saved is either seen
//! by the save, which keeps its phase, or refused because its phase is gone.

use std::sync::Arc;
use std::time::Duration;

use sc_catalog::Catalog;
use sc_construct::{
    ConstructionStatus, PHASES_TABLE, PhaseInput, PhaseStatus, Priority, Requirement, Task,
    TaskKind, bootstrap_constructions, create_construction, insert_task, parse_phases, phase_list,
    phases_of, require_construction, save_phases, task_insert, tasks_of_phase,
};
use sc_db::{DatabaseDriver, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Delete, Expr, Select, Source, Statement};
use sc_test_harness::TestDb;
use serde_json::json;
use uuid::Uuid;

async fn setup() -> Result<(Arc<Catalog>, TestDb, Uuid)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    bootstrap_constructions(&catalog).await?;
    let app = Uuid::new_v4();
    create_construction(&catalog, app, None).await?;
    Ok((catalog, db, app))
}

fn input(title: &str) -> PhaseInput {
    PhaseInput {
        id: None,
        title: title.to_owned(),
        description: format!("  {title}, described  "),
        requirements: vec![Requirement {
            text: format!("{title} works"),
            priority: Priority::Should,
        }],
    }
}

#[tokio::test]
async fn a_phase_list_arrives_as_rows_and_is_replaced_by_the_next() -> Result<()> {
    let (catalog, _db, app) = setup().await?;
    let saved = save_phases(
        &catalog,
        app,
        &[input("Data model"), input("Customers"), input("Invoices")],
    )
    .await?;
    assert_eq!(saved.construction.status, ConstructionStatus::Planned);
    let titles: Vec<&str> = saved
        .phases
        .iter()
        .map(|p| p.phase.title.as_str())
        .collect();
    assert_eq!(titles, ["Data model", "Customers", "Invoices"]);
    assert!(
        saved
            .phases
            .iter()
            .all(|p| p.status() == PhaseStatus::Unplanned)
    );
    // Trimmed on the way in.
    assert_eq!(saved.phases[0].phase.description, "Data model, described");
    let list = phase_list(&saved.phases);
    assert!(list.starts_with("1. Data model [unplanned] (id "), "{list}");
    assert!(list.contains("   - (should) Customers works"), "{list}");

    // Reordered and retitled by id; `Invoices` left out, so deleted; one new.
    let data = saved.phases[0].phase.id;
    let customers = saved.phases[1].phase.id;
    let mut renamed = input("Customers and contacts");
    renamed.id = Some(customers);
    let mut first = input("Data model");
    first.id = Some(data);
    let saved = save_phases(&catalog, app, &[renamed, first, input("Reports")]).await?;
    let rows = phases_of(&catalog, saved.construction.id).await?;
    let got: Vec<(Uuid, &str)> = rows.iter().map(|p| (p.id, p.title.as_str())).collect();
    assert_eq!(got[0], (customers, "Customers and contacts"));
    assert_eq!(got[1], (data, "Data model"));
    assert_eq!(got[2].1, "Reports");
    assert_eq!(rows.len(), 3);
    assert!(saved.kept.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_phase_with_tasks_is_kept_when_the_list_leaves_it_out() -> Result<()> {
    let (catalog, _db, app) = setup().await?;
    let saved = save_phases(&catalog, app, &[input("Data model"), input("Customers")]).await?;
    let started = saved.phases[0].phase.id;
    insert_task(
        &catalog,
        &Task::new(started, 1.0, TaskKind::DataModel, "Tables", "Create them"),
    )
    .await?;

    let saved = save_phases(&catalog, app, &[input("Customers"), input("Invoices")]).await?;
    assert_eq!(saved.kept, ["Data model"]);
    let titles: Vec<&str> = saved
        .phases
        .iter()
        .map(|p| p.phase.title.as_str())
        .collect();
    // Kept ahead of the list: it was reached first.
    assert_eq!(titles, ["Data model", "Customers", "Invoices"]);
    assert_eq!(saved.phases[0].phase.id, started);
    assert_eq!(saved.phases[0].status(), PhaseStatus::Planned);
    Ok(())
}

#[tokio::test]
async fn an_invalid_list_is_refused_whole_with_the_reason() -> Result<()> {
    let (catalog, _db, app) = setup().await?;
    let saved = save_phases(&catalog, app, &[input("Data model")]).await?;
    let construction = saved.construction.id;

    let refused = |list: Vec<PhaseInput>, needle: &'static str| {
        let catalog = catalog.clone();
        async move {
            let err = save_phases(&catalog, app, &list).await.unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
    };
    refused(vec![], "at least one phase").await;
    refused(vec![input("Fine"), input("  ")], "phase 2 has no title").await;
    let mut stranger = input("Elsewhere");
    stranger.id = Some(Uuid::new_v4());
    refused(vec![stranger], "not a phase of this construction").await;
    let mut empty = input("Requirement-less");
    empty.requirements[0].text = " ".to_owned();
    refused(vec![empty], "requirement 1 has no text").await;
    refused(
        (0..13).map(|i| input(&format!("P{i}"))).collect(),
        "too many",
    )
    .await;

    // Nothing of a refused list was written.
    let rows = phases_of(&catalog, construction).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Data model");

    // The JSON form names the bad phase and field.
    let err =
        parse_phases(&json!([{ "title": "A" }, { "title": "B", "priority": 1 }])).unwrap_err();
    assert!(err.to_string().contains("phase 2"), "{err}");
    let err = parse_phases(
        &json!([{ "title": "A", "requirements": [{ "text": "x", "priority": "maybe" }] }]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("phase 1"), "{err}");
    assert!(parse_phases(&json!({ "title": "A" })).is_err());

    // The construction is still `planned`.
    assert_eq!(
        require_construction(&catalog, app).await?.status,
        ConstructionStatus::Planned
    );
    Ok(())
}

/// A transaction that holds `phase` locked, as a save of the phases or a task
/// insert does while it decides.
async fn lock_phase(db: &TestDb, phase: Uuid) -> Result<Box<dyn Transaction>> {
    let driver = PgDriver::from_pool(db.pool().clone());
    let mut tx = driver.begin().await?;
    let lock = Select::from(Source::table(PHASES_TABLE))
        .filter(Expr::col("id").eq(Expr::lit(phase)))
        .for_update();
    tx.query(&Statement::from(lock))
        .await?
        .try_collect()
        .await?;
    Ok(tx)
}

fn tables_task(phase: Uuid) -> Task {
    Task::new(phase, 1.0, TaskKind::DataModel, "Tables", "Create them")
}

#[tokio::test]
async fn a_task_inserted_while_the_phases_are_saved_is_seen_by_the_save() -> Result<()> {
    let (catalog, db, app) = setup().await?;
    let saved = save_phases(&catalog, app, &[input("Data model")]).await?;
    let phase = saved.phases[0].phase.id;

    // A task is being inserted for the phase: it holds the phase locked.
    let mut tx = lock_phase(&db, phase).await?;
    let saving = tokio::spawn({
        let catalog = catalog.clone();
        async move { save_phases(&catalog, app, &[input("Customers")]).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !saving.is_finished(),
        "the save waits for the task's insert"
    );
    tx.query(&Statement::from(task_insert(&tables_task(phase))?))
        .await?
        .try_collect()
        .await?;
    tx.commit().await?;

    // The save then sees the task, so the phase stays.
    let saved = saving.await.expect("the save ran")?;
    assert_eq!(saved.kept, ["Data model"]);
    assert_eq!(tasks_of_phase(&catalog, phase).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_task_for_a_phase_the_save_deletes_waits_and_is_refused() -> Result<()> {
    let (catalog, db, app) = setup().await?;
    let saved = save_phases(&catalog, app, &[input("Data model")]).await?;
    let phase = saved.phases[0].phase.id;

    // A save of the phases holds the phase locked, then deletes it.
    let mut tx = lock_phase(&db, phase).await?;
    let inserting = tokio::spawn({
        let catalog = catalog.clone();
        async move { insert_task(&catalog, &tables_task(phase)).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!inserting.is_finished(), "the insert waits for the save");
    let delete = Delete::from(PHASES_TABLE).filter(Expr::col("id").eq(Expr::lit(phase)));
    tx.query(&Statement::from(delete))
        .await?
        .try_collect()
        .await?;
    tx.commit().await?;

    // The insert then finds no phase, and no task is left without one.
    let err = inserting.await.expect("the insert ran").unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
    assert!(tasks_of_phase(&catalog, phase).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn while_running_a_phase_with_tasks_keeps_them_and_one_without_is_planned() -> Result<()> {
    let (catalog, _db, app) = setup().await?;
    save_phases(&catalog, app, &[input("Data model"), input("Screens")]).await?;
    let phases = phases_of(&catalog, require_construction(&catalog, app).await?.id).await?;
    let task = |title: &str| sc_construct::TaskInput {
        kind: TaskKind::DataModel,
        title: title.to_owned(),
        brief: format!("make {title}"),
        depends_on: Vec::new(),
    };
    sc_construct::save_tasks(&catalog, app, phases[0].id, &[task("Tables")]).await?;
    sc_construct::set_construction_status(&catalog, app, ConstructionStatus::Running, None).await?;

    // The runner may have picked `Tables`: replacing it is refused.
    let err = sc_construct::save_tasks(&catalog, app, phases[0].id, &[task("Other")])
        .await
        .expect_err("refused while running");
    assert!(
        err.to_string().contains("stop it to plan them again"),
        "{err}"
    );
    assert_eq!(
        tasks_of_phase(&catalog, phases[0].id).await?[0].title,
        "Tables"
    );

    // The runner planning the phase it has reached is what running allows.
    sc_construct::save_tasks(&catalog, app, phases[1].id, &[task("List")]).await?;
    assert_eq!(tasks_of_phase(&catalog, phases[1].id).await?.len(), 1);
    Ok(())
}
