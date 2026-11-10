//! Where Saltcorn puts directories it owns, and how a store's name becomes one.
//!
//! Two backends need a place on disk that the admin did not name: the git
//! backend clones into one ([`clone_dir`](crate::clone_dir)), and the local
//! backend *suggests* one ([`suggest_local_dir`]) to an admin who has no opinion
//! about where a store's files should live. Both answers descend from the same
//! [`data_dir`], so an operator who moves one moves all of them, and the
//! server's own data stays in one place rather than two.

use sc_error::{Error, Result};
use std::path::PathBuf;

/// Environment variable overriding the base directory Saltcorn keeps its data
/// in, for tests and for an operator who wants it elsewhere.
///
/// Tests need this: they must not clone into the real user's data directory,
/// and a test that did would leave a repository behind on the developer's
/// machine and collide with the next run.
pub const DATA_DIR_ENV: &str = "SC_DATA_DIR";

/// The base directory Saltcorn keeps its own data in, chosen per operating
/// system.
///
/// Hand-rolled rather than taken from the `dirs` crate, which would be a
/// dependency in layer 5 for fifteen lines of `std::env::var`. The conventions
/// are stable and each is the platform's documented one:
///
/// | Platform | Directory |
/// |---|---|
/// | Windows | `%LOCALAPPDATA%\Feldspar` |
/// | macOS | `~/Library/Application Support/Feldspar` |
/// | other (XDG) | `$XDG_DATA_HOME/feldspar`, else `~/.local/share/feldspar` |
///
/// [`DATA_DIR_ENV`] overrides all of it. An environment with none of the
/// variables set — a daemon started with a scrubbed environment — is an error
/// rather than a fallback to the current directory, which would scatter clones
/// wherever the process happened to be started.
pub fn data_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var(DATA_DIR_ENV)
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    if cfg!(windows) {
        return std::env::var("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("Feldspar"))
            .map_err(|_| {
                Error::config(
                    "cannot decide where to keep Saltcorn's data: neither \
                     `SC_DATA_DIR` nor `LOCALAPPDATA` is set",
                )
            });
    }
    let home = std::env::var("HOME").map_err(|_| {
        Error::config(
            "cannot decide where to keep Saltcorn's data: neither `SC_DATA_DIR` nor `HOME` is set",
        )
    })?;
    if cfg!(target_os = "macos") {
        return Ok(PathBuf::from(home).join("Library/Application Support/Feldspar"));
    }
    match std::env::var("XDG_DATA_HOME") {
        Ok(xdg) if !xdg.trim().is_empty() => Ok(PathBuf::from(xdg).join("feldspar")),
        _ => Ok(PathBuf::from(home).join(".local/share/feldspar")),
    }
}

/// Where suggested local-store directories live: `<data dir>/local-stores`,
/// beside the `git-stores` clones live in.
///
/// This is only ever a *suggestion*. A local store's directory is the admin's
/// own — it is usually somewhere that already exists and already has files in
/// it, which is why [`local_config_spec`](crate::local_config_spec) requires
/// them to name it. What this answers is the question an admin creating their
/// first store cannot: "and if I have nowhere in particular in mind?"
pub(crate) fn local_store_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("local-stores"))
}

/// The directory to offer for a new local store named `name`:
/// `<local store dir>/<name>`.
///
/// Named after the store, so an operator looking at the data directory can tell
/// which store a directory belongs to without consulting the database — the
/// same reason a clone is named after its store.
pub fn suggest_local_dir(name: &str) -> Result<PathBuf> {
    Ok(local_store_dir()?.join(path_safe(name)))
}

/// A store name reduced to something safe to use as a single path component.
///
/// A store's name is admin-supplied text and may hold anything — a slash, a
/// leading dot, a colon Windows will not take — while this becomes a directory
/// name. Every character outside `[A-Za-z0-9._-]` becomes `_`, and a name that
/// reduces to nothing (or to `.`/`..`) becomes `store`, since a clone directory
/// called `..` would be the parent of every clone.
pub fn path_safe(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    match mapped.trim_matches('.') {
        "" => "store".to_owned(),
        _ => mapped,
    }
}

/// A test-only guard for the process-wide data-directory variable, shared with
/// the git module's tests: `set_var` is process-wide, so two tests setting it at
/// once would each see the other's value.
#[cfg(test)]
pub(crate) mod testing {
    use super::DATA_DIR_ENV;

    /// Run `f` with [`DATA_DIR_ENV`] set to `dir` (or, with `None`, whatever it
    /// already was), restoring it afterwards, under a lock.
    pub(crate) fn temp_env(f: impl FnOnce()) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = std::env::var(DATA_DIR_ENV).ok();
        f();
        match before {
            Some(v) => set_data_dir(&v),
            None => restore_unset(),
        }
    }

    /// Set [`DATA_DIR_ENV`] for the test running inside [`temp_env`].
    #[allow(
        unsafe_code,
        clippy::disallowed_methods,
        reason = "tests of the variable itself; temp_env's lock keeps them one at a time"
    )]
    pub(crate) fn set_data_dir(dir: &str) {
        // SAFETY: only called inside `temp_env`, whose lock serialises the
        // tests that touch the variable; nothing else in this test binary
        // reads it concurrently.
        unsafe { std::env::set_var(DATA_DIR_ENV, dir) };
    }

    #[allow(
        unsafe_code,
        clippy::disallowed_methods,
        reason = "tests of the variable itself; temp_env's lock keeps them one at a time"
    )]
    fn restore_unset() {
        // SAFETY: as in `set_data_dir`.
        unsafe { std::env::remove_var(DATA_DIR_ENV) };
    }
}

#[cfg(test)]
mod tests {
    use super::testing::temp_env;
    use super::*;

    #[test]
    fn a_store_name_becomes_one_safe_path_component() {
        assert_eq!(path_safe("docs"), "docs");
        assert_eq!(path_safe("my docs"), "my_docs");
        // The cases that would escape the clone directory or name it. A dot
        // survives (it is legal in a directory name) but the separator does
        // not, so what is left is one harmless component.
        assert_eq!(path_safe("../etc"), ".._etc");
        assert_eq!(path_safe(".."), "store");
        assert_eq!(path_safe(""), "store");
        assert!(!path_safe("a/b").contains('/'));
    }

    /// A suggestion sits under the data directory, is named after the store,
    /// and is beside the clones rather than among them.
    #[test]
    fn a_suggested_directory_sits_under_the_data_directory() {
        temp_env(|| {
            crate::paths::testing::set_data_dir("/tmp/sc-data");
            assert_eq!(
                local_store_dir().unwrap(),
                PathBuf::from("/tmp/sc-data/local-stores")
            );
            assert_eq!(
                suggest_local_dir("Customer uploads").unwrap(),
                PathBuf::from("/tmp/sc-data/local-stores/Customer_uploads")
            );
            // A name that is no use as a path still yields one component
            // inside the directory rather than something that escapes it.
            assert_eq!(
                suggest_local_dir("../..").unwrap(),
                PathBuf::from("/tmp/sc-data/local-stores/.._..")
            );
        });
    }
}
