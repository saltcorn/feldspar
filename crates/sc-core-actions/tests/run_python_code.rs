//! The `run_python_code` action, and the seam it reaches its engine through
//! (TODO phases 2.1 and 2.2).
//!
//! Deliberately **not** against a real interpreter: `sc-python` is behind a
//! feature that links `libpython`, and every test binary in this workspace would
//! then need a Python toolchain to link. What this file is about is the wiring
//! either way — that a trigger configured as `run_python_code` reaches the
//! adapter registered under `"python"`, carrying the body, the event's bindings
//! and the five host surfaces; that it says so by name when this process has no
//! adapter; and that the two languages' actions agree about everything that is
//! not the language. The interpreter's own behaviour is asserted in `sc-python`,
//! against the runtime rather than against a stub of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_action::{
    ActionContext, Event, EventKind, Trigger, TriggerDispatcher, bootstrap_triggers, save_trigger,
};
use sc_catalog::{CallerContext, Catalog};
use sc_core_actions::builtin_actions;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{CodeAdapter, CodeCall, PYTHON};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// One table, so a body has something a real host would let it reach.
const SCHEMA: &str = "CREATE TABLE books (id bigint primary key, title text);\
                      INSERT INTO books VALUES (7, 'A Book');";

/// An adapter that runs nothing and remembers everything: what body it was
/// handed, what the event bound, how long it was given, and which of the five
/// surfaces came with it.
struct Recorder {
    language: String,
    seen: Mutex<Vec<Seen>>,
    answer: Json,
}

#[derive(Clone, Debug)]
struct Seen {
    code: String,
    bindings: Vec<(String, Json)>,
    timeout_ms: Option<u128>,
    surfaces: [bool; 5],
}

impl Recorder {
    fn new(language: &str, answer: Json) -> Arc<Recorder> {
        Arc::new(Recorder {
            language: language.to_owned(),
            seen: Mutex::new(Vec::new()),
            answer,
        })
    }

    fn only(&self) -> Seen {
        let seen = self.seen.lock().expect("not poisoned");
        assert_eq!(seen.len(), 1, "expected one run, got {seen:?}");
        seen[0].clone()
    }
}

#[async_trait]
impl CodeAdapter for Recorder {
    fn language(&self) -> &str {
        &self.language
    }

    async fn run_code(&self, call: CodeCall<'_>) -> Result<Json> {
        self.seen.lock().expect("not poisoned").push(Seen {
            code: call.code.clone(),
            bindings: call
                .bindings
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            timeout_ms: call.timeout.map(|t| t.as_millis()),
            surfaces: [
                call.host.is_some(),
                call.fetch.is_some(),
                call.files.is_some(),
                call.triggers.is_some(),
                call.module_fns.is_some(),
            ],
        });
        Ok(self.answer.clone())
    }
}

struct World {
    catalog: Arc<Catalog>,
    _db: TestDb,
}

async fn setup() -> Result<World> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    bootstrap_triggers(&catalog).await?;
    Ok(World { catalog, _db: db })
}

/// An insert of a book by a known caller — the event most of these fire for.
fn book_insert() -> Event {
    Event::new(EventKind::Insert)
        .on("books")
        .row(json!({ "id": 7, "title": "A Book" }))
        .caller(1, Some(json!({ "email": "ada@example.com" })))
}

fn config(code: &str) -> Attrs {
    [("code".to_owned(), json!(code))].into_iter().collect()
}

#[tokio::test]
async fn a_python_trigger_reaches_the_adapter_registered_under_python() -> Result<()> {
    let world = setup().await?;
    let python = Recorder::new(PYTHON, json!({ "chased": 2 }));

    let registry = Arc::new(builtin_actions()?);
    let dispatcher = Arc::new(
        TriggerDispatcher::new(Arc::clone(&registry))
            .with_adapter(Arc::clone(&python) as Arc<dyn CodeAdapter>),
    );
    let trigger = Trigger::new("chase", EventKind::Insert, "run_python_code")
        .on("books")
        .config("code", "return {\"chased\": len(db.books.rows())}")
        .config("timeout_ms", json!(2500));
    save_trigger(&world.catalog, &registry, &trigger).await?;
    dispatcher.reload(&world.catalog).await?;

    // Fired the way a write fires it, through the dispatcher.
    let runs = dispatcher.dispatch(&world.catalog, &book_insert()).await;
    assert_eq!(runs.len(), 1);
    let outcome = runs.into_iter().next().unwrap().outcome?;
    assert_eq!(
        outcome,
        Some(json!({ "chased": 2 })),
        "the adapter's answer"
    );

    let seen = python.only();
    assert_eq!(seen.code, "return {\"chased\": len(db.books.rows())}");
    assert_eq!(seen.timeout_ms, Some(2500), "the configured wall clock");
    // Presence is scope, and it is the JavaScript action's rule unchanged: the
    // event's rows, its user and its payload, and no `context` outside a run.
    let names: Vec<&str> = seen.bindings.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        vec!["app", "old", "payload", "row", "session", "user"]
    );
    // All five surfaces travel: the same borrowed hosts a JavaScript body gets,
    // which is what makes the two languages agree about authority and events.
    assert_eq!(
        seen.surfaces,
        [true, true, true, true, false],
        "db, fetch, fs and trigger; no modules are loaded in this process"
    );
    Ok(())
}

#[tokio::test]
async fn a_process_with_no_python_adapter_says_so_by_name() -> Result<()> {
    let world = setup().await?;
    let registry = builtin_actions()?;
    let action = registry.require("run_python_code")?.clone();
    let event = book_insert();
    let cfg = config("return 1");
    // No `with_adapters` at all — a context that assembled none, which is what a
    // unit test and client generation are.
    let mut ctx = ActionContext::new(&world.catalog, &event, &cfg, "chase");
    let said = action.run(&mut ctx).await.unwrap_err().to_string();
    assert!(said.contains("chase"), "{said}");
    assert!(said.contains("python"), "{said}");
    Ok(())
}

#[tokio::test]
async fn an_adapter_is_reached_by_its_own_language_and_by_no_other() -> Result<()> {
    let world = setup().await?;
    let python = Recorder::new(PYTHON, json!("py"));
    let other = Recorder::new("ruby", json!("rb"));
    let dispatcher = TriggerDispatcher::new(Arc::new(builtin_actions()?))
        .with_adapter(Arc::clone(&other) as Arc<dyn CodeAdapter>)
        .with_adapter(Arc::clone(&python) as Arc<dyn CodeAdapter>);
    let event = book_insert();
    let cfg = config("return 1");
    let ctx = ActionContext::new(&world.catalog, &event, &cfg, "chase")
        .with_adapters(&dispatcher.services().adapters);

    assert_eq!(ctx.adapter(PYTHON)?.language(), PYTHON);
    assert_eq!(ctx.adapter("ruby")?.language(), "ruby");
    // A language nobody registered is a named error rather than a wrong engine.
    let said = match ctx.adapter("cobol") {
        Ok(_) => panic!("nobody registered cobol"),
        Err(e) => e.to_string(),
    };
    assert!(said.contains("cobol") && said.contains("chase"), "{said}");
    Ok(())
}

#[tokio::test]
async fn the_two_languages_differ_in_the_language_and_nothing_else() -> Result<()> {
    let registry = builtin_actions()?;
    let js = registry.require("run_js_code")?.config_spec();
    let py = registry.require("run_python_code")?.config_spec();
    let names = |spec: &[sc_types::FormField]| -> Vec<String> {
        spec.iter().map(|f| f.name().to_owned()).collect()
    };
    assert_eq!(names(&js), names(&py), "the same two settings");
    assert_eq!(js[1].default, py[1].default, "the same default timeout");
    assert_eq!(js[0].code_language.as_deref(), Some("javascript"));
    assert_eq!(py[0].code_language.as_deref(), Some("python"));

    // And the same refusals on the way in, so a timeout that is out of range is
    // a message on the form in either language.
    let world = setup().await?;
    let registry = Arc::new(registry);
    let bad = Trigger::new("chase", EventKind::None, "run_python_code")
        .config("code", "return 1")
        .config("timeout_ms", json!(600_000));
    let said = save_trigger(&world.catalog, &registry, &bad)
        .await
        .unwrap_err()
        .to_string();
    assert!(said.contains("timeout_ms"), "{said}");
    Ok(())
}

#[tokio::test]
async fn a_workflow_step_reaches_the_same_adapter_a_trigger_does() -> Result<()> {
    let world = setup().await?;
    let python = Recorder::new(PYTHON, json!({ "total": 12 }));
    let dispatcher = TriggerDispatcher::new(Arc::new(builtin_actions()?))
        .with_adapter(Arc::clone(&python) as Arc<dyn CodeAdapter>);
    let event = Event::new(EventKind::None).payload(json!({ "n": 3 }));
    let cfg = config("return {\"total\": context[\"so_far\"] * payload[\"n\"]}");
    let context: Attrs = [("so_far".to_owned(), json!(4))].into_iter().collect();
    // What the workflow driver builds for a step: the run's context in hand, and
    // the same services a trigger's own action is given.
    let mut ctx = ActionContext::new(&world.catalog, &event, &cfg, "step_two")
        .with_adapters(&dispatcher.services().adapters)
        .with_run_context(context);
    let out = sc_core_actions::builtin_actions()?
        .require("run_python_code")?
        .clone()
        .run(&mut ctx)
        .await?;
    assert_eq!(out, json!({ "total": 12 }));
    // `context` is in scope for a step and for nothing else — the other half of
    // decision 8, which the step shares with `run_js_code`.
    let seen = python.only();
    let names: Vec<&str> = seen.bindings.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, vec!["app", "context", "payload", "session", "user"]);
    let context = seen.bindings.iter().find(|(k, _)| k == "context").unwrap();
    assert_eq!(context.1, json!({ "so_far": 4 }), "the run so far");
    Ok(())
}

/// A caller for the dispatcher's own `run_trigger` path, which is the Run
/// button's — kept beside the write path above because the two build the
/// context differently and both must reach the adapter.
#[tokio::test]
async fn the_run_button_reaches_the_adapter_too() -> Result<()> {
    let world = setup().await?;
    let python = Recorder::new(PYTHON, json!("ran"));
    let registry = Arc::new(builtin_actions()?);
    let dispatcher = TriggerDispatcher::new(Arc::clone(&registry))
        .with_adapter(Arc::clone(&python) as Arc<dyn CodeAdapter>);
    let trigger = Trigger::new("reindex", EventKind::None, "run_python_code")
        .config("code", "return \"ran\"");
    save_trigger(&world.catalog, &registry, &trigger).await?;
    dispatcher.reload(&world.catalog).await?;

    let caller = CallerContext::new(1, Some(json!({ "email": "ada@example.com" })));
    let out = dispatcher
        .run_trigger(
            &world.catalog,
            "reindex",
            json!({ "why": "nightly" }),
            Some(&caller),
        )
        .await?;
    assert_eq!(out, json!("ran"));
    let seen = python.only();
    assert_eq!(
        seen.bindings
            .iter()
            .find(|(k, _)| k == "payload")
            .map(|(_, v)| v.clone()),
        Some(json!({ "why": "nightly" })),
        "a directly-run trigger's payload is what it was called with"
    );
    Ok(())
}
