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
// here rather than once per file, and `dead_code`-exempt because no single test
// uses all of it. `macro_use` because the module also defines `macro_rules!`
// helpers, whose textual scope has to reach the modules declared after it.
#[macro_use]
#[path = "common/mod.rs"]
mod common;

#[path = "bundled_catalog.rs"]
mod bundled_catalog;
#[path = "bundled_react_native.rs"]
mod bundled_react_native;
#[path = "bundled_rss.rs"]
mod bundled_rss;
#[path = "bundled_vue.rs"]
mod bundled_vue;
#[path = "deno_host.rs"]
mod deno_host;
#[path = "frameworks.rs"]
mod frameworks;
#[path = "host.rs"]
mod host;
#[path = "install.rs"]
mod install;
#[path = "map_tools.rs"]
mod map_tools;
#[path = "model_providers.rs"]
mod model_providers;
#[path = "module_actions.rs"]
mod module_actions;
#[path = "module_store.rs"]
mod module_store;
#[path = "pg_provider.rs"]
mod pg_provider;
#[path = "rss_provider.rs"]
mod rss_provider;
#[path = "stream_providers.rs"]
mod stream_providers;
#[path = "two_pools.rs"]
mod two_pools;
#[path = "v1_table.rs"]
mod v1_table;
#[path = "view_compat.rs"]
mod view_compat;
#[path = "view_runtime.rs"]
mod view_runtime;
