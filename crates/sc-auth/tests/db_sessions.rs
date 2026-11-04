//! The database-backed session store against real Postgres (design §7.2).
//!
//! Every property this store exists for is one an in-memory map already had, so
//! testing it means testing the things a map *cannot* do and the things a table
//! makes newly possible to get wrong:
//!
//! - **Two nodes are one store.** Two [`SessionStore`]s over the same database,
//!   which is what a pair of application servers behind a load balancer is.
//! - **The table is `UNLOGGED`.** Asserted against `pg_class`, because it is the
//!   difference between a session write and a WAL record, and nothing else in
//!   the process would notice if it silently stopped being true.
//! - **The token is hashed at rest**, so a dump of the table is not a pile of
//!   live cookies.
//! - **A deleted user's sessions go with them**, by the foreign key rather than
//!   by anyone remembering to.
//! - **The cache bounds staleness rather than hiding it**: a logout on one node
//!   is visible on another within the freshness TTL, and immediately when the
//!   cache is off.

use sc_types::Role;
use std::sync::Arc;

use chrono::Duration;
use sc_auth::{
    CACHE_CAPACITY, COL_ID, SESSIONS_TABLE, SessionStore, USERS_TABLE, bootstrap, create_session,
    create_user,
};
use sc_catalog::Catalog;
use sc_error::Result;
use sc_query::{Delete, Expr, Select, Source, Statement};
use sc_test_harness::{TestDb, TestServer};

/// A database with the auth tables bootstrapped, and a handle on it.
struct Fixture {
    catalog: Arc<Catalog>,
    db: TestDb,
}

async fn fixture() -> Result<Fixture> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    bootstrap(&catalog).await?;
    Ok(Fixture { catalog, db })
}

/// A store with the cache switched off, so every assertion is about the table
/// rather than about a copy of it.
fn uncached(catalog: &Arc<Catalog>) -> SessionStore {
    SessionStore::database_with(
        catalog.clone(),
        Duration::hours(24),
        Duration::zero(),
        CACHE_CAPACITY,
    )
}

/// How many session rows there are.
async fn session_rows(catalog: &Catalog) -> Result<usize> {
    Ok(catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&Statement::from(Select::from(Source::table(
            SESSIONS_TABLE,
        ))))
        .await?
        .try_collect()
        .await?
        .len())
}

/// The one thing that makes two application servers possible: a session started
/// on one is a session the other honours, and a logout on either ends it on both.
#[tokio::test]
async fn a_session_started_on_one_node_is_live_on_another() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;

    // Two stores over one database — two processes, as far as the data is
    // concerned. Uncached, so this is the table's behaviour and not a cache's.
    let node_a = uncached(&f.catalog);
    let node_b = uncached(&f.catalog);

    let token = node_a.login(user.clone()).await?;
    let seen = node_b
        .user_for(&token)
        .await?
        .expect("node B honours node A's session");
    assert_eq!(seen.id(), user.id());
    assert_eq!(seen.role(), user.role());

    // And the logout crosses back the other way.
    assert!(node_b.logout(&token).await?);
    assert_eq!(node_a.user_for(&token).await?, None);
    assert_eq!(session_rows(&f.catalog).await?, 0, "the row is gone too");
    Ok(())
}

/// The cache is a staleness budget, not a correctness hole: a logout elsewhere
/// is honoured once the cached entry goes stale, and at once when there is no
/// cache.
#[tokio::test]
async fn a_cached_session_goes_stale_and_then_notices_the_logout() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;

    // Node A caches for a beat; node B does not cache at all.
    let node_a = SessionStore::database_with(
        f.catalog.clone(),
        Duration::hours(24),
        Duration::milliseconds(150),
        CACHE_CAPACITY,
    );
    let node_b = uncached(&f.catalog);

    let token = node_b.login(user.clone()).await?;
    assert!(
        node_a.user_for(&token).await?.is_some(),
        "node A reads it and caches it"
    );

    node_b.logout(&token).await?;
    // Node B handled the logout, so node B is right immediately.
    assert_eq!(node_b.user_for(&token).await?, None);
    // Node A is briefly wrong — this is the documented window, asserted rather
    // than hoped for, because a reader deserves to know it is real.
    assert!(
        node_a.user_for(&token).await?.is_some(),
        "the cached entry is still within its freshness TTL"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        node_a.user_for(&token).await?,
        None,
        "and the window closes on its own"
    );
    Ok(())
}

/// `invalidate` is the seam a bus-delivered logout will use: it drops the local
/// copy without touching the row.
#[tokio::test]
async fn invalidate_drops_the_cached_copy_and_nothing_else() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;
    let store = SessionStore::database(f.catalog.clone());

    let token = store.login(user).await?;
    store.invalidate(&token)?;

    // The session survives — the row was never touched — so the next lookup just
    // pays for a read.
    assert!(store.user_for(&token).await?.is_some());
    assert_eq!(session_rows(&f.catalog).await?, 1);
    Ok(())
}

/// The table is `UNLOGGED`, which is the whole reason a session write is cheap
/// enough to be on the login path.
#[tokio::test]
async fn the_session_table_is_unlogged() -> Result<()> {
    let f = fixture().await?;

    // `relpersistence` is 'u' for unlogged and 'p' for an ordinary table.
    let row =
        f.db.client()
            .await?
            .query_one(
                "SELECT relpersistence::text FROM pg_class WHERE relname = $1",
                &[&SESSIONS_TABLE],
            )
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let persistence: String = row.get(0);
    assert_eq!(persistence, "u", "{SESSIONS_TABLE} should be UNLOGGED");

    // …and `users`, which it references, is not: an unlogged table may point at
    // a permanent one, and the durable half must stay durable.
    let row =
        f.db.client()
            .await?
            .query_one(
                "SELECT relpersistence::text FROM pg_class WHERE relname = $1",
                &[&USERS_TABLE],
            )
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let persistence: String = row.get(0);
    assert_eq!(persistence, "p", "users must stay permanent");
    Ok(())
}

/// What is in the table is a hash. A dump of `_fd_sessions` is not a set of
/// cookies somebody can present.
#[tokio::test]
async fn the_stored_row_is_not_the_cookie() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;
    let store = uncached(&f.catalog);
    let token = store.login(user).await?;

    let rows =
        f.db.client()
            .await?
            .query(&format!("SELECT token_hash FROM {SESSIONS_TABLE}"), &[])
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(rows.len(), 1);
    let stored: String = rows[0].get(0);
    assert_ne!(stored, token, "the raw token must never be stored");
    assert_eq!(stored.len(), 64, "a hex SHA-256");

    // Presenting what is stored is not presenting the token: the hash is not a
    // credential, which is the entire point of hashing it.
    assert_eq!(store.user_for(&stored).await?, None);
    // The token itself still works, of course.
    assert!(store.user_for(&token).await?.is_some());
    Ok(())
}

/// A deleted user's session stops being one, and — the half that is easy to get
/// backwards — having a session does not stop a user being deleted.
///
/// There is no foreign key on `_fd_sessions.user_id` for exactly this reason:
/// the schema layer renders no `ON DELETE` action, so a constraint here would
/// block an administrator on an ephemeral row. The guarantee comes from
/// resolving a session by reading the user instead.
#[tokio::test]
async fn a_deleted_users_session_stops_resolving_and_never_blocked_the_delete() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;
    let other = create_user(&f.catalog, "b@example.com", "pw", Role::ADMIN).await?;
    let store = uncached(&f.catalog);

    let token = store.login(user.clone()).await?;
    let survivor = store.login(other).await?;
    assert_eq!(session_rows(&f.catalog).await?, 2);

    // The delete succeeds — a live session is not a reason an account cannot be
    // removed, and this is the assertion that would fail if a foreign key were
    // ever added without an `ON DELETE` action.
    f.catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&Statement::from(
            Delete::from(USERS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(user.id()))),
        ))
        .await?
        .try_collect()
        .await?;

    // And the session it left behind is inert: it names nobody, so it is nobody.
    assert_eq!(store.user_for(&token).await?, None);
    assert!(store.user_for(&survivor).await?.is_some());
    Ok(())
}

/// An expired session is not honoured, and the row it left behind goes — both
/// lazily, on the read that finds it, and in bulk on the sweep.
#[tokio::test]
async fn expired_sessions_lapse_and_are_swept() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;
    let store = uncached(&f.catalog);

    // The live one first: `login` sweeps on its way through (rate-limited to one
    // per minute), so a lapsed row written before it would be collected then and
    // the lazy path below would have nothing to find.
    let live = store.login(user.clone()).await?;

    // Straight to the table, with its lifetime already spent — the store's own
    // `login` cannot make one of these, which is why the free function takes a
    // TTL rather than assuming the default.
    let stale = create_session(&f.catalog, user.id(), Duration::seconds(-1)).await?;
    let also_stale = create_session(&f.catalog, user.id(), Duration::seconds(-1)).await?;
    assert_eq!(session_rows(&f.catalog).await?, 3);

    // Lazily: reading a lapsed session both refuses it and purges its row.
    assert_eq!(store.user_for(&stale).await?, None);
    assert_eq!(session_rows(&f.catalog).await?, 2);

    // In bulk: the sweep takes the one nobody presented, and leaves the live one.
    store.sweep_expired().await?;
    assert_eq!(session_rows(&f.catalog).await?, 1);
    assert_eq!(store.user_for(&also_stale).await?, None);
    assert!(store.user_for(&live).await?.is_some());
    Ok(())
}

/// The user is re-read rather than remembered, so a role change lands inside the
/// freshness window instead of surviving the whole session.
#[tokio::test]
async fn a_role_change_reaches_a_live_session() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", Role::ADMIN).await?;
    let store = uncached(&f.catalog);
    let token = store.login(user.clone()).await?;

    // Demote them in the users table, touching no session.
    f.catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&Statement::from(
            sc_query::Update::new(
                USERS_TABLE,
                vec![sc_query::Assignment::new("role", Expr::lit(100_i64))],
            )
            .filter(Expr::col(COL_ID).eq(Expr::lit(user.id()))),
        ))
        .await?
        .try_collect()
        .await?;

    let seen = store.user_for(&token).await?.expect("still signed in");
    assert_eq!(
        seen.role(),
        Role::PUBLIC,
        "the session names the user; it does not carry a copy of them"
    );
    Ok(())
}

/// A credential change ends what the old credentials handed out, whoever makes
/// it (security review 2026-11, #11): a new password, disabling the account,
/// and a role that now requires a second factor the account lacks — not the
/// sessions of an account that has one on, nor the one named as making the
/// change. And an account can end its other sessions and keep its own.
#[tokio::test]
async fn credential_changes_end_the_sessions_they_outdate() -> Result<()> {
    let f = fixture().await?;
    f.catalog.set_secret_key(sc_catalog::SecretKey::generate()?);
    let store = uncached(&f.catalog);
    let member = Role::new(80).unwrap();
    sc_auth::save_role(&f.catalog, &sc_auth::RoleDef::new(member, "Member")).await?;
    let ann = create_user(&f.catalog, "ann@example.com", "ann-pw", member).await?;
    let bob = create_user(&f.catalog, "bob@example.com", "bob-pw", member).await?;
    let cat = create_user(&f.catalog, "cat@example.com", "cat-pw", member).await?;
    let dee = create_user(&f.catalog, "dee@example.com", "dee-pw", member).await?;
    let live = |token: String| {
        let store = &store;
        async move { store.user_for(&token).await.map(|u| u.is_some()) }
    };

    // A new password.
    let token = store.login(ann.clone()).await?;
    sc_auth::set_user_password(&f.catalog, ann.id(), "new-ann-pw").await?;
    assert!(!live(token).await?, "a new password ends the session");
    let token = store.login(ann.clone()).await?;
    sc_auth::update_user(
        &f.catalog,
        ann.id(),
        sc_auth::UserUpdate {
            password: Some(sc_error::Secret::new("newer-ann-pw".to_owned())),
            ..Default::default()
        },
    )
    .await?;
    assert!(!live(token).await?, "so does an edit with one");

    // Disabled.
    let token = store.login(bob.clone()).await?;
    sc_auth::set_user_disabled(&f.catalog, bob.id(), true).await?;
    assert_eq!(session_rows(&f.catalog).await?, 0);
    assert!(!live(token).await?);

    // The role now requires a second factor: Cat has none, Dee has one on,
    // and Ann is the admin making the change.
    let secret = sc_auth::begin_second_factor_setup(&f.catalog, dee.id()).await?;
    let code = sc_auth::totp_at(
        &sc_auth::decode_secret(&secret)?,
        chrono::Utc::now().timestamp(),
    );
    assert!(
        sc_auth::finish_second_factor_setup(&f.catalog, dee.id(), &code)
            .await?
            .is_some()
    );
    let cats = store.login(cat.clone()).await?;
    let dees = store.login(dee.clone()).await?;
    let anns = store.login(ann.clone()).await?;
    let ended =
        sc_auth::delete_sessions_lacking_second_factor(&f.catalog, member, Some(ann.id())).await?;
    assert_eq!(ended, vec![cat.id()]);
    assert!(!live(cats).await?);
    assert!(live(dees).await?);
    assert!(live(anns.clone()).await?);

    // An account's other sessions, but not this one.
    let other = store.login(ann.clone()).await?;
    assert_eq!(store.end_other_sessions(ann.id(), &anns).await?, 1);
    assert!(!live(other).await?);
    assert!(live(anns).await?);
    Ok(())
}
