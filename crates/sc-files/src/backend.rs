//! The file-store **backend registry**: which backends exist, what settings each
//! declares, and how a [`FileStoreDef`] becomes a connected [`FileStore`].
//!
//! This is the same "settings as data" move that makes "the admin picks a
//! framework" work (design §13.3), applied to stores. The admin UI must render a
//! settings form for a backend it knows nothing about — including one supplied
//! later by a guest language through `sc-code` — with no per-backend special
//! case. So a backend declares its settings as [`FormField`]s, the one
//! vocabulary §6.2 gives every configurable extension point, and the UI renders
//! whatever it is handed.
//!
//! The MVP registers exactly one backend, [`LOCAL_BACKEND`]. The registry is not
//! ceremony for that one case: it is what makes adding S3 or a git remote a
//! change *here* rather than a change in the admin UI.
//!
//! ## Two kinds of wrong, kept apart
//!
//! A store's configuration can be wrong in two quite different ways, and
//! conflating them would make the admin UI unusable:
//!
//! - **Structurally wrong** — a missing `path`, a `path` that is a number, a
//!   setting the backend has never heard of, an unknown backend. This is the
//!   admin's typo, it is knowable without touching the filesystem, and it is
//!   always the same answer on every machine. [`validate_file_store_config`]
//!   catches it, and `save_file_store` refuses the save, so the mistake is
//!   caught while the admin is still looking at the form.
//! - **Currently unreachable** — a perfectly well-formed definition whose
//!   directory has been unmounted, renamed, or not created yet. This is not a
//!   typo and often not the admin's fault; it can become true *after* a
//!   successful save, and it can stop being true without anything being edited.
//!   [`connect_from_def`] reports it.
//!
//! Only the first blocks a save. That is deliberate and follows from §1.1's
//! model: a definition is inert data that may describe a store which does not
//! currently work, and such a store must stay listable and **editable** — an
//! admin whose disk was unmounted needs to be able to open that store and fix
//! its path, which they could not do if saving demanded a reachable directory.
//! So reachability is reported, never enforced at save time.

use std::path::PathBuf;
use std::sync::Arc;

use sc_error::{Context, Error, Repr, Result};
use sc_types::{Attrs, BasicType, FormField, Operation, validate_attrs};

use crate::def::{
    CFG_BRANCH, CFG_CREATE, CFG_DIR, CFG_KEY_PATH, CFG_PATH, CFG_PUBLIC_KEY, CFG_URL, FileStoreDef,
    GIT_BACKEND, LOCAL_BACKEND,
};
use crate::git::{GitFileStore, GitRepo, clone_path, git_operations, validate_git_config};
use crate::local::{LocalFileStore, local_operations};
use crate::paths::suggest_local_dir;
use crate::store::FileStore;

/// The settings the [`local`](LOCAL_BACKEND) backend needs: which directory, and
/// whether to create it.
///
/// A free function as well as something the registry hands out, mirroring
/// `code_config_spec` in `sc-app`: the admin UI and the save-time check both
/// need a backend's settings *before* there is an instance to ask, and a
/// `LocalFileStore` cannot exist until its directory does — which is precisely
/// the thing being configured.
pub fn local_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_PATH, BasicType::Text)
            .label("Directory")
            .required(),
        // Defaults to false: creating directories on the server's filesystem is
        // a side effect an admin should opt into, and silently creating one is
        // how a typo'd path becomes a new empty store instead of an error.
        FormField::new(CFG_CREATE, BasicType::Bool)
            .label("Create the directory if it does not exist")
            .default_value(false),
    ]
}

/// The settings the [`git`](GIT_BACKEND) backend needs: which repository, where
/// to keep the working copy, which branch, and how to authenticate.
///
/// **Nothing here is required on its own**, and that is a statement about the
/// backend rather than a lapse. A URL alone is the ordinary case: Saltcorn
/// clones it into a directory it places itself (see
/// [`clone_path`](crate::clone_path)), which is the decision an admin has no
/// basis to make. A directory alone is the other case the backend supports: a
/// repository already checked out on the server is adopted as it stands, and the
/// remote it pushes to is the one that checkout already has. What is genuinely
/// required is *one of the two*, which is a relation between fields rather than
/// a property of either — so it is checked by
/// [`validate_git_config`](crate::git::validate_git_config), not declared here.
///
/// The directory is [`create_only`](FormField::create_only): it says where the
/// working tree was put, so an edit that changed it would abandon that tree
/// rather than move it.
///
/// `key_path` and `public_key` are ordinary settings even though the deploy-key
/// button usually fills them in, because an admin pointing a store at a key that
/// already exists on the machine is a configuration Saltcorn has no business
/// refusing.
pub fn git_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_URL, BasicType::Text).label("Repository URL"),
        FormField::new(CFG_DIR, BasicType::Text)
            .label("Working copy directory (blank to let Saltcorn choose)")
            .create_only(),
        FormField::new(CFG_BRANCH, BasicType::Text).label("Branch (blank for the default)"),
        FormField::new(CFG_KEY_PATH, BasicType::Text).label("SSH private key file"),
        FormField::new(CFG_PUBLIC_KEY, BasicType::Text)
            .label("Deploy key (public)")
            // A key is one long line the admin copies out, not a value they
            // type; the hint is what gets it a text area without the UI knowing
            // that this particular setting is a key.
            .multiline(),
    ]
}

/// The operations a backend offers beyond its settings (§6.2's vocabulary for
/// *acts*, [`Operation`]) — what the admin UI renders as buttons.
///
/// The [`local`](LOCAL_BACKEND) backend has one, and it acts on the *settings*
/// rather than on the directory: reading and writing files already covers
/// everything that can be done to a directory, but nothing in a form can tell an
/// admin where a store may sensibly live (see
/// [`local_operations`](crate::local_operations)). A backend with no operations
/// at all remains an ordinary shape, which is why they are declared rather than
/// assumed.
pub fn backend_operations(name: &str) -> Result<Vec<Operation>> {
    match name {
        LOCAL_BACKEND => Ok(local_operations()),
        GIT_BACKEND => Ok(git_operations()),
        other => Err(unknown_backend(other)),
    }
}

/// What an operation has to say for itself: prose for the admin, and — where a
/// backend has something a machine can read — the same facts as data.
///
/// `output` is the whole of the contract and the only part the admin UI renders:
/// a screen that understood branches would be a screen that could not render a
/// plugin backend's status at all (§14.1). `data` is the deliberate escape
/// hatch, taken by exactly one caller: the IDE's source-control view, which
/// cannot draw a list of changed files from a paragraph of English. It is
/// **optional**, unrendered where nothing sets it, and no backend has to grow
/// one to keep working.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperationOutcome {
    /// What happened, in the words the admin sees.
    pub output: String,
    /// The same, structured, for a client that needs to act on it.
    pub data: Option<serde_json::Value>,
}

impl OperationOutcome {
    /// An outcome that is only prose — what most operations are.
    pub fn text(output: impl Into<String>) -> OperationOutcome {
        OperationOutcome {
            output: output.into(),
            data: None,
        }
    }

    /// Carry structured data alongside the prose.
    pub fn with_data(self, data: serde_json::Value) -> OperationOutcome {
        OperationOutcome {
            data: Some(data),
            ..self
        }
    }
}

impl From<String> for OperationOutcome {
    fn from(output: String) -> OperationOutcome {
        OperationOutcome::text(output)
    }
}

/// Run one of a backend's declared [`operations`](backend_operations).
///
/// `def` is **mutable** because an operation may configure as well as act: the
/// deploy-key generator fills in two settings, and a clone records where it
/// cloned to. The caller persists whatever changed — a
/// [`Configure`](OperationScope::Configure) operation's changes go back to the
/// form the admin is still filling in, an [`Instance`](OperationScope::Instance)
/// operation's to the store's row.
///
/// Returns an [`OperationOutcome`]: text for the admin — a command's own output,
/// or a summary — and optionally the same facts as data. Anything that went
/// wrong is an `Err` carrying the text, since the reason a push failed is the
/// actionable part.
pub async fn run_backend_operation(
    def: &mut FileStoreDef,
    operation: &str,
    input: &Attrs,
) -> Result<OperationOutcome> {
    let declared = backend_operations(&def.backend)?;
    let spec = declared
        .iter()
        .find(|op| op.name == operation)
        .ok_or_else(|| {
            Error::invalid(format!(
                "the `{}` backend has no operation `{operation}`{}",
                def.backend,
                if declared.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; it offers {}",
                        declared
                            .iter()
                            .map(|op| op.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ))
        })?;
    // The operation's arguments are `FormField`s, so they are checked by the
    // same code that checks settings — a missing commit message is caught here,
    // with the same message shape, rather than by each operation separately.
    validate_attrs(&spec.input_spec, input)?;

    match def.backend.as_str() {
        LOCAL_BACKEND => crate::local::run_local_operation(def, operation, input),
        GIT_BACKEND => crate::git::run_git_operation(def, operation, input).await,
        // Unreachable: `backend_operations` above returned a spec, so the
        // backend both exists and declares this operation.
        other => Err(Error::config(format!(
            "file-store backend `{other}` declares operation `{operation}` but cannot run it"
        ))),
    }
}

/// The error for a backend nothing implements, naming what there is.
fn unknown_backend(name: &str) -> Error {
    Error::config(format!(
        "unknown file-store backend `{name}`; the registered backends are {}",
        registered_backends().join(", ")
    ))
}

/// The names of every registered backend — what the admin UI lists so an admin
/// can pick one and be shown its [`backend_config_spec`].
///
/// This is the single place that enumerates them, so a new backend is listed by
/// adding it here (and to [`backend_config_spec`] and [`connect_from_def`]).
pub fn registered_backends() -> Vec<String> {
    vec![LOCAL_BACKEND.to_owned(), GIT_BACKEND.to_owned()]
}

/// The settings the backend registered under `name` declares — the registry
/// lookup, resolving a [`FileStoreDef`]'s backend name to a spec without needing
/// an instance.
///
/// An unknown name is a configuration error rather than a backend with no
/// settings, mirroring `framework_config_spec`: a store whose backend nothing
/// implements cannot be connected, and saying so beats accepting it silently and
/// failing later with no explanation.
pub fn backend_config_spec(name: &str) -> Result<Vec<FormField>> {
    match name {
        LOCAL_BACKEND => Ok(local_config_spec()),
        GIT_BACKEND => Ok(git_config_spec()),
        other => Err(unknown_backend(other)),
    }
}

/// A stored definition's settings as the admin's form should **show** them: what
/// is stored, plus any value the backend decided for itself.
///
/// The counterpart to [`validate_file_store_config`], and deliberately not its
/// inverse. Validation asks what the admin said; this answers what is true. They
/// differ for exactly one kind of setting — one the admin may leave blank and the
/// backend then settles, of which a git store's working-copy directory is the
/// first: blank is the right thing to store (the directory is derived, and a
/// stored copy could disagree with the derivation) and the wrong thing to
/// display, since the control is read-only and an empty one reads as "there
/// isn't one".
///
/// **Output only.** Nothing here reaches a row: a form hands the filled-in value
/// straight back on the next save, and
/// [`preserve_create_only`](sc_types::preserve_create_only) drops it again
/// because there is nothing stored for it. That is what keeps the derivation the
/// single answer rather than seeding a second one.
pub fn display_config(def: &FileStoreDef) -> Attrs {
    let mut config = def.config.clone();
    if def.backend == GIT_BACKEND {
        crate::git::fill_display_config(def, &mut config);
    }
    config
}

/// Where a store definition arriving from **another installation** (a restored
/// backup) moved to, when its directory had to be moved: `(from, to)`.
pub type Relocation = (String, PathBuf);

/// Point a definition that came from another machine at a directory *this*
/// machine has, returning where it moved from and to — or `None` when it was
/// left alone.
///
/// A backup carries a store's directory as the absolute path it had where the
/// backup was taken, and that path usually means nothing here: another user's
/// home, another data directory, a mount this server cannot see. Restored as it
/// stands, the store would either fail to connect or — with `create` on — make
/// directories wherever the old path happens to be writable. So the rule is:
///
/// - **The path exists here: keep it.** That is a store on a shared drive, or a
///   restore onto the very machine the backup came from, and the directory is
///   the one the admin meant.
/// - **It does not: move it to where this installation puts such a store** — the
///   [`suggest_local_dir`] a new local store is offered (with `create` on, since
///   nothing is there yet), or the [`clone_dir`](crate::clone_dir) a git store is
///   checked out into when the admin names no directory. A git store with a URL
///   simply loses its directory setting, so the location is derived as it would
///   be for any store that let Saltcorn choose; one with no URL needs a
///   directory to be valid at all, so it is given that location explicitly.
///
/// Only the working directory is moved. A git store's `key_path` names a key the
/// backup does not carry, so moving it would point at nothing either way.
pub fn relocate_for_restore(def: &mut FileStoreDef) -> Result<Option<Relocation>> {
    let setting = match def.backend.as_str() {
        LOCAL_BACKEND => CFG_PATH,
        GIT_BACKEND => CFG_DIR,
        _ => return Ok(None),
    };
    let Some(from) = def
        .setting(setting)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    if std::path::Path::new(&from).exists() {
        return Ok(None);
    }
    let to = if def.backend == LOCAL_BACKEND {
        let to = suggest_local_dir(&def.name)?;
        def.config.insert(
            CFG_PATH.to_owned(),
            serde_json::Value::String(to.to_string_lossy().into_owned()),
        );
        def.config
            .insert(CFG_CREATE.to_owned(), serde_json::Value::Bool(true));
        to
    } else {
        def.config.remove(CFG_DIR);
        let has_url = def.setting(CFG_URL).is_some_and(|u| !u.trim().is_empty());
        let to = clone_path(def)?;
        if !has_url {
            def.config.insert(
                CFG_DIR.to_owned(),
                serde_json::Value::String(to.to_string_lossy().into_owned()),
            );
        }
        to
    };
    Ok(Some((from, to)))
}

/// Check a [`FileStoreDef`]'s settings against its backend's declared spec.
///
/// Called **on save** (see `sc_catalog::save_file_store`), which is the point of
/// it: a missing or ill-typed setting is the admin's to fix, and the admin is
/// standing in front of the form. Discovering it at connect time means a store
/// that silently never comes up.
///
/// This is the *structural* half of "is this store configured correctly" — see
/// the module docs for why it deliberately does not touch the filesystem.
///
/// Two checks, in order. The declared spec is checked field by field, which is
/// everything most backends need; then the backend gets to say what a spec of
/// independent fields cannot — the git backend requires a URL **or** a directory
/// holding a checkout, which is a relation between two optional settings. Only a
/// backend with such a relation needs the second half, so it is a match arm and
/// not a trait method.
pub fn validate_file_store_config(def: &FileStoreDef) -> Result<()> {
    let spec = backend_config_spec(&def.backend)?;
    validate_attrs(&spec, &def.config).map_err(|e| {
        // Name the store and its backend as well as the setting. Rebuilt rather
        // than wrapped, for the reason `validate_framework_config` documents:
        // `Invalid` renders its own "invalid:" prefix, so formatting the whole
        // error into a new one would say it twice, and a `Context` would show
        // only the context and hide the setting — the part the admin needs.
        if let Repr::Invalid(msg) = e.repr() {
            Error::invalid(format!(
                "file store `{}` (backend `{}`): {msg}",
                def.name, def.backend
            ))
        } else {
            e
        }
    })?;

    if def.backend == GIT_BACKEND {
        validate_git_config(def)?;
    }
    Ok(())
}

/// Turn a stored definition into a connected [`FileStore`] — the one place a
/// definition becomes an instance.
///
/// This is where **reachability** is decided, and an error here means "this
/// well-formed store is not currently usable", not "this configuration is
/// wrong": the directory may have been unmounted since the definition was saved.
/// Callers surface it rather than discarding the store — a store that failed to
/// connect must still be listed and editable, since editing it is how the admin
/// fixes it. Boot connects every stored store this way, and one failure must not
/// stop the others (TODO §1.3).
///
/// Settings are validated first, so a structurally broken definition gets the
/// clear "you mis-configured this" error rather than a confusing filesystem one.
pub fn connect_from_def(def: &FileStoreDef) -> Result<Arc<dyn FileStore>> {
    validate_file_store_config(def)?;

    match def.backend.as_str() {
        LOCAL_BACKEND => {
            let path = def.setting(CFG_PATH).ok_or_else(|| {
                // Unreachable in practice — `path` is required and validation
                // ran above — but the alternative is an `unwrap`, and a library
                // that cannot panic is worth the four lines (principle 5).
                Error::invalid(format!(
                    "file store `{}` has no `{CFG_PATH}` setting",
                    def.name
                ))
            })?;

            if creates_directory(def) && !std::path::Path::new(path).exists() {
                std::fs::create_dir_all(path).with_context(|| {
                    format!("creating directory {path} for file store `{}`", def.name)
                })?;
            }

            let store = LocalFileStore::new(&def.name, path)
                .with_context(|| format!("connecting file store `{}`", def.name))?;
            Ok(Arc::new(store) as Arc<dyn FileStore>)
        }
        GIT_BACKEND => {
            // Deliberately does **not** clone. Connecting happens at boot and on
            // every save, and both must be fast, offline-safe and free of
            // surprises: a clone is a network operation that can hang, can
            // authenticate as the wrong identity, and — on a store whose remote
            // has been repointed — could write a whole tree nobody asked for.
            // `GitRepo::ensure_cloned` is the explicit, awaited path, and the
            // server calls it when the admin saves the store.
            let repo = GitRepo::from_def(def)?;
            let store = GitFileStore::new(&def.name, repo)
                .with_context(|| format!("connecting file store `{}`", def.name))?;
            Ok(Arc::new(store) as Arc<dyn FileStore>)
        }
        // Unreachable while `validate_file_store_config` runs first, which
        // rejects an unknown backend; kept so adding a backend to the registry
        // without adding it here is a clear error rather than a fallthrough.
        other => Err(Error::config(format!(
            "file-store backend `{other}` declares settings but cannot be connected"
        ))),
    }
}

/// Whether a definition asks for its directory to be created. Absent or
/// non-boolean reads as `false` — the safe direction, and validation has already
/// rejected a non-boolean by the time this is reached.
fn creates_directory(def: &FileStoreDef) -> bool {
    def.config
        .get(CFG_CREATE)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error from a failed connect, as a string — the **whole** causal
    /// chain, which is what `connect_file_store_def` records and the admin UI
    /// shows. The outermost layer is only "connecting file store `x`"; the
    /// reason is underneath it.
    ///
    /// `unwrap_err` is unavailable here: it requires the `Ok` type to be `Debug`
    /// and `Arc<dyn FileStore>` is not, a store being a live handle rather than
    /// a value to print.
    fn connect_err(def: &FileStoreDef) -> String {
        match connect_from_def(def) {
            Ok(store) => panic!("expected `{}` not to connect", store.name()),
            Err(e) => sc_error::format_causes(&e),
        }
    }

    /// A unique temp path that does not exist yet.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "sc-files-backend-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }

    /// A restored store keeps a directory this machine has and is otherwise
    /// moved to where this installation puts such a store.
    #[test]
    fn a_restored_store_is_relocated_only_when_its_directory_is_missing() {
        crate::paths::testing::temp_env(|| {
            unsafe { std::env::set_var(crate::DATA_DIR_ENV, "/tmp/sc-relocate") };
            let missing = temp_path("missing").to_string_lossy().into_owned();
            let here = std::env::temp_dir().to_string_lossy().into_owned();

            // Local: a path that exists is kept, untouched.
            let mut kept = FileStoreDef::local("docs", here.clone());
            assert_eq!(relocate_for_restore(&mut kept).unwrap(), None);
            assert_eq!(kept.setting(CFG_PATH), Some(here.as_str()));

            // Local: a missing one becomes the suggested directory, created.
            let mut local = FileStoreDef::local("my docs", missing.clone());
            let (from, to) = relocate_for_restore(&mut local).unwrap().unwrap();
            assert_eq!(from, missing);
            assert_eq!(to, PathBuf::from("/tmp/sc-relocate/local-stores/my_docs"));
            assert_eq!(
                local.setting(CFG_PATH),
                Some("/tmp/sc-relocate/local-stores/my_docs")
            );
            assert_eq!(local.config.get(CFG_CREATE), Some(&serde_json::json!(true)));

            // Git with a URL: the directory is dropped, so it is derived.
            let mut git = FileStoreDef::git("app", "u").with(CFG_DIR, missing.clone());
            let (_, to) = relocate_for_restore(&mut git).unwrap().unwrap();
            assert_eq!(to, PathBuf::from("/tmp/sc-relocate/git-stores/app"));
            assert!(git.config.get(CFG_DIR).is_none());
            assert_eq!(clone_path(&git).unwrap(), to);

            // Git without one: still valid, so the directory is named outright.
            let mut adopted = FileStoreDef::new("app", GIT_BACKEND).with(CFG_DIR, missing);
            relocate_for_restore(&mut adopted).unwrap().unwrap();
            assert_eq!(
                adopted.setting(CFG_DIR),
                Some("/tmp/sc-relocate/git-stores/app")
            );
            assert!(validate_file_store_config(&adopted).is_ok());

            // Git that let Saltcorn choose has nothing to move.
            let mut derived = FileStoreDef::git("app", "u");
            assert_eq!(relocate_for_restore(&mut derived).unwrap(), None);
        });
    }

    #[test]
    fn the_local_backend_declares_its_settings() {
        let spec = local_config_spec();
        let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
        assert_eq!(names, [CFG_PATH, CFG_CREATE]);

        // Every setting carries a human label, because the admin UI renders this
        // and nothing else knows what these mean.
        assert!(spec.iter().all(|f| !f.base.label.is_empty()));

        // The directory is the admin's to state; creating it is opt-in, so it
        // defaults to false rather than being required.
        let required: Vec<&str> = spec
            .iter()
            .filter(|f| f.required)
            .map(|f| f.name())
            .collect();
        assert_eq!(required, [CFG_PATH]);
        assert_eq!(
            spec.iter()
                .find(|f| f.name() == CFG_CREATE)
                .unwrap()
                .default,
            Some(serde_json::json!(false))
        );
    }

    #[test]
    fn the_registry_resolves_a_name_to_the_same_spec() {
        assert_eq!(registered_backends(), [LOCAL_BACKEND, GIT_BACKEND]);
        assert_eq!(
            backend_config_spec(LOCAL_BACKEND).unwrap(),
            local_config_spec()
        );
        assert_eq!(backend_config_spec(GIT_BACKEND).unwrap(), git_config_spec());
    }

    #[test]
    fn the_git_backend_declares_no_setting_as_required_on_its_own() {
        let spec = git_config_spec();
        let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            [CFG_URL, CFG_DIR, CFG_BRANCH, CFG_KEY_PATH, CFG_PUBLIC_KEY]
        );
        // Not even the URL: a store may instead name a directory that already
        // holds a checkout. What is required is one of the two, which is a
        // relation between fields and is checked separately.
        assert!(spec.iter().all(|f| !f.required));
        assert!(spec.iter().all(|f| !f.base.label.is_empty()));

        // The working-copy directory is settable once, when the store is
        // created: it says where the tree was put, and an edit would abandon it.
        let dir = spec.iter().find(|f| f.name() == CFG_DIR).unwrap();
        assert!(dir.create_only);
        assert!(spec.iter().filter(|f| f.create_only).count() == 1);

        // No `path` setting — that is the `local` backend's, and a git store's
        // directory is a different question with a different name.
        assert!(!names.contains(&CFG_PATH));
    }

    #[test]
    fn a_store_that_named_no_directory_is_shown_the_one_it_got() {
        // The setting is read-only on an edit, so a store that let Saltcorn
        // choose must still be able to say where its files are. Where it got to
        // is `clone_path`'s answer — here the directory the clone recorded,
        // which is what a store in this state has.
        let mut def = FileStoreDef::git("web app", "git@example.com:me/app.git");
        crate::record_clone_path(&mut def, std::path::Path::new("/data/git-stores/web_app"));
        // Nothing is stored for the setting — the directory is derived, and a
        // stored copy could disagree with the derivation …
        assert!(def.config.get(CFG_DIR).is_none());
        // … but what is shown is the answer that derivation gives.
        let shown = display_config(&def);
        assert_eq!(
            shown.get(CFG_DIR).and_then(serde_json::Value::as_str),
            Some("/data/git-stores/web_app")
        );
        // Every other setting is passed through untouched.
        assert_eq!(shown.get(CFG_URL), def.config.get(CFG_URL));

        // A store that named its own directory is left exactly as it is, and so
        // is a backend that has no such setting.
        let named = FileStoreDef::git("app", "u").with(CFG_DIR, "/srv/checkout");
        assert_eq!(display_config(&named), named.config);
        let local = FileStoreDef::local("docs", "/srv/docs");
        assert_eq!(display_config(&local), local.config);
    }

    #[test]
    fn a_git_store_with_neither_a_url_nor_a_directory_is_rejected_on_save() {
        let def = FileStoreDef::new("app", GIT_BACKEND);
        let err = validate_file_store_config(&def).unwrap_err().to_string();
        assert!(err.contains("app"), "{err}");
        assert!(err.contains(CFG_URL), "{err}");
        assert!(err.contains(CFG_DIR), "{err}");
    }

    #[test]
    fn a_git_store_with_only_a_directory_is_a_valid_definition() {
        // The whole point of the pairing: a repository already checked out on
        // the server is connected by naming its directory, with no URL at all.
        // Whether the directory holds a checkout is reachability, not structure,
        // so this passes without touching the filesystem.
        let def = FileStoreDef::new("app", GIT_BACKEND).with(CFG_DIR, "/srv/checkout");
        assert!(validate_file_store_config(&def).is_ok());

        // A blank directory is no directory, so this is the same as saying
        // nothing at all.
        let blank = FileStoreDef::new("app", GIT_BACKEND).with(CFG_DIR, "   ");
        assert!(validate_file_store_config(&blank).is_err());
    }

    #[test]
    fn a_backends_operations_are_declared_like_its_settings() {
        // The `local` backend has one, and it configures rather than acts:
        // there is nothing to do to a directory that reading and writing files
        // does not already cover, but an admin still has to be told where a
        // store may sensibly live.
        let local = backend_operations(LOCAL_BACKEND).unwrap();
        assert_eq!(
            local.iter().map(|op| op.name.as_str()).collect::<Vec<_>>(),
            [crate::local::OP_SUGGEST_DIR]
        );
        assert_eq!(local[0].scope, sc_types::OperationScope::Configure);

        let ops = backend_operations(GIT_BACKEND).unwrap();
        let names: Vec<&str> = ops.iter().map(|op| op.name.as_str()).collect();
        assert_eq!(
            names,
            [
                crate::git::OP_GENERATE_KEY,
                crate::git::OP_STATUS,
                crate::git::OP_CLONE,
                crate::git::OP_PULL,
                crate::git::OP_PUSH,
                crate::git::OP_STAGE,
                crate::git::OP_UNSTAGE,
                crate::git::OP_DISCARD,
                crate::git::OP_COMMIT,
                crate::git::OP_CHECKOUT,
            ]
        );
        // Every one carries a button label, because the admin UI renders this
        // and nothing else knows what these mean.
        assert!(ops.iter().all(|op| !op.label.is_empty()));

        // An unknown backend has no operations for the same reason it has no
        // settings spec: nothing implements it.
        assert!(backend_operations("s3").is_err());
    }

    #[tokio::test]
    async fn an_undeclared_operation_is_refused_by_name() {
        // The check is the registry's, not any backend's — which is what a
        // backend added later relies on.
        let mut def = FileStoreDef::local("docs", "/srv/docs");
        let err = run_backend_operation(&mut def, "pull", &Attrs::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("pull"), "{err}");
        assert!(err.contains(LOCAL_BACKEND), "{err}");

        // A git store gets told what there *is*, since something is available.
        let mut def = FileStoreDef::git("app", "git@example.com:me/app.git");
        let err = run_backend_operation(&mut def, "rebase", &Attrs::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rebase"), "{err}");
        assert!(err.contains(crate::git::OP_PULL), "{err}");
    }

    #[tokio::test]
    async fn an_operations_arguments_are_validated_against_what_it_declared() {
        // A commit with no message is caught by the declared `required` field,
        // in the same place and with the same message shape a missing setting
        // is — not by anything that knows what a commit is.
        let mut def = FileStoreDef::git("app", "git@example.com:me/app.git");
        let err = run_backend_operation(&mut def, crate::git::OP_COMMIT, &Attrs::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(crate::git::ARG_MESSAGE), "{err}");
    }

    #[test]
    fn an_uncloned_git_store_does_not_connect_and_says_why() {
        let mut def = FileStoreDef::git("app", "git@example.com:me/app.git");
        // Structurally correct — the admin has configured it properly …
        assert!(validate_file_store_config(&def).is_ok());
        // … but there is no working tree, and connecting must not reach the
        // network to make one. The error names the store and says what is
        // missing, so the admin knows to clone rather than to re-check the URL.
        crate::git::record_clone_path(&mut def, std::path::Path::new("/definitely/not/here"));
        let err = connect_err(&def);
        assert!(err.contains("app"), "{err}");
        assert!(err.contains("clone"), "{err}");
    }

    #[test]
    fn an_unknown_backend_has_no_spec_and_says_what_is_available() {
        let err = backend_config_spec("s3").unwrap_err().to_string();
        assert!(err.contains("s3"), "{err}");
        assert!(err.contains(LOCAL_BACKEND), "should say what is available");
    }

    #[test]
    fn a_missing_required_setting_names_the_store_the_backend_and_the_setting() {
        let def = FileStoreDef::new("docs", LOCAL_BACKEND);
        let err = validate_file_store_config(&def).unwrap_err().to_string();
        assert!(err.contains("docs"), "{err}");
        assert!(err.contains(LOCAL_BACKEND), "{err}");
        assert!(err.contains(CFG_PATH), "{err}");
        // One "invalid:" prefix, not two — the message is rebuilt, not nested.
        assert_eq!(err.matches("invalid:").count(), 1, "{err}");
    }

    #[test]
    fn an_ill_typed_or_unknown_setting_is_rejected() {
        // `path` must be text, not a number.
        let def = FileStoreDef::new("docs", LOCAL_BACKEND).with(CFG_PATH, 42);
        let err = validate_file_store_config(&def).unwrap_err().to_string();
        assert!(err.contains(CFG_PATH), "{err}");

        // A setting the backend has never heard of is a typo, not something to
        // store and ignore.
        let def = FileStoreDef::local("docs", "/srv/docs").with("pth", "/srv/docs");
        let err = validate_file_store_config(&def).unwrap_err().to_string();
        assert!(err.contains("pth"), "{err}");
    }

    #[test]
    fn a_valid_definition_validates_without_touching_the_filesystem() {
        // The path does not exist and never will; structural validation must
        // still pass, because reachability is not a save-time question. This is
        // what keeps a store whose disk was unmounted editable.
        let def = FileStoreDef::local("gone", "/definitely/not/here");
        assert!(validate_file_store_config(&def).is_ok());
    }

    #[test]
    fn connecting_yields_a_working_store() {
        let dir = temp_path("connect");
        std::fs::create_dir_all(&dir).unwrap();

        let def = FileStoreDef::local("docs", dir.to_string_lossy());
        let store = connect_from_def(&def).unwrap();
        // The instance is named after the definition, which is the key the
        // catalog registry connects it under.
        assert_eq!(store.name(), "docs");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreachable_path_fails_to_connect_but_is_not_a_config_error() {
        let def = FileStoreDef::local("gone", "/definitely/not/here");
        // Structurally fine …
        assert!(validate_file_store_config(&def).is_ok());
        // … but not currently usable. The error names the store so a boot log
        // listing several failures is readable.
        let err = connect_err(&def);
        assert!(err.contains("gone"), "{err}");
    }

    #[test]
    fn create_makes_the_directory_only_when_asked() {
        let dir = temp_path("create");
        assert!(!dir.exists());

        // Without `create`, a missing directory is a failure to connect — a
        // typo'd path must not silently become a new empty store.
        let plain = FileStoreDef::local("new", dir.to_string_lossy());
        assert!(connect_from_def(&plain).is_err());
        assert!(!dir.exists(), "connecting must not have created it");

        // With it, the directory is created and the store connects.
        let creating = FileStoreDef::local("new", dir.to_string_lossy()).with(CFG_CREATE, true);
        let store = connect_from_def(&creating).unwrap();
        assert_eq!(store.name(), "new");
        assert!(dir.is_dir());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connecting_validates_first() {
        // A structurally broken definition gets the clear configuration error,
        // not a confusing filesystem one.
        let def = FileStoreDef::new("docs", "s3").with("bucket", "things");
        let err = connect_err(&def);
        assert!(err.contains("s3"), "{err}");
    }
}
