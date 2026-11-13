//! `GitRepo` as a **mirror**: the sync link's clone (APP_SYNC_PROPOSAL §2),
//! built with no file store, against a local bare repository over `file://`,
//! and reaching an HTTP remote with a token that appears nowhere but the
//! request.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts git directly to stand in for another clone"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use sc_files::{GitRepo, HttpsToken, generate_deploy_key_in};

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sc-files-mirror-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// An empty bare repository, and its `file://` URL.
fn bare(tag: &str) -> (PathBuf, String) {
    let dir = temp_dir(tag).join("origin.git");
    std::fs::create_dir_all(&dir).unwrap();
    git(
        &dir,
        &["init", "--bare", "-q", "--initial-branch=main", "."],
    );
    let url = format!("file://{}", dir.display());
    (dir, url)
}

fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(p, b)| ((*p).to_owned(), b.as_bytes().to_vec()))
        .collect()
}

#[tokio::test]
async fn a_mirror_commits_pushes_and_reads_back_without_a_working_tree() {
    let (_origin, url) = bare("roundtrip");
    let mirror = GitRepo::new(
        temp_dir("roundtrip-m").join("sync-repo"),
        Some(url.clone()),
        Some("main".into()),
        None,
    );
    mirror.init_mirror().await.unwrap();
    mirror.init_mirror().await.unwrap(); // idempotent
    mirror.fetch().await.unwrap();
    assert_eq!(
        mirror.remote_branches().await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        mirror.ref_commit("refs/remotes/origin/main").await.unwrap(),
        None
    );

    let tree = files(&[
        ("tables/tickets.json", "{}\n"),
        ("feldspar-structure.json", "{}\n"),
    ]);
    let first = mirror
        .commit_files(
            None,
            Some("app"),
            &tree,
            "first",
            Some(("Ann", "ann@example.com")),
        )
        .await
        .unwrap()
        .expect("a commit");
    // Nothing moved yet: the commit is not on the remote.
    assert_eq!(mirror.ref_commit("refs/heads/main").await.unwrap(), None);
    let pushed = mirror.push_commit(&first, "main").await.unwrap();
    assert!(pushed.success, "{}", pushed.output);
    mirror.fetch().await.unwrap();
    assert_eq!(
        mirror.ref_commit("refs/remotes/origin/main").await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        mirror.remote_branches().await.unwrap(),
        vec!["main".to_owned()]
    );
    assert!(mirror.on_remote(&first).await.unwrap());

    // Read back below the subdirectory, byte for byte.
    assert_eq!(
        mirror.read_files_at(&first, Some("app")).await.unwrap(),
        tree
    );
    // The same tree again is nothing to commit.
    assert_eq!(
        mirror
            .commit_files(Some(&first), Some("app"), &tree, "again", None)
            .await
            .unwrap(),
        None
    );
    // Nothing was ever checked out.
    let root = mirror.root().to_path_buf();
    let listed: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(listed, vec![std::ffi::OsString::from(".git")]);
}

#[tokio::test]
async fn a_commit_replaces_only_its_subdirectory() {
    let (origin, url) = bare("subdir");
    // Someone else's files beside the structure's.
    let seed = temp_dir("subdir-seed");
    git(&seed, &["clone", "-q", &url, "."]);
    git(&seed, &["config", "user.email", "s@example.com"]);
    git(&seed, &["config", "user.name", "S"]);
    std::fs::write(seed.join("README.md"), "# hello\n").unwrap();
    std::fs::create_dir_all(seed.join("app/views")).unwrap();
    std::fs::write(seed.join("app/views/old.json"), "{}\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-qm", "seed"]);
    git(&seed, &["push", "-q", "origin", "HEAD:main"]);
    let _ = origin;

    let mirror = GitRepo::new(temp_dir("subdir-m"), Some(url), None, None);
    mirror.init_mirror().await.unwrap();
    mirror.fetch().await.unwrap();
    let tip = mirror
        .ref_commit("refs/remotes/origin/main")
        .await
        .unwrap()
        .unwrap();
    let tree = files(&[("views/new.json", "{\"a\": 1}\n")]);
    let next = mirror
        .commit_files(Some(&tip), Some("app"), &tree, "replace", None)
        .await
        .unwrap()
        .unwrap();
    let all = mirror.read_files_at(&next, None).await.unwrap();
    let paths: Vec<&str> = all.keys().map(String::as_str).collect();
    assert_eq!(paths, ["README.md", "app/views/new.json"]);
    assert!(mirror.is_ancestor(&tip, &next).await.unwrap());
    assert!(!mirror.is_ancestor(&next, &tip).await.unwrap());
}

#[tokio::test]
async fn a_push_after_someone_else_pushed_is_refused_and_moves_nothing() {
    let (_origin, url) = bare("race");
    let mirror = GitRepo::new(temp_dir("race-m"), Some(url.clone()), None, None);
    mirror.init_mirror().await.unwrap();
    let first = mirror
        .commit_files(None, None, &files(&[("a.json", "1\n")]), "a", None)
        .await
        .unwrap()
        .unwrap();
    assert!(mirror.push_commit(&first, "main").await.unwrap().success);
    mirror.fetch().await.unwrap();

    // Another clone pushes in between.
    let other = temp_dir("race-other");
    git(&other, &["clone", "-q", &url, "."]);
    git(&other, &["config", "user.email", "o@example.com"]);
    git(&other, &["config", "user.name", "O"]);
    std::fs::write(other.join("b.json"), "2\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "b"]);
    git(&other, &["push", "-q", "origin", "HEAD:main"]);

    let mine = mirror
        .commit_files(
            Some(&first),
            None,
            &files(&[("a.json", "3\n")]),
            "mine",
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let refused = mirror.push_commit(&mine, "main").await.unwrap();
    assert!(!refused.success, "{}", refused.output);
    mirror.fetch().await.unwrap();
    let theirs = mirror
        .ref_commit("refs/remotes/origin/main")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(theirs, mine);
    assert!(mirror.is_ancestor(&first, &theirs).await.unwrap());
    assert!(!mirror.on_remote(&mine).await.unwrap());
}

#[tokio::test]
async fn blobs_are_read_in_one_batch_and_a_missing_one_is_none() {
    let (_origin, url) = bare("blobs");
    let mirror = GitRepo::new(temp_dir("blobs-m"), Some(url), None, None);
    mirror.init_mirror().await.unwrap();
    let commit = mirror
        .commit_files(
            None,
            None,
            &files(&[("x", "one\n"), ("y", "two\nlines\n")]),
            "m",
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let tree = mirror.tree_files(&commit, None).await.unwrap();
    let mut hashes: Vec<&str> = tree.iter().map(|f| f.hash.as_str()).collect();
    hashes.push("0123456789abcdef0123456789abcdef01234567");
    let blobs = mirror.read_blobs(&hashes).await.unwrap();
    assert_eq!(blobs[0].as_deref(), Some(&b"one\n"[..]));
    assert_eq!(blobs[1].as_deref(), Some(&b"two\nlines\n"[..]));
    assert_eq!(blobs[2], None);
}

/// A remote that asks for a password, and records the `Authorization` git
/// answers with.
async fn http_remote() -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let auth = request
                    .lines()
                    .find_map(|l| l.strip_prefix("Authorization: "))
                    .map(str::to_owned);
                let response = match auth {
                    None => {
                        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"t\"\r\n\
                             Content-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    Some(auth) => {
                        let _ = tx.send(auth);
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/repo.git"), rx)
}

#[tokio::test]
async fn an_https_token_reaches_the_remote_and_nothing_on_disk() {
    use base64::Engine as _;
    let (url, mut seen) = http_remote().await;
    let mirror = GitRepo::new(temp_dir("token"), Some(url), None, None)
        .with_https_token(HttpsToken::new(Some("deployer"), "tok-s3cr3t"));
    assert!(!format!("{mirror:?}").contains("tok-s3cr3t"));
    mirror.init_mirror().await.unwrap();
    let err = mirror.remote_branches().await.unwrap_err().to_string();
    assert!(!err.contains("tok-s3cr3t"), "{err}");

    let auth = seen.recv().await.expect("git sent credentials");
    let expected = base64::engine::general_purpose::STANDARD.encode("deployer:tok-s3cr3t");
    assert_eq!(auth.trim(), format!("Basic {expected}"));

    let config = std::fs::read_to_string(mirror.root().join(".git/config")).unwrap();
    assert!(!config.contains("tok-s3cr3t"), "{config}");
    assert!(!config.contains("deployer"), "{config}");
}

#[tokio::test]
async fn a_deploy_key_can_be_made_in_a_directory_of_its_own() {
    let dir = temp_dir("key").join("keys");
    let key = generate_deploy_key_in(&dir, "feldspar-sync").await.unwrap();
    assert_eq!(key.private_key_path, dir.join("feldspar-sync.key"));
    assert!(key.public_key.starts_with("ssh-ed25519 "));
}
