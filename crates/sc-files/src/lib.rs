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
//! - **Paths are confined to the store root.** Absolute paths and `..`
//!   traversal are rejected before any I/O.

mod access;
mod backend;
mod def;
mod disk;
mod git;
mod local;
mod paths;
mod reference;
mod search;
mod serve;
mod store;
mod stream;
pub mod xattr;

pub use access::{
    FileCaller, ROLE_ADMIN, ROLE_PUBLIC, VisibleEntry, check_access, check_write,
    effective_min_role, filter_visible, list_visible, record_owner, visible_entries,
};
pub use backend::{
    OperationOutcome, Relocation, backend_config_spec, backend_operations, connect_from_def,
    display_config, git_config_spec, local_config_spec, registered_backends, relocate_for_restore,
    run_backend_operation, validate_file_store_config,
};
pub use def::{
    ATTR_CLONE_PATH, CFG_BRANCH, CFG_CREATE, CFG_DIR, CFG_KEY_PATH, CFG_PATH, CFG_PUBLIC_KEY,
    CFG_URL, FileStoreDef, FileStoreDefId, GIT_BACKEND, LOCAL_BACKEND,
};
pub use disk::{remove_store_from_disk, store_directory};
pub use git::{
    ARG_BRANCH, ARG_CREATE, ARG_MESSAGE, ARG_PATHS, ARG_STAGED_ONLY, CommitOutcome, DeployKey,
    GitChange, GitFileStore, GitOutput, GitRepo, GitStatus, OP_CHECKOUT, OP_CLONE, OP_COMMIT,
    OP_DISCARD, OP_GENERATE_KEY, OP_PULL, OP_PUSH, OP_STAGE, OP_STATUS, OP_UNSTAGE, clone_dir,
    clone_path, generate_deploy_key, git_operations, key_dir, parse_change, record_clone_path,
    record_deploy_key,
};
pub use local::{LocalFileStore, OP_SUGGEST_DIR, local_operations};
pub use paths::{DATA_DIR_ENV, data_dir, local_store_dir, suggest_local_dir};
pub use reference::{free_path, mime_for_path, suffixed_path, validate_file_path};
pub use search::{
    DEFAULT_EXCLUDED_DIRS, DEFAULT_MAX_FOUND, DEFAULT_MAX_RESULTS, FoundFiles, MAX_FILE_BYTES,
    MAX_FILES_SCANNED, MAX_LINE_CHARS, SearchHit, SearchOutcome, SearchQuery, find_by_extension,
    find_files, glob_matches, search_store, walk_store,
};
pub use serve::ServeFile;
pub use store::{CONTENT_TYPE_ATTRIBUTE, Entry, FILENAME_ATTRIBUTE, FileMeta, FileStat, FileStore};
pub use stream::{BoxAsyncRead, BoxAsyncWrite, check_range, read_to_bytes};

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

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
            .write("a/b/hello.txt", Bytes::from_static(b"hello world"))
            .await
            .unwrap();
        let got = store.read("a/b/hello.txt").await.unwrap();
        assert_eq!(&got[..], b"hello world");
    }

    #[tokio::test]
    async fn stat_answers_the_entrys_own_facts_and_none_for_what_is_not_there() {
        let (_base, store) = temp_store();
        store
            .write("docs/a.txt", Bytes::from_static(b"12345"))
            .await
            .unwrap();

        let file = store
            .stat("docs/a.txt")
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

        let dir = store.stat("docs").await.unwrap().expect("it is there");
        assert!(dir.is_dir);
        assert_eq!(dir.size, 0, "a directory has no size to report");
        assert!(store.stat("").await.unwrap().unwrap().is_dir, "the root is");

        // Nothing there is an answer rather than an error — which is what makes
        // `exists()` a question instead of a `catch`. Including the case where a
        // *file* is in the middle of the path.
        assert!(store.stat("docs/gone.txt").await.unwrap().is_none());
        assert!(store.stat("nowhere/at/all").await.unwrap().is_none());
        assert!(store.stat("docs/a.txt/deeper").await.unwrap().is_none());
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
            async fn read(&self, path: &str) -> sc_error::Result<Bytes> {
                self.0.read(path).await
            }
            async fn write(&self, path: &str, data: Bytes) -> sc_error::Result<()> {
                self.0.write(path, data).await
            }
            async fn list(&self, dir: &str) -> sc_error::Result<Vec<Entry>> {
                self.0.list(dir).await
            }
            async fn mkdir(&self, path: &str) -> sc_error::Result<()> {
                self.0.mkdir(path).await
            }
            async fn delete(&self, path: &str) -> sc_error::Result<bool> {
                self.0.delete(path).await
            }
            async fn rename(&self, from: &str, to: &str) -> sc_error::Result<()> {
                self.0.rename(from, to).await
            }
            fn is_git_repo(&self) -> bool {
                false
            }
            async fn get_meta(&self, path: &str) -> sc_error::Result<FileMeta> {
                self.0.get_meta(path).await
            }
            async fn set_meta(&self, path: &str, meta: &FileMeta) -> sc_error::Result<()> {
                self.0.set_meta(path, meta).await
            }
        }

        let (_base, inner) = temp_store();
        let store = ListOnly(inner);
        store
            .write("docs/a.txt", Bytes::from_static(b"123"))
            .await
            .unwrap();

        let file = store.stat("docs/a.txt").await.unwrap().expect("listed");
        assert_eq!(file.size, 3);
        assert!(!file.is_dir);
        // The modification time now comes from the listing too — a backend
        // whose `list` reports one gets it through the default `stat` without
        // implementing anything. `None` stays the honest answer for a backend
        // that does not record it, rather than an invented time.
        assert_eq!(
            file.modified,
            store.0.stat("docs/a.txt").await.unwrap().unwrap().modified
        );
        assert!(file.modified.is_some());
        assert!(store.stat("docs").await.unwrap().unwrap().is_dir);
        assert!(store.stat("").await.unwrap().unwrap().is_dir);
        assert!(store.stat("docs/gone.txt").await.unwrap().is_none());
        assert!(store.stat("nowhere/at/all").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_sorted_children_with_sizes() {
        let (_base, store) = temp_store();
        store
            .write("z.txt", Bytes::from_static(b"12345"))
            .await
            .unwrap();
        store
            .write("a.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        store
            .write("sub/c.txt", Bytes::from_static(b"y"))
            .await
            .unwrap();

        let root = store.list("").await.unwrap();
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
        let children = store.list("sub").await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].path, "sub/c.txt");
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let (_base, store) = temp_store();
        let err = store.read("../secret").await.unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
        let err = store
            .write("a/../../b", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
    }

    #[tokio::test]
    async fn meta_round_trips_and_defaults() {
        let (_base, store) = temp_store();
        store
            .write("doc.md", Bytes::from_static(b"# hi"))
            .await
            .unwrap();

        // No metadata set yet → defaults.
        let meta = store.get_meta("doc.md").await.unwrap();
        assert_eq!(meta, FileMeta::default());

        let mut m = FileMeta {
            min_role: Some(20),
            ..Default::default()
        };
        m.attributes.insert("mime".into(), "text/markdown".into());
        store.set_meta("doc.md", &m).await.unwrap();

        let got = store.get_meta("doc.md").await.unwrap();
        assert_eq!(got, m);

        // Metadata lives in an extended attribute, so it never shows as a file.
        let names: Vec<_> = store
            .list("")
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
            .set_meta("missing.txt", &FileMeta::default())
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

    #[tokio::test]
    async fn mkdir_is_idempotent_but_refuses_to_shadow_a_file() {
        let (_base, store) = temp_store();

        store.mkdir("a/b/c").await.unwrap();
        let entries = store.list("a/b").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_dir);

        // Asking again for a directory that is already there is not an error —
        // the caller wanted one there and there is one.
        store.mkdir("a/b/c").await.unwrap();

        // But a file already occupying the name is a real conflict.
        store
            .write("a/file.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = store.mkdir("a/file.txt").await.unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
    }

    #[tokio::test]
    async fn delete_reports_whether_anything_was_there() {
        let (_base, store) = temp_store();
        store
            .write("dir/file.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();

        assert!(store.delete("dir/file.txt").await.unwrap());
        // Already gone is `false`, not an error: a caller would otherwise have
        // to race an existence check against the delete.
        assert!(!store.delete("dir/file.txt").await.unwrap());

        // A directory goes with everything in it.
        store
            .write("dir/nested/deep.txt", Bytes::from_static(b"y"))
            .await
            .unwrap();
        assert!(store.delete("dir").await.unwrap());
        assert!(store.list("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_refuses_the_store_root() {
        let (base, store) = temp_store();
        store
            .write("keep.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();

        // Every spelling of "the root" is refused — a store's root *is* the
        // store, and removing it would leave a definition pointing at nothing.
        for root in ["", "/", "."] {
            let err = store.delete(root).await.unwrap_err();
            assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)), "{root:?}");
        }
        assert!(base.is_dir());
        assert_eq!(store.list("").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rename_moves_within_the_store_and_never_clobbers() {
        let (_base, store) = temp_store();
        store
            .write("a/one.txt", Bytes::from_static(b"first"))
            .await
            .unwrap();
        store
            .write("b/two.txt", Bytes::from_static(b"second"))
            .await
            .unwrap();

        // Moving into a directory that does not exist yet creates it.
        store.rename("a/one.txt", "c/moved.txt").await.unwrap();
        assert_eq!(&store.read("c/moved.txt").await.unwrap()[..], b"first");
        assert!(store.list("a").await.unwrap().is_empty());

        // Refusing to overwrite is the important part: a silent replace on a
        // file manager's drag-and-drop is a lost file with no undo.
        let err = store.rename("b/two.txt", "c/moved.txt").await.unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::Invalid(_)));
        assert_eq!(&store.read("c/moved.txt").await.unwrap()[..], b"first");
        assert_eq!(&store.read("b/two.txt").await.unwrap()[..], b"second");

        // A source that is not there is a not-found, not a silent success.
        let err = store.rename("nope.txt", "x.txt").await.unwrap_err();
        assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)));

        // And traversal is rejected on both sides, so this cannot move data out
        // of the store.
        assert!(store.rename("b/two.txt", "../escaped.txt").await.is_err());
        assert!(store.rename("../secret", "here.txt").await.is_err());
    }

    /// The path-cumulative rule `FileMeta` has documented since the MVP, now
    /// actually enforced: a directory's restriction covers everything beneath it.
    #[tokio::test]
    async fn access_is_cumulative_along_the_path() {
        let (_base, store) = temp_store();
        store
            .write("public/open.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        store
            .write("private/secret.txt", Bytes::from_static(b"y"))
            .await
            .unwrap();

        // Restrict the *directory* only; the file inside carries no rule.
        store.mkdir("private").await.unwrap();
        store
            .set_meta(
                "private",
                &FileMeta {
                    min_role: Some(1),
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
            Some(1)
        );
        // An admin (role 1) clears it; a public caller (100) does not.
        assert!(
            check_access(&store, None, "private/secret.txt", &FileCaller::role(1))
                .await
                .is_ok()
        );
        assert!(
            check_access(&store, None, "private/secret.txt", &FileCaller::role(100))
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
            .write("dir/file.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        store.mkdir("dir").await.unwrap();

        // A permissive rule inside a restricted directory must NOT widen access:
        // otherwise restricting a folder would not restrict it, since anything
        // inside could opt back out.
        store
            .set_meta(
                "dir",
                &FileMeta {
                    min_role: Some(20),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .set_meta(
                "dir/file.txt",
                &FileMeta {
                    min_role: Some(100),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            effective_min_role(&store, None, "dir/file.txt")
                .await
                .unwrap(),
            Some(20),
            "the more restrictive rule on the path must win"
        );

        // The store-wide floor is simply the outermost entry on the path, and
        // composes the same way.
        assert_eq!(
            effective_min_role(&store, Some(10), "dir/file.txt")
                .await
                .unwrap(),
            Some(10)
        );
        // A laxer store floor does not loosen a tighter nested rule.
        assert_eq!(
            effective_min_role(&store, Some(80), "dir/file.txt")
                .await
                .unwrap(),
            Some(20)
        );
    }

    #[tokio::test]
    async fn listings_hide_entries_the_caller_may_not_open() {
        let (_base, store) = temp_store();
        store
            .write("open.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        store
            .write("secret.txt", Bytes::from_static(b"y"))
            .await
            .unwrap();
        store
            .set_meta(
                "secret.txt",
                &FileMeta {
                    min_role: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let all = store.list("").await.unwrap();
        assert_eq!(all.len(), 2);

        // A listing must be filtered rather than merely refused: showing a name
        // the caller cannot open leaks exactly what the rule was set to hide.
        let visible = filter_visible(&store, None, all.clone(), &FileCaller::anonymous())
            .await
            .unwrap();
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["open.txt"]);

        // An admin sees everything.
        let admin = filter_visible(&store, None, all, &FileCaller::role(1))
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
        store.mkdir("drops").await.unwrap();
        store
            .set_meta(
                "drops",
                &FileMeta {
                    min_role: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for (path, owner) in [("drops/a.png", "user-a"), ("drops/b.png", "user-b")] {
            store.write(path, Bytes::from_static(b"x")).await.unwrap();
            record_owner(&store, path, owner).await;
        }
        let a = FileCaller::new(80, Some("user-a".into()));
        let b = FileCaller::new(80, Some("user-b".into()));
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
            check_write(&store, None, "drops/a.png", &FileCaller::role(1))
                .await
                .is_ok()
        );
        // An owned file is the owner's even where the floor is public.
        store
            .write("open.txt", Bytes::from_static(b"o"))
            .await
            .unwrap();
        record_owner(&store, "open.txt", "user-a").await;
        assert!(check_access(&store, None, "open.txt", &b).await.is_ok());
        assert!(check_write(&store, None, "open.txt", &b).await.is_err());
        // An unowned file keeps the path rule for writes.
        store
            .write("shared.txt", Bytes::from_static(b"s"))
            .await
            .unwrap();
        assert!(check_write(&store, None, "shared.txt", &b).await.is_ok());
        // A folder holding somebody else's file is theirs to keep: deleting it
        // would delete the file.
        store
            .write("mixed/mine.txt", Bytes::from_static(b"m"))
            .await
            .unwrap();
        store
            .write("mixed/theirs.txt", Bytes::from_static(b"t"))
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
        let c = FileCaller::new(80, Some("user-c".into()));
        assert!(list_visible(&store, None, "drops", &c).await.is_err());
        assert!(
            list_visible(&store, None, "drops", &anonymous)
                .await
                .is_err()
        );
        assert!(list_visible(&store, None, "nowhere", &c).await.is_err());
        assert_eq!(
            list_visible(&store, None, "drops", &FileCaller::role(1))
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
            .write("f.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        record_owner(&store, "f.txt", "first").await;
        record_owner(&store, "f.txt", "second").await;
        assert_eq!(
            store.get_meta("f.txt").await.unwrap().owner.as_deref(),
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
        async fn read(&self, path: &str) -> sc_error::Result<Bytes> {
            self.0
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| sc_error::Error::not_found(path.to_owned()))
        }
        async fn write(&self, path: &str, data: Bytes) -> sc_error::Result<()> {
            self.0.lock().unwrap().insert(path.to_owned(), data);
            Ok(())
        }
        async fn list(&self, _dir: &str) -> sc_error::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        async fn mkdir(&self, _path: &str) -> sc_error::Result<()> {
            Ok(())
        }
        async fn delete(&self, path: &str) -> sc_error::Result<bool> {
            Ok(self.0.lock().unwrap().remove(path).is_some())
        }
        async fn rename(&self, _from: &str, _to: &str) -> sc_error::Result<()> {
            Ok(())
        }
        fn is_git_repo(&self) -> bool {
            false
        }
        async fn get_meta(&self, _path: &str) -> sc_error::Result<FileMeta> {
            Ok(FileMeta::default())
        }
        async fn set_meta(&self, _path: &str, _meta: &FileMeta) -> sc_error::Result<()> {
            Ok(())
        }
    }

    /// What every driver must do with `open_read`, `open_append` and `concat`,
    /// asserted the same way for each.
    async fn streaming_contract(store: &dyn FileStore) {
        use tokio::io::AsyncWriteExt;
        store
            .write("in/a.bin", Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        // Whole, and ranged (half-open), and the empty range at the end.
        let all = read_to_bytes(store.open_read("in/a.bin", None).await.unwrap())
            .await
            .unwrap();
        assert_eq!(&all[..], b"0123456789");
        let some = read_to_bytes(store.open_read("in/a.bin", Some(2..5)).await.unwrap())
            .await
            .unwrap();
        assert_eq!(&some[..], b"234");
        let none = read_to_bytes(store.open_read("in/a.bin", Some(10..10)).await.unwrap())
            .await
            .unwrap();
        assert!(none.is_empty());
        // Past the end is refused, not truncated; a missing file is not found.
        assert!(store.open_read("in/a.bin", Some(5..11)).await.is_err());
        let err = store.open_read("in/nope.bin", None).await.err().unwrap();
        assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)), "{err}");

        // Append creates, then appends across two openings.
        for chunk in [&b"abc"[..], b"def"] {
            let mut out = store.open_append("up/new.bin").await.unwrap();
            out.write_all(chunk).await.unwrap();
            out.shutdown().await.unwrap();
        }
        assert_eq!(&store.read("up/new.bin").await.unwrap()[..], b"abcdef");

        // Concat joins in order and answers the size.
        store
            .write("parts/1", Bytes::from_static(b"one-"))
            .await
            .unwrap();
        store
            .write("parts/2", Bytes::from_static(b"two"))
            .await
            .unwrap();
        let parts = ["parts/1".to_owned(), "parts/2".to_owned()];
        assert_eq!(store.concat(&parts, "joined/out.txt").await.unwrap(), 7);
        assert_eq!(&store.read("joined/out.txt").await.unwrap()[..], b"one-two");
        // A missing part fails without leaving a partial target: neither a new
        // one nor a change to one that was there.
        let broken = ["parts/1".to_owned(), "parts/missing".to_owned()];
        assert!(store.concat(&broken, "joined/partial.txt").await.is_err());
        assert!(store.read("joined/partial.txt").await.is_err());
        assert!(store.concat(&broken, "joined/out.txt").await.is_err());
        assert_eq!(&store.read("joined/out.txt").await.unwrap()[..], b"one-two");
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
        store.mkdir("adir").await.unwrap();
        assert!(store.open_read("adir", None).await.is_err());
        // A replaced join keeps the target's metadata, owner included.
        record_owner(&store, "joined/out.txt", "user-a").await;
        let parts = ["parts/2".to_owned()];
        store.concat(&parts, "joined/out.txt").await.unwrap();
        assert_eq!(
            store
                .get_meta("joined/out.txt")
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
        let mut out = store.open_append("late.bin").await.unwrap();
        out.write_all(b"x").await.unwrap();
        assert!(store.read("late.bin").await.is_err());
        out.shutdown().await.unwrap();
        assert_eq!(&store.read("late.bin").await.unwrap()[..], b"x");
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
                .write(path, Bytes::copy_from_slice(bytes))
                .await
                .unwrap();
        }
        // Not UTF-8: it has no lines, and "matched at line 3" of a PNG is noise.
        store
            .write(
                "logo.png",
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
            &FileCaller::role(1),
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
        let found = search_store(&store, None, &FileCaller::role(1), &query)
            .await
            .unwrap();
        let paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["src/deep/list.tsx"]);

        let mut query = SearchQuery::literal("todo");
        query.glob = Some("*.ts".to_owned());
        let found = search_store(&store, None, &FileCaller::role(1), &query)
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
                "src/deep",
                &FileMeta {
                    min_role: Some(1),
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
            Some(1),
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
                .write(&format!("f{n}.ts"), Bytes::from_static(b"needle\nneedle\n"))
                .await
                .unwrap();
        }
        let mut query = SearchQuery::literal("needle");
        query.max_results = 5;
        let found = search_store(&store, None, &FileCaller::role(1), &query)
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
            &FileCaller::role(1),
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
            &FileCaller::role(1),
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
            &FileCaller::role(1),
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
            &FileCaller::role(1),
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

        let found = find_files(&store, None, &FileCaller::role(1), "", "ts", 1)
            .await
            .unwrap();
        assert_eq!(found.entries.len(), 1);
        assert!(found.truncated);

        // A blank search is not "everything": it is a caller with nothing to
        // look for, and a walk of the whole store is not the answer to it.
        assert!(
            find_files(&store, None, &FileCaller::role(1), "", "  ", 10)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_name_search_cannot_surface_a_file_the_caller_could_not_open() {
        let (_base, store) = searchable().await;
        store
            .set_meta(
                "src/deep",
                &FileMeta {
                    min_role: Some(1),
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
