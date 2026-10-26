//! Brew Notes, the subscription SaaS of
//! [docs/tutorial-payments-saas.md](../../../docs/tutorial-payments-saas.md) (TODO-payments P4.5),
//! built the way the tutorial builds it, so its tables, binding and triggers do what it says:
//!
//! - signing up gives a fortnight of Basic with no card (`grant_role` on `login`, `once`), and a
//!   second sign-in gives no second trial;
//! - the tables' rules are the paywall: blends need Basic, flights need Pro;
//! - the daily reminder writes one reminder, and one email, for a trial ending tomorrow, and none
//!   once the user has subscribed;
//! - subscribing to Pro through `pay_checkout` serves the user at Pro; the account page reads the
//!   subscription's renewal date and the trial from the API;
//! - a plan change in the portal moves the role, and a cancellation, at the period's end, leaves
//!   the user on the free tier.
//!
//! Needs `npm`, which installs the module, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{StatusCode, header};
use sc_action::{ATTR_HOUR, EventKind, Trigger, save_trigger};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, TriggerRef};
use sc_catalog::{Catalog, DataFieldKind, FieldId, TableId};
use sc_server::MountedApp;
use serde_json::json;

use crate::payments_module::{error_of, have_npm, send};
use crate::tutorial_payments::{Booted, Browser, boot};

const NOTES: &str = "notes.example.com";

fn field(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    }
}

fn user_key(name: &str) -> FieldSpec {
    FieldSpec {
        kind: DataFieldKind::Key {
            target_table: TableId("users".to_owned()),
            target_field: FieldId(String::new()),
            summary_field: None,
        },
        required: true,
        ..field(name, "")
    }
}

fn id() -> FieldSpec {
    FieldSpec {
        primary_key: true,
        ..field("id", "int")
    }
}

/// Steps 1, 3 and 9's tables.
async fn create_tables(catalog: &Catalog) -> sc_error::Result<()> {
    let owner = || FieldSpec {
        default_expression: Some("user.id".to_owned()),
        ..user_key("owner")
    };
    let tables: Vec<(&str, u8, Option<&str>, Vec<FieldSpec>)> = vec![
        (
            "plans",
            100,
            None,
            vec![
                id(),
                FieldSpec {
                    required: true,
                    ..field("name", "text")
                },
                field("price", "text"),
                field("stripe_price", "text"),
                FieldSpec {
                    required: true,
                    ..field("role", "int")
                },
            ],
        ),
        (
            "notes",
            1,
            Some("owner === user.id"),
            vec![id(), owner(), field("text", "text")],
        ),
        (
            "blends",
            1,
            Some("owner === user.id && user.role <= 70"),
            vec![id(), owner(), field("text", "text")],
        ),
        (
            "flights",
            1,
            Some("owner === user.id && user.role <= 60"),
            vec![id(), owner(), field("text", "text")],
        ),
        (
            "reminders",
            1,
            None,
            vec![id(), user_key("user"), field("ends", "timestamp")],
        ),
    ];
    for (name, read, ownership, fields) in tables {
        schema_edit::apply(
            catalog,
            &[Operation::CreateTable {
                name: name.into(),
                database: String::new(),
                settings: TableSettings {
                    min_role_read: Some(read),
                    min_role_write: Some(1),
                    ownership_formula: ownership.map(str::to_owned),
                    ..TableSettings::default()
                },
                fields,
            }],
            &ApplyOptions::default(),
        )
        .await?;
    }
    Ok(())
}

/// Step 9's daily code, as the tutorial prints it.
const REMINDERS: &str = r#"const tomorrow = Date.now() + 24 * 3600 * 1000;
const ending = await db.role_grants
  .where({ source: "grant_role", revoked_at: null })
  .where({ expires_at: { gte: new Date(tomorrow).toISOString() } })
  .where({ expires_at: { lt: new Date(tomorrow + 24 * 3600 * 1000).toISOString() } })
  .rows();
for (const grant of ending) {
  // Somebody who has subscribed needs no reminder.
  const subscribed = await db.pay_objects
    .where({ kind: "subscription", user_id: grant.user_id, status: { in: ["trialing", "active", "past_due"] } })
    .count();
  if (!subscribed) await db.reminders.insert({ user: grant.user_id, ends: grant.expires_at });
}"#;

/// Steps 5, 6, 8 and 9's triggers.
fn triggers() -> Vec<Trigger> {
    let subscribe = [
        ("account", json!("saas")),
        ("mode", json!("subscription")),
        ("table", json!("plans")),
        ("key_from", json!("payload")),
        ("key_field", json!("plan")),
        ("read_check", json!(true)),
        ("subscription_quantity", json!("1")),
        ("currency", json!("EUR")),
        ("tax", json!("none")),
        ("shipping", json!(false)),
        ("success_path", json!("/welcome")),
        ("cancel_path", json!("/pricing")),
    ]
    .into_iter()
    .fold(
        Trigger::new("subscribe", EventKind::None, "pay_checkout").min_role(80),
        |t, (k, v)| t.config(k, v),
    );
    vec![
        Trigger::new("trial", EventKind::Login, "grant_role")
            .config("user", "user.id")
            .config("role", 70)
            .config("duration_days", 14)
            .config("key", "\"trial\"")
            .config("once", true)
            .config("note", "14-day trial"),
        subscribe,
        Trigger::new("manage_plan", EventKind::None, "pay_portal")
            .min_role(80)
            .config("account", "saas")
            .config("return_path", "/account"),
        Trigger::new("trial_reminders", EventKind::Daily, "run_js_code")
            .timing(ATTR_HOUR, 9)
            .config("code", REMINDERS),
        Trigger::new("remind_by_email", EventKind::Insert, "send_email")
            .on("reminders")
            .config("to", "{{ userⱵemail }}")
            .config("subject", "Your Brew Notes trial ends tomorrow")
            .config(
                "text",
                "Your blends stay yours. Choose a plan to keep using them: \
                 https://notes.example.com/pricing",
            ),
    ]
}

/// The server, with Brew Notes built on it.
async fn setup() -> sc_error::Result<Booted> {
    let booted = boot("saas").await?;
    let catalog = &booted.catalog;
    for (role, name) in [(60, "Pro"), (70, "Basic"), (80, "User")] {
        sc_auth::save_role(catalog, &sc_auth::Role::new(role, name)).await?;
    }
    // Step 1: the test account, with the plans' prices, and the plans.
    sc_api::rows::create_row(
        catalog,
        &catalog.require("pay_accounts")?,
        &json!({ "name": "saas", "adapter": "fake", "mode": "test",
                 "webhook_secret": "any random text",
                 "settings": { "grants_in_test_mode": true,
                               "prices": { "price_basic": { "amount": 500 },
                                           "price_pro": { "amount": 1500 } } } }),
    )
    .await?;
    create_tables(catalog).await?;
    let plans = catalog.require("plans")?;
    for plan in [
        json!({ "id": 1, "name": "Basic", "price": "5 € a month", "stripe_price": "price_basic", "role": 70 }),
        json!({ "id": 2, "name": "Pro", "price": "15 € a month", "stripe_price": "price_pro", "role": 60 }),
    ] {
        sc_api::rows::create_row(catalog, &plans, &plan).await?;
    }

    // Step 2: the plan binding, in the module's settings.
    let configured = send(
        &booted.router,
        "PUT",
        None,
        &format!("/api/modules/{}", booted.module_id),
        &booted.admin,
        &[("content-type", "application/json")],
        serde_json::to_vec(&json!({ "configuration": {
            "plans_table": "plans", "plan_price_field": "stripe_price",
            "plan_role_field": "role", "grace_days": 3,
        } }))
        .unwrap(),
    )
    .await;
    assert_eq!(configured.status, StatusCode::OK, "{}", configured.text);
    assert_eq!(
        configured.json()["reads"],
        json!(["plans"]),
        "{}",
        configured.text
    );

    // Steps 4–9.
    for trigger in triggers() {
        save_trigger(catalog, &booted.dispatcher.registry(), &trigger).await?;
    }
    booted.dispatcher.reload(catalog).await?;
    let issues: Vec<String> = booted
        .dispatcher
        .triggers()?
        .issues()
        .iter()
        .map(|i| format!("{}: {}", i.trigger, i.problem))
        .collect();
    assert!(issues.is_empty(), "{issues:?}");
    let api = ApiConfig::new("rest", "/api")
        .with(sc_api::REST_CFG_ALLOW_SIGNUP, true)
        .with(sc_api::REST_CFG_NEW_USER_ROLE, 80);
    let mut app = Application::new("Brew Notes", "notes", FrameworkRef::new("code"))
        .with_api(api)
        .with_trigger(TriggerRef::new("subscribe"))
        .with_trigger(TriggerRef::new("manage_plan"));
    for table in [
        "notes",
        "blends",
        "flights",
        "plans",
        "pay_objects",
        "role_grants",
    ] {
        app = app.with_table(TableId(table.to_owned()));
    }
    booted.apps.mount(MountedApp::new_with(
        app,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html><h1>Brew Notes</h1>"),
        )),
        catalog,
        booted.apps.evaluator(),
        booted.apps.triggers(),
    )?)?;
    Ok(booted)
}

/// Press a button on one of the fake provider's pages.
async fn press(booted: &Booted, path: &str, form: &str) -> StatusCode {
    send(
        &booted.router,
        "POST",
        Some(NOTES),
        path,
        &HashMap::new(),
        &[("content-type", "application/x-www-form-urlencoded")],
        form.as_bytes().to_vec(),
    )
    .await
    .status
}

/// The path of a URL on Brew Notes.
fn path_of(url: &str) -> String {
    url.strip_prefix(&format!("http://{NOTES}"))
        .unwrap_or_else(|| panic!("not on Brew Notes' host: {url}"))
        .to_owned()
}

/// Whether the browser's user may write a row of `table`: the paywall.
async fn may_write(browser: &mut Browser<'_>, table: &str) -> bool {
    let answer = browser
        .send(
            "POST",
            &format!("/api/{table}"),
            Some(json!({ "text": "Sencha, second steep" })),
        )
        .await;
    answer.status.is_success()
}

#[tokio::test]
async fn brew_notes_trials_subscribes_reminds_and_lapses_as_the_tutorial_says()
-> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let notes = setup().await?;
    let catalog = &notes.catalog;

    // Ann signs up: a fortnight of Basic, with no card.
    let mut ann = Browser::on(&notes.router, NOTES);
    ann.send("GET", "/", None).await;
    let signed_up = ann
        .send(
            "POST",
            "/api/signup",
            Some(json!({ "email": "ann@example.com", "password": "ann-password" })),
        )
        .await;
    assert_eq!(signed_up.status, StatusCode::CREATED, "{}", signed_up.text);
    let ann_id = signed_up.json()["id"].as_str().unwrap().to_owned();
    let trial_sql = "SELECT role, source, source_ref, expires_at > now() + interval '13 days' AS fortnight \
                     FROM role_grants WHERE source = 'grant_role'";
    let trials = notes.rows(trial_sql).await;
    assert_eq!(trials.len(), 1, "{trials:?}");
    assert_eq!(trials[0]["role"], json!(70));
    assert_eq!(trials[0]["source_ref"], json!(format!("{ann_id}:trial")));
    assert_eq!(trials[0]["fortnight"], json!(true));
    // The paywall: notes for everybody, blends on Basic, flights on Pro.
    assert!(may_write(&mut ann, "notes").await);
    assert!(may_write(&mut ann, "blends").await);
    assert!(!may_write(&mut ann, "flights").await);

    // Signing in again, elsewhere, is no second trial.
    let mut again = Browser::on(&notes.router, NOTES);
    again.send("GET", "/", None).await;
    again
        .ok(
            "POST",
            "/api/login",
            Some(json!({ "email": "ann@example.com", "password": "ann-password" })),
        )
        .await;
    assert_eq!(notes.rows(trial_sql).await.len(), 1);

    // The trial ends tomorrow: the daily reminder writes one reminder, and one email.
    notes
        .execute("UPDATE role_grants SET expires_at = now() + interval '30 hours' WHERE source = 'grant_role'")
        .await;
    notes
        .dispatcher
        .run_trigger(catalog, "trial_reminders", json!({}), None)
        .await?;
    assert_eq!(notes.rows("SELECT id FROM reminders").await.len(), 1);
    let sent = notes.mail.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].to[0].address, "ann@example.com");
    assert_eq!(sent[0].subject, "Your Brew Notes trial ends tomorrow");

    // Ann subscribes to Pro, and pays on the test provider's page.
    let started = ann
        .send("POST", "/api/actions/subscribe", Some(json!({ "plan": 2 })))
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.text);
    assert_eq!(
        started.json()["amount"],
        json!(1500),
        "the account's settings price it"
    );
    let page = path_of(started.json()["url"].as_str().unwrap());
    let paid = send(
        &notes.router,
        "POST",
        Some(NOTES),
        &format!("{page}/pay"),
        &HashMap::new(),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(paid.status, StatusCode::SEE_OTHER, "{}", paid.text);
    assert_eq!(
        paid.headers[header::LOCATION],
        format!("http://{NOTES}/welcome").as_str()
    );
    assert!(may_write(&mut ann, "flights").await, "Pro now");

    // The account page: her subscription's renewal date, and her trial, from the API.
    let objects = ann.ok("GET", "/api/pay_objects", None).await;
    let sub = objects
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["kind"] == json!("subscription"))
        .cloned()
        .unwrap();
    assert_eq!(sub["price_ref"], json!("price_pro"));
    assert_eq!(sub["cancel_at_period_end"], json!(false));
    assert!(sub["current_period_end"].is_string(), "{sub}");
    let grants = ann.ok("GET", "/api/role_grants", None).await;
    assert!(
        grants
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["source"] == json!("grant_role")),
        "{grants}"
    );

    // Subscribed: the next day's reminder has nobody to remind.
    notes.execute("DELETE FROM reminders").await;
    notes
        .dispatcher
        .run_trigger(catalog, "trial_reminders", json!({}), None)
        .await?;
    assert!(notes.rows("SELECT id FROM reminders").await.is_empty());
    assert_eq!(notes.mail.sent().len(), 1);

    // The portal: down to Basic. Flights go; blends stay.
    let portal = ann
        .send("POST", "/api/actions/manage_plan", Some(json!({})))
        .await;
    assert_eq!(portal.status, StatusCode::OK, "{}", portal.text);
    let portal = path_of(portal.json()["url"].as_str().unwrap());
    let sub_ref = sub["provider_ref"].as_str().unwrap();
    assert_eq!(
        press(
            &notes,
            &format!("{portal}/{sub_ref}/change"),
            "price=price_basic"
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert!(!may_write(&mut ann, "flights").await);
    assert!(may_write(&mut ann, "blends").await);
    // The trial runs out: still Basic, from the subscription.
    notes
        .execute("UPDATE role_grants SET expires_at = now() - interval '1 hour', starts_at = now() - interval '15 days' WHERE source = 'grant_role'")
        .await;
    assert!(may_write(&mut ann, "blends").await);

    // She cancels at the end of the period; the period ends. Free tier: notes only.
    assert_eq!(
        press(&notes, &format!("{portal}/{sub_ref}/cancel"), "").await,
        StatusCode::SEE_OTHER
    );
    assert!(
        may_write(&mut ann, "blends").await,
        "kept to the period's end"
    );
    assert_eq!(
        press(&notes, &format!("{portal}/{sub_ref}/renew"), "").await,
        StatusCode::SEE_OTHER
    );
    assert!(!may_write(&mut ann, "blends").await);
    assert!(may_write(&mut ann, "notes").await);
    let refused = ann
        .send("POST", "/api/actions/manage_plan", Some(json!({})))
        .await;
    assert!(
        refused.status.is_success(),
        "a past customer still has a portal: {}",
        refused.text
    );

    // A guest has no plan to buy.
    let mut guest = Browser::on(&notes.router, NOTES);
    guest.send("GET", "/", None).await;
    let anonymous = guest
        .send("POST", "/api/actions/subscribe", Some(json!({ "plan": 1 })))
        .await;
    assert!(!anonymous.status.is_success(), "{}", error_of(&anonymous));
    Ok(())
}
