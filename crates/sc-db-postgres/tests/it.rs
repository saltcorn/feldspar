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

#[path = "apply_schema.rs"]
mod apply_schema;
#[path = "crud.rs"]
mod crud;
#[path = "introspect.rs"]
mod introspect;
#[path = "query.rs"]
mod query;
#[path = "transaction.rs"]
mod transaction;
#[path = "vector.rs"]
mod vector;
