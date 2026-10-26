//! `sc-agent` — agents, traits and the loop (design §11.2).
//!
//! Layer 7. This crate knows **agents, traits and the loop, and nothing about
//! which vendor is on the other end** — that is [`sc_llm`]'s, below it. It also
//! knows nothing about *which* traits exist: it defines what a trait is, and the
//! built-in set lives in `sc-core-traits` at layer 9, above the row layer, so a
//! trait that writes a row goes through `sc-api` and its write is coerced,
//! validated and observed like any other.
//!
//! ## What is here
//!
//! - The **record** ([`agent`], [`store`], [`validate`]): [`Agent`], `_fd_agents`,
//!   and one validation function run on save *and* on load. An agent that fails
//!   is dropped from the live set ([`Agents`]) with its reason kept, and stays
//!   stored, listed and editable — editing it is the repair.
//! - The **extension point** ([`agent_trait`], [`registry`]): [`AgentTrait`] and
//!   [`AgentRegistry`], the twin of `sc-action`'s `ActionRegistry`.
//! - The **loop** ([`machine`]): [`AgentLoop`], a steppable, serialisable state
//!   machine that decides and does no IO.
//! - **The context** ([`context`]): the request layout (stable prefix, session
//!   header, history), the context budget, and clearing and compacting to keep
//!   within it — as overlays, so the stored transcript stays whole.
//! - **Loop control** ([`control`], [`schema`]): fingerprints, the doom-loop
//!   detectors, the malformed-call cap, trait [`Signal`]s and the escalation
//!   ladder, plus the check of a call's arguments against its tool's schema.
//! - The **driver** ([`driver`]): [`Runner`], the one thing that does IO for it —
//!   streams the provider, dispatches tools as the run's caller, and writes
//!   `_fd_runs` after every step.
//! - The **run** ([`run`], [`run_store`]): [`Run`] and `_fd_runs`, in the shape
//!   §10.3's workflow engine will also use.
//! - **Delegation** ([`delegate`]): [`Delegator`], the capability a run offers a
//!   trait that names *another agent* — a child run, under the same authority,
//!   with a context of its own and a bound on how deep the chain may go.
//! - **Looking at an application** ([`view`]): the preview-mount and browser
//!   seams a trait reaches through [`TraitContext`], implemented by the server.
//! - A **scripted provider** ([`testing`], behind the `testing` feature), because
//!   no test in this tree may need an API key or spend a token.
//!
//! ## The two decisions worth knowing before reading
//!
//! **The loop is a machine, not an `async fn`.** The state is a value, so it is
//! what `_fd_runs` stores and a resumed run is a loaded one. See [`machine`].
//!
//! **A tool runs as the caller, not as the server.** [`RunCaller`] travels with
//! the run and there is no default: a chat turn carries the person and a
//! triggered run carries the trigger's authority, so §7.3's ownership and RLS
//! apply inside an agent exactly as they do outside it.

pub mod agent;
pub mod agent_trait;
pub mod context;
pub mod control;
pub mod delegate;
pub mod driver;
pub mod interact;
pub mod ledger;
pub mod machine;
pub mod meter;
pub mod parts;
pub mod registry;
pub mod run;
pub mod run_store;
pub mod schema;
pub mod store;
pub mod validate;
pub mod view;

#[cfg(feature = "testing")]
#[cfg_attr(docsrs, doc(cfg(feature = "testing")))]
pub mod testing;

pub use agent::{
    ATTR_CHEAP, ATTR_CONTEXT_BUDGET, ATTR_MAX_COST, ATTR_MAX_IMAGES, ATTR_MAX_WALL_SECONDS,
    ATTR_PARALLEL_TOOL_CALLS, ATTR_REASONING_EFFORT, ATTR_STRONG, DEFAULT_MAX_IMAGES, ModelRef,
    ModelRole,
};
pub use agent::{
    ATTR_MAX_STEPS, ATTR_MAX_TOKENS, ATTR_TEMPERATURE, Agent, AgentId, DEFAULT_MAX_STEPS,
    EnabledTrait,
};
pub use agent::{ATTR_PICKABLE_MODELS, MODEL_CONTROL, MODEL_CONTROL_DEFAULT, PickableModel};
pub use agent_trait::{
    AfterToolsContext, AgentTrait, AnswerContext, CallExtras, ControlsContext, RunCaller,
    SessionContext, ToolsContext, TraitCheck, TraitContext, Turn,
};
pub use context::{
    ATTR_KEEP_TURNS, COMPACT_PERCENT, Compaction, ContextState, ContextVerdict, DEFAULT_KEEP_TURNS,
    Elidable, SUMMARY_PERCENT,
};
pub use control::{
    ATTR_CALM_ROUNDS, ATTR_MAX_IDENTICAL_CALLS, ATTR_MAX_MALFORMED_CALLS, ATTR_MAX_REPEATED_ROUNDS,
    ATTR_MAX_REPEATED_TEXT, ATTR_MAX_SIGNALS, ControlLimits, LoopControl, Rung, Signal,
    canonical_json, fingerprint,
};
pub use delegate::{
    ATTR_DELEGATED_BY, ATTR_PARENT_RUN, DEFAULT_MAX_DEPTH, DelegateRequest, Delegated, Delegator,
};
pub use driver::{
    ProviderConnector, RunObserver, Runner, StablePrefix, StoredProviders, composer_controls,
    composer_controls_for, connect, stable_prefix,
};
pub use interact::{
    Answer, ChoiceOption, ComposerControl, ControlKind, DisplayPart, DisplayRef, MAX_DISPLAY_ROWS,
    RequestKind, UserRequest, check_controls,
};
pub use ledger::{ChildLedger, CodeEvaluation, Ledger, LedgerStep, RoleTotals};
pub use machine::{
    AgentLoop, Budget, Budgets, Conclusion, IMAGE_STUB, Step, StepMeta, ToolOutcome,
    answer_display_key, trait_state_key,
};
pub use meter::{
    ATTR_CREDITS_MIN_BALANCE, ATTR_CREDITS_PER_1K_INPUT, ATTR_CREDITS_PER_1K_OUTPUT,
    ATTR_CREDITS_PER_DOLLAR, Meter,
};
pub use parts::{
    RUN_PARTS_TABLE, StoredPart, bootstrap_run_parts, load_display_part, save_display_part,
};
pub use registry::AgentRegistry;
pub use run::{ATTR_MODE, ATTR_ROLE, Run, RunId, RunKind, RunMode, RunState};
pub use run_store::{
    RUNS_TABLE, abort_run, bootstrap_runs, delete_run, list_live_children, list_runs,
    list_runs_for, load_run, note_wakeup, require_run, run_insert, run_update, save_run,
};
pub use store::{
    AGENTS_TABLE, bootstrap_agents, delete_agent, list_agents, load_agent, load_agent_by_name,
    require_agent, save_agent,
};
pub use validate::{AgentIssue, Agents, validate_agent, validate_trait};
pub use view::{
    AppHttpRequest, AppHttpResponse, AppPreviewer, AppRequester, BrowserAction, BrowserDriver,
    BrowserReport, BrowserRequest, HostCapabilities, ModuleInstall, ModuleManager, PreviewInfo,
    ViewServices,
};
