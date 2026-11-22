//! The construction tables against a real Postgres (TODO-app-constructor
//! AC1.4): they bootstrap twice, a construction keeps what is written to it,
//! a phase's status follows from its tasks, a phase with tasks stays, and a
//! deleted construction leaves no phase or task behind. Deleting a phase or a
//! construction while a task is being added, or creating a construction twice
//! at once, leaves the tables consistent. The links rows never outlive are
//! foreign keys.

use std::sync::Arc;
use std::time::Duration;

use sc_catalog::{Catalog, DataFieldKind};
use sc_construct::{
    CONSTRUCTION_RUNS_TABLE, CONSTRUCTIONS_TABLE, ConstructionStatus, ERRORS_TABLE, ErrorSource,
    FEEDBACK_TABLE, Failure, MOCKUPS_TABLE, PHASES_TABLE, Phase, PhaseStatus, Priority,
    QUESTIONS_TABLE, Requirement, RunPurpose, TASKS_TABLE, Task, TaskKind, TaskStatus,
    bootstrap_constructions, create_construction, current_build, delete_construction, delete_phase,
    errors_of, insert_construction_run, insert_phase, insert_task, latest_construction_run,
    list_constructions, load_construction, mark_running, phase_status, phases_of,
    require_construction, save_specification, set_agent_run, set_construction_status,
    set_plan_error, task_insert, tasks_of_phase,
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

#[tokio::test]
async fn the_links_that_rows_never_outlive_are_foreign_keys() -> Result<()> {
    let (_catalog, db) = setup().await?;
    // A catalog read from the database sees the keys the bootstrap made.
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let fresh = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(fresh.teardown_hook());
    let target = |table: &str, column: &str| {
        let table = fresh.get(table).unwrap().unwrap();
        match &table.field(column).unwrap().kind {
            DataFieldKind::Key { target_table, .. } => Some(target_table.clone()),
            _ => None,
        }
    };
    for (table, column, to) in [
        (PHASES_TABLE, "construction", CONSTRUCTIONS_TABLE),
        (TASKS_TABLE, "phase", PHASES_TABLE),
        (TASKS_TABLE, "feedback", FEEDBACK_TABLE),
        (FEEDBACK_TABLE, "phase", PHASES_TABLE),
        (MOCKUPS_TABLE, "construction", CONSTRUCTIONS_TABLE),
        (QUESTIONS_TABLE, "construction", CONSTRUCTIONS_TABLE),
        (CONSTRUCTION_RUNS_TABLE, "construction", CONSTRUCTIONS_TABLE),
        (ERRORS_TABLE, "construction", CONSTRUCTIONS_TABLE),
        (ERRORS_TABLE, "construction_run", CONSTRUCTION_RUNS_TABLE),
    ] {
        assert_eq!(
            target(table, column).as_deref(),
            Some(to),
            "{table}.{column}"
        );
    }
    // The log outlives what it names, so those stay plain.
    for column in ["phase", "task", "feedback", "run"] {
        assert_eq!(target(ERRORS_TABLE, column), None, "errors.{column}");
    }
    assert_eq!(target(CONSTRUCTION_RUNS_TABLE, "feedback"), None);
    // What the runs record is not stored again.
    for (table, column) in [
        (CONSTRUCTIONS_TABLE, "error"),
        (CONSTRUCTIONS_TABLE, "plan_run"),
        (FEEDBACK_TABLE, "run"),
        (FEEDBACK_TABLE, "construction"),
    ] {
        let table_def = fresh.get(table).unwrap().unwrap();
        assert!(table_def.field(column).is_none(), "{table}.{column}");
    }

    // A phase of no construction is refused.
    assert!(
        insert_phase(&fresh, &phase(Uuid::new_v4(), 1.0, "Orphan"))
            .await
            .is_err()
    );
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

#[tokio::test]
async fn the_current_error_is_the_latest_runs_and_every_error_stays_logged() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;

    // An agent run fails: its error is the current one.
    let mockups =
        insert_construction_run(&catalog, construction.id, RunPurpose::Mockups, None).await?;
    let run = Uuid::new_v4();
    let failure = Failure::new(ErrorSource::Mockups, "no mockups saved")
        .run(Some(run))
        .during(Some(mockups));
    let read = set_plan_error(&catalog, app, &failure).await?;
    assert_eq!(read.error.as_deref(), Some("no mockups saved"));

    // A runner session starts, which clears it, and fails.
    let running = mark_running(&catalog, app, Uuid::new_v4(), None).await?;
    assert_eq!(running.error, None);
    let session = current_build(&catalog, construction.id).await?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (phase, task) = (Uuid::new_v4(), Uuid::new_v4());
    let failed = set_construction_status(
        &catalog,
        app,
        ConstructionStatus::Failed,
        Some(
            &Failure::new(ErrorSource::Task, "the build is red")
                .phase(phase)
                .task(task)
                .during(Some(session)),
        ),
    )
    .await?;
    assert_eq!(failed.error.as_deref(), Some("the build is red"));
    let errors = errors_of(&catalog, construction.id).await?;
    assert_eq!(failed.error_entry, Some(errors[0].id));

    // The next run starts with no error; the log keeps both, newest first.
    insert_construction_run(&catalog, construction.id, RunPurpose::Phases, None).await?;
    assert_eq!(require_construction(&catalog, app).await?.error, None);
    let logged: Vec<(ErrorSource, &str)> = errors
        .iter()
        .map(|e| (e.source, e.message.as_str()))
        .collect();
    assert_eq!(
        logged,
        [
            (ErrorSource::Task, "the build is red"),
            (ErrorSource::Mockups, "no mockups saved"),
        ]
    );
    assert_eq!((errors[0].phase, errors[0].task), (Some(phase), Some(task)));
    assert_eq!(errors[1].run, Some(run));
    assert_eq!(errors[0].construction_run, Some(session));

    delete_construction(&catalog, app).await?;
    assert!(errors_of(&catalog, construction.id).await?.is_empty());
    assert_eq!(
        latest_construction_run(&catalog, construction.id).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn the_planning_run_is_read_from_the_runs_and_an_error_names_its_feedback() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;
    assert_eq!(construction.plan_run, None);

    // The latest `phases` run that launched is the planning run.
    for _ in 0..2 {
        let during =
            insert_construction_run(&catalog, construction.id, RunPurpose::Phases, None).await?;
        let run = Uuid::new_v4();
        set_agent_run(&catalog, during, run).await?;
        assert_eq!(
            require_construction(&catalog, app).await?.plan_run,
            Some(run)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // One that never launched, and runs for other purposes, do not count.
    let planned = require_construction(&catalog, app).await?.plan_run;
    insert_construction_run(&catalog, construction.id, RunPurpose::Phases, None).await?;
    insert_construction_run(&catalog, construction.id, RunPurpose::Build, None).await?;
    assert_eq!(require_construction(&catalog, app).await?.plan_run, planned);

    // An error planning a feedback names it.
    let feedback = Uuid::new_v4();
    let during = insert_construction_run(
        &catalog,
        construction.id,
        RunPurpose::Feedback,
        Some(feedback),
    )
    .await?;
    let failure = Failure::new(ErrorSource::Feedback, "no plan saved")
        .feedback(feedback)
        .during(Some(during));
    set_plan_error(&catalog, app, &failure).await?;
    let errors = errors_of(&catalog, construction.id).await?;
    assert_eq!(errors[0].feedback, Some(feedback));

    // Any other error names none.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let session =
        insert_construction_run(&catalog, construction.id, RunPurpose::Build, None).await?;
    let failure = Failure::new(ErrorSource::Runner, "the runner stopped").during(Some(session));
    set_plan_error(&catalog, app, &failure).await?;
    let errors = errors_of(&catalog, construction.id).await?;
    assert_eq!(errors[0].message, "the runner stopped");
    assert_eq!(errors[0].feedback, None);
    assert_eq!(errors[1].feedback, Some(feedback));
    Ok(())
}
