//! `scripts/install.sh`: the curl-to-sh installer published beside the release.
//!
//! Run for real against a fake release served from a `file://` URL, with
//! `sudo` on `PATH` replaced by a stub that records what it was asked to run
//! and runs none of it — the install target is the fixed `/opt/feldspar`, which
//! a test may not write. Under the stub, a recorded `install.sh` call *is* the
//! install.
//!
//! Every run names `--variant`: without it the script reads the installed
//! `/opt/feldspar/VARIANT`, which on the machine running the tests is whatever
//! that machine has. Another distribution is stood in for with
//! `FELDSPAR_OS_RELEASE`, and a machine missing a tool with a `PATH` of links
//! that leaves it out.

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
            "--variant",
            "static",
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
        "ID=fedora",
        "ID=arch",
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
    // The tarball's own installer, out of the unpacked tree — which prints its
    // own outcome, and leaves the next step to this script...
    assert!(
        calls[0].ends_with("/feldspar-0.1.0-test/install.sh --no-next-steps"),
        "{calls:#?}"
    );
    // ...and the link onto PATH.
    assert_eq!(calls[1], "mkdir -p /usr/local/bin");
    assert_eq!(
        calls[2],
        "ln -sfn /opt/feldspar/bin/feldspar /usr/local/bin/feldspar"
    );
    assert!(stdout.contains("checksum ok"));
    // It points at the next step — or, where /opt/feldspar already holds an
    // install (the machine running the tests may have one), says the upgrade
    // is the whole job.
    if Path::new("/opt/feldspar/VARIANT").exists() {
        assert!(stdout.contains("The upgrade is done"), "{stdout}");
        assert!(!stdout.contains("restart the service"), "{stdout}");
    } else {
        assert!(
            stdout.contains("feldspar setup --dev") && stdout.contains("feldspar setup --server"),
            "{stdout}"
        );
    }
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

/// Run the installer in `--dry-run` as if on another distribution
/// (`os_release` is the file's contents), with `PATH` limited to links to
/// `tools` plus a sudo that does nothing: the plan is what is checked.
fn dry_run_as(dir: &Path, os_release: &str, tools: &[&str], extra: &[&str]) -> Output {
    let osr = dir.join("os-release");
    fs::write(&osr, os_release).unwrap();
    let bin = dir.join("tools");
    fs::create_dir_all(&bin).unwrap();
    for tool in tools {
        let found = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join(tool))
            .find(|c| c.is_file());
        if let Some(found) = found {
            let link = bin.join(tool);
            if fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(found, link).unwrap();
            }
        }
    }
    if fs::symlink_metadata(bin.join("sudo")).is_err() {
        write_executable(&bin.join("sudo"), "#!/bin/sh\nexit 0\n");
    }
    Command::new(bin.join("sh"))
        .arg(script())
        .arg("--dry-run")
        .args(extra)
        .env("PATH", &bin)
        .env("FELDSPAR_OS_RELEASE", &osr)
        .env("TMPDIR", dir)
        .output()
        .unwrap()
}

/// What every run needs, and no downloader: so the plan installs one.
const BASE_TOOLS: &[&str] = &[
    "sh", "id", "cat", "uname", "mktemp", "rm", "dirname", "grep", "sed", "sort", "head",
    "getconf", "tar", "gzip", "find", "awk", "printf",
];

/// R2.1: Fedora/RHEL, openSUSE and Arch, and their derivatives by ID_LIKE,
/// are known, and get their own package manager.
#[test]
fn each_family_installs_what_is_missing_with_its_own_package_manager() {
    if as_root() {
        eprintln!("skipped: needs a non-root run");
        return;
    }
    let cases = [
        ("ID=fedora\n", "dnf install -y -q curl"),
        (
            "ID=almalinux\nID_LIKE=\"rhel centos fedora\"\n",
            "dnf install -y -q curl",
        ),
        (
            "ID=rocky\nID_LIKE=\"rhel centos fedora\"\n",
            "dnf install -y -q curl",
        ),
        (
            "ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n",
            "zypper --non-interactive --quiet install curl",
        ),
        (
            "ID=opensuse-leap\nID_LIKE=\"suse opensuse\"\n",
            "zypper --non-interactive --quiet install curl",
        ),
        ("ID=arch\n", "pacman -Syu --needed --noconfirm curl"),
        (
            "ID=cachyos\nID_LIKE=arch\n",
            "pacman -Syu --needed --noconfirm curl",
        ),
        (
            "ID=manjaro\nID_LIKE=arch\n",
            "pacman -Syu --needed --noconfirm curl",
        ),
        (
            "ID=endeavouros\nID_LIKE=arch\n",
            "pacman -Syu --needed --noconfirm curl",
        ),
        (
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n",
            "apt-get install -y -qq curl",
        ),
        ("ID=mx\nID_LIKE=debian\n", "apt-get install -y -qq curl"),
        ("ID=alpine\n", "apk add --no-cache curl"),
    ];
    for (os_release, expected) in cases {
        let dir = scratch("family");
        let out = dry_run_as(&dir, os_release, BASE_TOOLS, &["--variant", "static"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{os_release}: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains(expected),
            "{os_release}: expected `{expected}`:\n{stdout}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // A minimal container's missing find and awk are installed too, under the
    // family's names; the tools that are here are not.
    let dir = scratch("family-tools");
    let tools: Vec<&str> = BASE_TOOLS
        .iter()
        .copied()
        .filter(|t| *t != "find" && *t != "awk")
        .chain(["curl"])
        .collect();
    let out = dry_run_as(&dir, "ID=fedora\n", &tools, &["--variant", "static"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dnf install -y -q findutils gawk"),
        "{stdout}"
    );
    fs::remove_dir_all(&dir).ok();

    // And what it does not know is refused, naming what it does.
    let dir = scratch("family-unknown");
    let out = dry_run_as(&dir, "ID=gentoo\n", BASE_TOOLS, &["--variant", "static"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Fedora, RHEL, openSUSE, Arch and Alpine") && stderr.contains("'gentoo'"),
        "{stderr}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// `--variant linux` fetches the artifact with Python, and the plan says which
/// libc it will run on; `--no-restart` reaches the tarball's installer.
#[test]
fn the_linux_variant_is_fetched_and_its_libc_forecast() {
    if as_root() {
        eprintln!("skipped: needs a non-root run");
        return;
    }
    let dir = scratch("linux");
    let tools: Vec<&str> = BASE_TOOLS.iter().copied().chain(["curl", "ldd"]).collect();
    let out = dry_run_as(
        &dir,
        "ID=debian\n",
        &tools,
        &["--variant", "linux", "--no-restart"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("feldspar-linux-x86_64.tar.gz -> "),
        "{stdout}"
    );
    assert!(
        stdout.contains("install.sh --no-next-steps --no-restart"),
        "{stdout}"
    );
    let libc = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("libc: "))
        .unwrap_or_else(|| panic!("no libc line:\n{stdout}"));
    // This machine's glibc decides which of the two; either is a forecast the
    // tarball's installer then makes for real.
    assert!(
        libc.contains("the host's glibc") || libc.contains("the bundled glibc"),
        "{libc}"
    );

    // A musl host gets the bundled glibc.
    let ldd = dir.join("tools/ldd");
    fs::remove_file(&ldd).ok();
    write_executable(&ldd, "#!/bin/sh\necho 'musl libc (x86_64)' >&2\nexit 1\n");
    let out = dry_run_as(&dir, "ID=alpine\n", &tools, &["--variant", "linux"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("libc: the bundled glibc (this is a musl host)"),
        "{stdout}"
    );

    // The static one needs none, and an unknown variant is refused.
    let out = dry_run_as(&dir, "ID=debian\n", &tools, &["--variant", "static"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("feldspar.tar.gz -> ") && stdout.contains("libc: none needed"),
        "{stdout}"
    );
    let out = dry_run_as(&dir, "ID=debian\n", &tools, &["--variant", "musl"]);
    assert!(!out.status.success());
    fs::remove_dir_all(&dir).ok();
}

/// A `file://` base URL is a directory — for a mirror, or the build's smoke
/// test — and works with busybox's wget alone, which cannot fetch one.
#[test]
fn a_file_url_release_installs_with_wget_alone() {
    if as_root() {
        eprintln!("skipped: needs a non-root run");
        return;
    }
    let dir = scratch("file-url");
    let release = fake_release(&dir);
    let log = dir.join("sudo.log");
    let tools: Vec<&str> = BASE_TOOLS
        .iter()
        .copied()
        .chain(["cp", "sha256sum", "wget"])
        .collect();
    // Builds the PATH of links (and checks that the plan runs on it).
    let out = dry_run_as(&dir, "ID=alpine\n", &tools, &["--variant", "static"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::remove_file(dir.join("tools/sudo")).unwrap();
    write_executable(
        &dir.join("tools/sudo"),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
    );
    let out = Command::new(dir.join("tools/sh"))
        .arg(script())
        .args(["--base-url", &format!("file://{}", release.display())])
        .args(["--no-packages", "--variant", "static"])
        .env("PATH", dir.join("tools"))
        .env("FELDSPAR_OS_RELEASE", dir.join("os-release"))
        .env("TMPDIR", &dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("checksum ok"), "{stdout}");
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .contains("feldspar-0.1.0-test/install.sh --no-next-steps"),
    );
    fs::remove_dir_all(&dir).ok();
}
