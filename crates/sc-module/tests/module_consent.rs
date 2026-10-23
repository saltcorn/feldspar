//! `oauth.begin(…)` in a module's action (TODO-imap M10.3): the flow is kept
//! for the browser to take up, for the caller only, and a request that would
//! put a token anywhere but the module's own Secret fields is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
mod deno {
    use std::sync::Arc;

    use sc_action::{ActionContext, ActionRegistry, Event, EventKind};
    use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_table_meta};
    use sc_db::DatabaseDriver;
    use sc_db_postgres::PgDriver;
    use sc_module::consent::{CONSENT_PREFIX, flows};
    use sc_module::{Module, ModuleSet, ModuleSource, save_module};
    use sc_test_harness::TestDb;
    use sc_types::Attrs;
    use serde_json::{Value, json};

    use crate::common::{have_npm, installed, surfaces};

    const OWNER: &str = "00000000-0000-0000-0000-000000000007";

    async fn catalog(db: &TestDb) -> Arc<Catalog> {
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>)
            .await
            .unwrap();
        sc_auth::bootstrap(&cat).await.unwrap();
        sc_auth::bootstrap_roles(&cat).await.unwrap();
        bootstrap_table_meta(&cat).await.unwrap();
        bootstrap_field_meta(&cat).await.unwrap();
        sc_catalog::bootstrap_file_stores(&cat).await.unwrap();
        sc_module::bootstrap_modules(&cat).await.unwrap();
        Arc::new(cat)
    }

    async fn connect(
        cat: &Catalog,
        registry: &ActionRegistry,
        user: Option<&str>,
        payload: Value,
    ) -> Value {
        let mut event = Event::new(EventKind::None).payload(payload);
        if let Some(id) = user {
            event = event.caller(80, Some(json!({ "id": id, "role": 80 })));
        }
        let config = Attrs::new();
        let action = registry.require("connect").unwrap();
        let mut ctx = ActionContext::new(cat, &event, &config, "connect");
        action.run(&mut ctx).await.unwrap()
    }

    #[tokio::test]
    async fn an_action_begins_a_consent_flow_for_its_caller_and_its_own_secret_fields() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        let (installer, host, names) = installed("module-consent", &["oauth-module"]).await;
        save_module(
            &cat,
            &Module::new(&names[0], ModuleSource::Local, &names[0]),
        )
        .await
        .unwrap();
        let mut registry = ActionRegistry::new();
        let set = ModuleSet::load(&cat, &host, &installer, &surfaces(), &mut registry)
            .await
            .unwrap();
        assert!(set.issues().is_empty(), "{:?}", set.issues());
        // As the server does after every load: which module owns which table.
        cat.set_module_tables(set.module_tables()).unwrap();
        db.client()
            .await
            .unwrap()
            .batch_execute(&format!(
                "INSERT INTO oa_accounts (owner, email) VALUES ('{OWNER}', 'ada@outlook.com')"
            ))
            .await
            .unwrap();

        let begun = connect(&cat, &registry, Some(OWNER), json!({ "account_id": 1 })).await;
        let url = begun["url"].as_str().unwrap_or_else(|| panic!("{begun}"));
        let id = url
            .strip_prefix(&format!("{CONSENT_PREFIX}/"))
            .unwrap_or_else(|| panic!("{url}"));
        assert_eq!(begun["expires_in"], json!(600));
        let flow = flows().take(id).expect("the flow is kept");
        assert_eq!(flow.module, names[0]);
        assert_eq!(flow.user, OWNER, "only the caller may finish it");
        assert_eq!(
            flow.authorization_url,
            "https://login.example.com/authorize"
        );
        assert_eq!(flow.client_secret, "csecret");
        assert_eq!(flow.return_to, "/settings/mail");
        assert!(
            flow.params
                .contains(&("login_hint".into(), "ada@outlook.com".into()))
        );
        assert_eq!(flow.store.table, "oa_accounts");
        assert_eq!(flow.store.id, "1");
        assert_eq!(flow.store.refresh_token.as_deref(), Some("refresh_token"));
        assert_eq!(flow.store.set["auth"], json!("oauth2"));
        assert!(flows().take(id).is_none(), "taken once");

        // Refusals, each naming what is wrong.
        let refused = |begun: Value, says: &str| {
            let error = begun["error"].as_str().unwrap_or_else(|| panic!("{begun}"));
            assert!(error.contains(says), "{error} should say {says}");
        };
        refused(
            connect(
                &cat,
                &registry,
                None,
                json!({ "account_id": 1, "anyone": true }),
            )
            .await,
            "only an action called by a signed-in user",
        );
        for (request, says) in [
            (
                json!({ "store": { "table": "_sc_users", "id": 1, "fields": { "access_token": "password" } } }),
                "is not a table",
            ),
            (
                json!({ "store": { "table": "oa_accounts", "id": 1, "fields": { "access_token": "plain_token" } } }),
                "is not a Secret field",
            ),
            (
                json!({ "store": { "table": "oa_accounts", "id": 1, "fields": { "access_token": "access_token" }, "set": { "owner": "x", "access_token": "y" } } }),
                "not `access_token`",
            ),
            (
                json!({ "token_url": "http://login.example.com/token" }),
                "https://",
            ),
            (
                json!({ "return_to": "https://evil.example/" }),
                "a path on the application's host",
            ),
            (
                json!({ "params": { "redirect_uri": "https://evil.example/" } }),
                "may not set `redirect_uri`",
            ),
        ] {
            refused(
                connect(
                    &cat,
                    &registry,
                    Some(OWNER),
                    json!({ "account_id": 1, "request": request }),
                )
                .await,
                says,
            );
        }
        host.shutdown().await;
    }
}
