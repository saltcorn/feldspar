//! Backup and restore: the whole installation as a zip (design §16, GOALS
//! "restore a backup").
//!
//! **A backup is the admin API's own JSON, in a zip.** Every metadata record it
//! carries — a table's settings and fields, an application, an agent, a trigger,
//! a role — is written in exactly the shape the admin endpoint that lists it
//! returns, and read back through exactly the parser the endpoint that creates it
//! uses ([`crate::handlers`]'s `*_json` / `*_from_body` pairs). That is the point:
//! a second serialisation of an application would be a second thing to keep in
//! step with the first, and the failure would be silent — a field added to the
//! form and forgotten here, restored as its default a year later. Table *rows*
//! and file *contents* are the two things with no admin-API shape to borrow, and
//! they are written as what they are: a JSON array of row objects, and bytes.
//!
//! The layout inside the zip, one directory per kind:
//!
//! ```text
//! manifest.json                       what this file holds, and what wrote it
//! tables/<table>/table.json           { table, fields } — the overlay + columns
//! tables/<table>/rows.json            [ { column: value, … }, … ]
//! applications/<subdomain>.json       one application
//! applications/<subdomain>/views.json its Saltcorn UI views, when chosen
//! applications/<subdomain>/pages.json its Saltcorn UI pages, when chosen
//! applications/<subdomain>/library.json its Saltcorn UI library, with the views
//! applications/<subdomain>/translations.json { locale: catalogue } — a Saltcorn UI app's
//! file-stores/<store>/store.json      { definition, files: [ { path, mode, meta } ] }
//! file-stores/<store>/files/<path>    the bytes, as they are
//! users.json                          { roles, users, fields } — hashes included
//! modules.json                        [ module, … ] — reinstalled from where they came
//! db-connections.json                 [ connection, … ] — passwords included
//! analytics/datasets.json             [ dataset, … ]
//! analytics/models.json               [ model, … ]
//! analytics/workspaces.json           [ workspace, … ]
//! analytics/fits.json                 { instances, outputs } — fitted model instances
//! analytics/draws/<instance>.json     [ draw row, … ] — one entry per fit that sampled
//! streams.json                        [ stream, … ] — secrets included
//! llm-providers.json                  [ { provider, models: [ model, … ] }, … ] — keys included
//! agents.json                         [ agent, … ]
//! triggers.json                       [ trigger, … ] — a workflow's current steps inside
//! settings/<section>.json             one settings section's stored values
//! ```
//!
//! Three rules the rest of this module implements:
//!
//! - **Nothing is included by accident and nothing is excluded silently.** The
//!   admin picks what goes in ([`Selection`]); what is *available* to pick
//!   ([`Available`]) is described the same way whether it comes from this server
//!   or from a backup file's manifest, which is what lets one dialog drive both
//!   the backup and the restore.
//! - **The choice is remembered as what was left out** ([`BackupPreferences`]), not as
//!   what was ticked. A table created after the choice was made is in the next
//!   backup, because a backup that quietly stopped covering new tables is the
//!   worst way to find out how this works.
//! - **A row's data cannot be included without its table's metadata.** Rows
//!   restored into a table whose columns nobody described would be rows nothing
//!   could read.
//!
//! A **Saltcorn 1** backup can be restored too, and nothing above changes for it:
//! [`v1`] translates such an archive into the layout described here before the
//! restorer sees it, so the upload, the dialog, the selection and the report are
//! the same ones a Feldspar backup goes through.

mod restore;
mod v1;
mod write;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};
use uuid::Uuid;

pub use restore::{RestoreReport, inspect, restore_backup};
pub use write::{available, write_backup};

/// Where a browser posts a selection and gets a zip back.
///
/// Outside the typed endpoint set, because the response is bytes; see the route's
/// own comment in [`crate::router`].
pub const BACKUP_CREATE_ROUTE: &str = "/backup/create";

/// Where a browser hands over a backup file to be inspected. The restore itself is
/// the typed `restoreBackup`, which names what came back from here.
pub const BACKUP_UPLOAD_ROUTE: &str = "/backup/upload";

/// The manifest's file name inside the zip.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The settings section the SSL choice covers — the one section of `_fd_config`
/// a backup carries. Named here because the writer picks the keys out of it and
/// the restorer refuses every key that is not in it: a backup must not be a way to
/// set a setting that has nothing to do with certificates.
pub const SSL_SECTION: &str = "ssl";
/// The `format` a manifest carries, so a zip that is not one of ours is refused
/// by name rather than by a confusing missing-entry error.
///
/// It names the *product*, not the company: a Saltcorn 1 backup says
/// `saltcorn_version` in its own `backup-info.json`, and the two files have to be
/// told apart by something an admin can see when they unzip either of them.
pub const FORMAT: &str = "feldspar-backup";
/// The layout version. The system is a prototype and reads only its own current
/// format (there is no compatibility path, by project policy), so this exists to
/// *refuse* an older or newer file clearly rather than to migrate one.
pub const FORMAT_VERSION: i64 = 1;
/// The Feldspar that wrote the archive, recorded in the manifest beside the
/// format.
///
/// The format version says what the *layout* is; this says what wrote it, which
/// is the question asked of a file that arrives six months later with something
/// odd in it. Saltcorn 1 records `saltcorn_version` in `backup-info.json` for the
/// same reason, and a backup imported from one of those records where it came
/// from beside these two.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long an uploaded backup waits to be restored before it is forgotten.
///
/// The wait is one dialog: upload the file, read what is in it, untick two things,
/// press the button. Half an hour is generous for that and short enough that a
/// forgotten upload does not sit in memory for the life of the process.
const UPLOAD_TTL: Duration = Duration::from_secs(30 * 60);

/// How many uploaded backups are held at once. Two admins, or one admin who
/// picked the wrong file first: more than that is not a workflow, it is a leak.
const MAX_PENDING_UPLOADS: usize = 4;

/// Backups that have been uploaded and not yet restored.
///
/// The restore is deliberately **two requests** — hand over the file, then say
/// what to take from it — because the second cannot be composed until the first
/// has been read. Holding the bytes here is what keeps the archive from being
/// uploaded twice; the alternative (send the file again with the choice) doubles
/// the wait on the one operation an admin is already nervous about.
///
/// Bounded on both axes: an upload expires and the oldest is dropped when a fifth
/// arrives. The bytes never touch disk, so a process that dies loses nothing but
/// an upload the admin can repeat.
#[derive(Default)]
pub struct PendingUploads {
    held: Mutex<Vec<Pending>>,
}

struct Pending {
    id: Uuid,
    at: Instant,
    bytes: Bytes,
}

impl PendingUploads {
    /// A shared, empty holder — one per server.
    pub fn new() -> Arc<PendingUploads> {
        Arc::new(PendingUploads::default())
    }

    /// Keep an uploaded archive, returning the token that names it.
    pub fn keep(&self, bytes: Bytes) -> Result<Uuid> {
        let mut held = self.lock()?;
        held.retain(|pending| pending.at.elapsed() < UPLOAD_TTL);
        while held.len() >= MAX_PENDING_UPLOADS {
            held.remove(0);
        }
        let id = Uuid::new_v4();
        held.push(Pending {
            id,
            at: Instant::now(),
            bytes,
        });
        Ok(id)
    }

    /// Take an archive back out, removing it: a restore consumes its upload, so
    /// pressing the button twice cannot run the restore twice.
    pub fn take(&self, id: Uuid) -> Result<Bytes> {
        let mut held = self.lock()?;
        held.retain(|pending| pending.at.elapsed() < UPLOAD_TTL);
        match held.iter().position(|pending| pending.id == id) {
            Some(index) => Ok(held.remove(index).bytes),
            None => Err(Error::not_found(
                "that uploaded backup is no longer held — upload the file again",
            )),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<Pending>>> {
        self.held
            .lock()
            .map_err(|_| Error::msg("the uploaded-backup lock is poisoned"))
    }
}

/// One thing that can be included or left out, as both dialogs show it.
///
/// `count` is what the admin is really deciding about — 40 000 rows or 3 — and is
/// `None` where counting would cost more than the number is worth (a live file
/// store, which would have to be walked to answer).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The identity: a table name, an application's subdomain, a store's name.
    pub name: String,
    /// What to show: a table's label, an application's name. The name when there
    /// is nothing better.
    pub label: String,
    /// Rows, files, or `None` for "not counted".
    pub count: Option<i64>,
}

impl Item {
    /// An item whose label is its name and whose size is unknown.
    pub fn new(name: impl Into<String>) -> Item {
        let name = name.into();
        Item {
            label: name.clone(),
            name,
            count: None,
        }
    }

    /// Set the label shown beside the name.
    pub fn labelled(mut self, label: impl Into<String>) -> Item {
        let label = label.into();
        if !label.trim().is_empty() {
            self.label = label;
        }
        self
    }

    /// Set the count of rows or files.
    pub fn counting(mut self, count: i64) -> Item {
        self.count = Some(count);
        self
    }

    fn to_json(&self) -> Json {
        serde_json::json!({ "name": self.name, "label": self.label, "count": self.count })
    }

    fn from_json(value: &Json) -> Result<Item> {
        let obj = value
            .as_object()
            .ok_or_else(|| Error::invalid("an available item must be an object"))?;
        let name = obj
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("an available item must have a `name`"))?;
        Ok(Item {
            name: name.to_owned(),
            label: obj
                .get("label")
                .and_then(Json::as_str)
                .filter(|l| !l.trim().is_empty())
                .unwrap_or(name)
                .to_owned(),
            count: obj.get("count").and_then(Json::as_i64),
        })
    }
}

/// Everything that *could* be in a backup — from this server, when the admin is
/// making one, or from a manifest, when they are restoring one.
///
/// One type for both because the dialogs are the same dialog: a list of things
/// with tick boxes, and the difference between "what this server has" and "what
/// this file has" is where the value came from, not what it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Available {
    /// Tables whose metadata can be included, each with its row count.
    pub tables: Vec<Item>,
    /// Applications, keyed by subdomain and labelled with their name.
    pub applications: Vec<Item>,
    /// File stores, each with its file count where one is known.
    pub file_stores: Vec<Item>,
    /// How many user accounts (with their roles) there are to include.
    pub users: i64,
    /// How many installed modules.
    pub modules: i64,
    /// How many connections to other databases.
    pub db_connections: i64,
    /// How many streams.
    pub streams: i64,
    /// How many datasets, models and analytics workspaces — the Analytics
    /// choice, counted separately because the dialog says which.
    pub datasets: i64,
    /// How many models.
    pub models: i64,
    /// How many analytics workspaces.
    pub workspaces: i64,
    /// How many fitted (or failed) model instances, with their draws and output
    /// frames. A separate choice from the models, because they can be large.
    pub fits: i64,
    /// How many LLM providers (each carrying its models).
    pub llm_providers: i64,
    /// How many agents.
    pub agents: i64,
    /// How many triggers.
    pub triggers: i64,
    /// How many Saltcorn UI views, over every application on offer. They travel
    /// inside their applications, so only a chosen application's are included.
    pub views: i64,
    /// How many Saltcorn UI pages, likewise.
    pub pages: i64,
    /// Whether there are SSL settings to include.
    pub ssl: bool,
    /// Whether there are other settings — email, localisation, development — to
    /// include.
    pub settings: bool,
}

impl Available {
    /// The wire/manifest shape.
    pub fn to_json(&self) -> Json {
        serde_json::json!({
            "tables": self.tables.iter().map(Item::to_json).collect::<Vec<_>>(),
            "applications": self.applications.iter().map(Item::to_json).collect::<Vec<_>>(),
            "file_stores": self.file_stores.iter().map(Item::to_json).collect::<Vec<_>>(),
            "users": self.users,
            "modules": self.modules,
            "db_connections": self.db_connections,
            "streams": self.streams,
            "datasets": self.datasets,
            "models": self.models,
            "workspaces": self.workspaces,
            "fits": self.fits,
            "llm_providers": self.llm_providers,
            "agents": self.agents,
            "triggers": self.triggers,
            "views": self.views,
            "pages": self.pages,
            "ssl": self.ssl,
            "settings": self.settings,
        })
    }

    /// Read it back — what a restore does with a manifest's `contents`.
    pub fn from_json(value: &Json) -> Result<Available> {
        let obj = value
            .as_object()
            .ok_or_else(|| Error::invalid("a backup's contents must be an object"))?;
        let count = |key: &str| obj.get(key).and_then(Json::as_i64).unwrap_or(0);
        Ok(Available {
            tables: items(obj, "tables")?,
            applications: items(obj, "applications")?,
            file_stores: items(obj, "file_stores")?,
            users: count("users"),
            modules: count("modules"),
            db_connections: count("db_connections"),
            streams: count("streams"),
            datasets: count("datasets"),
            models: count("models"),
            workspaces: count("workspaces"),
            fits: count("fits"),
            llm_providers: count("llm_providers"),
            agents: count("agents"),
            triggers: count("triggers"),
            views: count("views"),
            pages: count("pages"),
            ssl: obj.get("ssl").and_then(Json::as_bool).unwrap_or(false),
            settings: obj.get("settings").and_then(Json::as_bool).unwrap_or(false),
        })
    }

    /// Whether there is anything for the Analytics choice to carry.
    pub fn has_analytics(&self) -> bool {
        self.datasets > 0 || self.models > 0 || self.workspaces > 0
    }

    fn table_names(&self) -> Vec<String> {
        self.tables.iter().map(|i| i.name.clone()).collect()
    }
}

/// Whether a store-relative path is in (or is) a `node_modules` directory.
///
/// Neither written to a backup nor restored from one. An installed dependency
/// tree is built for the machine it was installed on (native binaries), and its
/// `.bin` entries are symlinks that a zip read through the store holds as copies
/// of their targets, where a relative `import` no longer resolves. A build
/// installs the dependencies when `node_modules` is missing, so leaving it out
/// is how it comes back working.
pub(crate) fn is_installed_dependency(path: &str) -> bool {
    path.split('/').any(|part| part == "node_modules")
}

fn items(obj: &Map<String, Json>, key: &str) -> Result<Vec<Item>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(values)) => values.iter().map(Item::from_json).collect(),
        Some(_) => Err(Error::invalid(format!("`{key}` must be an array"))),
    }
}

/// What one backup (or one restore) includes.
///
/// The lists are names, not indices or ids: a name is what the admin ticked, what
/// the zip is keyed by, and what still means the same thing when the file is
/// restored onto a different server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    /// Tables whose metadata — settings and columns — is included.
    pub tables: Vec<String>,
    /// Tables whose **rows** are included. Always a subset of
    /// [`tables`](Selection::tables): [`Selection::from_json`] narrows it, so no
    /// caller can be handed rows for a table it knows nothing about.
    pub table_data: Vec<String>,
    /// Applications, by subdomain.
    pub applications: Vec<String>,
    /// File stores — their definitions, their files' metadata and their bytes.
    pub file_stores: Vec<String>,
    /// Whether user accounts and roles are included.
    pub users: bool,
    /// Whether installed modules are included — their rows, from which a restore
    /// reinstalls each package.
    pub modules: bool,
    /// Whether connections to other databases are included, passwords and all.
    pub db_connections: bool,
    /// Whether streams are included.
    pub streams: bool,
    /// Whether datasets, models and analytics workspaces are included.
    pub analytics: bool,
    /// Whether fitted model instances are included. Only with
    /// [`analytics`](Selection::analytics): a fit restored without its model
    /// belongs to nothing.
    pub fits: bool,
    /// Whether LLM providers, their models and their API keys are included.
    pub llm_providers: bool,
    /// Whether agents are included.
    pub agents: bool,
    /// Whether triggers — and a workflow trigger's current steps — are
    /// included. A trigger on a table whose metadata is
    /// *not* included is left out even so — see [`Selection::includes_trigger`].
    pub triggers: bool,
    /// Whether the included applications' views are — and, with them, a Saltcorn
    /// UI application's library, whose items the views place. On restore they
    /// **replace** the views (and library) the application has, which is what
    /// makes a second import of one backup leave the views it had rather than
    /// twice as many.
    pub views: bool,
    /// Whether the included applications' pages are, with the same rule.
    pub pages: bool,
    /// Whether the SSL settings are included.
    pub ssl: bool,
    /// Whether the other settings sections are included.
    pub settings: bool,
}

impl Selection {
    /// Everything on offer — the default, and what a restore dialog starts from.
    pub fn everything(available: &Available) -> Selection {
        Selection {
            tables: available.table_names(),
            // Only the tables whose rows there are: a count is `None` for a table
            // whose data is not in the file at all, and offering to restore rows
            // that are not there would be a tick box that does nothing.
            table_data: available
                .tables
                .iter()
                .filter(|item| item.count.is_some())
                .map(|item| item.name.clone())
                .collect(),
            applications: available
                .applications
                .iter()
                .map(|i| i.name.clone())
                .collect(),
            file_stores: available
                .file_stores
                .iter()
                .map(|i| i.name.clone())
                .collect(),
            users: available.users > 0,
            modules: available.modules > 0,
            db_connections: available.db_connections > 0,
            streams: available.streams > 0,
            analytics: available.has_analytics(),
            fits: available.fits > 0 && available.has_analytics(),
            llm_providers: available.llm_providers > 0,
            agents: available.agents > 0,
            triggers: available.triggers > 0,
            views: available.views > 0,
            pages: available.pages > 0,
            ssl: available.ssl,
            settings: available.settings,
        }
    }

    /// Read a selection a browser sent.
    ///
    /// Rows without their table's metadata are dropped here rather than refused:
    /// the dialog cannot produce that combination (its data box is disabled), so
    /// a payload carrying it is a client bug, and the safe reading of "back up
    /// these rows" is "…of the tables you are also describing".
    pub fn from_json(value: &Json) -> Result<Selection> {
        let obj = value
            .as_object()
            .ok_or_else(|| Error::invalid("`include` must be an object"))?;
        let tables = names(obj, "tables")?;
        let table_data = names(obj, "table_data")?
            .into_iter()
            .filter(|t| tables.contains(t))
            .collect();
        Ok(Selection {
            tables,
            table_data,
            applications: names(obj, "applications")?,
            file_stores: names(obj, "file_stores")?,
            users: flag(obj, "users"),
            modules: flag(obj, "modules"),
            db_connections: flag(obj, "db_connections"),
            streams: flag(obj, "streams"),
            analytics: flag(obj, "analytics"),
            // Fits without their models, narrowed for the reason rows without
            // their table are.
            fits: flag(obj, "fits") && flag(obj, "analytics"),
            llm_providers: flag(obj, "llm_providers"),
            agents: flag(obj, "agents"),
            triggers: flag(obj, "triggers"),
            views: flag(obj, "views"),
            pages: flag(obj, "pages"),
            ssl: flag(obj, "ssl"),
            settings: flag(obj, "settings"),
        })
    }

    /// The wire shape.
    pub fn to_json(&self) -> Json {
        serde_json::json!({
            "tables": self.tables,
            "table_data": self.table_data,
            "applications": self.applications,
            "file_stores": self.file_stores,
            "users": self.users,
            "modules": self.modules,
            "db_connections": self.db_connections,
            "streams": self.streams,
            "analytics": self.analytics,
            "fits": self.fits,
            "llm_providers": self.llm_providers,
            "agents": self.agents,
            "triggers": self.triggers,
            "views": self.views,
            "pages": self.pages,
            "ssl": self.ssl,
            "settings": self.settings,
        })
    }

    /// Whether this table's metadata is in.
    pub fn includes_table(&self, table: &str) -> bool {
        self.tables.iter().any(|t| t == table)
    }

    /// Whether this table's rows are in (which implies its metadata is).
    pub fn includes_data(&self, table: &str) -> bool {
        self.includes_table(table) && self.table_data.iter().any(|t| t == table)
    }

    /// Whether a trigger is in: triggers are a single choice, but a trigger on a
    /// table that is being left out is left out with it.
    ///
    /// `channel` is the table for a table event (`insert`/`update`/`delete`) and
    /// something else — or nothing — for every other kind, which is why the
    /// caller passes the table it resolved rather than the raw channel.
    pub fn includes_trigger(&self, table: Option<&str>) -> bool {
        if !self.triggers {
            return false;
        }
        match table {
            Some(table) => self.includes_table(table),
            None => true,
        }
    }

    /// Whether this store's files are in.
    pub fn includes_store(&self, store: &str) -> bool {
        self.file_stores.iter().any(|s| s == store)
    }

    /// Whether this application is in.
    pub fn includes_application(&self, subdomain: &str) -> bool {
        self.applications.iter().any(|a| a == subdomain)
    }

    /// Narrow to what a backup actually holds — the restore's reading of a
    /// selection made against a manifest that may have been edited by hand.
    pub fn intersect(&self, available: &Available) -> Selection {
        let has = |list: &[Item], name: &String| list.iter().any(|i| &i.name == name);
        let tables: Vec<String> = self
            .tables
            .iter()
            .filter(|t| has(&available.tables, t))
            .cloned()
            .collect();
        Selection {
            table_data: self
                .table_data
                .iter()
                .filter(|t| tables.contains(t))
                .cloned()
                .collect(),
            tables,
            applications: self
                .applications
                .iter()
                .filter(|a| has(&available.applications, a))
                .cloned()
                .collect(),
            file_stores: self
                .file_stores
                .iter()
                .filter(|s| has(&available.file_stores, s))
                .cloned()
                .collect(),
            users: self.users && available.users > 0,
            modules: self.modules && available.modules > 0,
            db_connections: self.db_connections && available.db_connections > 0,
            streams: self.streams && available.streams > 0,
            analytics: self.analytics && available.has_analytics(),
            fits: self.fits && self.analytics && available.fits > 0 && available.has_analytics(),
            llm_providers: self.llm_providers && available.llm_providers > 0,
            agents: self.agents && available.agents > 0,
            triggers: self.triggers && available.triggers > 0,
            views: self.views && available.views > 0,
            pages: self.pages && available.pages > 0,
            ssl: self.ssl && available.ssl,
            settings: self.settings && available.settings,
        }
    }
}

/// The remembered choice, stored in `_fd_config` under
/// [`BACKUP_INCLUDE`](sc_config::BACKUP_INCLUDE).
///
/// **Exclusions, not inclusions.** A stored list of what to include would freeze
/// the installation as it was the day the admin tuned the dialog: the table they
/// added next week would be missing from every backup after it, and nothing would
/// say so. Stored as what was *left out*, a new table is in by default and an
/// unticked one stays unticked — including when it is dropped and recreated,
/// which is the behaviour a name-keyed exclusion has and an id-keyed one could
/// not.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupPreferences {
    /// Tables whose metadata is left out.
    pub exclude_tables: Vec<String>,
    /// Tables whose rows are left out (their metadata may still be in).
    pub exclude_table_data: Vec<String>,
    /// Applications left out, by subdomain.
    pub exclude_applications: Vec<String>,
    /// File stores left out.
    pub exclude_file_stores: Vec<String>,
    /// Whether users are included.
    pub users: bool,
    /// Whether modules are included.
    pub modules: bool,
    /// Whether database connections are included.
    pub db_connections: bool,
    /// Whether streams are included.
    pub streams: bool,
    /// Whether datasets, models and workspaces are included.
    pub analytics: bool,
    /// Whether fitted model instances are included — **off** until an admin
    /// ticks it, because a posterior's draws can outweigh everything else.
    pub fits: bool,
    /// Whether LLM providers, their models and their API keys are included.
    pub llm_providers: bool,
    /// Whether agents are included.
    pub agents: bool,
    /// Whether triggers are included.
    pub triggers: bool,
    /// Whether the chosen applications' views are included.
    pub views: bool,
    /// Whether the chosen applications' pages are included.
    pub pages: bool,
    /// Whether the SSL settings are included.
    pub ssl: bool,
    /// Whether the other settings sections are included.
    pub settings: bool,
}

impl Default for BackupPreferences {
    /// Everything: no exclusions, every flag on. What an installation that has
    /// never opened the dialog gets.
    fn default() -> BackupPreferences {
        BackupPreferences {
            exclude_tables: Vec::new(),
            exclude_table_data: Vec::new(),
            exclude_applications: Vec::new(),
            exclude_file_stores: Vec::new(),
            users: true,
            modules: true,
            db_connections: true,
            streams: true,
            analytics: true,
            fits: false,
            llm_providers: true,
            agents: true,
            triggers: true,
            views: true,
            pages: true,
            ssl: true,
            settings: true,
        }
    }
}

impl BackupPreferences {
    /// Read the stored value. Anything missing takes its default, because a
    /// stored preference is a convenience and a half-written one must not stop an
    /// admin taking a backup.
    pub fn from_json(value: &Json) -> BackupPreferences {
        let Some(obj) = value.as_object() else {
            return BackupPreferences::default();
        };
        let list = |key: &str| -> Vec<String> {
            obj.get(key)
                .and_then(Json::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Json::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let on = |key: &str| obj.get(key).and_then(Json::as_bool).unwrap_or(true);
        BackupPreferences {
            exclude_tables: list("exclude_tables"),
            exclude_table_data: list("exclude_table_data"),
            exclude_applications: list("exclude_applications"),
            exclude_file_stores: list("exclude_file_stores"),
            users: on("users"),
            modules: on("modules"),
            db_connections: on("db_connections"),
            streams: on("streams"),
            analytics: on("analytics"),
            // Off unless it was ticked: absent is the default, and the default
            // is to leave the fits out.
            fits: obj.get("fits").and_then(Json::as_bool).unwrap_or(false),
            llm_providers: on("llm_providers"),
            agents: on("agents"),
            triggers: on("triggers"),
            views: on("views"),
            pages: on("pages"),
            ssl: on("ssl"),
            settings: on("settings"),
        }
    }

    /// The stored shape.
    pub fn to_json(&self) -> Json {
        serde_json::json!({
            "exclude_tables": self.exclude_tables,
            "exclude_table_data": self.exclude_table_data,
            "exclude_applications": self.exclude_applications,
            "exclude_file_stores": self.exclude_file_stores,
            "users": self.users,
            "modules": self.modules,
            "db_connections": self.db_connections,
            "streams": self.streams,
            "analytics": self.analytics,
            "fits": self.fits,
            "llm_providers": self.llm_providers,
            "agents": self.agents,
            "triggers": self.triggers,
            "views": self.views,
            "pages": self.pages,
            "ssl": self.ssl,
            "settings": self.settings,
        })
    }

    /// What was left out of `selection`, given what there was to choose from.
    ///
    /// A name that is *not* on offer keeps whatever it had: an admin who unticked
    /// a table on the production server and takes a backup on a copy that has not
    /// got it yet must not have the exclusion erased by the round trip. That is
    /// why the previous preferences are an input.
    pub fn of(
        previous: &BackupPreferences,
        available: &Available,
        selection: &Selection,
    ) -> BackupPreferences {
        let excluded = |offered: &[Item], chosen: &[String], carried: &[String]| -> Vec<String> {
            let mut out: Vec<String> = offered
                .iter()
                .map(|i| i.name.clone())
                .filter(|name| !chosen.contains(name))
                .collect();
            for name in carried {
                if !offered.iter().any(|i| &i.name == name) && !out.contains(name) {
                    out.push(name.clone());
                }
            }
            out.sort();
            out
        };
        BackupPreferences {
            exclude_tables: excluded(
                &available.tables,
                &selection.tables,
                &previous.exclude_tables,
            ),
            exclude_table_data: excluded(
                &available.tables,
                &selection.table_data,
                &previous.exclude_table_data,
            ),
            exclude_applications: excluded(
                &available.applications,
                &selection.applications,
                &previous.exclude_applications,
            ),
            exclude_file_stores: excluded(
                &available.file_stores,
                &selection.file_stores,
                &previous.exclude_file_stores,
            ),
            users: selection.users,
            modules: selection.modules,
            db_connections: selection.db_connections,
            streams: selection.streams,
            analytics: selection.analytics,
            fits: selection.fits,
            llm_providers: selection.llm_providers,
            agents: selection.agents,
            triggers: selection.triggers,
            views: selection.views,
            pages: selection.pages,
            ssl: selection.ssl,
            settings: selection.settings,
        }
    }

    /// The selection these preferences mean for what is on offer *now* — the
    /// dialog's starting state.
    pub fn selection(&self, available: &Available) -> Selection {
        let keep = |offered: &[Item], excluded: &[String]| -> Vec<String> {
            offered
                .iter()
                .map(|i| i.name.clone())
                .filter(|name| !excluded.contains(name))
                .collect()
        };
        let tables = keep(&available.tables, &self.exclude_tables);
        Selection {
            table_data: keep(&available.tables, &self.exclude_table_data)
                .into_iter()
                .filter(|t| tables.contains(t))
                .collect(),
            tables,
            applications: keep(&available.applications, &self.exclude_applications),
            file_stores: keep(&available.file_stores, &self.exclude_file_stores),
            users: self.users && available.users > 0,
            modules: self.modules && available.modules > 0,
            db_connections: self.db_connections && available.db_connections > 0,
            streams: self.streams && available.streams > 0,
            analytics: self.analytics && available.has_analytics(),
            fits: self.fits && self.analytics && available.fits > 0 && available.has_analytics(),
            llm_providers: self.llm_providers && available.llm_providers > 0,
            agents: self.agents && available.agents > 0,
            triggers: self.triggers && available.triggers > 0,
            views: self.views && available.views > 0,
            pages: self.pages && available.pages > 0,
            ssl: self.ssl && available.ssl,
            settings: self.settings && available.settings,
        }
    }
}

/// A required list-of-names field, absent or null meaning the empty list.
fn names(obj: &Map<String, Json>, key: &str) -> Result<Vec<String>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(values)) => values
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::invalid(format!("`{key}` must be a list of names")))
            })
            .collect(),
        Some(_) => Err(Error::invalid(format!("`{key}` must be a list of names"))),
    }
}

/// A flag, absent meaning off. The dialog always sends every flag, so an absent
/// one is a caller that did not ask for the thing.
fn flag(obj: &Map<String, Json>, key: &str) -> bool {
    obj.get(key).and_then(Json::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> Available {
        Available {
            tables: vec![
                Item::new("books").counting(3),
                Item::new("authors").counting(1),
            ],
            applications: vec![Item::new("blog").labelled("The blog")],
            file_stores: vec![Item::new("assets")],
            users: 2,
            modules: 1,
            db_connections: 1,
            streams: 2,
            datasets: 2,
            models: 1,
            workspaces: 1,
            fits: 3,
            llm_providers: 1,
            agents: 1,
            triggers: 4,
            views: 7,
            pages: 1,
            ssl: true,
            settings: true,
        }
    }

    #[test]
    fn everything_but_the_fits_is_the_default() {
        let selection = BackupPreferences::default().selection(&available());
        assert_eq!(
            selection,
            Selection {
                fits: false,
                ..Selection::everything(&available())
            }
        );
        assert_eq!(selection.tables, vec!["books", "authors"]);
        assert_eq!(selection.table_data, vec!["books", "authors"]);
        assert!(selection.users && selection.agents && selection.triggers && selection.ssl);
        assert!(selection.llm_providers && selection.modules && selection.db_connections);
        assert!(selection.streams && selection.analytics && selection.settings);
        assert!(selection.views && selection.pages);
        // Off until ticked — and remembered once it is.
        assert!(!selection.fits);
        let ticked = Selection {
            fits: true,
            ..selection
        };
        let prefs = BackupPreferences::of(&BackupPreferences::default(), &available(), &ticked);
        let read = BackupPreferences::from_json(&prefs.to_json());
        assert!(read.selection(&available()).fits);
        // The restore dialog offers everything a file holds, fits included.
        assert!(Selection::everything(&available()).fits);
    }

    /// Fits belong to models: neither a payload nor a stored choice can carry
    /// them without the Analytics choice.
    #[test]
    fn fits_are_never_included_without_their_models() {
        let read = Selection::from_json(&serde_json::json!({ "fits": true })).unwrap();
        assert!(!read.fits);
        let asked = Selection {
            analytics: false,
            fits: true,
            ..Selection::everything(&available())
        };
        assert!(!asked.intersect(&available()).fits);
    }

    /// The round trip the dialog makes: untick two things, save, come back.
    #[test]
    fn a_choice_is_remembered_as_what_was_left_out() {
        let available = available();
        let mut selection = BackupPreferences::default().selection(&available);
        selection.tables.retain(|t| t != "authors");
        selection.table_data.retain(|t| t != "authors");
        selection.table_data.retain(|t| t != "books");
        selection.agents = false;

        let prefs = BackupPreferences::of(&BackupPreferences::default(), &available, &selection);
        assert_eq!(prefs.exclude_tables, vec!["authors"]);
        assert_eq!(prefs.exclude_table_data, vec!["authors", "books"]);
        assert!(!prefs.agents);
        assert!(prefs.users);

        // Stored and read back, it is the same selection.
        let read = BackupPreferences::from_json(&prefs.to_json());
        assert_eq!(read, prefs);
        assert_eq!(read.selection(&available), selection);
    }

    /// The reason exclusions are stored rather than inclusions.
    #[test]
    fn a_table_created_after_the_choice_is_included() {
        let prefs = BackupPreferences {
            exclude_tables: vec!["authors".to_owned()],
            ..BackupPreferences::default()
        };
        let mut available = available();
        available.tables.push(Item::new("reviews").counting(0));
        let selection = prefs.selection(&available);
        assert_eq!(selection.tables, vec!["books", "reviews"]);
        assert_eq!(selection.table_data, vec!["books", "reviews"]);
    }

    /// An exclusion for something this server does not have must survive being
    /// round-tripped through a dialog that could not show it.
    #[test]
    fn an_exclusion_for_something_absent_is_kept() {
        let previous = BackupPreferences {
            exclude_tables: vec!["elsewhere".to_owned()],
            ..BackupPreferences::default()
        };
        let available = available();
        let selection = previous.selection(&available);
        let prefs = BackupPreferences::of(&previous, &available, &selection);
        assert_eq!(prefs.exclude_tables, vec!["elsewhere"]);
    }

    #[test]
    fn rows_cannot_be_included_without_their_table() {
        let selection = Selection::from_json(&serde_json::json!({
            "tables": ["books"],
            "table_data": ["books", "authors"],
            "users": true,
        }))
        .expect("a selection");
        assert_eq!(selection.table_data, vec!["books"]);
        assert!(selection.includes_data("books"));
        assert!(!selection.includes_data("authors"));
        // Absent flags are off: this payload asked for users and nothing else.
        assert!(selection.users);
        assert!(!selection.agents && !selection.ssl);
    }

    #[test]
    fn a_trigger_on_an_excluded_table_is_excluded_with_it() {
        let selection = Selection {
            tables: vec!["books".to_owned()],
            triggers: true,
            ..Selection::default()
        };
        assert!(selection.includes_trigger(Some("books")));
        assert!(!selection.includes_trigger(Some("authors")));
        // A trigger that is not on a table at all — a schedule, a login hook —
        // travels with the single triggers choice.
        assert!(selection.includes_trigger(None));
    }

    #[test]
    fn a_selection_is_narrowed_to_what_the_file_holds() {
        let available = available();
        let asked = Selection {
            tables: vec!["books".to_owned(), "gone".to_owned()],
            table_data: vec!["gone".to_owned()],
            applications: vec!["blog".to_owned(), "absent".to_owned()],
            file_stores: vec!["assets".to_owned()],
            users: true,
            modules: true,
            db_connections: true,
            streams: true,
            analytics: true,
            fits: true,
            llm_providers: true,
            agents: true,
            triggers: true,
            views: true,
            pages: true,
            ssl: true,
            settings: true,
        };
        let narrowed = asked.intersect(&available);
        assert_eq!(narrowed.tables, vec!["books"]);
        assert!(narrowed.table_data.is_empty());
        assert_eq!(narrowed.applications, vec!["blog"]);

        // …and against a manifest holding nothing, the flags go off too.
        let empty = asked.intersect(&Available::default());
        assert!(!empty.users && !empty.agents && !empty.triggers && !empty.ssl);
        assert!(!empty.views && !empty.pages && !empty.llm_providers);
        assert!(!empty.modules && !empty.db_connections && !empty.streams);
        assert!(!empty.analytics && !empty.fits && !empty.settings);
    }

    /// A backup can hold a table's definition and not its rows, and the restore
    /// dialog must not offer to put back rows that are not in the file.
    #[test]
    fn a_table_with_no_rows_in_the_file_offers_none() {
        let available = Available {
            tables: vec![
                Item::new("books").counting(3),
                // No count at all: the writer records one only when it wrote the
                // rows.
                Item::new("authors"),
            ],
            ..Available::default()
        };
        let selection = Selection::everything(&available);
        assert_eq!(selection.tables, vec!["books", "authors"]);
        assert_eq!(selection.table_data, vec!["books"]);
    }

    #[test]
    fn what_is_available_survives_the_manifest() {
        let available = available();
        let read = Available::from_json(&available.to_json()).expect("contents");
        assert_eq!(read, available);
        assert_eq!(read.applications[0].label, "The blog");
        assert_eq!(read.tables[0].count, Some(3));
    }
}
