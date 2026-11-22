//! The admin's feedback on what a phase built (TODO-app-constructor AC9).
//!
//! Feedback is given on one phase that has been built, at least in part, and
//! is only recorded ([`add_feedback`]): giving it starts nothing. The admin
//! approves it by starting the run that plans it: the latest of its runs
//! ([`crate::construction_runs`]) is its `run`. The construction agent turns it into requirements,
//! kept with the feedback, and follow-up tasks, added after the phase's own
//! tasks and naming the feedback ([`plan_feedback`]). The tasks wait as `todo`
//! for the admin to review and run, as a phase's planned tasks do: the runner
//! reaches them first, as they belong to the earliest unfinished phase.
//!
//! A feedback's status: given until approved (no `run`); then from its tasks
//! ([`crate::phase_status`]): none is not planned (yet, or its planning
//! failed), then planned, running, failed or done. Feedback whose tasks have
//! not started can be deleted, with its tasks, which declines it when it is
//! not approved; once one has run, what ran stays recorded.

use sc_catalog::{Catalog, DataField};
use sc_db::{Row, Transaction};
use sc_error::{Error, Result};
use sc_query::{
    Assignment, Delete, Expr, InSet, OrderBy, Projection, Select, Source, Statement, Update, Value,
};
use sc_types::BasicType;
use serde::Deserialize;
use uuid::Uuid;

use crate::construction_runs::{RunPurpose, construction_runs_for};
use crate::model::{Feedback, PhaseWithTasks, Requirement, Task, TaskStatus};
use crate::store::{
    PHASES_TABLE, TASKS_TABLE, from_json, insert_statement, key, lock_phases, req_float, req_text,
    req_uuid, require_construction, rows, task_insert, tasks_in, to_json, tx_rows, tx_run,
};
use crate::tasks::{TaskInput, build_tasks};

/// The admin's feedback on a construction's phases.
pub const FEEDBACK_TABLE: &str = "_fd_construction_feedback";

/// The longest a feedback's title may be, in characters.
pub const MAX_FEEDBACK_TITLE: usize = 200;
/// The most requirements one feedback may add: it asks for a change, not a
/// new phase.
pub const MAX_FEEDBACK_REQUIREMENTS: usize = 10;
/// The most follow-up tasks one feedback may plan.
pub const MAX_FEEDBACK_TASKS: usize = 5;

pub(crate) fn feedback_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        // The construction is the phase's.
        key("phase", PHASES_TABLE).required(),
        DataField::plain("position", BasicType::Float).required(),
        DataField::plain("title", BasicType::Text).required(),
        DataField::plain("description", BasicType::Text).required(),
        DataField::plain("requirements", BasicType::Json).required(),
    ]
}

/// The phase `phase`, if it can take feedback; else why not. It must have been
/// built, at least in part (a task done), and have no failed task, which
/// would stop the runner before the follow-up tasks.
pub fn require_feedback_phase(phases: &[PhaseWithTasks], phase: Uuid) -> Result<&PhaseWithTasks> {
    let Some(target) = phases.iter().find(|p| p.phase.id == phase) else {
        return Err(Error::not_found(format!(
            "no phase {phase} in this construction"
        )));
    };
    if !target.tasks.iter().any(|t| t.status == TaskStatus::Done) {
        return Err(Error::invalid(format!(
            "phase `{}` has not built anything yet; feedback is on what a phase built",
            target.phase.title
        )));
    }
    if let Some(failed) = target.tasks.iter().find(|t| t.status == TaskStatus::Failed) {
        return Err(Error::invalid(format!(
            "task `{}` of phase `{}` failed; retry or skip it before giving feedback",
            failed.title, target.phase.title
        )));
    }
    Ok(target)
}

/// Record feedback on `phase`, a phase of `application`'s construction that
/// can take it ([`require_feedback_phase`]), not approved yet. Checked under the phases' lock, so the phase cannot change meanwhile.
pub async fn add_feedback(
    catalog: &Catalog,
    application: Uuid,
    phase: Uuid,
    title: &str,
    description: &str,
) -> Result<Feedback> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Error::invalid("the feedback has no title"));
    }
    if title.chars().count() > MAX_FEEDBACK_TITLE {
        return Err(Error::invalid(format!(
            "the feedback's title is longer than {MAX_FEEDBACK_TITLE} characters; put the \
             detail in its description"
        )));
    }
    let description = description.trim();
    if description.is_empty() {
        return Err(Error::invalid(
            "the feedback has no description; say what should change",
        ));
    }
    let construction = require_construction(catalog, application).await?;
    let (title, description) = (title.to_owned(), description.to_owned());
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let phases = with_tasks(tx, construction.id).await?;
                require_feedback_phase(&phases, phase)?;
                let last = tx_rows(tx, feedback_select(construction.id))
                    .await?
                    .iter()
                    .map(feedback_from_row)
                    .collect::<Result<Vec<_>>>()?
                    .iter()
                    .map(|f| f.position)
                    .fold(0.0, f64::max);
                let feedback = Feedback {
                    id: Uuid::new_v4(),
                    phase,
                    position: last + 1.0,
                    title,
                    description,
                    requirements: Vec::new(),
                    run: None,
                };
                let insert = insert_statement(
                    FEEDBACK_TABLE,
                    &[
                        "id",
                        "phase",
                        "position",
                        "title",
                        "description",
                        "requirements",
                    ],
                    vec![
                        Value::Uuid(feedback.id),
                        Value::Uuid(feedback.phase),
                        Value::Float(feedback.position),
                        Value::Text(feedback.title.clone()),
                        Value::Text(feedback.description.clone()),
                        Value::Json(to_json(&feedback.requirements)?),
                    ],
                );
                tx_run(tx, &Statement::from(insert)).await?;
                Ok(feedback)
            })
        },
    )
    .await
}

/// A construction's feedback, in the order it was given.
pub async fn feedback_of(catalog: &Catalog, construction: Uuid) -> Result<Vec<Feedback>> {
    // Should not happen: the server bootstraps the table at startup.
    if !matches!(catalog.get(FEEDBACK_TABLE), Ok(Some(_))) {
        return Ok(Vec::new());
    }
    let mut feedback = rows(catalog, feedback_select(construction))
        .await?
        .iter()
        .map(feedback_from_row)
        .collect::<Result<Vec<_>>>()?;
    if feedback.is_empty() {
        return Ok(feedback);
    }
    let runs = construction_runs_for(catalog, construction, RunPurpose::Feedback).await?;
    for f in &mut feedback {
        f.run = runs
            .iter()
            .filter(|r| r.feedback == Some(f.id))
            .find_map(|r| r.run);
    }
    Ok(feedback)
}

/// The feedback `id` of `construction`, or not found.
pub async fn require_feedback(catalog: &Catalog, construction: Uuid, id: Uuid) -> Result<Feedback> {
    feedback_of(catalog, construction)
        .await?
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(|| Error::not_found(format!("no feedback {id} in this construction")))
}

/// The tasks planned for `feedback`, in order.
pub fn feedback_tasks(phases: &[PhaseWithTasks], feedback: Uuid) -> Vec<Task> {
    phases
        .iter()
        .flat_map(|p| &p.tasks)
        .filter(|t| t.feedback == Some(feedback))
        .cloned()
        .collect()
}

/// The plan the agent sends for one feedback.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackPlan {
    pub requirements: Vec<Requirement>,
    pub tasks: Vec<TaskInput>,
}

/// Read a feedback's plan from a tool's JSON arguments, naming what is
/// wrong: one to [`MAX_FEEDBACK_REQUIREMENTS`] requirements with text, and
/// one to [`MAX_FEEDBACK_TASKS`] tasks.
pub fn parse_feedback_plan(args: &serde_json::Value) -> Result<FeedbackPlan> {
    let requirements: Vec<Requirement> =
        serde_json::from_value(args.get("requirements").cloned().unwrap_or_default())
            .map_err(|e| Error::invalid(format!("`requirements`: {e}")))?;
    if requirements.is_empty() || requirements.len() > MAX_FEEDBACK_REQUIREMENTS {
        return Err(Error::invalid(format!(
            "write between 1 and {MAX_FEEDBACK_REQUIREMENTS} requirements, not {}",
            requirements.len()
        )));
    }
    let requirements = requirements
        .into_iter()
        .enumerate()
        .map(|(i, r)| match r.text.trim() {
            "" => Err(Error::invalid(format!("requirement {} has no text", i + 1))),
            text => Ok(Requirement {
                text: text.to_owned(),
                priority: r.priority,
            }),
        })
        .collect::<Result<Vec<_>>>()?;
    let tasks = crate::tasks::parse_tasks(args.get("tasks").unwrap_or(&serde_json::Value::Null))?;
    if tasks.is_empty() {
        return Err(Error::invalid(
            "plan at least one follow-up task; the list is empty",
        ));
    }
    if tasks.len() > MAX_FEEDBACK_TASKS {
        return Err(Error::invalid(format!(
            "{} follow-up tasks are too many for one feedback: at most {MAX_FEEDBACK_TASKS}",
            tasks.len()
        )));
    }
    Ok(FeedbackPlan {
        requirements,
        tasks,
    })
}

/// Save the plan for `feedback`, of `application`'s construction: its
/// requirements, and its tasks after the phase's last one, as `todo`.
/// Answers the tasks as saved.
///
/// Refused when the feedback has tasks already (it was planned), or its phase
/// can no longer take feedback. In one transaction under the phases' lock, so
/// a list that is not valid changes nothing.
pub async fn plan_feedback(
    catalog: &Catalog,
    application: Uuid,
    feedback: Uuid,
    plan: &FeedbackPlan,
) -> Result<Vec<Task>> {
    let construction = require_construction(catalog, application).await?;
    let feedback = require_feedback(catalog, construction.id, feedback).await?;
    let plan = plan.clone();
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let phases = with_tasks(tx, construction.id).await?;
                if !feedback_tasks(&phases, feedback.id).is_empty() {
                    return Err(Error::invalid(format!(
                        "the feedback `{}` is planned already",
                        feedback.title
                    )));
                }
                let phase = require_feedback_phase(&phases, feedback.phase)?;
                let last = phase.tasks.iter().map(|t| t.position).fold(0.0, f64::max);
                let mut tasks = build_tasks(phase.phase.id, &plan.tasks)?;
                for task in &mut tasks {
                    task.position += last;
                    task.feedback = Some(feedback.id);
                    tx_run(tx, &Statement::from(task_insert(task)?)).await?;
                }
                let update = Update::new(
                    FEEDBACK_TABLE,
                    vec![Assignment::new(
                        "requirements",
                        Expr::Lit(Value::Json(to_json(&plan.requirements)?)),
                    )],
                )
                .filter(Expr::col("id").eq(Expr::lit(feedback.id)));
                tx_run(tx, &Statement::from(update)).await?;
                Ok(tasks)
            })
        },
    )
    .await
}

/// Delete `feedback` of `application`'s construction with its tasks. Refused
/// once one of them has started: what ran stays recorded.
pub async fn delete_feedback(catalog: &Catalog, application: Uuid, feedback: Uuid) -> Result<()> {
    let construction = require_construction(catalog, application).await?;
    let feedback = require_feedback(catalog, construction.id, feedback).await?;
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let phases = with_tasks(tx, construction.id).await?;
                let tasks = feedback_tasks(&phases, feedback.id);
                if let Some(started) = tasks
                    .iter()
                    .find(|t| t.status != TaskStatus::Todo || !t.runs.is_empty())
                {
                    return Err(Error::invalid(format!(
                        "task `{}` of this feedback has started, so the feedback stays",
                        started.title
                    )));
                }
                let delete_tasks = Delete::from(TASKS_TABLE)
                    .filter(Expr::col("feedback").eq(Expr::lit(feedback.id)));
                tx_run(tx, &Statement::from(delete_tasks)).await?;
                let delete_feedback =
                    Delete::from(FEEDBACK_TABLE).filter(Expr::col("id").eq(Expr::lit(feedback.id)));
                tx_run(tx, &Statement::from(delete_feedback)).await?;
                Ok(())
            })
        },
    )
    .await
}

/// `construction`'s phases with their tasks, read in `tx` with the phases
/// locked.
async fn with_tasks(tx: &mut dyn Transaction, construction: Uuid) -> Result<Vec<PhaseWithTasks>> {
    let phases = lock_phases(tx, construction).await?;
    let mut out = Vec::with_capacity(phases.len());
    for phase in phases {
        let tasks = tasks_in(tx, phase.id).await?;
        out.push(PhaseWithTasks { phase, tasks });
    }
    Ok(out)
}

/// The feedback on `construction`'s phases.
fn feedback_select(construction: Uuid) -> Select {
    let mut phases = Select::from(Source::table(PHASES_TABLE))
        .filter(Expr::col("construction").eq(Expr::lit(construction)));
    phases.columns = vec![Projection::expr(Expr::col("id"))];
    let mut select = Select::from(Source::table(FEEDBACK_TABLE)).filter(Expr::In {
        e: Box::new(Expr::col("phase")),
        set: InSet::Subquery(Box::new(phases)),
    });
    select.order = vec![OrderBy::asc(Expr::col("position"))];
    select
}

fn feedback_from_row(row: &Row) -> Result<Feedback> {
    let t = FEEDBACK_TABLE;
    Ok(Feedback {
        id: req_uuid(row, t, "id")?,
        phase: req_uuid(row, t, "phase")?,
        position: req_float(row, t, "position")?,
        title: req_text(row, t, "title")?,
        description: req_text(row, t, "description")?,
        requirements: from_json(row, t, "requirements")?,
        // From its construction runs ([`feedback_of`]).
        run: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plan_is_a_few_requirements_and_tasks() {
        let task = json!({"kind": "feature", "title": "Sort", "brief": "Sort the list by date"});
        let plan = parse_feedback_plan(&json!({
            "requirements": [{"text": " Sorted by date ", "priority": "must"}],
            "tasks": [task],
        }))
        .unwrap();
        assert_eq!(plan.requirements[0].text, "Sorted by date");
        assert_eq!(plan.tasks.len(), 1);

        let error = |v| parse_feedback_plan(&v).unwrap_err().to_string();
        assert!(error(json!({"requirements": [], "tasks": [task]})).contains("between 1 and"));
        assert!(
            error(json!({"requirements": [{"text": " ", "priority": "must"}], "tasks": []}))
                .contains("no text")
        );
        let six = vec![task.clone(); MAX_FEEDBACK_TASKS + 1];
        assert!(
            error(json!({"requirements": [{"text": "x", "priority": "must"}], "tasks": six}))
                .contains("too many")
        );
    }
}
