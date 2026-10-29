//! The headless-browser seam (TODO §7b; TODO-checklists C2.1).
//!
//! Two things drive the server's headless Chromium: the coding agent's
//! `view_app`, one action per tool call over `@eN` refs, and the checklist
//! runner's `browser.*` steps, over [stable locators](locator). Neither may
//! name the other, and neither can name the server, which owns the browser —
//! so the trait they both call lives here, below both, and `sc-server`
//! implements it.
//!
//! - [`BrowserDriver`] performs one [`BrowserAction`] in a **browser context**
//!   of the caller's own (an agent run's, a checklist run's), as a session for
//!   a user or signed out, and reports what the page is now.
//! - [`BrowserTarget`] is what the context's pages may show: an application's
//!   live mount, or an agent run's [preview](PreviewTarget) of it.
//! - [`Locator`] names an element so the name survives the next build.

use std::path::PathBuf;
use std::time::Duration;

use sc_auth::User;
use sc_error::Result;
use uuid::Uuid;

pub mod locator;

pub use locator::{Element, ElementInfo, Locator, TextMatch};

/// An agent run's preview of an application: a second mount, reached at its
/// own host with a pass its run allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewTarget {
    /// The random label that owns the preview host.
    pub label: String,
    /// The preview's host name, `<label>--<subdomain>.<base-domain>`.
    pub host: String,
    /// The agent run whose preview it is: a pass that run allows reaches it.
    pub owner: Uuid,
}

/// What a browser context's pages show: one application, live or previewed.
/// Navigation anywhere else is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserTarget {
    /// The application's subdomain.
    pub subdomain: String,
    /// The run's preview of it; `None` for the live mount.
    pub preview: Option<PreviewTarget>,
}

impl BrowserTarget {
    /// The application's live mount.
    pub fn live(subdomain: impl Into<String>) -> BrowserTarget {
        BrowserTarget {
            subdomain: subdomain.into(),
            preview: None,
        }
    }

    /// A run's preview of the application.
    pub fn preview(subdomain: impl Into<String>, preview: PreviewTarget) -> BrowserTarget {
        BrowserTarget {
            subdomain: subdomain.into(),
            preview: Some(preview),
        }
    }
}

/// One thing to do in the page.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserAction {
    /// Open a path on the target host.
    Goto { path: String },
    /// Click an element.
    Click { element: Element },
    /// Type into an element, replacing its value.
    Fill { element: Element, text: String },
    /// Choose an option of a `<select>`, by its value or its text.
    Select { element: Element, option: String },
    /// Make a checkbox, radio or switch checked or not, clicking it only when
    /// it is not so already.
    Check { element: Element, checked: bool },
    /// Press a key (`Enter`, `Tab`, `Escape`, …) in the focused element, or in
    /// `element` after focusing it.
    Press {
        key: String,
        element: Option<Element>,
    },
    /// Set a file input's files (paths on the server).
    Upload {
        element: Element,
        files: Vec<PathBuf>,
    },
    /// Wait until the text appears in the page, or the element exists.
    WaitFor {
        text: Option<String>,
        reference: Option<String>,
        timeout: Duration,
    },
    /// Report the elements `locator` finds now (without waiting) and the
    /// page's visible text, instead of a snapshot. What an assertion polls.
    Inspect { locator: Option<Locator> },
    /// Only look.
    Snapshot,
    /// A JPEG of the viewport, or of the whole page.
    Screenshot { full_page: bool },
}

impl BrowserAction {
    /// The action's name, as a tool spells it.
    pub fn name(&self) -> &'static str {
        match self {
            BrowserAction::Goto { .. } => "goto",
            BrowserAction::Click { .. } => "click",
            BrowserAction::Fill { .. } => "fill",
            BrowserAction::Select { .. } => "select",
            BrowserAction::Check { .. } => "check",
            BrowserAction::Press { .. } => "press",
            BrowserAction::Upload { .. } => "upload",
            BrowserAction::WaitFor { .. } => "wait_for",
            BrowserAction::Inspect { .. } => "inspect",
            BrowserAction::Snapshot => "snapshot",
            BrowserAction::Screenshot { .. } => "screenshot",
        }
    }
}

/// One call on a browser context.
pub struct BrowserRequest<'a> {
    /// Whose context this is — an agent run's, a checklist run's. A first
    /// request creates it; [`BrowserDriver::close`] ends it.
    pub context: Uuid,
    /// What the context's pages show.
    pub target: &'a BrowserTarget,
    /// Whom the context's session is for. `None` looks as a visitor who is not
    /// signed in: the context carries no session for this call, and a later
    /// call with a user signs it back in.
    pub user: Option<&'a User>,
    /// What to do.
    pub action: BrowserAction,
    /// How long the whole call may take, including waiting for a context and,
    /// for a locator, for its element to appear.
    pub timeout: Duration,
}

/// What the page is after an action.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrowserReport {
    /// The page's URL.
    pub url: String,
    /// The HTTP status of the last document loaded, when known.
    pub status: Option<u16>,
    /// The accessibility snapshot, with refs on interactive elements. `None`
    /// for a screenshot and an inspection.
    pub snapshot: Option<String>,
    /// The JPEG, for a screenshot.
    pub screenshot: Option<Vec<u8>>,
    /// For an inspection: what its locator found.
    pub elements: Option<Vec<ElementInfo>>,
    /// For an inspection: the page's visible text, one accessible name or text
    /// run per line — what a person can read, hidden text left out.
    pub text: Option<String>,
    /// Console errors and uncaught exceptions since the previous call.
    pub console_errors: Vec<String>,
    /// Requests that failed or answered 4xx/5xx since the previous call.
    pub failed_requests: Vec<String>,
    /// How many of the page's requests are still in flight — streams that
    /// never end (an event source, a socket) not counted — when the driver
    /// keeps count. What "the network is idle" is judged on.
    pub pending_requests: Option<usize>,
    /// Something about the action the caller should know: a `wait_for` that
    /// timed out, a navigation that was refused.
    pub note: Option<String>,
}

/// Drives the headless browser (TODO 6b.6).
#[async_trait::async_trait]
pub trait BrowserDriver: Send + Sync {
    /// Perform one action in the request's context, creating the context and
    /// its session on first use, and report the page.
    async fn act(&self, request: BrowserRequest<'_>) -> Result<BrowserReport>;

    /// Close `context` and delete its session. Synchronous, because it is
    /// called from drop guards: start the release and return.
    fn close(&self, context: Uuid);
}
