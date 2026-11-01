//! Running a build target's **operations**: what a module does for a target on
//! request, such as generating an Android signing keystore.
//!
//! The module's code decides what to make; Saltcorn decides where it may go. An
//! operation answers files and settings, and this module writes the files into
//! the application's file store — never over an existing one — and sets the
//! settings on the stored application, so the two cannot drift apart: a
//! keystore exists exactly when the settings that name it and hold its password
//! are saved.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, merge_secrets, validate_attrs};
use serde_json::{Value as Json, json};

use crate::application::Application;
use crate::build::app_source_from_config;
use crate::declared::{clean_path, installed_frameworks};
use crate::framework::framework_config_spec;
use crate::store::save_application;

/// What running an operation did.
#[derive(Debug, Clone)]
pub struct OperationOutcome {
    /// What the module said, for the admin.
    pub message: String,
    /// The file store the files were written to.
    pub store: String,
    /// The files written, relative to the store.
    pub files: Vec<String>,
    /// Whether that store is a git repository — whose next commit would carry
    /// the files, a keystore included.
    pub git_repo: bool,
    /// The names of the settings the operation set.
    pub settings: Vec<String>,
    /// The application as saved with them.
    pub app: Application,
}

/// Run operation `operation` of `app`'s framework's target `target`.
///
/// `form_settings` is the application form's current framework settings, unsaved
/// edits included, with any secret the form holds only as the mask restored
/// from what is stored: the module sees what the admin sees (a password typed
/// but not yet saved), and its answer is applied to the **stored** settings, so
/// the admin's other unsaved edits stay theirs to save or discard.
///
/// Refused before anything is written when the operation answers a setting
/// that is not one of the target's, or a file that climbs out of the store or
/// already exists — a keystore that is replaced is an app that can no longer
/// be updated.
pub async fn run_target_operation(
    cat: &Catalog,
    app: &Application,
    target: &str,
    operation: &str,
    form_settings: &Attrs,
) -> Result<OperationOutcome> {
    let framework = &app.framework.name;
    let set = installed_frameworks();
    let decl = set.find(framework).ok_or_else(|| {
        Error::not_found(format!("framework `{framework}` declares no build targets"))
    })?;
    let options = decl
        .targets
        .iter()
        .find(|t| t.name == target)
        .map(|t| t.options.clone())
        .unwrap_or_default();
    let spec = framework_config_spec(framework)?;
    let settings = merge_secrets(&spec, &app.framework.config, form_settings);

    let source = app_source_from_config(&app.framework)?;
    let store = cat.require_file_store(&source.store.0, crate::APP_SOURCES)?;
    let context = json!({
        "app": {
            "name": app.name,
            "subdomain": app.subdomain.trim(),
        },
        "project": source.build.source_dir,
        "settings": Json::Object(settings),
    });
    let answer = set
        .call_target_operation(framework, target, operation, context)
        .await?;

    // The settings first: a stray one would be refused by the save anyway, but
    // only after the files were written.
    if let Some(name) = answer.settings.keys().find(|k| !options.contains(k)) {
        return Err(Error::config(format!(
            "operation `{operation}` of target `{target}` answered the setting `{name}`, \
             which is not one of the target's own ({})",
            options.join(", ")
        )));
    }
    let mut files = Vec::new();
    for (path, _) in &answer.files {
        let clean = clean_path(path);
        if clean.is_empty() || clean.split('/').any(|s| s == "..") {
            return Err(Error::config(format!(
                "operation `{operation}` answered a file at `{path}`, which is not a path \
                 inside the file store"
            )));
        }
        if store
            .stat(&sc_files::StorePath::parse(&clean)?)
            .await?
            .is_some()
        {
            return Err(Error::invalid(format!(
                "`{clean}` already exists in file store `{}` and was not replaced; if it is \
                 a key an app is already signed with, replacing it would stop that app from \
                 being updated. Delete or rename it first to make a new one.",
                source.store.0
            )));
        }
        files.push(clean);
    }

    let mut updated = app.clone();
    for (name, value) in &answer.settings {
        updated.framework.config.insert(name.clone(), value.clone());
    }
    validate_attrs(&spec, &updated.framework.config)?;
    // Save first, then write. A failed save then writes nothing, so a
    // generated password is never lost behind a keystore nobody can open; a
    // failed write leaves settings naming a missing file, and running the
    // operation again simply makes it.
    let saved = save_application(cat, &updated).await?;
    for ((_, bytes), path) in answer.files.iter().zip(&files) {
        store
            .write(&sc_files::StorePath::parse(path)?, bytes.clone().into())
            .await?;
    }
    Ok(OperationOutcome {
        message: answer.message,
        store: source.store.0.clone(),
        files,
        git_repo: store.is_git_repo(),
        settings: answer.settings.keys().cloned().collect(),
        app: saved,
    })
}
