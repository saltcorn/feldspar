//! The loaded module set: every stored module, what it supplies, and what is
//! wrong with it.
//!
//! This is what a server holds between the store (rows) and the registry
//! (actions). Loading is **reported, never fatal**: a module whose package is
//! missing, whose code throws at load, or whose action name is already taken is
//! carried in the set with its reason so the Modules tab can show it, and every
//! other module still works. A server that refused to start because one module
//! was broken would be a server nobody could fix from the admin UI — which is
//! the one place the module was installed from.

use std::sync::Arc;

use sc_action::ActionRegistry;
use sc_catalog::Catalog;
use sc_core_actions::CodeSurfaces;
use sc_error::Result;
use sc_types::{Attrs, FormField};
use sc_viewpattern::{BUILTIN_PATTERNS, PatternInfo, PluginAssets};
use serde_json::{Value as Json, json};

use crate::action::ModuleAction;
use crate::host::{ModuleHost, ModuleManifest};
use crate::install::Installer;
use crate::module::Module;
use crate::spec::config_fields_to_form_fields;
use crate::store::list_modules;

/// Something wrong with a module that did not stop the rest of the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleIssue {
    /// The module it is about.
    pub module: String,
    /// What is wrong, in a sentence an admin can act on.
    pub problem: String,
}

/// One module as it stands: its row, what its package says it supplies, and its
/// issues.
#[derive(Debug, Clone)]
pub struct LoadedModule {
    /// The stored row.
    pub module: Module,
    /// What the package turned out to supply, or `None` if it would not load.
    pub manifest: Option<ModuleManifest>,
    /// The module's own settings, translated from its `configuration_workflow`.
    pub config_spec: Vec<FormField>,
    /// Everything wrong with it.
    pub issues: Vec<String>,
}

impl LoadedModule {
    /// The action names this module contributed to the registry.
    pub fn action_names(&self) -> Vec<String> {
        self.manifest
            .as_ref()
            .map(|m| m.actions.iter().map(|a| a.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The table providers this module supplies (§8.3) — what the Modules tab
    /// lists beside its actions, and what the "new table" screen offers.
    pub fn table_provider_names(&self) -> Vec<String> {
        self.manifest
            .as_ref()
            .map(|m| m.table_providers.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The model providers this module supplies (TODO "Predictive models" §14)
    /// — what the Modules tab lists beside its table providers, and what the
    /// model form offers beside the built-in regressions.
    pub fn model_provider_names(&self) -> Vec<String> {
        self.manifest
            .as_ref()
            .map(|m| m.model_providers.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The stream providers this module supplies (TODO "Streams" §12) — what
    /// the Modules tab lists beside its model providers, and what the Streams
    /// form offers beside the built-in MQTT one.
    pub fn stream_provider_names(&self) -> Vec<String> {
        self.manifest
            .as_ref()
            .map(|m| m.stream_providers.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The view patterns this module supplies that are available (TODO
    /// "Saltcorn UI" 11.1) — what the Modules tab lists, and what a view may be
    /// saved with. A pattern whose name was taken is not here; it is an issue.
    pub fn view_pattern_names(&self) -> Vec<String> {
        self.manifest
            .as_ref()
            .map(|m| m.view_patterns.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Whether the module is loaded and contributing.
    pub fn is_loaded(&self) -> bool {
        self.manifest.is_some()
    }
}

/// Every installed module, loaded.
pub struct ModuleSet {
    modules: Vec<LoadedModule>,
}

impl ModuleSet {
    /// An empty set — a server with no modules installed, and the starting point
    /// for a test.
    pub fn empty() -> ModuleSet {
        ModuleSet {
            modules: Vec::new(),
        }
    }

    /// Load every stored module into `host`, and register the actions they
    /// supply into `registry`.
    ///
    /// The registry is the caller's, already carrying the built-ins, because
    /// **a module must not be able to displace a built-in**: registration
    /// refuses a duplicate name (that is `ActionRegistry`'s rule, for exactly
    /// this reason), and the refusal becomes the module's issue rather than an
    /// error here.
    pub async fn load(
        catalog: &Catalog,
        host: &Arc<ModuleHost>,
        installer: &Installer,
        surfaces: &Arc<CodeSurfaces>,
        registry: &mut ActionRegistry,
    ) -> Result<ModuleSet> {
        let stored = list_modules(catalog).await?;
        let mut modules = Vec::with_capacity(stored.len());
        for module in stored {
            // **This loader is JavaScript's**, and a module in another language
            // is not its business: a Python module's package is a distribution
            // in the server's Python environment rather than a directory under
            // `node_modules`, and it is loaded on the embedded interpreter
            // (`sc_python::pymodule`). It is skipped rather than carried with an
            // issue, because the set it belongs in is the other one and a module
            // listed twice on the Modules tab would be worse than either.
            if module.language != crate::module::ModuleLanguage::JavaScript {
                continue;
            }
            modules.push(load_one(&module, host, installer, surfaces, registry).await);
        }
        resolve_view_patterns(&mut modules);
        Ok(ModuleSet { modules })
    }

    /// Every map tool the modules declare, as `(module, declaration)`: what
    /// the Map workspace's toolbox installs (analytics TODO A5.12).
    pub fn map_tools(&self) -> Vec<(String, serde_json::Value)> {
        self.modules
            .iter()
            .flat_map(|loaded| {
                loaded.manifest.iter().flat_map(|manifest| {
                    manifest
                        .map_tools
                        .iter()
                        .map(|tool| (loaded.module.name.clone(), tool.clone()))
                })
            })
            .collect()
    }

    /// Every module's available view patterns, as the registry a view's save is
    /// checked against (TODO "Saltcorn UI" 11.1).
    pub fn view_patterns(&self) -> Vec<PatternInfo> {
        self.modules
            .iter()
            .flat_map(|loaded| {
                loaded.manifest.iter().flat_map(|manifest| {
                    manifest.view_patterns.iter().map(|pattern| PatternInfo {
                        name: pattern.name.clone(),
                        tableless: !pattern.table_required,
                        module: Some(loaded.module.name.clone()),
                    })
                })
            })
            .collect()
    }

    /// The same patterns as `(module, pattern)`: what the view runtime's
    /// registry is installed with ([`ModuleHost::install_view_patterns`]).
    pub fn installed_view_patterns(&self) -> Vec<(String, String)> {
        self.view_patterns()
            .into_iter()
            .filter_map(|p| p.module.map(|module| (module, p.name)))
            .collect()
    }

    /// What each JavaScript module brings to a rendered document: its declared
    /// headers and its package's `public/` (11.2), under the names v1 builds
    /// its public URLs from — the plugin's own name, and the package name
    /// without its scope.
    pub fn plugin_assets(&self, installer: &Installer) -> Vec<PluginAssets> {
        self.modules
            .iter()
            .filter(|loaded| loaded.module.language == crate::module::ModuleLanguage::JavaScript)
            .filter_map(|loaded| {
                let manifest = loaded.manifest.as_ref()?;
                let public = installer.package_dir(&loaded.module.name).join("public");
                let public_dir = public.is_dir().then_some(public);
                if manifest.headers.is_empty() && public_dir.is_none() {
                    return None;
                }
                let mut names: Vec<String> = Vec::new();
                for name in [
                    manifest.plugin_name.as_deref(),
                    loaded.module.name.rsplit('/').next(),
                ]
                .into_iter()
                .flatten()
                .map(str::trim)
                {
                    if !name.is_empty() && !names.iter().any(|n| n == name) {
                        names.push(name.to_owned());
                    }
                }
                Some(PluginAssets {
                    module: loaded.module.name.clone(),
                    names,
                    version: loaded.module.version.clone().unwrap_or_default(),
                    public_dir,
                    headers: manifest.headers.clone(),
                })
            })
            .collect()
    }

    /// The same set, with another language's loaded modules in it (§8).
    ///
    /// One tab, one set of endpoints and one `module_json`, so the two loaders'
    /// answers are merged **after** both have run and before anything renders.
    /// Ordered by name across both, because "which language is it in" is not the
    /// order an admin looks for a module in.
    #[must_use]
    pub fn merged(mut self, others: Vec<LoadedModule>) -> ModuleSet {
        self.modules.extend(others);
        self.modules
            .sort_by(|a, b| a.module.name.cmp(&b.module.name));
        self
    }

    /// The loaded modules, in name order.
    pub fn modules(&self) -> &[LoadedModule] {
        &self.modules
    }

    /// One module by package name.
    pub fn get(&self, name: &str) -> Option<&LoadedModule> {
        self.modules.iter().find(|m| m.module.name == name)
    }

    /// Every issue across every module — what a boot logs and what the Modules
    /// tab shows in red.
    pub fn issues(&self) -> Vec<ModuleIssue> {
        self.modules
            .iter()
            .flat_map(|loaded| {
                loaded.issues.iter().map(|problem| ModuleIssue {
                    module: loaded.module.name.clone(),
                    problem: problem.clone(),
                })
            })
            .collect()
    }
}

/// Load one module and register its actions, collecting everything that went
/// wrong instead of returning it.
async fn load_one(
    module: &Module,
    host: &Arc<ModuleHost>,
    installer: &Installer,
    surfaces: &Arc<CodeSurfaces>,
    registry: &mut ActionRegistry,
) -> LoadedModule {
    let mut issues = Vec::new();
    let dir = installer.package_dir(&module.name);
    if !installer.is_installed(&module.name) {
        issues.push(format!(
            "its package is not installed at {} — reinstall it from Settings → Modules",
            dir.display()
        ));
        return LoadedModule {
            module: module.clone(),
            manifest: None,
            config_spec: Vec::new(),
            issues,
        };
    }

    let configuration = Json::Object(module.configuration.clone());
    let manifest = match host
        .load(&module.name, &dir, &configuration, &module.permissions)
        .await
    {
        Ok(manifest) => manifest,
        Err(e) => {
            issues.push(format!("it did not load: {}", sc_error::format_chain(&e)));
            return LoadedModule {
                module: module.clone(),
                manifest: None,
                config_spec: Vec::new(),
                issues,
            };
        }
    };
    issues.extend(manifest.issues.iter().cloned());

    let (config_spec, spec_issues) =
        config_fields_to_form_fields(&manifest.config_fields, "this module");
    issues.extend(spec_issues);

    for action in &manifest.actions {
        let (spec, action_issues) = config_fields_to_form_fields(
            &action.config_fields,
            &format!("the action `{}`", action.name),
        );
        issues.extend(action_issues);
        let registered = ModuleAction::new(
            &module.name,
            &action.name,
            &action.description,
            spec,
            Arc::clone(host),
            Arc::clone(surfaces),
        );
        if let Err(e) = registry.register(Arc::new(registered)) {
            // The built-in — or the module that got there first — keeps the
            // name. Which implementation answers to `insert_row` must not
            // depend on the order modules were installed in.
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

/// One namespace of view pattern names, as a view stores its pattern by name
/// (TODO "Saltcorn UI" §6): v1's built-in patterns first, then each module's in
/// the set's order. A name already taken costs **that pattern** — removed from
/// the module's manifest, with the reason on its card — and the module keeps
/// everything else it supplies.
fn resolve_view_patterns(modules: &mut [LoadedModule]) {
    let mut taken: Vec<(String, Option<String>)> = BUILTIN_PATTERNS
        .iter()
        .map(|name| ((*name).to_owned(), None))
        .collect();
    for loaded in modules {
        let LoadedModule {
            module,
            manifest,
            issues,
            ..
        } = loaded;
        let Some(manifest) = manifest else {
            continue;
        };
        let mut kept = Vec::new();
        for pattern in std::mem::take(&mut manifest.view_patterns) {
            match taken.iter().find(|(name, _)| *name == pattern.name) {
                Some((_, None)) => issues.push(format!(
                    "its view pattern `{}` is not available: that is the name of one of Saltcorn \
                     1's built-in view patterns, and a view stores its pattern by name",
                    pattern.name
                )),
                Some((_, Some(owner))) => issues.push(format!(
                    "its view pattern `{}` is not available: the module {owner} already supplies \
                     a view pattern of that name, and a view stores its pattern by name",
                    pattern.name
                )),
                None => {
                    taken.push((pattern.name.clone(), Some(module.name.clone())));
                    kept.push(pattern);
                }
            }
        }
        manifest.view_patterns = kept;
    }
}

/// A module's configuration, redacted for the wire: every `secret` field
/// replaced by the sentinel (§11.1).
pub fn redacted_configuration(loaded: &LoadedModule) -> Attrs {
    sc_types::redact_attrs(&loaded.config_spec, &loaded.module.configuration)
}

/// A module's unsupported-entity census, as the API reports it.
pub fn unsupported_json(loaded: &LoadedModule) -> Vec<Json> {
    loaded
        .manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .unsupported
                .iter()
                .map(|entity| json!({ "key": entity.key, "count": entity.count }))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{ActionManifest, UnsupportedEntity};
    use crate::module::ModuleSource;

    fn loaded_with(manifest: Option<ModuleManifest>, issues: Vec<String>) -> LoadedModule {
        LoadedModule {
            module: Module::new("@saltcorn/mqtt", ModuleSource::Npm, "@saltcorn/mqtt"),
            manifest,
            config_spec: Vec::new(),
            issues,
        }
    }

    fn manifest() -> ModuleManifest {
        ModuleManifest {
            name: "@saltcorn/mqtt".into(),
            api_version: Some(1),
            plugin_name: None,
            actions: vec![ActionManifest {
                name: "mqtt_publish".into(),
                description: String::new(),
                require_row: false,
                config_fields: Vec::new(),
            }],
            functions: Vec::new(),
            table_providers: Vec::new(),
            model_providers: Vec::new(),
            stream_providers: Vec::new(),
            map_tools: Vec::new(),
            frameworks: Vec::new(),
            view_patterns: Vec::new(),
            headers: Vec::new(),
            config_fields: Vec::new(),
            unsupported: vec![UnsupportedEntity {
                key: "eventTypes".into(),
                count: Some(1),
            }],
            issues: Vec::new(),
        }
    }

    #[test]
    fn a_loaded_module_reports_the_actions_it_supplies() {
        let loaded = loaded_with(Some(manifest()), Vec::new());
        assert!(loaded.is_loaded());
        assert_eq!(loaded.action_names(), vec!["mqtt_publish".to_owned()]);
    }

    #[test]
    fn a_module_that_did_not_load_supplies_nothing_and_says_why() {
        let loaded = loaded_with(None, vec!["its package is not installed".into()]);
        assert!(!loaded.is_loaded());
        assert!(loaded.action_names().is_empty());
        let set = ModuleSet {
            modules: vec![loaded],
        };
        let issues = set.issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].module, "@saltcorn/mqtt");
        assert!(issues[0].problem.contains("not installed"));
    }

    #[test]
    fn the_unsupported_census_reaches_the_wire() {
        let loaded = loaded_with(Some(manifest()), Vec::new());
        let census = unsupported_json(&loaded);
        assert_eq!(census.len(), 1);
        assert_eq!(census[0]["key"], json!("eventTypes"));
        assert_eq!(census[0]["count"], json!(1));
    }

    fn with_patterns(module: &str, patterns: &[&str]) -> LoadedModule {
        let mut manifest = manifest();
        manifest.view_patterns = patterns
            .iter()
            .map(|name| {
                serde_json::from_value(json!({ "name": name, "table_required": true })).unwrap()
            })
            .collect();
        let mut loaded = loaded_with(Some(manifest), Vec::new());
        loaded.module.name = module.to_owned();
        loaded
    }

    /// 11.1: one namespace with the built-ins, first module first; a clash
    /// costs that pattern, on that module's card, and nothing else.
    #[test]
    fn a_view_pattern_whose_name_is_taken_is_lost_with_the_reason_on_the_card() {
        let mut modules = vec![
            with_patterns("@saltcorn/kanban", &["Kanban", "KanbanAllocator"]),
            with_patterns("@acme/boards", &["Kanban", "List", "Gantt"]),
        ];
        resolve_view_patterns(&mut modules);
        let set = ModuleSet { modules };

        assert_eq!(
            set.modules[0].view_pattern_names(),
            ["Kanban", "KanbanAllocator"]
        );
        assert!(
            set.modules[0].issues.is_empty(),
            "{:?}",
            set.modules[0].issues
        );
        let boards = &set.modules[1];
        assert_eq!(boards.view_pattern_names(), ["Gantt"]);
        assert_eq!(
            boards.action_names(),
            ["mqtt_publish"],
            "the rest of the module stays"
        );
        let issues = boards.issues.join("\n");
        assert!(
            issues.contains("`Kanban` is not available: the module @saltcorn/kanban already"),
            "{issues}"
        );
        assert!(
            issues.contains(
                "`List` is not available: that is the name of one of Saltcorn 1's built-in"
            ),
            "{issues}"
        );

        let registered: Vec<(String, bool, Option<String>)> = set
            .view_patterns()
            .into_iter()
            .map(|p| (p.name, p.tableless, p.module))
            .collect();
        assert_eq!(registered.len(), 3);
        assert_eq!(
            registered[2],
            ("Gantt".to_owned(), false, Some("@acme/boards".to_owned()))
        );
        assert_eq!(
            set.installed_view_patterns(),
            [
                ("@saltcorn/kanban".to_owned(), "Kanban".to_owned()),
                ("@saltcorn/kanban".to_owned(), "KanbanAllocator".to_owned()),
                ("@acme/boards".to_owned(), "Gantt".to_owned()),
            ]
        );
    }

    /// 11.2: a module's headers and `public/`, under the names v1's URLs use.
    #[test]
    fn a_plugins_assets_are_its_headers_and_its_public_directory() {
        let root = std::env::temp_dir().join(format!("sc-module-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let installer = Installer::new(&root);
        std::fs::create_dir_all(installer.package_dir("@saltcorn/kanban").join("public")).unwrap();

        let mut kanban = with_patterns("@saltcorn/kanban", &["Kanban"]);
        kanban.module.version = Some("0.5.5".into());
        if let Some(manifest) = kanban.manifest.as_mut() {
            manifest.plugin_name = Some("kanban".into());
            manifest.headers = vec![sc_viewpattern::PluginHeader {
                script: Some("/plugins/public/kanban@0.5.5/dragula.min.js".into()),
                only_views: Some(vec!["Kanban".into()]),
                ..Default::default()
            }];
        }
        // Neither headers nor a public directory: nothing to install.
        let plain = with_patterns("@saltcorn/mqtt", &[]);
        let set = ModuleSet {
            modules: vec![kanban, plain],
        };
        let assets = set.plugin_assets(&installer);
        assert_eq!(assets.len(), 1, "{assets:?}");
        assert_eq!(assets[0].names, ["kanban"]);
        assert_eq!(assets[0].version, "0.5.5");
        assert!(
            assets[0]
                .public_dir
                .as_ref()
                .is_some_and(|d| d.ends_with("kanban/public"))
        );
        assert_eq!(assets[0].headers.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_set_has_nothing_to_say() {
        let set = ModuleSet::empty();
        assert!(set.modules().is_empty());
        assert!(set.issues().is_empty());
        assert!(set.get("@saltcorn/mqtt").is_none());
    }
}
