//! Files a backup passes through: one being written before it is served or
//! sent, and one uploaded and waiting to be restored.
//!
//! **On real disk, on purpose.** The spool directory is
//! `<data dir>/backup-spool` ([`sc_files::data_dir`]) and only falls back to the
//! system temporary directory when that cannot be made. On Debian 13 `/tmp` is a
//! tmpfs, and a tmpfs's pages count against the memory limit of the cgroup that
//! wrote them — so a backup spooled there would be charged to the very limit
//! that streaming it was meant to stay under.
//!
//! Every spooled file has an owner, a [`SpoolFile`], and is deleted when that
//! owner is dropped, whichever way the work around it ended. A process that
//! dies leaves its files behind, which is what [`sweep`] at boot is for.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sc_error::{Error, Result};
use uuid::Uuid;

/// The spool directory's name inside the data directory.
const SPOOL_DIR: &str = "backup-spool";

/// How old a file in the spool directory must be before a boot removes it. A
/// day is longer than any backup or any upload's wait
/// ([`super::PendingUploads`]) and short enough that a crash does not leave a
/// gigabyte lying about for long.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Where spooled files go, created if it is not there yet.
pub fn spool_dir() -> PathBuf {
    sc_files::data_dir()
        .ok()
        .map(|dir| dir.join(SPOOL_DIR))
        .filter(|dir| std::fs::create_dir_all(dir).is_ok())
        .unwrap_or_else(std::env::temp_dir)
}

/// A file deleted when this is dropped.
#[derive(Debug)]
pub struct SpoolFile {
    path: PathBuf,
    armed: bool,
}

impl SpoolFile {
    /// A fresh, unique name in the spool directory. Nothing is created until
    /// [`create`](SpoolFile::create).
    pub fn new(what: &str) -> SpoolFile {
        SpoolFile::at(spool_dir().join(format!("{what}-{}.zip", Uuid::new_v4())))
    }

    /// Own `path`: whatever is there when this is dropped is deleted.
    pub fn at(path: PathBuf) -> SpoolFile {
        SpoolFile { path, armed: true }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the file (replacing one that is there) for writing.
    pub fn create(&self) -> Result<std::fs::File> {
        std::fs::File::create(&self.path)
            .map_err(|e| Error::file(format!("creating `{}`: {e}", self.path.display())))
    }

    /// Let the file outlive this — it has been renamed to where it belongs, or
    /// handed to an owner that deletes it itself.
    pub fn keep(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        if self.armed {
            // Already gone is fine: an unlinked-while-open download, or a
            // rename that moved it.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Remove spooled files older than a day — what a process that died mid-backup
/// or mid-upload left behind. Run once, when the server starts. Returns how many
/// were removed.
pub fn sweep() -> usize {
    sweep_older_than(&spool_dir(), STALE_AFTER)
}

fn sweep_older_than(dir: &Path, age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    // Only names this module gives, so the fallback (the system temporary
    // directory) never has anything of anybody else's removed.
    let ours = |name: &str| {
        name.ends_with(".zip")
            && ["backup-", "upload-"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|elapsed| elapsed >= age);
        if ours(&name) && old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spool_file_is_removed_when_dropped_unless_kept() {
        let dropped = SpoolFile::new("backup");
        std::fs::write(dropped.path(), b"x").unwrap();
        let path = dropped.path().to_owned();
        drop(dropped);
        assert!(!path.exists());

        let kept = SpoolFile::new("backup");
        std::fs::write(kept.path(), b"x").unwrap();
        let path = kept.keep();
        assert!(path.exists());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_sweep_takes_only_old_files_with_our_names() {
        let dir = std::env::temp_dir().join(format!("sc-spool-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["backup-a.zip", "upload-b.zip", "someone-elses.zip"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // Nothing is a day old yet.
        assert_eq!(sweep_older_than(&dir, STALE_AFTER), 0);
        // Everything is at least no time old.
        assert_eq!(sweep_older_than(&dir, Duration::ZERO), 2);
        assert!(dir.join("someone-elses.zip").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
