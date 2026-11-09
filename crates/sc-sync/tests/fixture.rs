//! The installation the export tests read: a database with every metadata table
//! bootstrapped, and a helper per entity a test puts in it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value as Json, json};

use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_catalog::{Catalog, DataFieldKind, FileOnConflict, FileOnDelete, FileStoreId};
use sc_error::Result;
use sc_files::FileStoreDef;
use sc_sync::{Registries, StructureTree};
use sc_test_harness::{TestDb, TestServer};

/// A test's installation. Keep it alive for the test: dropping `db` tears the
/// catalog down and deletes the database.
pub struct Installation {
    pub db: TestDb,
    pub catalog: Arc<Catalog>,
    pub registries: Registries,
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
        Ok(Installation {
            db,
            catalog,
            registries: Registries::builtin()?,
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

    /// Run SQL on the database directly, as an admin with `psql` would.
    pub async fn sql(&self, sql: &str) -> Result<()> {
        self.db.execute(sql).await?;
        self.catalog.reload().await
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
