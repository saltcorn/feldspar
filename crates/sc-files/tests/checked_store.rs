//! [`CheckedStore`]'s grants (TODO-code-quality Q3.6), against a real
//! directory: a path is reached through a grant the rule handed out, the
//! drop-box rule lets a new file in where the caller may not otherwise write,
//! a rename does not cross two stores, and only an administrator's binding
//! answers the store's directory.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use sc_files::{CheckedStore, FileCaller, FileMeta, FileStore, LocalFileStore};
use sc_types::Role;

/// A fresh unique temp directory for one test.
fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sc-files-checked-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A store with an admin-only folder `private` holding `a.txt`, owned by
/// `ada`, and a public `open.txt`.
async fn store(tag: &str) -> (Arc<dyn FileStore>, PathBuf) {
    let dir = temp_dir(tag);
    let store: Arc<dyn FileStore> = Arc::new(LocalFileStore::new("docs", &dir).unwrap());
    store.mkdir("private").await.unwrap();
    store
        .set_meta(
            "private",
            &FileMeta {
                min_role: Some(Role::ADMIN),
                ..FileMeta::default()
            },
        )
        .await
        .unwrap();
    store
        .write("private/a.txt", Bytes::from_static(b"ada's"))
        .await
        .unwrap();
    store
        .set_meta(
            "private/a.txt",
            &FileMeta {
                owner: Some("ada".to_owned()),
                ..FileMeta::default()
            },
        )
        .await
        .unwrap();
    store
        .write("open.txt", Bytes::from_static(b"anyone"))
        .await
        .unwrap();
    (store, dir)
}

fn as_user(store: &Arc<dyn FileStore>, id: &str) -> CheckedStore {
    CheckedStore::new(
        Arc::clone(store),
        None,
        FileCaller::new(Role::of(80), Some(id.to_owned())),
    )
}

#[tokio::test]
async fn a_grant_is_handed_out_only_where_the_rule_admits_the_caller() {
    let (raw, dir) = store("grants").await;
    let ada = as_user(&raw, "ada");
    let bob = as_user(&raw, "bob");

    // The owner reads past the folder's rule; somebody else at her role is
    // refused, and the refusal is an `auth`, not a missing file.
    let file = ada.readable("private/a.txt").await.unwrap();
    assert_eq!(&file.read().await.unwrap()[..], b"ada's");
    let refused = bob.readable("private/a.txt").await.err().expect("refused");
    assert!(refused.auth_message().is_some(), "{refused}");
    assert!(bob.writable("private/a.txt").await.is_err());
    assert!(bob.serve("private/a.txt").await.is_err());

    // A served file is the grant's path, in the grant's store.
    let served = bob.serve("open.txt").await.unwrap();
    assert_eq!(served.path(), "open.txt");
    assert_eq!(served.store_name(), "docs");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn the_drop_box_takes_a_new_file_and_refuses_to_replace_another_s() {
    let (raw, dir) = store("dropbox").await;
    let bob = as_user(&raw, "bob");

    // `private` is admin-only, so bob may not write there…
    assert!(bob.writable("private/b.txt").await.is_err());
    // …but a route whose own rule admitted him may deposit a new file, which
    // is then his.
    let dropped = bob.deposit("private/b.txt").await.unwrap();
    dropped.write(Bytes::from_static(b"bob's")).await.unwrap();
    dropped.claim().await;
    assert_eq!(
        raw.get_meta("private/b.txt").await.unwrap().owner.as_deref(),
        Some("bob")
    );
    // Replacing somebody else's file is a write, and is checked as one.
    assert!(bob.deposit("private/a.txt").await.is_err());
    // His own he may replace.
    assert!(bob.deposit("private/b.txt").await.is_ok());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_rename_stays_in_one_store_and_a_root_is_an_administrator_s() {
    let (raw, dir) = store("rename").await;
    let other_dir = temp_dir("rename-other");
    let other: Arc<dyn FileStore> = Arc::new(LocalFileStore::new("other", &other_dir).unwrap());
    let admin = FileCaller::new(Role::ADMIN, None);
    let here = CheckedStore::new(Arc::clone(&raw), None, admin.clone());
    let there = CheckedStore::new(Arc::clone(&other), None, admin);

    let from = here.writable("open.txt").await.unwrap();
    let to = there.writable("open.txt").await.unwrap();
    assert!(from.rename_to(&to).await.is_err(), "no rename across stores");
    // The same store bound twice is one store: the server's temporary file
    // moved to where a caller deposits it.
    let bob = as_user(&raw, "bob");
    let deposit = bob.deposit("moved.txt").await.unwrap();
    from.rename_to(&deposit).await.unwrap();
    assert!(raw.stat("moved.txt").await.unwrap().is_some());

    // Only an administrator's binding answers the directory a process (the
    // IDE's language server) would be given.
    assert_eq!(here.local_root().unwrap(), Some(dir.clone()));
    assert!(bob.local_root().is_err());

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&other_dir).ok();
}
