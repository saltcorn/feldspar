//! Scheduled runs (TODO-checklists C4.4): the `run_checklists` action, which a
//! trigger runs like any other.
//!
//! There is no scheduler of its own: a trigger on the `often`, `hourly`,
//! `daily` or `weekly` event with this action is a scheduled run, and the
//! trigger machinery supplies the rest — the schedule and its catch-up after a
//! restart, the trigger's enabled switch, and its error event. A run that does
//! not pass makes the action fail, naming the checklists, so an error trigger
//! hears of it; each run is recorded either way, triggered `schedule`.
//!
//! The runs get this server's capabilities as the admin's Run button does: the
//! dispatcher that fired the action, and the application router and the
//! headless browser through the agents' [`ViewServices`], read when the action
//! runs (they are installed after the action set is built).

use std::sync::Arc;

use sc_action::{Action, ActionContext};
use sc_agent::ViewServices;
use sc_error::{Error, Result};
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::model::RunStatus;
use crate::registry::StepRegistry;
use crate::runner::{CheckEnv, run_checklist};
use crate::store::{self, ChecklistScope};

/// What a scheduled run is triggered as.
pub const TRIGGER_SCHEDULE: &str = "schedule";

/// The action's name.
pub const RUN_CHECKLISTS_ACTION: &str = "run_checklists";

const CFG_APPLICATION: &str = "application";
const CFG_CHECKLIST: &str = "checklist";

/// `run_checklists`: run the enabled checklists of an application, or the ones
/// with no application, or one by name.
pub struct RunChecklists {
    view: Arc<ViewServices>,
}

impl RunChecklists {
    /// The action, reaching the router and the browser through `view`.
    pub fn new(view: Arc<ViewServices>) -> RunChecklists {
        RunChecklists { view }
    }
}

fn config_text(config: &sc_types::Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[async_trait::async_trait]
impl Action for RunChecklists {
    fn name(&self) -> &str {
        RUN_CHECKLISTS_ACTION
    }

    fn description(&self) -> &str {
        "Run checklists: every enabled checklist of an application, or the ones with no \
         application, or one by name. Put it on a scheduled trigger to check on a schedule. \
         Each run is recorded on its checklist; the action fails, naming them, when any \
         checklist does not pass."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_APPLICATION, BasicType::Text)
                .label("Application")
                .sublabel(
                    "The subdomain of the application whose checklists to run; blank for the \
                     checklists with no application",
                ),
            FormField::new(CFG_CHECKLIST, BasicType::Text)
                .label("Checklist")
                .sublabel("Only the checklist with this name; blank for all of them"),
        ]
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        // A checklist's own trigger starting checklists would be a run inside
        // a run, whose outbox and clean-up nobody could tell apart.
        if sc_catalog::outbox::current().is_some() {
            return Err(Error::invalid(format!(
                "trigger `{}`: checklists cannot be run on behalf of a checklist run",
                ctx.trigger
            )));
        }
        let scope = match config_text(ctx.config, CFG_APPLICATION) {
            Some(subdomain) => {
                let app = sc_app::load_application_by_subdomain(ctx.catalog, &subdomain)
                    .await?
                    .ok_or_else(|| {
                        Error::not_found(format!(
                            "trigger `{}`: no application with subdomain `{subdomain}`",
                            ctx.trigger
                        ))
                    })?;
                ChecklistScope::Application(app.id.0)
            }
            None => ChecklistScope::DataOnly,
        };
        let mut checklists = store::list_checklists(ctx.catalog, scope).await?;
        checklists.retain(|c| c.enabled);
        if let Some(name) = config_text(ctx.config, CFG_CHECKLIST) {
            checklists.retain(|c| c.name == name);
            if checklists.is_empty() {
                return Err(Error::not_found(format!(
                    "trigger `{}`: no enabled checklist called `{name}`",
                    ctx.trigger
                )));
            }
        }

        let registry = StepRegistry::installed();
        let triggers = ctx.triggers().and_then(|t| t.shared());
        let env = CheckEnv::new(ctx.catalog, &registry)
            .with_triggers(triggers.as_ref())
            .with_requests(self.view.requests().map(|r| r.as_ref() as &dyn sc_agent::AppRequester))
            .with_browser(self.view.browser().map(|b| b.as_ref() as &dyn sc_agent::BrowserDriver));
        let mut runs = Vec::new();
        for checklist in &checklists {
            runs.push(run_checklist(&env, checklist, TRIGGER_SCHEDULE).await?);
        }

        let not_passed: Vec<String> = runs
            .iter()
            .filter(|r| r.status != RunStatus::Passed)
            .map(|r| format!("{}: {}", r.checklist_name, r.status.as_str()))
            .collect();
        if !not_passed.is_empty() {
            return Err(Error::invalid(format!(
                "trigger `{}`: {} of {} checklist{} did not pass ({})",
                ctx.trigger,
                not_passed.len(),
                runs.len(),
                if runs.len() == 1 { "" } else { "s" },
                not_passed.join(", ")
            )));
        }
        Ok(json!({
            "runs": runs
                .iter()
                .map(|r| json!({
                    "id": r.id.to_string(),
                    "checklist_name": r.checklist_name,
                    "status": r.status.as_str(),
                }))
                .collect::<Vec<_>>(),
        }))
    }
}
