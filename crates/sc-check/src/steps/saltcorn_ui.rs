//! The `saltcorn_ui.*` step types (docs/CHECKLISTS.md §2, TODO C3.3): what
//! Saltcorn UI adds to the core's steps, in its own terms — views by name and
//! fields by name rather than paths and labels. Offered only for its
//! applications.
//!
//! - `saltcorn_ui.open_view` — `browser.open` of `/view/<name>`, with state.
//! - `saltcorn_ui.fill_field` — `browser.fill` of the form field named so,
//!   which survives a relabelling that a `label=` locator does not.
//! - `saltcorn_ui.views_render` — **every view of the application renders
//!   without error for `run_as`**: a free smoke test for a builder app, and
//!   what v1 most lacked. Over HTTP, so it needs no browser. "Without error"
//!   includes naming only fields its table still has: v1's patterns render a
//!   deleted field as an empty cell and answer 200, which is the quiet
//!   breakage a checklist exists to catch.

use sc_browser::Locator;
use sc_error::Error;
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::registry::{FrameworkScope, Needs, StepContext, StepOutcome, StepType};
use crate::steps::browser::{self, MIN_OPEN_TIMEOUT, text};

/// The framework these steps belong to.
const FRAMEWORK: &str = sc_viewpattern::SALTCORN_UI_FRAMEWORK;

/// The most failing views a `views_render` message names.
const MAX_NAMED: usize = 10;

/// The most of a failed view's page a message quotes.
const MAX_QUOTED: usize = 200;

fn scope() -> Option<FrameworkScope> {
    Some(FrameworkScope::named(FRAMEWORK))
}

/// `s` percent-encoded for a path segment or a query component.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The path a view is served at, with `state` as its query.
fn view_path(name: &str, state: Option<&Json>) -> String {
    let mut path = format!("/view/{}", encode(name));
    let pairs: Vec<String> = state
        .and_then(Json::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| {
            let value = match v {
                Json::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", encode(k), encode(&value))
        })
        .collect();
    if !pairs.is_empty() {
        path.push('?');
        path.push_str(&pairs.join("&"));
    }
    path
}

/// `saltcorn_ui.open_view`.
pub struct OpenView;

#[async_trait::async_trait]
impl StepType for OpenView {
    fn name(&self) -> &str {
        "saltcorn_ui.open_view"
    }

    fn description(&self) -> &str {
        "Open a view of the application by name, with state (e.g. {\"id\": 3} for a Show or \
         Edit view), in the headless browser as the checklist's user. Unless the checklist sets \
         implicit_clean to false, the page must also be clean. Its value is {url, status}."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("view", BasicType::Text)
                .label("View")
                .required(),
            FormField::new("state", BasicType::Json)
                .label("State")
                .sublabel("The view's query, e.g. {\"id\": \"{{ order.id }}\"}"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::BROWSER
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        scope()
    }

    fn min_timeout(&self) -> std::time::Duration {
        MIN_OPEN_TIMEOUT
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let name = match text(params, "view") {
            Ok(name) => name.trim().to_owned(),
            Err(e) => return e.into(),
        };
        let app = match cx.require_application() {
            Ok(app) => app,
            Err(e) => return e.into(),
        };
        match sc_viewpattern::load_view(cx.catalog, app.id, &name).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return StepOutcome::fail(format!(
                    "`{}` has no view named `{name}`",
                    app.subdomain
                ));
            }
            Err(e) => return e.into(),
        }
        let path = view_path(&name, params.get("state"));
        browser::Open.run(cx, &json!({ "path": path })).await
    }
}

/// `saltcorn_ui.fill_field`.
pub struct FillField;

#[async_trait::async_trait]
impl StepType for FillField {
    fn name(&self) -> &str {
        "saltcorn_ui.fill_field"
    }

    fn description(&self) -> &str {
        "Type text into the form field for a table field, found by the field's name rather than \
         its label, so the step survives a relabelling. Open the Edit view first."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("field", BasicType::Text)
                .label("Field")
                .required()
                .sublabel("The table field's name, e.g. title"),
            FormField::new("text", BasicType::Text)
                .label("Text")
                .sublabel("May use variables, e.g. Customer {{ run.id }}"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::BROWSER
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        scope()
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let field = match text(params, "field") {
            Ok(field) => field.trim().to_owned(),
            Err(e) => return e.into(),
        };
        let locator = Locator::Css(format!(
            "form [name=\"{}\"]:not([type=hidden])",
            field.replace('\\', "\\\\").replace('"', "\\\"")
        ));
        browser::Fill
            .run(
                cx,
                &json!({
                    "locator": locator.to_string(),
                    "text": params.get("text").cloned().unwrap_or(Json::Null),
                }),
            )
            .await
    }
}

/// `saltcorn_ui.views_render`.
pub struct ViewsRender;

#[async_trait::async_trait]
impl StepType for ViewsRender {
    fn name(&self) -> &str {
        "saltcorn_ui.views_render"
    }

    fn description(&self) -> &str {
        "Every view of the application the checklist's user may see renders without error for \
         them, and every field it names still exists (a v1 pattern shows a deleted field as an \
         empty cell rather than an error). A view whose field, table or trigger has gone fails \
         here, named. Views above the user's role are left out (they are another \
         checklist's); views that need state to render can be skipped by name. Over HTTP, \
         without a browser."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("skip", BasicType::Text)
                .label("Skip")
                .sublabel("Views to leave out, comma-separated"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::HTTP
    }

    fn mutates(&self) -> bool {
        false
    }

    fn framework(&self) -> Option<FrameworkScope> {
        scope()
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        match views_render(cx, params).await {
            Ok(outcome) => outcome,
            Err(e) => e.into(),
        }
    }
}

async fn views_render(cx: &mut StepContext<'_>, params: &Json) -> sc_error::Result<StepOutcome> {
    let app = cx.require_application()?;
    let requests = cx.requests()?;
    let skip: Vec<String> = params
        .get("skip")
        .and_then(Json::as_str)
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let role = cx.role();
    let views = sc_viewpattern::list_views(cx.catalog, app.id).await?;
    let (visible, hidden): (Vec<_>, Vec<_>) = views
        .into_iter()
        .filter(|v| !skip.contains(&v.name))
        .partition(|v| role <= v.min_role);
    if visible.is_empty() {
        return Ok(StepOutcome::pass(format!(
            "`{}` has no view this user may see{}",
            app.subdomain,
            if hidden.is_empty() {
                String::new()
            } else {
                format!(" ({} above their role)", hidden.len())
            }
        )));
    }
    let mut failed: Vec<String> = Vec::new();
    let mut rendered: Vec<String> = Vec::new();
    for view in &visible {
        // First what v1's patterns would hide: a field the view names that its
        // table no longer has renders as an empty cell, with a 200.
        if let Some(problem) = missing_fields(cx, view)? {
            failed.push(format!(
                "`{}` ({} over {}) {problem}",
                view.name,
                view.viewpattern,
                view.table_name.as_deref().unwrap_or("no table"),
            ));
            continue;
        }
        let response = requests
            .request(sc_agent::AppHttpRequest {
                subdomain: &app.subdomain,
                preview: cx.preview(),
                method: "GET".to_owned(),
                path: view_path(&view.name, None),
                headers: Vec::new(),
                body: Vec::new(),
                user: cx.run_as,
                timeout: cx.remaining().max(std::time::Duration::from_secs(1)),
            })
            .await
            .map_err(|e| {
                Error::msg(format!(
                    "view `{}` could not be requested: {}",
                    view.name,
                    sc_error::format_chain(&e)
                ))
            })?;
        if response.status < 400 {
            rendered.push(view.name.clone());
            continue;
        }
        let body = String::from_utf8_lossy(&response.body);
        failed.push(format!(
            "`{}` ({} over {}) answered {}: {}",
            view.name,
            view.viewpattern,
            view.table_name.as_deref().unwrap_or("no table"),
            response.status,
            page_text(&body)
        ));
    }
    let value = json!({ "rendered": rendered, "failed": failed });
    if failed.is_empty() {
        return Ok(StepOutcome::pass(format!(
            "{} view{} rendered{}",
            rendered.len(),
            if rendered.len() == 1 { "" } else { "s" },
            if hidden.is_empty() {
                String::new()
            } else {
                format!("; {} above this user's role left out", hidden.len())
            }
        ))
        .with_value(value));
    }
    let total = failed.len();
    let mut lines: Vec<String> = failed.into_iter().take(MAX_NAMED).collect();
    if total > MAX_NAMED {
        lines.push(format!("and {} more", total - MAX_NAMED));
    }
    Ok(StepOutcome::fail(format!(
        "{total} of {} views did not render:\n{}",
        visible.len(),
        lines.join("\n")
    ))
    .with_value(value))
}

/// What is wrong with the fields `view` names, if anything: its table is
/// gone, or fields it shows are.
fn missing_fields(
    cx: &StepContext<'_>,
    view: &sc_viewpattern::View,
) -> sc_error::Result<Option<String>> {
    let Some(table_name) = &view.table_name else {
        return Ok(None);
    };
    let Some(table) = cx.catalog.get(table_name)? else {
        return Ok(Some(format!(
            "is over the table `{table_name}`, which is gone"
        )));
    };
    let configuration = Json::Object(view.configuration.clone());
    let gone: Vec<String> = sc_viewpattern::referenced_fields(&configuration)
        .into_iter()
        .filter(|f| table.field(f).is_none())
        .map(|f| format!("`{f}`"))
        .collect();
    Ok((!gone.is_empty()).then(|| {
        format!(
            "names field{} `{table_name}` no longer has: {}",
            if gone.len() == 1 { "" } else { "s" },
            gone.join(", ")
        )
    }))
}

/// What a failed view's page says, without its markup: the error document
/// Saltcorn UI answers names the view and why.
fn page_text(html: &str) -> String {
    let body = html
        .find("<main")
        .or_else(|| html.find("<body"))
        .map_or(html, |i| &html[i..]);
    let mut text = String::new();
    let mut in_tag = false;
    let mut skipping = false;
    let lower = body.to_ascii_lowercase();
    let mut i = 0;
    let bytes = body.as_bytes();
    while i < bytes.len() {
        if !in_tag && (lower[i..].starts_with("<script") || lower[i..].starts_with("<style")) {
            skipping = true;
        }
        if skipping && (lower[i..].starts_with("</script>") || lower[i..].starts_with("</style>")) {
            skipping = false;
        }
        let c = body[i..].chars().next().unwrap_or(' ');
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            c if !in_tag && !skipping => text.push(c),
            _ => {}
        }
        i += c.len_utf8();
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= MAX_QUOTED {
        return text;
    }
    format!("{}…", text.chars().take(MAX_QUOTED).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_views_path_carries_its_state_encoded() {
        assert_eq!(view_path("List Books", None), "/view/List%20Books");
        assert_eq!(
            view_path("Edit", Some(&json!({ "id": 3, "q": "a&b", "none": null }))),
            "/view/Edit?id=3&q=a%26b"
        );
    }

    #[test]
    fn a_failed_pages_text_is_quoted_without_markup() {
        let html = "<html><head><style>p{}</style></head><body><main><h1>This could not be \
                    shown</h1><script>x()</script><p>No field <b>pages</b></p></main></body>";
        assert_eq!(page_text(html), "This could not be shown No field pages");
    }
}
