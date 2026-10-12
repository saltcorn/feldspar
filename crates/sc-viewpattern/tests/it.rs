//! Every integration test in this crate, in one binary (see `sc-app`'s
//! `tests/it.rs` for why).
//!
//! Add a new test file and it is picked up here — the list is the whole wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bundle_shape.rs"]
mod bundle_shape;
#[path = "files_serve.rs"]
mod files_serve;
#[path = "library_store.rs"]
mod library_store;
#[path = "view_store.rs"]
mod view_store;
