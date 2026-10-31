//! Integration test: roles as rows in `_fd_roles`, with `users.role` a foreign
//! key onto them (design §7.1, §9), against a real database.
//!
//! The point of making a role a row rather than a bare integer is that the row
//! can carry things — a name, and role-specific settings — and that a user's
//! role can be *constrained* to name one. So the things asserted here are: the
//! two built-ins exist after bootstrap and cannot be deleted; a role round-trips
//! including its attributes; a user cannot hold a role that does not exist,
//! because the database refuses it; and a role users still hold cannot be
//! deleted out from under them.

use sc_types::Role;
use std::sync::Arc;

use sc_auth::{
    ROLES_TABLE, RoleDef, bootstrap, create_first_user, create_user, delete_role, list_roles,
    load_role, save_role,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_test_harness::TestDb;

/// A catalog with the platform tables bootstrapped, over a database that has
/// been cleared of the v1 template's stray `users` tables.
async fn catalog(db: &TestDb) -> Result<Catalog> {
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
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap(&catalog).await?;
    Ok(catalog)
}

#[tokio::test]
async fn bootstrap_seeds_exactly_the_two_builtin_roles() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let roles = list_roles(&cat).await?;
    let numbers: Vec<u8> = roles.iter().map(|r| r.role.get()).collect();
    assert_eq!(numbers, vec![1, 100], "only the two the system depends on");
    assert_eq!(load_role(&cat, Role::ADMIN).await?.unwrap().name, "Admin");
    assert_eq!(load_role(&cat, Role::PUBLIC).await?.unwrap().name, "Public");
    assert!(roles.iter().all(|r| r.is_builtin()));

    // Bootstrapping again does not duplicate or overwrite — an admin who renamed
    // Admin to Owner keeps that across reboots.
    let mut owner = load_role(&cat, Role::ADMIN).await?.unwrap();
    owner.name = "Owner".to_owned();
    save_role(&cat, &owner).await?;
    bootstrap(&cat).await?;
    assert_eq!(load_role(&cat, Role::ADMIN).await?.unwrap().name, "Owner");
    assert_eq!(list_roles(&cat).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_role_round_trips_including_its_settings() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let mut staff = RoleDef::new(Role::of(40), "Staff").description("On the payroll");
    staff
        .attributes
        .insert("landing".into(), "/dashboard".into());
    save_role(&cat, &staff).await?;

    let loaded = load_role(&cat, Role::of(40))
        .await?
        .expect("saved role loads back");
    assert_eq!(loaded, staff);
    assert_eq!(
        loaded.attributes.get("landing").and_then(|v| v.as_str()),
        Some("/dashboard")
    );

    // The number is the key everything holds, so a second role cannot claim it.
    let clash = RoleDef::new(Role::of(40), "Different");
    let err = save_role(&cat, &clash).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_user_cannot_hold_a_role_that_does_not_exist() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    create_first_user(&cat, "admin@example.com", "admin-pw").await?;

    // Role 40 is not there yet: the create is refused, and the message is the
    // legible one, not a raw constraint violation.
    let err = create_user(&cat, "editor@example.com", "editor-pw", Role::of(40))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains("role 40"), "{err}");

    // Create the role, and now the same user is accepted.
    save_role(&cat, &RoleDef::new(Role::of(40), "Editor")).await?;
    let user = create_user(&cat, "editor@example.com", "editor-pw", Role::of(40)).await?;
    assert_eq!(user.role(), Role::of(40));
    Ok(())
}

#[tokio::test]
async fn a_role_still_held_cannot_be_deleted() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    create_first_user(&cat, "admin@example.com", "admin-pw").await?;
    save_role(&cat, &RoleDef::new(Role::of(40), "Editor")).await?;
    create_user(&cat, "editor@example.com", "editor-pw", Role::of(40)).await?;

    // Held by one user: refused, and the message says so rather than surfacing
    // the foreign key. Nothing cascades — a user's role is not the system's to
    // silently reassign.
    let err = delete_role(&cat, Role::of(40)).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains("held by"), "{err}");
    assert!(
        load_role(&cat, Role::of(40)).await?.is_some(),
        "not deleted"
    );

    // A built-in role is refused even when nobody holds it.
    let err = delete_role(&cat, Role::ADMIN).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");

    // Deleting a role that does not exist is not an error — it says nothing was
    // there.
    assert!(!delete_role(&cat, Role::of(55)).await?);
    Ok(())
}

#[tokio::test]
async fn the_roles_table_is_a_hidden_system_table() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let table = cat.require(ROLES_TABLE)?;
    assert!(table.is_system());
    Ok(())
}
