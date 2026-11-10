//! The installation the export tests read: a database with every metadata table
//! bootstrapped, and a helper per entity a test puts in it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};

use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_catalog::{Catalog, DataFieldKind, FileOnConflict, FileOnDelete, FileStoreId};
use sc_error::Result;
use sc_files::FileStoreDef;
use sc_sync::{
    Confirm, DeployHost, Deployer, Executor, Plan, Policy, Receipt, Registries, Request, Source,
    StructureTree,
};
use sc_test_harness::{TestDb, TestServer};

/// A test's installation. Keep it alive for the test: dropping `db` tears the
/// catalog down and deletes the database.
pub struct Installation {
    pub db: TestDb,
    pub catalog: Arc<Catalog>,
    pub registries: Registries,
    /// What a deploy reaches the server's services through.
    pub host: Host,
    /// A scratch directory, removed when the installation is dropped.
    pub dir: PathBuf,
}

impl Drop for Installation {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

impl Installation {
    /// An empty installation with every metadata table there.
    pub async fn new() -> Result<Installation> {
        let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
        sc_auth::bootstrap(&catalog).await?;
        sc_auth::bootstrap_roles(&catalog).await?;
        sc_config::bootstrap(&catalog).await?;
        sc_catalog::bootstrap_table_meta(&catalog).await?;
        sc_catalog::bootstrap_field_meta(&catalog).await?;
        sc_catalog::bootstrap_file_stores(&catalog).await?;
        sc_catalog::bootstrap_db_connections(&catalog).await?;
        sc_catalog::bootstrap_code_libraries(&catalog).await?;
        sc_app::bootstrap(&catalog).await?;
        sc_app::bootstrap_translations(&catalog).await?;
        sc_viewpattern::bootstrap(&catalog).await?;
        sc_action::bootstrap_triggers(&catalog).await?;
        sc_workflow::bootstrap_workflow_versions(&catalog).await?;
        sc_stream::bootstrap_streams(&catalog).await?;
        sc_agent::bootstrap_agents(&catalog).await?;
        sc_llm::bootstrap_llm_providers(&catalog).await?;
        sc_llm::bootstrap_llm_models(&catalog).await?;
        sc_mcp::bootstrap_mcp_servers(&catalog).await?;
        sc_module::bootstrap_modules(&catalog).await?;
        sc_dataset::bootstrap_datasets(&catalog).await?;
        sc_model::bootstrap_models(&catalog).await?;
        sc_analytics::bootstrap_workspaces(&catalog).await?;
        sc_check::bootstrap_checklists(&catalog).await?;
        catalog.reload().await?;
        let dir =
            std::env::temp_dir().join(format!("sc-sync-it-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let registries = Registries::builtin()?;
        Ok(Installation {
            db,
            catalog,
            host: Host::new(&registries)?,
            registries,
            dir,
        })
    }

    /// Export the installation.
    pub async fn export(&self) -> Result<sc_sync::Export> {
        sc_sync::export(&self.catalog, &self.registries).await
    }

    /// The exported tree.
    pub async fn tree(&self) -> Result<StructureTree> {
        Ok(self.export().await?.tree)
    }

    /// Run one schema batch.
    pub async fn schema(&self, ops: Vec<Operation>) -> Result<()> {
        schema_edit::apply(&self.catalog, &ops, &ApplyOptions::default()).await?;
        Ok(())
    }

    /// A table `name` with an integer primary key and text fields.
    pub async fn table(&self, name: &str, fields: &[&str]) -> Result<()> {
        let mut specs = vec![FieldSpec {
            name: "id".into(),
            type_name: "integer".into(),
            primary_key: true,
            ..FieldSpec::default()
        }];
        specs.extend(fields.iter().map(|f| text_field(f)));
        self.schema(vec![Operation::CreateTable {
            name: name.into(),
            database: String::new(),
            settings: TableSettings::default(),
            fields: specs,
        }])
        .await
    }

    /// A local store named `name` in a directory of its own, saved and
    /// returned with its directory.
    pub async fn local_store(
        &self,
        name: &str,
        def: impl FnOnce(FileStoreDef) -> FileStoreDef,
    ) -> Result<(FileStoreDef, PathBuf)> {
        let path = self.dir.join("stores").join(name);
        std::fs::create_dir_all(&path).unwrap();
        let def = def(FileStoreDef::local(name, path.to_string_lossy()));
        sc_catalog::save_file_store(&self.catalog, &def).await?;
        Ok((def, path))
    }

    /// Save an application from its admin-API JSON.
    pub async fn application(&self, body: Json) -> Result<sc_app::Application> {
        let app = sc_backup::codec::application_from_body(sc_app::AppId::new(), &body)?;
        sc_app::save_application(&self.catalog, &app).await
    }

    /// A deployer for this installation, as the operator at the CLI, with
    /// the installation's scratch directory as its data directory.
    pub fn deployer(&self) -> Deployer<'_> {
        Deployer::new(Executor {
            catalog: &self.catalog,
            host: &self.host,
            editor: sc_catalog::Editor::Operator(sc_catalog::Operator::at_cli("a test deploy")),
            data_dir: self.dir.join("data"),
        })
    }

    /// Plan deploying `target` here over `base`.
    pub async fn plan_deploy(&self, base: &StructureTree, target: &StructureTree) -> Result<Plan> {
        self.deployer().plan_deploy(&request(base, target)).await
    }

    /// Deploy `target` over `base` with the plan's default ticks, adjusted by
    /// `confirm`.
    pub async fn deploy(
        &self,
        base: &StructureTree,
        target: &StructureTree,
        confirm: impl FnOnce(Confirm) -> Confirm,
    ) -> Result<Receipt> {
        let request = request(base, target);
        let plan = self.deployer().plan_deploy(&request).await?;
        assert!(plan.blockers.is_empty(), "{:#?}", plan.blockers);
        let confirm = confirm(Confirm::defaults(&plan));
        self.deployer().deploy(&request, &confirm).await
    }

    /// The first column of the first row `sql` returns, as text.
    pub async fn text(&self, sql: &str) -> String {
        let client = self.db.client().await.unwrap();
        let row = client.query_one(sql, &[]).await.unwrap();
        row.get::<_, String>(0)
    }

    /// Run SQL on the database directly, as an admin with `psql` would.
    pub async fn sql(&self, sql: &str) -> Result<()> {
        self.db.execute(sql).await?;
        self.catalog.reload().await
    }
}

/// A deploy of `target` over `base`, from the CLI, under the default policy.
pub fn request<'a>(base: &'a StructureTree, target: &'a StructureTree) -> Request<'a> {
    Request {
        base,
        target,
        commit: None,
        branch: None,
        actor: Some("operator".into()),
        source: Source::Cli,
        policy: Policy::default(),
    }
}

/// The server's services as a deploy reaches them, without a server: the
/// built-in registries, no modules, and no application builds.
pub struct Host {
    pub dispatcher: Arc<sc_action::TriggerDispatcher>,
    pub agents: Arc<sc_agent::AgentRegistry>,
    pub streams: Arc<sc_stream::StreamRegistry>,
    pub models: Arc<sc_model::ModelRegistry>,
    /// A view (application, name) whose description `reload_all` changes, for
    /// a test that needs live to diverge from what was deployed.
    pub diverge: Mutex<Option<(sc_app::AppId, String)>>,
    /// The subdomains `build_and_mount` was asked for.
    pub built: Mutex<Vec<String>>,
}

impl Host {
    fn new(registries: &Registries) -> Result<Host> {
        Ok(Host {
            dispatcher: Arc::new(sc_action::TriggerDispatcher::new(Arc::clone(
                &registries.actions,
            ))),
            agents: Arc::clone(&registries.agent_traits),
            streams: Arc::clone(&registries.streams),
            models: Arc::new(sc_model::ModelRegistry::new()),
            diverge: Mutex::new(None),
            built: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl sc_backup::RestoreHost for Host {
    fn agent_registry(&self) -> Result<Arc<sc_agent::AgentRegistry>> {
        Ok(Arc::clone(&self.agents))
    }

    fn trigger_dispatcher(&self) -> Result<Arc<sc_action::TriggerDispatcher>> {
        Ok(Arc::clone(&self.dispatcher))
    }

    fn modules_installed(&self) -> Result<()> {
        Err(sc_error::Error::invalid(
            "this test installation runs no modules",
        ))
    }

    async fn install_module_package(
        &self,
        _language: sc_module::ModuleLanguage,
        _source: sc_module::ModuleSource,
        _location: &str,
    ) -> Result<sc_module::InstalledPackage> {
        Err(sc_error::Error::invalid(
            "this test installation runs no modules",
        ))
    }

    async fn reload_modules(&self) -> Result<()> {
        Ok(())
    }

    fn stream_registry(&self) -> Result<Arc<sc_stream::StreamRegistry>> {
        Ok(Arc::clone(&self.streams))
    }

    async fn reload_streams(&self, _catalog: &Catalog) -> Result<bool> {
        Ok(false)
    }

    fn model_registry(&self) -> Result<Arc<sc_model::ModelRegistry>> {
        Ok(Arc::clone(&self.models))
    }

    async fn dataset_shape(&self, _dataset: &sc_model::Dataset) -> Option<sc_model::DatasetShape> {
        None
    }

    async fn build_and_mount(&self, app: sc_app::Application) -> Result<sc_app::BuildReport> {
        self.built.lock().unwrap().push(app.subdomain.clone());
        Err(sc_error::Error::invalid(
            "this test installation builds nothing",
        ))
    }
}

#[async_trait::async_trait]
impl DeployHost for Host {
    async fn reload_all(&self, catalog: &Catalog) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(e) = catalog.reload().await {
            problems.push(e.causes());
        }
        let diverge = self.diverge.lock().unwrap().take();
        if let Some((app, name)) = diverge {
            let mut view = sc_viewpattern::load_view(catalog, app, &name)
                .await
                .unwrap()
                .expect("the view to diverge is there");
            view.description = "changed behind the deploy's back".into();
            sc_viewpattern::save_view(catalog, &view).await.unwrap();
        }
        problems
    }
}

/// A plain text field.
pub fn text_field(name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: "string".into(),
        ..FieldSpec::default()
    }
}

/// A `File` field on `store`, in `folder` if given.
pub fn file_field(name: &str, store: &str, folder: Option<&str>) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: "string".into(),
        kind: DataFieldKind::File {
            store: FileStoreId(store.into()),
            folder: folder.map(str::to_owned),
            mime_allow: Vec::new(),
            on_delete: FileOnDelete::default(),
            on_conflict: FileOnConflict::default(),
        },
        ..FieldSpec::default()
    }
}

/// Write `bytes` at `path` under `root`, making the directories.
pub fn put(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Every file of the tree as text (lossily), for "this never appears anywhere"
/// assertions.
pub fn all_text(tree: &StructureTree) -> String {
    tree.to_files()
        .iter()
        .map(|(path, bytes)| format!("== {path}\n{}\n", String::from_utf8_lossy(bytes)))
        .collect()
}

/// The JSON at `path`, which the test expects to be there.
pub fn entity<'a>(tree: &'a StructureTree, path: &str) -> &'a Json {
    tree.json(path).unwrap_or_else(|| {
        panic!(
            "no `{path}` in the tree; it has {:?}",
            tree.paths().collect::<Vec<_>>()
        )
    })
}

/// A `code` application on `store`, its source in `source` and its build
/// output in `output`.
pub fn code_app(subdomain: &str, store: &str, source: &str, output: &str) -> Json {
    json!({
        "name": subdomain,
        "subdomain": subdomain,
        "framework": {
            "name": "code",
            "config": {
                "store": store,
                "source": source,
                "output": output,
                "command": "npm run build",
            },
        },
        "tables": [],
        "file_stores": [store],
        "apis": [],
    })
}

/// Run `git` in `dir`, which must succeed.
#[allow(
    clippy::disallowed_methods,
    reason = "test code: a fixture repository is made with the git command line"
)]
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A module's table provider, without a module: `@test/feed`'s `Feed`, whose
/// settings are a URL and a secret token, presenting `title` and `link`.
pub struct FeedProvider;

impl FeedProvider {
    /// Install it on `inst`'s catalog, as a server does for its modules.
    pub async fn install(inst: &Installation) -> Result<()> {
        inst.catalog
            .set_table_providers(Arc::new(FeedProvider) as Arc<dyn sc_catalog::TableProviderHost>)?;
        inst.catalog.reload().await
    }
}

#[async_trait::async_trait]
impl sc_catalog::TableProviderHost for FeedProvider {
    fn providers(&self) -> Vec<sc_catalog::TableProviderKind> {
        use sc_types::{BasicType, FormField, TypeRef};
        vec![sc_catalog::TableProviderKind {
            module: "@test/feed".into(),
            provider: "Feed".into(),
            config_spec: vec![
                FormField::new("url", TypeRef::Basic(BasicType::Text)),
                FormField::new("token", TypeRef::Basic(BasicType::Text)).secret(),
            ],
        }]
    }

    async fn fields(&self, _: &str, _: &str, _: &Json) -> Result<Vec<sc_catalog::DataField>> {
        use sc_types::{BasicType, TypeRef};
        Ok(vec![
            sc_catalog::DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            sc_catalog::DataField::plain("link", TypeRef::Basic(BasicType::Text)),
        ])
    }

    async fn rows(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &Json,
        _: &Json,
        _: &Json,
    ) -> Result<Vec<Json>> {
        Ok(Vec::new())
    }

    async fn writes(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &Json,
    ) -> Result<sc_catalog::ProvidedWrites> {
        Ok(sc_catalog::ProvidedWrites::NONE)
    }

    async fn insert(&self, _: &str, _: &str, _: &str, _: &Json, _: &Json) -> Result<Json> {
        Err(sc_error::Error::invalid("read-only"))
    }

    async fn update(&self, _: &str, _: &str, _: &str, _: &Json, _: &Json, _: &Json) -> Result<()> {
        Err(sc_error::Error::invalid("read-only"))
    }

    async fn delete(&self, _: &str, _: &str, _: &str, _: &Json, _: &Json) -> Result<()> {
        Err(sc_error::Error::invalid("read-only"))
    }
}
