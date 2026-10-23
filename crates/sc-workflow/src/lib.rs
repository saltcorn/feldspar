//! `sc-workflow` — durable workflows (design §10.3).
//!
//! Layer 7, beside `sc-agent` and above `sc-action`. A **workflow is a trigger
//! body**, not a new top-level entity (decision 1): the trigger supplies the
//! event, the `only_if`, the role floor, the enabled flag, the periodic timing
//! and the exposure through an application, and this crate supplies the only
//! thing that is different — a *program* instead of one action, and an engine
//! that advances it durably.
//!
//! ## What is here
//!
//! - The **program** ([`workflow`]): [`Workflow`], [`Step`], [`StepKind`],
//!   [`Next`] and [`ErrorPolicy`] — pure data, whose serde shape *is* the stored
//!   shape, the API shape and the visual editor's shape.
//! - The **versions** ([`versions`]): `_fd_workflow_versions`, append-only, so a
//!   run pins the version it started on and finishes on it however many times the
//!   workflow is edited meanwhile.
//! - The **trace** ([`traces`]): `_fd_run_traces`, one row per completed step
//!   attempt, written only when the workflow asks for it.
//! - The **scope** ([`scope`]): [`workflow_shape`], the one place a step's
//!   formulas' scope is decided.
//! - The **machine** ([`machine`]): [`WorkflowRun`], the whole resumable state of
//!   one run as a single serialisable value, and the [`Decision`] it hands a
//!   driver. Sans-IO — it owns every decision and performs none of the work — so
//!   the engine's rules are testable synchronously, with no database, no runtime
//!   and no clock.
//! - The **run record** ([`run`]): the workflow half of an `_fd_runs` row — the
//!   machine state, the version it is pinned to, the event that started it and
//!   the chain that bounds its writes.
//! - The **driver** ([`driver`]): the one thing that does the IO — load the
//!   pinned version, ask the machine, run the action or evaluate the formulas,
//!   feed the outcome back, and **write once**.
//! - The **queue** ([`queue`]): which runs want the engine, claimed under a lease
//!   — a query over the runs table, which is what makes a crashed node's run
//!   recoverable without anybody having to detect the crash.
//! - The **checks** ([`validate`]): every way a workflow can be wrong, in one
//!   pass, run on save and again before a run of it starts (decision 11).
//! - The **operations** ([`ops`]): the four things a person does *to* a run —
//!   answer its form, cancel it, retry it, list the runs of one workflow.
//! - The **engine** ([`engine`]): [`WorkflowEngineTask`], the
//!   [`WorkflowEngine`](sc_action::WorkflowEngine) seam filled in and the one
//!   tokio task that advances runs nobody is waiting for.
//!
//! ## The two decisions worth knowing before reading
//!
//! **Control flow is data.** [`Next`] is an enum — a step, a branch, a formula,
//! the end — because a visual editor cannot round-trip an arbitrary expression
//! into edges. The formula variant keeps v1's expressiveness for the case that
//! needs it, drawn as one dashed edge to a computed marker.
//!
//! **A version is a row.** Editing a workflow mints a new one; nothing is
//! rewritten. That is the whole implementation of "a suspended run finishes with
//! its version of the workflow".

pub mod driver;
pub mod engine;
pub mod machine;
pub mod ops;
pub mod queue;
pub mod run;
pub mod scope;
pub mod traces;
pub mod validate;
pub mod versions;
pub mod workflow;

pub use driver::{Advanced, Clock, Driver, ManualClock, SystemClock, node_id, start_run};
pub use engine::{DEFAULT_LEASE_SECONDS, DEFAULT_POLL_SECONDS, WorkflowEngineTask};
pub use machine::{Conclusion, Decision, PendingForm, WorkflowRun};
pub use ops::{
    INLINE_LEASE_SECONDS, ROLE_ADMIN, cancel_run, check_may_resume, list_workflow_runs, may_resume,
    resume_run, retry_run,
};
pub use queue::{DEFAULT_BATCH, DatabaseQueue, WorkQueue};
pub use run::{
    ATTR_CHAIN, ATTR_CHANGESET, ATTR_EVENT, ATTR_WORKFLOW, StoredEvent, mark_aborted, new_run,
    record, release, run_chain, run_changeset, run_current_step, run_event, run_pending_form,
    run_state, run_version, run_workflow_id,
};
pub use scope::{WORKFLOW_SCOPE, workflow_shape};
pub use traces::{
    RunTrace, TRACES_TABLE, TraceOutcome, bootstrap_run_traces, delete_run_traces, list_run_traces,
    save_run_trace, trace_insert,
};
pub use validate::{
    WorkflowIssue, issues_message, usable_as_step, validate_workflow, workflow_issues,
};
pub use versions::{
    VERSIONS_TABLE, WorkflowVersion, bootstrap_workflow_versions, current_workflow,
    delete_workflow_versions, list_workflow_versions, load_workflow_version, max_version,
    require_current_workflow, require_workflow_version, save_workflow,
};
pub use workflow::{
    Assignment, Backoff, BranchArm, DEFAULT_MAX_STEPS, ErrorPolicy, FieldDecl, Next, Step,
    StepKind, Workflow,
};

/// The context key an [`ErrorPolicy::Handler`] jump puts the failure under, so a
/// handler step can read what went wrong (`context.error.message`).
///
/// Reserved: a `Set` writing this key would make the handler read its own
/// workflow's invention instead of the engine's report, so validation refuses it.
pub const ERROR_KEY: &str = "error";
