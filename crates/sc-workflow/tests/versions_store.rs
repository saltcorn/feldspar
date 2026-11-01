//! `_fd_workflow_versions` and `_fd_run_traces` against a real Postgres.
//!
//! What is under test is what only a database can settle: that saving an edited
//! workflow **mints** a version rather than rewriting one, that an old version is
//! still loadable after two edits (which is the whole of "a suspended run
//! finishes on its own version"), that `(workflow, version)` is unique in the
//! database and not merely in the code that inserts, and that a trace row comes
//! back as the step it recorded.

use std::sync::Arc;

use sc_action::TriggerId;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_workflow::versions::{COL_STEPS, VERSIONS_TABLE};
use sc_workflow::{
    Assignment, Next, RunTrace, Step, StepKind, TraceOutcome, Workflow, bootstrap_run_traces,
    bootstrap_workflow_versions, current_workflow, delete_workflow_versions, list_run_traces,
    list_workflow_versions, load_workflow_version, require_workflow_version, save_run_trace,
    save_workflow,
};
use serde_json::json;
use uuid::Uuid;

/// A catalog over a per-test database with both of this crate's tables up.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_workflow_versions(&catalog).await?;
    bootstrap_run_traces(&catalog).await?;
    Ok(catalog)
}

/// A one-step workflow whose `Set` writes `total`.
fn workflow(id: TriggerId, formula: &str) -> Workflow {
    Workflow::of(
        id,
        1,
        vec![
            Step::new(
                "total",
                StepKind::Set {
                    assignments: vec![Assignment::new("total", formula)],
                },
            )
            .then(Next::End),
        ],
    )
}

#[tokio::test]
async fn saving_an_edited_workflow_mints_a_version_and_leaves_the_old_one_loadable() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let id = TriggerId::new();

    let first = save_workflow(&catalog, &workflow(id, "1"), "the first draft", None).await?;
    assert_eq!(first.version, 1);
    let second = save_workflow(&catalog, &workflow(id, "2"), "twice as much", None).await?;
    let third = save_workflow(&catalog, &workflow(id, "3"), "thrice", None).await?;
    assert_eq!((second.version, third.version), (2, 3));

    // The history reads newest first, and nothing was rewritten: version 1 is
    // still the workflow it was when a run started on it, two edits ago.
    let history = list_workflow_versions(&catalog, id).await?;
    assert_eq!(
        history.iter().map(|v| v.version).collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert_eq!(
        history
            .iter()
            .map(|v| v.description.as_str())
            .collect::<Vec<_>>(),
        ["thrice", "twice as much", "the first draft"]
    );
    let pinned = require_workflow_version(&catalog, id, 1).await?;
    assert_eq!(pinned, first);
    let StepKind::Set { assignments } = &pinned.steps[0].kind else {
        panic!("the pinned version is the one that was saved");
    };
    assert_eq!(assignments[0].formula, "1");

    // A new run gets the newest.
    assert_eq!(current_workflow(&catalog, id).await?, Some(third));
    Ok(())
}

#[tokio::test]
async fn a_version_that_is_not_there_is_named_rather_than_replaced_by_the_current_one() -> Result<()>
{
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let id = TriggerId::new();
    save_workflow(&catalog, &workflow(id, "1"), "", None).await?;

    // Silently advancing a pinned run on the *current* version would run steps
    // it never started, which is precisely what pinning exists to prevent.
    let err = require_workflow_version(&catalog, id, 7).await.unwrap_err();
    assert!(err.to_string().contains("has no version 7"), "{err}");
    assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)));
    assert_eq!(load_workflow_version(&catalog, id, 7).await?, None);

    // And a workflow nobody has drawn yet has no current version at all — a
    // real state, said as `None` rather than as an empty program.
    assert_eq!(current_workflow(&catalog, TriggerId::new()).await?, None);
    Ok(())
}

#[tokio::test]
async fn the_database_refuses_a_second_row_claiming_one_version() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let id = TriggerId::new();
    save_workflow(&catalog, &workflow(id, "1"), "", None).await?;

    // What two admins saving at once would produce: `save_workflow` reads the
    // maximum and inserts the next, so the *database* has to be the authority
    // for the race. This is that constraint, exercised by writing the row the
    // losing save would have written.
    let insert = Insert::row(
        VERSIONS_TABLE,
        [
            "id",
            "workflow",
            "version",
            "description",
            COL_STEPS,
            "attributes",
            "created_at",
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect::<Vec<_>>(),
        vec![
            Expr::lit(Uuid::new_v4()),
            Expr::lit(id.0),
            Expr::lit(1_i64),
            Expr::lit("a fork"),
            Expr::Lit(Value::Json(json!({
                "id": id.0,
                "version": 1,
                "start": "total",
                "steps": [],
            }))),
            Expr::Lit(Value::Json(json!({}))),
            Expr::Lit(Value::Timestamp(chrono::Utc::now())),
        ],
    );
    let err = catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&Statement::from(insert))
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("sc_uq__fd_workflow_versions_workflow_version") || err.contains("unique"),
        "the second version 1 should be refused by the database, got: {err}"
    );
    // And the history still has exactly one version 1.
    assert_eq!(list_workflow_versions(&catalog, id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_stored_version_whose_document_names_another_workflow_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let id = TriggerId::new();
    // A row somebody wrote by hand: the columns say one thing, the document
    // another. Running it under the row's identity would run one workflow's
    // steps under another's name, so it is reported rather than read.
    let insert = Insert::row(
        VERSIONS_TABLE,
        [
            "id",
            "workflow",
            "version",
            "description",
            COL_STEPS,
            "attributes",
            "created_at",
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect::<Vec<_>>(),
        vec![
            Expr::lit(Uuid::new_v4()),
            Expr::lit(id.0),
            Expr::lit(4_i64),
            Expr::lit(""),
            Expr::Lit(Value::Json(json!({
                "id": Uuid::new_v4(),
                "version": 9,
                "start": "a",
                "steps": [],
            }))),
            Expr::Lit(Value::Json(json!({}))),
            Expr::Lit(Value::Timestamp(chrono::Utc::now())),
        ],
    );
    catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;

    let err = list_workflow_versions(&catalog, id).await.unwrap_err();
    assert!(err.to_string().contains("its stored steps say"), "{err}");
    Ok(())
}

#[tokio::test]
async fn deleting_a_workflow_takes_its_history_with_it() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let id = TriggerId::new();
    let other = TriggerId::new();
    save_workflow(&catalog, &workflow(id, "1"), "", None).await?;
    save_workflow(&catalog, &workflow(id, "2"), "", None).await?;
    save_workflow(&catalog, &workflow(other, "1"), "", None).await?;

    assert_eq!(delete_workflow_versions(&catalog, id).await?, 2);
    assert!(list_workflow_versions(&catalog, id).await?.is_empty());
    // Only that workflow's.
    assert_eq!(list_workflow_versions(&catalog, other).await?.len(), 1);
    // Deleting one that is already gone is not an error, it is zero.
    assert_eq!(delete_workflow_versions(&catalog, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_trace_row_comes_back_as_the_step_it_recorded() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let run = Uuid::new_v4();
    let started = chrono::Utc::now();

    save_run_trace(
        &catalog,
        &RunTrace::new(run, 1, "fetch", started, TraceOutcome::Error)
            .attempt(1)
            .error("the supplier timed out"),
    )
    .await?;
    save_run_trace(
        &catalog,
        &RunTrace::new(run, 2, "fetch", started, TraceOutcome::Ok)
            .attempt(2)
            .context(
                [("total".to_owned(), json!(120))]
                    .into_iter()
                    .collect::<sc_types::Attrs>(),
            ),
    )
    .await?;
    // Another run's rows are not this run's.
    save_run_trace(
        &catalog,
        &RunTrace::new(Uuid::new_v4(), 1, "elsewhere", started, TraceOutcome::Ok),
    )
    .await?;

    let traces = list_run_traces(&catalog, run).await?;
    assert_eq!(traces.len(), 2);
    // In the order the steps were taken, attempts and all — which is what makes
    // "it failed once and then worked" readable on the timeline.
    assert_eq!(traces[0].outcome, TraceOutcome::Error);
    assert_eq!(traces[0].attempt, 1);
    assert!(
        traces[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("timed out")
    );
    assert_eq!(traces[1].outcome, TraceOutcome::Ok);
    assert_eq!(traces[1].attempt, 2);
    assert_eq!(traces[1].context["total"], json!(120));
    assert_eq!(traces[1].error, None);

    assert_eq!(sc_workflow::delete_run_traces(&catalog, run).await?, 2);
    assert!(list_run_traces(&catalog, run).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn bootstrapping_twice_leaves_one_table_with_one_key() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    // Idempotent, as every bootstrap is — including the jointly-unique key,
    // which is added only when introspection says it is not already there.
    bootstrap_workflow_versions(&catalog).await?;
    let table = bootstrap_workflow_versions(&catalog).await?;
    let unique: Vec<_> = table
        .constraints
        .iter()
        .filter(|c| {
            matches!(&c.kind, sc_catalog::ConstraintKind::Unique { fields }
                if fields == &["workflow".to_owned(), "version".to_owned()])
        })
        .collect();
    assert_eq!(unique.len(), 1, "{:?}", table.constraints);
    Ok(())
}
