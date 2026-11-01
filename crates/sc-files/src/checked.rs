//! [`CheckedStore`]: a file store with a caller bound to it (code-quality
//! review §4.4, TODO-code-quality Q3.6).
//!
//! A [`FileStore`]'s methods take no caller; the path-cumulative rule
//! ([`check_access`], [`check_write`]) is a separate call that every route had
//! to remember to make first. A `CheckedStore` makes the check part of the
//! operation: each method decides for its caller, then does the work, so a
//! route that holds one cannot read or write a path its caller may not reach.
//!
//! A route that does several things to one path — stat it, read it, record its
//! owner — asks once: [`readable`](CheckedStore::readable) and
//! [`writable`](CheckedStore::writable) answer a grant for that one path
//! ([`Readable`], [`Writable`]), which is the proof that the check passed, and
//! whose operations reach that path and no other. The one rule that is not a
//! check of the caller's is the **drop box** (§2.1 of the F2 design): a route
//! whose own rule already let the caller deposit a file — a `File` field's
//! upload, an upload slot — may create a *new* file in a folder the caller
//! cannot otherwise write, and replacing one that is there is a write like any
//! other ([`deposit`](CheckedStore::deposit)).
//!
//! The raw store is reached from the catalog only with an `Unchecked` token
//! (`Catalog::file_store`); a caller's route goes through
//! `Catalog::checked_file_store`.

use std::sync::Arc;

use bytes::Bytes;
use sc_error::Result;
use sc_types::Role;

use crate::access::{
    FileCaller, VisibleEntry, check_access, check_write, effective_min_role, list_visible,
    record_owner,
};
use crate::serve::ServeFile;
use crate::store::{Entry, FileMeta, FileStat, FileStore};
use crate::stream::{BoxAsyncRead, BoxAsyncWrite};

/// A store, its store-wide floor and the caller every operation is decided
/// for.
#[derive(Clone)]
pub struct CheckedStore {
    store: Arc<dyn FileStore>,
    floor: Option<Role>,
    caller: FileCaller,
}

impl CheckedStore {
    /// `store` (whose definition's floor is `floor`) as `caller` reaches it.
    pub fn new(store: Arc<dyn FileStore>, floor: Option<Role>, caller: FileCaller) -> Self {
        CheckedStore {
            store,
            floor,
            caller,
        }
    }

    /// Who the operations are decided for.
    pub fn caller(&self) -> &FileCaller {
        &self.caller
    }

    /// The store-wide floor.
    pub fn floor(&self) -> Option<Role> {
        self.floor
    }

    /// The store's name.
    pub fn name(&self) -> &str {
        self.store.name()
    }

    /// Whether the store is a git working copy.
    pub fn is_git_repo(&self) -> bool {
        self.store.is_git_repo()
    }

    /// The store's own directory on disk, for a process that works on the
    /// whole tree (the IDE's language server): no path rule can reach inside a
    /// process, so only an administrator's caller, who clears every rule, is
    /// answered. `None` for a store with no local directory.
    pub fn local_root(&self) -> Result<Option<std::path::PathBuf>> {
        if !self.caller.is_admin() {
            return Err(sc_error::Error::auth(format!(
                "only an administrator may open file store {} as a directory",
                self.store.name()
            )));
        }
        self.store.local_path("")
    }

    /// Refused unless the caller may read `path`.
    pub async fn check_read(&self, path: &str) -> Result<()> {
        check_access(self.store.as_ref(), self.floor, path, &self.caller).await
    }

    /// Refused unless the caller may write (create, replace, delete, rename,
    /// re-rule) `path`.
    pub async fn check_write(&self, path: &str) -> Result<()> {
        check_write(self.store.as_ref(), self.floor, path, &self.caller).await
    }

    /// `path`, once the caller may read it.
    pub async fn readable(&self, path: &str) -> Result<Readable<'_>> {
        self.check_read(path).await?;
        Ok(Readable {
            store: self,
            path: path.to_owned(),
        })
    }

    /// `path`, once the caller may write it.
    pub async fn writable(&self, path: &str) -> Result<Writable<'_>> {
        self.check_write(path).await?;
        Ok(Writable {
            store: self,
            path: path.to_owned(),
        })
    }

    /// `path`, for a caller whose **route** already decided they may deposit a
    /// file there: the drop-box rule. Nothing at `path` yet is a new file, which
    /// needs nothing more — that is what makes a `min_role` 1 folder a private
    /// drop box; something there already is replaced, which is a write to it
    /// and is checked as one (somebody else's upload is refused).
    ///
    /// Only for a route with a rule of its own that admits the caller (a
    /// `File` field's table write rule, an upload slot's issuer), which is the
    /// caller's to name in the system-authority inventory.
    pub async fn deposit(&self, path: &str) -> Result<Writable<'_>> {
        if self.store.stat(path).await?.is_some() {
            self.check_write(path).await?;
        }
        Ok(Writable {
            store: self,
            path: path.to_owned(),
        })
    }

    /// The first free spelling of `path` (`name_1.ext`, …): an existence probe
    /// a [`deposit`](CheckedStore::deposit) that keeps what is there needs.
    /// It answers a name, never what is stored under one.
    pub async fn free_path(&self, path: &str) -> Result<String> {
        crate::reference::free_path(self.store.as_ref(), path).await
    }

    /// The rule that reaches `path`: the store's floor tightened by every
    /// directory above it and by the entry's own. Metadata, not content, and
    /// what a caller deciding whether a change only tightens it compares.
    pub async fn effective_min_role(&self, path: &str) -> Result<Option<Role>> {
        effective_min_role(self.store.as_ref(), self.floor, path).await
    }

    /// The bytes of `path`.
    pub async fn read(&self, path: &str) -> Result<Bytes> {
        self.readable(path).await?.read().await
    }

    /// A stream over `path` (or a byte range of it).
    pub async fn open_read(
        &self,
        path: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<BoxAsyncRead> {
        self.readable(path).await?.open_read(range).await
    }

    /// What is at `path`, if anything.
    pub async fn stat(&self, path: &str) -> Result<Option<FileStat>> {
        self.readable(path).await?.stat().await
    }

    /// The metadata of `path`.
    pub async fn get_meta(&self, path: &str) -> Result<FileMeta> {
        self.readable(path).await?.get_meta().await
    }

    /// `path` served by the transport's file builder (streamed, ranged), once
    /// the caller may read it.
    pub async fn serve(&self, path: &str) -> Result<ServeFile> {
        Ok(self.readable(path).await?.serve())
    }

    /// The entries of `dir` the caller may see (the rule filters a listing
    /// rather than refusing it; see [`list_visible`]).
    pub async fn list(&self, dir: &str) -> Result<Vec<VisibleEntry>> {
        list_visible(self.store.as_ref(), self.floor, dir, &self.caller).await
    }

    /// [`list`](CheckedStore::list) as bare entries — for an administrator,
    /// the store's own listing, without reading every entry's metadata to
    /// reach the foregone conclusion that they may see it.
    pub async fn list_entries(&self, dir: &str) -> Result<Vec<Entry>> {
        if self.caller.is_admin() {
            return self.store.list(dir).await;
        }
        Ok(self.list(dir).await?.into_iter().map(|v| v.entry).collect())
    }

    /// The files under `dir` whose names match `query` that the caller may
    /// read ([`find_files`](crate::find_files), which skips what a listing
    /// would hide), each with its metadata and the rule that reaches it; and
    /// whether the walk stopped at `max_results`.
    pub async fn find_files(
        &self,
        dir: &str,
        query: &str,
        max_results: usize,
    ) -> Result<(Vec<VisibleEntry>, bool)> {
        let found = crate::search::find_files(
            self.store.as_ref(),
            self.floor,
            &self.caller,
            dir,
            query,
            max_results,
        )
        .await?;
        let mut out = Vec::with_capacity(found.entries.len());
        for entry in found.entries {
            let meta = self.store.get_meta(&entry.path).await.unwrap_or_default();
            let effective_min_role = self.effective_min_role(&entry.path).await?;
            out.push(VisibleEntry {
                entry,
                meta,
                effective_min_role,
            });
        }
        Ok((out, found.truncated))
    }

    /// The files under `dir` with one of `extensions` that the caller may read
    /// ([`find_by_extension`](crate::find_by_extension)).
    pub async fn find_by_extension(
        &self,
        dir: &str,
        extensions: &[String],
    ) -> Result<crate::search::FoundFiles> {
        crate::search::find_by_extension(
            self.store.as_ref(),
            self.floor,
            &self.caller,
            dir,
            extensions,
        )
        .await
    }

    /// The lines matching `query` in the files the caller may read
    /// ([`search_store`](crate::search_store)).
    pub async fn search(
        &self,
        query: &crate::search::SearchQuery,
    ) -> Result<crate::search::SearchOutcome> {
        crate::search::search_store(self.store.as_ref(), self.floor, &self.caller, query).await
    }

    /// Write `data` to `path`, creating or replacing it.
    pub async fn write(&self, path: &str, data: Bytes) -> Result<()> {
        self.writable(path).await?.write(data).await
    }

    /// Make the directory `path`.
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.writable(path).await?.mkdir().await
    }

    /// Delete `path`, answering whether there was anything there.
    pub async fn delete(&self, path: &str) -> Result<bool> {
        self.writable(path).await?.delete().await
    }

    /// Move `from` to `to`: both ends are writes.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from = self.writable(from).await?;
        let to = self.writable(to).await?;
        from.rename_to(&to).await
    }

    /// Replace the metadata of `path` (its rule, its owner).
    pub async fn set_meta(&self, path: &str, meta: &FileMeta) -> Result<()> {
        self.writable(path).await?.set_meta(meta).await
    }
}

/// A path of a [`CheckedStore`] its caller may read: the proof that
/// [`check_access`] passed, and the operations that reach that path only.
pub struct Readable<'s> {
    store: &'s CheckedStore,
    path: String,
}

impl Readable<'_> {
    /// The path this grant is for.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The store's name.
    pub fn store_name(&self) -> &str {
        self.store.name()
    }

    /// The bytes.
    pub async fn read(&self) -> Result<Bytes> {
        self.store.store.read(&self.path).await
    }

    /// A stream over the file (or a byte range of it).
    pub async fn open_read(&self, range: Option<std::ops::Range<u64>>) -> Result<BoxAsyncRead> {
        self.store.store.open_read(&self.path, range).await
    }

    /// What is there, if anything.
    pub async fn stat(&self) -> Result<Option<FileStat>> {
        self.store.store.stat(&self.path).await
    }

    /// The metadata.
    pub async fn get_meta(&self) -> Result<FileMeta> {
        self.store.store.get_meta(&self.path).await
    }

    /// The file, for the transport's file builder.
    pub fn serve(self) -> ServeFile {
        ServeFile::new(Arc::clone(&self.store.store), self.path)
    }
}

/// A path of a [`CheckedStore`] its caller may write — create, replace,
/// delete, move, re-rule: the proof that [`check_write`] passed (or that the
/// drop-box rule let a new file in), and the operations that reach that path
/// only.
pub struct Writable<'s> {
    store: &'s CheckedStore,
    path: String,
}

impl Writable<'_> {
    /// The path this grant is for.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The store's name.
    pub fn store_name(&self) -> &str {
        self.store.name()
    }

    /// What is there, if anything.
    pub async fn stat(&self) -> Result<Option<FileStat>> {
        self.store.store.stat(&self.path).await
    }

    /// The metadata.
    pub async fn get_meta(&self) -> Result<FileMeta> {
        self.store.store.get_meta(&self.path).await
    }

    /// Write `data`, creating or replacing the file.
    pub async fn write(&self, data: Bytes) -> Result<()> {
        self.store.store.write(&self.path, data).await
    }

    /// A writer appending to the file, creating it (and its parents) when it
    /// is not there.
    pub async fn open_append(&self) -> Result<BoxAsyncWrite<'_>> {
        self.store.store.open_append(&self.path).await
    }

    /// Join `parts` — each a path its caller may read, of this store — in
    /// order, into this file, replacing it; answers the size written.
    pub async fn concat(&self, parts: &[Readable<'_>]) -> Result<u64> {
        let paths: Vec<String> = parts.iter().map(|p| p.path.clone()).collect();
        self.store.store.concat(&paths, &self.path).await
    }

    /// Make the directory.
    pub async fn mkdir(&self) -> Result<()> {
        self.store.store.mkdir(&self.path).await
    }

    /// Delete it, answering whether there was anything there.
    pub async fn delete(&self) -> Result<bool> {
        self.store.store.delete(&self.path).await
    }

    /// Move it to `to`, a path of the same store (whoever's grant it is: the
    /// server's temporary file moved to where a caller deposits it is one
    /// store bound twice). A path of another store is refused — there is no
    /// rename across two.
    pub async fn rename_to(&self, to: &Writable<'_>) -> Result<()> {
        if !Arc::ptr_eq(&self.store.store, &to.store.store) {
            return Err(sc_error::Error::invalid(format!(
                "{:?} in file store {} cannot be renamed into file store {}",
                self.path,
                self.store.name(),
                to.store.name()
            )));
        }
        self.store.store.rename(&self.path, &to.path).await
    }

    /// Replace the metadata (its rule, its owner).
    pub async fn set_meta(&self, meta: &FileMeta) -> Result<()> {
        self.store.store.set_meta(&self.path, meta).await
    }

    /// Record `owner` as the owner, unless one is recorded already
    /// ([`record_owner`]): what a write by somebody records for its creator.
    pub async fn record_owner(&self, owner: &str) {
        record_owner(self.store.store.as_ref(), &self.path, owner).await;
    }

    /// Record the caller as the owner, unless one is recorded already — the
    /// uploader owns their upload (§2.1). Nothing for a caller with no user.
    pub async fn claim(&self) {
        if let Some(owner) = &self.store.caller.user_id {
            record_owner(self.store.store.as_ref(), &self.path, owner).await;
        }
    }
}
