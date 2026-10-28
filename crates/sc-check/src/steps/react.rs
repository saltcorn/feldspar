//! The `react.*` step types (docs/CHECKLISTS.md §2, TODO C3.2): what the
//! `react` framework adds to the core's `browser.*` steps, offered only for
//! its applications.
//!
//! - `react.typecheck` — `tsc --noEmit` against the **current generated
//!   client**. Renaming a field regenerates `client.ts`, and the compiler then
//!   names every component that used the old name: without a browser, in
//!   seconds. The most valuable React check there is.
//! - `react.open_route` — open a client-side route and wait until the app has
//!   rendered: no request in flight and no suspense fallback showing. `load`
//!   is too early in a single-page app, whose content arrives after it.
//! - `react.no_error_boundary` — no error boundary is showing its fallback.
//!
//! The last two read markers the scaffold writes into `src/main.tsx`: the root
//! error boundary's fallback carries `data-feldspar-error-boundary`, and the
//! root suspense fallback `data-feldspar-pending`. An application's own
//! fallbacks are found when they carry the same attributes.

use std::time::Duration;

use sc_browser::{BrowserAction, Locator};
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::registry::{FrameworkScope, Needs, StepContext, StepOutcome, StepType};
use crate::steps::browser::{MIN_OPEN_TIMEOUT, POLL, clean, path_of, poll, quoted, text};

/// The framework these steps belong to.
const FRAMEWORK: &str = sc_app::REACT_FRAMEWORK;

/// The attribute the scaffold's root error boundary marks its fallback with.
pub const ERROR_BOUNDARY_ATTR: &str = "data-feldspar-error-boundary";

/// The attribute a suspense fallback is marked with.
pub const PENDING_ATTR: &str = "data-feldspar-pending";

/// How long the page must stay settled — nothing in flight, nothing pending —
/// before a route counts as rendered: a request a render starts after the
/// last one finished would otherwise be missed.
const QUIET: Duration = Duration::from_millis(500);

/// The most diagnostics a failed type check quotes.
const MAX_DIAGNOSTICS: usize = 8;

/// The most of the checker's output a failure without diagnostics quotes.
const OUTPUT_TAIL: usize = 1500;

/// `react.typecheck`.
pub struct Typecheck;

#[async_trait::async_trait]
impl StepType for Typecheck {
    fn name(&self) -> &str {
        "react.typecheck"
    }

    fn description(&self) -> &str {
        "The application's source type-checks (tsc --noEmit) against its current generated \
         client, regenerated first as a build would. A field or table renamed or removed fails \
         here, naming each file and line that still uses it. Nothing is built or served."
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn needs(&self) -> Needs {
        Needs::BUILD
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        Some(FrameworkScope::named(FRAMEWORK))
    }

    fn min_timeout(&self) -> Duration {
        // A first check installs the dependencies.
        Duration::from_secs(600)
    }

    async fn run(&self, cx: &mut StepContext<'_>, _params: &Json) -> StepOutcome {
        let app = match cx.require_application() {
            Ok(app) => app,
            Err(e) => return e.into(),
        };
        let report = match sc_app::typecheck_application(cx.catalog, app, cx.caps.triggers).await {
            Ok(report) => report,
            Err(e) => return e.into(),
        };
        let value = json!({
            "passed": report.passed,
            "diagnostics": report.diagnostics.iter().map(|d| json!({
                "file": d.file, "line": d.line, "column": d.column, "message": d.message,
            })).collect::<Vec<_>>(),
        });
        if report.passed {
            return StepOutcome::pass(format!("`{}` type-checks", report.command))
                .with_value(value);
        }
        let detail = if report.diagnostics.is_empty() {
            let chars: Vec<char> = report.output.trim().chars().collect();
            let start = chars.len().saturating_sub(OUTPUT_TAIL);
            chars[start..].iter().collect::<String>()
        } else {
            let mut lines: Vec<String> = report
                .diagnostics
                .iter()
                .take(MAX_DIAGNOSTICS)
                .map(|d| format!("{}:{}: {}", d.file, d.line, d.message))
                .collect();
            if report.diagnostics.len() > MAX_DIAGNOSTICS {
                lines.push(format!(
                    "and {} more",
                    report.diagnostics.len() - MAX_DIAGNOSTICS
                ));
            }
            lines.join("\n")
        };
        let count = report.diagnostics.len();
        StepOutcome::fail(format!(
            "`{}` found {} against the current client:\n{detail}",
            report.command,
            match count {
                0 => "errors".to_owned(),
                1 => "1 error".to_owned(),
                n => format!("{n} errors"),
            }
        ))
        .with_value(value)
    }
}

/// `react.open_route`.
pub struct OpenRoute;

#[async_trait::async_trait]
impl StepType for OpenRoute {
    fn name(&self) -> &str {
        "react.open_route"
    }

    fn description(&self) -> &str {
        "Open a client-side route and wait until the app has rendered it: no request in \
         flight and no suspense fallback (data-feldspar-pending) showing for half a second. \
         Unless the checklist sets implicit_clean to false, the page must also be clean. Its \
         value is {url, status}."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("path", BasicType::Text)
                .label("Route")
                .required()
                .default_value("/")
                .sublabel("Within the application, e.g. /orders/{{ order.id }}"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::BROWSER
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        Some(FrameworkScope::named(FRAMEWORK))
    }

    fn min_timeout(&self) -> Duration {
        MIN_OPEN_TIMEOUT
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let path = match text(params, "path") {
            Ok(path) => path.trim().to_owned(),
            Err(e) => return e.into(),
        };
        let report = match cx.browser(BrowserAction::Goto { path }).await {
            Ok(report) => report,
            Err(e) => return e.into(),
        };
        let mut opened = format!("opened {}", path_of(&report.url));
        if let Some(status) = report.status {
            opened.push_str(&format!(" ({status})"));
        }
        if let Some(note) = &report.note {
            return StepOutcome::fail(format!("{opened}: {note}"));
        }
        let value = json!({ "url": report.url, "status": report.status });

        let pending = Locator::Css(format!("[{PENDING_ATTR}]"));
        let mut quiet_since: Option<tokio::time::Instant> = None;
        let settled = poll(cx, Some(&pending), |page| {
            let idle = page.pending_requests.unwrap_or(0) == 0
                && page.elements.as_ref().is_none_or(Vec::is_empty);
            if !idle {
                quiet_since = None;
                return false;
            }
            let since = *quiet_since.get_or_insert_with(tokio::time::Instant::now);
            // `poll` looks every POLL; settled once the quiet has lasted.
            since.elapsed() + POLL >= QUIET
        })
        .await;
        let (rendered, last) = match settled {
            Ok(done) => done,
            Err(e) => return e.into(),
        };
        if !rendered {
            let mut busy = Vec::new();
            if let Some(n) = last.pending_requests.filter(|n| *n > 0) {
                busy.push(format!(
                    "{n} request{} still in flight",
                    if n == 1 { "" } else { "s" }
                ));
            }
            if last.elements.as_ref().is_some_and(|e| !e.is_empty()) {
                busy.push("a suspense fallback still showing".to_owned());
            }
            return StepOutcome::fail(format!(
                "{opened}, but it had not finished rendering when the step's time was up ({}). \
                 The page: {}",
                busy.join(", "),
                quoted(&last)
            ))
            .with_value(value);
        }
        let opened = format!("{opened} and it rendered");
        if !cx.implicit_clean() {
            return StepOutcome::pass(opened).with_value(value);
        }
        clean(cx, opened).with_value(value)
    }
}

/// `react.no_error_boundary`.
pub struct NoErrorBoundary;

#[async_trait::async_trait]
impl StepType for NoErrorBoundary {
    fn name(&self) -> &str {
        "react.no_error_boundary"
    }

    fn description(&self) -> &str {
        "No error boundary is showing its fallback on the current page — the scaffold's root \
         boundary marks it with data-feldspar-error-boundary, and so can the app's own. Put it \
         after react.open_route or a browser step."
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn needs(&self) -> Needs {
        Needs::BROWSER
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        Some(FrameworkScope::named(FRAMEWORK))
    }

    async fn run(&self, cx: &mut StepContext<'_>, _params: &Json) -> StepOutcome {
        let boundary = Locator::Css(format!("[{ERROR_BOUNDARY_ATTR}]"));
        let report = match cx
            .browser(BrowserAction::Inspect {
                locator: Some(boundary),
            })
            .await
        {
            Ok(report) => report,
            Err(e) => return e.into(),
        };
        let found = report.elements.unwrap_or_default();
        let page = path_of(&report.url).to_owned();
        match found.first() {
            None => StepOutcome::pass(format!("no error boundary is showing on {page}")),
            Some(element) => {
                let text = report.text.as_deref().unwrap_or("").replace('\n', " · ");
                let what = if element.name.trim().is_empty() {
                    text.chars().take(300).collect::<String>()
                } else {
                    element.name.clone()
                };
                StepOutcome::fail(format!(
                    "an error boundary is showing its fallback on {page}: {what}"
                ))
            }
        }
    }
}
