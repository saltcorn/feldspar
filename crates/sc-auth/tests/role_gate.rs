//! Phase 5 integration test: the admin-only role gate for the admin UI.
//!
//! A valid admin passes `authenticate_admin`; a valid **non-admin** user is
//! rejected by the gate even though their credentials are correct (proven by the
//! ungated `authenticate` still accepting them).

use sc_types::Role;
use std::sync::Arc;

use sc_auth::{
    COL_EMAIL, COL_ID, COL_PASSWORD_HASH, COL_ROLE, RoleDef, USERS_TABLE, authenticate,
    authenticate_admin, bootstrap, create_first_user, hash_password, save_role,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_query::{Expr, Insert, Statement};
use sc_test_harness::TestDb;
use uuid::Uuid;

#[tokio::test]
async fn admin_only_login_gate() -> sc_error::Result<()> {
    let db = TestDb::new().await?;

    // The multi-tenant v1 test template carries `users` tables in several
    // schemas; drop them all so bootstrap creates the clean MVP table. No-op in
    // clean CI.
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
    let catalog = Catalog::init(driver.clone() as Arc<dyn DatabaseDriver>).await?;
    bootstrap(&catalog).await?;

    // The first user is an admin.
    create_first_user(&catalog, "admin@example.com", "admin-pw").await?;

    // Role 40 has to exist before anyone can hold it: `users.role` is a foreign
    // key onto `_fd_roles` (§7.1), so this is not setup noise — it is the
    // constraint doing its job.
    save_role(&catalog, &RoleDef::new(Role::of(40), "Member")).await?;

    // Add a non-admin (role 40) directly, with a properly hashed password.
    let users = catalog.require(USERS_TABLE)?;
    let provider = catalog.provider(&users)?;
    let member_hash = hash_password("member-pw")?;
    provider
        .write(&Statement::from(Insert::row(
            USERS_TABLE,
            vec![
                COL_ID.into(),
                COL_ROLE.into(),
                COL_EMAIL.into(),
                COL_PASSWORD_HASH.into(),
            ],
            vec![
                Expr::lit(Uuid::new_v4()),
                Expr::lit(40_i64),
                Expr::lit("member@example.com"),
                Expr::lit(member_hash),
            ],
        )))
        .await?
        .try_collect()
        .await?;

    // Admin passes the gate.
    let admin = authenticate_admin(&catalog, "admin@example.com", "admin-pw").await?;
    assert!(admin.is_some());
    assert!(admin.unwrap().is_admin());

    // Admin with a wrong password is rejected.
    assert!(
        authenticate_admin(&catalog, "admin@example.com", "nope")
            .await?
            .is_none()
    );

    // The non-admin has valid credentials (ungated authenticate accepts them)…
    let member = authenticate(&catalog, "member@example.com", "member-pw").await?;
    assert!(member.is_some());
    assert_eq!(member.unwrap().role, Role::of(40));

    // …but the admin gate rejects them.
    assert!(
        authenticate_admin(&catalog, "member@example.com", "member-pw")
            .await?
            .is_none()
    );

    Ok(())
}
