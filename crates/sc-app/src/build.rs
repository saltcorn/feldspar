//! Wiring a code framework's **build step** (design §13.3).
//!
//! A code framework's source lives in a git repository that is (a sub-directory
//! of) one of the application's file stores, and it has a build step: a bundler
//! is invoked over the source and emits a static bundle, which is what
//! [`CodeFramework`] serves. This module runs that step.
//!
//! The bundler is an external process handed a working directory, so it needs a
//! real on-disk tree — which is why the source store must be one with a
//! [`local_path`](sc_files::FileStore::local_path). An object-store-backed app is
//! rejected with a clear error rather than silently failing to build.
//!
//! Two layers, mirroring the `serve`/`handle` split in [`crate::framework`]:
//! [`run_build`] takes a plain directory and is exercisable without a database,
//! and [`build_app`] resolves the store through the [`Catalog`] and delegates to
//! it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

use bytes::Bytes;
use sc_api::{EndpointSet, generate_client_with_streams};
use sc_catalog::{Attrs, Catalog, FileStoreId};
use sc_error::{Context, Error, Result};
use sc_types::FormField;
use serde_json::Value as Json;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::api::app_endpoints_with;
use crate::application::{Application, FrameworkRef};
use crate::build_cache::build_key;
use crate::declared::{
    FrameworkDecl, FrameworkSet, TargetRequirement, TargetRequirementKind, TargetSpec,
    installed_frameworks,
};
use crate::framework::{
    AssetBundle, BuildSpec, CFG_CLIENT, CFG_COMMAND, CFG_OUTPUT, CFG_SOURCE, CFG_STORE,
    CODE_FRAMEWORK, CodeFramework, InstallSpec, code_config_spec, validate_config_structure_in,
    validate_framework_config_structure,
};
use crate::react::{
    CFG_PROJECT, REACT_FRAMEWORK, react_build_spec, react_client_path, react_config_spec,
};
use crate::scaffold::{emit_app_runtime, has_generated_runtime};

/// How much of a failed build's output to quote in the error. A bundler can emit
/// a great deal on failure; the tail holds the actual error, and the whole of it
/// does not belong in an error message.
const OUTPUT_TAIL_BYTES: usize = 4096;

/// Where a code framework's source lives: the file store holding it, and the
/// build step to run over it (design §13.3).
///
/// The source and output sub-directories are carried by the [`BuildSpec`],
/// relative to the store root — so an app whose store is `apps` with a spec
/// sourced at `blog/web` builds from `<apps>/blog/web`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSource {
    /// The file store the app's source tree lives in. Expected to be a git repo
    /// (§13.3); [`BuildReport::git_repo`] reports whether it actually is.
    pub store: FileStoreId,
    /// The build step to run.
    pub build: BuildSpec,
    /// Where to emit the app's generated TypeScript client, relative to the
    /// store (e.g. `web/src/client.ts`). `None` for an app that does not consume
    /// a generated client.
    pub client_path: Option<String>,
}

impl AppSource {
    /// Source in file store `store`, built by `build`, with no generated client.
    pub fn new(store: FileStoreId, build: BuildSpec) -> AppSource {
        AppSource {
            store,
            build,
            client_path: None,
        }
    }

    /// Emit the generated TypeScript client at `path` (relative to the store)
    /// before building, returning `self` for chaining.
    pub fn with_client(mut self, path: impl Into<String>) -> AppSource {
        self.client_path = Some(path.into());
        self
    }
}

/// Resolve a code framework's [`AppSource`] (and its [`BuildSpec`]) **from a
/// stored application's framework config** (design §13.2/§13.3).
///
/// This is the join between "an app is a row the admin filled in" and "an app is
/// a thing that builds": before this, the store/sub-directories/build command
/// were hand-built in Rust at the call site, which is exactly the configuration
/// path §13.2 rules out. Now they come from [`FrameworkRef::config`], validated
/// against [`code_config_spec`].
///
/// The config's *structure* is validated first, so a missing or ill-typed
/// setting surfaces as the same [`Error::invalid`] naming the setting that a save
/// would have rejected — a stored app should never reach here invalid, but a
/// caller building a `FrameworkRef` in memory can. Whether the named store
/// actually exists is deliberately **not** re-checked here: that was settled on
/// save, and a build failing over it would report a configuration problem when
/// the real news is that the store cannot be resolved, which the build says
/// anyway and says better.
///
/// Both registered frameworks resolve here and produce the same [`AppSource`],
/// which is what keeps the build and serve paths shared rather than forked: for
/// `code` the store, directories and command are *stated*; for `react` they are
/// *derived* from the project name (see [`crate::react`]). Nothing downstream can
/// tell which framework it is building.
pub fn app_source_from_config(fw: &FrameworkRef) -> Result<AppSource> {
    app_source_in(&installed_frameworks(), fw)
}

/// [`app_source_from_config`] against an explicit framework set.
///
/// A **declared** framework resolves here too, and produces the same
/// [`AppSource`] the two built-ins do: its store, source, output and client are
/// its own path templates rendered against the settings the admin filled in
/// (`sc-app`'s `declared` module). So the sentence above — nothing downstream can
/// tell which framework it is building — survives a framework arriving from a
/// module, which is the whole reason the seam is shaped this way.
pub fn app_source_in(set: &FrameworkSet, fw: &FrameworkRef) -> Result<AppSource> {
    match fw.name.as_str() {
        CODE_FRAMEWORK => code_source_from_config(fw),
        REACT_FRAMEWORK => react_source_from_config(fw),
        other => match set.find(other) {
            Some(decl) => declared_source_from_config(set, decl, fw),
            None => Err(Error::config(format!(
                "framework `{other}` has no build step; only `{CODE_FRAMEWORK}`, \
                 `{REACT_FRAMEWORK}` and the frameworks this server's modules declare \
                 build from a file store"
            ))),
        },
    }
}

/// A declared framework's resolution: its own settings in, its own path
/// templates rendered, the same [`AppSource`] out.
fn declared_source_from_config(
    set: &FrameworkSet,
    decl: &FrameworkDecl,
    fw: &FrameworkRef,
) -> Result<AppSource> {
    // The same structural check the built-ins do first, and for the same reason:
    // a missing or ill-typed setting is reported as the setting it is, not as a
    // template that rendered to nothing several layers down.
    validate_config_structure_in(set, fw)?;
    let source = AppSource::new(
        FileStoreId(decl.store(&fw.config)?),
        decl.build_spec(&fw.config)?,
    );
    Ok(match decl.client_path(&fw.config)? {
        Some(path) => source.with_client(path),
        None => source,
    })
}

/// The `code` framework's resolution: every path and the command come from the
/// admin's five settings.
fn code_source_from_config(fw: &FrameworkRef) -> Result<AppSource> {
    validate_framework_config_structure(fw)?;

    let spec = code_config_spec();
    let store = required_setting(&spec, &fw.config, CFG_STORE)?;
    let output_dir = required_setting(&spec, &fw.config, CFG_OUTPUT)?;
    let command_line = required_setting(&spec, &fw.config, CFG_COMMAND)?;
    // Optional, and defaulted to the store root by the spec.
    let source_dir = setting(&spec, &fw.config, CFG_SOURCE)?.unwrap_or_default();
    let client = setting(&spec, &fw.config, CFG_CLIENT)?.filter(|p| !p.trim().is_empty());

    let (command, args) = split_command(&command_line)?;
    let source = AppSource::new(
        FileStoreId(store),
        BuildSpec {
            command,
            args,
            source_dir,
            output_dir,
            // A `code` app's dependencies are the admin's business; their build
            // command is where they say how to get them.
            install: None,
        },
    );
    Ok(match client {
        Some(path) => source.with_client(path),
        None => source,
    })
}

/// The `react` framework's resolution: two settings in, the same [`AppSource`]
/// out, with the source directory, output directory, build command and client
/// path all derived from the project name.
///
/// The project name is not re-checked here: `validate_framework_config_structure`
/// applies the framework's own naming rule, so a name like `../..` is refused as
/// the setting the admin typed rather than, several layers down, as a build path
/// `resolve_under` caught escaping the store.
fn react_source_from_config(fw: &FrameworkRef) -> Result<AppSource> {
    validate_framework_config_structure(fw)?;

    let spec = react_config_spec();
    let store = required_setting(&spec, &fw.config, CFG_STORE)?;
    // Optional, and defaulted to the store root by the spec: an app whose store
    // holds nothing else needs no sub-directory (see `CFG_PROJECT`). Trimmed
    // because a box containing only spaces is an empty box, and every path below
    // is derived from this string.
    let project = setting(&spec, &fw.config, CFG_PROJECT)?.unwrap_or_default();
    let project = project.trim();

    Ok(
        AppSource::new(FileStoreId(store), react_build_spec(project))
            .with_client(react_client_path(project)),
    )
}

/// A setting's resolved value (stored, else the spec's default) as a string.
///
/// `Ok(None)` means "not set and no default"; a non-string value is impossible
/// once the config has been validated against a spec whose fields are all text,
/// so it is reported rather than silently coerced.
fn setting(spec: &[FormField], config: &Attrs, name: &str) -> Result<Option<String>> {
    let field = spec
        .iter()
        .find(|f| f.name() == name)
        .ok_or_else(|| Error::config(format!("this framework declares no `{name}` setting")))?;
    match field.resolve(config) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(Error::invalid(format!(
            "setting `{name}` should be text, got {other}"
        ))),
    }
}

/// A setting that must be present — [`Error::invalid`] naming it if it is not.
fn required_setting(spec: &[FormField], config: &Attrs, name: &str) -> Result<String> {
    setting(spec, config, name)?
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::invalid(format!("setting `{name}` is required")))
}

/// Split a build command line into the executable and its arguments.
///
/// Whitespace-separated, with **no quoting or escaping**: `npm run build` is the
/// shape this is for, and an argument containing a space cannot be expressed. The
/// alternative — a separate list-of-arguments setting — needs a form control the
/// MVP does not have (§6.2's list options are post-MVP), and a real shell parse
/// would invite the quoting bugs that come with it. The bundler is spawned
/// directly, not through a shell, so this is a split rather than shell semantics:
/// no globbing, no pipes, no substitution.
fn split_command(line: &str) -> Result<(String, Vec<String>)> {
    let mut parts = line.split_whitespace().map(str::to_owned);
    let command = parts.next().ok_or_else(|| {
        Error::invalid(format!(
            "setting `{CFG_COMMAND}` is empty; it should be a build command such as `npm run build`"
        ))
    })?;
    Ok((command, parts.collect()))
}

/// The outcome of a successful build.
#[derive(Debug, Clone)]
pub struct BuildReport {
    /// The bundle loaded from the build's output directory — what the framework
    /// serves.
    pub bundle: AssetBundle,
    /// The absolute output directory the bundle was loaded from.
    pub output_dir: PathBuf,
    /// Whether the source store is a git repository. The design expects it to be
    /// (§13.3), but a build from a plain directory still works, so this is
    /// reported rather than enforced.
    pub git_repo: bool,
    /// What the bundler wrote to stdout, for surfacing in build logs.
    pub stdout: String,
    /// What the bundler wrote to stderr. Bundlers routinely report progress here
    /// on success, so this being non-empty does not mean the build failed.
    pub stderr: String,
    /// The path the generated TypeScript client was emitted to, when the build
    /// went through [`build_application`] and the app declares one.
    pub client_path: Option<String>,
    /// Whether dependencies were installed as part of this build — true only on
    /// the first build of a project (or after its `node_modules` went away).
    pub installed: bool,
    /// Whether the bundler was skipped because nothing it reads had changed
    /// since its last successful run, and the bundle is that run's output
    /// ([`build_application_if_changed`]). Always `false` from a real build.
    pub reused: bool,
    /// What the installer wrote, when it ran. Kept separate from the bundler's
    /// output because it answers a different question, and because the first
    /// build of a scaffolded app is mostly this.
    pub install_log: Option<String>,
}

/// Run an application's build step, resolving its source store through the
/// catalog.
///
/// Fails if the store is not connected, has no on-disk path, or the build itself
/// fails.
pub async fn build_app(cat: &Catalog, source: &AppSource) -> Result<BuildReport> {
    let store = cat.require_file_store(&source.store.0)?;
    let root = store_root(&store, source)?;
    let mut report = run_build(&source.build, &root).await?;
    report.git_repo = store.is_git_repo();
    Ok(report)
}

/// The on-disk root of an app source's file store.
///
/// A build spawns an external process with a working directory and a reload
/// reads files off a disk, so both need a store that *is* a directory; an
/// object-store-backed app is refused here with one message rather than two
/// (§13.3).
fn store_root(
    store: &std::sync::Arc<dyn sc_files::FileStore>,
    source: &AppSource,
) -> Result<PathBuf> {
    store.local_path("")?.ok_or_else(|| {
        Error::config(format!(
            "file store {:?} has no local path, so it cannot host a code framework's \
             build step; use a local store",
            source.store.0
        ))
    })
}

/// Delete an application's installed dependencies — a `react` app's
/// `node_modules` — so that its next build installs them from scratch: the
/// first half of the admin's **Deep clean**, of which a build is the second.
///
/// For the tree that no build fixes: a `node_modules` from an interrupted
/// install, from a cache that was corrupted under it (see [`BUILD_LOCK`]), or
/// from dependencies somebody changed by hand. Returns whether there was
/// anything to delete. Refused for a framework with no install step, whose
/// dependencies — if it has any — are not the server's to manage.
pub async fn remove_app_dependencies(cat: &Catalog, source: &AppSource) -> Result<bool> {
    let store = cat.require_file_store(&source.store.0)?;
    let root = store_root(&store, source)?;
    remove_dependencies(&source.build, &root).await
}

/// [`remove_app_dependencies`] over a plain directory, as [`run_build`] is to
/// [`build_app`].
///
/// Under [`BUILD_LOCK`], so a build of the same project is never left running
/// against a tree that is being deleted underneath it.
pub async fn remove_dependencies(spec: &BuildSpec, root: &Path) -> Result<bool> {
    let install = spec.install.as_ref().ok_or_else(|| {
        Error::invalid(
            "this application's framework does not install its dependencies, \
             so there is nothing for a deep clean to reinstall",
        )
    })?;
    let marker = resolve_under(&resolve_under(root, &spec.source_dir)?, &install.marker)?;
    let _serialised = BUILD_LOCK.lock().await;
    if !marker.exists() {
        return Ok(false);
    }
    let target = marker.clone();
    tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&target))
        .await
        .map_err(|e| Error::config(format!("deleting {}: {e}", marker.display())))?
        .with_context(|| format!("deleting {}", marker.display()))?;
    Ok(true)
}

/// Load an application's **already-built** bundle from its output directory,
/// running no bundler and no installer.
///
/// This is the other half of [`build_app`]: that one runs the build step and
/// loads what it produced, this one only loads. It exists because the bytes a
/// [`CodeFramework`] serves are a snapshot taken when the *server* last built —
/// so a developer (or a coding agent) who runs `npm run build` in the project
/// directory changes the disk and nothing a browser can see. Re-reading the
/// output directory is what makes that build visible, and it is cheap enough
/// (a directory walk) to sit on a signal handler, where re-running a bundler
/// would not be.
///
/// Deliberately **not** a build: the output directory is taken as it is found.
/// An application that has never been built has no output directory, and saying
/// so is the right answer — a reload that quietly started running `npm install`
/// would be a very slow surprise.
pub fn load_app_bundle(cat: &Catalog, source: &AppSource) -> Result<AssetBundle> {
    load_output_dir(&app_output_dir(cat, source)?)
}

/// Where an application's build writes its bundle, on disk — whether or not a
/// build has written it yet. What [`load_app_bundle`] reads, and what a coding
/// run's `view_app` previews when the run has built nothing of its own.
pub fn app_output_dir(cat: &Catalog, source: &AppSource) -> Result<PathBuf> {
    let store = cat.require_file_store(&source.store.0)?;
    let root = store_root(&store, source)?;
    resolve_under(&root, &source.build.output_dir)
}

/// Load the bundle in `output_dir`, refusing a missing or empty one.
fn load_output_dir(output_dir: &Path) -> Result<AssetBundle> {
    if !output_dir.is_dir() {
        return Err(Error::config(format!(
            "there is no build output at {} to reload; build the application first",
            output_dir.display()
        )));
    }
    let bundle = AssetBundle::from_dir(output_dir)?;
    if bundle.is_empty() {
        return Err(Error::config(format!(
            "the build output directory {} is empty",
            output_dir.display()
        )));
    }
    Ok(bundle)
}

/// Emit an application's typed TypeScript client into its source tree, then
/// build it — the whole path from an [`Application`] to a servable bundle.
///
/// The client is generated from the app's own [`EndpointSet`](sc_api::EndpointSet)
/// (every provider it enables, projected — see [`app_endpoints`]) by the same
/// generator the admin SPA's client comes from (§13.1). It is written **before**
/// the bundler runs, because the app's source imports it: an app's endpoints
/// depend on which tables and triggers it declares, so unlike the admin's client
/// it cannot be a checked-in artifact and is regenerated on every build. An app
/// that declares no [`client_path`](AppSource::client_path) just builds.
///
/// `dispatcher` is what the app's exposed triggers are resolved against
/// ([`app_triggers`](crate::app_triggers)); an app that exposes none needs none.
pub async fn build_application(
    cat: &Catalog,
    app: &Application,
    source: &AppSource,
    dispatcher: Option<&std::sync::Arc<sc_action::TriggerDispatcher>>,
) -> Result<BuildReport> {
    let (client_path, _) = emit_generated(cat, app, source, dispatcher).await?;
    let mut report = build_app(cat, source).await?;
    report.client_path = client_path;
    Ok(report)
}

/// [`build_application`], except that the bundler is **skipped** when nothing
/// it reads has changed since its last successful run here — what the server
/// does at boot, where rebuilding every application every time was most of the
/// start-up (design §13.2).
///
/// "Nothing changed" is a key over the source directory's git tree as it stands
/// on disk (staged, unstaged and untracked files included), the build spec and
/// the generated files, which are emitted first exactly as for a real build; see
/// [`crate::build_cache`]. A source outside any git repository has no key and is
/// always built. A skipped build loads the existing output and reports
/// [`BuildReport::reused`].
///
/// A person pressing Build, or an agent's build tool, wants the bundler to run —
/// to see its output, or to pick up something the key cannot see — so those
/// paths call [`build_application`].
pub async fn build_application_if_changed(
    cat: &Catalog,
    app: &Application,
    source: &AppSource,
    dispatcher: Option<&std::sync::Arc<sc_action::TriggerDispatcher>>,
) -> Result<BuildReport> {
    let (client_path, generated) = emit_generated(cat, app, source, dispatcher).await?;
    let store = cat.require_file_store(&source.store.0)?;
    let root = store_root(&store, source)?;
    let source_dir = resolve_under(&root, &source.build.source_dir)?;
    let output_dir = resolve_under(&root, &source.build.output_dir)?;
    let mut contents = Vec::with_capacity(generated.len());
    for path in generated {
        let bytes = std::fs::read(resolve_under(&root, &path)?)?;
        contents.push((path, bytes));
    }
    let key = || build_key(&source.build, &source_dir, &output_dir, &contents);

    let before = key().await;
    if let Some(before) = &before
        && before.matches_stamp()
        && let Ok(bundle) = load_output_dir(&output_dir)
    {
        return Ok(BuildReport {
            bundle,
            output_dir,
            git_repo: store.is_git_repo(),
            stdout: String::new(),
            stderr: String::new(),
            client_path,
            installed: false,
            install_log: None,
            reused: true,
        });
    }
    if let Some(before) = &before {
        before.clear_stamp();
    }

    let mut report = build_app(cat, source).await?;
    report.client_path = client_path;

    // Stamped only if the source is as it was when the build started. The first
    // install rewrites `package-lock.json`, and a file saved mid-build is not in
    // the bundle: either way the next boot builds again, which settles it.
    if let Some(before) = before
        && key().await.is_some_and(|after| after.key == before.key)
    {
        before.write_stamp();
    }
    Ok(report)
}

/// Write an application's generated files — its typed client and, for a
/// framework with a generated runtime, that runtime — ahead of a build.
///
/// Returns the client's path, when the app declares one, and every path
/// written.
async fn emit_generated(
    cat: &Catalog,
    app: &Application,
    source: &AppSource,
    dispatcher: Option<&std::sync::Arc<sc_action::TriggerDispatcher>>,
) -> Result<(Option<String>, Vec<String>)> {
    let mut written =
        emit_client(cat, app, source, &app_endpoints_with(app, cat, dispatcher)?).await?;
    let client_path = written.first().cloned();
    // An app whose framework generates a runtime gets more than the client: its
    // hooks (or its composables) are typed from this app's tables, so they are
    // regenerated on the same schedule and for the same reason (§2.1/§2.3).
    // Adding a table in the admin UI makes `useNewTable()` exist at the next
    // build, with nobody regenerating anything by hand. `emit_app_runtime`
    // rewrites the client too, which is harmless and keeps "the runtime is one
    // directory" true.
    if has_generated_runtime(app) {
        written.extend(emit_app_runtime(cat, app, source, dispatcher).await?);
    }
    Ok((client_path, written))
}

/// Write `endpoints` as a generated TypeScript client into the app's source
/// tree, at [`AppSource::client_path`] — **and its helper and its `SKILL.md`
/// beside it**.
///
/// Three files. The client is this application's endpoints and tables, and it
/// imports the half that is the same in every application (how a request is
/// made, how a failure is reported, the types a read is expressed in) from a
/// `helper.ts` in the same directory. One without the other does not compile, so
/// nothing writes one without the other.
///
/// The third is [`SKILL_FILE`](crate::SKILL_FILE): what a coding agent reading
/// this repository has to know about the half of the application that is *not*
/// in it, and the administration MCP tools that reach that half (§13.6). It is
/// written on the same schedule as the client for the same reason — it describes
/// the same application — and is what makes the generated directory
/// self-explanatory to the agent that finds it.
///
/// Returns the paths written, in that order, or nothing when the app declares no
/// client path. Written through the [`FileStore`](sc_files::FileStore), not the
/// local filesystem, so the source tree is reached the same way everything else
/// reaches it (and the store's own traversal sandboxing applies).
pub async fn emit_client(
    cat: &Catalog,
    app: &Application,
    source: &AppSource,
    endpoints: &EndpointSet,
) -> Result<Vec<String>> {
    let Some(path) = &source.client_path else {
        return Ok(Vec::new());
    };
    let store = cat.require_file_store(&source.store.0)?;
    // The streams this app exposes ride in the same module as its endpoints
    // (TODO "Streams" §10): they are not endpoints — a subscription has no
    // shape in that model — but they are this application's contract with its
    // own code, and an app's client is emitted at build time, so keeping them
    // in step costs nothing.
    let streams = crate::app_streams(app, cat).await?;
    let client = generate_client_with_streams(endpoints, &crate::stream_exports(app, &streams));
    store.write(path, Bytes::from(client.into_bytes())).await?;
    let helper = sibling(path, sc_api::CLIENT_HELPER_FILE);
    store
        .write(&helper, Bytes::from(sc_api::client_helper().into_bytes()))
        .await?;
    let skill = sibling(path, crate::SKILL_FILE);
    let client_file = path.rsplit('/').next().unwrap_or(path.as_str()).to_owned();
    store
        .write(
            &skill,
            Bytes::from(crate::generate_skill(cat, app, &client_file).into_bytes()),
        )
        .await?;
    Ok(vec![path.clone(), helper, skill])
}

/// A path beside `path`: same directory, given file name.
fn sibling(path: &str, name: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_owned(),
    }
}

/// Rewrite an application's **generated** files from its current definition —
/// the typed client, and a `react` app's whole `src/feldspar/` runtime — without
/// building anything.
///
/// This is "if the API definition changes, the client code must be updated
/// automatically" (GOALS) reduced to what it actually requires: re-emitting the
/// generated files is fast, needs no external process, and cannot fail on a
/// bundler, so it can run wherever a definition changes — a query added from the
/// command line, a column added in the admin UI — leaving `npm run build` to the
/// build button and the dev server.
///
/// Returns the paths written, which is what a caller reports. An application
/// whose framework declares no client path writes nothing and says so with an
/// empty list rather than an error: an app with no generated client is a
/// configuration, not a failure.
pub async fn emit_app_client(
    cat: &Catalog,
    app: &Application,
    dispatcher: Option<&std::sync::Arc<sc_action::TriggerDispatcher>>,
) -> Result<Vec<String>> {
    let source = app_source_from_config(&app.framework)?;
    // Such an app's client is one file of a generated *directory* whose hooks are
    // typed from the same endpoint set, so rewriting only the client would leave
    // the two disagreeing. `emit_app_runtime` writes the client too, which is why
    // this is an either/or rather than both.
    if has_generated_runtime(app) {
        return emit_app_runtime(cat, app, &source, dispatcher).await;
    }
    let endpoints = app_endpoints_with(app, cat, dispatcher)?;
    emit_client(cat, app, &source, &endpoints).await
}

/// Build an application and return a [`CodeFramework`] serving the result, with
/// the [`BuildSpec`] attached.
///
/// This is the whole path from "source in a file store" to "framework that serves
/// the bundle".
pub async fn build_code_framework(
    cat: &Catalog,
    name: impl Into<String>,
    source: &AppSource,
) -> Result<CodeFramework> {
    let report = build_app(cat, source).await?;
    Ok(CodeFramework::new(name, report.bundle).with_build(source.build.clone()))
}

/// One build target an application's framework offers — what a button says and
/// what a build request names.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetInfo {
    /// The key — `android`.
    pub name: String,
    /// The label — `Android APK`.
    pub label: String,
    /// The framework settings that configure this target alone (see
    /// [`crate::TargetTemplate::options`]).
    pub options: Vec<String>,
    /// What the module can do for the target on request (see
    /// [`crate::TargetOperation`]).
    pub operations: Vec<crate::TargetOperation>,
}

/// The build targets framework `name` declares, in declaration order. Empty for
/// the built-in frameworks and for a name nothing declares.
pub fn framework_build_targets(name: &str) -> Vec<TargetInfo> {
    installed_frameworks()
        .find(name)
        .map(|decl| {
            decl.targets
                .iter()
                .map(|t| TargetInfo {
                    name: t.name.clone(),
                    label: if t.label.trim().is_empty() {
                        t.name.clone()
                    } else {
                        t.label.clone()
                    },
                    options: t.options.clone(),
                    operations: t.operations.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The directory a target's build logs are written to, inside the project —
/// `todo/build-logs/android-20260927-101500.log`. In the file store, so the admin
/// opens a running build's log in the file manager and a finished one stays
/// there to read; not in git, which is the project's `.gitignore`'s business.
const TARGET_LOG_DIR: &str = "build-logs";

/// How many logs of one target are kept; older ones are removed as a new build
/// starts. An APK build logs megabytes, and ten is enough history to compare a
/// failing build with the last one that worked.
const TARGET_LOGS_KEPT: usize = 10;

/// How much of a finished target build's log a report carries. The whole of it
/// is in the file; this is the part a response and a toast can hold.
const TARGET_LOG_TAIL_BYTES: usize = 32 * 1024;

/// The outcome of a successful target build: the file it left in the store.
#[derive(Debug, Clone)]
pub struct TargetReport {
    /// The target's key — `android`.
    pub target: String,
    /// The target's label — `Android APK`.
    pub label: String,
    /// The file store the artifact and the log are in.
    pub store: String,
    /// The artifact's path, relative to the store — where the file manager finds it.
    pub artifact: String,
    /// The artifact's size in bytes.
    pub size: u64,
    /// Whether the source store is a git repository.
    pub git_repo: bool,
    /// The build's log, relative to the store: the install and the command,
    /// written as they ran.
    pub log_path: String,
    /// The end of that log.
    pub log: String,
    /// Whether dependencies were installed first.
    pub installed: bool,
}

/// Target `target` of `app`, resolved against the app's settings — or the
/// reason there is none to build: a framework that declares no targets, or a
/// name it does not declare. Asked before a build starts, so a wrong name is a
/// refusal of the request rather than a job that fails.
pub fn app_target_spec(app: &Application, target: &str) -> Result<TargetSpec> {
    let set = installed_frameworks();
    let decl = set.find(&app.framework.name).ok_or_else(|| {
        Error::not_found(format!(
            "framework `{}` declares no build targets, so there is no `{target}` to build",
            app.framework.name
        ))
    })?;
    decl.target_spec(target, &app.framework.config)
}

/// Whether this machine can build a target now — a **state**, checked against
/// the file system and the environment each time it is asked, not a property of
/// the target. Its opposite is the list of what is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetReadiness {
    /// What the target requires that this machine does not have, one sentence
    /// each with its hint — empty when it is ready.
    pub missing: Vec<String>,
}

impl TargetReadiness {
    /// Whether nothing is missing.
    pub fn is_ready(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Whether this machine can build `spec`: each of its requirements checked
/// against the environment the build would see — the target's own `env` first,
/// then the server's.
///
/// Asked before a build starts, so a missing SDK is a refusal the admin reads at
/// once rather than a Gradle failure minutes later, and asked when the targets
/// are listed, so the button can say so before it is pressed.
pub fn target_readiness(spec: &TargetSpec) -> TargetReadiness {
    readiness_in(spec, |name| std::env::var(name).ok(), std::env::consts::OS)
}

/// [`target_readiness`] of target `target` of `app`.
pub fn app_target_readiness(app: &Application, target: &str) -> Result<TargetReadiness> {
    Ok(target_readiness(&app_target_spec(app, target)?))
}

/// Refuse `spec` when this machine lacks something it needs, naming everything
/// missing at once: an admin who fixes one setting should not discover the next
/// only on the next attempt. What the server asks before it starts a build job,
/// and [`run_target`] again before it runs anything.
pub fn require_target_ready(spec: &TargetSpec) -> Result<()> {
    let readiness = target_readiness(spec);
    if readiness.is_ready() {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "the {} cannot be built on this server yet:\n- {}",
        spec.label,
        readiness.missing.join("\n- ")
    )))
}

/// [`target_readiness`] against a given server environment and operating
/// system, so it is testable without depending on the machine running the test.
fn readiness_in(
    spec: &TargetSpec,
    server_env: impl Fn(&str) -> Option<String>,
    os: &str,
) -> TargetReadiness {
    let var = |name: &str| {
        spec.env
            .get(name)
            .cloned()
            .or_else(|| server_env(name))
            .filter(|v| !v.trim().is_empty())
    };
    TargetReadiness {
        missing: spec
            .requires
            .iter()
            .filter_map(|requirement| why_missing(requirement, &var, os))
            .collect(),
    }
}

/// Why one requirement is not met, with its hint — or `None` when it is.
fn why_missing(
    requirement: &TargetRequirement,
    var: &impl Fn(&str) -> Option<String>,
    os: &str,
) -> Option<String> {
    let problem = match &requirement.kind {
        TargetRequirementKind::Env { name, directory } => match var(name) {
            None => format!("`{name}` is not set."),
            Some(value) if *directory && !Path::new(&value).is_dir() => {
                format!("`{name}` is `{value}`, which is not a directory.")
            }
            Some(_) => return None,
        },
        TargetRequirementKind::Command { name, dir_env } => {
            let runs = |dir: &Path| {
                let candidate = dir.join(name);
                candidate.is_file() && is_executable(&candidate)
            };
            let extra = dir_env.as_deref().and_then(|env| Some((env, var(env)?)));
            if extra.as_ref().is_some_and(|(_, dir)| runs(Path::new(dir))) {
                return None;
            }
            let path = var("PATH").unwrap_or_default();
            if std::env::split_paths(&path).any(|dir| runs(&dir)) {
                return None;
            }
            match extra {
                Some((env, dir)) => {
                    format!("`{name}` is neither in `{dir}` (`{env}`) nor on the PATH.")
                }
                None => format!("`{name}` is not on the PATH."),
            }
        }
        TargetRequirementKind::Os { name } => {
            if os == name {
                return None;
            }
            format!("This target builds only on {name}; this server runs on {os}.")
        }
    };
    Some(match requirement.hint.trim() {
        "" => problem,
        hint => format!("{problem} {hint}"),
    })
}

/// Whether a file may be executed — its permission bits, where there are any.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Where a build of `spec` started at `stamp` (e.g. `20260927-101500`) writes its
/// log, relative to the store: `<source>/build-logs/<target>-<stamp>.log`.
pub fn target_log_path(spec: &TargetSpec, stamp: &str) -> String {
    crate::declared::clean_path(&format!(
        "{}/{TARGET_LOG_DIR}/{}-{stamp}.log",
        spec.source_dir, spec.name
    ))
}

/// Build `spec` for `app`: regenerate its client and runtime, as a web build
/// does, then run the target's command, logging to `log_path`, and check it
/// left its artifact.
///
/// The generated code is rewritten first for the reason [`build_application`]
/// rewrites it: an APK bundles the project's JavaScript, which imports the typed
/// client, so a target built against a stale client ships a stale contract.
/// Nothing is mounted — a target is a file to take away, not something this
/// server serves.
pub async fn build_application_target(
    cat: &Catalog,
    app: &Application,
    spec: &TargetSpec,
    log_path: &str,
    dispatcher: Option<&std::sync::Arc<sc_action::TriggerDispatcher>>,
) -> Result<TargetReport> {
    let source = app_source_from_config(&app.framework)?;
    let store = cat.require_file_store(&source.store.0)?;
    let root = store_root(&store, &source)?;
    emit_generated(cat, app, &source, dispatcher).await?;

    let mut report = run_target(spec, &root, log_path).await?;
    report.store = source.store.0.clone();
    report.git_repo = store.is_git_repo();
    Ok(report)
}

/// Run `spec` with `root` as the file-store root: install when needed, run the
/// command in the source directory with its output going **straight into the
/// log** at `log_path`, and check the artifact is there.
///
/// Written as it runs rather than collected and written at the end, so a build
/// that takes a quarter of an hour can be followed in the file manager, and a
/// server that stops mid-build leaves the log of how far it got. A failure's
/// error carries the log's last lines and names the file for the rest.
///
/// Catalog-free, as [`run_build`] is, so it is testable without a database.
/// [`TargetReport::store`] is empty and [`TargetReport::git_repo`] `false` here.
pub async fn run_target(spec: &TargetSpec, root: &Path, log_path: &str) -> Result<TargetReport> {
    let source_dir = resolve_under(root, &spec.source_dir)?;
    if !source_dir.is_dir() {
        return Err(Error::config(format!(
            "build source directory {} does not exist",
            source_dir.display()
        )));
    }
    require_target_ready(spec)?;
    let log_file = resolve_under(root, log_path)?;
    let mut log = open_target_log(&log_file, spec)?;
    let line = std::iter::once(spec.command.as_str())
        .chain(spec.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let failed = |status: String| {
        Error::config(format!(
            "the {} build `{line}` failed in {} with {status}\n{}\nThe whole log is {log_path} in \
             the file store.",
            spec.label,
            source_dir.display(),
            last_bytes(&file_tail(&log_file, OUTPUT_TAIL_BYTES)).trim_end()
        ))
    };

    use std::io::Write as _;
    // The install shares the installer's cache with every other build, so it
    // waits its turn under [`BUILD_LOCK`]; the native build after it does not,
    // or one APK would hold every web build for a quarter of an hour.
    let install = {
        let _serialised = BUILD_LOCK.lock().await;
        run_install(spec.install.as_ref(), &source_dir, &spec.env).await
    };
    let install_log = match install {
        Ok(output) => output,
        Err(e) => {
            let _ = writeln!(log, "{}", sc_error::format_chain(&e));
            return Err(failed("its install step failing".to_owned()));
        }
    };
    if let Some(output) = &install_log {
        let _ = writeln!(log, "{output}");
    }
    let _ = writeln!(log, "$ {line}");

    let status = Command::new(&spec.command)
        .args(&spec.args)
        .envs(&spec.env)
        .current_dir(&source_dir)
        .stdout(log.try_clone().with_context(|| log_path.to_owned())?)
        .stderr(log.try_clone().with_context(|| log_path.to_owned())?)
        // A server that stops takes its builds with it, rather than leaving a
        // Gradle nobody will read the result of.
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| {
            format!(
                "launching the {} build `{line}` in {}",
                spec.label,
                source_dir.display()
            )
        })?;
    if !status.success() {
        return Err(failed(status.to_string()));
    }

    // A command that succeeded without producing the file is a declaration and a
    // project that disagree about where the file goes; saying so names the path
    // the admin would otherwise look for in vain.
    let artifact = resolve_under(root, &spec.artifact)?;
    let size = std::fs::metadata(&artifact)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .ok_or_else(|| {
            Error::config(format!(
                "the {} build `{line}` succeeded but left no file at {}",
                spec.label,
                artifact.display()
            ))
        })?;

    Ok(TargetReport {
        target: spec.name.clone(),
        label: spec.label.clone(),
        store: String::new(),
        artifact: spec.artifact.clone(),
        size,
        git_repo: false,
        log_path: log_path.to_owned(),
        log: file_tail(&log_file, TARGET_LOG_TAIL_BYTES),
        installed: install_log.is_some(),
    })
}

/// Create a target build's log file, and remove the oldest logs of the same
/// target beyond [`TARGET_LOGS_KEPT`]. Names sort by their timestamp, so the
/// oldest are the first in order.
fn open_target_log(path: &Path, spec: &TargetSpec) -> Result<std::fs::File> {
    let dir = path.parent().ok_or_else(|| {
        Error::invalid(format!(
            "a build log at {} has no directory",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut older: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| is_target_log(n, &spec.name))
                })
                .collect()
        })
        .unwrap_or_default();
    older.sort();
    // The one about to be written counts towards the ten.
    let excess = (older.len() + 1).saturating_sub(TARGET_LOGS_KEPT);
    for old in older.iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
    std::fs::File::create(path)
        .with_context(|| format!("creating the build log {}", path.display()))
}

/// Whether `file` is one of target `target`'s logs: exactly
/// `<target>-<YYYYMMDD>-<HHMMSS>.log`, as [`target_log_path`] names them with the
/// stamp the server writes.
///
/// Exact rather than a prefix, because a prefix is another target's logs as soon
/// as two names share one: `android-` also begins `android-debug-…`, and pruning
/// `android`'s history must not delete `android-debug`'s.
fn is_target_log(file: &str, target: &str) -> bool {
    let Some(stamp) = file
        .strip_prefix(target)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".log"))
    else {
        return false;
    };
    let bytes = stamp.as_bytes();
    bytes.len() == 15
        && bytes[8] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit())
}

/// The last `bytes` of a file, as text — what a report or an error quotes from
/// a log whose whole is on disk. Empty when the file cannot be read.
fn file_tail(path: &Path, bytes: usize) -> String {
    let Ok(data) = std::fs::read(path) else {
        return String::new();
    };
    let start = data.len().saturating_sub(bytes);
    String::from_utf8_lossy(&data[start..]).into_owned()
}

/// Run `spec` with `root` as the file-store root: invoke the bundler in the
/// source directory and load the bundle from the output directory.
///
/// Catalog-free so it is testable without a database; [`build_app`] wraps it.
/// [`BuildReport::git_repo`] is always `false` here — only the store knows.
pub async fn run_build(spec: &BuildSpec, root: &Path) -> Result<BuildReport> {
    let source_dir = resolve_under(root, &spec.source_dir)?;
    if !source_dir.is_dir() {
        return Err(Error::config(format!(
            "build source directory {} does not exist",
            source_dir.display()
        )));
    }

    // One build at a time, install included; see [`BUILD_LOCK`].
    let _serialised = BUILD_LOCK.lock().await;

    let install_log = run_install(spec.install.as_ref(), &source_dir, &BTreeMap::new()).await?;

    let output = run_bundler(spec, &source_dir).await.with_context(|| {
        format!(
            "launching build command `{}` in {}",
            command_line(spec),
            source_dir.display()
        )
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        // The bundler's own diagnostics are the only useful part of this error,
        // so carry them rather than just the exit code (principle 5).
        return Err(Error::config(format!(
            "build command `{}` failed in {} with {}\n{}",
            command_line(spec),
            source_dir.display(),
            output.status,
            tail(&stderr, &stdout)
        )));
    }

    let output_dir = resolve_under(root, &spec.output_dir)?;
    if !output_dir.is_dir() {
        return Err(Error::config(format!(
            "build command `{}` succeeded but produced no output directory {}",
            command_line(spec),
            output_dir.display()
        )));
    }

    let bundle = AssetBundle::from_dir(&output_dir)?;
    if bundle.is_empty() {
        return Err(Error::config(format!(
            "build command `{}` produced an empty output directory {}",
            command_line(spec),
            output_dir.display()
        )));
    }

    Ok(BuildReport {
        bundle,
        output_dir,
        git_repo: false,
        stdout,
        stderr,
        client_path: None,
        installed: install_log.is_some(),
        install_log,
        reused: false,
    })
}

/// Run the build command in `source_dir`: an `npm run <script>` the way npm
/// would run it but without npm ([`NpmScript`]), anything else as it is.
async fn run_bundler(spec: &BuildSpec, source_dir: &Path) -> std::io::Result<Output> {
    match NpmScript::resolve(spec, source_dir) {
        Some(script) => script.run(source_dir).await,
        None => {
            Command::new(&spec.command)
                .args(&spec.args)
                .current_dir(source_dir)
                .output()
                .await
        }
    }
}

/// An `npm run <script>` that [`run_build`] runs itself instead of through npm.
///
/// **Memory.** `npm run build` is a Node process that reads `package.json`,
/// starts a shell, and then waits — about 60 MB held for the whole of a build
/// whose own peak is `vite`'s ~220 MB, on servers with 1 GB. Doing its part
/// here takes that away and changes nothing about what is built.
///
/// What npm does for a script and this does too: runs `pre<script>`,
/// `<script>` and `post<script>`, those that are declared, stopping at the
/// first that fails; runs each with `sh -c` in the package directory, with
/// `node_modules/.bin` of that directory and of every directory above it ahead
/// of `PATH`; and sets the `npm_lifecycle_*`, `npm_package_{name,version,json}`
/// and `INIT_CWD` variables. It prints npm's `> name@version script` banner
/// too, so a build log reads as it did.
///
/// What it does not do is everything else npm is — `.npmrc`, the
/// `npm_config_*` variables, workspaces. A build command that is not exactly
/// `npm run <script>` (extra arguments, a `--`), a `package.json` that is
/// missing, unreadable or does not declare the script, or a non-Unix host, is
/// left to npm, which also gives the better error for the broken ones.
struct NpmScript {
    /// `(event, command line)` for each of `pre<script>`, `<script>` and
    /// `post<script>` that the manifest declares, in that order.
    steps: Vec<(String, String)>,
    /// The package's `name`, for the environment and the banner.
    name: String,
    /// The package's `version`, likewise.
    version: String,
    /// The absolute path of the `package.json` the scripts came from.
    manifest: PathBuf,
}

impl NpmScript {
    /// The script `spec` runs in `source_dir`, if it is one this can run.
    fn resolve(spec: &BuildSpec, source_dir: &Path) -> Option<NpmScript> {
        if !cfg!(unix) || spec.command != "npm" {
            return None;
        }
        let [run, script] = spec.args.as_slice() else {
            return None;
        };
        if run != "run" && run != "run-script" {
            return None;
        }
        let manifest = std::path::absolute(source_dir.join("package.json")).ok()?;
        let json: Json = serde_json::from_slice(&std::fs::read(&manifest).ok()?).ok()?;
        let scripts = json.get("scripts")?.as_object()?;
        let step = |event: String| {
            let line = scripts.get(&event)?.as_str()?.to_owned();
            Some((event, line))
        };
        let main = step(script.clone())?;
        let steps = [
            step(format!("pre{script}")),
            Some(main),
            step(format!("post{script}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let field = |key: &str| {
            json.get(key)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Some(NpmScript {
            steps,
            name: field("name"),
            version: field("version"),
            manifest,
        })
    }

    /// Run the steps in `dir`, as one process's worth of output: both streams
    /// concatenated, and the status of the last step that ran.
    async fn run(&self, dir: &Path) -> std::io::Result<Output> {
        let dir = std::path::absolute(dir)?;
        let path = Self::search_path(&dir)?;
        let mut all = Output {
            status: ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        for (event, line) in &self.steps {
            all.stdout.extend_from_slice(
                format!("\n> {}@{} {event}\n> {line}\n\n", self.name, self.version).as_bytes(),
            );
            let step = Command::new("sh")
                .arg("-c")
                .arg(line)
                .current_dir(&dir)
                .env("PATH", &path)
                .env("npm_lifecycle_event", event)
                .env("npm_lifecycle_script", line)
                .env("npm_package_name", &self.name)
                .env("npm_package_version", &self.version)
                .env("npm_package_json", &self.manifest)
                .env("INIT_CWD", &dir)
                .output()
                .await?;
            all.stdout.extend(step.stdout);
            all.stderr.extend(step.stderr);
            all.status = step.status;
            if !step.status.success() {
                break;
            }
        }
        Ok(all)
    }

    /// `PATH` as npm sets it for a script in `dir`: the `node_modules/.bin` of
    /// `dir` and of each directory above it, nearest first, then the server's.
    fn search_path(dir: &Path) -> std::io::Result<OsString> {
        let bins = dir.ancestors().map(|d| d.join("node_modules").join(".bin"));
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(bins.chain(std::env::split_paths(&inherited)))
            .map_err(std::io::Error::other)
    }
}

/// Serialises [`run_build`] — the install and the bundler together — across
/// this process.
///
/// **Memory.** One React build is `npm` waiting on `tsc`, then on `vite`, and
/// peaks at about 300 MB; the server runs on machines with 1 GB. Builds start
/// from the Build button, an agent's build and check tools, and the rebuild
/// that follows a definition change, and nothing else stops those overlapping,
/// so without this two or three at once push a small machine into swap. The
/// cost is that a build waits behind another rather than running beside it,
/// which on such a machine is faster than both of them swapping.
///
/// **The installer's cache.** The installs are in *different* directories, so
/// this is not about the projects — the installer's cache is one directory per
/// machine and is not safe against concurrent writers. Two `npm install`s
/// racing on it produce a `node_modules` missing the platform-specific optional
/// dependency the bundler needs ("Cannot find native binding", npm/cli#4828),
/// and leave the cache in a state where every later install reproduces the same
/// broken tree until someone runs `npm cache clean --force` — so the cost of
/// the race is not one failed build but every build after it.
///
/// Per process: `feldspar build` from a shell is another process and does not
/// wait on the server's builds.
static BUILD_LOCK: Mutex<()> = Mutex::const_new(());

/// Install the app's dependencies when the [`BuildSpec`]'s install step says to
/// and its marker directory is absent (TODO §2.3).
///
/// Called only with [`BUILD_LOCK`] held, which is what keeps two installs off
/// the installer's cache at once.
///
/// Returns the installer's output when it ran, `None` when there was nothing to
/// do. The MVP tutorial made the admin run this over SSH; an admin with no shell
/// could not, which is the whole reason it is here.
///
/// A failure is an **Application** error carrying the installer's own output
/// (§16), exactly as a failed build is: "npm install failed" tells an admin
/// nothing, while the registry error, the missing peer dependency or the ENOSPC
/// underneath it tells them what to do.
async fn run_install(
    install: Option<&InstallSpec>,
    source_dir: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let Some(install) = install else {
        return Ok(None);
    };
    if source_dir.join(&install.marker).exists() {
        return Ok(None);
    }

    let line = format!("{} {}", install.command, install.args.join(" "));
    let output = Command::new(&install.command)
        .args(&install.args)
        .envs(env)
        .current_dir(source_dir)
        .output()
        .await
        .with_context(|| {
            format!(
                "launching install command `{line}` in {}",
                source_dir.display()
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(Error::config(format!(
            "install command `{line}` failed in {} with {}\n{}",
            source_dir.display(),
            output.status,
            tail(&stderr, &stdout)
        )));
    }
    Ok(Some(format!("{stdout}{stderr}")))
}

/// Join a store-relative path onto `root`, rejecting anything that escapes it.
///
/// The same sandboxing the file store applies to reads and writes: a build spec
/// is admin-supplied configuration, and it does not get to reach outside the
/// app's store.
fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in rel.split(['/', '\\']) {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(Error::invalid(format!(
                    "build path {rel:?} escapes the file store root"
                )));
            }
            seg => out.push(seg),
        }
    }
    Ok(out)
}

/// Render a spec's command line for diagnostics, e.g. `npm run build`.
fn command_line(spec: &BuildSpec) -> String {
    let mut line = spec.command.clone();
    for arg in &spec.args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// The tail of a failed build's output: **both** streams, stdout first.
///
/// Not one or the other, because a build step is more than one tool and they do
/// not agree on where to fail to. The React framework's is `tsc --noEmit && vite
/// build`: `tsc` writes its diagnostics — the file, the line and the column of
/// every type error — to *stdout*, while `npm` and the bundler report the failure
/// itself on *stderr*. Preferring stderr would therefore drop exactly the part
/// worth reading, and with it what the IDE parses into the Problems panel
/// (§12.1). Each stream is bounded separately so a chatty one cannot crowd the
/// other out.
fn tail(stderr: &str, stdout: &str) -> String {
    [stdout, stderr]
        .into_iter()
        .map(str::trim_end)
        .filter(|text| !text.trim().is_empty())
        .map(last_bytes)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last [`OUTPUT_TAIL_BYTES`] of `text`, cut at a character boundary.
fn last_bytes(text: &str) -> &str {
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text;
    }
    let mut start = text.len() - OUTPUT_TAIL_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::{AppRequest, Framework, InstallSpec};

    /// A scratch directory removed when the guard drops, so a failing assertion
    /// does not leave a tree behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            // Unique per test: the pid alone collides across tests in a binary.
            let unique = format!(
                "sc-app-build-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            );
            let dir = std::env::temp_dir().join(unique);
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A stand-in bundler: a shell script that writes a bundle into `$1`. Real
    /// bundlers (vite, webpack) are exactly this from the build step's point of
    /// view — a process that emits files — and depending on a Node toolchain
    /// here would make the Rust test suite need one.
    fn write_fake_bundler(source: &Path, script: &str) {
        write_script(source, "build.sh", script);
    }

    /// Write an executable script into `dir`.
    fn write_script(dir: &Path, name: &str, script: &str) {
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn spec(args: &[&str]) -> BuildSpec {
        BuildSpec {
            command: "sh".to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            source_dir: "web".to_owned(),
            output_dir: "web/dist".to_owned(),
            install: None,
        }
    }

    /// A fully-populated `code` framework config — what the admin UI's form for
    /// `code_config_spec` would post back.
    fn code_config() -> FrameworkRef {
        FrameworkRef::new(CODE_FRAMEWORK)
            .with(CFG_STORE, "apps")
            .with(CFG_SOURCE, "web")
            .with(CFG_OUTPUT, "web/dist")
            .with(CFG_COMMAND, "npm run build")
    }

    #[test]
    fn a_valid_config_resolves_to_a_build_spec() {
        let source = app_source_from_config(&code_config()).expect("valid config");

        assert_eq!(source.store, FileStoreId("apps".to_owned()));
        // The build command is split into the executable and its arguments.
        assert_eq!(source.build.command, "npm");
        assert_eq!(source.build.args, ["run", "build"]);
        assert_eq!(source.build.source_dir, "web");
        assert_eq!(source.build.output_dir, "web/dist");
        // `client` is optional: not set means no generated client.
        assert_eq!(source.client_path, None);

        // Set, it comes through.
        let with_client = code_config().with(CFG_CLIENT, "web/src/client.ts");
        let source = app_source_from_config(&with_client).expect("valid config");
        assert_eq!(source.client_path.as_deref(), Some("web/src/client.ts"));
    }

    #[test]
    fn the_source_directory_defaults_to_the_store_root() {
        // `source` is the one optional directory: an app whose source sits at the
        // store root should not have to say so.
        let mut config = code_config();
        config.config.remove(CFG_SOURCE);
        let source = app_source_from_config(&config).expect("valid config");
        assert_eq!(source.build.source_dir, "");
    }

    #[test]
    fn a_missing_setting_is_an_invalid_error_naming_it() {
        for missing in [CFG_STORE, CFG_OUTPUT, CFG_COMMAND] {
            let mut config = code_config();
            config.config.remove(missing);
            let err = app_source_from_config(&config)
                .expect_err("should reject a config missing a required setting")
                .to_string();
            assert!(err.contains(missing), "should name `{missing}`: {err}");
            assert!(err.contains("required"), "{err}");
            // And name the framework, so an app with extra frameworks is
            // unambiguous.
            assert!(err.contains(CODE_FRAMEWORK), "{err}");
        }
    }

    #[test]
    fn an_ill_typed_setting_is_an_invalid_error_naming_it() {
        // The admin UI renders `store` as a text control, so a number here means
        // something posted the wrong shape — better said than coerced.
        let config = FrameworkRef::new(CODE_FRAMEWORK)
            .with(CFG_STORE, 42)
            .with(CFG_OUTPUT, "web/dist")
            .with(CFG_COMMAND, "npm run build");
        let err = app_source_from_config(&config)
            .expect_err("should reject an ill-typed setting")
            .to_string();
        assert!(err.contains(CFG_STORE), "{err}");
        assert!(err.contains("text"), "should say what it wanted: {err}");
        assert!(err.contains("a number"), "should say what it got: {err}");
    }

    #[test]
    fn an_unknown_setting_is_rejected_rather_than_ignored() {
        let config = code_config().with("sorce", "web");
        let err = app_source_from_config(&config)
            .expect_err("should reject an unknown setting")
            .to_string();
        assert!(err.contains("sorce"), "{err}");
    }

    #[test]
    fn an_empty_build_command_is_rejected() {
        // Whitespace-only passes the type check but is not a command; the error
        // says what one looks like.
        let config = code_config().with(CFG_COMMAND, "   ");
        let err = app_source_from_config(&config)
            .expect_err("should reject a blank command")
            .to_string();
        assert!(err.contains(CFG_COMMAND), "{err}");
    }

    #[test]
    fn only_a_code_framework_resolves_a_build_step() {
        let err = app_source_from_config(&FrameworkRef::new("saltcorn-v1"))
            .expect_err("a build-less framework has no source to resolve")
            .to_string();
        assert!(err.contains("saltcorn-v1"), "{err}");
        // Both frameworks that do build are named, so the error says what would
        // have worked.
        assert!(
            err.contains(CODE_FRAMEWORK) && err.contains(REACT_FRAMEWORK),
            "{err}"
        );
    }

    /// A `react` config: the whole form is two fields.
    fn react_config() -> FrameworkRef {
        FrameworkRef::new(REACT_FRAMEWORK)
            .with(CFG_STORE, "apps")
            .with(CFG_PROJECT, "todo")
    }

    #[test]
    fn a_react_config_derives_what_code_has_to_be_told() {
        let source = app_source_from_config(&react_config()).expect("valid config");

        assert_eq!(source.store, FileStoreId("apps".to_owned()));
        // Everything below came from the project name `todo` and nothing else.
        assert_eq!(source.build.command, "npm");
        assert_eq!(source.build.args, ["run", "build"]);
        assert_eq!(source.build.source_dir, "todo");
        assert_eq!(source.build.output_dir, "todo/dist");
        // The client is not opt-in as it is for `code`: the scaffold imports it,
        // so an app that did not emit one would not compile.
        assert_eq!(
            source.client_path.as_deref(),
            Some("todo/src/feldspar/client.ts")
        );

        // Dependencies install themselves on first build (§2.3): the framework
        // knows its projects are npm projects, so an admin with no shell never
        // has to run `npm install`.
        let install = source.build.install.clone().expect("react installs");
        assert_eq!(install.command, "npm");
        assert_eq!(install.args, ["install"]);
        assert_eq!(install.marker, "node_modules");

        // The two frameworks meet in the same type, which is why `build_app`,
        // `build_application` and the mount path needed no change at all. The
        // install step is the one thing `code` does not get: an arbitrary app's
        // dependencies are the admin's business, and their build command is
        // where they say how to get them.
        let equivalent = code_config()
            .with(CFG_SOURCE, "todo")
            .with(CFG_OUTPUT, "todo/dist")
            .with(CFG_COMMAND, "npm run build")
            .with(CFG_CLIENT, "todo/src/feldspar/client.ts");
        let code_build = app_source_from_config(&equivalent).unwrap().build;
        assert_eq!(code_build.install, None);
        assert_eq!(
            code_build,
            BuildSpec {
                install: None,
                ..source.build.clone()
            }
        );
    }

    #[test]
    fn a_react_config_takes_no_other_settings() {
        // Stating a `code` setting on a `react` app is rejected rather than
        // ignored: it would otherwise look configured and behave as if it were
        // not, which is the failure mode conventions exist to remove.
        for stated in [CFG_SOURCE, CFG_OUTPUT, CFG_COMMAND, CFG_CLIENT] {
            let err = app_source_from_config(&react_config().with(stated, "web"))
                .expect_err("react declares no such setting")
                .to_string();
            assert!(err.contains(stated), "{err}");
        }
    }

    #[test]
    fn a_react_config_missing_a_setting_names_it_and_the_framework() {
        let mut config = react_config();
        config.config.remove(CFG_STORE);
        let err = app_source_from_config(&config)
            .expect_err("should reject a config missing a required setting")
            .to_string();
        assert!(err.contains(CFG_STORE), "should name `{CFG_STORE}`: {err}");
        assert!(err.contains(REACT_FRAMEWORK), "{err}");
    }

    #[test]
    fn a_react_app_with_no_project_directory_is_the_whole_store() {
        // The store *is* the project: a git store cloned from the app's own
        // repository has no sub-directory to name, and requiring one would make
        // the admin invent a nesting level their repository does not have.
        for blank in ["", "   "] {
            let source = app_source_from_config(&react_config().with(CFG_PROJECT, blank))
                .expect("a blank project directory means the store root");
            assert_eq!(source.build.source_dir, "");
            assert_eq!(source.build.output_dir, "dist");
            assert_eq!(
                source.client_path.as_deref(),
                Some("src/feldspar/client.ts"),
                "no path acquires a leading slash"
            );
            // Still an npm project, so it still installs itself.
            assert!(source.build.install.is_some());
        }
        // Unset is the same as blank — the form omits an empty box, and the
        // spec's default answers for it.
        let mut omitted = react_config();
        omitted.config.remove(CFG_PROJECT);
        let source = app_source_from_config(&omitted).expect("the default is the store root");
        assert_eq!(source.build.source_dir, "");
        assert_eq!(source.build.output_dir, "dist");
    }

    #[test]
    fn a_project_name_that_is_not_a_directory_name_is_refused_by_name() {
        // The check is here, at the setting, rather than several layers down in
        // `resolve_under`: a traversal attempt should be reported as the setting
        // it came from, not as a build path that escaped the store.
        for bad in ["../../etc", "a/b", "my app", ".hidden"] {
            let err = app_source_from_config(&react_config().with(CFG_PROJECT, bad))
                .expect_err("should reject an unusable project name")
                .to_string();
            assert!(err.contains(CFG_PROJECT), "{bad:?}: {err}");
        }
    }

    #[tokio::test]
    async fn a_react_apps_conventional_output_is_served_with_spa_deep_links() {
        // The conventions have to point at the directory the bundler actually
        // fills, and what lands there has to serve like an SPA. `npm` is stubbed
        // — a Node toolchain in the Rust test suite would make every build test
        // depend on one — but the source and output directories under test are
        // the derived ones, not hand-written paths. §2.3's integration test runs
        // the real thing, once there is a scaffold to run it on.
        let tmp = TempDir::new("react");
        let source = app_source_from_config(&react_config()).unwrap();
        let project = tmp.path().join("todo");
        std::fs::create_dir_all(&project).unwrap();
        write_fake_bundler(
            &project,
            "#!/bin/sh\n\
             set -e\n\
             mkdir -p dist/assets\n\
             printf '<!doctype html><div id=root>' > dist/index.html\n\
             printf 'export {}' > dist/assets/index.js\n",
        );
        let stubbed = BuildSpec {
            command: "sh".to_owned(),
            args: vec!["build.sh".to_owned()],
            // The install step is stubbed out along with the bundler: it is
            // covered on its own below, and a real `npm install` needs a network.
            install: None,
            ..source.build.clone()
        };

        let report = run_build(&stubbed, tmp.path()).await.unwrap();
        // Built into `<project>/dist`, exactly where the convention said.
        assert_eq!(report.output_dir, project.join("dist"));

        let fw = CodeFramework::new(REACT_FRAMEWORK, report.bundle).with_build(source.build);
        assert_eq!(fw.name(), REACT_FRAMEWORK);
        assert_eq!(fw.serve(&AppRequest::get("/")).status, 200);
        assert_eq!(fw.serve(&AppRequest::get("/assets/index.js")).status, 200);
        // A client-routed deep link resolves to the entry point — the reason
        // `react` reuses this serving path rather than growing its own.
        let deep = fw.serve(&AppRequest::get("/tasks/42"));
        assert_eq!(deep.status, 200);
        assert_eq!(&deep.body[..], b"<!doctype html><div id=root>");
    }

    #[test]
    fn a_single_word_command_has_no_arguments() {
        let config = code_config().with(CFG_COMMAND, "build.sh");
        let source = app_source_from_config(&config).expect("valid config");
        assert_eq!(source.build.command, "build.sh");
        assert!(source.build.args.is_empty());
    }

    /// A source tree whose "bundler" emits an index.html + a JS asset into dist.
    fn good_source(root: &Path) {
        let web = root.join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_fake_bundler(
            &web,
            "#!/bin/sh\n\
             set -e\n\
             mkdir -p dist/assets\n\
             printf '<!doctype html><div id=root>' > dist/index.html\n\
             printf 'console.log(1)' > dist/assets/app.js\n\
             echo built\n",
        );
    }

    #[tokio::test]
    async fn build_invokes_the_bundler_and_loads_its_output() {
        let tmp = TempDir::new("ok");
        good_source(tmp.path());

        let report = run_build(&spec(&["build.sh"]), tmp.path()).await.unwrap();

        // The bundler ran in the source dir and its output was picked up.
        assert_eq!(report.output_dir, tmp.path().join("web").join("dist"));
        assert_eq!(report.bundle.len(), 2);
        assert!(report.bundle.get("index.html").is_some());
        assert_eq!(
            report.bundle.get("assets/app.js").unwrap().content_type,
            "text/javascript; charset=utf-8"
        );
        assert!(report.stdout.contains("built"));
    }

    #[tokio::test]
    async fn built_bundle_is_served_by_a_code_framework() {
        let tmp = TempDir::new("serve");
        good_source(tmp.path());

        let report = run_build(&spec(&["build.sh"]), tmp.path()).await.unwrap();
        let fw = CodeFramework::new("code", report.bundle);

        // The freshly built index.html is served at `/`...
        let root = fw.serve(&AppRequest::get("/"));
        assert_eq!(root.status, 200);
        assert_eq!(&root.body[..], b"<!doctype html><div id=root>");

        // ...its assets at their own paths...
        assert_eq!(fw.serve(&AppRequest::get("/assets/app.js")).status, 200);

        // ...and a client-routed deep link falls back to the SPA entry point,
        // because `from_dir` adopted index.html as the fallback.
        let deep = fw.serve(&AppRequest::get("/posts/42"));
        assert_eq!(deep.status, 200);
        assert_eq!(&deep.body[..], b"<!doctype html><div id=root>");
    }

    fn target_spec(script: &str) -> TargetSpec {
        TargetSpec {
            name: "android".to_owned(),
            label: "Android APK".to_owned(),
            command: "sh".to_owned(),
            args: vec![script.to_owned()],
            source_dir: "web".to_owned(),
            artifact: "web/out/app.apk".to_owned(),
            install: None,
            env: BTreeMap::new(),
            requires: Vec::new(),
        }
    }

    fn env_requirement(name: &str, hint: &str) -> TargetRequirement {
        TargetRequirement {
            kind: TargetRequirementKind::Env {
                name: name.to_owned(),
                directory: true,
            },
            hint: hint.to_owned(),
        }
    }

    #[test]
    fn an_env_requirement_is_met_by_the_targets_env_or_the_servers_and_must_be_a_directory() {
        let tmp = TempDir::new("reqenv");
        let sdk = tmp.path().display().to_string();
        let mut spec = target_spec("apk.sh");
        spec.requires = vec![
            env_requirement("ANDROID_HOME", "Set the Android SDK directory."),
            env_requirement("JAVA_HOME", ""),
        ];
        let no_server = |_: &str| None;

        // Nothing set: both named, the hint appended to the one that has one.
        let missing = readiness_in(&spec, no_server, "linux").missing;
        assert_eq!(
            missing,
            [
                "`ANDROID_HOME` is not set. Set the Android SDK directory.",
                "`JAVA_HOME` is not set."
            ]
        );

        // The target's own env (the module's settings) meets one; the server's
        // environment the other.
        spec.env.insert("ANDROID_HOME".to_owned(), sdk.clone());
        let server = |name: &str| (name == "JAVA_HOME").then(|| sdk.clone());
        assert!(readiness_in(&spec, server, "linux").is_ready());

        // A value that is not a directory is not met, and says what it was.
        spec.env
            .insert("ANDROID_HOME".to_owned(), "/no/such/sdk".to_owned());
        let missing = readiness_in(&spec, server, "linux").missing;
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("/no/such/sdk"), "{missing:?}");
    }

    #[test]
    fn a_command_requirement_looks_on_the_builds_path_and_an_os_requirement_at_the_host() {
        let tmp = TempDir::new("reqcmd");
        write_script(
            tmp.path(),
            "xcodebuild",
            "#!/bin/sh
",
        );
        let mut spec = target_spec("ios.sh");
        spec.requires = vec![
            TargetRequirement {
                kind: TargetRequirementKind::Os {
                    name: "macos".to_owned(),
                },
                hint: String::new(),
            },
            TargetRequirement {
                kind: TargetRequirementKind::Command {
                    name: "xcodebuild".to_owned(),
                    dir_env: None,
                },
                hint: "Install Xcode.".to_owned(),
            },
        ];
        let path = tmp.path().display().to_string();
        let with_path = |name: &str| (name == "PATH").then(|| path.clone());
        let empty_path = |_: &str| None;

        assert!(readiness_in(&spec, with_path, "macos").is_ready());
        assert_eq!(
            readiness_in(&spec, empty_path, "linux").missing,
            [
                "This target builds only on macos; this server runs on linux.",
                "`xcodebuild` is not on the PATH. Install Xcode."
            ]
        );
    }

    #[test]
    fn a_command_requirement_also_looks_in_the_directory_its_variable_names() {
        let tmp = TempDir::new("reqdir");
        write_script(tmp.path(), "pod", "#!/bin/sh\n");
        let mut spec = target_spec("ios.sh");
        spec.requires = vec![TargetRequirement {
            kind: TargetRequirementKind::Command {
                name: "pod".to_owned(),
                dir_env: Some("FELDSPAR_POD_DIR".to_owned()),
            },
            hint: "Set the CocoaPods directory.".to_owned(),
        }];
        let dir = tmp.path().display().to_string();

        // Found in the target's directory, with nothing on the PATH.
        spec.env.insert("FELDSPAR_POD_DIR".to_owned(), dir.clone());
        assert!(readiness_in(&spec, |_| None, "macos").is_ready());

        // Not there: the PATH still counts.
        spec.env
            .insert("FELDSPAR_POD_DIR".to_owned(), "/nonexistent".to_owned());
        let path = dir.clone();
        let with_path = move |name: &str| (name == "PATH").then(|| path.clone());
        assert!(readiness_in(&spec, with_path, "macos").is_ready());

        // Neither: the message names both places.
        assert_eq!(
            readiness_in(&spec, |_| None, "macos").missing,
            [
                "`pod` is neither in `/nonexistent` (`FELDSPAR_POD_DIR`) nor on the PATH. \
              Set the CocoaPods directory."
            ]
        );

        // The variable unset: as a plain command requirement.
        spec.env.remove("FELDSPAR_POD_DIR");
        assert_eq!(
            readiness_in(&spec, |_| None, "macos").missing,
            ["`pod` is not on the PATH. Set the CocoaPods directory."]
        );
    }

    #[tokio::test]
    async fn a_target_whose_requirements_are_unmet_is_refused_before_anything_runs() {
        let tmp = TempDir::new("requnmet");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_script(
            &web,
            "apk.sh",
            "#!/bin/sh
touch ran
",
        );
        let mut spec = target_spec("apk.sh");
        spec.requires = vec![env_requirement(
            "FELDSPAR_TEST_NO_SUCH_SDK",
            "Set the Android SDK directory.",
        )];
        let msg = run_target(&spec, tmp.path(), LOG)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("cannot be built on this server yet"), "{msg}");
        assert!(msg.contains("FELDSPAR_TEST_NO_SUCH_SDK"), "{msg}");
        // Nothing ran and no log was started.
        assert!(!web.join("ran").exists());
        assert!(!tmp.path().join(LOG).exists());
    }

    /// Where the tests below log: the path a server would name, in the store.
    const LOG: &str = "web/build-logs/android-20260927-101500.log";

    fn read_log(tmp: &TempDir) -> String {
        std::fs::read_to_string(tmp.path().join(LOG)).expect("the build log is written")
    }

    #[tokio::test]
    async fn a_target_build_runs_in_the_source_directory_and_reports_its_artifact() {
        let tmp = TempDir::new("target");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_script(
            &web,
            "apk.sh",
            "#!/bin/sh\nset -e\nmkdir -p out\nprintf 'PK-apk' > out/app.apk\necho gradle done\n",
        );
        let mut spec = target_spec("apk.sh");
        spec.install = Some(install_step());
        write_script(
            &web,
            "install.sh",
            "#!/bin/sh\nmkdir -p node_modules\necho installed\n",
        );

        let report = run_target(&spec, tmp.path(), LOG).await.unwrap();
        assert_eq!(report.target, "android");
        assert_eq!(report.artifact, "web/out/app.apk");
        assert_eq!(report.size, 6);
        // The same install step the web build runs comes first.
        assert!(report.installed);
        // Both are in the log in the store, in order, and the report quotes it.
        assert_eq!(report.log_path, LOG);
        let log = read_log(&tmp);
        let installed = log.find("installed").expect("the install is logged");
        let built = log.find("gradle done").expect("the build is logged");
        assert!(installed < built, "{log}");
        assert!(log.contains("$ sh apk.sh"), "{log}");
        assert!(report.log.contains("gradle done"), "{}", report.log);
    }

    #[test]
    fn a_target_logs_under_its_projects_build_logs_directory() {
        let spec = target_spec("apk.sh");
        assert_eq!(
            target_log_path(&spec, "20260927-101500"),
            "web/build-logs/android-20260927-101500.log"
        );
        // A project at the store root logs at the store root's `build-logs`.
        let mut root = spec.clone();
        root.source_dir = String::new();
        assert_eq!(target_log_path(&root, "1"), "build-logs/android-1.log");
    }

    #[tokio::test]
    async fn only_the_latest_logs_of_a_target_are_kept() {
        let tmp = TempDir::new("targetprune");
        let web = tmp.path().join("web");
        let logs = web.join(TARGET_LOG_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        for n in 0..12 {
            std::fs::write(logs.join(format!("android-20260901-0000{n:02}.log")), "old").unwrap();
        }
        // Another target's logs are not this one's to remove — including one
        // whose name merely begins with this one's.
        std::fs::write(logs.join("ios-20260901-000000.log"), "other").unwrap();
        for n in 0..3 {
            std::fs::write(
                logs.join(format!("android-debug-20260801-0000{n:02}.log")),
                "debug",
            )
            .unwrap();
        }
        write_script(
            &web,
            "apk.sh",
            "#!/bin/sh\nmkdir -p out\nprintf x > out/app.apk\n",
        );

        run_target(&target_spec("apk.sh"), tmp.path(), LOG)
            .await
            .unwrap();
        let mut kept: Vec<String> = std::fs::read_dir(&logs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| is_target_log(n, "android"))
            .collect();
        kept.sort();
        assert_eq!(kept.len(), TARGET_LOGS_KEPT, "{kept:?}");
        // The oldest went; the newest — the one just written — stayed.
        assert!(!kept.contains(&"android-20260901-000000.log".to_owned()));
        assert_eq!(kept.last().unwrap(), "android-20260927-101500.log");
        assert!(logs.join("ios-20260901-000000.log").exists());
        for n in 0..3 {
            assert!(
                logs.join(format!("android-debug-20260801-0000{n:02}.log"))
                    .exists(),
                "android's pruning removed android-debug's log {n}"
            );
        }
    }

    #[test]
    fn a_targets_logs_are_named_exactly() {
        assert!(is_target_log("android-20260927-101500.log", "android"));
        assert!(!is_target_log(
            "android-debug-20260927-101500.log",
            "android"
        ));
        assert!(is_target_log(
            "android-debug-20260927-101500.log",
            "android-debug"
        ));
        assert!(!is_target_log("android-2026092-101500.log", "android"));
        assert!(!is_target_log("android-20260927-101500.txt", "android"));
        assert!(!is_target_log("android-notes.log", "android"));
    }

    /// The toolchain a target declares — the React Native module's SDK paths,
    /// from its settings — reaches the install step and the command, rather than
    /// depending on whatever shell started the server.
    #[tokio::test]
    async fn a_targets_environment_reaches_its_install_and_its_command() {
        let tmp = TempDir::new("targetenv");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_script(
            &web,
            "install.sh",
            "#!/bin/sh\nmkdir -p node_modules\necho \"install sees $ANDROID_HOME\"\n",
        );
        write_script(
            &web,
            "apk.sh",
            "#!/bin/sh\nset -e\nmkdir -p out\nprintf x > out/app.apk\necho \"gradle sees $ANDROID_HOME and $JAVA_HOME\"\n",
        );
        let mut spec = target_spec("apk.sh");
        spec.install = Some(install_step());
        spec.env = [
            ("ANDROID_HOME".to_owned(), "/opt/sdk".to_owned()),
            ("JAVA_HOME".to_owned(), "/opt/jdk".to_owned()),
        ]
        .into_iter()
        .collect();

        run_target(&spec, tmp.path(), LOG).await.unwrap();
        let log = read_log(&tmp);
        assert!(log.contains("install sees /opt/sdk"), "{log}");
        assert!(log.contains("gradle sees /opt/sdk and /opt/jdk"), "{log}");
    }

    #[tokio::test]
    async fn a_target_that_leaves_no_artifact_says_where_it_looked() {
        let tmp = TempDir::new("targetnone");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_script(&web, "apk.sh", "#!/bin/sh\necho nothing\n");
        let msg = run_target(&target_spec("apk.sh"), tmp.path(), LOG)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("left no file"), "{msg}");
        assert!(msg.contains("app.apk"), "{msg}");
    }

    #[tokio::test]
    async fn a_failing_target_carries_the_logs_end_and_names_the_log() {
        let tmp = TempDir::new("targetfail");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_script(
            &web,
            "apk.sh",
            "#!/bin/sh\necho 'SDK location not found' >&2\nexit 1\n",
        );
        let msg = run_target(&target_spec("apk.sh"), tmp.path(), LOG)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("Android APK"), "{msg}");
        assert!(msg.contains("SDK location not found"), "{msg}");
        assert!(msg.contains(LOG), "{msg}");
        // And the log in the store has it too, for the admin who opens it.
        assert!(read_log(&tmp).contains("SDK location not found"));
    }

    /// A stand-in installer: creates the marker directory, as `npm install`
    /// creates `node_modules`. Stubbed for the same reason the bundler is — the
    /// Rust suite should not need a Node toolchain or a network.
    fn install_step() -> InstallSpec {
        InstallSpec {
            command: "sh".to_owned(),
            args: vec!["install.sh".to_owned()],
            marker: "node_modules".to_owned(),
        }
    }

    #[tokio::test]
    async fn dependencies_are_installed_once_when_the_marker_is_absent() {
        let tmp = TempDir::new("install");
        good_source(tmp.path());
        let web = tmp.path().join("web");
        write_script(
            &web,
            "install.sh",
            "#!/bin/sh\nmkdir -p node_modules\necho 'added 214 packages'\n",
        );
        let mut spec = spec(&["build.sh"]);
        spec.install = Some(install_step());

        // First build: nothing installed, so the installer runs and its output is
        // reported separately from the bundler's — on a first build that log is
        // most of what there is to see.
        let report = run_build(&spec, tmp.path()).await.unwrap();
        assert!(report.installed);
        assert!(
            report
                .install_log
                .as_deref()
                .unwrap_or_default()
                .contains("added 214 packages")
        );
        assert!(web.join("node_modules").is_dir());

        // Second build: the marker is there, so it does not run again. The check
        // is the directory on disk rather than a remembered flag, so restoring a
        // store from a backup installs again instead of building against nothing.
        let report = run_build(&spec, tmp.path()).await.unwrap();
        assert!(!report.installed);
        assert_eq!(report.install_log, None);
    }

    /// The installer's cache is one directory per machine and npm does not
    /// guard it, so two installs running at once corrupt it for every build
    /// after them. [`BUILD_LOCK`] is the guard, and this is what
    /// asserts it is still there: each install writes a marker into a shared
    /// directory on entry and removes it on exit, so an overlap is a file that
    /// is already present — the same shape as the real failure, without needing
    /// npm to reproduce it.
    #[tokio::test]
    async fn two_builds_never_install_at_the_same_time() {
        let tmp = TempDir::new("installrace");
        let shared = tmp.path().join("inflight");
        std::fs::create_dir_all(&shared).unwrap();

        let mut builds = Vec::new();
        for n in 0..4 {
            let root = tmp.path().join(format!("app{n}"));
            std::fs::create_dir_all(&root).unwrap();
            good_source(&root);
            let web = root.join("web");
            // Enter, refuse to run if anyone else is in here, dwell, leave.
            write_script(
                &web,
                "install.sh",
                &format!(
                    "#!/bin/sh\n\
                     busy='{shared}/busy'\n\
                     if [ -e \"$busy\" ]; then echo 'a second install overlapped' >&2; exit 1; fi\n\
                     : > \"$busy\"\n\
                     sleep 0.2\n\
                     rm -f \"$busy\"\n\
                     mkdir -p node_modules\n",
                    shared = shared.display()
                ),
            );
            let mut spec = spec(&["build.sh"]);
            spec.install = Some(install_step());
            builds.push(tokio::spawn(async move { run_build(&spec, &root).await }));
        }

        for build in builds {
            // The script's own message travels out with the error (§16), so a
            // regression here names the overlap rather than "install failed".
            build.await.unwrap().unwrap();
        }
    }

    /// An `npm run` spec over a project whose `package.json` declares the
    /// script.
    fn npm_project(root: &Path, scripts: Json) -> BuildSpec {
        let web = root.join("web");
        std::fs::create_dir_all(&web).unwrap();
        let manifest =
            serde_json::json!({ "name": "todo", "version": "0.1.0", "scripts": scripts });
        std::fs::write(web.join("package.json"), manifest.to_string()).unwrap();
        BuildSpec {
            command: "npm".to_owned(),
            ..spec(&["run", "build"])
        }
    }

    /// `npm run build` runs the project's script with no `npm` process: about
    /// 60 MB less for the length of a build. What npm would have done is still
    /// done — the hooks in order, the bundler found in a `node_modules/.bin`
    /// (here the store root's, one level up, as npm searches), and the
    /// lifecycle variables set — and `npm_config_user_agent`, which npm always
    /// sets, is not.
    #[tokio::test]
    async fn an_npm_run_build_runs_the_package_script_without_npm() {
        let tmp = TempDir::new("npmscript");
        let spec = npm_project(
            tmp.path(),
            serde_json::json!({
                "prebuild": "echo pre >> order.log",
                "build": "fakebundle && echo \"event=$npm_lifecycle_event agent=$npm_config_user_agent\" > dist/env.txt",
                "postbuild": "echo post >> order.log",
            }),
        );
        let bin = tmp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin,
            "fakebundle",
            "#!/bin/sh\nset -e\necho main >> order.log\nmkdir -p dist\nprintf '<!doctype html>' > dist/index.html\n",
        );

        let report = run_build(&spec, tmp.path()).await.unwrap();

        let web = tmp.path().join("web");
        let order = std::fs::read_to_string(web.join("order.log")).unwrap();
        assert_eq!(order, "pre\nmain\npost\n");
        let env = std::fs::read_to_string(web.join("dist/env.txt")).unwrap();
        assert_eq!(env.trim(), "event=build agent=", "run by npm? {env}");
        // The log still says what ran, the way npm's banner did.
        assert!(
            report.stdout.contains("> todo@0.1.0 build\n> fakebundle"),
            "{}",
            report.stdout
        );
    }

    /// A failing step fails the build with its own output, and the steps after
    /// it do not run — as with npm.
    #[tokio::test]
    async fn a_failing_npm_script_step_stops_the_build() {
        let tmp = TempDir::new("npmfail");
        let spec = npm_project(
            tmp.path(),
            serde_json::json!({
                "prebuild": "echo 'src/App.tsx(3,7): error TS2322: nope'; exit 2",
                "build": "echo built > built.txt",
            }),
        );

        let err = run_build(&spec, tmp.path()).await.unwrap_err().to_string();

        assert!(err.contains("`npm run build`"), "{err}");
        assert!(err.contains("src/App.tsx(3,7): error TS2322"), "{err}");
        assert!(!tmp.path().join("web/built.txt").exists());
    }

    /// Only exactly `npm run <declared script>` is run without npm; anything
    /// else goes to npm as written, which is also what reports a broken or
    /// missing `package.json` best.
    #[test]
    fn only_a_plain_run_of_a_declared_script_skips_npm() {
        let tmp = TempDir::new("npmresolve");
        let spec = npm_project(tmp.path(), serde_json::json!({ "build": "vite build" }));
        let web = tmp.path().join("web");
        let resolves = |spec: &BuildSpec| NpmScript::resolve(spec, &web).is_some();

        assert!(resolves(&spec));
        assert!(resolves(&BuildSpec {
            args: vec!["run-script".into(), "build".into()],
            ..spec.clone()
        }));
        // Arguments of its own, another script, another program.
        assert!(!resolves(&BuildSpec {
            args: vec!["run".into(), "build".into(), "--".into(), "--watch".into()],
            ..spec.clone()
        }));
        assert!(!resolves(&BuildSpec {
            args: vec!["run".into(), "test".into()],
            ..spec.clone()
        }));
        assert!(!resolves(&BuildSpec {
            command: "pnpm".into(),
            ..spec.clone()
        }));
        // No usable manifest.
        std::fs::write(web.join("package.json"), "{ not json").unwrap();
        assert!(!resolves(&spec));
        std::fs::remove_file(web.join("package.json")).unwrap();
        assert!(!resolves(&spec));
    }

    /// Deep clean's first half: the marker directory goes, so the next build
    /// installs again — the install step's own "is it installed?" check is what
    /// makes the reinstall happen, rather than a flag of its own.
    #[tokio::test]
    async fn removing_dependencies_makes_the_next_build_reinstall() {
        let tmp = TempDir::new("deepclean");
        good_source(tmp.path());
        let web = tmp.path().join("web");
        write_script(
            &web,
            "install.sh",
            "#!/bin/sh\nmkdir -p node_modules/left-pad\necho 'added 1 package'\n",
        );
        let mut spec = spec(&["build.sh"]);
        spec.install = Some(install_step());
        assert!(run_build(&spec, tmp.path()).await.unwrap().installed);
        std::fs::write(web.join("node_modules/left-pad/stale.js"), "broken").unwrap();

        assert!(remove_dependencies(&spec, tmp.path()).await.unwrap());
        assert!(!web.join("node_modules").exists());
        // Nothing left to remove is not an error.
        assert!(!remove_dependencies(&spec, tmp.path()).await.unwrap());

        let report = run_build(&spec, tmp.path()).await.unwrap();
        assert!(report.installed, "the build after a clean installs again");
        assert!(!web.join("node_modules/left-pad/stale.js").exists());
    }

    /// A framework with no install step has no dependencies the server
    /// manages, so there is nothing to clean — said, rather than a silent no-op
    /// that looks like it worked.
    #[tokio::test]
    async fn removing_dependencies_without_an_install_step_is_refused() {
        let tmp = TempDir::new("deepclean-none");
        good_source(tmp.path());
        let err = remove_dependencies(&spec(&["build.sh"]), tmp.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not install its dependencies"), "{err}");
    }

    /// A build is a few hundred megabytes of `tsc` and `vite`, and the server
    /// runs on machines with 1 GB, so builds started at once — the Build button,
    /// an agent's check, a rebuild after a definition change — must queue
    /// rather than run side by side. The same enter-and-refuse shape as the
    /// install race above, on the bundler this time.
    #[tokio::test]
    async fn two_builds_never_run_the_bundler_at_the_same_time() {
        let tmp = TempDir::new("buildrace");
        let shared = tmp.path().join("inflight");
        std::fs::create_dir_all(&shared).unwrap();

        let mut builds = Vec::new();
        for n in 0..4 {
            let root = tmp.path().join(format!("app{n}"));
            let web = root.join("web");
            std::fs::create_dir_all(&web).unwrap();
            write_fake_bundler(
                &web,
                &format!(
                    "#!/bin/sh\n\
                     busy='{shared}/busy'\n\
                     if [ -e \"$busy\" ]; then echo 'a second build overlapped' >&2; exit 1; fi\n\
                     : > \"$busy\"\n\
                     sleep 0.2\n\
                     rm -f \"$busy\"\n\
                     mkdir -p dist\n\
                     printf '<!doctype html>' > dist/index.html\n",
                    shared = shared.display()
                ),
            );
            builds.push(tokio::spawn(async move {
                run_build(&spec(&["build.sh"]), &root).await
            }));
        }

        for build in builds {
            build.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn a_failing_install_carries_the_installers_own_output() {
        let tmp = TempDir::new("installfail");
        good_source(tmp.path());
        write_script(
            &tmp.path().join("web"),
            "install.sh",
            "#!/bin/sh\necho 'npm ERR! 404 Not Found - GET registry/nope' >&2\nexit 1\n",
        );
        let mut spec = spec(&["build.sh"]);
        spec.install = Some(install_step());

        let err = run_build(&spec, tmp.path()).await.unwrap_err().to_string();
        // §16: the useful part of an install failure is what npm said, not that
        // something failed. And it must not be reported as a *build* failure —
        // the bundler never ran.
        assert!(err.contains("npm ERR! 404 Not Found"), "{err}");
        assert!(err.contains("install command"), "{err}");
    }

    #[tokio::test]
    async fn a_failing_build_reports_the_bundler_diagnostics() {
        let tmp = TempDir::new("fail");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_fake_bundler(
            &web,
            "#!/bin/sh\necho 'TS2304: Cannot find name foo' >&2\nexit 2\n",
        );

        let err = run_build(&spec(&["build.sh"]), tmp.path())
            .await
            .unwrap_err();
        let msg = err.to_string();
        // No silent failures: the bundler's own error reaches the caller.
        assert!(msg.contains("TS2304: Cannot find name foo"), "{msg}");
        assert!(msg.contains("build.sh"), "{msg}");
    }

    /// The contract the IDE's Problems panel is parsed from (§12.1).
    ///
    /// A React build is `tsc --noEmit && vite build` run by `npm`, and the two
    /// halves use different streams: `tsc` names the file, line and column on
    /// stdout, while npm reports the failure on stderr. The error must carry the
    /// former — an exit status and "the build failed" cannot be turned into a
    /// diagnostic at a line, so a squiggle in the editor depends on this.
    #[tokio::test]
    async fn a_failing_build_carries_the_type_errors_file_and_line() {
        let tmp = TempDir::new("tsc");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        write_fake_bundler(
            &web,
            "#!/bin/sh\n\
             echo \"src/App.tsx(12,15): error TS2322: Type 'string' is not assignable to type 'number'.\"\n\
             echo 'npm error Lifecycle script `build` failed with error:' >&2\n\
             exit 2\n",
        );

        let msg = run_build(&spec(&["build.sh"]), tmp.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("src/App.tsx(12,15): error TS2322:"),
            "the type error's position must survive npm's own noise: {msg}"
        );
        // And the failure itself is still reported, from the other stream.
        assert!(msg.contains("npm error Lifecycle script"), "{msg}");
        // The directory is what lets an absolute path in a diagnostic be placed
        // back inside the store.
        assert!(
            msg.contains(&format!("failed in {} with", web.display())),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn a_build_producing_no_output_is_an_error() {
        let tmp = TempDir::new("noout");
        let web = tmp.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        // Exits 0 but writes no dist/ — a success status is not proof of a bundle.
        write_fake_bundler(&web, "#!/bin/sh\nexit 0\n");

        let err = run_build(&spec(&["build.sh"]), tmp.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("produced no output directory"));
    }

    #[tokio::test]
    async fn a_missing_source_directory_is_an_error() {
        let tmp = TempDir::new("nosrc");
        let err = run_build(&spec(&["build.sh"]), tmp.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn a_missing_bundler_is_an_error_not_a_panic() {
        let tmp = TempDir::new("nocmd");
        std::fs::create_dir_all(tmp.path().join("web")).unwrap();
        let mut spec = spec(&["build.sh"]);
        spec.command = "definitely-not-a-real-bundler".to_owned();

        let err = run_build(&spec, tmp.path()).await.unwrap_err();
        assert!(err.to_string().contains("launching build command"));
    }

    #[test]
    fn build_paths_cannot_escape_the_file_store() {
        let root = Path::new("/srv/store");
        // Ordinary relative paths resolve under the root.
        assert_eq!(
            resolve_under(root, "web/dist").unwrap(),
            Path::new("/srv/store/web/dist")
        );
        // `..` is refused rather than resolved, as it is for reads/writes.
        assert!(resolve_under(root, "../../etc").is_err());
        assert!(resolve_under(root, "web/../../etc").is_err());
        // An empty path is the root itself.
        assert_eq!(resolve_under(root, "").unwrap(), root);
    }
}
