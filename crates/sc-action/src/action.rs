//! The [`Action`] trait and the [`ActionContext`] it runs in (design §10.1).
//!
//! An action is **one elementary step**: it is handed an event, its own
//! configuration, and a catalog, and it returns a JSON result. Control flow —
//! branching, looping, retrying — is deliberately not here; it belongs to the
//! workflow engine (§10.3), which is what keeps the built-in action set as small
//! as GOALS asks for.
//!
//! The shape is chosen so that a workflow step reuses it unchanged: the
//! `context` an action reads and writes *is* the run context a workflow
//! accumulates, and the returned value is what a step contributes to it. A
//! trigger firing once starts with an empty context and drops it afterwards; a
//! step is handed the run's ([`ActionContext::with_run_context`]), which is also
//! what puts `context` in scope for the action's own settings — decision 8, and
//! the one thing an action has to know about the difference.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_catalog::{Catalog, SharedTx};
use sc_email::Mailer;
use sc_error::{Error, Result};
use sc_expr::{
    Ambient, CodeAdapter, ConsoleSink, Formula, JsEvaluator, SchemaShape, Template, value_from_json,
};
use sc_query::Value;
use sc_types::{Attrs, FormField};
use serde_json::Value as Json;

use crate::dispatch::TriggerDispatcher;
use crate::event::Event;
use crate::scope::{EventBindings, action_shape, step_shape};

/// One elementary step: configurable, run against an event.
///
/// Object-safe and dynamically dispatched, because which action runs is decided
/// at runtime from stored configuration and may — once `sc-code` lands — be
/// implemented in a guest language behind a single Rust shim (§2.1).
#[async_trait::async_trait]
pub trait Action: Send + Sync {
    /// The name the action is registered and stored under (`insert_row`, …).
    /// Stable: it is what a saved trigger references.
    fn name(&self) -> &str;

    /// One line for the admin UI's action picker.
    fn description(&self) -> &str;

    /// The configuration this action takes, as form fields — the same
    /// "settings as data" move that lets the admin UI render a form for a file
    /// store backend or a framework it knows nothing about (§6.2). It is also
    /// what a trigger's configuration is validated against on save (Phase 2).
    fn config_spec(&self) -> Vec<FormField>;

    /// The configuration this action takes **for a trigger on `channel`**.
    ///
    /// The default is [`config_spec`](Action::config_spec), and for almost every
    /// action that is the whole story: what an HTTP request needs does not depend
    /// on which table fired it.
    ///
    /// It is a separate method because for *some* actions it does. `send_email`
    /// offers one checkbox per **File field of the table**, because "attach the
    /// invoice this row points at" cannot be spelled without knowing that
    /// `invoice` is a file — and the alternative, a free-text list of field
    /// names, would push the checking to save time and the guessing to the
    /// admin. The declaration stays data either way: the admin UI still renders
    /// whatever it is handed and knows nothing about attachments.
    ///
    /// Whatever this returns is what the configuration is **validated against**
    /// ([`validate_trigger`](crate::validate_trigger)), so a setting that is not
    /// in it for this channel is an unknown setting — which is the point: ticking
    /// `attach_invoice` on a table with no `invoice` file field is a mistake, not
    /// a value quietly carried along.
    fn config_spec_for(&self, catalog: &Catalog, channel: Option<&str>) -> Vec<FormField> {
        let _ = (catalog, channel);
        self.config_spec()
    }

    /// Check a configuration beyond what [`config_spec`](Action::config_spec) can
    /// express — the part only this action knows: that a table it names exists and
    /// can be addressed by primary key, that a setting holding a **formula**
    /// parses and resolves in the scope the event will give it.
    ///
    /// Runs where the generic check runs: on save, in front of the admin, *and*
    /// again on load, so a trigger whose world changed underneath it (a dropped
    /// table, a renamed field) leaves the live set with a reason rather than
    /// failing when it fires. The default is `Ok(())` — an action whose
    /// configuration is fully described by its spec has nothing more to say.
    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let _ = check;
        Ok(())
    }

    /// Run against `ctx`, returning the action's result.
    ///
    /// The result is the response body of a directly-run trigger, and will be a
    /// workflow step's contribution to the run context. An action with nothing to
    /// report returns [`Json::Null`].
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json>;
}

/// What an action's own configuration check ([`Action::validate_config`]) gets:
/// the catalog to resolve names against, the configuration under scrutiny, and
/// the event's table.
///
/// The channel is here because an action's formulas are read in the scope the
/// *event* provides (decision 7): `row`/`old` are in scope exactly when the
/// trigger listens to a table, so the same configuration can be valid on an
/// `insert` trigger and invalid on a `login` one, and the message has to say so
/// while the admin is still looking at the form.
pub struct ConfigCheck<'a> {
    /// The live catalog: what tables and fields exist.
    pub catalog: &'a Catalog,
    /// The configuration being validated, keyed by
    /// [`config_spec`](Action::config_spec) field names.
    pub config: &'a Attrs,
    /// The table the trigger's event fires on, or `None` for an event with no
    /// row (validation has already refused the mismatched combinations).
    pub channel: Option<&'a str>,
    /// The scope this configuration's formulas and templates are read in —
    /// [`action_shape`](crate::action_shape) for a trigger's own action body,
    /// [`step_shape`](crate::step_shape) for a **workflow step**, which is the
    /// same thing plus the ambient `context` (§10.3, decision 8).
    ///
    /// It is handed to the action rather than rebuilt by it because only the
    /// caller knows which of the two this is, and because an action that built
    /// its own would be free to build a different one from the scope its
    /// formulas are then *evaluated* in — which is exactly the drift
    /// [`ActionContext::shape`] exists to prevent on the other side.
    pub shape: &'a SchemaShape,
}

impl ConfigCheck<'_> {
    /// Check one configured formula in `scope`: [`check_formula`]'s syntax
    /// and scope rules, then every `predict("…")` it makes against the models
    /// (milestone 31 §4) — the model exists, is a model of `scope`'s table,
    /// and answers something per row.
    ///
    /// The one call an action's `validate_config` makes per formula, so a
    /// prediction in an `update_rows` assignment is refused on save with the
    /// same sentence as one in an `only if`.
    ///
    /// [`check_formula`]: crate::check_formula
    pub async fn formula(&self, scope: &str, formula: &Formula, what: &str) -> Result<()> {
        let analysis = crate::scope::check_formula(self.shape, scope, formula, what)?;
        sc_catalog::check_model_calls(self.catalog, scope, &analysis)
            .await
            .map_err(|e| Error::invalid(format!("{what}: {e}")))?;
        Ok(())
    }

    /// [`formula`](ConfigCheck::formula), for a template: every token.
    pub async fn template(&self, scope: &str, template: &Template, what: &str) -> Result<()> {
        for analysis in crate::scope::check_template(self.shape, scope, template, what)? {
            sc_catalog::check_model_calls(self.catalog, scope, &analysis)
                .await
                .map_err(|e| Error::invalid(format!("{what}: {e}")))?;
        }
        Ok(())
    }
}

/// Everything one action run has access to.
///
/// Borrowed rather than owned (hence the lifetime): a run is a single `await` on
/// the caller's stack, and copying a catalog handle and a configuration map per
/// firing would be waste for no gain in clarity.
pub struct ActionContext<'a> {
    /// The data layer. An action that writes rows goes through the row layer
    /// (`sc-api`, above this crate) rather than the driver, so its writes are
    /// themselves events — which is what makes a cascade possible, and bounded.
    pub catalog: &'a Catalog,
    /// What happened.
    pub event: &'a Event,
    /// This trigger's configuration for this action, keyed by the
    /// [`config_spec`](Action::config_spec) field names.
    pub config: &'a Attrs,
    /// The name of the trigger being run, for error messages: an action failure
    /// the admin cannot attribute to a trigger is a failure they cannot fix.
    pub trigger: &'a str,
    /// The chain of trigger names that led here, *including* this one — what
    /// [`Event::firing`](crate::Event::firing) returned. Any event this action's
    /// writes raise carries it, so the next level down knows how deep it is.
    pub chain: Vec<String>,
    /// The JavaScript engine, when the caller has one. Absent in contexts that
    /// have no engine (client generation, unit tests); an action that needs it
    /// asks through [`evaluator`](ActionContext::evaluator) and gets a named
    /// configuration error rather than silently doing nothing.
    evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// The mail transport, when the caller has one — reached exactly as the
    /// evaluator is, and absent in exactly the same contexts (client generation,
    /// a unit test), so an action that sends mail out of context gets a named
    /// configuration error rather than doing nothing.
    mailer: Option<&'a Arc<dyn Mailer>>,
    /// The **other guest languages** this process can run a code body in, keyed
    /// by language — reached as [`adapter`](ActionContext::adapter), and empty
    /// in a context that installed none.
    ///
    /// Borrowed from [`ActionServices`](crate::ActionServices) rather than
    /// cloned, for the reason every other handle here is borrowed: a run is one
    /// `await` on the caller's stack, and an adapter is a process-wide thing the
    /// dispatcher already holds.
    adapters: Option<&'a BTreeMap<String, Arc<dyn CodeAdapter>>>,
    /// The dispatcher this action was fired by, for an action that can run
    /// **another trigger** — `run_js_code`'s `trigger(…)`, and a workflow's
    /// steps once §10.3 lands.
    ///
    /// *The* dispatcher rather than a handle of its own: what a body runs must
    /// be the trigger the admin configured, with its `only_if`, its floor and
    /// its cascade bound, so this is one more thing that can ask and not a
    /// second way to fire. Borrowed, because the dispatcher is what is running
    /// this action — the borrow is its own stack frame, one level up.
    ///
    /// `None` where a context has none (client generation, a unit test), and an
    /// action that needs it says so by name rather than doing nothing.
    triggers: Option<&'a TriggerDispatcher>,
    /// The transaction this action's writes belong in, when it has one.
    ///
    /// A **workflow step** has one (§10.3, decision 6): everything the step does
    /// to the database — its own writes, and the writes of any trigger those
    /// writes cascade into — commits with the run's advance or is rolled back
    /// with it. An ordinary trigger's action has none, and writes as it always
    /// did, one statement at a time.
    ///
    /// Cloned rather than borrowed ([`SharedTx`] is a handle on one transaction,
    /// not the transaction): an action's write happens several frames below this
    /// one, through a row layer that cannot be handed a unique borrow, and a
    /// handle that can be cloned into those frames is what makes "one step, one
    /// transaction" implementable at all.
    tx: Option<SharedTx>,
    /// The run context: a JSON object the action may read and write — what the
    /// steps before this one left behind, and what this one contributes to.
    pub context: Attrs,
    /// Where this run's code body sends what it prints, when somebody is
    /// collecting it — the admin's **Test run**, which shows the transcript
    /// beside the result (or beside the failure, which is the case it is for).
    ///
    /// `None` for every ordinary firing: `console.log` still works there and
    /// goes to the server's log. Only `run_js_code` reads it, and it hands it
    /// straight to the engine — nothing between here and the isolate has to
    /// know the admin is watching.
    console: Option<ConsoleSink>,
    /// Whether this action is a **workflow step** rather than a trigger's own
    /// body, which is what puts [`context`](ActionContext::context) in scope for
    /// its settings.
    ///
    /// A flag rather than "the context is not empty", because presence is scope
    /// (the rule `row` already follows): the first step of a run has an empty
    /// context and must still be able to name it, and an ordinary trigger's
    /// formula naming `context` must be the unknown identifier it is rather than
    /// a null that reads as "nothing has happened yet".
    in_run: bool,
    /// The request a **custom query**'s code body answers (§13.4): `body` and
    /// `query`, bound in the body's scope in place of the event's `payload`.
    ///
    /// `None` for every trigger. Only the code-body actions read it.
    request: Option<Attrs>,
}

impl<'a> ActionContext<'a> {
    /// A context for running `trigger`'s action against `event` with `config`.
    ///
    /// The chain starts as just this trigger — the caller replaces it with
    /// [`Event::firing`](crate::Event::firing)'s result when the event descends
    /// from another trigger.
    pub fn new(
        catalog: &'a Catalog,
        event: &'a Event,
        config: &'a Attrs,
        trigger: &'a str,
    ) -> ActionContext<'a> {
        ActionContext {
            catalog,
            event,
            config,
            trigger,
            chain: vec![trigger.to_owned()],
            evaluator: None,
            mailer: None,
            adapters: None,
            triggers: None,
            tx: None,
            console: None,
            context: Attrs::new(),
            in_run: false,
            request: None,
        }
    }

    /// Supply the JavaScript engine (the server's one isolate, §7.3).
    pub fn with_evaluator(mut self, evaluator: &'a Arc<dyn JsEvaluator>) -> ActionContext<'a> {
        self.evaluator = Some(evaluator);
        self
    }

    /// Supply the mail transport (§18.2) — the server's
    /// [`SettingsMailer`](sc_email::SettingsMailer), or a recording one in a test.
    pub fn with_mailer(mut self, mailer: &'a Arc<dyn Mailer>) -> ActionContext<'a> {
        self.mailer = Some(mailer);
        self
    }

    /// Supply the adapters for the other languages a code body may be written
    /// in — what a dispatcher hands every action it runs.
    pub fn with_adapters(
        mut self,
        adapters: &'a BTreeMap<String, Arc<dyn CodeAdapter>>,
    ) -> ActionContext<'a> {
        self.adapters = Some(adapters);
        self
    }

    /// Collect what this action's code body prints — the admin's **Test run**.
    ///
    /// The sink is filled as the lines happen, so the caller reads it whether
    /// the action answered or failed. It reaches this action's own body and no
    /// further: a trigger this one runs is a firing of its own, logged where
    /// every other firing is, and folding its output into this transcript would
    /// have an admin reading another trigger's lines as their own.
    pub fn with_console(mut self, console: ConsoleSink) -> ActionContext<'a> {
        self.console = Some(console);
        self
    }

    /// Where this action's code body should send what it prints, if anywhere.
    pub fn console(&self) -> Option<&ConsoleSink> {
        self.console.as_ref()
    }

    /// Supply the dispatcher, so this action can run another trigger.
    pub fn with_triggers(mut self, triggers: &'a TriggerDispatcher) -> ActionContext<'a> {
        self.triggers = Some(triggers);
        self
    }

    /// Run this action **inside `tx`**: every row it writes joins that
    /// transaction, and so does every write of every trigger it cascades into.
    ///
    /// What the workflow driver hands a step (§10.3, decision 6). An action that
    /// writes rows does not have to know: it asks
    /// [`transaction`](ActionContext::transaction) and hands what it gets to the
    /// row layer, which is one line and the same line in each of them.
    pub fn with_transaction(mut self, tx: SharedTx) -> ActionContext<'a> {
        self.tx = Some(tx);
        self
    }

    /// The transaction this action's writes belong in, if any — a **handle** on
    /// it, so an action may keep it for as long as it is writing.
    pub fn transaction(&self) -> Option<SharedTx> {
        self.tx.clone()
    }

    /// Run this action as a **workflow step**, with the run's context in hand.
    ///
    /// The only way `in_run` is set, so the two halves of decision 8 arrive
    /// together: what the action reads and writes ([`context`]) and the fact that
    /// its settings may *name* it ([`shape`], [`bindings`]).
    ///
    /// [`context`]: ActionContext::context
    /// [`shape`]: ActionContext::shape
    /// [`bindings`]: ActionContext::bindings
    pub fn with_run_context(mut self, context: Attrs) -> ActionContext<'a> {
        self.context = context;
        self.in_run = true;
        self
    }

    /// The workflow run's context, or `None` when this is an ordinary trigger's
    /// action body.
    pub fn run_context(&self) -> Option<&Attrs> {
        self.in_run.then_some(&self.context)
    }

    /// The scope this action's configured formulas and templates are read in:
    /// [`action_shape`](crate::action_shape), plus the ambient `context` when
    /// this is a workflow step ([`step_shape`](crate::step_shape)).
    ///
    /// The evaluation-time twin of [`ConfigCheck::shape`], and the reason both
    /// exist as one call each: a setting accepted on save and then unbound when
    /// the step runs would be the worst of the two, so neither side gets to
    /// decide the scope for itself.
    pub fn shape(&self) -> Result<SchemaShape> {
        // The **table** channel: a stream event's channel names a stream, and
        // a scope built by looking it up as a table would fail naming a table
        // nobody mentioned. A stream trigger's action reads the envelope
        // through `payload` in `EVENT_SCOPE`, which is what is left.
        let channel = self.event.table_channel();
        match self.in_run {
            true => step_shape(self.catalog, channel),
            false => action_shape(self.catalog, channel),
        }
    }

    /// What the event — and, in a run, the context — binds in that scope.
    ///
    /// Every action that evaluates a setting builds its bindings through this or
    /// through [`bind_values`](ActionContext::bind_values), so `context` is in
    /// scope for exactly the runs [`shape`](ActionContext::shape) says it is.
    pub fn bindings(&self) -> EventBindings {
        self.bind_values(|_, _, json| value_from_json(json))
    }

    /// [`bindings`](ActionContext::bindings) with a caller-supplied reading of
    /// each field — how a row action types the event's values against real
    /// columns so an inlined `user.id` can be compared to a `uuid` column in SQL.
    pub fn bind_values(&self, value: impl Fn(Ambient, &str, &Json) -> Value) -> EventBindings {
        let bindings = EventBindings::with_values(self.event, value);
        match self.run_context() {
            Some(context) => bindings.with_context(context),
            None => bindings,
        }
    }

    /// Run this action as a **custom query**'s body, with the request's names
    /// in scope ([`TriggerDispatcher::run_code`]).
    pub fn with_request(mut self, request: Attrs) -> ActionContext<'a> {
        self.request = Some(request);
        self
    }

    /// The request a custom query's body answers, or `None` for a trigger.
    pub fn request(&self) -> Option<&Attrs> {
        self.request.as_ref()
    }

    /// Supply the chain this run descends from (`Event::firing`'s result).
    pub fn with_chain(mut self, chain: Vec<String>) -> ActionContext<'a> {
        self.chain = chain;
        self
    }

    /// The JavaScript engine, or a configuration error naming the trigger.
    ///
    /// Fails rather than skipping: an action whose configuration is formulas and
    /// whose engine is missing has nothing correct to do, and quietly doing
    /// nothing is the silent failure principle 5 forbids.
    pub fn evaluator(&self) -> Result<&Arc<dyn JsEvaluator>> {
        self.evaluator.ok_or_else(|| {
            Error::config(format!(
                "trigger `{}` needs to evaluate a formula but no JavaScript engine \
                 is available in this context",
                self.trigger
            ))
        })
    }

    /// The adapter for `language`, or a configuration error naming the trigger
    /// and the language.
    ///
    /// Fails for the reason [`evaluator`](ActionContext::evaluator) does, and
    /// its message is the one an admin needs: a `run_python_code` trigger in a
    /// process that registered no Python adapter has nothing correct to do, and
    /// the one thing it must not do is return success. What it does **not** say
    /// is *why* there is no adapter — a server registers one either way, and the
    /// sentence about a build without Python support comes from the adapter
    /// itself, which is the only thing that knows.
    pub fn adapter(&self, language: &str) -> Result<&Arc<dyn CodeAdapter>> {
        self.adapters
            .and_then(|adapters| adapters.get(language))
            .ok_or_else(|| {
                Error::config(format!(
                    "trigger `{}` runs {language} code but no {language} adapter is \
                     available in this context",
                    self.trigger
                ))
            })
    }

    /// The dispatcher, when this context has one.
    ///
    /// An `Option` rather than [`evaluator`](ActionContext::evaluator)'s
    /// `Result`, because the caller that asks is `run_js_code`, and what it does
    /// with `None` is bind no `trigger` at all — so a body that names it in a
    /// context with no dispatcher gets a `ReferenceError` naming it, which is
    /// what every other absent surface there does.
    pub fn triggers(&self) -> Option<&TriggerDispatcher> {
        self.triggers
    }

    /// The mail transport, or a configuration error naming the trigger.
    ///
    /// Fails for the same reason [`evaluator`](ActionContext::evaluator) does: a
    /// `send_email` with nothing to send through has nothing correct to do, and
    /// the one thing it must not do is return success. The two errors are
    /// different sentences on purpose — "no engine here" is a wiring mistake in
    /// the process, while "no mailer here" is what a caller who never installed
    /// one sees.
    pub fn mailer(&self) -> Result<&Arc<dyn Mailer>> {
        self.mailer.ok_or_else(|| {
            Error::config(format!(
                "trigger `{}` sends mail but no mail transport is available in \
                 this context",
                self.trigger
            ))
        })
    }

    /// A configuration setting, if present.
    pub fn setting(&self, key: &str) -> Option<&Json> {
        self.config.get(key)
    }

    /// A required string setting, or an error naming the trigger and the setting.
    ///
    /// Save-time validation against the [`config_spec`](Action::config_spec)
    /// should mean this always succeeds; it exists because "should" is not a
    /// guarantee once a row can be edited by a restore, and because the
    /// alternative at the call site is an `unwrap`.
    ///
    /// Returns an owned `String` rather than a borrow of the configuration: an
    /// action reads its settings *and* writes [`context`](ActionContext::context),
    /// and a borrow of `&self` here would make the second of those a borrow-check
    /// error in every action that does both. One small clone per setting is the
    /// cheaper half of that trade.
    pub fn require_str(&self, key: &str) -> Result<String> {
        match self.config.get(key) {
            Some(Json::String(s)) if !s.is_empty() => Ok(s.clone()),
            _ => Err(Error::config(format!(
                "trigger `{}` is missing the `{key}` setting",
                self.trigger
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal action, standing in for the Phase 3 built-ins: it reads one
    /// setting, writes to the run context, and returns a value derived from the
    /// event — the whole `ActionContext` contract in one implementation.
    struct Echo;

    #[async_trait::async_trait]
    impl Action for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Return a configured message and the event's channel"
        }
        fn config_spec(&self) -> Vec<FormField> {
            vec![FormField::new("message", sc_types::BasicType::Text).required()]
        }
        async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
            let message = ctx.require_str("message")?;
            ctx.context.insert("ran".into(), json!(true));
            Ok(json!({
                "message": message,
                "channel": ctx.event.channel,
                "trigger": ctx.trigger,
            }))
        }
    }

    // Everything an `ActionContext` does needs a `Catalog`, which needs a
    // database, so the context's own contract is pinned in
    // `tests/action_context.rs` against a real one. What is asserted here is what
    // is genuinely catalog-free: that the trait is object-safe and declares its
    // configuration as data.
    #[test]
    fn the_action_trait_is_object_safe_and_declares_its_config() {
        let action: Arc<dyn Action> = Arc::new(Echo);
        assert_eq!(action.name(), "echo");
        assert!(!action.description().is_empty());
        let spec = action.config_spec();
        assert_eq!(spec.len(), 1);
        assert_eq!(spec[0].name(), "message");
        assert!(spec[0].required);
    }
}
