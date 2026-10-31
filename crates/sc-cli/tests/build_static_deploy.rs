//! `scripts/build-static.sh --deploy HOST`: the copy-unpack-install step that
//! runs after the tarball is written.
//!
//! The build itself is not what is under test here — compiling V8 to check a
//! `scp` would be absurd — so this drives the script inside a throwaway
//! "repository" (the script, and the `Cargo.toml` it reads the version from)
//! with a `PATH` in front of it holding stubs for the commands the deploy path
//! cannot really run: `cargo` (writes a one-line executable where the build would
//! have left the binary), `rustup`, `ssh`, `sudo` — and `systemctl`, which the
//! deploy asks whether the service is running and which a test must never reach
//! for real.
//!
//! The `ssh` stub is what makes this a real test rather than a spelling check:
//! it runs the command it was handed **locally**, so the remote half of the
//! deploy — the unpack, `install.sh`, the smoke run of the installed binary, the
//! cleanup — actually executes, against a `--prefix` and a `--remote-tmp` inside
//! the test's temporary directory. What the assertions then look at is the
//! installed tree, not the script's text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file()
            && fs::read_to_string(&manifest)
                .unwrap_or_default()
                .contains("[workspace]")
        {
            return dir;
        }
        assert!(dir.pop(), "reached / without finding the workspace root");
    }
}

/// A unique scratch directory, removed by the caller on success (and left behind
/// on failure, where its contents are the evidence).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feldspar-deploy-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

fn write_executable(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The throwaway repository, and the stub `PATH` in front of it. `systemctl` is
/// the body of that stub, in `sh`, so each test decides what the "remote"
/// machine's service manager reports.
struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
    /// Every ssh argv, one per line.
    log: PathBuf,
    /// Every systemctl call that was not a query, one per line.
    events: PathBuf,
    prefix: PathBuf,
    remote_tmp: PathBuf,
}

const TARGET: &str = "x86_64-unknown-linux-gnu";

fn fixture(name: &str) -> Fixture {
    let root = workspace_root();
    let dir = scratch(name);

    // The repository the script thinks it is in: itself, plus the Cargo.toml it
    // reads the workspace version from. Not a git checkout, so the artifact name
    // is version-only and this test does not depend on the state of the tree.
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("scripts")).unwrap();
    fs::copy(
        root.join("scripts/build-static.sh"),
        repo.join("scripts/build-static.sh"),
    )
    .expect("copy build-static.sh");
    // The packaging step puts this in the artifact, so the throwaway repository
    // has to have one.
    fs::copy(
        root.join("scripts/setup-host.sh"),
        repo.join("scripts/setup-host.sh"),
    )
    .expect("copy setup-host.sh");
    // And the tree installer it ships as install.sh, which is what the deploy
    // runs on the far side.
    fs::create_dir_all(repo.join("scripts/lib")).unwrap();
    fs::copy(
        root.join("scripts/lib/install-tree.sh"),
        repo.join("scripts/lib/install-tree.sh"),
    )
    .expect("copy install-tree.sh");
    // And the release library the script sources, whether or not it publishes.
    fs::copy(
        root.join("scripts/lib/release.sh"),
        repo.join("scripts/lib/release.sh"),
    )
    .expect("copy release.sh");
    fs::copy(root.join("Cargo.toml"), repo.join("Cargo.toml")).unwrap();
    // And the bundled modules, which the packaging step also puts in the
    // artifact. One entry is enough to exercise the copy; the real catalog is
    // `sc-module`'s to assert.
    let plugins = repo.join("plugins/rss");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(plugins.join("feldspar-module.json"), "{}").unwrap();

    let bin = dir.join("bin");
    let log = dir.join("ssh.log");
    let events = dir.join("systemctl.log");

    // `cargo build --release --target T -p sc-cli`, minus the compiler: leave an
    // executable where the real build would have left one. It prints usage and
    // exits 0, which is what the deploy's smoke run checks for.
    write_executable(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\nset -eu\nout=\"${{CARGO_TARGET_DIR:-$PWD/target}}/{TARGET}/release\"\n\
             mkdir -p \"$out\"\n\
             printf '#!/bin/sh\\necho feldspar usage\\n' > \"$out/feldspar\"\n\
             chmod +x \"$out/feldspar\"\n"
        ),
    );
    write_executable(
        &bin.join("rustup"),
        &format!("#!/bin/sh\n[ \"${{1:-}}\" = target ] && echo {TARGET}\nexit 0\n"),
    );
    // The remote, played locally: record the argv, then run the command the
    // script sent (the last argument) with its stdin attached, exactly as the
    // far side of an ssh would.
    write_executable(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nset -eu\n\
             printf '%s\\n' \"$*\" >> {log}\n\
             for cmd; do :; done\n\
             exec sh -c \"$cmd\"\n",
            log = log.display()
        ),
    );
    // install.sh is run through sudo when the login is not root, which it is not
    // here; the prefix is writable, so the stub only has to get out of the way.
    write_executable(&bin.join("sudo"), "#!/bin/sh\nexec \"$@\"\n");

    // `--prefix`'s *parent* has to exist and be writable, or install.sh asks to
    // be re-run as root.
    let prefix = dir.join("opt/feldspar");
    fs::create_dir_all(prefix.parent().unwrap()).unwrap();

    let f = Fixture {
        remote_tmp: dir.join("remote-tmp"),
        dir,
        repo,
        bin,
        log,
        events,
        prefix,
    };
    // The default remote has no service running, so the deploy is an install and
    // nothing else. A test that wants one calls `systemctl` again.
    f.systemctl(false, 0);
    f
}

impl Fixture {
    fn deploy(&self) -> (std::process::Output, String, String) {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new("bash")
            .arg(self.repo.join("scripts/build-static.sh"))
            .args([
                "--native",
                "--no-ui",
                "--no-strip",
                "--no-verify",
                "--variant",
                "static",
            ])
            .arg("--prefix")
            .arg(&self.prefix)
            .arg("--remote-tmp")
            .arg(&self.remote_tmp)
            .arg("--output")
            .arg(self.dir.join("dist"))
            .args(["--ssh-opt", "-oStrictHostKeyChecking=no", "--deploy", "vm"])
            .env("PATH", &path)
            .output()
            .expect("run build-static.sh");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output, stdout, stderr)
    }

    /// What the systemctl stub recorded, in the order it was called.
    fn systemctl_calls(&self) -> Vec<String> {
        fs::read_to_string(&self.events)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Rewrite the fixture's `systemctl` stub. `active` decides whether
/// `list-units` — how the tree's install.sh finds running feldspar units —
/// answers with `feldspar.service`, and `stop_status` the exit code of `stop`;
/// every call that is not a query is appended to the log together with the
/// second line of the installed binary as it was at that moment — the line that
/// differs between the old and the new one — which is how the ordering against
/// the swap is checked without timestamps.
impl Fixture {
    fn systemctl(&self, active: bool, stop_status: i32) {
        let stub = format!(
            "#!/bin/sh\n\
             events='{events}'\n\
             installed='{prefix}/bin/feldspar'\n\
             seen=\"$(sed -n 2p \"$installed\" 2>/dev/null || true)\"\n\
             case \"${{1:-}}\" in\n\
             list-units) {list_units} ;;\n\
             stop) echo \"stop [$seen]\" >> \"$events\"; exit {stop_status} ;;\n\
             start) echo \"start [$seen]\" >> \"$events\"; exit 0 ;;\n\
             esac\n\
             exit 0\n",
            events = self.events.display(),
            prefix = self.prefix.display(),
            list_units = if active {
                "echo 'feldspar.service loaded active running Saltcorn Feldspar'"
            } else {
                ":"
            },
        );
        write_executable(&self.bin.join("systemctl"), &stub);
    }

    /// The prefix as an upgrade finds it: a previous release already installed
    /// and — as far as the stub above is concerned — running.
    fn pretend_already_installed(&self) {
        write_executable(
            &self.prefix.join("bin/feldspar"),
            "#!/bin/sh\necho previous release\n",
        );
    }
}

#[test]
fn deploy_copies_unpacks_and_installs_over_ssh() {
    // Nothing running on the far side: the deploy is the install and nothing else.
    let f = fixture("ok");
    let (dir, remote_tmp, prefix, log) = (&f.dir, &f.remote_tmp, &f.prefix, &f.log);
    let (output, stdout, stderr) = f.deploy();

    assert!(
        output.status.success(),
        "build-static.sh --deploy failed ({})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );

    // 1. It installed: the tree is at the prefix on the "remote", and the binary
    //    is executable there.
    let installed = prefix.join("bin/feldspar");
    assert!(
        installed.is_file(),
        "nothing was installed at {installed:?}\n{stdout}\n{stderr}"
    );
    assert!(
        fs::metadata(&installed).unwrap().permissions().mode() & 0o111 != 0,
        "the installed binary is not executable"
    );

    // 2. The host installer came with it, executable, beside the tree rather than
    //    on PATH: `sudo /opt/feldspar/setup-host.sh` is the next thing a new host
    //    runs, and it has no checkout to get it from.
    let setup = prefix.join("setup-host.sh");
    assert!(
        setup.is_file(),
        "the artifact should install {setup:?}\n{stdout}\n{stderr}"
    );
    assert!(
        fs::metadata(&setup).unwrap().permissions().mode() & 0o111 != 0,
        "the installed setup-host.sh is not executable"
    );
    assert_eq!(
        fs::read_to_string(&setup).unwrap(),
        fs::read_to_string(workspace_root().join("scripts/setup-host.sh")).unwrap(),
        "the installed setup-host.sh should be this repository's",
    );
    assert!(
        !prefix.join("bin/setup-host.sh").exists(),
        "setup-host.sh does not belong on PATH; it is run once"
    );

    // 2a. And the bundled modules, at the path the binary was compiled to look
    //     for them (`$SC_BUNDLE_PREFIX/plugins`). An artifact without them is a
    //     Modules tab with an empty catalog and no way to fill it.
    let manifest = prefix.join("plugins/rss/feldspar-module.json");
    assert!(
        manifest.is_file(),
        "the artifact should install {manifest:?}\n{stdout}\n{stderr}"
    );

    // 3. It cleaned up after itself: the tarball, the unpacked tree and the
    //    generated remote script are all gone from the staging directory.
    let leftovers: Vec<String> = fs::read_dir(remote_tmp)
        .expect("the remote staging directory should still exist")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the deploy left {leftovers:?} in {remote_tmp:?}"
    );

    // 4. The artifact is still written locally — a deploy is in addition to the
    //    tarball, not instead of it.
    let tarballs: Vec<String> = fs::read_dir(dir.join("dist"))
        .expect("dist")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tar.gz"))
        .collect();
    assert_eq!(tarballs.len(), 1, "expected one tarball, got {tarballs:?}");

    // 5. What went over the wire, in order: the reachability probe before the
    //    build, the tarball, the remote script, and the run of that script. And
    //    --ssh-opt reached every one of them.
    let sent = fs::read_to_string(log).expect("the ssh stub should have logged");
    let calls: Vec<&str> = sent.lines().collect();
    assert!(
        calls.len() >= 4,
        "expected a probe, two copies and a run, got {calls:?}"
    );
    assert!(
        calls[0].contains("BatchMode=yes"),
        "the first ssh should be the pre-build reachability probe: {}",
        calls[0]
    );
    assert!(
        calls.iter().any(|c| c.contains(".tar.gz")),
        "the tarball was never sent: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("sh ") && c.contains(".deploy.sh")),
        "the remote script was never run: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|c| c.contains("-oStrictHostKeyChecking=no")),
        "--ssh-opt must be passed to every ssh invocation: {calls:?}"
    );
    assert!(
        stdout.contains("installed") && stdout.contains("vm"),
        "the script should report where it installed:\n{stdout}"
    );

    // 6. A service that was not running is not touched: no stop, and above all
    //    no start of something the operator had deliberately left down.
    assert!(
        f.systemctl_calls().is_empty(),
        "an inactive unit should be left alone, got {:?}",
        f.systemctl_calls()
    );

    fs::remove_dir_all(dir).ok();
}

/// The reason this exists: a running server holds its own executable open, and
/// `cp` over it fails with ETXTBSY. The unit is stopped between the unpack and
/// the install, and started again once the new binary is in place.
#[test]
fn deploy_stops_a_running_service_around_the_install() {
    let f = fixture("running");
    f.systemctl(true, 0);
    f.pretend_already_installed();

    let (output, stdout, stderr) = f.deploy();
    assert!(
        output.status.success(),
        "deploy over a running service failed ({})\n{stdout}\n{stderr}",
        output.status
    );

    // The stub logs the first line of the installed binary at the moment it is
    // called, so the log alone says on which side of the install each call fell:
    // the stop saw the previous release, the start saw the new one.
    let calls = f.systemctl_calls();
    assert_eq!(
        calls.len(),
        2,
        "expected exactly a stop and a start, got {calls:?}"
    );
    assert_eq!(
        calls[0], "stop [echo previous release]",
        "the stop must come while the previous release is still installed: {calls:?}"
    );
    assert_eq!(
        calls[1], "start [echo feldspar usage]",
        "the service should be started again, after the new binary is in place: {calls:?}"
    );

    // Belt and braces on the ordering: what is installed now is the new build.
    let installed = fs::read_to_string(f.prefix.join("bin/feldspar")).unwrap();
    assert!(
        installed.contains("feldspar usage"),
        "the new binary should have replaced the old one, found: {installed}"
    );

    fs::remove_dir_all(&f.dir).ok();
}

/// A stop that fails is not fatal — the install goes ahead — and it must not be
/// followed by a start, which would leave the machine running something the
/// deploy never managed to stop.
#[test]
fn a_failed_stop_does_not_abort_the_install_or_start_the_service() {
    let f = fixture("stop-fails");
    f.systemctl(true, 1);
    f.pretend_already_installed();
    let (output, stdout, stderr) = f.deploy();
    assert!(
        output.status.success(),
        "a failed stop should not fail the deploy ({})\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("could not stop"),
        "the failed stop should be reported:\n{stderr}"
    );

    let calls = f.systemctl_calls();
    assert_eq!(
        calls.len(),
        1,
        "a failed stop must not be followed by a start: {calls:?}"
    );
    assert!(
        f.prefix.join("bin/feldspar").is_file(),
        "it should still have installed"
    );

    fs::remove_dir_all(&f.dir).ok();
}

/// The options that cannot work are refused before the build, not after an hour
/// of it: a `--deploy` with no host, and a relative `--remote-tmp`.
#[test]
fn unusable_deploy_options_are_refused_up_front() {
    let script = workspace_root().join("scripts/build-static.sh");

    let missing_value = Command::new("bash")
        .arg(&script)
        .arg("--deploy")
        .output()
        .expect("run build-static.sh");
    assert_eq!(missing_value.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&missing_value.stderr).contains("--deploy needs a value"),
        "a --deploy with no host should say so"
    );

    let relative_tmp = Command::new("bash")
        .arg(&script)
        .args(["--deploy", "vm", "--remote-tmp", "tmp"])
        .output()
        .expect("run build-static.sh");
    assert_eq!(relative_tmp.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&relative_tmp.stderr).contains("--remote-tmp must be absolute"),
        "a relative --remote-tmp should be refused"
    );

    // Deploying an artifact this run does not build cannot work.
    let unbuilt = Command::new("bash")
        .arg(&script)
        .args([
            "--variant",
            "static",
            "--deploy",
            "vm",
            "--deploy-variant",
            "python",
        ])
        .output()
        .expect("run build-static.sh");
    assert_eq!(unbuilt.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unbuilt.stderr).contains("is not being built"),
        "{}",
        String::from_utf8_lossy(&unbuilt.stderr)
    );

    // And the deploy options are documented, since --help is where an operator
    // finds them.
    let help = Command::new("bash")
        .arg(&script)
        .arg("--help")
        .output()
        .expect("run build-static.sh --help");
    let help = String::from_utf8_lossy(&help.stdout);
    for option in [
        "--deploy",
        "--ssh-opt",
        "--remote-tmp",
        "--deploy-variant",
        "--variant",
    ] {
        assert!(help.contains(option), "--help should document {option}");
    }
}
