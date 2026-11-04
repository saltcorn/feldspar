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
/// The user is still the JSON object the `sc.user` GUC carries; it becomes a
/// proof-carrying `AuthenticatedUser` with TODO-code-quality Q2.2.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Principal {
    /// The caller's role (public when anonymous).
    pub role: Role,
    /// The logged-in user's fields as a JSON object, or `None` when anonymous —
    /// in which case `sc.user` is left unset and the policies'
    /// `current_setting('sc.user', true)` reads `NULL`.
    pub user: Option<Json>,
    /// The trigger names whose actions led to this write, outermost first. Empty
    /// for a write a request made directly.
    pub chain: Vec<String>,
    /// The application the write's request came through, if any.
    pub app: Option<CallingApp>,
    /// The guest session's public id, if the caller has one — what the
    /// `sc.session` GUC carries.
    pub session: Option<String>,
    /// The changeset this write belongs to. `None` lets the change-log trigger
    /// mint one of its own (`source = 'sql'`), as for a write from outside.
    pub changeset: Option<Arc<Changeset>>,
}

impl Principal {
    /// A context for `role` carrying `user`'s fields, in the ambient changeset
    /// ([`changeset::current`]) when the task runs inside one.
    pub fn new(role: Role, user: Option<Json>) -> Principal {
        Principal {
            role,
            user,
            chain: Vec::new(),
            app: None,
            session: None,
            changeset: changeset::current(),
        }
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

    /// A context for `role` with no user (anonymous).
    pub fn anonymous(role: Role) -> Principal {
        Principal::new(role, None)
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
            Authority::System(_) => std::borrow::Cow::Owned(Principal::new(Role::ADMIN, None)),
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
            Authority::Caller(principal) => Unchecked::admin(principal),
        }
    }
}

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
/// - [`Unchecked::admin`]: an administrator's principal. An administrator meets
///   every role floor, which is when the ownership rule is not consulted, so
///   unchecked and checked access are the same; any other principal is refused.
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
    /// `principal`'s unchecked access, when it is an administrator's; `None`
    /// otherwise.
    pub fn admin(principal: &'a Principal) -> Option<Unchecked<'a>> {
        principal.role.is_admin().then_some(Unchecked {
            who: Authority::Caller(principal),
            by: None,
        })
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
        sc_files::FileCaller::new(self.role, self.user_id().map(str::to_owned))
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
        let caller = Principal::new(Role::of(40), None)
            .chained(vec!["on_insert".into()])
            .untrusted();
        let (scoped, bare) = with_origin(
            origin.clone(),
            changeset::scope(cs.clone(), async move {
                let scoped = spawn_scoped(async move {
                    let rebuilt = Principal::new(Role::of(40), None).in_current_origin();
                    (caller.chain.clone(), rebuilt.origin(), rebuilt.changeset)
                });
                let bare = tokio::spawn(async {
                    let rebuilt = Principal::new(Role::of(40), None).in_current_origin();
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
        let ada = Principal::new(Role::of(40), Some(json!({ "id": "u-1", "email": "a@b.c" })));
        assert_eq!(ada.user_id(), Some("u-1"));
        assert_eq!(
            ada.file_caller(),
            sc_files::FileCaller::new(Role::of(40), Some("u-1".into()))
        );
        assert_eq!(
            Principal::anonymous(Role::PUBLIC).file_caller(),
            sc_files::FileCaller::anonymous()
        );
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

        let anonymous = Principal::anonymous(Role::PUBLIC);
        let caller = Authority::from(&anonymous);
        assert!(!caller.is_system());
        assert_eq!(caller.reason(), None);
        assert_eq!(caller.role(), Role::PUBLIC);
        assert_eq!(caller.principal(), Some(&anonymous));
    }

    #[test]
    fn a_caller_carries_its_role_its_user_and_what_led_here() {
        let anonymous = Principal::anonymous(Role::PUBLIC);
        assert_eq!(anonymous.role, Role::PUBLIC);
        assert!(anonymous.user.is_none());
        // Nothing led here: the ordinary case is a request, at depth 0.
        assert!(anonymous.chain.is_empty());
        assert_eq!(anonymous.user_json(), None);

        let user = json!({ "id": 1, "email": "a@b.c" });
        let caller = Principal::new(Role::ADMIN, Some(user.clone())).chained(vec!["audit".into()]);
        assert_eq!(caller.chain, vec!["audit".to_owned()]);
        // The GUC carries the object as compact JSON — what `UserEnv::Guc`'s
        // `current_setting('sc.user', …)::jsonb ->> 'x'` reads.
        assert_eq!(
            caller.user_json().as_deref(),
            Some(user.to_string().as_str())
        );
    }
}
