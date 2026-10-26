//! The bundled mail module in ingest mode, on the real host (TODO-imap M8.7, M8.8).
//!
//! `plugins/imap` is installed and loaded on this server's Deno worker, its
//! service is started by the [`ServiceSupervisor`], and it syncs a local
//! Dovecot (`plugins/imap/test/servers/dovecot.sh`) into Postgres. Then:
//!
//! - **ownership** (§3): the shared account's mail is visible to a staff user,
//!   not to a user below its `min_role`, and never through another user's
//!   personal account; `imap_password` is in no REST answer;
//! - **new mail during IDLE** reaches its row within 5 s, from inside the worker;
//! - **the actions** (M8.6) load as the caller first: `mail_set_flags` changes
//!   the server, `mail_attachment` stores a file again, `mail_sync_now` and
//!   `mail_test_account` refuse what the caller may not see;
//! - **the order example** (M8.8): a trigger on `mail_messages`, filtered to the
//!   shared account, makes one `orders` row per mail with its PDF.
//!
//! The IMAP behaviour itself — 1,000 mails, a server restart mid-batch,
//! UIDVALIDITY, a broken mail, the processed folder — is tested at speed by
//! `npm test` in `plugins/imap` (`test/ingest.test.mjs`) against the same
//! Dovecot.
//!
//! The helpers here are shared with `imap_mirror.rs` (M9.5).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
pub(crate) mod deno {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use sc_action::{
        ActionContext, ActionRegistry, Event, EventKind, Trigger, TriggerDispatcher,
        bootstrap_triggers, save_trigger,
    };
    use sc_api::code_host::TableHost;
    use sc_api::{ApiProvider, ApiRequest, RestProvider};
    use sc_catalog::{Catalog, SecretKey, TableEvents, bootstrap_field_meta, bootstrap_table_meta};
    use sc_db::DatabaseDriver;
    use sc_db_postgres::PgDriver;
    use sc_expr::{CodeHost, DenoEvaluator, JsEvaluator};
    use sc_module::{
        ANY_HOST, Backoff, BundledModules, Installer, Module, ModulePermissions, ModuleSet,
        ModuleSource, ServiceSupervisor, save_module,
    };
    use sc_test_harness::TestDb;
    use sc_types::{Attrs, SECRET_SENTINEL};
    use serde_json::{Value as Json, json};

    use crate::common::{have_npm, surfaces, temp_root};

    pub(crate) const PASSWORD: &str = "Grüße-€-pässwort";
    pub(crate) const MODULE: &str = "@feldspar/imap";

    pub(crate) fn servers() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/imap/test/servers")
    }

    pub(crate) fn dovecot(args: &[&str]) -> bool {
        Command::new(servers().join("dovecot.sh"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Not inherited: a daemon started here would hold the test's output open.
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Deliver a mail to `user`'s `folder`, as the mail server would.
    pub(crate) fn deliver(dir: &Path, user: &str, name: &str, source: &str) {
        let path = dir.join(name);
        std::fs::write(&path, source.replace('\n', "\r\n")).unwrap();
        assert!(
            dovecot(&["deliver", user, "INBOX", &path.display().to_string()]),
            "doveadm save failed"
        );
    }

    fn order_mail(n: u32) -> String {
        format!(
            "From: Shop Customer <customer{n}@example.com>\n\
             To: orders@example.com\n\
             Subject: Order {n}\n\
             Message-ID: <order{n}@example.com>\n\
             Date: Wed, 21 Oct 2026 10:00:00 +0000\n\
             MIME-Version: 1.0\n\
             Content-Type: multipart/mixed; boundary=\"b\"\n\
             \n\
             --b\n\
             Content-Type: text/plain; charset=utf-8\n\
             \n\
             Please send {n} widgets.\n\
             --b\n\
             Content-Type: application/pdf; name=\"order-{n}.pdf\"\n\
             Content-Disposition: attachment; filename=\"order-{n}.pdf\"\n\
             Content-Transfer-Encoding: base64\n\
             \n\
             {}\n\
             --b--\n",
            base64(format!("%PDF-1.4 order {n}").as_bytes())
        )
    }

    fn plain_mail(subject: &str) -> String {
        format!(
            "From: someone@example.com\nTo: x@example.com\nSubject: {subject}\n\
             Message-ID: <{}@example.com>\nDate: Wed, 21 Oct 2026 10:00:00 +0000\n\n\
             Just text.\n",
            subject.replace(' ', "-")
        )
    }

    pub(crate) fn base64(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    const SCHEMA: &str = "
        CREATE TABLE orders (id bigint generated by default as identity primary key,
            message bigint, customer text, title text, pdf text, attachments int);
        CREATE TABLE mail_log (id bigint generated by default as identity primary key,
            message bigint, subject text, attachments_seen int, who text);
    ";

    pub(crate) async fn catalog(db: &TestDb) -> Arc<Catalog> {
        db.client()
            .await
            .unwrap()
            .batch_execute(SCHEMA)
            .await
            .unwrap();
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>)
            .await
            .unwrap();
        cat.set_secret_key(SecretKey::generate().unwrap());
        sc_auth::bootstrap(&cat).await.unwrap();
        sc_auth::bootstrap_roles(&cat).await.unwrap();
        bootstrap_table_meta(&cat).await.unwrap();
        bootstrap_field_meta(&cat).await.unwrap();
        sc_catalog::bootstrap_file_stores(&cat).await.unwrap();
        bootstrap_triggers(&cat).await.unwrap();
        sc_module::bootstrap_modules(&cat).await.unwrap();
        Arc::new(cat)
    }

    /// `mail_files` as a local store in a temporary directory, made before the
    /// module loads so its table sync uses it rather than the data directory.
    pub(crate) async fn store(cat: &Catalog) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-module-mail-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut def = sc_module::tables::local_store("mail_files").unwrap();
        def.config
            .insert("path".into(), json!(dir.display().to_string()));
        sc_catalog::save_file_store(cat, &def).await.unwrap();
        sc_catalog::connect_file_store_def(cat, &def).unwrap();
        dir
    }

    pub(crate) async fn query(db: &TestDb, sql: &str) -> Vec<Json> {
        let rows = db
            .client()
            .await
            .unwrap()
            .query(&format!("SELECT to_jsonb(t) FROM ({sql}) t"), &[])
            .await
            .unwrap();
        rows.iter().map(|r| r.get::<_, Json>(0)).collect()
    }

    pub(crate) async fn until(
        db: &TestDb,
        what: &str,
        timeout: Duration,
        sql: &str,
        ready: impl Fn(&[Json]) -> bool,
    ) -> Vec<Json> {
        let start = Instant::now();
        loop {
            let rows = query(db, sql).await;
            if ready(&rows) {
                return rows;
            }
            if start.elapsed() > timeout {
                panic!("timed out waiting for {what}: {rows:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub(crate) fn user_json(user: &sc_auth::User) -> Json {
        json!({ "id": user.id.to_string(), "role": user.role })
    }

    /// Run one of the module's actions as `user`, as a trigger with event None
    /// called by the app would.
    pub(crate) async fn act(
        cat: &Catalog,
        registry: &ActionRegistry,
        action: &str,
        user: &sc_auth::User,
        payload: Json,
    ) -> sc_error::Result<Json> {
        let event = Event::new(EventKind::None)
            .payload(payload)
            .caller(user.role, Some(user_json(user)));
        let config = Attrs::new();
        let action = registry.require(action).unwrap();
        let mut ctx = ActionContext::new(cat, &event, &config, "mail action");
        action.run(&mut ctx).await
    }

    pub(crate) fn as_user<'a>(cat: &'a Catalog, user: &sc_auth::User) -> TableHost<'a> {
        TableHost::new(cat)
            .caused_by(user.role, Some(user_json(user)))
            .with_evaluator(Some(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>))
    }

    async fn subjects(cat: &Catalog, user: &sc_auth::User) -> Vec<String> {
        let rows = as_user(cat, user)
            .call(
                json!({ "op": "select", "table": "mail_messages", "authority": "user",
                          "select": ["subject"], "order": [{ "field": "subject" }] }),
            )
            .await
            .unwrap();
        rows.as_array()
            .unwrap()
            .iter()
            .map(|r| r["subject"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    /// The bundled mail module installed as the Modules tab does it, with the
    /// grant its card asks for and settings that trust `trusted` (the local
    /// Dovecot, the SMTP sink) and the test CA; loaded, with its actions in
    /// the registry beside the built-in ones.
    pub(crate) async fn load_module(
        cat: &Catalog,
        tag: &str,
        trusted: &[&str],
    ) -> (
        Installer,
        Arc<sc_module::ModuleHost>,
        ModuleSet,
        ActionRegistry,
    ) {
        let entry = BundledModules::discover(None);
        let entry = entry.get("imap").expect("the mail module ships");
        let root = temp_root(tag);
        let installer = Installer::new(&root);
        let package = installer
            .install(ModuleSource::Local, &entry.directory.display().to_string())
            .await
            .unwrap_or_else(|e| panic!("installing plugins/imap: {e}"));
        assert_eq!(package.name, MODULE);
        let host = Arc::new(sc_module::ModuleHost::new(&root));
        let mut module = Module::new(MODULE, ModuleSource::Local, MODULE);
        module.permissions = ModulePermissions {
            net: vec![ANY_HOST.to_owned()],
            ..ModulePermissions::closed()
        };
        let ca = std::fs::read_to_string(servers().join("run/certs/ca.crt")).unwrap();
        module
            .configuration
            .insert("trusted_servers".into(), json!(trusted.join("\n")));
        module
            .configuration
            .insert("ca_certificates".into(), json!(ca));
        save_module(cat, &module).await.unwrap();
        let mut registry = sc_core_actions::builtin_actions().unwrap();
        let set = ModuleSet::load(cat, &host, &installer, &surfaces(), &mut registry)
            .await
            .unwrap();
        assert!(set.issues().is_empty(), "{:?}", set.issues());
        assert_eq!(set.get(MODULE).unwrap().service_names(), ["sync"]);
        (installer, host, set, registry)
    }

    #[tokio::test]
    #[ignore = "installs plugins/imap from npm and runs a local Dovecot (plugins/imap/test/servers)"]
    async fn the_mail_module_ingests_a_shared_mailbox_for_the_right_users() {
        skip_without!(have_npm(), "npm is not on the PATH");
        skip_without!(dovecot(&["clean"]), "the test Dovecot could not be started");
        sc_expr::set_isolate_prime(sc_module::prime_v8);
        let mails = temp_root("imap-ingest-mails");
        std::fs::create_dir_all(&mails).unwrap();
        // Waiting when the service starts: two orders and a note for the shared
        // mailbox, and one private mail for ann.
        deliver(&mails, "ingest", "1.eml", &order_mail(1));
        deliver(&mails, "ingest", "2.eml", &order_mail(2));
        deliver(&mails, "ingest", "3.eml", &plain_mail("A note"));
        deliver(&mails, "other", "4.eml", &plain_mail("Private to ann"));

        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        // As the server does at boot: the declared defaults (`mode`, `enabled`, …) are formulas.
        cat.set_formula_evaluator(Arc::new(DenoEvaluator::new()))
            .unwrap();
        let files_dir = store(&cat).await;

        let (installer, host, set, registry) =
            load_module(&cat, "imap-ingest", &["127.0.0.1:11993"]).await;
        let registry = Arc::new(registry);

        // Users: staff (40) may read the shared mailbox, bob (80) may not, ann
        // (80) has a personal account of her own.
        for (role, name) in [(40, "Staff"), (80, "User")] {
            sc_auth::save_role(&cat, &sc_auth::Role::new(role, name))
                .await
                .unwrap();
        }
        let staff = sc_auth::create_user(&cat, "staff@example.com", "pw-staff-1", 40)
            .await
            .unwrap();
        let ann = sc_auth::create_user(&cat, "ann@example.com", "pw-ann-1", 80)
            .await
            .unwrap();
        let bob = sc_auth::create_user(&cat, "bob@example.com", "pw-bob-1", 80)
            .await
            .unwrap();

        // The accounts, as the admin saves them.
        let admin = TableHost::new(&cat)
            .with_evaluator(Some(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>));
        let imap = |user: &str| {
            json!({ "imap_host": "127.0.0.1", "imap_port": 11993, "imap_security": "tls",
                    "imap_user": user, "imap_password": PASSWORD })
        };
        let mut shared = imap("ingest");
        shared.as_object_mut().unwrap().extend(
            json!({ "label": "orders@", "email": "orders@example.com", "min_role": 40, "send_role": 40 })
                .as_object()
                .unwrap()
                .clone(),
        );
        let shared_id = admin
            .call(json!({ "op": "insert", "table": "mail_accounts", "values": shared }))
            .await
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        let mut anns = imap("other");
        anns.as_object_mut().unwrap().extend(
            json!({ "label": "ann@", "email": "ann@example.com", "owner": ann.id.to_string(), "min_role": 1 })
                .as_object()
                .unwrap()
                .clone(),
        );
        admin
            .call(json!({ "op": "insert", "table": "mail_accounts", "values": anns }))
            .await
            .unwrap();

        // M8.8: the order example. The Update trigger fires once per mail, when
        // its import is finished, so the attachment rows are there; the Insert
        // trigger fires first and sees the mail without them.
        let order_from_mail = Trigger::new("order_from_mail", EventKind::Update, "run_js_code")
            .on("mail_messages")
            .only_if(format!(r#"old.body_state === "importing" && account === {shared_id} && subject.startsWith("Order")"#))
            .config(
                "code",
                r#"
const files = await db.mail_attachments.where({ message: row.id }).rows();
const pdf = files.find((f) => f.content_type === "application/pdf");
await db.orders.insert({
  message: row.id,
  customer: row.from_addr,
  title: row.subject,
  pdf: pdf ? pdf.file : null,
  attachments: files.length,
});
"#,
            );
        let mail_log = Trigger::new("mail_log", EventKind::Insert, "run_js_code")
            .on("mail_messages")
            .only_if(format!("account === {shared_id}"))
            .config(
                "code",
                r#"
await db.mail_log.insert({
  message: row.id,
  subject: row.subject,
  attachments_seen: await db.mail_attachments.where({ message: row.id }).count(),
  who: JSON.stringify(user),
});
"#,
            );
        for trigger in [order_from_mail, mail_log] {
            save_trigger(&cat, &registry, &trigger).await.unwrap();
        }
        let dispatcher = Arc::new(
            TriggerDispatcher::new(Arc::clone(&registry))
                .with_evaluator(Arc::new(DenoEvaluator::new())),
        );
        dispatcher.reload(&cat).await.unwrap();
        cat.add_table_events(
            sc_catalog::TRIGGER_EVENTS,
            Arc::clone(&dispatcher) as Arc<dyn TableEvents>,
        )
        .unwrap();

        // --- The service ---------------------------------------------------------
        let supervisor = ServiceSupervisor::new(&cat, &host, Backoff::default()).unwrap();
        supervisor.start(&set).await;
        let messages = "SELECT m.id, m.subject, m.account, m.uid, m.seen, m.body_state, m.import_error, m.from_addr \
                        FROM mail_messages m ORDER BY m.id";
        until(
            &db,
            "the four mails",
            Duration::from_secs(30),
            messages,
            |rows| rows.len() == 4 && rows.iter().all(|r| r["body_state"] == json!("full")),
        )
        .await;
        let accounts = until(&db, "both accounts idle", Duration::from_secs(10),
            "SELECT id, status, last_error, last_sync_at, imap_password FROM mail_accounts ORDER BY id",
            |rows| rows.iter().all(|r| r["status"] == json!("idle")))
        .await;
        for account in &accounts {
            assert!(account["last_sync_at"].is_string());
            let column = account["imap_password"].as_str().unwrap();
            assert!(
                sc_catalog::secrets::is_envelope(column),
                "stored sealed: {column}"
            );
        }

        // --- Ownership (§3) ------------------------------------------------------------
        assert_eq!(
            subjects(&cat, &staff).await,
            ["A note", "Order 1", "Order 2"],
            "staff reads the shared mailbox"
        );
        assert!(
            subjects(&cat, &bob).await.is_empty(),
            "bob is below its min_role"
        );
        assert_eq!(
            subjects(&cat, &ann).await,
            ["Private to ann"],
            "ann reads her own account only"
        );
        // Through REST, as each of them: the same rows, and never a password.
        let tables: Vec<_> = ["mail_accounts", "mail_messages", "mail_attachments"]
            .iter()
            .map(|t| cat.require(t).unwrap())
            .collect();
        let rest =
            RestProvider::project("/api", &tables).with_evaluator(Arc::new(DenoEvaluator::new()));
        for (user, accounts_seen, messages_seen) in [(&staff, 1, 3), (&bob, 0, 0), (&ann, 1, 1)] {
            let listed = rest
                .handle(ApiRequest::get("/api/mail_accounts"), &cat, Some(user))
                .await
                .unwrap();
            assert_eq!(listed.status, 200, "{}", listed.body);
            assert_eq!(
                listed.body.as_array().unwrap().len(),
                accounts_seen,
                "{}",
                listed.body
            );
            for row in listed.body.as_array().unwrap() {
                assert_eq!(row["imap_password"], json!(SECRET_SENTINEL));
            }
            let selected = rest
                .handle(
                    ApiRequest::get("/api/mail_accounts").query("select", "id,imap_password"),
                    &cat,
                    Some(user),
                )
                .await
                .unwrap();
            let messages = rest
                .handle(ApiRequest::get("/api/mail_messages"), &cat, Some(user))
                .await
                .unwrap();
            assert_eq!(
                messages.body.as_array().unwrap().len(),
                messages_seen,
                "{}",
                messages.body
            );
            for output in [&listed.body, &selected.body, &messages.body] {
                assert!(
                    !output.to_string().contains(PASSWORD),
                    "REST carried the password: {output}"
                );
            }
        }

        // --- M8.8: the orders -----------------------------------------------------------
        let orders = query(
            &db,
            "SELECT message, customer, title, pdf, attachments FROM orders ORDER BY title",
        )
        .await;
        assert_eq!(
            orders.len(),
            2,
            "one order per order mail, none for the note or ann's mail: {orders:?}"
        );
        assert_eq!(orders[0]["title"], json!("Order 1"));
        assert_eq!(orders[0]["customer"], json!("customer1@example.com"));
        assert_eq!(orders[0]["attachments"], json!(1));
        let pdf = orders[0]["pdf"].as_str().unwrap().to_owned();
        let store = cat.require_file_store("mail_files").unwrap();
        assert_eq!(
            store.read(&pdf).await.unwrap().as_ref(),
            b"%PDF-1.4 order 1"
        );
        let log = query(
            &db,
            "SELECT subject, attachments_seen, who FROM mail_log ORDER BY subject",
        )
        .await;
        assert_eq!(
            log.len(),
            3,
            "the Insert trigger fires once per shared mail: {log:?}"
        );
        assert!(
            log.iter().all(|r| r["attachments_seen"] == json!(0)),
            "at insert, no attachment rows yet: {log:?}"
        );
        assert!(
            log[0]["who"].as_str().unwrap().contains(MODULE),
            "written as the module: {log:?}"
        );

        // --- New mail during IDLE, from inside the worker -----------------------------------
        tokio::time::sleep(Duration::from_secs(1)).await;
        let sent = Instant::now();
        deliver(&mails, "ingest", "5.eml", &order_mail(3));
        until(
            &db,
            "order 3",
            Duration::from_secs(5),
            "SELECT title FROM orders",
            |rows| rows.iter().any(|r| r["title"] == json!("Order 3")),
        )
        .await;
        println!(
            "order 3 became an order {} ms after delivery",
            sent.elapsed().as_millis()
        );

        // --- The actions (M8.6) --------------------------------------------------------------
        let rows = query(&db, messages).await;
        let order1 = rows
            .iter()
            .find(|r| r["subject"] == json!("Order 1"))
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        let private = rows
            .iter()
            .find(|r| r["subject"] == json!("Private to ann"))
            .unwrap()["id"]
            .as_i64()
            .unwrap();

        // mail_set_flags: bob and staff on ann's mail find nothing; staff on the
        // shared mail sets it on the server and on the row.
        let refused = act(
            &cat,
            &registry,
            "mail_set_flags",
            &bob,
            json!({ "message_ids": [order1], "seen": true }),
        )
        .await
        .unwrap();
        assert_eq!(refused["missing"], json!([order1]), "{refused}");
        let refused = act(
            &cat,
            &registry,
            "mail_set_flags",
            &staff,
            json!({ "message_ids": [private], "seen": true }),
        )
        .await
        .unwrap();
        assert_eq!(refused["missing"], json!([private]), "{refused}");
        let done = act(
            &cat,
            &registry,
            "mail_set_flags",
            &staff,
            json!({ "message_ids": [order1], "seen": true }),
        )
        .await
        .unwrap();
        assert_eq!(done["updated"], json!([order1]), "{done}");
        let seen = |rows: &[Json]| {
            rows.iter()
                .any(|r| r["id"] == json!(order1) && r["seen"] == json!(true))
        };
        assert!(seen(&query(&db, messages).await));
        // On the server too: the row made stale by hand comes back from the
        // server's flags at the next sync.
        db.client()
            .await
            .unwrap()
            .execute(
                &format!("UPDATE mail_messages SET seen = false WHERE id = {order1}"),
                &[],
            )
            .await
            .unwrap();
        let synced = act(
            &cat,
            &registry,
            "mail_sync_now",
            &staff,
            json!({ "account_id": shared_id, "wait": true }),
        )
        .await
        .unwrap();
        assert_eq!(synced["queued"], json!(true), "{synced}");
        until(
            &db,
            "the server's \\Seen",
            Duration::from_secs(10),
            messages,
            seen,
        )
        .await;
        let refused = act(
            &cat,
            &registry,
            "mail_sync_now",
            &bob,
            json!({ "account_id": shared_id }),
        )
        .await;
        assert!(
            refused.is_err_and(|e| e.to_string().contains("not found")),
            "bob may not sync the shared account"
        );

        // mail_attachment: stored already, so a no-op; stored again from the
        // server when its file is gone.
        let attachment = query(
            &db,
            &format!("SELECT id, file FROM mail_attachments WHERE message = {order1}"),
        )
        .await;
        let att = attachment[0]["id"].as_i64().unwrap();
        let again = act(
            &cat,
            &registry,
            "mail_attachment",
            &staff,
            json!({ "attachment_id": att }),
        )
        .await
        .unwrap();
        assert_eq!(again, json!({ "file": pdf, "stored": false }));
        let refused = act(
            &cat,
            &registry,
            "mail_attachment",
            &bob,
            json!({ "attachment_id": att }),
        )
        .await;
        assert!(
            refused.is_err(),
            "bob may not open the shared mail's attachment"
        );
        db.client()
            .await
            .unwrap()
            .execute(
                &format!("UPDATE mail_attachments SET file = NULL WHERE id = {att}"),
                &[],
            )
            .await
            .unwrap();
        let fetched = act(
            &cat,
            &registry,
            "mail_attachment",
            &staff,
            json!({ "attachment_id": att }),
        )
        .await
        .unwrap();
        assert_eq!(fetched["stored"], json!(true), "{fetched}");
        let path = fetched["file"].as_str().unwrap();
        assert_eq!(
            store.read(path).await.unwrap().as_ref(),
            b"%PDF-1.4 order 1"
        );

        // mail_test_account: the saved password, without the caller seeing it;
        // then the rate limit.
        let tested = act(
            &cat,
            &registry,
            "mail_test_account",
            &staff,
            json!({ "account_id": shared_id, "imap_password": SECRET_SENTINEL }),
        )
        .await
        .unwrap();
        assert_eq!(tested["imap"], json!(true), "{tested}");
        let wrong = act(
            &cat,
            &registry,
            "mail_test_account",
            &ann,
            json!({ "imap_host": "127.0.0.1", "imap_port": 11993, "imap_security": "tls",
                    "imap_user": "other", "imap_password": "wrong-Pässwort" }),
        )
        .await
        .unwrap();
        assert_eq!(wrong["imap"], json!(false));
        assert!(
            !wrong["error"].as_str().unwrap().contains("wrong-Pässwort"),
            "{wrong}"
        );
        let refused = act(
            &cat,
            &registry,
            "mail_test_account",
            &bob,
            json!({ "account_id": shared_id }),
        )
        .await;
        assert!(refused.is_err_and(|e| e.to_string().contains("not found")));
        let mut limited = None;
        for _ in 0..6 {
            if let Err(e) = act(
                &cat,
                &registry,
                "mail_test_account",
                &staff,
                json!({ "account_id": shared_id }),
            )
            .await
            {
                limited = Some(e.to_string());
                break;
            }
        }
        assert!(
            limited.is_some_and(|e| e.contains("at most 5")),
            "the per-user limit applies"
        );

        // Disabled: the service stops the account and says so.
        admin
            .call(json!({ "op": "update", "table": "mail_accounts", "where": { "id": shared_id }, "values": { "enabled": false } }))
            .await
            .unwrap();
        until(
            &db,
            "disabled",
            Duration::from_secs(10),
            &format!("SELECT status FROM mail_accounts WHERE id = {shared_id}"),
            |rows| rows[0]["status"] == json!("disabled"),
        )
        .await;

        supervisor.stop_all().await;
        host.shutdown().await;
        let _ = std::fs::remove_dir_all(installer.root());
        let _ = std::fs::remove_dir_all(files_dir);
        let _ = std::fs::remove_dir_all(mails);
    }
}
