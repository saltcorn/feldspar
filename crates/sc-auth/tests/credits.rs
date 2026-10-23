//! LLM credits against real Postgres (TODO-payments P6.1, design C3).
//!
//! The ledger's promises, each asserted on the database:
//!
//! - **concurrent debits never overdraw**: many debits at once, for one user,
//!   take exactly their sum, no bucket goes below zero, and what the buckets
//!   could not cover is debt, not lost;
//! - **a replayed grant does nothing**, and nor does a debit for a step already
//!   charged;
//! - debits take from the **soonest-expiring** bucket first, and an expired
//!   bucket does not count, with no write at its expiry;
//! - **debt is repaid** by the next grant before it fills its bucket;
//! - a debit that takes the balance below the user's threshold writes **one**
//!   `low_balance` entry, not one per later call.

use std::sync::Arc;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sc_auth::{
    CREDIT_ACCOUNTS_TABLE, CREDIT_BUCKETS_TABLE, CREDIT_ENTRIES_TABLE, CreditDebit,
    ENTRY_LOW_BALANCE, NewCreditGrant, ROLE_ADMIN, Role, bootstrap, create_user, credit_balance,
    debit_credits, delete_user, grant_credits, list_credit_buckets, list_credit_entries, save_role,
    set_low_balance_threshold,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
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
    bootstrap(&catalog).await?;
    save_role(&catalog, &Role::new(80, "Member")).await?;
    Ok(Fixture { catalog, _db: db })
}

fn d(n: i64) -> Decimal {
    Decimal::from(n)
}

fn grant(user: Uuid, amount: i64, key: &str) -> NewCreditGrant {
    NewCreditGrant {
        user_id: user,
        amount: d(amount),
        source: "test".into(),
        key: key.into(),
        kind: String::new(),
        expires_at: None,
        note: None,
    }
}

fn debit(user: Uuid, amount: Decimal, key: &str) -> CreditDebit {
    CreditDebit {
        user_id: user,
        amount,
        key: key.into(),
        ..CreditDebit::default()
    }
}

#[tokio::test]
async fn the_tables_are_ordinary_owner_readable_ones() -> Result<()> {
    let f = fixture().await?;
    for name in [
        CREDIT_BUCKETS_TABLE,
        CREDIT_ENTRIES_TABLE,
        CREDIT_ACCOUNTS_TABLE,
    ] {
        let table = f.catalog.require(name)?;
        assert!(!table.is_system(), "{name}: an application reads it");
        assert_eq!(table.access.min_role_write, ROLE_ADMIN, "{name}");
        assert_eq!(
            table.ownership.as_ref().map(|f| f.source()),
            Some(sc_auth::CREDITS_OWNERSHIP),
            "{name}: {:?}",
            table.ownership_error
        );
    }
    let buckets = f.catalog.require(CREDIT_BUCKETS_TABLE)?;
    bootstrap(&f.catalog).await?;
    assert_eq!(f.catalog.require(CREDIT_BUCKETS_TABLE)?, buckets);
    Ok(())
}

#[tokio::test]
async fn a_replayed_grant_does_nothing() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "a@example.com", "pw", 80).await?;
    let first = grant_credits(&f.catalog, grant(user.id, 100, "invoice:in_1"), None).await?;
    assert!(!first.replayed);
    assert_eq!(first.balance, d(100));

    let again = grant_credits(&f.catalog, grant(user.id, 100, "invoice:in_1"), None).await?;
    assert!(again.replayed, "the same key is the same grant");
    assert_eq!(again.bucket.id, first.bucket.id);
    assert_eq!(credit_balance(&f.catalog, user.id).await?.balance, d(100));
    assert_eq!(list_credit_buckets(&f.catalog, user.id).await?.len(), 1);
    assert_eq!(list_credit_entries(&f.catalog, user.id, 10).await?.len(), 1);

    // The same key from another source is another grant.
    let other = NewCreditGrant {
        source: "admin".into(),
        ..grant(user.id, 5, "invoice:in_1")
    };
    assert!(!grant_credits(&f.catalog, other, None).await?.replayed);
    assert_eq!(credit_balance(&f.catalog, user.id).await?.balance, d(105));

    // And a key used for one user is refused for another, not handed over.
    let someone = create_user(&f.catalog, "b@example.com", "pw", 80).await?;
    let stolen = grant_credits(&f.catalog, grant(someone.id, 100, "invoice:in_1"), None).await;
    assert!(stolen.is_err(), "{stolen:?}");
    assert_eq!(credit_balance(&f.catalog, someone.id).await?.balance, d(0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_debits_never_overdraw() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "c@example.com", "pw", 80).await?;
    grant_credits(&f.catalog, grant(user.id, 10, "a"), None).await?;
    grant_credits(
        &f.catalog,
        NewCreditGrant {
            expires_at: Some(Utc::now() + Duration::days(1)),
            ..grant(user.id, 5, "b")
        },
        None,
    )
    .await?;

    // Forty debits of 0.5 at once: 20 against 15 in the buckets.
    let half = Decimal::new(5, 1);
    let tasks: Vec<_> = (0..40)
        .map(|i| {
            let catalog = f.catalog.clone();
            let user = user.id;
            tokio::spawn(async move {
                debit_credits(&catalog, debit(user, half, &format!("run:{i}")), None).await
            })
        })
        .collect();
    for task in tasks {
        let debited = task.await.expect("the task ran")?;
        assert!(!debited.replayed);
    }

    let balance = credit_balance(&f.catalog, user.id).await?;
    assert_eq!(balance.balance, d(-5), "exactly the sum was taken");
    assert_eq!(balance.available, d(0));
    assert_eq!(balance.debt, d(5), "what the buckets lacked is owed");
    for bucket in list_credit_buckets(&f.catalog, user.id).await? {
        assert_eq!(bucket.remaining, d(0), "no bucket below zero: {bucket:?}");
    }
    // Every debit recorded the balance after it, and the forty balances are the
    // forty steps from 14.5 down to -5: no two debits saw the same balance.
    let mut after: Vec<Decimal> = list_credit_entries(&f.catalog, user.id, 100)
        .await?
        .into_iter()
        .filter(|e| e.kind == "debit")
        .map(|e| e.balance)
        .collect();
    after.sort();
    let expected: Vec<Decimal> = (0..40).map(|i| d(-5) + half * Decimal::from(i)).collect();
    assert_eq!(after, expected);
    Ok(())
}

#[tokio::test]
async fn a_step_already_charged_is_not_charged_again() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "r@example.com", "pw", 80).await?;
    grant_credits(&f.catalog, grant(user.id, 10, "g"), None).await?;
    let first = debit_credits(&f.catalog, debit(user.id, d(3), "run-1:1"), None).await?;
    assert_eq!(first.balance, d(7));
    let again = debit_credits(&f.catalog, debit(user.id, d(3), "run-1:1"), None).await?;
    assert!(again.replayed);
    assert_eq!(again.balance, d(7));
    Ok(())
}

#[tokio::test]
async fn the_soonest_expiring_bucket_is_spent_first_and_an_expired_one_does_not_count() -> Result<()>
{
    let f = fixture().await?;
    let user = create_user(&f.catalog, "e@example.com", "pw", 80).await?;
    let forever = grant_credits(&f.catalog, grant(user.id, 10, "forever"), None).await?;
    let month = grant_credits(
        &f.catalog,
        NewCreditGrant {
            expires_at: Some(Utc::now() + Duration::days(30)),
            ..grant(user.id, 10, "month")
        },
        None,
    )
    .await?;
    let soon = grant_credits(
        &f.catalog,
        NewCreditGrant {
            expires_at: Some(Utc::now() + Duration::milliseconds(1500)),
            ..grant(user.id, 4, "soon")
        },
        None,
    )
    .await?;

    debit_credits(&f.catalog, debit(user.id, d(6), "run:1"), None).await?;
    let remaining = |id: Uuid, buckets: &[sc_auth::CreditBucket]| {
        buckets.iter().find(|b| b.id == id).unwrap().remaining
    };
    let buckets = list_credit_buckets(&f.catalog, user.id).await?;
    assert_eq!(remaining(soon.bucket.id, &buckets), d(0), "soonest first");
    assert_eq!(remaining(month.bucket.id, &buckets), d(8), "then the next");
    assert_eq!(
        remaining(forever.bucket.id, &buckets),
        d(10),
        "never-expiring last"
    );

    // Expiry is a read: grant a little more to the soon bucket's kind and let
    // it lapse, with nothing written at the moment it does.
    grant_credits(
        &f.catalog,
        NewCreditGrant {
            expires_at: Some(Utc::now() + Duration::milliseconds(800)),
            ..grant(user.id, 3, "brief")
        },
        None,
    )
    .await?;
    assert_eq!(credit_balance(&f.catalog, user.id).await?.balance, d(21));
    tokio::time::sleep(std::time::Duration::from_millis(1600)).await;
    assert_eq!(credit_balance(&f.catalog, user.id).await?.balance, d(18));
    Ok(())
}

#[tokio::test]
async fn debt_is_repaid_by_the_next_grant() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "d@example.com", "pw", 80).await?;
    // No credits at all: the call has happened, so the debit is debt.
    let debited = debit_credits(&f.catalog, debit(user.id, d(4), "run:1"), None).await?;
    assert_eq!(debited.balance, d(-4));

    let small = grant_credits(&f.catalog, grant(user.id, 3, "g1"), None).await?;
    assert_eq!(small.repaid, d(3));
    assert_eq!(small.bucket.remaining, d(0));
    assert_eq!(small.balance, d(-1));

    let big = grant_credits(&f.catalog, grant(user.id, 10, "g2"), None).await?;
    assert_eq!(big.repaid, d(1));
    assert_eq!(big.bucket.amount, d(10));
    assert_eq!(big.bucket.remaining, d(9));
    let balance = credit_balance(&f.catalog, user.id).await?;
    assert_eq!((balance.balance, balance.debt), (d(9), d(0)));
    Ok(())
}

#[tokio::test]
async fn crossing_the_threshold_writes_one_low_balance_entry() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "l@example.com", "pw", 80).await?;
    grant_credits(&f.catalog, grant(user.id, 10, "g"), None).await?;
    set_low_balance_threshold(&f.catalog, user.id, Some(d(5)), None).await?;

    assert!(
        !debit_credits(&f.catalog, debit(user.id, d(4), "s1"), None)
            .await?
            .low_balance
    );
    assert!(
        debit_credits(&f.catalog, debit(user.id, d(2), "s2"), None)
            .await?
            .low_balance
    );
    assert!(
        !debit_credits(&f.catalog, debit(user.id, d(1), "s3"), None)
            .await?
            .low_balance,
        "already below: no second notice"
    );
    let notices = list_credit_entries(&f.catalog, user.id, 20)
        .await?
        .into_iter()
        .filter(|e| e.kind == ENTRY_LOW_BALANCE)
        .count();
    assert_eq!(notices, 1);

    // A top-up lifts it back over, and the next fall is a new notice.
    grant_credits(&f.catalog, grant(user.id, 10, "top-up"), None).await?;
    assert!(
        debit_credits(&f.catalog, debit(user.id, d(10), "s4"), None)
            .await?
            .low_balance
    );
    Ok(())
}

#[tokio::test]
async fn nonsense_is_refused_and_deleting_the_user_deletes_their_credits() -> Result<()> {
    let f = fixture().await?;
    let user = create_user(&f.catalog, "x@example.com", "pw", 80).await?;
    assert!(
        grant_credits(&f.catalog, grant(user.id, 0, "zero"), None)
            .await
            .is_err()
    );
    assert!(
        grant_credits(&f.catalog, grant(user.id, 1, " "), None)
            .await
            .is_err(),
        "a grant needs a key"
    );
    let expired = NewCreditGrant {
        expires_at: Some(Utc::now() - Duration::seconds(1)),
        ..grant(user.id, 1, "late")
    };
    assert!(grant_credits(&f.catalog, expired, None).await.is_err());
    assert!(
        debit_credits(&f.catalog, debit(user.id, d(-1), "neg"), None)
            .await
            .is_err(),
        "a debit cannot give credits"
    );
    assert!(
        grant_credits(&f.catalog, grant(Uuid::new_v4(), 1, "nobody"), None)
            .await
            .is_err(),
        "no such user"
    );

    grant_credits(&f.catalog, grant(user.id, 5, "g"), None).await?;
    debit_credits(&f.catalog, debit(user.id, d(1), "s"), None).await?;
    assert!(delete_user(&f.catalog, user.id).await?);
    assert!(list_credit_buckets(&f.catalog, user.id).await?.is_empty());
    assert!(
        list_credit_entries(&f.catalog, user.id, 10)
            .await?
            .is_empty()
    );
    Ok(())
}
