//! Every integration test in this crate, in one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "changes.rs"]
mod changes;
#[path = "doc_format.rs"]
mod doc_format;
#[path = "documents.rs"]
mod documents;
#[path = "filters.rs"]
mod filters;
#[path = "hub.rs"]
mod hub;
#[path = "publish.rs"]
mod publish;
#[path = "readers.rs"]
mod readers;
