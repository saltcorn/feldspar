//! The bundled mail module in mirror mode, on the real host (TODO-imap M9.5).
//!
//! Two users, ann and bob, each with two personal mirror accounts on the local
//! Dovecot (`plugins/imap/test/servers/dovecot.sh`), synced into Postgres by the
//! module's service on this server's Deno worker. Then:
//!
//! - **isolation**: neither user ever sees the other's mail or accounts —
//!   through the row layer, REST, GraphQL, or a live subscription to
//!   `mail_messages` and `mail_accounts` while new mail arrives for both — and
//!   no password is in any of those answers;
//! - **the client actions** run in the worker as their caller: `mail_send`
//!   replies through the SMTP sink (`test/servers/smtp-sink.mjs`) with an
//!   application file attached (`callerFiles`), its copy lands in Sent and the
//!   question is marked answered; `mail_delete` and `mail_set_flags` refuse the
//!   other user's mail.
//!
//! Mirror sync itself — newest first, `body_days`, backfill, expunges,
//! UIDVALIDITY, folders, drafts, the connection budget — is tested at speed by
//! `npm test` in `plugins/imap` (`test/mirror.test.mjs`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
mod deno {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use sc_action::ActionRegistry;
    use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method, RestProvider};
    use sc_catalog::{Catalog, LIVE_EVENTS, TableEvents};
    use sc_expr::{CodeHost, DenoEvaluator, JsEvaluator};
    use sc_live::{
        ChangeAccess, ConnectionInfo, Delivery, LikeRules, LiveHub, LiveLimits, RowFilter,
    };
    use sc_module::{Backoff, ServiceSupervisor};
    use sc_stream::{
        RunningStream, Stream, StreamConfig, StreamProvider, StreamRegistry, StreamSupervisor,
        TableChanges,
    };
    use sc_test_harness::TestDb;
    use sc_types::SECRET_SENTINEL;
    use serde_json::{Value as Json, json};
    use tokio::sync::mpsc;

    use crate::common::{have_npm, temp_root};
    use crate::imap_ingest::deno::{
        PASSWORD, as_user, catalog, deliver, dovecot, load_module, query, servers, store, until,
    };

    fn plain_mail(subject: &str) -> String {
        format!(
            "From: someone@example.com\nTo: x@example.com\nSubject: {subject}\n\
             Message-ID: <{}@example.com>\nDate: Wed, 21 Oct 2026 10:00:00 +0000\n\n\
             Just text.\n",
            subject.replace(' ', "-")
        )
    }

    /// The SMTP sink, as a process: each mail it accepts is written to `dir`.
    struct Sink(Child);

    impl Sink {
        fn start(dir: &Path) -> Option<Sink> {
            let script = servers().join("smtp-sink.mjs");
            let child = Command::new("node")
                .arg(script)
                .arg(dir)
                .arg("10465")
                .arg("10587")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let sink = Sink(child);
            let start = Instant::now();
            while std::net::TcpStream::connect("127.0.0.1:10465").is_err() {
                if start.elapsed() > Duration::from_secs(10) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Some(sink)
        }
    }

    impl Drop for Sink {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    async fn act(
        cat: &Catalog,
        registry: &ActionRegistry,
        action: &str,
        user: &sc_auth::User,
        payload: Json,
    ) -> sc_error::Result<Json> {
        crate::imap_ingest::deno::act(cat, registry, action, user, payload).await
    }

    /// One user's live page: subscribed to a `table_changes` stream as them.
    struct Page {
        _connection: sc_live::Connection,
        queue: mpsc::Receiver<Delivery>,
        _attached: sc_live::Attached,
    }

    impl Page {
        async fn open(
            hub: &Arc<LiveHub>,
            cat: &Arc<Catalog>,
            stream: &Arc<RunningStream>,
            table: &str,
            user: &sc_auth::User,
        ) -> Page {
            let (connection, queue) = hub
                .connect(ConnectionInfo {
                    app: "mail".to_owned(),
                    user: Some(user.id),
                    session: None,
                })
                .expect("connects");
            connection.set_user(Some(user.clone()));
            let table = cat.require(table).unwrap();
            let access = ChangeAccess {
                catalog: Arc::clone(cat),
                evaluator: Some(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>),
                filter: RowFilter::parse(&table, None, LikeRules::POSTGRES).unwrap(),
            };
            let (attached, _replay) = connection.attach_changes(stream, access).await;
            Page {
                _connection: connection,
                queue,
                _attached: attached,
            }
        }

        /// Every frame sent until none has come for `wait`.
        async fn drain(&mut self, wait: Duration) -> Vec<Json> {
            let mut frames = Vec::new();
            while let Ok(Some(delivery)) = tokio::time::timeout(wait, self.queue.recv()).await {
                frames.push(delivery.envelope.value.clone());
            }
            frames
        }
    }

    async fn stream(
        supervisor: &Arc<StreamSupervisor>,
        name: &str,
        table: &str,
    ) -> Arc<RunningStream> {
        let mut stream = Stream::new(name, "table_changes");
        stream.configuration = json!({ "table": table }).as_object().cloned().unwrap();
        supervisor.start(stream).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "installs plugins/imap from npm and runs a local Dovecot and SMTP sink (plugins/imap/test/servers)"]
    async fn two_users_mirror_accounts_never_show_each_others_mail() {
        skip_without!(have_npm(), "npm is not on the PATH");
        skip_without!(dovecot(&["clean"]), "the test Dovecot could not be started");
        let mails = temp_root("imap-mirror-mails");
        std::fs::create_dir_all(&mails).unwrap();
        let sink_dir = temp_root("imap-mirror-sink");
        let Some(sink) = Sink::start(&sink_dir) else {
            eprintln!("skipping: the SMTP sink could not be started (npm install in plugins/imap)");
            return;
        };
        sc_expr::set_isolate_prime(sc_module::prime_v8);
        for (user, subject) in [
            ("ann1", "Ann one first"),
            ("ann2", "Ann two first"),
            ("bob1", "Bob one first"),
            ("bob2", "Bob two first"),
        ] {
            deliver(&mails, user, &format!("{user}.eml"), &plain_mail(subject));
        }

        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        cat.set_formula_evaluator(Arc::new(DenoEvaluator::new()))
            .unwrap();
        let files_dir = store(&cat).await;
        // An application file store ann can read from, for an attachment.
        let uploads_dir =
            std::env::temp_dir().join(format!("sc-module-mail-uploads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&uploads_dir);
        let mut uploads = sc_module::tables::local_store("uploads").unwrap();
        uploads
            .config
            .insert("path".into(), json!(uploads_dir.display().to_string()));
        sc_catalog::save_file_store(&cat, &uploads).await.unwrap();
        sc_catalog::connect_file_store_def(&cat, &uploads).unwrap();
        cat.require_file_store("uploads")
            .unwrap()
            .write("quotes/q-17.pdf", b"%PDF quote 17".to_vec().into())
            .await
            .unwrap();

        let (installer, host, set, registry) =
            load_module(&cat, "imap-mirror", &["127.0.0.1:11993", "127.0.0.1:10465"]).await;
        let registry = Arc::new(registry);

        sc_auth::save_role(&cat, &sc_auth::Role::new(80, "User"))
            .await
            .unwrap();
        let ann = sc_auth::create_user(&cat, "ann@example.com", "pw-ann-1", 80)
            .await
            .unwrap();
        let bob = sc_auth::create_user(&cat, "bob@example.com", "pw-bob-1", 80)
            .await
            .unwrap();

        // The live hub, before any account exists, so the subscribers see every write.
        let mut streams = StreamRegistry::new();
        streams
            .register(Arc::new(TableChanges) as Arc<dyn StreamProvider>)
            .unwrap();
        let supervisor = Arc::new(StreamSupervisor::new(
            Arc::new(streams),
            StreamConfig::default(),
        ));
        let messages_live = stream(&supervisor, "mail_live", "mail_messages").await;
        let accounts_live = stream(&supervisor, "accounts_live", "mail_accounts").await;
        let hub = LiveHub::new(LiveLimits::default());
        hub.set_streams(Arc::clone(&supervisor));
        cat.add_table_events(LIVE_EVENTS, Arc::clone(&hub) as Arc<dyn TableEvents>)
            .unwrap();
        let mut ann_messages = Page::open(&hub, &cat, &messages_live, "mail_messages", &ann).await;
        let mut bob_messages = Page::open(&hub, &cat, &messages_live, "mail_messages", &bob).await;
        let mut ann_accounts = Page::open(&hub, &cat, &accounts_live, "mail_accounts", &ann).await;
        let mut bob_accounts = Page::open(&hub, &cat, &accounts_live, "mail_accounts", &bob).await;

        // Two personal mirror accounts each, saved by each user as themselves.
        let mut ids = std::collections::BTreeMap::new();
        for (user, login) in [
            (&ann, "ann1"),
            (&ann, "ann2"),
            (&bob, "bob1"),
            (&bob, "bob2"),
        ] {
            let values = json!({
                "owner": user.id.to_string(), "label": login, "email": format!("{login}@example.com"),
                "display_name": login, "mode": "mirror", "min_role": 1,
                "imap_host": "127.0.0.1", "imap_port": 11993, "imap_security": "tls",
                "imap_user": login, "imap_password": PASSWORD,
                "smtp_host": "127.0.0.1", "smtp_port": 10465, "smtp_security": "tls",
                "smtp_user": "sender", "smtp_password": PASSWORD,
            });
            let row = as_user(&cat, user)
                .call(json!({ "op": "insert", "table": "mail_accounts", "authority": "user", "values": values }))
                .await
                .unwrap_or_else(|e| panic!("{login}: {e}"));
            ids.insert(login, row["id"].as_i64().unwrap());
        }
        let ann_accounts_ids: BTreeSet<i64> = [ids["ann1"], ids["ann2"]].into();
        let bob_accounts_ids: BTreeSet<i64> = [ids["bob1"], ids["bob2"]].into();

        let services = ServiceSupervisor::new(&cat, &host, Backoff::default()).unwrap();
        services.start(&set).await;
        let messages = "SELECT id, subject, account, folder, uid, seen, answered, body_state FROM mail_messages ORDER BY id";
        until(
            &db,
            "the four mails",
            Duration::from_secs(30),
            messages,
            |rows| rows.len() == 4 && rows.iter().all(|r| r["body_state"] == json!("full")),
        )
        .await;
        until(
            &db,
            "four accounts idle",
            Duration::from_secs(10),
            "SELECT status FROM mail_accounts",
            |rows| rows.iter().all(|r| r["status"] == json!("idle")),
        )
        .await;

        // New mail for both while they watch.
        tokio::time::sleep(Duration::from_secs(1)).await;
        deliver(&mails, "ann2", "ann-new.eml", &plain_mail("Ann two second"));
        deliver(&mails, "bob1", "bob-new.eml", &plain_mail("Bob one second"));
        until(
            &db,
            "the new mail",
            Duration::from_secs(10),
            messages,
            |rows| rows.len() == 6 && rows.iter().all(|r| r["body_state"] == json!("full")),
        )
        .await;

        // --- Live: each page is sent only its user's rows ------------------------------
        let account_of = |rows: &[Json], id: i64| {
            rows.iter()
                .find(|r| r["id"] == json!(id))
                .map(|r| r["account"].as_i64().unwrap())
        };
        let all = query(&db, messages).await;
        for (who, page, mine, subject) in [
            (
                "ann",
                &mut ann_messages,
                &ann_accounts_ids,
                "Ann two second",
            ),
            (
                "bob",
                &mut bob_messages,
                &bob_accounts_ids,
                "Bob one second",
            ),
        ] {
            let frames = page.drain(Duration::from_millis(1500)).await;
            assert!(!frames.is_empty(), "{who} was sent their own mail");
            for frame in &frames {
                let key = frame["key"].as_i64().unwrap();
                let account = account_of(&all, key)
                    .unwrap_or_else(|| frame["row"]["account"].as_i64().unwrap());
                assert!(
                    mine.contains(&account),
                    "{who} was sent message {key} of account {account}: {frame}"
                );
                assert!(!frame.to_string().contains(PASSWORD));
            }
            assert!(
                frames.iter().any(|f| f["row"]["subject"] == json!(subject)
                    && f["row"]["body_state"] == json!("full")),
                "{who} saw {subject:?} arrive: {frames:?}"
            );
        }
        for (who, page, mine) in [
            ("ann", &mut ann_accounts, &ann_accounts_ids),
            ("bob", &mut bob_accounts, &bob_accounts_ids),
        ] {
            let frames = page.drain(Duration::from_millis(500)).await;
            assert!(!frames.is_empty(), "{who} was sent their accounts' status");
            for frame in &frames {
                let key = frame["key"].as_i64().unwrap();
                assert!(mine.contains(&key), "{who} was sent account {key}: {frame}");
                let text = frame.to_string();
                assert!(
                    !text.contains(PASSWORD),
                    "{who}'s live frame carries a password: {text}"
                );
            }
        }

        // --- The row layer, REST and GraphQL -------------------------------------------------
        let tables: Vec<_> = [
            "mail_accounts",
            "mail_folders",
            "mail_messages",
            "mail_attachments",
        ]
        .iter()
        .map(|t| cat.require(t).unwrap())
        .collect();
        let rest =
            RestProvider::project("/api", &tables).with_evaluator(Arc::new(DenoEvaluator::new()));
        let graphql = GraphqlProvider::project("/graphql", &tables).unwrap();
        for (who, user, mine, subjects) in [
            (
                "ann",
                &ann,
                &ann_accounts_ids,
                ["Ann one first", "Ann two first", "Ann two second"],
            ),
            (
                "bob",
                &bob,
                &bob_accounts_ids,
                ["Bob one first", "Bob one second", "Bob two first"],
            ),
        ] {
            let rows = as_user(&cat, user)
                .call(
                    json!({ "op": "select", "table": "mail_messages", "authority": "user",
                              "select": ["subject"], "order": [{ "field": "subject" }] }),
                )
                .await
                .unwrap();
            let seen: Vec<&str> = rows
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["subject"].as_str().unwrap())
                .collect();
            assert_eq!(seen, subjects, "{who} through the row layer");

            let listed = rest
                .handle(ApiRequest::get("/api/mail_messages"), &cat, Some(user))
                .await
                .unwrap();
            assert_eq!(listed.status, 200, "{}", listed.body);
            let accounts: BTreeSet<i64> = listed
                .body
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["account"].as_i64().unwrap())
                .collect();
            assert_eq!(
                listed.body.as_array().unwrap().len(),
                3,
                "{who}: {}",
                listed.body
            );
            assert!(accounts.is_subset(mine), "{who} through REST: {accounts:?}");
            let own = rest
                .handle(ApiRequest::get("/api/mail_accounts"), &cat, Some(user))
                .await
                .unwrap();
            let own_ids: BTreeSet<i64> = own
                .body
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect();
            assert_eq!(&own_ids, mine, "{who}'s accounts through REST");
            for row in own.body.as_array().unwrap() {
                assert_eq!(row["imap_password"], json!(SECRET_SENTINEL));
                assert_eq!(row["smtp_password"], json!(SECRET_SENTINEL));
            }
            let folders = rest
                .handle(ApiRequest::get("/api/mail_folders"), &cat, Some(user))
                .await
                .unwrap();
            assert!(
                folders
                    .body
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|f| mine.contains(&f["account"].as_i64().unwrap()))
            );

            let answer = graphql
                .handle(
                    ApiRequest::new(Method::Post, "/graphql").body(json!({
                        "query": "{ mail_messages { id subject } mail_accounts { id label imap_password } mail_folders { id } }"
                    })),
                    &cat,
                    Some(user),
                )
                .await
                .unwrap();
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert!(answer.body.get("errors").is_none(), "{}", answer.body);
            let data = &answer.body["data"];
            // A key to `mail_accounts` cannot be followed here (its rule decides row by
            // row), so each message's account is looked up.
            let got: BTreeSet<i64> = data["mail_messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| account_of(&all, r["id"].as_i64().unwrap()).unwrap())
                .collect();
            assert_eq!(
                data["mail_messages"].as_array().unwrap().len(),
                3,
                "{who} through GraphQL: {data}"
            );
            assert!(got.is_subset(mine), "{who} through GraphQL: {data}");
            let got: BTreeSet<i64> = data["mail_accounts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect();
            assert_eq!(&got, mine, "{who}'s accounts through GraphQL: {data}");
            let theirs: BTreeSet<i64> = data["mail_folders"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect();
            let expected: BTreeSet<i64> = folders
                .body
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["id"].as_i64().unwrap())
                .collect();
            assert_eq!(theirs, expected, "{who}'s folders through GraphQL: {data}");
            for output in [&listed.body, &own.body, &answer.body] {
                assert!(
                    !output.to_string().contains(PASSWORD),
                    "{who}: a password went out: {output}"
                );
            }
        }

        // --- The client actions, in the worker, as their caller ---------------------------------
        let rows = query(&db, messages).await;
        let question = rows
            .iter()
            .find(|r| r["subject"] == json!("Ann one first"))
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        let bobs = rows
            .iter()
            .find(|r| r["subject"] == json!("Bob one first"))
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        let sent = act(
            &cat,
            &registry,
            "mail_send",
            &ann,
            json!({
                "account_id": ids["ann1"],
                "to": "Shop <shop@example.com>",
                "subject": "Re: Ann one first",
                "text": "The answer.",
                "in_reply_to": question,
                "attachments": [{ "store": "uploads", "path": "quotes/q-17.pdf" }],
            }),
        )
        .await
        .unwrap();
        assert_eq!(sent["saved"], json!(true), "{sent}");
        let log = std::fs::read_to_string(sink_dir.join("log.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        let source = std::fs::read_to_string(sink_dir.join("1.eml")).unwrap();
        assert!(
            source.contains("In-Reply-To: <Ann-one-first@example.com>"),
            "{source}"
        );
        assert!(source.contains("q-17.pdf"), "{source}");
        let copy = until(&db, "the Sent copy", Duration::from_secs(10),
            &format!("SELECT m.subject, m.in_reply_to, m.thread_key, f.role FROM mail_messages m JOIN mail_folders f ON f.id = m.folder WHERE m.id = {}", sent["id"]),
            |rows| rows.len() == 1)
        .await;
        assert_eq!(copy[0]["role"], json!("sent"));
        assert_eq!(copy[0]["thread_key"], json!("<Ann-one-first@example.com>"));
        let answered = query(
            &db,
            &format!("SELECT answered FROM mail_messages WHERE id = {question}"),
        )
        .await;
        assert_eq!(answered[0]["answered"], json!(true));

        // The other user's mail and accounts are "not found" to each action.
        let refused = act(
            &cat,
            &registry,
            "mail_send",
            &bob,
            json!({ "account_id": ids["ann1"], "to": "x@example.com" }),
        )
        .await;
        assert!(refused.is_err_and(|e| e.to_string().contains("not found")));
        let refused = act(
            &cat,
            &registry,
            "mail_set_flags",
            &ann,
            json!({ "message_ids": [bobs], "seen": true }),
        )
        .await
        .unwrap();
        assert_eq!(refused["missing"], json!([bobs]));
        let refused = act(
            &cat,
            &registry,
            "mail_delete",
            &ann,
            json!({ "message_ids": [bobs] }),
        )
        .await
        .unwrap();
        assert_eq!(refused["missing"], json!([bobs]));
        let refused = act(
            &cat,
            &registry,
            "mail_fetch_body",
            &ann,
            json!({ "message_id": bobs }),
        )
        .await;
        assert!(refused.is_err_and(|e| e.to_string().contains("not found")));
        let refused = act(
            &cat,
            &registry,
            "mail_create_folder",
            &bob,
            json!({ "account_id": ids["ann2"], "name": "Mine" }),
        )
        .await;
        assert!(refused.is_err_and(|e| e.to_string().contains("not found")));
        // And ann's own delete goes to her Trash.
        let trashed = act(
            &cat,
            &registry,
            "mail_delete",
            &ann,
            json!({ "message_ids": [question] }),
        )
        .await
        .unwrap();
        assert_eq!(trashed["trashed"], json!([question]), "{trashed}");
        let moved = query(&db, &format!("SELECT f.role FROM mail_messages m JOIN mail_folders f ON f.id = m.folder WHERE m.id = {question}")).await;
        assert_eq!(moved[0]["role"], json!("trash"));

        services.stop_all().await;
        host.shutdown().await;
        drop(sink);
        let _ = std::fs::remove_dir_all(installer.root());
        let _ = std::fs::remove_dir_all(files_dir);
        let _ = std::fs::remove_dir_all(uploads_dir);
        let _ = std::fs::remove_dir_all(mails);
        let _ = std::fs::remove_dir_all(sink_dir);
    }
}
