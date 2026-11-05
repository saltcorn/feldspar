//! File stores (layer 5): the [`FileStore`] trait and its drivers.
//!
//! A file store is one named directory or object store (technical design
//! §14.1). This crate owns the *contract* — [`FileStore`] plus its descriptors
//! [`Entry`] and [`FileMeta`] — and the [`LocalFileStore`] driver backing a
//! store with a local directory.
//!
//! Design invariants encoded here as types rather than prose:
//!
//! - **Files have no database row** (design §9). Per-file metadata is
//!   [`FileMeta`], round-tripped via [`FileStore::get_meta`] /
//!   [`FileStore::set_meta`]; for on-disk stores it is kept beside the bytes in
//!   cross-platform extended attributes (see [`xattr`]).
//! - **Paths are confined to the store root.** Every [`FileStore`] method takes
//!   a [`StorePath`], which only the one validator makes: `..`, `\`, `:`, NUL
//!   and Windows-trimmed segments are refused before any backend sees the path,
//!   and `.git` in any spelling is known (TODO-code-quality Q4.1).
//!
//! `unreachable_pub` (security review 2026-11, F-15): an item is `pub` only
//! when another crate calls it. A primitive that skips the guarded entry
//! points stays `pub(crate)`, so a handler elsewhere cannot reach past the
//! guard by importing it.
//!
//! Indexing, string slicing and unchecked arithmetic are warned about here
//! (security review 2026-11, §6; TODO-type-hardening H1.7) when built with
//! `--cfg feldspar_advisory_lints` (the CI `clippy-advisory` job): this crate
//! handles request input, where each is a panic a request can cause. Advisory
//! until the existing hits are fixed, so they do not fail `-D warnings`.
#![warn(unreachable_pub)]
#![cfg_attr(
    feldspar_advisory_lints,
    warn(
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects
    )
)]

mod access;
mod backend;
mod checked;
mod def;
mod disk;
mod git;
mod local;
mod paths;
mod reference;
mod search;
mod serve;
mod store;
mod store_path;
mod stream;
pub mod xattr;

pub use access::{
    FileCaller, VisibleEntry, check_access, check_write, effective_min_role, list_visible,
    record_owner,
};
pub use backend::{
    OperationOutcome, Relocation, backend_config_spec, backend_operations, connect_from_def,
    display_config, registered_backends, relocate_for_restore, run_backend_operation,
    validate_file_store_config,
};
pub use checked::{CheckedStore, Readable, Writable};
pub use def::{
    ATTR_CLONE_PATH, CFG_BRANCH, CFG_CREATE, CFG_DIR, CFG_KEY_PATH, CFG_PATH, CFG_PUBLIC_KEY,
    CFG_URL, FileStoreDef, FileStoreDefId, GIT_BACKEND, LOCAL_BACKEND,
};
pub use disk::{remove_store_from_disk, store_directory};
pub use git::{
    ARG_BRANCH, ARG_CREATE, ARG_MESSAGE, ARG_PATHS, ARG_STAGED_ONLY, CommitOutcome, DeployKey,
    GitChange, GitFileStore, GitOutput, GitRepo, GitStatus, OP_CHECKOUT, OP_CLONE, OP_COMMIT,
    OP_DISCARD, OP_GENERATE_KEY, OP_PULL, OP_PUSH, OP_STAGE, OP_STATUS, OP_UNSTAGE, clone_path,
    generate_deploy_key, parse_change, record_clone_path,
};
pub use local::{LocalFileStore, OP_SUGGEST_DIR};
pub use paths::{DATA_DIR_ENV, data_dir, suggest_local_dir};
pub use reference::{free_path, mime_allowed, mime_for_path, validate_file_path};
pub use search::{
    DEFAULT_EXCLUDED_DIRS, DEFAULT_MAX_FOUND, DEFAULT_MAX_RESULTS, FoundFiles, MAX_FILE_BYTES,
    MAX_FILES_SCANNED, MAX_LINE_CHARS, SearchHit, SearchOutcome, SearchQuery, find_by_extension,
    find_files, glob_matches, search_store, walk_store,
};
pub use serve::ServeFile;
pub use store::{CONTENT_TYPE_ATTRIBUTE, Entry, FILENAME_ATTRIBUTE, FileMeta, FileStat, FileStore};
pub use store_path::{AsStorePath, HostCode, StorePath, in_git_dir};
pub use stream::{BoxAsyncRead, BoxAsyncWrite, READ_CHUNK, read_chunks, read_to_bytes};

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sc_types::Role;

    /// Create a `LocalFileStore` rooted at a fresh unique temp directory.
    ///
    /// The counter is what makes it *unique*: the timestamp alone can repeat
    /// between two tests starting in the same clock tick on different threads,
    /// and two tests sharing a store root fail on each other's files — rarely,
    /// and therefore confusingly.
    fn temp_store() -> (std::path::PathBuf, LocalFileStore) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "sc-files-test-{}-{:?}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        let store = LocalFileStore::new("test", &base).unwrap();
        (base, store)
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let (_base, store) = temp_store();
        assert_eq!(store.name(), "test");
        store
            .write(
                &StorePath::from_static("a/b/hello.txt"),
                Bytes::from_static(b"hello world"),
            )
            .await
            .unwrap();
        let got = store
            .read(&StorePath::from_static("a/b/hello.txt"))
            .await
            .unwrap();
        assert_eq!(&got[..], b"hello world");
    }

    #[tokio::test]
    async fn stat_answers_the_entrys_own_facts_and_none_for_what_is_not_there() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("docs/a.txt"),
                Bytes::from_static(b"12345"),
            )
            .await
            .unwrap();

        let file = store
            .stat(&StorePath::from_static("docs/a.txt"))
            .await
            .unwrap()
            .expect("it is there");
        assert_eq!(file.size, 5);
        assert!(!file.is_dir);
        // A real modification time, in the shape every other timestamp crosses
        // the wire in.
        let modified = file.modified.expect("a local file has one");
        assert!(
            modified.contains('T') && modified.ends_with('Z'),
            "{modified}"
        );

        let dir = store
            .stat(&StorePath::from_static("docs"))
            .await
            .unwrap()
            .expect("it is there");
        assert!(dir.is_dir);
        assert_eq!(dir.size, 0, "a directory has no size to report");
        assert!(
            store
                .stat(&StorePath::root())
                .await
                .unwrap()
                .unwrap()
                .is_dir,
            "the root is"
        );

        // Nothing there is an answer rather than an error — which is what makes
        // `exists()` a question instead of a `catch`. Including the case where a
        // *file* is in the middle of the path.
        assert!(
            store
                .stat(&StorePath::from_static("docs/gone.txt"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .stat(&StorePath::from_static("nowhere/at/all"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .stat(&StorePath::from_static("docs/a.txt/deeper"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_default_stat_answers_from_a_listing() {
        /// A store with no `stat` of its own: everything else is the local
        /// store's, so what is under test is the trait's default — the answer a
        /// backend that can only list still gives.
        struct ListOnly(LocalFileStore);

        #[async_trait::async_trait]
        impl FileStore for ListOnly {
            fn name(&self) -> &str {
                self.0.name()
            }
            async fn read(&self, path: &StorePath) -> sc_error::Result<Bytes> {
                self.0.read(path).await
            }
            async fn write(&self, path: &StorePath, data: Bytes) -> sc_error::Result<()> {
                self.0.write(path, data).await
            }
            async fn list(&self, dir: &StorePath) -> sc_error::Result<Vec<Entry>> {
                self.0.list(dir).await
            }
            async fn mkdir(&self, path: &StorePath) -> sc_error::Result<()> {
                self.0.mkdir(path).await
            }
            async fn delete(&self, path: &StorePath) -> sc_error::Result<bool> {
                self.0.delete(path).await
            }
            async fn rename(&self, from: &StorePath, to: &StorePath) -> sc_error::Result<()> {
                self.0.rename(from, to).await
            }
            fn is_git_repo(&self) -> bool {
                false
            }
            async fn get_meta(&self, path: &StorePath) -> sc_error::Result<FileMeta> {
                self.0.get_meta(path).await
            }
            async fn set_meta(&self, path: &StorePath, meta: &FileMeta) -> sc_error::Result<()> {
                self.0.set_meta(path, meta).await
            }
        }

        let (_base, inner) = temp_store();
        let store = ListOnly(inner);
        store
            .write(
                &StorePath::from_static("docs/a.txt"),
                Bytes::from_static(b"123"),
            )
            .await
            .unwrap();

        let file = store
            .stat(&StorePath::from_static("docs/a.txt"))
            .await
            .unwrap()
            .expect("listed");
        assert_eq!(file.size, 3);
        assert!(!file.is_dir);
        // The modification time now comes from the listing too — a backend
        // whose `list` reports one gets it through the default `stat` without
        // implementing anything. `None` stays the honest answer for a backend
        // that does not record it, rather than an invented time.
        assert_eq!(
            file.modified,
            store
                .0
                .stat(&StorePath::from_static("docs/a.txt"))
                .await
                .unwrap()
                .unwrap()
                .modified
        );
        assert!(file.modified.is_some());
        assert!(
            store
                .stat(&StorePath::from_static("docs"))
                .await
                .unwrap()
                .unwrap()
                .is_dir
        );
        assert!(
            store
                .stat(&StorePath::root())
                .await
                .unwrap()
                .unwrap()
                .is_dir
        );
        assert!(
            store
                .stat(&StorePath::from_static("docs/gone.txt"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .stat(&StorePath::from_static("nowhere/at/all"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_returns_sorted_children_with_sizes() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("z.txt"),
                Bytes::from_static(b"12345"),
            )
            .await
            .unwrap();
        store
            .write(&StorePath::from_static("a.txt"), Bytes::from_static(b"x"))
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("sub/c.txt"),
                Bytes::from_static(b"y"),
            )
            .await
            .unwrap();

        let root = store.list(&StorePath::root()).await.unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "sub", "z.txt"]);

        let a = &root[0];
        assert!(!a.is_dir);
        assert_eq!(a.path, "a.txt");
        assert_eq!(a.size, Some(1));

        let sub = &root[1];
        assert!(sub.is_dir);
        assert_eq!(sub.size, None);

        let z = &root[2];
        assert_eq!(z.size, Some(5));

        // Listing a sub-directory yields paths relative to the root.
        let children = store.list(&StorePath::from_static("sub")).await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].path, "sub/c.txt");
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let (_base, store) = temp_store();
        // Refused by the one validator, before any backend is reached.
        let err = StorePath::parse("../secret").unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
        let err = StorePath::parse("a/../../b").unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
        // The root is the store, whichever way it is spelled.
        store
            .write(
                &StorePath::from_static("in-root.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        assert_eq!(
            &store
                .read(&StorePath::from_static("/./in-root.txt"))
                .await
                .unwrap()[..],
            b"x"
        );
    }

    /// Security review #13: on Windows `PathBuf::push` reads `\` as a separator
    /// and `C:` as a drive, so a segment holding either is refused on every
    /// platform rather than pushed — as is one Windows trims to `..`.
    #[tokio::test]
    async fn windows_separators_and_drives_are_rejected() {
        let (_base, store) = temp_store();
        for bad in [
            "uploads/..\\..\\..\\Windows\\win.ini",
            "uploads/C:\\x",
            "C:",
            "uploads/a.txt:stream",
            "uploads/.. ",
            "uploads/...",
            "uploads/a\0b",
        ] {
            // No backend is ever handed one: there is no `StorePath` to hand.
            let err = StorePath::parse(bad).unwrap_err();
            assert!(
                matches!(err.repr(), sc_error::Repr::Invalid(_)),
                "{bad:?}: {err}"
            );
        }
        // Dots inside a name are still a name.
        store
            .write(
                &StorePath::from_static("a..b/c.d.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        assert_eq!(
            &store
                .read(&StorePath::from_static("a..b/c.d.txt"))
                .await
                .unwrap()[..],
            b"x"
        );
    }

    /// Security review #12: a symlink out of the store (`uploads -> /etc`,
    /// committed upstream) is not followed for reads or writes, nor is a
    /// dangling one written through; a link that stays inside is fine, and an
    /// escaping link can still be deleted.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_the_store_are_not_followed() {
        let (base, store) = temp_store();
        let outside = base.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "not yours").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("uploads")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), base.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("new.txt"), base.join("dangling.txt")).unwrap();
        store
            .write(
                &StorePath::from_static("inside/real.txt"),
                Bytes::from_static(b"ok"),
            )
            .await
            .unwrap();
        std::os::unix::fs::symlink(base.join("inside"), base.join("alias")).unwrap();

        for path in ["uploads/secret.txt", "leak.txt"] {
            let err = store
                .read(&StorePath::parse(path).unwrap())
                .await
                .unwrap_err();
            assert!(
                matches!(err.repr(), sc_error::Repr::Invalid(_)),
                "{path}: {err}"
            );
            assert!(
                store
                    .open_read(&StorePath::parse(path).unwrap(), None)
                    .await
                    .is_err(),
                "{path}"
            );
            assert!(
                store.stat(&StorePath::parse(path).unwrap()).await.is_err(),
                "{path}"
            );
        }
        assert!(
            store
                .list(&StorePath::from_static("uploads"))
                .await
                .is_err()
        );
        for path in [
            "uploads/new.txt",
            "uploads/sub/new.txt",
            "dangling.txt",
            "leak.txt",
        ] {
            assert!(
                store
                    .write(&StorePath::parse(path).unwrap(), Bytes::from_static(b"x"))
                    .await
                    .is_err(),
                "{path}"
            );
        }
        assert!(!outside.join("new.txt").exists());
        assert!(!outside.join("sub").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
            "not yours"
        );

        // Inside the store, a link is just another way to the same file.
        assert_eq!(
            &store
                .read(&StorePath::from_static("alias/real.txt"))
                .await
                .unwrap()[..],
            b"ok"
        );
        // And the escaping link itself can be removed, without touching its target.
        assert!(
            store
                .delete(&StorePath::from_static("uploads"))
                .await
                .unwrap()
        );
        assert!(outside.join("secret.txt").exists());

        std::fs::remove_dir_all(&outside).ok();
    }

    /// Security review #16: below admin, the files that decide what a build runs
    /// are not writable on a public store; source beside them still is.
    #[tokio::test]
    async fn build_inputs_are_admin_only_writes() {
        let (_base, store) = temp_store();
        let staff = FileCaller::of_principal(Role::of(40), Some("u1".into()));
        for path in [
            "web/package.json",
            "web/Package.JSON",
            "package-lock.json",
            "web/yarn.lock",
            "web/.npmrc",
            "web/vite.config.js",
            "site/astro.config.mjs",
            "web/vite.config.prod.ts",
            "web/.postcssrc.cjs",
            "web/node_modules/.bin/typescript-language-server",
            "web/node_modules",
            "web/NODE_MODULES./x/index.js",
        ] {
            assert!(
                matches!(
                    StorePath::parse(path).unwrap().host_code_kind(),
                    Some(HostCode::PackageManager | HostCode::BuildConfig)
                ),
                "{path}"
            );
            assert!(
                check_write(&store, None, path, &staff).await.is_err(),
                "{path}"
            );
            assert!(
                check_write(&store, None, path, &FileCaller::at_role(Role::ADMIN))
                    .await
                    .is_ok(),
                "{path}"
            );
        }
        for path in [
            "web/src/App.tsx",
            "web/config.js",
            "web/src/config/index.ts",
            "docs/package.json.md",
            "web/README.md",
        ] {
            assert_eq!(
                StorePath::parse(path).unwrap().host_code_kind(),
                None,
                "{path}"
            );
            assert!(
                check_write(&store, None, path, &staff).await.is_ok(),
                "{path}"
            );
        }
    }

    /// Claude Code's project configuration is written by an admin only: the
    /// admin's terminal starts in a store and runs its hooks as the server's
    /// user (security review 2026-11, #2).
    #[tokio::test]
    async fn coding_agent_config_is_an_admin_only_write() {
        let (_base, store) = temp_store();
        let staff = FileCaller::of_principal(Role::of(40), Some("u1".into()));
        for path in [
            ".claude/settings.json",
            "web/.claude/settings.json",
            "web/.claude/settings.local.json",
            "web/.Claude./commands/x.md",
            "CLAUDE.md",
            "web/claude.md",
            "web/CLAUDE.local.md",
            "web/.claude",
            "web/.mcp.json",
        ] {
            assert_eq!(
                StorePath::parse(path).unwrap().host_code_kind(),
                Some(HostCode::AgentConfig),
                "{path}"
            );
            let err = check_write(&store, None, path, &staff).await.unwrap_err();
            assert!(err.to_string().contains("coding agent"), "{path}: {err}");
            assert!(
                check_write(&store, None, path, &FileCaller::at_role(Role::ADMIN))
                    .await
                    .is_ok(),
                "{path}"
            );
        }
        for path in [
            "web/claude.txt",
            "docs/CLAUDE.md.bak",
            "web/src/claude/x.ts",
            // Instructions the coding agents keep themselves (`HostCode` docs).
            "web/AGENTS.md",
        ] {
            assert_eq!(
                StorePath::parse(path).unwrap().host_code_kind(),
                None,
                "{path}"
            );
            assert!(
                check_write(&store, None, path, &staff).await.is_ok(),
                "{path}"
            );
        }
    }

    /// Security review 2026-11 C1 (TODO-type-hardening H4.3): below admin, no
    /// write lays out a bare repository git would discover — a `HEAD` beside
    /// `objects/` and `refs/` — nor reaches into a `.git`; ordinary files
    /// named `config`, `objects/…` and `refs/…`, and a `head` directory, stay
    /// writable.
    #[tokio::test]
    async fn git_internals_are_admin_only_writes() {
        let (_base, store) = temp_store();
        let staff = FileCaller::of_principal(Role::of(40), Some("u1".into()));
        for path in [
            "web/HEAD",
            "HEAD",
            "web/.git/config",
            "web/.git",
            "a/GIT~1/HEAD",
        ] {
            assert_eq!(
                StorePath::parse(path).unwrap().host_code_kind(),
                Some(HostCode::GitInternals),
                "{path}"
            );
            let err = check_write(&store, None, path, &staff).await.unwrap_err();
            assert!(
                err.to_string().contains("which repository git runs"),
                "{path}: {err}"
            );
            assert!(
                check_write(&store, None, path, &FileCaller::at_role(Role::ADMIN))
                    .await
                    .is_ok(),
                "{path}"
            );
        }
        // On Linux only the exact spelling; elsewhere `head` is `HEAD` to git.
        let lower = if cfg!(any(target_os = "macos", windows)) {
            Some(HostCode::GitInternals)
        } else {
            None
        };
        assert_eq!(
            StorePath::parse("web/head").unwrap().host_code_kind(),
            lower
        );
        for path in [
            "web/config",
            "web/objects/a.png",
            "web/refs/x",
            "web/src/components/head/Meta.astro",
            "web/HEAD.md",
        ] {
            assert_eq!(
                StorePath::parse(path).unwrap().host_code_kind(),
                None,
                "{path}"
            );
            assert!(
                check_write(&store, None, path, &staff).await.is_ok(),
                "{path}"
            );
        }
    }

    /// Security review #12: a checkout's `.git` is not readable or listed below
    /// admin, whatever the floors on the store say.
    #[tokio::test]
    async fn a_git_dir_is_admin_only() {
        let (base, _) = temp_store();
        let store = LocalFileStore::new("test", &base)
            .unwrap()
            .writing_git_dir();
        store
            .write(
                &StorePath::from_static(".git/config"),
                Bytes::from_static(b"[remote]"),
            )
            .await
            .unwrap();
        store
            .write(&StorePath::from_static("a.txt"), Bytes::from_static(b"a"))
            .await
            .unwrap();
        let staff = FileCaller::of_principal(Role::of(40), Some("u1".into()));
        for path in [".git/config", ".git", ".GIT/config"] {
            assert!(
                check_access(&store, None, path, &staff).await.is_err(),
                "{path}"
            );
            assert!(
                check_access(&store, None, path, &FileCaller::anonymous())
                    .await
                    .is_err()
            );
        }
        assert!(
            check_access(
                &store,
                None,
                ".git/config",
                &FileCaller::at_role(Role::ADMIN)
            )
            .await
            .is_ok()
        );
        let names = |v: Vec<VisibleEntry>| v.into_iter().map(|e| e.entry.name).collect::<Vec<_>>();
        assert_eq!(
            names(list_visible(&store, None, "", &staff).await.unwrap()),
            ["a.txt"]
        );
        assert_eq!(
            names(
                list_visible(&store, None, "", &FileCaller::at_role(Role::ADMIN))
                    .await
                    .unwrap()
            ),
            [".git", "a.txt"]
        );
    }

    #[tokio::test]
    async fn meta_round_trips_and_defaults() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("doc.md"),
                Bytes::from_static(b"# hi"),
            )
            .await
            .unwrap();

        // No metadata set yet → defaults.
        let meta = store
            .get_meta(&StorePath::from_static("doc.md"))
            .await
            .unwrap();
        assert_eq!(meta, FileMeta::default());

        let mut m = FileMeta {
            min_role: Some(Role::of(20)),
            ..Default::default()
        };
        m.attributes.insert("mime".into(), "text/markdown".into());
        store
            .set_meta(&StorePath::from_static("doc.md"), &m)
            .await
            .unwrap();

        let got = store
            .get_meta(&StorePath::from_static("doc.md"))
            .await
            .unwrap();
        assert_eq!(got, m);

        // Metadata lives in an extended attribute, so it never shows as a file.
        let names: Vec<_> = store
            .list(&StorePath::root())
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["doc.md"]);
    }

    #[tokio::test]
    async fn set_meta_requires_the_file_to_exist() {
        let (_base, store) = temp_store();
        let err = store
            .set_meta(&StorePath::from_static("missing.txt"), &FileMeta::default())
            .await
            .unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)));
    }

    #[tokio::test]
    async fn is_git_repo_detects_dot_git() {
        let (base, store) = temp_store();
        assert!(!store.is_git_repo());
        std::fs::create_dir(base.join(".git")).unwrap();
        assert!(store.is_git_repo());
    }

    /// `.git/config` is commands the server's own `git` runs (`core.fsmonitor`
    /// on every status), so no write through a store may land in `.git` — in
    /// any case, at any depth, by any write path — except the restore of a
    /// backup's own checkout.
    #[tokio::test]
    async fn nothing_is_written_inside_dot_git() {
        let (base, store) = temp_store();
        std::fs::create_dir(base.join(".git")).unwrap();
        std::fs::write(base.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(base.join("ok.txt"), "x").unwrap();
        let evil = Bytes::from_static(b"[core]\n\tfsmonitor = touch /tmp/owned\n");
        for path in [
            ".git/config",
            ".GIT/config",
            ".git./config",
            "sub/.git/hooks/pre-commit",
            "GIT~1/config",
        ] {
            let path = StorePath::parse(path).unwrap();
            assert!(path.in_git_dir(), "{path}");
            assert!(store.write(&path, evil.clone()).await.is_err(), "{path}");
        }
        // A backslash spelling never becomes a path at all.
        assert!(StorePath::parse("a\\.git\\config").is_err());
        assert!(
            store
                .mkdir(&StorePath::from_static(".git/hooks"))
                .await
                .is_err()
        );
        assert!(
            store
                .delete(&StorePath::from_static(".git/config"))
                .await
                .is_err()
        );
        assert!(
            store
                .rename(
                    &StorePath::from_static("ok.txt"),
                    &StorePath::from_static(".git/config")
                )
                .await
                .is_err()
        );
        assert!(
            store
                .open_append(&StorePath::from_static(".git/config"))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(base.join(".git/config")).unwrap(),
            "[core]\n",
            "the repository's config is untouched"
        );
        // Reading is not writing, and a file merely *named* like it is fine.
        assert!(
            store
                .read(&StorePath::from_static(".git/config"))
                .await
                .is_ok()
        );
        store
            .write(
                &StorePath::from_static("docs/.gitignore"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("dot.git/x"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();

        let restoring = LocalFileStore::new("t", &base).unwrap().writing_git_dir();
        restoring
            .write(&StorePath::from_static(".git/config"), evil)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mkdir_is_idempotent_but_refuses_to_shadow_a_file() {
        let (_base, store) = temp_store();

        store.mkdir(&StorePath::from_static("a/b/c")).await.unwrap();
        let entries = store.list(&StorePath::from_static("a/b")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_dir);

        // Asking again for a directory that is already there is not an error —
        // the caller wanted one there and there is one.
        store.mkdir(&StorePath::from_static("a/b/c")).await.unwrap();

        // But a file already occupying the name is a real conflict.
        store
            .write(
                &StorePath::from_static("a/file.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        let err = store
            .mkdir(&StorePath::from_static("a/file.txt"))
            .await
            .unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
    }

    #[tokio::test]
    async fn delete_reports_whether_anything_was_there() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("dir/file.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();

        assert!(
            store
                .delete(&StorePath::from_static("dir/file.txt"))
                .await
                .unwrap()
        );
        // Already gone is `false`, not an error: a caller would otherwise have
        // to race an existence check against the delete.
        assert!(
            !store
                .delete(&StorePath::from_static("dir/file.txt"))
                .await
                .unwrap()
        );

        // A directory goes with everything in it.
        store
            .write(
                &StorePath::from_static("dir/nested/deep.txt"),
                Bytes::from_static(b"y"),
            )
            .await
            .unwrap();
        assert!(store.delete(&StorePath::from_static("dir")).await.unwrap());
        assert!(store.list(&StorePath::root()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_refuses_the_store_root() {
        let (base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("keep.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();

        // Every spelling of "the root" is refused — a store's root *is* the
        // store, and removing it would leave a definition pointing at nothing.
        for root in ["", "/", "."] {
            let err = store
                .delete(&StorePath::parse(root).unwrap())
                .await
                .unwrap_err();
            assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)), "{root:?}");
        }
        assert!(base.is_dir());
        assert_eq!(store.list(&StorePath::root()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rename_moves_within_the_store_and_never_clobbers() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("a/one.txt"),
                Bytes::from_static(b"first"),
            )
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("b/two.txt"),
                Bytes::from_static(b"second"),
            )
            .await
            .unwrap();

        // Moving into a directory that does not exist yet creates it.
        store
            .rename(
                &StorePath::from_static("a/one.txt"),
                &StorePath::from_static("c/moved.txt"),
            )
            .await
            .unwrap();
        assert_eq!(
            &store
                .read(&StorePath::from_static("c/moved.txt"))
                .await
                .unwrap()[..],
            b"first"
        );
        assert!(
            store
                .list(&StorePath::from_static("a"))
                .await
                .unwrap()
                .is_empty()
        );

        // Refusing to overwrite is the important part: a silent replace on a
        // file manager's drag-and-drop is a lost file with no undo.
        let err = store
            .rename(
                &StorePath::from_static("b/two.txt"),
                &StorePath::from_static("c/moved.txt"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
        assert_eq!(
            &store
                .read(&StorePath::from_static("c/moved.txt"))
                .await
                .unwrap()[..],
            b"first"
        );
        assert_eq!(
            &store
                .read(&StorePath::from_static("b/two.txt"))
                .await
                .unwrap()[..],
            b"second"
        );

        // A source that is not there is a not-found, not a silent success.
        let err = store
            .rename(
                &StorePath::from_static("nope.txt"),
                &StorePath::from_static("x.txt"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)));

        // And traversal is rejected on both sides, so this cannot move data out
        // of the store.
        assert!(StorePath::parse("../escaped.txt").is_err());
        assert!(StorePath::parse("../secret").is_err());
    }

    /// The path-cumulative rule `FileMeta` has documented since the MVP, now
    /// actually enforced: a directory's restriction covers everything beneath it.
    #[tokio::test]
    async fn access_is_cumulative_along_the_path() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("public/open.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("private/secret.txt"),
                Bytes::from_static(b"y"),
            )
            .await
            .unwrap();

        // Restrict the *directory* only; the file inside carries no rule.
        store
            .mkdir(&StorePath::from_static("private"))
            .await
            .unwrap();
        store
            .set_meta(
                &StorePath::from_static("private"),
                &FileMeta {
                    min_role: Some(Role::ADMIN),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // The file inherits the directory's restriction even though nothing was
        // set on the file. This is the whole point of the rule.
        assert_eq!(
            effective_min_role(&store, None, "private/secret.txt")
                .await
                .unwrap(),
            Some(Role::ADMIN)
        );
        // An admin (role 1) clears it; a public caller (100) does not.
        assert!(
            check_access(
                &store,
                None,
                "private/secret.txt",
                &FileCaller::at_role(Role::ADMIN)
            )
            .await
            .is_ok()
        );
        assert!(
            check_access(
                &store,
                None,
                "private/secret.txt",
                &FileCaller::at_role(Role::PUBLIC)
            )
            .await
            .is_err()
        );
        // An unrelated path is unaffected.
        assert!(
            check_access(&store, None, "public/open.txt", &FileCaller::anonymous())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_nested_rule_can_only_tighten_and_the_store_floor_applies_first() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("dir/file.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        store.mkdir(&StorePath::from_static("dir")).await.unwrap();

        // A permissive rule inside a restricted directory must NOT widen access:
        // otherwise restricting a folder would not restrict it, since anything
        // inside could opt back out.
        store
            .set_meta(
                &StorePath::from_static("dir"),
                &FileMeta {
                    min_role: Some(Role::of(20)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .set_meta(
                &StorePath::from_static("dir/file.txt"),
                &FileMeta {
                    min_role: Some(Role::PUBLIC),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            effective_min_role(&store, None, "dir/file.txt")
                .await
                .unwrap(),
            Some(Role::of(20)),
            "the more restrictive rule on the path must win"
        );

        // The store-wide floor is simply the outermost entry on the path, and
        // composes the same way.
        assert_eq!(
            effective_min_role(&store, Some(Role::of(10)), "dir/file.txt")
                .await
                .unwrap(),
            Some(Role::of(10))
        );
        // A laxer store floor does not loosen a tighter nested rule.
        assert_eq!(
            effective_min_role(&store, Some(Role::of(80)), "dir/file.txt")
                .await
                .unwrap(),
            Some(Role::of(20))
        );
    }

    #[tokio::test]
    async fn listings_hide_entries_the_caller_may_not_open() {
        let (_base, store) = temp_store();
        store
            .write(
                &StorePath::from_static("open.txt"),
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("secret.txt"),
                Bytes::from_static(b"y"),
            )
            .await
            .unwrap();
        store
            .set_meta(
                &StorePath::from_static("secret.txt"),
                &FileMeta {
                    min_role: Some(Role::ADMIN),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let all = store.list(&StorePath::root()).await.unwrap();
        assert_eq!(all.len(), 2);

        // A listing must be filtered rather than merely refused: showing a name
        // the caller cannot open leaks exactly what the rule was set to hide.
        let visible = access::filter_visible(&store, None, all.clone(), &FileCaller::anonymous())
            .await
            .unwrap();
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["open.txt"]);

        // An admin sees everything.
        let admin = access::filter_visible(&store, None, all, &FileCaller::at_role(Role::ADMIN))
            .await
            .unwrap();
        assert_eq!(admin.len(), 2);
    }

    /// The owner rule (TODO F2.1, §2.1): an admin-only file is readable by its
    /// owner, writable only by its owner, and listed to its owner inside a folder
    /// they could not otherwise read.
    #[tokio::test]
    async fn the_owner_may_read_and_write_what_nobody_else_at_their_role_may() {
        let (_base, store) = temp_store();
        store.mkdir(&StorePath::from_static("drops")).await.unwrap();
        store
            .set_meta(
                &StorePath::from_static("drops"),
                &FileMeta {
                    min_role: Some(Role::ADMIN),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for (path, owner) in [("drops/a.png", "user-a"), ("drops/b.png", "user-b")] {
            store
                .write(&StorePath::parse(path).unwrap(), Bytes::from_static(b"x"))
                .await
                .unwrap();
            record_owner(&store, path, owner).await;
        }
        let a = FileCaller::of_principal(Role::of(80), Some("user-a".into()));
        let b = FileCaller::of_principal(Role::of(80), Some("user-b".into()));
        let anonymous = FileCaller::anonymous();

        assert!(check_access(&store, None, "drops/a.png", &a).await.is_ok());
        assert!(check_access(&store, None, "drops/a.png", &b).await.is_err());
        assert!(
            check_access(&store, None, "drops/a.png", &anonymous)
                .await
                .is_err()
        );
        // The folder itself is not A's, so reading it is still refused.
        assert!(check_access(&store, None, "drops", &a).await.is_err());

        // Writing: the owner, and admin authority; not B, though B's role is
        // the same as A's.
        assert!(check_write(&store, None, "drops/a.png", &a).await.is_ok());
        assert!(check_write(&store, None, "drops/a.png", &b).await.is_err());
        assert!(
            check_write(
                &store,
                None,
                "drops/a.png",
                &FileCaller::at_role(Role::ADMIN)
            )
            .await
            .is_ok()
        );
        // An owned file is the owner's even where the floor is public.
        store
            .write(
                &StorePath::from_static("open.txt"),
                Bytes::from_static(b"o"),
            )
            .await
            .unwrap();
        record_owner(&store, "open.txt", "user-a").await;
        assert!(check_access(&store, None, "open.txt", &b).await.is_ok());
        assert!(check_write(&store, None, "open.txt", &b).await.is_err());
        // An unowned file keeps the path rule for writes.
        store
            .write(
                &StorePath::from_static("shared.txt"),
                Bytes::from_static(b"s"),
            )
            .await
            .unwrap();
        assert!(check_write(&store, None, "shared.txt", &b).await.is_ok());
        // A folder holding somebody else's file is theirs to keep: deleting it
        // would delete the file.
        store
            .write(
                &StorePath::from_static("mixed/mine.txt"),
                Bytes::from_static(b"m"),
            )
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("mixed/theirs.txt"),
                Bytes::from_static(b"t"),
            )
            .await
            .unwrap();
        record_owner(&store, "mixed/theirs.txt", "user-a").await;
        assert!(check_write(&store, None, "mixed", &b).await.is_err());
        assert!(check_write(&store, None, "mixed", &a).await.is_ok());

        // Listings: each user sees their own upload in the private folder, and
        // a caller who owns nothing there is refused as before.
        let names = |v: Vec<VisibleEntry>| v.into_iter().map(|e| e.entry.name).collect::<Vec<_>>();
        assert_eq!(
            names(list_visible(&store, None, "drops", &a).await.unwrap()),
            ["a.png"]
        );
        assert_eq!(
            names(list_visible(&store, None, "drops", &b).await.unwrap()),
            ["b.png"]
        );
        let c = FileCaller::of_principal(Role::of(80), Some("user-c".into()));
        assert!(list_visible(&store, None, "drops", &c).await.is_err());
        assert!(
            list_visible(&store, None, "drops", &anonymous)
                .await
                .is_err()
        );
        assert!(list_visible(&store, None, "nowhere", &c).await.is_err());
        assert_eq!(
            list_visible(&store, None, "drops", &FileCaller::at_role(Role::ADMIN))
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn recording_an_owner_keeps_the_first_one() {
        let (_base, store) = temp_store();
        store
            .write(&StorePath::from_static("f.txt"), Bytes::from_static(b"x"))
            .await
            .unwrap();
        record_owner(&store, "f.txt", "first").await;
        record_owner(&store, "f.txt", "second").await;
        assert_eq!(
            store
                .get_meta(&StorePath::from_static("f.txt"))
                .await
                .unwrap()
                .owner
                .as_deref(),
            Some("first")
        );
        // Two callers without accounts do not "own" an unowned file.
        assert!(!FileCaller::anonymous().owns(&FileMeta::default()));
    }

    // --- streaming (F2.4) ----------------------------------------------------

    /// A store with only the required methods, so every streaming call takes
    /// the trait's default — the shape a module driver has.
    struct MemStore(std::sync::Mutex<std::collections::BTreeMap<String, Bytes>>);

    #[async_trait::async_trait]
    impl FileStore for MemStore {
        fn name(&self) -> &str {
            "mem"
        }
        async fn read(&self, path: &StorePath) -> sc_error::Result<Bytes> {
            self.0
                .lock()
                .unwrap()
                .get(path.as_str())
                .cloned()
                .ok_or_else(|| sc_error::Error::not_found(path.to_string()))
        }
        async fn write(&self, path: &StorePath, data: Bytes) -> sc_error::Result<()> {
            self.0.lock().unwrap().insert(path.to_string(), data);
            Ok(())
        }
        async fn list(&self, _dir: &StorePath) -> sc_error::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        async fn mkdir(&self, _path: &StorePath) -> sc_error::Result<()> {
            Ok(())
        }
        async fn delete(&self, path: &StorePath) -> sc_error::Result<bool> {
            Ok(self.0.lock().unwrap().remove(path.as_str()).is_some())
        }
        async fn rename(&self, _from: &StorePath, _to: &StorePath) -> sc_error::Result<()> {
            Ok(())
        }
        fn is_git_repo(&self) -> bool {
            false
        }
        async fn get_meta(&self, _path: &StorePath) -> sc_error::Result<FileMeta> {
            Ok(FileMeta::default())
        }
        async fn set_meta(&self, _path: &StorePath, _meta: &FileMeta) -> sc_error::Result<()> {
            Ok(())
        }
    }

    /// What every driver must do with `open_read`, `open_append` and `concat`,
    /// asserted the same way for each.
    async fn streaming_contract(store: &dyn FileStore) {
        use tokio::io::AsyncWriteExt;
        store
            .write(
                &StorePath::from_static("in/a.bin"),
                Bytes::from_static(b"0123456789"),
            )
            .await
            .unwrap();
        // Whole, and ranged (half-open), and the empty range at the end.
        let all = read_to_bytes(
            store
                .open_read(&StorePath::from_static("in/a.bin"), None)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(&all[..], b"0123456789");
        let some = read_to_bytes(
            store
                .open_read(&StorePath::from_static("in/a.bin"), Some(2..5))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(&some[..], b"234");
        let none = read_to_bytes(
            store
                .open_read(&StorePath::from_static("in/a.bin"), Some(10..10))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(none.is_empty());
        // Past the end is refused, not truncated; a missing file is not found.
        assert!(
            store
                .open_read(&StorePath::from_static("in/a.bin"), Some(5..11))
                .await
                .is_err()
        );
        let err = store
            .open_read(&StorePath::from_static("in/nope.bin"), None)
            .await
            .err()
            .unwrap();
        assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)), "{err}");

        // Append creates, then appends across two openings.
        for chunk in [&b"abc"[..], b"def"] {
            let mut out = store
                .open_append(&StorePath::from_static("up/new.bin"))
                .await
                .unwrap();
            out.write_all(chunk).await.unwrap();
            out.shutdown().await.unwrap();
        }
        assert_eq!(
            &store
                .read(&StorePath::from_static("up/new.bin"))
                .await
                .unwrap()[..],
            b"abcdef"
        );

        // Concat joins in order and answers the size.
        store
            .write(
                &StorePath::from_static("parts/1"),
                Bytes::from_static(b"one-"),
            )
            .await
            .unwrap();
        store
            .write(
                &StorePath::from_static("parts/2"),
                Bytes::from_static(b"two"),
            )
            .await
            .unwrap();
        let parts = [
            StorePath::from_static("parts/1"),
            StorePath::from_static("parts/2"),
        ];
        assert_eq!(
            store
                .concat(&parts, &StorePath::from_static("joined/out.txt"))
                .await
                .unwrap(),
            7
        );
        assert_eq!(
            &store
                .read(&StorePath::from_static("joined/out.txt"))
                .await
                .unwrap()[..],
            b"one-two"
        );
        // A missing part fails without leaving a partial target: neither a new
        // one nor a change to one that was there.
        let broken = [
            StorePath::from_static("parts/1"),
            StorePath::from_static("parts/missing"),
        ];
        assert!(
            store
                .concat(&broken, &StorePath::from_static("joined/partial.txt"))
                .await
                .is_err()
        );
        assert!(
            store
                .read(&StorePath::from_static("joined/partial.txt"))
                .await
                .is_err()
        );
        assert!(
            store
                .concat(&broken, &StorePath::from_static("joined/out.txt"))
                .await
                .is_err()
        );
        assert_eq!(
            &store
                .read(&StorePath::from_static("joined/out.txt"))
                .await
                .unwrap()[..],
            b"one-two"
        );
    }

    #[tokio::test]
    async fn the_local_driver_streams_appends_and_joins() {
        let (base, store) = temp_store();
        streaming_contract(&store).await;
        // The join left no temporary beside its target.
        let leftovers: Vec<_> = std::fs::read_dir(base.join("joined"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, ["out.txt"]);
        // A directory is not a file to stream.
        store.mkdir(&StorePath::from_static("adir")).await.unwrap();
        assert!(
            store
                .open_read(&StorePath::from_static("adir"), None)
                .await
                .is_err()
        );
        // A replaced join keeps the target's metadata, owner included.
        record_owner(&store, "joined/out.txt", "user-a").await;
        let parts = [StorePath::from_static("parts/2")];
        store
            .concat(&parts, &StorePath::from_static("joined/out.txt"))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_meta(&StorePath::from_static("joined/out.txt"))
                .await
                .unwrap()
                .owner
                .as_deref(),
            Some("user-a")
        );
    }

    #[tokio::test]
    async fn a_driver_with_only_read_and_write_gets_working_defaults() {
        let store = MemStore(Default::default());
        streaming_contract(&store).await;
        // The default append writes nothing until it is shut down.
        use tokio::io::AsyncWriteExt;
        let mut out = store
            .open_append(&StorePath::from_static("late.bin"))
            .await
            .unwrap();
        out.write_all(b"x").await.unwrap();
        assert!(
            store
                .read(&StorePath::from_static("late.bin"))
                .await
                .is_err()
        );
        out.shutdown().await.unwrap();
        assert_eq!(
            &store
                .read(&StorePath::from_static("late.bin"))
                .await
                .unwrap()[..],
            b"x"
        );
    }

    // --- the store-side search ----------------------------------------------

    /// A small project to search: two source files, one binary, and a
    /// dependency directory that must not be walked.
    async fn searchable() -> (std::path::PathBuf, LocalFileStore) {
        let (base, store) = temp_store();
        for (path, bytes) in [
            ("src/app.ts", &b"export function todo() {}\n"[..]),
            ("src/deep/list.tsx", b"// TODO: paginate\nconst n = 1;\n"),
            ("readme.md", b"nothing to see\n"),
            ("node_modules/pkg/index.js", b"todo everywhere\n"),
        ] {
            store
                .write(
                    &StorePath::parse(path).unwrap(),
                    Bytes::copy_from_slice(bytes),
                )
                .await
                .unwrap();
        }
        // Not UTF-8: it has no lines, and "matched at line 3" of a PNG is noise.
        store
            .write(
                &StorePath::from_static("logo.png"),
                Bytes::from_static(&[0xff, 0xfe, b't', b'o', 0x00]),
            )
            .await
            .unwrap();
        (base, store)
    }

    #[tokio::test]
    async fn a_search_walks_the_tree_and_skips_what_it_should() {
        let (_base, store) = searchable().await;
        let found = search_store(
            &store,
            None,
            &FileCaller::at_role(Role::ADMIN),
            &SearchQuery::literal("todo"),
        )
        .await
        .unwrap();
        let mut paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        paths.sort();
        // `node_modules` is not descended into and the binary is not scanned, so
        // a store with a dependency tree in it is still searchable.
        assert_eq!(paths, ["src/app.ts", "src/deep/list.tsx"]);
        assert!(!found.truncated);

        // Line and column are 1-based, and the line comes back whole.
        let hit = found
            .hits
            .iter()
            .find(|h| h.path == "src/app.ts")
            .expect("the source file");
        assert_eq!(hit.line, 1);
        assert_eq!(hit.column, 17);
        assert_eq!(hit.text, "export function todo() {}");
    }

    #[tokio::test]
    async fn a_search_can_be_narrowed_by_directory_and_by_glob() {
        let (_base, store) = searchable().await;

        let mut query = SearchQuery::literal("todo");
        query.dir = "src/deep".to_owned();
        let found = search_store(&store, None, &FileCaller::at_role(Role::ADMIN), &query)
            .await
            .unwrap();
        let paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["src/deep/list.tsx"]);

        let mut query = SearchQuery::literal("todo");
        query.glob = Some("*.ts".to_owned());
        let found = search_store(&store, None, &FileCaller::at_role(Role::ADMIN), &query)
            .await
            .unwrap();
        let paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["src/app.ts"]);
    }

    #[tokio::test]
    async fn a_search_cannot_report_a_line_the_caller_could_not_have_read() {
        let (_base, store) = searchable().await;
        store
            .set_meta(
                &StorePath::from_static("src/deep"),
                &FileMeta {
                    min_role: Some(Role::ADMIN),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // The restricted directory is not walked for a caller who could not open
        // it — otherwise its contents would leak one matching line at a time.
        let found = search_store(
            &store,
            None,
            &FileCaller::anonymous(),
            &SearchQuery::literal("todo"),
        )
        .await
        .unwrap();
        let paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["src/app.ts"]);

        // The store's own floor composes the same way: above it, nothing at all.
        let found = search_store(
            &store,
            Some(Role::ADMIN),
            &FileCaller::anonymous(),
            &SearchQuery::literal("todo"),
        )
        .await
        .unwrap();
        assert!(found.hits.is_empty());
    }

    #[tokio::test]
    async fn a_search_that_hit_its_ceiling_says_so() {
        let (_base, store) = temp_store();
        for n in 0..10 {
            store
                .write(
                    &StorePath::parse(&format!("f{n}.ts")).unwrap(),
                    Bytes::from_static(b"needle\nneedle\n"),
                )
                .await
                .unwrap();
        }
        let mut query = SearchQuery::literal("needle");
        query.max_results = 5;
        let found = search_store(&store, None, &FileCaller::at_role(Role::ADMIN), &query)
            .await
            .unwrap();
        assert_eq!(found.hits.len(), 5);
        assert!(found.truncated);
    }

    // --- finding files by name ----------------------------------------------

    #[tokio::test]
    async fn a_name_search_walks_the_whole_tree_case_insensitively() {
        let (_base, store) = searchable().await;
        let found = find_files(
            &store,
            None,
            &FileCaller::at_role(Role::ADMIN),
            "",
            "LIST",
            DEFAULT_MAX_FOUND,
        )
        .await
        .unwrap();
        let paths: Vec<&str> = found.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["src/deep/list.tsx"]);
        assert!(!found.truncated);

        // Unlike the *content* search, nothing is excluded by default: someone
        // hunting for a file in `node_modules` means it. And a directory is a
        // thing people look for by name, so directories match too.
        let found = find_files(
            &store,
            None,
            &FileCaller::at_role(Role::ADMIN),
            "",
            "de",
            DEFAULT_MAX_FOUND,
        )
        .await
        .unwrap();
        let mut paths: Vec<&str> = found.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            ["node_modules", "node_modules/pkg/index.js", "src/deep"]
        );

        // The row the file manager draws comes back whole — a hit with no size
        // is three empty columns and another round trip to fill them.
        let hit = find_files(
            &store,
            None,
            &FileCaller::at_role(Role::ADMIN),
            "",
            "readme",
            DEFAULT_MAX_FOUND,
        )
        .await
        .unwrap();
        assert_eq!(hit.entries[0].size, Some(15));
        assert!(hit.entries[0].modified.is_some());
    }

    #[tokio::test]
    async fn a_name_search_is_narrowed_by_directory_and_bounded() {
        let (_base, store) = searchable().await;
        let found = find_files(
            &store,
            None,
            &FileCaller::at_role(Role::ADMIN),
            "src",
            "ts",
            DEFAULT_MAX_FOUND,
        )
        .await
        .unwrap();
        let mut paths: Vec<&str> = found.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        // Rooted at `src`, so `readme.md` and the dependency tree are outside it.
        assert_eq!(paths, ["src/app.ts", "src/deep/list.tsx"]);

        let found = find_files(&store, None, &FileCaller::at_role(Role::ADMIN), "", "ts", 1)
            .await
            .unwrap();
        assert_eq!(found.entries.len(), 1);
        assert!(found.truncated);

        // A blank search is not "everything": it is a caller with nothing to
        // look for, and a walk of the whole store is not the answer to it.
        assert!(
            find_files(
                &store,
                None,
                &FileCaller::at_role(Role::ADMIN),
                "",
                "  ",
                10
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_name_search_cannot_surface_a_file_the_caller_could_not_open() {
        let (_base, store) = searchable().await;
        store
            .set_meta(
                &StorePath::from_static("src/deep"),
                &FileMeta {
                    min_role: Some(Role::ADMIN),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Neither the restricted directory nor anything under it: a search box
        // that answers "list.tsx" for a caller who cannot enter `src/deep` has
        // leaked the listing the rule was set to hide.
        let found = find_files(
            &store,
            None,
            &FileCaller::anonymous(),
            "",
            "list",
            DEFAULT_MAX_FOUND,
        )
        .await
        .unwrap();
        assert!(found.entries.is_empty());
        let found = find_files(
            &store,
            None,
            &FileCaller::anonymous(),
            "",
            "deep",
            DEFAULT_MAX_FOUND,
        )
        .await
        .unwrap();
        assert!(found.entries.is_empty());
    }
}
