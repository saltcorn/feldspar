//! The construction tables against a real Postgres (TODO-app-constructor
//! AC1.4): they bootstrap twice, a construction keeps what is written to it,
//! a phase's status follows from its tasks, a phase with tasks stays, and a
//! deleted construction leaves no phase or task behind. Deleting a phase or a
//! construction while a task is being added, or creating a construction twice
//! at once, leaves the tables consistent.

use std::sync::Arc;
use std::time::Duration;

use sc_catalog::Catalog;
use sc_construct::{
    ConstructionStatus, Phase, PhaseStatus, Priority, Requirement, Task, TaskKind, TaskStatus,
    bootstrap_constructions, create_construction, delete_construction, delete_phase, insert_phase,
    insert_task, list_constructions, load_construction, phase_status, phases_of,
    save_specification, task_insert, tasks_of_phase,
};
use sc_db::{DatabaseDriver, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Expr, Select, Source, Statement};
use sc_test_harness::TestDb;
use uuid::Uuid;

async fn setup() -> Result<(Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    bootstrap_constructions(&catalog).await?;
    Ok((catalog, db))
}

fn phase(construction: Uuid, position: f64, title: &str) -> Phase {
    Phase {
        id: Uuid::new_v4(),
        construction,
        position,
        title: title.to_owned(),
        description: format!("{title}, described"),
        requirements: vec![Requirement {
            text: "Customers can be listed".to_owned(),
            priority: Priority::Must,
        }],
    }
}

#[tokio::test]
async fn a_construction_keeps_what_is_written_to_it() -> Result<()> {
    let (catalog, _db) = setup().await?;
    // A second bootstrap, as every start makes, changes nothing.
    bootstrap_constructions(&catalog).await?;

    let app = Uuid::new_v4();
    assert!(load_construction(&catalog, app).await?.is_none());
    let created = create_construction(&catalog, app, Some("construct-crm")).await?;
    assert_eq!(created.status, ConstructionStatus::Draft);
    assert_eq!(created.agent.as_deref(), Some("construct-crm"));
    assert_eq!(created.specification, "");

    // One per application.
    let err = create_construction(&catalog, app, None).await.unwrap_err();
    assert!(
        err.to_string().contains("already has a construction"),
        "{err}"
    );

    let saved = save_specification(&catalog, app, "A CRM for a small agency.").await?;
    assert_eq!(saved.specification, "A CRM for a small agency.");
    assert_eq!(saved.id, created.id);
    assert_eq!(list_constructions(&catalog).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn phases_and_tasks_come_back_in_order_and_a_phase_status_follows_its_tasks() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let construction = create_construction(&catalog, Uuid::new_v4(), None).await?;
    let second = phase(construction.id, 2.0, "Contacts");
    let first = phase(construction.id, 1.0, "Data model");
    insert_phase(&catalog, &second).await?;
    insert_phase(&catalog, &first).await?;
    let phases = phases_of(&catalog, construction.id).await?;
    assert_eq!(
        phases.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(),
        ["Data model", "Contacts"]
    );
    assert_eq!(phases[0], first);

    // No tasks: not planned yet.
    assert_eq!(
        phase_status(&tasks_of_phase(&catalog, first.id).await?),
        PhaseStatus::Unplanned
    );

    let tables = Task::new(
        first.id,
        1.0,
        TaskKind::DataModel,
        "Tables",
        "Make the tables",
    );
    let mut queries = Task::new(first.id, 2.0, TaskKind::DataModel, "Queries", "Add queries");
    queries.depends_on = vec![tables.id];
    insert_task(&catalog, &queries).await?;
    insert_task(&catalog, &tables).await?;
    let mut tasks = tasks_of_phase(&catalog, first.id).await?;
    assert_eq!(tasks[0], tables);
    assert_eq!(tasks[1].depends_on, vec![tables.id]);
    assert_eq!(phase_status(&tasks), PhaseStatus::Planned);

    tasks[0].status = TaskStatus::Running;
    assert_eq!(phase_status(&tasks), PhaseStatus::Running);
    tasks[0].status = TaskStatus::Failed;
    assert_eq!(phase_status(&tasks), PhaseStatus::Failed);
    tasks[0].status = TaskStatus::Done;
    tasks[1].status = TaskStatus::Skipped;
    assert_eq!(phase_status(&tasks), PhaseStatus::Done);

    // A phase with tasks stays; one without goes.
    let err = delete_phase(&catalog, first.id).await.unwrap_err();
    assert!(err.to_string().contains("has tasks"), "{err}");
    assert!(delete_phase(&catalog, second.id).await?);
    assert_eq!(phases_of(&catalog, construction.id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn deleting_a_construction_takes_its_phases_and_tasks() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let other = create_construction(&catalog, Uuid::new_v4(), None).await?;
    let construction = create_construction(&catalog, app, None).await?;
    let mine = phase(construction.id, 1.0, "Data model");
    let theirs = phase(other.id, 1.0, "Data model");
    insert_phase(&catalog, &mine).await?;
    insert_phase(&catalog, &theirs).await?;
    insert_task(
        &catalog,
        &Task::new(mine.id, 1.0, TaskKind::DataModel, "T", "B"),
    )
    .await?;
    insert_task(
        &catalog,
        &Task::new(theirs.id, 1.0, TaskKind::Feature, "T", "B"),
    )
    .await?;

    let deleted = delete_construction(&catalog, app).await?.unwrap();
    assert_eq!(deleted.id, construction.id);
    assert!(load_construction(&catalog, app).await?.is_none());
    assert!(phases_of(&catalog, construction.id).await?.is_empty());
    assert!(tasks_of_phase(&catalog, mine.id).await?.is_empty());
    // Another application's construction is untouched.
    assert_eq!(phases_of(&catalog, other.id).await?.len(), 1);
    assert_eq!(tasks_of_phase(&catalog, theirs.id).await?.len(), 1);
    // A second delete finds nothing.
    assert!(delete_construction(&catalog, app).await?.is_none());
    Ok(())
}

/// A transaction that holds `phase` locked and adds a task to it, as
/// `insert_task` does, but commits only when the test says so.
async fn adding_a_task(db: &TestDb, phase: Uuid) -> Result<Box<dyn Transaction>> {
    let driver = PgDriver::from_pool(db.pool().clone());
    let mut tx = driver.begin().await?;
    let lock = Select::from(Source::table(sc_construct::PHASES_TABLE))
        .filter(Expr::col("id").eq(Expr::lit(phase)))
        .for_update();
    tx.query(&Statement::from(lock))
        .await?
        .try_collect()
        .await?;
    let task = Task::new(phase, 1.0, TaskKind::DataModel, "T", "B");
    tx.query(&Statement::from(task_insert(&task)?))
        .await?
        .try_collect()
        .await?;
    Ok(tx)
}

#[tokio::test]
async fn deleting_a_construction_waits_for_a_task_being_added_and_takes_it() -> Result<()> {
    let (catalog, db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;
    let first = phase(construction.id, 1.0, "Data model");
    insert_phase(&catalog, &first).await?;

    let tx = adding_a_task(&db, first.id).await?;
    let deleting = sc_catalog::spawn_scoped({
        let catalog = catalog.clone();
        async move { delete_construction(&catalog, app).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!deleting.is_finished(), "the delete waits for the task");
    tx.commit().await?;

    assert!(deleting.await.expect("the delete ran")?.is_some());
    // The task committed first, and went with its phase.
    assert!(tasks_of_phase(&catalog, first.id).await?.is_empty());
    assert!(phases_of(&catalog, construction.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn deleting_a_phase_waits_for_a_task_being_added_and_is_then_refused() -> Result<()> {
    let (catalog, db) = setup().await?;
    let construction = create_construction(&catalog, Uuid::new_v4(), None).await?;
    let first = phase(construction.id, 1.0, "Data model");
    insert_phase(&catalog, &first).await?;

    let tx = adding_a_task(&db, first.id).await?;
    let deleting = sc_catalog::spawn_scoped({
        let catalog = catalog.clone();
        async move { delete_phase(&catalog, first.id).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!deleting.is_finished(), "the delete waits for the task");
    tx.commit().await?;

    let err = deleting.await.expect("the delete ran").unwrap_err();
    assert!(err.to_string().contains("has tasks"), "{err}");
    assert_eq!(phases_of(&catalog, construction.id).await?.len(), 1);
    // A phase that is not there answers `false`.
    assert!(!delete_phase(&catalog, Uuid::new_v4()).await?);
    Ok(())
}

#[tokio::test]
async fn two_creates_at_once_make_one_construction_and_one_clear_refusal() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let (a, b) = tokio::join!(
        create_construction(&catalog, app, None),
        create_construction(&catalog, app, None)
    );
    let (made, refused): (Vec<_>, Vec<_>) = [a, b].into_iter().partition(Result::is_ok);
    assert_eq!(made.len(), 1);
    let err = refused.into_iter().next().unwrap().unwrap_err();
    assert!(
        err.to_string().contains("already has a construction"),
        "{err}"
    );
    assert_eq!(list_constructions(&catalog).await?.len(), 1);
    Ok(())
}
