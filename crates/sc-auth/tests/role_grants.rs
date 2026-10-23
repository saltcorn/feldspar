//! Role grants against real Postgres (TODO-payments P3.1, design C2).
//!
//! A grant is a row, and a user's role is the most privileged of the base role
//! and the grants in force. What is asserted here is the store and every way of
//! loading a user honouring it:
//!
//! - **the effective role** is what `load_user`, a sign-in and a session serve,
//!   while `users.role` — and `load_base_user` — stay what the admin set;
//! - **a grant that ends lapses with no write**: the row is untouched, and a
//!   session cached for a minute notices at the expiry, not a minute later;
//! - **one grant per source and reference**: a renewal extends it, a revoked
//!   one comes back, and a `once` grant never comes back after it has existed;
//! - deleting the user or a role it was granted cleans up after itself.

use std::sync::Arc;

use chrono::{Duration, Utc};
use sc_auth::{
    CACHE_CAPACITY, NewRoleGrant, ROLE_ADMIN, ROLE_GRANTS_TABLE, Role, SessionStore, authenticate,
    bootstrap, create_user, delete_role, delete_user, effective_role, find_role_grant, grant_role,
    grant_role_once, list_role_grants, load_base_user, load_user, revoke_role_grant, save_role,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use uuid::Uuid;

struct Fixture {
    catalog: Arc<Catalog>,
    _db: TestDb,
}

async fn fixture() -> Result<Fixture> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    bootstrap(&catalog).await?;
    for (role, name) in [(40, "Staff"), (60, "Pro"), (70, "Basic"), (80, "Member")] {
        save_role(&catalog, &Role::new(role, name)).await?;
    }
    Ok(Fixture { catalog, _db: db })
}

fn new_grant(user: Uuid, role: u8, source_ref: &str) -> NewRoleGrant {
    NewRoleGrant {
        user_id: user,
        role,
        source: "test".into(),
        source_ref: source_ref.into(),
        starts_at: None,
        expires_at: Some(Utc::now() + Duration::days(30)),
        note: None,
    }
}

#[tokio::test]
async fn the_table_is_an_ordinary_owner_readable_one() -> Result<()> {
    let f = fixture().await?;
    let table = f.catalog.require(ROLE_GRANTS_TABLE)?;
    assert!(!table.is_system(), "an application reads it like any table");
    assert_eq!(table.access.min_role_read, ROLE_ADMIN);
    assert_eq!(table.access.min_role_write, ROLE_ADMIN);
    assert_eq!(
        table.ownership.as_ref().map(|f| f.source()),
        Some(sc_auth::ROLE_GRANTS_OWNERSHIP),
        "{:?}",
        table.ownership_error
    );
    // Bootstrapping again changes nothing.
    bootstrap(&f.catalog).await?;
    assert_eq!(f.catalog.require(ROLE_GRANTS_TABLE)?, table);
    Ok(())
}

#[tokio::test]
async fn a_grant_in_force_is_the_role_every_load_serves_and_the_base_role_stays() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "m@example.com", "pw", 80).await?;
    assert_eq!(load_user(&f.catalog, user.id).await?.unwrap().role, 80);

    grant_role(&f.catalog, new_grant(user.id, 60, "sub_1")).await?;
    assert_eq!(load_user(&f.catalog, user.id).await?.unwrap().role, 60);
    assert_eq!(
        authenticate(&f.catalog, "m@example.com", "pw")
            .await?
            .unwrap()
            .role,
        60,
        "a sign-in serves the effective role"
    );
    assert_eq!(
        load_base_user(&f.catalog, user.id).await?.unwrap().role,
        80,
        "users.role is untouched"
    );

    // A less privileged grant beside it changes nothing; the most privileged wins.
    grant_role(&f.catalog, new_grant(user.id, 70, "trial")).await?;
    assert_eq!(effective_role(&f.catalog, user.id, 80).await?.role, 60);

    // A grant never lowers a role: a staff member with a Basic grant is staff.
    let staff = create_user(&f.catalog, "s@example.com", "pw", 40).await?;
    grant_role(&f.catalog, new_grant(staff.id, 70, "sub_2")).await?;
    assert_eq!(load_user(&f.catalog, staff.id).await?.unwrap().role, 40);
    Ok(())
}

#[tokio::test]
async fn an_expired_grant_ends_access_with_no_write_even_through_a_cached_session() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "m@example.com", "pw", 80).await?;
    let ends = Utc::now() + Duration::milliseconds(1500);
    grant_role(
        &f.catalog,
        NewRoleGrant {
            expires_at: Some(ends),
            ..new_grant(user.id, 60, "sub_1")
        },
    )
    .await?;
    let before = find_role_grant(&f.catalog, "test", "sub_1").await?.unwrap();

    // A store that would believe a cached session for a whole minute.
    let store = SessionStore::database_with(
        f.catalog.clone(),
        Duration::hours(24),
        Duration::seconds(60),
        CACHE_CAPACITY,
    );
    let token = store.login(user.clone()).await?;
    assert_eq!(store.user_for(&token).await?.unwrap().role, 60);

    tokio::time::sleep(std::time::Duration::from_millis(1700)).await;
    assert_eq!(
        store.user_for(&token).await?.unwrap().role,
        80,
        "the cached entry was believed only until the grant ended"
    );
    // Nothing was written to make that happen.
    assert_eq!(
        find_role_grant(&f.catalog, "test", "sub_1").await?.unwrap(),
        before
    );
    assert_eq!(load_base_user(&f.catalog, user.id).await?.unwrap().role, 80);
    Ok(())
}

#[tokio::test]
async fn a_grant_lands_in_a_session_within_the_freshness_ttl() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "m@example.com", "pw", 80).await?;
    let store = SessionStore::database_with(
        f.catalog.clone(),
        Duration::hours(24),
        Duration::milliseconds(300),
        CACHE_CAPACITY,
    );
    let token = store.login(user.clone()).await?;
    assert_eq!(store.user_for(&token).await?.unwrap().role, 80);

    grant_role(&f.catalog, new_grant(user.id, 60, "sub_1")).await?;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(store.user_for(&token).await?.unwrap().role, 60);

    // And a revocation lands the same way.
    assert!(revoke_role_grant(&f.catalog, "test", "sub_1").await?);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(store.user_for(&token).await?.unwrap().role, 80);
    Ok(())
}

#[tokio::test]
async fn one_grant_per_source_and_reference_upserted_and_once_means_once() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "m@example.com", "pw", 80).await?;

    // A renewal extends the one grant.
    let first = grant_role(&f.catalog, new_grant(user.id, 70, "sub_1")).await?;
    let later = Utc::now() + Duration::days(60);
    let renewed = grant_role(
        &f.catalog,
        NewRoleGrant {
            role: 60,
            expires_at: Some(later),
            ..new_grant(user.id, 70, "sub_1")
        },
    )
    .await?;
    assert_eq!(renewed.id, first.id);
    assert_eq!(renewed.role, 60, "a tier change moves the role");
    assert_eq!(list_role_grants(&f.catalog, user.id).await?.len(), 1);

    // A revoked grant is kept, and an upsert brings it back.
    assert!(revoke_role_grant(&f.catalog, "test", "sub_1").await?);
    assert!(!revoke_role_grant(&f.catalog, "test", "sub_1").await?);
    assert_eq!(effective_role(&f.catalog, user.id, 80).await?.role, 80);
    grant_role(&f.catalog, new_grant(user.id, 60, "sub_1")).await?;
    assert_eq!(effective_role(&f.catalog, user.id, 80).await?.role, 60);

    // `once`: granted the first time, never again — not after it is revoked.
    let trial = || NewRoleGrant {
        source: "grant_role".into(),
        ..new_grant(user.id, 70, &format!("trial:{}", user.id))
    };
    assert!(grant_role_once(&f.catalog, trial()).await?.is_some());
    assert!(grant_role_once(&f.catalog, trial()).await?.is_none());
    revoke_role_grant(&f.catalog, "grant_role", &format!("trial:{}", user.id)).await?;
    assert!(grant_role_once(&f.catalog, trial()).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn grants_that_cannot_mean_anything_are_refused() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "m@example.com", "pw", 80).await?;
    for (role, why) in [(100, "public"), (0, "out of range"), (55, "no such role")] {
        assert!(
            grant_role(&f.catalog, new_grant(user.id, role, "x"))
                .await
                .is_err(),
            "{why}"
        );
    }
    let backwards = NewRoleGrant {
        starts_at: Some(Utc::now()),
        expires_at: Some(Utc::now() - Duration::hours(1)),
        ..new_grant(user.id, 60, "x")
    };
    assert!(grant_role(&f.catalog, backwards).await.is_err());
    Ok(())
}

#[tokio::test]
async fn deleting_the_user_or_an_unheld_role_cleans_up_the_grants() -> Result<()> {
    let f = fixture().await?;
    create_user(&f.catalog, "admin@example.com", "pw", ROLE_ADMIN).await?;
    let user = create_user(&f.catalog, "m@example.com", "pw", 80).await?;

    // A role held through a grant in force cannot be deleted…
    grant_role(&f.catalog, new_grant(user.id, 70, "sub_1")).await?;
    assert!(delete_role(&f.catalog, 70).await.is_err());
    // …and once the grant has ended, its history does not keep the role alive.
    revoke_role_grant(&f.catalog, "test", "sub_1").await?;
    assert!(delete_role(&f.catalog, 70).await?);

    grant_role(&f.catalog, new_grant(user.id, 60, "sub_2")).await?;
    assert!(delete_user(&f.catalog, user.id).await?);
    assert!(list_role_grants(&f.catalog, user.id).await?.is_empty());
    Ok(())
}
