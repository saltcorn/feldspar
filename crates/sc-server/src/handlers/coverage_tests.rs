//! The admin endpoint table (`sc_api::admin_endpoints`, which includes the
//! Analytics UI's endpoints) and the handler registry built here are joined by
//! name only. A declared endpoint with no handler is a `501` at request time, so
//! this checks the two halves agree before anything is served.

use std::sync::Arc;

use sc_api::HandlerRef;
use sc_catalog::Catalog;

use crate::apps::AppMounts;
use crate::handler::HandlerRegistry;

/// The registry the router serves the admin API with: [`super::admin_handlers`]
/// plus what the router registers itself.
async fn served_registry(teardown: &sc_test_harness::Teardown) -> HandlerRegistry {
    let driver = sc_db_sqlite::SqliteDriver::open_in_memory().unwrap();
    let catalog = Arc::new(Catalog::init(Arc::new(driver)).await.unwrap());
    teardown.add(catalog.teardown_hook());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let mut reg = super::admin_handlers(catalog, apps);
    crate::builder::register_status_handler(&mut reg, None);
    reg
}

#[tokio::test]
async fn every_named_admin_endpoint_has_a_handler() {
    let teardown = sc_test_harness::Teardown::new();
    let reg = served_registry(&teardown).await;
    let set = sc_api::admin_endpoints();
    let mut named = 0;
    let missing: Vec<&str> = set
        .iter()
        .filter_map(|ep| match &ep.handler {
            HandlerRef::Named(name) => {
                named += 1;
                reg.get(name).is_none().then_some(name.as_str())
            }
            HandlerRef::GuestCode { .. } | HandlerRef::Custom(_) => None,
        })
        .collect();
    assert!(
        missing.is_empty(),
        "admin endpoints with no registered handler (a 501 at request time): {missing:?}"
    );
    // The set really is the admin table, Analytics UI included.
    assert!(named > 200, "{named}");
    for name in ["listUsers", "listDatasets", "builderStatus"] {
        assert!(set.find(name).is_some(), "`{name}` is not an endpoint");
    }
}
