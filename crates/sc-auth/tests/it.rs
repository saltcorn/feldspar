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

#[path = "auth_flow.rs"]
mod auth_flow;
#[path = "credits.rs"]
mod credits;
#[path = "db_api_tokens.rs"]
mod db_api_tokens;
#[path = "db_password_tokens.rs"]
mod db_password_tokens;
#[path = "db_sessions.rs"]
mod db_sessions;
#[path = "first_user.rs"]
mod first_user;
#[path = "login.rs"]
mod login;
#[path = "role_gate.rs"]
mod role_gate;
#[path = "role_grants.rs"]
mod role_grants;
#[path = "roles_store.rs"]
mod roles_store;
#[path = "user_management.rs"]
mod user_management;
#[path = "user_roundtrip.rs"]
mod user_roundtrip;
#[path = "users.rs"]
mod users;
