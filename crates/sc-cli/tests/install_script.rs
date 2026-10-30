//! `scripts/install.sh`: the curl-to-sh installer published beside the release.
//!
//! Run for real against a fake release served from a `file://` URL, with
//! `sudo` on `PATH` replaced by a stub that records what it was asked to run
//! and runs none of it — the install target is the fixed `/opt/feldspar`, which
//! a test may not write. Under the stub, a recorded `install.sh` call *is* the
//! install.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feldspar-install-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_executable(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A release directory as the bucket holds it: `feldspar.tar.gz` (a
/// `feldspar-<version>/` tree with an install.sh) and its checksum.
fn fake_release(dir: &Path) -> PathBuf {
    let tree = dir.join("build/feldspar-0.1.0-test");
    write_executable(&tree.join("bin/feldspar"), "#!/bin/sh\necho feldspar\n");
    write_executable(&tree.join("install.sh"), "#!/bin/sh\necho installed\n");
    let release = dir.join("release");
    fs::create_dir_all(&release).unwrap();
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(release.join("feldspar.tar.gz"))
        .arg("-C")
        .arg(dir.join("build"))
        .arg("feldspar-0.1.0-test")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let sum = Command::new("sha256sum")
        .arg("feldspar.tar.gz")
        .current_dir(&release)
        .output()
        .unwrap();
    fs::write(release.join("feldspar.tar.gz.sha256"), sum.stdout).unwrap();
    release
}

/// Run the installer with a recording `sudo` first on PATH.
fn install(dir: &Path, release: &Path, extra: &[&str]) -> (Output, String) {
    let bin = dir.join("bin");
    let log = dir.join("sudo.log");
    write_executable(
        &bin.join("sudo"),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = Command::new("sh")
        .arg(script())
        .args([
            "--base-url",
            &format!("file://{}", release.display()),
            "--no-packages",
        ])
        .args(extra)
        .env("PATH", path)
        .env("TMPDIR", dir)
        .output()
        .unwrap();
    (out, fs::read_to_string(&log).unwrap_or_default())
}

fn as_root() -> bool {
    // A root test run would not go through the sudo stub, and would install.
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
}

fn on_a_known_distribution() -> bool {
    let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    [
        "ID=debian",
        "ID=ubuntu",
        "ID=alpine",
        "ID_LIKE=debian",
        "ID_LIKE=\"ubuntu debian\"",
    ]
    .iter()
    .any(|l| release.lines().any(|r| r == *l))
}

#[test]
fn installs_the_downloaded_tree_and_links_feldspar_onto_path() {
    if as_root() || !on_a_known_distribution() {
        eprintln!("skipped: needs a non-root run on Debian, Ubuntu or Alpine");
        return;
    }
    let dir = scratch("ok");
    let release = fake_release(&dir);
    let (out, sudo) = install(&dir, &release, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "install.sh failed\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls: Vec<&str> = sudo.lines().collect();
    assert_eq!(calls.len(), 3, "{calls:#?}");
    // The tarball's own installer, out of the unpacked tree...
    assert!(
        calls[0].ends_with("/feldspar-0.1.0-test/install.sh"),
        "{calls:#?}"
    );
    // ...and the link onto PATH.
    assert_eq!(calls[1], "mkdir -p /usr/local/bin");
    assert_eq!(
        calls[2],
        "ln -sfn /opt/feldspar/bin/feldspar /usr/local/bin/feldspar"
    );
    assert!(stdout.contains("checksum ok"));
    // It points at the next step.
    assert!(stdout.contains("feldspar setup --dev") && stdout.contains("feldspar setup --server"));
    // And cleans up after itself.
    assert!(
        !fs::read_dir(&dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("feldspar-install.")),
        "the download directory was left behind"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_download_that_does_not_match_its_checksum_installs_nothing() {
    if as_root() || !on_a_known_distribution() {
        eprintln!("skipped: needs a non-root run on Debian, Ubuntu or Alpine");
        return;
    }
    let dir = scratch("bad-sum");
    let release = fake_release(&dir);
    fs::write(
        release.join("feldspar.tar.gz.sha256"),
        format!("{}  feldspar.tar.gz\n", "0".repeat(64)),
    )
    .unwrap();
    let (out, sudo) = install(&dir, &release, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum"));
    assert!(
        sudo.is_empty(),
        "nothing should have been installed: {sudo}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dry_run_downloads_and_installs_nothing() {
    if as_root() || !on_a_known_distribution() {
        eprintln!("skipped: needs a non-root run on Debian, Ubuntu or Alpine");
        return;
    }
    let dir = scratch("dry");
    let (out, sudo) = install(&dir, &dir.join("no-release-here"), &["--dry-run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sudo.is_empty(), "{sudo}");
    assert!(stdout.contains("ln -sfn /opt/feldspar/bin/feldspar /usr/local/bin/feldspar"));
    fs::remove_dir_all(&dir).ok();
}

/// It is published as plain `sh`, and must stay that way: Alpine has no bash.
#[test]
fn the_installer_is_posix_sh_and_documented() {
    let text = fs::read_to_string(script()).unwrap();
    assert!(text.starts_with("#!/bin/sh\n"));
    assert!(
        !text.contains("[["),
        "a bashism ([[) in a script Alpine's sh runs"
    );
    let mode = fs::metadata(script()).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "scripts/install.sh should be executable");
    let ok = Command::new("sh")
        .arg("-n")
        .arg(script())
        .status()
        .unwrap()
        .success();
    assert!(ok, "sh -n rejects scripts/install.sh");

    let readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md")).unwrap();
    assert!(
        readme.contains("curl -fsSL https://feldspar-latest-static.saltcorn.com/install.sh | sh"),
        "README should give the one-line install"
    );
}
