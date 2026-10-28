//! The checklist step types the JavaScript modules declare (TODO-checklists
//! C3.5), installed into `sc-check`'s registry after every module change.
//!
//! The two crates do not know each other: `sc-module` runs a module's
//! `checklist_steps` and offers its asks as the `check` surface, and
//! `sc-check` runs a [`DeclaredStep`] through a [`StepWorker`] and answers its
//! asks in the step's context. This is the bridge — a worker that is the module
//! host, and a `check` surface that is the step's asks — and the one place a
//! manifest entry is read into a declaration.

use std::sync::Arc;
use std::time::Duration;

use sc_check::{DeclaredStep, StepAsks, StepDecl, StepType, StepWorker};
use sc_error::Result;
use sc_module::{CallHosts, CheckAsks, CheckStepManifest, ModuleHost, ModuleSet};
use serde_json::Value as Json;

/// Install the step types the modules in `set` declare, in place of the last
/// set. What could not be installed is reported, one line each, and the rest
/// still are.
pub(crate) fn install_module_step_types(host: &Arc<ModuleHost>, set: &ModuleSet) {
    let worker: Arc<dyn StepWorker> = Arc::new(ModuleStepWorker {
        host: Arc::clone(host),
    });
    let mut types: Vec<Arc<dyn StepType>> = Vec::new();
    for loaded in set.modules() {
        let Some(manifest) = &loaded.manifest else {
            continue;
        };
        for step in &manifest.check_steps {
            match declaration(step, &loaded.module.name) {
                Ok(decl) => types.push(Arc::new(DeclaredStep::new(decl, Arc::clone(&worker)))),
                Err(e) => eprintln!(
                    "feldspar: the checklist step type `{}` declared by {} is not available: {e}",
                    step.name, loaded.module.name
                ),
            }
        }
    }
    for issue in sc_check::install_step_types(types) {
        eprintln!("feldspar: {issue}");
    }
}

/// One manifest entry as the declaration `sc-check` runs.
pub(crate) fn declaration(step: &CheckStepManifest, module: &str) -> Result<StepDecl> {
    // As a framework's: the translation's issues are the module's, already
    // on its card from the load.
    let (config_spec, _) = sc_module::config_fields_to_form_fields(
        &step.config_fields,
        &format!("the checklist step `{}`", step.name),
    );
    Ok(StepDecl {
        module: module.to_owned(),
        name: step.name.trim().to_owned(),
        description: step.description.clone(),
        config_spec,
        needs: sc_check::declared::parse_needs(&step.needs)?,
        mutates: step.mutates,
        framework: step
            .framework
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(str::to_owned),
        min_timeout: step
            .timeout
            .filter(|t| t.is_finite() && *t > 0.0)
            .map_or(Duration::ZERO, Duration::from_secs_f64),
    })
}

/// Runs a declared step in its module's worker.
struct ModuleStepWorker {
    host: Arc<ModuleHost>,
}

#[async_trait::async_trait]
impl StepWorker for ModuleStepWorker {
    async fn run(
        &self,
        module: &str,
        step: &str,
        request: Json,
        asks: &dyn StepAsks,
    ) -> Result<Json> {
        let bridge = AskBridge(asks);
        // The `check` surface and nothing else: a step reaches the database,
        // the router and the browser only through the run it is a step of.
        let call = CallHosts::default().with_check(&bridge);
        self.host.run_check_step(module, step, request, call).await
    }
}

/// A step's asks, as the module host's `check` surface.
struct AskBridge<'a>(&'a dyn StepAsks);

#[async_trait::async_trait]
impl CheckAsks for AskBridge<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        self.0.ask(plan).await
    }
}
