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

#[path = "agent_eval.rs"]
mod agent_eval;
#[path = "api_queries.rs"]
mod api_queries;
#[path = "auth_token.rs"]
mod auth_token;
#[path = "backup.rs"]
mod backup;
#[path = "build_app.rs"]
mod build_app;
#[path = "build_script.rs"]
mod build_script;
#[path = "build_static_deploy.rs"]
mod build_static_deploy;
#[path = "build_static_release.rs"]
mod build_static_release;
#[path = "cmdstan.rs"]
mod cmdstan;
#[path = "config_file.rs"]
mod config_file;
#[path = "config_values.rs"]
mod config_values;
#[path = "core_deps.rs"]
mod core_deps;
#[path = "demo.rs"]
mod demo;
#[path = "i18n.rs"]
mod i18n;
#[path = "mcp_token.rs"]
mod mcp_token;
#[path = "repo_hygiene.rs"]
mod repo_hygiene;
#[path = "serve.rs"]
mod serve;
#[path = "setup_host.rs"]
mod setup_host;
#[path = "sqlite_primary.rs"]
mod sqlite_primary;
#[path = "users.rs"]
mod users;
#[path = "whale_ci.rs"]
mod whale_ci;
