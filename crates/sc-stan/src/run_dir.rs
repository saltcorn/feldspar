//! The raw run (TODO §14): CmdStan's own output, kept where `cmdstanpy`,
//! ArviZ and standalone generated quantities can read it.
//!
//! CmdStan needs a real path while it runs, so a fit writes into a **scratch
//! directory** on this node — `stan-run-<pid>-<run id>` under the provider's
//! scratch root — laid out as the published run will be:
//!
//! ```text
//! program/…            the program and its includes, as fitted (§6)
//! data.json            the bound data, exactly as CmdStan read it (§10)
//! coordinates.json     every dimension's keys and labels (§8)
//! config.json          the method, every sampler argument, the seed, the CmdStan version
//! chain-1.csv …        CmdStan's output (gzipped when published)
//! chain-1.log …        each chain's stdout and stderr
//! ```
//!
//! When the model names a `runs_store`, the directory is **published** to
//! `<runs_dir>/<model name>/<run id>/` in that store in one pass after the
//! draws have been read; in a git-backed store a `.gitignore` of `*` is
//! written into `runs_dir` on first use, so megabytes of CSV never become a
//! commit. Either way the scratch directory is removed when the fit is done
//! with it — including when it fails or is cancelled — and a server that died
//! mid-fit has its leftovers removed at the next boot
//! ([`clean_stale_scratch`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use sc_error::{Context, Error, Result};
use sc_files::FileStore;

/// The prefix of every scratch run directory.
const PREFIX: &str = "stan-run-";

/// Where a published run is: a store and a path in it. Kept in the
/// instance's state, which is how [`discard`](crate::StanProvider) finds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunLocation {
    /// The file store.
    pub store: String,
    /// The run directory in it.
    pub path: String,
}

/// A fit's scratch directory, removed when dropped.
#[derive(Debug)]
pub struct RunDir(PathBuf);

impl RunDir {
    /// A fresh scratch directory for run `id` under `root`.
    pub fn create(root: &Path, id: &str) -> Result<RunDir> {
        let dir = root.join(format!("{PREFIX}{}-{}", std::process::id(), segment(id)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the run directory {}", dir.display()))?;
        Ok(RunDir(dir))
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write `bytes` to `name` in the directory.
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A name made safe to be one path segment: a model called `a/b` is published
/// under `a_b`.
pub fn segment(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').trim().to_owned();
    if cleaned.is_empty() {
        "run".to_owned()
    } else {
        cleaned
    }
}

/// Publish the run directory at `dir` to `store`, at
/// `<runs_dir>/<model>/<id>/`, the CSVs gzipped. Answers the published path.
pub async fn publish(
    dir: &Path,
    store: &dyn FileStore,
    runs_dir: &str,
    model: &str,
    id: &str,
) -> Result<String> {
    let runs_dir = runs_dir.trim().trim_matches('/');
    let runs_dir = if runs_dir.is_empty() {
        DEFAULT_RUNS_DIR
    } else {
        runs_dir
    };
    if store.is_git_repo() {
        let ignore = format!("{runs_dir}/.gitignore");
        if store.stat(&ignore).await?.is_none() {
            store
                .write(&ignore, Bytes::from_static(b"*\n"))
                .await
                .with_context(|| {
                    format!("writing {ignore} in the file store `{}`", store.name())
                })?;
        }
    }
    let target = format!("{runs_dir}/{}/{}", segment(model), segment(id));
    for file in files_under(dir)? {
        let relative = file
            .strip_prefix(dir)
            .map_err(|_| Error::msg("a run file outside its run directory"))?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        let (name, bytes) = if relative.ends_with(".csv") {
            (format!("{relative}.gz"), gzip(&bytes)?)
        } else {
            (relative, bytes)
        };
        let path = format!("{target}/{name}");
        store
            .write(&path, Bytes::from(bytes))
            .await
            .with_context(|| format!("publishing {path} to the file store `{}`", store.name()))?;
    }
    Ok(target)
}

/// `runs_dir` when the configuration leaves it empty (§14).
pub const DEFAULT_RUNS_DIR: &str = "stan-runs";

/// Every file under `dir`, recursively, in a stable order.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in
            std::fs::read_dir(&next).with_context(|| format!("listing {}", next.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .context("compressing a chain's CSV")?;
    encoder.finish().context("compressing a chain's CSV")
}

/// Every file of the published run at `path` in `store`, recursively, with its
/// path relative to the run — the CSVs **gunzipped** back to `chain-1.csv`, so
/// the files are what CmdStan wrote and `cmdstanpy.from_csv` reads (§16).
pub async fn read_published(store: &dyn FileStore, path: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let root = path.trim_end_matches('/');
    let mut out = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        let entries = store
            .list(&dir)
            .await
            .with_context(|| format!("listing {dir} in the file store `{}`", store.name()))?;
        for entry in entries {
            if entry.is_dir {
                dirs.push(entry.path);
                continue;
            }
            let bytes = store
                .read(&entry.path)
                .await
                .with_context(|| format!("reading {} from `{}`", entry.path, store.name()))?;
            let relative = entry
                .path
                .strip_prefix(root)
                .unwrap_or(&entry.path)
                .trim_start_matches('/')
                .to_owned();
            out.push(match relative.strip_suffix(".gz") {
                Some(plain) => (plain.to_owned(), gunzip(&bytes)?),
                None => (relative, bytes.to_vec()),
            });
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .context("decompressing a chain's CSV")?;
    Ok(out)
}

/// Remove the scratch run directories under `root` left by a server process
/// that is no longer running — boot's cleanup (§13). A directory of a process
/// that is alive (another server on this machine, mid-fit) is left alone.
/// Answers how many were removed.
pub fn clean_stale_scratch(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !alive(pid) && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Whether a process with this id is running.
pub(crate) fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 checks for the process's existence and delivers
        // nothing.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gzipped_csv_reads_back_as_written() {
        let csv = b"lp__,alpha.1\n-1.5,0.25\n";
        assert_eq!(gunzip(&gzip(csv).unwrap()).unwrap(), csv);
    }

    #[test]
    fn a_model_name_becomes_one_safe_segment() {
        assert_eq!(segment("Radon"), "Radon");
        assert_eq!(segment("a/b"), "a_b");
        assert_eq!(segment(".."), "run");
        assert_eq!(segment("../../etc"), "_.._etc");
    }

    #[cfg(unix)]
    #[test]
    fn stale_scratch_is_removed_and_live_scratch_is_not() {
        let root = std::env::temp_dir().join(format!("sc-stan-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // No process has id 2^31 − 2 (the kernel's pid_max is far lower).
        let dead = root.join(format!("{PREFIX}2147483646-abc"));
        let mine = root.join(format!("{PREFIX}{}-def", std::process::id()));
        let other = root.join("something-else");
        for dir in [&dead, &mine, &other] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        assert_eq!(clean_stale_scratch(&root), 1);
        assert!(!dead.exists());
        assert!(mine.exists() && other.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
