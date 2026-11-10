//! Store files in Status and the base (S2.5): a file is an entity compared by
//! its hash, the base keeps only the hash, and the sync mirror keeps the bytes
//! reachable under `refs/feldspar/base` — so Discard can still put a file back
//! after the remote's history was rewritten.

use std::path::Path;

use sc_error::Result;
use sc_files::{GitRepo, SyncContents};
use sc_sync::{
    BASE_REF, Class, Content, OpBody, Status, base_file_bytes, discard_target, keep_base_files,
    plan,
};

use crate::fixture::{Installation, git, put};

/// Write `tree`'s files into the checkout `dir` (replacing what is there),
/// commit and push them.
fn commit_tree(dir: &Path, tree: &sc_sync::StructureTree, message: &str) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != ".git" {
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    for (path, bytes) in tree.to_files() {
        put(dir, &path, &bytes);
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

#[tokio::test]
async fn discard_restores_a_file_after_the_remote_was_force_pushed() -> Result<()> {
    let inst = Installation::new().await?;
    let (_, assets) = inst
        .local_store("assets", |d| d.with_sync_contents(SyncContents::Include))
        .await?;
    put(&assets, "logo.svg", b"<svg>v1</svg>\n");
    put(&assets, "style.css", b"body {}\n");
    let committed = inst.tree().await?;
    assert!(committed.get("file-stores/assets/files/logo.svg").is_some());

    // The remote, and this instance's mirror of it, holding that commit.
    let remote = inst.dir.join("structure.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let mirror_dir = inst.dir.join("sync-repo");
    std::fs::create_dir_all(&mirror_dir).unwrap();
    git(&mirror_dir, &["init", "-q"]);
    git(
        &mirror_dir,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    commit_tree(&mirror_dir, &committed, "first");
    git(&mirror_dir, &["push", "-q", "-u", "origin", "main"]);
    let mirror = GitRepo::new(&mirror_dir, None, Some("main".into()), None);

    // The base keeps hashes, and the mirror keeps the bytes under its ref.
    let base = keep_base_files(&mirror, &committed).await?;
    assert!(matches!(
        base.get("file-stores/assets/files/logo.svg"),
        Some(sc_sync::tree::TreeEntry::Blob { .. })
    ));
    let recorded = git(&mirror_dir, &["rev-parse", BASE_REF]);
    assert_eq!(recorded.len(), 40);

    // Someone rewrites the remote's history without the logo, and the mirror
    // follows it (a reset after a force-push, §2) and is garbage-collected.
    let other = inst.dir.join("other-clone");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    let mut rewritten = committed.clone();
    rewritten.remove("file-stores/assets/files/logo.svg");
    commit_tree(&other, &rewritten, "rewritten");
    git(
        &other,
        &[
            "push",
            "-q",
            "--force",
            &remote.to_string_lossy(),
            "main:main",
        ],
    );
    git(&mirror_dir, &["fetch", "-q", "origin"]);
    git(&mirror_dir, &["reset", "-q", "--hard", "origin/main"]);
    git(&mirror_dir, &["reflog", "expire", "--expire=now", "--all"]);
    git(&mirror_dir, &["gc", "-q", "--prune=now"]);
    let tip = rewritten;

    // Here, the logo was edited: a local change against a tip that deleted
    // it, so a conflict.
    put(&assets, "logo.svg", b"<svg>mine</svg>\n");
    let live = inst.tree().await?;
    let status = Status::of(&base, &tip, &live);
    let key = "store_file:assets/logo.svg";
    let entry = status.entry(key).unwrap();
    assert_eq!(entry.class, Class::Conflict);
    // The unchanged stylesheet is in sync, compared by hash against the base.
    assert_eq!(
        status.entry("store_file:assets/style.css").unwrap().class,
        Class::InSync
    );

    // Discard mine: the base version comes back, out of the mirror alone —
    // neither the tip nor live has those bytes any more.
    let bytes = base_file_bytes(&mirror, &status, &[key]).await?;
    assert_eq!(bytes.len(), 1);
    let target = discard_target(&live, &status, &[key], &bytes)?;
    let plan = plan(&live, &target, Some(&base));
    assert_eq!(plan.operations.len(), 1, "{:#?}", plan.operations);
    let op = &plan.operations[0];
    assert_eq!((op.action, op.entity.as_str()), ("update", key));
    let OpBody::Put { target: file, .. } = &op.body else {
        panic!("{:?}", op.body);
    };
    let Content::File {
        bytes: Some(restored),
        ..
    } = &file.content
    else {
        panic!("{:?}", file.content);
    };
    assert_eq!(restored, b"<svg>v1</svg>\n");

    // After the discard, the logo is an incoming delete, as the tip says.
    let status = Status::of(&base, &tip, &target);
    assert_eq!(status.entry(key).unwrap().class, Class::IncomingDelete);

    // A blob the mirror does not have cannot be restored, and says so.
    let err = discard_target(
        &live,
        &Status::of(&base, &tip, &live),
        &[key],
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no longer in the sync mirror"), "{err}");
    Ok(())
}
