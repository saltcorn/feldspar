//! Server-side session bookkeeping (technical design §7.2: "session cookie
//! baseline").
//!
//! A login mints an opaque, high-entropy **session token**; the server sends it
//! to the browser in a cookie and hands it back here on each request to recover
//! the logged-in [`User`]. Logout drops the token.
//!
//! # Why sessions are rows
//!
//! An in-memory map is the fastest possible session store and the reason there
//! can only ever be **one** application server: a session minted on node A is
//! not a session node B has heard of, so a load balancer in front of two
//! processes logs people out at random. So sessions live in
//! [`_fd_sessions`](SESSIONS_TABLE) in the primary database — the one thing every
//! node already shares — and every node keeps a **read-through cache** in front
//! of it so the common case is still a map lookup rather than a round trip.
//!
//! Three decisions make that table cheap enough to be on the request path:
//!
//! - **It is `UNLOGGED`** (where the backend supports it — see
//!   [`create_unlogged_table`](sc_catalog::Catalog::create_unlogged_table)). A
//!   session is worth sharing between nodes and not worth a write-ahead-log
//!   record. The price is that an unclean shutdown truncates the table and its
//!   rows never reach a physical standby; both cost a re-login, which is what a
//!   restart cost before this existed.
//! - **Nothing is written per request.** The expiry is fixed at login, so a
//!   read is a read. (A sliding expiry would put a write on every request; if
//!   one is ever wanted it must be throttled, not literal.)
//! - **The row names the user, it does not copy them.** It carries `user_id`,
//!   and a cache miss reads the user through [`load_user`]. Serialising a
//!   [`User`]'s admin-defined `extra` columns into the session row would freeze
//!   a snapshot of them for the session's whole 24 hours; reading the row back
//!   means a role change takes effect within [`CACHE_TTL_SECONDS`] instead.
//!   The user is read at their **effective role** (the base role and the role
//!   grants in force, [`crate::effective_role`]), and an entry is believed no
//!   later than the moment that can next change — a grant that ends lapses on
//!   time, not up to a TTL later.
//!
//! # The cache, and what it costs
//!
//! Bounded two ways, because either alone is not enough: a **capacity**
//! ([`CACHE_CAPACITY`], least-recently-used eviction) bounds memory, and a
//! per-entry **freshness TTL** ([`CACHE_TTL_SECONDS`]) bounds staleness. An
//! entry older than the TTL is re-read rather than trusted.
//!
//! **Misses are never cached.** This is not an optimisation, it is the property
//! that makes multi-node work: node A mints a session, the browser's next
//! request lands on node B, and a node that had cached "no such token" would
//! keep the user logged out until the entry aged out. An unknown token always
//! asks the database — a primary-key lookup on an unlogged table.
//!
//! **Logout is the honest weak spot.** [`logout`](SessionStore::logout) deletes
//! the row and evicts the local cache entry, so the node that handled it is
//! consistent immediately — but another node holding a cached entry keeps
//! honouring that cookie until the entry goes stale. So the window is
//! [`CACHE_TTL_SECONDS`], deliberately short, and it closes entirely once the
//! message bus (§16) can carry an eviction to every node; [`invalidate`] is the
//! seam that will hook to. A deployment that will not accept the window sets the
//! cache TTL to zero, which turns every lookup into a database read.
//!
//! # Who else is told a session ended
//!
//! A session is also held open by things that are not requests: an
//! application's live socket (TODO.md "Live updates" §3 rule 6) authenticated
//! once, at its upgrade, and would otherwise go on delivering to a signed-out
//! browser until its next re-check. So the store keeps a list of
//! [`SessionListener`]s and tells each one, synchronously, when
//! [`logout`](SessionStore::logout), [`end_user_sessions`](SessionStore::end_user_sessions)
//! or [`end_all_sessions`](SessionStore::end_all_sessions) ends something —
//! after the rows are gone, so a listener that re-reads finds them gone. Held
//! weakly: a listener that has gone away is skipped, not kept alive.
//!
//! # Tokens at rest
//!
//! What is stored is the **SHA-256 of the token**, never the token. A session
//! cookie is a bearer credential, so a table full of live ones is a table worth
//! stealing; a hash of one is not. A fast hash is the right one here (unlike a
//! password, which gets argon2id in [`crate::password`]): the token is already
//! 256 bits of uniform randomness, so there is no dictionary to run and nothing
//! for a slow hash to buy.
//!
//! # The host scope a session was issued for
//!
//! A session is minted either for the **admin host** — the admin UI's sign-in,
//! the admin handoff, `feldspar auth token` — or for **one application's**
//! host: that application's own sign-in, a checklist's browser, `call_api`.
//! The scope is part of the token itself ([`SessionScope::of_token`]): an
//! application session's token is [`APP_TOKEN_PREFIX`], the application's id
//! (32 hex digits), `-` and the random part; an admin session's is the plain
//! 64 hex digits every token was before scopes existed. The token is hashed
//! whole, so the prefix cannot be added, removed or pointed at another
//! application without making a token nobody issued, and nothing is added to
//! the table. An application token from before tokens named their application
//! (`app-` and 64 hex digits) is [`SessionScope::AnyApp`]: it goes on working
//! where it did until it expires, but is never a session *issued by* the
//! application it is presented to (security review 2026-11, §2).
//!
//! The server refuses an application session admin authority on the admin
//! host: with shared sign-in, or an admin signed in to an application, the
//! cookie an application's page can make the browser send is never an admin
//! session (security review #15). A token minted before scopes existed reads
//! as an admin-host one, so nobody is signed out by an upgrade; such a token
//! lapses with its expiry.
//!
//! # The memory backend
//!
//! [`SessionStore::memory`] keeps the original in-memory map, and is what the
//! router's own tests — which have no database at all — run against. It is a
//! test seam, not a deployment mode: `feldspar serve` always builds the
//! database-backed store.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Duration, Utc};
use lru::LruCache;
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, Update, Value,
};
use sc_types::{BasicType, TypeRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::epoch::{AuthEpoch, CredentialsChanged, current_epoch};
use crate::lookup::load_user_until;
use crate::signin::FullyAuthenticated;
use crate::user::{AuthenticatedUser, User};

/// What an application-scoped session token starts with (module docs, "The host
/// scope a session was issued for").
pub const APP_TOKEN_PREFIX: &str = "app-";

/// The host a session was issued on, and so the host it may act for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    /// Issued on the admin host (or by `feldspar auth token`). Such a session
    /// also works on an application's host, as it always has (shared sign-in).
    Admin,
    /// Issued on the host of the application with this id. Never admin
    /// authority on the admin host, whatever the user's role.
    App(Uuid),
    /// Issued on an application's host before tokens named their application
    /// (module docs). Never minted now.
    AnyApp,
}

impl SessionScope {
    /// The scope `token` was issued for. A token without the application
    /// prefix — including every token minted before scopes existed — is an
    /// admin-host one.
    pub fn of_token(token: &str) -> SessionScope {
        let Some(rest) = token.strip_prefix(APP_TOKEN_PREFIX) else {
            return SessionScope::Admin;
        };
        match rest.split_once('-') {
            Some((app, random)) if app.len() == 32 && random.len() == 64 => {
                Uuid::try_parse(app).map_or(SessionScope::AnyApp, SessionScope::App)
            }
            _ => SessionScope::AnyApp,
        }
    }

    /// Whether this is an application's session (of whichever application).
    pub fn is_app(self) -> bool {
        !matches!(self, SessionScope::Admin)
    }
}

/// A session a token names, as [`SessionStore::session`] finds it: who it is,
/// and the host it was issued for.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSession {
    /// The signed-in user, at their effective role.
    pub user: AuthenticatedUser,
    /// Where the session was issued ([`SessionScope::of_token`]).
    pub scope: SessionScope,
}

/// Default session lifetime: one day.
pub const DEFAULT_TTL_HOURS: i64 = 24;

/// How long a cached session is trusted before it is re-read from the database.
///
/// This is the staleness budget for everything the session row and the user row
/// say: a logout on another node, a role change, a deleted user. Short enough
/// that none of those linger, long enough that a busy session is a map lookup
/// rather than a query.
pub const CACHE_TTL_SECONDS: i64 = 60;

/// How many sessions a node caches before evicting the least recently used.
pub const CACHE_CAPACITY: usize = 10_000;

/// Name of the session table in the primary database.
pub const SESSIONS_TABLE: &str = "_fd_sessions";

/// The SHA-256 of the session token, hex-encoded — the primary key.
pub const COL_TOKEN_HASH: &str = "token_hash";
/// The `users.id` this session is for. Not a foreign key — see
/// [`session_fields`] for why.
pub const COL_USER: &str = "user_id";
/// When the session lapses.
pub const COL_EXPIRES_AT: &str = "expires_at";
/// The account's [`AuthEpoch`] when the session started (security review
/// 2026-11, H2.3): a session from an earlier epoch is nobody's. Nullable, so
/// the bootstrap adds it to an existing table, and `NULL` — a session from
/// before epochs — reads as the initial epoch.
pub const COL_AUTH_EPOCH: &str = "auth_epoch";

/// How often a process sweeps lapsed rows, at most.
const SWEEP_INTERVAL_SECONDS: i64 = 60;

/// The fields of the session table, in declaration order.
///
/// **`user_id` is deliberately not a foreign key**, which is the opposite of the
/// rule everywhere else in `sc-auth`, so it needs its reason stated. The schema
/// layer renders a plain `REFERENCES` with no `ON DELETE` action, so a foreign
/// key here would mean a user with a live session **cannot be deleted** — an
/// administrator blocked by an ephemeral row, which is the wrong answer to the
/// wrong question.
///
/// Nothing is given up by leaving it out, because the guarantee the constraint
/// would buy is already unconditional: [`user_for`](SessionStore::user_for)
/// resolves a session by *reading the user*, so a session naming somebody who is
/// gone resolves to nobody. The row that outlives its user is inert, and the
/// sweep collects it at its expiry.
fn session_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_TOKEN_HASH, text())
            .required()
            .primary_key(),
        DataField::plain(COL_USER, uuid()).required(),
        DataField::plain(COL_EXPIRES_AT, timestamp()).required(),
        DataField::plain(COL_AUTH_EPOCH, TypeRef::Basic(BasicType::Int)),
    ]
}

/// Ensure the session table exists.
///
/// **Must run after the users table**, which it references.
/// [`bootstrap`](crate::bootstrap) does them in that order. Idempotent, like
/// every other bootstrap — including across the truncation an unclean shutdown
/// does to an unlogged table, which empties it without dropping it.
pub(crate) async fn bootstrap_sessions(catalog: &Catalog) -> Result<Table> {
    if catalog.get(SESSIONS_TABLE)?.is_some() {
        // Additively reconciled: a column added since (`auth_epoch`) is
        // created; the table stays unlogged.
        return catalog
            .bootstrap_table(SESSIONS_TABLE, &session_fields())
            .await;
    }
    catalog
        .create_unlogged_table(SESSIONS_TABLE, &session_fields())
        .await
}

/// Mint a session for `user_id` straight into the database, returning the token
/// to put in a cookie.
///
/// The store's own [`login`](SessionStore::login) is this plus a cache write.
/// It is public because a session being **a row** is the whole point of this
/// module: `feldspar auth token` (§7.2) runs in a shell holding the primary
/// database's credentials and can now write one itself, with no running server
/// to ask and no one-time grant to bridge the gap. That is not a new authority —
/// whoever can write this row can already read every password hash and rewrite
/// any of them — it is the same authority, spelled once instead of twice.
///
/// The session it makes is an ordinary one: it does exactly what that user's
/// account does, and expires like any other. It starts from a
/// [`FullyAuthenticated`] like every other session (`feldspar auth token`'s is
/// [`SecondFactorWaiver::Cli`](crate::SecondFactorWaiver::Cli)).
pub async fn create_session(
    catalog: &Catalog,
    who: FullyAuthenticated,
    ttl: Duration,
) -> Result<String> {
    let epoch = epoch_to_start_at(catalog, &who).await?;
    let token = new_token(SessionScope::Admin)?;
    let expires_at = Utc::now() + ttl;
    insert_session(
        catalog,
        &token_hash(&token),
        who.user().id(),
        expires_at,
        epoch,
    )
    .await?;
    Ok(token)
}

/// The epoch a session for `who` starts at: the account's now, which must be
/// the one its sign-in was checked at (a credential changed since refuses it).
async fn epoch_to_start_at(catalog: &Catalog, who: &FullyAuthenticated) -> Result<AuthEpoch> {
    let now = current_epoch(catalog, who.user().id()).await?;
    refuse_stale(who, now)?;
    Ok(now)
}

/// Refuse to start a session from a sign-in checked before the account's
/// credentials last changed.
fn refuse_stale(who: &FullyAuthenticated, now: AuthEpoch) -> Result<()> {
    match who.epoch() {
        Some(checked) if checked != now => Err(Error::auth(
            "this account's sign-in details changed while it was signing in; sign in again",
        )),
        _ => Ok(()),
    }
}

/// One cached session: who it is, when the session lapses, and how long this
/// answer may be believed without asking the database again.
#[derive(Clone)]
struct Cached {
    user: User,
    expires_at: DateTime<Utc>,
    fresh_until: DateTime<Utc>,
}

/// One session held by the in-memory backend.
struct Entry {
    user: User,
    expires_at: DateTime<Utc>,
    epoch: AuthEpoch,
}

/// The in-memory backend's sessions, and the epochs its bumps raised.
#[derive(Default)]
struct MemorySessions {
    entries: std::collections::HashMap<String, Entry>,
    epochs: std::collections::HashMap<Uuid, AuthEpoch>,
}

impl MemorySessions {
    fn epoch_of(&self, user: Uuid) -> AuthEpoch {
        self.epochs
            .get(&user)
            .copied()
            .unwrap_or(AuthEpoch::INITIAL)
    }
}

/// Where sessions actually live.
enum Backend {
    /// A plain map in this process — no database, no sharing (see the module
    /// docs: a test seam).
    Memory(RwLock<MemorySessions>),
    /// Rows in [`_fd_sessions`](SESSIONS_TABLE), shared by every node.
    Database {
        catalog: Arc<Catalog>,
        /// When this process last swept lapsed rows, so that a busy server does
        /// not sweep once per login.
        last_sweep: Mutex<DateTime<Utc>>,
    },
}

/// What ended, as a [`SessionListener`] is told it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnded<'a> {
    /// The session with this token: a sign-out, or a session replaced by a
    /// new one.
    Token(&'a str),
    /// Every session of this user: a forced sign-out, a disabled or deleted
    /// account.
    User(Uuid),
    /// Every session of this user but the one with token `keep`: the account
    /// changed its own credentials from that session (security review
    /// 2026-11, #11).
    UserExcept {
        /// The account.
        user: Uuid,
        /// The session that made the change, which goes on.
        keep: &'a str,
    },
    /// Every session there is.
    All,
}

impl SessionEnded<'_> {
    /// Whether this ending covers the session with `token`, of `user`.
    pub fn covers(&self, token: Option<&str>, user: Option<Uuid>) -> bool {
        match self {
            SessionEnded::Token(t) => token == Some(*t),
            SessionEnded::User(u) => user == Some(*u),
            SessionEnded::UserExcept { user: u, keep } => user == Some(*u) && token != Some(*keep),
            SessionEnded::All => token.is_some(),
        }
    }
}

/// Something that holds a session open between requests and must let go of it
/// when it ends — an application's live socket (see the module docs).
///
/// Called synchronously from the store, after the session is gone, so it must
/// return promptly: a listener that has work to do signals its own task.
pub trait SessionListener: Send + Sync {
    /// A session, a user's sessions or every session ended on this node.
    fn session_ended(&self, ended: &SessionEnded<'_>);
}

/// A store of active sessions, keyed by opaque token.
pub struct SessionStore {
    ttl: Duration,
    cache_ttl: Duration,
    backend: Backend,
    /// Told when a session ends (see the module docs). Weak, so registering
    /// does not keep a listener alive.
    listeners: RwLock<Vec<std::sync::Weak<dyn SessionListener>>>,
    /// The read-through cache in front of [`Backend::Database`], keyed by the
    /// **raw** token (the hash is what the database is keyed by). Unused by the
    /// memory backend, which is already a map.
    cache: Mutex<LruCache<String, Cached>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        SessionStore::memory()
    }
}

impl SessionStore {
    /// An in-process store with the default session lifetime.
    pub fn memory() -> SessionStore {
        SessionStore::with_ttl(Duration::hours(DEFAULT_TTL_HOURS))
    }

    /// An in-process store whose sessions expire `ttl` after they are created.
    pub(crate) fn with_ttl(ttl: Duration) -> SessionStore {
        SessionStore {
            ttl,
            cache_ttl: Duration::seconds(CACHE_TTL_SECONDS),
            backend: Backend::Memory(RwLock::new(MemorySessions::default())),
            cache: new_cache(CACHE_CAPACITY),
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// A store backed by [`_fd_sessions`](SESSIONS_TABLE), shared with every
    /// other node against the same database, with the default lifetimes.
    pub fn database(catalog: Arc<Catalog>) -> SessionStore {
        SessionStore::database_with(
            catalog,
            Duration::hours(DEFAULT_TTL_HOURS),
            Duration::seconds(CACHE_TTL_SECONDS),
            CACHE_CAPACITY,
        )
    }

    /// A database-backed store with every knob given explicitly.
    ///
    /// A `cache_ttl` of zero disables the cache: every lookup reads the
    /// database, which is what a deployment that will not tolerate a stale
    /// logout on another node wants (see the module docs).
    pub fn database_with(
        catalog: Arc<Catalog>,
        ttl: Duration,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> SessionStore {
        SessionStore {
            ttl,
            cache_ttl,
            backend: Backend::Database {
                catalog,
                // Epoch, so the first login sweeps.
                last_sweep: Mutex::new(DateTime::<Utc>::MIN_UTC),
            },
            cache: new_cache(cache_capacity),
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// Tell `listener` whenever a session ends on this node, for as long as it
    /// lives (it is held weakly).
    pub fn add_listener(&self, listener: std::sync::Weak<dyn SessionListener>) {
        let mut listeners = match self.listeners.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Forget the ones that have gone, so a long-lived store does not
        // accumulate dead entries one router at a time.
        listeners.retain(|l| l.strong_count() > 0);
        listeners.push(listener);
    }

    /// Tell every listener that `ended` ended.
    fn ended(&self, ended: SessionEnded<'_>) {
        let listeners: Vec<Arc<dyn SessionListener>> = match self.listeners.read() {
            Ok(guard) => guard.iter().filter_map(std::sync::Weak::upgrade).collect(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect(),
        };
        for listener in listeners {
            listener.session_ended(&ended);
        }
    }

    /// How long a session lasts from login — what its cookie's `Max-Age` says,
    /// so a client that keeps cookies across restarts (a phone app) keeps the
    /// session exactly as long as the server honours it.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Start a session for `who` on `scope`'s host that lasts `ttl`, and
    /// return its token (to be set as a cookie) — the one way a session is
    /// started (module docs of [`crate::signin`]).
    ///
    /// Refused when `who` was signed in at an earlier [`AuthEpoch`] than the
    /// account is at now (by the database-backed store, which can read it),
    /// and for [`SessionScope::AnyApp`], which is never minted.
    pub async fn start(
        &self,
        who: FullyAuthenticated,
        ttl: Duration,
        scope: SessionScope,
    ) -> Result<String> {
        let token = new_token(scope)?;
        let expires_at = Utc::now() + ttl;
        match &self.backend {
            Backend::Memory(sessions) => {
                // No database here, so no epoch to hold the sign-in to: the
                // map's own epochs only count the bumps this store was given
                // (a test seam, module docs).
                let mut sessions = sessions.write().map_err(|_| poisoned())?;
                let epoch = sessions.epoch_of(who.user().id());
                sessions.entries.insert(
                    token.clone(),
                    Entry {
                        user: who.into_user(),
                        expires_at,
                        epoch,
                    },
                );
            }
            Backend::Database {
                catalog,
                last_sweep,
            } => {
                let epoch = epoch_to_start_at(catalog, &who).await?;
                let user = who.into_user();
                insert_session(catalog, &token_hash(&token), user.id(), expires_at, epoch).await?;
                // Housekeeping rides along with the one write sessions do, but
                // at most once a minute: a login is common and a full-table
                // delete is not free.
                if due_for_sweep(last_sweep)? {
                    sweep_sessions(catalog).await?;
                }
                // Cached as the next request would read it — at the effective
                // role, until that next changes — rather than as the caller
                // happened to hold it: a session started straight after a
                // sign-up whose trigger granted a trial is served the trial.
                match load_user_until(catalog, user.id()).await? {
                    Some((current, changes_at)) => {
                        self.cache_put(&token, &current, expires_at, changes_at);
                    }
                    None => self.cache_put(&token, &user, expires_at, None),
                }
            }
        }
        Ok(token)
    }

    /// A test's session for `user` on the admin host. Only with the
    /// `testing` feature: a server starts a session from a
    /// [`FullyAuthenticated`].
    #[cfg(any(test, feature = "testing"))]
    pub async fn login(&self, user: User) -> Result<String> {
        self.login_scoped(user, self.ttl, SessionScope::Admin).await
    }

    /// A test's session for `user` on `scope`'s host.
    #[cfg(any(test, feature = "testing"))]
    pub async fn login_in(&self, user: User, scope: SessionScope) -> Result<String> {
        self.login_scoped(user, self.ttl, scope).await
    }

    /// A test's admin-host session for `user` lasting `ttl`.
    #[cfg(any(test, feature = "testing"))]
    pub async fn login_for(&self, user: User, ttl: Duration) -> Result<String> {
        self.login_scoped(user, ttl, SessionScope::Admin).await
    }

    /// A test's session for `user` on `scope`'s host lasting `ttl`.
    #[cfg(any(test, feature = "testing"))]
    pub async fn login_scoped(
        &self,
        user: User,
        ttl: Duration,
        scope: SessionScope,
    ) -> Result<String> {
        let who = FullyAuthenticated::waived(user, crate::SecondFactorWaiver::Test);
        self.start(who, ttl, scope).await
    }

    /// The user for a session token, if the token is known and unexpired. An
    /// expired token is treated as absent (and eagerly purged).
    ///
    /// One of the two places an [`AuthenticatedUser`] is made: the token was
    /// presented and this store issued it. Which host it was issued for is
    /// [`SessionScope::of_token`]'s to say.
    pub async fn user_for(&self, token: &str) -> Result<Option<AuthenticatedUser>> {
        Ok(self.resolve(token).await?.map(AuthenticatedUser::verified))
    }

    /// The session `token` names — who, and the host it was issued for — if
    /// it is known, unexpired and of the account's current epoch. What a
    /// server reads a request's session with, and then holds to the host the
    /// request arrived on.
    pub async fn session(&self, token: &str) -> Result<Option<LiveSession>> {
        Ok(self.user_for(token).await?.map(|user| LiveSession {
            user,
            scope: SessionScope::of_token(token),
        }))
    }

    /// [`user_for`](SessionStore::user_for)'s lookup, before the proof is
    /// attached.
    async fn resolve(&self, token: &str) -> Result<Option<User>> {
        let now = Utc::now();
        match &self.backend {
            Backend::Memory(sessions) => {
                // Fast path: a valid session under a read lock.
                {
                    let guard = sessions.read().map_err(|_| poisoned())?;
                    match guard.entries.get(token) {
                        None => return Ok(None),
                        Some(entry)
                            if entry.expires_at > now
                                && entry.epoch == guard.epoch_of(entry.user.id()) =>
                        {
                            return Ok(Some(entry.user.clone()));
                        }
                        Some(_) => {} // expired or stale — fall through to purge
                    }
                }
                sessions
                    .write()
                    .map_err(|_| poisoned())?
                    .entries
                    .remove(token);
                Ok(None)
            }
            Backend::Database { catalog, .. } => {
                // A cached entry answers only while it is both unexpired and
                // fresh; a stale one is re-read rather than believed, which is
                // what bounds a logout on another node.
                if let Some(cached) = self.cache_get(token)? {
                    if cached.expires_at <= now {
                        // The expiry was fixed at login, so a cached entry that
                        // has lapsed has definitively lapsed: no read needed.
                        self.invalidate(token)?;
                        return Ok(None);
                    }
                    if cached.fresh_until > now {
                        return Ok(Some(cached.user));
                    }
                }

                let Some((user_id, expires_at, epoch)) =
                    select_session(catalog, &token_hash(token)).await?
                else {
                    // Deliberately not cached — see the module docs.
                    self.invalidate(token)?;
                    return Ok(None);
                };
                if expires_at <= now {
                    delete_session(catalog, &token_hash(token)).await?;
                    self.invalidate(token)?;
                    return Ok(None);
                }
                // A session from before the account's last credential change
                // ended with it (crate::epoch), on whichever node it was
                // started. Refused, not deleted: the one session a change
                // keeps is restamped a moment after the epoch goes up, and a
                // request of its own in between must not delete it
                // (`bump_epoch` deletes the others, the sweep the rest).
                if epoch != current_epoch(catalog, user_id).await? {
                    self.invalidate(token)?;
                    return Ok(None);
                }
                // Reading the user is what makes a deleted user's session stop
                // being one — see `session_fields`, which leaves the foreign key
                // out precisely because this check does not need it. A disabled
                // user is the same case: the row is there, the account is not.
                let Some((user, changes_at)) = load_user_until(catalog, user_id).await? else {
                    self.invalidate(token)?;
                    return Ok(None);
                };
                if user.is_disabled() {
                    self.invalidate(token)?;
                    return Ok(None);
                }
                self.cache_put(token, &user, expires_at, changes_at);
                Ok(Some(user))
            }
        }
    }

    /// End a session (logout). Returns `true` if a session was ended.
    ///
    /// The row goes immediately and so does this node's cache entry; another
    /// node's cache entry lives out its freshness TTL (module docs).
    pub async fn logout(&self, token: &str) -> Result<bool> {
        let ended = match &self.backend {
            Backend::Memory(sessions) => sessions
                .write()
                .map_err(|_| poisoned())?
                .entries
                .remove(token)
                .is_some(),
            Backend::Database { catalog, .. } => {
                self.invalidate(token)?;
                delete_session(catalog, &token_hash(token)).await?
            }
        };
        self.ended(SessionEnded::Token(token));
        Ok(ended)
    }

    /// End **every** session belonging to one user, returning how many rows went.
    ///
    /// The admin screen's "force logout", and what disabling or deleting a user
    /// does on the way past: a credential that has already been handed out is not
    /// withdrawn by changing what the account may do, only by dropping the
    /// sessions holding it.
    ///
    /// The same caveat as [`logout`](SessionStore::logout), and no worse: the
    /// rows go immediately and this node forgets its cached copies, while another
    /// node's cache honours a token it already resolved until the entry goes
    /// stale ([`CACHE_TTL_SECONDS`]).
    pub async fn end_user_sessions(&self, user_id: Uuid) -> Result<usize> {
        let ended = self.end_user_rows(user_id).await?;
        self.ended(SessionEnded::User(user_id));
        Ok(ended)
    }

    /// [`end_user_sessions`](SessionStore::end_user_sessions)' work, before
    /// the listeners are told.
    async fn end_user_rows(&self, user_id: Uuid) -> Result<usize> {
        match &self.backend {
            Backend::Memory(sessions) => {
                let mut guard = sessions.write().map_err(|_| poisoned())?;
                let before = guard.entries.len();
                guard.entries.retain(|_, e| e.user.id() != user_id);
                Ok(before - guard.entries.len())
            }
            Backend::Database { catalog, .. } => {
                // The cache is keyed by token, so the tokens to forget are found
                // by asking the cached users who they are — the rows are gone
                // either way, this is only about the copies in front of them.
                let stale: Vec<String> = {
                    let cache = self.cache.lock().map_err(|_| poisoned())?;
                    cache
                        .iter()
                        .filter(|(_, cached)| cached.user.id() == user_id)
                        .map(|(token, _)| token.clone())
                        .collect()
                };
                for token in &stale {
                    self.invalidate(token)?;
                }
                delete_sessions_for_user(catalog, user_id).await
            }
        }
    }

    /// End every session of `user_id` **but** the one with token `keep`,
    /// returning how many went: what a credential change the account makes
    /// itself does (turning its second factor on or off), so that a session
    /// somebody else holds does not outlive it while the one that made the
    /// change goes on (security review 2026-11, #11).
    pub async fn end_other_sessions(&self, user_id: Uuid, keep: &str) -> Result<usize> {
        let ended = match &self.backend {
            Backend::Memory(sessions) => {
                let mut guard = sessions.write().map_err(|_| poisoned())?;
                let before = guard.entries.len();
                guard
                    .entries
                    .retain(|token, e| e.user.id() != user_id || token == keep);
                before - guard.entries.len()
            }
            Backend::Database { catalog, .. } => {
                let stale: Vec<String> = {
                    let cache = self.cache.lock().map_err(|_| poisoned())?;
                    cache
                        .iter()
                        .filter(|(token, cached)| cached.user.id() == user_id && *token != keep)
                        .map(|(token, _)| token.clone())
                        .collect()
                };
                for token in &stale {
                    self.invalidate(token)?;
                }
                let delete = Delete {
                    returning: vec![Projection::expr(Expr::col(COL_TOKEN_HASH))],
                    ..Delete::from(SESSIONS_TABLE).filter(
                        Expr::col(COL_USER).eq(Expr::lit(user_id)).and(Expr::binary(
                            BinOp::Ne,
                            Expr::col(COL_TOKEN_HASH),
                            Expr::lit(token_hash(keep)),
                        )),
                    )
                };
                rows(catalog, Statement::from(delete)).await?.len()
            }
        };
        self.ended(SessionEnded::UserExcept {
            user: user_id,
            keep,
        });
        Ok(ended)
    }

    /// Apply a credential change ([`CredentialsChanged`]): end every session
    /// of the account from before it, but the one with token `keep` — the
    /// session that made the change, which goes on at the new epoch. Its row
    /// is restamped, the others go, this node's cached copies are dropped and
    /// the live sockets holding one are told. Returns how many sessions ended.
    ///
    /// The epoch is already raised in the database, so another node refuses
    /// an older session at its next read of it; a cached copy there lives out
    /// its freshness ([`CACHE_TTL_SECONDS`]), as after a logout.
    pub async fn bump_epoch(
        &self,
        changed: CredentialsChanged,
        keep: Option<&str>,
    ) -> Result<usize> {
        let (user_id, epoch) = changed.into_parts();
        let ended = match &self.backend {
            Backend::Memory(sessions) => {
                let mut guard = sessions.write().map_err(|_| poisoned())?;
                let raised = guard.epoch_of(user_id).next().max(epoch);
                guard.epochs.insert(user_id, raised);
                let before = guard.entries.len();
                guard
                    .entries
                    .retain(|token, e| e.user.id() != user_id || Some(token.as_str()) == keep);
                if let Some(entry) = keep.and_then(|keep| guard.entries.get_mut(keep)) {
                    entry.epoch = raised;
                }
                before - guard.entries.len()
            }
            Backend::Database { catalog, .. } => {
                let mine = || Expr::col(COL_USER).eq(Expr::lit(user_id));
                let mut others = mine();
                if let Some(keep) = keep {
                    let update = Update {
                        table: SESSIONS_TABLE.to_owned(),
                        assignments: vec![Assignment::new(
                            COL_AUTH_EPOCH,
                            Expr::lit(epoch.value()),
                        )],
                        filter: Some(
                            mine().and(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(token_hash(keep)))),
                        ),
                        returning: Vec::new(),
                    };
                    run(catalog, Statement::from(update)).await?;
                    others = others.and(Expr::binary(
                        BinOp::Ne,
                        Expr::col(COL_TOKEN_HASH),
                        Expr::lit(token_hash(keep)),
                    ));
                }
                let delete = Delete {
                    returning: vec![Projection::expr(Expr::col(COL_TOKEN_HASH))],
                    ..Delete::from(SESSIONS_TABLE).filter(others)
                };
                let ended = rows(catalog, Statement::from(delete)).await?.len();
                // Last, so a request that re-reads finds the rows as they now
                // are: the kept one restamped, the others gone.
                let cached: Vec<String> = {
                    let cache = self.cache.lock().map_err(|_| poisoned())?;
                    cache
                        .iter()
                        .filter(|(_, cached)| cached.user.id() == user_id)
                        .map(|(token, _)| token.clone())
                        .collect()
                };
                for token in &cached {
                    self.invalidate(token)?;
                }
                ended
            }
        };
        self.ended(match keep {
            Some(keep) => SessionEnded::UserExcept {
                user: user_id,
                keep,
            },
            None => SessionEnded::User(user_id),
        });
        Ok(ended)
    }

    /// End every session there is: the rows (or the in-memory map) and this
    /// node's cached copies. What Clear all does once it has deleted every
    /// account, so a session resolved a moment before cannot outlive its user.
    pub async fn end_all_sessions(&self) -> Result<()> {
        match &self.backend {
            Backend::Memory(sessions) => {
                sessions.write().map_err(|_| poisoned())?.entries.clear();
            }
            Backend::Database { catalog, .. } => {
                self.invalidate_all()?;
                run(catalog, Statement::from(Delete::from(SESSIONS_TABLE))).await?;
            }
        }
        self.ended(SessionEnded::All);
        Ok(())
    }

    /// Drop this node's cached answer for `token`, without touching the
    /// database.
    ///
    /// The seam a bus-delivered logout (§16) will call on every other node: the
    /// row is already gone, what remains is the copies.
    pub fn invalidate(&self, token: &str) -> Result<()> {
        self.cache.lock().map_err(|_| poisoned())?.pop(token);
        Ok(())
    }

    /// Drop every cached session on this node. The blunt instrument, for a
    /// change too broad to name tokens for.
    pub(crate) fn invalidate_all(&self) -> Result<()> {
        self.cache.lock().map_err(|_| poisoned())?.clear();
        Ok(())
    }

    /// Drop every expired session. Optional housekeeping; `user_for` already
    /// purges lazily and `login` sweeps periodically.
    pub async fn sweep_expired(&self) -> Result<()> {
        match &self.backend {
            Backend::Memory(sessions) => {
                let now = Utc::now();
                sessions
                    .write()
                    .map_err(|_| poisoned())?
                    .entries
                    .retain(|_, e| e.expires_at > now);
                Ok(())
            }
            Backend::Database { catalog, .. } => sweep_sessions(catalog).await,
        }
    }

    /// The cached entry for `token`, if there is one.
    fn cache_get(&self, token: &str) -> Result<Option<Cached>> {
        if self.cache_ttl.is_zero() {
            return Ok(None);
        }
        Ok(self
            .cache
            .lock()
            .map_err(|_| poisoned())?
            .get(token)
            .cloned())
    }

    /// Cache `user` under `token` for one freshness TTL.
    /// Cache a resolved session, believed for the freshness TTL or until
    /// `changes_at` — when the user's effective role next changes — whichever
    /// is sooner.
    fn cache_put(
        &self,
        token: &str,
        user: &User,
        expires_at: DateTime<Utc>,
        changes_at: Option<DateTime<Utc>>,
    ) {
        if self.cache_ttl.is_zero() {
            return;
        }
        // A cache is an optimisation: a poisoned lock loses it rather than
        // failing the request that was only trying to be fast.
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(
                token.to_owned(),
                Cached {
                    user: user.clone(),
                    expires_at,
                    fresh_until: changes_at.map_or(Utc::now() + self.cache_ttl, |at| {
                        at.min(Utc::now() + self.cache_ttl)
                    }),
                },
            );
        }
    }
}

/// An LRU of `capacity` entries, never zero-sized (`LruCache` requires a
/// non-zero bound, and a store configured with nonsense should degrade to a
/// tiny cache rather than panic).
fn new_cache(capacity: usize) -> Mutex<LruCache<String, Cached>> {
    let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
    Mutex::new(LruCache::new(capacity))
}

/// Whether enough time has passed to sweep again, recording the attempt.
fn due_for_sweep(last_sweep: &Mutex<DateTime<Utc>>) -> Result<bool> {
    let mut last = last_sweep.lock().map_err(|_| poisoned())?;
    let now = Utc::now();
    if now - *last < Duration::seconds(SWEEP_INTERVAL_SECONDS) {
        return Ok(false);
    }
    *last = now;
    Ok(true)
}

/// Write one session row.
async fn insert_session(
    catalog: &Catalog,
    token_hash: &str,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
    epoch: AuthEpoch,
) -> Result<()> {
    let insert = Insert::row(
        SESSIONS_TABLE,
        vec![
            COL_TOKEN_HASH.to_owned(),
            COL_USER.to_owned(),
            COL_EXPIRES_AT.to_owned(),
            COL_AUTH_EPOCH.to_owned(),
        ],
        vec![
            Expr::lit(token_hash),
            Expr::lit(user_id),
            Expr::Lit(Value::Timestamp(expires_at)),
            Expr::lit(epoch.value()),
        ],
    );
    run(catalog, Statement::from(insert)).await
}

/// The user, expiry and epoch a session row names, if the row is there.
async fn select_session(
    catalog: &Catalog,
    token_hash: &str,
) -> Result<Option<(Uuid, DateTime<Utc>, AuthEpoch)>> {
    let select = Select::from(Source::table(SESSIONS_TABLE))
        .filter(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(token_hash)))
        .limit(1);
    let rows = rows(catalog, Statement::from(select)).await?;
    rows.first().map(session_from_row).transpose()
}

/// Delete one session row, reporting whether there was one.
async fn delete_session(catalog: &Catalog, token_hash: &str) -> Result<bool> {
    let delete = Delete {
        // `RETURNING` so logout is one statement rather than a read and a write:
        // whether a session existed is exactly what the deleted rows say.
        returning: vec![Projection::expr(Expr::col(COL_TOKEN_HASH))],
        ..Delete::from(SESSIONS_TABLE).filter(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(token_hash)))
    };
    Ok(!rows(catalog, Statement::from(delete)).await?.is_empty())
}

/// Delete every session row naming `user_id`, returning how many there were.
///
/// A free function for the same reason [`create_session`] is one: the rows are
/// the session, so a caller holding the database can end them without a running
/// server's [`SessionStore`] to ask. [`SessionStore::end_user_sessions`] is this
/// plus the local cache eviction.
pub async fn delete_sessions_for_user(catalog: &Catalog, user_id: Uuid) -> Result<usize> {
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_TOKEN_HASH))],
        ..Delete::from(SESSIONS_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user_id)))
    };
    Ok(rows(catalog, Statement::from(delete)).await?.len())
}

/// End the sessions of every account whose (effective) role is `role` and
/// that has no second factor on, except `except`'s: what making `role` require
/// a second factor does, since a session started before it was asked for none
/// (security review 2026-11, #11). Only accounts that hold a session are
/// looked at. Each such account's epoch goes up (crate::epoch) and its rows
/// go; the changes are returned for the session store
/// ([`SessionStore::bump_epoch`]).
pub async fn delete_sessions_lacking_second_factor(
    catalog: &Catalog,
    role: sc_types::Role,
    except: Option<Uuid>,
) -> Result<Vec<CredentialsChanged>> {
    let select = Select::from(Source::table(SESSIONS_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_USER))]);
    let mut holders: Vec<Uuid> = rows(catalog, Statement::from(select))
        .await?
        .iter()
        .filter_map(|row| match row.get(COL_USER) {
            Some(Value::Uuid(id)) => Some(*id),
            _ => None,
        })
        .collect();
    holders.sort_unstable();
    holders.dedup();
    let protected = crate::second_factor::second_factor_users(catalog).await?;
    let mut ended = Vec::new();
    for id in holders {
        if Some(id) == except || protected.contains(&id) {
            continue;
        }
        let Some(user) = crate::lookup::load_user(catalog, id).await? else {
            continue;
        };
        if user.role() == role {
            delete_sessions_for_user(catalog, id).await?;
            ended.push(crate::epoch::bump_auth_epoch(catalog, id).await?);
        }
    }
    Ok(ended)
}

/// Delete every session that has lapsed.
async fn sweep_sessions(catalog: &Catalog) -> Result<()> {
    let delete = Delete::from(SESSIONS_TABLE).filter(Expr::binary(
        BinOp::Le,
        Expr::col(COL_EXPIRES_AT),
        Expr::Lit(Value::Timestamp(Utc::now())),
    ));
    run(catalog, Statement::from(delete)).await
}

/// The two columns a lookup needs, read strictly: a missing or ill-typed column
/// in an `_fd_*` table is an error naming it, never a silent default.
fn session_from_row(row: &Row) -> Result<(Uuid, DateTime<Utc>, AuthEpoch)> {
    let user_id = match row.get(COL_USER) {
        Some(Value::Uuid(u)) => *u,
        other => return Err(bad_column(COL_USER, "a uuid", other)),
    };
    let expires_at = match row.get(COL_EXPIRES_AT) {
        Some(Value::Timestamp(ts)) => *ts,
        other => return Err(bad_column(COL_EXPIRES_AT, "a timestamp", other)),
    };
    Ok((
        user_id,
        expires_at,
        AuthEpoch::of_column(row.get(COL_AUTH_EPOCH))?,
    ))
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{SESSIONS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    rows(catalog, statement).await?;
    Ok(())
}

/// Run a statement and collect its rows.
async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await
}

fn poisoned() -> Error {
    Error::msg("session store lock poisoned")
}

/// A fresh, unguessable session token for `scope`: 256 bits from two v4
/// UUIDs, hex-encoded, after [`APP_TOKEN_PREFIX`] and the application's id for
/// an application's (module docs).
fn new_token(scope: SessionScope) -> Result<String> {
    let prefix = match scope {
        SessionScope::Admin => String::new(),
        SessionScope::App(app) => format!("{APP_TOKEN_PREFIX}{}-", app.simple()),
        SessionScope::AnyApp => {
            return Err(Error::invalid(
                "a new application session names its application",
            ));
        }
    };
    Ok(format!(
        "{prefix}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    ))
}

/// The key a changeset records for the session `token` belongs to (§14.12):
/// derived from the token's hash, and never the hash itself, which is
/// `_fd_sessions`' primary key and would let a reader of the log find the
/// session row. Stable for the session's life, so an undo can step back
/// through one session's changesets.
pub fn changeset_session_key(token: &str) -> String {
    token_hash(&format!("changeset-session:{}", token_hash(token)))
}

/// [`changeset_session_key`] for a bearer-token caller (§13.6), derived from
/// the token's public id.
pub fn changeset_token_key(token_id: Uuid) -> String {
    token_hash(&format!("changeset-token:{token_id}"))
}

/// What the database stores in place of the token: its SHA-256, hex-encoded.
///
/// Fast on purpose — see the module docs. The token is uniform randomness, so
/// there is no guessing to slow down.
fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use sc_catalog::DataFieldKind;

    use super::*;
    use sc_types::Role;

    fn admin() -> User {
        User::new(Uuid::new_v4(), Role::ADMIN)
    }

    #[tokio::test]
    async fn login_returns_a_token_that_resolves_to_the_user() {
        let store = SessionStore::default();
        let user = admin();
        let token = store.login(user.clone()).await.unwrap();
        assert_eq!(token.len(), 64); // two hex UUIDs
        assert_eq!(
            store.user_for(&token).await.unwrap().map(|u| u.into_user()),
            Some(user)
        );
    }

    #[tokio::test]
    async fn a_session_can_be_given_its_own_lifetime() {
        let store = SessionStore::default();
        let token = store.login_for(admin(), Duration::zero()).await.unwrap();
        assert_eq!(store.user_for(&token).await.unwrap(), None);
        let token = store.login_for(admin(), Duration::hours(1)).await.unwrap();
        assert!(store.user_for(&token).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn unknown_token_resolves_to_none() {
        let store = SessionStore::default();
        assert_eq!(store.user_for("nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn logout_ends_the_session() {
        let store = SessionStore::default();
        let token = store.login(admin()).await.unwrap();
        assert!(store.logout(&token).await.unwrap());
        assert_eq!(store.user_for(&token).await.unwrap(), None);
        assert!(!store.logout(&token).await.unwrap()); // already gone
    }

    /// A listener that writes down what it was told.
    #[derive(Default)]
    struct Heard(Mutex<Vec<String>>);

    impl SessionListener for Heard {
        fn session_ended(&self, ended: &SessionEnded<'_>) {
            let line = match ended {
                SessionEnded::Token(token) => format!("token {token}"),
                SessionEnded::User(user) => format!("user {user}"),
                SessionEnded::UserExcept { user, keep } => format!("user {user} but {keep}"),
                SessionEnded::All => "all".to_owned(),
            };
            self.0.lock().unwrap().push(line);
        }
    }

    #[tokio::test]
    async fn a_listener_hears_every_way_a_session_ends_and_is_held_weakly() {
        let store = SessionStore::default();
        let heard = Arc::new(Heard::default());
        let listener: Arc<dyn SessionListener> = heard.clone();
        store.add_listener(Arc::downgrade(&listener));

        let user = admin();
        let token = store.login(user.clone()).await.unwrap();
        store.logout(&token).await.unwrap();
        store.end_user_sessions(user.id()).await.unwrap();
        store.end_all_sessions().await.unwrap();
        assert_eq!(
            *heard.0.lock().unwrap(),
            vec![
                format!("token {token}"),
                format!("user {}", user.id()),
                "all".to_owned()
            ]
        );

        // Gone listeners are skipped, not kept alive and not called.
        drop(listener);
        drop(heard);
        store.end_all_sessions().await.unwrap();
        store.add_listener(Arc::downgrade(
            &(Arc::new(Heard::default()) as Arc<dyn SessionListener>),
        ));
        assert_eq!(
            store.listeners.read().unwrap().len(),
            1,
            "the dead entry was dropped on the next registration"
        );
    }

    #[tokio::test]
    async fn expired_session_is_not_returned() {
        // A zero-length TTL means every session is already expired on lookup.
        let store = SessionStore::with_ttl(Duration::zero());
        let token = store.login(admin()).await.unwrap();
        assert_eq!(store.user_for(&token).await.unwrap(), None);
    }

    #[tokio::test]
    async fn ending_a_users_sessions_ends_all_of_them_and_nobody_elses() {
        let store = SessionStore::default();
        let user = admin();
        let phone = store.login(user.clone()).await.unwrap();
        let laptop = store.login(user.clone()).await.unwrap();
        let other = store.login(admin()).await.unwrap();

        assert_eq!(store.end_user_sessions(user.id()).await.unwrap(), 2);
        assert_eq!(store.user_for(&phone).await.unwrap(), None);
        assert_eq!(store.user_for(&laptop).await.unwrap(), None);
        assert!(store.user_for(&other).await.unwrap().is_some());

        // Idempotent: a user with nothing to end ends nothing.
        assert_eq!(store.end_user_sessions(user.id()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn tokens_are_distinct_across_logins() {
        let store = SessionStore::default();
        let a = store.login(admin()).await.unwrap();
        let b = store.login(admin()).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn a_session_says_which_host_issued_it() {
        let store = SessionStore::memory();
        let admin_token = store.login(admin()).await.unwrap();
        let app = Uuid::new_v4();
        let app_token = store
            .login_in(admin(), SessionScope::App(app))
            .await
            .unwrap();
        assert_eq!(SessionScope::of_token(&admin_token), SessionScope::Admin);
        assert_eq!(SessionScope::of_token(&app_token), SessionScope::App(app));
        // An application token from before tokens named their application is
        // an application's, but no application's in particular.
        let unnamed = format!(
            "{APP_TOKEN_PREFIX}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        assert_eq!(SessionScope::of_token(&unnamed), SessionScope::AnyApp);
        assert!(SessionScope::AnyApp.is_app() && !SessionScope::Admin.is_app());
        // And none is minted any more.
        assert!(store.login_in(admin(), SessionScope::AnyApp).await.is_err());
        // The application inside the token is hashed with it: pointing it at
        // another application is a token nobody issued.
        let moved = app_token.replace(
            &app.simple().to_string(),
            &Uuid::new_v4().simple().to_string(),
        );
        assert!(store.user_for(&moved).await.unwrap().is_none());
        // Both resolve: the scope is the server's to apply, not a dead token.
        assert!(store.user_for(&app_token).await.unwrap().is_some());
        // A token minted before scopes existed — 64 hex digits — reads as the
        // admin host's, so an upgrade signs nobody out.
        let legacy = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        assert_eq!(SessionScope::of_token(&legacy), SessionScope::Admin);
        // And the prefix is inside what is hashed: stripping it is a token
        // nobody issued.
        let stripped = app_token.trim_start_matches(APP_TOKEN_PREFIX);
        assert!(store.user_for(stripped).await.unwrap().is_none());
    }

    #[test]
    fn the_token_is_hashed_before_it_is_stored() {
        // The known SHA-256 of the empty string: this is the standard hash and
        // not some homegrown digest, which is the whole point of using it.
        assert_eq!(
            token_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let token = new_token(SessionScope::Admin).unwrap();
        let hash = token_hash(&token);
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, token); // what is stored is never what is presented
        assert_eq!(hash, token_hash(&token)); // and it is deterministic
    }

    #[test]
    fn schema_names_the_user_and_the_expiry_without_constraining_the_user() {
        let fields = session_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).expect(n);

        let pk = by_name(COL_TOKEN_HASH);
        assert!(pk.primary_key && pk.required);
        assert!(by_name(COL_USER).required);
        assert!(by_name(COL_EXPIRES_AT).required);

        // Not a foreign key, on purpose: the schema layer has no `ON DELETE`
        // action, so one here would stop an administrator deleting a user who
        // happens to be signed in. See `session_fields`.
        assert!(
            fields
                .iter()
                .all(|f| !matches!(f.kind, DataFieldKind::Key { .. })),
            "a session must not pin the user row it names"
        );
    }

    #[tokio::test]
    async fn a_credential_change_ends_the_other_sessions_and_keeps_the_one_that_made_it() {
        let store = SessionStore::default();
        let heard = Arc::new(Heard::default());
        let listener: Arc<dyn SessionListener> = heard.clone();
        store.add_listener(Arc::downgrade(&listener));
        let user = admin();
        let here = store.login(user.clone()).await.unwrap();
        let there = store.login(user.clone()).await.unwrap();
        let other = store.login(admin()).await.unwrap();

        let changed = CredentialsChanged::new(user.id(), AuthEpoch::INITIAL.next());
        assert_eq!(store.bump_epoch(changed, Some(&here)).await.unwrap(), 1);
        assert!(store.user_for(&here).await.unwrap().is_some());
        assert_eq!(store.user_for(&there).await.unwrap(), None);
        assert!(store.user_for(&other).await.unwrap().is_some());
        assert_eq!(
            *heard.0.lock().unwrap(),
            vec![format!("user {} but {here}", user.id())]
        );

        // A session started after it is at the new epoch, and lives.
        let after = store.login(user.clone()).await.unwrap();
        assert!(store.user_for(&after).await.unwrap().is_some());

        // With nothing kept, every session of the account ends.
        let changed = CredentialsChanged::new(user.id(), AuthEpoch::INITIAL.next().next());
        assert_eq!(store.bump_epoch(changed, None).await.unwrap(), 2);
        assert_eq!(store.user_for(&here).await.unwrap(), None);
    }

    #[test]
    fn a_proof_for_one_account_does_not_sign_in_another() {
        let user = admin();
        let proof = crate::signin::SecondFactorProof::new(Uuid::new_v4());
        assert!(
            crate::signin::PasswordVerified::resumed(user, AuthEpoch::INITIAL)
                .complete(crate::signin::SecondFactor::Proof(proof))
                .is_err()
        );
    }

    #[test]
    fn a_sweep_is_rate_limited_to_one_per_interval() {
        let last = Mutex::new(DateTime::<Utc>::MIN_UTC);
        assert!(due_for_sweep(&last).unwrap()); // first login sweeps
        assert!(!due_for_sweep(&last).unwrap()); // the next one does not
    }
}
