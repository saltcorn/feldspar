//! Local-directory [`FileStore`] driver.
//!
//! Backs a named store with a directory on the local filesystem. All I/O is
//! confined to the store root: paths are sanitised so absolute paths and `..`
//! traversal are rejected before touching disk.
//!
//! ## Metadata
//!
//! Per-file [`FileMeta`] is stored in a single extended attribute
//! ([`META_ATTR`]) on the file itself, via the cross-platform [`crate::xattr`]
//! layer (POSIX xattrs on Unix, NTFS Alternate Data Streams on Windows). Files
//! therefore need no database row and no sidecar (design §9).
//!
//! ## Suggesting a directory
//!
//! The backend's one [`Operation`] ([`local_operations`]) answers the question
//! an admin creating their first store cannot: *where should this live?* The
//! directory stays theirs to choose — that is why it is a required setting — but
//! a suggestion under the directory Saltcorn already owns ([`suggest_local_dir`])
//! is an answer where they would otherwise be guessing at what a service account
//! may write to.

use async_trait::async_trait;
use bytes::Bytes;
use sc_error::{Context, Error, Result};
use sc_types::{Attrs, Operation, OperationScope};
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::backend::OperationOutcome;
use crate::def::{CFG_CREATE, CFG_PATH, FileStoreDef};
use crate::paths::suggest_local_dir;
use crate::store::{Entry, FileMeta, FileStat, FileStore};
use crate::stream::{BoxAsyncRead, BoxAsyncWrite, check_range};

/// A modification time as RFC 3339 in UTC, which is how every other timestamp
/// crosses this server's wire.
fn rfc3339(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Whether a store-relative path is inside (or is) a `.git` directory, at any
/// depth: a segment that is `.git` in any case, since a case-insensitive file
/// system opens `.GIT/config` as the same file, and with trailing dots or spaces
/// ignored, since Windows drops them. `git~1` (and `git~<digits>`) counts too:
/// it is the 8.3 short name NTFS gives `.git`, which git's own
/// `core.protectNTFS` refuses for the same reason (security review #31).
pub fn in_git_dir(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| {
        let segment = segment.trim_end_matches(['.', ' ']);
        segment.eq_ignore_ascii_case(".git") || is_git_short_name(segment)
    })
}

/// `git~1`, in any case, with any number after the tilde.
fn is_git_short_name(segment: &str) -> bool {
    match (segment.get(..4), segment.get(4..)) {
        (Some(head), Some(tail)) => {
            head.eq_ignore_ascii_case("git~")
                && !tail.is_empty()
                && tail.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    }
}

/// Name of the extended attribute holding a file's serialised [`FileMeta`]. On
/// Unix the effective key is `user.saltcorn.meta` (the `user.` namespace is
/// applied by the xattr layer).
const META_ATTR: &str = "saltcorn.meta";

/// A [`FileStore`] backed by a directory on the local filesystem.
#[derive(Debug, Clone)]
pub struct LocalFileStore {
    name: String,
    root: PathBuf,
    /// Whether a write may land inside a `.git` directory
    /// ([`writing_git_dir`](Self::writing_git_dir)). Off for every store a
    /// caller reaches.
    git_dir_writable: bool,
}

impl LocalFileStore {
    /// Connect a local store named `name` rooted at `root`.
    ///
    /// The root must be an existing directory; the path is canonicalised so that
    /// later traversal checks compare against a fully-resolved root.
    pub fn new(name: impl Into<String>, root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root)
            .with_context(|| format!("file store root {}", root.display()))?;
        if !canonical.is_dir() {
            return Err(Error::config(format!(
                "file store root {} is not a directory",
                canonical.display()
            )));
        }
        Ok(Self {
            name: name.into(),
            root: canonical,
            git_dir_writable: false,
        })
    }

    /// The same store, allowed to write inside `.git`: only for restoring a
    /// backup's own checkout, an admin's act whose archive *is* the repository.
    pub fn writing_git_dir(mut self) -> Self {
        self.git_dir_writable = true;
        self
    }

    /// [`resolve`](Self::resolve) for a path about to be written, created,
    /// renamed or deleted: refused inside a `.git` directory ([`in_git_dir`]).
    ///
    /// A repository's `.git/config` and `.git/hooks` are commands the server's
    /// own `git` runs — `core.fsmonitor` on every `git status` — so whoever may
    /// write a file there may run a program as the server's operating-system
    /// user. Nobody who reaches a store through Saltcorn (an agent's edit, an
    /// upload, a File field) has a reason to.
    fn resolve_for_write(&self, rel: &str) -> Result<PathBuf> {
        if !self.git_dir_writable && in_git_dir(rel) {
            return Err(Error::invalid(format!(
                "path {rel:?} is inside a `.git` directory, which file store {} does not write",
                self.name
            )));
        }
        self.resolve(rel)
    }

    /// Resolve a store-relative path to an absolute path inside the root,
    /// rejecting anything that would escape it — by its spelling
    /// ([`lexical`](Self::lexical)) or through a symbolic link on disk
    /// ([`beneath_root`](Self::beneath_root)).
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let abs = self.lexical(rel)?;
        self.beneath_root(&abs, rel, true)?;
        Ok(abs)
    }

    /// [`resolve`](Self::resolve) for an entry acted on **as itself** — deleted
    /// or moved — so a link is the link and not what it points at: only its
    /// parent has to lie inside the root. A link out of the store can still be
    /// removed, which is the one thing worth doing with it.
    fn resolve_entry(&self, rel: &str) -> Result<PathBuf> {
        if !self.git_dir_writable && in_git_dir(rel) {
            return Err(Error::invalid(format!(
                "path {rel:?} is inside a `.git` directory, which file store {} does not write",
                self.name
            )));
        }
        let abs = self.lexical(rel)?;
        self.beneath_root(&abs, rel, false)?;
        Ok(abs)
    }

    /// The path `rel` names under the root, by its spelling alone.
    ///
    /// Split on `/` only, and every segment must be one plain name: a `\`, a
    /// `:` or a NUL is refused rather than handed to [`PathBuf::push`], which on
    /// Windows reads `..\..\x` as two steps up and `C:\x` as a whole new path —
    /// and either would also slip past the folder floors `check_access` walks,
    /// which are split on `/`. So is a segment of nothing but dots and spaces,
    /// which Windows trims to `..` or to nothing.
    fn lexical(&self, rel: &str) -> Result<PathBuf> {
        let escapes = || Error::invalid(format!("path {rel:?} escapes the file store root"));
        let mut out = self.root.clone();
        for segment in rel.split('/') {
            match segment {
                // Empty (leading/trailing/double slash) and "." are no-ops.
                "" | "." => continue,
                ".." => return Err(escapes()),
                seg if seg.contains(['\\', ':', '\0'])
                    || seg.trim_end_matches(['.', ' ']).is_empty() =>
                {
                    return Err(escapes());
                }
                seg => {
                    let mut components = Path::new(seg).components();
                    match (components.next(), components.next()) {
                        (Some(std::path::Component::Normal(_)), None) => out.push(seg),
                        _ => return Err(escapes()),
                    }
                }
            }
        }
        Ok(out)
    }

    /// Refuse `abs` if what is on disk takes it out of the root: a symbolic
    /// link (or a directory replaced by one) anywhere on the path, pointing
    /// outside — `uploads -> /etc`, committed upstream and arriving with a pull.
    ///
    /// The deepest part of the path that exists is canonicalised and must still
    /// be under the root; what does not exist yet cannot be a link. A link that
    /// leads nowhere is refused too, since writing through it would create its
    /// target wherever it points. With `follow_last` off the entry itself is not
    /// followed, only its parent. Links that stay inside the store are fine.
    ///
    /// A check before the open rather than `openat2(RESOLVE_BENEATH)`, so it is
    /// the same on every platform; it stops links that are there when a caller
    /// asks, which is the case a checkout produces.
    fn beneath_root(&self, abs: &Path, rel: &str, follow_last: bool) -> Result<()> {
        let escapes = || Error::invalid(format!("path {rel:?} escapes the file store root"));
        let mut probe = if follow_last || abs == self.root {
            abs
        } else {
            abs.parent().unwrap_or(&self.root)
        };
        loop {
            match std::fs::symlink_metadata(probe) {
                Ok(_) => {
                    return match std::fs::canonicalize(probe) {
                        Ok(real) if real.starts_with(&self.root) => {
                            self.lands_outside_git(&real, rel)
                        }
                        Ok(_) => Err(escapes()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(escapes()),
                        Err(e) => Err(Error::from(e))
                            .with_context(|| format!("resolving {rel:?} in store {}", self.name)),
                    };
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    if probe == self.root {
                        return Ok(());
                    }
                    match probe.parent() {
                        Some(parent) if parent.starts_with(&self.root) => probe = parent,
                        _ => return Ok(()),
                    }
                }
                Err(e) => {
                    return Err(Error::from(e))
                        .with_context(|| format!("resolving {rel:?} in store {}", self.name));
                }
            }
        }
    }

    /// Refuse a path that is not spelled inside `.git` but **lands** there on
    /// disk — through a symbolic link in the working tree pointing into `.git`,
    /// which can arrive with a pull (security review #31). The `.git` rule is
    /// otherwise only as good as the spelling it reads: `link/config` with
    /// `link -> .git` is `.git/config`. `real` is the canonical path of the
    /// deepest part of the path that exists; what does not exist yet cannot be
    /// a link, and its spelling is already in `rel`. A path spelled inside
    /// `.git` is left to the rules that read its spelling (refused for writes,
    /// decided by `check_access` for reads).
    fn lands_outside_git(&self, real: &Path, rel: &str) -> Result<()> {
        if self.git_dir_writable || in_git_dir(rel) {
            return Ok(());
        }
        let landed = real.strip_prefix(&self.root).is_ok_and(|inside| {
            inside.components().any(|c| match c {
                std::path::Component::Normal(name) => in_git_dir(&name.to_string_lossy()),
                _ => false,
            })
        });
        if landed {
            return Err(Error::invalid(format!(
                "path {rel:?} leads inside a `.git` directory, which file store {} does not reach",
                self.name
            )));
        }
        Ok(())
    }

    /// Turn a filesystem error into a domain error, keeping **"it is not there"**
    /// distinct from "it broke".
    ///
    /// This is the difference between a 404 and a 500 at the API (§16), and it is
    /// not cosmetic: asking whether a path exists by trying to read it is the
    /// normal thing for a client to do — the IDE's filesystem (§12.1) stats a
    /// path before writing it, and looks for optional files like
    /// `.vscode/settings.json` that are usually absent. A missing file reported as
    /// a system failure makes every one of those an error the admin is shown.
    fn io<T>(&self, result: std::io::Result<T>, path: &str, doing: &str) -> Result<T> {
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::not_found(format!(
                "{path:?} does not exist in store {}",
                self.name
            ))),
            other => other.with_context(|| format!("{doing} {path:?} in store {}", self.name)),
        }
    }
}

#[async_trait]
impl FileStore for LocalFileStore {
    fn name(&self) -> &str {
        &self.name
    }

    async fn read(&self, path: &str) -> Result<Bytes> {
        let abs = self.resolve(path)?;
        let data = self.io(tokio::fs::read(&abs).await, path, "reading")?;
        Ok(Bytes::from(data))
    }

    async fn write(&self, path: &str, data: Bytes) -> Result<()> {
        let abs = self.resolve_for_write(path)?;
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating parent dirs for {path:?}"))?;
        }
        tokio::fs::write(&abs, &data)
            .await
            .with_context(|| format!("writing {path:?} to store {}", self.name))?;
        Ok(())
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let abs = self.resolve(dir)?;
        let mut read_dir = self.io(tokio::fs::read_dir(&abs).await, dir, "listing")?;
        // Normalise the directory prefix so child paths are `<dir>/<name>` with
        // no leading, trailing, or doubled separators.
        let prefix = dir.trim_matches('/');
        let mut entries = Vec::new();
        while let Some(dirent) = read_dir
            .next_entry()
            .await
            .with_context(|| format!("reading dir entry in {dir:?}"))?
        {
            let name = dirent.file_name().to_string_lossy().into_owned();
            let meta = dirent
                .metadata()
                .await
                .with_context(|| format!("stat {name:?} in {dir:?}"))?;
            let is_dir = meta.is_dir();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            entries.push(Entry {
                name,
                path,
                is_dir,
                size: if is_dir { None } else { Some(meta.len()) },
                // Free here: the listing already stats every child for its size
                // and type, and the same `Metadata` carries the mtime.
                modified: meta.modified().ok().map(rfc3339),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let abs = self.resolve_for_write(path)?;
        // A file already sitting there is a real conflict — `create_dir_all`
        // would report an opaque OS error, so name it.
        if abs.is_file() {
            return Err(Error::invalid(format!(
                "cannot create directory {path:?} in store {}: a file already exists there",
                self.name
            )));
        }
        // Idempotent by construction: `create_dir_all` succeeds when the
        // directory is already there.
        tokio::fs::create_dir_all(&abs)
            .await
            .with_context(|| format!("creating directory {path:?} in store {}", self.name))?;
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<bool> {
        let abs = self.resolve_entry(path)?;
        // `resolve` collapses "", "/" and "." to the root, so this also catches
        // an attempt to delete the whole store by passing an empty path.
        if abs == self.root {
            return Err(Error::invalid(format!(
                "refusing to delete the root of file store {}",
                self.name
            )));
        }
        let meta = match tokio::fs::symlink_metadata(&abs).await {
            Ok(meta) => meta,
            // Already gone: report it rather than erroring, so a caller need not
            // race an existence check against the delete.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(Error::from(e))
                    .with_context(|| format!("stat {path:?} in store {}", self.name));
            }
        };
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&abs)
                .await
                .with_context(|| format!("deleting directory {path:?} in store {}", self.name))?;
        } else {
            tokio::fs::remove_file(&abs)
                .await
                .with_context(|| format!("deleting {path:?} in store {}", self.name))?;
        }
        Ok(true)
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let src = self.resolve_entry(from)?;
        let dst = self.resolve_for_write(to)?;
        if src == self.root || dst == self.root {
            return Err(Error::invalid(format!(
                "refusing to rename the root of file store {}",
                self.name
            )));
        }
        if !tokio::fs::try_exists(&src)
            .await
            .with_context(|| format!("checking {from:?} exists"))?
        {
            return Err(Error::not_found(format!(
                "{from:?} does not exist in store {}",
                self.name
            )));
        }
        // Refuse to clobber. `tokio::fs::rename` would silently replace the
        // destination, destroying data the caller never named — and on a file
        // manager's drag-and-drop that is a lost file with no undo.
        if tokio::fs::try_exists(&dst)
            .await
            .with_context(|| format!("checking {to:?} exists"))?
        {
            return Err(Error::invalid(format!(
                "cannot rename {from:?} to {to:?} in store {}: the destination already exists",
                self.name
            )));
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating parent dirs for {to:?}"))?;
        }
        tokio::fs::rename(&src, &dst)
            .await
            .with_context(|| format!("renaming {from:?} to {to:?} in store {}", self.name))?;
        Ok(())
    }

    fn is_git_repo(&self) -> bool {
        // A `.git` directory (normal repo) or file (worktree/submodule link).
        self.root.join(".git").exists()
    }

    fn local_path(&self, rel: &str) -> Result<Option<PathBuf>> {
        // A local store is all local path; `resolve` applies the same traversal
        // sandboxing the byte-level methods get.
        self.resolve(rel).map(Some)
    }

    /// The entry's own facts, from one `stat` call rather than the default
    /// implementation's listing of its parent — which on a directory of ten
    /// thousand files is ten thousand entries built to answer one question.
    ///
    /// Follows symlinks, as [`read`](FileStore::read) does: what a caller asks
    /// about is the file they would read.
    async fn stat(&self, path: &str) -> Result<Option<FileStat>> {
        let abs = self.resolve(path)?;
        let meta = match tokio::fs::metadata(&abs).await {
            Ok(meta) => meta,
            // Nothing there is an answer, not a failure — and so is a path whose
            // parent is a file, which the OS reports as `NotADirectory`.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::NotADirectory =>
            {
                return Ok(None);
            }
            Err(e) => {
                return Err(Error::from(e))
                    .with_context(|| format!("stat {path:?} in store {}", self.name));
            }
        };
        let is_dir = meta.is_dir();
        Ok(Some(FileStat {
            size: if is_dir { 0 } else { meta.len() },
            is_dir,
            modified: meta.modified().ok().map(rfc3339),
        }))
    }

    /// The open file itself, seeked and limited to the range: nothing is read
    /// until the stream is.
    async fn open_read(&self, path: &str, range: Option<Range<u64>>) -> Result<BoxAsyncRead> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let abs = self.resolve(path)?;
        let mut file = self.io(tokio::fs::File::open(&abs).await, path, "opening")?;
        let size = self.io(file.metadata().await, path, "stat")?;
        if size.is_dir() {
            return Err(Error::invalid(format!(
                "{path:?} in store {} is a directory, not a file to read",
                self.name
            )));
        }
        let Some(range) = range else {
            return Ok(Box::new(file));
        };
        check_range(&range, size.len(), path)?;
        self.io(
            file.seek(std::io::SeekFrom::Start(range.start)).await,
            path,
            "seeking in",
        )?;
        Ok(Box::new(file.take(range.end - range.start)))
    }

    /// The file opened for appending, created (with its parents) if missing.
    async fn open_append<'a>(&'a self, path: &str) -> Result<BoxAsyncWrite<'a>> {
        let abs = self.resolve_for_write(path)?;
        if abs == self.root {
            return Err(Error::invalid(format!(
                "the root of file store {} is not a file to append to",
                self.name
            )));
        }
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating parent dirs for {path:?}"))?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)
            .await
            .with_context(|| format!("opening {path:?} to append in store {}", self.name))?;
        Ok(Box::new(file))
    }

    /// Streamed into a temporary file beside the target and renamed over it,
    /// so the target is the whole join or untouched: every part is checked
    /// before a byte is copied, and a failure part-way removes the temporary.
    async fn concat(&self, parts: &[String], to: &str) -> Result<u64> {
        let target = self.resolve_for_write(to)?;
        if target == self.root {
            return Err(Error::invalid(format!(
                "the root of file store {} is not a file to join into",
                self.name
            )));
        }
        let mut sources = Vec::with_capacity(parts.len());
        for part in parts {
            let abs = self.resolve(part)?;
            let meta = self.io(tokio::fs::metadata(&abs).await, part, "stat")?;
            if meta.is_dir() {
                return Err(Error::invalid(format!(
                    "{part:?} in store {} is a directory, not a part to join",
                    self.name
                )));
            }
            sources.push((part, abs));
        }
        let parent = target.parent().unwrap_or(&self.root).to_path_buf();
        tokio::fs::create_dir_all(&parent)
            .await
            .with_context(|| format!("creating parent dirs for {to:?}"))?;
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temp = parent.join(format!(".{name}.{}.joining", uuid::Uuid::new_v4().simple()));
        let joined = async {
            let mut out = tokio::fs::File::create(&temp)
                .await
                .with_context(|| format!("creating the join for {to:?}"))?;
            let mut total = 0u64;
            for (part, abs) in &sources {
                let mut input = self.io(tokio::fs::File::open(abs).await, part, "opening")?;
                total += tokio::io::copy(&mut input, &mut out)
                    .await
                    .with_context(|| format!("joining {part:?} into {to:?}"))?;
            }
            tokio::io::AsyncWriteExt::flush(&mut out)
                .await
                .with_context(|| format!("writing the join for {to:?}"))?;
            Ok::<u64, Error>(total)
        }
        .await;
        let total = match joined {
            Ok(total) => total,
            Err(e) => {
                tokio::fs::remove_file(&temp).await.ok();
                return Err(e);
            }
        };
        // A replaced target keeps its metadata — its owner above all — which
        // the rename would otherwise drop with the old inode's attributes.
        let kept = match tokio::fs::try_exists(&target).await {
            Ok(true) => Some(self.get_meta(to).await.unwrap_or_default()),
            _ => None,
        };
        if let Err(e) = tokio::fs::rename(&temp, &target).await {
            tokio::fs::remove_file(&temp).await.ok();
            return Err(Error::from(e)).with_context(|| format!("placing the join at {to:?}"));
        }
        if let Some(meta) = kept.filter(|m| *m != FileMeta::default()) {
            self.set_meta(to, &meta).await?;
        }
        Ok(total)
    }

    async fn get_meta(&self, path: &str) -> Result<FileMeta> {
        let abs = self.resolve(path)?;
        match crate::xattr::get(&abs, META_ATTR).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing metadata for {path:?}")),
            // The attribute has never been set: default metadata.
            None => Ok(FileMeta::default()),
        }
    }

    async fn set_meta(&self, path: &str, meta: &FileMeta) -> Result<()> {
        // The target file must exist before it can carry metadata.
        let abs = self.resolve_for_write(path)?;
        if !tokio::fs::try_exists(&abs)
            .await
            .with_context(|| format!("checking {path:?} exists"))?
        {
            return Err(Error::not_found(format!(
                "cannot set metadata: {path:?} does not exist in store {}",
                self.name
            )));
        }
        let json = serde_json::to_vec(meta)
            .with_context(|| format!("serialising metadata for {path:?}"))?;
        crate::xattr::set(&abs, META_ATTR, json).await?;
        Ok(())
    }
}

/// The [`local`](crate::LOCAL_BACKEND) backend's one operation: suggest a
/// directory for a store that has none yet.
///
/// Named as a constant for the reason every other operation name is: it is what
/// the admin UI posts back, so it is part of the wire, not an incidental string.
pub const OP_SUGGEST_DIR: &str = "suggest_dir";

/// What the [`local`](crate::LOCAL_BACKEND) backend can do beyond holding
/// settings (§6.2's [`Operation`] vocabulary) — one button, rendered by the same
/// admin-UI code that renders the git backend's, which knows what neither means.
///
/// The directory is the admin's decision and the form is right to ask for it: a
/// local store usually points at somewhere that already exists, often with files
/// already in it. But an admin creating their **first** store has no basis for
/// that decision and no way to learn where a server that runs as its own account
/// may even write — so the answer is offered rather than assumed. It is a
/// [`Configure`](OperationScope::Configure) operation because it fills in the
/// settings the admin is still editing, exactly as the deploy-key generator
/// does, and like every suggestion it can be typed over before saving.
pub fn local_operations() -> Vec<Operation> {
    vec![
        Operation::new(OP_SUGGEST_DIR, OperationScope::Configure)
            .label("Suggest a directory")
            .description(
                "Fills in a directory named after this store, inside the data directory \
                 Saltcorn owns.",
            ),
    ]
}

/// Run [`local_operations`]'s one operation against `def`, which is the unsaved
/// definition the admin is still editing.
///
/// Called through
/// [`run_backend_operation`](crate::run_backend_operation), which has already
/// checked that the operation exists and that its arguments match what it
/// declared.
///
/// Two settings are written, not one: the suggested directory **and** `create`.
/// The directory Saltcorn is proposing does not exist yet — that is the whole
/// point of proposing it — so a suggestion that left `create` off would produce a
/// store that saves and then refuses to connect, which is precisely the
/// confusion the button is there to spare the admin.
pub(crate) fn run_local_operation(
    def: &mut FileStoreDef,
    operation: &str,
    _input: &Attrs,
) -> Result<OperationOutcome> {
    match operation {
        OP_SUGGEST_DIR => {
            // The suggestion is named after the store, so an unnamed store has
            // nothing to suggest. Said plainly rather than suggesting a
            // directory called `store`, which every unnamed store would share.
            if def.name.trim().is_empty() {
                return Err(Error::invalid(
                    "name the store first — the suggested directory is named after it",
                ));
            }
            let dir = suggest_local_dir(def.name.trim())?;
            let shown = dir.to_string_lossy().into_owned();
            def.config.insert(
                CFG_PATH.to_owned(),
                serde_json::Value::String(shown.clone()),
            );
            def.config
                .insert(CFG_CREATE.to_owned(), serde_json::Value::Bool(true));
            Ok(OperationOutcome::text(format!(
                "Suggested {shown}. It is created when the store is saved; \
                 replace it with a directory of your own if you have one."
            )))
        }
        other => Err(Error::invalid(format!(
            "unknown local file store operation `{other}`"
        ))),
    }
}

#[cfg(test)]
mod operation_tests {
    use super::*;
    use crate::def::LOCAL_BACKEND;
    use crate::paths::DATA_DIR_ENV;
    use crate::paths::testing::temp_env;

    /// The suggestion fills in both settings, and the directory it names is the
    /// one under the data directory rather than anything the admin must know.
    #[test]
    fn suggesting_a_directory_fills_in_the_settings() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let mut def = FileStoreDef::new("Customer uploads", LOCAL_BACKEND);
            let outcome = run_local_operation(&mut def, OP_SUGGEST_DIR, &Attrs::new()).unwrap();

            assert_eq!(
                def.setting(CFG_PATH),
                Some("/tmp/sc-data/local-stores/Customer_uploads")
            );
            // Without this the suggested store would save and then fail to
            // connect, since nothing has created the directory.
            assert_eq!(
                def.config.get(CFG_CREATE),
                Some(&serde_json::Value::Bool(true))
            );
            assert!(
                outcome.output.contains("/tmp/sc-data/local-stores"),
                "{}",
                outcome.output
            );
        });
    }

    /// An unnamed store gets an explanation rather than a directory every
    /// unnamed store would share.
    #[test]
    fn a_store_with_no_name_has_nothing_to_suggest() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let mut def = FileStoreDef::new("  ", LOCAL_BACKEND);
            let err = run_local_operation(&mut def, OP_SUGGEST_DIR, &Attrs::new())
                .unwrap_err()
                .to_string();
            assert!(err.contains("name the store first"), "{err}");
            assert_eq!(def.setting(CFG_PATH), None);
        });
    }
}

#[cfg(all(test, unix))]
mod git_dir_tests {
    use super::*;

    /// Security review #31: the `.git` rule is applied to where a path lands,
    /// not only to how it is spelled. A symbolic link in the working tree that
    /// points into `.git` — which can arrive with a pull — does not make
    /// `.git/config` writable or readable again; and `GIT~1`, NTFS's short name
    /// for `.git`, is refused like `.git` is.
    #[tokio::test]
    async fn a_link_into_git_does_not_reach_it() {
        let dir = std::env::temp_dir().join(format!("sc-git-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
        std::os::unix::fs::symlink(dir.join(".git"), dir.join("innocent")).unwrap();
        std::os::unix::fs::symlink(dir.join(".git/config"), dir.join("settings")).unwrap();
        let store = LocalFileStore::new("apps", &dir).unwrap();

        for path in ["innocent/config", "innocent/hooks/pre-commit", "settings"] {
            let err = store
                .write(path, Bytes::from_static(b"[core]\nfsmonitor = evil\n"))
                .await
                .expect_err(path)
                .to_string();
            assert!(err.contains("`.git`"), "{path}: {err}");
        }
        for path in ["innocent/config", "settings"] {
            let err = store.read(path).await.expect_err(path).to_string();
            assert!(err.contains("`.git`"), "{path}: {err}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join(".git/config")).unwrap(),
            "[core]\n"
        );

        // The 8.3 short name is `.git` too.
        assert!(in_git_dir("GIT~1/config"));
        assert!(in_git_dir("sub/git~12/hooks/x"));
        assert!(!in_git_dir("git~/x"));
        assert!(!in_git_dir("digit~1/x"));
        assert!(!in_git_dir("é~1/x"));
        // A plain file still reads and writes.
        store
            .write("notes.txt", Bytes::from_static(b"hi"))
            .await
            .unwrap();
        assert_eq!(&store.read("notes.txt").await.unwrap()[..], b"hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
