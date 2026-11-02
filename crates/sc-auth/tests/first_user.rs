//! Phase 5 integration test: the create-first-user flow against real Postgres.
//!
//! Bootstraps the `users` table, then walks the flow: no user exists → create
//! the first (admin) user → a user now exists → a second `create_first_user`
//! call is refused → the stored password is a hash, not the plaintext.

use sc_types::Role;

use sc_auth::{
    COL_EMAIL, COL_ID, COL_PASSWORD_HASH, USERS_TABLE, any_user_exists, bootstrap,
    create_first_user, is_valid_hash,
};
use sc_query::{Expr, Projection, Select, Source, Value};
use sc_test_harness::TestServer;

#[tokio::test]
async fn create_first_user_flow() -> sc_error::Result<()> {
    let TestServer {
        db: _db, catalog, ..
    } = TestServer::builder().build().await?;
    bootstrap(&catalog).await?;

    // No user yet → the flow reports the create-first-user state.
    assert!(!any_user_exists(&catalog).await?);

    // Blank inputs are rejected before anything is written.
    assert!(create_first_user(&catalog, "  ", "pw").await.is_err());
    assert!(create_first_user(&catalog, "a@b.c", "").await.is_err());
    assert!(!any_user_exists(&catalog).await?);

    // Create the first user: an admin.
    let user = create_first_user(&catalog, "admin@example.com", "s3cret-pw").await?;
    assert_eq!(user.role(), Role::ADMIN);
    assert!(user.is_admin());
    assert_eq!(
        user.get(COL_EMAIL),
        Some(&Value::Text("admin@example.com".into()))
    );

    // A user now exists, and a second attempt is refused.
    assert!(any_user_exists(&catalog).await?);
    assert!(
        create_first_user(&catalog, "other@example.com", "another")
            .await
            .is_err()
    );

    // Exactly one row, and its stored password is a hash — never the plaintext.
    let users = catalog.require(USERS_TABLE)?;
    let provider = catalog.provider(&users, sc_catalog::Unchecked::system("test"))?;
    let rows = provider
        .query(&Select::from(Source::table(USERS_TABLE)).columns(vec![
            Projection::expr(Expr::col(COL_ID)),
            Projection::expr(Expr::col(COL_PASSWORD_HASH)),
        ]))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(COL_ID), Some(&Value::Uuid(user.id())));
    match rows[0].get(COL_PASSWORD_HASH) {
        Some(Value::Text(hash)) => {
            assert_ne!(hash, "s3cret-pw");
            assert!(is_valid_hash(hash), "stored value is a PHC hash: {hash}");
        }
        other => panic!("expected a text password hash, got {other:?}"),
    }

    Ok(())
}
