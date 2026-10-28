//! The `browser.*` step types (docs/CHECKLISTS.md §2, TODO C2.3, C2.4): the
//! checklist's application in the server's headless browser, signed in as
//! `run_as`, one browser context per run.
//!
//! Elements are named by **stable locators** (`button "Save"`, `label=Email`,
//! `text="Order saved"`, `testid=submit`, `css=…`; see
//! [`sc_browser::locator`]), never by a snapshot's `@eN` refs, which are valid
//! for one snapshot only. An action waits for its element until the step's time
//! is up, and refuses a locator that matches several; an assertion polls the
//! page until it holds or the time is up, so no step needs a `core.wait` in
//! front of it.
//!
//! **A clean page is checked implicitly.** After every `browser.open` the page
//! must have logged no console error, thrown no uncaught exception and had no
//! request fail or answer 4xx/5xx — a white screen from a thrown error is the
//! commonest UI regression. A checklist turns it off with
//! `attributes.implicit_clean = false`; `browser.expect_clean` checks the same
//! at any point. Whatever a step's browser calls saw is on its result either
//! way.
//!
//! Offered only for an application whose framework serves a UI.
//!
//! None of these steps counts as **mutating**: a checklist that only opens
//! pages and clicks runs on production (§3). One that submits forms there
//! writes rows as `run_as`, which is why that user should be a check user.

use std::str::FromStr;
use std::time::Duration;

use sc_browser::{BrowserAction, BrowserReport, Element, ElementInfo, Locator, TextMatch};
use sc_error::{Error, Result};
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::registry::{Needs, StepContext, StepOutcome, StepType};

/// How often an assertion looks at the page again.
pub(crate) const POLL: Duration = Duration::from_millis(250);

/// How long `browser.open` waits after the page loaded before it checks the
/// page is clean, for errors thrown once the first data has arrived.
pub(crate) const SETTLE: Duration = Duration::from_millis(400);

/// The least time `browser.open` is given: the first one of a run may have to
/// start the browser. Other steps keep the checklist's timeout, so a locator
/// that finds nothing fails as soon as the checklist says it should.
pub(crate) const MIN_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of the page's text a failure message quotes.
const MAX_QUOTED: usize = 300;

pub(crate) fn locator_field(required: bool) -> FormField {
    let field = FormField::new("locator", BasicType::Text)
        .label("Element")
        .sublabel(
            "button \"Save\", link Sign in, label=Email, text=\"Order saved\", testid=submit or \
             css=.selector",
        );
    if required { field.required() } else { field }
}

/// A string parameter, required.
pub(crate) fn text<'p>(params: &'p Json, key: &str) -> Result<&'p str> {
    params
        .get(key)
        .and_then(Json::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// The `locator` parameter, parsed.
fn locator(params: &Json) -> Result<Locator> {
    Locator::from_str(text(params, "locator")?)
}

/// The optional `locator` parameter, parsed.
fn optional_locator(params: &Json) -> Result<Option<Locator>> {
    match params.get("locator").and_then(Json::as_str).map(str::trim) {
        None | Some("") => Ok(None),
        Some(raw) => Locator::from_str(raw).map(Some),
    }
}

/// Check a locator as the step is saved, unless it is a template.
fn check_locator(params: &Json) -> Result<()> {
    match params.get("locator").and_then(Json::as_str) {
        Some(raw) if !raw.trim().is_empty() && !raw.contains("{{") => {
            Locator::from_str(raw).map(|_| ())
        }
        _ => Ok(()),
    }
}

/// Poll the page — the elements `locator` finds, and its visible text — until
/// `done` holds or the step's time is nearly up. The last report either way.
pub(crate) async fn poll(
    cx: &mut StepContext<'_>,
    locator: Option<&Locator>,
    mut done: impl FnMut(&BrowserReport) -> bool,
) -> Result<(bool, BrowserReport)> {
    loop {
        let report = cx
            .browser(BrowserAction::Inspect {
                locator: locator.cloned(),
            })
            .await?;
        if done(&report) {
            return Ok((true, report));
        }
        if cx.remaining() <= POLL * 4 {
            return Ok((false, report));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The page's visible text, shortened for a message.
pub(crate) fn quoted(report: &BrowserReport) -> String {
    let text = report.text.as_deref().unwrap_or("").replace('\n', " · ");
    if text.chars().count() <= MAX_QUOTED {
        return text;
    }
    let cut: String = text.chars().take(MAX_QUOTED).collect();
    format!("{cut}…")
}

/// The path and query of a URL, without its scheme and host.
pub(crate) fn path_of(url: &str) -> &str {
    match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => url,
    }
}

/// Fail when the step's browser calls saw console errors or failed requests.
pub(crate) fn clean(cx: &StepContext<'_>, passed: String) -> StepOutcome {
    let (errors, failed) = cx.seen();
    if errors.is_empty() && failed.is_empty() {
        return StepOutcome::pass(passed);
    }
    let mut problems: Vec<String> = Vec::new();
    if !errors.is_empty() {
        problems.push(format!(
            "{} console error{} ({})",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" },
            errors[0]
        ));
    }
    if !failed.is_empty() {
        problems.push(format!(
            "{} failed request{} ({})",
            failed.len(),
            if failed.len() == 1 { "" } else { "s" },
            failed[0]
        ));
    }
    StepOutcome::fail(format!(
        "{passed}, but the page is not clean: {}",
        problems.join("; ")
    ))
}

macro_rules! browser_type {
    ($name:literal, $desc:literal) => {
        fn name(&self) -> &str {
            $name
        }
        fn description(&self) -> &str {
            $desc
        }
        fn needs(&self) -> Needs {
            Needs::BROWSER
        }
        fn mutates(&self) -> bool {
            false
        }
    };
}

/// `browser.open`: open a path of the application.
pub struct Open;

#[async_trait::async_trait]
impl StepType for Open {
    browser_type!(
        "browser.open",
        "Open a path of the checklist's application in the headless browser, signed in as the \
         checklist's user. Unless the checklist sets implicit_clean to false, the page must also \
         be clean: no console errors, no uncaught exceptions, no failed requests. Its value is \
         {url, status}."
    );

    fn min_timeout(&self) -> Duration {
        MIN_OPEN_TIMEOUT
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("path", BasicType::Text)
                .label("Path")
                .required()
                .default_value("/")
                .sublabel("Within the application, e.g. /orders/{{ order.id }}"),
        ]
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let path = match text(params, "path") {
            Ok(path) => path.trim().to_owned(),
            Err(e) => return e.into(),
        };
        let report = match cx.browser(BrowserAction::Goto { path: path.clone() }).await {
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
        if !cx.implicit_clean() {
            return StepOutcome::pass(opened).with_value(value);
        }
        // Errors a page throws once its first data has arrived come a moment
        // after the load: one more look collects them.
        tokio::time::sleep(SETTLE).await;
        if let Err(e) = cx.browser(BrowserAction::Inspect { locator: None }).await {
            return e.into();
        }
        clean(cx, opened).with_value(value)
    }
}

/// `browser.click`.
pub struct Click;

#[async_trait::async_trait]
impl StepType for Click {
    browser_type!(
        "browser.click",
        "Click the element a locator names (waiting for it to appear). The locator must name \
         exactly one element."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![locator_field(true)]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        act(
            cx,
            params,
            |element, _| Ok(BrowserAction::Click { element }),
            "clicked",
        )
        .await
    }
}

/// `browser.fill`.
pub struct Fill;

#[async_trait::async_trait]
impl StepType for Fill {
    browser_type!(
        "browser.fill",
        "Type text into a field, replacing what it held."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            locator_field(true),
            FormField::new("text", BasicType::Text)
                .label("Text")
                .sublabel("May use variables, e.g. Customer {{ run.id }}"),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        act(
            cx,
            params,
            |element, params| {
                Ok(BrowserAction::Fill {
                    element,
                    text: params
                        .get("text")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_owned(),
                })
            },
            "filled",
        )
        .await
    }
}

/// `browser.select`.
pub struct Select;

#[async_trait::async_trait]
impl StepType for Select {
    browser_type!(
        "browser.select",
        "Choose an option of a select box, by its value or its text."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            locator_field(true),
            FormField::new("option", BasicType::Text)
                .label("Option")
                .required(),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        act(
            cx,
            params,
            |element, params| {
                Ok(BrowserAction::Select {
                    element,
                    option: text(params, "option")?.to_owned(),
                })
            },
            "chose an option of",
        )
        .await
    }
}

/// `browser.check`.
pub struct Check;

#[async_trait::async_trait]
impl StepType for Check {
    browser_type!(
        "browser.check",
        "Make a checkbox, radio button or switch checked (or, with checked false, unchecked); \
         it is clicked only when it is not so already."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            locator_field(true),
            FormField::new("checked", BasicType::Bool)
                .label("Checked")
                .default_value(true),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let checked = params
            .get("checked")
            .and_then(Json::as_bool)
            .unwrap_or(true);
        act(
            cx,
            params,
            |element, _| Ok(BrowserAction::Check { element, checked }),
            if checked { "checked" } else { "unchecked" },
        )
        .await
    }
}

/// `browser.press`.
pub struct Press;

#[async_trait::async_trait]
impl StepType for Press {
    browser_type!(
        "browser.press",
        "Press a key (Enter, Tab, Escape, ArrowDown, …) in the focused element, or in the element \
         a locator names."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("key", BasicType::Text)
                .label("Key")
                .required()
                .default_value("Enter"),
            locator_field(false),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let (key, element) = match (text(params, "key"), optional_locator(params)) {
            (Ok(key), Ok(element)) => (key.to_owned(), element.map(Element::Locator)),
            (Err(e), _) | (_, Err(e)) => return e.into(),
        };
        let what = element
            .as_ref()
            .map_or_else(String::new, |e| format!(" in `{e}`"));
        match cx
            .browser(BrowserAction::Press {
                key: key.clone(),
                element,
            })
            .await
        {
            Ok(_) => StepOutcome::pass(format!("pressed {key}{what}")),
            Err(e) => StepOutcome::fail(format!("could not press {key}{what}: {e}")),
        }
    }
}

/// `browser.upload`.
pub struct Upload;

#[async_trait::async_trait]
impl StepType for Upload {
    browser_type!(
        "browser.upload",
        "Choose a file for a file input: a file of the given name with the given text content."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            locator_field(true),
            FormField::new("filename", BasicType::Text)
                .label("File name")
                .required()
                .default_value("test.txt"),
            FormField::new("content", BasicType::Text)
                .label("Content")
                .multiline(),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)?;
        match params.get("filename").and_then(Json::as_str) {
            Some(name) if name.contains(['/', '\\']) || name.starts_with('.') => Err(
                Error::invalid(format!("`filename` is a plain file name, not `{name}`")),
            ),
            _ => Ok(()),
        }
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        if let Err(e) = self.check(params) {
            return e.into();
        }
        let file = match (text(params, "filename"), cx.scratch_dir()) {
            (Ok(name), Ok(dir)) => dir.join(name.trim()),
            (Err(e), _) | (_, Err(e)) => return e.into(),
        };
        let content = params.get("content").and_then(Json::as_str).unwrap_or("");
        if let Err(e) = std::fs::write(&file, content) {
            return StepOutcome::error(format!("the file could not be written: {e}"));
        }
        act(
            cx,
            params,
            |element, _| {
                Ok(BrowserAction::Upload {
                    element,
                    files: vec![file.clone()],
                })
            },
            "chose a file for",
        )
        .await
    }
}

/// Run one action on the element `params.locator` names. A failure to find or
/// use the element is the step's failure: it is what the checklist asserts.
async fn act(
    cx: &mut StepContext<'_>,
    params: &Json,
    action: impl FnOnce(Element, &Json) -> Result<BrowserAction>,
    did: &str,
) -> StepOutcome {
    let locator = match locator(params) {
        Ok(locator) => locator,
        Err(e) => return e.into(),
    };
    let action = match action(Element::Locator(locator.clone()), params) {
        Ok(action) => action,
        Err(e) => return e.into(),
    };
    let name = action.name();
    match cx.browser(action).await {
        Ok(_) => StepOutcome::pass(format!("{did} `{locator}`")),
        Err(e) => StepOutcome::fail(format!(
            "could not {name} `{locator}`: {}",
            sc_error::format_chain(&e)
        )),
    }
}

/// `browser.expect_text`.
pub struct ExpectText;

#[async_trait::async_trait]
impl StepType for ExpectText {
    browser_type!(
        "browser.expect_text",
        "Assert that text is visible on the page (in the accessibility tree: hidden text does not \
         count), waiting for it. Case and runs of whitespace are ignored."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("text", BasicType::Text)
                .label("Text")
                .required(),
        ]
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let want = match text(params, "text") {
            Ok(want) => TextMatch::contains(want),
            Err(e) => return e.into(),
        };
        match poll(cx, None, |r| want.matches(r.text.as_deref().unwrap_or(""))).await {
            Ok((true, _)) => StepOutcome::pass(format!("{:?} is on the page", want.text)),
            Ok((false, report)) => StepOutcome::fail(format!(
                "{:?} is not on {}; it shows: {}",
                want.text,
                path_of(&report.url),
                quoted(&report)
            )),
            Err(e) => e.into(),
        }
    }
}

/// `browser.expect_no_text`.
pub struct ExpectNoText;

#[async_trait::async_trait]
impl StepType for ExpectNoText {
    browser_type!(
        "browser.expect_no_text",
        "Assert that text is not visible on the page, waiting for it to go (a \"Loading…\" or an \
         error message)."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("text", BasicType::Text)
                .label("Text")
                .required(),
        ]
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let unwanted = match text(params, "text") {
            Ok(unwanted) => TextMatch::contains(unwanted),
            Err(e) => return e.into(),
        };
        match poll(cx, None, |r| {
            !unwanted.matches(r.text.as_deref().unwrap_or(""))
        })
        .await
        {
            Ok((true, _)) => StepOutcome::pass(format!("{:?} is not on the page", unwanted.text)),
            Ok((false, report)) => StepOutcome::fail(format!(
                "{:?} is still on {}; it shows: {}",
                unwanted.text,
                path_of(&report.url),
                quoted(&report)
            )),
            Err(e) => e.into(),
        }
    }
}

/// What `browser.expect_element` expects of the elements it finds.
#[derive(Debug, Clone, PartialEq)]
struct Expectation {
    state: String,
    count: Option<usize>,
    value: Option<String>,
}

impl Expectation {
    fn of(params: &Json) -> Result<Expectation> {
        let state = params
            .get("state")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("visible")
            .to_owned();
        if !STATES.contains(&state.as_str()) {
            return Err(Error::invalid(format!(
                "`state` is one of {}, not `{state}`",
                STATES.join(", ")
            )));
        }
        let count = match params.get("count") {
            None | Some(Json::Null) => None,
            Some(n) => Some(
                n.as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| {
                        Error::invalid(format!("`count` should be a number, got {n}"))
                    })?,
            ),
        };
        let value = params
            .get("value")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        Ok(Expectation {
            state,
            count,
            value,
        })
    }

    /// Whether the elements found meet it.
    fn met(&self, found: &[ElementInfo]) -> bool {
        let visible: Vec<&ElementInfo> = found.iter().filter(|e| e.visible).collect();
        if self.state == "hidden" {
            return visible.is_empty();
        }
        let count_ok = match self.count {
            Some(n) => visible.len() == n,
            None => !visible.is_empty(),
        };
        let each = |e: &&ElementInfo| match self.state.as_str() {
            "enabled" => e.enabled,
            "disabled" => !e.enabled,
            "checked" => e.checked == Some(true),
            "unchecked" => e.checked == Some(false),
            _ => true,
        };
        let value_ok = |e: &&ElementInfo| {
            self.value.as_ref().is_none_or(|want| {
                TextMatch::exact(want.as_str()).matches(e.value.as_deref().unwrap_or(""))
            })
        };
        count_ok && visible.iter().all(|e| each(e) && value_ok(e))
    }

    fn describe(&self) -> String {
        let mut out = match self.count {
            Some(n) => format!("{n} {}", self.state),
            None => self.state.clone(),
        };
        if let Some(value) = &self.value {
            out.push_str(&format!(" with value {value:?}"));
        }
        out
    }
}

/// The states `browser.expect_element` checks.
const STATES: [&str; 6] = [
    "visible",
    "hidden",
    "enabled",
    "disabled",
    "checked",
    "unchecked",
];

/// `browser.expect_element`.
pub struct ExpectElement;

#[async_trait::async_trait]
impl StepType for ExpectElement {
    browser_type!(
        "browser.expect_element",
        "Assert the elements a locator finds, waiting until they match: visible (at least one, or \
         exactly `count`), hidden (none visible), enabled, disabled, checked or unchecked, and \
         optionally that each has `value`."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            locator_field(true),
            FormField::new("state", BasicType::Text)
                .label("State")
                .options(STATES)
                .default_value("visible"),
            FormField::new("count", BasicType::Int)
                .label("Count")
                .sublabel("Exactly this many visible; empty for at least one"),
            FormField::new("value", BasicType::Text)
                .label("Value")
                .sublabel("A field's text, a select's chosen option"),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        check_locator(params)?;
        Expectation::of(params).map(|_| ())
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let (locator, expected) = match (locator(params), Expectation::of(params)) {
            (Ok(locator), Ok(expected)) => (locator, expected),
            (Err(e), _) | (_, Err(e)) => return e.into(),
        };
        let found = |r: &BrowserReport| r.elements.clone().unwrap_or_default();
        match poll(cx, Some(&locator), |r| expected.met(&found(r))).await {
            Ok((true, _)) => StepOutcome::pass(format!("`{locator}` is {}", expected.describe())),
            Ok((false, report)) => {
                let found = found(&report);
                let seen: Vec<String> = found
                    .iter()
                    .filter(|e| e.visible)
                    .take(5)
                    .map(|e| {
                        let mut line = format!("{} {:?}", e.role, e.name);
                        if !e.enabled {
                            line.push_str(" disabled");
                        }
                        if let Some(checked) = e.checked {
                            line.push_str(if checked { " checked" } else { " unchecked" });
                        }
                        if let Some(value) = &e.value {
                            line.push_str(&format!(" = {value:?}"));
                        }
                        line
                    })
                    .collect();
                StepOutcome::fail(format!(
                    "`{locator}` is not {}: {}",
                    expected.describe(),
                    if seen.is_empty() {
                        "no visible element matches".to_owned()
                    } else {
                        format!("it finds {}", seen.join(", "))
                    }
                ))
            }
            Err(e) => e.into(),
        }
    }
}

/// `browser.expect_url`.
pub struct ExpectUrl;

#[async_trait::async_trait]
impl StepType for ExpectUrl {
    browser_type!(
        "browser.expect_url",
        "Assert the page's path (with its query string) after a navigation or redirect, waiting \
         for it: it `equals` the value, `contains` it, or `matches` a regular expression."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("url", BasicType::Text)
                .label("Path")
                .required()
                .sublabel("e.g. /orders/{{ order.id }}"),
            FormField::new("op", BasicType::Text)
                .label("Comparison")
                .options(["equals", "contains", "matches"])
                .default_value("equals"),
        ]
    }

    fn check(&self, params: &Json) -> Result<()> {
        let op = params.get("op").and_then(Json::as_str).unwrap_or("equals");
        match op {
            "equals" | "contains" | "" => Ok(()),
            "matches" => match params.get("url").and_then(Json::as_str) {
                Some(pattern) if !pattern.contains("{{") => regex_lite::Regex::new(pattern)
                    .map(|_| ())
                    .map_err(|e| Error::invalid(format!("`url` is not a regular expression: {e}"))),
                _ => Ok(()),
            },
            other => Err(Error::invalid(format!(
                "`op` should be equals, contains or matches, not `{other}`"
            ))),
        }
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        if let Err(e) = self.check(params) {
            return e.into();
        }
        let want = match text(params, "url") {
            Ok(want) => want.trim().to_owned(),
            Err(e) => return e.into(),
        };
        let op = params
            .get("op")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("equals")
            .to_owned();
        let pattern = match op.as_str() {
            "matches" => match regex_lite::Regex::new(&want) {
                Ok(re) => Some(re),
                Err(e) => {
                    return StepOutcome::error(format!("`url` is not a regular expression: {e}"));
                }
            },
            _ => None,
        };
        let holds = |url: &str| {
            let path = path_of(url);
            match (op.as_str(), &pattern) {
                ("contains", _) => path.contains(want.as_str()),
                (_, Some(re)) => re.is_match(path),
                // Without a query string, the path alone is compared.
                _ => path == want || (!want.contains('?') && path.split('?').next() == Some(&want)),
            }
        };
        match poll(cx, None, |r| holds(&r.url)).await {
            Ok((true, report)) => {
                StepOutcome::pass(format!("the page is {}", path_of(&report.url)))
            }
            Ok((false, report)) => StepOutcome::fail(format!(
                "the page is {}, which does not {op} {want}",
                path_of(&report.url)
            )),
            Err(e) => e.into(),
        }
    }
}

/// `browser.expect_clean`.
pub struct ExpectClean;

#[async_trait::async_trait]
impl StepType for ExpectClean {
    browser_type!(
        "browser.expect_clean",
        "Assert that since the last step the page logged no console error, threw no uncaught \
         exception and had no request fail or answer 4xx/5xx. Runs after every browser.open by \
         itself unless the checklist sets implicit_clean to false."
    );

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    async fn run(&self, cx: &mut StepContext<'_>, _params: &Json) -> StepOutcome {
        tokio::time::sleep(SETTLE).await;
        match cx.browser(BrowserAction::Inspect { locator: None }).await {
            Ok(report) => clean(cx, format!("{} is clean", path_of(&report.url))),
            Err(e) => e.into(),
        }
    }
}

/// `browser.screenshot`.
pub struct Screenshot;

#[async_trait::async_trait]
impl StepType for Screenshot {
    browser_type!(
        "browser.screenshot",
        "Take a screenshot of the page and keep it with the run, for a person to look at. There \
         is no comparison: it never fails a checklist on what it shows."
    );

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("full_page", BasicType::Bool)
                .label("Whole page")
                .default_value(false),
        ]
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let full_page = params
            .get("full_page")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        match cx.browser(BrowserAction::Screenshot { full_page }).await {
            Ok(report) => match report.screenshot {
                Some(jpeg) => {
                    let size = jpeg.len().div_ceil(1024);
                    cx.keep_screenshot(jpeg);
                    StepOutcome::pass(format!(
                        "took a screenshot of {} ({size} KB)",
                        path_of(&report.url)
                    ))
                }
                None => StepOutcome::error("the browser returned no screenshot"),
            },
            Err(e) => e.into(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn element(
        visible: bool,
        enabled: bool,
        checked: Option<bool>,
        value: Option<&str>,
    ) -> ElementInfo {
        ElementInfo {
            role: "checkbox".into(),
            name: "Paid".into(),
            value: value.map(str::to_owned),
            enabled,
            checked,
            visible,
        }
    }

    #[test]
    fn an_expectation_is_met_by_the_visible_elements() {
        let visible = Expectation::of(&json!({})).unwrap();
        assert!(visible.met(&[element(true, true, None, None)]));
        assert!(!visible.met(&[element(false, true, None, None)]));
        assert!(!visible.met(&[]));

        let hidden = Expectation::of(&json!({"state": "hidden"})).unwrap();
        assert!(hidden.met(&[element(false, true, None, None)]));
        assert!(!hidden.met(&[element(true, true, None, None)]));

        let two = Expectation::of(&json!({"count": 2})).unwrap();
        assert!(!two.met(&[element(true, true, None, None)]));
        assert!(two.met(&[
            element(true, true, None, None),
            element(true, true, None, None)
        ]));

        let checked = Expectation::of(&json!({"state": "checked"})).unwrap();
        assert!(checked.met(&[element(true, true, Some(true), None)]));
        assert!(!checked.met(&[element(true, true, Some(false), None)]));

        let disabled = Expectation::of(&json!({"state": "disabled"})).unwrap();
        assert!(disabled.met(&[element(true, false, None, None)]));

        let value = Expectation::of(&json!({"value": "a@b.c"})).unwrap();
        assert!(value.met(&[element(true, true, None, Some("A@B.C"))]));
        assert!(!value.met(&[element(true, true, None, Some("x"))]));

        assert!(Expectation::of(&json!({"state": "shiny"})).is_err());
    }

    #[test]
    fn a_bad_locator_is_refused_when_the_step_is_saved_unless_it_is_a_template() {
        assert!(Click.check(&json!({"locator": "button \"Save\""})).is_ok());
        assert!(Click.check(&json!({"locator": "#save"})).is_err());
        assert!(Click.check(&json!({"locator": "{{ which }}"})).is_ok());
        assert!(
            Upload
                .check(&json!({"locator": "label=File", "filename": "../x"}))
                .is_err()
        );
        assert!(
            ExpectUrl
                .check(&json!({"url": "(", "op": "matches"}))
                .is_err()
        );
        assert!(ExpectUrl.check(&json!({"url": "/x", "op": "is"})).is_err());
    }

    #[test]
    fn a_url_is_compared_by_its_path() {
        assert_eq!(
            path_of("http://a--b.example.com/orders/3?x=1"),
            "/orders/3?x=1"
        );
        assert_eq!(path_of("http://a.example.com"), "/");
    }
}
