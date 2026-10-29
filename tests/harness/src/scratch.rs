//! Clearing away the scratch directories earlier test runs left in the temp dir.
//!
//! A test's scratch directory is named `<prefix>-<pid>…` and is usually removed
//! by a guard when the test ends. Usually is not enough: a run that is killed
//! never drops its guards, a directory held in a process-wide `static` is never
//! dropped at all, and a child process (an `npm install` a build started) can
//! write into a directory after its guard removed it. Each leaves the directory
//! behind, and on a box whose temp dir is a size-limited tmpfs a few days of test
//! runs fill it until every test that writes a file fails with "Disk quota
//! exceeded".
//!
//! [`sweep_stale_scratch`] removes them on the next run instead: any
//! `<prefix>-<pid>…` whose process is gone. A live process's directories —
//! another test binary running beside this one — are never touched.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Remove every `<temp dir>/<prefix>-<pid>…` entry whose `<pid>` is no longer
/// running. Done once per prefix per process, so a test can call it each time it
/// makes a scratch directory.
///
/// Only Linux can tell whether a process is running (through `/proc`); elsewhere
/// this does nothing.
pub fn sweep_stale_scratch(prefix: &str) {
    static SWEPT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let swept = SWEPT.get_or_init(Default::default);
    let first = match swept.lock() {
        Ok(mut swept) => swept.insert(prefix.to_string()),
        Err(poisoned) => poisoned.into_inner().insert(prefix.to_string()),
    };
    if !first || !Path::new("/proc/self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| owning_pid(name, prefix)) else {
            continue;
        };
        if pid == std::process::id() || Path::new("/proc").join(pid.to_string()).exists() {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).ok();
        } else {
            std::fs::remove_file(&path).ok();
        }
    }
}

/// The pid in `name` when it is `<prefix>-<pid>` or `<prefix>-<pid>-…`.
fn owning_pid(name: &str, prefix: &str) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let digits = rest.split('-').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pid_follows_the_prefix_and_nothing_else_matches() {
        assert_eq!(owning_pid("sc-module-123", "sc-module"), Some(123));
        assert_eq!(owning_pid("sc-module-123-tag-ThreadId(4)", "sc-module"), Some(123));
        // A longer prefix is someone else's.
        assert_eq!(owning_pid("sc-module-routes-123", "sc-module"), None);
        assert_eq!(owning_pid("sc-modules-123", "sc-module"), None);
        assert_eq!(owning_pid("sc-module", "sc-module"), None);
        assert_eq!(owning_pid("sc-module-", "sc-module"), None);
    }

    #[test]
    fn a_dead_process_s_directory_is_removed_and_a_live_one_s_kept() {
        let prefix = format!("sc-harness-sweep-{}", std::process::id());
        // No real process has a pid this large (`pid_max` tops out at 2^22).
        let dead = std::env::temp_dir().join(format!("{prefix}-4294967294-x"));
        let live = std::env::temp_dir().join(format!("{prefix}-{}-x", std::process::id()));
        std::fs::create_dir_all(dead.join("inner")).unwrap();
        std::fs::create_dir_all(&live).unwrap();

        sweep_stale_scratch(&prefix);

        assert!(!dead.exists(), "a dead process's scratch must be removed");
        assert!(live.exists(), "this process's own scratch must be kept");
        std::fs::remove_dir_all(&live).ok();
    }
}
