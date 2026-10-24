//! What every coding tool shares: the file store it works in, and the paths it
//! will accept (§11.3, TODO Phase 5).
//!
//! `read_file`, `find_files`, `search_files`, `write_file`, `edit_file`,
//! `apply_patch` and `run_script` are seven tools over one **scope** — a configured file store,
//! optionally rooted at a sub-directory — and the scope is here rather than in
//! any one of them for the reason [`crate::table`] gives: each of them is a
//! promise to the model, and a path that means one thing to `read_file` and
//! another to `edit_file` is a model that cannot use either. That they now
//! share a *single configuration* as well ([`crate::Coding`]) is the same
//! argument taken one step further.
//!
//! ## The root, and what it is for
//!
//! A store already confines every path to itself: the byte-level methods reject
//! an absolute path and any `..` that would escape the store root. The
//! configured [`CFG_ROOT`] is the *second* confinement, and the one an admin
//! sets: "this agent may work in `web/todo`, not in the rest of the store". So a
//! path the model sends is resolved **under** the root and refused if it tries to
//! leave, exactly as it is refused for trying to leave the store — and every path
//! the model is *shown* is relative to that root, because a model told about a
//! prefix it may not change will eventually send it back and be refused for it.
//!
//! ## Access is §9's rule, unchanged
//!
//! Every path goes through [`check_access`](sc_files::check_access) with the
//! run's caller, so the path-cumulative rule the file manager applies applies
//! here: an agent chatting with a user is not a way to read a directory that
//! user could not open. The store's own floor is part of it, which is why
//! [`FileScope::connect`] returns the two together and nothing in this crate
//! reads a store without it.

use std::sync::Arc;

use sc_agent::{RunCaller, TraitCheck, TraitContext};
use sc_catalog::{Catalog, load_file_store_by_name};
use sc_error::{Error, Result};
use sc_files::{FileCaller, FileStore, check_access, check_write};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json};

use crate::table::config_str;

/// The file store a coding trait works in.
pub const CFG_STORE: &str = "store";
/// The sub-directory it is rooted at. Empty means the whole store.
pub const CFG_ROOT: &str = "root";

/// The path argument every file tool takes.
pub const ARG_PATH: &str = "path";

/// The longest tool name a provider will accept. Both vendors cap names at 64
/// characters, and a name derived from a long store and a deep root can reach
/// it — so it is refused on save, where the admin can shorten the root, rather
/// than by the vendor mid-conversation.
pub const MAX_TOOL_NAME: usize = 64;

/// The two settings the coding trait declares first, in the order the form
/// shows them.
///
/// The store is a [`server_query`](FormField::server_query) pick-list, like every
/// other setting in the tree that names a store: an admin choosing from the
/// stores that exist cannot typo one that does not.
pub fn scope_fields() -> Vec<FormField> {
    vec![
        FormField::new(CFG_STORE, BasicType::Text)
            .label("File store")
            .required()
            .server_query(sc_catalog::QUERY_FILE_STORES),
        FormField::new(CFG_ROOT, BasicType::Text)
            .label("Sub-directory")
            .default_value(""),
    ]
}

/// One trait instance's configured store and root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScope {
    /// The store's name, as the catalog knows it.
    pub store: String,
    /// The sub-directory, store-relative, without leading or trailing slashes.
    /// Empty is the store root.
    pub root: String,
}

/// The scope a configuration names.
///
/// Refuses an empty store and a root that tries to climb out of the store, which
/// are the two ways the setting can be wrong before anything has been reached.
pub fn configured_scope(config: &Attrs) -> Result<FileScope> {
    let store = config_str(config, CFG_STORE);
    if store.is_empty() {
        return Err(Error::invalid(format!("`{CFG_STORE}` is required")));
    }
    let root = normalise_path(&config_str(config, CFG_ROOT)).map_err(|_| {
        Error::invalid(format!(
            "`{CFG_ROOT}` must be a directory inside the store, without `..`"
        ))
    })?;
    Ok(FileScope { store, root })
}

/// A name an admin typed, folded into something a provider will accept in a tool
/// name: lower-case ASCII letters and digits, everything else a single `_`.
///
/// Used for a file store, a sub-directory and an application's subdomain, so
/// every derived tool name is built the same way and two instances over the same
/// thing **collide** — which is what makes the clash refusable on save (§11.2)
/// rather than a surprise when the model picks the wrong tool.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = false;
    for c in text.chars() {
        match c.is_ascii_alphanumeric() {
            true => {
                out.push(c.to_ascii_lowercase());
                last_underscore = false;
            }
            false if !last_underscore && !out.is_empty() => {
                out.push('_');
                last_underscore = true;
            }
            false => {}
        }
    }
    out.trim_end_matches('_').to_owned()
}

/// The scope a configuration names, as best it can be read.
///
/// [`configured_scope`]'s forgiving twin, for [`AgentTrait::tools`]: that method
/// is infallible and is called while *reporting* why an agent is invalid, so an
/// instance with a half-filled form must still produce the tool name its
/// configuration implies. Otherwise "this agent names a store that is gone"
/// would be reported as "this agent has no tools", and the collision check
/// (§11.2) would stop seeing two instances that both name nothing.
///
/// [`AgentTrait::tools`]: sc_agent::AgentTrait::tools
pub fn scope_as_written(config: &Attrs) -> FileScope {
    FileScope {
        store: config_str(config, CFG_STORE),
        root: normalise_path(&config_str(config, CFG_ROOT)).unwrap_or_default(),
    }
}

impl FileScope {
    /// The identifier this scope contributes to a tool's name — the store, and
    /// the root where there is one, with everything a provider will not accept
    /// in a tool name folded to `_`.
    ///
    /// This is what makes one trait enabled twice offer two distinguishable
    /// tools (§11.2), and it is derived rather than configured so that two
    /// instances over the same scope **collide** and are refused on save.
    pub fn slug(&self) -> String {
        match self.root.is_empty() {
            true => slugify(&self.store),
            false => slugify(&format!("{}/{}", self.store, self.root)),
        }
    }

    /// How this scope is named to the model: the store, and the directory when
    /// the agent is confined to one.
    pub fn label(&self) -> String {
        match self.root.is_empty() {
            true => format!("the `{}` file store", self.store),
            false => format!("`{}` in the `{}` file store", self.root, self.store),
        }
    }

    /// The store-relative path a tool argument names.
    ///
    /// The argument is relative to the configured root, and a path that would
    /// leave it is an error naming what happened — the model can read that and
    /// send a path inside, whereas a silently clamped path would have it editing
    /// a file it did not mean.
    pub fn resolve(&self, rel: &str) -> Result<String> {
        let rel = normalise_path(rel).map_err(|_| {
            Error::invalid(format!(
                "`{rel}` is outside {}; paths are relative to it and cannot contain `..`",
                self.label()
            ))
        })?;
        Ok(match (self.root.is_empty(), rel.is_empty()) {
            (true, _) => rel,
            (false, true) => self.root.clone(),
            (false, false) => format!("{}/{rel}", self.root),
        })
    }

    /// The path a tool **reports**, given a store-relative one: the inverse of
    /// [`resolve`](FileScope::resolve), so what comes back is what may be sent
    /// again.
    pub fn relative(&self, store_path: &str) -> String {
        if self.root.is_empty() {
            return store_path.to_owned();
        }
        match store_path.strip_prefix(&format!("{}/", self.root)) {
            Some(rest) => rest.to_owned(),
            // Not under the root at all: report it whole rather than mangle it.
            None if store_path == self.root => String::new(),
            None => store_path.to_owned(),
        }
    }

    /// Connect the store, with the floor its definition sets.
    ///
    /// The two travel together because every access check needs both, and a
    /// caller that forgot the floor would be applying the per-file rules while
    /// ignoring the store-wide one.
    pub async fn connect(&self, catalog: &Catalog) -> Result<(Arc<dyn FileStore>, Option<u8>)> {
        let store = catalog.require_file_store(&self.store)?;
        let floor = load_file_store_by_name(catalog, &self.store)
            .await
            .ok()
            .flatten()
            .and_then(|def| def.min_role);
        Ok((store, floor))
    }
}

/// Everything one file tool needs, resolved: the store, the caller's right to
/// the path, and the path itself.
///
/// One function, so no tool can reach a store having skipped the access check —
/// the shape [`crate::write`] uses for the same reason on the row side.
pub async fn open_at(
    scope: &FileScope,
    ctx: &TraitContext<'_>,
    rel: &str,
) -> Result<(Arc<dyn FileStore>, String)> {
    let (store, floor) = scope.connect(ctx.catalog).await?;
    let path = scope.resolve(rel)?;
    check_access(store.as_ref(), floor, &path, &file_caller(ctx.caller)).await?;
    Ok((store, path))
}

/// [`open_at`] for a tool that **writes** the path: checked by the write rule
/// (§2.1), so an agent run as a user cannot overwrite a file somebody else owns.
pub async fn open_to_write(
    scope: &FileScope,
    ctx: &TraitContext<'_>,
    rel: &str,
) -> Result<(Arc<dyn FileStore>, String)> {
    let (store, floor) = scope.connect(ctx.catalog).await?;
    let path = scope.resolve(rel)?;
    // Whatever the backend: a repository's `.git` holds commands the server's
    // own `git` runs, so a file written there is a program run as the server.
    if sc_files::in_git_dir(&path) {
        return Err(Error::invalid(format!(
            "`{rel}` is inside a `.git` directory, which an agent may not write"
        )));
    }
    // Below admin — and for a triggered run, which holds role 1 but acts on
    // whatever the event said — nothing that decides what the server runs:
    // the build's own inputs, and what a build wrote, which is served (an
    // Astro server entry runs with admin authority on every request) without
    // being built again (security review #16).
    if !crate::coding::is_admin(ctx.caller) {
        if sc_files::controls_build(&path) {
            return Err(Error::invalid(format!(
                "`{rel}` decides what a build or install runs on the server, so only a run \
                 whose caller is an administrator may write it"
            )));
        }
        if let Some(output) = build_output_holding(ctx.catalog, &scope.store, &path).await? {
            return Err(Error::invalid(format!(
                "`{rel}` is inside `{output}`, where an application's build writes what is \
                 served; only a build run by an administrator may change it"
            )));
        }
    }
    check_write(store.as_ref(), floor, &path, &file_caller(ctx.caller)).await?;
    Ok((store, path))
}

/// The build output directory of an application built from `store` that the
/// store-relative `path` is inside, if any — compared without case, since a
/// case-insensitive file system opens `Dist/` as `dist/`.
async fn build_output_holding(
    catalog: &Catalog,
    store: &str,
    path: &str,
) -> Result<Option<String>> {
    let path = path.to_ascii_lowercase();
    for app in sc_app::list_applications(catalog).await? {
        for framework in std::iter::once(&app.framework).chain(&app.extra_frameworks) {
            let Ok(source) = sc_app::app_source_from_config(framework) else {
                continue;
            };
            if source.store.0 != store {
                continue;
            }
            let output = normalise_path(&source.build.output_dir).unwrap_or_default();
            if output.is_empty() {
                continue;
            }
            let lower = output.to_ascii_lowercase();
            if path == lower || path.starts_with(&format!("{lower}/")) {
                return Ok(Some(output));
            }
        }
    }
    Ok(None)
}

/// A run's caller as a file rule sees them: the role every check runs at, and
/// the user id the owner rule compares with (none for a system run).
pub fn file_caller(caller: &RunCaller) -> FileCaller {
    FileCaller::new(caller.role, caller.user.as_ref().map(|u| u.id.to_string()))
}

/// The save-and-load check the coding trait makes: the store is one this
/// deployment has, and the root is a path inside it.
///
/// Checked against the **definition** as well as the live connection, for
/// `run_trigger`'s reason: a store whose disk is currently unmounted is a
/// repairable state of the store, and an agent that named it should not also be
/// invalid. Calling the tool then reports the store's own problem.
pub async fn check_scope(check: &TraitCheck<'_>) -> Result<FileScope> {
    let scope = configured_scope(check.config)?;
    let defined = load_file_store_by_name(check.catalog, &scope.store)
        .await?
        .is_some();
    if !defined && check.catalog.require_file_store(&scope.store).is_err() {
        return Err(Error::invalid(format!(
            "no file store named `{}`",
            scope.store
        )));
    }
    Ok(scope)
}

/// Refuse a derived tool name no provider will accept.
pub fn check_tool_name(name: &str) -> Result<()> {
    if name.len() > MAX_TOOL_NAME {
        return Err(Error::invalid(format!(
            "the tool name this would offer the model, `{name}`, is {} characters; \
             providers accept at most {MAX_TOOL_NAME}. Use a shorter file store name \
             or sub-directory.",
            name.len()
        )));
    }
    Ok(())
}

/// A path with its segments normalised, or `Err(())` if it escapes.
///
/// Empty segments and `.` are dropped; `..` is refused outright rather than
/// resolved. `a/../b` is harmless and would resolve to `b`, but a model that
/// wrote it meant something, and telling it the rule is more useful than
/// silently agreeing with a path it did not intend.
fn normalise_path(path: &str) -> std::result::Result<String, ()> {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.trim().split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(()),
            other => out.push(other),
        }
    }
    Ok(out.join("/"))
}

/// A string argument, required.
pub fn string_arg(args: &Map<String, Json>, name: &str) -> Result<String> {
    match args.get(name) {
        Some(Json::String(s)) => Ok(s.clone()),
        None | Some(Json::Null) => Err(Error::invalid(format!("`{name}` is required"))),
        Some(other) => Err(Error::invalid(format!(
            "`{name}` should be text, got {other}"
        ))),
    }
}

/// A string argument that may be absent, defaulting to `""`.
pub fn optional_string_arg(args: &Map<String, Json>, name: &str) -> Result<String> {
    match args.get(name) {
        None | Some(Json::Null) => Ok(String::new()),
        Some(Json::String(s)) => Ok(s.clone()),
        Some(other) => Err(Error::invalid(format!(
            "`{name}` should be text, got {other}"
        ))),
    }
}

/// A boolean argument that may be absent.
pub fn optional_bool_arg(args: &Map<String, Json>, name: &str, default: bool) -> Result<bool> {
    match args.get(name) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Bool(b)) => Ok(*b),
        Some(other) => Err(Error::invalid(format!(
            "`{name}` should be true or false, got {other}"
        ))),
    }
}

/// A configured whole number, or `default` when the admin set none.
pub fn config_count(config: &Attrs, key: &str, default: u64) -> Result<u64> {
    match config.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Number(n)) => match n.as_i64() {
            Some(n) if n >= 1 => Ok(n as u64),
            _ => Err(Error::invalid(format!(
                "`{key}` must be a whole number of at least 1, got {n}"
            ))),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a number, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope(store: &str, root: &str) -> FileScope {
        FileScope {
            store: store.to_owned(),
            root: root.to_owned(),
        }
    }

    #[test]
    fn a_slug_is_the_store_and_root_folded_to_a_tool_name() {
        assert_eq!(scope("app-src", "").slug(), "app_src");
        assert_eq!(scope("app-src", "web/todo").slug(), "app_src_web_todo");
        assert_eq!(scope("My Store", "a b").slug(), "my_store_a_b");
    }

    #[test]
    fn a_path_resolves_under_the_root_and_cannot_leave_it() {
        let scope = scope("src", "web");
        assert_eq!(scope.resolve("app.ts").unwrap(), "web/app.ts");
        assert_eq!(scope.resolve("/app.ts").unwrap(), "web/app.ts");
        assert_eq!(scope.resolve("./deep/app.ts").unwrap(), "web/deep/app.ts");
        // The root itself is the empty path.
        assert_eq!(scope.resolve("").unwrap(), "web");

        let err = scope.resolve("../secrets.txt").unwrap_err().to_string();
        assert!(err.contains("outside"), "{err}");
        assert!(scope.resolve("a/../../b").is_err());
    }

    #[test]
    fn a_store_wide_scope_resolves_to_the_path_itself() {
        let scope = scope("src", "");
        assert_eq!(scope.resolve("app.ts").unwrap(), "app.ts");
        assert_eq!(scope.resolve("").unwrap(), "");
        assert!(scope.resolve("..").is_err());
    }

    #[test]
    fn what_is_reported_is_what_may_be_sent_again() {
        let rooted = scope("src", "web");
        let store_path = rooted.resolve("deep/app.ts").unwrap();
        assert_eq!(rooted.relative(&store_path), "deep/app.ts");
        assert_eq!(rooted.relative("web"), "");
        // A store-wide scope reports what it resolved.
        assert_eq!(scope("src", "").relative("deep/app.ts"), "deep/app.ts");
    }

    #[test]
    fn a_configuration_naming_no_store_says_so() {
        let err = configured_scope(&Attrs::new()).unwrap_err().to_string();
        assert!(err.contains(CFG_STORE), "{err}");

        let config: Attrs = [
            (CFG_STORE.to_owned(), json!("src")),
            (CFG_ROOT.to_owned(), json!("../elsewhere")),
        ]
        .into_iter()
        .collect();
        let err = configured_scope(&config).unwrap_err().to_string();
        assert!(err.contains(CFG_ROOT), "{err}");
    }

    #[test]
    fn a_tool_name_longer_than_a_provider_accepts_is_refused_on_save() {
        assert!(check_tool_name("read_file_src").is_ok());
        let long = format!("read_file_{}", "x".repeat(MAX_TOOL_NAME));
        let err = check_tool_name(&long).unwrap_err().to_string();
        assert!(err.contains("64"), "{err}");
    }

    #[test]
    fn an_argument_is_read_by_name_and_by_type() {
        let args = crate::table::arguments(&json!({"path": "a.ts"}), &[ARG_PATH]).unwrap();
        assert_eq!(string_arg(&args, ARG_PATH).unwrap(), "a.ts");
        assert_eq!(optional_string_arg(&args, "dir").unwrap(), "");
        // An argument of the wrong type is a mistake the model can correct.
        let args = crate::table::arguments(&json!({"path": 3}), &[ARG_PATH]).unwrap();
        assert!(string_arg(&args, ARG_PATH).is_err());
    }
}
