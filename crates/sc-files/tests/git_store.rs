//! The git file-store backend, against a **real** repository.
//!
//! The remote is a bare repository in a temp directory rather than a hosted one:
//! `git clone`, `pull` and `push` behave identically over a local path, so this
//! exercises the whole round trip — clone, edit through the `FileStore` API,
//! commit, push, and see the push arrive — with no network and no credentials.
//! What it deliberately does not cover is SSH authentication, which cannot be
//! tested without a remote to authenticate to; the deploy-key test below checks
//! the key is generated in the right shape and with the right permissions, which
//! is the half that is ours rather than OpenSSH's.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use sc_files::{
    ARG_BRANCH, ARG_CREATE, ARG_MESSAGE, ARG_PATHS, ARG_STAGED_ONLY, CFG_BRANCH, CFG_DIR,
    DATA_DIR_ENV, FileStore, FileStoreDef, GIT_BACKEND, GitFileStore, GitRepo, OP_CHECKOUT,
    OP_COMMIT, OP_DISCARD, OP_STAGE, OP_STATUS, OP_UNSTAGE, clone_path, connect_from_def,
    generate_deploy_key, record_clone_path, run_backend_operation, validate_file_store_config,
};
use sc_types::Attrs;

/// A fresh unique temp directory for one test.
fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sc-files-git-{tag}-{}-{}-{}",
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

/// Run git in `dir`, asserting it succeeded.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("running `git {}`: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "`git {}` failed: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A bare repository with one commit on `main`, standing in for the remote an
/// admin would paste the URL of.
fn origin_with_a_commit(tag: &str) -> PathBuf {
    let base = temp_dir(tag);
    let bare = base.join("origin.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main", "."]);

    // Seed it through a throwaway working copy: a bare repo has no tree to
    // commit in.
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "--initial-branch=main", "."]);
    git(&seed, &["config", "user.email", "seed@example.com"]);
    git(&seed, &["config", "user.name", "Seed"]);
    std::fs::write(seed.join("README.md"), "# from the remote\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-m", "initial"]);
    git(&seed, &["remote", "add", "origin", &bare.to_string_lossy()]);
    git(&seed, &["push", "origin", "main"]);

    bare
}

/// A git store definition pointed at `url`, with its clone directory pinned into
/// a temp directory — never the developer's real data directory.
fn git_store_def(name: &str, url: &Path, clone_into: &Path) -> FileStoreDef {
    let mut def = FileStoreDef::git(name, url.to_string_lossy().into_owned());
    record_clone_path(&mut def, clone_into);
    def
}

#[tokio::test]
async fn a_git_store_clones_serves_commits_and_pushes() {
    let origin = origin_with_a_commit("roundtrip");
    let workspace = temp_dir("roundtrip-clone");
    let def = git_store_def("app", &origin, &workspace.join("app"));

    // The definition is well-formed …
    validate_file_store_config(&def).unwrap();
    let repo = GitRepo::from_def(&def).unwrap();
    // … but nothing has been cloned, so there is nothing to connect to yet.
    assert!(!repo.is_cloned());
    assert!(connect_from_def(&def).is_err());
    assert!(!repo.status().await.unwrap().cloned);

    // Clone. This is the explicit, awaited step — connecting never does it.
    repo.ensure_cloned().await.unwrap();
    assert!(repo.is_cloned());

    // And now it is an ordinary file store, serving what the remote had.
    let store = connect_from_def(&def).unwrap();
    assert_eq!(store.name(), "app");
    assert!(store.is_git_repo());
    let readme = store.read("README.md").await.unwrap();
    assert_eq!(&readme[..], b"# from the remote\n");

    // A second clone is a no-op rather than a re-clone: saving the store must
    // not throw away a working tree.
    let again = repo.ensure_cloned().await.unwrap();
    assert!(again.success);
    assert!(
        again.output.contains("using the working copy already in"),
        "{}",
        again.output
    );

    // Write through the FileStore API — the admin's file manager or an app's
    // build output — and git sees it.
    store
        .write("notes/today.md", Bytes::from_static(b"hello\n"))
        .await
        .unwrap();
    let status = repo.status().await.unwrap();
    assert!(status.cloned);
    assert_eq!(status.branch, "main");
    assert!(!status.clean(), "the new file should show as a change");
    assert!(
        status.changes.iter().any(|c| c.contains("notes/")),
        "{:?}",
        status.changes
    );
    assert!(
        status.last_commit.contains("initial"),
        "{}",
        status.last_commit
    );

    // Commit everything, with a message.
    let commit = repo.commit("add today's notes", true).await.unwrap();
    assert!(commit.committed, "{}", commit.output);
    let status = repo.status().await.unwrap();
    assert!(status.clean());
    assert!(status.last_commit.contains("add today's notes"));
    // Committed but not pushed: one commit ahead of the remote.
    assert_eq!((status.ahead, status.behind), (1, 0));

    // Committing again with nothing changed is a no-op, not a failure.
    let nothing = repo.commit("nothing to see", true).await.unwrap();
    assert!(!nothing.committed, "{}", nothing.output);

    // Push, and the remote has it.
    repo.push().await.unwrap();
    assert_eq!(git(&origin, &["show", "main:notes/today.md"]), "hello");
    let status = repo.status().await.unwrap();
    assert_eq!((status.ahead, status.behind), (0, 0));
}

#[tokio::test]
async fn a_pull_brings_down_what_someone_else_pushed() {
    let origin = origin_with_a_commit("pull");
    let workspace = temp_dir("pull-clone");
    let def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();
    // A store cloned by us commits with our fallback identity when the machine
    // has none configured; make the test independent of the developer's config.
    let store = GitFileStore::new("app", GitRepo::from_def(&def).unwrap()).unwrap();

    // Someone else pushes a file to the remote.
    let other = workspace.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["clone", &origin.to_string_lossy(), "."]);
    git(&other, &["config", "user.email", "other@example.com"]);
    git(&other, &["config", "user.name", "Other"]);
    std::fs::write(other.join("upstream.txt"), "from elsewhere\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-m", "upstream change"]);
    git(&other, &["push", "origin", "main"]);

    // Before the pull the store cannot see it …
    assert!(store.read("upstream.txt").await.is_err());
    // … after it, it can, and the file store serves it.
    repo.pull().await.unwrap();
    let got = store.read("upstream.txt").await.unwrap();
    assert_eq!(&got[..], b"from elsewhere\n");
    assert!(repo.status().await.unwrap().clean());
}

#[tokio::test]
async fn a_branch_setting_checks_out_that_branch() {
    let origin = origin_with_a_commit("branch");
    // Add a second branch to the remote.
    let workspace = temp_dir("branch-clone");
    let seed = workspace.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["clone", &origin.to_string_lossy(), "."]);
    git(&seed, &["config", "user.email", "seed@example.com"]);
    git(&seed, &["config", "user.name", "Seed"]);
    git(&seed, &["checkout", "-b", "release"]);
    std::fs::write(seed.join("VERSION"), "1.0\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-m", "release 1.0"]);
    git(&seed, &["push", "origin", "release"]);

    let mut def =
        FileStoreDef::git("app", origin.to_string_lossy().into_owned()).with(CFG_BRANCH, "release");
    record_clone_path(&mut def, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();

    let status = repo.status().await.unwrap();
    assert_eq!(status.branch, "release");
    let store = connect_from_def(&def).unwrap();
    assert_eq!(&store.read("VERSION").await.unwrap()[..], b"1.0\n");
}

#[tokio::test]
async fn a_bad_url_fails_the_clone_with_gits_own_message() {
    let workspace = temp_dir("badurl");
    let def = git_store_def(
        "app",
        &workspace.join("no-such-repository"),
        &workspace.join("app"),
    );
    let repo = GitRepo::from_def(&def).unwrap();
    let err = repo.ensure_cloned().await.unwrap_err().to_string();
    // The admin needs git's reason, not "clone failed".
    assert!(err.contains("no-such-repository"), "{err}");
    assert!(!repo.is_cloned());
}

#[tokio::test]
async fn operations_on_a_store_that_was_never_cloned_say_so() {
    let workspace = temp_dir("uncloned");
    let def = git_store_def(
        "app",
        Path::new("git@example.com:me/app.git"),
        &workspace.join("app"),
    );
    let repo = GitRepo::from_def(&def).unwrap();

    for err in [
        repo.pull().await.unwrap_err().to_string(),
        repo.push().await.unwrap_err().to_string(),
        repo.commit("anything", true).await.unwrap_err().to_string(),
    ] {
        assert!(err.contains("not a git clone yet"), "{err}");
    }
    // `status` is the exception: asking what state an unconnected store is in is
    // exactly how the UI decides to offer a Clone button, so it answers rather
    // than failing.
    assert!(!repo.status().await.unwrap().cloned);
}

#[tokio::test]
async fn the_status_operation_carries_the_working_copy_as_data() {
    // The IDE's source-control view (§12.1) cannot list changed files from a
    // paragraph of prose, so `status` answers twice: `output` for the admin
    // screen, `data` for a client that has to act on it. Both, from one run.
    let origin = origin_with_a_commit("statusdata");
    let workspace = temp_dir("statusdata-clone");
    let mut def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();

    let store = connect_from_def(&def).unwrap();
    store
        .write("notes/today.md", Bytes::from_static(b"hello\n"))
        .await
        .unwrap();

    let outcome = run_backend_operation(&mut def, OP_STATUS, &Attrs::new())
        .await
        .unwrap();
    // The prose is unchanged and still the whole of what the admin UI renders.
    assert!(
        outcome.output.contains("On branch main"),
        "{}",
        outcome.output
    );

    let data = outcome.data.expect("the git backend fills the payload");
    assert_eq!(data["cloned"], serde_json::json!(true));
    assert_eq!(data["branch"], serde_json::json!("main"));
    assert_eq!(data["ahead"], serde_json::json!(0));
    assert_eq!(data["behind"], serde_json::json!(0));
    assert!(
        data["last_commit"].as_str().unwrap().contains("initial"),
        "{data}"
    );
    assert_eq!(
        data["branches"].as_array().unwrap(),
        &[serde_json::json!("main")],
        "{data}"
    );

    // The porcelain lines, split into what a view addresses a file by: git's own
    // two-letter code, and the path.
    let changes = data["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{data}");
    assert_eq!(changes[0]["status"], serde_json::json!("??"));
    assert_eq!(changes[0]["path"], serde_json::json!("notes/today.md"));

    // A commit through the same entry point, and the change is gone from the
    // payload rather than only from the prose.
    let mut input = Attrs::new();
    input.insert(ARG_MESSAGE.to_owned(), serde_json::json!("add notes"));
    let outcome = run_backend_operation(&mut def, OP_COMMIT, &input)
        .await
        .unwrap();
    let data = outcome
        .data
        .expect("every instance operation reports the state it left");
    assert!(data["changes"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["ahead"], serde_json::json!(1), "{data}");
}

/// The index, which is what a Source Control view's two groups are (§12.1):
/// stage one of two changes, commit only what is staged, and the other change is
/// still waiting afterwards.
#[tokio::test]
async fn staging_decides_what_a_commit_takes() {
    let origin = origin_with_a_commit("staging");
    let workspace = temp_dir("staging-clone");
    let mut def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();
    let clone = workspace.join("app");
    git(&clone, &["config", "user.email", "test@example.com"]);
    git(&clone, &["config", "user.name", "Test"]);
    let store = connect_from_def(&def).unwrap();

    // Two new files and one edit: an untracked pair and a modification, which
    // are the three codes the view draws letters from.
    store
        .write("notes/today.md", Bytes::from_static(b"today\n"))
        .await
        .unwrap();
    store
        .write("notes/later.md", Bytes::from_static(b"later\n"))
        .await
        .unwrap();
    store
        .write("README.md", Bytes::from_static(b"# edited\n"))
        .await
        .unwrap();
    let codes = |status: &sc_files::GitStatus, path: &str| {
        status
            .changes
            .iter()
            .filter_map(|line| sc_files::parse_change(line))
            .find(|change| change.path == path)
            .unwrap_or_else(|| panic!("{path} is not among {:?}", status.changes))
            .status
    };
    let status = repo.status().await.unwrap();
    assert_eq!(codes(&status, "notes/today.md"), "??");
    assert_eq!(codes(&status, "README.md"), " M");

    // Stage one path. git's code moves from the working-tree column into the
    // index column, which is the whole reason the view can tell the groups apart.
    repo.stage(&["notes/today.md".to_owned()]).await.unwrap();
    let status = repo.status().await.unwrap();
    assert_eq!(codes(&status, "notes/today.md"), "A ");
    assert_eq!(codes(&status, "notes/later.md"), "??");
    assert_eq!(codes(&status, "README.md"), " M");

    // A commit of what is staged takes that file and leaves the others.
    let committed = repo.commit("add today's notes", false).await.unwrap();
    assert!(committed.committed, "{}", committed.output);
    assert_eq!(
        git(&clone, &["show", "--name-only", "--pretty=format:", "HEAD"]),
        "notes/today.md"
    );
    let status = repo.status().await.unwrap();
    assert_eq!(codes(&status, "notes/later.md"), "??");
    assert_eq!(codes(&status, "README.md"), " M");

    // Committing staged-only again commits nothing — and is not a failure, the
    // same way an unchanged working copy is not.
    let nothing = repo.commit("nothing staged", false).await.unwrap();
    assert!(!nothing.committed, "{}", nothing.output);

    // Staging with no paths stages everything, including the edit.
    repo.stage(&[]).await.unwrap();
    let status = repo.status().await.unwrap();
    assert_eq!(codes(&status, "notes/later.md"), "A ");
    assert_eq!(codes(&status, "README.md"), "M ");

    // Unstaging one path puts it back in the working tree, with its contents
    // untouched — the file is still edited, it is just no longer staged.
    repo.unstage(&["README.md".to_owned()]).await.unwrap();
    let status = repo.status().await.unwrap();
    assert_eq!(codes(&status, "README.md"), " M");
    assert_eq!(codes(&status, "notes/later.md"), "A ");
    assert_eq!(
        &store.read("README.md").await.unwrap()[..],
        b"# edited\n",
        "unstaging must not touch the file"
    );

    // Unstaging with no paths unstages the rest, and a file that had never been
    // committed is untracked again rather than lost.
    repo.unstage(&[]).await.unwrap();
    let status = repo.status().await.unwrap();
    assert_eq!(codes(&status, "notes/later.md"), "??");

    // The same through the declared operations, which is how the IDE reaches
    // them: stage one path by name, then commit `staged_only`.
    let mut input = Attrs::new();
    input.insert(ARG_PATHS.to_owned(), serde_json::json!("notes/later.md\n"));
    let outcome = run_backend_operation(&mut def, OP_STAGE, &input)
        .await
        .unwrap();
    // `git add` prints nothing on success, and an operation whose output is git's
    // own has to say something rather than nothing.
    assert!(
        outcome.output.contains("notes/later.md"),
        "{}",
        outcome.output
    );
    let data = outcome.data.unwrap();
    let staged = data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == serde_json::json!("notes/later.md"))
        .unwrap()
        .clone();
    assert_eq!(staged["status"], serde_json::json!("A "), "{data}");

    let mut input = Attrs::new();
    input.insert(ARG_MESSAGE.to_owned(), serde_json::json!("add later"));
    input.insert(ARG_STAGED_ONLY.to_owned(), serde_json::json!(true));
    let outcome = run_backend_operation(&mut def, OP_COMMIT, &input)
        .await
        .unwrap();
    let data = outcome.data.unwrap();
    let left: Vec<&str> = data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    assert_eq!(left, ["README.md"], "{data}");

    // And unstaging through the operation says what it did, since git does not.
    run_backend_operation(&mut def, OP_STAGE, &Attrs::new())
        .await
        .unwrap();
    let outcome = run_backend_operation(&mut def, OP_UNSTAGE, &Attrs::new())
        .await
        .unwrap();
    assert!(
        outcome.output.contains("every change"),
        "{}",
        outcome.output
    );
    let data = outcome.data.unwrap();
    assert_eq!(data["changes"].as_array().unwrap().len(), 1, "{data}");
}

/// A path from a client is a **pathspec** once git sees it, and the three shapes
/// that are not a file in this working tree are refused before it does.
#[tokio::test]
async fn a_staged_path_cannot_escape_the_working_copy() {
    let origin = origin_with_a_commit("pathspec");
    let workspace = temp_dir("pathspec-clone");
    let def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();

    async fn refused(repo: &GitRepo, path: &str) -> String {
        repo.stage(&[path.to_owned()])
            .await
            .unwrap_err()
            .to_string()
    }
    assert!(
        refused(&repo, "../elsewhere/secrets")
            .await
            .contains("escapes"),
        "a traversal must be refused"
    );
    assert!(
        refused(&repo, "/etc/passwd")
            .await
            .contains("inside the working copy"),
        "an absolute path must be refused"
    );
    assert!(
        refused(&repo, "--git-dir=/tmp")
            .await
            .contains("git option"),
        "an option must be refused"
    );
    assert!(
        refused(&repo, ":(exclude)src")
            .await
            .contains("pathspec pattern"),
        "pathspec magic must be refused"
    );
    // A blank line is not an error: the list arrives as text, and an empty one
    // means everything.
    repo.stage(&["  ".to_owned()]).await.unwrap();
}

#[tokio::test]
async fn a_checkout_switches_branch_creates_one_and_refuses_to_discard_work() {
    let origin = origin_with_a_commit("checkout");
    let workspace = temp_dir("checkout-clone");
    let mut def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();
    let clone = workspace.join("app");
    git(&clone, &["config", "user.email", "test@example.com"]);
    git(&clone, &["config", "user.name", "Test"]);

    // Create a branch and land a change on it.
    let mut input = Attrs::new();
    input.insert(ARG_BRANCH.to_owned(), serde_json::json!("feature"));
    input.insert(ARG_CREATE.to_owned(), serde_json::json!(true));
    let outcome = run_backend_operation(&mut def, OP_CHECKOUT, &input)
        .await
        .unwrap();
    let data = outcome.data.unwrap();
    assert_eq!(data["branch"], serde_json::json!("feature"), "{data}");
    // Both branches are now switchable, so both are offered.
    let branches = data["branches"].as_array().unwrap();
    assert!(
        branches.contains(&serde_json::json!("main"))
            && branches.contains(&serde_json::json!("feature")),
        "{data}"
    );

    std::fs::write(clone.join("README.md"), "# changed on the branch\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-m", "diverge"]);

    // Switching back to an existing branch needs no `create`.
    let mut input = Attrs::new();
    input.insert(ARG_BRANCH.to_owned(), serde_json::json!("main"));
    let outcome = run_backend_operation(&mut def, OP_CHECKOUT, &input)
        .await
        .unwrap();
    assert_eq!(outcome.data.unwrap()["branch"], serde_json::json!("main"));
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).unwrap(),
        "# from the remote\n"
    );

    // And an uncommitted change the switch would overwrite stops it — with
    // git's own message, naming the file. No force, no stash: the alternative
    // is an editor that silently eats what someone just typed.
    std::fs::write(clone.join("README.md"), "# edited in the IDE\n").unwrap();
    let mut input = Attrs::new();
    input.insert(ARG_BRANCH.to_owned(), serde_json::json!("feature"));
    let err = run_backend_operation(&mut def, OP_CHECKOUT, &input)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("README.md"), "{err}");
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).unwrap(),
        "# edited in the IDE\n",
        "the refused checkout must leave the working copy alone"
    );

    // A branch name that is not there is a failure, not a silent creation.
    let mut input = Attrs::new();
    input.insert(ARG_BRANCH.to_owned(), serde_json::json!("nonexistent"));
    assert!(
        run_backend_operation(&mut def, OP_CHECKOUT, &input)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_empty_commit_message_is_refused() {
    let origin = origin_with_a_commit("nomsg");
    let workspace = temp_dir("nomsg-clone");
    let def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();
    repo.ensure_cloned().await.unwrap();

    let err = repo.commit("   ", true).await.unwrap_err().to_string();
    assert!(err.contains("message"), "{err}");
}

#[tokio::test]
async fn a_generated_deploy_key_is_an_ed25519_key_only_we_can_read() {
    let data = temp_dir("deploykey");
    // Scoped to this test's temp directory, so nothing lands in the real one.
    let key = with_data_dir(&data, || generate_deploy_key("my app"))
        .await
        .unwrap();

    // The public half is what the admin pastes into the repository's settings.
    assert!(
        key.public_key.starts_with("ssh-ed25519 "),
        "{}",
        key.public_key
    );
    assert!(
        key.public_key.contains("saltcorn:my app"),
        "{}",
        key.public_key
    );
    assert!(!key.public_key.contains('\n'), "one line, to be pasted");

    // The private half stays on disk, under the name-derived filename, and is
    // readable only by us — `ssh` refuses a key that is not.
    assert_eq!(
        key.private_key_path,
        data.join("keys").join("my_app.key"),
        "the key is named after the store, made path-safe"
    );
    let private = std::fs::read_to_string(&key.private_key_path).unwrap();
    assert!(private.contains("OPENSSH PRIVATE KEY"), "{private}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key.private_key_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    // Generating again replaces it: an admin who needs a new key must be able to
    // get one, and the old public key may already have been rejected.
    let again = with_data_dir(&data, || generate_deploy_key("my app"))
        .await
        .unwrap();
    assert_eq!(again.private_key_path, key.private_key_path);
    assert_ne!(again.public_key, key.public_key);
}

#[tokio::test]
async fn a_store_the_admin_has_not_cloned_still_knows_where_it_would_go() {
    // `clone_path` is what the whole flow hangs off, so it must answer for a
    // definition that has never been saved or cloned.
    let data = temp_dir("clonepath");
    let def = FileStoreDef::git("web app", "git@example.com:me/app.git");
    let path = with_data_dir(&data, || async { clone_path(&def).unwrap() }).await;
    assert_eq!(path, data.join("git-stores").join("web_app"));
    assert_eq!(def.backend, GIT_BACKEND);
}

#[tokio::test]
async fn a_directory_chosen_on_creation_is_where_the_working_copy_lands() {
    // The admin names the directory, and the clone goes there rather than into
    // the location Saltcorn would have picked.
    let origin = origin_with_a_commit("chosen-dir");
    let workspace = temp_dir("chosen-dir-clone");
    let chosen = workspace.join("checkouts").join("site");

    let mut def = FileStoreDef::git("app", origin.to_string_lossy().into_owned())
        .with(CFG_DIR, chosen.to_string_lossy().into_owned());
    validate_file_store_config(&def).unwrap();

    let repo = GitRepo::from_def(&def).unwrap();
    assert_eq!(repo.root(), chosen);
    // The create path records where it cloned, exactly as it does for a
    // Saltcorn-chosen directory: the setting is what was asked for, the
    // attribute is what happened.
    record_clone_path(&mut def, repo.root());
    repo.ensure_cloned().await.unwrap();

    assert!(chosen.join(".git").exists());
    let store = connect_from_def(&def).unwrap();
    assert_eq!(
        &store.read("README.md").await.unwrap()[..],
        b"# from the remote\n"
    );
}

#[tokio::test]
async fn an_existing_checkout_is_adopted_rather_than_cloned_over() {
    // The case the directory setting exists for: a repository someone already
    // cloned onto the server, connected by naming its directory and **no URL at
    // all**. Nothing may be re-cloned, and nothing uncommitted may be lost.
    let origin = origin_with_a_commit("adopt");
    let workspace = temp_dir("adopt-existing");
    let existing = workspace.join("already-here");
    std::fs::create_dir_all(&existing).unwrap();
    git(&existing, &["clone", &origin.to_string_lossy(), "."]);
    git(&existing, &["config", "user.email", "someone@example.com"]);
    git(&existing, &["config", "user.name", "Someone"]);
    // Uncommitted work, which a re-clone would destroy.
    std::fs::write(existing.join("scratch.txt"), "mine\n").unwrap();

    let def = FileStoreDef::new("app", GIT_BACKEND)
        .with(CFG_DIR, existing.to_string_lossy().into_owned());
    // A definition with no URL is well-formed: the directory is the other half
    // of the pairing.
    validate_file_store_config(&def).unwrap();

    let repo = GitRepo::from_def(&def).unwrap();
    assert!(repo.is_cloned());
    let out = repo.ensure_cloned().await.unwrap();
    assert!(out.success);
    assert!(
        out.output.contains("using the working copy already in"),
        "{}",
        out.output
    );

    // Adopted as it stands: the remote's file is served, the uncommitted file is
    // still there, and git still reports it as a change.
    let store = connect_from_def(&def).unwrap();
    assert!(store.is_git_repo());
    assert_eq!(
        &store.read("README.md").await.unwrap()[..],
        b"# from the remote\n"
    );
    assert_eq!(&store.read("scratch.txt").await.unwrap()[..], b"mine\n");

    let status = repo.status().await.unwrap();
    assert!(status.cloned);
    assert_eq!(status.branch, "main");
    assert!(
        status.changes.iter().any(|c| c.contains("scratch.txt")),
        "{:?}",
        status.changes
    );

    // And it is a working store, not merely a readable one: the remote the
    // existing checkout already has is the one it pushes to.
    store
        .write("adopted.txt", Bytes::from_static(b"from saltcorn\n"))
        .await
        .unwrap();
    repo.commit("adopted", true).await.unwrap();
    repo.push().await.unwrap();
    assert!(
        git(&origin, &["log", "-1", "--pretty=%s", "main"]).contains("adopted"),
        "the push should have reached the remote"
    );
}

#[tokio::test]
async fn a_directory_with_no_checkout_and_no_url_has_nothing_to_clone() {
    // The other side of adoption: an empty directory and no URL is a store that
    // cannot be brought up, and the message has to name both ways out.
    let workspace = temp_dir("adopt-empty");
    let empty = workspace.join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let def =
        FileStoreDef::new("app", GIT_BACKEND).with(CFG_DIR, empty.to_string_lossy().into_owned());
    let repo = GitRepo::from_def(&def).unwrap();
    let err = repo.ensure_cloned().await.unwrap_err().to_string();
    assert!(err.contains("url"), "{err}");
    assert!(err.contains(&empty.to_string_lossy().into_owned()), "{err}");
    // Connecting fails too, and for the reason it always does: there is no
    // working tree.
    assert!(connect_from_def(&def).is_err());
}

/// Run an operation with [`DATA_DIR_ENV`] pointed at `dir`.
///
/// The variable is process-wide, so this serialises on **one** lock and restores
/// what was there — two tests setting it concurrently would each see the other's
/// directory, and the failure would be a key or a clone appearing in the wrong
/// place, which is the kind of thing that only fails sometimes. That is also why
/// there is a single helper rather than a sync and an async one: two helpers
/// meant two locks, which serialised nothing.
async fn with_data_dir<T, F, Fut>(dir: &Path, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    // A tokio mutex rather than a `std` one: it is held across the await, and it
    // has to be — the environment must not change while the operation runs.
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let before = set_data_dir(Some(dir));
    let out = f().await;
    set_data_dir(before.as_deref().map(Path::new));
    out
}

/// Set (or clear) the data-directory variable, returning what it was.
fn set_data_dir(dir: Option<&Path>) -> Option<String> {
    let before = std::env::var(DATA_DIR_ENV).ok();
    unsafe {
        match dir {
            Some(d) => std::env::set_var(DATA_DIR_ENV, d),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
    }
    before
}

/// Discard throws away unstaged changes — an edit goes back to the index, an
/// untracked file is deleted — and leaves staged work alone. It is the one
/// irreversible operation, so it refuses to guess: no paths, a clean path, or a
/// path that is only staged are all errors rather than no-ops.
#[tokio::test]
async fn discard_reverts_edits_deletes_untracked_files_and_keeps_the_index() {
    let origin = origin_with_a_commit("discard");
    let workspace = temp_dir("discard-clone");
    let mut def = git_store_def("app", &origin, &workspace.join("app"));
    let repo = GitRepo::from_def(&def).unwrap();

    // Before cloning, an empty (here: missing) directory is one a clone may use.
    let before = repo.status().await.unwrap();
    assert!(!before.cloned);
    assert!(before.can_clone);

    repo.ensure_cloned().await.unwrap();
    let clone = workspace.join("app");
    let status = repo.status().await.unwrap();
    assert!(!status.can_clone, "a clone is not offered over a clone");
    assert!(status.upstream, "a fresh clone tracks origin/main");

    std::fs::write(clone.join("README.md"), "# edited\n").unwrap();
    std::fs::write(clone.join("scratch.txt"), "junk\n").unwrap();
    std::fs::write(clone.join("*.md"), "a file with a glob for a name\n").unwrap();
    std::fs::write(clone.join("kept.txt"), "staged\n").unwrap();
    repo.stage(&["kept.txt".to_owned()]).await.unwrap();

    // Nothing named is not "everything".
    let err = run_backend_operation(&mut def, OP_DISCARD, &Attrs::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("paths"), "{err}");
    // A path with only staged changes is refused by name.
    let err = repo
        .discard(&["kept.txt".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("only staged"), "{err}");

    // A literal `*.md` discards that file, not README.md with it.
    repo.discard(&["*.md".to_owned()]).await.unwrap();
    assert!(!clone.join("*.md").exists());
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).unwrap(),
        "# edited\n"
    );

    let mut input = Attrs::new();
    input.insert(
        ARG_PATHS.to_owned(),
        serde_json::json!("README.md\nscratch.txt"),
    );
    let outcome = run_backend_operation(&mut def, OP_DISCARD, &input)
        .await
        .unwrap();
    assert!(outcome.output.contains("2 paths"), "{}", outcome.output);
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).unwrap(),
        "# from the remote\n"
    );
    assert!(!clone.join("scratch.txt").exists());
    let data = outcome.data.unwrap();
    let left: Vec<&str> = data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    assert_eq!(left, ["kept.txt"], "the staged file survives: {data}");
    assert_eq!(data["upstream"], serde_json::json!(true));
    assert_eq!(data["can_clone"], serde_json::json!(false));

    // Discarding what is already clean says so.
    let err = repo
        .discard(&["README.md".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no changes"), "{err}");

    // A branch made here has no upstream until it is pushed.
    repo.checkout("topic", true).await.unwrap();
    assert!(!repo.status().await.unwrap().upstream);
}

/// A directory that already has someone's files in it is not a clone target.
#[tokio::test]
async fn a_non_empty_directory_is_not_offered_a_clone() {
    let origin = origin_with_a_commit("occupied");
    let workspace = temp_dir("occupied-clone");
    let dir = workspace.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    let def = git_store_def("app", &origin, &dir);
    let repo = GitRepo::from_def(&def).unwrap();
    assert!(
        repo.status().await.unwrap().can_clone,
        "an empty directory may be cloned into"
    );
    std::fs::write(dir.join("notes.txt"), "mine\n").unwrap();
    assert!(!repo.status().await.unwrap().can_clone);
}

/// F2.4: the git driver streams, appends and joins as the local one does —
/// over its working tree — so an upload into a git store is an ordinary file.
#[tokio::test]
async fn a_git_store_streams_appends_and_joins() {
    use tokio::io::AsyncWriteExt;
    let origin = origin_with_a_commit("stream");
    let workspace = temp_dir("stream-clone");
    let def = git_store_def("app", &origin, &workspace.join("app"));
    GitRepo::from_def(&def)
        .unwrap()
        .ensure_cloned()
        .await
        .unwrap();
    let store = connect_from_def(&def).unwrap();

    let readme = sc_files::read_to_bytes(store.open_read("README.md", Some(2..6)).await.unwrap())
        .await
        .unwrap();
    assert_eq!(&readme[..], b"from");

    let mut out = store.open_append("uploads/a.bin").await.unwrap();
    out.write_all(b"part one, ").await.unwrap();
    out.shutdown().await.unwrap();
    let mut out = store.open_append("uploads/a.bin").await.unwrap();
    out.write_all(b"part two").await.unwrap();
    out.shutdown().await.unwrap();
    assert_eq!(
        &store.read("uploads/a.bin").await.unwrap()[..],
        b"part one, part two"
    );

    let parts = ["README.md".to_owned(), "uploads/a.bin".to_owned()];
    let size = store.concat(&parts, "uploads/joined.txt").await.unwrap();
    assert_eq!(size, 18 + 18);
    let missing = ["README.md".to_owned(), "nope".to_owned()];
    assert!(store.concat(&missing, "uploads/never.txt").await.is_err());
    assert!(store.stat("uploads/never.txt").await.unwrap().is_none());
    // The join is a file git sees, like any other write.
    let status = std::process::Command::new("git")
        .current_dir(workspace.join("app"))
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.contains("uploads/joined.txt"), "{status}");
    assert!(!status.contains("joining"), "no temporary left: {status}");
}
