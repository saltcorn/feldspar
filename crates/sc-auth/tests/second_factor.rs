//! Two-factor authentication against real Postgres (design §7.2).
//!
//! - setting one up takes the first code from the app; until then the account
//!   is not asked for one;
//! - a code is good once (a replay in the same 30 seconds is refused), and a
//!   recovery code is good once;
//! - a role can require one, and the challenge between a password and a session
//!   is read without being spent, then spent once;
//! - turning it off, and deleting the account, take the secret and the codes.

use std::sync::Arc;

use chrono::Utc;
use sc_auth::{
    ATTR_REQUIRE_SECOND_FACTOR, RECOVERY_CODE_COUNT, RECOVERY_CODES_TABLE, RoleDef,
    SECOND_FACTOR_TABLE, STEP_SECONDS, SecondFactorNeeded, begin_second_factor_setup, bootstrap,
    create_user, decode_secret, delete_user, disable_second_factor, finish_second_factor_setup,
    issue_second_factor_challenge, load_user, regenerate_recovery_codes, save_role,
    second_factor_challenge, second_factor_enabled, second_factor_needed, second_factor_status,
    spend_second_factor_challenge, totp_at, verify_second_factor,
};
use sc_catalog::{Catalog, SecretKey};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Select, Source, Statement};
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
    db.on_teardown(catalog.teardown_hook());
    catalog.set_secret_key(SecretKey::generate()?);
    bootstrap(&catalog).await?;
    Ok(Fixture { catalog, _db: db })
}

async fn account(catalog: &Catalog, email: &str, role: sc_types::Role) -> Result<Uuid> {
    Ok(
        create_user(catalog, email, "pw-123456", role)
            .await?
            .id(),
    )
}

async fn count(catalog: &Catalog, table: &str) -> Result<usize> {
    let rows: Vec<sc_db::Row> = catalog
        .primary(sc_catalog::Unchecked::system("test: count rows"))
        .query(&Statement::from(Select::from(Source::table(table))))
        .await?
        .try_collect()
        .await?;
    Ok(rows.len())
}

/// The code the app would show `steps` steps from now.
fn code(secret: &str, steps: i64) -> String {
    totp_at(
        &decode_secret(secret).unwrap(),
        Utc::now().timestamp() + steps * STEP_SECONDS,
    )
}

#[tokio::test]
async fn setting_up_takes_a_code_and_a_code_works_once() -> Result<()> {
    let f = fixture().await?;
    let cat = &f.catalog;
    let id = account(cat, "ann@example.com", sc_types::Role::ADMIN).await?;
    let user = load_user(cat, id).await?.unwrap();
    assert_eq!(
        second_factor_needed(cat, &user).await?,
        SecondFactorNeeded::No
    );

    let secret = begin_second_factor_setup(cat, id).await?;
    assert!(
        !second_factor_enabled(cat, id).await?,
        "a setup waiting for its first code asks nothing at sign-in"
    );
    assert_eq!(
        finish_second_factor_setup(cat, id, "000000")
            .await?
            .filter(|_| {
                // A one-in-a-million chance the wrong code is right.
                code(&secret, 0) != "000000"
            }),
        None
    );
    // A code from the step before is accepted (clock drift), and turns it on.
    let codes = finish_second_factor_setup(cat, id, &code(&secret, -1))
        .await?
        .expect("the app's code turns it on");
    assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
    assert!(second_factor_enabled(cat, id).await?);
    assert_eq!(
        second_factor_needed(cat, &user).await?,
        SecondFactorNeeded::Code
    );
    assert!(
        begin_second_factor_setup(cat, id).await.is_err(),
        "not set up again over the top of one that is on"
    );

    // The step-before code was used to turn it on: it, and the same step's
    // code again, are replays now. The current one is not.
    assert!(!verify_second_factor(cat, id, &code(&secret, -1)).await?);
    assert!(verify_second_factor(cat, id, &code(&secret, 0)).await?);
    assert!(
        !verify_second_factor(cat, id, &code(&secret, 0)).await?,
        "the same code twice"
    );

    // A recovery code once, however it is typed; then never again.
    let typed = codes[0].to_uppercase().replace('-', " ");
    assert!(verify_second_factor(cat, id, &typed).await?);
    assert!(!verify_second_factor(cat, id, &codes[0]).await?);
    assert_eq!(
        second_factor_status(cat, id).await?.recovery_codes_left,
        RECOVERY_CODE_COUNT - 1
    );
    // Another account's codes are no good here.
    let other = account(cat, "bob@example.com", sc_types::Role::ADMIN).await?;
    assert!(!verify_second_factor(cat, other, &codes[1]).await?);

    // New codes replace the old ones.
    let fresh = regenerate_recovery_codes(cat, id).await?;
    assert!(!verify_second_factor(cat, id, &codes[1]).await?);
    assert!(verify_second_factor(cat, id, &fresh[0]).await?);

    assert!(disable_second_factor(cat, id).await?);
    assert!(!second_factor_enabled(cat, id).await?);
    assert!(!verify_second_factor(cat, id, &fresh[1]).await?);
    assert_eq!(count(cat, RECOVERY_CODES_TABLE).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_role_can_require_one_and_a_challenge_is_spent_once() -> Result<()> {
    let f = fixture().await?;
    let cat = &f.catalog;
    let mut staff = RoleDef::new(sc_types::Role::of(40), "Staff");
    staff
        .attributes
        .insert(ATTR_REQUIRE_SECOND_FACTOR.into(), serde_json::json!(true));
    save_role(cat, &staff).await?;
    let id = account(cat, "staff@example.com", sc_types::Role::of(40)).await?;
    let user = load_user(cat, id).await?.unwrap();
    assert_eq!(
        second_factor_needed(cat, &user).await?,
        SecondFactorNeeded::Setup
    );

    let token = issue_second_factor_challenge(cat, id, false).await?;
    // Read as often as a wrong code needs, and it says "Remember me" was off.
    assert_eq!(
        second_factor_challenge(cat, &token).await?,
        Some((id, false))
    );
    assert_eq!(
        second_factor_challenge(cat, &token).await?,
        Some((id, false))
    );
    assert_eq!(second_factor_challenge(cat, "nonsense").await?, None);
    // A challenge is not a password link.
    assert!(
        sc_auth::redeem_password_token(cat, &token, "new-password")
            .await?
            .is_none()
    );
    assert_eq!(spend_second_factor_challenge(cat, &token).await?, Some(id));
    assert_eq!(spend_second_factor_challenge(cat, &token).await?, None);
    assert_eq!(second_factor_challenge(cat, &token).await?, None);

    let remembered = issue_second_factor_challenge(cat, id, true).await?;
    assert_eq!(
        second_factor_challenge(cat, &remembered).await?,
        Some((id, true))
    );

    // Deleting the account takes its second factor with it.
    let secret = begin_second_factor_setup(cat, id).await?;
    finish_second_factor_setup(cat, id, &code(&secret, 0))
        .await?
        .unwrap();
    assert_eq!(count(cat, SECOND_FACTOR_TABLE).await?, 1);
    assert!(delete_user(cat, id).await?);
    assert_eq!(count(cat, SECOND_FACTOR_TABLE).await?, 0);
    assert_eq!(count(cat, RECOVERY_CODES_TABLE).await?, 0);
    Ok(())
}
