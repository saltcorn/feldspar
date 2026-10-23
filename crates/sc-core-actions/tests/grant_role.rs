//! `grant_role` against a real database (TODO-payments P3.4, design C8): a
//! card-less trial granted when a user is created — the insert on `users` that
//! signing up raises — is granted once, and not again after it has expired.
//!
//! Also pinned: the grant is the role the user is then served at, the key is
//! per user (two users' `"trial"` keys are two grants), and a configuration
//! that cannot mean anything is refused on save.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_action::{ActionContext, Event, EventKind, Trigger, validate_trigger};
use sc_auth::{Role, create_user, list_role_grants, load_user, save_role};
use sc_catalog::Catalog;
use sc_core_actions::builtin_actions;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

async fn setup(db: &TestDb) -> Result<Catalog> {
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
        .map_err(|e| Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_auth::bootstrap(&catalog).await?;
    save_role(&catalog, &Role::new(70, "Trial")).await?;
    save_role(&catalog, &Role::new(80, "Member")).await?;
    Ok(catalog)
}

fn config(entries: &[(&str, Json)]) -> Attrs {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// The trial: a fortnight of role 70, once per user.
fn trial() -> Attrs {
    config(&[
        ("user", json!("row.id")),
        ("role", json!(70)),
        ("duration_days", json!(14)),
        ("key", json!("\"trial\"")),
        ("once", json!(true)),
        ("note", json!("14-day trial")),
    ])
}

/// The event signing up raises: an insert on `users`.
fn signed_up(user: &sc_auth::User) -> Event {
    Event::new(EventKind::Insert)
        .on("users")
        .row(json!({ "id": user.id.to_string(), "email": "new@example.com", "role": 80 }))
}

async fn run(catalog: &Catalog, event: &Event, cfg: &Attrs) -> Result<Json> {
    let engine: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let registry = builtin_actions()?;
    let action = registry.require("grant_role")?.clone();
    let mut ctx =
        ActionContext::new(catalog, event, cfg, "trial-on-signup").with_evaluator(&engine);
    action.run(&mut ctx).await
}

#[tokio::test]
async fn a_trial_granted_on_sign_up_is_not_granted_again_after_it_expires() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let user = create_user(&catalog, "new@example.com", "pw", 80).await?;
    let registry = builtin_actions()?;
    let trigger = Trigger::new("trial-on-signup", EventKind::Insert, "grant_role")
        .on("users")
        .with_configuration(trial());
    validate_trigger(&catalog, &registry, &trigger).await?;

    let first = run(&catalog, &signed_up(&user), &trial()).await?;
    assert_eq!(first["granted"], json!(true), "{first}");
    assert_eq!(first["role"], json!(70));
    assert_eq!(load_user(&catalog, user.id).await?.unwrap().role, 70);
    let grants = list_role_grants(&catalog, user.id).await?;
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].source, "grant_role");
    assert_eq!(grants[0].note.as_deref(), Some("14-day trial"));
    let days = (grants[0].expires_at.unwrap() - chrono::Utc::now()).num_hours();
    assert!((335..=336).contains(&days), "a fortnight: {days} hours");

    // The same event again — a replayed trigger — grants nothing more.
    let again = run(&catalog, &signed_up(&user), &trial()).await?;
    assert_eq!(again["granted"], json!(false), "{again}");

    // The trial runs out…
    db.client()
        .await?
        .execute(
            "UPDATE role_grants SET expires_at = now() - interval '1 day', \
             starts_at = now() - interval '15 days'",
            &[],
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    assert_eq!(load_user(&catalog, user.id).await?.unwrap().role, 80);
    // …and is still not granted again.
    let after = run(&catalog, &signed_up(&user), &trial()).await?;
    assert_eq!(after["granted"], json!(false), "{after}");
    assert_eq!(load_user(&catalog, user.id).await?.unwrap().role, 80);
    assert_eq!(list_role_grants(&catalog, user.id).await?.len(), 1);

    // The key is per user: somebody else signing up gets a trial of their own.
    let other = create_user(&catalog, "other@example.com", "pw", 80).await?;
    let theirs = run(&catalog, &signed_up(&other), &trial()).await?;
    assert_eq!(theirs["granted"], json!(true), "{theirs}");
    assert_eq!(load_user(&catalog, user.id).await?.unwrap().role, 80);

    // Without `once`, a second run extends the grant rather than adding one.
    let comp = config(&[
        ("user", json!("row.id")),
        ("role", json!(70)),
        ("expires_at", json!("\"2099-01-01T00:00:00Z\"")),
        ("key", json!("\"comp\"")),
    ]);
    run(&catalog, &signed_up(&user), &comp).await?;
    run(&catalog, &signed_up(&user), &comp).await?;
    assert_eq!(list_role_grants(&catalog, user.id).await?.len(), 2);
    assert_eq!(load_user(&catalog, user.id).await?.unwrap().role, 70);
    Ok(())
}

#[tokio::test]
async fn a_grant_that_cannot_mean_anything_is_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let registry = builtin_actions()?;
    let cases: &[(&[(&str, Json)], &str)] = &[
        (&[("role", json!(1))], "admin role"),
        (&[("role", json!(55))], "does not exist"),
        (&[("role", json!(100))], "public"),
        (
            &[("expires_at", json!("\"2099-01-01T00:00:00Z\""))],
            "not both",
        ),
        (&[("duration_days", json!(-3))], "positive number of days"),
        (&[("user", json!("row.nope"))], "nope"),
    ];
    for (changes, expected) in cases {
        let mut cfg = trial();
        for (k, v) in *changes {
            cfg.insert((*k).to_owned(), v.clone());
        }
        let trigger = Trigger::new("trial-on-signup", EventKind::Insert, "grant_role")
            .on("users")
            .with_configuration(cfg);
        let msg = validate_trigger(&catalog, &registry, &trigger)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains(expected), "expected `{expected}` in: {msg}");
    }
    Ok(())
}
