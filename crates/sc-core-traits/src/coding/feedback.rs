//! Feedback after a model turn's edits (TODO 5.10, §6).
//!
//! After the **last** edit of a turn, not after each one, the harness does what
//! a careful person does after saving:
//!
//! 1. **Formats** the edited files with the project's own `prettier`
//!    (`node_modules/.bin/prettier`), if the project has it installed. Only the
//!    edited files, so the diff stays the model's. A file prettier changed is
//!    recorded as seen in its new form, so the next edit is not refused as stale.
//! 2. **Diagnoses**: runs the configured `diagnose` script (`typecheck` by
//!    default) when `package.json` declares it, and attaches its diagnostics to
//!    the turn's last edit result. Files the turn edited come first, and each
//!    diagnostic is marked **new** or **pre-existing**.
//!
//! New or pre-existing is decided against the **baseline** `check` keeps
//! ([`super::check::record_baseline`]): what the script reported before the
//! run's first edit, recorded just before that edit. A diagnostic is keyed by its
//! file and message, not its line, since the model's edits move lines.
//!
//! Both steps run project code, so both need the `may_check` grant. Without it
//! nothing runs, and the edit results stand as they are.

use sc_catalog::Catalog;
use sc_error::Result;
use sc_types::Attrs;
use serde_json::Value as Json;

use super::check::{describe, diagnose_script, run_script};
use super::script::{Ran, project, run_bounded};
use super::state::CodingState;
use super::{CFG_MAY_CHECK, may};
use crate::files::{FileScope, config_count};

/// How long prettier may take.
const PRETTIER_TIMEOUT_SECONDS: u64 = 60;

/// Format and diagnose the files this turn edited, and say what happened. `None`
/// when the turn edited nothing or the grant is off.
pub async fn after_edits(
    scope: &FileScope,
    config: &Attrs,
    catalog: &Catalog,
    caller: &sc_files::FileCaller,
    state_json: &mut Json,
) -> Result<Option<String>> {
    let mut state = CodingState::load(state_json);
    let edited = std::mem::take(&mut state.turn_edits);
    state.store(state_json);
    if edited.is_empty() || !may(config, CFG_MAY_CHECK) {
        return Ok(None);
    }

    let mut report = Vec::new();
    if let Some(formatted) = format(scope, catalog, &edited, &mut state).await? {
        report.push(formatted);
    }
    state.store(state_json);

    let script = diagnose_script(config);
    let edited_rel: Vec<String> = edited.iter().map(|p| scope.relative(p)).collect();
    let project = project(scope, catalog, caller).await?;
    // A project that does not declare the script is not type-checked after a
    // turn; `check` is where a missing script is a failure.
    if project.scripts.contains(&script) {
        let timeout = config_count(config, super::CFG_TIMEOUT, super::DEFAULT_TIMEOUT_SECONDS)?;
        let run = run_script(&project, &script, timeout).await?;
        report.push(describe(
            &script,
            &run.outcome,
            state.baseline.get(&script),
            &edited_rel,
            None,
        ));
    }
    Ok((!report.is_empty()).then(|| report.join("\n")))
}

/// Run the project's prettier over the edited files that still exist, and
/// record the ones it changed as seen.
async fn format(
    scope: &FileScope,
    catalog: &Catalog,
    edited: &[String],
    state: &mut CodingState,
) -> Result<Option<String>> {
    let (store, _) = scope.connect(catalog).await?;
    let Some(root) = store.local_path(&scope.resolve("")?)? else {
        return Ok(None);
    };
    let prettier = root.join("node_modules").join(".bin").join("prettier");
    if !prettier.exists() {
        return Ok(None);
    }
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new();
    for path in edited {
        if let Some(stat) = store.stat(path).await?
            && !stat.is_dir
        {
            files.push((
                path.clone(),
                scope.relative(path),
                store.read(path).await?.to_vec(),
            ));
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    let program = prettier.to_string_lossy().into_owned();
    let mut args = vec!["--write", "--ignore-unknown", "--log-level", "warn"];
    args.extend(files.iter().map(|(_, rel, _)| rel.as_str()));
    if let Ran::TimedOut = run_bounded(&root, &program, &args, PRETTIER_TIMEOUT_SECONDS).await? {
        return Ok(Some(
            "prettier: still running after 60 seconds; stopped.".to_owned(),
        ));
    }
    let mut changed = Vec::new();
    for (path, rel, before) in files {
        let after = store.read(&path).await?;
        if after.as_ref() != before.as_slice() {
            state.saw(&path, &after);
            changed.push(rel);
        }
    }
    Ok((!changed.is_empty()).then(|| format!("Formatted by prettier: {}.", changed.join(", "))))
}
