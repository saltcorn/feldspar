//! The App Constructor (TODO-app-constructor; docs/TECHNICAL_DESIGN.md §11.6):
//! an application described in a few paragraphs, broken into phases, each
//! phase into tasks, each task run by an agent.
//!
//! - [`model`] — the construction, the phase, the task, and a phase's status,
//!   which follows from its tasks.
//! - [`store`] — the three `_fd_construction*` tables.
//! - [`plan`] — saving the phases, validated once for the agent's tool and
//!   the admin's edits.
//! - [`tasks`] — a phase's tasks, planned when the phase is reached, and what
//!   the runner does next.

/// This crate's own tables' access to the primary database
/// (TODO-code-quality Q3.5): the one `Unchecked::system` its internal stores
/// make, listed in docs/SECURITY_REVIEW_2026-10.md's system-authority
/// inventory. Crate-private, so nothing outside the crate borrows it.
pub(crate) const INTERNAL_TABLES: sc_catalog::Unchecked<'static> =
    sc_catalog::Unchecked::system("sc-construct: the construction store");

pub mod model;
pub mod plan;
pub mod store;
pub mod tasks;

pub use model::{
    Construction, ConstructionStatus, Phase, PhaseStatus, PhaseWithTasks, Priority, Requirement,
    Task, TaskKind, TaskStatus, phase_status,
};
pub use plan::{MAX_PHASES, PhaseInput, SavedPhases, parse_phases, phase_list, save_phases};
pub use store::{
    CONSTRUCTIONS_TABLE, PHASES_TABLE, TASKS_TABLE, bootstrap_constructions, create_construction,
    delete_construction, delete_phase, insert_phase, insert_task, list_constructions,
    load_construction, load_task, mark_running, phases_of, phases_with_tasks, require_construction,
    running_constructions, save_specification, set_construction_status, set_plan_error,
    set_plan_run, task_insert, tasks_of_phase, update_task,
};
pub use tasks::{MAX_TASKS, Next, TaskInput, next_step, parse_tasks, save_tasks, task_list};
