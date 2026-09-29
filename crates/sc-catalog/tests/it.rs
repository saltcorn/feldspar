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

#[path = "bootstrap_table.rs"]
mod bootstrap_table;
#[path = "catalog.rs"]
mod catalog;
#[path = "db_connections.rs"]
mod db_connections;
#[path = "field_meta_merge.rs"]
mod field_meta_merge;
#[path = "field_meta_store.rs"]
mod field_meta_store;
#[path = "file_store_live.rs"]
mod file_store_live;
#[path = "file_store_store.rs"]
mod file_store_store;
#[path = "model_calls.rs"]
mod model_calls;
#[path = "module_functions.rs"]
mod module_functions;
#[path = "provided_tables.rs"]
mod provided_tables;
#[path = "provided_writes.rs"]
mod provided_writes;
#[path = "rls_policies.rs"]
mod rls_policies;
#[path = "shared_tx.rs"]
mod shared_tx;
#[path = "sqlite_connections.rs"]
mod sqlite_connections;
#[path = "table_meta_merge.rs"]
mod table_meta_merge;
#[path = "table_meta_store.rs"]
mod table_meta_store;
