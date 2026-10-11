//! `_fd_api_tokens` against real Postgres (design §13.6, TODO phase 2).
//!
//! Every property here is one the credential's *value* rests on, so each is
//! asserted against the table rather than against the value returned beside it:
//!
//! - **The plaintext is not recoverable from the table.** The row holds a hash;
//!   nothing anywhere holds the token after `mint_api_token` returns.
//! - **Revoked, expired, deleted, disabled and demoted all stop working**, each
//!   with its own message, because each is a different thing to tell the holder.
//! - **`last_used_at` is written once a minute**, not once a call: a token is on
//!   the request path and a write there is a write on every tool call.
//! - **The table is logged**, which is the one thing `_fd_sessions` is not, and
//!   the difference between a lost session and a support call.

use std::sync::Arc;

use chrono::{Duration, Utc};
use sc_auth::{
    API_TOKENS_TABLE, COL_TOKEN_ID, NewApiToken, ROLE_ADMIN, ROLE_PUBLIC, TOKEN_PREFIX, User,
    UserUpdate, authenticate_api_token, bootstrap, create_user, delete_user, list_api_tokens,
    mint_api_token, revoke_api_token, set_user_disabled, update_user,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Expr, Select, Source, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::json;

/// A database with the auth tables bootstrapped, and a handle on it.
struct Fixture {
    catalog: Arc<Catalog>,
    db: TestDb,
}

async fn fixture() -> Result<Fixture> {
    let db = TestDb::new().await?;
    // As `db_sessions`: the v1 test template carries `users` tables in several
    // schemas, and bootstrap must find none of them.
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
    Ok(Fixture { catalog, db })
}

/// An admin to mint for.
async fn admin(catalog: &Catalog, email: &str) -> Result<User> {
    create_user(catalog, email, "pw", ROLE_ADMIN).await
}

/// The six flags as a token stores them.
fn grants() -> Attrs {
    json!({
        "allow_create": true,
        "allow_edit": true,
        "allow_drop": false,
        "allow_access_changes": false,
        "allow_triggers": true,
        "allow_applications": true,
    })
    .as_object()
    .cloned()
    .expect("an object")
}

fn new_token(user: &User, label: &str) -> NewApiToken {
    NewApiToken {
        user_id: user.id,
        label: label.to_owned(),
        grants: grants(),
        expires_at: None,
    }
}

/// Every row of the table, as raw values — the only way to ask what is actually
/// stored rather than what the API says is.
async fn raw_rows(catalog: &Catalog) -> Result<Vec<sc_db::Row>> {
    catalog
        .primary()
        .query(&Statement::from(Select::from(Source::table(
            API_TOKENS_TABLE,
        ))))
        .await?
        .try_collect()
        .await
}

/// The credential works, and the table it came from cannot give it back.
#[tokio::test]
async fn a_minted_token_authenticates_and_its_plaintext_is_not_in_the_table() -> Result<()> {
    let f = fixture().await?;
    let user = admin(&f.catalog, "a@example.com").await?;

    let minted = mint_api_token(&f.catalog, new_token(&user, "claude-code on my laptop")).await?;
    assert!(minted.secret.starts_with(TOKEN_PREFIX));

    let caller = authenticate_api_token(&f.catalog, &minted.secret).await?;
    assert_eq!(caller.user.id, user.id);
    assert_eq!(caller.user.role, ROLE_ADMIN);
    assert_eq!(caller.token.id, minted.token.id);
    // The grants arrive back as they were minted: this crate stores them and
    // does not interpret them (§13.6 — the vocabulary is `sc-api::mcp`'s).
    assert_eq!(caller.token.grants, grants());

    // Now the property the whole design rests on. Every value in every column of
    // every row, rendered — the secret appears in none of them, and neither does
    // any prefix of it long enough to be worth guessing from.
    let rows = raw_rows(&f.catalog).await?;
    assert_eq!(rows.len(), 1);
    let stored = format!("{:?}", rows[0]);
    assert!(
        !stored.contains(&minted.secret),
        "the token itself must not be in the table"
    );
    assert!(
        !stored.contains(minted.secret.trim_start_matches(TOKEN_PREFIX)),
        "nor its random half without the prefix"
    );
    // What *is* there is a 64-character hex digest.
    let Some(Value::Text(hash)) = rows[0].get("token_hash") else {
        panic!("token_hash should be text");
    };
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

    // And the listing — what an admin screen is made of — carries neither.
    let listed = list_api_tokens(&f.catalog).await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].label, "claude-code on my laptop");
    let rendered = format!("{listed:?}");
    assert!(!rendered.contains(&minted.secret));
    assert!(!rendered.contains(hash.as_str()));
    Ok(())
}

/// Revocation is the only way to take a token back, so it has to actually take
/// it back — and it has to leave the evidence that it happened.
#[tokio::test]
async fn a_revoked_token_stops_working_and_stays_in_the_list() -> Result<()> {
    let f = fixture().await?;
    let user = admin(&f.catalog, "a@example.com").await?;
    let minted = mint_api_token(&f.catalog, new_token(&user, "laptop")).await?;
    authenticate_api_token(&f.catalog, &minted.secret).await?;

    assert!(revoke_api_token(&f.catalog, minted.token.id).await?);
    let err = authenticate_api_token(&f.catalog, &minted.secret)
        .await
        .expect_err("a revoked token authenticates nobody");
    let message = err.to_string();
    assert!(message.contains("revoked"), "{message}");
    assert!(
        message.contains("laptop"),
        "the refusal names it: {message}"
    );

    // The row stays, marked: a revocation is a thing that happened and the list
    // is where an admin sees that it did.
    let listed = list_api_tokens(&f.catalog).await?;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked_at.is_some());
    assert!(!listed[0].is_live(Utc::now()));

    // Revoking twice changes nothing, and says so rather than failing.
    assert!(!revoke_api_token(&f.catalog, minted.token.id).await?);
    // A token nobody has is not an error either.
    assert!(!revoke_api_token(&f.catalog, uuid::Uuid::new_v4()).await?);
    Ok(())
}

/// An expiry is what makes a ninety-day credential defensible, so it must be
/// enforced at the lookup rather than by anyone remembering to sweep.
#[tokio::test]
async fn an_expired_token_stops_working() -> Result<()> {
    let f = fixture().await?;
    let user = admin(&f.catalog, "a@example.com").await?;

    // Minted in the future, then moved into the past, because minting an
    // already-expired token is itself refused — the two are different mistakes.
    let expiring = NewApiToken {
        expires_at: Some(Utc::now() + Duration::hours(1)),
        ..new_token(&user, "short-lived")
    };
    let minted = mint_api_token(&f.catalog, expiring).await?;
    authenticate_api_token(&f.catalog, &minted.secret).await?;

    expire(&f, minted.token.id, Utc::now() - Duration::minutes(1)).await?;
    let err = authenticate_api_token(&f.catalog, &minted.secret)
        .await
        .expect_err("an expired token authenticates nobody");
    assert!(err.to_string().contains("expired"), "{err}");

    // And minting one that has already lapsed is refused up front.
    let err = mint_api_token(
        &f.catalog,
        NewApiToken {
            expires_at: Some(Utc::now() - Duration::seconds(1)),
            ..new_token(&user, "born expired")
        },
    )
    .await
    .expect_err("an expiry in the past is a mistake, not a token");
    assert!(err.to_string().contains("in the past"), "{err}");
    Ok(())
}

/// The reason the row names the user rather than copying them: what the account
/// may do is read fresh on every call, so a demotion, a disabling and a deletion
/// each take the token with them.
#[tokio::test]
async fn a_token_is_no_stronger_than_the_account_it_names() -> Result<()> {
    let f = fixture().await?;
    // Somebody to stay admin: the last enabled admin cannot be disabled or
    // deleted (TODO F1.9), and this test is about tokens, not about that.
    admin(&f.catalog, "keeper@example.com").await?;

    // Demoted below admin.
    let demoted = admin(&f.catalog, "demoted@example.com").await?;
    let theirs = mint_api_token(&f.catalog, new_token(&demoted, "demoted")).await?;
    authenticate_api_token(&f.catalog, &theirs.secret).await?;
    update_user(
        &f.catalog,
        demoted.id,
        UserUpdate {
            role: Some(ROLE_PUBLIC),
            ..UserUpdate::default()
        },
    )
    .await?;
    let err = authenticate_api_token(&f.catalog, &theirs.secret)
        .await
        .expect_err("a demoted user's token is not an administrator's");
    assert!(err.to_string().contains("administrator"), "{err}");

    // Disabled.
    let disabled = admin(&f.catalog, "disabled@example.com").await?;
    let theirs = mint_api_token(&f.catalog, new_token(&disabled, "disabled")).await?;
    set_user_disabled(&f.catalog, disabled.id, true).await?;
    let err = authenticate_api_token(&f.catalog, &theirs.secret)
        .await
        .expect_err("a disabled account's token is not a credential");
    assert!(err.to_string().contains("disabled"), "{err}");

    // Deleted — and the delete itself must not be blocked by the token, which is
    // why `user_id` is not a foreign key.
    let deleted = admin(&f.catalog, "deleted@example.com").await?;
    let theirs = mint_api_token(&f.catalog, new_token(&deleted, "deleted")).await?;
    assert!(delete_user(&f.catalog, deleted.id).await?);
    let err = authenticate_api_token(&f.catalog, &theirs.secret)
        .await
        .expect_err("there is nobody left to run as");
    assert!(err.to_string().contains("no longer exists"), "{err}");
    Ok(())
}

/// Ten calls in a minute are one write. `last_used_at` is worth having and is
/// not worth a write on every tool call.
#[tokio::test]
async fn last_used_is_written_once_a_minute_and_never_on_a_refusal() -> Result<()> {
    let f = fixture().await?;
    let user = admin(&f.catalog, "a@example.com").await?;
    let minted = mint_api_token(&f.catalog, new_token(&user, "busy")).await?;

    // Freshly minted: nothing has used it yet.
    assert!(list_api_tokens(&f.catalog).await?[0].last_used_at.is_none());

    let mut seen = Vec::new();
    for _ in 0..10 {
        let caller = authenticate_api_token(&f.catalog, &minted.secret).await?;
        seen.push(caller.token.last_used_at.expect("a use is recorded"));
    }
    // Every call reports the same instant, because only the first wrote it.
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "ten calls in one minute should share one `last_used_at`: {seen:?}"
    );
    assert_eq!(
        list_api_tokens(&f.catalog).await?[0].last_used_at,
        Some(seen[0])
    );

    // A refusal writes nothing at all — a replayed stolen token must not be able
    // to churn this table.
    assert!(revoke_api_token(&f.catalog, minted.token.id).await?);
    assert!(
        authenticate_api_token(&f.catalog, &minted.secret)
            .await
            .is_err()
    );
    assert_eq!(
        list_api_tokens(&f.catalog).await?[0].last_used_at,
        Some(seen[0])
    );
    Ok(())
}

/// The two refusals that come before the table is trusted at all, and the sweep
/// that collects what has lapsed.
#[tokio::test]
async fn an_unrecognised_credential_is_told_apart_from_a_malformed_one() -> Result<()> {
    let f = fixture().await?;
    let user = admin(&f.catalog, "a@example.com").await?;

    let err = authenticate_api_token(&f.catalog, "not-a-token")
        .await
        .expect_err("no prefix, no token");
    assert!(err.to_string().contains(TOKEN_PREFIX), "{err}");

    let err = authenticate_api_token(&f.catalog, &format!("{TOKEN_PREFIX}deadbeef"))
        .await
        .expect_err("well-formed and unknown");
    assert!(err.to_string().contains("not recognised"), "{err}");

    // The sweep rides along with a mint, and takes lapsed rows only: a revoked
    // token is evidence and stays.
    let lapsed = mint_api_token(
        &f.catalog,
        NewApiToken {
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..new_token(&user, "lapsed")
        },
    )
    .await?;
    let revoked = mint_api_token(&f.catalog, new_token(&user, "revoked")).await?;
    revoke_api_token(&f.catalog, revoked.token.id).await?;
    expire(&f, lapsed.token.id, Utc::now() - Duration::minutes(1)).await?;

    mint_api_token(&f.catalog, new_token(&user, "fresh")).await?;
    let labels: Vec<String> = list_api_tokens(&f.catalog)
        .await?
        .into_iter()
        .map(|t| t.label)
        .collect();
    assert!(!labels.contains(&"lapsed".to_owned()), "{labels:?}");
    assert!(labels.contains(&"revoked".to_owned()), "{labels:?}");
    assert!(labels.contains(&"fresh".to_owned()), "{labels:?}");
    Ok(())
}

/// The one difference from `_fd_sessions`: this table is worth a WAL record,
/// because a truncated session table costs a re-login and a truncated token
/// table costs a support call.
#[tokio::test]
async fn the_token_table_is_logged() -> Result<()> {
    let f = fixture().await?;
    let row =
        f.db.client()
            .await?
            .query_one(
                "SELECT relpersistence::text FROM pg_class WHERE relname = $1",
                &[&API_TOKENS_TABLE],
            )
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let persistence: String = row.get(0);
    assert_eq!(
        persistence, "p",
        "{API_TOKENS_TABLE} should be an ordinary logged table"
    );
    Ok(())
}

/// A label is what the audit line names a token by, so a blank one is refused
/// rather than stored.
#[tokio::test]
async fn a_token_needs_a_label() -> Result<()> {
    let f = fixture().await?;
    let user = admin(&f.catalog, "a@example.com").await?;
    let err = mint_api_token(&f.catalog, new_token(&user, "   "))
        .await
        .expect_err("a blank label is not a label");
    assert!(err.to_string().contains("label"), "{err}");
    assert!(list_api_tokens(&f.catalog).await?.is_empty());
    Ok(())
}

/// Move a token's expiry, which is not something the API offers: an expiry is
/// fixed at mint, and a test that wants a lapsed one has to reach past the API
/// to make it. Doing it in SQL rather than by sleeping keeps the test honest and
/// fast.
async fn expire(f: &Fixture, id: uuid::Uuid, at: chrono::DateTime<Utc>) -> Result<()> {
    let update = sc_query::Update::new(
        API_TOKENS_TABLE,
        vec![sc_query::Assignment::new(
            "expires_at".to_owned(),
            Expr::Lit(Value::Timestamp(at)),
        )],
    )
    .filter(Expr::col(COL_TOKEN_ID).eq(Expr::lit(id)));
    f.catalog
        .primary()
        .query(&Statement::from(update))
        .await?
        .try_collect()
        .await?;
    Ok(())
}
