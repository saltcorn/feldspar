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

#[path = "code_fetch.rs"]
mod code_fetch;
#[path = "code_files.rs"]
mod code_files;
#[path = "code_run_triggers.rs"]
mod code_run_triggers;
#[path = "fetch_action.rs"]
mod fetch_action;
#[path = "row_actions.rs"]
mod row_actions;
#[path = "run_js_code.rs"]
mod run_js_code;
#[path = "run_python_code.rs"]
mod run_python_code;
#[path = "send_email.rs"]
mod send_email;
#[path = "send_email_relay.rs"]
mod send_email_relay;
#[path = "tutorial_triggers.rs"]
mod tutorial_triggers;
