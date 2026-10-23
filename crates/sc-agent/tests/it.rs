//! Every integration test in this crate, in one binary.
//!
//! Each file below is still an ordinary test file — it is pulled in as a module
//! rather than compiled as its own target. The workspace statically links V8 into
//! every test binary, so a target per file cost ~400 MB of disk and a link each;
//! CI ran out of disk on the link (`ld terminated with signal 7`) before it ran
//! out of patience. Files stay where they are, so paths relative to a test file
//! (fixtures, `include_str!`, `#[path]`) are unaffected.
//!
//! Add a new test file and it is picked up here — the list is the whole wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// Shared by the modules below, which reach it as `crate::common`. Declared once
// here rather than once per file; it is `dead_code`-exempt because no single
// test uses all of it (the module carries that exemption itself). `macro_use`
// because the module also defines `macro_rules!` helpers, whose textual scope
// has to reach the modules declared after it.
#[macro_use]
#[path = "common/mod.rs"]
mod common;

#[path = "agent_loop.rs"]
mod agent_loop;
#[path = "agent_store.rs"]
mod agent_store;
#[path = "context.rs"]
mod context;
#[path = "credits.rs"]
mod credits;
#[path = "loop_control.rs"]
mod loop_control;
#[path = "roles_modes_budgets.rs"]
mod roles_modes_budgets;
#[path = "run_store.rs"]
mod run_store;
#[path = "view.rs"]
mod view;
