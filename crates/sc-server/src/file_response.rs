//! The one file response builder (TODO F2, §2.2): static directories, the REST
//! `File`-field route, an application's `/files/serve/*` and the admin download
//! all answer through [`serve`].
//!
//! What it gives every one of them:
//!
//! - **a streamed body** ([`FileStore::open_read`](sc_files::FileStore)), so a
//!   50 MB video is never 50 MB in this process;
//! - **`Range`** — `206` with `Content-Range`, `Accept-Ranges: bytes`, and `416`
//!   for a range that starts past the end. A multi-range request is answered
//!   with the whole file, which RFC 9110 allows and every client handles; an
//!   `If-Range` that no longer matches does the same;
//! - **`Content-Disposition`** with an RFC 5987 `filename*=UTF-8''…` beside an
//!   ASCII `filename="…"`, so `naïve 名前.png` is saved as itself. The name is
//!   the descriptor's, else the file's `attributes.filename`, else the storage
//!   basename; `?download=1` makes it an `attachment`;
//! - **a cheap ETag** over size, modification time and path — nothing is read
//!   to compute it — with `Last-Modified`, and `304` on `If-None-Match`.
//!
//! The access check is not here: whoever built the [`ServeFile`] made it. A file
//! that is not there (or is a directory) is the same `404` a refused one gets.

use std::hash::{Hash, Hasher};
use std::ops::Range;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use sc_files::{BoxAsyncRead, FILENAME_ATTRIBUTE, ServeFile};

/// How much of the file one body frame carries.
const CHUNK: usize = 64 * 1024;

/// The default `Cache-Control`: always revalidated (the ETag makes that cheap)
/// and never kept by a shared cache, since who may read a file depends on who
/// is asking.
const DEFAULT_CACHE_CONTROL: &str = "private, no-cache";

/// What a `Range` header asks of a file of a given size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RangeAsk {
    /// No usable range: serve the whole file (no header, a unit other than
    /// bytes, a syntax error, or several ranges).
    Whole,
    /// One satisfiable range, half-open.
    Part(Range<u64>),
    /// A range that starts at or past the end: `416`.
    Unsatisfiable,
}

/// Read a `Range` header against a file of `size` bytes.
pub(crate) fn parse_range(value: &str, size: u64) -> RangeAsk {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeAsk::Whole;
    };
    if spec.contains(',') {
        return RangeAsk::Whole;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return RangeAsk::Whole;
    };
    let (start, end) = (start.trim(), end.trim());
    let number = |text: &str| text.parse::<u64>().ok();
    match (start.is_empty(), end.is_empty()) {
        // `bytes=-N`: the last N bytes.
        (true, false) => match number(end) {
            Some(0) => RangeAsk::Unsatisfiable,
            Some(_) if size == 0 => RangeAsk::Unsatisfiable,
            Some(n) => RangeAsk::Part(size.saturating_sub(n)..size),
            None => RangeAsk::Whole,
        },
        // `bytes=N-`: from N to the end.
        (false, true) => match number(start) {
            Some(s) if s >= size => RangeAsk::Unsatisfiable,
            Some(s) => RangeAsk::Part(s..size),
            None => RangeAsk::Whole,
        },
        // `bytes=N-M`, inclusive, the end clamped to the file.
        (false, false) => match (number(start), number(end)) {
            (Some(s), Some(e)) if s > e => RangeAsk::Whole,
            (Some(s), Some(_)) if s >= size => RangeAsk::Unsatisfiable,
            (Some(s), Some(e)) => RangeAsk::Part(s..e.saturating_add(1).min(size)),
            _ => RangeAsk::Whole,
        },
        (true, true) => RangeAsk::Whole,
    }
}

/// `Content-Disposition` for `filename`: `inline` or `attachment`, with an
/// ASCII `filename` for old clients and the exact name as `filename*`.
pub(crate) fn content_disposition(filename: &str, attachment: bool) -> String {
    let kind = if attachment { "attachment" } else { "inline" };
    let ascii: String = filename
        .chars()
        .map(|c| match c {
            ' '..='~' if c != '"' && c != '\\' => c,
            _ => '_',
        })
        .collect();
    let mut encoded = String::new();
    for byte in filename.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => encoded.push(*byte as char),
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~' => {
                encoded.push(*byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// The ETag of a file: a hash of its path, size and modification time — which
/// is what changes when the bytes do, and costs no read.
pub(crate) fn cheap_etag(path: &str, size: u64, modified: Option<&str>) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    size.hash(&mut hasher);
    modified.hash(&mut hasher);
    format!("\"{:016x}-{size:x}\"", hasher.finish())
}

/// RFC 3339 as an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date(rfc3339: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(rfc3339).ok()?;
    Some(
        at.with_timezone(&chrono::Utc)
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
    )
}

/// Whether `?download=1` (or `true`) is on the request.
fn download_asked(uri: &Uri) -> bool {
    uri.query().is_some_and(|q| {
        q.split('&')
            .any(|pair| matches!(pair, "download=1" | "download=true"))
    })
}

/// A not-found, in the JSON every other refusal is.
fn missing() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": "not found" })),
    )
        .into_response()
}

/// The file `file` names, answered for a request of `method` to `uri` with
/// `headers`.
pub(crate) async fn serve(
    file: &ServeFile,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Response {
    let store = file.store.as_ref();
    let stat = match store.stat(&file.path).await {
        Ok(Some(stat)) if !stat.is_dir => stat,
        Ok(_) => return missing(),
        Err(e) => {
            crate::router::log_failure("reading a file to serve", &e);
            return missing();
        }
    };
    let size = stat.size;
    let etag = cheap_etag(&file.path, size, stat.modified.as_deref());
    let last_modified = stat.modified.as_deref().and_then(http_date);

    let mut out = HeaderMap::new();
    let mut put = |name: header::HeaderName, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            out.insert(name, value);
        }
    };
    put(header::ETAG, &etag);
    if let Some(date) = &last_modified {
        put(header::LAST_MODIFIED, date);
    }
    put(header::ACCEPT_RANGES, "bytes");
    put(
        header::CACHE_CONTROL,
        file.cache_control
            .as_deref()
            .unwrap_or(DEFAULT_CACHE_CONTROL),
    );

    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    if not_modified {
        return (StatusCode::NOT_MODIFIED, out).into_response();
    }

    let content_type = file.content_type.clone().unwrap_or_else(|| {
        sc_files::mime_for_path(&file.path).unwrap_or_else(|| "application/octet-stream".to_owned())
    });
    put(header::CONTENT_TYPE, &content_type);
    let name = match &file.filename {
        Some(name) => name.clone(),
        None => store
            .get_meta(&file.path)
            .await
            .ok()
            .and_then(|meta| meta.attributes.get(FILENAME_ATTRIBUTE).cloned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                file.path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&file.path)
                    .to_owned()
            }),
    };
    put(
        header::CONTENT_DISPOSITION,
        &content_disposition(&name, file.download || download_asked(uri)),
    );

    // `If-Range`: the range is only for the representation the client has; a
    // validator that no longer matches gets the whole file.
    let range_still_good = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v == etag || last_modified.as_deref() == Some(v));
    let ask = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(value) if range_still_good => parse_range(value, size),
        _ => RangeAsk::Whole,
    };
    let (status, range) = match ask {
        RangeAsk::Whole => (StatusCode::OK, None),
        RangeAsk::Part(range) => {
            put(
                header::CONTENT_RANGE,
                &format!("bytes {}-{}/{size}", range.start, range.end - 1),
            );
            (StatusCode::PARTIAL_CONTENT, Some(range))
        }
        RangeAsk::Unsatisfiable => {
            put(header::CONTENT_RANGE, &format!("bytes */{size}"));
            out.remove(header::CONTENT_TYPE);
            out.remove(header::CONTENT_DISPOSITION);
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                out,
                axum::Json(serde_json::json!({
                    "error": format!("that range is not within the file, which is {size} bytes")
                })),
            )
                .into_response();
        }
    };
    let length = range.as_ref().map_or(size, |r| r.end - r.start);
    put(header::CONTENT_LENGTH, &length.to_string());

    if method == Method::HEAD {
        return (status, out).into_response();
    }
    let reader = match store.open_read(&file.path, range).await {
        Ok(reader) => reader,
        Err(e) => {
            crate::router::log_failure("opening a file to serve", &e);
            return missing();
        }
    };
    (status, out, Body::from_stream(chunks(reader))).into_response()
}

/// A reader as a stream of body frames of at most [`CHUNK`] bytes.
fn chunks(
    reader: BoxAsyncRead,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static {
    futures::stream::try_unfold(reader, |mut reader| async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0u8; CHUNK];
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        Ok(Some((bytes::Bytes::from(buffer), reader)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn a_range_is_read_as_rfc_9110_says() {
        assert_eq!(parse_range("bytes=0-99", 1000), RangeAsk::Part(0..100));
        assert_eq!(parse_range("bytes=900-", 1000), RangeAsk::Part(900..1000));
        assert_eq!(parse_range("bytes=-100", 1000), RangeAsk::Part(900..1000));
        // A suffix longer than the file is the whole file, as a range.
        assert_eq!(parse_range("bytes=-5000", 1000), RangeAsk::Part(0..1000));
        // An end past the file is clamped to it.
        assert_eq!(
            parse_range("bytes=990-5000", 1000),
            RangeAsk::Part(990..1000)
        );
        assert_eq!(parse_range("bytes=1000-", 1000), RangeAsk::Unsatisfiable);
        assert_eq!(
            parse_range("bytes=2000-2100", 1000),
            RangeAsk::Unsatisfiable
        );
        assert_eq!(parse_range("bytes=-0", 1000), RangeAsk::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), RangeAsk::Unsatisfiable);
        // Several ranges, another unit, or nonsense: the whole file.
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), RangeAsk::Whole);
        assert_eq!(parse_range("items=0-1", 1000), RangeAsk::Whole);
        assert_eq!(parse_range("bytes=x-y", 1000), RangeAsk::Whole);
        assert_eq!(parse_range("bytes=9-3", 1000), RangeAsk::Whole);
    }

    #[test]
    fn a_name_survives_in_filename_star_with_an_ascii_fallback() {
        let header = content_disposition("naïve 名前.png", false);
        assert_eq!(
            header,
            "inline; filename=\"na_ve __.png\"; \
             filename*=UTF-8''na%C3%AFve%20%E5%90%8D%E5%89%8D.png"
        );
        let header = content_disposition("say \"hi\".txt", true);
        assert!(
            header.starts_with("attachment; filename=\"say _hi_.txt\""),
            "{header}"
        );
        assert!(
            header.ends_with("filename*=UTF-8''say%20%22hi%22.txt"),
            "{header}"
        );
        // And the header value is one a client will accept.
        assert!(HeaderValue::from_str(&header).is_ok());
    }

    #[test]
    fn the_etag_follows_size_time_and_path() {
        let a = cheap_etag("a.png", 10, Some("2026-01-01T00:00:00Z"));
        assert_eq!(a, cheap_etag("a.png", 10, Some("2026-01-01T00:00:00Z")));
        assert_ne!(a, cheap_etag("a.png", 11, Some("2026-01-01T00:00:00Z")));
        assert_ne!(a, cheap_etag("a.png", 10, Some("2026-01-02T00:00:00Z")));
        assert_ne!(a, cheap_etag("b.png", 10, Some("2026-01-01T00:00:00Z")));
        assert!(a.starts_with('"') && a.ends_with('"'));
    }

    /// A file of `size` bytes that exists only as a counter: `open_read`
    /// produces it as it is read, and `read` — the whole file in memory — is
    /// counted, so a test can assert it never happened.
    struct Generated {
        size: u64,
        whole_reads: AtomicU64,
        attributes: std::collections::BTreeMap<String, String>,
    }

    /// A reader of `remaining` bytes of `b'x'`, allocating nothing beyond the
    /// buffer it is handed.
    struct Lazy {
        remaining: u64,
    }

    impl tokio::io::AsyncRead for Lazy {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let n = (buf.remaining() as u64).min(self.remaining) as usize;
            buf.put_slice(&vec![b'x'; n]);
            self.remaining -= n as u64;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[async_trait::async_trait]
    impl sc_files::FileStore for Generated {
        fn name(&self) -> &str {
            "generated"
        }
        async fn read(&self, _path: &str) -> sc_error::Result<bytes::Bytes> {
            self.whole_reads.fetch_add(1, Ordering::SeqCst);
            Ok(bytes::Bytes::from(vec![b'x'; self.size as usize]))
        }
        async fn write(&self, _: &str, _: bytes::Bytes) -> sc_error::Result<()> {
            Ok(())
        }
        async fn list(&self, _: &str) -> sc_error::Result<Vec<sc_files::Entry>> {
            Ok(Vec::new())
        }
        async fn mkdir(&self, _: &str) -> sc_error::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &str) -> sc_error::Result<bool> {
            Ok(false)
        }
        async fn rename(&self, _: &str, _: &str) -> sc_error::Result<()> {
            Ok(())
        }
        fn is_git_repo(&self) -> bool {
            false
        }
        async fn stat(&self, path: &str) -> sc_error::Result<Option<sc_files::FileStat>> {
            Ok((path != "missing.bin").then(|| sc_files::FileStat {
                size: self.size,
                is_dir: false,
                modified: Some("2026-10-12T10:00:00Z".to_owned()),
            }))
        }
        async fn open_read(
            &self,
            _path: &str,
            range: Option<Range<u64>>,
        ) -> sc_error::Result<BoxAsyncRead> {
            let remaining = range.map_or(self.size, |r| r.end - r.start);
            Ok(Box::new(Lazy { remaining }))
        }
        async fn get_meta(&self, _: &str) -> sc_error::Result<sc_files::FileMeta> {
            Ok(sc_files::FileMeta {
                attributes: self.attributes.clone(),
                ..Default::default()
            })
        }
        async fn set_meta(&self, _: &str, _: &sc_files::FileMeta) -> sc_error::Result<()> {
            Ok(())
        }
    }

    fn generated(size: u64) -> Arc<Generated> {
        Arc::new(Generated {
            size,
            whole_reads: AtomicU64::new(0),
            attributes: [(FILENAME_ATTRIBUTE.to_owned(), "naïve 名前.png".to_owned())]
                .into_iter()
                .collect(),
        })
    }

    async fn ask(
        store: &Arc<Generated>,
        path: &str,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
    ) -> Response {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        let file = ServeFile::new(store.clone(), path);
        serve(&file, &Method::GET, &uri.parse().unwrap(), &map).await
    }

    fn header_of(response: &Response, name: header::HeaderName) -> &str {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    #[tokio::test]
    async fn a_range_is_a_206_and_a_bad_one_a_416() {
        let store = generated(1000);
        let response = ask(&store, "a.png", "/a.png", &[(header::RANGE, "bytes=0-99")]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_of(&response, header::CONTENT_RANGE),
            "bytes 0-99/1000"
        );
        assert_eq!(header_of(&response, header::CONTENT_LENGTH), "100");
        assert_eq!(header_of(&response, header::ACCEPT_RANGES), "bytes");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.len(), 100);

        let response = ask(&store, "a.png", "/a.png", &[(header::RANGE, "bytes=-10")]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_of(&response, header::CONTENT_RANGE),
            "bytes 990-999/1000"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.len(), 10);

        let response = ask(&store, "a.png", "/a.png", &[(header::RANGE, "bytes=5000-")]).await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header_of(&response, header::CONTENT_RANGE), "bytes */1000");

        // A stale `If-Range` gets the whole file.
        let response = ask(
            &store,
            "a.png",
            "/a.png",
            &[
                (header::RANGE, "bytes=0-9"),
                (header::IF_RANGE, "\"stale\""),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // A missing file is a 404.
        let response = ask(&store, "missing.bin", "/missing.bin", &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_name_validators_and_disposition_are_set() {
        let store = generated(10);
        let response = ask(&store, "stored/0b1c.bin", "/f?download=1", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, header::CONTENT_DISPOSITION),
            "attachment; filename=\"na_ve __.png\"; \
             filename*=UTF-8''na%C3%AFve%20%E5%90%8D%E5%89%8D.png",
            "the file's own name, not the storage path's"
        );
        assert_eq!(
            header_of(&response, header::LAST_MODIFIED),
            "Mon, 12 Oct 2026 10:00:00 GMT"
        );
        let etag = header_of(&response, header::ETAG).to_owned();
        let response = ask(
            &store,
            "stored/0b1c.bin",
            "/f",
            &[(header::IF_NONE_MATCH, &etag)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let response = ask(&store, "stored/0b1c.bin", "/f", &[]).await;
        assert!(header_of(&response, header::CONTENT_DISPOSITION).starts_with("inline;"));
    }

    /// 50 MB served, and never held: the store's whole-file `read` is not
    /// called, and no body frame is larger than [`CHUNK`].
    #[tokio::test]
    async fn a_large_file_is_streamed_rather_than_read_whole() {
        use futures::StreamExt;
        let size = 50 * 1024 * 1024;
        let store = generated(size);
        let response = ask(&store, "big.bin", "/big.bin", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, header::CONTENT_LENGTH),
            size.to_string()
        );
        let mut body = response.into_body().into_data_stream();
        let (mut total, mut largest) = (0u64, 0usize);
        while let Some(data) = body.next().await {
            let data = data.unwrap();
            total += data.len() as u64;
            largest = largest.max(data.len());
        }
        assert_eq!(total, size);
        assert!(largest <= CHUNK, "a frame of {largest} bytes");
        assert_eq!(store.whole_reads.load(Ordering::SeqCst), 0, "read whole");
    }
}
