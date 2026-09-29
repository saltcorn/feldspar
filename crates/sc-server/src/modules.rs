//! Bringing **modules** up at boot, and keeping them up while the server runs
//! (TODO "Modules", phase 3).
//!
//! A module's actions are ordinary [`Action`](sc_action::Action)s in the
//! registry the dispatcher runs from, so installing one has to *change that
//! registry* — on a server that is already serving, without a restart, because
//! the Modules tab is where the module was installed from and a form that
//! appears to do nothing is the worst outcome available.
//!
//! [`ModuleServices`] is what makes that a single act:
//!
//! 1. rebuild the **base** registry (the built-ins plus the agent action) from
//!    scratch — never mutate the live one, which a firing trigger may be
//!    reading;
//! 2. load every stored module into it — **both languages**, JavaScript's on
//!    their workers and Python's on the embedded interpreter — collecting each
//!    one's issues;
//! 3. swap it into the dispatcher; and
//! 4. reload the trigger set against it, which is what turns a trigger that was
//!    broken ("unknown action `mqtt_publish`") back into a working one.
//!
//! Every step reports rather than fails. A module that will not install, will
//! not load or claims a name that is taken is carried in the set with its
//! reason; the server starts, and the admin can see and fix it.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use sc_action::TriggerDispatcher;
use sc_app::{FrameworkSet, install_frameworks};
use sc_catalog::{Catalog, TableProviderHosts};
use sc_core_actions::CodeSurfaces;
use sc_error::{Context, Error, Result};
use sc_expr::ModuleFnHosts;
use sc_module::{
    BundledModules, Installer, ModuleFrameworks, ModuleFunctions, ModuleHost, ModuleModelProviders,
    ModuleSet, ModuleStreamProviders, ModuleTableProviders, ModuleViewRuntime, bootstrap_modules,
};
use sc_python::pymodule::{
    PyModuleFunctions, PyModuleHost, PyModuleModelProviders, PyModuleSet, PyModuleTableProviders,
};

use crate::agents::AgentServices;

/// The module machinery a running server holds: the npm project, the Python
/// environment, the worker pool modules run on, and the loaded set.
pub struct ModuleServices {
    catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    agents: AgentServices,
    /// The model machinery, for the two things a module change does to it: the
    /// rebuilt action set carries `fit_model` over the *current* provider
    /// registry, and (Phase 7) a module supplying model providers replaces that
    /// registry.
    models: crate::models::ModelServices,
    /// The stream machinery, for the one thing a module change does to it: a
    /// module supplying stream providers replaces the registry a stream
    /// resolves its provider through, and the supervisor is then reloaded so a
    /// stream whose provider has just arrived starts and one whose provider has
    /// just gone says so.
    ///
    /// Behind a lock and set afterwards rather than taken at `install`, because
    /// the streams are brought up *after* the modules at boot — a stream over a
    /// module-supplied provider has to find it — so at this point they do not
    /// exist yet. `None` is that window, and a server that is not serving.
    streams: RwLock<Option<crate::streams::StreamServices>>,
    installer: Installer,
    /// The modules this server ships with, read from `plugins/` at boot
    /// (`sc_module::bundled`). Read once: the directory is part of the artifact,
    /// so it changes when the binary does and not while it runs.
    bundled: BundledModules,
    /// The Python runtime, for the half of `_fd_modules` that pip installs
    /// (§8, §9). The runtime rather than an environment, because the
    /// environment cannot be built without the embedded interpreter's version
    /// and asking the runtime for that is what makes sure there is one.
    python: Arc<sc_python::PythonRuntime>,
    host: Arc<ModuleHost>,
    /// The **Python** module host, over that same runtime: one interpreter per
    /// process, so a Python module and a Python body share it (§1).
    python_host: Arc<PyModuleHost>,
    /// The five host surfaces a module's action reaches, and the HTTP client
    /// behind `fetch` — built once here for the reason `run_python_code` builds
    /// one once: a client is a connection pool and a TLS configuration.
    surfaces: Arc<CodeSurfaces>,
    /// The loaded set, replaced whole by [`reload`](ModuleServices::reload).
    loaded: RwLock<Arc<ModuleSet>>,
}

impl ModuleServices {
    /// Bring the modules up: ensure the table, load every stored module, and
    /// swap the resulting action set into `dispatcher`.
    ///
    /// `root` is where packages are installed — `--modules-dir`, or the
    /// platform's data directory. Resolving it is **not** fatal when it fails
    /// and no module is installed: a server with no modules should not refuse to
    /// start because it could not work out where it would have put them.
    /// `workers` is how many module workers the pool runs (`--module-workers`):
    /// a module is pinned to one for its lifetime, so the reason to run a second
    /// is blast radius rather than throughput. `python` is the same runtime the
    /// dispatcher took as a code adapter — one interpreter per process, so a
    /// Python module and a Python body share it.
    ///
    /// `plugins` is where the **bundled** modules are — the artifact's own
    /// `plugins/` directory, which the binary knows the path of. `None` falls
    /// back to the checkout's, and a directory that is not there is an empty
    /// catalog rather than a failure to start.
    ///
    /// `saltcorn_ui` is the Saltcorn UI bundle directory, when this server was
    /// built with one. Its view runtime runs on this pool as the built-in
    /// `@feldspar/saltcorn-ui` and is installed as the server's view runtime
    /// (TODO "Saltcorn UI" §3) — installed, not started: nothing is imported
    /// until a view or a module needs it.
    // Eight, because eight things a server assembled before this one have to
    // reach it: three services, two directories, a worker count and a runtime.
    // Grouping them would invent a struct whose only purpose is this call.
    #[allow(clippy::too_many_arguments)]
    pub async fn install(
        catalog: &Arc<Catalog>,
        dispatcher: &Arc<TriggerDispatcher>,
        agents: &AgentServices,
        models: &crate::models::ModelServices,
        root: Option<PathBuf>,
        plugins: Option<PathBuf>,
        workers: usize,
        python: Arc<sc_python::PythonRuntime>,
        saltcorn_ui: Option<PathBuf>,
    ) -> Result<Arc<ModuleServices>> {
        bootstrap_modules(catalog)
            .await
            .context("ensuring the modules table exists")?;

        let root = match root {
            Some(root) => root,
            None => match sc_module::default_modules_root() {
                Ok(root) => root,
                Err(e) => {
                    // Report and carry on with a root nothing will be installed
                    // into: an install through the API will fail with the same
                    // message, in front of the admin who can act on it.
                    eprintln!("feldspar: {}", sc_error::format_chain(&e));
                    PathBuf::from("modules")
                }
            },
        };

        // A directory without its runtime file is refused on mount, with the
        // sentence naming the file; here it is simply a server with no runtime.
        let view_runtime = saltcorn_ui
            .as_deref()
            .and_then(|dir| sc_viewpattern::require_view_runtime(Some(dir)).ok());
        let services = Arc::new(ModuleServices {
            catalog: Arc::clone(catalog),
            dispatcher: Arc::clone(dispatcher),
            agents: agents.clone(),
            models: models.clone(),
            streams: RwLock::new(None),
            installer: Installer::new(&root),
            bundled: BundledModules::discover(plugins),
            python_host: Arc::new(PyModuleHost::new(Arc::clone(&python))),
            python,
            surfaces: Arc::new(CodeSurfaces::new()?),
            host: Arc::new(
                ModuleHost::with_workers(&root, workers).with_view_runtime(view_runtime.clone()),
            ),
            loaded: RwLock::new(Arc::new(ModuleSet::empty())),
        });
        services.reload().await?;
        // The view runtime is the bundle's and not a stored module's, so it is
        // installed once here rather than on every module reload.
        if view_runtime.is_some() {
            sc_viewpattern::install_view_runtime(Arc::new(ModuleViewRuntime::new(&services.host)))?;
        }
        for issue in services.bundled.issues() {
            eprintln!("feldspar: a bundled module could not be read: {issue}");
        }
        for issue in services.modules().issues() {
            eprintln!(
                "feldspar: module `{}` is installed but not fully usable: {}",
                issue.module, issue.problem
            );
        }
        Ok(services)
    }

    /// Rebuild the action set from the built-ins plus every stored module, swap
    /// it into the dispatcher, and reload the triggers against it.
    ///
    /// The one operation every module change goes through — install, configure,
    /// delete, and the Reload button — so there is one answer to "what happens
    /// to the live server", and no caller has to remember the four steps.
    pub async fn reload(&self) -> Result<()> {
        let mut registry = crate::triggers::base_action_registry(&self.agents, &self.models)?;
        let set = ModuleSet::load(
            &self.catalog,
            &self.host,
            &self.installer,
            &self.surfaces,
            &mut registry,
        )
        .await?;
        // And the other language's, into the **same** registry: the two share
        // one namespace of action names, so a Python module claiming a name a
        // built-in or a JavaScript module already has is refused by the registry
        // and carries the refusal as its own issue. JavaScript first, so which
        // implementation answers to a name does not depend on the order two
        // package managers were run in.
        let python = PyModuleSet::load(
            &self.catalog,
            &self.python_host,
            &self.surfaces,
            &mut registry,
        )
        .await?;
        self.dispatcher.set_registry(Arc::new(registry))?;
        // The functions the modules supply, on the catalog (§4a). Installed here
        // rather than beside the registry because they are not actions and their
        // callers are not the dispatcher: a formula hoists one through
        // `prefetch_bindings` and a code body calls one through `modfn`, and
        // what both of those hold is a `Catalog`. **Both languages'**, merged
        // into one host (§8): a formula that hoists `md_to_html` and a body that
        // writes `modfn.md_to_html(x)` must not have to know which package
        // manager put it there.
        self.catalog
            .set_module_functions(Arc::new(ModuleFnHosts::new(vec![
                Arc::new(ModuleFunctions::new(&self.host, &set)),
                Arc::new(PyModuleFunctions::new(&self.python_host, python.modules())),
            ])))?;
        // And the **table providers** (§8.3), on the same catalog and for the
        // same kind of reason: what needs them is `Catalog::reload`, which builds
        // a provided table out of its `_fd_tables` row, and `Catalog::provider`,
        // which serves its rows.
        self.catalog
            .set_table_providers(Arc::new(TableProviderHosts::new(vec![
                Arc::new(ModuleTableProviders::new(&self.host, &set)),
                Arc::new(PyModuleTableProviders::new(
                    &self.python_host,
                    python.modules(),
                )),
            ])))?;
        // And the **application frameworks** a module declares (§13.3), installed
        // into `sc-app`'s registry so the application form offers them beside
        // `react` and `code`, and so an application already stored on one is
        // configurable, buildable and scaffoldable again after a restart.
        //
        // Installed **whole** on every module change, like the action registry
        // and the table providers, and for the same reason: a framework that has
        // just been uninstalled must stop being offered, and an application on it
        // must start saying so rather than half-working.
        //
        // A declaration that could not be translated costs that framework and is
        // reported here — the module still loaded, and its actions still run.
        let frameworks = ModuleFrameworks::new(&self.host, &set);
        for issue in frameworks.issues() {
            eprintln!("feldspar: {issue}");
        }
        if let Err(e) = install_frameworks(FrameworkSet::new(Arc::new(frameworks))) {
            eprintln!(
                "feldspar: the frameworks the modules declare could not be installed, so the \
                 framework list was left as it was: {}",
                sc_error::format_chain(&e)
            );
        }
        // And the **view patterns** (TODO "Saltcorn UI" 11.1): into
        // `sc-viewpattern`'s registry, which a view's save is checked against,
        // and into the view runtime's, which renders them — both whole, so an
        // uninstalled module's pattern stops being savable and renderable at
        // once. A name clash was already decided by the set, on the module's
        // card. With them, the headers and `public/` a plugin brings (11.2).
        if let Err(e) = sc_viewpattern::install_patterns(set.view_patterns()) {
            eprintln!(
                "feldspar: the view patterns the modules supply could not be installed, so the \
                 pattern list was left as it was: {}",
                sc_error::format_chain(&e)
            );
        }
        self.host
            .install_view_patterns(&set.installed_view_patterns());
        if let Err(e) = sc_viewpattern::install_plugin_assets(set.plugin_assets(&self.installer)) {
            eprintln!(
                "feldspar: the headers and public files the modules supply could not be \
                 installed: {}",
                sc_error::format_chain(&e)
            );
        }
        // And the **model providers**, which is the third source the model
        // registry composes: the built-ins (and Stan), whatever the JavaScript
        // modules supply, and whatever the Python ones do. Rebuilt from the base
        // rather than mutated, and swapped in whole — so a fit that is already
        // running keeps the registry it started with, which is the rule the
        // action registry follows for the same reason.
        //
        // A module whose provider cannot be registered — the one real case is a
        // name a built-in or another module already has — is **reported and the
        // rest kept**: the registry refuses the duplicate naming both sources,
        // and a server that dropped every other estimator over one clash would
        // be answering a name collision with an outage.
        match self.models.base_registry() {
            Ok(mut providers) => {
                for (what, outcome) in [
                    (
                        "a JavaScript module",
                        providers
                            .register_host(Arc::new(ModuleModelProviders::new(&self.host, &set))),
                    ),
                    (
                        "a Python module",
                        providers.register_host(Arc::new(PyModuleModelProviders::new(
                            &self.python_host,
                            python.modules(),
                        ))),
                    ),
                ] {
                    if let Err(e) = outcome {
                        eprintln!(
                            "feldspar: {what}'s model providers are not all available: {}",
                            sc_error::format_chain(&e)
                        );
                    }
                }
                self.models.set_registry(Arc::new(providers));
            }
            Err(e) => eprintln!(
                "feldspar: the built-in model providers could not be registered, so the model \
                 provider set was left as it was: {}",
                sc_error::format_chain(&e)
            ),
        }
        // And the models on the catalog, for `predict("…")` and a code body's
        // `models.get` (milestone 31 §4). The services read the registry
        // swapped in above at every call, so this is the same host again —
        // installed here too so a catalog whose models were installed from
        // somewhere else ends a module change pointing at this server's.
        crate::models::install_model_host(&self.catalog, &self.models)?;
        // And the **stream providers** (TODO "Streams" §12), which is the same
        // two-source composition one entity along: the built-ins (MQTT, unless
        // it was compiled out) plus whatever the JavaScript modules supply as a
        // poll. Rebuilt whole and swapped in, so a subscription that is already
        // running keeps the provider it started with — it holds an `Arc` to the
        // code and not a name.
        //
        // The supervisor is then **reloaded**, which is what makes the swap
        // visible: a stream whose provider has just been installed starts, and
        // one whose module has just gone away becomes `failed` with the
        // sentence naming it rather than disappearing from the Streams screen.
        // Reloaded below, with the catalog, because the reload reads the rows.
        if let Some(streams) = self.streams() {
            streams.supervisor().set_registry(self.stream_registry());
        }
        // Then reload the catalog, because that is what *applies* the line
        // above: a provided table's columns are the module's answer, so
        // installing, configuring or deleting a module can change them — and a
        // module that has just been deleted leaves a table that must stop
        // claiming columns nothing can serve. The reload also costs nothing on a
        // server with no provided tables, which is why it is unconditional
        // rather than guarded by a comparison nobody could keep correct.
        self.catalog
            .reload()
            .await
            .context("reloading the catalog after a module change")?;
        // The trigger set is validated against the action registry, so a
        // trigger naming a module action was invalid until this moment.
        self.dispatcher
            .reload(&self.catalog)
            .await
            .context("reloading the triggers after a module change")?;
        // And the stream set against the registry swapped in above. Reported
        // rather than fatal, for the reason every other step here is: a module
        // change that could not restart a stream must not be a module change
        // that failed, and the stream carries its own reason on the Streams
        // screen.
        if let Some(streams) = self.streams()
            && let Err(e) = streams.reload(&self.catalog).await
        {
            eprintln!(
                "feldspar: the streams could not be reloaded after a module change, so they are \
                 running as they were: {}",
                sc_error::format_chain(&e)
            );
        }
        // One set from here up: the Modules tab, the endpoints and `module_json`
        // are written once and serve both languages (§8).
        match self.loaded.write() {
            Ok(mut guard) => *guard = Arc::new(set.merged(python.into_modules())),
            Err(_) => return Err(Error::msg("module set lock poisoned")),
        }
        Ok(())
    }

    /// The stream provider registry as the modules now stand: the built-ins
    /// plus every JavaScript module's polled providers (TODO "Streams" §12).
    ///
    /// Built **whole** every time, never mutated, which is the rule the action
    /// registry and the model providers follow for their reason: the live one
    /// may be being read by a subscription that is starting.
    ///
    /// A module whose provider cannot be registered — the one real case is a
    /// name a built-in or another module already has — is reported and the rest
    /// kept, and a failure to build the built-in set at all leaves an empty
    /// registry rather than no registry: every stream then says "unknown stream
    /// provider", which is visible, where a server that refused to reload
    /// modules over it would not be.
    pub fn stream_registry(&self) -> Arc<sc_stream::StreamRegistry> {
        let mut registry = match sc_stream::builtin_registry() {
            Ok(registry) => registry,
            Err(e) => {
                eprintln!(
                    "feldspar: the built-in stream providers could not be registered: {}",
                    sc_error::format_chain(&e)
                );
                sc_stream::StreamRegistry::new()
            }
        };
        let set = self.modules();
        if let Err(e) =
            registry.register_host(Arc::new(ModuleStreamProviders::new(&self.host, &set)))
        {
            eprintln!(
                "feldspar: a JavaScript module's stream providers are not all available: {}",
                sc_error::format_chain(&e)
            );
        }
        Arc::new(registry)
    }

    /// Tell the modules where the streams are, so a module change can rebuild
    /// their provider registry and reload them.
    ///
    /// Called once, at boot, straight after `install_streams_with` — which is
    /// after this service exists, because a stream over a module-supplied
    /// provider has to find it at boot.
    pub fn set_streams(&self, streams: crate::streams::StreamServices) {
        if let Ok(mut guard) = self.streams.write() {
            *guard = Some(streams);
        }
    }

    /// The streams, on a server that is serving.
    fn streams(&self) -> Option<crate::streams::StreamServices> {
        match self.streams.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The loaded set as it stands — what the Modules tab renders.
    pub fn modules(&self) -> Arc<ModuleSet> {
        match self.loaded.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// The npm project modules are installed into.
    /// The modules this server ships with — the catalog the Modules tab lists
    /// beside the installed ones.
    pub fn bundled(&self) -> &BundledModules {
        &self.bundled
    }

    pub fn installer(&self) -> &Installer {
        &self.installer
    }

    /// The worker pool JavaScript modules run on.
    pub fn host(&self) -> &Arc<ModuleHost> {
        &self.host
    }

    /// The interpreter Python modules run on.
    pub fn python_host(&self) -> &Arc<PyModuleHost> {
        &self.python_host
    }

    /// The Python runtime this server's Python modules install into and run on.
    pub fn python(&self) -> &Arc<sc_python::PythonRuntime> {
        &self.python
    }

    /// The Python environment, ready to install into — the virtual environment
    /// created if it was not there and checked against this process's own
    /// interpreter (§9).
    ///
    /// Fails on a server that has no interpreter to check against, with the
    /// sentence firing a Python trigger would answer: installing packages that
    /// nothing here could import is not a partial success.
    pub fn python_environment(&self) -> Result<sc_python::PythonEnvironment> {
        self.python.environment()
    }

    /// Install a module's package with whichever package manager its language
    /// uses, and answer what it turned out to be.
    ///
    /// The two installers agree on the shape of the answer — the package's own
    /// name, the version that landed, and the tool's own output — so the
    /// endpoint above them has one path and not two.
    /// A **bundled** module is a local install whose directory the server fills
    /// in: the id names an entry in the catalog, and the entry names the
    /// directory it ships in. Everything below this line then treats it as the
    /// local directory it is.
    pub async fn install_package(
        &self,
        language: sc_module::ModuleLanguage,
        source: sc_module::ModuleSource,
        location: &str,
    ) -> Result<sc_module::InstalledPackage> {
        let (source, location) = match source {
            sc_module::ModuleSource::Bundled => {
                let entry = self.bundled.require(location)?;
                (
                    sc_module::ModuleSource::Local,
                    entry.directory.display().to_string(),
                )
            }
            other => (other, location.to_owned()),
        };
        let location = location.as_str();
        match language {
            sc_module::ModuleLanguage::JavaScript => self.installer.install(source, location).await,
            sc_module::ModuleLanguage::Python => {
                let installed = self
                    .python_environment()?
                    .install(python_source(source)?, location)
                    .await?;
                Ok(sc_module::InstalledPackage {
                    name: installed.name,
                    version: installed.version,
                    log: installed.log,
                })
            }
        }
    }

    /// Take a module's package off the disk again, with the same routing.
    ///
    /// Best effort in both languages, and for the same reason: the row is what
    /// makes a module exist to this server, so a package the package manager
    /// declines to remove must not leave a module nobody can delete.
    pub async fn uninstall_package(&self, module: &sc_module::Module) -> Result<String> {
        match module.language {
            sc_module::ModuleLanguage::JavaScript => self.installer.uninstall(&module.name).await,
            sc_module::ModuleLanguage::Python => {
                self.python_environment()?.uninstall(&module.name).await
            }
        }
    }
}

/// A module source as the Python environment names it.
///
/// `npm` never reaches here — the row's language and its source are checked
/// against each other before it is stored (`save_module`) — so this is the
/// wiring mistake's error rather than the admin's.
fn python_source(source: sc_module::ModuleSource) -> Result<sc_python::PythonSource> {
    match source {
        sc_module::ModuleSource::Pypi => Ok(sc_python::PythonSource::Pypi),
        sc_module::ModuleSource::Local => Ok(sc_python::PythonSource::Local),
        sc_module::ModuleSource::Npm => Err(Error::invalid(
            "`npm` is a JavaScript module's source; pip cannot install one",
        )),
        // `install_package` resolves a bundled id to the directory it ships in
        // before anything is installed, so pip is never handed one.
        sc_module::ModuleSource::Bundled => Err(Error::invalid(
            "a bundled module's id is resolved to a directory before it is installed; pip \
             cannot install one from its id",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_services_are_shareable_and_name_their_root() {
        // Nothing is spawned or read here — the host is lazy and the installer
        // is a path — so this is the one thing worth asserting without a
        // database: that the two handles point where the caller said.
        let installer = Installer::new("/srv/modules");
        assert_eq!(installer.root(), std::path::Path::new("/srv/modules"));
        let host = ModuleHost::new("/srv/modules");
        assert_eq!(host.root(), std::path::Path::new("/srv/modules"));
    }
}
