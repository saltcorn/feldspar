//! What answers a module's `apps` asks (TODO-app-constructor AC12.3):
//! applications, their builds and the files of their projects —
//! `req.feldspar.apps` in a route and `ctx.apps` in a service.
//!
//! **Held to the `apps` permission** an admin granted the module
//! ([`AppsGrant`]): `read` reads applications and their builds; `build` also
//! writes an application's project and builds it, which runs the project's own
//! scripts. With none, every ask is refused by name.
//!
//! **Administrators only.** Applications are the admin's to read and build, so
//! at a route only an administrator's call is answered; a service asks as the
//! module, with what it was granted.
//!
//! **Builds run here**, in a task of their own, as the Build button's do (the
//! client brought up to date, the bundle built and mounted, the checklists that
//! follow a build). A module starts one, then waits on it or reads how it
//! ended. The last build of each application is kept in this process only:
//! after a restart there is none until the next.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sc_app::{AppId, Application};
use sc_catalog::{Catalog, Principal};
use sc_error::{Error, Result};
use sc_module::{AppsGrant, Ask};
use serde_json::{Value as Json, json};
use tokio::sync::watch;

use crate::apps::AppMounts;

/// The longest a `wait_build` holds its call.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// The server's half of `apps`: one per server, a surface per call.
#[derive(Clone)]
pub struct ModuleAppHost {
    shared: Arc<SharedAppState>,
}

/// What the host and every call's [`AppsAsk`] share: the application mounts a
/// build goes through, and each application's last build.
struct SharedAppState {
    catalog: Arc<Catalog>,
    /// The application mounts a build goes through, set once the server has
    /// them (they hold the modules, so this cannot be a strong reference).
    mounts: RwLock<Weak<AppMounts>>,
    /// The last build of each application, by id.
    builds: Mutex<HashMap<uuid::Uuid, Build>>,
}

/// One build: how it stands, and how to learn it has finished.
#[derive(Clone)]
struct Build {
    state: BuildState,
    log: String,
    error: Option<String>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    finished: watch::Receiver<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BuildState {
    Running,
    Succeeded,
    Failed,
}

impl BuildState {
    fn as_str(self) -> &'static str {
        match self {
            BuildState::Running => "running",
            BuildState::Succeeded => "succeeded",
            BuildState::Failed => "failed",
        }
    }
}

impl Build {
    fn json(&self) -> Json {
        json!({
            "state": self.state.as_str(),
            "log": self.log,
            "error": self.error,
            "started_at": self.started_at.to_rfc3339(),
            "finished_at": self.finished_at.map(|t| t.to_rfc3339()),
        })
    }
}

impl ModuleAppHost {
    pub fn new(catalog: &Arc<Catalog>) -> ModuleAppHost {
        ModuleAppHost {
            shared: Arc::new(SharedAppState {
                catalog: Arc::clone(catalog),
                mounts: RwLock::new(Weak::new()),
                builds: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The mounts a module's build goes through.
    pub fn set_mounts(&self, mounts: &Arc<AppMounts>) {
        if let Ok(mut slot) = self.shared.mounts.write() {
            *slot = Arc::downgrade(mounts);
        }
    }

    /// The surface for one call of `module`.
    pub fn surface(
        &self,
        module: &str,
        grant: Option<AppsGrant>,
        caller: Option<&Principal>,
    ) -> Box<dyn Ask> {
        Box::new(AppsAsk {
            shared: Arc::clone(&self.shared),
            module: module.to_owned(),
            grant,
            admin: caller.is_none_or(|c| c.role().is_admin()),
        })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Handles a module's `apps` requests for one route call or service.
/// Knows which module is asking, what its grant allows, and whether
/// the caller is an admin, so [`ask`](Ask::ask) can refuse what isn't allowed.
struct AppsAsk {
    shared: Arc<SharedAppState>,
    module: String,
    grant: Option<AppsGrant>,
    /// Whether the call is an administrator's route or the module's service.
    admin: bool,
}

#[async_trait::async_trait]
impl Ask for AppsAsk {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let Some(grant) = self.grant else {
            return Err(Error::invalid(format!(
                "module `{}` has not been allowed to reach applications; an admin allows it on \
                 the Modules tab (\"Applications\")",
                self.module
            )));
        };
        if !self.admin {
            return Err(Error::auth(format!(
                "only an administrator may reach applications through module `{}`",
                self.module
            )));
        }
        let op = plan.get("op").and_then(Json::as_str).unwrap_or_default();
        let builds = |what: &str| -> Result<()> {
            if grant == AppsGrant::Build {
                Ok(())
            } else {
                Err(Error::invalid(format!(
                    "module `{}` may read applications but not {what}; an admin allows it on the \
                     Modules tab (\"Applications\": build)",
                    self.module
                )))
            }
        };
        match op {
            // One application, or `null`.
            "get" => Ok(match self.find(&plan).await? {
                Some(app) => app_json(&app),
                None => Json::Null,
            }),
            // Every application.
            "list" => Ok(Json::Array(
                sc_app::list_applications(&self.shared.catalog)
                    .await?
                    .iter()
                    .map(app_json)
                    .collect(),
            )),
            // This process's last build of the application, or `null`.
            "last_build" => {
                let app = self.require(&plan).await?;
                Ok(lock(&self.shared.builds)
                    .get(&app.id.0)
                    .map_or(Json::Null, Build::json))
            }
            "wait_build" => self.wait_build(&plan).await,
            "read" => self.read(&plan).await,
            "files" => self.files(&plan).await,
            "build" => {
                builds("build them")?;
                self.build(&plan).await
            }
            "write" => {
                builds("change their projects")?;
                self.write(&plan).await
            }
            "remove" => {
                builds("change their projects")?;
                self.remove(&plan).await
            }
            other => Err(Error::invalid(format!(
                "`{other}` is not something `apps` does; it does get, list, build, last_build, \
                 wait_build, read, write, files and remove"
            ))),
        }
    }
}

impl AppsAsk {
    /// The application named by `app` (its id or subdomain), or `None`.
    async fn find(&self, plan: &Json) -> Result<Option<Application>> {
        let named = plan
            .get("app")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| Error::invalid("which application? `app` is its id or subdomain"))?;
        let catalog = &self.shared.catalog;
        match uuid::Uuid::parse_str(named) {
            Ok(id) => sc_app::load_application(catalog, AppId(id)).await,
            Err(_) => sc_app::load_application_by_subdomain(catalog, named).await,
        }
    }

    /// Like `find`, but "not found" when it doesn't exist.
    async fn require(&self, plan: &Json) -> Result<Application> {
        self.find(plan).await?.ok_or_else(|| {
            Error::not_found(format!(
                "there is no application {}",
                plan.get("app").unwrap_or(&Json::Null)
            ))
        })
    }

    /// Starts a build of the application (or returns the one already running).
    async fn build(&self, plan: &Json) -> Result<Json> {
        let app = self.require(plan).await?;
        let mounts = self
            .shared
            .mounts
            .read()
            .ok()
            .and_then(|m| m.upgrade())
            .ok_or_else(|| Error::config("this server cannot build applications"))?;
        Ok(SharedAppState::start_build(&self.shared, mounts, app))
    }

    /// Waits until the application's build is done, at most `timeout_ms`,
    /// then returns it. `null` if it was never built.
    async fn wait_build(&self, plan: &Json) -> Result<Json> {
        let app = self.require(plan).await?;
        let timeout = plan
            .get("timeout_ms")
            .and_then(Json::as_u64)
            .map_or(MAX_WAIT, Duration::from_millis)
            .min(MAX_WAIT);
        let finished = lock(&self.shared.builds)
            .get(&app.id.0)
            .map(|b| b.finished.clone());
        let Some(mut finished) = finished else {
            return Ok(Json::Null);
        };
        // Listen for the build's "finished" signal, but no longer than `timeout`.
        let _ = tokio::time::timeout(timeout, async {
            let _ = finished.wait_for(|f| *f).await.map(|_| ());
        })
        .await;
        Ok(lock(&self.shared.builds)
            .get(&app.id.0)
            .map_or(Json::Null, Build::json))
    }

    /// A project file's text, or `null` if it doesn't exist.
    async fn read(&self, plan: &Json) -> Result<Json> {
        let (store, path) = self.project_path(plan).await?;
        if store.stat(&path).await?.is_none() {
            return Ok(Json::Null);
        }
        let bytes = store.read(&path).await?;
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
            Error::invalid(format!("`{}` is not text", path.as_str()))
        })?;
        Ok(Json::String(text))
    }

    /// Writes `content` to a project file, creating its folder if needed.
    async fn write(&self, plan: &Json) -> Result<Json> {
        let (store, path) = self.project_path(plan).await?;
        let content = plan
            .get("content")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`content` must be text"))?;
        if let Some((dir, _)) = path.as_str().rsplit_once('/') {
            let dir = sc_files::StorePath::parse(dir)?;
            if store.stat(&dir).await?.is_none() {
                store.mkdir(&dir).await?;
            }
        }
        store
            .write(&path, bytes::Bytes::from(content.to_owned()))
            .await?;
        Ok(json!({ "written": path.as_str() }))
    }

    /// The entries of a project folder: `{ name, path, dir }` each.
    async fn files(&self, plan: &Json) -> Result<Json> {
        let (store, path) = self.project_path(plan).await?;
        if store.stat(&path).await?.is_none() {
            return Ok(json!([]));
        }
        let base = self.source_dir(plan).await?;
        let entries = store.list(&path).await?;
        Ok(Json::Array(
            entries
                .into_iter()
                .map(|e| {
                    let relative = e
                        .path
                        .strip_prefix(&base)
                        .unwrap_or(&e.path)
                        .trim_start_matches('/')
                        .to_owned();
                    json!({ "name": e.name, "path": relative, "dir": e.is_dir })
                })
                .collect(),
        ))
    }

    /// Deletes a project file.
    async fn remove(&self, plan: &Json) -> Result<Json> {
        let (store, path) = self.project_path(plan).await?;
        Ok(json!({ "deleted": store.delete(&path).await? }))
    }

    /// The directory of the application's project sources in its store.
    async fn source_dir(&self, plan: &Json) -> Result<String> {
        let app = self.require(plan).await?;
        let source = sc_app::app_source_from_config(&app.framework)?;
        Ok(project_base(&source.build.source_dir))
    }

    /// The store of `plan.app`'s project and `plan.path` in it, under the
    /// project's source directory and never outside it.
    async fn project_path(
        &self,
        plan: &Json,
    ) -> Result<(Arc<dyn sc_files::FileStore>, sc_files::StorePath)> {
        let app = self.require(plan).await?;
        let source = sc_app::app_source_from_config(&app.framework)?;
        let store = self
            .shared
            .catalog
            .require_file_store(&source.store.0, crate::ADMIN_TASKS)?;
        let relative = plan
            .get("path")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .trim_matches('/');
        if relative.split('/').any(|part| part == ".." || part == ".") {
            return Err(Error::invalid(format!(
                "`{relative}` leaves the application's project; a path is relative to it"
            )));
        }
        let base = project_base(&source.build.source_dir);
        let joined = match (base.is_empty(), relative.is_empty()) {
            (true, _) => relative.to_owned(),
            (false, true) => base,
            (false, false) => format!("{base}/{relative}"),
        };
        Ok((store, sc_files::StorePath::parse(&joined)?))
    }
}

/// A project's source directory as a store path prefix: `""` for the root.
fn project_base(source_dir: &str) -> String {
    match source_dir.trim_matches('/') {
        "." => String::new(),
        other => other.to_owned(),
    }
}

impl SharedAppState {
    /// Start building `app` unless a build of it is running; answers the
    /// build as it stands.
    #[allow(
        clippy::disallowed_methods,
        reason = "sc_catalog::spawn_scoped: a module's build outlives the call that started it"
    )]
    fn start_build(shared: &Arc<SharedAppState>, mounts: Arc<AppMounts>, app: Application) -> Json {
        let id = app.id.0;
        let (done, finished) = watch::channel(false);
        {
            let mut builds = lock(&shared.builds);
            if let Some(running) = builds.get(&id).filter(|b| b.state == BuildState::Running) {
                return running.json();
            }
            builds.insert(
                id,
                Build {
                    state: BuildState::Running,
                    log: String::new(),
                    error: None,
                    started_at: Utc::now(),
                    finished_at: None,
                    finished,
                },
            );
        }
        let answer = lock(&shared.builds).get(&id).map_or(Json::Null, Build::json);
        let shared = Arc::clone(shared);
        sc_catalog::spawn_scoped(async move {
            let outcome =
                crate::handlers::applications::build_now(&shared.catalog, &mounts, app, String::new(), false)
                    .await;
            {
                let mut builds = lock(&shared.builds);
                if let Some(build) = builds.get_mut(&id) {
                    build.finished_at = Some(Utc::now());
                    match outcome {
                        Ok(response) => {
                            build.state = BuildState::Succeeded;
                            build.log = response
                                .body
                                .get("log")
                                .and_then(Json::as_str)
                                .unwrap_or_default()
                                .to_owned();
                        }
                        Err(e) => {
                            build.state = BuildState::Failed;
                            build.error = Some(e.to_string());
                        }
                    }
                }
            }
            let _ = done.send(true);
        });
        answer
    }
}

/// An application as a module reads it.
fn app_json(app: &Application) -> Json {
    json!({
        "id": app.id.0.to_string(),
        "name": app.name,
        "subdomain": app.subdomain,
        "description": app.description,
        "framework": app.framework.name,
        "settings": app.attributes,
        "builder_agent": sc_app::builder_agent_name(app),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_path_is_under_its_source_directory() {
        assert_eq!(project_base("."), "");
        assert_eq!(project_base("/crm/"), "crm");
    }
}
