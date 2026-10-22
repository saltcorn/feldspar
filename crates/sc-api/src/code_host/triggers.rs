//! Other triggers in a code body: the host behind `trigger` (§10.2).
//!
//! `sc-expr` runs the JavaScript and knows nothing about triggers — its
//! [`TriggerHost`] seam is one JSON request in, one JSON value out, the shape
//! §15's other guest languages implement. [`TriggerRunHost`] is the
//! implementation of that seam for *this* server, and it lives here for
//! [`TableHost`](super::TableHost)'s reason: everything it needs is already at
//! this layer — the catalog, and the one
//! [`TriggerDispatcher`](sc_action::TriggerDispatcher) every other event fires
//! on.
//!
//! # It runs the dispatcher's trigger, not a copy of it
//!
//! The whole of the design, and the same sentence the agent's `run_trigger`
//! trait is built on: the call goes through the dispatcher, so the target's
//! `only_if` runs, a disabled trigger stays disabled, a trigger that failed
//! validation says why, and the **cascade bound** counts this run — a code body
//! is one more thing that can ask, not a second way to fire.
//!
//! That bound is what makes the recursion safe rather than merely bounded in
//! practice. The event this host builds carries the chain the body's own trigger
//! was handed, so `Event::firing` refuses past `MAX_DEPTH` and names the whole
//! chain: a body that runs the trigger it is itself the action of stops at the
//! fifth turn with a sentence an admin can read, not a stack overflow.
//!
//! # Authority
//!
//! `db`'s rule, for `db`'s reason. A trigger is server-side configuration, so
//! running one from another is configuration calling configuration: the default
//! is the **admin's** authority, under which the target's `min_role` is not
//! consulted at all. `asUser()` says "on behalf of whoever caused this event"
//! instead, and then the target's own floor decides — the check the agent trait
//! makes, in the same words, because exposing one trigger must not thereby
//! expose every trigger it can reach.
//!
//! What does **not** depend on the authority is who the child event says caused
//! it: the event's role and user travel either way, so the trigger that runs
//! sees the same `user` it would have seen had that caller run it directly, and
//! its own `db.asUser()` means the same person. Causation is a fact; authority
//! is a decision.
//!
//! # Bounds
//!
//! The width — how many triggers one body may run — is counted here as well as
//! in the guest, because a bound the guest could edit is not a bound. The
//! clock is the parent's: `timeout_ms` arrives filled in with what was left of
//! the body's wall clock, and the run is abandoned at it, so a slow child fails
//! inside the body that started it rather than holding the request that fired
//! the outermost trigger.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sc_action::TriggerDispatcher;
use sc_catalog::{CallerContext, Catalog, SharedTx};
use sc_error::{Error, Result};
use sc_expr::{DEFAULT_MAX_TRIGGER_RUNS, TriggerHost};
use serde::Deserialize;
use serde_json::Value as Json;

use super::plan::Authority;

/// The `trigger` handle of one code-body run, on the host's side.
///
/// **One per run**, for [`TableHost`](super::TableHost)'s reason: the run budget
/// is counted on it, and the event's caller and trigger chain ride on every run
/// it starts — so sharing one between runs would share a budget between them and
/// attribute one body's cascade to another's.
pub struct TriggerRunHost<'a> {
    /// The one dispatcher every other event fires on.
    ///
    /// **Borrowed**, like the catalog and for the same reason: what holds it is
    /// the server, and threading an `Arc` down to a firing trigger would mean
    /// threading one through the write that fired it.
    dispatcher: &'a TriggerDispatcher,
    /// The catalog the run happens against.
    catalog: &'a Catalog,
    /// The transaction the trigger this body runs should write in — a workflow
    /// step's, when the body is a step of one (§10.3, decision 6). What
    /// `trigger("name")` does to the database is as much the step's work as what
    /// the body did itself, and lands where the step lands.
    tx: Option<SharedTx>,
    /// The role the event was served at — what a delegated run is checked
    /// against, and public for an event with no caller at all.
    role: u8,
    /// The event's caller, as the event carries it: what the trigger that runs
    /// sees as `user`, whichever authority it runs under.
    user: Option<Json>,
    /// The triggers that led here, including the one running. The child event
    /// carries it, which is what bounds the cascade.
    chain: Vec<String>,
    /// How many other triggers this run may run.
    max_runs: u32,
    /// How many it has run.
    runs: AtomicU32,
}

impl<'a> TriggerRunHost<'a> {
    /// A handle over `dispatcher`, with the default bound and no caller.
    pub fn new(dispatcher: &'a TriggerDispatcher, catalog: &'a Catalog) -> TriggerRunHost<'a> {
        TriggerRunHost {
            dispatcher,
            catalog,
            role: sc_auth::ROLE_PUBLIC,
            user: None,
            chain: Vec::new(),
            tx: None,
            max_runs: DEFAULT_MAX_TRIGGER_RUNS,
            runs: AtomicU32::new(0),
        }
    }

    /// Attach the event's caller: the role it was served at and the user's own
    /// fields, exactly as `Event::caller` carries them.
    #[must_use]
    pub fn caused_by(mut self, role: u8, user: Option<Json>) -> TriggerRunHost<'a> {
        self.role = role;
        self.user = user;
        self
    }

    /// Attach the trigger chain that led here — what bounds the cascade.
    #[must_use]
    pub fn chained(mut self, chain: Vec<String>) -> TriggerRunHost<'a> {
        self.chain = chain;
        self
    }

    /// Run the triggers this body starts **inside `tx`** — a workflow step's
    /// transaction, when the body is a step of one (§10.3, decision 6).
    #[must_use]
    pub fn in_transaction(mut self, tx: Option<SharedTx>) -> TriggerRunHost<'a> {
        self.tx = tx;
        self
    }

    /// Set the bound, in place of [`DEFAULT_MAX_TRIGGER_RUNS`].
    #[must_use]
    pub fn with_max_runs(mut self, max_runs: u32) -> TriggerRunHost<'a> {
        self.max_runs = max_runs;
        self
    }

    /// Run one trigger, and answer what its action returned.
    async fn run_one(&self, request: Request) -> Result<Json> {
        // Counted here as well as in the guest: what the guest counts is what a
        // body could rewrite if it found a way to, and this is the number that
        // actually bounds the width of the cascade.
        let spent = self.runs.fetch_add(1, Ordering::SeqCst);
        if spent >= self.max_runs {
            return Err(Error::invalid(format!(
                "this code ran more than {} other triggers in one run; the bound \
                 exists so a loop over rows cannot become a run per row",
                self.max_runs
            )));
        }
        let name = request.trigger.trim();
        if name.is_empty() {
            return Err(Error::invalid("trigger() needs the name of a trigger"));
        }
        // Whose run this is. Three of the four authorities are somebody the floor
        // is checked against; the fourth is the admin's own, where there is
        // nothing to check because a trigger is configuration and configuration
        // is the admin's. A **named** user is v1's `run_trigger(…, user)`: it is
        // loaded where users live and then treated exactly as the event's caller
        // is, at that user's own role — which can only narrow, since a body that
        // said nothing runs as admin.
        let (caller, named) = match &request.authority {
            // The event's own caller, which is what both of these run as: the
            // admin's authority is the *trigger's*, and the run still carries
            // who caused it.
            Authority::Admin | Authority::User => (
                CallerContext::new(self.role, self.user.clone()).in_current_origin(),
                None,
            ),
            Authority::Public => (CallerContext::new(sc_auth::ROLE_PUBLIC, None), None),
            Authority::Named(id) => {
                let user = super::load_named_user(self.catalog, id).await?;
                let caller = crate::ownership::caller_context_at(user.role, Some(&user));
                (caller, Some(user))
            }
        };
        // The floor, before anything happens and only where it applies.
        // `min_role` absent means admin-only — the safe reading a trigger nobody
        // has thought about the access of gets everywhere else (§10.2).
        if !request.authority.is_admin() {
            // `require` first, so "you may not run it" never stands in for "there
            // is no such trigger": the second is the answer the author needs.
            let triggers = self.dispatcher.triggers()?;
            let min_role = triggers.require(name)?.min_role;
            let floor = min_role.unwrap_or(sc_auth::ROLE_ADMIN);
            if caller.role > floor {
                return Err(Error::auth(format!(
                    "{} may not run `{name}`; it needs role {floor} or better",
                    match (&named, &request.authority) {
                        (Some(user), _) => format!("user `{}`", user.id),
                        (None, Authority::Public) => "the public role".to_owned(),
                        _ => "this event's caller".to_owned(),
                    }
                )));
            }
        }
        // The caller carries the chain that led here, so the trigger that runs
        // sees who caused it and the cascade bound counts this level.
        let caller = caller.chained(self.chain.clone());
        let run = self.dispatcher.run_trigger_in(
            self.catalog,
            name,
            request.payload,
            Some(&caller),
            self.tx.as_ref(),
        );
        let Some(limit) = request.timeout_ms.map(Duration::from_millis) else {
            return run.await;
        };
        // Abandoned at the parent's deadline rather than allowed to outlive it:
        // what runs at the other end has a clock of its own and no idea anything
        // is waiting on it, and an unbounded hold on the request that fired the
        // outermost trigger is exactly what the wall clock exists to prevent.
        match tokio::time::timeout(limit, run).await {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::invalid(format!(
                "trigger `{name}` did not finish within the {} ms this code had left",
                limit.as_millis()
            ))),
        }
    }
}

/// One trigger run, as it crosses the seam.
#[derive(Debug, Deserialize)]
struct Request {
    /// The trigger to run, by the name an admin gave it.
    trigger: String,
    /// The event's payload — what the trigger's action reads, and what `POST
    /// {mount}/actions/{name}` would have sent. Absent is an empty object, which
    /// is what the guest sends for `run()` with no argument.
    #[serde(default = "empty_payload")]
    payload: Json,
    /// Whose authority the run carries.
    #[serde(default)]
    authority: Authority,
    /// What is left of the calling body's wall clock, filled in by the op.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn empty_payload() -> Json {
    Json::Object(serde_json::Map::new())
}

#[async_trait]
impl TriggerHost for TriggerRunHost<'_> {
    async fn run(&self, request: Json) -> Result<Json> {
        let request: Request = serde_json::from_value(request)
            .map_err(|e| Error::invalid(format!("this trigger run is not one: {e}")))?;
        self.run_one(request).await
    }

    /// What this run may name — read from the live set, so a typo is a sentence
    /// naming the triggers that do exist rather than an error at `run()`.
    ///
    /// Every stored trigger that is usable, including the disabled ones: a
    /// disabled trigger exists, and "there is no trigger named `nightly`" would
    /// be the wrong sentence about one an admin switched off this morning.
    fn trigger_names(&self) -> Vec<String> {
        match self.dispatcher.triggers() {
            Ok(triggers) => triggers.all().iter().map(|t| t.name.clone()).collect(),
            // The lock is poisoned, or there is no set yet: an empty list is the
            // guest's "no list to check against", so the name goes through and
            // `run()` answers with whatever the dispatcher says about it.
            Err(_) => Vec::new(),
        }
    }
}
