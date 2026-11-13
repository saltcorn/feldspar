//! A phase's tasks (TODO-app-constructor AC3.1) and what the runner does next
//! (AC3.2).
//!
//! The construction agent plans a phase's tasks **when the runner reaches the
//! phase**, with [`save_tasks`], so a feature task is written against the
//! tables the data-model task actually made. Tasks are few and sizeable: each
//! starts a fresh session that reads the schema and the code again.
//!
//! A task's `depends_on` names earlier tasks of the same list by their number
//! (1 is the first), so a list cannot depend on itself in a circle. The tasks
//! of earlier phases are all done before a phase starts, so they need no
//! naming.
//!
//! [`next_step`] reads the phases and their tasks and says what the runner
//! does next. It is a pure function, so the order is tested without a model.

use std::collections::HashSet;

use sc_catalog::Catalog;
use sc_db::Transaction;
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, Statement};
use serde::Deserialize;
use uuid::Uuid;

use crate::model::{Phase, PhaseWithTasks, Task, TaskKind, TaskStatus, spelling};
use crate::store::{TASKS_TABLE, lock_phases, require_construction, task_insert, tasks_in, tx_run};

/// The most tasks one phase may have. v1's phases had two to eight; a longer
/// list is a sign the phase should be split.
pub const MAX_TASKS: usize = 20;

/// The longest task title, in characters.
pub const MAX_TASK_TITLE: usize = 200;

/// One task as the agent sends it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub kind: TaskKind,
    pub title: String,
    /// What to make, for the agent that runs the task: it reads nothing else
    /// of the plan.
    pub brief: String,
    /// Earlier tasks of the same list, by number (1 is the first).
    #[serde(default)]
    pub depends_on: Vec<usize>,
}

/// Read a list of tasks from a tool's JSON arguments. A wrong shape is
/// refused with the task it is in.
pub fn parse_tasks(value: &serde_json::Value) -> Result<Vec<TaskInput>> {
    let Some(items) = value.as_array() else {
        return Err(Error::invalid("`tasks` must be a list of tasks"));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            serde_json::from_value(item.clone())
                .map_err(|e| Error::invalid(format!("task {}: {e}", i + 1)))
        })
        .collect()
}

/// Replace the tasks of `phase`, a phase of `application`'s construction,
/// with `inputs`, in order. Answers the saved tasks.
///
/// Refused when one of the phase's tasks has started: what ran stays recorded.
/// Refused too while the construction runs and the phase has tasks already:
/// the runner may have picked one of them, and would run a task that no
/// longer exists. Planning a phase with none, which the runner does when it
/// reaches one, is what a running construction allows. A list that is not
/// valid changes nothing.
pub async fn save_tasks(
    catalog: &Catalog,
    application: Uuid,
    phase: Uuid,
    inputs: &[TaskInput],
) -> Result<Vec<Task>> {
    let construction = require_construction(catalog, application).await?;
    let running = construction.status == crate::ConstructionStatus::Running;
    let inputs = inputs.to_vec();
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| Box::pin(write_tasks(tx, construction.id, phase, inputs, running)),
    )
    .await
}

/// The transactional part of [`save_tasks`]. The construction's phases are
/// locked first, as a save of the phases locks them, so a phase cannot be
/// deleted while its tasks are written.
async fn write_tasks(
    tx: &mut dyn Transaction,
    construction: Uuid,
    phase: Uuid,
    inputs: Vec<TaskInput>,
    running: bool,
) -> Result<Vec<Task>> {
    let phases = lock_phases(tx, construction).await?;
    let Some(phase) = phases.iter().find(|p| p.id == phase) else {
        return Err(Error::invalid(format!(
            "phase {phase} is not a phase of this construction"
        )));
    };
    let existing = tasks_in(tx, phase.id).await?;
    if existing.iter().any(|t| t.status != TaskStatus::Todo) {
        return Err(Error::invalid(format!(
            "phase `{}` has started, so its tasks cannot be planned again",
            phase.title
        )));
    }
    if running && !existing.is_empty() {
        return Err(Error::invalid(format!(
            "the construction is running and phase `{}` has its tasks already; stop it to \
             plan them again",
            phase.title
        )));
    }
    let tasks = build_tasks(phase.id, &inputs)?;

    let delete = Delete::from(TASKS_TABLE).filter(Expr::col("phase").eq(Expr::lit(phase.id)));
    tx_run(tx, &Statement::from(delete)).await?;
    for task in &tasks {
        tx_run(tx, &Statement::from(task_insert(task)?)).await?;
    }
    Ok(tasks)
}

/// The phase `phase`, if its tasks may be planned (again) on their own, before
/// a run; else why not. Refused unless every phase before it is done, since its
/// tasks are planned against what those made, and unless none of its tasks has
/// started, since what ran stays recorded. The phase comes with the tasks a new
/// plan replaces (none when it has not been planned yet).
pub fn require_plannable(phases: &[PhaseWithTasks], phase: Uuid) -> Result<&PhaseWithTasks> {
    let Some(index) = phases.iter().position(|p| p.phase.id == phase) else {
        return Err(Error::not_found(format!(
            "no phase {phase} in this construction"
        )));
    };
    let target = &phases[index];
    if let Some(before) = phases[..index]
        .iter()
        .find(|p| p.status() != crate::PhaseStatus::Done)
    {
        return Err(Error::invalid(format!(
            "phase `{}` is not done yet, and `{}`'s tasks are planned against what the phases \
             before it made; run up to `{}` first",
            before.phase.title, target.phase.title, before.phase.title
        )));
    }
    if target
        .tasks
        .iter()
        .any(|t| t.status != TaskStatus::Todo || !t.runs.is_empty())
    {
        return Err(Error::invalid(format!(
            "phase `{}` has started, so its tasks cannot be planned again",
            target.phase.title
        )));
    }
    Ok(target)
}

/// What the run that plans a phase's tasks again is told besides the phase:
/// the plan it replaces, and the admin's `note` on it. `None` when there is
/// neither.
pub fn replan_note(previous: &[Task], note: Option<&str>) -> Option<String> {
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    let mut out = String::new();
    if !previous.is_empty() {
        out.push_str("This phase was planned before, as:\n");
        for (i, task) in previous.iter().enumerate() {
            out.push_str(&format!(
                "{}. ({}) {}: {}\n",
                i + 1,
                spelling(task.kind),
                task.title,
                task.brief
            ));
        }
        out.push_str(
            "The admin asked for the tasks to be planned again, so that plan is gone. \
             Plan the phase anew; keep what was right in it.\n",
        );
    }
    if let Some(note) = note {
        out.push_str(&format!(
            "The admin's note on how to plan this phase: {note}\n"
        ));
    }
    (!out.is_empty()).then_some(out)
}

/// Delete the tasks of `phase`, a phase of `application`'s construction, so
/// they can be planned again. Answers the deleted tasks. In one transaction
/// that checks [`require_plannable`] again under the phases' lock, so a task that
/// started in between is never deleted.
pub async fn clear_tasks(catalog: &Catalog, application: Uuid, phase: Uuid) -> Result<Vec<Task>> {
    let construction = require_construction(catalog, application).await?;
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let phases = lock_phases(tx, construction.id).await?;
                let mut with_tasks = Vec::with_capacity(phases.len());
                for p in phases {
                    let tasks = tasks_in(tx, p.id).await?;
                    with_tasks.push(PhaseWithTasks { phase: p, tasks });
                }
                let target = require_plannable(&with_tasks, phase)?;
                let delete =
                    Delete::from(TASKS_TABLE).filter(Expr::col("phase").eq(Expr::lit(phase)));
                tx_run(tx, &Statement::from(delete)).await?;
                Ok(target.tasks.clone())
            })
        },
    )
    .await
}

/// Check `inputs` and turn them into tasks of `phase`, numbered in order.
fn build_tasks(phase: Uuid, inputs: &[TaskInput]) -> Result<Vec<Task>> {
    if inputs.is_empty() {
        return Err(Error::invalid(
            "a phase needs at least one task; the list is empty",
        ));
    }
    if inputs.len() > MAX_TASKS {
        return Err(Error::invalid(format!(
            "{} tasks are too many for one phase: at most {MAX_TASKS}",
            inputs.len()
        )));
    }
    let mut tasks: Vec<Task> = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        let n = i + 1;
        let title = input.title.trim();
        if title.is_empty() {
            return Err(Error::invalid(format!("task {n} has no title")));
        }
        if title.chars().count() > MAX_TASK_TITLE {
            return Err(Error::invalid(format!(
                "task {n}'s title is longer than {MAX_TASK_TITLE} characters; put the detail \
                 in its brief"
            )));
        }
        let brief = input.brief.trim();
        if brief.is_empty() {
            return Err(Error::invalid(format!(
                "task {n} has no brief; say what it makes"
            )));
        }
        let mut depends_on = Vec::new();
        for &earlier in &input.depends_on {
            if earlier == 0 || earlier >= n {
                return Err(Error::invalid(format!(
                    "task {n} depends on task {earlier}, which is not before it; a task can \
                     only depend on earlier tasks of the list"
                )));
            }
            let id = tasks[earlier - 1].id;
            if !depends_on.contains(&id) {
                depends_on.push(id);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let mut task = Task::new(phase, n as f64, input.kind, title, brief);
        task.depends_on = depends_on;
        tasks.push(task);
    }
    Ok(tasks)
}

/// The tasks as the agent reads them: one numbered line each, with its kind,
/// status and what it depends on.
pub fn task_list(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "No tasks yet.".to_owned();
    }
    let number = |id: &Uuid| tasks.iter().position(|t| t.id == *id).map(|i| i + 1);
    let mut out = String::new();
    for (i, task) in tasks.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} [{}, {}]",
            i + 1,
            task.title,
            spelling(task.kind),
            spelling(task.status)
        ));
        let after: Vec<String> = task
            .depends_on
            .iter()
            .filter_map(number)
            .map(|n| n.to_string())
            .collect();
        if !after.is_empty() {
            out.push_str(&format!(" after {}", after.join(", ")));
        }
        out.push('\n');
    }
    out
}

/// What the runner does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// The phase has no tasks yet: plan them.
    PlanTasks(Phase),
    /// Run this task: the first `todo` one whose dependencies are done, or
    /// the one still `running` when the server stopped.
    RunTask(Task),
    /// A task failed: the construction stops here.
    Failed(Task),
    /// The phase has tasks to do, but none can start: each waits on another
    /// that is not done.
    Blocked(Phase),
    /// Nothing left to do: every phase is done, or the phase the run ends
    /// after is.
    Finished,
}

/// Decide what the runner does next.
///
/// `phases` are the construction's phases in order, each with its tasks.
/// `until` is the phase the run should stop after; `None` runs all phases.
///
/// Phases run one after another. A phase's tasks are only planned once every
/// earlier phase is done. A skipped task counts as done, so tasks that depend
/// on it can still run.
pub fn next_step(phases: &[PhaseWithTasks], until: Option<Uuid>) -> Next {
    let finished: HashSet<Uuid> = phases
        .iter()
        .flat_map(|p| &p.tasks)
        .filter(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
        .map(|t| t.id)
        .collect();
    for PhaseWithTasks { phase, tasks } in phases {
        if tasks.is_empty() {
            return Next::PlanTasks(phase.clone());
        }
        if let Some(failed) = tasks.iter().find(|t| t.status == TaskStatus::Failed) {
            return Next::Failed(failed.clone());
        }
        if let Some(running) = tasks.iter().find(|t| t.status == TaskStatus::Running) {
            return Next::RunTask(running.clone());
        }
        if !tasks.iter().all(|t| finished.contains(&t.id)) {
            return tasks
                .iter()
                .find(|t| {
                    t.status == TaskStatus::Todo
                        && t.depends_on.iter().all(|d| finished.contains(d))
                })
                .map_or_else(
                    || Next::Blocked(phase.clone()),
                    |t| Next::RunTask(t.clone()),
                );
        }
        if until == Some(phase.id) {
            return Next::Finished;
        }
    }
    Next::Finished
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(title: &str) -> Phase {
        Phase {
            id: Uuid::new_v4(),
            construction: Uuid::nil(),
            position: 1.0,
            title: title.to_owned(),
            description: String::new(),
            requirements: Vec::new(),
        }
    }

    fn input(title: &str, depends_on: &[usize]) -> TaskInput {
        TaskInput {
            kind: TaskKind::Feature,
            title: title.to_owned(),
            brief: format!("Make {title}"),
            depends_on: depends_on.to_vec(),
        }
    }

    fn with(phase: &Phase, tasks: Vec<Task>) -> PhaseWithTasks {
        PhaseWithTasks {
            phase: phase.clone(),
            tasks,
        }
    }

    fn with_status(mut task: Task, status: TaskStatus) -> Task {
        task.status = status;
        task
    }

    #[test]
    fn depends_on_names_earlier_tasks_by_number() {
        let p = Uuid::new_v4();
        let tasks = build_tasks(
            p,
            &[input("A", &[]), input("B", &[1]), input("C", &[1, 2, 1])],
        )
        .unwrap();
        assert_eq!(tasks[1].depends_on, [tasks[0].id]);
        assert_eq!(tasks[2].depends_on, [tasks[0].id, tasks[1].id]);
        assert_eq!(tasks[2].position, 3.0);

        let err = build_tasks(p, &[input("A", &[1])]).unwrap_err().to_string();
        assert!(err.contains("task 1 depends on task 1"), "{err}");
        let err = build_tasks(p, &[input("A", &[]), input("B", &[3])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not before it"), "{err}");
        let err = build_tasks(p, &[]).unwrap_err().to_string();
        assert!(err.contains("at least one task"), "{err}");
        let mut blank = input("A", &[]);
        blank.brief = "  ".to_owned();
        let err = build_tasks(p, &[blank]).unwrap_err().to_string();
        assert!(err.contains("task 1 has no brief"), "{err}");
    }

    #[test]
    fn the_next_step_plans_a_phase_only_when_it_is_reached() {
        let (first, second) = (phase("Data model"), phase("Contacts"));
        let tables = Task::new(first.id, 1.0, TaskKind::DataModel, "Tables", "Make them");
        let plan = vec![with(&first, vec![tables.clone()]), with(&second, vec![])];
        assert_eq!(next_step(&plan, None), Next::RunTask(tables.clone()));

        let plan = vec![
            with(&first, vec![with_status(tables, TaskStatus::Done)]),
            with(&second, vec![]),
        ];
        assert_eq!(next_step(&plan, None), Next::PlanTasks(second));
        assert_eq!(next_step(&plan, Some(first.id)), Next::Finished);
    }

    #[test]
    fn depends_on_orders_the_tasks_and_a_skip_counts_as_done() {
        let p = phase("Contacts");
        let tasks = build_tasks(p.id, &[input("List", &[]), input("Edit", &[1])]).unwrap();
        // Saved in another order: the dependency still decides.
        let plan = vec![with(&p, vec![tasks[1].clone(), tasks[0].clone()])];
        assert_eq!(next_step(&plan, None), Next::RunTask(tasks[0].clone()));

        let skipped = with_status(tasks[0].clone(), TaskStatus::Skipped);
        let plan = vec![with(&p, vec![skipped, tasks[1].clone()])];
        assert_eq!(next_step(&plan, None), Next::RunTask(tasks[1].clone()));
    }

    #[test]
    fn a_failed_task_stops_and_a_running_one_is_resumed_first() {
        let p = phase("Contacts");
        let tasks = build_tasks(p.id, &[input("List", &[]), input("Edit", &[])]).unwrap();
        let failed = with_status(tasks[0].clone(), TaskStatus::Failed);
        let plan = vec![with(&p, vec![failed.clone(), tasks[1].clone()])];
        assert_eq!(next_step(&plan, None), Next::Failed(failed));

        let running = with_status(tasks[1].clone(), TaskStatus::Running);
        let plan = vec![with(&p, vec![tasks[0].clone(), running.clone()])];
        assert_eq!(next_step(&plan, None), Next::RunTask(running));

        // Waiting on a task of a later phase can never start.
        let later = Uuid::new_v4();
        let mut stuck = tasks[0].clone();
        stuck.depends_on = vec![later];
        let plan = vec![with(&p, vec![stuck])];
        assert_eq!(next_step(&plan, None), Next::Blocked(p));
    }

    #[test]
    fn only_the_next_phase_whose_tasks_have_not_started_is_plannable() {
        let (first, second) = (phase("Data model"), phase("Contacts"));
        let tasks = build_tasks(first.id, &[input("Tables", &[])]).unwrap();
        // Nothing planned: the first phase is, the second waits on it.
        let fresh = [with(&first, Vec::new()), with(&second, Vec::new())];
        assert!(
            require_plannable(&fresh, first.id)
                .unwrap()
                .tasks
                .is_empty()
        );
        let refused = require_plannable(&fresh, second.id)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("`Data model` is not done yet"),
            "{refused}"
        );
        // Planned, nothing started: planned again, answering the old tasks.
        let planned = [with(&first, tasks.clone()), with(&second, Vec::new())];
        assert_eq!(require_plannable(&planned, first.id).unwrap().tasks, tasks);
        // A task that ran (even one put back to todo by a stop) has started.
        let mut stopped = tasks[0].clone();
        stopped.runs.push(Uuid::new_v4());
        let refused = require_plannable(&[with(&first, vec![stopped])], first.id).unwrap_err();
        assert!(refused.to_string().contains("has started"), "{refused}");
        // The first done (a skipped task counts): the second is next.
        let done = [
            with(
                &first,
                vec![with_status(tasks[0].clone(), TaskStatus::Skipped)],
            ),
            with(&second, Vec::new()),
        ];
        assert!(require_plannable(&done, second.id).is_ok());
        assert!(require_plannable(&done, Uuid::new_v4()).is_err());
    }

    #[test]
    fn a_replan_is_told_the_old_plan_and_the_note() {
        let tasks = build_tasks(Uuid::new_v4(), &[input("Tables", &[])]).unwrap();
        let note = replan_note(&tasks, Some("  no audit log ")).unwrap();
        assert!(note.contains("planned before"), "{note}");
        assert!(note.contains("1. (feature) Tables: Make Tables"), "{note}");
        assert!(
            note.contains("note on how to plan this phase: no audit log\n"),
            "{note}"
        );
        let first = replan_note(&[], Some("contacts only")).unwrap();
        assert!(!first.contains("planned before"), "{first}");
        assert_eq!(replan_note(&[], Some("  ")), None);
        assert_eq!(replan_note(&[], None), None);
    }
}
