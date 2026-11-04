//! Two-factor authentication against real Postgres (design §7.2).
//!
//! - setting one up takes the first code from the app; until then the account
//!   is not asked for one;
//! - a code is good once (a replay in the same 30 seconds is refused), and a
//!   recovery code is good once;
//! - a role can require one, and the challenge between a password and a session
//!   is read without being spent, then spent once;
//! - turning it off, and deleting the account, take the secret and the codes;
//! - every check of a code takes a permit counted before it (H2.4), recovery
//!   codes are long and keyed (H2.5), and a credential change ends a sign-in
//!   waiting for its code (H2.3).

use std::sync::Arc;

use chrono::Utc;
use sc_auth::{
    ATTR_REQUIRE_SECOND_FACTOR, Attempt, AttemptPermit, ChallengeToken, FirstFactor,
    PasswordVerified, RECOVERY_CODE_COUNT, RECOVERY_CODES_TABLE, RoleDef, SECOND_FACTOR_TABLE,
    STEP_SECONDS, SecondFactorNeeded, SecondFactorProof, TotpSecret, begin_second_factor_setup,
    bootstrap, create_user, delete_user, disable_second_factor, finish_second_factor_setup,
    issue_second_factor_challenge, load_user, regenerate_recovery_codes, reserve_attempt,
    save_role, second_factor_challenge, second_factor_enabled, second_factor_needed,
    second_factor_status, spend_second_factor_challenge, totp_at, verify_second_factor,
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
    Ok(create_user(catalog, email, "pw-123456", role).await?.id())
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
fn code(secret: &TotpSecret, steps: i64) -> String {
    totp_at(secret, Utc::now().timestamp() + steps * STEP_SECONDS)
}

/// One guess at `id`'s code, counted.
async fn permit(catalog: &Catalog, id: Uuid) -> Result<AttemptPermit> {
    match reserve_attempt(catalog, id).await? {
        Attempt::Permitted(permit) => Ok(permit),
        Attempt::Limited { .. } => panic!("out of guesses"),
    }
}

/// Check `typed` against `id`'s second factor, the proof if it was right.
async fn check(catalog: &Catalog, id: Uuid, typed: &str) -> Result<Option<SecondFactorProof>> {
    verify_second_factor(catalog, permit(catalog, id).await?, id, typed).await
}

/// Start `id`'s guessing window again, as fifteen minutes passing would: the
/// walk-through below makes more wrong guesses than one window allows.
async fn forgive(catalog: &Catalog, id: Uuid) -> Result<()> {
    let update = sc_query::Update {
        table: SECOND_FACTOR_TABLE.to_owned(),
        assignments: vec![sc_query::Assignment::new(
            "guesses",
            sc_query::Expr::Lit(sc_query::Value::Null),
        )],
        filter: Some(sc_query::Expr::col("user_id").eq(sc_query::Expr::lit(id))),
        returning: Vec::new(),
    };
    catalog
        .primary(sc_catalog::Unchecked::system("test: a new window"))
        .query(&Statement::from(update))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Whether `typed` is right for `id`.
async fn right(catalog: &Catalog, id: Uuid, typed: &str) -> Result<bool> {
    Ok(check(catalog, id, typed).await?.is_some())
}

/// Turn `id`'s second factor on with the app's code for `secret`: the
/// recovery codes, as shown.
async fn enable(
    catalog: &Catalog,
    id: Uuid,
    secret: &TotpSecret,
    steps: i64,
) -> Result<Vec<String>> {
    let enabled = finish_second_factor_setup(
        catalog,
        permit(catalog, id).await?,
        id,
        &code(secret, steps),
    )
    .await?
    .expect("the app's code turns it on");
    let _changed = enabled.changed;
    Ok(enabled
        .recovery_codes
        .iter()
        .map(|c| c.expose().to_owned())
        .collect())
}

/// A sign-in by `id` whose password was right.
async fn signing_in(catalog: &Catalog, id: Uuid) -> Result<PasswordVerified> {
    PasswordVerified::by(
        catalog,
        load_user(catalog, id).await?.unwrap(),
        FirstFactor::Password,
    )
    .await
}

#[tokio::test]
async fn setting_up_takes_a_code_and_a_code_works_once() -> Result<()> {
    let f = fixture().await?;
    let cat = &f.catalog;
    let id = account(cat, "ann@example.com", sc_types::Role::ADMIN).await?;
    let user = load_user(cat, id).await?.unwrap();
    assert!(!second_factor_needed(cat, &user).await?.is_needed());

    let secret = begin_second_factor_setup(cat, id).await?;
    assert!(
        !second_factor_enabled(cat, id).await?,
        "a setup waiting for its first code asks nothing at sign-in"
    );
    // A one-in-a-million chance the wrong code is right.
    if code(&secret, 0) != "000000" && code(&secret, -1) != "000000" {
        assert!(
            finish_second_factor_setup(cat, permit(cat, id).await?, id, "000000")
                .await?
                .is_none()
        );
    }
    // A code from the step before is accepted (clock drift), and turns it on.
    let codes = enable(cat, id, &secret, -1).await?;
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
    assert!(!right(cat, id, &code(&secret, -1)).await?);
    let proof = check(cat, id, &code(&secret, 0))
        .await?
        .expect("the current code");
    assert_eq!(proof.user(), id);
    assert!(
        !right(cat, id, &code(&secret, 0)).await?,
        "the same code twice"
    );

    // A recovery code once, however it is typed; then never again.
    let typed = codes[0].to_uppercase().replace('-', " ");
    assert!(right(cat, id, &typed).await?);
    assert!(!right(cat, id, &codes[0]).await?);
    assert_eq!(
        second_factor_status(cat, id).await?.recovery_codes_left,
        RECOVERY_CODE_COUNT - 1
    );
    // Another account's codes are no good here.
    let other = account(cat, "bob@example.com", sc_types::Role::ADMIN).await?;
    assert!(!right(cat, other, &codes[1]).await?);
    // Nor is a permit counted against another account.
    assert!(
        verify_second_factor(cat, permit(cat, other).await?, id, &codes[1])
            .await
            .is_err()
    );

    // New codes replace the old ones, for a right code.
    forgive(cat, id).await?;
    let fresh = regenerate_recovery_codes(cat, proof).await?;
    let fresh: Vec<String> = fresh.iter().map(|c| c.expose().to_owned()).collect();
    assert!(!right(cat, id, &codes[1]).await?);
    assert!(right(cat, id, &fresh[0]).await?);

    let removed = disable_second_factor(cat, id).await?;
    assert!(removed.existed);
    assert!(
        removed.changed.is_some(),
        "turning it off is a credential change"
    );
    assert!(!second_factor_enabled(cat, id).await?);
    assert!(!right(cat, id, &fresh[1]).await?);
    assert_eq!(count(cat, RECOVERY_CODES_TABLE).await?, 0);
    Ok(())
}

/// Recovery codes have 18 symbols (89 bits) and are stored under a keyed
/// hash; one made before that, under the old unkeyed SHA-256 of ten symbols,
/// still signs in once (security review 2026-11, B5).
#[tokio::test]
async fn recovery_codes_are_long_and_stored_under_a_keyed_hash() -> Result<()> {
    use sha2::{Digest, Sha256};
    let f = fixture().await?;
    let cat = &f.catalog;
    let id = account(cat, "keyed@example.com", sc_types::Role::ADMIN).await?;
    let secret = begin_second_factor_setup(cat, id).await?;
    let codes = enable(cat, id, &secret, 0).await?;
    for shown in &codes {
        let symbols: String = shown.chars().filter(char::is_ascii_alphanumeric).collect();
        assert_eq!(symbols.len(), 18, "{shown}");
    }
    let rows: Vec<sc_db::Row> = cat
        .primary(sc_catalog::Unchecked::system("test: read the hashes"))
        .query(&Statement::from(Select::from(Source::table(
            RECOVERY_CODES_TABLE,
        ))))
        .await?
        .try_collect()
        .await?;
    let unkeyed = |code: &str| -> String {
        let mut hasher = Sha256::new();
        hasher.update(id.as_bytes());
        hasher.update(code.replace('-', "").as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    for row in &rows {
        let Some(sc_query::Value::Text(hash)) = row.get("code_hash") else {
            panic!("a hash");
        };
        assert!(hash.starts_with("k1:"), "{hash}");
        assert!(codes.iter().all(|c| unkeyed(c) != *hash));
    }

    // A code from before, stored the old way.
    let legacy = "abcde-fghjk";
    let insert = sc_query::Insert::row(
        RECOVERY_CODES_TABLE,
        vec!["code_hash".into(), "user_id".into(), "created_at".into()],
        vec![
            sc_query::Expr::lit(unkeyed(legacy)),
            sc_query::Expr::lit(id),
            sc_query::Expr::Lit(sc_query::Value::Timestamp(Utc::now())),
        ],
    );
    cat.primary(sc_catalog::Unchecked::system("test: an old code"))
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    assert!(right(cat, id, "ABCDE FGHJK").await?);
    assert!(!right(cat, id, legacy).await?, "once");
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

    let token = issue_second_factor_challenge(cat, &signing_in(cat, id).await?, false).await?;
    // Read as often as a wrong code needs, and it says "Remember me" was off.
    let pending = second_factor_challenge(cat, &token).await?.unwrap();
    assert_eq!((pending.user, pending.remember), (id, false));
    assert!(second_factor_challenge(cat, &token).await?.is_some());
    assert_eq!(
        second_factor_challenge(cat, &ChallengeToken::presented("nonsense")).await?,
        None
    );
    // A challenge is not a password link.
    assert!(
        sc_auth::redeem_password_token(cat, token.expose(), "new-password")
            .await?
            .is_none()
    );
    let spent = spend_second_factor_challenge(cat, &token, None)
        .await?
        .unwrap();
    assert_eq!(spent.signing_in.id(), id);
    assert!(!spent.remember);
    assert!(
        spend_second_factor_challenge(cat, &token, None)
            .await?
            .is_none()
    );
    assert_eq!(second_factor_challenge(cat, &token).await?, None);

    let remembered = issue_second_factor_challenge(cat, &signing_in(cat, id).await?, true).await?;
    assert!(
        second_factor_challenge(cat, &remembered)
            .await?
            .unwrap()
            .remember
    );

    // Deleting the account takes its second factor with it.
    let secret = begin_second_factor_setup(cat, id).await?;
    enable(cat, id, &secret, 0).await?;
    assert_eq!(count(cat, SECOND_FACTOR_TABLE).await?, 1);
    assert!(delete_user(cat, id).await?);
    assert_eq!(count(cat, SECOND_FACTOR_TABLE).await?, 0);
    assert_eq!(count(cat, RECOVERY_CODES_TABLE).await?, 0);
    Ok(())
}

/// A sign-in waiting for its code ends when the account's credentials change
/// meanwhile — here an admin setting a new password (security review 2026-11,
/// B4; H2.3): the challenge is spent, and no sign-in comes of it.
#[tokio::test]
async fn a_credential_change_ends_a_sign_in_waiting_for_its_code() -> Result<()> {
    let f = fixture().await?;
    let cat = &f.catalog;
    let id = account(cat, "wait@example.com", sc_types::Role::ADMIN).await?;
    let before = signing_in(cat, id).await?;
    let token = issue_second_factor_challenge(cat, &before, true).await?;
    let changed = sc_auth::set_user_password(cat, id, "a-new-password").await?;
    let changed = changed.credentials.expect("a new password");
    assert_eq!(changed.user(), id);
    assert!(changed.epoch() > before.epoch());
    // The admin's password change took the challenge row with it…
    assert!(
        spend_second_factor_challenge(cat, &token, None)
            .await?
            .is_none()
    );
    // …and one issued before a change that does not touch it (the second
    // factor turned off) is no good either.
    let before = signing_in(cat, id).await?;
    let token = issue_second_factor_challenge(cat, &before, true).await?;
    let secret = begin_second_factor_setup(cat, id).await?;
    enable(cat, id, &secret, 0).await?;
    assert!(
        spend_second_factor_challenge(cat, &token, None)
            .await?
            .is_none()
    );
    Ok(())
}

/// Guesses at a code are counted before the code is checked, in the
/// database: the account gets `MAX_GUESSES` in the window and then none, and
/// a right code gives its guess back (security review 2026-11, #9; H2.4).
#[tokio::test]
async fn guesses_are_reserved_in_the_database_and_a_right_one_is_given_back() -> Result<()> {
    let f = fixture().await?;
    let cat = &f.catalog;
    let id = account(cat, "guess@example.com", sc_types::Role::ADMIN).await?;
    // No second factor: nothing to guess, nothing counted.
    for _ in 0..(sc_auth::MAX_GUESSES + 2) {
        assert!(!right(cat, id, "123456").await?);
    }
    let secret = begin_second_factor_setup(cat, id).await?;
    let codes = enable(cat, id, &secret, 0).await?;
    // A right code's guess is given back, so it never uses the budget up.
    for shown in codes
        .iter()
        .take(usize::try_from(sc_auth::MAX_GUESSES + 2).unwrap())
    {
        assert!(right(cat, id, shown).await?, "right codes are given back");
    }
    for _ in 0..sc_auth::MAX_GUESSES {
        let wrong = if code(&secret, 0) == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert!(!right(cat, id, wrong).await?);
    }
    match reserve_attempt(cat, id).await? {
        Attempt::Limited { retry_after_secs } => {
            assert!(retry_after_secs > 0 && retry_after_secs <= 15 * 60);
        }
        Attempt::Permitted(_) => panic!("the budget is spent"),
    }
    Ok(())
}
