//! Serving applications from the Saltcorn process (design §13.2/§13.3).
//!
//! Each application is served on **its own subdomain** by its one primary
//! [`Framework`], with its API providers mounted on sub-paths beneath it. A
//! [`MountedApp`] is that triple — the [`Application`] record, the framework that
//! serves its UI, and the providers that serve its data — and [`AppMounts`] is
//! the registry the router resolves a request's `Host` against.
//!
//! **The app never touches the database.** Its framework serves static bundled
//! assets and nothing else; every byte of data it shows arrives through an
//! [`ApiProvider`], which enforces the app's declared table subset (§13.2) and
//! the §7 authorization layer. That is a structural guarantee, not a convention:
//! a [`Framework`] is handed a [`Catalog`] but [`CodeFramework`](sc_app::CodeFramework)
//! ignores it, and the app's own code is JavaScript in a browser with no route
//! to the database at all.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use sc_agent::{AppPreviewer, PreviewInfo, RunId};
use sc_api::ApiProvider;
use sc_app::{
    Application, CodeFramework, Framework, app_source_from_config, build_application,
    list_applications,
};
use sc_catalog::{Catalog, ReprojectedApp, SchemaChanged, SchemaObserver};
use sc_error::{Error, Repr, Result};

/// One application served by this process: its record, its UI framework, and its
/// API providers.
pub struct MountedApp {
    /// The application record — subdomain, table subset, CSP.
    pub app: Application,
    /// The framework serving its UI (a [`CodeFramework`](sc_app::CodeFramework)
    /// over a built bundle, for the MVP).
    pub framework: Arc<dyn Framework>,
    /// The API providers it enables, each on its own sub-path.
    pub providers: Vec<Box<dyn ApiProvider>>,
    /// The app's catalogues, as they are served (§16.1, D7): locale tag → the
    /// bytes and the ETag of `{mount}/i18n/{tag}.json`.
    ///
    /// **A cache, not the truth.** The truth is the `CatalogStore` — a file in
    /// the admin's repository or an `_fd_translations` row — and this is what
    /// keeps a page load from reading it. It is emptied when a translation is
    /// saved ([`invalidate_catalogs`](MountedApp::invalidate_catalogs)) and it
    /// is born empty on every remount, which is what makes a `SIGHUP` and a
    /// rebuild re-read without either knowing this field exists.
    catalogs: RwLock<HashMap<String, ServedCatalog>>,
}

/// One application catalogue, as bytes on the wire.
#[derive(Debug, Clone)]
pub struct ServedCatalog {
    /// The JSON body, ready to send.
    pub body: bytes::Bytes,
    /// Its entity tag, quoted, for `If-None-Match`.
    pub etag: String,
}

impl ServedCatalog {
    /// The bytes, tagged. The tag is a hash of the body, so an edit that
    /// happens to restore a previous catalogue is correctly not a change.
    pub fn new(body: bytes::Bytes) -> ServedCatalog {
        ServedCatalog {
            etag: etag_of(&body),
            body,
        }
    }
}

/// The quoted entity tag for a body: a hash of the bytes, so two responses with
/// the same content have the same tag and a change to either is a new one.
///
/// Shared by everything this server serves out of a store rather than off disk —
/// an application's catalogue and its static directories — so `If-None-Match`
/// means one thing across them.
pub fn etag_of(body: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

impl MountedApp {
    /// Mount `app`, served by `framework`, with the API providers it declares.
    ///
    /// The providers come from [`sc_app::app_providers`], so a mounted app's API
    /// is exactly the one its record declares and its generated client is typed
    /// against — there is no way to mount an API the app did not ask for.
    pub fn new(
        app: Application,
        framework: Arc<dyn Framework>,
        cat: &Catalog,
    ) -> Result<MountedApp> {
        MountedApp::new_with(app, framework, cat, None, None)
    }

    /// [`new`](MountedApp::new) with the server's JavaScript evaluator and
    /// trigger dispatcher injected into the providers, so ownership formulas'
    /// reified path (§7.3) has an engine and the app's exposed triggers (§10.2)
    /// resolve and run. The boot and refresh paths pass the [`AppMounts`]' own;
    /// a mount without an evaluator fails closed on formulas that need it, and
    /// one without a dispatcher is refused outright if the app exposes a trigger.
    pub fn new_with(
        app: Application,
        framework: Arc<dyn Framework>,
        cat: &Catalog,
        evaluator: Option<Arc<dyn sc_expr::JsEvaluator>>,
        dispatcher: Option<&Arc<sc_action::TriggerDispatcher>>,
    ) -> Result<MountedApp> {
        let providers = sc_app::app_providers_with(&app, cat, evaluator, dispatcher)?;
        Ok(MountedApp {
            app,
            framework,
            providers,
            catalogs: RwLock::new(HashMap::new()),
        })
    }

    /// The served catalogue for `tag`, if this mount has already read it.
    pub fn cached_catalog(&self, tag: &str) -> Option<ServedCatalog> {
        self.catalogs.read().ok()?.get(tag).cloned()
    }

    /// Remember `catalog` as the answer for `tag`.
    pub fn cache_catalog(&self, tag: &str, catalog: ServedCatalog) {
        if let Ok(mut cache) = self.catalogs.write() {
            cache.insert(tag.to_owned(), catalog);
        }
    }

    /// Forget every cached catalogue — what saving a translation calls, so the
    /// next request re-reads the store (D7: a translation is live without a
    /// bundler).
    pub fn invalidate_catalogs(&self) {
        if let Ok(mut cache) = self.catalogs.write() {
            cache.clear();
        }
        // And whatever the framework holds: a server-side framework looks the
        // phrases up as it renders, so its copy is the one a visitor would
        // otherwise keep seeing.
        self.framework.forget_catalogues();
    }

    /// The provider whose mount claims `path`, if any.
    ///
    /// The longest mount wins, so a provider at `/api/v2` takes precedence over
    /// one at `/api` for `/api/v2/posts` regardless of registration order.
    pub fn provider_for(&self, path: &str) -> Option<&dyn ApiProvider> {
        self.providers
            .iter()
            .filter(|p| path_under_mount(&p.mount(), path))
            .max_by_key(|p| p.mount().len())
            .map(AsRef::as_ref)
    }
}

/// The applications this server serves, resolved by subdomain — **live shared
/// state**, not a value frozen at router-build time (design §13.2 "the mount
/// registry is live; a full restart should never be required").
///
/// The registry is mutated in place through a shared handle: the boot path
/// [`mount_all`]s every stored app, and a later create/edit/delete
/// [`build_and_mount`]s or [`unmount`](AppMounts::unmount)s one app while the
/// server keeps serving the rest. The mounts live behind an `RwLock` so requests
/// read them concurrently and a mount/unmount briefly takes the write lock; each
/// [`MountedApp`] is an [`Arc`] so a reader clones its handle and drops the lock
/// rather than holding it across the request.
///
/// Empty by default: a server with no apps mounted serves only the admin API and
/// its SPA, which is exactly the MVP's default deployment.
#[derive(Default)]
pub struct AppMounts {
    /// The catalog the providers run against — and what the apps build against.
    /// `None` only for [`none`](AppMounts::none), the admin-only server that can
    /// never mount an app.
    catalog: Option<Arc<Catalog>>,
    /// The JavaScript engine ownership formulas' reified path runs on (§7.3),
    /// shared by every provider of every mount. `None` (tests, admin-only
    /// servers) fails closed where a formula would need it.
    evaluator: Option<Arc<dyn sc_expr::JsEvaluator>>,
    /// The trigger dispatcher, for the events a *request* raises rather than a
    /// row write: a successful login, and an error becoming a response (§10.2).
    ///
    /// It rides here for the same reason the catalog and the evaluator do — this
    /// is the handle the router and the admin handlers both already hold, so a
    /// server-wide service put here reaches both without a new parameter on
    /// either. `None` is a process with no triggers installed, where nothing
    /// fires: a test, or the admin-only server.
    triggers: Option<Arc<sc_action::TriggerDispatcher>>,
    /// The agent trait registry and the provider connector (§11.4), for the
    /// admin API's agent handlers and the chat socket. Here for exactly the
    /// reason the dispatcher above is: both of those already hold this handle,
    /// and both must validate an agent against the *same* trait set. `None` is a
    /// process with no agents installed — a test, or a server booted without
    /// them — and every agent surface says so rather than pretending.
    agents: Option<crate::agents::AgentServices>,
    /// The module machinery (TODO "Modules"): the npm project, the Node host,
    /// and the loaded set. Here for the reason the three above are — the admin
    /// handlers already hold this handle, and a module change has to reach the
    /// *same* dispatcher a firing trigger runs from. `None` is a process with no
    /// modules installed, where the Modules tab says so rather than pretending.
    modules: Option<Arc<crate::modules::ModuleServices>>,
    /// The model machinery (TODO "Predictive models"): the provider registry,
    /// the dataset seam and the row cap. Here for the reason the four above are
    /// — the admin handlers already hold this handle, and the `predict_row`
    /// action in the trigger registry has to predict with the *same* registry a
    /// fit was run with. `None` is a process with no models installed, where the
    /// Models tab says so rather than pretending.
    models: Option<crate::models::ModelServices>,
    /// The stream machinery (TODO "Streams"): the provider registry and the
    /// supervisor holding one subscription per enabled stream. Here for the
    /// reason the five above are — the admin handlers already hold this handle,
    /// the observe sockets subscribe to a running stream through it, and a save
    /// has to reload the *same* supervisor the trigger bridge is delivering
    /// from. `None` is a process with no streams installed, where the Streams
    /// tab says so rather than pretending.
    streams: Option<crate::streams::StreamServices>,
    /// The Python runtime this process built from its own flags (§15), for the
    /// **one** thing that needs the runtime rather than the adapter: the
    /// diagnostics on Settings → Development, which report which of §7's states
    /// this process is in, where its environment is and how many runs are
    /// resident. It rides here for the reason the four above do — the admin
    /// handlers already hold this handle. `None` is a process that booted no
    /// adapters, where the screen says the same thing it says for a binary built
    /// without Python: nothing about Python is available here.
    python: Option<Arc<sc_python::PythonRuntime>>,
    /// Where Saltcorn UI's built bundle is (`ServerConfig::saltcorn_ui_dir`).
    /// `None` is a binary built without it, where an application whose framework
    /// is `saltcorn-ui` fails to mount — once, naming the bundle.
    saltcorn_ui_dir: Option<PathBuf>,
    /// The TLS certificate to keep in step with what is mounted (§13.5).
    ///
    /// An application is served on a subdomain, and a subdomain a certificate
    /// does not cover is a name a browser refuses before a single byte of the app
    /// is read — so mounting one is not finished until the certificate has been
    /// told. `None` is every server whose certificate is not this process's to
    /// order: TLS off, or a pasted certificate.
    ///
    /// A [`OnceLock`](std::sync::OnceLock) because it is installed by
    /// [`serve`](crate::serve), after the registry the boot path already filled.
    certificate: std::sync::OnceLock<Arc<dyn crate::tls::Certificate>>,
    /// Subdomain → the app served there. Behind an `RwLock` for live mutation.
    by_subdomain: RwLock<HashMap<String, Arc<MountedApp>>>,
    /// The second registry: a coding run's **previews** of the applications it
    /// built, by label (TODO §7b). See [`mount_preview`](AppMounts::mount_preview).
    previews: RwLock<Previews>,
    /// The domain applications are served under, for a preview's host name.
    base_domain: Option<String>,
    /// How long a preview may go unused before [`sweep_previews`] removes it.
    ///
    /// [`sweep_previews`]: AppMounts::sweep_previews
    preview_idle: Duration,
}

/// One run's preview of one application.
struct Preview {
    run: RunId,
    mounted: Arc<MountedApp>,
    last_used: Instant,
}

/// The previews, and the sessions that may reach each run's.
#[derive(Default)]
struct Previews {
    by_label: HashMap<String, Preview>,
    sessions: HashMap<RunId, HashSet<String>>,
}

impl AppMounts {
    /// No applications — the admin-only server.
    pub fn none() -> AppMounts {
        AppMounts::default()
    }

    /// A registry whose apps build and run against `catalog`.
    pub fn new(catalog: Arc<Catalog>) -> AppMounts {
        // A server that can mount applications can mount every framework
        // compiled into it. Saltcorn UI is constructed rather than built, so it
        // is a factory in `sc-app`'s registry (TODO "Saltcorn UI" 5.2), and
        // installing it twice is harmless.
        if let Err(e) = sc_viewpattern::install_saltcorn_ui() {
            eprintln!("feldspar: the Saltcorn UI framework could not be registered: {e}");
        }
        AppMounts {
            catalog: Some(catalog),
            evaluator: None,
            triggers: None,
            agents: None,
            modules: None,
            models: None,
            streams: None,
            python: None,
            saltcorn_ui_dir: None,
            certificate: std::sync::OnceLock::new(),
            by_subdomain: RwLock::new(HashMap::new()),
            previews: RwLock::new(Previews::default()),
            base_domain: None,
            preview_idle: Duration::from_secs(crate::config::DEFAULT_PREVIEW_IDLE_MINUTES * 60),
        }
    }

    /// Say which domain applications are served under, so a preview has a host
    /// name (`<label>--<subdomain>.<base-domain>`).
    pub fn with_base_domain(mut self, base_domain: Option<String>) -> AppMounts {
        self.base_domain = base_domain;
        self
    }

    /// The domain applications are served under, if this deployment has one.
    pub fn base_domain(&self) -> Option<&str> {
        self.base_domain.as_deref()
    }

    /// How long a preview may go unused before the sweep removes it.
    pub fn with_preview_idle(mut self, idle: Duration) -> AppMounts {
        self.preview_idle = idle;
        self
    }

    /// Mount `app` as `run`'s **preview** of its application, beside the live
    /// mount and replacing nothing (TODO §7b).
    ///
    /// A run keeps one label per application: a later green build re-mounts
    /// under the same label, so the page the agent has open keeps working. The
    /// label is random, and it is one DNS label with the subdomain
    /// (`k3j9x2m4pq--todo`), so the wildcard DNS and certificate that cover the
    /// application cover its previews.
    pub fn mount_preview(&self, run: RunId, app: MountedApp) -> PreviewInfo {
        let subdomain = app.app.subdomain.clone();
        let mut previews = self.previews_mut();
        let label = previews
            .by_label
            .iter()
            .find(|(_, p)| p.run == run && p.mounted.app.subdomain == subdomain)
            .map(|(label, _)| label.clone())
            .unwrap_or_else(new_label);
        previews.by_label.insert(
            label.clone(),
            Preview {
                run,
                mounted: Arc::new(app),
                last_used: Instant::now(),
            },
        );
        self.preview_info(&label, &subdomain)
    }

    /// Unmount the preview under `label`. Returns whether there was one.
    pub fn unmount_preview(&self, label: &str) -> bool {
        let mut previews = self.previews_mut();
        let Some(removed) = previews.by_label.remove(label) else {
            return false;
        };
        if !previews.by_label.values().any(|p| p.run == removed.run) {
            previews.sessions.remove(&removed.run);
        }
        true
    }

    /// Unmount every preview `run` owns, and forget its sessions. Returns how
    /// many went.
    pub fn unmount_run_previews(&self, run: RunId) -> usize {
        let mut previews = self.previews_mut();
        let before = previews.by_label.len();
        previews.by_label.retain(|_, p| p.run != run);
        previews.sessions.remove(&run);
        before - previews.by_label.len()
    }

    /// Let the session `token` reach `run`'s previews: the session the run's
    /// browser context carries. No other session does.
    pub fn allow_preview_session(&self, run: RunId, token: &str) {
        self.previews_mut()
            .sessions
            .entry(run)
            .or_default()
            .insert(token.to_owned());
    }

    /// Remove every preview unused for longer than the idle time, as of `now`,
    /// returning their labels. What a crashed run leaves behind goes this way.
    pub fn sweep_previews(&self, now: Instant) -> Vec<String> {
        let idle = self.preview_idle;
        let stale: Vec<String> = self
            .previews()
            .by_label
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.last_used) > idle)
            .map(|(label, _)| label.clone())
            .collect();
        for label in &stale {
            self.unmount_preview(label);
        }
        stale
    }

    /// The preview a request to `<label>--<subdomain>` reaches, if `label` is a
    /// preview of `subdomain` and `session` is its run's session.
    ///
    /// `Err(())` is a label that **is** a preview, reached without its session:
    /// the router answers 404. `Ok(None)` is no such preview, and the host is
    /// resolved as an application subdomain as usual.
    #[allow(clippy::result_unit_err)]
    pub fn resolve_preview(
        &self,
        label: &str,
        subdomain: &str,
        session: Option<&str>,
    ) -> std::result::Result<Option<Arc<MountedApp>>, ()> {
        let mut previews = self.previews_mut();
        let Previews { by_label, sessions } = &mut *previews;
        let Some(preview) = by_label.get_mut(label) else {
            return Ok(None);
        };
        let allowed = session.is_some_and(|token| {
            sessions
                .get(&preview.run)
                .is_some_and(|tokens| tokens.contains(token))
        });
        if preview.mounted.app.subdomain != subdomain || !allowed {
            return Err(());
        }
        preview.last_used = Instant::now();
        Ok(Some(preview.mounted.clone()))
    }

    /// How many previews are mounted.
    pub fn preview_count(&self) -> usize {
        self.previews().by_label.len()
    }

    fn preview_info(&self, label: &str, subdomain: &str) -> PreviewInfo {
        let base = self.base_domain.as_deref().unwrap_or("localhost");
        PreviewInfo {
            subdomain: subdomain.to_owned(),
            label: label.to_owned(),
            host: format!("{label}--{subdomain}.{base}"),
        }
    }

    fn previews(&self) -> std::sync::RwLockReadGuard<'_, Previews> {
        self.previews.read().unwrap_or_else(|e| e.into_inner())
    }

    fn previews_mut(&self) -> std::sync::RwLockWriteGuard<'_, Previews> {
        self.previews.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Attach the server's JavaScript evaluator; every later mount and
    /// re-projection builds its providers with it.
    pub fn with_evaluator(mut self, evaluator: Arc<dyn sc_expr::JsEvaluator>) -> AppMounts {
        self.evaluator = Some(evaluator);
        self
    }

    /// The evaluator mounts are built with, for callers assembling a
    /// [`MountedApp`] by hand (tests, custom boot paths).
    pub fn evaluator(&self) -> Option<Arc<dyn sc_expr::JsEvaluator>> {
        self.evaluator.clone()
    }

    /// Attach the trigger dispatcher, so the events a request raises — a login,
    /// an error — reach the triggers listening for them.
    pub fn with_triggers(mut self, triggers: Arc<sc_action::TriggerDispatcher>) -> AppMounts {
        self.triggers = Some(triggers);
        self
    }

    /// The trigger dispatcher, if this server has one.
    pub fn triggers(&self) -> Option<&Arc<sc_action::TriggerDispatcher>> {
        self.triggers.as_ref()
    }

    /// Attach the agent services (§11.4): the trait registry every agent is
    /// validated against, and how its provider is connected.
    pub fn with_agents(mut self, agents: crate::agents::AgentServices) -> AppMounts {
        self.agents = Some(agents);
        self
    }

    /// The agent services, if this server has them.
    pub fn agents(&self) -> Option<&crate::agents::AgentServices> {
        self.agents.as_ref()
    }

    /// Attach the module services, so the Modules tab can install, configure and
    /// remove modules on the running server.
    pub fn with_modules(mut self, modules: Arc<crate::modules::ModuleServices>) -> AppMounts {
        self.modules = Some(modules);
        self
    }

    /// The module services, if this server has them.
    pub fn modules(&self) -> Option<&Arc<crate::modules::ModuleServices>> {
        self.modules.as_ref()
    }

    /// Attach the model services, so the Models tab can define, fit and predict
    /// with models on the running server.
    pub fn with_models(mut self, models: crate::models::ModelServices) -> AppMounts {
        self.models = Some(models);
        self
    }

    /// The model services, if this server has them.
    pub fn models(&self) -> Option<&crate::models::ModelServices> {
        self.models.as_ref()
    }

    /// Attach the stream services, so the Streams tab can define, observe and
    /// reload streams on the running server.
    pub fn with_streams(mut self, streams: crate::streams::StreamServices) -> AppMounts {
        self.streams = Some(streams);
        self
    }

    /// The stream services, if this server has them.
    pub fn streams(&self) -> Option<&crate::streams::StreamServices> {
        self.streams.as_ref()
    }

    /// Attach the Python runtime, so the diagnostics screen can say which of
    /// §7's states this process is in.
    pub fn with_python(mut self, python: Arc<sc_python::PythonRuntime>) -> AppMounts {
        self.python = Some(python);
        self
    }

    /// Say where Saltcorn UI's bundle is, so a `saltcorn-ui` application can
    /// mount; `None` is a build without it.
    pub fn with_saltcorn_ui_dir(mut self, dir: Option<PathBuf>) -> AppMounts {
        self.saltcorn_ui_dir = dir;
        self
    }

    /// Where Saltcorn UI's bundle is, if this server was built with one.
    pub fn saltcorn_ui_dir(&self) -> Option<&Path> {
        self.saltcorn_ui_dir.as_deref()
    }

    /// The bundle this binary was built with for the framework `name`, which is
    /// what a factory mounting it is handed.
    pub fn framework_bundle(&self, name: &str) -> Option<&Path> {
        (name == sc_viewpattern::SALTCORN_UI_FRAMEWORK)
            .then(|| self.saltcorn_ui_dir())
            .flatten()
    }

    /// The Python runtime, if this server built one.
    pub fn python(&self) -> Option<&Arc<sc_python::PythonRuntime>> {
        self.python.as_ref()
    }

    /// Say which certificate covers the applications, so mounting one on a new
    /// subdomain orders a certificate that covers it — with no restart (§13.5).
    ///
    /// Installed by [`serve`](crate::serve) rather than built in, because only
    /// the serving path knows whether this process is the one terminating TLS.
    /// A second call is ignored: there is one listener, so there is one
    /// certificate.
    ///
    /// Installing one orders nothing. The boot path mounts every stored
    /// application *before* it builds the serving plan, so the first order — the
    /// one the serving path starts — already covers them; what this registers for
    /// is the application mounted after that, which is the one a restart used to
    /// be needed for.
    pub fn set_certificate(&self, certificate: Arc<dyn crate::tls::Certificate>) {
        let _ = self.certificate.set(certificate);
    }

    /// Tell the certificate what is served now. Cheap and idempotent — the
    /// implementation compares before it orders anything — so every mutation of
    /// the registry may call it.
    fn certificate_changed(&self) {
        if let Some(certificate) = self.certificate.get() {
            certificate.subdomains_changed(&self.subdomains());
        }
    }

    /// Mount an app on its declared subdomain, refusing a collision.
    ///
    /// Two apps may not claim the same subdomain — it is how a request is routed
    /// to one of them, so a collision is a configuration error rather than a
    /// last-one-wins surprise. Use [`remount`](AppMounts::remount) to replace the
    /// app already on a subdomain (an edit or a rebuild).
    pub fn mount(&self, app: MountedApp) -> Result<()> {
        let subdomain = app.app.subdomain.clone();
        let mut mounts = self.write();
        if mounts.contains_key(&subdomain) {
            return Err(Error::config(format!(
                "two applications claim the subdomain `{subdomain}`"
            )));
        }
        mounts.insert(subdomain, Arc::new(app));
        // The lock is dropped first: ordering a certificate must not be done
        // while every reader of the registry is blocked behind it.
        drop(mounts);
        self.certificate_changed();
        Ok(())
    }

    /// Mount an app, **replacing** whatever was on its subdomain — the runtime
    /// re-mount an edit or a rebuild does.
    ///
    /// A request in flight against the previous mount keeps serving from the
    /// [`Arc`] it already cloned; the next request resolves the new one.
    pub fn remount(&self, app: MountedApp) {
        let subdomain = app.app.subdomain.clone();
        self.write().insert(subdomain, Arc::new(app));
        // A re-mount is usually the same subdomain again, where this is a no-op —
        // but an application whose subdomain was *edited* arrives here too, and
        // that is a name nothing has a certificate for.
        self.certificate_changed();
    }

    /// Unmount the app on `subdomain`, so it stops resolving. Returns whether one
    /// was there to remove.
    pub fn unmount(&self, subdomain: &str) -> bool {
        let removed = self.write().remove(subdomain).is_some();
        // Reported, and deliberately not an order: the certificate does not
        // shrink while the process runs (see [`AcmeCertificate`](crate::tls::AcmeCertificate)).
        // This is here so a rename — unmount then mount — cannot leave the
        // certificate behind whichever half runs last.
        if removed {
            self.certificate_changed();
        }
        removed
    }

    /// Re-project the API providers of every mounted app that exposes `table`,
    /// so a change to that table's access rules takes effect **with no restart
    /// and no rebuild** — the table-side counterpart to the live app remount an
    /// edit already does (§13.2, "the mount registry is live").
    ///
    /// This closes a seam that is otherwise invisible until it bites. A
    /// provider encodes a table's access rules — [`RestProvider::project`] reads
    /// `table.access` when it builds the endpoint set (§7) — and it is built
    /// from the catalog **at mount time**. So a mounted app goes on enforcing
    /// the rules the table had when it was mounted, and an admin who tightens a
    /// role watches it save and change nothing until the next restart. That is
    /// exactly the silent no-op this method exists to prevent, and it is why the
    /// table-settings handlers call it.
    ///
    /// The framework — the built bundle — is left untouched, and deliberately:
    /// an access change alters *who may reach* an app's data, not a single byte
    /// the app serves, so re-running the bundler would be wasted work. This
    /// re-projects the providers against the current catalog and keeps the
    /// existing framework, which is why it is cheap enough to run on every
    /// settings save.
    ///
    /// A provider that fails to re-project (a table the app declares has since
    /// been dropped, say) leaves that app on its previous mount and returns the
    /// error, rather than tearing a working app down over a change to an
    /// unrelated one — the same "one bad app must not take the others with it"
    /// rule [`mount_all`] follows.
    pub fn refresh_table(&self, table: &str) -> Result<Vec<ReprojectedApp>> {
        self.reproject(|app| app.tables.iter().any(|t| t.0 == table))
    }

    /// Re-project the API providers of every mounted app that exposes a trigger,
    /// so a change to the trigger set takes effect with no restart — the
    /// trigger-side counterpart of [`refresh_table`](AppMounts::refresh_table),
    /// and for the same reason.
    ///
    /// A provider encodes an exposed trigger's `min_role` as its endpoint's auth
    /// requirement, resolved from the live set **at mount time**. So without this,
    /// an admin who tightens a trigger's role watches it save and change nothing
    /// until the next restart — and, worse, an admin who *deletes* an exposed
    /// trigger leaves its endpoint mounted, answering with the dispatcher's
    /// "no trigger named" instead of not being there.
    ///
    /// Every app with a non-empty exposed subset is re-projected rather than only
    /// the ones naming the trigger that changed: the caller (a create, an update,
    /// a delete) knows which row it touched, but a *rename* changes two names at
    /// once, and re-projecting an app whose endpoints turn out identical costs a
    /// walk of its tables.
    pub fn refresh_triggers(&self) -> Result<Vec<ReprojectedApp>> {
        self.reproject(|app| !app.triggers.is_empty())
    }

    /// Re-project the providers of every mounted app matching `select`, keeping
    /// each app's built framework as it is — and rewrite those apps' generated
    /// files, so the projection in memory and the client on disk describe the
    /// same API.
    ///
    /// Returns what it did, because the caller may be a model rather than a
    /// screen: an `edit_schema` batch reports the applications it moved, and
    /// which of them are served from a bundle this **kept** rather than rebuilt
    /// (see [`ReprojectedApp`] and §13.6).
    fn reproject(&self, select: impl Fn(&Application) -> bool) -> Result<Vec<ReprojectedApp>> {
        let Some(catalog) = self.catalog() else {
            return Ok(Vec::new());
        };
        // Snapshot the affected mounts under the read lock, then rebuild outside
        // it: `MountedApp::new` touches the catalog, and holding the registry
        // lock across that is neither needed nor wise.
        let affected: Vec<Arc<MountedApp>> = self
            .read()
            .values()
            .filter(|m| select(&m.app))
            .cloned()
            .collect();
        let apps: Vec<Application> = affected.iter().map(|m| m.app.clone()).collect();
        // Read off the mount rather than the record: whether an application is
        // served from a built bundle is its *framework's* answer, and only the
        // mounted instance has one.
        let report: Vec<ReprojectedApp> = affected
            .iter()
            .map(|m| ReprojectedApp {
                id: m.app.id.0.to_string(),
                subdomain: m.app.subdomain.clone(),
                wants_build: m.framework.build().is_some(),
            })
            .collect();
        for mounted in affected {
            let refreshed = MountedApp::new_with(
                mounted.app.clone(),
                mounted.framework.clone(),
                catalog,
                self.evaluator(),
                self.triggers(),
            )?;
            self.remount(refreshed);
        }
        self.reemit_clients(apps);
        Ok(report)
    }

    /// Rewrite the generated files (`src/feldspar/**`) of each of `apps`, in the
    /// background — the automatic half of "if the API definition changes, the
    /// client code must be updated automatically" (GOALS, decision 10).
    ///
    /// A re-projection means the app's endpoint set just changed: a column
    /// added, a table's access tightened, a trigger's role. The projection in
    /// memory changes at once; without this, the `client.ts` and `schema.sql` in
    /// the app's source tree would go on describing the API as it was until
    /// somebody built, which is precisely the drift §13.1 exists to prevent.
    ///
    /// **Background and non-fatal.** The caller is a synchronous schema
    /// observer running inside somebody's admin request, and writing files
    /// through a store is neither its job nor fast; more importantly, a re-emit
    /// that fails (an unreachable store, a `code` app with no client path)
    /// must never take a mounted application down or fail the schema change
    /// that triggered it. So it is spawned, and its failures are logged in the
    /// operator's console beside every other thing that went wrong at boot.
    ///
    /// With no Tokio runtime — a unit test constructing an `AppMounts` by hand —
    /// there is nothing to spawn onto and nothing to do.
    fn reemit_clients(&self, apps: Vec<Application>) {
        if apps.is_empty() {
            return;
        }
        let (Some(catalog), Ok(handle)) = (
            self.catalog().cloned(),
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        let triggers = self.triggers().cloned();
        handle.spawn(async move {
            for app in apps {
                // A framework constructed from a factory (Saltcorn UI) has no
                // source tree and so no client to rewrite; asking would log a
                // "no build step" on every change to one.
                if sc_app::framework_factory(&app.framework.name).is_some() {
                    continue;
                }
                match sc_app::emit_app_client(&catalog, &app, triggers.as_ref()).await {
                    // An app with no generated client is a configuration, not a
                    // failure, and it says nothing.
                    Ok(_) => {}
                    Err(e) => eprintln!(
                        "feldspar: application `{}` changed, but its generated client \
                         could not be rewritten: {e}",
                        app.subdomain
                    ),
                }
            }
        });
    }

    /// The app served on `subdomain`, as an [`Arc`] the caller holds after the
    /// read lock is released.
    pub fn get(&self, subdomain: &str) -> Option<Arc<MountedApp>> {
        self.read().get(subdomain).cloned()
    }

    /// Put a changed application **record** in front of the running mount,
    /// without rebuilding anything (§16.1, task 4.4).
    ///
    /// The case this exists for is the locale set: which languages an
    /// application serves is a property of its record, the router negotiates
    /// against it, and a server-rendered framework reads it as it renders — so
    /// a mount still holding yesterday's record would go on serving English
    /// after the admin turned French on.
    ///
    /// **Not a build.** An application served from a bundle keeps its bundle
    /// and its framework; only the record and the providers are rebuilt. One
    /// that is *constructed* rather than built — Saltcorn UI — is re-mounted
    /// through its factory, because its framework holds the record itself, and
    /// for such a framework that costs no bundler.
    ///
    /// An application that is not mounted is not an error: saving a locale set
    /// on an application nobody has built yet is an ordinary thing to do.
    pub async fn refresh_mount(&self, app: Application) -> Result<()> {
        let Some(catalog) = self.catalog() else {
            return Ok(());
        };
        let Some(mounted) = self.get(&app.subdomain) else {
            return Ok(());
        };
        if mounted.framework.build().is_some() {
            let refreshed = MountedApp::new_with(
                app,
                mounted.framework.clone(),
                catalog,
                self.evaluator(),
                self.triggers(),
            )?;
            self.remount(refreshed);
            return Ok(());
        }
        build_and_mount(self, app).await.map(|_| ())
    }

    /// Drop every mounted app's cached catalogues, so the next request for one
    /// re-reads its store (§16.1, D7).
    ///
    /// Called when a translation is saved. It is a sweep over the mounts rather
    /// than a lookup by id because the admin API holds the application, not the
    /// mount, and a handful of applications is what a server has; the
    /// alternative is a second index that exists for one caller.
    pub fn invalidate_catalogs(&self, id: sc_app::AppId) {
        for mounted in self.read().values() {
            if mounted.app.id == id {
                mounted.invalidate_catalogs();
            }
        }
    }

    /// The catalog the apps' providers run against.
    pub fn catalog(&self) -> Option<&Arc<Catalog>> {
        self.catalog.as_ref()
    }

    /// Whether any app is mounted.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// The mounted subdomains, sorted.
    pub fn subdomains(&self) -> Vec<String> {
        let mut names: Vec<String> = self.read().keys().cloned().collect();
        names.sort_unstable();
        names
    }

    /// Read the mounts, recovering from a poisoned lock: a panic while mounting
    /// must not take the whole registry down — the worst a torn write leaves is a
    /// stale entry, which the next mount overwrites.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<MountedApp>>> {
        self.by_subdomain.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the mounts, recovering from a poisoned lock (see [`read`](Self::read)).
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<MountedApp>>> {
        self.by_subdomain.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// A random preview label: twelve lowercase letters and digits.
fn new_label() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

/// The previews a coding run's `check` mounts and `view_app` looks at (TODO
/// §7b), through the seam `sc-agent` declares.
#[async_trait::async_trait]
impl AppPreviewer for AppMounts {
    /// Build a [`MountedApp`] from the stored application and the bundle a green
    /// build just wrote — the same framework and API providers a real mount
    /// builds — and mount it as the run's preview.
    async fn mount_preview(
        &self,
        run: RunId,
        subdomain: &str,
        output_dir: &Path,
    ) -> Result<PreviewInfo> {
        let catalog = self.catalog().ok_or_else(|| {
            Error::config("this server was built with no catalog, so it cannot mount a preview")
        })?;
        let app = sc_app::load_application_by_subdomain(catalog, subdomain)
            .await?
            .ok_or_else(|| Error::invalid(format!("no application is served at `{subdomain}`")))?;
        let source = app_source_from_config(&app.framework)?;
        let dir = output_dir.to_owned();
        let bundle = tokio::task::spawn_blocking(move || sc_app::AssetBundle::from_dir(&dir))
            .await
            .map_err(|e| Error::msg(format!("loading the preview bundle: {e}")))??;
        let framework = Arc::new(
            CodeFramework::new(app.framework.name.clone(), bundle).with_build(source.build.clone()),
        );
        let mounted =
            MountedApp::new_with(app, framework, catalog, self.evaluator(), self.triggers())?;
        Ok(AppMounts::mount_preview(self, run, mounted))
    }

    fn preview(&self, run: RunId, subdomain: &str) -> Option<PreviewInfo> {
        let mut previews = self.previews_mut();
        let (label, preview) = previews
            .by_label
            .iter_mut()
            .find(|(_, p)| p.run == run && p.mounted.app.subdomain == subdomain)?;
        preview.last_used = Instant::now();
        let label = label.clone();
        drop(previews);
        Some(self.preview_info(&label, subdomain))
    }

    fn unmount_previews(&self, run: RunId) {
        self.unmount_run_previews(run);
    }
}

/// A mounted app re-projects when the schema underneath it moves (Phase 7).
///
/// This used to be a `refresh_table` call in each of the three admin handlers
/// that changed a schema, which worked only while an HTTP request was the only
/// way to change one. Now that an **agent** can (§11.3), the notification has to
/// come from where the change is made — `sc_api::schema_edit` — and that crate
/// cannot name this one. So the catalog carries the seam and this is the
/// implementation the server installs into it at boot; the handlers' own
/// `refresh_table` calls are gone rather than double-firing beside it.
///
/// A **dropped** table is re-projected like any other change: an app that
/// declared it must stop serving endpoints for a table that is not there, and
/// [`reproject`](AppMounts::reproject) leaving a failing app on its previous
/// mount is the right outcome — the admin sees the error and fixes the app's
/// table subset.
impl SchemaObserver for AppMounts {
    fn schema_changed(
        &self,
        _catalog: &Catalog,
        change: &SchemaChanged,
    ) -> Result<Vec<ReprojectedApp>> {
        self.refresh_table(change.table())
    }
}

/// The same arrangement for the **trigger** set, and for the same reason one
/// sentence further on: an agent carrying `admin_copilot` (§11.3) can now save
/// and delete triggers, so the re-projection cannot live in the admin handler
/// that used to be the only writer. `sc-action` — where every writer's reload
/// lands — cannot name this crate, so it carries the seam and this is what the
/// server installs into it at boot.
impl sc_action::TriggerObserver for AppMounts {
    fn triggers_changed(&self, _catalog: &Catalog) -> Result<()> {
        // The report is dropped here and only here: a trigger's `min_role` moving
        // changes an endpoint's auth requirement, not a line of the generated
        // client, so there is no stale bundle for anybody to be told about.
        self.refresh_triggers().map(|_| ())
    }
}

/// Build an application from its stored configuration and mount it live on its
/// subdomain, with **no process restart** (design §13.2).
///
/// An application whose framework is constructed rather than built — one with
/// a factory in `sc-app`'s registry, like Saltcorn UI — is mounted by its
/// factory and reports an empty build.
///
/// This is the runtime create/edit path: resolve the app's build step from its
/// framework config, run the bundler, and [`remount`](AppMounts::remount) the
/// resulting framework — replacing any earlier version on the same subdomain.
///
/// A **failed build leaves the previously mounted version serving**: the build
/// runs to completion before anything is mounted, so an `Err` here — carrying the
/// bundler's own diagnostics (§16) — never disturbs what is already up.
pub async fn build_and_mount(apps: &AppMounts, app: Application) -> Result<sc_app::BuildReport> {
    let catalog = apps.catalog().ok_or_else(|| {
        Error::config("this server was built with no catalog, so it cannot mount applications")
    })?;
    if let Some(factory) = sc_app::framework_factory(&app.framework.name) {
        // A framework with nothing to build (Saltcorn UI) is constructed. What
        // it needs to serve anything — a bundle, a runtime — is checked by the
        // factory here, on the mount, so a missing one is one line at boot (or
        // one error on save) rather than a failure on every request.
        let framework = factory
            .mount(
                &app,
                sc_app::MountContext {
                    catalog,
                    evaluator: apps.evaluator(),
                    triggers: apps.triggers().cloned(),
                    bundle_dir: apps.framework_bundle(&app.framework.name),
                },
            )
            .await
            .map_err(|e| {
                let reason = match e.repr() {
                    Repr::Config(m) | Repr::Invalid(m) => m.clone(),
                    _ => e.to_string(),
                };
                Error::config(format!("application `{}`: {reason}", app.subdomain))
            })?;
        let mounted =
            MountedApp::new_with(app, framework, catalog, apps.evaluator(), apps.triggers())?;
        apps.remount(mounted);
        // Nothing was built, and the report says so: no bundle, no output, no
        // log.
        return Ok(sc_app::BuildReport {
            bundle: sc_app::AssetBundle::new(),
            output_dir: PathBuf::new(),
            git_repo: false,
            stdout: String::new(),
            stderr: String::new(),
            client_path: None,
            installed: false,
            install_log: None,
        });
    }
    let source = app_source_from_config(&app.framework)?;
    let report = build_application(catalog, &app, &source, apps.triggers()).await?;
    // The build step travels onto the mounted framework, as
    // [`sc_app::build_code_framework`] already does it: `Framework::build()` is
    // how the rest of the process asks "is this application served from a bundle
    // somebody has to rebuild?", and a mount that dropped the answer would make
    // every re-projection report say no (§13.6).
    let framework = Arc::new(
        CodeFramework::new(app.framework.name.clone(), report.bundle.clone())
            .with_build(source.build.clone()),
    );
    let mounted = MountedApp::new_with(app, framework, catalog, apps.evaluator(), apps.triggers())?;
    apps.remount(mounted);
    Ok(report)
}

/// How much longer one application's build may ask the service manager for.
///
/// A first build of an application with a cold npm cache is minutes, not
/// seconds; this is per application and is requested again before each one, so a
/// server mounting ten of them is not racing a single deadline.
const APP_BUILD_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// Load every stored application and build + mount each — what the server does at
/// boot (design §13.2).
///
/// **A single app that fails to build must not stop the server or the other
/// apps**, so a per-app failure is logged and skipped rather than propagated: the
/// operator gets a running server with the apps that built and a clear line about
/// the one that did not, which they can fix and rebuild without a restart.
pub async fn mount_all(apps: &AppMounts) {
    let catalog = match apps.catalog() {
        Some(catalog) => catalog,
        // An admin-only server has nothing to mount.
        None => return,
    };
    let stored = match list_applications(catalog).await {
        Ok(apps) => apps,
        Err(e) => {
            eprintln!("feldspar: could not load applications to mount: {e}");
            return;
        }
    };
    // Mounting is the slow half of the boot — `build_and_mount` runs `npm
    // install` for an application whose dependencies are not on disk yet — and it
    // is the half that happens before the port opens. So each application asks
    // the service manager for more time before it starts, rather than the unit
    // carrying one `TimeoutStartSec` big enough for the worst case and useless
    // for every real failure. Where no service manager started this process, both
    // calls do nothing.
    let service = crate::systemd::ServiceManager::from_env();
    for app in stored {
        let subdomain = app.subdomain.clone();
        service.notify_status(&format!("building application `{subdomain}`"));
        service.extend_timeout(APP_BUILD_GRACE);
        match build_and_mount(apps, app).await {
            Ok(_) => eprintln!("feldspar: mounted application `{subdomain}`"),
            Err(e) => {
                eprintln!("feldspar: application `{subdomain}` failed to build, skipping: {e}")
            }
        }
    }
}

/// The application subdomain a `Host` names, given the server's base domain.
///
/// `blog.example.com` under base domain `example.com` is the app `blog`; the
/// base domain itself, a host under a different domain, or a deeper label
/// (`a.b.example.com`) is not an app — those fall through to the admin. The port
/// is ignored, so `blog.example.com:3032` resolves in local development.
///
/// Returns `None` when no base domain is configured: app routing is opt-in, and
/// guessing an app from an arbitrary `Host` header would let a request pick its
/// own app.
pub fn subdomain_of<'h>(host: &'h str, base_domain: Option<&str>) -> Option<&'h str> {
    let base = base_domain?;
    // Strip the port. An IPv6 literal host has no subdomain to find anyway, and
    // its colons would confuse this — but `[::1]` never matches a base domain.
    let host = host.split(':').next()?;
    let host = host.strip_suffix('.').unwrap_or(host); // tolerate a fully-qualified trailing dot
    let label = host.strip_suffix(base)?.strip_suffix('.')?;
    // Exactly one label: `blog` yes, `a.b` no.
    (!label.is_empty() && !label.contains('.')).then_some(label)
}

/// Whether `path` falls under a provider's `mount`.
///
/// `/api` claims `/api` and `/api/posts` but not `/apiary`; a mount of `/`
/// claims everything.
fn path_under_mount(mount: &str, path: &str) -> bool {
    if mount == "/" {
        return true;
    }
    path == mount || path.starts_with(&format!("{mount}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomain_resolution() {
        let base = Some("example.com");
        assert_eq!(subdomain_of("blog.example.com", base), Some("blog"));
        // The port is ignored, so local development resolves.
        assert_eq!(subdomain_of("blog.example.com:3032", base), Some("blog"));
        // A fully-qualified name with a trailing dot is the same host.
        assert_eq!(subdomain_of("blog.example.com.", base), Some("blog"));

        // The base domain itself is not an app.
        assert_eq!(subdomain_of("example.com", base), None);
        // A deeper name is not a single app subdomain.
        assert_eq!(subdomain_of("a.b.example.com", base), None);
        // A different domain is not ours — notably one that merely *ends* with
        // the base domain's text.
        assert_eq!(subdomain_of("blog.notexample.com", base), None);
        assert_eq!(subdomain_of("evil.com", base), None);

        // Without a configured base domain, no host names an app: a request must
        // not be able to choose its own app via the Host header.
        assert_eq!(subdomain_of("blog.example.com", None), None);
    }

    #[test]
    fn refreshing_a_table_on_an_admin_only_server_is_a_no_op() {
        // No catalog means no apps can be mounted, so there is nothing to
        // re-project — and the table-settings handlers call this unconditionally,
        // so it must be a quiet success rather than an error on that server.
        let apps = AppMounts::none();
        assert!(apps.refresh_table("posts").is_ok());
    }

    #[test]
    fn mount_prefix_matching() {
        assert!(path_under_mount("/api", "/api"));
        assert!(path_under_mount("/api", "/api/posts"));
        // A prefix that is not a path boundary does not match.
        assert!(!path_under_mount("/api", "/apiary"));
        assert!(!path_under_mount("/api", "/other"));
        // A root mount claims everything.
        assert!(path_under_mount("/", "/anything/at/all"));
    }
}
