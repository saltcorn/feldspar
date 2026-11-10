//! The executor: applying a [`Plan`] (APP_SYNC_PROPOSAL §5, §6 step 8).
//!
//! **Every operation goes through the save path an admin's edit would.** A
//! table operation is its `schema_edit::apply` batch, with the ids the plan
//! gives the tables and fields it creates. Every other entity is parsed by the
//! admin API's own `*_from_body` (`sc_backup::codec`) and written by its own
//! store, so a deployed view, trigger or agent is checked for exactly the
//! reasons one typed into the form would be, and keeps the id it has in the
//! repository (§3.2).
//!
//! **Secrets are the instance's** (§3.3). The tree holds every secret as the
//! sentinel, and before an entity is parsed each sentinel is replaced by the
//! value stored here at the same place ([`unmask`]) — the rule
//! `merge_secrets` applies to a form, without needing the spec, since the
//! export redacted some values no spec declares. A sentinel with nothing stored
//! behind it is dropped, never stored: the entity is created without that
//! secret, and the receipt lists it as needing one.
//!
//! **Instance-bound settings stay** (§3.4, §3.5): a store that exists keeps its
//! own directory, key and — for a git store — its own URL; a new local store
//! is made at `<data dir>/local-stores/<name>`, a new git store is cloned from
//! its public URL (or the HTTPS URL its SSH one implies) and then checked out
//! at the pinned commit.
//!
//! A registry save (§7.6) is the exception: it plans an instance tree, and
//! [`Executor::apply_instance`] applies what is in it — a store's directory,
//! key and URL, a non-portable setting, a build host — as typed.
//!
//! **Operations run in the plan's order and stop at the first failure.** What
//! was applied up to it stays applied, and [`Applied`] says exactly what that
//! was. Unticked operations are skipped (deferred, §5).
//!
//! What only the running server has — the action, agent-trait, stream and
//! model registries, installing a module, building an application,
//! `reload_all` — is reached through a [`DeployHost`].

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value as Json};
use uuid::Uuid;

use sc_api::schema_edit::{self, ApplyOptions};
use sc_backup::RestoreHost;
use sc_backup::codec::{
    agent_json, application_from_body, application_json, backup_llm_provider_json,
    backup_module_json, backup_stream_json, db_connection_from_body, file_store_from_body,
    library_item_from_body, page_from_body, trigger_from_body, trigger_json, view_from_body,
};
use sc_catalog::{Catalog, Editor};
use sc_error::{Error, Result};
use sc_files::{
    ATTR_SYNC_CONTENTS, CFG_PATH, CFG_PUBLIC_URL, CFG_URL, FileStoreDef, GIT_BACKEND, GitRepo,
    LOCAL_BACKEND, derive_https_url,
};
use sc_types::{Role, SECRET_SENTINEL};

use crate::entity::{Content, Entities, Entity, Kind};
use crate::export::instance_settings;
use crate::plan::{OpBody, Plan, PlannedOp, Step};
use crate::secrets::Registries;
use crate::tree::StructureTree;

/// What a deploy needs from the running server, beyond what a backup restore
/// does ([`RestoreHost`]).
#[async_trait::async_trait]
pub trait DeployHost: RestoreHost {
    /// Load everything again after a deploy (§6 step 9): the catalog, the code
    /// libraries, every application (remounted, or unmounted where its row is
    /// gone), the streams and the modules. Each problem is a sentence; none
    /// stops the deploy, since what was applied is applied.
    async fn reload_all(&self, catalog: &Catalog) -> Vec<String>;

    /// The registries an export redacts secrets by. The default is this
    /// host's actions, stream providers and agent traits, with no module
    /// settings; a server with modules loaded adds theirs.
    fn registries(&self) -> Result<Registries> {
        Ok(Registries {
            actions: self.trigger_dispatcher()?.registry(),
            streams: self.stream_registry()?,
            agent_traits: self.agent_registry()?,
            modules: std::collections::BTreeMap::new(),
        })
    }
}

/// Where a deploy runs, and as whom.
pub struct Executor<'a> {
    /// The installation.
    pub catalog: &'a Catalog,
    /// The server's services.
    pub host: &'a dyn DeployHost,
    /// Who is deploying: a person in the admin UI, or the operator at the CLI.
    pub editor: Editor,
    /// The data directory: where a new local store, a new git store's clone
    /// and the safety backups go (`sc_files::data_dir` on a server).
    pub data_dir: PathBuf,
}

/// What applying a plan did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Applied {
    /// The operations applied, by id, in order.
    pub applied: Vec<String>,
    /// The operations skipped because they were not ticked (§5).
    pub deferred: Vec<String>,
    /// The operations after a failure, which were not attempted.
    pub not_reached: Vec<String>,
    /// The operation that failed, and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Failure>,
    /// Creates of an entity the tree has a secret for and this instance has
    /// not, refused by its save path because the secret is required (an LLM
    /// provider's key). Not a failure: the entity stays incoming until it can
    /// be created — by an admin making it here with its secret, which the
    /// next deploy then matches by name (§3.2, §3.3).
    pub needs_secret: Vec<Failure>,
    /// What went wrong without stopping the deploy: an application that did
    /// not build, a connection that does not answer yet, a store not cloned.
    pub notes: Vec<String>,
}

impl Applied {
    /// The operations that were not applied, for whatever reason.
    pub fn held(&self) -> BTreeSet<String> {
        self.deferred
            .iter()
            .chain(&self.not_reached)
            .chain(self.failed.as_ref().map(|f| &f.op))
            .chain(self.needs_secret.iter().map(|f| &f.op))
            .cloned()
            .collect()
    }
}

/// The operation a deploy stopped at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// Its id.
    pub op: String,
    /// What it was.
    pub summary: String,
    /// Why it failed.
    pub message: String,
}

/// What the operations so far have touched, for the reloads and builds at
/// the end.
#[derive(Default)]
struct Touched {
    modules: bool,
    triggers: bool,
    streams: bool,
    /// Applications (by subdomain) put, or with something in them put.
    applications: BTreeSet<String>,
    /// Stores (by name) whose files were written or whose pin was checked out.
    stores: BTreeSet<String>,
}

impl Executor<'_> {
    /// Apply `plan`'s ticked operations in order, turning `live` into
    /// `target`. The trees are the ones the plan was made from: they say which
    /// application a view is in and which store a file is in.
    pub async fn apply(
        &self,
        plan: &Plan,
        live: &StructureTree,
        target: &StructureTree,
    ) -> Applied {
        self.apply_in(plan, live, target, false).await
    }

    /// Apply a plan of instance trees (a registry save, §7.6): as
    /// [`apply`](Self::apply), with the instance-bound values in `target`
    /// applied rather than kept.
    pub async fn apply_instance(
        &self,
        plan: &Plan,
        live: &StructureTree,
        target: &StructureTree,
    ) -> Applied {
        self.apply_in(plan, live, target, true).await
    }

    async fn apply_in(
        &self,
        plan: &Plan,
        live: &StructureTree,
        target: &StructureTree,
        instance: bool,
    ) -> Applied {
        let trees = Trees {
            live: Entities::of(live),
            target: Entities::of(target),
            instance,
        };
        let mut out = Applied::default();
        let mut touched = Touched::default();
        let mut step = None;
        for op in &plan.operations {
            if out.failed.is_some() {
                out.not_reached.push(op.id.clone());
                continue;
            }
            if !op.ticked {
                out.deferred.push(op.id.clone());
                continue;
            }
            // What the modules supply — tables, actions, frameworks — is live
            // before anything after them names it.
            if step == Some(Step::Modules) && op.step != Step::Modules && touched.modules {
                if let Err(e) = self.host.reload_modules().await {
                    out.notes
                        .push(format!("the modules could not be reloaded: {}", e.causes()));
                }
                touched.modules = false;
            }
            step = Some(op.step);
            match self
                .apply_op(op, &trees, &mut touched, &mut out.notes)
                .await
            {
                Ok(()) => out.applied.push(op.id.clone()),
                Err(e) => {
                    let failure = Failure {
                        op: op.id.clone(),
                        summary: op.summary.clone(),
                        message: e.causes(),
                    };
                    if creates_without_secret(op, plan) {
                        out.notes.push(format!(
                            "the {} was not created: it needs a secret this instance has not \
                             got ({}). Create it here with its secret under the same name, \
                             and the next deploy takes it as the same one",
                            op.summary.trim_start_matches("create the "),
                            failure.message
                        ));
                        out.needs_secret.push(failure);
                    } else {
                        out.failed = Some(failure);
                    }
                }
            }
        }
        self.finish(touched, &mut out.notes).await;
        out
    }

    /// The reloads and builds the operations call for.
    async fn finish(&self, touched: Touched, notes: &mut Vec<String>) {
        let catalog = self.catalog;
        if touched.modules
            && let Err(e) = self.host.reload_modules().await
        {
            notes.push(format!("the modules could not be reloaded: {}", e.causes()));
        }
        if touched.triggers {
            match self.host.trigger_dispatcher() {
                Ok(dispatcher) => {
                    if let Err(e) = dispatcher.reload(catalog).await {
                        notes.push(format!(
                            "the triggers are saved but not live yet: {}",
                            e.causes()
                        ));
                    }
                }
                Err(e) => notes.push(format!("the triggers are not live yet: {}", e.causes())),
            }
        }
        if touched.streams
            && let Err(e) = self.host.reload_streams(catalog).await
        {
            notes.push(format!(
                "the streams are saved but not running yet: {}",
                e.causes()
            ));
        }
        // Every application put, and every one built from a store whose files
        // or checkout moved: a deployed application serves the deployed code.
        let mut build = touched.applications;
        if !touched.stores.is_empty()
            && let Ok(apps) = sc_app::list_applications(catalog).await
        {
            for app in apps {
                let built_from = std::iter::once(&app.framework)
                    .chain(&app.extra_frameworks)
                    .filter_map(|f| f.config.get("store").and_then(Json::as_str))
                    .any(|store| touched.stores.contains(store));
                if built_from {
                    build.insert(app.subdomain.clone());
                }
            }
        }
        for subdomain in build {
            let app = match sc_app::load_application_by_subdomain(catalog, &subdomain).await {
                Ok(Some(app)) => app,
                Ok(None) => continue,
                Err(e) => {
                    notes.push(format!("application `{subdomain}`: {}", e.causes()));
                    continue;
                }
            };
            if let Err(e) = self.host.build_and_mount(app).await {
                notes.push(format!(
                    "application `{subdomain}` is deployed but not serving the new version \
                     yet — build it from the Applications screen: {}",
                    e.causes()
                ));
            }
        }
    }

    async fn apply_op(
        &self,
        op: &PlannedOp,
        trees: &Trees,
        touched: &mut Touched,
        notes: &mut Vec<String>,
    ) -> Result<()> {
        match &op.body {
            OpBody::Schema { batch, ids, .. } => {
                if batch.is_empty() {
                    return Err(Error::invalid(format!("a deploy cannot {}", op.summary)));
                }
                let options = ApplyOptions {
                    editor: self.editor,
                    ids: ids.clone(),
                    ..ApplyOptions::default()
                };
                let result = schema_edit::apply(self.catalog, batch, &options).await?;
                notes.extend(result.notes);
                Ok(())
            }
            // What is stored now is read back by id rather than taken from
            // the live tree, which has its secrets masked.
            OpBody::Put { target, .. } => self.put(target, trees, touched, notes).await,
            OpBody::Delete { live } => self.delete(live, trees, touched, notes).await,
        }
    }

    // --- create and update ----------------------------------------------------------

    async fn put(
        &self,
        target: &Entity,
        trees: &Trees,
        touched: &mut Touched,
        notes: &mut Vec<String>,
    ) -> Result<()> {
        let catalog = self.catalog;
        if target.kind == Kind::StoreFile {
            return self.put_store_file(target, trees, touched).await;
        }
        let mut value = target
            .json()
            .cloned()
            .ok_or_else(|| Error::invalid(format!("`{}` is not JSON", target.path)))?;
        match target.kind {
            Kind::Role => put_role(catalog, &value).await,
            Kind::Setting => put_setting(catalog, &target.name, &value, notes).await,
            Kind::Module => {
                touched.modules = true;
                self.put_module(value).await
            }
            Kind::DbConnection => {
                let id = sc_catalog::DbConnectionId(entity_id(target)?);
                let existing = sc_catalog::load_db_connection(catalog, id).await?;
                unmask(&mut value, None);
                let mut def = db_connection_from_body(id, &value, existing.as_ref())?;
                if let Some(existing) = &existing {
                    def.attributes = existing.attributes.clone();
                }
                sc_catalog::check_db_connection_saveable(catalog, &def).await?;
                sc_catalog::save_db_connection(catalog, &def).await?;
                if let Err(e) = sc_catalog::connect_db_connection(catalog, &def).await {
                    notes.push(format!(
                        "the database connection `{}` is saved but not connected: {}",
                        def.name,
                        e.causes()
                    ));
                }
                catalog.reload().await
            }
            Kind::FileStore => {
                self.put_file_store(target, value, trees.instance, touched, notes)
                    .await
            }
            Kind::CodeLibrary => {
                let library = sc_app::code_library_from_json(entity_id(target)?, &value)?;
                sc_app::save_code_library_checked(catalog, &library).await
            }
            Kind::LlmProvider => put_llm_provider(catalog, target, value, notes).await,
            Kind::McpServer => {
                let id = sc_mcp::McpServerDefId(entity_id(target)?);
                let headers = sc_mcp::load_mcp_server(catalog, id)
                    .await?
                    .map(|s| s.headers)
                    .unwrap_or_default();
                let def = sc_backup::codec::server_from_body(id, &value, &headers)?;
                sc_mcp::save_mcp_server(catalog, &def).await.map(|_| ())
            }
            Kind::Trigger => {
                touched.triggers = true;
                self.put_trigger(target, value).await
            }
            Kind::Stream => {
                touched.streams = true;
                let id = sc_stream::StreamId(entity_id(target)?);
                let stored = sc_stream::load_stream(catalog, id).await?;
                unmask(&mut value, stored.as_ref().map(backup_stream_json).as_ref());
                let stream = sc_backup::codec::stream_from_body(object(&value, target)?)?;
                let registry = self.host.stream_registry()?;
                sc_stream::save_stream(catalog, registry.as_ref(), &stream).await
            }
            Kind::Agent => {
                let id = sc_agent::AgentId(entity_id(target)?);
                let stored = sc_agent::load_agent(catalog, id).await?;
                unmask(
                    &mut value,
                    stored.as_ref().map(|a| agent_json(a, None)).as_ref(),
                );
                let agent = sc_backup::codec::agent_from_body(id, &value)?;
                sc_agent::save_agent(catalog, self.host.agent_registry()?.as_ref(), &agent).await
            }
            Kind::Dataset => {
                let id = sc_dataset::DatasetId(entity_id(target)?);
                let def = sc_backup::codec::dataset_def_from_input(&value, id)?;
                sc_dataset::save_dataset(catalog, &def).await
            }
            Kind::Model => {
                let obj = object(&value, target)?;
                let model = sc_backup::codec::model_from_body(catalog, obj).await?;
                let shape = self.host.dataset_shape(&model.dataset).await;
                let registry = self.host.model_registry()?;
                sc_model::save_model(catalog, registry.as_ref(), &model, shape.as_ref()).await?;
                if let Some(Json::Object(view_state)) = value.get("view_state")
                    && !view_state.is_empty()
                {
                    sc_model::patch_model_view_state(catalog, model.id, view_state).await?;
                }
                Ok(())
            }
            Kind::Workspace => put_workspace(catalog, target, &value).await,
            Kind::Application => {
                let id = sc_app::AppId(entity_id(target)?);
                let stored = sc_app::load_application(catalog, id).await?;
                unmask(&mut value, stored.as_ref().map(application_json).as_ref());
                let app = application_from_body(id, &value)?;
                let saved = sc_app::save_application(catalog, &app).await?;
                touched.applications.insert(saved.subdomain);
                Ok(())
            }
            Kind::View | Kind::Page | Kind::LibraryItem | Kind::Translation => {
                let app = trees.application(catalog, target, true).await?;
                touched.applications.insert(app.subdomain.clone());
                put_in_application(catalog, &app, target, &value).await
            }
            Kind::Checklist => self.put_checklist(target, &value).await,
            Kind::BuildHost => put_build_host(catalog, target, &value).await,
            Kind::Table => self.put_provided_table(target, value).await,
            Kind::Users | Kind::StoreFile | Kind::Other => Err(Error::invalid(format!(
                "`{}` cannot be deployed as a whole entity",
                target.path
            ))),
        }
    }

    /// A provided table (the planner sends every other table as schema
    /// batches): created through the admin API's own path with its id, or
    /// renamed and reconfigured; then its settings, as any table's.
    async fn put_provided_table(&self, target: &Entity, value: Json) -> Result<()> {
        let catalog = self.catalog;
        let obj = object(&value, target)?;
        let provided = obj
            .get("provider")
            .filter(|p| p.is_object())
            .ok_or_else(|| Error::invalid(format!("`{}` names no provider", target.path)))?;
        let (module, provider) = (text(provided, "module"), text(provided, "provider"));
        let id = sc_catalog::TableMetaId(entity_id(target)?);
        let existing = match sc_catalog::load_table_meta(catalog, id).await? {
            Some(meta) => Some(meta),
            None => sc_catalog::load_table_meta_by_name(catalog, &target.name).await?,
        };
        let stored = existing.as_ref().and_then(sc_catalog::TableMeta::provider);
        let mut configuration = provided
            .get("configuration")
            .cloned()
            .unwrap_or_else(|| Json::Object(Map::new()));
        unmask(
            &mut configuration,
            stored
                .as_ref()
                .map(|d| Json::Object(d.configuration.clone()))
                .as_ref(),
        );
        let configuration = configuration.as_object().cloned().unwrap_or_default();
        let name = target.name.as_str();
        match (&existing, &stored) {
            (None, _) => {
                sc_api::provided_tables::create_with_id(
                    catalog,
                    id,
                    name,
                    &module,
                    &provider,
                    configuration,
                )
                .await?;
            }
            (Some(meta), None) => {
                return Err(Error::invalid(format!(
                    "`{}` is a database table here and a provided one in the target; a deploy \
                     cannot turn one into the other — drop or rename it here first",
                    meta.table_name
                )));
            }
            (Some(meta), Some(def)) => {
                if def.module != module || def.provider != provider {
                    return Err(Error::invalid(format!(
                        "`{}` is served by `{}` of `{}` here and by `{provider}` of `{module}` \
                         in the target; a deploy does not move a table to another provider — \
                         delete it here, then deploy again",
                        meta.table_name, def.provider, def.module
                    )));
                }
                if meta.table_name != name {
                    self.schema(vec![schema_edit::Operation::RenameTable {
                        table: meta.table_name.clone(),
                        to: name.to_owned(),
                    }])
                    .await?;
                }
                if def.configuration != configuration {
                    sc_api::provided_tables::configure(catalog, name, configuration).await?;
                }
            }
        }
        let settings = sc_backup::codec::table_settings_from_body(obj)?;
        self.schema(vec![schema_edit::Operation::AlterTable {
            table: name.to_owned(),
            settings,
        }])
        .await
    }

    /// Run one schema batch as the deploying editor.
    async fn schema(&self, batch: Vec<schema_edit::Operation>) -> Result<()> {
        let options = ApplyOptions {
            editor: self.editor,
            ..ApplyOptions::default()
        };
        schema_edit::apply(self.catalog, &batch, &options).await?;
        Ok(())
    }

    async fn put_module(&self, mut value: Json) -> Result<()> {
        let catalog = self.catalog;
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned();
        let existing = sc_module::load_module_by_name(catalog, &name).await?;
        unmask(
            &mut value,
            existing.as_ref().map(backup_module_json).as_ref(),
        );
        let mut module = sc_backup::codec::module_from_backup(&value)?;
        let reinstall = existing.as_ref().is_none_or(|e| {
            e.source != module.source
                || e.location != module.location
                || e.version != module.version
        });
        if let Some(existing) = &existing {
            module.id = existing.id;
            module.attributes = existing.attributes.clone();
        } else if sc_module::load_module(catalog, module.id).await?.is_some() {
            module.id = sc_module::ModuleId::new();
        }
        if reinstall {
            self.host.modules_installed()?;
            let package = self
                .host
                .install_module_package(module.language, module.source, &module.location)
                .await?;
            module.name = package.name;
            module.version = Some(package.version);
        }
        sc_module::save_module(catalog, &module).await
    }

    async fn put_file_store(
        &self,
        target: &Entity,
        mut value: Json,
        instance: bool,
        touched: &mut Touched,
        notes: &mut Vec<String>,
    ) -> Result<()> {
        let catalog = self.catalog;
        let id = sc_files::FileStoreDefId(entity_id(target)?);
        let existing = sc_catalog::load_file_store(catalog, id).await?;
        let stored_config = existing.as_ref().map(|e| Json::Object(e.config.clone()));
        if let Some(config) = value.get_mut("config") {
            unmask(config, stored_config.as_ref());
        }
        let mut def = file_store_from_body(id, &value)?;
        match &existing {
            // A registry save: this instance's own settings are as typed.
            Some(existing) if instance => {
                def.attributes = existing.attributes.clone();
                set_clone_path(&mut def, &value);
            }
            None if instance && has_own_place(&def) => set_clone_path(&mut def, &value),
            Some(existing) => {
                // This instance's own: its directory, key and — for a git
                // store — the URL it reaches the remote by (§3.5).
                def.attributes = existing.attributes.clone();
                let mut own: Vec<&str> = instance_settings(&def.backend).to_vec();
                if def.backend == GIT_BACKEND {
                    own.push(CFG_URL);
                }
                for key in own {
                    match existing.config.get(key) {
                        Some(v) => def.config.insert(key.to_owned(), v.clone()),
                        None => def.config.remove(key),
                    };
                }
            }
            None => self.place_new_store(&mut def)?,
        }
        match value.get("sync_contents").and_then(Json::as_str) {
            None | Some("auto") => {
                def.attributes.remove(ATTR_SYNC_CONTENTS);
            }
            Some(other) => {
                def.attributes.insert(
                    ATTR_SYNC_CONTENTS.to_owned(),
                    Json::String(other.to_owned()),
                );
            }
        }
        sc_catalog::check_file_store_saveable(catalog, &def).await?;
        if existing.is_none() && def.backend == GIT_BACKEND {
            // A clone that fails still leaves a saved store: the definition is
            // what the admin repairs (a deploy key, a URL), and the receipt
            // says it needs credentials.
            if let Err(e) = sc_backup::codec::create_backend_resources(&mut def).await {
                notes.push(format!(
                    "the git store `{}` is defined but not cloned: {}",
                    def.name,
                    e.causes()
                ));
            }
        }
        sc_catalog::save_file_store(catalog, &def).await?;
        if def.backend == GIT_BACKEND
            && let Some(commit) = value.get("commit").and_then(Json::as_str)
        {
            let repo = GitRepo::from_def(&def)?;
            if repo.is_cloned() && repo.head_commit().await?.as_deref() != Some(commit) {
                repo.checkout_pinned(commit).await?;
                touched.stores.insert(def.name.clone());
            }
        }
        if let Err(e) = sc_catalog::connect_file_store_def(catalog, &def) {
            notes.push(format!(
                "the file store `{}` is saved but not connected: {}",
                def.name,
                e.causes()
            ));
        }
        Ok(())
    }

    /// Where a store new to this instance goes: a local store at
    /// `<data dir>/local-stores/<name>`, a git store cloned into
    /// `<data dir>/git-stores/<name>` from its public URL, else the anonymous
    /// HTTPS URL its URL implies, else its URL (§3.5).
    fn place_new_store(&self, def: &mut FileStoreDef) -> Result<()> {
        let segment = sc_files::path_safe(&def.name);
        match def.backend.as_str() {
            LOCAL_BACKEND => {
                let dir = self.data_dir.join("local-stores").join(segment);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| Error::file(format!("creating {}: {e}", dir.display())))?;
                def.config.insert(
                    CFG_PATH.to_owned(),
                    Json::String(dir.to_string_lossy().into_owned()),
                );
            }
            GIT_BACKEND => {
                let setting = |k: &str| {
                    def.setting(k)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let url = setting(CFG_PUBLIC_URL)
                    .or_else(|| setting(CFG_URL).and_then(|u| derive_https_url(&u)))
                    .or_else(|| setting(CFG_URL));
                if let Some(url) = url {
                    def.config.insert(CFG_URL.to_owned(), Json::String(url));
                }
                let dir = self.data_dir.join("git-stores").join(segment);
                sc_files::record_clone_path(def, &dir);
            }
            _ => {}
        }
        Ok(())
    }

    async fn put_store_file(
        &self,
        target: &Entity,
        trees: &Trees,
        touched: &mut Touched,
    ) -> Result<()> {
        let Content::File {
            bytes: Some(bytes), ..
        } = &target.content
        else {
            return Err(Error::invalid(format!(
                "`{}` has no content to write",
                target.path
            )));
        };
        let store = trees.store_of(&trees.target, target)?;
        let path = self.store_file_path(&store, &target.name).await?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::file(format!("creating {}: {e}", parent.display())))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| Error::file(format!("writing {}: {e}", path.display())))?;
        touched.stores.insert(store);
        Ok(())
    }

    /// Where the file `name` of the local store `store` is on this disk.
    async fn store_file_path(&self, store: &str, name: &str) -> Result<PathBuf> {
        let def = sc_catalog::load_file_store_by_name(self.catalog, store)
            .await?
            .ok_or_else(|| Error::not_found(format!("there is no file store `{store}` here")))?;
        let root = def
            .setting(CFG_PATH)
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "the file store `{store}` is not a local store with a directory, so its synced \
                 files have nowhere to go"
                ))
            })?;
        let relative = Path::new(name);
        if !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(Error::invalid(format!(
                "`{name}` is not a path inside the store"
            )));
        }
        Ok(Path::new(root).join(relative))
    }

    async fn put_trigger(&self, target: &Entity, mut value: Json) -> Result<()> {
        let catalog = self.catalog;
        let id = sc_action::TriggerId(entity_id(target)?);
        let stored = sc_action::load_trigger(catalog, id).await?;
        let mut workflow = match &mut value {
            Json::Object(map) => map.remove("workflow").filter(|w| !w.is_null()),
            _ => None,
        };
        unmask(
            &mut value,
            stored.as_ref().map(|t| trigger_json(t, None)).as_ref(),
        );
        let trigger = trigger_from_body(id, &value)?;
        let dispatcher = self.host.trigger_dispatcher()?;
        sc_action::save_trigger(catalog, &dispatcher.registry(), &trigger).await?;
        let Some(steps) = workflow.as_mut() else {
            return Ok(());
        };
        let current = match sc_workflow::current_workflow(catalog, id).await? {
            Some(current) => Some(
                serde_json::to_value(&current)
                    .map_err(|e| Error::msg(format!("workflow `{}`: {e}", trigger.name)))?,
            ),
            None => None,
        };
        unmask(steps, current.as_ref());
        let comparable = |w: &Json| {
            let mut w = w.clone();
            if let Json::Object(map) = &mut w {
                map.remove("id");
                map.remove("version");
            }
            w
        };
        if current.as_ref().map(comparable) == Some(comparable(steps)) {
            return Ok(());
        }
        let workflow = sc_backup::codec::workflow_from_document(id, steps)?;
        sc_workflow::validate_workflow(
            catalog,
            &dispatcher.registry(),
            &workflow,
            trigger.channel.as_deref(),
        )
        .await?;
        sc_workflow::save_workflow(catalog, &workflow, "deployed", None).await?;
        Ok(())
    }

    async fn put_checklist(&self, target: &Entity, value: &Json) -> Result<()> {
        let id = entity_id(target)?;
        let mut input = sc_check::ChecklistInput::new(text(value, "name"));
        input.description = text(value, "description");
        input.application = value
            .get("application")
            .and_then(Json::as_str)
            .and_then(|a| Uuid::parse_str(a).ok());
        input.enabled = value.get("enabled").and_then(Json::as_bool).unwrap_or(true);
        input.attributes = value
            .get("attributes")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default();
        let mut steps = Vec::new();
        for step in value
            .get("steps")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let mut draft = sc_check::StepDraft::new(text(step, "type"));
            draft.params = step
                .get("params")
                .and_then(Json::as_object)
                .cloned()
                .unwrap_or_default();
            draft.label = text(step, "label");
            draft.capture = step
                .get("capture")
                .and_then(Json::as_str)
                .map(str::to_owned);
            steps.push(draft);
        }
        sc_check::edit::put_checklist(
            self.catalog,
            &sc_check::StepRegistry::installed(),
            &self.check_editor(),
            id,
            input,
            steps,
        )
        .await
        .map(|_| ())
    }

    /// The deploying person, as a checklist edit records them; the operator at
    /// the CLI is nobody's account.
    fn check_editor(&self) -> sc_check::Editor {
        sc_check::Editor::Human(self.editor.person_id().unwrap_or_else(Uuid::nil))
    }

    // --- delete -------------------------------------------------------------------

    async fn delete(
        &self,
        live: &Entity,
        trees: &Trees,
        touched: &mut Touched,
        notes: &mut Vec<String>,
    ) -> Result<()> {
        let catalog = self.catalog;
        let mut gone = |found: bool| {
            if !found {
                notes.push(format!("`{}` was already gone", live.key()));
            }
        };
        match live.kind {
            Kind::Role => {
                let level = live
                    .json()
                    .and_then(|v| v.get("role"))
                    .and_then(Json::as_i64)
                    .and_then(|l| Role::try_from(l).ok())
                    .ok_or_else(|| Error::invalid("a role must have a level"))?;
                gone(sc_auth::delete_role(catalog, level).await?);
            }
            Kind::Setting => {
                let key = live
                    .name
                    .split_once('.')
                    .map_or(live.name.as_str(), |(_, k)| k);
                sc_config::delete_config(catalog, key).await?;
            }
            Kind::Module => {
                touched.modules = true;
                if let Some(module) = sc_module::load_module_by_name(catalog, &live.name).await? {
                    gone(sc_module::delete_module(catalog, module.id).await?);
                }
            }
            Kind::DbConnection => {
                let id = sc_catalog::DbConnectionId(entity_id(live)?);
                gone(sc_catalog::delete_db_connection(catalog, id).await?);
            }
            Kind::FileStore => {
                let id = sc_files::FileStoreDefId(entity_id(live)?);
                gone(sc_catalog::delete_file_store(catalog, id, &[]).await?);
            }
            Kind::StoreFile => {
                let store = trees.store_of(&trees.live, live)?;
                let path = self.store_file_path(&store, &live.name).await?;
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => gone(false),
                    Err(e) => {
                        return Err(Error::file(format!("deleting {}: {e}", path.display())));
                    }
                }
                touched.stores.insert(store);
            }
            Kind::CodeLibrary => {
                gone(sc_catalog::delete_code_library(catalog, entity_id(live)?).await?);
            }
            Kind::LlmProvider => {
                let id = sc_llm::LlmProviderDefId(entity_id(live)?);
                gone(sc_llm::delete_llm_provider(catalog, id, &[]).await?);
            }
            Kind::McpServer => {
                let id = sc_mcp::McpServerDefId(entity_id(live)?);
                gone(sc_mcp::delete_mcp_server(catalog, id, &[]).await?);
            }
            Kind::Trigger => {
                touched.triggers = true;
                let id = sc_action::TriggerId(entity_id(live)?);
                gone(sc_action::delete_trigger(catalog, id).await?);
                sc_workflow::delete_workflow_versions(catalog, id).await?;
            }
            Kind::Stream => {
                touched.streams = true;
                let id = sc_stream::StreamId(entity_id(live)?);
                gone(sc_stream::delete_stream(catalog, id, &[]).await?);
            }
            Kind::Agent => {
                gone(sc_agent::delete_agent(catalog, sc_agent::AgentId(entity_id(live)?)).await?);
            }
            Kind::Dataset => {
                let id = sc_dataset::DatasetId(entity_id(live)?);
                gone(sc_dataset::delete_dataset(catalog, id).await?);
            }
            Kind::Model => {
                let id = sc_model::ModelId(entity_id(live)?);
                let registry = self.host.model_registry()?;
                gone(sc_model::delete_model(catalog, registry.as_ref(), id).await?);
            }
            Kind::Workspace => {
                let id = sc_analytics::WorkspaceId(entity_id(live)?);
                gone(sc_analytics::delete_workspace(catalog, id).await?);
            }
            Kind::Application => {
                let id = sc_app::AppId(entity_id(live)?);
                sc_viewpattern::delete_application_views_pages_and_library(catalog, id).await?;
                sc_app::delete_application_translations(catalog, id).await?;
                gone(sc_app::delete_application(catalog, id).await?);
            }
            Kind::View | Kind::Page | Kind::LibraryItem | Kind::Translation => {
                let app = trees.application(catalog, live, false).await?;
                touched.applications.insert(app.subdomain.clone());
                gone(delete_in_application(catalog, &app, live).await?);
            }
            Kind::Checklist => {
                sc_check::edit::delete_checklist(catalog, &self.check_editor(), entity_id(live)?)
                    .await?;
            }
            Kind::BuildHost => {
                gone(sc_app::delete_build_host(catalog, entity_id(live)?).await?);
            }
            Kind::Table => {
                // A provided table's rows are its provider's: deleting it is
                // forgetting its definition, which is all there is of it here.
                gone(sc_api::provided_tables::forget(catalog, &live.name).await?);
            }
            Kind::Users | Kind::Other => {
                return Err(Error::invalid(format!(
                    "`{}` cannot be deleted as a whole entity",
                    live.path
                )));
            }
        }
        Ok(())
    }
}

/// The two trees a plan was made from, read as entities, for finding the
/// application a view is in or the store a file is in.
struct Trees {
    live: Entities,
    target: Entities,
    /// The trees are instance trees (a registry save).
    instance: bool,
}

impl Trees {
    /// The name of the store the file `e` is in, from `side`'s tree.
    fn store_of(&self, side: &Entities, e: &Entity) -> Result<String> {
        e.scope
            .as_deref()
            .and_then(|scope| side.at(scope))
            .map(|i| side.list[i].name.clone())
            .ok_or_else(|| Error::invalid(format!("`{}` is in no file store", e.path)))
    }

    /// The application `e` is in, as it is here now: the target's (just
    /// saved) for a put, live's for a delete.
    async fn application(
        &self,
        catalog: &Catalog,
        e: &Entity,
        target: bool,
    ) -> Result<sc_app::Application> {
        let side = if target { &self.target } else { &self.live };
        let parent = e
            .scope
            .as_deref()
            .and_then(|scope| side.at(scope))
            .map(|i| &side.list[i])
            .ok_or_else(|| Error::invalid(format!("`{}` is in no application", e.path)))?;
        if let Some(app) = parent.id.as_deref().and_then(|id| Uuid::parse_str(id).ok())
            && let Some(app) = sc_app::load_application(catalog, sc_app::AppId(app)).await?
        {
            return Ok(app);
        }
        sc_app::load_application_by_subdomain(catalog, &parent.name)
            .await?
            .ok_or_else(|| {
                Error::not_found(format!(
                    "the application `{}` that `{}` is in is not here",
                    parent.name, e.path
                ))
            })
    }
}

async fn put_role(catalog: &Catalog, value: &Json) -> Result<()> {
    let level = value
        .get("role")
        .and_then(Json::as_i64)
        .and_then(|n| Role::try_from(n).ok())
        .ok_or_else(|| Error::invalid("a role must have a number between 1 and 100"))?;
    let name = value
        .get("name")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Error::invalid("a role must have a name"))?;
    let existing = sc_auth::list_roles(catalog)
        .await?
        .into_iter()
        .find(|r| r.role == level);
    let mut role = match existing {
        Some(mut role) => {
            role.name = name.to_owned();
            role
        }
        None => sc_auth::RoleDef::new(level, name),
    };
    role.description = text(value, "description").trim().to_owned();
    let second_factor = value
        .get("require_second_factor")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    if second_factor {
        role.attributes.insert(
            sc_auth::ATTR_REQUIRE_SECOND_FACTOR.to_owned(),
            Json::Bool(true),
        );
    } else {
        role.attributes.remove(sc_auth::ATTR_REQUIRE_SECOND_FACTOR);
    }
    sc_auth::save_role(catalog, &role).await
}

async fn put_setting(
    catalog: &Catalog,
    name: &str,
    value: &Json,
    notes: &mut Vec<String>,
) -> Result<()> {
    let key = name.split_once('.').map_or(name, |(_, k)| k);
    if is_sentinel(value) {
        return Ok(());
    }
    // A secret inside a setting (an identity provider's client secret) is
    // the sentinel too.
    let mut value = value.clone();
    unmask(
        &mut value,
        sc_config::stored_config(catalog, key).await?.as_ref(),
    );
    if sc_config::host_config_keys(catalog)
        .iter()
        .any(|k| k == key)
    {
        notes.push(format!(
            "the setting `{key}` is pinned by this host's feldspar.toml and was kept"
        ));
        return Ok(());
    }
    sc_config::set_config(catalog, key, value).await
}

/// A build host, by id (a registry save; build hosts are never in git). Only
/// its name and targets: a new one is added on the Build hosts screen, which
/// shows its token the once it exists.
async fn put_build_host(catalog: &Catalog, target: &Entity, value: &Json) -> Result<()> {
    let targets: Vec<String> = value
        .get("targets")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|t| {
            t.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid("a build host's `targets` must be names"))
        })
        .collect::<Result<_>>()?;
    let updated =
        sc_app::update_build_host(catalog, entity_id(target)?, &text(value, "name"), &targets)
            .await?;
    match updated {
        Some(_) => Ok(()),
        None => Err(Error::invalid(
            "a build host is added on the Build hosts screen, which shows its token once; \
             the registry edits one that is there",
        )),
    }
}

/// A git store's clone path, from an instance tree's `clone_path`.
fn set_clone_path(def: &mut FileStoreDef, value: &Json) {
    match value.get(sc_files::ATTR_CLONE_PATH).and_then(Json::as_str) {
        Some(path) if !path.trim().is_empty() => {
            sc_files::record_clone_path(def, Path::new(path.trim()));
        }
        _ => {
            def.attributes.remove(sc_files::ATTR_CLONE_PATH);
        }
    }
}

/// Whether a new store in an instance tree says where it goes: a local
/// store's directory, a git store's clone path.
fn has_own_place(def: &FileStoreDef) -> bool {
    match def.backend.as_str() {
        LOCAL_BACKEND => def.setting(CFG_PATH).is_some_and(|p| !p.trim().is_empty()),
        _ => false,
    }
}

async fn put_llm_provider(
    catalog: &Catalog,
    target: &Entity,
    mut value: Json,
    notes: &mut Vec<String>,
) -> Result<()> {
    let id = sc_llm::LlmProviderDefId(entity_id(target)?);
    let stored = sc_llm::load_llm_provider(catalog, id).await?;
    let stored_models = match &stored {
        Some(def) => sc_llm::list_llm_models(catalog, def).await?,
        None => Vec::new(),
    };
    unmask(
        &mut value,
        stored
            .as_ref()
            .map(|def| backup_llm_provider_json(def, &stored_models))
            .as_ref(),
    );
    let def = sc_backup::codec::llm_provider_from_body(id, &value)?;
    sc_llm::save_llm_provider(catalog, &def).await?;
    let mut kept = BTreeSet::new();
    for model in value
        .get("models")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        let name = text(model, "name");
        let given = model
            .get("id")
            .and_then(Json::as_str)
            .and_then(|i| Uuid::parse_str(i).ok());
        let here = stored_models
            .iter()
            .find(|m| Some(m.id.0) == given)
            .or_else(|| stored_models.iter().find(|m| m.name == name));
        let id = here.map_or_else(
            || sc_llm::LlmModelDefId(given.unwrap_or_else(Uuid::new_v4)),
            |m| m.id,
        );
        kept.insert(id.0);
        let model = sc_backup::codec::llm_model_from_body(id, def.id, model)?;
        sc_llm::save_llm_model(catalog, &model).await?;
    }
    for model in stored_models.iter().filter(|m| !kept.contains(&m.id.0)) {
        if let Err(e) = sc_llm::delete_llm_model(catalog, model.id, &[]).await {
            notes.push(format!(
                "the model `{}` of the LLM provider `{}` was kept: {}",
                model.name,
                def.name,
                e.causes()
            ));
        }
    }
    Ok(())
}

async fn put_workspace(catalog: &Catalog, target: &Entity, value: &Json) -> Result<()> {
    let id = sc_analytics::WorkspaceId(entity_id(target)?);
    let name = text(value, "name");
    let share_role = value
        .get("share_role")
        .and_then(Json::as_u64)
        .and_then(|r| Role::try_from(r).ok());
    let state = match value.get("state") {
        Some(state @ Json::Object(_)) => state.clone(),
        _ => Json::Object(Map::new()),
    };
    let Some(existing) = sc_analytics::load_workspace(catalog, id).await? else {
        let kind = sc_analytics::WorkspaceKind::parse(&text(value, "kind"))?;
        let mut workspace = sc_analytics::Workspace::new(name, kind, None);
        workspace.id = id;
        workspace.share_role = share_role;
        workspace.application = value
            .get("application")
            .and_then(Json::as_str)
            .and_then(|a| Uuid::parse_str(a).ok());
        workspace.state = state;
        return sc_analytics::create_workspace(catalog, &workspace).await;
    };
    if existing.name != name {
        sc_analytics::rename_workspace(catalog, id, &name).await?;
    }
    if existing.share_role != share_role {
        sc_analytics::share_workspace(catalog, id, share_role).await?;
    }
    if existing.state != state {
        sc_analytics::save_workspace_state(catalog, id, state).await?;
    }
    Ok(())
}

async fn put_in_application(
    catalog: &Catalog,
    app: &sc_app::Application,
    target: &Entity,
    value: &Json,
) -> Result<()> {
    match target.kind {
        Kind::View => {
            let id = sc_viewpattern::ViewId(entity_id(target)?);
            let view = view_from_body(id, app.id, value)?;
            sc_viewpattern::save_view(catalog, &view).await.map(|_| ())
        }
        Kind::Page => {
            let id = sc_viewpattern::PageId(entity_id(target)?);
            let page = page_from_body(id, app.id, value)?;
            sc_viewpattern::save_page(catalog, &page).await.map(|_| ())
        }
        Kind::LibraryItem => {
            let id = sc_viewpattern::LibraryItemId(entity_id(target)?);
            let item = library_item_from_body(id, app.id, value)?;
            sc_viewpattern::save_library_item(catalog, &item)
                .await
                .map(|_| ())
        }
        _ => {
            use sc_app::CatalogStore;
            let locale = sc_i18n::Locale::parse(&target.name)?;
            sc_app::RowCatalogStore::new(app.id)
                .save(catalog, &sc_i18n::Catalog::from_json(locale, value)?)
                .await
        }
    }
}

async fn delete_in_application(
    catalog: &Catalog,
    app: &sc_app::Application,
    live: &Entity,
) -> Result<bool> {
    match live.kind {
        Kind::View => sc_viewpattern::delete_view(catalog, app.id, &live.name).await,
        Kind::Page => sc_viewpattern::delete_page(catalog, app.id, &live.name).await,
        Kind::LibraryItem => {
            let id = sc_viewpattern::LibraryItemId(entity_id(live)?);
            sc_viewpattern::delete_library_item(catalog, app.id, id).await
        }
        _ => {
            use sc_app::CatalogStore;
            let locale = sc_i18n::Locale::parse(&live.name)?;
            sc_app::RowCatalogStore::new(app.id)
                .delete(catalog, &locale)
                .await
        }
    }
}

// --- secrets ------------------------------------------------------------------------

/// Whether `op` creates an entity the plan lists as needing a secret.
fn creates_without_secret(op: &PlannedOp, plan: &Plan) -> bool {
    matches!(&op.body, OpBody::Put { live: None, target } if plan.needs_secrets.contains(&target.key()))
}

fn is_sentinel(value: &Json) -> bool {
    value.as_str() == Some(SECRET_SENTINEL)
}

/// Put back what the tree hides (§3.3): wherever `target` holds the sentinel,
/// take the value `stored` has at the same place. A sentinel with nothing
/// stored behind it is dropped — taken out of its object — so the mask itself
/// is never what gets stored.
///
/// Objects are matched by key; a list's items by their `id`, `name` or
/// `trait` where they have one (a header, a model, an agent's trait), else by
/// position.
pub fn unmask(target: &mut Json, stored: Option<&Json>) {
    match target {
        Json::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let below = stored.and_then(|s| s.get(&key));
                let masked = map.get(&key).is_some_and(is_sentinel);
                if masked {
                    match below.filter(|b| !is_sentinel(b) && !b.is_null()) {
                        Some(found) => {
                            map.insert(key, found.clone());
                        }
                        None => {
                            map.remove(&key);
                        }
                    }
                } else if let Some(value) = map.get_mut(&key) {
                    unmask(value, below);
                }
            }
        }
        Json::Array(items) => {
            let stored = stored.and_then(Json::as_array).map(Vec::as_slice);
            for (i, item) in items.iter_mut().enumerate() {
                let below = stored.and_then(|list| counterpart(item, i, list));
                if is_sentinel(item) {
                    *item = below
                        .filter(|b| !is_sentinel(b))
                        .cloned()
                        .unwrap_or(Json::String(String::new()));
                } else {
                    unmask(item, below);
                }
            }
        }
        _ => {}
    }
}

/// The item of `list` that `item` (at `index`) stands for.
fn counterpart<'a>(item: &Json, index: usize, list: &'a [Json]) -> Option<&'a Json> {
    for key in ["id", "name", "trait"] {
        if let Some(k) = item.get(key).filter(|v| !v.is_null()) {
            return list.iter().find(|s| s.get(key) == Some(k));
        }
    }
    list.get(index)
}

// --- helpers --------------------------------------------------------------------------

/// The entity's UUID, which every kind with an id has in the tree.
fn entity_id(e: &Entity) -> Result<Uuid> {
    e.id.as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| Error::invalid(format!("`{}` has no id", e.path)))
}

fn object<'a>(value: &'a Json, e: &Entity) -> Result<&'a Map<String, Json>> {
    value
        .as_object()
        .ok_or_else(|| Error::invalid(format!("`{}` is not a JSON object", e.path)))
}

fn text(value: &Json, key: &str) -> String {
    value
        .get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_sentinel_takes_the_stored_value_or_is_dropped() {
        let mut target = json!({
            "host": "relay.example.net",
            "password": SECRET_SENTINEL,
            "token": SECRET_SENTINEL,
            "headers": [
                { "name": "X-B", "value": SECRET_SENTINEL },
                { "name": "X-A", "value": SECRET_SENTINEL },
            ],
        });
        let stored = json!({
            "host": "old.example.net",
            "password": "real",
            "headers": [{ "name": "X-A", "value": "a" }, { "name": "X-B", "value": "b" }],
        });
        unmask(&mut target, Some(&stored));
        assert_eq!(
            target,
            json!({
                "host": "relay.example.net",
                "password": "real",
                "headers": [{ "name": "X-B", "value": "b" }, { "name": "X-A", "value": "a" }],
            })
        );

        // A new entity: nothing stored, so nothing masked is kept.
        let mut new = json!({ "config": { "api_key": SECRET_SENTINEL, "model": "m" } });
        unmask(&mut new, None);
        assert_eq!(new, json!({ "config": { "model": "m" } }));
    }
}
