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

#[path = "agent_context.rs"]
mod agent_context;
#[path = "admin_copilot.rs"]
mod admin_copilot;
#[path = "admin_copilot_apps.rs"]
mod admin_copilot_apps;
#[path = "admin_copilot_build.rs"]
mod admin_copilot_build;
#[path = "admin_copilot_triggers.rs"]
mod admin_copilot_triggers;
#[path = "build_application.rs"]
mod build_application;
#[path = "builder_agent_traits.rs"]
mod builder_agent_traits;
#[path = "coding_agent.rs"]
mod coding_agent;
#[path = "coding_assets.rs"]
mod coding_assets;
#[path = "coding_check.rs"]
mod coding_check;
#[path = "coding_edits.rs"]
mod coding_edits;
#[path = "coding_images.rs"]
mod coding_images;
#[path = "coding_modules.rs"]
mod coding_modules;
#[path = "coding_plan.rs"]
mod coding_plan;
#[path = "coding_prompt.rs"]
mod coding_prompt;
#[path = "coding_repo_map.rs"]
mod coding_repo_map;
#[path = "coding_shell.rs"]
mod coding_shell;
#[path = "coding_traits.rs"]
mod coding_traits;
#[path = "docs_agents.rs"]
mod docs_agents;
#[path = "host_privileges.rs"]
mod host_privileges;
#[path = "http_fetch.rs"]
mod http_fetch;
#[path = "media.rs"]
mod media;
#[path = "query_table.rs"]
mod query_table;
#[path = "retrieval.rs"]
mod retrieval;
#[path = "reaching_out.rs"]
mod reaching_out;
#[path = "run_agent.rs"]
mod run_agent;
#[path = "library_tools.rs"]
mod library_tools;
#[path = "run_js_code.rs"]
mod run_js_code;
#[path = "run_trigger.rs"]
mod run_trigger;
#[path = "sandbox.rs"]
mod sandbox;
#[path = "subagent.rs"]
mod subagent;
#[path = "write_rows.rs"]
mod write_rows;
