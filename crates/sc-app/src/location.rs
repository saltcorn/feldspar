//! Where an application's source is on this machine.
//!
//! Asked by three callers that must agree: `feldspar app list --json`, and the
//! `create_application` and `describe_applications` tools, which hand the
//! directory to an external coding agent so it can go on building there
//! (§13.6). Resolved as a build resolves it — the framework's settings name a
//! store and a directory in it, and the **connected** store says where that is.

use std::path::PathBuf;

use sc_catalog::Catalog;
use sc_error::{Error, Result};

use crate::application::Application;
use crate::build::app_source_from_config;
use crate::none::none_source_dir;

/// The file store and the directory inside it an application's source is in.
pub fn app_source_location(app: &Application) -> Result<(String, String)> {
    if let Some(found) = none_source_dir(&app.framework) {
        return Ok(found);
    }
    let source = app_source_from_config(&app.framework)?;
    Ok((source.store.0, source.build.source_dir))
}

/// The absolute directory `dir` of the connected store `store`.
pub fn store_dir(catalog: &Catalog, store: &str, dir: &str) -> Result<PathBuf> {
    let connected = catalog
        .file_store(store, crate::APP_SOURCES)?
        .ok_or_else(|| Error::not_found(format!("file store `{store}` is not connected")))?;
    connected.local_path(dir)?.ok_or_else(|| {
        Error::invalid(format!(
            "file store `{store}` is not on this machine's disk, so it has no project directory"
        ))
    })
}

/// The absolute project directory of `app`, through its connected store.
pub fn app_project_dir(catalog: &Catalog, app: &Application) -> Result<PathBuf> {
    let (store, dir) = app_source_location(app)?;
    store_dir(catalog, &store, &dir)
}
