//! `_fd_password_tokens` and invitations against real Postgres (design §7.2).
//!
//! - an invited account exists with **no password**, cannot sign in, and is
//!   given one by spending its token — once;
//! - spending a token ends the user's other tokens and their sessions;
//! - a lapsed token, an unknown one, and a disabled account's are all the same
//!   `None`; a blank password is refused *before* the token is spent;
//! - an invitation is to a less powerful role only, is resent to a pending
//!   account, and never to an active one.

use std::sync::Arc;

use chrono::Duration;
use sc_auth::{
    InviteOutcome, NewUser, PASSWORD_TOKENS_TABLE, PasswordTokenPurpose, Role, SESSIONS_TABLE,
    authenticate, bootstrap, claim_unproven_account, create_session, create_user, invite_user,
    issue_password_token, load_user, password_token_issued_within, redeem_password_token,
    save_role, set_user_disabled, sign_up_user, user_has_password,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Assignment, Expr, Select, Source, Statement, Update, Value};
use sc_test_harness::TestDb;

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
    save_role(&catalog, &Role::new(40, "Therapist")).await?;
    save_role(&catalog, &Role::new(80, "Patient")).await?;
    Ok(Fixture { catalog, _db: db })
}

fn patient(email: &str) -> NewUser {
    NewUser {
        email: email.to_owned(),
        role: 80,
        ..NewUser::default()
    }
}

async fn count(catalog: &Catalog, table: &str) -> Result<usize> {
    let rows: Vec<sc_db::Row> = catalog
        .primary()
        .query(&Statement::from(Select::from(Source::table(table))))
        .await?
        .try_collect()
        .await?;
    Ok(rows.len())
}

#[tokio::test]
async fn an_invited_account_chooses_its_password_with_its_token_once() -> Result<()> {
    let f = fixture().await?;
    let InviteOutcome::Created { user, token } =
        invite_user(&f.catalog, patient(" pat@example.com "), 40).await?
    else {
        panic!("a new address is a new account");
    };
    assert_eq!(user.role, 80);
    assert!(!user_has_password(&f.catalog, user.id).await?);
    assert!(
        authenticate(&f.catalog, "pat@example.com", "")
            .await?
            .is_none()
    );

    // The token is stored as a hash, never as itself.
    let stored: Vec<sc_db::Row> = f
        .catalog
        .primary()
        .query(&Statement::from(Select::from(Source::table(
            PASSWORD_TOKENS_TABLE,
        ))))
        .await?
        .try_collect()
        .await?;
    assert_eq!(stored.len(), 1);
    assert!(!format!("{:?}", stored[0]).contains(&token));

    // A blank password does not spend the link.
    assert!(redeem_password_token(&f.catalog, &token, "").await.is_err());
    // Resending is a second live token; spending either ends both.
    let InviteOutcome::Resent { token: second, .. } =
        invite_user(&f.catalog, patient("pat@example.com"), 40).await?
    else {
        panic!("a pending account is resent");
    };
    let redeemed = redeem_password_token(&f.catalog, &token, "pat-pw").await?;
    assert_eq!(redeemed.map(|u| u.id), Some(user.id));
    assert!(
        authenticate(&f.catalog, "pat@example.com", "pat-pw")
            .await?
            .is_some()
    );
    assert!(
        redeem_password_token(&f.catalog, &token, "again")
            .await?
            .is_none()
    );
    assert!(
        redeem_password_token(&f.catalog, &second, "again")
            .await?
            .is_none()
    );
    assert_eq!(count(&f.catalog, PASSWORD_TOKENS_TABLE).await?, 0);

    // Now it is active, and an invitation is not a way to reset it.
    assert!(matches!(
        invite_user(&f.catalog, patient("pat@example.com"), 40).await?,
        InviteOutcome::AlreadyActive
    ));
    Ok(())
}

#[tokio::test]
async fn a_reset_ends_the_users_sessions_and_a_bad_token_is_just_none() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "pat@example.com", "old-pw", 80).await?;
    create_session(&f.catalog, user.id, Duration::hours(1)).await?;
    assert_eq!(count(&f.catalog, SESSIONS_TABLE).await?, 1);

    let token = issue_password_token(&f.catalog, user.id, PasswordTokenPurpose::Reset).await?;
    let within = Duration::seconds(60);
    assert!(
        password_token_issued_within(&f.catalog, user.id, PasswordTokenPurpose::Reset, within)
            .await?
    );
    assert!(
        !password_token_issued_within(&f.catalog, user.id, PasswordTokenPurpose::Invite, within)
            .await?
    );
    assert!(
        redeem_password_token(&f.catalog, "not-a-token", "x")
            .await?
            .is_none()
    );
    assert!(
        redeem_password_token(&f.catalog, &token, "new-pw")
            .await?
            .is_some()
    );
    assert_eq!(
        count(&f.catalog, SESSIONS_TABLE).await?,
        0,
        "sessions ended"
    );
    assert!(
        authenticate(&f.catalog, "pat@example.com", "old-pw")
            .await?
            .is_none()
    );

    // A lapsed token is refused.
    let lapsed = issue_password_token(&f.catalog, user.id, PasswordTokenPurpose::Reset).await?;
    let expire = Update::new(
        PASSWORD_TOKENS_TABLE,
        vec![Assignment::new(
            "expires_at",
            Expr::Lit(Value::Timestamp(chrono::Utc::now() - Duration::minutes(1))),
        )],
    );
    f.catalog
        .primary()
        .query(&Statement::from(expire))
        .await?
        .try_collect()
        .await?;
    assert!(
        redeem_password_token(&f.catalog, &lapsed, "x")
            .await?
            .is_none()
    );

    // So is a disabled account's.
    let token = issue_password_token(&f.catalog, user.id, PasswordTokenPurpose::Reset).await?;
    set_user_disabled(&f.catalog, user.id, true).await?;
    assert!(
        redeem_password_token(&f.catalog, &token, "x")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn an_invitation_is_only_to_less_power() -> Result<()> {
    let f = fixture().await?;
    // Not to the inviter's own role, nor a more powerful one.
    let mut peer = patient("peer@example.com");
    peer.role = 40;
    assert!(invite_user(&f.catalog, peer.clone(), 40).await.is_err());
    // A pending account at the therapist's level cannot be resent by a
    // therapist, though an admin made it.
    let InviteOutcome::Created { .. } = invite_user(&f.catalog, peer, 1).await? else {
        panic!("an admin may invite a therapist");
    };
    assert!(
        invite_user(&f.catalog, patient("peer@example.com"), 40)
            .await
            .is_err()
    );
    Ok(())
}

/// A self-service signup's address is unproven (security review #41) until a
/// link sent there is redeemed. Claiming an unproven account takes its
/// password, its reset links and its sessions away; claiming a proven one
/// changes nothing.
#[tokio::test]
async fn a_signup_is_unproven_until_a_link_is_redeemed_and_is_claimed_whole() -> Result<()> {
    let f = fixture().await?;
    let made = create_user(&f.catalog, "made@example.com", "made-pw", 80).await?;
    assert!(
        !made.email_unproven(),
        "an account somebody else made is not unproven"
    );

    let signed = sign_up_user(&f.catalog, "signed@example.com", "signed-pw", 80).await?;
    assert!(signed.email_unproven());
    assert!(
        load_user(&f.catalog, signed.id)
            .await?
            .unwrap()
            .email_unproven()
    );
    let token = issue_password_token(&f.catalog, signed.id, PasswordTokenPurpose::Reset).await?;
    let redeemed = redeem_password_token(&f.catalog, &token, "new-pw")
        .await?
        .unwrap();
    assert!(!redeemed.email_unproven());
    assert!(
        !load_user(&f.catalog, signed.id)
            .await?
            .unwrap()
            .email_unproven()
    );

    let squat = sign_up_user(&f.catalog, "squat@example.com", "squat-pw", 80).await?;
    create_session(&f.catalog, squat.id, Duration::hours(1)).await?;
    issue_password_token(&f.catalog, squat.id, PasswordTokenPurpose::Reset).await?;
    let claimed = claim_unproven_account(&f.catalog, squat.clone()).await?;
    assert_eq!(claimed.id, squat.id);
    assert!(!claimed.email_unproven());
    assert!(!user_has_password(&f.catalog, squat.id).await?);
    assert!(
        authenticate(&f.catalog, "squat@example.com", "squat-pw")
            .await?
            .is_none()
    );
    assert_eq!(count(&f.catalog, SESSIONS_TABLE).await?, 0);
    assert_eq!(count(&f.catalog, PASSWORD_TOKENS_TABLE).await?, 0);

    let kept = claim_unproven_account(&f.catalog, made.clone()).await?;
    assert!(user_has_password(&f.catalog, kept.id).await?);
    Ok(())
}

/// A password that cannot be written does not spend the link: an instance whose
/// `users` table lacks `email_verified` (the 2026-10-26 upgrade, before
/// `TABLES_RENAME.sql` was run) failed the write *after* deleting the token and
/// setting the hash, so its holder saw an error, had a new password anyway, and
/// needed a new link. Now none of it lands, and the same link works once a
/// restart's bootstrap has added the column back.
#[tokio::test]
async fn a_failed_redemption_leaves_the_token_and_the_password_as_they_were() -> Result<()> {
    let f = fixture().await?;
    let InviteOutcome::Created { user, token } =
        invite_user(&f.catalog, patient("pat@example.com"), 40).await?
    else {
        panic!("a new address is a new account");
    };
    let client = f._db.client().await?;
    let alter = async |sql: &str| {
        client
            .batch_execute(sql)
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))
    };
    alter(r#"ALTER TABLE "users" DROP COLUMN "email_verified""#).await?;

    assert!(
        redeem_password_token(&f.catalog, &token, "pat-pw")
            .await
            .is_err()
    );
    assert_eq!(count(&f.catalog, PASSWORD_TOKENS_TABLE).await?, 1);
    assert!(!user_has_password(&f.catalog, user.id).await?);

    // A restart puts the column back: bootstrap reconciles `users` additively.
    let driver = Arc::new(PgDriver::from_pool(f._db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    let users = bootstrap(&catalog).await?;
    assert!(users.field("email_verified").is_some());

    let redeemed = redeem_password_token(&catalog, &token, "pat-pw").await?;
    assert_eq!(redeemed.map(|u| u.id), Some(user.id));
    assert!(
        authenticate(&catalog, "pat@example.com", "pat-pw")
            .await?
            .is_some()
    );
    assert_eq!(count(&catalog, PASSWORD_TOKENS_TABLE).await?, 0);
    Ok(())
}
