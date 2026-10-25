//! Tea Sommelier, the LLM-credits application of
//! [docs/tutorial-payments-credits.md](../../../docs/tutorial-payments-credits.md)
//! (TODO-payments P7.3), built the way the tutorial builds it, so its plan, packs and triggers do
//! what it says:
//!
//! - subscribing to the Assistant plan grants its 100 credits for the month;
//! - with auto top-up on, an agent run that takes the balance below 20 buys a pack on the saved
//!   card by itself, once;
//! - the one-click top-up charges the saved card, and a renewal carries at most 50 unspent
//!   credits over;
//! - a card that asks to authenticate, and a user with no card, get a checkout instead.
//!
//! The agent's model calls are stood in for by the ledger's debit, made as the agent loop makes
//! it (`sc_auth::debit_credits` with the run's user as the caller); metered runs themselves are
//! tested with the agents (crates/sc-agent/tests/credits.rs).
//!
//! Needs `npm`, which installs the module, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use rust_decimal::Decimal;
use sc_action::{EventKind, Trigger, save_trigger};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, TriggerRef};
use sc_catalog::{CallerContext, Catalog, TableId};
use sc_server::MountedApp;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::payments_module::send;
use crate::tutorial_payments::{Booted, Browser, boot};

const TEA: &str = "tea.example.com";

fn field(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    }
}

fn required(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        required: true,
        ..field(name, type_name)
    }
}

fn id() -> FieldSpec {
    FieldSpec {
        primary_key: true,
        ..field("id", "int")
    }
}

/// Step 1's plans and Step 4's packs, both public to read and admin to write.
async fn create_tables(catalog: &Catalog) -> sc_error::Result<()> {
    let tables = [
        (
            "plans",
            vec![
                id(),
                required("name", "text"),
                field("price", "text"),
                field("stripe_price", "text"),
                required("role", "int"),
                field("credits", "int"),
                field("rollover", "text"),
                field("rollover_cap", "int"),
            ],
        ),
        (
            "credit_packs",
            vec![
                id(),
                required("name", "text"),
                required("price", "int"),
                required("units", "int"),
            ],
        ),
    ];
    for (name, fields) in tables {
        schema_edit::apply(
            catalog,
            &[Operation::CreateTable {
                name: name.into(),
                database: String::new(),
                settings: TableSettings {
                    min_role_read: Some(100),
                    min_role_write: Some(1),
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

/// A trigger with the settings of a table's rows of `(name, value)`.
fn configured(trigger: Trigger, settings: &[(&str, Value)]) -> Trigger {
    settings
        .iter()
        .fold(trigger, |t, (k, v)| t.config(*k, v.clone()))
}

/// Step 4's settings, which Step 5's and Step 6's charges share.
fn pack_settings() -> Vec<(&'static str, Value)> {
    vec![
        ("account", json!("tea")),
        ("table", json!("credit_packs")),
        ("read_check", json!(true)),
        ("lines", json!("one")),
        ("amount", json!("price")),
        ("description", json!("name")),
        ("currency", json!("EUR")),
        ("tax", json!("none")),
        ("grant_credits", json!("units")),
        ("credits_expire_days", json!(365)),
        ("success_path", json!("/credits")),
        ("cancel_path", json!("/credits")),
    ]
}

/// Part 2's `subscribe` and `manage_plan`, and Steps 4–6's triggers.
fn triggers() -> Vec<Trigger> {
    let subscribe = configured(
        Trigger::new("subscribe", EventKind::None, "pay_checkout").min_role(80),
        &[
            ("account", json!("tea")),
            ("mode", json!("subscription")),
            ("table", json!("plans")),
            ("key_from", json!("payload")),
            ("key_field", json!("plan")),
            ("read_check", json!(true)),
            ("subscription_quantity", json!("1")),
            ("currency", json!("EUR")),
            ("tax", json!("none")),
            ("success_path", json!("/welcome")),
            ("cancel_path", json!("/pricing")),
        ],
    );
    let by_payload = [("key_from", json!("payload")), ("key_field", json!("pack"))];
    let buy_credits = configured(
        configured(
            Trigger::new("buy_credits", EventKind::None, "pay_checkout").min_role(80),
            &pack_settings(),
        ),
        &[
            by_payload[0].clone(),
            by_payload[1].clone(),
            ("mode", json!("payment")),
            ("save_card", json!(true)),
        ],
    );
    let top_up = configured(
        configured(
            Trigger::new("top_up", EventKind::None, "pay_charge_saved").min_role(80),
            &pack_settings(),
        ),
        &by_payload,
    );
    let auto_top_up = configured(
        Trigger::new("auto_top_up", EventKind::Insert, "pay_charge_saved")
            .on("credit_entries")
            .only_if("row.kind === \"low_balance\""),
        &pack_settings(),
    )
    .config("key_from", "fixed")
    .config("key_value", "1");
    vec![
        subscribe,
        Trigger::new("manage_plan", EventKind::None, "pay_portal")
            .min_role(80)
            .config("account", "tea")
            .config("return_path", "/credits"),
        buy_credits,
        top_up,
        Trigger::new("auto_top_up_on", EventKind::None, "update_rows")
            .min_role(80)
            .config("table", "credit_accounts")
            .config("where", "user_id === user.id")
            .config("assignments", json!({ "low_balance_threshold": "\"20\"" })),
        auto_top_up,
    ]
}

/// The server, with Tea Sommelier built on it.
async fn setup() -> sc_error::Result<Booted> {
    let booted = boot("credits").await?;
    let catalog = &booted.catalog;
    for (role, name) in [(70, "Assistant"), (80, "User")] {
        sc_auth::save_role(catalog, &sc_auth::Role::new(role, name)).await?;
    }
    // Step 1: the test account, the plan with its credits; Step 4: the pack.
    sc_api::rows::create_row(
        catalog,
        &catalog.require("pay_accounts")?,
        &json!({ "name": "tea", "adapter": "fake", "mode": "test",
                 "webhook_secret": "any random text",
                 "settings": { "grants_in_test_mode": true,
                               "prices": { "price_assistant": { "amount": 500 } } } }),
    )
    .await?;
    create_tables(catalog).await?;
    sc_api::rows::create_row(
        catalog,
        &catalog.require("plans")?,
        &json!({ "id": 1, "name": "Assistant", "price": "5 € a month", "stripe_price": "price_assistant",
                 "role": 70, "credits": 100, "rollover": "capped", "rollover_cap": 50 }),
    )
    .await?;
    sc_api::rows::create_row(
        catalog,
        &catalog.require("credit_packs")?,
        &json!({ "id": 1, "name": "100 credits", "price": 200, "units": 100 }),
    )
    .await?;

    // Step 2: the plan binding, with the credits' fields.
    let configured = send(
        &booted.router,
        "PUT",
        None,
        &format!("/api/modules/{}", booted.module_id),
        &booted.admin,
        &[("content-type", "application/json")],
        serde_json::to_vec(&json!({ "configuration": {
            "plans_table": "plans", "plan_price_field": "stripe_price", "plan_role_field": "role",
            "plan_allowance_field": "credits", "plan_rollover_field": "rollover",
            "plan_rollover_cap_field": "rollover_cap", "grace_days": 3,
        } }))
        .unwrap(),
    )
    .await;
    assert_eq!(configured.status, StatusCode::OK, "{}", configured.text);

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
    let mut app = Application::new("Tea Sommelier", "tea", FrameworkRef::new("code")).with_api(api);
    for trigger in [
        "subscribe",
        "manage_plan",
        "buy_credits",
        "top_up",
        "auto_top_up_on",
    ] {
        app = app.with_trigger(TriggerRef::new(trigger));
    }
    for table in [
        "plans",
        "credit_packs",
        "credit_buckets",
        "credit_entries",
        "pay_objects",
    ] {
        app = app.with_table(TableId(table.to_owned()));
    }
    booted.apps.mount(MountedApp::new_with(
        app,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html><h1>Tea Sommelier</h1>"),
        )),
        catalog,
        booted.apps.evaluator(),
        booted.apps.triggers(),
    )?)?;
    Ok(booted)
}

/// The path of a URL on Tea Sommelier.
fn path_of(url: &str) -> String {
    url.strip_prefix(&format!("http://{TEA}"))
        .unwrap_or_else(|| panic!("not on Tea Sommelier's host: {url}"))
        .to_owned()
}

/// Press a button on one of the fake provider's pages.
async fn press(booted: &Booted, path: &str) -> StatusCode {
    send(
        &booted.router,
        "POST",
        Some(TEA),
        path,
        &HashMap::new(),
        &[],
        Vec::new(),
    )
    .await
    .status
}

/// A user signed up on Tea Sommelier, and their id.
async fn sign_up<'a>(booted: &'a Booted, email: &str) -> (Browser<'a>, Uuid) {
    let mut browser = Browser::on(&booted.router, TEA);
    browser.send("GET", "/", None).await;
    let signed_up = browser
        .send(
            "POST",
            "/api/signup",
            Some(json!({ "email": email, "password": "a-password" })),
        )
        .await;
    assert_eq!(signed_up.status, StatusCode::CREATED, "{}", signed_up.text);
    let id = signed_up.json()["id"].as_str().unwrap().parse().unwrap();
    (browser, id)
}

async fn balance(catalog: &Catalog, user: Uuid) -> Decimal {
    sc_auth::credit_balance(catalog, user)
        .await
        .unwrap()
        .balance
}

#[tokio::test]
async fn tea_sommelier_grants_tops_up_and_rolls_over_as_the_tutorial_says() -> sc_error::Result<()>
{
    if !crate::payments_module::have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let tea = setup().await?;
    let catalog = &tea.catalog;
    let (mut ann, ann_id) = sign_up(&tea, "ann@example.com").await;

    // Ann subscribes to the Assistant plan and pays: its 100 credits, for the month.
    let started = ann
        .send("POST", "/api/actions/subscribe", Some(json!({ "plan": 1 })))
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.text);
    let page = path_of(started.json()["url"].as_str().unwrap());
    assert_eq!(
        press(&tea, &format!("{page}/pay")).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(100));
    let buckets = ann.ok("GET", "/api/credit_buckets", None).await;
    let buckets = buckets.as_array().unwrap();
    assert_eq!(buckets.len(), 1, "{buckets:?}");
    assert_eq!(buckets[0]["kind"], json!("allowance"));
    assert!(
        buckets[0]["expires_at"].is_string(),
        "it ends with the month"
    );

    // She turns auto top-up on: a threshold of 20.
    ann.ok("POST", "/api/actions/auto_top_up_on", Some(json!({})))
        .await;
    let account = sc_auth::credit_balance(catalog, ann_id).await?;
    assert_eq!(account.low_balance_threshold, Some(Decimal::from(20)));

    // The agent's model calls take her to 15, below 20: a pack is bought on her saved card, once.
    let caller = CallerContext::new(
        70,
        Some(json!({ "id": ann_id.to_string(), "email": "ann@example.com", "role": 70 })),
    );
    let debited = sc_auth::debit_credits(
        catalog,
        sc_auth::CreditDebit {
            user_id: ann_id,
            amount: Decimal::from(85),
            key: "run:1:1".into(),
            agent: Some("sommelier".into()),
            ..sc_auth::CreditDebit::default()
        },
        Some(&caller),
    )
    .await?;
    assert!(debited.low_balance);
    assert_eq!(
        balance(catalog, ann_id).await,
        Decimal::from(115),
        "15 left, and a pack of 100"
    );
    let packs = tea
        .rows("SELECT ref_key, amount, status FROM pay_events WHERE kind = 'payment.succeeded' AND ref_table = 'credit_packs'")
        .await;
    assert_eq!(
        packs,
        vec![json!({ "ref_key": 1, "amount": 200, "status": "succeeded" })]
    );
    // Further calls below the threshold buy nothing more.
    sc_auth::debit_credits(
        catalog,
        sc_auth::CreditDebit {
            user_id: ann_id,
            amount: Decimal::from(1),
            key: "run:1:2".into(),
            ..sc_auth::CreditDebit::default()
        },
        Some(&caller),
    )
    .await?;
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(114));

    // One click: another pack on the same card, with no page.
    let topped = ann
        .ok("POST", "/api/actions/top_up", Some(json!({ "pack": 1 })))
        .await;
    assert_eq!(topped["status"], json!("paid"), "{topped}");
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(214));

    // A month goes by: 100 more, and the 14 left of the first month's carried over.
    let portal = ann
        .ok("POST", "/api/actions/manage_plan", Some(json!({})))
        .await;
    let portal = path_of(portal["url"].as_str().unwrap());
    let sub = tea
        .rows("SELECT provider_ref FROM pay_objects WHERE kind = 'subscription'")
        .await;
    let sub_ref = sub[0]["provider_ref"].as_str().unwrap().to_owned();
    assert_eq!(
        press(&tea, &format!("{portal}/{sub_ref}/renew")).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(314));
    let rollover = tea
        .rows("SELECT amount::text, note FROM credit_entries WHERE kind = 'rollover'")
        .await;
    assert_eq!(rollover.len(), 1, "{rollover:?}");
    assert_eq!(
        rollover[0]["note"]
            .as_str()
            .unwrap()
            .split(" carried")
            .next(),
        Some("14")
    );

    // Her bank asks her to confirm: a checkout instead, and the card it is paid with is saved.
    assert_eq!(
        press(&tea, &format!("{portal}/card/authenticate")).await,
        StatusCode::SEE_OTHER
    );
    let asked = ann
        .ok("POST", "/api/actions/top_up", Some(json!({ "pack": 1 })))
        .await;
    assert_eq!(asked["status"], json!("requires_checkout"), "{asked}");
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(314));
    let page = path_of(asked["url"].as_str().unwrap());
    assert_eq!(
        press(&tea, &format!("{page}/pay")).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(414));
    let again = ann
        .ok("POST", "/api/actions/top_up", Some(json!({ "pack": 1 })))
        .await;
    assert_eq!(again["status"], json!("paid"), "{again}");
    assert_eq!(balance(catalog, ann_id).await, Decimal::from(514));

    // Bob has no saved card: his top-up is a checkout, which buys his first pack.
    let (mut bob, bob_id) = sign_up(&tea, "bob@example.com").await;
    let first = bob
        .ok("POST", "/api/actions/top_up", Some(json!({ "pack": 1 })))
        .await;
    assert_eq!(first["status"], json!("requires_checkout"), "{first}");
    assert_eq!(
        first["reason"],
        json!("you have no saved card on \"tea\" yet")
    );
    let page = path_of(first["url"].as_str().unwrap());
    assert_eq!(
        press(&tea, &format!("{page}/pay")).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(balance(catalog, bob_id).await, Decimal::from(100));
    // Each reads only their own credits.
    let bobs = bob.ok("GET", "/api/credit_buckets", None).await;
    assert_eq!(bobs.as_array().unwrap().len(), 1, "{bobs}");
    assert_eq!(bobs[0]["kind"], json!("top_up"));
    Ok(())
}
