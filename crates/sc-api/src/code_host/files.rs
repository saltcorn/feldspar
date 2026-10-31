//! Files in a code body: the host behind `fs` (§10.1, the addendum "Files in a
//! code body").
//!
//! `sc-expr` runs the JavaScript and knows nothing about stores — its
//! [`FileHost`] seam is one JSON operation in, one JSON value out, which is what
//! makes it the seam §15's other guest languages implement rather than a
//! JavaScript feature. [`FileStoreHost`] is the implementation of that seam for
//! *this* server's stores, and it lives here for the reason [`TableHost`] does:
//! everything it needs is already at this layer — the catalog that resolves a
//! store name to a connected [`FileStore`], the store definitions that carry the
//! store-wide access floor, and `sc-files`'s path-cumulative rule.
//!
//! [`TableHost`]: super::TableHost
//!
//! # What crosses
//!
//! One operation per method the guest calls on a file — `await file.text()` is
//! one round trip — as a flat JSON object naming the store, the path, whose
//! authority it runs under, and whatever the operation itself needs:
//!
//! ```json
//! { "op": "read", "store": "uploads", "path": "notes/a.txt",
//!   "authority": "admin", "encoding": "text" }
//! ```
//!
//! **Nothing in it is trusted.** The store is resolved through the catalog, the
//! path is re-checked for the traversal the guest already refused, the authority
//! decides the role every check runs at, and an unknown operation or an unknown
//! field is refused naming it rather than ignored.
//!
//! # Authority
//!
//! The **admin's** by default, exactly as `db`'s is and for the same reason: a
//! trigger is server-side configuration, and a file an ordinary caller may not
//! read is the archetype of what a trigger exists to write. `fs(…).asUser()`
//! delegates to the event's caller, and then §14.1's path-cumulative `min_role`
//! rule decides every operation — the store's floor, then every directory on the
//! path, then the entry itself, most restrictive wins.
//!
//! Two consequences worth stating, because they are decisions rather than
//! consequences of the code:
//!
//! - under **admin** authority no rule is consulted at all. Not a shortcut taken
//!   for speed — `ROLE_ADMIN` clears every rule that can be written, so
//!   consulting them would be reading metadata up the whole path to reach a
//!   foregone conclusion — but it does mean the store floor is not read from the
//!   database on the common path either.
//! - a **delegated** `setMeta` may tighten a rule and never loosen one. A caller
//!   who can reach a file can already read it; letting them publish it to
//!   everyone would make a non-admin the author of an access rule, which is not
//!   what delegating a trigger to its caller was meant to grant.
//!
//! # Owners
//!
//! §2.1 of the F2 design: a file's owner may read it whatever its rule, and only
//! its owner (or admin authority) may change it. A **delegated** write records
//! the caller as the owner of what it creates, so `fs(s).asUser()` is how a body
//! saves a user's private file. An **admin** write records nobody, unless the
//! body says who with `write(bytes, { owner, minRole, filename })`. Under user
//! authority the owner cannot be chosen or changed (not by `write`, not by
//! `setMeta`); under admin authority it can.
//!
//! # Bounds
//!
//! The **operation budget** is the runtime's ([`DEFAULT_MAX_FILE_OPS`]), since
//! only it can count what the guest does. What is here is what only this side
//! knows: how many bytes may cross the seam ([`MAX_FILE_BYTES`]) and how many
//! may be copied *without* crossing it ([`MAX_COPY_BYTES`]). A read past the cap
//! is **refused rather than truncated**, because a body handed the first 8 MB of
//! a larger file would go on to compute a wrong answer out of a right-looking
//! one — the same rule the row cap follows. A larger file is read in pieces
//! (`file.read({ offset, length })`), built with `append` and `concat`, and
//! hashed with `sha256()`, none of which carries more than one piece across
//! (TODO F2.8). Both bounds are server settings (`--code-max-file-bytes`,
//! `--code-max-copy-bytes`), set once at start-up with [`set_file_limits`].
//!
//! [`DEFAULT_MAX_FILE_OPS`]: sc_expr::DEFAULT_MAX_FILE_OPS

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use sc_action::FileUrls;
use sc_catalog::{Catalog, load_file_store_by_name};
use sc_error::{Error, Result};
use sc_expr::FileHost;
use sc_files::{
    Entry, FILENAME_ATTRIBUTE, FileCaller, FileMeta, FileStat, FileStore, check_access,
    check_write, effective_min_role, list_visible, mime_for_path, record_owner,
};
use serde::Deserialize;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Authority;

/// How many bytes one read or one write may carry **across the seam**.
///
/// The seam is JSON and bytes travel base64, so a read is materialised twice —
/// once in the host and once in the isolate, a third larger again as text. Eight
/// megabytes is the same ceiling a `fetch` response has, and for the same
/// reason: it is generous for what a body does with the bytes it can actually
/// hold, and small enough that a hundred resident runs are not the process.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// How many bytes one **copy** may move.
///
/// Larger than [`MAX_FILE_BYTES`] by a lot, because the bytes never enter the
/// isolate: `await backup.write(original)` and `await file.copyTo(…)` are read
/// and written here, so what bounds them is what the *server* can hold rather
/// than what a run may. It is the upload route's own cap, since that is the
/// other place a file of this size is handled in one piece.
pub const MAX_COPY_BYTES: u64 = 256 * 1024 * 1024;

/// The server's `--code-max-file-bytes`, which every new [`FileStoreHost`]
/// starts from.
static FILE_BYTES: AtomicU64 = AtomicU64::new(MAX_FILE_BYTES);
/// The server's `--code-max-copy-bytes`, likewise.
static COPY_BYTES: AtomicU64 = AtomicU64::new(MAX_COPY_BYTES);

/// Set the bounds every code body's `fs` starts from: how many bytes one read
/// or write may carry across the seam, and how many one host-side copy or
/// `concat` may move. Process-wide, as the code workers are — the server calls
/// it once, from its configuration.
pub fn set_file_limits(max_file_bytes: u64, max_copy_bytes: u64) {
    FILE_BYTES.store(max_file_bytes, Ordering::Relaxed);
    COPY_BYTES.store(max_copy_bytes, Ordering::Relaxed);
}

/// The `fs` handle of one code-body run, on the host's side.
///
/// **One per run**, like [`TableHost`](super::TableHost): the event's caller
/// rides on every access check it makes, and the store floors it reads are
/// cached for the run — both of which would be wrong to share between two runs
/// carrying different authority.
pub struct FileStoreHost<'a> {
    /// The catalog every store name is resolved through, and the connection the
    /// store definitions are read over.
    catalog: &'a Catalog,
    /// The event's caller — the role a delegated operation is checked against
    /// (public for an event with no caller at all) and the user id the owner
    /// rule compares with and a delegated write records.
    caller: FileCaller,
    /// The most bytes one read or write may carry across the seam.
    max_bytes: u64,
    /// The most bytes one host-side copy may move.
    max_copy_bytes: u64,
    /// The store-wide floors already read, by store name.
    ///
    /// A floor is a row in `_fd_file_stores`, so reading it per operation would
    /// put a query in front of every file a loop touches. It cannot change under
    /// a run that is measured in milliseconds, and a run that raced an admin
    /// editing the store would be as arbitrary either way.
    floors: Mutex<BTreeMap<String, Option<u8>>>,
    /// Where the run's application serves store files, for `file.url()`.
    urls: Option<Arc<dyn FileUrls>>,
    /// What an operation that names no authority runs under: the admin's for a
    /// trigger, the caller's for a body answering a request
    /// ([`defaulting_to`](FileStoreHost::defaulting_to)).
    default_authority: Authority,
}

impl<'a> FileStoreHost<'a> {
    /// A handle over `catalog`, with the default bounds and no caller — which
    /// means delegated operations run as the public role.
    pub fn new(catalog: &'a Catalog) -> FileStoreHost<'a> {
        FileStoreHost {
            catalog,
            caller: FileCaller::anonymous(),
            max_bytes: FILE_BYTES.load(Ordering::Relaxed),
            max_copy_bytes: COPY_BYTES.load(Ordering::Relaxed),
            floors: Mutex::new(BTreeMap::new()),
            urls: None,
            default_authority: Authority::Admin,
        }
    }

    /// Run an operation that names no authority under `authority` rather than
    /// the admin's — [`TableHost::defaulting_to`]'s rule, so a request-answering
    /// body's `fs("x")` is checked against the store's file rules unless it says
    /// `asAdmin()`.
    ///
    /// [`TableHost::defaulting_to`]: super::TableHost::defaulting_to
    #[must_use]
    pub fn defaulting_to(mut self, authority: Authority) -> FileStoreHost<'a> {
        self.default_authority = authority.resolve(&Authority::Admin);
        self
    }

    /// Attach the event's caller — the role it was served at, which is what a
    /// delegated operation is checked against, and the user's fields, of which
    /// only `id` is read: it is what the owner rule (§2.1) compares with and
    /// what a delegated write records as the owner.
    ///
    /// The same pair [`TableHost::caused_by`] takes, so a call site hands both
    /// hosts the same thing.
    ///
    /// [`TableHost::caused_by`]: super::TableHost::caused_by
    #[must_use]
    pub fn caused_by(mut self, role: u8, user: Option<&Json>) -> FileStoreHost<'a> {
        let user_id = user.and_then(|u| u.get("id")).and_then(|id| match id {
            Json::String(s) => Some(s.clone()),
            Json::Number(n) => Some(n.to_string()),
            _ => None,
        });
        self.caller = FileCaller::new(role, user_id);
        self
    }

    /// Attach where the run's application serves store files, which is what
    /// `file.url()` answers from. Without it every `url()` is `null`.
    #[must_use]
    pub fn with_urls(mut self, urls: Option<Arc<dyn FileUrls>>) -> FileStoreHost<'a> {
        self.urls = urls;
        self
    }

    /// Set the byte bounds, in place of the server's ([`set_file_limits`]).
    #[must_use]
    pub fn with_limits(mut self, max_bytes: u64, max_copy_bytes: u64) -> FileStoreHost<'a> {
        self.max_bytes = max_bytes;
        self.max_copy_bytes = max_copy_bytes;
        self
    }

    /// The connected store of that name, or an error naming it.
    fn store(&self, name: &str) -> Result<Arc<dyn FileStore>> {
        self.catalog.require_file_store(name)
    }

    /// The store-wide floor, read once per store per run.
    ///
    /// `None` for a store with no definition row — an ephemeral `--file-store` —
    /// which is the same answer every other file endpoint resolves for one.
    async fn floor(&self, name: &str) -> Result<Option<u8>> {
        if let Ok(cache) = self.floors.lock()
            && let Some(known) = cache.get(name)
        {
            return Ok(*known);
        }
        let floor = load_file_store_by_name(self.catalog, name)
            .await?
            .and_then(|def| def.min_role);
        if let Ok(mut cache) = self.floors.lock() {
            cache.insert(name.to_owned(), floor);
        }
        Ok(floor)
    }

    /// Check that this operation may **read** `path` in `store`.
    ///
    /// Admin authority clears every rule that can be written, so the whole
    /// path-cumulative walk — one metadata read per directory, plus the store
    /// definition — is skipped rather than performed to reach a foregone
    /// conclusion.
    async fn permit(
        &self,
        store: &dyn FileStore,
        name: &str,
        path: &str,
        authority: &Authority,
    ) -> Result<()> {
        if authority.is_admin() {
            return Ok(());
        }
        let floor = self.floor(name).await?;
        check_access(store, floor, path, &self.caller).await
    }

    /// Check that this operation may **write** `path` in `store`: create,
    /// replace, delete, move or re-rule it. Under user authority somebody
    /// else's file is refused, however low its rule (§2.1).
    async fn permit_write(
        &self,
        store: &dyn FileStore,
        name: &str,
        path: &str,
        authority: &Authority,
    ) -> Result<()> {
        if authority.is_admin() {
            return Ok(());
        }
        let floor = self.floor(name).await?;
        check_write(store, floor, path, &self.caller).await
    }

    /// The metadata a write's options (`write(bytes, { owner, minRole,
    /// filename })`) and its authority ask to be recorded, checked **before**
    /// the bytes land so a refusal writes nothing.
    async fn planned_meta(
        &self,
        store: &dyn FileStore,
        req: &FileRequest,
        path: &str,
    ) -> Result<PlannedMeta> {
        let owner = match (&req.authority, owner_option(req)?) {
            // A delegated write is the caller's file: what it creates is theirs,
            // and naming somebody else would be handing them a file — or taking
            // one — which delegation was never meant to grant.
            (Authority::User, Some(Some(named)))
                if Some(&named) != self.caller.user_id.as_ref() =>
            {
                return Err(Error::auth(format!(
                    "not permitted to choose the owner of {path:?} in file store {} — a \
                     delegated body's files are its caller's",
                    store.name()
                )));
            }
            (Authority::User, Some(None)) => {
                return Err(Error::auth(format!(
                    "not permitted to clear the owner of {path:?} in file store {}",
                    store.name()
                )));
            }
            (Authority::User, _) => self.caller.user_id.clone().map(OwnerPlan::IfUnset),
            (_, Some(Some(named))) => Some(OwnerPlan::Set(named)),
            (_, Some(None)) => Some(OwnerPlan::Clear),
            (_, None) => None,
        };
        if let Some(role) = req.min_role {
            valid_role(role)?;
            if req.authority == Authority::User {
                self.only_tightens(store, req, path, Some(role)).await?;
            }
        }
        Ok(PlannedMeta {
            owner,
            min_role: req.min_role,
            filename: req.filename.clone(),
        })
    }

    /// Record what [`planned_meta`](Self::planned_meta) decided, once the
    /// entry exists. Nothing is written when nothing was asked for.
    async fn apply_meta(&self, store: &dyn FileStore, path: &str, plan: PlannedMeta) -> Result<()> {
        if let Some(OwnerPlan::IfUnset(owner)) = &plan.owner
            && plan.min_role.is_none()
            && plan.filename.is_none()
        {
            record_owner(store, path, owner).await;
            return Ok(());
        }
        if plan.owner.is_none() && plan.min_role.is_none() && plan.filename.is_none() {
            return Ok(());
        }
        let mut meta = store.get_meta(path).await.unwrap_or_default();
        match plan.owner {
            Some(OwnerPlan::IfUnset(owner)) => {
                meta.owner.get_or_insert(owner);
            }
            Some(OwnerPlan::Set(owner)) => meta.owner = Some(owner),
            Some(OwnerPlan::Clear) => meta.owner = None,
            None => {}
        }
        if let Some(role) = plan.min_role {
            meta.min_role = Some(role);
        }
        if let Some(filename) = plan.filename {
            meta.attributes
                .insert(FILENAME_ATTRIBUTE.to_owned(), filename);
        }
        store.set_meta(path, &meta).await
    }

    /// Refuse a delegated change that would make `path` reachable by anyone it
    /// is not reachable by now — a delegated body may tighten a rule and never
    /// loosen one.
    async fn only_tightens(
        &self,
        store: &dyn FileStore,
        req: &FileRequest,
        path: &str,
        min_role: Option<u8>,
    ) -> Result<()> {
        let floor = self.floor(&req.store).await?;
        let before = effective_min_role(store, floor, path).await?;
        let parent = match path.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };
        let above = effective_min_role(store, floor, parent).await?;
        let after = tightest(above, min_role);
        if looser(after, before) {
            return Err(Error::auth(format!(
                "not permitted to loosen the access rule on {path:?} in file store {} — a delegated body may only tighten one",
                store.name()
            )));
        }
        Ok(())
    }
}

/// One operation, as it arrives from the guest.
///
/// Flat rather than an enum with per-operation shapes, because every operation
/// carries the same three things (the store, the path and the authority) and the
/// difference between them is one or two fields. `deny_unknown_fields` is the
/// safeguard that keeps it honest: a field this server does not know is a
/// refusal naming it, not a setting silently ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileRequest {
    /// Which operation.
    op: String,
    /// The store's name, as the catalog knows it.
    store: String,
    /// Whose authority it runs under.
    #[serde(default)]
    authority: Authority,
    /// The path, store-relative; `""` is the root.
    #[serde(default)]
    path: String,
    /// `read`: `text` or `base64`.
    #[serde(default)]
    encoding: Option<String>,
    /// `write`: the content, as text.
    #[serde(default)]
    text: Option<String>,
    /// `write`: the content, as bytes.
    #[serde(default)]
    base64: Option<String>,
    /// `write`, `copy`, `rename`: whether an existing destination may be
    /// replaced.
    #[serde(default)]
    overwrite: bool,
    /// `copy`, `rename`: the destination store, when it is not this one.
    #[serde(default)]
    to_store: Option<String>,
    /// `copy`, `rename`: the destination path.
    #[serde(default)]
    to_path: Option<String>,
    /// `setMeta`: the rule to set on this entry, or `null` for none.
    #[serde(default)]
    min_role: Option<u8>,
    /// `setMeta`: the entry's attributes, which replace what was there.
    #[serde(default)]
    attributes: Option<BTreeMap<String, String>>,
    /// `write`, `copy`, `setMeta`: the owner to record — a user id, or `null`
    /// for none. Admin authority only (§2.1). Absent leaves it alone, which is
    /// why it is read as "present", so `null` is not mistaken for absent.
    #[serde(default, deserialize_with = "present")]
    owner: Option<Json>,
    /// `write`, `copy`: the name to serve the file under
    /// (`attributes.filename`), when it is not the path's own.
    #[serde(default)]
    filename: Option<String>,
    /// `read`: where a partial read starts (TODO F2.8).
    #[serde(default)]
    offset: Option<u64>,
    /// `read`: how many bytes a partial read carries, at most.
    #[serde(default)]
    length: Option<u64>,
    /// `concat`: the files to join, in order, as paths in this store.
    #[serde(default)]
    parts: Option<Vec<String>>,
    /// `url`: whether the URL asks for an `attachment`.
    #[serde(default)]
    download: bool,
}

/// What a write records beside its bytes.
struct PlannedMeta {
    owner: Option<OwnerPlan>,
    min_role: Option<u8>,
    filename: Option<String>,
}

/// How a write treats the owner.
enum OwnerPlan {
    /// The delegated caller, unless an owner is already recorded — the creator
    /// stays the owner when somebody rewrites the bytes.
    IfUnset(String),
    /// What an admin-authority body named.
    Set(String),
    /// None, as an admin-authority body asked with `owner: null`.
    Clear,
}

/// A field that was there, whatever it held — `null` included.
fn present<'de, D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Option<Json>, D::Error> {
    Json::deserialize(de).map(Some)
}

/// The `owner` option: absent (`None`), `null` (`Some(None)`) or a user id.
fn owner_option(req: &FileRequest) -> Result<Option<Option<String>>> {
    match &req.owner {
        None => Ok(None),
        Some(Json::Null) => Ok(Some(None)),
        Some(Json::String(id)) if !id.trim().is_empty() => Ok(Some(Some(id.trim().to_owned()))),
        Some(Json::Number(n)) => Ok(Some(Some(n.to_string()))),
        Some(other) => Err(Error::invalid(format!(
            "`owner` is a user id, or null for none — not {other}"
        ))),
    }
}

/// Refuse a role outside the scale.
fn valid_role(role: u8) -> Result<()> {
    if !(1..=sc_auth::ROLE_PUBLIC).contains(&role) {
        return Err(Error::invalid(format!(
            "`minRole` is a role from 1 (admin) to {} (public), not {role}",
            sc_auth::ROLE_PUBLIC
        )));
    }
    Ok(())
}

/// The server-log line for a file operation the access rules refused, or `None`
/// when `err` is not a refusal (a missing file, a bad argument).
fn refusal_warning(req: &FileRequest, caller: &FileCaller, err: &Error) -> Option<String> {
    let message = err.auth_message()?;
    let who = match &caller.user_id {
        Some(id) => format!("user `{id}` (role {})", caller.role),
        None if caller.role == sc_auth::ROLE_PUBLIC => "the public role".to_owned(),
        None => format!("role {}", caller.role),
    };
    Some(format!(
        "feldspar: warning: a code body's file `{}` of {:?} in store `{}`, as {who}, was \
         refused: {message}",
        req.op, req.path, req.store
    ))
}

#[async_trait]
impl FileHost for FileStoreHost<'_> {
    async fn files(&self, request: Json) -> Result<Json> {
        let mut req: FileRequest = serde_json::from_value(request).map_err(|e| {
            Error::invalid(format!(
                "this file request is not one the server understands: {e}"
            ))
        })?;
        req.authority = req.authority.resolve(&self.default_authority);
        let answer = self.run(&req).await;
        // A refusal is the body's to catch, and cleanup code catches it without
        // a word — a delete that left somebody else's upload behind, a shared
        // file that stopped updating. The server log says so either way.
        if let Err(err) = &answer
            && let Some(warning) = refusal_warning(&req, &self.caller, err)
        {
            eprintln!("{warning}");
        }
        answer
    }

    fn store_names(&self) -> Vec<String> {
        // The connected stores, which is what a body can actually open — a
        // definition that failed to connect is not one `fs(name)` should promise.
        // An error here is answered with no list at all, and the guest then lets
        // every name through to be decided by the operation itself.
        self.catalog.file_store_names().unwrap_or_default()
    }
}

impl FileStoreHost<'_> {
    /// Answer one operation.
    async fn run(&self, req: &FileRequest) -> Result<Json> {
        // Two of the four authorities, and the other two refused by name. `fs`
        // offers `asAdmin()` and `asUser()` and nothing else, so `public` and a
        // named user can only arrive from a guest that built the request itself
        // — and this surface has no rule to apply them with: its access check
        // reads *this run's* role (§10.1's file rule), which is not the role
        // either of them means. Answering them as the run's own caller would be
        // a body asking to be treated as somebody smaller and being treated as
        // itself, which is the failure principle 5 exists to prevent.
        if !matches!(req.authority, Authority::Admin | Authority::User) {
            return Err(Error::invalid(format!(
                "`{}` is not an authority the file surface carries: a file operation is the \
                 admin's or this event's caller's, and nothing here can act as {} \
                 (the file rule is checked against the role this event was served at)",
                req.authority.spelling(),
                req.authority.spelling()
            )));
        }
        let path = clean_path(&req.path)?;
        let store = self.store(&req.store)?;
        match req.op.as_str() {
            "read" => self.read(store.as_ref(), req, &path).await,
            "write" => self.write(store.as_ref(), req, &path).await,
            "stat" => self.stat(store.as_ref(), req, &path).await,
            "list" => self.list(store.as_ref(), req, &path).await,
            "mkdir" => {
                self.permit_write(store.as_ref(), &req.store, &path, &req.authority)
                    .await?;
                store.mkdir(&path).await?;
                // A delegated body's directory is its caller's, as its files are.
                if req.authority == Authority::User
                    && let Some(owner) = &self.caller.user_id
                {
                    record_owner(store.as_ref(), &path, owner).await;
                }
                Ok(Json::Null)
            }
            "delete" => {
                self.permit_write(store.as_ref(), &req.store, &path, &req.authority)
                    .await?;
                Ok(Json::Bool(store.delete(&path).await?))
            }
            "copy" | "rename" => self.relocate(store.as_ref(), req, &path).await,
            "append" => self.append(store.as_ref(), req, &path).await,
            "concat" => self.concat(store.as_ref(), req, &path).await,
            "sha256" => self.sha256(store.as_ref(), req, &path).await,
            "url" => Ok(self.url(req, &path)),
            "meta" => self.meta(store.as_ref(), req, &path).await,
            "setMeta" => self.set_meta(store.as_ref(), req, &path).await,
            other => Err(Error::invalid(format!(
                "`{other}` is not a file operation this server knows"
            ))),
        }
    }

    /// A `read`: the whole file, as text or as bytes.
    async fn read(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit(store, &req.store, path, &req.authority).await?;
        let name = store.name();
        // Asked before the bytes are read, so a file too large to carry is
        // refused without first being loaded into this process to prove it.
        match store.stat(path).await? {
            None => {
                return Err(Error::not_found(format!(
                    "{path:?} does not exist in file store {name}"
                )));
            }
            Some(FileStat { is_dir: true, .. }) => {
                return Err(Error::invalid(format!(
                    "{path:?} in file store {name} is a directory — list it rather than reading it"
                )));
            }
            Some(stat) if req.offset.is_some() || req.length.is_some() => {
                return self.read_part(store, req, path, stat.size).await;
            }
            Some(stat) => self.within_read_cap(stat.size, path, name)?,
        }
        let bytes = store.read(path).await?;
        // Again on what actually came back: a store whose `stat` and whose bytes
        // disagree must not be the way past the bound.
        self.within_read_cap(bytes.len() as u64, path, name)?;
        match req.encoding.as_deref() {
            // The default is text, which is what `file.text()` asks for and what
            // an adapter that says nothing almost certainly means.
            None | Some("text") => {
                let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
                    Error::invalid(format!(
                        "{path:?} in file store {name} is not valid UTF-8 — read it with `bytes()`"
                    ))
                })?;
                Ok(serde_json::json!({ "text": text }))
            }
            Some("base64") => Ok(serde_json::json!({ "base64": BASE64.encode(&bytes) })),
            Some(other) => Err(Error::invalid(format!(
                "`{other}` is not an encoding a read can answer in; they are: text, base64"
            ))),
        }
    }

    /// A partial `read` — `file.read({ offset, length })`: at most `length`
    /// bytes from `offset`, streamed from the store so a file of any size can
    /// be read a piece at a time. Bytes, always: a piece of a UTF-8 file need
    /// not end on a character.
    async fn read_part(
        &self,
        store: &dyn FileStore,
        req: &FileRequest,
        path: &str,
        size: u64,
    ) -> Result<Json> {
        let start = req.offset.unwrap_or(0).min(size);
        let end = match req.length {
            Some(length) => start.saturating_add(length).min(size),
            None => size,
        };
        // The cap is on the piece, so asking for more than may cross is a
        // refusal naming the limit rather than a short read the body would
        // mistake for the end of the file.
        self.within_read_cap(end - start, path, store.name())?;
        let mut reader = store.open_read(path, Some(start..end)).await?;
        let mut bytes = Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
        reader.read_to_end(&mut bytes).await.map_err(|e| {
            Error::file(format!(
                "reading {path:?} in file store {}: {e}",
                store.name()
            ))
        })?;
        Ok(serde_json::json!({ "base64": BASE64.encode(&bytes) }))
    }

    /// An `append`: add bytes to the end of a file, creating it (and its
    /// parents) when it is not there. A new file records its owner and options
    /// as a `write` would; an existing one keeps what it has.
    async fn append(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit_write(store, &req.store, path, &req.authority)
            .await?;
        let name = store.name();
        if path.is_empty() {
            return Err(Error::invalid(format!(
                "the root of file store {name} is not a file to append to"
            )));
        }
        let bytes = self.payload(req, path, name)?;
        let existing = store.stat(path).await?;
        if existing.as_ref().is_some_and(|s| s.is_dir) {
            return Err(Error::invalid(format!(
                "{path:?} in file store {name} is a directory, not a file to append to"
            )));
        }
        let plan = match existing {
            None => Some(self.planned_meta(store, req, path).await?),
            Some(_) => None,
        };
        let mut writer = store.open_append(path).await?;
        let io = |e: std::io::Error| {
            Error::file(format!("appending to {path:?} in file store {name}: {e}"))
        };
        writer.write_all(&bytes).await.map_err(io)?;
        writer.shutdown().await.map_err(io)?;
        drop(writer);
        if let Some(plan) = plan {
            self.apply_meta(store, path, plan).await?;
        }
        Ok(serde_json::json!({ "bytes": bytes.len() }))
    }

    /// A `concat`: join `parts` (files of this store), in order, into this
    /// file, replacing it — host-side, so the result may be as large as one
    /// copy may move however small a read is.
    async fn concat(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        let name = store.name();
        let Some(parts) = &req.parts else {
            return Err(Error::invalid("a `concat` names the files it joins"));
        };
        if parts.is_empty() {
            return Err(Error::invalid("a `concat` joins at least one file"));
        }
        if path.is_empty() {
            return Err(Error::invalid(format!(
                "the root of file store {name} is not a file to join into"
            )));
        }
        let parts = parts
            .iter()
            .map(|p| clean_path(p))
            .collect::<Result<Vec<String>>>()?;
        let mut total = 0u64;
        for part in &parts {
            self.permit(store, &req.store, part, &req.authority).await?;
            match store.stat(part).await? {
                None => {
                    return Err(Error::not_found(format!(
                        "{part:?} does not exist in file store {name}, so it cannot be joined"
                    )));
                }
                Some(FileStat { is_dir: true, .. }) => {
                    return Err(Error::invalid(format!(
                        "{part:?} in file store {name} is a directory, not a part to join"
                    )));
                }
                Some(stat) => total = total.saturating_add(stat.size),
            }
        }
        if total > self.max_copy_bytes {
            return Err(Error::invalid(format!(
                "joining into {path:?} in file store {name} would make {} MB, and one host-side \
                 operation may move {} MB (--code-max-copy-bytes)",
                total / (1024 * 1024),
                self.max_copy_bytes / (1024 * 1024),
            )));
        }
        self.permit_write(store, &req.store, path, &req.authority)
            .await?;
        if parts.iter().any(|p| p == path) {
            return Err(Error::invalid(format!(
                "{path:?} cannot be one of the parts joined into it"
            )));
        }
        let plan = self.planned_meta(store, req, path).await?;
        let size = store.concat(&parts, path).await?;
        self.apply_meta(store, path, plan).await?;
        Ok(serde_json::json!({ "bytes": size }))
    }

    /// A `sha256`: the file's SHA-256, as lower-case hex, taken over the
    /// streamed file — no size limit, and nothing in the isolate.
    async fn sha256(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit(store, &req.store, path, &req.authority).await?;
        let name = store.name();
        match store.stat(path).await? {
            None => {
                return Err(Error::not_found(format!(
                    "{path:?} does not exist in file store {name}"
                )));
            }
            Some(FileStat { is_dir: true, .. }) => {
                return Err(Error::invalid(format!(
                    "{path:?} in file store {name} is a directory, and only a file has a digest"
                )));
            }
            Some(_) => {}
        }
        let mut reader = store.open_read(path, None).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| Error::file(format!("reading {path:?} in file store {name}: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(serde_json::json!({ "hex": hex }))
    }

    /// A `url`: where the run's application serves this file, or `null` when
    /// no route does. It reads nothing from the store — whether the file may
    /// be read is decided when the URL is fetched.
    fn url(&self, req: &FileRequest, path: &str) -> Json {
        let url = self
            .urls
            .as_ref()
            .and_then(|urls| urls.file_url(&req.store, path));
        match url {
            Some(url) if req.download => Json::String(format!("{url}?download=1")),
            Some(url) => Json::String(url),
            None => Json::Null,
        }
    }

    /// The bytes a `write` or an `append` carries, within what may cross.
    fn payload(&self, req: &FileRequest, path: &str, name: &str) -> Result<Bytes> {
        let bytes = match (&req.text, &req.base64) {
            (Some(text), None) => Bytes::from(text.clone().into_bytes()),
            (None, Some(encoded)) => Bytes::from(BASE64.decode(encoded).map_err(|e| {
                Error::invalid(format!("the bytes to write are not valid base64: {e}"))
            })?),
            _ => {
                return Err(Error::invalid(
                    "a write carries exactly one of `text` and `base64`",
                ));
            }
        };
        if bytes.len() as u64 > self.max_bytes {
            return Err(Error::invalid(format!(
                "writing {path:?} to file store {name} would carry {} MB, and one write may carry \
                 {} MB (--code-max-file-bytes) — write a large file in pieces with append()",
                bytes.len() / (1024 * 1024),
                self.max_bytes / (1024 * 1024),
            )));
        }
        Ok(bytes)
    }

    /// A `write`: create or replace, making the parent directories on the way.
    async fn write(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit_write(store, &req.store, path, &req.authority)
            .await?;
        let name = store.name();
        if path.is_empty() {
            return Err(Error::invalid(format!(
                "the root of file store {name} is not a file to write to"
            )));
        }
        let bytes = self.payload(req, path, name)?;
        // The existence check `create()` is: refused here rather than left to the
        // store, so the message says which file and in which store.
        if !req.overwrite {
            self.refuse_existing(store, path).await?;
        }
        let plan = self.planned_meta(store, req, path).await?;
        let written = bytes.len();
        store.write(path, bytes).await?;
        self.apply_meta(store, path, plan).await?;
        Ok(serde_json::json!({ "bytes": written }))
    }

    /// A `stat`: the entry's own facts, or `null` when nothing is there.
    async fn stat(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit(store, &req.store, path, &req.authority).await?;
        let Some(stat) = store.stat(path).await? else {
            return Ok(Json::Null);
        };
        Ok(stat_json(path, &stat))
    }

    /// A `list`: the direct children, filtered to what the authority may see.
    async fn list(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        // Filtered rather than refused, exactly as the browse endpoint is: naming
        // an entry the caller cannot open leaks what the rule was set to hide.
        // And owner-aware: a delegated listing of a folder the caller may not
        // read still shows the files they own in it (§2.1).
        let visible: Vec<Entry> = if req.authority == Authority::User {
            let floor = self.floor(&req.store).await?;
            list_visible(store, floor, path, &self.caller)
                .await?
                .into_iter()
                .map(|v| v.entry)
                .collect()
        } else {
            store.list(path).await?
        };
        Ok(Json::Array(visible.iter().map(entry_json).collect()))
    }

    /// A `copy` or a `rename`, within one store or across two.
    ///
    /// Both are checked at **both ends** — the source is read and the
    /// destination is written, so both have to be permitted — and both refuse an
    /// existing destination unless the operation said otherwise, since a silent
    /// replace is a lost file with no undo.
    async fn relocate(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        let Some(to_path) = req.to_path.as_deref() else {
            return Err(Error::invalid(format!(
                "a `{}` names where it is going",
                req.op
            )));
        };
        let to_path = clean_path(to_path)?;
        if to_path.is_empty() {
            return Err(Error::invalid(
                "the root of a file store is not a destination",
            ));
        }
        let to_name = req.to_store.clone().unwrap_or_else(|| req.store.clone());
        let same_store = to_name == req.store;
        let dest = if same_store {
            None
        } else {
            Some(self.store(&to_name)?)
        };
        let dest_store = dest.as_deref().unwrap_or(store);
        // A copy reads its source; a move takes it away, which is a write.
        if req.op == "rename" {
            self.permit_write(store, &req.store, path, &req.authority)
                .await?;
        } else {
            self.permit(store, &req.store, path, &req.authority).await?;
        }
        self.permit_write(dest_store, &to_name, &to_path, &req.authority)
            .await?;
        let plan = self.planned_meta(dest_store, req, &to_path).await?;
        if !req.overwrite {
            self.refuse_existing(dest_store, &to_path).await?;
        }
        // A move within one store is the store's own rename: atomic where the
        // backend is, and it does not read the bytes at all.
        if same_store && req.op == "rename" {
            store.rename(path, &to_path).await?;
            // A moved file keeps its metadata, owner included: moving somebody's
            // file does not make it the mover's.
            return Ok(Json::Null);
        }
        let name = store.name();
        let size = match store.stat(path).await? {
            None => {
                return Err(Error::not_found(format!(
                    "{path:?} does not exist in file store {name}"
                )));
            }
            Some(FileStat { is_dir: true, .. }) => {
                return Err(Error::invalid(format!(
                    "{path:?} in file store {name} is a directory, and a directory is not copied as a file"
                )));
            }
            Some(stat) => stat.size,
        };
        if size > self.max_copy_bytes {
            return Err(Error::invalid(format!(
                "{path:?} in file store {name} is {} MB, and one copy may move {} MB",
                size / (1024 * 1024),
                self.max_copy_bytes / (1024 * 1024),
            )));
        }
        // The bytes stay in this process: a copy is not bounded by what one read
        // may carry into the isolate, because it never goes there.
        let bytes = store.read(path).await?;
        let moved = bytes.len();
        dest_store.write(&to_path, bytes).await?;
        self.apply_meta(dest_store, &to_path, plan).await?;
        // A cross-store move is a copy and then a delete, and it is **not**
        // atomic: two stores are two filesystems, and there is no rename across
        // them to borrow. The delete comes last, so an interrupted move leaves
        // the file where it was rather than nowhere.
        if req.op == "rename" {
            store.delete(path).await?;
        }
        Ok(serde_json::json!({ "bytes": moved }))
    }

    /// A `meta`: the rule set on this entry, the rule that applies, and the
    /// attributes.
    async fn meta(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit(store, &req.store, path, &req.authority).await?;
        let meta = store.get_meta(path).await?;
        let floor = self.floor(&req.store).await?;
        let effective = effective_min_role(store, floor, path).await?;
        Ok(serde_json::json!({
            "minRole": meta.min_role,
            // What actually applies, given the store and every directory above:
            // reporting only what is set here would let a body believe a file is
            // reachable when its folder has locked it.
            "effectiveMinRole": effective,
            "owner": meta.owner,
            "attributes": meta.attributes,
        }))
    }

    /// A `setMeta`: replace this entry's metadata.
    ///
    /// The owner is kept unless an admin-authority body names a new one (or
    /// `null`); under user authority it cannot be changed at all (§2.1).
    async fn set_meta(&self, store: &dyn FileStore, req: &FileRequest, path: &str) -> Result<Json> {
        self.permit_write(store, &req.store, path, &req.authority)
            .await?;
        if let Some(role) = req.min_role {
            valid_role(role)?;
        }
        let current = store.get_meta(path).await?.owner;
        let owner = match owner_option(req)? {
            None => current,
            Some(named) if req.authority == Authority::User => {
                if named != current {
                    return Err(Error::auth(format!(
                        "not permitted to change the owner of {path:?} in file store {} — \
                         only admin authority may",
                        store.name()
                    )));
                }
                current
            }
            Some(named) => named,
        };
        let meta = FileMeta {
            min_role: req.min_role,
            owner,
            attributes: req.attributes.clone().unwrap_or_default(),
        };
        // A delegated body may **tighten** a rule and never loosen one. The
        // caller can already reach the file, so tightening grants them nothing;
        // loosening would publish it to everyone else, which would make a
        // non-admin the author of an access rule — and delegating a trigger to
        // its caller was never meant to grant that.
        if req.authority == Authority::User {
            self.only_tightens(store, req, path, meta.min_role).await?;
        }
        store.set_meta(path, &meta).await?;
        Ok(Json::Null)
    }

    /// Refuse when something is already at `path`, in the words of the operation
    /// that must not replace it.
    async fn refuse_existing(&self, store: &dyn FileStore, path: &str) -> Result<()> {
        if store.stat(path).await?.is_some() {
            return Err(Error::invalid(format!(
                "{path:?} already exists in file store {}",
                store.name()
            )));
        }
        Ok(())
    }

    /// Refuse a read larger than what one may carry across the seam, naming both
    /// sizes — the fix is a smaller file or a different tool, and neither is
    /// discoverable from "too large".
    fn within_read_cap(&self, size: u64, path: &str, store: &str) -> Result<()> {
        if size > self.max_bytes {
            return Err(Error::invalid(format!(
                "{path:?} in file store {store} is {} MB, and a code body may read {} MB in one go \
                 (--code-max-file-bytes) — read it in pieces with read({{ offset, length }})",
                size / (1024 * 1024),
                self.max_bytes / (1024 * 1024),
            )));
        }
        Ok(())
    }
}

/// One [`FileStat`], as the guest reads it.
fn stat_json(path: &str, stat: &FileStat) -> Json {
    serde_json::json!({
        "size": stat.size,
        "isDirectory": stat.is_dir,
        "modified": stat.modified,
        // By extension, exactly as a `File` field's MIME allow-list is decided —
        // the two must agree about what a path is. A directory has none.
        "mimeType": if stat.is_dir { None } else { mime_for_path(path) },
    })
}

/// One listing [`Entry`], as the guest reads it.
fn entry_json(entry: &Entry) -> Json {
    serde_json::json!({
        "name": entry.name,
        "path": entry.path,
        "isDirectory": entry.is_dir,
        "size": entry.size,
        "modified": entry.modified,
    })
}

/// The more restrictive (numerically smaller) of two optional rules, with `None`
/// meaning "says nothing" rather than "public".
fn tightest(a: Option<u8>, b: Option<u8>) -> Option<u8> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// Whether `after` grants access to anyone `before` did not.
fn looser(after: Option<u8>, before: Option<u8>) -> bool {
    match (after, before) {
        // Nothing said at all is the loosest there is.
        (None, Some(_)) => true,
        (Some(after), Some(before)) => after > before,
        _ => false,
    }
}

/// The store-relative path an operation acts on: `/`-separated, with no empty or
/// `.` segments, and confined to the store.
///
/// The guest normalises paths too, and this is not that check repeated for
/// tidiness: what arrives here is JSON that a guest language adapter, or a body
/// that reached the seam another way, could have written by hand. A store's own
/// driver refuses traversal as well — three layers, because the consequence of
/// the one that is missing is reading `/etc/passwd`.
fn clean_path(path: &str) -> Result<String> {
    if path.contains('\0') {
        return Err(Error::invalid("a file path may not contain a null byte"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(Error::invalid(format!(
            "{path:?} is an absolute path; a file store's paths are relative to its root"
        )));
    }
    let mut parts = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(Error::invalid(format!(
                    "{path:?} leaves the file store: `..` is not a path segment here"
                )));
            }
            other => parts.push(other),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_confined_to_the_store() {
        // Normalised, not merely accepted: the quirks of a caller's joining are
        // not allowed to change which file is meant.
        assert_eq!(clean_path("a//b/./c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(clean_path("").unwrap(), "");
        assert!(
            clean_path("/")
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        for bad in ["../etc/passwd", "a/../../b", ".."] {
            let msg = clean_path(bad).unwrap_err().to_string();
            assert!(msg.contains("leaves the file store"), "{bad}: {msg}");
        }
    }

    #[test]
    fn a_rule_can_be_tightened_and_not_loosened() {
        // Smaller is more restrictive, and "nothing said" is the loosest of all.
        assert_eq!(tightest(Some(40), Some(10)), Some(10));
        assert_eq!(tightest(None, Some(10)), Some(10));
        assert_eq!(tightest(Some(40), None), Some(40));
        assert!(looser(Some(100), Some(40)), "public is looser than a floor");
        assert!(looser(None, Some(40)), "no rule is looser than a rule");
        assert!(!looser(Some(10), Some(40)), "tightening is allowed");
        assert!(!looser(Some(40), Some(40)), "unchanged is not looser");
        assert!(!looser(Some(40), None), "adding a rule is tightening");
    }

    #[test]
    fn a_refused_operation_is_a_warning_and_nothing_else_is() {
        let req: FileRequest = serde_json::from_value(serde_json::json!({
            "op": "delete", "store": "uploads", "authority": "user", "path": "p/7/a.pdf",
        }))
        .unwrap();
        let refused = Error::auth(r#"not permitted to access "p/7/a.pdf" in file store uploads"#);
        let warning =
            refusal_warning(&req, &FileCaller::new(80, Some("12".into())), &refused).unwrap();
        assert_eq!(
            warning,
            "feldspar: warning: a code body's file `delete` of \"p/7/a.pdf\" in store \
             `uploads`, as user `12` (role 80), was refused: not permitted to access \
             \"p/7/a.pdf\" in file store uploads"
        );
        let anonymous = refusal_warning(&req, &FileCaller::anonymous(), &refused).unwrap();
        assert!(anonymous.contains("as the public role"), "{anonymous}");
        // Not a refusal: nothing to warn about.
        let missing = Error::not_found("no such file");
        assert!(refusal_warning(&req, &FileCaller::anonymous(), &missing).is_none());
    }
}
