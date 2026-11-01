//! The typed handler adapter (`crate::handler::typed`): the body is read into
//! the handler's input type, its answer is serialized, and a whole
//! `HandlerResponse` keeps its status.

use std::sync::Arc;

use sc_catalog::Catalog;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::HandlerState;
use crate::apps::AppMounts;
use crate::handler::{Admin, Caller, HandlerCtx, HandlerRegistry, HandlerResponse, typed};

#[derive(Deserialize)]
struct Greeting {
    name: String,
}

#[derive(Serialize)]
struct Greeted {
    text: String,
    tables: usize,
}

async fn greet(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    input: Greeting,
) -> sc_error::Result<Greeted> {
    Ok(Greeted {
        text: format!("hello {}", input.name),
        tables: state.catalog.tables()?.len(),
    })
}

async fn created(
    _state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    _input: (),
) -> sc_error::Result<HandlerResponse> {
    Ok(HandlerResponse::ok(json!({ "made": true })).with_status(201))
}

fn request(body: Json) -> HandlerCtx {
    HandlerCtx {
        path_params: Default::default(),
        query: Vec::new(),
        body,
        who: Caller::Admin(Admin::delegated(None, "typed handler test")),
        raw_body: None,
        raw_file: None,
        analytics_app: None,
        client_ip: None,
        locale: sc_i18n::Locale::source(),
    }
}

async fn registry(teardown: &sc_test_harness::Teardown) -> HandlerRegistry {
    let driver = sc_db_sqlite::SqliteDriver::open_in_memory().unwrap();
    let catalog = Arc::new(Catalog::init(Arc::new(driver)).await.unwrap());
    teardown.add(catalog.teardown_hook());
    let state = HandlerState {
        catalog: catalog.clone(),
        apps: Arc::new(AppMounts::new(catalog)),
    };
    let mut reg = HandlerRegistry::new();
    reg.register_admin("greet", typed(&state, greet));
    reg.register_admin("created", typed(&state, created));
    reg
}

#[tokio::test]
async fn a_typed_handler_reads_its_input_and_writes_its_answer() {
    let teardown = sc_test_harness::Teardown::new();
    let reg = registry(&teardown).await;
    let handler = reg.get("greet").unwrap();

    let response = handler(request(json!({ "name": "Ada" }))).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body["text"], json!("hello Ada"));
    assert!(response.body["tables"].is_u64());

    let refused = match handler(request(json!({ "nom": "Ada" }))).await {
        Ok(_) => panic!("a body without `name` does not fit"),
        Err(e) => e,
    };
    assert!(refused.to_string().contains("name"), "{refused}");
}

#[tokio::test]
async fn a_typed_handler_answering_a_response_keeps_its_status() {
    let teardown = sc_test_harness::Teardown::new();
    let reg = registry(&teardown).await;
    let response = reg.get("created").unwrap()(request(Json::Null))
        .await
        .unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.body, json!({ "made": true }));
}
