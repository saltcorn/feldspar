//! Integration tests for the row-access layer, one binary for the crate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "scoped.rs"]
mod scoped;

#[path = "self_read.rs"]
mod self_read;

#[path = "time_sqlite.rs"]
mod time_sqlite;
