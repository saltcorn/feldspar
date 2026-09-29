//! Every integration test in this crate, in one binary.
//!
//! Each file below is still an ordinary test file — it is pulled in as a module
//! rather than compiled as its own target, for the reason the rest of the
//! workspace does it: a target per file is a link per file, and the tree these
//! link is large.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// The **environment** is a subprocess in either build (§9, phase 5), so its
// tests run in both: what the feature decides is only whether there is an
// embedded interpreter to check an environment's version against, and the one
// test about that supplies a version by hand.
#[path = "python_env.rs"]
mod python_env;

#[cfg(feature = "python-host")]
#[path = "bundled_markdown.rs"]
mod bundled_markdown;

#[cfg(feature = "python-host")]
#[path = "bundled_sklearn.rs"]
mod bundled_sklearn;

#[cfg(feature = "python-host")]
#[path = "python_db.rs"]
mod python_db;

#[cfg(feature = "python-host")]
#[path = "python_db_live.rs"]
mod python_db_live;

#[cfg(feature = "python-host")]
#[path = "python_imports.rs"]
mod python_imports;

#[cfg(feature = "python-host")]
#[path = "python_fetch.rs"]
mod python_fetch;

#[cfg(feature = "python-host")]
#[path = "python_files.rs"]
mod python_files;

#[cfg(feature = "python-host")]
#[path = "python_modfn.rs"]
mod python_modfn;

#[cfg(feature = "python-host")]
#[path = "python_models.rs"]
mod python_models;

#[cfg(feature = "python-host")]
#[path = "python_modules.rs"]
mod python_modules;

#[cfg(feature = "python-host")]
#[path = "python_runtime.rs"]
mod python_runtime;

#[cfg(feature = "python-host")]
#[path = "python_triggers.rs"]
mod python_triggers;

#[cfg(not(feature = "python-host"))]
#[path = "without_python.rs"]
mod without_python;
