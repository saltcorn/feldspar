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
use sc_query::{BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::lookup::load_user;
use crate::user::User;

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
    ]
}

/// Ensure the session table exists.
///
/// **Must run after the users table**, which it references.
/// [`bootstrap`](crate::bootstrap) does them in that order. Idempotent, like
/// every other bootstrap — including across the truncation an unclean shutdown
/// does to an unlogged table, which empties it without dropping it.
pub async fn bootstrap_sessions(catalog: &Catalog) -> Result<Table> {
    if let Some(existing) = catalog.get(SESSIONS_TABLE)? {
        return Ok(existing);
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
/// account does, and expires like any other.
pub async fn create_session(catalog: &Catalog, user_id: Uuid, ttl: Duration) -> Result<String> {
    let token = new_token();
    let expires_at = Utc::now() + ttl;
    insert_session(catalog, &token_hash(&token), user_id, expires_at).await?;
    Ok(token)
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
}

/// Where sessions actually live.
enum Backend {
    /// A plain map in this process — no database, no sharing (see the module
    /// docs: a test seam).
    Memory(RwLock<std::collections::HashMap<String, Entry>>),
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
    /// Every session there is.
    All,
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
    pub fn with_ttl(ttl: Duration) -> SessionStore {
        SessionStore {
            ttl,
            cache_ttl: Duration::seconds(CACHE_TTL_SECONDS),
            backend: Backend::Memory(RwLock::new(std::collections::HashMap::new())),
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

    /// Start a session for `user` and return its token (to be set as a cookie).
    pub async fn login(&self, user: User) -> Result<String> {
        let token = new_token();
        let expires_at = Utc::now() + self.ttl;
        match &self.backend {
            Backend::Memory(entries) => {
                entries
                    .write()
                    .map_err(|_| poisoned())?
                    .insert(token.clone(), Entry { user, expires_at });
            }
            Backend::Database {
                catalog,
                last_sweep,
            } => {
                insert_session(catalog, &token_hash(&token), user.id, expires_at).await?;
                // Housekeeping rides along with the one write sessions do, but
                // at most once a minute: a login is common and a full-table
                // delete is not free.
                if due_for_sweep(last_sweep)? {
                    sweep_sessions(catalog).await?;
                }
                self.cache_put(&token, &user, expires_at);
            }
        }
        Ok(token)
    }

    /// The user for a session token, if the token is known and unexpired. An
    /// expired token is treated as absent (and eagerly purged).
    pub async fn user_for(&self, token: &str) -> Result<Option<User>> {
        let now = Utc::now();
        match &self.backend {
            Backend::Memory(entries) => {
                // Fast path: a valid session under a read lock.
                {
                    let guard = entries.read().map_err(|_| poisoned())?;
                    match guard.get(token) {
                        None => return Ok(None),
                        Some(entry) if entry.expires_at > now => {
                            return Ok(Some(entry.user.clone()));
                        }
                        Some(_) => {} // expired — fall through to purge
                    }
                }
                entries.write().map_err(|_| poisoned())?.remove(token);
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

                let Some((user_id, expires_at)) =
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
                // Reading the user is what makes a deleted user's session stop
                // being one — see `session_fields`, which leaves the foreign key
                // out precisely because this check does not need it. A disabled
                // user is the same case: the row is there, the account is not.
                let Some(user) = load_user(catalog, user_id).await? else {
                    self.invalidate(token)?;
                    return Ok(None);
                };
                if user.is_disabled() {
                    self.invalidate(token)?;
                    return Ok(None);
                }
                self.cache_put(token, &user, expires_at);
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
            Backend::Memory(entries) => entries
                .write()
                .map_err(|_| poisoned())?
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
            Backend::Memory(entries) => {
                let mut guard = entries.write().map_err(|_| poisoned())?;
                let before = guard.len();
                guard.retain(|_, e| e.user.id != user_id);
                Ok(before - guard.len())
            }
            Backend::Database { catalog, .. } => {
                // The cache is keyed by token, so the tokens to forget are found
                // by asking the cached users who they are — the rows are gone
                // either way, this is only about the copies in front of them.
                let stale: Vec<String> = {
                    let cache = self.cache.lock().map_err(|_| poisoned())?;
                    cache
                        .iter()
                        .filter(|(_, cached)| cached.user.id == user_id)
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

    /// End every session there is: the rows (or the in-memory map) and this
    /// node's cached copies. What Clear all does once it has deleted every
    /// account, so a session resolved a moment before cannot outlive its user.
    pub async fn end_all_sessions(&self) -> Result<()> {
        match &self.backend {
            Backend::Memory(entries) => {
                entries.write().map_err(|_| poisoned())?.clear();
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
    pub fn invalidate_all(&self) -> Result<()> {
        self.cache.lock().map_err(|_| poisoned())?.clear();
        Ok(())
    }

    /// Drop every expired session. Optional housekeeping; `user_for` already
    /// purges lazily and `login` sweeps periodically.
    pub async fn sweep_expired(&self) -> Result<()> {
        match &self.backend {
            Backend::Memory(entries) => {
                let now = Utc::now();
                entries
                    .write()
                    .map_err(|_| poisoned())?
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
    fn cache_put(&self, token: &str, user: &User, expires_at: DateTime<Utc>) {
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
                    fresh_until: Utc::now() + self.cache_ttl,
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
) -> Result<()> {
    let insert = Insert::row(
        SESSIONS_TABLE,
        vec![
            COL_TOKEN_HASH.to_owned(),
            COL_USER.to_owned(),
            COL_EXPIRES_AT.to_owned(),
        ],
        vec![
            Expr::lit(token_hash),
            Expr::lit(user_id),
            Expr::Lit(Value::Timestamp(expires_at)),
        ],
    );
    run(catalog, Statement::from(insert)).await
}

/// The user and expiry a session row names, if the row is there.
async fn select_session(
    catalog: &Catalog,
    token_hash: &str,
) -> Result<Option<(Uuid, DateTime<Utc>)>> {
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
fn session_from_row(row: &Row) -> Result<(Uuid, DateTime<Utc>)> {
    let user_id = match row.get(COL_USER) {
        Some(Value::Uuid(u)) => *u,
        other => return Err(bad_column(COL_USER, "a uuid", other)),
    };
    let expires_at = match row.get(COL_EXPIRES_AT) {
        Some(Value::Timestamp(ts)) => *ts,
        other => return Err(bad_column(COL_EXPIRES_AT, "a timestamp", other)),
    };
    Ok((user_id, expires_at))
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
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

fn poisoned() -> Error {
    Error::msg("session store lock poisoned")
}

/// A fresh, unguessable session token: 256 bits from two v4 UUIDs, hex-encoded.
fn new_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
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
    use crate::users::ROLE_ADMIN;

    fn admin() -> User {
        User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap()
    }

    #[tokio::test]
    async fn login_returns_a_token_that_resolves_to_the_user() {
        let store = SessionStore::default();
        let user = admin();
        let token = store.login(user.clone()).await.unwrap();
        assert_eq!(token.len(), 64); // two hex UUIDs
        assert_eq!(store.user_for(&token).await.unwrap(), Some(user));
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
        store.end_user_sessions(user.id).await.unwrap();
        store.end_all_sessions().await.unwrap();
        assert_eq!(
            *heard.0.lock().unwrap(),
            vec![
                format!("token {token}"),
                format!("user {}", user.id),
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

        assert_eq!(store.end_user_sessions(user.id).await.unwrap(), 2);
        assert_eq!(store.user_for(&phone).await.unwrap(), None);
        assert_eq!(store.user_for(&laptop).await.unwrap(), None);
        assert!(store.user_for(&other).await.unwrap().is_some());

        // Idempotent: a user with nothing to end ends nothing.
        assert_eq!(store.end_user_sessions(user.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn tokens_are_distinct_across_logins() {
        let store = SessionStore::default();
        let a = store.login(admin()).await.unwrap();
        let b = store.login(admin()).await.unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_token_is_hashed_before_it_is_stored() {
        // The known SHA-256 of the empty string: this is the standard hash and
        // not some homegrown digest, which is the whole point of using it.
        assert_eq!(
            token_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let token = new_token();
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

    #[test]
    fn a_sweep_is_rate_limited_to_one_per_interval() {
        let last = Mutex::new(DateTime::<Utc>::MIN_UTC);
        assert!(due_for_sweep(&last).unwrap()); // first login sweeps
        assert!(!due_for_sweep(&last).unwrap()); // the next one does not
    }
}
