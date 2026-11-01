//! [`CheckedStore`]: a file store with a caller bound to it (code-quality
//! review §4.4, TODO-code-quality Q3.6).
//!
//! A [`FileStore`]'s methods take no caller; the path-cumulative rule
//! ([`check_access`], [`check_write`]) is a separate call that every route had
//! to remember to make first. A `CheckedStore` makes the check part of the
//! operation: each method decides for its caller, then does the work, so a
//! route that holds one cannot read or write a path its caller may not reach.

use std::sync::Arc;

use bytes::Bytes;
use sc_error::Result;
use sc_types::Role;

use crate::access::{FileCaller, VisibleEntry, check_access, check_write, list_visible};
use crate::store::{FileMeta, FileStat, FileStore};
use crate::stream::BoxAsyncRead;

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

    /// Refused unless the caller may read `path`.
    pub async fn check_read(&self, path: &str) -> Result<()> {
        check_access(self.store.as_ref(), self.floor, path, &self.caller).await
    }

    /// Refused unless the caller may write (create, replace, delete, rename,
    /// re-rule) `path`.
    pub async fn check_write(&self, path: &str) -> Result<()> {
        check_write(self.store.as_ref(), self.floor, path, &self.caller).await
    }

    /// The bytes of `path`.
    pub async fn read(&self, path: &str) -> Result<Bytes> {
        self.check_read(path).await?;
        self.store.read(path).await
    }

    /// A stream over `path` (or a byte range of it).
    pub async fn open_read(
        &self,
        path: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<BoxAsyncRead> {
        self.check_read(path).await?;
        self.store.open_read(path, range).await
    }

    /// What is at `path`, if anything.
    pub async fn stat(&self, path: &str) -> Result<Option<FileStat>> {
        self.check_read(path).await?;
        self.store.stat(path).await
    }

    /// The metadata of `path`.
    pub async fn get_meta(&self, path: &str) -> Result<FileMeta> {
        self.check_read(path).await?;
        self.store.get_meta(path).await
    }

    /// The entries of `dir` the caller may see (the rule filters a listing
    /// rather than refusing it; see [`list_visible`]).
    pub async fn list(&self, dir: &str) -> Result<Vec<VisibleEntry>> {
        list_visible(self.store.as_ref(), self.floor, dir, &self.caller).await
    }

    /// Write `data` to `path`, creating or replacing it.
    pub async fn write(&self, path: &str, data: Bytes) -> Result<()> {
        self.check_write(path).await?;
        self.store.write(path, data).await
    }

    /// Make the directory `path`.
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.check_write(path).await?;
        self.store.mkdir(path).await
    }

    /// Delete `path`, answering whether there was anything there.
    pub async fn delete(&self, path: &str) -> Result<bool> {
        self.check_write(path).await?;
        self.store.delete(path).await
    }

    /// Move `from` to `to`: both ends are writes.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.check_write(from).await?;
        self.check_write(to).await?;
        self.store.rename(from, to).await
    }

    /// Replace the metadata of `path` (its rule, its owner).
    pub async fn set_meta(&self, path: &str, meta: &FileMeta) -> Result<()> {
        self.check_write(path).await?;
        self.store.set_meta(path, meta).await
    }
}
