//! The loaded Python module set: every stored row whose language is Python,
//! what its distribution supplies, and what is wrong with it.
//!
//! `sc_module::ModuleSet`'s counterpart, answering the **same**
//! [`LoadedModule`] — so the Modules tab, the module endpoints and
//! `merge_secrets` are written once and serve both languages without knowing
//! there are two (§8). The JavaScript loader now skips a Python row rather than
//! reporting it as unsupported, and this is where it goes instead.
//!
//! Loading is **reported, never fatal**, for the reason the other loader gives:
//! a module whose distribution is missing, whose import raises, or whose action
//! name is already taken is carried in the set with its reason so an admin can
//! see it, and every other module still works.

use std::sync::Arc;

use sc_action::ActionRegistry;
use sc_action_builtins::CodeSurfaces;
use sc_catalog::Catalog;
use sc_error::Result;
use sc_module::{LoadedModule, Module, ModuleLanguage, list_modules};
use serde_json::Value as Json;

use super::action::PyModuleAction;
use super::fields;
use super::host::PyModuleHost;

/// Every installed Python module, loaded.
pub struct PyModuleSet {
    modules: Vec<LoadedModule>,
}

impl PyModuleSet {
    /// An empty set — a server with no Python modules, and the starting point
    /// for a test.
    #[must_use]
    pub fn empty() -> PyModuleSet {
        PyModuleSet {
            modules: Vec::new(),
        }
    }

    /// Load every stored **Python** module into `host`, and register the actions
    /// they supply into `registry`.
    ///
    /// The registry is the caller's, already carrying the built-ins and
    /// whatever the JavaScript modules claimed, because the two languages share
    /// one namespace of action names: a Python module that claims `insert_row`
    /// — or a name a JavaScript module got to first — is refused by the
    /// registry, and the refusal becomes that module's issue rather than an
    /// error here. Which implementation answers to a name must not depend on
    /// the order two package managers were run in.
    pub async fn load(
        catalog: &Catalog,
        host: &Arc<PyModuleHost>,
        surfaces: &Arc<CodeSurfaces>,
        registry: &mut ActionRegistry,
    ) -> Result<PyModuleSet> {
        let stored = list_modules(catalog).await?;
        let mut modules = Vec::new();
        for module in stored {
            if module.language != ModuleLanguage::Python {
                continue;
            }
            modules.push(load_one(&module, host, surfaces, registry).await);
        }
        Ok(PyModuleSet { modules })
    }

    /// The loaded modules, in the order the store had them.
    #[must_use]
    pub fn modules(&self) -> &[LoadedModule] {
        &self.modules
    }

    /// The loaded modules, for a caller merging them into one set.
    #[must_use]
    pub fn into_modules(self) -> Vec<LoadedModule> {
        self.modules
    }
}

/// Load one module and register its actions, collecting everything that went
/// wrong instead of returning it.
async fn load_one(
    module: &Module,
    host: &Arc<PyModuleHost>,
    surfaces: &Arc<CodeSurfaces>,
    registry: &mut ActionRegistry,
) -> LoadedModule {
    let mut issues = Vec::new();
    let unloaded = |issues: Vec<String>| LoadedModule {
        module: module.clone(),
        manifest: None,
        config_spec: Vec::new(),
        issues,
    };

    // Two things about the environment are worth saying **before** the import,
    // because both produce the same symptom — nothing is importable — and have
    // different remedies. A mismatched environment is §9's refusal (rebuild it,
    // or point `--python-bin` at the interpreter this server was built against);
    // a missing distribution is a reinstall.
    if let Some(environment) = host.environment() {
        if let Err(e) = environment.check_abi() {
            issues.push(format!("its environment cannot be imported from: {e}"));
            return unloaded(issues);
        }
        if !environment.is_installed(&module.name) {
            issues.push(format!(
                "its distribution is not installed in {} — reinstall it from \
                 Settings → Modules",
                environment.dir().display()
            ));
            return unloaded(issues);
        }
    }

    let configuration = Json::Object(module.configuration.clone());
    let manifest = match host.load(&module.name, &configuration).await {
        Ok(manifest) => manifest,
        Err(e) => {
            issues.push(format!("it did not load: {}", sc_error::format_chain(&e)));
            return unloaded(issues);
        }
    };
    issues.extend(manifest.issues.iter().cloned());

    let config_spec = fields::form_fields(&manifest.config_fields);

    for action in &manifest.actions {
        let registered = PyModuleAction::new(
            &module.name,
            &action.name,
            &action.description,
            fields::form_fields(&action.config_fields),
            Arc::clone(host),
            Arc::clone(surfaces),
        );
        if let Err(e) = registry.register(Arc::new(registered)) {
            issues.push(format!(
                "its action `{}` is not available: {e}",
                action.name
            ));
        }
    }

    LoadedModule {
        module: module.clone(),
        manifest: Some(manifest),
        config_spec,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_set_supplies_nothing_and_merges_to_nothing() {
        let set = PyModuleSet::empty();
        assert!(set.modules().is_empty());
        assert!(set.into_modules().is_empty());
    }
}
