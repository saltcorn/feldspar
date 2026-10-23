//! Role grants from JavaScript modules (TODO-payments P3.3, design C4): a
//! fixture module's action grants a role through `roles`, extends it by the
//! same reference, and is refused a role above the ceiling an admin allowed it
//! — and the admin role whatever the ceiling — and refused everything when it
//! was allowed none.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
mod deno {
    use std::sync::Arc;

    use sc_action::{ActionContext, ActionRegistry, Event, EventKind};
    use sc_auth::{Role, create_user, find_role_grant, load_user, save_role};
    use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_table_meta};
    use sc_db::DatabaseDriver;
    use sc_db_postgres::PgDriver;
    use sc_module::{
        Module, ModulePermissions, ModuleSet, ModuleSource, load_module_by_name, save_module,
    };
    use sc_test_harness::TestDb;
    use sc_types::Attrs;
    use serde_json::{Value, json};

    use crate::common::{have_npm, installed, surfaces};

    async fn catalog(db: &TestDb) -> Arc<Catalog> {
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>)
            .await
            .unwrap();
        sc_auth::bootstrap(&cat).await.unwrap();
        bootstrap_table_meta(&cat).await.unwrap();
        bootstrap_field_meta(&cat).await.unwrap();
        sc_module::bootstrap_modules(&cat).await.unwrap();
        for (role, name) in [(40, "Staff"), (60, "Pro"), (70, "Basic"), (80, "Member")] {
            save_role(&cat, &Role::new(role, name)).await.unwrap();
        }
        Arc::new(cat)
    }

    /// Load the module with `ceiling` and run its action for `user`.
    async fn run(
        cat: &Catalog,
        name: &str,
        host: &Arc<sc_module::ModuleHost>,
        installer: &sc_module::Installer,
        ceiling: Option<u8>,
        user: uuid::Uuid,
    ) -> Value {
        let mut module = match load_module_by_name(cat, name).await.unwrap() {
            Some(module) => module,
            None => Module::new(name, ModuleSource::Local, name),
        };
        module.permissions = ModulePermissions {
            roles: ceiling,
            ..ModulePermissions::closed()
        };
        save_module(cat, &module).await.unwrap();
        let mut registry = ActionRegistry::new();
        let set = ModuleSet::load(cat, host, installer, &surfaces(), &mut registry)
            .await
            .unwrap();
        assert!(set.issues().is_empty(), "{:?}", set.issues());
        let event = Event::new(EventKind::None).payload(json!({ "user": user.to_string() }));
        let config = Attrs::new();
        let action = registry.require("grant_roles").unwrap();
        let mut ctx = ActionContext::new(cat, &event, &config, "grant roles");
        action.run(&mut ctx).await.unwrap()
    }

    #[tokio::test]
    async fn a_module_grants_up_to_its_ceiling_and_never_the_admin_role() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        let user = create_user(&cat, "m@example.com", "pw", 80).await.unwrap();
        let (installer, host, names) = installed("module-roles", &["roles-module"]).await;
        let name = names[0].clone();

        // Allowed roles 60 to 99.
        let result = run(&cat, &name, &host, &installer, Some(60), user.id).await;
        assert_eq!(result["granted"]["ok"]["role"], json!(70), "{result}");
        assert_eq!(result["granted"]["ok"]["note"], json!("Basic"));
        // The renewal is the same grant, moved to the better tier.
        assert_eq!(result["renewed"]["ok"]["id"], result["granted"]["ok"]["id"]);
        assert_eq!(result["renewed"]["ok"]["role"], json!(60));
        assert_eq!(result["got"]["role"], json!(60));
        assert_eq!(result["got"]["in_force"], json!(true));
        // Above the ceiling, and the admin role: refused by name.
        let above = result["above"]["refused"].as_str().unwrap();
        assert!(above.contains("may only grant roles 60 to 99"), "{above}");
        let admin = result["admin"]["refused"].as_str().unwrap();
        assert!(admin.contains("admin role"), "{admin}");
        assert_eq!(result["revoked"], json!({ "revoked": true }));
        assert_eq!(result["missing"], Value::Null);

        // The grant is the module's, by its own name, and it was revoked.
        let grant = find_role_grant(&cat, &name, "sub_1")
            .await
            .unwrap()
            .unwrap();
        assert!(grant.revoked_at.is_some());
        assert!(
            find_role_grant(&cat, &name, "sub_2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_role_grant(&cat, &name, "sub_3")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(load_user(&cat, user.id).await.unwrap().unwrap().role, 80);

        // Allowed nothing: every grant is refused, saying where to allow it.
        let result = run(&cat, &name, &host, &installer, None, user.id).await;
        let refused = result["granted"]["refused"].as_str().unwrap();
        assert!(
            refused.contains("has not been allowed to grant roles"),
            "{refused}"
        );
        assert!(result["admin"]["refused"].is_string());

        host.shutdown().await;
    }
}
