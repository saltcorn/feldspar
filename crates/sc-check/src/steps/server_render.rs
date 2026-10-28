//! `server_render.page_renders` (docs/CHECKLISTS.md §2, TODO C3.4): a page of
//! a server-rendered site — an Astro site, any framework whose build declares
//! a server entry (`sc_app::server_render`) — renders for `run_as`.
//!
//! Over HTTP rather than in a browser: what a server-rendered page is, is the
//! HTML the server sends, and a render that throws is answered with the site's
//! error page and a 5xx. The core's `browser.*` steps check what the page does
//! once it is in a browser.

use sc_agent::AppHttpRequest;
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::registry::{FrameworkScope, Needs, StepContext, StepOutcome, StepType};
use crate::steps::browser::text;

/// The most of a page a failure message quotes.
const MAX_QUOTED: usize = 300;

/// `server_render.page_renders`.
pub struct PageRenders;

#[async_trait::async_trait]
impl StepType for PageRenders {
    fn name(&self) -> &str {
        "server_render.page_renders"
    }

    fn description(&self) -> &str {
        "A page of the server-rendered site renders for the checklist's user: it answers 2xx \
         with a document (a redirect is not followed, and fails), containing the expected text \
         when one is given. Over HTTP, without a browser. Its value \
         is {status, length}."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("path", BasicType::Text)
                .label("Path")
                .required()
                .default_value("/")
                .sublabel("Within the site, e.g. /blog/{{ post.slug }}"),
            FormField::new("contains", BasicType::Text)
                .label("Contains")
                .sublabel("Text the rendered HTML must contain"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::HTTP
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        Some(FrameworkScope::ServerRendered)
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let path = match text(params, "path") {
            Ok(path) => path.trim().to_owned(),
            Err(e) => return e.into(),
        };
        if !path.starts_with('/') {
            return StepOutcome::error(format!(
                "`path` is a path within the site and starts with `/`, not `{path}`"
            ));
        }
        let (app, requests) = match (cx.require_application(), cx.requests()) {
            (Ok(app), Ok(requests)) => (app, requests),
            (Err(e), _) | (_, Err(e)) => return e.into(),
        };
        let response = match requests
            .request(AppHttpRequest {
                subdomain: &app.subdomain,
                preview: cx.preview(),
                method: "GET".to_owned(),
                path: path.clone(),
                headers: vec![("accept".to_owned(), "text/html".to_owned())],
                body: Vec::new(),
                user: cx.run_as,
                timeout: cx.timeout,
            })
            .await
        {
            Ok(response) => response,
            Err(e) => return e.into(),
        };
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let value = json!({ "status": response.status, "length": response.body.len() });
        let quote = || {
            let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
            if flat.chars().count() <= MAX_QUOTED {
                flat
            } else {
                format!("{}…", flat.chars().take(MAX_QUOTED).collect::<String>())
            }
        };
        if !(200..300).contains(&response.status) {
            let location = response
                .headers
                .iter()
                .find(|(name, _)| name == "location")
                .map(|(_, to)| format!(" to {to}"))
                .unwrap_or_default();
            return StepOutcome::fail(format!(
                "GET {path} answered {}{location}: {}",
                response.status,
                quote()
            ))
            .with_value(value);
        }
        if body.trim().is_empty() {
            return StepOutcome::fail(format!("GET {path} answered an empty page"))
                .with_value(value);
        }
        if let Some(expected) = params
            .get("contains")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            && !body.contains(expected)
        {
            return StepOutcome::fail(format!(
                "GET {path} rendered, without `{expected}`: {}",
                quote()
            ))
            .with_value(value);
        }
        StepOutcome::pass(format!(
            "GET {path} rendered ({}, {} bytes)",
            response.status,
            response.body.len()
        ))
        .with_value(value)
    }
}
