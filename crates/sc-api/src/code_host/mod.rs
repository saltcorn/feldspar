//! Tables in a code body: the host behind `db` (§10.1, the milestone "Tables in
//! code").
//!
//! `sc-expr` runs the JavaScript and knows nothing about tables — its
//! [`CodeHost`] seam is one JSON request in, one JSON value out, which is what
//! makes it the seam §15's other guest languages implement rather than a
//! JavaScript feature. [`TableHost`] is the implementation of that seam for *this*
//! server's tables, and it lives here because everything it needs already does:
//! the catalog, the formula translator, the shared filter vocabulary, §7.3's
//! ownership rule and the row layer whose writes are events.
//!
//! # What crosses
//!
//! One [`Plan`] per terminal — `db.books.where(…).limit(10).rows()` is one plan
//! and one round trip. The guest builds it; **nothing in it is trusted**. The
//! table is resolved through the catalog, every column against that table, every
//! Ⱶ-path through [`ownership::join_guard`](sc_access::ownership), every formula
//! through the one translator, and anything else is refused naming it: a plan
//! carries names and values, and the statement is built here.
//!
//! # The one exception, named
//!
//! `db.sql("select …", [args])` sends a [`SqlPlan`] instead, and its text is the
//! statement. It exists because the row layer's read does not express everything
//! — a window function, a recursive CTE, an `ON CONFLICT` — and a code body that
//! has to leave the server to ask is not an escape hatch. It is the **same**
//! admission §13.4's custom SQL queries are, on the same grounds and with the
//! same paragraph of consequences: a `run_js_code` body is server-side
//! configuration written by an administrator, so the rule on
//! [`Statement::Raw`](sc_query::Statement::Raw) — raw SQL is authored, never
//! assembled from what a caller sent — holds by construction. The body's own
//! values reach it as **binds** and nowhere else.
//!
//! What that costs is exactly the list of things the paragraph above is: no
//! ownership formula filters it, no rich type coerces what it returns, no
//! `File`-field rule governs it, and a write inside one raises **no table
//! event** — so a trigger will not see it, and the row layer's `db.books.insert`
//! is still the way to write a row that other triggers are meant to notice. What
//! does still hold is the caller-context transaction (an RLS-protected table's
//! policies decide), the row cap and the call budget.
//!
//! # Authority
//!
//! A code body's reads and writes are the **admin's** by default (§5), carrying
//! the event's user so an RLS policy that reads `user` still sees who caused it —
//! the rule `rows_scope.rs` already states for `insert_row`/`update_rows`: a
//! trigger is server-side configuration, and an audit row the caller may not
//! insert is the archetype of what a trigger exists to write.
//!
//! `asUser()` sets one field of the plan and **delegates to the event's caller**
//! instead. Every operation then goes through [`sc_access::ownership`]'s `*_as`
//! functions at the event's own role and user — §7.3's rule, the same functions
//! the agent tools go through, with no second implementation of "meets the floor
//! OR the formula grants it" here to be subtly wrong. So a delegated read is
//! narrowed to the rows the ownership formula grants (in the `WHERE` where it
//! can be, row by row where it cannot), a delegated update is checked on the row
//! as it is *and* on the row as it would become, a withheld row is the same
//! **not found** an absent row gets, and an RLS table is read and written inside
//! a caller-context transaction where the database's own policies decide.
//!
//! Delegation means less for `db.sql()` than it does for the chain, and the
//! difference is stated where the operation is: it runs the statement at the
//! caller's role and user, which is what row-level security reads — and nothing
//! else, because an ownership formula is applied by a row layer raw SQL does not
//! go through.
//!
//! Two more authorities exist for Saltcorn 1's sake, and both are that same
//! delegation with a different somebody in it (TODO "the v1 `Table` API" §4).
//! v1 says whose view of the data it wants with an **argument** —
//! `getRows(where, { forUser: u })`, `insertRow(row, user)` — and that argument
//! is frequently a user the plugin looked up rather than the event's caller. So
//! a plan may say `{ "user": id }`, which loads that user and goes through the
//! very same `*_as` functions, at that user's own role; and `"public"`, which is
//! the public role with nobody in it. Neither can be an escalation — the body
//! already runs as admin and could read everything by saying nothing — and a
//! named user who does not exist is an error naming the id, never a quiet fall
//! back to the default.
//!
//! Events differ in whom they have to delegate to, and that difference is
//! honoured rather than hidden: a table event or a directly-run trigger carries
//! the user who caused it, while a scheduled or startup trigger carries nobody
//! and therefore reads as the **public** role. Which is why `asAdmin()` is the
//! default.
//!
//! # Bounds
//!
//! Two of the three are here rather than in the guest, because a bound the guest
//! could edit is not a bound: the **row cap** (a read is materialised into the
//! isolate, so an unbounded `.rows()` on a large table is an out-of-memory) and
//! the **call budget** (an accidental N+1 must not hammer the database quietly).
//! The third — the wall clock — is the runtime's, since only it can stop a body
//! that never asks for anything.
//!
//! # Streaming
//!
//! `.iter()` is the answer to the row cap rather than an exception to it. It
//! sends one plan per **batch**, each a read of its own resumed from where the
//! last stopped, so the cap holds on every one of them and what a body may walk
//! is bounded by the call budget instead — which is the honest bound, because
//! what a stream spends is round trips and not memory.
//!
//! Resuming is a **keyset** on the read's own ordering, built in [`plan`]: the
//! primary key is appended to it so the order is total, null placement is stated
//! so the predicate and the `ORDER BY` cannot disagree, and the cursor is the
//! sort values of the last row read — projected into the same `SELECT` under
//! reserved aliases, because a `.select()` narrows what comes back and a Ⱶ-path
//! is no column of the table at all. Nothing in a cursor is trusted: it is
//! coerced against the column it sorts on exactly as a `.where()` literal is.
//!
//! Delegation is where a stream can be refused, and it is §5's asymmetry again:
//! a batch is bounded by the database, so the caller's read rule has to reach
//! inside the statement. RLS policies do and a translatable ownership formula
//! does; a formula only the evaluator can decide does not, and a batch of it
//! would fetch the whole table, hand back what survived and do it again — so it
//! is refused naming `.rows()`, which can decide row by row.

use sc_types::Role;
mod files;
mod history;
mod models;
mod plan;
pub mod schema;
mod triggers;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use sc_access::Scoped;
use sc_auth::User;
use sc_catalog::{
    AdminPrincipal, Authority as ScopedAuthority, Catalog, Principal, SharedTx, Table, Unchecked,
};
use sc_error::{Error, Repr, Result};
use sc_expr::{CodeHost, DEFAULT_MAX_HOST_CALLS, JsEvaluator};
use serde_json::Value as Json;

use sc_access::ownership;
use sc_access::rows;
use sc_types::value_to_json;

pub use files::{FileStoreHost, MAX_COPY_BYTES, MAX_FILE_BYTES, set_file_limits};
pub use models::CODE_MAX_DRAWS;
pub use plan::{AggSpec, Authority, Dir, Op, OrderKey, Plan, Selection, SqlOp, SqlPlan};
pub use schema::snapshot as schema_snapshot;
pub use triggers::TriggerRunHost;

/// How many rows one read may return before it is refused.
///
/// A read is materialised into the isolate: an unbounded `.rows()` over a large
/// table is an out-of-memory, not a slow query, and the error says to add a
/// `.limit()` because that is the fix. A table that has to be walked whole is
/// walked with `.iter()`, whose every batch is bounded by this same cap.
pub const DEFAULT_MAX_ROWS: u64 = 1000;

/// What one run of a code body may spend against the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    /// The most rows one read may return.
    pub max_rows: u64,
    /// The most database calls one run may make.
    pub max_calls: u32,
}

impl Default for HostLimits {
    fn default() -> Self {
        HostLimits {
            max_rows: DEFAULT_MAX_ROWS,
            max_calls: DEFAULT_MAX_HOST_CALLS,
        }
    }
}

/// The `db` handle of one code-body run, on the host's side.
///
/// **One per run.** The call budget is counted on it, and the event's caller and
/// trigger chain ride on every statement it makes — so sharing one between runs
/// would share a budget between them and attribute one run's writes to another's
/// cascade.
pub struct TableHost<'a> {
    /// The catalog every name is resolved through.
    ///
    /// **Borrowed**: the catalog is what the row layer, an action's context and
    /// every request handler already hold a reference to, and threading an `Arc`
    /// down to a firing trigger would mean threading one through the write that
    /// fired it. `sc_expr::CodeCall` therefore borrows its host too, and the code
    /// runtime bridges the borrow across to its isolate thread.
    catalog: &'a Catalog,
    /// The event's caller, as a principal (TODO-code-quality Q3.4): the role
    /// it was served at — what a delegated operation is checked against, and
    /// public for an event with no caller at all; the user, as the event
    /// carries it, attached to every statement (on an RLS table it is what the
    /// policies read, and off one it is what the **event** this run raises
    /// reports as its cause; under `asUser()` it is also read back as an
    /// [`sc_auth::User`], which is what §7.3's rule takes); and the triggers
    /// that led here, including the one running, which a write this host makes
    /// carries, so `Event::firing`'s cascade bound applies to a code body
    /// exactly as it does to an action.
    ///
    /// Every operation runs through an [`sc_access::Scoped`] built from it
    /// ([`scope`](TableHost::scope)); the origin and the changeset are the task's at
    /// the time of each call, as they always were.
    caller: Principal,
    /// The bounds.
    limits: HostLimits,
    /// The formula engine, for a delegated operation whose ownership formula the
    /// translator refuses and which therefore has to be decided row by row.
    ///
    /// A code thread blocks on the host call while this runs, on the *formula*
    /// isolate — which is exactly why the two runtimes are separate (decision 1):
    /// one isolate serving both would deadlock here, waiting for itself.
    evaluator: Option<Arc<dyn JsEvaluator>>,
    /// Where this run's statements go: a **workflow step's** transaction when the
    /// body is a step of one (§10.3, decision 6), so what a code body writes
    /// commits with the step or is rolled back with it, and the pooled path
    /// everywhere else.
    executor: rows::Executor,
    /// How many calls this run has made.
    calls: AtomicU32,
    /// Whether the caller's authority is the **ceiling** rather than something a
    /// plan may step down to ([`viewer_only`](TableHost::viewer_only)).
    viewer_only: bool,
    /// What a plan that names no authority runs under: the admin's for a
    /// trigger, the caller's for a body answering a request
    /// ([`defaulting_to`](TableHost::defaulting_to)).
    default_authority: Authority,
}

impl<'a> TableHost<'a> {
    /// A handle over `catalog`, with the default bounds and no caller.
    pub fn new(catalog: &'a Catalog) -> TableHost<'a> {
        TableHost {
            catalog,
            caller: Principal::anonymous(),
            limits: HostLimits::default(),
            evaluator: None,
            executor: rows::Executor::Pooled,
            calls: AtomicU32::new(0),
            viewer_only: false,
            default_authority: Authority::Admin,
        }
    }

    /// Run a plan that names no authority under `authority` instead of the
    /// admin's — what a body **answering a request** is given (a custom query, a
    /// server-rendered page), with [`Authority::User`], so the caller's
    /// ownership rules hold unless the body says `asAdmin()`.
    #[must_use]
    pub fn defaulting_to(mut self, authority: Authority) -> TableHost<'a> {
        self.default_authority = authority.resolve(&Authority::Admin);
        self
    }

    /// Run every statement of this host **inside `tx`** — what a workflow step
    /// hands a code body, so `db.orders.insert(…)` in a step lands where the
    /// step lands (§10.3, decision 6).
    #[must_use]
    pub fn in_transaction(mut self, tx: Option<SharedTx>) -> TableHost<'a> {
        self.executor = rows::Executor::of(tx);
        self
    }

    /// Attach the event's caller: the role it was served at and the user's own
    /// fields, exactly as `Event::caller` carries them.
    ///
    /// Both, rather than the user alone, because the two answer different
    /// questions: the role is what a delegated operation's floor is checked
    /// against, and the fields are what an ownership formula and an RLS policy
    /// read. An event with no caller keeps the public role, which is what
    /// `asUser()` in a scheduled trigger honestly means.
    #[must_use]
    pub fn caused_by(mut self, role: Role, user: Option<Json>) -> TableHost<'a> {
        let chain = self.caller.chain().to_vec();
        self.caller = Principal::recorded(role, user).chained(chain);
        self
    }

    /// Attach the event's caller as a principal — its role, its user and the
    /// chain that led here (`ActionContext::caller`): [`caused_by`] and
    /// [`chained`] in one.
    ///
    /// [`caused_by`]: TableHost::caused_by
    /// [`chained`]: TableHost::chained
    #[must_use]
    pub fn for_caller(mut self, caller: &Principal) -> TableHost<'a> {
        self.caller = Principal::recorded(caller.role(), caller.user().cloned())
            .chained(caller.chain().to_vec());
        self
    }

    /// Attach an administrator's authority as the caller: what a module's
    /// webhook and service run as (TODO-type-hardening H3.6). The proof is a
    /// named [`AdminPrincipal`], listed in the system-authority inventory,
    /// rather than an admin role written into `caused_by`.
    #[must_use]
    pub fn for_admin(self, admin: &AdminPrincipal) -> TableHost<'a> {
        self.for_caller(admin.principal())
    }

    /// Make the caller's authority the only one a plan runs under — what a
    /// **view** is rendered with (TODO "Saltcorn UI" §11, 7.2).
    ///
    /// A code body runs as admin and may only narrow, which is why a plan that
    /// says nothing is the admin's. A view is the other way round: it is somebody
    /// looking, and a v1 pattern (or a plugin's) that leaves `forUser` off a read
    /// must not be handed the rows the viewer may not see. So here a plan that
    /// says nothing — or says `admin` — runs as the caller, and `forUser` naming
    /// anybody but the caller is refused by name: in a view, naming another user
    /// is not narrowing, it is borrowing their rows.
    #[must_use]
    pub fn viewer_only(mut self) -> TableHost<'a> {
        self.viewer_only = true;
        self
    }

    /// Give delegated operations the formula engine, for an ownership formula the
    /// translator cannot lower.
    #[must_use]
    pub fn with_evaluator(mut self, evaluator: Option<Arc<dyn JsEvaluator>>) -> TableHost<'a> {
        self.evaluator = evaluator;
        self
    }

    /// Attach the trigger chain that led here.
    #[must_use]
    pub fn chained(mut self, chain: Vec<String>) -> TableHost<'a> {
        self.caller = self.caller.chained(chain);
        self
    }

    /// Set the bounds, in place of [`HostLimits::default`].
    #[must_use]
    pub fn with_limits(mut self, limits: HostLimits) -> TableHost<'a> {
        self.limits = limits;
        self
    }

    /// Answer one plan.
    async fn run(&self, plan: &Plan) -> Result<Json> {
        // Resolved once per plan, and before anything is looked up: the authority
        // decides the role every name in the plan is resolved *at*, so a
        // delegated read of a table the caller may not reach through a key is
        // refused by the same guard a REST embed is.
        let actor = self.actor(&plan.authority).await?;
        if plan.render && !matches!(plan.op, Op::Select) {
            return Err(Error::invalid(format!(
                "the statement of a `{}` of `{}` is not something to ask for: only a read \
                 renders, and only because Saltcorn 1's `getJoinedQuery` answers one",
                match plan.op {
                    Op::Aggregate => "grouped read",
                    Op::Insert => "insert",
                    Op::Update => "update",
                    _ => "delete",
                },
                plan.table
            )));
        }
        match plan.op {
            Op::Select if plan.render => self.render(plan, &actor).await,
            Op::Select => self.select(plan, &actor).await,
            Op::Aggregate => self.aggregate(plan, &actor).await,
            Op::Insert => self.insert(plan, &actor).await,
            Op::Update | Op::Delete => self.write(plan, &actor).await,
        }
    }

    /// The **statement** a read would run, as `{ sql, values }` — v1's
    /// `getJoinedQuery` (TODO "the v1 `Table` API" §3.5).
    ///
    /// The same plan `select` resolves, lowered by the same code, and then
    /// rendered rather than run: what comes back is the text and the binds, in
    /// v1's own shape. **This server will not run it** — there is no v1 `db`
    /// module in this version — so it is for the plugin that inspects or logs
    /// its query, and the tutorial says so where the method is documented.
    ///
    /// Delegation is where it gets interesting, and where it is narrower than a
    /// `select`. A rendered statement is a statement: it cannot decide row by
    /// row, so the caller's read rule has to be expressible *inside* it. That is
    /// exactly the question [`ownership::aggregate_guard`] answers, so it is
    /// asked here rather than answered a second time — the floor, a translatable
    /// ownership formula ANDed into the `WHERE`, or the database's own policies.
    /// A caller the rule does not admit at all gets v1's own answer,
    /// `{ notAuthorized: true }`; a formula only the evaluator can decide is a
    /// refusal naming it, because a statement that quietly dropped it would hand
    /// somebody SQL that reads rows they may not.
    async fn render(&self, plan: &Plan, actor: &Actor) -> Result<Json> {
        let read = plan::read(self.catalog, plan, &self.limits, actor.role())?;
        if read.table.provider().is_some() {
            return Err(Error::invalid(format!(
                "`{}` is served by a table provider, so there is no SQL statement to \
                 answer with: its rows come from a module",
                read.table.name
            )));
        }
        let mut query = read.query;
        if let Actor::Caller { principal, user } = actor {
            match ownership::aggregate_guard(
                self.catalog,
                &read.table,
                &read.table.name,
                principal.role(),
                user.as_ref(),
            ) {
                Ok(ownership::AggregateGuard::Predicate(Some(pred))) => {
                    query = query.and_filter(pred);
                }
                // Nothing to add: the caller is at or above the floor, or the
                // table's rule is the database's own policies — which apply when
                // the statement runs, wherever that turns out to be.
                Ok(_) => {}
                Err(e) if matches!(e.repr(), Repr::Auth(_)) => {
                    return Ok(serde_json::json!({ "notAuthorized": true }));
                }
                Err(e) => {
                    return Err(Error::invalid(format!(
                        "the statement of a read of `{}` cannot be rendered for this user, \
                         because a statement decides once and this rule decides row by row: \
                         {e}",
                        read.table.name
                    )));
                }
            }
        }
        let select = rows::read_select(self.catalog, &read.table, &query)?;
        let driver = self.catalog.driver_info(&read.table.database)?;
        let (sql, binds) = driver
            .dialect()
            .render(&sc_query::Statement::Select(Box::new(select)))?;
        let mut out = serde_json::Map::with_capacity(2);
        out.insert("sql".to_owned(), Json::String(sql));
        out.insert(
            "values".to_owned(),
            Json::Array(binds.iter().map(value_to_json).collect()),
        );
        Ok(Json::Object(out))
    }

    /// Whose authority this plan runs under, as the value the operations dispatch
    /// on. Delegation reads the event's caller back as an [`sc_auth::User`] here,
    /// so a caller object that is not one is refused before any statement runs.
    ///
    /// A **named** user is the one form with I/O behind it — v1's `forUser` and
    /// its `user` argument, which are frequently a user the plugin looked up and
    /// not the event's caller at all. It is loaded here, once per plan and
    /// before anything is resolved, and what comes out of it is the very same
    /// [`Actor::Caller`] `asUser()` produces: one role, one user, and
    /// §7.3's rule applied by the same `*_as` functions. A user who does not
    /// exist is an error naming the id, never a silent fall back to admin —
    /// which is the one mistake here that would matter, because the caller asked
    /// to be treated as somebody *smaller*.
    async fn actor(&self, authority: &Authority) -> Result<Actor> {
        if self.viewer_only {
            return self.viewer_actor(authority);
        }
        match authority.resolve(&self.default_authority) {
            // `Default` cannot survive the resolve: the host's own default is
            // never `Default` (`defaulting_to` resolves it on the way in).
            Authority::Default | Authority::Admin => Ok(Actor::Admin(self.admin())),
            Authority::User => Ok(self.delegated(
                self.caller.role(),
                User::from_json(self.caller.role(), self.caller.user())?,
            )),
            // The public role with nobody in it, which the seam can already
            // express and which no lookup can improve on.
            Authority::Public => Ok(self.delegated(Role::PUBLIC, None)),
            Authority::Named(id) => {
                let user = load_named_user(self.catalog, &id).await?;
                Ok(self.delegated(user.role(), Some(user)))
            }
        }
    }

    /// [`actor`](TableHost::actor) under [`viewer_only`](TableHost::viewer_only):
    /// every authority is the caller's, or the public's, or refused.
    ///
    /// `forUser` naming the caller is the caller — v1's patterns pass
    /// `forUser: req.user` on every read, and that must keep working — and is
    /// answered without a lookup, since the caller is already here.
    fn viewer_actor(&self, authority: &Authority) -> Result<Actor> {
        let caller = || -> Result<Actor> {
            Ok(self.delegated(
                self.caller.role(),
                User::from_json(self.caller.role(), self.caller.user())?,
            ))
        };
        match authority {
            Authority::Default | Authority::Admin | Authority::User => caller(),
            Authority::Public => Ok(self.delegated(Role::PUBLIC, None)),
            Authority::Named(id) => {
                let named = match id {
                    Json::String(text) => Some(text.as_str()),
                    _ => None,
                };
                let own = self
                    .caller
                    .user()
                    .as_ref()
                    .and_then(|u| u.get("id"))
                    .and_then(Json::as_str);
                match (named, own) {
                    (Some(named), Some(own)) if named == own => caller(),
                    _ => Err(Error::invalid(format!(
                        "a view reads and writes as the person looking at it, and `{id}` is not \
                         them: `forUser` (or a write's user argument) may name only the viewer"
                    ))),
                }
            }
        }
    }

    /// A read row's secrets as this host hands them out (TODO-imap M2): their
    /// plaintext to a code body, a module action or a service, which are
    /// server-side code; their sentinel to a view, which is rendered for
    /// somebody looking.
    fn secrets_of(
        &self,
        table: &Table,
        row: &mut std::collections::BTreeMap<String, sc_query::Value>,
    ) -> Result<()> {
        if self.viewer_only {
            sc_catalog::secrets::redact_values(table, row);
            Ok(())
        } else {
            sc_catalog::secrets::reveal_values(self.catalog, table, row)
        }
    }

    /// A `select`: the rows, as the REST wire shape.
    ///
    /// One batch of an `.iter()` is the same read with two differences, both of
    /// them in [`plan::read`]: it sorts by a **total** order, and it answers
    /// `{ rows, cursor }` so the guest knows where the next batch starts and
    /// whether there is one.
    async fn select(&self, plan: &Plan, actor: &Actor) -> Result<Json> {
        let read = plan::read(self.catalog, plan, &self.limits, actor.role())?;
        if read.cursor.is_some()
            && let Actor::Caller { principal, user } = actor
        {
            self.streamable(&read.table, principal.role(), user.as_ref())?;
        }
        // For a delegated read, §7.3's rule, in the one place it lives: the
        // formula narrows the rows in the `WHERE` where it can and row by row
        // where it cannot, and the bound is applied after it either way.
        let values = self
            .read_scope(actor)
            .read_values(&read.table, &read.query)
            .await?;
        // The cap is enforced on what came back rather than by trimming it: a
        // body handed 1000 of 4000 rows would go on to compute a wrong answer out
        // of a right-looking one, and never know.
        self.within_cap(
            values.len(),
            &format!("reading `{}` returned", read.table.name),
            "add a `.limit()` or narrow the `.where()`",
        )?;
        let mut values = values;
        for row in &mut values {
            self.secrets_of(&read.table, row)?;
        }
        let rows = Json::Array(values.iter().map(|v| read.row(v)).collect());
        let Some(cursor) = &read.cursor else {
            return Ok(rows);
        };
        let mut out = serde_json::Map::with_capacity(2);
        out.insert("rows".to_owned(), rows);
        out.insert("cursor".to_owned(), cursor.next(&values));
        Ok(Json::Object(out))
    }

    /// An `aggregate`: one object keyed by the aliases the plan asked for, or —
    /// when the plan grouped — one such object **per group**, with the group keys
    /// beside the values.
    ///
    /// The two are one path (§phase 6): `.count()` is
    /// `.aggregate({ value: "count()" })` with nothing to group by, so the filter,
    /// the caller context and §5's refusal are decided once rather than twice.
    /// Only the shape of the answer differs, and it differs because the questions
    /// do: a scalar terminal answers a value, and a grouped one answers rows.
    async fn aggregate(&self, plan: &Plan, actor: &Actor) -> Result<Json> {
        let agg = plan::aggregate(self.catalog, plan, &self.limits, actor.role())?;
        if let Actor::Caller { principal, user } = actor {
            self.delegable_aggregate(&agg.table, principal.role(), user.as_ref())?;
        }
        let values = self
            .scope(actor)
            .aggregate_grouped(&agg.table, agg.projections, &agg.query)
            .await?;
        let group = |values: &std::collections::BTreeMap<String, sc_query::Value>| {
            let mut out = serde_json::Map::with_capacity(agg.keys.len());
            for key in &agg.keys {
                out.insert(
                    key.clone(),
                    values.get(key).map_or(Json::Null, value_to_json),
                );
            }
            Json::Object(out)
        };
        if !agg.grouped {
            // A scalar aggregate is one row by construction; an empty answer is
            // the database saying something else, and reads back as nulls rather
            // than as invented zeroes.
            return Ok(group(&values.first().cloned().unwrap_or_default()));
        }
        // A grouped aggregate answers rows, so it is bounded like a read: a body
        // that grouped a million-row table by a free-text column would otherwise
        // materialise a million groups into the isolate.
        self.within_cap(
            values.len(),
            &format!("grouping `{}` produced", agg.table.name),
            "add a `.limit()` or narrow the `.where()`",
        )?;
        Ok(Json::Array(values.iter().map(group).collect()))
    }

    /// A `db.sql(…)`: the body's own SQL, and its rows as JSON objects keyed by
    /// the column names the database reported.
    ///
    /// **The one thing in `db` that is not resolved through the catalog**, and
    /// the module documentation's paragraph on it says what that costs. What is
    /// still true here is the part that does not depend on knowing the tables:
    /// the arguments are **binds**, so a value that spells SQL is a value; the
    /// row cap applies, because these rows are materialised into the isolate
    /// exactly as a `.rows()`'s are; the call is one of the run's budget; and the
    /// statement runs inside the same caller-context transaction every other
    /// operation does, so an RLS-protected table's policies still decide what it
    /// can see.
    ///
    /// What `asUser()` means here is therefore **narrower than it is anywhere
    /// else**, and narrow in a way worth saying out loud: it runs the statement
    /// at the caller's role and user, which is exactly what row-level security
    /// reads — and nothing more. An ownership *formula* is applied by the row
    /// layer, and raw SQL does not go through the row layer, so it does not
    /// filter this. A delegated `db.sql()` over a table that is owned by formula
    /// rather than by RLS sees the whole table; the body that wants §7.3's rule
    /// wants the chain, which is why the chain is the default surface and this is
    /// the escape hatch.
    async fn sql(&self, plan: &SqlPlan) -> Result<Json> {
        let statement = plan::statement(self.catalog, plan)?;
        let actor = self.actor(&plan.authority).await?;
        // The body's own statement, at the actor's role and user: the admin's
        // (who meets every floor) or, delegated, vouched for by the rule this
        // escape hatch is admitted on — admin-authored SQL whose only row rule
        // is the policies the caller context reaches.
        let access = match &actor {
            Actor::Admin(context) => sc_access::admin_access(context),
            Actor::Caller { principal, .. } => Unchecked::vouched(principal, CODE_SQL_RULE),
        };
        let rows = match &self.executor {
            // Inside a step's transaction the statement joins it, so a `db.sql()`
            // sees what the step has written and what it writes is undone with
            // the step. The GUCs are applied per statement there, which is what
            // keeps `asUser()` meaning to raw SQL exactly what it means to the
            // chain (§10.3, decision 6).
            rows::Executor::Transaction(tx) => tx.run(access, &statement).await?,
            rows::Executor::Pooled => {
                sc_catalog::run_in_context(self.catalog, access, &statement).await?
            }
        };
        self.within_cap(
            rows.len(),
            "this `db.sql()` returned",
            "add a `LIMIT` to the statement",
        )?;
        Ok(crate::rest::custom::rows_to_json(&rows))
    }

    /// An `insert`: the written row, or an array of them for an array in.
    ///
    /// Through [`rows::create_row`], which is the whole point — a code body's
    /// write is coerced against its columns, validated, File-field-checked and
    /// **observed by triggers** exactly as a write through the API is. It carries
    /// this run's caller, so the event it raises says who caused it and how deep
    /// in a cascade it already is.
    ///
    /// Delegated, the same write goes through [`ownership::insert_row_as`] —
    /// §7.3's write rule, the proposed row checked against the ownership formula
    /// with `_insert` folded true — and then through the very same row layer, so a
    /// delegated write is an event exactly as an admin's is.
    async fn insert(&self, plan: &Plan, actor: &Actor) -> Result<Json> {
        let insertion = plan::insert(self.catalog, plan, &self.limits)?;
        let scope = self.scope(actor);
        let mut written = Vec::with_capacity(insertion.rows.len());
        for row in &insertion.rows {
            written.push(scope.insert(&insertion.table, row).await?);
        }
        for row in &mut written {
            if self.viewer_only {
                sc_catalog::secrets::redact_json(&insertion.table, row);
            } else {
                sc_catalog::secrets::reveal_json(self.catalog, &insertion.table, row)?;
            }
        }
        Ok(match insertion.many {
            true => Json::Array(written),
            false => written.into_iter().next().unwrap_or(Json::Null),
        })
    }

    /// An `update` or a `delete`: `{ updated | deleted, ids }`, the shape the
    /// `update_rows` and `delete_rows` actions already answer.
    ///
    /// The matched rows are resolved **first** and then written one at a time
    /// through the row layer, exactly as those actions do — the events are the
    /// point (§4). No transaction spans the two (§6): a body that fails half way
    /// leaves the rows it already wrote, and their events have already gone out.
    ///
    /// Delegated, **the rows are resolved through the delegated read** and each
    /// one written through [`ownership::update_row_as`] /
    /// [`ownership::delete_row_as`]. Both halves matter: a row the caller cannot
    /// see must never be a row they can reach by predicate, and an update must be
    /// granted on the row as it is *and* on the row as it would become.
    async fn write(&self, plan: &Plan, actor: &Actor) -> Result<Json> {
        let write = plan::write(self.catalog, plan, &self.limits, actor.role())?;
        let table = &write.matched.table;
        let values = self
            .read_scope(actor)
            .read_values(table, &write.matched.query)
            .await?;
        let verb = match plan.op {
            Op::Update => "update",
            _ => "delete",
        };
        self.within_cap(
            values.len(),
            &format!("this {verb} of `{}` matched", table.name),
            "add a `.limit()` or narrow the `.where()`",
        )?;

        let scope = self.scope(actor);
        let mut ids = Vec::with_capacity(values.len());
        for row in &values {
            let (id, key) = rows::row_key(table, &write.pk, row)?;
            match &write.values {
                Some(assignments) => {
                    scope.update(table, &id, assignments).await?;
                }
                None => {
                    scope.delete(table, &id).await?;
                }
            }
            ids.push(key);
        }
        let count = Json::from(ids.len());
        let mut out = serde_json::Map::with_capacity(2);
        out.insert(
            match plan.op {
                Op::Update => "updated".to_owned(),
                _ => "deleted".to_owned(),
            },
            count,
        );
        out.insert("ids".to_owned(), Json::Array(ids));
        Ok(Json::Object(out))
    }

    /// The row cap, asserted on rows that are already in hand — a read's answer,
    /// the rows a bulk write matched, or what the body's own SQL returned.
    ///
    /// Both are refusals rather than truncations, and for the same reason: a body
    /// handed 1000 of 4000 rows computes a wrong answer out of a right-looking
    /// one, and a body that updated 1000 of 4000 rows would report having done
    /// what it was asked.
    fn within_cap(&self, rows: usize, clause: &str, fix: &str) -> Result<()> {
        if rows as u64 > self.limits.max_rows {
            return Err(Error::invalid(format!(
                "{clause} more than the {} rows a code body may take at once; {fix}",
                self.limits.max_rows
            )));
        }
        Ok(())
    }

    /// The caller every statement of this run carries: **admin, in the event's
    /// user's name**, with the trigger chain that led here.
    fn admin(&self) -> AdminPrincipal {
        AdminPrincipal::vouched(self.caller.user().cloned(), CODE_ADMIN_RULE)
            .chained(self.caller.chain().to_vec())
    }

    /// A delegated actor: `user` at `role`, as §7.3's rule takes them, with the
    /// chain that led here — the principal the guarded functions always built
    /// (`ownership::principal_at`), and the user as it was loaded.
    fn delegated(&self, role: Role, user: Option<User>) -> Actor {
        Actor::Caller {
            principal: ownership::principal_at(role, user.as_ref())
                .chained(self.caller.chain().to_vec()),
            user,
        }
    }

    /// The handle `actor`'s operations run through, on this run's executor and
    /// with its formula engine (TODO-code-quality Q3.4): the row layer for the
    /// admin, the ownership rule for a delegated caller.
    fn scope<'s>(&'s self, actor: &'s Actor) -> Scoped<'s> {
        let scoped = match actor {
            Actor::Admin(context) => Scoped::new(self.catalog, ScopedAuthority::from(context)),
            Actor::Caller { principal, user } => {
                Scoped::new(self.catalog, ScopedAuthority::Caller(principal))
                    .with_user(user.as_ref())
            }
        };
        scoped
            .in_executor(self.executor.clone())
            .with_evaluator(self.evaluator.clone())
    }

    /// [`scope`](TableHost::scope) for a read: a delegated read goes through
    /// the pool, as it always has (the ownership functions' reads were never
    /// handed the step's transaction); the admin's reads join it.
    fn read_scope<'s>(&'s self, actor: &'s Actor) -> Scoped<'s> {
        match actor {
            Actor::Admin(_) => self.scope(actor),
            Actor::Caller { .. } => self.scope(actor).in_executor(rows::Executor::Pooled),
        }
    }

    /// §5's one asymmetry, said in the code body's own terms.
    ///
    /// A delegated aggregate over a table whose ownership formula the translator
    /// refuses cannot be answered: [`ownership::aggregate_guard`] will not fall
    /// back to the evaluator, because an aggregate over rows it cannot filter
    /// would silently count rows the caller may not see, and a plausible number
    /// hides that where a refusal cannot. The guard is asked here — ahead of
    /// [`ownership::aggregate_values_as`], which asks it again and would answer in
    /// its own words — for one reason: from here the message can name the way out,
    /// and in a code body there is one.
    ///
    /// A caller who may not read the table at all gets that answer unchanged: it
    /// is an `auth` refusal, the same one a read gets, and reading the rows
    /// instead would not help them.
    fn delegable_aggregate(&self, table: &Table, role: Role, user: Option<&User>) -> Result<()> {
        match ownership::aggregate_guard(self.catalog, table, &table.name, role, user) {
            Ok(_) => Ok(()),
            Err(e) => Err(match e.repr() {
                Repr::Invalid(message) => Error::invalid(format!(
                    "{message}. Read the rows with `.rows()` and aggregate them in your code \
                     body instead, which can decide row by row"
                )),
                _ => e,
            }),
        }
    }

    /// Whether a **delegated** read of `table` can be streamed — §5's asymmetry
    /// again, in the one other place it bites.
    ///
    /// Streaming a read means the database bounding it: each batch is a `LIMIT`
    /// over the rows the caller may see, resumed from where the last one
    /// stopped. That works when the ownership rule reaches the statement — RLS
    /// policies do, a translatable formula does — and cannot when the rule is a
    /// formula only the evaluator can decide, because then the bound is applied
    /// *after* the rows come back ([`ownership::read_row_values_as`] says so, and
    /// says why). A batch of 1000 would fetch the whole table, hand back
    /// whatever survived the formula, and do it again for the next batch: not
    /// streaming, and quadratic while pretending otherwise.
    ///
    /// [`ownership::aggregate_guard`] is exactly this question already asked —
    /// "can this caller's read rule be carried inside the statement?" — so it is
    /// the thing asked, rather than a second copy of the rule to be subtly
    /// wrong. Only the wording is this surface's own: an aggregate's way out is
    /// not a stream's.
    fn streamable(&self, table: &Table, role: Role, user: Option<&User>) -> Result<()> {
        match ownership::aggregate_guard(self.catalog, table, &table.name, role, user) {
            Ok(_) => Ok(()),
            Err(e) => Err(match e.repr() {
                Repr::Invalid(_) => Error::invalid(format!(
                    "`{}` cannot be streamed as this user: its ownership formula has to be \
                     decided row by row, so a batch of it cannot be bounded by the database. \
                     Read it with `.rows()` and a `.limit()`, which can decide row by row",
                    table.name
                )),
                _ => e,
            }),
        }
    }

    /// Spend one of the run's database calls.
    ///
    /// The runtime counts these too, and both counts are wanted: the guest's
    /// stops the loop before the plan is built, and this one is the number that
    /// is true — a host shared or a prelude tampered with cannot lower it.
    fn spend_call(&self) -> Result<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.limits.max_calls {
            return Err(Error::invalid(format!(
                "this code made more than {} database calls in one run; the bound exists so \
                 an accidental loop cannot hammer the database",
                self.limits.max_calls
            )));
        }
        Ok(())
    }
}

/// The user a `{ user: id }` authority names, loaded.
///
/// **Where users live and nowhere else**: a caller that named somebody is
/// asking to be treated as a row of the users table, so the row is read and its
/// own role is what the floor is then checked against — not the role the event
/// was served at, which is somebody else's.
///
/// Two refusals, both by name. A user id here is a uuid, because that is what a
/// user is identified by on this server (§7.1) — Saltcorn 1's integer ids have
/// no counterpart, and a plugin handing one over is told so rather than left
/// with an empty answer. And a uuid that names nobody is an error, never an
/// unowned read: the request was to be treated as somebody smaller, and
/// answering it as admin would be the one wrong answer that matters.
pub(crate) async fn load_named_user(catalog: &Catalog, id: &Json) -> Result<User> {
    let Json::String(text) = id else {
        return Err(Error::invalid(format!(
            "`{id}` is not a user id: a user is identified by a uuid on this server, so \
             pass the user row or its `id`"
        )));
    };
    let uuid = text.parse::<uuid::Uuid>().map_err(|_| {
        Error::invalid(format!(
            "`{text}` is not a user id: a user is identified by a uuid on this server"
        ))
    })?;
    sc_auth::load_user(catalog, uuid).await?.ok_or_else(|| {
        Error::invalid(format!(
            "there is no user with id `{text}`, so there is nobody to run as"
        ))
    })
}

/// Whose authority one plan runs under, resolved (§5).
///
/// Two shapes and only two, which is what makes every operation's dispatch a
/// two-armed match rather than a flag threaded through it: the **trigger's own**
/// authority, already built as the caller context every statement of an
/// undelegated run carries, and the **event's caller**, as the role and user
/// §7.3's rule takes.
enum Actor {
    /// Admin, in the event's user's name, with the chain that led here.
    Admin(AdminPrincipal),
    /// The event's caller (or the public role, or a named user). `user` is
    /// `None` for an event that carries nobody — a scheduled or startup
    /// trigger — which then reads as the public role.
    Caller {
        /// The role every floor is checked against, the user's fields an RLS
        /// policy reads, and the chain a write's event carries.
        principal: Principal,
        /// The user an ownership formula reads, as it was loaded.
        user: Option<User>,
    },
}

impl Actor {
    /// The role every name in the plan is resolved at — a Ⱶ-path's hops, a
    /// Ↄ-aggregation's child table — as well as every row rule.
    fn role(&self) -> Role {
        match self {
            Actor::Admin(_) => Role::ADMIN,
            Actor::Caller { principal, .. } => principal.role(),
        }
    }
}

/// What stands behind a code body's admin actor (an undelegated plan, or
/// `asAdmin()`): a code body is server-side configuration an administrator
/// wrote, and runs as admin in its event's user's name unless it narrows
/// (§10.3). Listed in the system-authority inventory.
pub(crate) const CODE_ADMIN_RULE: &str = "a code body's admin actor (admin-authored configuration)";

/// What stands behind a delegated `db.sql()`'s unchecked statement: the SQL is
/// the body's own — server-side configuration an administrator wrote, the
/// rule custom SQL queries are admitted on (§13.4) — and it runs in the
/// caller's context, so an RLS table's policies decide what it reaches.
const CODE_SQL_RULE: &str = "a code body's admin-authored db.sql(), in its caller's context";

#[async_trait]
impl CodeHost for TableHost<'_> {
    async fn call(&self, request: Json) -> Result<Json> {
        self.spend_call()?;
        let refuse = |e: serde_json::Error| {
            Error::invalid(format!(
                "this database request is not one the server understands: {e}"
            ))
        };
        // Two shapes cross this seam, and the `op` says which before either is
        // read: a plan over a table, and the body's own SQL — which shares
        // nothing with a plan but the authority. Told apart here rather than by
        // an untagged enum, so a malformed request is refused in the words of the
        // shape it was trying to be.
        if request.get("op").and_then(Json::as_str) == Some("sql") {
            let plan: SqlPlan = serde_json::from_value(request).map_err(refuse)?;
            return self.sql(&plan).await;
        }
        // A model handle's requests (milestone 31 §3): a fit resolved, a
        // prediction, a posterior's draws or its write-back — on the same
        // budget, and a write-back under this run's authority and chain.
        if request.get("op").and_then(Json::as_str) == Some("models") {
            return models::answer(self, request).await;
        }
        // Row history (§14.12): a row's versions read, a deleted row restored,
        // a row put back — through the same reader and reverser as the admin
        // API, at the plan's authority.
        if request.get("op").and_then(Json::as_str) == Some("history") {
            return history::answer(self, request).await;
        }
        let plan: Plan = serde_json::from_value(request).map_err(refuse)?;
        // The values this terminal presents, and no others: the plan's own
        // `.withRequest(…)`, or nothing (TODO-type-hardening H7.6).
        let presented = match &plan.request {
            Some(values) => sc_expr::RequestValues::new(values.iter())?,
            None => sc_expr::RequestValues::default(),
        };
        let origin = sc_catalog::Origin {
            request: presented,
            ..sc_catalog::current_origin()
        };
        sc_catalog::with_origin(origin, self.run(&plan)).await
    }
}

#[cfg(test)]
mod tests;
