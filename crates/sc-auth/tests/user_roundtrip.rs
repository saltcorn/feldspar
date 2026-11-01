//! Phase 5 integration test: a `users` row inserted into real Postgres reads
//! back into a [`User`] via [`User::from_row`], with the password hash excluded.

use sc_types::Role;

use sc_auth::{COL_EMAIL, COL_ID, COL_PASSWORD_HASH, COL_ROLE, USERS_TABLE, User, bootstrap};
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_test_harness::TestServer;
use uuid::Uuid;

#[tokio::test]
async fn user_row_round_trips_into_user() -> sc_error::Result<()> {
    let TestServer {
        db: _db, catalog, ..
    } = TestServer::builder().build().await?;

    let users = bootstrap(&catalog).await?;
    let provider = catalog.provider(&users, sc_catalog::Unchecked::system("test"))?;

    // Insert an admin user (UUID generated in Rust, not by a DB default).
    let id = Uuid::new_v4();
    provider
        .write(&Statement::from(Insert {
            table: USERS_TABLE.into(),
            columns: vec![
                COL_ID.into(),
                COL_ROLE.into(),
                COL_EMAIL.into(),
                COL_PASSWORD_HASH.into(),
            ],
            rows: vec![vec![
                Expr::lit(id),
                Expr::lit(1_i64),
                Expr::lit("admin@example.com"),
                Expr::lit("$argon2id$placeholder"),
            ]],
            returning: Vec::new(),
        }))
        .await?
        .try_collect()
        .await?;

    // Read the whole row back and build a User from it.
    let rows = provider
        .query(
            &Select::from(Source::table(USERS_TABLE))
                .columns(vec![
                    Projection::expr(Expr::col(COL_ID)),
                    Projection::expr(Expr::col(COL_ROLE)),
                    Projection::expr(Expr::col(COL_EMAIL)),
                    Projection::expr(Expr::col(COL_PASSWORD_HASH)),
                ])
                .filter(Expr::col(COL_ID).eq(Expr::lit(id))),
        )
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);

    let user = User::from_row(&rows[0])?;
    assert_eq!(user.id(), id);
    assert_eq!(user.role(), Role::ADMIN);
    assert!(user.is_admin());
    assert_eq!(
        user.get(COL_EMAIL),
        Some(&Value::Text("admin@example.com".into()))
    );
    // The password hash is never carried on a User, even when the row includes it.
    assert!(!user.extra().contains_key(COL_PASSWORD_HASH));

    Ok(())
}
