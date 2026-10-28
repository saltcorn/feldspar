//! The step-type registry (docs/CHECKLISTS.md §2, TODO C1.2).
//!
//! A step type is a name, a parameter spec, the capabilities it needs and what
//! it does. The core supplies `core.*`, `data.*`, `http.*` and `app.*`;
//! frameworks and modules add theirs in later milestones, which is why the
//! registry is a value and not a `match`.
//!
//! **A step sees only what it declared.** [`StepContext`] hands a step the
//! application router only if its type [`needs`](StepType::needs) HTTP, and so
//! on — so a module step type (C3) reaches exactly the capabilities its
//! manifest names, and a mistake in a type's declaration is a loud error in the
//! first run rather than a quiet extra privilege.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sc_action::TriggerDispatcher;
use sc_agent::AppRequester;
use sc_app::Application;
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_types::FormField;
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::model::{Outcome, SkipReason};

/// The capabilities a step type needs from the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    /// The catalog and the row layer, as `run_as`. Every host has it.
    pub data: bool,
    /// The application router (`AppRequester`), to send requests to the
    /// checklist's application.
    pub http: bool,
    /// A headless browser signed in as `run_as` (C2).
    pub browser: bool,
    /// The application's build.
    pub build: bool,
}

impl Needs {
    /// Nothing at all.
    pub const NONE: Needs = Needs {
        data: false,
        http: false,
        browser: false,
        build: false,
    };
    /// The row layer.
    pub const DATA: Needs = Needs {
        data: true,
        ..Needs::NONE
    };
    /// The application router.
    pub const HTTP: Needs = Needs {
        http: true,
        ..Needs::NONE
    };
    /// The browser.
    pub const BROWSER: Needs = Needs {
        browser: true,
        ..Needs::NONE
    };
    /// The build.
    pub const BUILD: Needs = Needs {
        build: true,
        ..Needs::NONE
    };

    /// Whether a step needing this has to belong to a checklist with an
    /// application: anything that reaches the application rather than the
    /// data.
    pub fn application(&self) -> bool {
        self.http || self.browser || self.build
    }

    /// The names, for `describe_step_types` and the admin UI.
    pub fn names(&self) -> Vec<&'static str> {
        [
            ("data", self.data),
            ("http", self.http),
            ("browser", self.browser),
            ("build", self.build),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect()
    }
}

/// What one step reports (TODO C1.2): how it went, in a sentence, and the
/// value a following `as` captures.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// Pass, fail, error or skipped.
    pub outcome: Outcome,
    /// Why it was skipped, when it was.
    pub skip: Option<SkipReason>,
    /// What happened.
    pub message: String,
    /// The value the step produced — the inserted row, the response — which
    /// the step's `as` captures into the run's variables.
    pub value: Option<Json>,
}

impl StepOutcome {
    /// Every assertion held.
    pub fn pass(message: impl Into<String>) -> StepOutcome {
        StepOutcome::new(Outcome::Pass, message)
    }

    /// An assertion did not hold.
    pub fn fail(message: impl Into<String>) -> StepOutcome {
        StepOutcome::new(Outcome::Fail, message)
    }

    /// The step could not be carried out.
    pub fn error(message: impl Into<String>) -> StepOutcome {
        StepOutcome::new(Outcome::Error, message)
    }

    /// Not run, for `reason`.
    pub fn skipped(reason: SkipReason, message: impl Into<String>) -> StepOutcome {
        StepOutcome {
            skip: Some(reason),
            ..StepOutcome::new(Outcome::Skipped, message)
        }
    }

    /// With the value it produced.
    pub fn with_value(mut self, value: Json) -> StepOutcome {
        self.value = Some(value);
        self
    }

    fn new(outcome: Outcome, message: impl Into<String>) -> StepOutcome {
        StepOutcome {
            outcome,
            skip: None,
            message: message.into(),
            value: None,
        }
    }
}

/// A step's error as an outcome: what `?` inside [`StepType::run`] should
/// become. An [`Error::auth`] from the row layer is not an error of the step
/// but a denial, which the step decides what to do with before this.
impl From<Error> for StepOutcome {
    fn from(e: Error) -> StepOutcome {
        StepOutcome::error(sc_error::format_chain(&e))
    }
}

/// A row a `data.insert` wrote, to delete when the run ends.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertedRow {
    /// The table.
    pub table: String,
    /// The row's primary key, as the row layer addresses it.
    pub key: String,
}

/// What the host can offer a run. Which of it a step gets is the step type's
/// [`needs`](StepType::needs).
#[derive(Clone, Copy, Default)]
pub struct Capabilities<'a> {
    /// The trigger dispatcher, for `data.run_trigger`.
    pub triggers: Option<&'a Arc<TriggerDispatcher>>,
    /// The formula evaluator, for a table whose ownership formula does not
    /// translate to SQL.
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// The application router, for `http.*`.
    pub requests: Option<&'a dyn AppRequester>,
}

impl Capabilities<'_> {
    /// Why the host cannot give a step `needs`, if it cannot.
    pub fn missing(&self, needs: Needs) -> Option<String> {
        if needs.browser {
            return Some("this host has no headless browser for checklists yet".to_owned());
        }
        if needs.http && self.requests.is_none() {
            return Some(
                "this host cannot send requests to applications (it serves none: no base \
                 domain, or not a server)"
                    .to_owned(),
            );
        }
        None
    }
}

/// What a step runs with (TODO C1.2): who it runs as, the run's variables, and
/// the capabilities its type declared — nothing else.
pub struct StepContext<'a> {
    /// The catalog.
    pub catalog: &'a Catalog,
    /// The user the checklist runs as, or `None` for an anonymous visitor.
    pub run_as: Option<&'a User>,
    /// The run's id — `{{ run.id }}`.
    pub run_id: Uuid,
    /// The application the checklist checks, if it checks one.
    pub application: Option<&'a Application>,
    /// The run's variables: `run`, and every earlier step's `as`.
    pub vars: &'a Json,
    /// How long the step may take.
    pub timeout: Duration,
    pub(crate) needs: Needs,
    pub(crate) caps: Capabilities<'a>,
    pub(crate) inserted: &'a mut Vec<InsertedRow>,
    pub(crate) last_response: &'a mut Option<Json>,
    pub(crate) step_type: &'a str,
}

impl<'a> StepContext<'a> {
    /// The role `run_as` acts at: theirs, or public.
    pub fn role(&self) -> u8 {
        self.run_as.map_or(sc_auth::ROLE_PUBLIC, |u| u.role)
    }

    /// The formula evaluator, where the type needs data and the host has one.
    pub fn evaluator(&self) -> Result<Option<&'a Arc<dyn JsEvaluator>>> {
        self.require(self.needs.data, "data")?;
        Ok(self.caps.evaluator)
    }

    /// The trigger dispatcher.
    pub fn triggers(&self) -> Result<&'a Arc<TriggerDispatcher>> {
        self.require(self.needs.data, "data")?;
        self.caps.triggers.ok_or_else(|| {
            Error::config("this host has no trigger dispatcher, so no trigger can be run")
        })
    }

    /// The application router.
    pub fn requests(&self) -> Result<&'a dyn AppRequester> {
        self.require(self.needs.http, "http")?;
        self.caps
            .requests
            .ok_or_else(|| Error::config("this host cannot send requests to applications"))
    }

    /// The application, which every step that needs HTTP, a browser or a
    /// build has (the save refuses one on a checklist without).
    pub fn require_application(&self) -> Result<&'a Application> {
        self.application.ok_or_else(|| {
            Error::invalid(format!(
                "`{}` checks an application, and this checklist has none",
                self.step_type
            ))
        })
    }

    /// Remember a row this step wrote, to delete when the run ends.
    pub fn inserted(&mut self, row: InsertedRow) {
        self.inserted.push(row);
    }

    /// The last response an `http.*` step received in this run.
    pub fn last_response(&self) -> Option<&Json> {
        self.last_response.as_ref()
    }

    /// Record the response an `http.*` step received.
    pub fn set_last_response(&mut self, response: Json) {
        *self.last_response = Some(response);
    }

    fn require(&self, declared: bool, what: &str) -> Result<()> {
        if declared {
            return Ok(());
        }
        Err(Error::config(format!(
            "step type `{}` uses the `{what}` capability without declaring it in `needs`",
            self.step_type
        )))
    }
}

/// One kind of step (docs/CHECKLISTS.md §2).
#[async_trait::async_trait]
pub trait StepType: Send + Sync {
    /// The namespaced name: `data.insert`.
    fn name(&self) -> &str;

    /// What it does, for a person choosing one and a model learning the
    /// vocabulary (`describe_step_types`).
    fn description(&self) -> &str;

    /// Its parameters. The admin UI renders the form from this, and a save
    /// validates against it.
    fn config_spec(&self) -> Vec<FormField>;

    /// What it needs from the host.
    fn needs(&self) -> Needs;

    /// Whether it writes data. Feeds `Checklist::mutates`.
    fn mutates(&self) -> bool;

    /// The least time a step of this type is given, whatever the checklist's
    /// `timeout` says — a build that installs dependencies takes minutes.
    fn min_timeout(&self) -> Duration {
        Duration::ZERO
    }

    /// Run it, with its string parameters already rendered over the run's
    /// variables.
    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome;
}

/// The step types this process knows, by name.
#[derive(Clone, Default)]
pub struct StepRegistry {
    types: BTreeMap<String, Arc<dyn StepType>>,
}

impl std::fmt::Debug for StepRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.types.keys()).finish()
    }
}

impl StepRegistry {
    /// An empty registry.
    pub fn new() -> StepRegistry {
        StepRegistry::default()
    }

    /// The core's step types: `core.*`, `data.*`, `http.*` and `app.*`.
    pub fn builtin() -> StepRegistry {
        let mut registry = StepRegistry::new();
        for step in crate::steps::builtin() {
            // The built-in names are distinct and namespaced by construction;
            // a clash here is a bug in this crate, caught by its own tests.
            if let Err(e) = registry.register(step) {
                sc_log::log_warn!("checklists: {e}");
            }
        }
        registry
    }

    /// Add a step type. A name that is not `namespace.name`, or one already
    /// registered, is an error.
    pub fn register(&mut self, step: Arc<dyn StepType>) -> Result<()> {
        let name = step.name().to_owned();
        let valid = name.split_once('.').is_some_and(|(ns, rest)| {
            let part = |p: &str| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            };
            part(ns) && part(rest)
        });
        if !valid {
            return Err(Error::invalid(format!(
                "step type `{name}` should be named `namespace.name` in lower case, \
                 e.g. `data.insert`"
            )));
        }
        if self.types.contains_key(&name) {
            return Err(Error::invalid(format!(
                "step type `{name}` is registered twice"
            )));
        }
        self.types.insert(name, step);
        Ok(())
    }

    /// The type called `name`, if registered.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn StepType>> {
        self.types.get(name)
    }

    /// The type called `name`, or an error listing the namespaces there are.
    pub fn require(&self, name: &str) -> Result<&Arc<dyn StepType>> {
        self.get(name).ok_or_else(|| {
            let mut namespaces: Vec<&str> = self
                .types
                .keys()
                .filter_map(|n| n.split_once('.').map(|(ns, _)| ns))
                .collect();
            namespaces.dedup();
            Error::invalid(format!(
                "there is no step type `{name}`; the types are in {} (describe_step_types \
                 lists them)",
                namespaces.join(", ")
            ))
        })
    }

    /// Every type, by name.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn StepType>> {
        self.types.values()
    }

    /// Every type a checklist may use: with an application, all of them;
    /// without one, only those that need none.
    pub fn offered(&self, with_application: bool) -> Vec<&Arc<dyn StepType>> {
        self.all()
            .filter(|t| with_application || !t.needs().application())
            .collect()
    }

    /// One type described, for the admin UI and `describe_step_types`.
    pub fn describe(step: &dyn StepType) -> Json {
        json!({
            "name": step.name(),
            "description": step.description(),
            "needs": step.needs().names(),
            "mutates": step.mutates(),
            "params": step.config_spec().iter().map(spec_json).collect::<Vec<_>>(),
        })
    }
}

/// One parameter of a step type, as a client renders it.
pub fn spec_json(field: &FormField) -> Json {
    json!({
        "name": field.base.name,
        "label": field.base.label,
        "type": field.base.type_.as_basic().map(|b| b.name()).unwrap_or("Json"),
        "required": field.required,
        "default": field.default,
        "options": field.static_options(),
        "multiline": field.multiline,
        "sublabel": field.sublabel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl StepType for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn config_spec(&self) -> Vec<FormField> {
            Vec::new()
        }
        fn needs(&self) -> Needs {
            Needs::NONE
        }
        fn mutates(&self) -> bool {
            false
        }
        async fn run(&self, _cx: &mut StepContext<'_>, _params: &Json) -> StepOutcome {
            StepOutcome::pass("")
        }
    }

    #[test]
    fn names_are_namespaced_and_registered_once() {
        let mut registry = StepRegistry::new();
        registry.register(Arc::new(Named("my.step"))).unwrap();
        let msg = registry
            .register(Arc::new(Named("my.step")))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("registered twice"), "{msg}");
        for bad in ["step", "My.step", ".step", "my.", "my step.x"] {
            assert!(registry.register(Arc::new(Named(bad))).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_builtin_types_are_all_there_and_distinct() {
        let registry = StepRegistry::builtin();
        for name in [
            "core.wait",
            "core.manual",
            "data.insert",
            "data.update",
            "data.delete",
            "data.expect_rows",
            "data.expect_denied",
            "data.run_trigger",
            "data.expect_calculated",
            "http.request",
            "http.expect_status",
            "http.expect_json",
            "app.builds",
        ] {
            assert!(registry.get(name).is_some(), "{name}");
        }
        assert_eq!(registry.all().count(), 13);
        // Without an application, only the steps that need none are offered.
        let names: Vec<&str> = registry.offered(false).iter().map(|t| t.name()).collect();
        assert!(names.contains(&"data.insert"));
        assert!(!names.contains(&"http.request"));
        assert!(!names.contains(&"app.builds"));
    }
}
