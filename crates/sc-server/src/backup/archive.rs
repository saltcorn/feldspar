//! Reading a backup a piece at a time.
//!
//! A restore used to expand the whole zip into memory first, which made the
//! largest backup an installation could restore smaller than the largest it
//! could take. [`Archive`] reads from the file instead: the metadata documents
//! whole (they are small), and the two things that can be large — a table's
//! rows and a file store's bytes — as a stream, through a blocking thread that
//! hands them over a few at a time.
//!
//! The thread is there because the zip reader is synchronous and borrows the
//! archive, and the restore is asynchronous: a borrow cannot be held across an
//! `await`. So each streamed entry gets its own clone of the archive (the parsed
//! central directory is shared, and the file is read positionally — see
//! [`SharedFile`]), its own thread, and a channel two items deep, which is what
//! bounds how far the reader can run ahead of the inserts.
//!
//! A Saltcorn 1 backup is the exception: [`super::v1::convert`] rewrites it in
//! memory, as it always has, and the result is an [`Archive::Memory`] read
//! through the same methods.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use sc_error::{Error, Result};
use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use zip::ZipArchive;

/// A backup's entries as path → bytes: what a Saltcorn 1 import is converted
/// into, and what the unit tests build archives from.
pub(super) type Entries = BTreeMap<String, Vec<u8>>;

/// At most this many rows in one [`RowBatch`] — the size of one multi-row
/// `INSERT` on the way back in.
pub(super) const BATCH_ROWS: usize = 250;

/// …or this many bytes of row text, whichever comes first, so a table of large
/// rows is handed over a few at a time.
const BATCH_BYTES: usize = 4 * 1024 * 1024;

/// The size of one read of a file store's bytes.
const CHUNK: usize = 64 * 1024;

/// A file up to this size is read into memory and written with one call; a
/// larger one is appended a chunk at a time.
const WHOLE_FILE_LIMIT: usize = 1024 * 1024;

/// A file shared by every reader of one archive, each at its own position.
///
/// Read with `pread` (`seek_read` on Windows), never by moving the file's own
/// cursor, so cloning it is cheap and the clones do not disturb one another.
#[derive(Clone)]
pub(super) struct SharedFile {
    file: Arc<File>,
    len: u64,
    pos: u64,
}

impl SharedFile {
    fn open(path: &Path) -> Result<SharedFile> {
        let file = File::open(path)
            .map_err(|e| Error::file(format!("opening `{}`: {e}", path.display())))?;
        let len = file
            .metadata()
            .map_err(|e| Error::file(format!("reading `{}`: {e}", path.display())))?
            .len();
        Ok(SharedFile {
            file: Arc::new(file),
            len,
            pos: 0,
        })
    }
}

impl Read for SharedFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = read_at(&self.file, buf, self.pos)?;
        self.pos += read as u64;
        Ok(read)
    }
}

impl Seek for SharedFile {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(n) => self.len.checked_add_signed(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
        };
        self.pos = pos.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a seek before the start of the file",
            )
        })?;
        Ok(self.pos)
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

/// A backup to restore from.
#[derive(Clone)]
pub(super) enum Archive {
    /// A zip on disk, read as the restore goes.
    Zip(ZipArchive<SharedFile>),
    /// Entries already in memory — a converted Saltcorn 1 backup.
    Memory(Arc<Entries>),
}

impl Archive {
    /// Open a zip, refusing a file that is not one.
    pub(super) fn open_zip(path: &Path) -> Result<Archive> {
        let file = SharedFile::open(path)?;
        let zip = ZipArchive::new(file).map_err(|e| {
            Error::invalid(format!(
                "this file is not a zip archive, so it is not a Saltcorn backup: {e}"
            ))
        })?;
        Ok(Archive::Zip(zip))
    }

    /// Every entry's name, directories left out.
    pub(super) fn names(&self) -> Vec<String> {
        match self {
            Archive::Zip(zip) => zip
                .file_names()
                .filter(|name| !name.ends_with('/'))
                .map(str::to_owned)
                .collect(),
            Archive::Memory(entries) => entries.keys().cloned().collect(),
        }
    }

    /// Every entry, read whole — what a Saltcorn 1 backup is converted from.
    pub(super) fn expand(&self) -> Result<Entries> {
        let mut out = Entries::new();
        for name in self.names() {
            let bytes = self.bytes(&name)?;
            out.insert(name, bytes);
        }
        Ok(out)
    }

    pub(super) fn contains(&self, path: &str) -> bool {
        match self {
            Archive::Zip(zip) => zip.index_for_name(path).is_some(),
            Archive::Memory(entries) => entries.contains_key(path),
        }
    }

    /// The entries whose names start with `prefix`, sorted, directories left
    /// out.
    pub(super) fn names_under(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> = match self {
            Archive::Zip(zip) => zip
                .file_names()
                .filter(|name| name.starts_with(prefix) && !name.ends_with('/'))
                .map(str::to_owned)
                .collect(),
            Archive::Memory(entries) => entries
                .keys()
                .filter(|name| name.starts_with(prefix))
                .cloned()
                .collect(),
        };
        names.sort();
        names
    }

    /// One entry, read whole and parsed as JSON — for the metadata documents,
    /// which are small.
    pub(super) fn json(&self, path: &str) -> Result<Json> {
        let bytes = self.bytes(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::invalid(format!("`{path}` in the backup is not valid JSON: {e}")))
    }

    /// One entry's bytes, whole. The zip's checksum is checked as the entry is
    /// read to its end, and a mismatch is reported against the entry.
    pub(super) fn bytes(&self, path: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        with_entry(self.clone(), path, |reader| {
            reader
                .read_to_end(&mut out)
                .map_err(|e| damaged(path, &e))?;
            Ok(())
        })?;
        Ok(out)
    }

    /// An NDJSON entry's rows, a batch at a time ([`RowReader`]).
    pub(super) fn rows(&self, path: &str) -> RowReader {
        let (tx, rx) = mpsc::channel(2);
        let archive = self.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = read_rows(archive, &path, &tx) {
                let _ = tx.blocking_send(Err(e));
            }
        });
        RowReader { rx }
    }

    /// Copy one entry into a file store, a chunk at a time, returning how many
    /// bytes were written.
    ///
    /// A small file is written with a single [`write`](sc_files::FileStore::write),
    /// as it always was. A larger one is written empty and then appended to
    /// ([`open_append`](sc_files::FileStore::open_append)), which the local store
    /// streams to disk; a backend without its own append buffers it, as its
    /// uploads do. A file that cannot be read to the end is removed again
    /// rather than left half-written.
    pub(super) async fn copy_into(
        &self,
        path: &str,
        store: &dyn sc_files::FileStore,
        store_path: &str,
    ) -> Result<u64> {
        let (tx, mut rx) = mpsc::channel::<Result<Bytes>>(2);
        let archive = self.clone();
        let entry = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let result = with_entry(archive, &entry, |reader| {
                loop {
                    let mut buffer = vec![0u8; CHUNK];
                    let read = reader.read(&mut buffer).map_err(|e| damaged(&entry, &e))?;
                    if read == 0 {
                        return Ok(());
                    }
                    buffer.truncate(read);
                    if tx.blocking_send(Ok(Bytes::from(buffer))).is_err() {
                        return Ok(());
                    }
                }
            });
            if let Err(e) = result {
                let _ = tx.blocking_send(Err(e));
            }
        });

        // Gather up to the limit: most files end before it.
        let mut head = Vec::new();
        let mut ended = false;
        while head.len() <= WHOLE_FILE_LIMIT {
            match rx.recv().await {
                Some(chunk) => head.extend_from_slice(&chunk?),
                None => {
                    ended = true;
                    break;
                }
            }
        }
        let mut written = head.len() as u64;
        if ended {
            store.write(store_path, Bytes::from(head)).await?;
            return Ok(written);
        }

        store.write(store_path, Bytes::new()).await?;
        let streamed = async {
            let mut writer = store.open_append(store_path).await?;
            let io = |e: std::io::Error| Error::file(format!("writing `{store_path}`: {e}"));
            writer.write_all(&head).await.map_err(io)?;
            drop(head);
            while let Some(chunk) = rx.recv().await {
                let chunk = chunk?;
                writer.write_all(&chunk).await.map_err(io)?;
                written += chunk.len() as u64;
            }
            writer.shutdown().await.map_err(io)?;
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = streamed {
            let _ = store.delete(store_path).await;
            return Err(e);
        }
        Ok(written)
    }
}

/// Run `read` over one entry's bytes, decompressed and checksummed.
fn with_entry<T>(
    archive: Archive,
    path: &str,
    read: impl FnOnce(&mut dyn Read) -> Result<T>,
) -> Result<T> {
    match archive {
        Archive::Zip(mut zip) => {
            let mut entry = zip.by_name(path).map_err(|e| match e {
                zip::result::ZipError::FileNotFound => missing(path),
                other => {
                    Error::invalid(format!("`{path}` cannot be read from the backup: {other}"))
                }
            })?;
            read(&mut entry)
        }
        Archive::Memory(entries) => {
            let bytes = entries.get(path).ok_or_else(|| missing(path))?;
            read(&mut bytes.as_slice())
        }
    }
}

fn missing(path: &str) -> Error {
    Error::invalid(format!("the backup has no `{path}`"))
}

/// An entry that could not be read to its end: a checksum that does not match,
/// a stream that will not decompress, a file cut short.
fn damaged(path: &str, e: &std::io::Error) -> Error {
    Error::invalid(format!(
        "`{path}` in the backup is damaged and could not be read to its end: {e}"
    ))
}

/// Rows handed over from the reading thread.
#[derive(Debug, Default)]
pub(super) struct RowBatch {
    /// The rows that parsed, in file order.
    pub rows: Vec<Json>,
    /// One line per line that did not: `line 12: …`.
    pub bad: Vec<String>,
    /// How many lines this batch covers, rows and bad lines both — what the
    /// restore counts against the manifest.
    pub lines: u64,
}

/// The consuming end of [`Archive::rows`].
pub(super) struct RowReader {
    rx: mpsc::Receiver<Result<RowBatch>>,
}

impl RowReader {
    /// The next batch, `None` at the end. An `Err` is the end too: the entry
    /// could not be read any further.
    pub(super) async fn next_batch(&mut self) -> Option<Result<RowBatch>> {
        self.rx.recv().await
    }
}

/// The reading thread's half of [`Archive::rows`]: one line at a time, so
/// neither end holds more than a batch.
///
/// A line that is not a JSON object costs that row, reported with its line
/// number, and the reading goes on. A read that fails — a checksum, a stream cut
/// short — ends it, reported against the entry.
fn read_rows(archive: Archive, path: &str, tx: &mpsc::Sender<Result<RowBatch>>) -> Result<()> {
    with_entry(archive, path, |reader| {
        let mut reader = BufReader::with_capacity(CHUNK, reader);
        let mut batch = RowBatch::default();
        let mut bytes = 0;
        let mut line_no = 0u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .map_err(|e| damaged(path, &e))?;
            if read == 0 {
                break;
            }
            line_no += 1;
            let text = line.trim_ascii();
            if text.is_empty() {
                continue;
            }
            batch.lines += 1;
            bytes += text.len();
            match serde_json::from_slice::<Json>(text) {
                Ok(row @ Json::Object(_)) => batch.rows.push(row),
                Ok(_) => batch.bad.push(format!("line {line_no}: not a row object")),
                Err(e) => batch.bad.push(format!("line {line_no}: {e}")),
            }
            if batch.rows.len() >= BATCH_ROWS || bytes >= BATCH_BYTES {
                if tx.blocking_send(Ok(std::mem::take(&mut batch))).is_err() {
                    return Ok(());
                }
                bytes = 0;
            }
        }
        if batch.lines > 0 {
            let _ = tx.blocking_send(Ok(batch));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A zip of `entries` in a temporary file, kept for as long as the guard.
    fn zip_of(entries: &[(&str, &[u8])]) -> (super::super::spool::SpoolFile, Archive) {
        let spool = super::super::spool::SpoolFile::new("backup");
        let mut zip = zip::ZipWriter::new(spool.create().unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        let archive = Archive::open_zip(spool.path()).unwrap();
        (spool, archive)
    }

    async fn all(reader: &mut RowReader) -> Result<(Vec<RowBatch>, u64)> {
        let mut batches = Vec::new();
        let mut lines = 0;
        while let Some(batch) = reader.next_batch().await {
            let batch = batch?;
            lines += batch.lines;
            batches.push(batch);
        }
        Ok((batches, lines))
    }

    #[tokio::test]
    async fn rows_are_read_back_in_batches() {
        let mut text = String::new();
        for i in 0..(BATCH_ROWS * 2 + 3) {
            text.push_str(&format!("{{\"id\":{i}}}\n"));
        }
        let (_spool, archive) = zip_of(&[("tables/t/rows.ndjson", text.as_bytes())]);
        let (batches, lines) = all(&mut archive.rows("tables/t/rows.ndjson"))
            .await
            .unwrap();
        assert_eq!(lines, (BATCH_ROWS * 2 + 3) as u64);
        let sizes: Vec<usize> = batches.iter().map(|b| b.rows.len()).collect();
        assert_eq!(sizes, vec![BATCH_ROWS, BATCH_ROWS, 3]);
        assert_eq!(
            batches[2].rows[2],
            serde_json::json!({ "id": BATCH_ROWS * 2 + 2 })
        );
    }

    #[tokio::test]
    async fn large_rows_are_handed_over_a_few_at_a_time() {
        let big = "x".repeat(BATCH_BYTES / 2 + 1);
        let mut text = String::new();
        for _ in 0..5 {
            text.push_str(&format!("{{\"blob\":\"{big}\"}}\n"));
        }
        let (_spool, archive) = zip_of(&[("rows.ndjson", text.as_bytes())]);
        let (batches, lines) = all(&mut archive.rows("rows.ndjson")).await.unwrap();
        assert_eq!(lines, 5);
        let sizes: Vec<usize> = batches.iter().map(|b| b.rows.len()).collect();
        assert_eq!(sizes, vec![2, 2, 1]);
    }

    #[tokio::test]
    async fn a_damaged_line_costs_its_row_and_says_which() {
        let text = b"{\"id\":1}\n{\"id\":2,\"tit\n{\"id\":3}\n[1]\n{\"id\":4}";
        let (_spool, archive) = zip_of(&[("rows.ndjson", text.as_slice())]);
        let (batches, lines) = all(&mut archive.rows("rows.ndjson")).await.unwrap();
        assert_eq!(lines, 5, "a bad line is still a line");
        let rows: Vec<&Json> = batches.iter().flat_map(|b| &b.rows).collect();
        assert_eq!(rows.len(), 3, "the last line needs no newline");
        let bad: Vec<&String> = batches.iter().flat_map(|b| &b.bad).collect();
        assert_eq!(bad.len(), 2);
        assert!(bad[0].starts_with("line 2:"), "{bad:?}");
        assert!(bad[1].starts_with("line 4:"), "{bad:?}");
    }

    #[tokio::test]
    async fn a_checksum_that_does_not_match_is_reported_against_the_entry() {
        // Stored, so the bytes are in the file as they are: flip one.
        let spool2 = super::super::spool::SpoolFile::new("backup");
        let mut zip = zip::ZipWriter::new(spool2.create().unwrap());
        zip.start_file(
            "rows.ndjson",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"{\"id\":1}\n{\"id\":2}\n").unwrap();
        zip.finish().unwrap();
        let mut bytes = std::fs::read(spool2.path()).unwrap();
        let at = bytes.windows(8).position(|w| w == b"{\"id\":2}").unwrap();
        bytes[at + 6] = b'3';
        std::fs::write(spool2.path(), &bytes).unwrap();

        let archive = Archive::open_zip(spool2.path()).unwrap();
        let e = all(&mut archive.rows("rows.ndjson")).await.unwrap_err();
        assert!(e.to_string().contains("`rows.ndjson`"), "{e}");
        assert!(e.to_string().contains("damaged"), "{e}");
        assert!(archive.json("rows.ndjson").is_err());
    }

    #[tokio::test]
    async fn an_entry_is_copied_into_a_store_small_or_large() {
        let large: Vec<u8> = (0..(WHOLE_FILE_LIMIT * 3 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let (_spool, archive) = zip_of(&[
            ("small.txt", b"small".as_slice()),
            ("large.bin", large.as_slice()),
        ]);
        let root = std::env::temp_dir().join(format!("sc-archive-copy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = sc_files::LocalFileStore::new("s", &root).unwrap();
        assert_eq!(
            archive
                .copy_into("small.txt", &store, "a/small.txt")
                .await
                .unwrap(),
            5
        );
        assert_eq!(
            archive
                .copy_into("large.bin", &store, "b/large.bin")
                .await
                .unwrap(),
            large.len() as u64
        );
        assert_eq!(std::fs::read(root.join("a/small.txt")).unwrap(), b"small");
        assert!(std::fs::read(root.join("b/large.bin")).unwrap() == large);
        // A second copy replaces the first rather than appending to it.
        archive
            .copy_into("large.bin", &store, "b/large.bin")
            .await
            .unwrap();
        assert_eq!(
            std::fs::metadata(root.join("b/large.bin")).unwrap().len(),
            large.len() as u64
        );
        assert!(archive.copy_into("absent", &store, "c").await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn names_and_documents_are_found_by_path() {
        let (_spool, archive) = zip_of(&[
            ("manifest.json", b"{\"a\":1}".as_slice()),
            ("file-stores/s/files/b.txt", b"b".as_slice()),
            ("file-stores/s/files/a.txt", b"a".as_slice()),
            ("file-stores/s/store.json", b"{}".as_slice()),
        ]);
        assert!(archive.contains("manifest.json"));
        assert!(!archive.contains("nothing.json"));
        assert_eq!(archive.json("manifest.json").unwrap()["a"], 1);
        assert_eq!(
            archive.names_under("file-stores/s/files/"),
            vec!["file-stores/s/files/a.txt", "file-stores/s/files/b.txt"]
        );
        assert!(
            archive
                .json("nothing.json")
                .unwrap_err()
                .to_string()
                .contains("no `nothing.json`")
        );
    }
}
