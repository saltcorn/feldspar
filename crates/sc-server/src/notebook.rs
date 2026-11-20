//! Notebook sessions: running a notebook's cells (analytics TODO A10.3–A10.6).
//!
//! A notebook's code runs on the server, on the runtimes code bodies already
//! use. What a notebook adds is the **session** — Jupyter's kernel: the
//! globals one cell defines are there for the next, until the session is
//! restarted or idles out. One session per notebook and administrator:
//!
//! - **JavaScript** — an isolate of the session's own (a one-worker
//!   [`CodeRuntime`]), so its `globalThis` is the session's. Each cell is
//!   rewritten by [`sc_expr::notebook_cell`] to leave its declarations there;
//!   the `nb` prelude ([`nb::NB_JS`]) is run once when the isolate starts.
//! - **Python** — globals of the session's own in `sc-python`'s one
//!   interpreter ([`PythonRuntime::run_cell`]), the prelude ([`nb::NB_PY`])
//!   run in them once.
//! - **LLM prompt** — no globals: an agent answers the prompt with the `nb`
//!   functions as its tools ([`crate::notebook_llm`]).
//!
//! Every cell runs with a code body's bounds — the call budget, the JS slice
//! and the heap — and a wall clock of [`CELL_TIMEOUT`]. A cell that overruns
//! them is stopped and the session runs the next one: what earlier cells
//! defined is still there. **Interrupt** stops the cell that is running the
//! same way, from outside.
//!
//! What a cell reaches is `db` (as the administrator) and `nb`, answered by
//! one host ([`NbHost`]): `{ op: "nb" }` plans by [`nb::answer`] — and `fit`
//! here, because a fit is the server's job — everything else by the table
//! host a code body has.
//!
//! The sessions are a typed extension on the catalog ([`NotebookSessions`]),
//! so `Catalog::teardown` ends them with everything else, and they hold
//! nothing that holds the catalog: a run is handed it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sc_analytics::WorkspaceId;
use sc_analytics::nb;
use sc_analytics::notebook::{Cell, CellRun, NotebookLanguage, Output};
use sc_api::code_host::TableHost;
use sc_catalog::Catalog;
use sc_error::{Context as _, Error, Result};
use sc_expr::{CodeCall, CodeHost, CodeRuntime};
use sc_python::PythonRuntime;
use serde::Serialize;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::apps::AppMounts;

/// How long one cell may run: a code body's bound, longer than a trigger's
/// default because a cell may wait for a fit.
pub const CELL_TIMEOUT: Duration = Duration::from_secs(30);

/// How many sessions this server keeps at once. Past it, the one idle
/// longest is ended to make room; when every one is running a cell, a new
/// one is refused.
pub const MAX_SESSIONS: usize = 16;

/// How long a session is kept without a cell being run in it.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often idle sessions are looked for.
const SWEEP: Duration = Duration::from_secs(60);

/// How long the JavaScript prelude may take to define `nb`.
const PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);

/// A session's status, as the browser is told it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionStatus {
    /// `none` (no session: nothing is defined), `idle` or `running`.
    pub state: &'static str,
    /// The notebook's language, when there is a session.
    pub language: Option<&'static str>,
    /// The cell running.
    pub cell: Option<String>,
    /// How many cells this session has run.
    pub runs: u64,
    /// When the session started.
    pub started: Option<DateTime<Utc>>,
    /// The fit the running cell is waiting for.
    pub fit: Option<Uuid>,
}

impl SessionStatus {
    /// No session.
    pub fn none() -> SessionStatus {
        SessionStatus {
            state: "none",
            language: None,
            cell: None,
            runs: 0,
            started: None,
            fit: None,
        }
    }
}

/// Who a session is for: the notebook and the administrator.
type Key = (WorkspaceId, String);

/// Every notebook session on this server: a typed extension on the catalog.
pub struct NotebookSessions {
    sessions: Mutex<HashMap<Key, Arc<Session>>>,
    max: usize,
    idle: Duration,
}

/// One session.
pub(crate) struct Session {
    /// Its own name, which a Python session's globals are kept under.
    id: String,
    language: NotebookLanguage,
    started: DateTime<Utc>,
    /// The runtime, held while a cell runs: one cell at a time.
    kernel: tokio::sync::Mutex<Kernel>,
    live: Mutex<Live>,
    interrupt: tokio::sync::Notify,
    /// The interpreter a Python session's globals live in.
    python: Option<Arc<PythonRuntime>>,
}

/// What a session is doing, read without waiting for the cell running.
struct Live {
    runs: u64,
    cell: Option<String>,
    fit: Option<Uuid>,
    used: Instant,
}

/// A session's runtime.
enum Kernel {
    /// The isolate, started with the first cell.
    Js(Option<CodeRuntime>),
    /// The globals are `sc-python`'s, under the session's id.
    Python,
    /// Nothing is kept between prompts.
    Llm,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(python) = &self.python {
            python.end_session(&self.id);
        }
    }
}

impl Session {
    fn status(&self) -> SessionStatus {
        let live = lock(&self.live);
        SessionStatus {
            state: match live.cell {
                Some(_) => "running",
                None => "idle",
            },
            language: Some(self.language.as_str()),
            cell: live.cell.clone(),
            runs: live.runs,
            started: Some(self.started),
            fit: live.fit,
        }
    }

    fn busy(&self) -> bool {
        lock(&self.live).cell.is_some()
    }

    /// The fit the running cell waits for, or none.
    pub(crate) fn waiting_for(&self, fit: Option<Uuid>) {
        lock(&self.live).fit = fit;
    }
}

/// A lock that survives a poisoned mutex: a panicked run has failed already.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serialises the first install of the sessions on a catalog.
static INSTALL: Mutex<()> = Mutex::new(());

impl NotebookSessions {
    fn new(max: usize, idle: Duration) -> NotebookSessions {
        NotebookSessions {
            sessions: Mutex::new(HashMap::new()),
            max,
            idle,
        }
    }

    /// The catalog's sessions, installed on first use, with the sweep that
    /// ends idle ones.
    pub fn of(catalog: &Catalog) -> Arc<NotebookSessions> {
        let _installing = lock(&INSTALL);
        if let Some(sessions) = catalog.extension::<NotebookSessions>() {
            return sessions;
        }
        let sessions = Arc::new(NotebookSessions::new(MAX_SESSIONS, IDLE_TIMEOUT));
        catalog.set_extension(Some(Arc::clone(&sessions)));
        sweep(Arc::downgrade(&sessions));
        sessions
    }

    /// The status of `user`'s session of the notebook `ws`.
    pub fn status(&self, ws: WorkspaceId, user: &str) -> SessionStatus {
        lock(&self.sessions)
            .get(&(ws, user.to_owned()))
            .map_or_else(SessionStatus::none, |s| s.status())
    }

    /// End `user`'s session of `ws`: what its cells defined is gone. A cell
    /// running in it is stopped.
    pub fn restart(&self, ws: WorkspaceId, user: &str) -> SessionStatus {
        let removed = lock(&self.sessions).remove(&(ws, user.to_owned()));
        if let Some(session) = removed {
            session.interrupt.notify_waiters();
        }
        SessionStatus::none()
    }

    /// End every session of `ws` — a deleted notebook's.
    pub fn forget(&self, ws: WorkspaceId) {
        let gone: Vec<Arc<Session>> = {
            let mut all = lock(&self.sessions);
            let keys: Vec<Key> = all.keys().filter(|(w, _)| *w == ws).cloned().collect();
            keys.into_iter().filter_map(|k| all.remove(&k)).collect()
        };
        for session in gone {
            session.interrupt.notify_waiters();
        }
    }

    /// Stop the cell running in `user`'s session of `ws`, if one is.
    pub fn interrupt(&self, ws: WorkspaceId, user: &str) -> SessionStatus {
        let session = lock(&self.sessions).get(&(ws, user.to_owned())).cloned();
        match session {
            Some(session) => {
                session.interrupt.notify_waiters();
                session.status()
            }
            None => SessionStatus::none(),
        }
    }

    /// How many sessions there are.
    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// `user`'s session of `ws`, started if there is none — making room by
    /// ending the one idle longest when there are [`MAX_SESSIONS`].
    fn session(
        &self,
        ws: WorkspaceId,
        user: &str,
        language: NotebookLanguage,
        python: Option<&Arc<PythonRuntime>>,
    ) -> Result<Arc<Session>> {
        let key = (ws, user.to_owned());
        let mut all = lock(&self.sessions);
        if let Some(session) = all.get(&key) {
            if session.language == language {
                return Ok(Arc::clone(session));
            }
            all.remove(&key);
        }
        if all.len() >= self.max {
            let oldest = all
                .iter()
                .filter(|(_, s)| !s.busy())
                .min_by_key(|(_, s)| lock(&s.live).used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    all.remove(&k);
                }
                None => {
                    return Err(Error::invalid(format!(
                        "{} notebooks are running a cell right now, which is as many as this \
                         server runs at once; try again when one has finished",
                        self.max
                    )));
                }
            }
        }
        let session = Arc::new(Session {
            id: Uuid::new_v4().to_string(),
            language,
            started: Utc::now(),
            kernel: tokio::sync::Mutex::new(match language {
                NotebookLanguage::Javascript => Kernel::Js(None),
                NotebookLanguage::Python => Kernel::Python,
                NotebookLanguage::Llm => Kernel::Llm,
            }),
            live: Mutex::new(Live {
                runs: 0,
                cell: None,
                fit: None,
                used: Instant::now(),
            }),
            interrupt: tokio::sync::Notify::new(),
            python: match language {
                NotebookLanguage::Python => python.cloned(),
                _ => None,
            },
        });
        all.insert(key, Arc::clone(&session));
        Ok(session)
    }

    /// End the sessions idle for longer than they may be.
    fn sweep_idle(&self) {
        let idle = self.idle;
        let gone: Vec<Arc<Session>> = {
            let mut all = lock(&self.sessions);
            let keys: Vec<Key> = all
                .iter()
                .filter(|(_, s)| !s.busy() && lock(&s.live).used.elapsed() > idle)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter().filter_map(|k| all.remove(&k)).collect()
        };
        drop(gone);
    }

    /// Run one code cell of the notebook `ws` in `user`'s session, answering
    /// its outputs and its run. `earlier` is the cells above it — an LLM
    /// prompt's context.
    pub(crate) async fn run(
        &self,
        cx: &RunContext<'_>,
        cell: &Cell,
        earlier: &[Cell],
    ) -> Result<CellResult> {
        let session = self.session(cx.workspace, cx.user, cx.language, cx.apps.python())?;
        let mut kernel = session.kernel.lock().await;
        let count = {
            let mut live = lock(&session.live);
            live.runs += 1;
            live.cell = Some(cell.id.clone());
            live.used = Instant::now();
            live.runs
        };
        let began = Instant::now();
        let ran = async {
            match &mut *kernel {
                Kernel::Js(runtime) => run_js(cx, &session, runtime, &cell.source).await,
                Kernel::Python => run_python(cx, &session, &cell.source).await,
                Kernel::Llm => crate::notebook_llm::run(cx, &session, cell, earlier).await,
            }
        };
        let outputs = tokio::select! {
            outputs = ran => outputs,
            () = session.interrupt.notified() => {
                vec![Output::Error {
                    message: "Interrupted: the cell was stopped before it finished.".to_owned(),
                    line: None,
                }]
            }
        };
        let ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        {
            let mut live = lock(&session.live);
            live.cell = None;
            live.fit = None;
            live.used = Instant::now();
        }
        drop(kernel);
        let failed = outputs.iter().any(|o| matches!(o, Output::Error { .. }));
        Ok(CellResult {
            outputs,
            run: CellRun {
                count,
                at: Utc::now(),
                ms: Some(ms),
            },
            failed,
            status: session.status(),
        })
    }
}

/// End idle sessions every [`SWEEP`], for as long as the sessions exist.
#[allow(
    clippy::disallowed_methods,
    reason = "tokio::spawn: the sweep outlives the request that installed the sessions, and ends with them (it holds them weakly)"
)]
fn sweep(sessions: Weak<NotebookSessions>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP).await;
            match sessions.upgrade() {
                Some(sessions) => sessions.sweep_idle(),
                None => return,
            }
        }
    });
}

/// What one run of a cell made.
#[derive(Debug, Clone)]
pub(crate) struct CellResult {
    pub(crate) outputs: Vec<Output>,
    pub(crate) run: CellRun,
    /// Whether it stopped with an error.
    pub(crate) failed: bool,
    pub(crate) status: SessionStatus,
}

/// What a cell runs with: the catalog, the server's services, and who.
pub(crate) struct RunContext<'a> {
    pub(crate) catalog: &'a Catalog,
    pub(crate) apps: &'a AppMounts,
    pub(crate) workspace: WorkspaceId,
    /// The session's owner, as a key.
    pub(crate) user: &'a str,
    pub(crate) language: NotebookLanguage,
    /// The administrator, as the row layer's proof.
    pub(crate) admin: sc_catalog::AdminPrincipal,
    /// The administrator, as an agent's caller.
    pub(crate) run_caller: sc_agent::RunCaller,
    /// Ask the LLM again rather than replaying a prompt's calls.
    pub(crate) regenerate: bool,
}

impl RunContext<'_> {
    /// Who a cell's reads are read as.
    pub(crate) fn caller(&self) -> sc_dataset::Caller {
        sc_dataset::Caller::admin(&self.admin)
    }
}

/// Run a JavaScript cell on the session's isolate, starting it first.
async fn run_js(
    cx: &RunContext<'_>,
    session: &Session,
    runtime: &mut Option<CodeRuntime>,
    source: &str,
) -> Vec<Output> {
    let body = match sc_expr::notebook_cell(source) {
        Ok(body) => body,
        Err(e) => return vec![error_output(e.message())],
    };
    if runtime.is_none() {
        let started = CodeRuntime::with_workers_and_inflight(1, 1);
        let prelude = CodeCall {
            code: nb::NB_JS.to_owned(),
            timeout: Some(PRELUDE_TIMEOUT),
            ..CodeCall::default()
        };
        if let Err(e) = started.run(prelude).await {
            return vec![error_output(&format!(
                "the notebook could not start: {}",
                e.message()
            ))];
        }
        *runtime = Some(started);
    }
    let Some(runtime) = runtime.as_ref() else {
        return vec![error_output("the notebook could not start")];
    };
    let host = NbHost::new(cx, session);
    let call = CodeCall {
        code: body,
        host: Some(&host),
        timeout: Some(CELL_TIMEOUT),
        ..CodeCall::default()
    };
    let ran = runtime.run(call).await;
    answered(cx, ran).await
}

/// Run a Python cell in the session's globals.
async fn run_python(cx: &RunContext<'_>, session: &Session, source: &str) -> Vec<Output> {
    let Some(python) = session.python.as_ref() else {
        return vec![error_output(
            "this server has no Python runtime, so a Python notebook cannot run here",
        )];
    };
    let host = NbHost::new(cx, session);
    let call = CodeCall {
        code: source.to_owned(),
        host: Some(&host),
        timeout: Some(CELL_TIMEOUT),
        ..CodeCall::default()
    };
    let ran = python.run_cell(&session.id, nb::NB_PY, call).await;
    answered(cx, ran).await
}

/// A run's answer as the cell's outputs: the envelope the prelude made, or
/// the error the runtime stopped it with.
async fn answered(cx: &RunContext<'_>, ran: Result<Json>) -> Vec<Output> {
    match ran {
        Ok(envelope) => match nb::cell_outputs(cx.catalog, &cx.caller(), &envelope).await {
            Ok(answer) => answer.outputs,
            Err(e) => vec![error_output(e.message())],
        },
        Err(e) => vec![error_output(e.message())],
    }
}

/// An error output from a runtime's sentence, with the line it names.
pub(crate) fn error_output(message: &str) -> Output {
    let message = message
        .strip_prefix("JavaScript code failed: ")
        .or_else(|| message.strip_prefix("this Python code failed: "))
        .unwrap_or(message)
        .trim();
    Output::Error {
        message: message.to_owned(),
        line: line_in(message),
    }
}

/// The line a runtime's sentence names: `line 3: …`, `line 3, column 7: …`.
fn line_in(message: &str) -> Option<u32> {
    let at = message.find("line ")?;
    let digits: String = message[at + 5..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|l| *l > 0)
}

/// What a cell's `db` and `nb` reach: `nb` plans answered here, the rest by
/// the table host a code body has, as the administrator.
pub(crate) struct NbHost<'a> {
    table: TableHost<'a>,
    cx: &'a RunContext<'a>,
    session: &'a Session,
    deadline: Instant,
}

impl<'a> NbHost<'a> {
    pub(crate) fn new(cx: &'a RunContext<'a>, session: &'a Session) -> NbHost<'a> {
        NbHost {
            table: TableHost::new(cx.catalog)
                .for_admin(&cx.admin)
                .with_evaluator(cx.apps.evaluator()),
            cx,
            session,
            deadline: Instant::now() + CELL_TIMEOUT,
        }
    }

    /// Answer one `nb` plan.
    pub(crate) async fn answer(&self, plan: &Json) -> Result<Json> {
        match plan.get("what").and_then(Json::as_str) {
            Some("fit") => self.fit(plan).await,
            _ => nb::answer(self.cx.catalog, &self.cx.caller(), plan).await,
        }
    }

    /// `nb.fit(model)`: start a fit of the model, as **Fit** in the model
    /// editor does, and wait for it — its progress pushed meanwhile through
    /// the session's status, which names it.
    async fn fit(&self, plan: &Json) -> Result<Json> {
        let catalog = self.cx.catalog;
        let name = plan
            .get("model")
            .and_then(Json::as_str)
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| {
                Error::invalid("nb.fit() takes a model's name, as in nb.fit(\"House prices\")")
            })?;
        let models = crate::handlers::models_of(self.cx.apps)?;
        let Some(model) = sc_model::load_model_by_name(catalog, name).await? else {
            let mut names: Vec<String> = sc_model::list_models(catalog)
                .await?
                .into_iter()
                .map(|m| format!("`{}`", m.name))
                .collect();
            names.sort();
            return Err(Error::invalid(match names.is_empty() {
                true => format!("there is no model called `{name}`, and there are no models"),
                false => format!(
                    "there is no model called `{name}`; the models are {}",
                    names.join(", ")
                ),
            }));
        };
        sc_model::validate_model(catalog, &models.registry(), &model, None).await?;
        let mut instance = sc_model::ModelInstance::starting(model.id);
        instance.description = "Fitted from a notebook".to_owned();
        let instance = models.start_fit(&model, instance).await?;
        self.session.waiting_for(Some(instance.id.0));
        let finished = loop {
            let now = sc_model::require_model_instance(catalog, instance.id).await?;
            if now.status != sc_model::FitStatus::Fitting {
                break now;
            }
            if Instant::now() + Duration::from_millis(500) > self.deadline {
                self.session.waiting_for(None);
                return Err(Error::invalid(format!(
                    "the fit of `{name}` is still running after the cell's {} s; it goes on, \
                     and the model editor shows it",
                    CELL_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        self.session.waiting_for(None);
        if finished.status != sc_model::FitStatus::Fitted {
            return Err(Error::invalid(format!(
                "the fit of `{name}` failed: {}",
                finished.error().unwrap_or("it did not say why")
            )));
        }
        nb::fit_handle(catalog, name, instance.id.0).await
    }
}

#[async_trait::async_trait]
impl CodeHost for NbHost<'_> {
    async fn call(&self, request: Json) -> Result<Json> {
        if request.get("op").and_then(Json::as_str) != Some("nb") {
            return self.table.call(request).await;
        }
        self.answer(&request).await.map_err(plain)
    }
}

/// `e` as its bare sentence: what the cell's `catch` and its error output
/// show, without the kind a log would want.
pub(crate) fn plain(e: Error) -> Error {
    let message = e.message().to_owned();
    match Err::<(), Error>(e).context(message.clone()) {
        Err(wrapped) => wrapped,
        Ok(()) => Error::invalid(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtimes_sentence_names_its_line() {
        assert_eq!(line_in("line 3: Unexpected token"), Some(3));
        assert_eq!(
            line_in("this Python code has a syntax error: line 12, column 4: invalid syntax"),
            Some(12)
        );
        assert_eq!(line_in("no column `pirce`"), None);
        let Output::Error { message, .. } = error_output("JavaScript code failed: TypeError: x")
        else {
            panic!()
        };
        assert_eq!(message, "TypeError: x");
    }

    #[test]
    fn a_full_server_ends_the_session_idle_longest_and_refuses_when_all_are_busy() {
        let sessions = NotebookSessions::new(2, IDLE_TIMEOUT);
        let ws = |n: u128| WorkspaceId(Uuid::from_u128(n));
        let js = NotebookLanguage::Javascript;
        let a = sessions.session(ws(1), "u", js, None).unwrap();
        let b = sessions.session(ws(2), "u", js, None).unwrap();
        // The same notebook and person is the same session.
        assert!(Arc::ptr_eq(
            &a,
            &sessions.session(ws(1), "u", js, None).unwrap()
        ));
        lock(&b.live).used = Instant::now() + Duration::from_secs(1);
        let _c = sessions.session(ws(3), "u", js, None).unwrap();
        assert_eq!(
            sessions.status(ws(1), "u"),
            SessionStatus::none(),
            "a was idle longest"
        );
        assert_eq!(sessions.status(ws(2), "u").state, "idle");
        for s in lock(&sessions.sessions).values() {
            lock(&s.live).cell = Some("c".into());
        }
        let err = sessions.session(ws(4), "u", js, None).err().unwrap();
        assert!(
            err.to_string().contains("running a cell right now"),
            "{err}"
        );
        assert_eq!(sessions.restart(ws(2), "u"), SessionStatus::none());
        assert_eq!(sessions.len(), 1);
    }
}
