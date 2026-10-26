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

#[path = "anthropic_stream.rs"]
mod anthropic_stream;
#[path = "call_logging.rs"]
mod call_logging;
#[path = "embed.rs"]
mod embed;
#[path = "host_provider.rs"]
mod host_provider;
#[path = "openai_chat_stream.rs"]
mod openai_chat_stream;
#[path = "openai_stream.rs"]
mod openai_stream;
#[path = "provider_store.rs"]
mod provider_store;
