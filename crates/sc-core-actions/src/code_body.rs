//! What the two code-body actions have in common, which is everything but the
//! language.
//!
//! `run_js_code` and `run_python_code` are the same action twice: the same two
//! settings, the same default and the same ceiling on the timeout, the same
//! scope rule for what the event binds, and the same six host surfaces built
//! from the same [`ActionContext`]. The only differences are which engine runs
//! the source and what the editor is told to highlight.
//!
//! So it is one implementation with the language as an argument, rather than two
//! that have to be kept in step. That is not tidiness: the two languages
//! agreeing about authority, budgets and events is the property TODO §2 is
//! after, and a second copy of `bindings` is exactly how a body would come to
//! see `payload` in one language and not the other.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sc_api::code_host::{FileStoreHost, TableHost, TriggerRunHost, schema_snapshot};
use sc_error::{Error, Result};
use sc_expr::{
    CodeCall, CodeHosts, ConsoleSink, DEFAULT_CODE_TIMEOUT, JsEvaluator, LiveHost,
    MAX_CODE_TIMEOUT, ModuleFnHost, SchemaSnapshot, TriggerHost,
};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use sc_action::{ActionContext, Event};

use crate::code_fetch::CodeFetchHost;

/// The code body.
pub(crate) const CFG_CODE: &str = "code";
/// How long one run may take.
pub(crate) const CFG_TIMEOUT: &str = "timeout_ms";

/// What a trigger that names no timeout gets — the engine's own default, said in
/// the unit the form takes.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = DEFAULT_CODE_TIMEOUT.as_millis() as u64;

/// The ceiling on a configured timeout.
///
/// Bounded for the reason `fetch`'s is: a trigger runs inside the request or the
/// write that fired it, so an unbounded body is an unbounded hold on that caller.
/// A body now reads and writes tables, so it can genuinely need more than the
/// default — and still not a minute.
pub(crate) const MAX_TIMEOUT_MS: u64 = MAX_CODE_TIMEOUT.as_millis() as u64;

/// The two settings a code body has, with the editor told which language to
/// highlight and complete.
///
/// The declaration is the whole coupling between the action and the admin UI: no
/// screen knows either of these settings by name, so a third language is a third
/// call here and nothing else.
pub(crate) fn config_spec(language: &str) -> Vec<FormField> {
    vec![
        FormField::new(CFG_CODE, BasicType::Text)
            .label("Code")
            .code(language)
            .required(),
        FormField::new(CFG_TIMEOUT, BasicType::Int)
            .label("Timeout (ms)")
            .default_value(DEFAULT_TIMEOUT_MS),
    ]
}

/// The configured wall clock for one run, **bounded** ([`MAX_TIMEOUT_MS`]), or
/// `None` when the trigger names none.
///
/// `None` rather than [`DEFAULT_TIMEOUT_MS`] resolved here, because the engine
/// has that default already and a process may have been started with another one:
/// an admin who left the field alone said "whatever this server's default is",
/// and turning that into a number would overrule it.
///
/// Out of range is refused rather than clamped, exactly as `fetch`'s is: an admin
/// who typed five minutes should be told it is not allowed, not quietly given one.
pub(crate) fn timeout(config: &Attrs) -> Result<Option<Duration>> {
    let ms = match config.get(CFG_TIMEOUT) {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::Number(n)) => n.as_i64().ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_TIMEOUT}` must be a whole number of milliseconds"
            ))
        })?,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{CFG_TIMEOUT}` must be a whole number of milliseconds, got {other}"
            )));
        }
    };
    if !(1..=MAX_TIMEOUT_MS as i64).contains(&ms) {
        return Err(Error::invalid(format!(
            "`{CFG_TIMEOUT}` must be between 1 and {MAX_TIMEOUT_MS} milliseconds, got {ms}"
        )));
    }
    Ok(Some(Duration::from_millis(ms.unsigned_abs())))
}

/// What the event binds in the code's scope (`row`, `old`, `user`, `payload`) —
/// plus `context` when this body is a **workflow step** (§10.3, decision 8): the
/// run so far, as an object, which is the same thing a step's formulas read.
///
/// Presence is scope, exactly as it is for a formula: an absent binding is an
/// unknown-identifier error naming it (a `ReferenceError` in JavaScript, a
/// `NameError` in Python), a binding present as null is a value. `old` on an
/// insert is the case that distinguishes the two — in scope, null. `context` is
/// the other: outside a run it is not bound at all, so a body that names it says
/// so rather than reading an empty object as "nothing has happened yet".
///
/// A **custom query**'s body (§13.4) has a request rather than an event: its
/// names (`body`, `query`) are bound in place of `payload`, which it has none of,
/// and `user` is the caller as it is everywhere else.
pub(crate) fn bindings(
    event: &Event,
    run_context: Option<&Attrs>,
    request: Option<&Attrs>,
) -> BTreeMap<String, Json> {
    let mut bindings = BTreeMap::new();
    if let Some(request) = request {
        bindings.extend(request.iter().map(|(k, v)| (k.clone(), v.clone())));
        bindings.insert("user".to_owned(), event.user.clone().unwrap_or(Json::Null));
        return bindings;
    }
    if let Some(context) = run_context {
        bindings.insert(
            "context".to_owned(),
            Json::Object(
                context
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
    }
    if event.kind.is_table_event() {
        bindings.insert("row".to_owned(), Json::Object(event.row_object()));
        bindings.insert(
            "old".to_owned(),
            match event.old_row {
                Some(_) => Json::Object(event.old_row_object()),
                None => Json::Null,
            },
        );
    }
    bindings.insert("user".to_owned(), event.user.clone().unwrap_or(Json::Null));
    bindings.insert("payload".to_owned(), event.payload.clone());
    bindings
}

/// What an error from this run is about: the trigger, or the custom query whose
/// body it is — an API caller who gets a message about a trigger they never
/// heard of is left guessing.
pub(crate) fn subject(ctx: &ActionContext<'_>) -> String {
    match ctx.request() {
        Some(_) => format!("custom query `{}`", ctx.trigger),
        None => format!("trigger `{}`", ctx.trigger),
    }
}

/// The six host surfaces one run may reach — and the schema it reads without
/// reaching for anything — built from the context that fired it and borrowed by
/// the [`CodeCall`] for exactly as long as the run.
///
/// A struct rather than five locals at each call site because a [`CodeCall`]
/// borrows all of them: they have to outlive the call, so they have to live
/// somewhere the caller named.
///
/// **Public**, because a code body is not the only thing that runs over these
/// five. A Python **plugin module**'s action reaches the same five (`sc_python`,
/// TODO "Python plugin modules" §2) and is not a code body: what it runs is an
/// installed package's function. Building them there a second time is exactly
/// how the two would come to disagree about who a module's write is caused by,
/// so they are built here, once, and handed over as
/// [`surfaces`](Hosts::surfaces).
///
/// The schema snapshot is here for the same reason and is **not** one of the
/// five: nothing calls it. It is what v1's synchronous `Table.findOne` is
/// answered from (TODO "the v1 `Table` API" §2), local to the isolate and
/// asked of nobody.
pub struct Hosts<'a> {
    table: TableHost<'a>,
    fetch: CodeFetchHost,
    files: FileStoreHost<'a>,
    triggers: Option<TriggerRunHost<'a>>,
    module_fns: Option<Arc<dyn ModuleFnHost>>,
    /// Live updates (`live.publish`), when this server has a live hub.
    live: Option<Arc<dyn LiveHost>>,
    schema: Option<Arc<SchemaSnapshot>>,
    /// Where this run's `console.*` lines go, when the context is collecting
    /// them (the admin's Test run). Not one of the six surfaces and not a
    /// capability: every body has a console, and this is only whether anybody
    /// is reading it.
    console: Option<ConsoleSink>,
}

impl<'a> Hosts<'a> {
    /// Everything this run may reach, on this event's terms.
    ///
    /// The evaluator is an `Option` rather than a requirement: what needs it is
    /// a **delegated** read whose ownership rule is a formula (§7.3), evaluated
    /// on the formula isolate while this body's own run waits. A process with no
    /// JavaScript engine can still run a Python body, and the read that needs a
    /// formula fails there saying so — which is a better answer than refusing
    /// every Python body in a process that has no JavaScript in it.
    pub fn new(
        ctx: &'a ActionContext<'a>,
        evaluator: Option<&Arc<dyn JsEvaluator>>,
        client: reqwest::Client,
    ) -> Hosts<'a> {
        Hosts {
            // The `db` handle, for this run only: the call budget is counted on
            // it, and the event's caller and this trigger's chain ride on every
            // statement it makes — so a write from the body is an event that
            // says who caused it and how deep in a cascade it already is.
            table: TableHost::new(ctx.catalog)
                .caused_by(ctx.event.role, ctx.event.user.clone())
                .chained(ctx.chain.clone())
                // The step's transaction, when this body is a workflow step:
                // what it writes commits with the step or is rolled back with
                // it, and what it reads sees what the step has already written
                // (§10.3, decision 6).
                .in_transaction(ctx.transaction())
                .with_evaluator(evaluator.map(Arc::clone)),
            // The network, on the same terms: borrowed for this run, bounded by
            // the run's own clock, and counted on a budget of its own.
            fetch: CodeFetchHost::new(client, ctx.trigger),
            // The file stores, on the same terms again. It carries the event's
            // role rather than its user, because a file rule is a role floor:
            // `as_user` is checked against it, and the admin default clears
            // every one.
            files: FileStoreHost::new(ctx.catalog)
                .caused_by(ctx.event.role, ctx.event.user.as_ref()),
            // The other triggers, when this context has a dispatcher — and
            // nothing at all when it does not, so a body that names `trigger`
            // outside a server says so by name. It carries the caller *and* the
            // chain: the caller because the trigger that runs must see who
            // caused it, and the chain because that is what stops a body running
            // the trigger it is itself the action of, at the same depth every
            // other cascade stops at.
            triggers: ctx.triggers().map(|d| {
                TriggerRunHost::new(d, ctx.catalog)
                    .caused_by(ctx.event.role, ctx.event.user.clone())
                    .chained(ctx.chain.clone())
                    .in_transaction(ctx.transaction())
            }),
            // This server's tables as the guest sees them without asking — what
            // v1's synchronous `Table.findOne` is answered from. Built here,
            // once, for the reason the six surfaces are: a code body, a
            // workflow step and a module's action must all be looking at the
            // same schema, and the catalog caches it behind its generation
            // stamp so this is a clone of an `Arc` on every firing but the
            // first after a reload.
            //
            // `None` only where the catalog cannot be read at all — a poisoned
            // lock, on which this run's `db` call is about to fail by name too
            // — and a `Table` built over nothing refuses by name rather than
            // answering an empty schema.
            schema: schema_snapshot(ctx.catalog).ok(),
            // The module functions, when this server has modules installed and
            // loaded — and nothing at all when it does not, so a body that names
            // `modfn` on a server with no modules says so by name rather than
            // finding an empty handle. The catalog is where they are because a
            // formula's hoisted call needs them too, from three other crates.
            module_fns: ctx.catalog.module_functions(),
            // Live updates, when this server has a hub — and nothing at all
            // when it does not, so a body that names `live` in a process with
            // no hub says so by name. Server-side publishing is trusted (§3
            // rule 8), so nothing about the event's caller rides on it.
            live: ctx.catalog.live_host(),
            // Whoever asked for the transcript, if anyone did.
            console: ctx.console().cloned(),
        }
    }

    /// The six surfaces alone, for a run that is not a code body — a Python
    /// module's action, which has no source and no bindings.
    pub fn surfaces(&'a self) -> CodeHosts<'a> {
        CodeHosts {
            host: Some(&self.table),
            fetch: Some(&self.fetch),
            files: Some(&self.files),
            triggers: self.triggers.as_ref().map(|r| r as &dyn TriggerHost),
            module_fns: self.module_fns.as_deref(),
            live: self.live.as_ref(),
        }
    }

    /// This server's schema as the guest sees it, for a run that is not a code
    /// body — the same value, from the same place, for the reason
    /// [`surfaces`](Hosts::surfaces) exists.
    pub fn schema(&self) -> Option<&SchemaSnapshot> {
        self.schema.as_deref()
    }

    /// One call over these hosts: the source, what the event binds, and how long
    /// it may take.
    pub(crate) fn call(
        &'a self,
        code: String,
        bindings: BTreeMap<String, Json>,
        timeout: Option<Duration>,
    ) -> CodeCall<'a> {
        let surfaces = self.surfaces();
        CodeCall {
            code,
            bindings,
            host: surfaces.host,
            fetch: surfaces.fetch,
            files: surfaces.files,
            triggers: surfaces.triggers,
            module_fns: surfaces.module_fns,
            live: surfaces.live,
            schema: self.schema(),
            console: self.console.clone(),
            timeout,
            ..CodeCall::default()
        }
    }
}

/// The HTTP client behind `fetch`, held once, and the six surfaces built over
/// it (§10.1).
///
/// What it is *for* is a caller outside this crate: a Python **plugin module**'s
/// action reaches the same six surfaces a code body does, and building them
/// needs a `reqwest::Client` — which has to be built once, because a client is a
/// connection pool and a TLS configuration and one per firing would pay for a
/// handshake every time a trigger runs. Holding one of these is how a caller
/// gets that without naming `reqwest` itself.
pub struct CodeSurfaces {
    client: reqwest::Client,
}

impl CodeSurfaces {
    /// Build the client. Fallible because it initialises the TLS stack: a
    /// deployment where that fails should say so at boot rather than at the
    /// first call that needs the network.
    pub fn new() -> Result<CodeSurfaces> {
        Ok(CodeSurfaces {
            client: crate::fetch::http_client()?,
        })
    }

    /// The six surfaces this context can reach, borrowed for as long as the
    /// value is held.
    ///
    /// The evaluator is the context's own where it has one, exactly as
    /// `run_python_code` takes it: what needs it is a **delegated** read whose
    /// ownership rule is a formula, and a process with no JavaScript engine
    /// should still run everything that does not.
    pub fn build<'a>(&self, ctx: &'a ActionContext<'a>) -> Hosts<'a> {
        Hosts::new(ctx, ctx.evaluator().ok(), self.client.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_action::EventKind;
    use serde_json::json;

    #[test]
    fn the_timeout_defers_to_the_engine_when_unset_and_is_bounded_when_set() {
        let config =
            |value: Json| -> Attrs { [(CFG_TIMEOUT.to_owned(), value)].into_iter().collect() };
        // Unset is not "5000" but "whatever this server's default is": resolving
        // it here would overrule a process started with another one.
        assert_eq!(timeout(&Attrs::new()).unwrap(), None);
        assert_eq!(timeout(&config(Json::Null)).unwrap(), None);
        assert_eq!(
            timeout(&config(json!(250))).unwrap(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            timeout(&config(json!(MAX_TIMEOUT_MS))).unwrap(),
            Some(MAX_CODE_TIMEOUT),
            "the ceiling itself is allowed"
        );
        // Refused rather than clamped, and each refusal names the setting.
        for bad in [
            json!(MAX_TIMEOUT_MS + 1),
            json!(0),
            json!(-5),
            json!("soon"),
        ] {
            let msg = timeout(&config(bad.clone())).unwrap_err().to_string();
            assert!(msg.contains(CFG_TIMEOUT), "{bad}: {msg}");
        }
    }

    #[test]
    fn presence_is_scope_for_the_events_bindings() {
        // An update binds both rows; every name in the scope is there.
        let update = Event::new(EventKind::Update)
            .on("books")
            .row(json!({ "id": 1, "title": "now" }))
            .old_row(json!({ "id": 1, "title": "was" }))
            .caller(1, Some(json!({ "email": "a@b.c" })));
        let bound = bindings(&update, None, None);
        assert_eq!(
            bound.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["old", "payload", "row", "user"]
        );
        assert_eq!(bound["old"]["title"], json!("was"));
        assert_eq!(bound["user"]["email"], json!("a@b.c"));
        assert_eq!(bound["payload"], Json::Null);

        // An insert has `old` in scope and null — a value, not an absence.
        let insert = Event::new(EventKind::Insert)
            .on("books")
            .row(json!({ "id": 1 }));
        let bound = bindings(&insert, None, None);
        assert_eq!(bound["old"], Json::Null);
        assert_eq!(bound["user"], Json::Null, "anonymous binds null");

        // An event with no row binds neither, so code naming `row` there fails
        // in the engine instead of reading undefined.
        let called = Event::new(EventKind::None).payload(json!({ "n": 2 }));
        let bound = bindings(&called, None, None);
        assert!(!bound.contains_key("row") && !bound.contains_key("old"));
        assert_eq!(bound["payload"], json!({ "n": 2 }));

        // Outside a run there is no `context` at all; as a workflow step it is
        // bound — including on the first step, where it is empty. Presence is
        // scope, so a body may ask what has happened before anything has.
        assert!(!bound.contains_key("context"));
        let step = bindings(&called, Some(&Attrs::new()), None);
        assert_eq!(step["context"], json!({}));
        let later: Attrs = [("total".to_owned(), json!(120))].into_iter().collect();
        assert_eq!(
            bindings(&called, Some(&later), None)["context"],
            json!({"total": 120})
        );
    }

    #[test]
    fn a_custom_querys_body_sees_the_request_and_the_caller_and_no_payload() {
        let called = Event::new(EventKind::None).caller(80, Some(json!({ "email": "a@b.c" })));
        let request: Attrs = [
            ("body".to_owned(), json!({ "title": "Dune" })),
            ("query".to_owned(), json!({ "since": 1965 })),
        ]
        .into_iter()
        .collect();
        let bound = bindings(&called, None, Some(&request));
        assert_eq!(
            bound.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["body", "query", "user"]
        );
        assert_eq!(bound["body"]["title"], json!("Dune"));
        assert_eq!(bound["query"]["since"], json!(1965));
        assert_eq!(bound["user"]["email"], json!("a@b.c"));
    }

    #[test]
    fn the_two_languages_declare_the_same_settings_with_their_own_editor() {
        for (language, spec) in [
            ("javascript", config_spec("javascript")),
            ("python", config_spec("python")),
        ] {
            let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
            assert_eq!(names, vec![CFG_CODE, CFG_TIMEOUT]);
            assert!(spec[0].required);
            assert_eq!(spec[0].code_language.as_deref(), Some(language));
            assert!(!spec[1].required, "a body may take the server's default");
            assert_eq!(spec[1].default, Some(json!(DEFAULT_TIMEOUT_MS)));
        }
    }
}
