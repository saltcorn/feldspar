//! Step types a **module** declares (docs/CHECKLISTS.md §2, TODO C3.5).
//!
//! A module declares its step types as data — name, description, parameters,
//! what it needs, whether it writes — the way it declares a framework
//! (`sc_app::declared`): the declaration is read once, when the module loads,
//! and a [`DeclaredStep`] is that declaration as a [`StepType`]. Only `run`
//! is a call, to the module's worker ([`StepWorker`]).
//!
//! # Host calls only
//!
//! A module step never sees the Chrome DevTools protocol, the router or the
//! database. Everything it does, it asks the host for while its call runs
//! ([`StepAsks`], served by [`answer`]), and the host does it **in the step's
//! own context** — the run's browser page, its user, its variables, its
//! clean-up list:
//!
//! - `{ "op": "step", "type": "browser.click", "params": {…} }` — run one of
//!   the core's step types (`browser.*`, `http.*`, `data.*`, `core.wait`) and
//!   answer its `{ outcome, message, value }`. What the module could do, it
//!   does by composing what the core already checks.
//! - `{ "op": "inspect", "locator": "button \"Save\"" }` — the page now: its
//!   URL, visible text and the elements the locator finds, for an assertion
//!   of the module's own.
//! - `{ "op": "rows", "table": "orders", "where": {…}, "limit": 100 }` — the
//!   rows `run_as` can read.
//!
//! **The declaration is the boundary.** A core step needing a capability the
//! module's type did not declare is refused by the same check that refuses a
//! core type's undeclared use ([`StepContext`]), and a writing step is refused
//! to a type that did not declare `mutates` — so `Checklist::mutates`, and with
//! it the production guard, is told the truth.

use std::sync::Arc;
use std::time::Duration;

use sc_error::{Error, Result};
use sc_types::FormField;
use serde_json::{Value as Json, json};
use tokio::sync::Mutex;

use crate::model::Outcome;
use crate::registry::{FrameworkScope, Needs, StepContext, StepOutcome, StepType};

/// The most rows a `rows` ask answers.
const MAX_ROWS: u64 = 1000;

/// A step type as a module declared it.
#[derive(Debug, Clone, PartialEq)]
pub struct StepDecl {
    /// The module that declared it, whose worker runs it.
    pub module: String,
    /// The namespaced name: `astro.has_title`.
    pub name: String,
    /// What it does.
    pub description: String,
    /// Its parameters.
    pub config_spec: Vec<FormField>,
    /// What it needs from the host.
    pub needs: Needs,
    /// Whether it writes data.
    pub mutates: bool,
    /// The framework whose applications alone it checks, if it is one's.
    pub framework: Option<String>,
    /// The least time a step of it is given.
    pub min_timeout: Duration,
}

/// Reads a module step type's `needs` list: `data`, `http`, `browser`,
/// `build`. An unknown name is an error naming the ones there are.
pub fn parse_needs(names: &[String]) -> Result<Needs> {
    let mut needs = Needs::NONE;
    for name in names {
        match name.trim() {
            "data" => needs.data = true,
            "http" => needs.http = true,
            "browser" => needs.browser = true,
            "build" => needs.build = true,
            other => {
                return Err(Error::invalid(format!(
                    "`{other}` is not something a step can need; it needs any of data, http, \
                     browser and build"
                )));
            }
        }
    }
    Ok(needs)
}

/// What one call of a module step asks of the host while it runs.
#[async_trait::async_trait]
pub trait StepAsks: Send + Sync {
    /// Answer one ask (see the module docs for the plans).
    async fn ask(&self, plan: Json) -> Result<Json>;
}

/// Runs a declared step in its module's worker.
#[async_trait::async_trait]
pub trait StepWorker: Send + Sync {
    /// Call `step` of `module` with `request` — `{ params, context }` — and
    /// answer what it returned, serving its asks from `asks` meanwhile.
    async fn run(
        &self,
        module: &str,
        step: &str,
        request: Json,
        asks: &dyn StepAsks,
    ) -> Result<Json>;
}

/// A module's step type.
pub struct DeclaredStep {
    decl: StepDecl,
    worker: Arc<dyn StepWorker>,
}

impl DeclaredStep {
    /// `decl`, run by `worker`.
    pub fn new(decl: StepDecl, worker: Arc<dyn StepWorker>) -> DeclaredStep {
        DeclaredStep { decl, worker }
    }

    /// The declaration.
    pub fn decl(&self) -> &StepDecl {
        &self.decl
    }
}

#[async_trait::async_trait]
impl StepType for DeclaredStep {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn config_spec(&self) -> Vec<FormField> {
        self.decl.config_spec.clone()
    }

    fn needs(&self) -> Needs {
        self.decl.needs
    }

    fn mutates(&self) -> bool {
        self.decl.mutates
    }

    fn framework(&self) -> Option<FrameworkScope> {
        self.decl.framework.as_deref().map(FrameworkScope::named)
    }

    fn min_timeout(&self) -> Duration {
        self.decl.min_timeout
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let request = json!({
            "params": params,
            "context": {
                "run": { "id": cx.run_id.to_string() },
                "vars": cx.vars,
                "user": cx.run_as.map(|u| json!({
                    "id": u.id.to_string(),
                    "email": u.get(sc_auth::COL_EMAIL).and_then(sc_query::Value::as_text),
                    "role": u.role,
                })),
                "application": cx.application.map(|a| json!({
                    "id": a.id.0.to_string(),
                    "subdomain": a.subdomain,
                    "framework": a.framework.name,
                })),
                "timeout_ms": u64::try_from(cx.timeout.as_millis()).unwrap_or(u64::MAX),
            },
        });
        let asks = HostCalls {
            cx: Mutex::new(cx),
            mutates: self.decl.mutates,
            step: self.decl.name.clone(),
        };
        let answered = self
            .worker
            .run(&self.decl.module, &self.decl.name, request, &asks)
            .await;
        match answered {
            Ok(answer) => outcome_of(&answer, &self.decl),
            Err(e) => StepOutcome::error(format!(
                "`{}` of module {} failed: {}",
                self.decl.name,
                self.decl.module,
                sc_error::format_chain(&e)
            )),
        }
    }
}

/// What a module's `run` returned, as an outcome: `{ outcome, message, value }`,
/// or `true`/`false` for a bare pass or fail.
fn outcome_of(answer: &Json, decl: &StepDecl) -> StepOutcome {
    let (outcome, message, value) = match answer {
        Json::Bool(true) => (Outcome::Pass, String::new(), None),
        Json::Bool(false) => (Outcome::Fail, String::new(), None),
        Json::Object(map) => {
            let outcome = match map.get("outcome").and_then(Json::as_str) {
                Some("pass") => Outcome::Pass,
                Some("fail") => Outcome::Fail,
                Some("error") => Outcome::Error,
                other => {
                    return StepOutcome::error(format!(
                        "`{}` of module {} answered outcome {}; a step answers \"pass\", \
                         \"fail\" or \"error\"",
                        decl.name,
                        decl.module,
                        other.map_or_else(|| "nothing".to_owned(), |o| format!("`{o}`"))
                    ));
                }
            };
            let message = map
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            (outcome, message, map.get("value").cloned())
        }
        other => {
            return StepOutcome::error(format!(
                "`{}` of module {} answered {other}; a step answers {{ outcome, message, value \
                 }}",
                decl.name, decl.module
            ));
        }
    };
    let message = if message.is_empty() {
        format!("{} ({})", outcome.as_str(), decl.name)
    } else {
        message
    };
    let result = match outcome {
        Outcome::Pass => StepOutcome::pass(message),
        Outcome::Fail => StepOutcome::fail(message),
        _ => StepOutcome::error(message),
    };
    match value {
        Some(value) if !value.is_null() => result.with_value(value),
        _ => result,
    }
}

/// The asks of one module step's call, served in its context.
struct HostCalls<'c, 'a> {
    /// The step's context. Asks are answered one at a time: they act on one
    /// browser page and one clean-up list, in the order the module awaited
    /// them.
    cx: Mutex<&'c mut StepContext<'a>>,
    /// Whether the module's type declared that it writes.
    mutates: bool,
    /// Its name, for refusals.
    step: String,
}

#[async_trait::async_trait]
impl StepAsks for HostCalls<'_, '_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let mut cx = self.cx.lock().await;
        answer(&mut cx, &plan, self.mutates, &self.step).await
    }
}

/// Answer one ask of a module step, in `cx`.
pub(crate) async fn answer(
    cx: &mut StepContext<'_>,
    plan: &Json,
    mutates: bool,
    step: &str,
) -> Result<Json> {
    match plan.get("op").and_then(Json::as_str).unwrap_or("") {
        "step" => {
            let name = plan.get("type").and_then(Json::as_str).unwrap_or("");
            let core = crate::steps::builtin()
                .into_iter()
                .find(|t| t.name() == name)
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`{name}` is not a core step type; a module step can run the core's \
                         (browser.*, http.*, data.*, core.wait)"
                    ))
                })?;
            if core.mutates() && !mutates {
                return Err(Error::auth(format!(
                    "`{step}` runs `{name}`, which writes data, and does not declare `mutates`; \
                     a step type that writes says so, so the production guard knows"
                )));
            }
            if core.needs().build {
                return Err(Error::invalid(format!(
                    "a module step cannot run `{name}`; put it in the checklist itself"
                )));
            }
            let params = plan.get("params").cloned().unwrap_or_else(|| json!({}));
            let outcome = core.run(cx, &params).await;
            Ok(json!({
                "outcome": outcome.outcome.as_str(),
                "message": outcome.message,
                "value": outcome.value,
            }))
        }
        "inspect" => {
            let locator = match plan.get("locator").and_then(Json::as_str).map(str::trim) {
                None | Some("") => None,
                Some(raw) => Some(raw.parse::<sc_browser::Locator>()?),
            };
            let report = cx
                .browser(sc_browser::BrowserAction::Inspect { locator })
                .await?;
            Ok(json!({
                "url": report.url,
                "status": report.status,
                "text": report.text,
                "elements": report.elements.unwrap_or_default().iter().map(|e| json!({
                    "role": e.role,
                    "name": e.name,
                    "value": e.value,
                    "enabled": e.enabled,
                    "checked": e.checked,
                    "visible": e.visible,
                })).collect::<Vec<_>>(),
                "pending_requests": report.pending_requests,
            }))
        }
        "rows" => {
            let table = crate::steps::data::table(cx, plan)?;
            let filter =
                sc_api::filter::api_where(&table, plan.get("where").filter(|w| !w.is_null()))?;
            let limit = plan
                .get("limit")
                .and_then(Json::as_u64)
                .unwrap_or(MAX_ROWS)
                .min(MAX_ROWS);
            Ok(Json::Array(
                crate::steps::data::read(cx, &table, filter, limit).await?,
            ))
        }
        other => Err(Error::invalid(format!(
            "`{other}` is not something a checklist step can ask for; it asks to run a core \
             step (`step`), to inspect the page (`inspect`) or to read rows (`rows`)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl() -> StepDecl {
        StepDecl {
            module: "shop".to_owned(),
            name: "shop.ok".to_owned(),
            description: String::new(),
            config_spec: Vec::new(),
            needs: Needs::NONE,
            mutates: false,
            framework: None,
            min_timeout: Duration::ZERO,
        }
    }

    #[test]
    fn a_modules_answer_becomes_an_outcome() {
        let pass = outcome_of(
            &json!({ "outcome": "pass", "message": "fine", "value": 3 }),
            &decl(),
        );
        assert_eq!(pass.outcome, Outcome::Pass);
        assert_eq!(pass.message, "fine");
        assert_eq!(pass.value, Some(json!(3)));
        assert_eq!(outcome_of(&json!(false), &decl()).outcome, Outcome::Fail);
        let odd = outcome_of(&json!({ "outcome": "great" }), &decl());
        assert_eq!(odd.outcome, Outcome::Error);
        assert!(odd.message.contains("`great`"), "{}", odd.message);
        assert_eq!(outcome_of(&json!(7), &decl()).outcome, Outcome::Error);
    }

    #[test]
    fn needs_are_read_by_name() {
        let needs = parse_needs(&["http".to_owned(), "data".to_owned()]).unwrap();
        assert!(needs.http && needs.data && !needs.browser);
        let msg = parse_needs(&["gpu".to_owned()]).unwrap_err().to_string();
        assert!(msg.contains("`gpu`"), "{msg}");
    }
}
