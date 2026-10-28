//! The headless browser `view_app` and the checklists' `browser.*` steps drive
//! (TODO 6b.6, 6b.7; TODO-checklists C2.2).
//!
//! **One Chromium for the whole server**, started on the first call and again
//! if it has died, with a **fresh browser context per run** — an agent run's or
//! a checklist run's: its own cookies and storage, closed when the run ends. At most `--browser-contexts` runs
//! hold one at a time; a call beyond that waits, within its timeout.
//!
//! **How the browser reaches a preview.** Chromium starts with
//! `--host-resolver-rules` mapping the base domain and every name under it to
//! the loopback address, and **every other name to "not found"**, so a page can
//! reach this server and nothing else: `view_app` is a view of this application,
//! not a way to browse the internet from the server. The port is a listener of
//! its own that `serve` binds on the loopback address only, serving the same
//! router in plain HTTP with non-`Secure` cookies (see
//! [`crate::serve`](crate::serve())). So no certificate has to be trusted,
//! however the public listener is set up. Every navigation is checked as well:
//! `goto` takes only a path, and a page that leaves the preview host is taken
//! back, with the attempt reported.
//!
//! **The session is the caller's.** The first signed-in call of a run creates a
//! session for the user the request names ([`SessionStore::login`], the
//! `sc_auth::create_session` row plus this node's cache) and puts its cookie
//! straight into the run's context. Nothing is written to disk. Closing the
//! context logs the session out.
//!
//! **Elements by ref or by locator.** `view_app` names an element by a ref of
//! its last snapshot; a checklist step by a stable [`Locator`], resolved
//! against the accessibility tree (or, for `testid` and `css`, the DOM) when
//! the action runs, and waited for until the call's deadline. An action needs
//! exactly one element: a locator that finds several is refused, naming them,
//! so a step never clicks the wrong button by accident.
//!
//! **A context shows one application**: the live mount, or an agent run's
//! preview of it. A checklist run against a preview uses a context of its own,
//! carrying a pass the preview's run allows.
//!
//! **The preview is reached with a pass, not the session.** Each context also
//! carries a random [`PREVIEW_COOKIE`] that the run's previews accept
//! ([`AppMounts::allow_preview_token`]), so a call with no user — `view_app`'s
//! `signed_out` — sees the preview as an anonymous visitor would. Switching
//! between signed in and signed out removes the session cookie or puts it back,
//! clears the origin's storage (an app may cache who is signed in there), and
//! reloads the page.
//!
//! **What changed since the last call** — console errors, uncaught exceptions,
//! failed requests and 4xx/5xx responses — is collected by listeners on the
//! page and handed back with the next report, so a white screen caused by a
//! thrown error is a line of text.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams;
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetBoxModelParams, GetDocumentParams,
    QuerySelectorAllParams, ResolveNodeParams, ScrollIntoViewIfNeededParams,
    SetFileInputFilesParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, InsertTextParams,
};
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, CookieSameSite, DeleteCookiesParams, EventLoadingFailed, EventLoadingFinished,
    EventRequestWillBeSent, EventResponseReceived, ResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::browser_protocol::storage::ClearDataForOriginParams;
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, ConsoleApiCalledType, EventConsoleApiCalled, EventExceptionThrown,
    RemoteObject,
};
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::layout::Point;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use sc_agent::{
    BrowserAction, BrowserDriver, BrowserReport, BrowserRequest, BrowserTarget, Element,
    ElementInfo, Locator, RunId,
};
use sc_auth::SessionStore;
use sc_error::{Error, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::locate::Tree;
use super::snapshot::{AxNode, render};
use crate::apps::AppMounts;
use crate::security::{PREVIEW_COOKIE, SESSION_COOKIE, new_csrf_token};

/// The viewport every page gets.
const VIEWPORT: (u32, u32) = (1280, 800);

/// A screenshot larger than this is taken again at a lower quality.
pub(crate) const MAX_SCREENSHOT_BYTES: usize = 1_500_000;

/// The most console errors, and the most failed requests, one report carries.
const MAX_EVENTS: usize = 10;

/// How a [`ChromiumDriver`] starts its browser.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// The Chromium binary.
    pub executable: PathBuf,
    /// Whether it keeps its sandbox.
    pub sandbox: bool,
    /// How many runs may hold a context at once.
    pub contexts: usize,
    /// The domain applications are served under.
    pub base_domain: String,
    /// The loopback port the browser's listener is bound to.
    pub port: u16,
}

/// The browser process, the task pumping its protocol connection, and the
/// watchdog that cleans up after it.
struct Launched {
    browser: Browser,
    handler: tokio::task::JoinHandle<()>,
    /// Kills the browser if this process dies without dropping it, and removes
    /// its profile once it has gone either way.
    watchdog: Option<std::process::Child>,
}

impl Drop for Launched {
    fn drop(&mut self) {
        self.handler.abort();
        // The browser is `kill_on_drop` and goes with this value; the watchdog
        // sees it go, removes the profile and exits. Reaped off this thread.
        if let Some(mut watchdog) = self.watchdog.take() {
            std::thread::spawn(move || watchdog.wait());
        }
    }
}

/// What the page's listeners saw since the last report.
#[derive(Default)]
struct Seen {
    console_errors: Vec<String>,
    failed_requests: Vec<String>,
    /// The last document response: its URL and status.
    document: Option<(String, u16)>,
    /// Request URLs by id, for a failure that names only the id.
    requests: HashMap<String, String>,
    /// The requests sent and not yet finished or failed, by id — not those
    /// that stay open by design (an event source, a socket).
    in_flight: HashSet<String>,
}

impl Seen {
    fn push(list: &mut Vec<String>, line: String) {
        if list.len() < MAX_EVENTS {
            list.push(line);
        } else if list.len() == MAX_EVENTS {
            list.push("[more not shown]".to_owned());
        }
    }
}

/// One run's context: its page, its session, and what its listeners saw.
struct RunPage {
    context: BrowserContextId,
    page: Page,
    /// The caller's session, made on the first signed-in call.
    session: Option<String>,
    /// The token the run's previews accept from this context.
    pass: String,
    /// Whether the last call was signed out; `None` before the first.
    signed_out: Option<bool>,
    /// The origins the cookies for the current mode have been set for.
    origins: Vec<String>,
    /// The DOM node each ref of the last snapshot names.
    refs: HashMap<String, i64>,
    seen: Arc<Mutex<Seen>>,
    listeners: Vec<tokio::task::JoinHandle<()>>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for RunPage {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.abort();
        }
    }
}

/// The server's headless Chromium.
pub struct ChromiumDriver {
    config: DriverConfig,
    sessions: Arc<SessionStore>,
    apps: Arc<AppMounts>,
    browser: Arc<tokio::sync::Mutex<Option<Launched>>>,
    runs: Mutex<HashMap<uuid::Uuid, Arc<tokio::sync::Mutex<RunPage>>>>,
    permits: Arc<Semaphore>,
}

impl ChromiumDriver {
    /// A driver that starts `config`'s browser on first use, creating sessions
    /// in `sessions` and letting them reach `apps`' previews.
    pub fn new(
        config: DriverConfig,
        sessions: Arc<SessionStore>,
        apps: Arc<AppMounts>,
    ) -> ChromiumDriver {
        let permits = Arc::new(Semaphore::new(config.contexts.max(1)));
        ChromiumDriver {
            config,
            sessions,
            apps,
            browser: Arc::new(tokio::sync::Mutex::new(None)),
            runs: Mutex::new(HashMap::new()),
            permits,
        }
    }

    /// The origin a preview host is reached at through the loopback listener.
    pub fn origin(&self, host: &str) -> String {
        format!("http://{host}:{}", self.config.port)
    }

    /// The host a target's pages are on: the preview's, or the application's
    /// live one.
    pub fn host(&self, target: &BrowserTarget) -> String {
        match &target.preview {
            Some(preview) => preview.host.clone(),
            None if target.subdomain == sc_config::ROOT_SUBDOMAIN => {
                self.config.base_domain.clone()
            }
            None => format!("{}.{}", target.subdomain, self.config.base_domain),
        }
    }

    /// Let this context's pass reach the target's preview, if it is one.
    fn allow(&self, target: &BrowserTarget, pass: &str) {
        if let Some(preview) = &target.preview {
            self.apps.allow_preview_token(RunId(preview.owner), pass);
        }
    }

    /// How many runs hold a context now.
    pub fn open_contexts(&self) -> usize {
        self.runs_lock().len()
    }

    /// Stop the browser, if it is running. For the server's shutdown.
    pub fn shutdown(&self) {
        if let Ok(mut browser) = self.browser.try_lock() {
            // `kill_on_drop` ends the process.
            browser.take();
        }
    }

    /// The arguments the browser is started with.
    fn launch_config(&self, profile: &std::path::Path) -> Result<BrowserConfig> {
        let base = &self.config.base_domain;
        let rules = format!("MAP *.{base} 127.0.0.1, MAP {base} 127.0.0.1, MAP * ~NOTFOUND");
        let mut builder = BrowserConfig::builder()
            .chrome_executable(&self.config.executable)
            .respect_https_errors()
            .window_size(VIEWPORT.0, VIEWPORT.1)
            .viewport(Viewport {
                width: VIEWPORT.0,
                height: VIEWPORT.1,
                ..Viewport::default()
            })
            .request_timeout(Duration::from_secs(30))
            .launch_timeout(Duration::from_secs(30))
            .user_data_dir(profile)
            .arg(("host-resolver-rules", rules.as_str()))
            .arg("no-proxy-server")
            .arg("disable-gpu");
        if !self.config.sandbox {
            builder = builder.no_sandbox();
        }
        builder
            .build()
            .map_err(|e| Error::config(format!("the browser could not be configured: {e}")))
    }

    /// The browser, started if it is not running.
    async fn launched(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Launched>>> {
        let mut guard = self.browser.lock().await;
        let dead = match guard.as_mut() {
            Some(launched) => launched.handler.is_finished(),
            None => true,
        };
        if dead {
            guard.take();
            // A profile of its own, gone with the process: nothing the browser
            // stores, cookies included, outlives it.
            let profile = std::env::temp_dir().join(format!(
                "feldspar-browser-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            let config = self.launch_config(&profile)?;
            let (mut browser, mut handler) = Browser::launch(config).await.map_err(|e| {
                Error::config(format!(
                    "the browser at `{}` did not start: {e}",
                    self.config.executable.display()
                ))
            })?;
            let handler = tokio::spawn(async move {
                while let Some(event) = handler.next().await {
                    if event.is_err() {
                        break;
                    }
                }
            });
            sc_log::log_info!(
                "started the browser at {} for view_app",
                self.config.executable.display()
            );
            let watchdog = browser
                .get_mut_child()
                .and_then(|child| child.inner.id())
                .and_then(|pid| watchdog(pid, &profile));
            *guard = Some(Launched {
                browser,
                handler,
                watchdog,
            });
        }
        Ok(guard)
    }

    fn runs_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<uuid::Uuid, Arc<tokio::sync::Mutex<RunPage>>>> {
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The run's page, creating its context and session on first use.
    async fn run_page(
        &self,
        request: &BrowserRequest<'_>,
        deadline: Instant,
    ) -> Result<Arc<tokio::sync::Mutex<RunPage>>> {
        if let Some(page) = self.runs_lock().get(&request.context) {
            return Ok(page.clone());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let permit = tokio::time::timeout(left, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| {
                Error::invalid(format!(
                    "every browser context ({}) is in use by other runs; try again shortly",
                    self.config.contexts
                ))
            })?
            .map_err(|_| Error::msg("the browser is shutting down"))?;

        let (context, page) = {
            let guard = self.launched().await?;
            let browser = &guard
                .as_ref()
                .ok_or_else(|| Error::msg("no browser"))?
                .browser;
            let context = browser
                .create_browser_context(CreateBrowserContextParams::default())
                .await
                .map_err(cdp)?;
            let target = CreateTargetParams::builder()
                .url("about:blank")
                .browser_context_id(context.clone())
                .build()
                .map_err(Error::msg)?;
            let page = browser.new_page(target).await.map_err(cdp)?;
            (context, page)
        };

        let pass = new_csrf_token();
        self.allow(request.target, &pass);
        let seen = Arc::new(Mutex::new(Seen::default()));
        let listeners = listen(&page, &seen).await?;
        let run_page = Arc::new(tokio::sync::Mutex::new(RunPage {
            context,
            page,
            session: None,
            pass,
            signed_out: None,
            origins: Vec::new(),
            refs: HashMap::new(),
            seen,
            listeners,
            _permit: permit,
        }));
        self.runs_lock().insert(request.context, run_page.clone());
        Ok(run_page)
    }

    async fn act_in(
        &self,
        request: &BrowserRequest<'_>,
        deadline: Instant,
    ) -> Result<BrowserReport> {
        let page = self.run_page(request, deadline).await?;
        let mut page = page.lock().await;
        let host = self.host(request.target);
        let origin = self.origin(&host);
        // Again on every call: a preview the idle sweep removed and a later
        // check mounted afresh has forgotten it.
        self.allow(request.target, &page.pass);

        let signed_out = request.user.is_none();
        let switched = page.signed_out.is_some_and(|was| was != signed_out);
        page.signed_out = Some(signed_out);
        if switched {
            for visited in std::mem::take(&mut page.origins) {
                forget_session(&page.page, &visited).await?;
            }
        }
        if let Some(user) = request.user
            && page.session.is_none()
        {
            page.session = Some(self.sessions.login(user.clone()).await?);
        }
        if !page.origins.contains(&origin) {
            let mut cookies = vec![(PREVIEW_COOKIE, page.pass.clone())];
            if let (false, Some(session)) = (signed_out, &page.session) {
                cookies.push((SESSION_COOKIE, session.clone()));
            }
            for (name, value) in cookies {
                let cookie = CookieParam::builder()
                    .name(name)
                    .value(value)
                    .url(origin.clone())
                    .http_only(true)
                    .same_site(CookieSameSite::Strict)
                    .build()
                    .map_err(Error::msg)?;
                page.page.set_cookie(cookie).await.map_err(cdp)?;
            }
            page.origins.push(origin.clone());
        }

        let mut note = None;
        let current = page.page.url().await.map_err(cdp)?.unwrap_or_default();
        let on_preview = host_of(&current) == Some(host.as_str());
        match &request.action {
            BrowserAction::Goto { path } => {
                let path = preview_path(path)?;
                navigate(&page.page, &format!("{origin}{path}")).await?;
            }
            action => {
                // Everything but `goto` acts on the page as it is; a page that
                // has not been opened on this preview yet is opened at `/`, and
                // one opened in the other mode is loaded again in this one.
                if !on_preview {
                    navigate(&page.page, &format!("{origin}/")).await?;
                } else if switched {
                    navigate(&page.page, &current).await?;
                }
                match action {
                    BrowserAction::Click { element } => {
                        let node = element_node(&page, element, deadline).await?;
                        click(&page.page, node).await?;
                        settle(&page.page).await;
                    }
                    BrowserAction::Fill { element, text } => {
                        let node = element_node(&page, element, deadline).await?;
                        fill(&page.page, node, text).await?;
                    }
                    BrowserAction::Select { element, option } => {
                        let node = element_node(&page, element, deadline).await?;
                        select(&page.page, node, option).await?;
                        settle(&page.page).await;
                    }
                    BrowserAction::Check { element, checked } => {
                        let node = element_node(&page, element, deadline).await?;
                        check(&page.page, node, *checked).await?;
                    }
                    BrowserAction::Press { key, element } => {
                        if let Some(element) = element {
                            let node = element_node(&page, element, deadline).await?;
                            call_on(&page.page, node, "function() { this.focus(); }").await?;
                        }
                        press(&page.page, key).await?;
                        settle(&page.page).await;
                    }
                    BrowserAction::Upload { element, files } => {
                        let node = element_node(&page, element, deadline).await?;
                        upload(&page.page, node, files).await?;
                    }
                    BrowserAction::WaitFor {
                        text,
                        reference,
                        timeout,
                    } => {
                        let until = (Instant::now() + *timeout).min(deadline);
                        let node = reference
                            .as_deref()
                            .map(|r| node_for(&page.refs, r))
                            .transpose()?;
                        if !wait_for(&page.page, text.as_deref(), node, until).await? {
                            note = Some(format!(
                                "wait_for timed out after {:.0} seconds",
                                timeout.as_secs_f64()
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        // A page that left the preview is taken back.
        let now = page.page.url().await.map_err(cdp)?.unwrap_or_default();
        if host_of(&now) != Some(host.as_str()) {
            navigate(&page.page, &format!("{origin}/")).await?;
            note = Some(format!(
                "the page tried to leave {host} for {now}; only this application is shown, \
                 so it was taken back to /"
            ));
        }

        let mut report = BrowserReport {
            url: page
                .page
                .url()
                .await
                .map_err(cdp)?
                .unwrap_or_default()
                .replacen(&format!(":{}", self.config.port), "", 1),
            note,
            ..BrowserReport::default()
        };
        match &request.action {
            BrowserAction::Screenshot { full_page } => {
                report.screenshot = Some(screenshot(&page.page, *full_page).await?);
            }
            BrowserAction::Inspect { locator } => {
                let nodes = ax_nodes(&page.page).await?;
                let tree = Tree::new(&nodes);
                if let Some(locator) = locator {
                    let found = find(&page.page, &tree, locator).await?;
                    report.elements = Some(found.into_iter().map(|(_, info)| info).collect());
                }
                report.text = Some(tree.visible_text());
            }
            _ => {
                let snapshot = snapshot(&page.page).await?;
                page.refs = snapshot.refs;
                report.snapshot = Some(snapshot.text);
            }
        }
        let mut seen = page.seen.lock().unwrap_or_else(|e| e.into_inner());
        report.console_errors = std::mem::take(&mut seen.console_errors);
        report.failed_requests = std::mem::take(&mut seen.failed_requests);
        report.status = seen.document.as_ref().map(|(_, status)| *status);
        report.pending_requests = Some(seen.in_flight.len());
        Ok(report)
    }
}

#[async_trait::async_trait]
impl BrowserDriver for ChromiumDriver {
    async fn act(&self, request: BrowserRequest<'_>) -> Result<BrowserReport> {
        let deadline = Instant::now() + request.timeout;
        match tokio::time::timeout(request.timeout, self.act_in(&request, deadline)).await {
            Ok(result) => result,
            Err(_) => Err(Error::invalid(format!(
                "`{}` did not finish within {} seconds",
                request.action.name(),
                request.timeout.as_secs()
            ))),
        }
    }

    fn close(&self, context: uuid::Uuid) {
        let Some(page) = self.runs_lock().remove(&context) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let sessions = self.sessions.clone();
        let browser = self.browser.clone();
        runtime.spawn(async move {
            let page = page.lock().await;
            if let Some(session) = &page.session
                && let Err(e) = sessions.logout(session).await
            {
                sc_log::log_warn!("{context}: its browser session could not be deleted: {e}");
            }
            if let Some(launched) = browser.lock().await.as_ref() {
                let _ = launched
                    .browser
                    .dispose_browser_context(page.context.clone())
                    .await;
            }
        });
    }
}

/// A process that kills the browser once this process is gone — however it
/// went — and removes the browser's profile once the browser is gone. A browser
/// outliving a killed server would otherwise hold its memory, and its profile,
/// until the next reboot.
#[cfg(unix)]
fn watchdog(browser: u32, profile: &std::path::Path) -> Option<std::process::Child> {
    let script = format!(
        "while kill -0 {server} 2>/dev/null && kill -0 {browser} 2>/dev/null; do sleep 1; done; \
         kill -9 {browser} 2>/dev/null; sleep 1; rm -rf '{profile}'",
        server = std::process::id(),
        profile = profile.display().to_string().replace('\'', ""),
    );
    std::process::Command::new("sh")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

#[cfg(not(unix))]
fn watchdog(_browser: u32, _profile: &std::path::Path) -> Option<std::process::Child> {
    None
}

/// A protocol failure, as an error the model can read.
/// Remove the session cookie from `origin`, and what the application may have
/// stored there about who is signed in. The preview pass stays.
async fn forget_session(page: &Page, origin: &str) -> Result<()> {
    page.execute(
        DeleteCookiesParams::builder()
            .name(SESSION_COOKIE)
            .url(origin)
            .build()
            .map_err(Error::msg)?,
    )
    .await
    .map_err(cdp)?;
    page.execute(
        ClearDataForOriginParams::builder()
            .origin(origin)
            .storage_types("local_storage,session_storage,indexeddb,cache_storage,service_workers")
            .build()
            .map_err(Error::msg)?,
    )
    .await
    .map_err(cdp)?;
    Ok(())
}

fn cdp(e: chromiumoxide::error::CdpError) -> Error {
    Error::msg(format!("the browser reported: {e}"))
}

/// The host of a URL, without its port.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(authority.split(':').next().unwrap_or(authority))
}

/// A `goto` target: a path on the preview, and nothing else.
fn preview_path(path: &str) -> Result<String> {
    let path = path.trim();
    if path.contains("://") || path.starts_with("//") || !path.starts_with('/') {
        return Err(Error::invalid(format!(
            "`goto` takes a path on this application, starting with `/`, not `{path}`; \
             only this application is shown"
        )));
    }
    Ok(path.to_owned())
}

/// The DOM node a ref of the last snapshot names.
fn node_for(refs: &HashMap<String, i64>, reference: &str) -> Result<i64> {
    let reference = reference.trim();
    let key = if reference.starts_with('@') {
        reference.to_owned()
    } else {
        format!("@{reference}")
    };
    refs.get(&key).copied().ok_or_else(|| {
        Error::invalid(format!(
            "`{reference}` is not a ref in the last snapshot; take a snapshot and use one of \
             the @e refs it shows"
        ))
    })
}

/// Open `url` and wait for it to load.
async fn navigate(page: &Page, url: &str) -> Result<()> {
    page.goto(url).await.map_err(cdp)?;
    settle(page).await;
    Ok(())
}

/// Give an action's consequences a moment: a navigation it started, a render.
async fn settle(page: &Page) {
    tokio::time::sleep(Duration::from_millis(250)).await;
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        let ready = page
            .evaluate("document.readyState")
            .await
            .ok()
            .and_then(|r| r.into_value::<String>().ok());
        if ready.as_deref() == Some("complete") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Click the middle of a node, scrolled into view.
async fn click(page: &Page, node: i64) -> Result<()> {
    let id = BackendNodeId::new(node);
    page.execute(
        ScrollIntoViewIfNeededParams::builder()
            .backend_node_id(id)
            .build(),
    )
    .await
    .map_err(stale)?;
    let model = page
        .execute(GetBoxModelParams::builder().backend_node_id(id).build())
        .await
        .map_err(stale)?
        .result
        .model;
    let quad = model.content.inner();
    let (x, y) = (
        (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0,
        (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0,
    );
    page.click(Point { x, y }).await.map_err(cdp)?;
    Ok(())
}

/// Replace a field's value with `text`, as typing would.
async fn fill(page: &Page, node: i64, text: &str) -> Result<()> {
    let object = page
        .execute(
            ResolveNodeParams::builder()
                .backend_node_id(BackendNodeId::new(node))
                .build(),
        )
        .await
        .map_err(stale)?
        .result
        .object;
    let object_id = object
        .object_id
        .ok_or_else(|| Error::invalid("that element cannot be filled"))?;
    // Clear through the native setter, so a framework's controlled input sees
    // the change, then type.
    let clear = CallFunctionOnParams::builder()
        .object_id(object_id)
        .function_declaration(
            "function() { this.focus(); \
               const proto = Object.getPrototypeOf(this); \
               const desc = Object.getOwnPropertyDescriptor(proto, 'value'); \
               if (desc && desc.set) { desc.set.call(this, ''); } \
               else if (this.isContentEditable) { this.textContent = ''; } \
               this.dispatchEvent(new Event('input', { bubbles: true })); }",
        )
        .build()
        .map_err(Error::msg)?;
    page.execute(clear).await.map_err(cdp)?;
    page.execute(InsertTextParams::new(text))
        .await
        .map_err(cdp)?;
    Ok(())
}

/// Press one key in the focused element.
async fn press(page: &Page, key: &str) -> Result<()> {
    let definition = chromiumoxide::keys::USKEYBOARD_LAYOUT
        .iter()
        .find(|k| k.key.eq_ignore_ascii_case(key) && k.key.len() == key.len())
        .ok_or_else(|| {
            Error::invalid(format!(
                "`{key}` is not a key name; use one like Enter, Tab, Escape, Backspace or \
                 ArrowDown"
            ))
        })?;
    for kind in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
        let mut event = DispatchKeyEventParams::builder()
            .r#type(kind.clone())
            .key(definition.key)
            .code(definition.code)
            .windows_virtual_key_code(definition.key_code)
            .native_virtual_key_code(definition.key_code);
        if kind == DispatchKeyEventType::KeyDown
            && let Some(text) = definition.text
        {
            event = event.text(text);
        }
        page.execute(event.build().map_err(Error::msg)?)
            .await
            .map_err(cdp)?;
    }
    Ok(())
}

/// Wait until `text` is in the page or `node` is laid out. `false` on timeout.
async fn wait_for(
    page: &Page,
    text: Option<&str>,
    node: Option<i64>,
    until: Instant,
) -> Result<bool> {
    let probe = text.map(|t| {
        format!(
            "!!(document.body && document.body.innerText.includes({}))",
            serde_json::to_string(t).unwrap_or_default()
        )
    });
    loop {
        let text_ok = match &probe {
            Some(probe) => page
                .evaluate(probe.as_str())
                .await
                .ok()
                .and_then(|r| r.into_value::<bool>().ok())
                .unwrap_or(false),
            None => true,
        };
        let node_ok = match node {
            Some(node) => page
                .execute(
                    GetBoxModelParams::builder()
                        .backend_node_id(BackendNodeId::new(node))
                        .build(),
                )
                .await
                .is_ok(),
            None => true,
        };
        if text_ok && node_ok {
            return Ok(true);
        }
        if Instant::now() >= until {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A ref whose element is gone.
fn stale(e: chromiumoxide::error::CdpError) -> Error {
    Error::invalid(format!(
        "that element is no longer on the page ({e}); take a snapshot for fresh refs"
    ))
}

/// The page's accessibility tree.
async fn ax_nodes(page: &Page) -> Result<Vec<AxNode>> {
    let tree = page
        .execute(GetFullAxTreeParams::default())
        .await
        .map_err(cdp)?
        .result
        .nodes;
    let json = serde_json::to_value(tree).map_err(|e| Error::msg(e.to_string()))?;
    serde_json::from_value(json).map_err(|e| Error::msg(format!("accessibility tree: {e}")))
}

/// The page's accessibility tree, rendered.
async fn snapshot(page: &Page) -> Result<super::snapshot::Snapshot> {
    Ok(render(&ax_nodes(page).await?))
}

/// The DOM node an action's element names: a ref of the last snapshot, or the
/// one element a locator finds, waited for until `deadline`.
async fn element_node(page: &RunPage, element: &Element, deadline: Instant) -> Result<i64> {
    match element {
        Element::Ref(reference) => node_for(&page.refs, reference),
        Element::Locator(locator) => resolve(&page.page, locator, deadline).await,
    }
}

/// The one element `locator` finds, waiting for it until `deadline`. Visible
/// elements are preferred: a hidden one is chosen only when nothing visible
/// matches (a styled file input is hidden behind its button). Several matches
/// are an error naming them.
async fn resolve(page: &Page, locator: &Locator, deadline: Instant) -> Result<i64> {
    // Leave a moment of the call's time for the action itself.
    let until = deadline
        .checked_sub(Duration::from_millis(500))
        .unwrap_or(deadline);
    loop {
        let nodes = ax_nodes(page).await?;
        let found = find(page, &Tree::new(&nodes), locator).await?;
        let visible: Vec<&(i64, ElementInfo)> = found.iter().filter(|(_, i)| i.visible).collect();
        let candidates: Vec<&(i64, ElementInfo)> = if visible.is_empty() {
            found.iter().collect()
        } else {
            visible
        };
        match candidates.as_slice() {
            [(node, _)] => return Ok(*node),
            [] => {}
            many => {
                let named: Vec<String> = many
                    .iter()
                    .take(5)
                    .map(|(_, i)| format!("{} {:?}", i.role, i.name))
                    .collect();
                return Err(Error::invalid(format!(
                    "`{locator}` matches {} elements ({}{}); make it name one, e.g. by quoting \
                     the whole name",
                    many.len(),
                    named.join(", "),
                    if many.len() > 5 { ", …" } else { "" }
                )));
            }
        }
        if Instant::now() >= until {
            return Err(Error::invalid(format!(
                "no element matches `{locator}`"
            )));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The elements `locator` finds now, described.
async fn find(page: &Page, tree: &Tree<'_>, locator: &Locator) -> Result<Vec<(i64, ElementInfo)>> {
    let nodes = match locator.selector() {
        Some(selector) => query_all(page, &selector).await?,
        None => tree.find(locator),
    };
    Ok(nodes.into_iter().map(|n| (n, tree.describe(n))).collect())
}

/// The DOM nodes a CSS selector matches, as backend node ids.
async fn query_all(page: &Page, selector: &str) -> Result<Vec<i64>> {
    let root = page
        .execute(GetDocumentParams::builder().depth(0).build())
        .await
        .map_err(cdp)?
        .result
        .root
        .node_id;
    let ids = page
        .execute(QuerySelectorAllParams::new(root, selector))
        .await
        .map_err(|e| Error::invalid(format!("`{selector}` is not a CSS selector the page takes ({e})")))?
        .result
        .node_ids;
    let mut out = Vec::new();
    for id in ids {
        let node = page
            .execute(DescribeNodeParams::builder().node_id(id).build())
            .await
            .map_err(cdp)?
            .result
            .node;
        out.push(*node.backend_node_id.inner());
    }
    Ok(out)
}

/// Call `function` on a DOM node, and return what it returns.
async fn call_on(page: &Page, node: i64, function: &str) -> Result<serde_json::Value> {
    let object_id = page
        .execute(
            ResolveNodeParams::builder()
                .backend_node_id(BackendNodeId::new(node))
                .build(),
        )
        .await
        .map_err(stale)?
        .result
        .object
        .object_id
        .ok_or_else(|| Error::invalid("that element cannot be scripted"))?;
    let call = CallFunctionOnParams::builder()
        .object_id(object_id)
        .function_declaration(function)
        .return_by_value(true)
        .build()
        .map_err(Error::msg)?;
    let result = page.execute(call).await.map_err(cdp)?.result;
    if let Some(details) = result.exception_details {
        return Err(Error::invalid(format!("the page threw: {}", details.text)));
    }
    Ok(result.result.value.unwrap_or(serde_json::Value::Null))
}

/// Choose an option of a `<select>`, by value or by text, as a person would:
/// with `input` and `change` events.
async fn select(page: &Page, node: i64, option: &str) -> Result<()> {
    let option = serde_json::to_string(option).map_err(|e| Error::msg(e.to_string()))?;
    let outcome = call_on(
        page,
        node,
        &format!(
            "function() {{ const want = {option}; \
               if (!(this instanceof HTMLSelectElement)) return 'that element is not a select'; \
               const options = Array.from(this.options); \
               const o = options.find(o => o.value === want) \
                 || options.find(o => o.text.trim().toLowerCase() === want.trim().toLowerCase()); \
               if (!o) return 'it has no option ' + JSON.stringify(want) + '; it has ' \
                 + options.map(o => JSON.stringify(o.text.trim())).join(', '); \
               const desc = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value'); \
               desc.set.call(this, o.value); \
               this.dispatchEvent(new Event('input', {{ bubbles: true }})); \
               this.dispatchEvent(new Event('change', {{ bubbles: true }})); \
               return ''; }}"
        ),
    )
    .await?;
    match outcome.as_str() {
        Some("") => Ok(()),
        Some(why) => Err(Error::invalid(why.to_owned())),
        None => Ok(()),
    }
}

/// Whether a checkbox, radio or switch is checked; `None` for anything else.
async fn checked(page: &Page, node: i64) -> Result<Option<bool>> {
    Ok(call_on(
        page,
        node,
        "function() { if (typeof this.checked === 'boolean' \
           && (this.type === 'checkbox' || this.type === 'radio')) return this.checked; \
           const a = this.getAttribute('aria-checked'); \
           return a === null ? null : a === 'true'; }",
    )
    .await?
    .as_bool())
}

/// Make a checkbox checked or not, clicking it only when it is not so already.
async fn check(page: &Page, node: i64, want: bool) -> Result<()> {
    let Some(now) = checked(page, node).await? else {
        return Err(Error::invalid(
            "that element is not a checkbox, radio or switch",
        ));
    };
    if now == want {
        return Ok(());
    }
    click(page, node).await?;
    settle(page).await;
    if checked(page, node).await? != Some(want) {
        return Err(Error::invalid(format!(
            "clicking it did not make it {}",
            if want { "checked" } else { "unchecked" }
        )));
    }
    Ok(())
}

/// Set a file input's files.
async fn upload(page: &Page, node: i64, files: &[PathBuf]) -> Result<()> {
    let files: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    page.execute(
        SetFileInputFilesParams::builder()
            .files(files)
            .backend_node_id(BackendNodeId::new(node))
            .build()
            .map_err(Error::msg)?,
    )
    .await
    .map_err(|e| Error::invalid(format!("that element does not take files ({e})")))?;
    Ok(())
}

/// A JPEG of the page, capped in size.
async fn screenshot(page: &Page, full_page: bool) -> Result<Vec<u8>> {
    for (quality, full) in [(70, full_page), (40, full_page), (40, false)] {
        let shot = page
            .screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Jpeg)
                    .quality(quality)
                    .full_page(full)
                    .build(),
            )
            .await
            .map_err(cdp)?;
        if shot.len() <= MAX_SCREENSHOT_BYTES {
            return Ok(shot);
        }
    }
    Err(Error::invalid(
        "the page is too large to screenshot; take a snapshot instead",
    ))
}

/// Start collecting the page's console errors and failed requests.
async fn listen(page: &Page, seen: &Arc<Mutex<Seen>>) -> Result<Vec<tokio::task::JoinHandle<()>>> {
    let lock = |seen: &Arc<Mutex<Seen>>| {
        let seen = seen.clone();
        move |f: &mut dyn FnMut(&mut Seen)| f(&mut seen.lock().unwrap_or_else(|e| e.into_inner()))
    };
    let mut tasks = Vec::new();

    let mut console = page
        .event_listener::<EventConsoleApiCalled>()
        .await
        .map_err(cdp)?;
    let with = lock(seen);
    tasks.push(tokio::spawn(async move {
        while let Some(event) = console.next().await {
            if event.r#type != ConsoleApiCalledType::Error {
                continue;
            }
            let text = event
                .args
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join(" ");
            with(&mut |s| Seen::push(&mut s.console_errors, format!("console.error: {text}")));
        }
    }));

    let mut thrown = page
        .event_listener::<EventExceptionThrown>()
        .await
        .map_err(cdp)?;
    let with = lock(seen);
    tasks.push(tokio::spawn(async move {
        while let Some(event) = thrown.next().await {
            let details = &event.exception_details;
            let what = details
                .exception
                .as_ref()
                .and_then(|e| e.description.clone())
                .unwrap_or_else(|| details.text.clone());
            let what = what.lines().next().unwrap_or_default().to_owned();
            let at = details
                .url
                .as_deref()
                .map(|u| format!(" at {}:{}", strip_origin(u), details.line_number + 1))
                .unwrap_or_default();
            with(&mut |s| Seen::push(&mut s.console_errors, format!("uncaught {what}{at}")));
        }
    }));

    let mut sent = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .map_err(cdp)?;
    let with = lock(seen);
    tasks.push(tokio::spawn(async move {
        while let Some(event) = sent.next().await {
            let (id, url) = (event.request_id.inner().clone(), event.request.url.clone());
            // A stream that stays open by design never finishes, so counting
            // it would keep the network from ever being idle.
            let endless = matches!(
                event.r#type,
                Some(ResourceType::EventSource | ResourceType::WebSocket | ResourceType::Ping)
            );
            with(&mut |s| {
                if s.requests.len() > 1000 {
                    s.requests.clear();
                }
                s.requests.insert(id.clone(), url.clone());
                if !endless {
                    s.in_flight.insert(id.clone());
                }
            });
        }
    }));

    let mut finished = page
        .event_listener::<EventLoadingFinished>()
        .await
        .map_err(cdp)?;
    let with = lock(seen);
    tasks.push(tokio::spawn(async move {
        while let Some(event) = finished.next().await {
            let id = event.request_id.inner().clone();
            with(&mut |s| {
                s.in_flight.remove(&id);
            });
        }
    }));

    let mut responses = page
        .event_listener::<EventResponseReceived>()
        .await
        .map_err(cdp)?;
    let with = lock(seen);
    tasks.push(tokio::spawn(async move {
        while let Some(event) = responses.next().await {
            let status = u16::try_from(event.response.status).unwrap_or(0);
            let url = event.response.url.clone();
            let document = event.r#type == ResourceType::Document;
            with(&mut |s| {
                if document {
                    s.document = Some((url.clone(), status));
                }
                if status >= 400 {
                    Seen::push(
                        &mut s.failed_requests,
                        format!("{status} {}", strip_origin(&url)),
                    );
                }
            });
        }
    }));

    let mut failures = page
        .event_listener::<EventLoadingFailed>()
        .await
        .map_err(cdp)?;
    let with = lock(seen);
    tasks.push(tokio::spawn(async move {
        while let Some(event) = failures.next().await {
            let (id, error) = (event.request_id.inner().clone(), event.error_text.clone());
            if event.canceled == Some(true) {
                with(&mut |s| {
                    s.in_flight.remove(&id);
                });
                continue;
            }
            with(&mut |s| {
                s.in_flight.remove(&id);
                let url = s.requests.get(&id).cloned().unwrap_or_default();
                Seen::push(
                    &mut s.failed_requests,
                    format!("failed {} ({error})", strip_origin(&url)),
                );
            });
        }
    }));
    Ok(tasks)
}

/// A console argument as text.
fn describe(object: &RemoteObject) -> String {
    match (&object.value, &object.description) {
        (Some(serde_json::Value::String(s)), _) => s.clone(),
        (Some(other), _) => other.to_string(),
        (None, Some(description)) => description.lines().next().unwrap_or_default().to_owned(),
        (None, None) => String::new(),
    }
}

/// A URL with its scheme and host taken off, when it has them.
fn strip_origin(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(i) => rest[i..].to_owned(),
            None => "/".to_owned(),
        },
        None => url.to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn goto_takes_a_path_and_nothing_else() {
        assert_eq!(preview_path(" /tasks?x=1 ").unwrap(), "/tasks?x=1");
        for bad in [
            "https://evil.example/",
            "//evil.example/",
            "tasks",
            "javascript:alert(1)",
        ] {
            let err = preview_path(bad).unwrap_err().to_string();
            assert!(err.contains("starting with `/`"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_url_is_reduced_to_its_host_and_path() {
        assert_eq!(
            host_of("http://a--b.example.com:4000/x"),
            Some("a--b.example.com")
        );
        assert_eq!(host_of("about:blank"), None);
        assert_eq!(
            strip_origin("http://a.example.com:4000/api/x?y"),
            "/api/x?y"
        );
        assert_eq!(strip_origin("http://a.example.com"), "/");
    }

    #[test]
    fn a_ref_is_looked_up_with_or_without_its_at() {
        let refs: HashMap<String, i64> = [("@e2".to_owned(), 7)].into_iter().collect();
        assert_eq!(node_for(&refs, "@e2").unwrap(), 7);
        assert_eq!(node_for(&refs, "e2").unwrap(), 7);
        assert!(
            node_for(&refs, "@e9")
                .unwrap_err()
                .to_string()
                .contains("take a snapshot")
        );
    }
}
