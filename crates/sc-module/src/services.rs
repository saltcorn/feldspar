//! **Module services**: what a module runs in the background, for as long as
//! it is loaded (TODO-imap M5, TECHNICAL_DESIGN §15.1c).
//!
//! A module call is request/response: an action is called with somebody's
//! authority, asks the server what it needs while that caller waits, and
//! answers. A mail module needs something else — IDLE connections held open
//! for hours, writing rows as mail arrives, with nobody waiting on anything.
//! That is a **service**:
//!
//! ```js
//! services: {
//!   sync: { start: async (ctx) => { …; return async () => { /* stop */ } } },
//! }
//! ```
//!
//! ## One call for the service's life
//!
//! A service is started with one module call, `service_start`, which the host
//! script answers only when the service stops. That is the whole trick, and it
//! is why so little here is new: the call has an id, so every `ctx.db` read and
//! write is an **ask on that call**, routed by the worker to the future awaiting
//! it — [`ModuleHost::run_service`], in [`supervise`] — exactly as an action's
//! asks are routed to the action's caller. What differs is the bounds: a
//! service's call has **no wall clock and no call budget**, because it is the
//! service's life. The JS slice and the heap bound are the worker's, and still
//! apply.
//!
//! ## The module's authority, over the module's tables
//!
//! Nobody called a service, so there is no caller to borrow authority from. It
//! runs as **the module**: the admin role, with `{ "module": <name> }` as the
//! write's user — which is what a trigger fired by its insert sees as `user`,
//! and what `own` in an `onTableChange` event is decided by. What makes that
//! acceptable without a permission key is [`ScopedTables`]: a service reaches
//! **only the tables its module declared** (TODO-imap M4). Any other table,
//! `db.sql()` (which can name any table), a Ⱶ-path that leaves the module's
//! tables, and `asUser()` (there is no user) are refused by name. Writes go
//! through the row layer, so triggers and live streams fire.
//!
//! Secret fields come back decrypted, because the row layer reveals them to
//! server-side readers (TODO-imap M2.3) — and since the reads are of the
//! module's own tables only, that *is* `ctx.secrets`: there is no second API.
//!
//! ## Restarts
//!
//! A service whose `start` throws, that calls `ctx.fail(e)`, or whose worker
//! dies (a slice or heap trip, `process.exit()`, another module's crash on a
//! shared worker) is logged and restarted with exponential backoff
//! ([`Backoff`]), and its state is on the module card ([`ServiceStatus`]). A
//! service that ran for a while before failing starts again from the shortest
//! delay. One whose module was reloaded or unloaded under it simply stops: the
//! server's reload starts the new set ([`ServiceSupervisor::start`]).
//!
//! ## Table changes
//!
//! `ctx.onTableChange(table, cb)` (§5.5) subscribes through the call's
//! `service` ask surface. [`ChangeRouter`] is a catalog [`TableEvents`]
//! listener that observes only subscribed tables — so a table nobody watches
//! costs a write nothing — and hands each **committed** write to the service
//! as a `service_event` call, in order, from a task of its own.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use sc_action::TriggerDispatcher;
use sc_api::code_host::{DEFAULT_MAX_ROWS, HostLimits, TableHost, TriggerRunHost, schema_snapshot};
use sc_auth::ROLE_ADMIN;
use sc_catalog::{Catalog, DataFieldKind, TableEvents, TableWrite, WriteOp};
use sc_error::{Error, Result};
use sc_expr::{CodeHost, CodeHosts, TriggerHost};
use serde::Serialize;
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, oneshot};

use crate::files::{FileArea, ModuleFiles, file_areas};
use crate::host::{CallHosts, ModuleHost, ServiceAsks};
use crate::module::ModuleLanguage;
use crate::modules::ModuleSet;

/// The key [`ChangeRouter`] is registered under on the catalog's write seam.
pub const SERVICE_EVENTS: &str = "module-services";

/// How long a stopping service is given to answer before its task is
/// abandoned.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// How a failed service is restarted: after `first`, doubling up to `max`, and
/// from `first` again once a run has lasted `reset_after`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// The first delay.
    pub first: Duration,
    /// The longest delay.
    pub max: Duration,
    /// A run at least this long resets the delay to `first`: it failed, but it
    /// was not failing.
    pub reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            first: Duration::from_secs(1),
            max: Duration::from_secs(300),
            reset_after: Duration::from_secs(60),
        }
    }
}

/// Where a service is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Its `service_start` call is in flight.
    Running,
    /// It failed and is waiting out its backoff.
    Restarting,
    /// It was stopped, or stopped by itself.
    Stopped,
}

/// One service, as the module card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceStatus {
    /// The module it belongs to.
    pub module: String,
    /// Its name within the module.
    pub service: String,
    /// What the module says it does.
    pub description: String,
    /// Where it is.
    pub state: ServiceState,
    /// How many times it has been restarted since this server loaded it.
    pub restarts: u32,
    /// Why it last failed, if it has.
    pub last_error: Option<String>,
}

/// The services of the loaded modules, and the tasks that keep them running.
///
/// One per server, held by `sc-server`'s `ModuleServices`: [`stop_all`] before
/// every module reload, [`start`] after it.
///
/// [`start`]: ServiceSupervisor::start
/// [`stop_all`]: ServiceSupervisor::stop_all
pub struct ServiceSupervisor {
    shared: Arc<Shared>,
    running: tokio::sync::Mutex<Vec<Running>>,
}

/// What every supervising task reaches.
struct Shared {
    catalog: Arc<Catalog>,
    host: Arc<ModuleHost>,
    /// The trigger dispatcher, for `ctx.runTrigger` — set after construction,
    /// because a server builds its dispatcher and its modules in that order but
    /// a test may have none.
    dispatcher: RwLock<Option<Arc<TriggerDispatcher>>>,
    backoff: Backoff,
    statuses: RwLock<BTreeMap<(String, String), ServiceStatus>>,
    router: Arc<ChangeRouter>,
}

/// One supervising task.
struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl ServiceSupervisor {
    /// A supervisor over `host`'s modules, writing through `catalog` — and its
    /// [`ChangeRouter`] registered on the catalog's write seam.
    pub fn new(
        catalog: &Arc<Catalog>,
        host: &Arc<ModuleHost>,
        backoff: Backoff,
    ) -> Result<ServiceSupervisor> {
        let router = Arc::new(ChangeRouter::default());
        catalog.add_table_events(SERVICE_EVENTS, Arc::clone(&router) as Arc<dyn TableEvents>)?;
        Ok(ServiceSupervisor {
            shared: Arc::new(Shared {
                catalog: Arc::clone(catalog),
                host: Arc::clone(host),
                dispatcher: RwLock::new(None),
                backoff,
                statuses: RwLock::new(BTreeMap::new()),
                router,
            }),
            running: tokio::sync::Mutex::new(Vec::new()),
        })
    }

    /// Give `ctx.runTrigger` a dispatcher to run triggers through.
    pub fn set_dispatcher(&self, dispatcher: Arc<TriggerDispatcher>) {
        if let Ok(mut guard) = self.shared.dispatcher.write() {
            *guard = Some(dispatcher);
        }
    }

    /// Start every service of every loaded JavaScript module in `set`, each on
    /// a task that restarts it when it fails. Whatever was running is stopped
    /// first.
    pub async fn start(&self, set: &ModuleSet) {
        self.stop_all().await;
        let mut running = self.running.lock().await;
        if let Ok(mut statuses) = self.shared.statuses.write() {
            statuses.clear();
        }
        for loaded in set.modules() {
            if loaded.module.language != ModuleLanguage::JavaScript {
                continue;
            }
            let Some(manifest) = loaded.manifest.as_ref() else {
                continue;
            };
            if manifest.services.is_empty() {
                continue;
            }
            let tables = Arc::new(TableScope::of(loaded));
            let areas: Arc<Vec<FileArea>> = Arc::new(file_areas(&loaded.table_declarations()));
            let roles = loaded.module.permissions.roles;
            for service in &manifest.services {
                let module = loaded.module.name.clone();
                self.shared.set_status(ServiceStatus {
                    module: module.clone(),
                    service: service.name.clone(),
                    description: service.description.clone(),
                    state: ServiceState::Running,
                    restarts: 0,
                    last_error: None,
                });
                let (stop, stopped) = oneshot::channel();
                let task = tokio::spawn(supervise(
                    Arc::clone(&self.shared),
                    module,
                    service.name.clone(),
                    Arc::clone(&tables),
                    Arc::clone(&areas),
                    roles,
                    stopped,
                ));
                running.push(Running {
                    stop: Some(stop),
                    task,
                });
            }
        }
    }

    /// Stop every running service and wait for each — its `stop` called, its
    /// call answered — for up to ten seconds apiece.
    pub async fn stop_all(&self) {
        let running: Vec<Running> = std::mem::take(&mut *self.running.lock().await);
        for mut one in running {
            if let Some(stop) = one.stop.take() {
                let _ = stop.send(());
            }
            let mut task = one.task;
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                task.abort();
            }
        }
    }

    /// Every service this server has started, in module order.
    pub fn statuses(&self) -> Vec<ServiceStatus> {
        self.shared
            .statuses
            .read()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default()
    }

    /// One module's services, for its card.
    pub fn statuses_of(&self, module: &str) -> Vec<ServiceStatus> {
        self.statuses()
            .into_iter()
            .filter(|s| s.module == module)
            .collect()
    }
}

impl Shared {
    fn set_status(&self, status: ServiceStatus) {
        if let Ok(mut statuses) = self.statuses.write() {
            statuses.insert((status.module.clone(), status.service.clone()), status);
        }
    }

    fn update_status(&self, module: &str, service: &str, change: impl FnOnce(&mut ServiceStatus)) {
        if let Ok(mut statuses) = self.statuses.write()
            && let Some(status) = statuses.get_mut(&(module.to_owned(), service.to_owned()))
        {
            change(status);
        }
    }
}

/// The user a service's writes carry: the module, by name.
fn module_user(module: &str, service: &str) -> Json {
    json!({ "module": module, "service": service })
}

/// How one run of a service ended.
enum Ended {
    /// The supervisor was asked to stop it.
    Asked,
    /// Its call answered: it was stopped from inside the worker (a reload or
    /// an unload of its module).
    ByItself,
    /// It failed.
    Failed(Error),
}

/// One service's life: run it, and when it fails, wait out the backoff and run
/// it again — until `stop` fires.
async fn supervise(
    shared: Arc<Shared>,
    module: String,
    service: String,
    tables: Arc<TableScope>,
    areas: Arc<Vec<FileArea>>,
    roles: Option<u8>,
    mut stop: oneshot::Receiver<()>,
) {
    let backoff = shared.backoff;
    let mut delay = backoff.first;
    loop {
        shared.update_status(&module, &service, |s| s.state = ServiceState::Running);
        let began = Instant::now();
        let ended = run_once(
            &shared, &module, &service, &tables, &areas, roles, &mut stop,
        )
        .await;
        shared.router.forget(&module, &service);
        let error = match ended {
            Ended::Asked | Ended::ByItself => {
                shared.update_status(&module, &service, |s| s.state = ServiceState::Stopped);
                return;
            }
            Ended::Failed(error) => sc_error::format_chain(&error),
        };
        if began.elapsed() >= backoff.reset_after {
            delay = backoff.first;
        }
        sc_log::log_error!(
            "feldspar: module `{module}`'s service `{service}` stopped: {error}. It will be \
             restarted in {} ms",
            delay.as_millis()
        );
        shared.update_status(&module, &service, |s| {
            s.state = ServiceState::Restarting;
            s.last_error = Some(error);
        });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = &mut stop => {
                shared.update_status(&module, &service, |s| s.state = ServiceState::Stopped);
                return;
            }
        }
        delay = (delay * 2).min(backoff.max);
        shared.update_status(&module, &service, |s| s.restarts += 1);
    }
}

/// One run: build the hosts the service's asks are served from, make the
/// `service_start` call, and serve it until it ends or `stop` fires.
async fn run_once(
    shared: &Shared,
    module: &str,
    service: &str,
    tables: &TableScope,
    areas: &[FileArea],
    roles: Option<u8>,
    stop: &mut oneshot::Receiver<()>,
) -> Ended {
    let ended = serve_once(shared, module, service, tables, areas, roles, stop).await;
    shared.router.forget(module, service);
    ended
}

/// [`run_once`]'s body, in a function of its own so that everything the call
/// borrows — and the event sender with it — is dropped before the router
/// forgets the run's subscriptions.
async fn serve_once(
    shared: &Shared,
    module: &str,
    service: &str,
    tables: &TableScope,
    areas: &[FileArea],
    roles: Option<u8>,
    stop: &mut oneshot::Receiver<()>,
) -> Ended {
    let catalog = &*shared.catalog;
    let user = module_user(module, service);
    let scoped = ScopedTables {
        inner: TableHost::new(catalog)
            .caused_by(ROLE_ADMIN, Some(user.clone()))
            // A service asks for as long as it runs; the row cap still holds
            // on every read.
            .with_limits(HostLimits {
                max_rows: DEFAULT_MAX_ROWS,
                max_calls: u32::MAX,
            }),
        catalog,
        module,
        tables,
        kind: "service",
    };
    let dispatcher = shared.dispatcher.read().ok().and_then(|d| d.clone());
    let triggers = dispatcher.as_deref().map(|d| {
        TriggerRunHost::new(d, catalog)
            .caused_by(ROLE_ADMIN, Some(user.clone()))
            .with_max_runs(u32::MAX)
    });
    let (events, received) = mpsc::unbounded_channel::<Json>();
    let asks = Subscriptions {
        router: &shared.router,
        module,
        service,
        tables: &tables.own,
        events,
    };
    // Delivered from a task of its own, in order: a change is a short call into
    // the worker, and the run's future below is busy serving asks.
    let delivery = tokio::spawn(deliver(
        Arc::clone(&shared.host),
        module.to_owned(),
        service.to_owned(),
        received,
    ));
    let files = ModuleFiles::new(catalog, module, areas);
    let roles = crate::roles::ModuleRoles::new(catalog, module, roles);
    let schema = schema_snapshot(catalog).ok();
    let call = CallHosts {
        hosts: CodeHosts {
            host: Some(&scoped),
            triggers: triggers.as_ref().map(|t| t as &dyn TriggerHost),
            ..CodeHosts::default()
        },
        schema: schema.as_deref(),
        views: None,
        service: Some(&asks),
        module_files: Some(&files),
        roles: Some(&roles),
        oauth: None,
    };
    let run = shared.host.run_service(module, service, call);
    tokio::pin!(run);
    let ended = tokio::select! {
        // `run` first, so its `service_start` is on its way to the worker
        // before a stop that is already waiting can send `service_stop`: a
        // stop that overtook the start would find nothing to stop, and the
        // service would start after it with nobody left to stop it.
        biased;
        outcome = &mut run => match outcome {
            Ok(_) => Ended::ByItself,
            Err(e) => Ended::Failed(e),
        },
        _ = &mut *stop => {
            // Asked to stop: tell the service, and keep serving its call —
            // a `stop` that writes a last status row is asking — until it
            // answers.
            let (_, _) = tokio::join!(
                shared.host.stop_service(module, service),
                tokio::time::timeout(STOP_GRACE, &mut run),
            );
            Ended::Asked
        }
    };
    // Everything queued is still delivered; the task ends once the last
    // sender has gone, which is when the router forgets this run.
    drop(delivery);
    ended
}

/// Hand each change to the service, one call at a time.
async fn deliver(
    host: Arc<ModuleHost>,
    module: String,
    service: String,
    mut received: mpsc::UnboundedReceiver<Json>,
) {
    while let Some(event) = received.recv().await {
        if let Err(e) = host.service_event(&module, &service, event).await {
            sc_log::log_warn!(
                "feldspar: a table change could not be handed to module `{module}`'s service \
                 `{service}`: {}",
                sc_error::format_chain(&e)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The module's own tables
// ---------------------------------------------------------------------------

/// The tables a module's service or webhook reaches: the ones it declared,
/// and the application tables its settings name for reading
/// ([`crate::reads`], TODO-payments P4.1).
pub(crate) struct TableScope {
    /// Declared: read and written.
    pub(crate) own: BTreeSet<String>,
    /// Named in its settings: read only.
    pub(crate) reads: BTreeSet<String>,
}

impl TableScope {
    pub(crate) fn of(loaded: &crate::LoadedModule) -> TableScope {
        TableScope {
            own: loaded.table_names().into_iter().collect(),
            reads: loaded.read_names().into_iter().collect(),
        }
    }
}

/// A service's `db`: a [`TableHost`] that answers only plans over the tables
/// its module declared, and reads of the ones its settings name. A webhook's
/// `ctx.db` is the same (TODO-payments P1.1).
pub(crate) struct ScopedTables<'a> {
    pub(crate) inner: TableHost<'a>,
    pub(crate) catalog: &'a Catalog,
    pub(crate) module: &'a str,
    pub(crate) tables: &'a TableScope,
    /// What is asking, for the refusals: `service` or `webhook`.
    pub(crate) kind: &'static str,
}

impl ScopedTables<'_> {
    /// Refuse a request that would reach past the module's tables, naming why.
    fn admit(&self, request: &Json) -> Result<()> {
        match request.get("op").and_then(Json::as_str) {
            Some("sql") => {
                return Err(Error::invalid(format!(
                    "a {kind} of `{}` cannot run its own SQL: a {kind} reaches only its \
                     module's tables, and SQL text can name any table — use ctx.db's chain",
                    self.module,
                    kind = self.kind
                )));
            }
            Some("models") => {
                return Err(Error::invalid(format!(
                    "a {kind} of `{}` cannot use models: a {kind} reaches only its module's \
                     tables",
                    self.module,
                    kind = self.kind
                )));
            }
            _ => {}
        }
        let table = request
            .get("table")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("this database request names no table"))?;
        let reading = matches!(
            request.get("op").and_then(Json::as_str),
            Some("select" | "aggregate")
        );
        self.own(table, None, reading)?;
        if request.get("authority").and_then(Json::as_str) == Some("user") {
            return Err(Error::invalid(format!(
                "a {} of `{}` has no caller to act as, so `asUser()` means nothing \
                 here: it writes as its module (the default), or for a named user with \
                 `forUser`",
                self.kind, self.module
            )));
        }
        let mut paths = Vec::new();
        if let Json::Object(map) = request {
            for (key, value) in map {
                // The values a write stores are data, and a mail body may
                // contain any character at all.
                if key == "values" || key == "after" || key == "pk" {
                    continue;
                }
                join_paths(value, &mut paths);
            }
        }
        for path in paths {
            self.walk(table, &path, reading)?;
        }
        Ok(())
    }

    /// `table` is one of the module's — or, for a read, one its settings name
    /// — or a refusal naming the ones that are.
    fn own(&self, table: &str, through: Option<&str>, reading: bool) -> Result<()> {
        if self.tables.own.contains(table) || (reading && self.tables.reads.contains(table)) {
            return Ok(());
        }
        if self.tables.reads.contains(table) {
            return Err(Error::invalid(format!(
                "a {} of `{}` may read `{table}`, which its settings name, but not write it",
                self.kind, self.module
            )));
        }
        let declared: Vec<String> = self.tables.own.iter().map(|t| format!("`{t}`")).collect();
        Err(Error::invalid(format!(
            "a {} of `{}` may reach only the tables its module declared ({}), and {} \
             `{table}` is not one of them",
            self.kind,
            self.module,
            if declared.is_empty() {
                "it declared none".to_owned()
            } else {
                declared.join(", ")
            },
            match through {
                Some(path) => format!("`{path}` reaches"),
                None => "this request names".to_owned(),
            }
        )))
    }

    /// Follow one Ⱶ-path from `table`, refusing a step onto a table that is
    /// not the module's. A step that is not a key is left for the table host to
    /// refuse in its own words.
    fn walk(&self, table: &str, path: &str, reading: bool) -> Result<()> {
        if path.contains('Ↄ') {
            return Err(Error::invalid(format!(
                "a {} of `{}` cannot follow a relation (`{path}`): it reads its module's \
                 tables one at a time",
                self.kind, self.module
            )));
        }
        let segments: Vec<&str> = path.split('Ⱶ').collect();
        let mut current = table.to_owned();
        for segment in &segments[..segments.len().saturating_sub(1)] {
            let Some(here) = self.catalog.get(&current)? else {
                return Ok(());
            };
            let Some(field) = here.field(segment) else {
                return Ok(());
            };
            let DataFieldKind::Key { target_table, .. } = &field.kind else {
                return Ok(());
            };
            current = target_table.0.clone();
            self.own(&current, Some(path), reading)?;
        }
        Ok(())
    }
}

/// Every Ⱶ- or Ↄ-path written anywhere in `value` — in a string or an object
/// key, inside a formula or on its own.
fn join_paths(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::String(s) => paths_in(s, out),
        Json::Array(items) => items.iter().for_each(|v| join_paths(v, out)),
        Json::Object(map) => {
            for (key, v) in map {
                paths_in(key, out);
                join_paths(v, out);
            }
        }
        _ => {}
    }
}

/// The paths in one piece of text: each maximal run of identifier characters
/// that has a Ⱶ or a Ↄ in it.
fn paths_in(text: &str, out: &mut Vec<String>) {
    let mut run = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_alphanumeric() || c == '_' || c == 'Ⱶ' || c == 'Ↄ' {
            run.push(c);
        } else {
            if run.contains('Ⱶ') || run.contains('Ↄ') {
                out.push(std::mem::take(&mut run));
            }
            run.clear();
        }
    }
}

#[async_trait::async_trait]
impl CodeHost for ScopedTables<'_> {
    async fn call(&self, request: Json) -> Result<Json> {
        self.admit(&request)?;
        self.inner.call(request).await
    }
}

// ---------------------------------------------------------------------------
// Table changes
// ---------------------------------------------------------------------------

/// The `service` ask surface of one run: `{ "subscribe": table }`.
struct Subscriptions<'a> {
    router: &'a ChangeRouter,
    module: &'a str,
    service: &'a str,
    tables: &'a BTreeSet<String>,
    events: mpsc::UnboundedSender<Json>,
}

#[async_trait::async_trait]
impl ServiceAsks for Subscriptions<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let Some(table) = plan.get("subscribe").and_then(Json::as_str) else {
            return Err(Error::invalid(format!(
                "a service can ask its supervisor to subscribe to a table's changes \
                 (`ctx.onTableChange`), and {plan} is not that"
            )));
        };
        if !self.tables.contains(table) {
            return Err(Error::invalid(format!(
                "a service of `{}` may watch only the tables its module declared, and `{table}` \
                 is not one of them",
                self.module
            )));
        }
        self.router
            .subscribe(self.module, self.service, table, self.events.clone());
        Ok(json!({ "subscribed": table }))
    }
}

/// One service's interest in one table.
struct Subscription {
    module: String,
    service: String,
    table: String,
    events: mpsc::UnboundedSender<Json>,
}

/// The catalog listener behind `ctx.onTableChange`.
#[derive(Default)]
pub struct ChangeRouter {
    subscriptions: RwLock<Vec<Subscription>>,
}

impl ChangeRouter {
    fn subscribe(
        &self,
        module: &str,
        service: &str,
        table: &str,
        events: mpsc::UnboundedSender<Json>,
    ) {
        let Ok(mut subs) = self.subscriptions.write() else {
            return;
        };
        if subs
            .iter()
            .any(|s| s.module == module && s.service == service && s.table == table)
        {
            return;
        }
        subs.push(Subscription {
            module: module.to_owned(),
            service: service.to_owned(),
            table: table.to_owned(),
            events,
        });
    }

    fn forget(&self, module: &str, service: &str) {
        if let Ok(mut subs) = self.subscriptions.write() {
            subs.retain(|s| !(s.module == module && s.service == service));
        }
    }
}

#[async_trait::async_trait]
impl TableEvents for ChangeRouter {
    fn observes(&self, table: &str, _op: WriteOp) -> bool {
        self.subscriptions
            .read()
            .is_ok_and(|subs| subs.iter().any(|s| s.table == table))
    }

    async fn emit(&self, catalog: &Catalog, write: TableWrite<'_>) -> Result<()> {
        let targets: Vec<(String, mpsc::UnboundedSender<Json>)> = match self.subscriptions.read() {
            Ok(subs) => subs
                .iter()
                .filter(|s| s.table == write.table.name)
                .map(|s| (s.module.clone(), s.events.clone()))
                .collect(),
            Err(_) => return Ok(()),
        };
        if targets.is_empty() {
            return Ok(());
        }
        let by = write
            .caller
            .and_then(|c| c.user.as_ref())
            .and_then(|u| u.get("module"))
            .and_then(Json::as_str)
            .map(str::to_owned);
        // The module's own table, so its own secrets: revealed, as a read
        // through `ctx.db` would be.
        let mut row = write.row;
        let mut old = write.old_row.unwrap_or(Json::Null);
        let _ = sc_catalog::secrets::reveal_json(catalog, write.table, &mut row);
        let _ = sc_catalog::secrets::reveal_json(catalog, write.table, &mut old);
        let (row, old) = match write.op {
            WriteOp::Delete => (Json::Null, row),
            _ => (row, old),
        };
        let table = write.table.name.clone();
        let op = write.op.as_str();
        let send = move || {
            for (module, events) in &targets {
                let _ = events.send(json!({
                    "table": table,
                    "op": op,
                    "row": row,
                    "old": old,
                    "own": by.as_deref() == Some(module.as_str()),
                }));
            }
        };
        // Only what committed is announced.
        match &write.tx {
            Some(tx) => tx.after_commit(send),
            None => send(),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_paths_are_found_in_keys_strings_and_formulas() {
        let mut out = Vec::new();
        join_paths(
            &json!({
                "accountⱵowner": 5,
                "or": [{ "formula": "messageⱵaccountⱵmin_role <= 40 && seen" }],
                "plain": "no path here",
            }),
            &mut out,
        );
        out.sort();
        assert_eq!(out, vec!["accountⱵowner", "messageⱵaccountⱵmin_role"]);
    }

    #[test]
    fn the_module_user_names_the_module_and_the_service() {
        let user = module_user("@feldspar/imap", "sync");
        assert_eq!(user["module"], json!("@feldspar/imap"));
        assert_eq!(user["service"], json!("sync"));
    }
}
