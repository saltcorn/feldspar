//! LLM credits from JavaScript modules (TODO-payments P6.4, design C4): a
//! fixture module's action grants credits through `credits`, keyed so that a
//! replayed grant does nothing, reads the balance, sets the low-balance
//! threshold, and has no way to spend them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
mod deno {
    use std::sync::Arc;

    use rust_decimal::Decimal;
    use sc_action::{ActionContext, ActionRegistry, Event, EventKind};
    use sc_auth::{Role, create_user, credit_balance, list_credit_buckets, save_role};
    use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_table_meta};
    use sc_db::DatabaseDriver;
    use sc_db_postgres::PgDriver;
    use sc_module::{Module, ModulePermissions, ModuleSet, ModuleSource, save_module};
    use sc_test_harness::TestDb;
    use sc_types::Attrs;
    use serde_json::json;

    use crate::common::{have_npm, installed, surfaces};

    #[tokio::test]
    async fn a_module_grants_credits_once_per_key_and_cannot_spend_them() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>)
            .await
            .unwrap();
        sc_auth::bootstrap(&cat).await.unwrap();
        bootstrap_table_meta(&cat).await.unwrap();
        bootstrap_field_meta(&cat).await.unwrap();
        sc_module::bootstrap_modules(&cat).await.unwrap();
        save_role(&cat, &Role::new(80, "Member")).await.unwrap();
        let user = create_user(&cat, "c@example.com", "pw", 80).await.unwrap();

        let (installer, host, names) = installed("module-credits", &["credits-module"]).await;
        let name = names[0].clone();
        // No permission of its own: granting credits needs none.
        let mut module = Module::new(&name, ModuleSource::Local, &name);
        module.permissions = ModulePermissions::closed();
        save_module(&cat, &module).await.unwrap();
        let mut registry = ActionRegistry::new();
        let set = ModuleSet::load(&cat, &host, &installer, &surfaces(), &mut registry)
            .await
            .unwrap();
        assert!(set.issues().is_empty(), "{:?}", set.issues());

        let event = Event::new(EventKind::None).payload(json!({ "user": user.id.to_string() }));
        let config = Attrs::new();
        let action = registry.require("grant_credits").unwrap();
        let mut ctx = ActionContext::new(&cat, &event, &config, "grant credits");
        let result = action.run(&mut ctx).await.unwrap();

        // Numbers, as JavaScript reads them.
        let num = |v: &serde_json::Value| v.as_f64().unwrap_or(f64::NAN);
        assert_eq!(num(&result["granted"]["ok"]["amount"]), 100.0, "{result}");
        assert_eq!(result["granted"]["ok"]["replayed"], json!(false));
        assert_eq!(
            result["replayed"]["ok"]["replayed"],
            json!(true),
            "{result}"
        );
        assert_eq!(
            result["replayed"]["ok"]["bucket"],
            result["granted"]["ok"]["bucket"]
        );
        assert_eq!(num(&result["topUp"]["ok"]["balance"]), 102.5, "{result}");
        let keyless = result["keyless"]["refused"].as_str().unwrap();
        assert!(keyless.contains("needs `key`"), "{keyless}");
        assert_eq!(
            num(&result["threshold"]["ok"]["low_balance_threshold"]),
            10.0
        );
        assert_eq!(num(&result["balance"]["balance"]), 102.5);
        assert_eq!(
            result["debit"],
            json!("undefined"),
            "no way to spend credits"
        );

        // The buckets are the module's, under its own name.
        let balance = credit_balance(&cat, user.id).await.unwrap();
        assert_eq!(balance.balance, Decimal::new(1025, 1));
        assert_eq!(balance.low_balance_threshold, Some(Decimal::from(10)));
        let buckets = list_credit_buckets(&cat, user.id).await.unwrap();
        assert_eq!(buckets.len(), 2);
        assert!(buckets.iter().all(|b| b.source == name), "{buckets:?}");
        assert!(
            buckets
                .iter()
                .any(|b| b.kind == "allowance" && b.expires_at.is_some())
        );

        host.shutdown().await;
    }
}
