//! A file to be served over HTTP, as the layers below the server describe it
//! (TODO F2, §2.2).
//!
//! The REST provider, an application framework and the admin handlers all end in
//! "serve this file". None of them can build the response: Range, conditional
//! requests and a streamed body need the request's headers and the transport's
//! body type, and both belong to `sc-server`. So each answers a [`ServeFile`] —
//! which store, which path, and how to name it — and the server's one response
//! builder turns it into bytes on the wire. The access check has already been
//! made by whoever built it; a `ServeFile` is a decision, not a request.

use std::fmt;
use std::sync::Arc;

use crate::store::FileStore;

/// Which file to serve, and how.
#[derive(Clone)]
pub struct ServeFile {
    /// The store the file is in. Private, with [`path`](ServeFile::path): a
    /// `ServeFile` handed out by a [`CheckedStore`](crate::CheckedStore) is a
    /// grant for one path, and its operations below reach that path only.
    store: Arc<dyn FileStore>,
    /// Its store-relative path.
    path: String,
    /// The name to give it in `Content-Disposition`, in place of the file's own
    /// (`attributes.filename`, else the storage basename).
    pub filename: Option<String>,
    /// Whether it is an `attachment` (saved) rather than `inline` (shown).
    /// `?download=1` on the request asks for the same.
    pub download: bool,
    /// The `Content-Type`, in place of the one guessed from the path.
    pub content_type: Option<String>,
    /// The `Cache-Control`, in place of the builder's default (`private,
    /// no-cache`: revalidated by its ETag, and never kept by a shared cache,
    /// since whether a file may be read depends on who is asking).
    pub cache_control: Option<String>,
}

impl ServeFile {
    /// The file's store-relative path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The name of the store the file is in.
    pub fn store_name(&self) -> &str {
        self.store.name()
    }

    /// What is at the path, if anything.
    pub async fn stat(&self) -> sc_error::Result<Option<crate::FileStat>> {
        self.store.stat(&self.path).await
    }

    /// The file's metadata.
    pub async fn get_meta(&self) -> sc_error::Result<crate::FileMeta> {
        self.store.get_meta(&self.path).await
    }

    /// A stream over the file, or a byte range of it.
    pub async fn open_read(
        &self,
        range: Option<std::ops::Range<u64>>,
    ) -> sc_error::Result<crate::BoxAsyncRead> {
        self.store.open_read(&self.path, range).await
    }

    /// Serve `path` from `store`, named and typed as the file itself says.
    pub fn new(store: Arc<dyn FileStore>, path: impl Into<String>) -> ServeFile {
        ServeFile {
            store,
            path: path.into(),
            filename: None,
            download: false,
            content_type: None,
            cache_control: None,
        }
    }

    /// The same, under `content_type`.
    #[must_use]
    pub fn content_type(mut self, content_type: impl Into<String>) -> ServeFile {
        self.content_type = Some(content_type.into());
        self
    }

    /// The same, with `cache_control`.
    #[must_use]
    pub fn cache_control(mut self, cache_control: impl Into<String>) -> ServeFile {
        self.cache_control = Some(cache_control.into());
        self
    }

    /// The same, named `filename`.
    #[must_use]
    pub fn filename(mut self, filename: impl Into<String>) -> ServeFile {
        self.filename = Some(filename.into());
        self
    }

    /// The same, as an attachment.
    #[must_use]
    pub fn download(mut self, download: bool) -> ServeFile {
        self.download = download;
        self
    }
}

impl fmt::Debug for ServeFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServeFile")
            .field("store", &self.store.name())
            .field("path", &self.path)
            .field("filename", &self.filename)
            .field("download", &self.download)
            .field("content_type", &self.content_type)
            .field("cache_control", &self.cache_control)
            .finish()
    }
}

/// Two descriptions of the same file served the same way. The store is compared
/// by name, which is what identifies a store to everything else.
impl PartialEq for ServeFile {
    fn eq(&self, other: &ServeFile) -> bool {
        self.store.name() == other.store.name()
            && self.path == other.path
            && self.filename == other.filename
            && self.download == other.download
            && self.content_type == other.content_type
            && self.cache_control == other.cache_control
    }
}
