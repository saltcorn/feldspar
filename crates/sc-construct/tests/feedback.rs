//! The admin's feedback on a built phase against a real Postgres
//! (TODO-app-constructor AC9): only a phase that built something takes it;
//! its plan adds requirements and follow-up tasks after the phase's own,
//! which the runner reaches next; it is planned once; deleting it takes its
//! tasks until one has started; deleting the construction takes it.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_construct::{
    Next, PhaseInput, PhaseStatus, Priority, Requirement, Task, TaskKind, TaskStatus, add_feedback,
    bootstrap_constructions, create_construction, delete_construction, delete_feedback,
    feedback_of, feedback_tasks, insert_task, next_step, parse_feedback_plan, phases_with_tasks,
    plan_feedback, save_phases, update_task,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use serde_json::json;
use uuid::Uuid;

/// A construction with two phases, the first built (one task done), the
/// second not planned yet. Answers the application and the two phase ids.
async fn built() -> Result<(Arc<Catalog>, TestDb, Uuid, Uuid, Uuid)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    bootstrap_constructions(&catalog).await?;
    let app = Uuid::new_v4();
    create_construction(&catalog, app, None).await?;
    let input = |title: &str| PhaseInput {
        id: None,
        title: title.to_owned(),
        description: format!("{title}, described"),
        requirements: vec![Requirement {
            text: format!("{title} works"),
            priority: Priority::Must,
        }],
    };
    let saved = save_phases(&catalog, app, &[input("Contacts"), input("Reports")]).await?;
    let (first, second) = (saved.phases[0].phase.id, saved.phases[1].phase.id);
    let mut task = Task::new(first, 1.0, TaskKind::Feature, "Contact list", "List them");
    insert_task(&catalog, &task).await?;
    task.status = TaskStatus::Done;
    update_task(&catalog, &task).await?;
    Ok((catalog, db, app, first, second))
}

fn plan() -> sc_construct::FeedbackPlan {
    parse_feedback_plan(&json!({
        "requirements": [{"text": "The list is sorted by name", "priority": "must"}],
        "tasks": [
            {"kind": "feature", "title": "Sort the list", "brief": "Sort the contact list by name"},
            {"kind": "feature", "title": "Remember the order", "brief": "Keep it", "depends_on": [1]},
        ],
    }))
    .unwrap()
}

#[tokio::test]
async fn feedback_plans_follow_up_tasks_after_the_phases_own() -> Result<()> {
    let (catalog, _db, app, first, second) = built().await?;

    // Feedback is on what a phase built.
    let err = add_feedback(&catalog, app, second, "Charts", "Add charts")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not built anything"), "{err}");
    let err = add_feedback(&catalog, app, first, " ", "x")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no title"), "{err}");

    let feedback = add_feedback(&catalog, app, first, " Sorting ", " Sort by name ").await?;
    assert_eq!(
        (feedback.title.as_str(), feedback.description.as_str()),
        ("Sorting", "Sort by name")
    );

    let saved = plan_feedback(&catalog, app, feedback.id, &plan()).await?;
    assert_eq!(saved.len(), 2);
    let construction = sc_construct::require_construction(&catalog, app).await?;
    let phases = phases_with_tasks(&catalog, construction.id).await?;
    let tasks = &phases[0].tasks;
    // After the phase's own task, in order, naming the feedback and waiting.
    let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Contact list", "Sort the list", "Remember the order"]
    );
    assert_eq!(tasks[0].feedback, None);
    assert!(tasks[1..].iter().all(|t| t.feedback == Some(feedback.id)));
    assert_eq!(tasks[2].depends_on, vec![tasks[1].id]);
    assert_eq!(phases[0].status(), PhaseStatus::Planned);
    assert_eq!(feedback_tasks(&phases, feedback.id).len(), 2);
    let stored = &feedback_of(&catalog, construction.id).await?[0];
    assert_eq!(stored.requirements[0].text, "The list is sorted by name");

    // The runner reaches them before the next phase.
    assert_eq!(next_step(&phases, None), Next::RunTask(tasks[1].clone()));

    // Planned once.
    let err = plan_feedback(&catalog, app, feedback.id, &plan())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("planned already"), "{err}");
    Ok(())
}

#[tokio::test]
async fn feedback_is_deleted_with_its_tasks_until_one_has_started() -> Result<()> {
    let (catalog, _db, app, first, _) = built().await?;
    let construction = sc_construct::require_construction(&catalog, app).await?;

    let feedback = add_feedback(&catalog, app, first, "Sorting", "Sort by name").await?;
    plan_feedback(&catalog, app, feedback.id, &plan()).await?;
    delete_feedback(&catalog, app, feedback.id).await?;
    assert!(feedback_of(&catalog, construction.id).await?.is_empty());
    let phases = phases_with_tasks(&catalog, construction.id).await?;
    assert_eq!(phases[0].tasks.len(), 1);
    assert_eq!(phases[0].status(), PhaseStatus::Done);

    let feedback = add_feedback(&catalog, app, first, "Sorting", "Sort by name").await?;
    let mut started = plan_feedback(&catalog, app, feedback.id, &plan())
        .await?
        .remove(0);
    started.status = TaskStatus::Done;
    update_task(&catalog, &started).await?;
    let err = delete_feedback(&catalog, app, feedback.id)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has started"), "{err}");

    delete_construction(&catalog, app).await?;
    assert!(feedback_of(&catalog, construction.id).await?.is_empty());
    Ok(())
}
