//! Phase 5 acceptance walk (the "Integration tests" TODO item): one end-to-end
//! story against real Postgres covering create-first-user, login success and
//! failure, logout, and session expiry.
//!
//! Per-feature integration tests live alongside this one (`first_user`, `login`,
//! `role_gate`); this file is the consolidated Phase 5 definition-of-done for the
//! users-and-auth story.

use sc_types::Role;
use std::time::Duration as StdDuration;

use chrono::Duration;
use sc_auth::{
    CACHE_CAPACITY, COL_EMAIL, SessionStore, any_user_exists, authenticate, bootstrap,
    create_first_user,
};
use sc_query::Value;
use sc_test_harness::TestServer;

#[tokio::test]
async fn phase5_auth_acceptance_walk() -> sc_error::Result<()> {
    let TestServer {
        db: _db, catalog, ..
    } = TestServer::builder().build().await?;
    bootstrap(&catalog).await?;

    // --- create-first-user ----------------------------------------------------
    assert!(
        !any_user_exists(&catalog).await?,
        "no user before bootstrap"
    );
    let admin = create_first_user(&catalog, "admin@example.com", "s3cret-pw").await?;
    assert!(admin.is_admin());
    assert!(
        any_user_exists(&catalog).await?,
        "user exists after creation"
    );
    // The flow only creates the *first* user.
    assert!(
        create_first_user(&catalog, "second@example.com", "pw")
            .await
            .is_err(),
        "second create-first-user is refused"
    );

    // --- login failure --------------------------------------------------------
    assert!(
        authenticate(&catalog, "admin@example.com", "wrong")
            .await?
            .is_none(),
        "wrong password fails"
    );
    assert!(
        authenticate(&catalog, "nobody@example.com", "s3cret-pw")
            .await?
            .is_none(),
        "unknown email fails"
    );

    // --- login success --------------------------------------------------------
    let user = authenticate(&catalog, "admin@example.com", "s3cret-pw")
        .await?
        .expect("valid credentials authenticate")
        .into_user();
    assert_eq!(user.id(), admin.id());
    assert_eq!(user.role(), Role::ADMIN);
    assert_eq!(
        user.get(COL_EMAIL),
        Some(&Value::Text("admin@example.com".into()))
    );

    // --- logout ---------------------------------------------------------------
    // The real store, not the in-process test seam: this walk is the phase's
    // definition of done, so the sessions in it are rows like a server's are.
    let sessions = SessionStore::database(catalog.clone());
    let token = sessions.login(user.clone()).await?;
    assert_eq!(
        sessions.user_for(&token).await?.map(|u| u.into_user()),
        Some(user.clone()),
        "session resolves before logout"
    );
    assert!(sessions.logout(&token).await?, "logout removes the session");
    assert_eq!(
        sessions.user_for(&token).await?,
        None,
        "session gone after logout"
    );

    // --- session expiry -------------------------------------------------------
    // A short-lived session resolves while fresh, then lapses after its TTL.
    // The cache is switched off (a zero freshness TTL) so what lapses here is
    // the row, not a cached copy of it.
    let short = SessionStore::database_with(
        catalog.clone(),
        Duration::milliseconds(150),
        Duration::zero(),
        CACHE_CAPACITY,
    );
    let expiring = short.login(user.clone()).await?;
    assert_eq!(
        short.user_for(&expiring).await?.map(|u| u.into_user()),
        Some(user),
        "session valid within its TTL"
    );
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert_eq!(
        short.user_for(&expiring).await?,
        None,
        "session expired after its TTL"
    );

    Ok(())
}
