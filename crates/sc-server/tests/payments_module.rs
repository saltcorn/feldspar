//! The bundled **payments** module, installed on a running server (TODO-payments P2.1, P2.2).
//!
//! - P2.1: installing it creates its three tables, with the account keys kept secret, and lists
//!   its two webhooks; an account that cannot be used is refused by name — an adapter the module
//!   does not have, the fake provider in live mode, a missing account, no signing secret.
//! - P2.2: the `fake` provider's checkout page, served on the shop's own host, takes a payment
//!   and delivers signed webhooks to the module's `events` handler, which verifies them; one
//!   signed with anything but the account's secret is refused.
//!
//! Needs `npm`, which installs the module, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_action::{EventKind, Trigger, save_trigger};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation as SchemaOp, TableSettings};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, TriggerRef};
use sc_auth::{SessionStore, create_user};
use sc_catalog::{Catalog, DataFieldKind, FieldId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const SHOP: &str = "shop.example.com";
const SECRET: &str = "whsec_test_secret";

pub(crate) fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) text: String,
}

impl Answer {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
}

/// One request, as a browser on `host` (or the admin, with no host) sends it.
pub(crate) async fn send(
    router: &Router,
    method: &str,
    host: Option<&str>,
    path: &str,
    cookies: &HashMap<String, String>,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Answer {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    if !cookies.is_empty() {
        let jar = cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        builder = builder.header(header::COOKIE, jar);
        if method != "GET"
            && let Some(csrf) = cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    Answer {
        status,
        headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Keep the cookies an answer sets.
pub(crate) fn keep(cookies: &mut HashMap<String, String>, answer: &Answer) {
    for raw in answer.headers.get_all(header::SET_COOKIE) {
        if let Some((name, value)) = raw
            .to_str()
            .ok()
            .and_then(|t| t.split(';').next())
            .and_then(|p| p.split_once('='))
        {
            cookies.insert(name.to_owned(), value.to_owned());
        }
    }
}

/// HMAC-SHA256, by hand: the provider's signature, as a test of the receiving side computes it.
fn hmac_sha256(key: &[u8], message: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    let outer = Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize();
    outer.iter().map(|b| format!("{b:02x}")).collect()
}

/// `t=…,v1=…` over `body` with `secret`, at the current time.
fn signature(secret: &str, body: &[u8]) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut message = format!("{t}.").into_bytes();
    message.extend_from_slice(body);
    format!("t={t},v1={}", hmac_sha256(secret.as_bytes(), &message))
}

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    root: PathBuf,
    _modules: Arc<ModuleServices>,
    _db: TestDb,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Server {
    /// One query's rows, as JSON objects.
    async fn rows(&self, sql: &str) -> Vec<Value> {
        let wrapped = format!("SELECT coalesce(json_agg(q), '[]'::json) FROM ({sql}) q");
        let value: Value = self
            ._db
            .client()
            .await
            .unwrap()
            .query_one(&wrapped, &[])
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .get(0);
        value.as_array().unwrap().clone()
    }
}

fn field(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    }
}

fn key(name: &str, target: &str) -> FieldSpec {
    FieldSpec {
        kind: DataFieldKind::Key {
            target_table: TableId(target.to_owned()),
            target_field: FieldId(String::new()),
            summary_field: None,
        },
        ..field(name, "")
    }
}

fn primary(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        primary_key: true,
        ..field(name, type_name)
    }
}

/// One application table, with the floors and the ownership rule given.
async fn create_table(
    catalog: &Catalog,
    name: &str,
    read: u8,
    ownership: Option<&str>,
    fields: Vec<FieldSpec>,
) -> sc_error::Result<()> {
    schema_edit::apply(
        catalog,
        &[SchemaOp::CreateTable {
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
    Ok(())
}

/// The shop's own tables (TODO-payments §6.1): what it sells, its orders and their lines, which
/// have **no price column** — a line's price is its product's — plus invoices keyed by text,
/// bookings keyed by a UUID, and a log its trigger writes once per payment.
async fn create_shop_tables(catalog: &Catalog) -> sc_error::Result<()> {
    create_table(
        catalog,
        "products",
        100,
        None,
        vec![
            primary("id", "int"),
            field("name", "text"),
            field("price", "int"),
        ],
    )
    .await?;
    create_table(
        catalog,
        "orders",
        1,
        Some("user ? owner === user.id : session_id === session.id"),
        vec![
            primary("id", "int"),
            field("owner", "uuid"),
            field("session_id", "text"),
            field("status", "text"),
            field("email", "text"),
        ],
    )
    .await?;
    create_table(
        catalog,
        "order_lines",
        1,
        None,
        vec![
            primary("id", "int"),
            key("order", "orders"),
            key("product", "products"),
            field("quantity", "int"),
        ],
    )
    .await?;
    create_table(
        catalog,
        "invoices",
        1,
        None,
        vec![
            primary("number", "text"),
            field("amount", "int"),
            field("status", "text"),
        ],
    )
    .await?;
    create_table(
        catalog,
        "bookings",
        1,
        None,
        vec![
            primary("id", "uuid"),
            field("deposit", "int"),
            field("status", "text"),
        ],
    )
    .await?;
    create_table(
        catalog,
        "payment_log",
        1,
        None,
        vec![primary("id", "int"), field("note", "text")],
    )
    .await?;
    Ok(())
}

/// A `pay_checkout` trigger an application exposes.
fn checkout(name: &str, settings: Value) -> Trigger {
    let mut trigger = Trigger::new(name, EventKind::None, "pay_checkout").min_role(100);
    let defaults = json!({
        "account": "shop", "mode": "payment", "key_from": "payload", "read_check": true,
        "lines": "one", "tax": "none", "shipping": false,
    });
    for (k, v) in defaults
        .as_object()
        .unwrap()
        .iter()
        .chain(settings.as_object().unwrap())
    {
        trigger = trigger.config(k, v.clone());
    }
    trigger
}

/// The application's trigger marking its own row paid when the payment for it succeeds:
/// `update_rows` where the key is the event's `ref_key`, whatever the key's type (§3.4).
fn mark_paid(table: &str, key: &str) -> Trigger {
    Trigger::new(format!("{table}_paid"), EventKind::Insert, "update_rows")
        .on("pay_events")
        .only_if(format!(
            "row.kind === \"payment.succeeded\" && row.ref_table === \"{table}\""
        ))
        .config("table", table)
        .config("where", format!("{key} === row.ref_key"))
        .config("assignments", json!({ "status": "\"paid\"" }))
}

async fn setup(tag: &str) -> sc_error::Result<(Server, Value)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    // Where a declared field's default lives, as on a real server.
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let evaluator = default_js_evaluator();
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!("sc-payments-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.clone()),
        // The checkout's own `plugins/`: the bundled catalog as it ships.
        None,
        1,
        sc_server::default_python_adapter(),
        None,
    )
    .await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone())
            .with_agents(agents)
            .with_modules(modules.clone()),
    );
    let config = ServerConfig {
        base_domain: Some("example.com".to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;

    // The admin installs the bundled module from the Modules tab.
    let mut admin = HashMap::new();
    let primed = send(
        &router,
        "GET",
        None,
        "/api/auth/status",
        &admin,
        &[],
        Vec::new(),
    )
    .await;
    keep(&mut admin, &primed);
    let first = send(
        &router,
        "POST",
        None,
        "/api/first-user",
        &admin,
        &[("content-type", "application/json")],
        serde_json::to_vec(&json!({ "email": "admin@example.com", "password": "hunter2pass" }))
            .unwrap(),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    keep(&mut admin, &first);
    let installed = send(
        &router,
        "POST",
        None,
        "/api/modules",
        &admin,
        &[("content-type", "application/json")],
        serde_json::to_vec(&json!({ "source": "bundled", "location": "payments" })).unwrap(),
    )
    .await;
    assert_eq!(installed.status, StatusCode::CREATED, "{}", installed.text);
    let installed = installed.json();

    // Its accounts, as an admin enters them.
    let accounts = catalog.require("pay_accounts")?;
    for row in [
        json!({ "name": "shop", "adapter": "fake", "mode": "test", "webhook_secret": SECRET }),
        json!({ "name": "unsigned", "adapter": "fake", "mode": "test" }),
        json!({ "name": "acme", "adapter": "acme", "mode": "test", "webhook_secret": SECRET }),
        json!({ "name": "real", "adapter": "fake", "mode": "live", "webhook_secret": SECRET }),
        json!({ "name": "off", "adapter": "fake", "mode": "test", "webhook_secret": SECRET,
                "enabled": false }),
        // A provider whose webhooks a test delivers by hand, in its own order.
        json!({ "name": "held", "adapter": "fake", "mode": "test", "webhook_secret": SECRET,
                "settings": { "webhooks": "hold" } }),
    ] {
        sc_api::rows::create_row(&catalog, &accounts, &row).await?;
    }

    // The shop: its tables, its checkouts, and its triggers on what the payments do.
    create_shop_tables(&catalog).await?;
    sc_auth::save_role(&catalog, &sc_auth::Role::new(80, "User")).await?;
    create_user(&catalog, "ada@example.com", "ada-password", 80).await?;
    let triggers = [
        Trigger::new("try_account", EventKind::None, "pay_account_test").min_role(100),
        checkout(
            "checkout_order",
            json!({
                "table": "orders", "key_field": "order", "lines": "children",
                "child_table": "order_lines", "child_key": "order", "quantity": "quantity",
                "unit_amount": "productⱵprice", "line_description": "productⱵname",
                "item": "product", "currency": "EUR", "email": "email",
                "success_path": "/?paid={key}", "cancel_path": "/basket",
            }),
        ),
        // Pay links a customer opens with no account: nothing for them to read.
        checkout(
            "checkout_invoice",
            json!({
                "table": "invoices", "key_field": "number", "read_check": false,
                "amount": "amount", "description": "\"Invoice \" + number", "currency": "GBP",
            }),
        ),
        checkout(
            "checkout_booking",
            json!({
                "table": "bookings", "key_field": "booking", "read_check": false,
                "amount": "deposit", "currency": "JPY",
            }),
        ),
        Trigger::new("refund_order", EventKind::None, "pay_refund")
            .min_role(100)
            .config("source", "row")
            .config("table", "orders")
            .config("key_from", "payload")
            .config("key_field", "order")
            .config("read_check", true)
            .config("reason", "requested_by_customer"),
        Trigger::new("sync_payments", EventKind::None, "pay_sync").min_role(100),
        Trigger::new("provider_code", EventKind::None, "pay_provider_code")
            .min_role(100)
            .config("account", "shop")
            .config(
                "code",
                "const c = await client.checkouts.retrieve(payload.id); \
                 return { status: c.status, account };",
            ),
        Trigger::new("call_api", EventKind::None, "run_js_code")
            .min_role(100)
            .config(
                "code",
                "return await modfn.pay_api(\"shop\", \"GET\", \"/checkouts/\" + payload.id);",
            ),
        mark_paid("orders", "id"),
        mark_paid("invoices", "number"),
        mark_paid("bookings", "id"),
        Trigger::new("log_payment", EventKind::Insert, "insert_row")
            .on("pay_events")
            .only_if("row.kind === \"payment.succeeded\"")
            .config("table", "payment_log")
            .config("values", json!({ "note": "row.event_id" })),
    ];
    for trigger in &triggers {
        save_trigger(&catalog, &dispatcher.registry(), trigger).await?;
    }
    dispatcher.reload(&catalog).await?;
    let mut app = Application::new("Shop", "shop", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_table(TableId("orders".to_owned()))
        .attribute(sc_app::GUEST_SESSIONS_ATTRIBUTE, true);
    for name in [
        "try_account",
        "checkout_order",
        "checkout_invoice",
        "checkout_booking",
        "refund_order",
        "sync_payments",
        "provider_code",
        "call_api",
    ] {
        app = app.with_trigger(TriggerRef::new(name));
    }
    apps.mount(MountedApp::new_with(
        app,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html><h1>Shop</h1>"),
        )),
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;

    Ok((
        Server {
            router,
            catalog,
            root,
            _modules: modules,
            _db: db,
            _registries: registries,
        },
        installed,
    ))
}

/// `api.runTryAccount(payload)` on the shop.
async fn try_account(server: &Server, payload: Value) -> Answer {
    send(
        &server.router,
        "POST",
        Some(SHOP),
        "/api/actions/try_account",
        &HashMap::new(),
        &[("content-type", "application/json")],
        serde_json::to_vec(&payload).unwrap(),
    )
    .await
}

/// The path of a URL on the shop.
fn path_of(url: &str) -> String {
    url.strip_prefix(&format!("http://{SHOP}"))
        .unwrap_or_else(|| panic!("not on the shop's host: {url}"))
        .to_owned()
}

#[tokio::test]
async fn the_module_installs_its_tables_and_refuses_an_account_it_cannot_use()
-> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, installed) = setup("install").await?;
    assert_eq!(installed["loaded"], json!(true), "{installed}");
    assert_eq!(installed["issues"], json!([]), "{installed}");
    assert_eq!(
        installed["tables"],
        json!(["pay_accounts", "pay_objects", "pay_events"])
    );
    let hooks: Vec<&str> = installed["webhooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        hooks,
        [
            "/_feldspar/hooks/payments/events",
            "/_feldspar/hooks/payments/fake"
        ]
    );
    // The keys are secrets: sealed at rest, never the plaintext in the column.
    let accounts = server.catalog.require("pay_accounts")?;
    for key in ["secret_key", "webhook_secret"] {
        assert!(accounts.field(key).unwrap().is_secret(), "{key}");
    }
    let stored: String = server
        ._db
        .client()
        .await?
        .query_one(
            "SELECT webhook_secret FROM pay_accounts WHERE name = 'shop'",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?
        .get(0);
    assert_ne!(stored, SECRET);

    // Each account that cannot be used says why, by name.
    for (account, refusal) in [
        (
            "acme",
            "names the adapter \"acme\", which this module does not have; it has: fake",
        ),
        ("real", "takes no real money, so it cannot be in live mode"),
        ("off", "is disabled"),
        ("nobody", "there is no payment account \"nobody\""),
        ("unsigned", "has no webhook signing secret"),
    ] {
        let answer = try_account(&server, json!({ "account": account })).await;
        assert!(!answer.status.is_success(), "{account}: {}", answer.text);
        let error = answer.json()["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(error.contains(refusal), "{account}: {error}");
    }
    let zero = try_account(&server, json!({ "account": "shop", "amount": 0 })).await;
    assert!(zero.text.contains("the amount is 0"), "{}", zero.text);
    let float = try_account(&server, json!({ "account": "shop", "amount": 19.99 })).await;
    assert!(float.text.contains("whole number"), "{}", float.text);
    Ok(())
}

#[tokio::test]
async fn the_fake_provider_takes_a_payment_and_delivers_signed_webhooks() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, _) = setup("fake").await?;
    let none = HashMap::new();

    // The shop starts a checkout; its page is on the shop's own host.
    let started = try_account(
        &server,
        json!({ "account": "shop", "amount": 1999, "currency": "eur" }),
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.text);
    let started = started.json();
    assert_eq!(started["amount"], json!(1999));
    assert_eq!(started["currency"], json!("EUR"));
    let url = started["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!(
            "http://{SHOP}/_feldspar/hooks/payments/fake/shop/checkout/cs_fake_"
        )),
        "{url}"
    );
    let page = path_of(url);

    let shown = send(
        &server.router,
        "GET",
        Some(SHOP),
        &page,
        &none,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text);
    assert!(shown.text.contains("19.99 EUR"), "{}", shown.text);
    assert!(shown.text.contains(">Pay</button>"), "{}", shown.text);
    let policy = shown.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(policy.starts_with("default-src 'none'"), "{policy}");

    // A declined card: the checkout stays open, and the provider says so.
    let declined = send(
        &server.router,
        "POST",
        Some(SHOP),
        &format!("{page}/fail"),
        &none,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(declined.status, StatusCode::SEE_OTHER, "{}", declined.text);
    let view = send(
        &server.router,
        "GET",
        Some(SHOP),
        &format!("{page}?format=json"),
        &none,
        &[],
        Vec::new(),
    )
    .await
    .json();
    assert_eq!(view["checkout"]["status"], json!("open"));
    assert_eq!(view["checkout"]["declined"], json!(1));

    // Paying: the buyer is sent on, and each event reached the module's handler,
    // signed, and was accepted.
    let paid = send(
        &server.router,
        "POST",
        Some(SHOP),
        &format!("{page}/pay"),
        &none,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(paid.status, StatusCode::SEE_OTHER, "{}", paid.text);
    assert_eq!(paid.headers[header::LOCATION].to_str().unwrap(), url);
    let view = send(
        &server.router,
        "GET",
        Some(SHOP),
        &format!("{page}?format=json"),
        &none,
        &[],
        Vec::new(),
    )
    .await
    .json();
    assert_eq!(view["checkout"]["status"], json!("complete"), "{view}");
    assert_eq!(view["payment"]["status"], json!("succeeded"), "{view}");
    assert_eq!(view["payment"]["amount"], json!(1999));
    let deliveries: Vec<(String, i64)> = view["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["type"].as_str().unwrap().to_owned(),
                d["status"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        deliveries,
        [
            ("payment.failed".to_owned(), 200),
            ("checkout.completed".to_owned(), 200),
            ("payment.succeeded".to_owned(), 200),
        ]
    );
    // The page now shows what happened, and offers no buttons.
    let after = send(
        &server.router,
        "GET",
        Some(SHOP),
        &page,
        &none,
        &[],
        Vec::new(),
    )
    .await;
    assert!(after.text.contains("complete") && !after.text.contains(">Pay</button>"));

    // The same handler, over HTTP at the URL a provider is given: an event
    // signed with the account's secret is accepted, anything else refused.
    let payment = view["payment"]["id"].as_str().unwrap().to_owned();
    let event = json!({
        "id": "evt_by_hand", "type": "payment.succeeded", "account": "shop",
        "data": { "object": { "kind": "payment", "id": payment } },
    });
    let body = serde_json::to_vec(&event).unwrap();
    let hook = "/_feldspar/hooks/payments/events/shop";
    let good = signature(SECRET, &body);
    let accepted = send(
        &server.router,
        "POST",
        None,
        hook,
        &none,
        &[("fake-signature", &good)],
        body.clone(),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
    assert_eq!(accepted.json()["received"][0]["id"], json!("evt_by_hand"));
    assert_eq!(
        accepted.json()["received"][0]["logged"],
        json!(["payment.succeeded"])
    );
    let forged = signature("not the secret", &body);
    for headers in [vec![("fake-signature", forged.as_str())], vec![]] {
        let refused = send(
            &server.router,
            "POST",
            None,
            hook,
            &none,
            &headers,
            body.clone(),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.text);
    }
    // The bytes are what is signed: the same event re-serialised does not verify.
    let respaced = serde_json::to_vec_pretty(&event).unwrap();
    let refused = send(
        &server.router,
        "POST",
        None,
        hook,
        &none,
        &[("fake-signature", &good)],
        respaced,
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.text);
    // An account that is not there, and a method the receiver does not answer.
    let missing = send(
        &server.router,
        "POST",
        None,
        "/_feldspar/hooks/payments/events/nobody",
        &none,
        &[],
        body.clone(),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text);
    let get = send(&server.router, "GET", None, hook, &none, &[], Vec::new()).await;
    assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED, "{}", get.text);

    // Cancel sends the buyer to the cancel URL — the page itself, here — and
    // nothing happens at the provider.
    let second = try_account(&server, json!({ "account": "shop" }))
        .await
        .json();
    let second_page = path_of(second["url"].as_str().unwrap());
    let cancelled = send(
        &server.router,
        "POST",
        Some(SHOP),
        &format!("{second_page}/cancel"),
        &none,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::SEE_OTHER);
    let view = send(
        &server.router,
        "GET",
        Some(SHOP),
        &format!("{second_page}?format=json"),
        &none,
        &[],
        Vec::new(),
    )
    .await
    .json();
    assert_eq!(view["checkout"]["status"], json!("open"));
    assert_eq!(view["deliveries"], json!([]));
    Ok(())
}

/// A browser on the shop, keeping its cookies.
struct Browser<'a> {
    server: &'a Server,
    cookies: HashMap<String, String>,
}

impl<'a> Browser<'a> {
    fn new(server: &'a Server) -> Browser<'a> {
        Browser {
            server,
            cookies: HashMap::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> Answer {
        let (headers, bytes) = match body {
            Some(b) => (
                vec![("content-type", "application/json")],
                serde_json::to_vec(&b).unwrap(),
            ),
            None => (vec![], Vec::new()),
        };
        let answer = send(
            &self.server.router,
            method,
            Some(SHOP),
            path,
            &self.cookies,
            &headers,
            bytes,
        )
        .await;
        keep(&mut self.cookies, &answer);
        answer
    }

    /// The first page load, and `api.startGuestSession()`: the guest session's public id.
    async fn guest(&mut self) -> String {
        self.send("GET", "/", None).await;
        let started = self.send("POST", "/api/guest-session", None).await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.text);
        started.json()["id"].as_str().unwrap().to_owned()
    }

    /// The buyer, on the provider's page, presses `button`: the page posts it, and the
    /// browser follows the redirect — a cross-site navigation, so it carries no cookie.
    async fn press(&mut self, url: &str, button: &str) -> String {
        let pressed = send(
            &self.server.router,
            "POST",
            Some(SHOP),
            &format!("{}/{button}", path_of(url)),
            &HashMap::new(),
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(pressed.status, StatusCode::SEE_OTHER, "{}", pressed.text);
        pressed.headers[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned()
    }
}

/// An error answer's sentence.
pub(crate) fn error_of(answer: &Answer) -> String {
    answer.json()["error"]
        .as_str()
        .unwrap_or(&answer.text)
        .to_owned()
}

#[tokio::test]
async fn a_guest_pays_for_their_own_order_and_the_apps_trigger_marks_it_paid()
-> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, _) = setup("checkout").await?;
    let products = server.catalog.require("products")?;
    let orders = server.catalog.require("orders")?;
    let lines = server.catalog.require("order_lines")?;
    for product in [
        json!({ "id": 1, "name": "Tea", "price": 450 }),
        json!({ "id": 2, "name": "Cup", "price": 250 }),
    ] {
        sc_api::rows::create_row(&server.catalog, &products, &product).await?;
    }

    // Ann, a guest, has an open order of her own: two teas and a cup, with no prices on it.
    let mut ann = Browser::new(&server);
    let ann_session = ann.guest().await;
    let order = sc_api::rows::create_row(
        &server.catalog,
        &orders,
        &json!({ "session_id": ann_session, "status": "open", "email": "ann@example.com" }),
    )
    .await?;
    let order_id = order["id"].as_i64().unwrap();
    for (product, quantity) in [(1, 2), (2, 1)] {
        sc_api::rows::create_row(
            &server.catalog,
            &lines,
            &json!({ "order": order_id, "product": product, "quantity": quantity }),
        )
        .await?;
    }

    // Nobody else can check her order out: another guest, or a caller with no session at all.
    let mut bob = Browser::new(&server);
    bob.guest().await;
    let mut nobody = Browser::new(&server);
    nobody.send("GET", "/", None).await;
    for browser in [&mut bob, &mut nobody] {
        let refused = browser
            .send(
                "POST",
                "/api/actions/checkout_order",
                Some(json!({ "order": order_id })),
            )
            .await;
        assert!(!refused.status.is_success(), "{}", refused.text);
        assert!(
            error_of(&refused).contains(&format!(
                "there is no orders row {order_id} that you can read"
            )),
            "{}",
            refused.text
        );
    }
    assert!(server.rows("SELECT * FROM pay_objects").await.is_empty());

    // Ann checks out. The amount she sends is not read: the lines are priced through Ⱶ.
    let started = ann
        .send(
            "POST",
            "/api/actions/checkout_order",
            Some(json!({ "order": order_id, "amount": 1 })),
        )
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.text);
    let started = started.json();
    assert_eq!(started["amount"], json!(2 * 450 + 250), "{started}");
    assert_eq!(started["currency"], json!("EUR"));
    let url = started["url"].as_str().unwrap().to_owned();
    let mirror = server
        .rows("SELECT kind, ref_table, ref_key, email, amount, status, lines FROM pay_objects")
        .await;
    assert_eq!(mirror.len(), 1, "{mirror:?}");
    assert_eq!(mirror[0]["ref_table"], json!("orders"));
    // The key as the order has it: a number, which the trigger's `id === row.ref_key` finds.
    assert_eq!(mirror[0]["ref_key"], json!(order_id));
    assert_eq!(mirror[0]["email"], json!("ann@example.com"));
    assert_eq!(mirror[0]["amount"], json!(1150));
    assert_eq!(mirror[0]["lines"][0]["description"], json!("Tea"));
    assert_eq!(mirror[0]["lines"][0]["unit_amount"], json!(450));

    // She pays on the provider's page and is sent back to the shop.
    let back = ann.press(&url, "pay").await;
    assert_eq!(back, format!("http://{SHOP}/?paid={order_id}"));

    // The module logged it against her order, and the shop's own trigger marked the order paid.
    let events = server
        .rows(
            "SELECT kind, ref_table, ref_key, amount, currency, status FROM pay_events ORDER BY id",
        )
        .await;
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["checkout.completed", "payment.succeeded"]);
    assert_eq!(events[1]["ref_key"], json!(order_id));
    assert_eq!(events[1]["amount"], json!(1150));
    assert_eq!(events[1]["status"], json!("succeeded"));
    let status = server
        .rows(&format!("SELECT status FROM orders WHERE id = {order_id}"))
        .await;
    assert_eq!(status[0]["status"], json!("paid"));

    // The return was a cross-site navigation, which carries no guest cookie; the success page's
    // own API call does, and finds her order paid.
    let landing = send(
        &server.router,
        "GET",
        Some(SHOP),
        &path_of(&back),
        &HashMap::new(),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(landing.status, StatusCode::OK, "{}", landing.text);
    let mine = ann.send("GET", "/api/orders", None).await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.text);
    assert_eq!(mine.json()[0]["status"], json!("paid"), "{}", mine.text);
    assert_eq!(bob.send("GET", "/api/orders", None).await.json(), json!([]));

    // P2.6: a refund of her order, found from the order; only she can ask for it.
    let refused = bob
        .send(
            "POST",
            "/api/actions/refund_order",
            Some(json!({ "order": order_id })),
        )
        .await;
    assert!(!refused.status.is_success(), "{}", refused.text);
    let refunded = ann
        .send(
            "POST",
            "/api/actions/refund_order",
            Some(json!({ "order": order_id })),
        )
        .await;
    assert_eq!(refunded.status, StatusCode::OK, "{}", refunded.text);
    assert_eq!(refunded.json()["amount"], json!(1150), "{}", refunded.text);
    let after = server
        .rows("SELECT kind, ref_key, status FROM pay_events ORDER BY id")
        .await;
    assert_eq!(
        after.last().unwrap(),
        &json!({ "kind": "payment.refunded", "ref_key": order_id, "status": "refunded" })
    );
    // Nothing is left to refund.
    let again = ann
        .send(
            "POST",
            "/api/actions/refund_order",
            Some(json!({ "order": order_id })),
        )
        .await;
    assert!(
        error_of(&again).contains("there is no payment for orders"),
        "{}",
        again.text
    );
    Ok(())
}

#[tokio::test]
async fn rows_keyed_by_text_and_by_uuid_are_paid_for_and_found_again() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, _) = setup("keys").await?;
    let booking = "6f1c2d3e-4b5a-4c6d-8e7f-0123456789ab";
    sc_api::rows::create_row(
        &server.catalog,
        &server.catalog.require("invoices")?,
        &json!({ "number": "INV-7", "amount": 12000, "status": "sent" }),
    )
    .await?;
    sc_api::rows::create_row(
        &server.catalog,
        &server.catalog.require("bookings")?,
        &json!({ "id": booking, "deposit": 5000, "status": "held" }),
    )
    .await?;

    let mut customer = Browser::new(&server);
    customer.send("GET", "/", None).await;
    for (trigger, payload, amount, currency) in [
        (
            "checkout_invoice",
            json!({ "number": "INV-7" }),
            12000,
            "GBP",
        ),
        // A key sent in capitals is the same booking.
        (
            "checkout_booking",
            json!({ "booking": booking.to_uppercase() }),
            5000,
            "JPY",
        ),
    ] {
        let started = customer
            .send("POST", &format!("/api/actions/{trigger}"), Some(payload))
            .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.text);
        let started = started.json();
        assert_eq!(started["amount"], json!(amount));
        assert_eq!(started["currency"], json!(currency));
        // The shop has no success path for these: the buyer comes back to its home page.
        let back = customer
            .press(started["url"].as_str().unwrap(), "pay")
            .await;
        assert_eq!(back, format!("http://{SHOP}/"));
    }
    let keys = server
        .rows("SELECT ref_table, ref_key FROM pay_events WHERE kind = 'payment.succeeded' ORDER BY id")
        .await;
    assert_eq!(
        keys,
        [
            json!({ "ref_table": "invoices", "ref_key": "INV-7" }),
            json!({ "ref_table": "bookings", "ref_key": booking }),
        ]
    );
    let invoice = server
        .rows("SELECT status FROM invoices WHERE number = 'INV-7'")
        .await;
    assert_eq!(invoice[0]["status"], json!("paid"));
    let paid = server.rows("SELECT status FROM bookings").await;
    assert_eq!(paid[0]["status"], json!("paid"));
    let described = server
        .rows("SELECT lines FROM pay_objects WHERE ref_table = 'invoices'")
        .await;
    assert_eq!(
        described[0]["lines"][0]["description"],
        json!("Invoice INV-7")
    );
    Ok(())
}

#[tokio::test]
async fn each_provider_event_is_processed_once_in_whatever_order_it_arrives() -> sc_error::Result<()>
{
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, _) = setup("events").await?;
    let none = HashMap::new();
    let mut buyer = Browser::new(&server);
    buyer.send("GET", "/", None).await;

    // A provider that holds its webhooks: the card is declined once, then paid.
    let started = try_account(&server, json!({ "account": "held" }))
        .await
        .json();
    let url = started["url"].as_str().unwrap().to_owned();
    buyer.press(&url, "fail").await;
    buyer.press(&url, "pay").await;
    assert!(server.rows("SELECT * FROM pay_events").await.is_empty());
    let view = send(
        &server.router,
        "GET",
        Some(SHOP),
        &format!("{}?format=json", path_of(&url)),
        &none,
        &[],
        Vec::new(),
    )
    .await
    .json();
    let events: Vec<(String, String)> = view["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["id"].as_str().unwrap().to_owned(),
                e["type"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let types: Vec<&str> = events.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(
        types,
        ["payment.failed", "checkout.completed", "payment.succeeded"]
    );

    // Delivered last first, and each of them more than once.
    let resend =
        |id: &str| format!("/_feldspar/hooks/payments/fake/held/events/{id}/resend?format=json");
    for (id, _) in events.iter().rev().chain(events.iter()) {
        let sent = send(
            &server.router,
            "POST",
            None,
            &resend(id),
            &none,
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.text);
        assert_eq!(sent.json()["status"], json!(200), "{}", sent.text);
    }
    let logged = server
        .rows("SELECT kind, event_id, status FROM pay_events ORDER BY id")
        .await;
    let kinds: Vec<&str> = logged.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["payment.succeeded", "checkout.completed", "payment.failed"]
    );
    // The application's trigger ran once for the one payment.
    assert_eq!(server.rows("SELECT * FROM payment_log").await.len(), 1);
    // The mirror says what the provider says now, whatever order it heard things in.
    let mirror = server
        .rows("SELECT kind, status FROM pay_objects ORDER BY kind, status")
        .await;
    assert_eq!(
        mirror,
        [
            json!({ "kind": "checkout", "status": "complete" }),
            json!({ "kind": "payment", "status": "failed" }),
            json!({ "kind": "payment", "status": "succeeded" }),
        ]
    );

    // A provider event the module does not know is kept for the application, as `other`.
    let hook = "/_feldspar/hooks/payments/events/held";
    let odd = serde_json::to_vec(&json!({
        "id": "evt_payout", "type": "payout.paid", "account": "held", "data": {},
    }))
    .unwrap();
    let accepted = send(
        &server.router,
        "POST",
        None,
        hook,
        &none,
        &[("fake-signature", &signature(SECRET, &odd))],
        odd.clone(),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
    let other = server
        .rows("SELECT kind, provider_type, raw FROM pay_events WHERE event_id = 'evt_payout'")
        .await;
    assert_eq!(other[0]["kind"], json!("other"));
    assert_eq!(other[0]["provider_type"], json!("payout.paid"));
    assert_eq!(other[0]["raw"]["id"], json!("evt_payout"));

    // A bad signature stores nothing; an event about an object the provider does not have is a
    // 500, so the provider tries again, and stores nothing either.
    let count = server.rows("SELECT * FROM pay_events").await.len();
    let forged = send(
        &server.router,
        "POST",
        None,
        hook,
        &none,
        &[("fake-signature", &signature("guess", &odd))],
        odd,
    )
    .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let gone = serde_json::to_vec(&json!({
        "id": "evt_gone", "type": "payment.succeeded", "account": "held",
        "data": { "object": { "kind": "payment", "id": "pay_fake_gone" } },
    }))
    .unwrap();
    let failed = send(
        &server.router,
        "POST",
        None,
        hook,
        &none,
        &[("fake-signature", &signature(SECRET, &gone))],
        gone,
    )
    .await;
    assert_eq!(
        failed.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        failed.text
    );
    assert_eq!(server.rows("SELECT * FROM pay_events").await.len(), count);
    Ok(())
}

#[tokio::test]
async fn the_sweep_catches_what_was_never_delivered_and_the_escape_hatches_reach_the_provider()
-> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, installed) = setup("sweep").await?;
    // The module runs its reconcile service.
    assert_eq!(
        installed["services"][0]["name"],
        json!("reconcile"),
        "{installed}"
    );
    let mut buyer = Browser::new(&server);
    buyer.send("GET", "/", None).await;

    // The held provider takes a payment and delivers nothing: nothing is known of it.
    let started = try_account(&server, json!({ "account": "held" }))
        .await
        .json();
    buyer.press(started["url"].as_str().unwrap(), "pay").await;
    assert!(server.rows("SELECT * FROM pay_events").await.is_empty());

    // The sweep reads the provider's events and processes them; a second sweep finds nothing new.
    let swept = buyer
        .send(
            "POST",
            "/api/actions/sync_payments",
            Some(json!({ "account": "held" })),
        )
        .await;
    assert_eq!(swept.status, StatusCode::OK, "{}", swept.text);
    assert_eq!(
        swept.json()["swept"][0]["logged"],
        json!(2),
        "{}",
        swept.text
    );
    let again = buyer
        .send("POST", "/api/actions/sync_payments", Some(json!({})))
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text);
    let logged: Vec<i64> = again.json()["swept"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["logged"].as_i64())
        .collect();
    assert!(logged.iter().all(|n| *n == 0), "{}", again.text);
    let kinds = server.rows("SELECT kind FROM pay_events ORDER BY id").await;
    assert_eq!(
        kinds,
        [
            json!({ "kind": "checkout.completed" }),
            json!({ "kind": "payment.succeeded" })
        ]
    );
    assert_eq!(server.rows("SELECT * FROM payment_log").await.len(), 1);
    let synced = server
        .rows("SELECT name FROM pay_accounts WHERE synced_at IS NOT NULL ORDER BY name")
        .await;
    assert!(synced.contains(&json!({ "name": "held" })), "{synced:?}");

    // The escape hatches: the provider's client in a body of the admin's, and `pay_api` from any
    // code body — both on the shop's account.
    let shop = try_account(&server, json!({ "account": "shop" }))
        .await
        .json();
    let id = shop["id"].as_str().unwrap();
    let code = buyer
        .send(
            "POST",
            "/api/actions/provider_code",
            Some(json!({ "id": id })),
        )
        .await;
    assert_eq!(code.status, StatusCode::OK, "{}", code.text);
    assert_eq!(
        code.json(),
        json!({ "status": "open", "account": { "name": "shop", "adapter": "fake", "mode": "test" } })
    );
    let api = buyer
        .send("POST", "/api/actions/call_api", Some(json!({ "id": id })))
        .await;
    assert_eq!(api.status, StatusCode::OK, "{}", api.text);
    assert_eq!(api.json()["status"], json!("open"), "{}", api.text);
    assert_eq!(api.json()["amount_total"], json!(1000), "{}", api.text);
    Ok(())
}
