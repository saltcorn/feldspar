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

#[path = "api_provider_config.rs"]
mod api_provider_config;
#[path = "app_client.rs"]
mod app_client;
#[path = "app_i18n.rs"]
mod app_i18n;
#[path = "app_skill.rs"]
mod app_skill;
#[path = "app_store.rs"]
mod app_store;
#[path = "build_app.rs"]
mod build_app;
#[path = "build_cache.rs"]
mod build_cache;
#[path = "custom_query_store.rs"]
mod custom_query_store;
#[path = "declared_framework.rs"]
mod declared_framework;
#[path = "graphql_scaffold.rs"]
mod graphql_scaffold;
#[path = "live_doc.rs"]
mod live_doc;
#[path = "scaffold_app.rs"]
mod scaffold_app;
#[path = "stream_registry.rs"]
mod stream_registry;
#[path = "tutorial_app_build.rs"]
mod tutorial_app_build;
