//! Integration test: managing a user after it exists (design §7.1) — editing it,
//! disabling it, resetting its password, deleting it — against a real database.
//!
//! The claims worth a database rather than a unit test are the ones about what
//! the *row* does: that an admin-added column round-trips through create and
//! update; that a disabled account stops authenticating without losing anything
//! about itself, and starts again when re-enabled; that a blank password on
//! create yields a real, usable one that comes back exactly once; and that the
//! system's own columns cannot be written through the bag of admin-defined ones.

use sc_types::Role;
use std::collections::BTreeMap;
use std::sync::Arc;

use sc_auth::{
    COL_DISABLED, COL_EMAIL, NewUser, RoleDef, USERS_TABLE, UserUpdate, authenticate, bootstrap,
    create_first_user, create_user_with, delete_user, load_user, random_password, save_role,
    set_user_disabled, set_user_password, update_user,
};
use sc_catalog::{Catalog, DataField};
use sc_error::Result;
use sc_query::Value;
use sc_test_harness::{TestDb, TestServer};
use sc_types::{BasicType, TypeRef};

/// A catalog with the platform tables bootstrapped, over a database cleared of
/// the v1 template's stray `users` tables.
async fn catalog(db: &TestDb) -> Result<Arc<Catalog>> {
    let catalog = TestServer::builder().build_on(db).await?;
    bootstrap(&catalog).await?;
    Ok(catalog)
}

/// Role 40, so a user has somewhere to be that is neither admin nor public.
async fn staff_role(cat: &Catalog) -> Result<()> {
    save_role(cat, &RoleDef::new(Role::of(40), "Staff")).await
}

/// Add a column to the users table, as an admin is invited to (§7.1).
async fn add_nickname(cat: &Catalog) -> Result<()> {
    cat.create_field(
        USERS_TABLE,
        &DataField::plain("nickname", TypeRef::Basic(BasicType::Text)),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_user_carries_the_columns_the_admin_added() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    staff_role(&cat).await?;
    add_nickname(&cat).await?;

    let created = create_user_with(
        &cat,
        NewUser {
            email: "staff@example.com".into(),
            password: "correct-horse".into(),
            role: Role::of(40),
            language: None,
            extra: BTreeMap::from([("nickname".to_owned(), Value::Text("Sam".into()))]),
        },
    )
    .await?;
    assert!(
        created.generated_password.is_none(),
        "nothing to reveal when the admin chose the password"
    );

    // The added column is on the row, and comes back on the user read from it.
    let user = load_user(&cat, created.user.id()).await?.expect("user");
    assert_eq!(user.get("nickname"), Some(&Value::Text("Sam".into())));

    // …and it is editable like any other field, without resending the password.
    let updated = update_user(
        &cat,
        user.id(),
        UserUpdate {
            extra: BTreeMap::from([("nickname".to_owned(), Value::Text("Sammy".into()))]),
            ..UserUpdate::default()
        },
    )
    .await?;
    assert_eq!(updated.get("nickname"), Some(&Value::Text("Sammy".into())));
    assert!(
        authenticate(&cat, "staff@example.com", "correct-horse")
            .await?
            .is_some(),
        "editing a field must not disturb the password"
    );

    // The system's own columns are not reachable through the admin-field bag —
    // in particular the password hash.
    let refused = update_user(
        &cat,
        user.id(),
        UserUpdate {
            extra: BTreeMap::from([(
                "password_hash".to_owned(),
                Value::Text("$argon2id$forged".into()),
            )]),
            ..UserUpdate::default()
        },
    )
    .await;
    assert!(refused.is_err());
    assert!(
        authenticate(&cat, "staff@example.com", "correct-horse")
            .await?
            .is_some()
    );

    Ok(())
}

#[tokio::test]
async fn a_blank_password_is_generated_and_handed_back_once() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    staff_role(&cat).await?;

    let created = create_user_with(
        &cat,
        NewUser {
            email: "new@example.com".into(),
            password: sc_error::Secret::default(),
            role: Role::of(40),
            language: None,
            extra: BTreeMap::new(),
        },
    )
    .await?;
    let password = created
        .generated_password
        .expect("a blank password is a request for a generated one");

    // It is a real password: it is what the account now authenticates with, and
    // nothing else does.
    assert!(
        authenticate(&cat, "new@example.com", &password)
            .await?
            .is_some()
    );
    assert!(authenticate(&cat, "new@example.com", "").await?.is_none());

    // Resetting it invalidates the old one, which is the whole point.
    let next = random_password();
    set_user_password(&cat, created.user.id(), &next).await?;
    assert!(
        authenticate(&cat, "new@example.com", &password)
            .await?
            .is_none()
    );
    assert!(
        authenticate(&cat, "new@example.com", &next)
            .await?
            .is_some()
    );

    Ok(())
}

#[tokio::test]
async fn disabling_stops_sign_in_without_losing_the_account() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    staff_role(&cat).await?;

    let created = create_user_with(
        &cat,
        NewUser {
            email: "leaver@example.com".into(),
            password: "still-my-password".into(),
            role: Role::of(40),
            language: None,
            extra: BTreeMap::new(),
        },
    )
    .await?;
    let id = created.user.id();
    assert!(!created.user.is_disabled());

    let disabled = set_user_disabled(&cat, id, true).await?;
    assert!(disabled.is_disabled());
    assert_eq!(disabled.get(COL_DISABLED), Some(&Value::Bool(true)));
    assert!(
        authenticate(&cat, "leaver@example.com", "still-my-password")
            .await?
            .is_none(),
        "a disabled account does not sign in, right password or not"
    );

    // The row is still there and still says everything it said: disabling is not
    // deletion, which is why both exist.
    let still = load_user(&cat, id).await?.expect("still a row");
    assert_eq!(still.role(), Role::of(40));
    assert_eq!(
        still.get(COL_EMAIL),
        Some(&Value::Text("leaver@example.com".into()))
    );

    set_user_disabled(&cat, id, false).await?;
    assert!(
        authenticate(&cat, "leaver@example.com", "still-my-password")
            .await?
            .is_some(),
        "and re-enabling gives the account back, password and all"
    );

    Ok(())
}

#[tokio::test]
async fn a_user_can_be_edited_and_deleted() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    staff_role(&cat).await?;
    // The first user is an admin, so the roles below are a real change.
    create_first_user(&cat, "admin@example.com", "adminpass").await?;

    let created = create_user_with(
        &cat,
        NewUser {
            email: "before@example.com".into(),
            password: "first-password".into(),
            role: Role::PUBLIC,
            // §16.1: an account created for somebody who reads French.
            language: Some("fr".to_owned()),
            extra: BTreeMap::new(),
        },
    )
    .await?;
    let id = created.user.id();
    // §16.1: the chosen language is on the row and on the user read back from
    // it, so the router can negotiate from it without a second read.
    assert_eq!(created.user.language(), Some("fr"));
    assert_eq!(
        load_user(&cat, id).await?.expect("user").language(),
        Some("fr")
    );

    let updated = update_user(
        &cat,
        id,
        UserUpdate {
            email: Some("after@example.com".into()),
            role: Some(Role::of(40)),
            password: Some("second-password".into()),
            // `None` is "leave it alone", and the assertion below is what makes
            // that different from `Some(None)`.
            language: None,
            extra: BTreeMap::new(),
        },
    )
    .await?;
    assert_eq!(updated.role(), Role::of(40));
    assert_eq!(
        updated.language(),
        Some("fr"),
        "an update that said nothing about the language changed nothing"
    );
    assert_eq!(
        updated.get(COL_EMAIL),
        Some(&Value::Text("after@example.com".into()))
    );
    assert!(
        authenticate(&cat, "after@example.com", "second-password")
            .await?
            .is_some()
    );
    assert!(
        authenticate(&cat, "before@example.com", "first-password")
            .await?
            .is_none(),
        "the old identifier is nobody's"
    );

    // `Some(None)` is the form's "Site default": it clears the column, which is
    // a change rather than an omission.
    let cleared = update_user(
        &cat,
        id,
        UserUpdate {
            language: Some(None),
            ..UserUpdate::default()
        },
    )
    .await?;
    assert_eq!(cleared.language(), None);

    // A role nobody defined is refused with a sentence rather than a constraint
    // violation, and changes nothing.
    let refused = update_user(
        &cat,
        id,
        UserUpdate {
            role: Some(Role::of(77)),
            ..UserUpdate::default()
        },
    )
    .await;
    assert!(refused.is_err());
    assert_eq!(
        load_user(&cat, id).await?.expect("user").role(),
        Role::of(40)
    );

    assert!(delete_user(&cat, id).await?);
    assert!(load_user(&cat, id).await?.is_none());
    // Deleting the same user twice is `false`, not an error at the door.
    assert!(!delete_user(&cat, id).await?);

    Ok(())
}
