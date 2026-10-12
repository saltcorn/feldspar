//! Streaming over a file store (TODO F2, §2.4): the types
//! [`FileStore::open_read`](crate::FileStore::open_read) and
//! [`FileStore::open_append`](crate::FileStore::open_append) answer, and the
//! fallbacks a driver gets without implementing either.
//!
//! The fallbacks are the reason a module driver written against `read`/`write`
//! keeps working: a ranged read slices a whole read, an append buffers and
//! writes the joined bytes back when it is shut down. Neither streams — that is
//! what overriding them is for, as [`crate::LocalFileStore`] does — but neither
//! is wrong.

use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use sc_error::{Error, Repr, Result};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::store::FileStore;

/// A file's bytes as a stream, owning everything it reads from — so a response
/// body can outlive the request that opened it.
pub type BoxAsyncRead = Box<dyn AsyncRead + Send + Unpin + 'static>;

/// Somewhere to append a file's bytes. It borrows the store it writes to.
///
/// **Shut it down** (`AsyncWriteExt::shutdown`) when done: a driver that
/// buffers, as the default does, writes nothing until then, and one that does
/// not is flushed by it.
pub type BoxAsyncWrite<'a> = Box<dyn AsyncWrite + Send + Unpin + 'a>;

/// Check a requested range against a file of `size` bytes, as the HTTP layer
/// has already resolved it: non-empty or empty, never inverted, never past the
/// end.
pub fn check_range(range: &Range<u64>, size: u64, path: &str) -> Result<()> {
    if range.start > range.end || range.end > size {
        return Err(Error::invalid(format!(
            "bytes {}..{} are not within {path:?}, which is {size} bytes",
            range.start, range.end
        )));
    }
    Ok(())
}

/// The default [`FileStore::open_read`](crate::FileStore::open_read): the
/// whole file, sliced.
pub(crate) async fn read_by_slicing(
    store: &(impl FileStore + ?Sized),
    path: &str,
    range: Option<Range<u64>>,
) -> Result<BoxAsyncRead> {
    let bytes = store.read(path).await?;
    let bytes = match range {
        None => bytes,
        Some(range) => {
            check_range(&range, bytes.len() as u64, path)?;
            bytes.slice(range.start as usize..range.end as usize)
        }
    };
    Ok(Box::new(std::io::Cursor::new(bytes)))
}

/// The default [`FileStore::concat`](crate::FileStore::concat): every part read,
/// joined in memory and written once — so a missing part fails before the
/// target is touched.
pub(crate) async fn concat_in_memory(
    store: &(impl FileStore + ?Sized),
    parts: &[String],
    to: &str,
) -> Result<u64> {
    let mut joined = BytesMut::new();
    for part in parts {
        joined.extend_from_slice(&store.read(part).await?);
    }
    let size = joined.len() as u64;
    store.write(to, joined.freeze()).await?;
    Ok(size)
}

/// What a flush in progress is waiting on.
type Flush<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// The default [`FileStore::open_append`](crate::FileStore::open_append):
/// buffered in memory, and written back — what was there, then what was
/// appended — when shut down.
pub(crate) struct BufferedAppend<'a, S: FileStore + ?Sized> {
    store: &'a S,
    path: String,
    buffer: Vec<u8>,
    flush: Option<Flush<'a>>,
}

impl<'a, S: FileStore + ?Sized> BufferedAppend<'a, S> {
    pub(crate) fn new(store: &'a S, path: &str) -> BufferedAppend<'a, S> {
        BufferedAppend {
            store,
            path: path.to_owned(),
            buffer: Vec::new(),
            flush: None,
        }
    }
}

impl<'a, S: FileStore + ?Sized> AsyncWrite for BufferedAppend<'a, S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.flush.is_some() {
            return Poll::Ready(Err(std::io::Error::other("this append has been shut down")));
        }
        self.buffer.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Nothing reaches the store before shutdown: a flush that wrote the
        // whole file back on every call would make each chunk cost the file.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.flush.is_none() {
            let store = this.store;
            let path = this.path.clone();
            let appended = std::mem::take(&mut this.buffer);
            this.flush = Some(Box::pin(async move {
                let mut joined = match store.read(&path).await {
                    Ok(existing) => BytesMut::from(&existing[..]),
                    Err(e) if matches!(e.repr(), Repr::NotFound(_)) => BytesMut::new(),
                    Err(e) => return Err(e),
                };
                joined.extend_from_slice(&appended);
                store.write(&path, joined.freeze()).await
            }));
        }
        match this.flush.as_mut().map(|f| f.as_mut().poll(cx)) {
            Some(Poll::Ready(result)) => {
                // Leave a finished future behind would poll it again; an empty
                // one that is already done answers every later shutdown.
                this.flush = Some(Box::pin(async { Ok(()) }));
                Poll::Ready(result.map_err(std::io::Error::other))
            }
            Some(Poll::Pending) => Poll::Pending,
            None => Poll::Ready(Ok(())),
        }
    }
}

/// Bytes, for a test or a caller that wants to read a stream back whole.
pub async fn read_to_bytes(mut reader: BoxAsyncRead) -> Result<Bytes> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .await
        .map_err(|e| Error::file(format!("reading a file stream: {e}")))?;
    Ok(Bytes::from(out))
}
