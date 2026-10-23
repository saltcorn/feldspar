//! The shop of [docs/tutorial-payments.md](../../../docs/tutorial-payments.md), part 1
//! (TODO-payments P2.8), built the way the tutorial builds it, so the tutorial's tables, rules and
//! triggers are known to do what it says they do:
//!
//! - a guest keeps an `open` order of their own, and cannot touch anybody else's, nor set their
//!   order's status, nor price a line (there is no price column);
//! - `api.runCheckout({ order })` charges the lines priced through `productⱵprice`;
//! - paying marks the order paid, takes the stock down, writes a download row the buyer (and
//!   nobody else) can read and download, and emails them;
//! - signing in from the same browser moves the guest's order to the account, and somebody
//!   else's sign-in moves nothing.
//!
//! Needs `npm`, which installs the module, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::{StatusCode, header};
use sc_action::{EventKind, Trigger, TriggerDispatcher, save_trigger};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, TriggerRef};
use sc_auth::{SessionStore, create_user};
use sc_catalog::{Catalog, DataFieldKind, FieldId, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_email::{Mailer, RecordingMailer, parse_mailbox};
use sc_server::{
    AppMounts, GUEST_COOKIE, ModuleServices, MountedApp, ServerConfig, admin_handlers,
    base_action_registry, build_router_with_apps, default_js_evaluator, install_agents,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};

use crate::payments_module::{Answer, error_of, have_npm, send};

const SHOP: &str = "shop.example.com";

/// A browser on the shop, keeping (and dropping) its cookies.
pub(crate) struct Browser<'a> {
    router: &'a Router,
    host: &'static str,
    pub(crate) cookies: HashMap<String, String>,
}

impl<'a> Browser<'a> {
    pub(crate) fn new(router: &'a Router) -> Browser<'a> {
        Browser::on(router, SHOP)
    }

    /// A browser on another application's host.
    pub(crate) fn on(router: &'a Router, host: &'static str) -> Browser<'a> {
        Browser {
            router,
            host,
            cookies: HashMap::new(),
        }
    }

    pub(crate) async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> Answer {
        let (headers, bytes) = match body {
            Some(b) => (
                vec![("content-type", "application/json")],
                serde_json::to_vec(&b).unwrap(),
            ),
            None => (vec![], Vec::new()),
        };
        let answer = send(
            self.router,
            method,
            Some(self.host),
            path,
            &self.cookies,
            &headers,
            bytes,
        )
        .await;
        for raw in answer.headers.get_all(header::SET_COOKIE) {
            if let Some((name, value)) = raw
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .and_then(|p| p.split_once('='))
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        answer
    }

    pub(crate) async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let answer = self.send(method, path, body).await;
        assert!(
            answer.status.is_success(),
            "{method} {path}: {} {}",
            answer.status,
            answer.text
        );
        answer.json()
    }

    /// The shop's first page, and `api.startGuestSession()` when the basket gets its first item.
    async fn guest(router: &'a Router) -> Browser<'a> {
        let mut browser = Browser::new(router);
        browser.send("GET", "/", None).await;
        browser.ok("POST", "/api/guest-session", None).await;
        browser
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

fn required(spec: FieldSpec) -> FieldSpec {
    FieldSpec {
        required: true,
        ..spec
    }
}

fn defaulted(spec: FieldSpec, expression: &str) -> FieldSpec {
    FieldSpec {
        default_expression: Some(expression.to_owned()),
        ..spec
    }
}

fn file(name: &str) -> FieldSpec {
    FieldSpec {
        kind: DataFieldKind::File {
            store: FileStoreId("goods".to_owned()),
            folder: None,
            mime_allow: Vec::new(),
            on_delete: Default::default(),
            on_conflict: Default::default(),
        },
        ..field(name, "")
    }
}

/// The ownership rule of the tutorial's Step 2: the signed-in buyer's, or the guest's.
const MINE: &str = "user ? user_id === user.id : session_id === session.id";

/// Step 2's tables, as the Tables screen makes them.
async fn create_tables(catalog: &Catalog) -> sc_error::Result<()> {
    let id = || FieldSpec {
        primary_key: true,
        ..field("id", "int")
    };
    let tables: Vec<(&str, u8, Option<String>, Vec<FieldSpec>)> = vec![
        (
            "products",
            100,
            None,
            vec![
                id(),
                required(field("name", "text")),
                required(field("price", "int")),
                field("stock", "int"),
            ],
        ),
        (
            "product_files",
            1,
            None,
            vec![id(), required(key("product", "products")), file("file")],
        ),
        (
            "orders",
            1,
            Some(format!("({MINE}) && (_read || status === \"open\")")),
            vec![
                id(),
                defaulted(key("user_id", "users"), "user && user.id"),
                defaulted(field("session_id", "text"), "session && session.id"),
                required(defaulted(field("status", "text"), "\"open\"")),
                field("email", "text"),
            ],
        ),
        (
            "order_lines",
            1,
            Some(
                "(user ? orderⱵuser_id === user.id : orderⱵsession_id === session.id) \
                 && (_read || orderⱵstatus === \"open\")"
                    .to_owned(),
            ),
            vec![
                id(),
                required(key("order", "orders")),
                required(key("product", "products")),
                required(defaulted(field("quantity", "int"), "1")),
            ],
        ),
        (
            "downloads",
            1,
            Some(
                "_read && (user ? orderⱵuser_id === user.id : orderⱵsession_id === session.id)"
                    .to_owned(),
            ),
            vec![
                id(),
                required(key("order", "orders")),
                required(key("product", "products")),
                file("file"),
            ],
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
                    ownership_formula: ownership,
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

/// What runs when a payment for an order succeeds.
const PAID_ORDER: &str = "row.kind === \"payment.succeeded\" && row.ref_table === \"orders\"";

/// Steps 3–6's triggers.
fn triggers() -> Vec<Trigger> {
    let checkout = [
        ("account", json!("shop")),
        ("mode", json!("payment")),
        ("table", json!("orders")),
        ("key_from", json!("payload")),
        ("key_field", json!("order")),
        ("read_check", json!(true)),
        ("lines", json!("children")),
        ("child_table", json!("order_lines")),
        ("child_key", json!("order")),
        ("quantity", json!("quantity")),
        ("unit_amount", json!("productⱵprice")),
        ("line_description", json!("productⱵname")),
        ("item", json!("product")),
        ("currency", json!("EUR")),
        ("email", json!("email")),
        ("tax", json!("none")),
        ("shipping", json!(false)),
        ("success_path", json!("/orders/{key}")),
        ("cancel_path", json!("/basket")),
    ]
    .into_iter()
    .fold(
        Trigger::new("checkout", EventKind::None, "pay_checkout").min_role(100),
        |t, (k, v)| t.config(k, v),
    );
    vec![
        checkout,
        Trigger::new("order_paid", EventKind::Insert, "update_rows")
            .on("pay_events")
            .only_if(PAID_ORDER)
            .config("table", "orders")
            .config("where", "id === row.ref_key")
            .config("assignments", json!({ "status": "\"paid\"" })),
        Trigger::new("take_stock", EventKind::Insert, "update_rows")
            .on("pay_events")
            .only_if(PAID_ORDER)
            .config("table", "products")
            .config(
                "where",
                "stock !== null && order_linesↃproduct.some(l => l.order === row.ref_key)",
            )
            .config(
                "assignments",
                json!({
                    "stock": "stock - order_linesↃproduct.filter(l => l.order === row.ref_key).sum(\"quantity\")"
                }),
            ),
        Trigger::new("deliver_downloads", EventKind::Insert, "run_js_code")
            .on("pay_events")
            .only_if(PAID_ORDER)
            .config(
                "code",
                "const lines = await db.order_lines.where({ order: row.ref_key }).rows();\n\
                 for (const line of lines) {\n\
                   const download = await db.product_files.where({ product: line.product }).first();\n\
                   if (download) {\n\
                     await db.downloads.insert({ order: line.order, product: line.product, file: download.file });\n\
                   }\n\
                 }",
            ),
        Trigger::new("email_download", EventKind::Insert, "send_email")
            .on("downloads")
            .config("to", "{{ orderⱵemail }}")
            .config("subject", "Your download of {{ productⱵname }}")
            .config(
                "text",
                "Thank you for your order. Your download is on its page: \
                 https://shop.example.com/orders/{{ order }}",
            ),
        Trigger::new("adopt_orders", EventKind::Login, "update_rows")
            .config("table", "orders")
            .config(
                "where",
                "session_id !== null && session_id === payload.guest_session",
            )
            .config("assignments", json!({ "user_id": "user.id" })),
    ]
}

struct Shop {
    router: Router,
    catalog: Arc<Catalog>,
    mail: Arc<RecordingMailer>,
    root: PathBuf,
    db: TestDb,
    _modules: Arc<ModuleServices>,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

impl Drop for Shop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Shop {
    async fn rows(&self, sql: &str) -> Vec<Value> {
        rows_of(&self.db, sql).await
    }
}

/// A server as Step 1 of either part of the tutorial leaves it: the payments module installed
/// from the Modules tab by a signed-in admin, with a mailer that records what it sends and a
/// session store that re-reads a user on every request (so a role grant shows at once).
pub(crate) struct Booted {
    pub(crate) router: Router,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) mail: Arc<RecordingMailer>,
    pub(crate) apps: Arc<AppMounts>,
    pub(crate) dispatcher: Arc<TriggerDispatcher>,
    pub(crate) root: PathBuf,
    pub(crate) db: TestDb,
    pub(crate) admin: HashMap<String, String>,
    pub(crate) module_id: String,
    pub(crate) modules: Arc<ModuleServices>,
    pub(crate) registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

impl Booted {
    pub(crate) async fn rows(&self, sql: &str) -> Vec<Value> {
        rows_of(&self.db, sql).await
    }

    /// Run one statement that answers no rows: an `UPDATE` standing in for time passing.
    pub(crate) async fn execute(&self, sql: &str) {
        self.db
            .client()
            .await
            .unwrap()
            .execute(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// One query's rows, as JSON objects.
async fn rows_of(db: &TestDb, sql: &str) -> Vec<Value> {
    let wrapped = format!("SELECT coalesce(json_agg(q), '[]'::json) FROM ({sql}) q");
    let value: Value = db
        .client()
        .await
        .unwrap()
        .query_one(&wrapped, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0);
    value.as_array().unwrap().clone()
}

pub(crate) async fn boot(tag: &str) -> sc_error::Result<Booted> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-tutorial-payments-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let goods = root.join("goods");
    std::fs::create_dir_all(&goods).unwrap();
    std::fs::write(goods.join("tea-guide.pdf"), b"%PDF the tea guide").unwrap();
    sc_catalog::save_file_store(
        &catalog,
        &sc_files::FileStoreDef::local("goods", goods.to_string_lossy().as_ref()),
    )
    .await?;
    sc_catalog::connect_all_file_stores(&catalog).await?;

    // The dispatcher `install_triggers` makes, with a mailer that records what it sends.
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let evaluator = default_js_evaluator();
    sc_action::bootstrap_triggers(&catalog).await?;
    sc_workflow::bootstrap_workflow_versions(&catalog).await?;
    sc_workflow::bootstrap_run_traces(&catalog).await?;
    catalog.set_formula_evaluator(Arc::clone(&evaluator))?;
    let mail = Arc::new(RecordingMailer::sending_as(
        parse_mailbox("shop@example.com").unwrap(),
    ));
    let dispatcher = Arc::new(
        TriggerDispatcher::new(Arc::new(base_action_registry(&agents, &models)?))
            .with_evaluator(evaluator.clone())
            .with_mailer(mail.clone() as Arc<dyn Mailer>),
    );
    dispatcher.reload(&catalog).await?;
    catalog.add_table_events(
        sc_catalog::TRIGGER_EVENTS,
        Arc::clone(&dispatcher) as Arc<dyn sc_catalog::TableEvents>,
    )?;

    let registries = crate::module_registries().read().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.join("modules")),
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
        Arc::new(SessionStore::database_with(
            catalog.clone(),
            chrono::Duration::hours(24),
            chrono::Duration::zero(),
            100,
        )),
        &config,
        apps.clone(),
    )?;

    // Step 1: install the module and add a test account on the fake provider.
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
    crate::payments_module::keep(&mut admin, &primed);
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
    crate::payments_module::keep(&mut admin, &first);
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
    let module_id = installed.json()["id"].as_str().unwrap().to_owned();
    Ok(Booted {
        router,
        catalog,
        mail,
        apps,
        dispatcher,
        root,
        db,
        admin,
        module_id,
        modules,
        registries,
    })
}

async fn setup() -> sc_error::Result<Shop> {
    let Booted {
        router,
        catalog,
        mail,
        apps,
        dispatcher,
        root,
        db,
        modules,
        registries,
        ..
    } = boot("shop").await?;
    sc_api::rows::create_row(
        &catalog,
        &catalog.require("pay_accounts")?,
        &json!({ "name": "shop", "adapter": "fake", "mode": "test",
                 "webhook_secret": "any random text" }),
    )
    .await?;

    // Steps 2–6.
    create_tables(&catalog).await?;
    sc_auth::save_role(&catalog, &sc_auth::Role::new(80, "User")).await?;
    for trigger in triggers() {
        save_trigger(&catalog, &dispatcher.registry(), &trigger).await?;
    }
    dispatcher.reload(&catalog).await?;
    let issues: Vec<String> = dispatcher
        .triggers()?
        .issues()
        .iter()
        .map(|i| format!("{}: {}", i.trigger, i.problem))
        .collect();
    assert!(issues.is_empty(), "{issues:?}");
    let mut app = Application::new("Shop", "shop", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_trigger(TriggerRef::new("checkout"))
        .attribute(sc_app::GUEST_SESSIONS_ATTRIBUTE, true);
    for table in ["products", "orders", "order_lines", "downloads"] {
        app = app.with_table(TableId(table.to_owned()));
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

    // What the shop sells: two teas, and a guide to download.
    let products = catalog.require("products")?;
    for product in [
        json!({ "id": 1, "name": "Sencha", "price": 450, "stock": 10 }),
        json!({ "id": 2, "name": "Teapot", "price": 2500, "stock": 3 }),
        json!({ "id": 3, "name": "The tea guide", "price": 900 }),
    ] {
        sc_api::rows::create_row(&catalog, &products, &product).await?;
    }
    sc_api::rows::create_row(
        &catalog,
        &catalog.require("product_files")?,
        &json!({ "product": 3, "file": "tea-guide.pdf" }),
    )
    .await?;

    Ok(Shop {
        router,
        catalog,
        mail,
        root,
        db,
        _modules: modules,
        _registries: registries,
    })
}

#[tokio::test]
async fn the_tutorials_shop_takes_a_guests_order_to_paid_and_delivers_it() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let shop = setup().await?;

    // Ann's basket, as a guest: an open order of her own, and its lines — no prices.
    let mut ann = Browser::guest(&shop.router).await;
    let order = ann
        .ok(
            "POST",
            "/api/orders",
            Some(json!({ "email": "ann@example.com" })),
        )
        .await;
    let order_id = order["id"].as_i64().unwrap();
    assert_eq!(order["status"], json!("open"));
    assert_eq!(order["user_id"], json!(null));
    assert!(order["session_id"].as_str().unwrap().starts_with("gs_"));
    for (product, quantity) in [(1, 2), (3, 1)] {
        ann.ok(
            "POST",
            "/api/order_lines",
            Some(json!({ "order": order_id, "product": product, "quantity": quantity })),
        )
        .await;
    }
    // The basket survives a reload: the same cookie reads it again.
    assert_eq!(
        ann.ok("GET", "/api/order_lines", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Nobody else can see it or add to it, and Ann cannot mark it paid herself.
    let mut bob = Browser::guest(&shop.router).await;
    assert_eq!(bob.ok("GET", "/api/orders", None).await, json!([]));
    let sneaked = bob
        .send(
            "POST",
            "/api/order_lines",
            Some(json!({ "order": order_id, "product": 2, "quantity": 1 })),
        )
        .await;
    assert!(!sneaked.status.is_success(), "{}", sneaked.text);
    let paid_by_hand = ann
        .send(
            "PUT",
            &format!("/api/orders/{order_id}"),
            Some(json!({ "status": "paid" })),
        )
        .await;
    assert!(!paid_by_hand.status.is_success(), "{}", paid_by_hand.text);

    // Checkout: two Sencha and the guide, priced by the products.
    let started = ann
        .ok(
            "POST",
            "/api/actions/checkout",
            Some(json!({ "order": order_id })),
        )
        .await;
    assert_eq!(started["amount"], json!(2 * 450 + 900), "{started}");
    let url = started["url"].as_str().unwrap();
    let page = url.strip_prefix(&format!("http://{SHOP}")).unwrap();
    let paid = send(
        &shop.router,
        "POST",
        Some(SHOP),
        &format!("{page}/pay"),
        &HashMap::new(),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(paid.status, StatusCode::SEE_OTHER, "{}", paid.text);
    assert_eq!(
        paid.headers[header::LOCATION].to_str().unwrap(),
        format!("http://{SHOP}/orders/{order_id}")
    );

    // Step 4: the order is paid, and the stock went down by what was sold.
    let orders = ann.ok("GET", "/api/orders", None).await;
    assert_eq!(orders[0]["status"], json!("paid"), "{orders}");
    let stock = shop
        .rows("SELECT id, stock FROM products ORDER BY id")
        .await;
    assert_eq!(
        stock,
        [
            json!({ "id": 1, "stock": 8 }),
            json!({ "id": 2, "stock": 3 }),
            json!({ "id": 3, "stock": null }),
        ]
    );
    // A paid order is closed: its lines no longer change.
    let line = ann.ok("GET", "/api/order_lines", None).await[0]["id"].clone();
    let changed = ann
        .send(
            "PUT",
            &format!("/api/order_lines/{line}"),
            Some(json!({ "quantity": 5 })),
        )
        .await;
    assert!(!changed.status.is_success(), "{}", changed.text);

    // Step 5: the guide is Ann's to download, and nobody else's; she is emailed.
    let downloads = ann.ok("GET", "/api/downloads", None).await;
    assert_eq!(downloads.as_array().unwrap().len(), 1, "{downloads}");
    let download = downloads[0]["id"].as_i64().unwrap();
    let bytes = ann
        .send("GET", &format!("/api/downloads/{download}/file"), None)
        .await;
    assert_eq!(bytes.status, StatusCode::OK, "{}", bytes.text);
    assert_eq!(bytes.text, "%PDF the tea guide");
    assert_eq!(bob.ok("GET", "/api/downloads", None).await, json!([]));
    let refused = bob
        .send("GET", &format!("/api/downloads/{download}/file"), None)
        .await;
    assert!(!refused.status.is_success(), "{}", refused.text);
    let sent = shop.mail.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].to[0].address, "ann@example.com");
    assert_eq!(sent[0].subject, "Your download of The tea guide");
    assert!(
        sent[0]
            .text
            .as_deref()
            .unwrap()
            .contains(&format!("/orders/{order_id}")),
        "{:?}",
        sent[0].text
    );

    // Step 6: Ann signs up from the same browser, and her order is now her account's. Ada,
    // signing in with no guest session, moves nothing.
    let ann_id = create_user(&shop.catalog, "ann@example.com", "ann-password", 80)
        .await?
        .id;
    create_user(&shop.catalog, "ada@example.com", "ada-password", 80).await?;
    let mut ada = Browser::new(&shop.router);
    ada.send("GET", "/", None).await;
    ada.ok(
        "POST",
        "/api/login",
        Some(json!({ "email": "ada@example.com", "password": "ada-password" })),
    )
    .await;
    assert_eq!(
        shop.rows("SELECT user_id FROM orders").await,
        [json!({ "user_id": null })]
    );
    ann.ok(
        "POST",
        "/api/login",
        Some(json!({ "email": "ann@example.com", "password": "ann-password" })),
    )
    .await;
    assert!(!ann.cookies.contains_key(GUEST_COOKIE));
    assert_eq!(
        shop.rows("SELECT user_id FROM orders").await,
        [json!({ "user_id": ann_id.to_string() })]
    );
    let hers = ann.ok("GET", "/api/orders", None).await;
    assert_eq!(hers[0]["id"], json!(order_id), "{hers}");
    assert_eq!(
        ann.ok("GET", "/api/downloads", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(ada.ok("GET", "/api/orders", None).await, json!([]));
    // Signed in, Ann's next order is her account's, with no guest session. Somebody else's
    // sign-in has no guest session either, and the trigger's `session_id !== null` is what keeps
    // it from matching every such order.
    let second = ann
        .ok(
            "POST",
            "/api/orders",
            Some(json!({ "email": "ann@example.com" })),
        )
        .await;
    assert_eq!(second["session_id"], json!(null));
    assert_eq!(second["user_id"], json!(ann_id.to_string()));
    let mut ada_again = Browser::new(&shop.router);
    ada_again.send("GET", "/", None).await;
    ada_again
        .ok(
            "POST",
            "/api/login",
            Some(json!({ "email": "ada@example.com", "password": "ada-password" })),
        )
        .await;
    assert_eq!(
        shop.rows(&format!(
            "SELECT user_id FROM orders WHERE id = {}",
            second["id"]
        ))
        .await,
        [json!({ "user_id": ann_id.to_string() })]
    );
    // A refusal names what is wrong, as every refusal here does.
    let other = ada
        .send(
            "POST",
            "/api/actions/checkout",
            Some(json!({ "order": order_id })),
        )
        .await;
    assert!(
        error_of(&other).contains("that you can read"),
        "{}",
        other.text
    );
    Ok(())
}
