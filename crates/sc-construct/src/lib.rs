//! The App Constructor (TODO-app-constructor; docs/TECHNICAL_DESIGN.md §11.6):
//! an application described in a few paragraphs, broken into phases, each
//! phase into tasks, each task run by an agent.
//!
//! - [`model`] — the construction, the phase, the task, and a phase's status,
//!   which follows from its tasks.
//! - [`store`] — the construction, phase and task tables.
//! - [`plan`] — saving the phases, validated once for the agent's tool and
//!   the admin's edits.
//! - [`tasks`] — a phase's tasks, planned when the phase is reached, and what
//!   the runner does next.
//! - [`mockups`] — screens proposed before the phases are planned, one of
//!   which the admin may choose.
//! - [`questions`] — what the agent asks about the specification first, and
//!   the admin's answers.
//! - [`feedback`] — feedback on what a phase built, planned as requirements
//!   and follow-up tasks once approved.
//! - [`errors`] — every error the construction met, newest first.
//! - [`construction_runs`] — the runs it started; the latest one's error is
//!   its current error.

/// This crate's own tables' access to the primary database
/// (TODO-code-quality Q3.5): the one `Unchecked::system` its internal stores
/// make, listed in docs/SECURITY_REVIEW_2026-10.md's system-authority
/// inventory. Crate-private, so nothing outside the crate borrows it.
pub(crate) const INTERNAL_TABLES: sc_catalog::Unchecked<'static> =
    sc_catalog::Unchecked::system("sc-construct: the construction store");

pub mod construction_runs;
pub mod errors;
pub mod feedback;
pub mod mockups;
pub mod model;
pub mod plan;
pub mod questions;
pub mod store;
pub mod tasks;

pub use construction_runs::{
    CONSTRUCTION_RUNS_TABLE, ConstructionRun, RunPurpose, construction_runs_for, current_build,
    insert_construction_run, latest_agent_run, latest_construction_run, set_agent_run,
};
pub use errors::{
    ConstructionError, ERRORS_TABLE, ErrorSource, Failure, MAX_ERRORS_SHOWN, current_error,
    errors_of,
};
pub use feedback::{
    FEEDBACK_TABLE, FeedbackPlan, MAX_FEEDBACK_TASKS, add_feedback, delete_feedback, feedback_of,
    feedback_tasks, parse_feedback_plan, plan_feedback, require_feedback, require_feedback_phase,
};
pub use mockups::{
    DEFAULT_MOCKUPS, MAX_MOCKUPS, MOCKUPS_TABLE, MockupInput, choose_mockup, chosen_mockup,
    latest_round, mockup_text, mockups_of, parse_mockups, save_mockups, screen_file,
    set_mockup_feedback,
};
pub use model::{
    Construction, ConstructionStatus, Feedback, Mockup, MockupFeedback, MockupScreen, MockupStatus,
    Phase, PhaseStatus, PhaseWithTasks, Priority, Question, QuestionAnswer, Requirement, Task,
    TaskKind, TaskStatus, phase_status,
};
pub use plan::{MAX_PHASES, PhaseInput, SavedPhases, parse_phases, phase_list, save_phases};
pub use questions::{
    MAX_QUESTIONS, QUESTIONS_TABLE, SavedQuestions, answer_questions, answers_text,
    parse_questions, questions_of, save_questions,
};
pub use store::{
    CONSTRUCTIONS_TABLE, PHASES_TABLE, TASKS_TABLE, bootstrap_constructions, create_construction,
    delete_construction, delete_phase, insert_phase, insert_task, list_constructions,
    load_construction, load_task, mark_planning, mark_running, phases_of, phases_with_tasks,
    require_construction, running_constructions, save_specification, set_construction_status,
    set_plan_error, task_insert, tasks_of_phase, update_task,
};
pub use tasks::{
    MAX_TASKS, Next, TaskInput, clear_tasks, next_step, parse_tasks, replan_note,
    require_plannable, save_tasks, task_list,
};
