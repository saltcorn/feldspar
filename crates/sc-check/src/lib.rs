//! Checklists (docs/CHECKLISTS.md; TODO-checklists C1): what "working" means
//! for an instance's applications and data, written down so that every change
//! — by a person or by an agent — can be checked against it.
//!
//! A **checklist** is an ordered list of **steps** run as one user. Each step
//! does something (insert a row, call an endpoint, build the application)
//! and/or asserts something (these rows exist, that request answers 403); a
//! failing assertion fails the step, and with it the checklist.
//!
//! - [`model`] — the checklist, the step, the proposal and the run.
//! - [`store`] — the four `_fd_checklist*` tables. Steps are never updated in
//!   place: every edit is a new version, every deletion a tombstone.
//! - [`registry`] — the step-type extension point: [`StepType`], what it
//!   [`Needs`], the [`StepContext`] it runs with and the [`StepOutcome`] it
//!   reports. The core's types are in [`steps`] — `browser.*` among them,
//!   driving the server's headless browser by stable locators; frameworks and
//!   modules add theirs in later milestones.
//! - [`vars`] — `as` captures, and string parameters as templates over them.
//! - [`runner`] — running a checklist as its user, with the production guard,
//!   against the live mount or an agent's preview, and recording the run.
//! - [`screenshots`] — the internal store of failed browser steps' screenshots.
//! - [`edit`] — every change to a checklist, and the agent policy of §4.
//! - [`tools`] — the agent tools, shared by the copilot and the MCP server.

pub mod edit;
pub mod model;
pub mod registry;
pub mod runner;
pub mod screenshots;
pub mod steps;
pub mod store;
pub mod tools;
pub mod vars;

pub use edit::{Change, ChecklistGrant, ChecklistInput, Editor};
pub use model::{
    Author, Checklist, ChecklistRun, Outcome, Proposal, ProposalChange, RunStatus, SkipReason,
    Step, StepDraft, StepResult,
};
pub use registry::{Capabilities, Needs, StepContext, StepOutcome, StepRegistry, StepType};
pub use runner::{CheckEnv, TARGET_LIVE, run_checklist, run_checklists};
pub use store::{
    CHECKLISTS_TABLE, ChecklistScope, PROPOSALS_TABLE, RUNS_TABLE, STEPS_TABLE,
    bootstrap_checklists,
};
