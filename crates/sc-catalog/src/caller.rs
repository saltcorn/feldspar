//! Who is writing, and what led them here (§7.3, §10.2).
//!
//! A [`Principal`] travels with **every** row write, not only with the ones
//! on an RLS table. It started as the RLS half alone — the role and user an
//! `sc.role`/`sc.user` GUC carries into a policy — and grew the rest when table
//! events landed (Phase 4): an event has to say *who* caused the write, and the
//! row layer has no other way to know. One carrier rather than two, because a
//! second parameter threaded through the same eight entry points would be a
//! second thing to forget.
//!
//! So it answers three questions about one write:
//!
//! - **Whose authority?** [`role`](Principal::role) — what the policies gate
//!   on, and what an event reports.
//! - **Which user?** [`user`](Principal::user) — the fields the formula
//!   language binds `user` to, and the object the `sc.user` GUC carries.
//! - **What led here?** [`chain`](Principal::chain) — the trigger names, if
//!   any, whose actions caused this write. RLS ignores it; the dispatcher uses it
//!   to bound a cascade (`Event::firing`, §10.2's `MAX_DEPTH`). A write made by a
//!   request rather than by a trigger carries an empty chain, which is what makes
//!   depth 0 the ordinary case.
//! - **Which action?** [`changeset`](Principal::changeset) — the user-level
//!   action the write belongs to (§14.12), which the change-log trigger records
//!   it under. A trigger's write carries the changeset of the event that fired
//!   it, so the user's edit and its trigger's follow-up are one action.

use sc_types::Role;
use std::sync::Arc;

use serde_json::{Value as Json, json};
use uuid::Uuid;

/// The chain entry marking a cascade that began on **untrusted input**: a
/// write or a trigger run made by an agent whose prompt came from outside the
/// admin console — a triggered run, or an application's exposed chat
/// (security review #32).
///
/// It rides in the chain because the chain already travels with every write,
/// every event it raises, every action those fire and every workflow they
/// start, so nothing on the way can drop it. `run_agent` reads it: a
/// `trusted_input` trigger reached through such a cascade still starts a
/// triggered run, without the server's own tools, because the event it fired
/// on is one an untrusted prompt caused. It does not count towards the
/// cascade's depth; a trigger an admin gave this name would only mark its own
/// cascades untrusted, which takes tools away and grants nothing.
pub const UNTRUSTED_ORIGIN: &str = "(untrusted input)";

/// `chain` with the [`UNTRUSTED_ORIGIN`] mark at its head, once.
pub fn untrusted_chain(mut chain: Vec<String>) -> Vec<String> {
    if !is_untrusted_chain(&chain) {
        chain.insert(0, UNTRUSTED_ORIGIN.to_owned());
    }
    chain
}

/// Whether `chain` began on untrusted input ([`UNTRUSTED_ORIGIN`]).
pub fn is_untrusted_chain(chain: &[String]) -> bool {
    chain.iter().any(|name| name == UNTRUSTED_ORIGIN)
}

/// The application a request came through (TODO-payments P1.3, design C5): what
/// a checkout's success URL and a portal's return URL are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallingApp {
    /// The application's id.
    pub id: Uuid,
    /// Its public base URL as this request reached it — `https://shop.example.com`
    /// — with no trailing slash. An application served under two hosts reports
    /// the one this request used.
    pub url: String,
    /// The URL scheme its native apps open (`myshop`), when it has one.
    pub scheme: Option<String>,
}

impl CallingApp {
    /// The object a code body binds as `app`: `{ id, url, scheme }`.
    pub fn to_json(&self) -> Json {
        json!({ "id": self.id, "url": self.url, "scheme": self.scheme })
    }

    /// [`to_json`](CallingApp::to_json) read back — what a workflow run keeps
    /// of its event. `None` for anything that is not one.
    pub fn from_json(json: &Json) -> Option<CallingApp> {
        Some(CallingApp {
            id: json.get("id")?.as_str()?.parse().ok()?,
            url: json.get("url")?.as_str()?.to_owned(),
            scheme: json.get("scheme").and_then(Json::as_str).map(str::to_owned),
        })
    }
}

/// Where a request came from beyond who is signed in: the application it came
/// through and the **guest session** it carries (TODO-payments P1.3, P1.5).
///
/// The transport knows both, and the places that need them — an event's `app`,
/// an ownership formula's `session.id` — are many calls below it, past
/// signatures that carry only the user. So the transport runs the request
/// **inside** an origin ([`with_origin`]), and what builds a caller context or
/// an event reads it ([`current_origin`]) and carries it explicitly from there:
/// on [`Principal`], on the event, and back into scope by the dispatcher
/// while a trigger's action runs. Outside any request it is empty, which is
/// what a scheduled trigger, a service and a webhook all are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Origin {
    /// The application the request came through.
    pub app: Option<CallingApp>,
    /// The **public id** of the caller's guest session — what a formula sees
    /// as `session.id` and a table stores. Never the cookie's token.
    pub session: Option<String>,
}

tokio::task_local! {
    static ORIGIN: Origin;
}

/// The origin this task is running inside, or the empty one.
pub fn current_origin() -> Origin {
    ORIGIN.try_with(Clone::clone).unwrap_or_default()
}

/// Run `fut` inside `origin`.
pub async fn with_origin<F: std::future::Future>(origin: Origin, fut: F) -> F::Output {
    ORIGIN.scope(origin, fut).await
}

use crate::changeset::{self, Changeset};

/// The task-local context a caller's work runs inside: the request's [`Origin`],
/// the ambient changeset and the run outbox (code-quality review §3.3).
///
/// `tokio::spawn` silently drops every task-local, which is how security review
/// #32's marker came to be lost across a cascade. A [`Principal`] carries the
/// trigger chain, origin and changeset as values, so work handed one keeps
/// them; what still lives in task-locals is captured here and re-entered by
/// [`spawn_scoped`], which is what work done on a caller's behalf spawns with.
#[derive(Clone, Default)]
pub struct TaskScope {
    origin: Option<Origin>,
    changeset: Option<Arc<Changeset>>,
    outbox: Option<Arc<crate::outbox::RunOutbox>>,
}

impl TaskScope {
    /// The scope the running task is inside.
    pub fn current() -> TaskScope {
        TaskScope {
            origin: ORIGIN.try_with(Clone::clone).ok(),
            changeset: changeset::current(),
            outbox: crate::outbox::current(),
        }
    }

    /// Run `fut` inside this scope.
    pub async fn run<F: std::future::Future>(self, fut: F) -> F::Output {
        let fut = crate::outbox::scope_if(self.outbox, changeset::scope_if(self.changeset, fut));
        match self.origin {
            Some(origin) => with_origin(origin, fut).await,
            None => fut.await,
        }
    }
}

/// `tokio::spawn`, keeping the spawning task's [`TaskScope`]: its origin,
/// changeset and outbox reach the spawned task.
#[allow(
    clippy::disallowed_methods,
    reason = "the replacement clippy.toml names for tokio::spawn"
)]
pub fn spawn_scoped<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Boxed first: the scope wrappers would otherwise copy a large future (an
    // agent turn) around on the stack as it is moved into the task.
    tokio::spawn(TaskScope::current().run(Box::pin(fut)))
}

/// Who a row operation runs as: one caller, with everything that travels with
/// them — role, user, the trigger chain that led here, the application and guest
/// session of the request, and the changeset the write belongs to
/// (code-quality review §4.2). It replaced `CallerContext` and is what
/// [`Authority::Caller`] carries; the server acting on its own authority is
/// [`Authority::System`], not a `Principal`.
///
/// The fields are private and there is no `Default` (security review 2026-11
/// #15, TODO-type-hardening H3.1): a principal is made by one of the named
/// constructors, each greppable —
///
/// - [`anonymous`](Principal::anonymous): nobody, at the public role;
/// - [`of`](Principal::of): a signed-in user, from the [`VerifiedUser`] proof
///   sc-auth's `AuthenticatedUser` is;
/// - [`recorded`](Principal::recorded): a caller some layer above already
///   established and is handing on — the user a request resolved
///   (`sc_access::principal_of`), an event's cause, a stored run's caller;
/// - the system's, crate-private, reached through
///   [`Authority::as_principal`] and [`AdminPrincipal::system`].
///
/// An administrator's role in a principal is not enough to skip the row rule:
/// [`Unchecked::admin`] takes an [`AdminPrincipal`], which only an admin
/// session's proof or a named system reason makes.
///
/// A principal's role cannot be raised after it is made:
///
/// ```compile_fail
/// let mut caller = sc_catalog::Principal::anonymous();
/// caller.role = sc_types::Role::ADMIN;
/// ```
///
/// and one whose role says admin is not the proof:
///
/// ```compile_fail
/// let caller = sc_catalog::Principal::recorded(sc_types::Role::ADMIN, None);
/// let _ = sc_catalog::Unchecked::admin(&caller);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    /// The caller's role (public when anonymous).
    role: Role,
    /// The logged-in user's fields as a JSON object, or `None` when anonymous —
    /// in which case `sc.user` is left unset and the policies'
    /// `current_setting('sc.user', true)` reads `NULL`.
    user: Option<Json>,
    /// The trigger names whose actions led to this write, outermost first. Empty
    /// for a write a request made directly.
    chain: Vec<String>,
    /// The application the write's request came through, if any.
    app: Option<CallingApp>,
    /// The guest session's public id, if the caller has one — what the
    /// `sc.session` GUC carries.
    session: Option<String>,
    /// The changeset this write belongs to. `None` lets the change-log trigger
    /// mint one of its own (`source = 'sql'`), as for a write from outside.
    changeset: Option<Arc<Changeset>>,
}

/// A signed-in user whose credential was verified: what [`Principal::of`]
/// takes. sc-auth's `AuthenticatedUser` is the one implementation
/// (sc-catalog sits below sc-auth, so the proof is named by this trait);
/// `impl VerifiedUser for` is the inventory grep.
pub trait VerifiedUser {
    /// The role the user is served at.
    fn verified_role(&self) -> Role;
    /// The user's fields as the JSON object the `sc.user` GUC carries.
    fn verified_fields(&self) -> Json;
}

impl Principal {
    fn build(role: Role, user: Option<Json>) -> Principal {
        Principal {
            role,
            user,
            chain: Vec::new(),
            app: None,
            session: None,
            changeset: changeset::current(),
        }
    }

    /// Nobody: the public role with no user, in the ambient changeset.
    pub fn anonymous() -> Principal {
        Principal::build(Role::PUBLIC, None)
    }

    /// A signed-in user, at the role their proof carries, in the origin and
    /// changeset this task runs inside.
    pub fn of(user: &impl VerifiedUser) -> Principal {
        Principal::build(user.verified_role(), Some(user.verified_fields())).in_current_origin()
    }

    /// A caller a layer above has already established and hands on: `role` and
    /// the user's fields as the `sc.user` GUC carries them. What
    /// `sc_access::principal_of` builds a request's caller with, and what an
    /// event's cause and a stored run's caller are restored as.
    ///
    /// It decides what the row rule sees, never more: an administrator's role
    /// here meets every floor, as the rule says, but does not make an
    /// [`AdminPrincipal`], so it opens no unchecked layer.
    pub fn recorded(role: Role, user: Option<Json>) -> Principal {
        Principal::build(role, user)
    }

    /// The server's own principal: the admin role and no user.
    pub(crate) fn system() -> Principal {
        Principal::build(Role::ADMIN, None)
    }

    /// The caller's role (public when anonymous).
    pub fn role(&self) -> Role {
        self.role
    }

    /// The signed-in user's fields, or `None` when anonymous.
    pub fn user(&self) -> Option<&Json> {
        self.user.as_ref()
    }

    /// The trigger names whose actions led here, outermost first.
    pub fn chain(&self) -> &[String] {
        &self.chain
    }

    /// The application the request came through, if any.
    pub fn app(&self) -> Option<&CallingApp> {
        self.app.as_ref()
    }

    /// The guest session's public id, if the caller has one.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The changeset this write belongs to.
    pub fn changeset(&self) -> Option<&Arc<Changeset>> {
        self.changeset.as_ref()
    }

    /// Whether this caller's cascade began on untrusted input
    /// ([`UNTRUSTED_ORIGIN`]).
    pub fn is_untrusted(&self) -> bool {
        is_untrusted_chain(&self.chain)
    }

    /// The same caller served at no more privilege than `ceiling`: an
    /// application's maximum role. Lowering a role forges nothing.
    #[must_use]
    pub fn capped_at(mut self, ceiling: Role) -> Principal {
        if !ceiling.meets(self.role) {
            self.role = ceiling;
        }
        self
    }

    /// The same, from `origin`: the application and the guest session.
    pub fn from_origin(mut self, origin: Origin) -> Principal {
        self.app = origin.app;
        self.session = origin.session;
        self
    }

    /// The same, from the origin this task is running inside.
    pub fn in_current_origin(self) -> Principal {
        self.from_origin(current_origin())
    }

    /// The origin this context carries.
    pub fn origin(&self) -> Origin {
        Origin {
            app: self.app.clone(),
            session: self.session.clone(),
        }
    }

    /// The chain of triggers this write descends from — what
    /// `Event::firing` handed the action making it.
    pub fn chained(mut self, chain: Vec<String>) -> Principal {
        self.chain = chain;
        self
    }

    /// The same, marked as made on **untrusted input** ([`UNTRUSTED_ORIGIN`]).
    pub fn untrusted(mut self) -> Principal {
        self.chain = untrusted_chain(self.chain);
        self
    }

    /// Put this write in `changeset` instead of the one it was built with.
    pub fn in_changeset(mut self, changeset: Option<Arc<Changeset>>) -> Principal {
        self.changeset = changeset;
        self
    }

    /// What the `sc.changeset` GUC carries for this writer: empty when there
    /// is no changeset, so the trigger mints one.
    pub(crate) fn changeset_json(&self) -> String {
        self.changeset
            .as_ref()
            .map(|c| c.guc_value(self.user.as_ref()))
            .unwrap_or_default()
    }

    /// The user's fields as the JSON text the `sc.user` GUC carries.
    pub(crate) fn user_json(&self) -> Option<String> {
        self.user.as_ref().map(Json::to_string)
    }
}

/// A principal known to be an administrator's (security review 2026-11 #15,
/// TODO-type-hardening H3.1): what [`Unchecked::admin`] and
/// `sc_access::admin_access` take, so an admin role written into a
/// [`Principal`] opens no unchecked layer.
///
/// Two ways to make one, both greppable and listed in the system-authority
/// inventory (docs/SECURITY_REVIEW_2026-10.md):
///
/// - [`AdminPrincipal::system`]: the server's own authority, with a reason;
/// - [`AdminPrincipal::vouched`]: an administrator's authority a named check
///   stands behind — sc-server's `Admin` session proof, the code host's
///   `asAdmin()` handle, an action running as admin in its cause's name.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminPrincipal {
    principal: Principal,
    by: &'static str,
}

impl AdminPrincipal {
    /// The server's own authority, for `reason`: the admin role and no user.
    pub fn system(reason: &'static str) -> AdminPrincipal {
        AdminPrincipal {
            principal: Principal::system(),
            by: reason,
        }
    }

    /// An administrator's authority in `user`'s name, which the check named
    /// `by` stands behind. `user` is the fields the `sc.user` GUC carries
    /// (`None` for an admin tool run with no user), and the principal is in the
    /// origin and changeset this task runs inside.
    pub fn vouched(user: Option<Json>, by: &'static str) -> AdminPrincipal {
        AdminPrincipal {
            principal: Principal::build(Role::ADMIN, user).in_current_origin(),
            by,
        }
    }

    /// The principal: the admin role, and the user it acts in the name of.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The principal, without the proof.
    pub fn into_principal(self) -> Principal {
        self.principal
    }

    /// The check, or the system reason, that stands behind it.
    pub fn vouched_by(&self) -> &'static str {
        self.by
    }

    /// The same, carrying the trigger chain that led here.
    #[must_use]
    pub fn chained(mut self, chain: Vec<String>) -> AdminPrincipal {
        self.principal = self.principal.chained(chain);
        self
    }

    /// The same, in `changeset`.
    #[must_use]
    pub fn in_changeset(mut self, changeset: Option<Arc<Changeset>>) -> AdminPrincipal {
        self.principal = self.principal.in_changeset(changeset);
        self
    }

    /// The same, from `origin`.
    #[must_use]
    pub fn from_origin(mut self, origin: Origin) -> AdminPrincipal {
        self.principal = self.principal.from_origin(origin);
        self
    }
}

/// An administrator **at the admin console** (security review 2026-11 #16,
/// TODO-type-hardening H3.3): what the catalog's
/// [`AdminHost`](crate::AdminHost) and the administrative tools take.
///
/// Narrower than an [`AdminPrincipal`]: a code body or a trigger's action runs
/// as admin too, but its prompt or input came from whoever caused its event,
/// and that is not an administrator at the console. So there is no conversion
/// from an `AdminPrincipal`; the two ways to make one are greppable and listed
/// in the system-authority inventory:
///
/// - [`ConsoleAdmin::at_console`]: an administrator's console authority a
///   named check stands behind — sc-server's admin session, an administrator's
///   MCP token, an agent run started from the console (`RunCaller`);
/// - [`ConsoleAdmin::system`]: the server's own, with a reason (the operator's
///   CLI, a run an administrator marked as on trusted input).
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleAdmin {
    admin: AdminPrincipal,
}

impl ConsoleAdmin {
    /// An administrator at the console, in `user`'s name (the fields the
    /// `sc.user` GUC carries; `None` for a tool run with no user), which the
    /// check named `by` stands behind.
    pub fn at_console(user: Option<Json>, by: &'static str) -> ConsoleAdmin {
        ConsoleAdmin {
            admin: AdminPrincipal::vouched(user, by),
        }
    }

    /// The server's own console authority, for `reason`.
    pub fn system(reason: &'static str) -> ConsoleAdmin {
        ConsoleAdmin {
            admin: AdminPrincipal::system(reason),
        }
    }

    /// The administrator as the row layer's admin proof.
    pub fn principal(&self) -> &AdminPrincipal {
        &self.admin
    }

    /// The administrator's user id, or `None` for a run with no user.
    pub fn user_id(&self) -> Option<Uuid> {
        self.admin.user_id()?.parse().ok()
    }

    /// The check, or the system reason, that stands behind it.
    pub fn vouched_by(&self) -> &'static str {
        self.admin.vouched_by()
    }
}

impl std::ops::Deref for AdminPrincipal {
    type Target = Principal;

    fn deref(&self) -> &Principal {
        &self.principal
    }
}

/// Why code runs on the server's own authority: a short label written in the
/// source ("retention sweep", "model fit"). Only [`Authority::system`] makes one,
/// so every system-authority call site is a greppable `Authority::system(`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemReason(&'static str);

impl SystemReason {
    /// The label.
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// On whose authority a row operation runs (code-quality review §4.2).
///
/// It replaced `Option<&CallerContext>`, where `None` silently meant "nobody,
/// plus whatever changeset the task holds". Now the server's own authority has
/// to be written out, with a reason:
///
/// - [`System`](Authority::System): the server itself — the admin role and no
///   user. On a row-level-security table it sets `sc.role` to the admin's, so a
///   system job (a retention sweep, a model fit, an import an admin started)
///   sees and writes every row; the write belongs to the ambient changeset.
/// - [`Caller`](Authority::Caller): a [`Principal`], whose role, user, origin
///   and changeset decide.
///
/// The system call sites are listed in docs/SECURITY_REVIEW_2026-10.md
/// ("System-authority inventory"); keep that list current.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Authority<'a> {
    /// The server's own authority.
    System(SystemReason),
    /// A caller's.
    Caller(&'a Principal),
}

impl<'a> Authority<'a> {
    /// The server's own authority, for `reason`.
    pub const fn system(reason: &'static str) -> Authority<'static> {
        Authority::System(SystemReason(reason))
    }

    /// `principal`'s authority.
    pub const fn caller(principal: &'a Principal) -> Authority<'a> {
        Authority::Caller(principal)
    }

    /// The caller, or `None` for the system.
    pub fn principal(self) -> Option<&'a Principal> {
        match self {
            Authority::System(_) => None,
            Authority::Caller(principal) => Some(principal),
        }
    }

    /// Whether this is the server's own authority.
    pub fn is_system(self) -> bool {
        matches!(self, Authority::System(_))
    }

    /// The role access is checked at: the admin's for the system.
    pub fn role(self) -> Role {
        match self {
            Authority::System(_) => Role::ADMIN,
            Authority::Caller(principal) => principal.role,
        }
    }

    /// The system's reason, or `None` for a caller.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Authority::System(reason) => Some(reason.as_str()),
            Authority::Caller(_) => None,
        }
    }

    /// The principal the database's caller GUCs are set from: the caller, or for
    /// the system the admin role with no user, in the ambient changeset.
    pub fn as_principal(self) -> std::borrow::Cow<'a, Principal> {
        match self {
            Authority::Caller(principal) => std::borrow::Cow::Borrowed(principal),
            Authority::System(_) => std::borrow::Cow::Owned(Principal::system()),
        }
    }
}

impl<'a> Authority<'a> {
    /// This authority as an [`Unchecked`] token: the system's, or an
    /// administrator's (who meets every floor, so skipping the check decides
    /// nothing); `None` for anybody else, who goes through the ownership rule.
    pub fn unchecked(self) -> Option<Unchecked<'a>> {
        match self {
            Authority::System(_) => Some(Unchecked {
                who: self,
                by: None,
            }),
            // An administrator meets every floor, which is when the rule
            // consults nothing else: this is the rule's own decision.
            Authority::Caller(principal) => principal.role.is_admin().then_some(Unchecked {
                who: self,
                by: Some(ADMIN_MEETS_EVERY_FLOOR),
            }),
        }
    }
}

/// What [`Authority::unchecked`] vouches an administrator's principal with: the
/// rule itself, under which a caller at the admin role meets every floor and
/// nothing else is consulted.
pub const ADMIN_MEETS_EVERY_FLOOR: &str = "the row rule: an administrator meets every floor";

/// Permission to read or write rows **without** a table's role floors and
/// ownership formula (code-quality review §4.4, TODO-code-quality Q3.5).
///
/// The row layer's unchecked operations (`sc_access::rows`), a table's provider
/// ([`Catalog::provider`](crate::Catalog::provider)) and a database's driver
/// ([`Catalog::driver_for`](crate::Catalog::driver_for),
/// [`driver_named`](crate::Catalog::driver_named)) take one, so a caller that
/// holds only a `&Catalog` and a [`Principal`] cannot reach rows past the rule:
/// it goes through `sc_access::Scoped`, or it writes down why it may skip it.
/// There are three ways to make one, each greppable:
///
/// - [`Unchecked::system`]: the server's own authority, with a reason. Listed in
///   docs/SECURITY_REVIEW_2026-10.md's system-authority inventory.
/// - [`Unchecked::admin`]: an [`AdminPrincipal`], which only an admin session's
///   proof or a named system reason makes. An administrator meets every role
///   floor, which is when the ownership rule is not consulted, so unchecked and
///   checked access are the same.
/// - [`Unchecked::vouched`]: a caller whose access a named check already
///   decided (the REST provider's own rule, the ownership functions themselves).
///   Listed in the same inventory, with the check that stands behind it.
///
/// It still carries the authority: the role and user an RLS table's policies
/// read, and the caller the write's event reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Unchecked<'a> {
    who: Authority<'a>,
    /// The check that stood behind a [`vouched`](Unchecked::vouched) caller.
    by: Option<&'static str>,
}

impl Unchecked<'static> {
    /// The server's own authority, for `reason` — [`Authority::system`]'s
    /// unchecked form.
    pub const fn system(reason: &'static str) -> Unchecked<'static> {
        Unchecked {
            who: Authority::system(reason),
            by: None,
        }
    }
}

impl<'a> Unchecked<'a> {
    /// An administrator's unchecked access. Infallible: the
    /// [`AdminPrincipal`] is the proof.
    pub fn admin(admin: &'a AdminPrincipal) -> Unchecked<'a> {
        Unchecked {
            who: Authority::Caller(admin.principal()),
            by: Some(admin.vouched_by()),
        }
    }

    /// `principal`'s access, which the check named `by` has already decided.
    ///
    /// Not a way round the rule: each call site is listed in the
    /// system-authority inventory with the check that stands behind it.
    pub const fn vouched(principal: &'a Principal, by: &'static str) -> Unchecked<'a> {
        Unchecked {
            who: Authority::Caller(principal),
            by: Some(by),
        }
    }

    /// The authority the operation still runs under: what an RLS table's
    /// policies read and the write's event reports.
    pub fn authority(self) -> Authority<'a> {
        self.who
    }

    /// The caller, or `None` for the system.
    pub fn principal(self) -> Option<&'a Principal> {
        self.who.principal()
    }

    /// The check a vouched caller's access was decided by.
    pub fn vouched_by(self) -> Option<&'static str> {
        self.by
    }
}

impl<'a> From<Unchecked<'a>> for Authority<'a> {
    fn from(unchecked: Unchecked<'a>) -> Authority<'a> {
        unchecked.who
    }
}

impl Principal {
    /// The signed-in user's id, as the user object carries it (`id`).
    pub fn user_id(&self) -> Option<&str> {
        self.user.as_ref()?.get("id")?.as_str()
    }

    /// This caller as a file rule sees them (`sc_files` sits below this crate,
    /// so the view is built here): the role, and the user id the owner rule
    /// compares with.
    pub fn file_caller(&self) -> sc_files::FileCaller {
        // A user id is a uuid's text; a number (a test's, or a v1 import's) is
        // compared as its text, as the owner column records it.
        let id = self
            .user
            .as_ref()
            .and_then(|u| u.get("id"))
            .and_then(|id| match id {
                Json::String(s) => Some(s.clone()),
                Json::Number(n) => Some(n.to_string()),
                _ => None,
            });
        sc_files::FileCaller::of_principal(self.role, id)
    }
}

impl<'a> From<&'a AdminPrincipal> for Authority<'a> {
    fn from(admin: &'a AdminPrincipal) -> Authority<'a> {
        Authority::Caller(admin.principal())
    }
}

impl<'a> From<&'a Principal> for Authority<'a> {
    fn from(principal: &'a Principal) -> Authority<'a> {
        Authority::Caller(principal)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: it drives its own servers, tasks and channels; the replacements are for production paths"
)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Security review #32's class: context lost across a `tokio::spawn`. The
    /// chain (with its untrusted-input marker) travels in the `Principal`; the
    /// origin and the changeset, which are task-locals, travel by
    /// `spawn_scoped` — and a bare `tokio::spawn` is shown dropping them.
    #[tokio::test]
    async fn a_triggered_chain_and_its_origin_survive_a_spawn() {
        let origin = Origin {
            app: None,
            session: Some("guest-1".into()),
        };
        let cs = Arc::new(Changeset::new(crate::changeset::ChangesetSource::Request));
        let caller = Principal::recorded(Role::of(40), None)
            .chained(vec!["on_insert".into()])
            .untrusted();
        let (scoped, bare) = with_origin(
            origin.clone(),
            changeset::scope(cs.clone(), async move {
                let scoped = spawn_scoped(async move {
                    let rebuilt = Principal::recorded(Role::of(40), None).in_current_origin();
                    (caller.chain.clone(), rebuilt.origin(), rebuilt.changeset)
                });
                let bare = tokio::spawn(async {
                    let rebuilt = Principal::recorded(Role::of(40), None).in_current_origin();
                    (rebuilt.origin(), rebuilt.changeset)
                });
                (scoped.await.unwrap(), bare.await.unwrap())
            }),
        )
        .await;
        let (chain, scoped_origin, scoped_changeset) = scoped;
        assert!(
            is_untrusted_chain(&chain),
            "the triggered marker survives: {chain:?}"
        );
        assert_eq!(chain.last().map(String::as_str), Some("on_insert"));
        assert_eq!(scoped_origin, origin);
        assert_eq!(scoped_changeset.map(|c| c.id), Some(cs.id));
        // What `spawn_scoped` is for: a bare spawn starts outside every scope.
        assert_eq!(bare.0, Origin::default());
        assert!(bare.1.is_none());
    }

    #[test]
    fn a_file_rule_sees_the_principals_role_and_user_id() {
        let ada = Principal::recorded(Role::of(40), Some(json!({ "id": "u-1", "email": "a@b.c" })));
        assert_eq!(ada.user_id(), Some("u-1"));
        assert_eq!(
            ada.file_caller(),
            sc_files::FileCaller::of_principal(Role::of(40), Some("u-1".into()))
        );
        assert_eq!(
            Principal::anonymous().file_caller(),
            sc_files::FileCaller::anonymous()
        );
    }

    struct Verified;

    impl VerifiedUser for Verified {
        fn verified_role(&self) -> Role {
            Role::of(40)
        }

        fn verified_fields(&self) -> Json {
            json!({ "id": "u-7", "role": 40 })
        }
    }

    /// TODO-type-hardening H3.6: every admin proof made outside a test names a
    /// reason (a literal, or the constant holding one) that the
    /// system-authority inventory in docs/SECURITY_REVIEW_2026-10.md lists, so
    /// a new one is a decision somebody reviews rather than a line nobody
    /// greps for.
    #[test]
    fn admin_proofs_are_in_the_inventory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let inventory = std::fs::read_to_string(root.join("docs/SECURITY_REVIEW_2026-10.md"))
            .expect("the inventory");
        const MAKERS: [&str; 5] = [
            "AdminPrincipal::vouched(",
            "AdminPrincipal::system(",
            "ConsoleAdmin::at_console(",
            "ConsoleAdmin::system(",
            "Caller::system(",
        ];
        let mut missing = Vec::new();
        let mut sources = vec![root.join("crates")];
        while let Some(dir) = sources.pop() {
            for entry in std::fs::read_dir(&dir)
                .expect("a source directory")
                .flatten()
            {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if name != "tests" && name != "target" && name != "node_modules" {
                        sources.push(path);
                    }
                    continue;
                }
                if !name.ends_with(".rs") || name == "tests.rs" || name.ends_with("_tests.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                // The source, up to its test module.
                let mut code = String::new();
                let mut lines = text.lines().peekable();
                while let Some(line) = lines.next() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("mod tests")
                        || trimmed.starts_with("pub(crate) mod tests")
                    {
                        break;
                    }
                    if line.trim() == "#[cfg(test)]"
                        && lines
                            .peek()
                            .is_some_and(|next| next.trim_start().starts_with("mod "))
                    {
                        break;
                    }
                    if !line.trim_start().starts_with("//") {
                        code.push_str(line);
                        code.push('\n');
                    }
                }
                for maker in MAKERS {
                    for (at, _) in code.match_indices(maker) {
                        let args = &code[at + maker.len()..];
                        let args = &args[..args.find(';').unwrap_or(args.len())];
                        let reason = reason_in(maker, args);
                        let Some(reason) = reason else { continue };
                        if !inventory.contains(&reason) {
                            missing.push(format!("{}: {maker} {reason}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(missing.is_empty(), "not in the inventory: {missing:#?}");
    }

    /// The reason a maker's call names: its string literal, or the constant
    /// holding one (an UPPER_CASE name). `None` for a variable (the makers'
    /// own definitions pass theirs through).
    fn reason_in(maker: &str, args: &str) -> Option<String> {
        // The reason is the last argument of a two-argument maker.
        let last = match maker {
            "AdminPrincipal::vouched(" | "ConsoleAdmin::at_console(" => {
                let mut depth = 0i32;
                let mut split = None;
                for (i, c) in args.char_indices() {
                    match c {
                        '(' | '[' | '{' => depth += 1,
                        ')' | ']' | '}' if depth == 0 => break,
                        ')' | ']' | '}' => depth -= 1,
                        ',' if depth == 0 => split = Some(i),
                        _ => {}
                    }
                }
                &args[split? + 1..]
            }
            _ => args,
        };
        let last = last.trim_start();
        if let Some(rest) = last.strip_prefix('"') {
            return Some(rest[..rest.find('"')?].to_owned());
        }
        let ident: String = last
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
            .collect();
        let ident = ident.rsplit("::").next()?.to_owned();
        (!ident.is_empty() && ident.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
            .then_some(ident)
    }

    /// TODO-type-hardening H3.1: the named constructors, and the admin proof.
    #[test]
    fn a_principal_comes_from_a_proof_and_only_the_admin_proof_is_unchecked() {
        let ada = Principal::of(&Verified);
        assert_eq!(ada.role(), Role::of(40));
        assert_eq!(ada.user_id(), Some("u-7"));
        assert!(Principal::anonymous().role().is_public());
        assert!(Principal::anonymous().user().is_none());

        // An application's maximum role lowers, and never raises.
        assert_eq!(ada.clone().capped_at(Role::of(80)).role(), Role::of(80));
        assert_eq!(ada.clone().capped_at(Role::ADMIN).role(), Role::of(40));

        // The proof names what stands behind it, and is the admin role.
        let admin = AdminPrincipal::vouched(Some(json!({ "id": "a-1" })), "a test's check");
        let token = Unchecked::admin(&admin);
        assert_eq!(token.vouched_by(), Some("a test's check"));
        assert_eq!(token.principal().map(Principal::role), Some(Role::ADMIN));
        assert_eq!(token.principal().and_then(Principal::user_id), Some("a-1"));
        let system = AdminPrincipal::system("a test's job");
        assert!(system.user().is_none() && system.role().is_admin());

        // An admin role in a principal still meets every floor — the rule
        // decides that, and says so — but it is not the admin proof.
        let recorded = Principal::recorded(Role::ADMIN, None);
        let decided = Authority::caller(&recorded).unchecked().unwrap();
        assert_eq!(decided.vouched_by(), Some(ADMIN_MEETS_EVERY_FLOOR));
        assert!(Authority::caller(&ada).unchecked().is_none());
    }

    #[test]
    fn the_system_is_the_admin_with_no_user_and_says_why() {
        let system = Authority::system("retention sweep");
        assert!(system.is_system());
        assert_eq!(system.reason(), Some("retention sweep"));
        assert_eq!(system.role(), Role::ADMIN);
        assert!(system.principal().is_none());
        let as_principal = system.as_principal();
        assert!(as_principal.role.is_admin() && as_principal.user.is_none());

        let anonymous = Principal::anonymous();
        let caller = Authority::from(&anonymous);
        assert!(!caller.is_system());
        assert_eq!(caller.reason(), None);
        assert_eq!(caller.role(), Role::PUBLIC);
        assert_eq!(caller.principal(), Some(&anonymous));
    }

    #[test]
    fn a_caller_carries_its_role_its_user_and_what_led_here() {
        let anonymous = Principal::anonymous();
        assert_eq!(anonymous.role, Role::PUBLIC);
        assert!(anonymous.user.is_none());
        // Nothing led here: the ordinary case is a request, at depth 0.
        assert!(anonymous.chain.is_empty());
        assert_eq!(anonymous.user_json(), None);

        let user = json!({ "id": 1, "email": "a@b.c" });
        let caller =
            Principal::recorded(Role::ADMIN, Some(user.clone())).chained(vec!["audit".into()]);
        assert_eq!(caller.chain, vec!["audit".to_owned()]);
        // The GUC carries the object as compact JSON — what `UserEnv::Guc`'s
        // `current_setting('sc.user', …)::jsonb ->> 'x'` reads.
        assert_eq!(
            caller.user_json().as_deref(),
            Some(user.to_string().as_str())
        );
    }
}
