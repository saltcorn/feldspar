#![allow(dead_code)] // each test binary compiles this module and uses part of it
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The scaffolding the trait tests share: a per-test database with a small
//! library in it, the metadata tables bootstrapped, one connected LLM provider
//! to point agents at, and a way to call one trait's tool as any caller.
//!
//! Every trait in this crate is pinned against a **real** database and, where a
//! table's ownership formula does not translate to SQL, the real V8 engine. A
//! mock would hide exactly the mistakes these tests exist to catch: the §7.3
//! rule is enforced by code this crate calls rather than code it contains, so a
//! test that stubbed it out would be testing nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_action::{ActionRegistry, TriggerDispatcher, bootstrap_triggers};
use sc_agent::{
    AgentRegistry, RunCaller, RunId, ToolsContext, TraitCheck, TraitContext, bootstrap_agents,
    bootstrap_runs,
};
use sc_auth::User;
use sc_catalog::{
    Catalog, TableEvents, TableMeta, bootstrap_field_meta, bootstrap_file_stores,
    bootstrap_table_meta, connect_file_store_def, save_file_store, save_table_meta,
};
use sc_core_traits::builtin_traits;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_files::FileStoreDef;
use sc_llm::{ToolSpec, bootstrap_llm_providers};
use sc_query::Value;
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::{Value as Json, json};
use uuid::Uuid;

/// A library with two owners, so an ownership formula has something to divide,
/// and a `notes` column an allow-list can keep from the model.
pub const SCHEMA: &str = "
    CREATE TABLE books (
        id bigint primary key,
        title text,
        pages bigint,
        owner text,
        notes text
    );
    INSERT INTO books VALUES
        (1, 'Dune',   412, 'ada@example.com', 'ada''s copy'),
        (2, 'Emma',   474, 'bob@example.com', 'bob''s copy'),
        (3, 'Ilium',  576, 'ada@example.com', 'also ada''s'),
        (4, 'Ubik',   224, 'bob@example.com', NULL);
";

/// Everything a trait needs to run, and the handles a test needs to change the
/// world underneath it.
pub struct Env {
    pub db: TestDb,
    pub catalog: Catalog,
    pub registry: AgentRegistry,
    pub evaluator: Option<Arc<dyn JsEvaluator>>,
    pub dispatcher: Option<Arc<TriggerDispatcher>>,
}

impl Env {
    /// A catalog over a per-test database with the library, the overlay table,
    /// the trigger table and the agent tables, plus one connected provider —
    /// an agent that names no connected provider does not validate, so every
    /// test would otherwise write the same row.
    pub async fn new() -> Result<Env> {
        Env::with_schema(SCHEMA).await
    }

    /// [`Env::new`] over a schema of the test's own.
    pub async fn with_schema(schema: &str) -> Result<Env> {
        Env::with_db(TestDb::new().await?, schema).await
    }

    /// [`Env::with_schema`] over a database of the test's own making — one
    /// with pgvector (`TestDb::with_pgvector`), say.
    pub async fn with_db(db: TestDb, schema: &str) -> Result<Env> {
        db.client()
            .await?
            .batch_execute(schema)
            .await
            .map_err(|e| Error::database(e.to_string()))?;
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
        bootstrap_table_meta(&catalog).await?;
        bootstrap_field_meta(&catalog).await?;
        bootstrap_triggers(&catalog).await?;
        bootstrap_llm_providers(&catalog).await?;
        bootstrap_file_stores(&catalog).await?;
        bootstrap_agents(&catalog).await?;
        bootstrap_runs(&catalog).await?;
        // The applications table, for `build_application`: the trait resolves
        // the app it builds from its stored row.
        sc_app::bootstrap(&catalog).await?;
        sc_llm::save_llm_provider(
            &catalog,
            &sc_llm::LlmProviderDef::new("main", sc_llm::ANTHROPIC_BACKEND)
                .with(sc_llm::CFG_API_KEY, "sk-ant-not-a-real-key"),
        )
        .await?;
        // The model an agent naming no model calls: the provider's default row.
        let provider = sc_llm::require_llm_provider(&catalog, "main").await?;
        sc_llm::save_llm_model(
            &catalog,
            &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
        )
        .await?;
        catalog.reload().await?;
        Ok(Env {
            db,
            catalog,
            registry: builtin_traits()?,
            evaluator: None,
            dispatcher: None,
        })
    }

    /// Define and connect a **local** file store rooted at a fresh temporary
    /// directory, returning that directory.
    ///
    /// A real store on a real disk, for the reason every other test here uses a
    /// real database: the coding traits' whole job is what happens when bytes
    /// meet paths, and a stubbed store would confirm only that the seam was
    /// called.
    pub async fn with_file_store(&self, name: &str, min_role: Option<u8>) -> Result<PathBuf> {
        let dir = temp_dir(name);
        std::fs::create_dir_all(&dir).map_err(|e| Error::config(e.to_string()))?;
        let mut def = FileStoreDef::local(name, dir.to_string_lossy());
        def.min_role = min_role;
        save_file_store(&self.catalog, &def).await?;
        connect_file_store_def(&self.catalog, &def)?;
        Ok(dir)
    }

    /// Write a file into a store's directory, creating parents — the arrange
    /// half of a coding-trait test.
    pub fn put(&self, dir: &Path, rel: &str, contents: &str) -> Result<()> {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::config(e.to_string()))?;
        }
        std::fs::write(&path, contents).map_err(|e| Error::config(e.to_string()))
    }

    /// Read a file back out of a store's directory — the assert half.
    pub fn slurp(&self, dir: &Path, rel: &str) -> Result<String> {
        std::fs::read_to_string(dir.join(rel)).map_err(|e| Error::config(e.to_string()))
    }

    /// Call a named tool of a trait that offers several, as `caller`.
    ///
    /// [`Env::call`] takes the first tool, which is every trait in this crate;
    /// this exists so a test can name the tool it means and therefore assert that
    /// the name the configuration derives is the name that works.
    pub async fn call_tool(
        &self,
        trait_: &str,
        config: &Attrs,
        tool: &str,
        args: Json,
        caller: &RunCaller,
    ) -> Result<Json> {
        let trait_ = self.registry.require(trait_)?.clone();
        let mut ctx = TraitContext {
            catalog: &self.catalog,
            caller,
            agent: "librarian",
            run: RunId::new(),
            mode: sc_agent::RunMode::Act,
            // Fresh per call: a tool called outside a run keeps nothing.
            trait_state: &mut Json::Null,
            evaluator: self.evaluator.as_ref(),
            triggers: self.dispatcher.as_ref(),
            // A tool called directly, outside a run, has no runner to delegate
            // through — which is what a `subagent` trait tested this way gets
            // told, and why its own test drives a whole run instead.
            delegate: None,
            previews: None,
            browser: None,
            requests: None,
            modules: None,
            signals: Vec::new(),
            images: Vec::new(),
            extras: Default::default(),
        };
        trait_.call(config, tool, &args, &mut ctx).await
    }

    /// A tool context for `caller` outside any run, for a test that drives a
    /// trait-side helper (the sandbox) rather than a whole tool.
    pub fn context<'a>(&'a self, caller: &'a RunCaller, state: &'a mut Json) -> TraitContext<'a> {
        TraitContext {
            catalog: &self.catalog,
            caller,
            agent: "librarian",
            run: RunId::new(),
            mode: sc_agent::RunMode::Act,
            trait_state: state,
            evaluator: self.evaluator.as_ref(),
            triggers: self.dispatcher.as_ref(),
            delegate: None,
            previews: None,
            browser: None,
            requests: None,
            modules: None,
            signals: Vec::new(),
            images: Vec::new(),
            extras: Default::default(),
        }
    }

    /// Give the tools the real JavaScript engine — what a table whose ownership
    /// formula does not translate needs.
    /// Call a trait's tool as one step of a run whose trait state is `state`,
    /// so what one call records (a read, the ledger) the next one sees. Returns
    /// the result and the signals the call raised.
    pub async fn call_in_run(
        &self,
        state: &mut Json,
        trait_: &str,
        config: &Attrs,
        tool: &str,
        args: Json,
        caller: &RunCaller,
    ) -> (Result<Json>, Vec<sc_agent::Signal>) {
        let trait_ = match self.registry.require(trait_) {
            Ok(t) => t.clone(),
            Err(e) => return (Err(e), Vec::new()),
        };
        let mut ctx = TraitContext {
            catalog: &self.catalog,
            caller,
            agent: "librarian",
            run: RunId::new(),
            mode: sc_agent::RunMode::Act,
            trait_state: state,
            evaluator: self.evaluator.as_ref(),
            triggers: self.dispatcher.as_ref(),
            delegate: None,
            previews: None,
            browser: None,
            requests: None,
            modules: None,
            signals: Vec::new(),
            images: Vec::new(),
            extras: Default::default(),
        };
        let result = trait_.call(config, tool, &args, &mut ctx).await;
        (result, ctx.signals)
    }

    /// The trait's `after_tools` hook, as the loop calls it at the end of a
    /// turn, over the same run state.
    pub async fn after_tools(
        &self,
        state: &mut Json,
        trait_: &str,
        config: &Attrs,
        caller: &RunCaller,
    ) -> Result<Option<String>> {
        let trait_ = self.registry.require(trait_)?.clone();
        let mut cx = sc_agent::AfterToolsContext {
            catalog: &self.catalog,
            caller,
            agent: "librarian",
            run: RunId::new(),
            mode: sc_agent::RunMode::Act,
            calls: &[],
            trait_state: state,
        };
        trait_.after_tools(config, &mut cx).await
    }

    pub fn with_engine(mut self) -> Env {
        self.evaluator = Some(Arc::new(DenoEvaluator::new()));
        self
    }

    /// Install a trigger dispatcher over `actions`, **and** hook it up as the
    /// catalog's write listener, so a row a trait writes raises the events a row
    /// written any other way raises.
    pub fn with_triggers(mut self, actions: ActionRegistry) -> Result<Env> {
        let dispatcher = Arc::new(
            TriggerDispatcher::new(Arc::new(actions))
                .with_evaluator(Arc::new(DenoEvaluator::new())),
        );
        self.catalog.add_table_events(
            sc_catalog::TRIGGER_EVENTS,
            Arc::clone(&dispatcher) as Arc<dyn TableEvents>,
        )?;
        self.dispatcher = Some(dispatcher);
        Ok(self)
    }

    /// Reload the trigger set — after saving one.
    pub async fn reload_triggers(&self) -> Result<()> {
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.reload(&self.catalog).await?;
        }
        Ok(())
    }

    /// The tools one configured instance of `trait_` offers.
    pub fn tools(&self, trait_: &str, config: &Attrs) -> Vec<ToolSpec> {
        self.registry
            .require(trait_)
            .expect("a built-in trait")
            .tools(&self.tools_context(), config)
    }

    /// What `tools` is told: an `act` run on a model with the built-in
    /// capabilities of an unknown one.
    pub fn tools_context(&self) -> ToolsContext<'_> {
        static CAPABILITIES: std::sync::LazyLock<sc_llm::ModelCapabilities> =
            std::sync::LazyLock::new(|| sc_llm::ModelCapabilities::built_in("", ""));
        ToolsContext::new(&self.catalog, sc_agent::RunMode::Act, &CAPABILITIES)
    }

    /// The trait's own configuration check — the admin's Save button.
    pub async fn check(&self, trait_: &str, config: &Attrs) -> Result<()> {
        self.registry
            .require(trait_)?
            .validate_config(&TraitCheck {
                catalog: &self.catalog,
                config,
                agent: "librarian",
                host: self.registry.host(),
            })
            .await
    }

    /// Call the trait's (single) tool directly, as `caller`.
    pub async fn call(
        &self,
        trait_: &str,
        config: &Attrs,
        args: Json,
        caller: &RunCaller,
    ) -> Result<Json> {
        let trait_ = self.registry.require(trait_)?.clone();
        let tool = trait_.tools(&self.tools_context(), config)[0].name.clone();
        let mut ctx = TraitContext {
            catalog: &self.catalog,
            caller,
            agent: "librarian",
            run: RunId::new(),
            mode: sc_agent::RunMode::Act,
            // Fresh per call: a tool called outside a run keeps nothing.
            trait_state: &mut Json::Null,
            evaluator: self.evaluator.as_ref(),
            triggers: self.dispatcher.as_ref(),
            // A tool called directly, outside a run, has no runner to delegate
            // through — which is what a `subagent` trait tested this way gets
            // told, and why its own test drives a whole run instead.
            delegate: None,
            previews: None,
            browser: None,
            requests: None,
            modules: None,
            signals: Vec::new(),
            images: Vec::new(),
            extras: Default::default(),
        };
        trait_.call(config, &tool, &args, &mut ctx).await
    }

    /// Put `formula` on a table as a runtime ownership rule — **not** RLS, so
    /// the §7.3 checks run in `sc-api` rather than in the database, which is the
    /// path a tool takes on an ordinary table.
    pub async fn own(&self, table: &str, formula: &str) -> Result<()> {
        let mut meta = TableMeta::new(table);
        meta.set_ownership_formula(Some(formula));
        save_table_meta(&self.catalog, &meta).await?;
        assert!(
            self.catalog.require(table)?.ownership.is_some(),
            "the formula is live"
        );
        Ok(())
    }

    /// Run some SQL against the test database.
    pub async fn execute(&self, sql: &str) -> Result<()> {
        self.db
            .client()
            .await?
            .batch_execute(sql)
            .await
            .map_err(|e| Error::database(e.to_string()))
    }

    /// Every row of a table, as JSON, read with no access rule in the way — how
    /// a test checks what a write actually did.
    pub async fn rows(&self, table: &str) -> Result<Vec<Json>> {
        let table = self.catalog.require(table)?;
        let rows = sc_api::rows::list_rows(&self.catalog, &table).await?;
        Ok(rows.as_array().cloned().unwrap_or_default())
    }
}

/// A fresh, unique temporary directory to root a store at.
///
/// Unique on process, clock *and* a counter: two tests starting in the same tick
/// on different threads would otherwise share a store root and fail on each
/// other's files — rarely, and therefore confusingly.
pub fn temp_dir(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "sc-core-traits-{name}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// A configuration from `(key, value)` pairs.
pub fn config(entries: &[(&str, Json)]) -> Attrs {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// A user at role 40 — below `books`' admin-only floors, so their access is
/// whatever the ownership formula grants and nothing else.
pub fn reader(email: &str) -> User {
    let mut user = User::new(Uuid::new_v4(), 40).unwrap();
    user.extra = BTreeMap::from([("email".to_owned(), Value::Text(email.to_owned()))]);
    user
}

/// That user as a run's caller.
pub fn as_user(email: &str) -> RunCaller {
    RunCaller::user(reader(email))
}

/// The `title` of every row in a tool result, in the order they came back.
pub fn titles(result: &Json) -> Vec<String> {
    result["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| r["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The `title` of every row of a table, ordered by primary key.
pub fn row_titles(rows: &[Json]) -> Vec<String> {
    let mut rows = rows.to_vec();
    rows.sort_by_key(|r| r["id"].as_i64().unwrap_or_default());
    rows.iter()
        .map(|r| r["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A trivial JSON object, for a payload a test does not care about.
pub fn empty() -> Json {
    json!({})
}
