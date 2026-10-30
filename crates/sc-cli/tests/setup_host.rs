//! `scripts/setup-host.sh`: the host side of an installation — packages, the
//! service account, the database, `feldspar.toml` and the systemd unit.
//!
//! Two kinds of test, because the script does two kinds of thing. The files it
//! writes and the unit it generates are checked by **running it for real** with
//! `--config` and `--unit` pointing into a temporary directory and stubs on
//! `PATH` for the commands that need a machine to act on (`sudo`, `apt-get`,
//! `systemctl`, `adduser`, `chown`, `ln`); the assertions are then on the files
//! that actually appeared and on the log the stubs kept of how they were called.
//! What cannot be run under a test user — installing PostgreSQL, building from
//! source — is checked through `--dry-run`, which prints the same plan the
//! script would execute because it is the same code path with the doing removed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn script() -> PathBuf {
    workspace_root().join("scripts/setup-host.sh")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feldspar-setup-host-{name}-{}-{}",
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

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run the script under `sh` with the given arguments and no stubs — enough for
/// `--help`, argument errors and `--dry-run`.
fn run_plain(args: &[&str]) -> Output {
    Command::new("sh")
        .arg(script())
        .args(args)
        .output()
        .expect("run setup-host.sh")
}

/// The commands a machine would answer, replaced by stubs that record how they
/// were called. `sudo` is the interesting one: it drops a leading `-u NAME` and
/// runs the rest, so everything the script does "as root" still happens, in this
/// directory, as this user.
fn stub_dir(dir: &Path) -> (PathBuf, PathBuf) {
    let bin = dir.join("bin");
    let log = dir.join("commands.log");
    let record = |name: &str, body: &str| {
        write_executable(
            &bin.join(name),
            &format!(
                "#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> {log}\n{body}",
                log = log.display()
            ),
        )
    };
    record("apt-get", "exit 0\n");
    record("adduser", "exit 0\n");
    // The npm the script asks its version, answered by a stub rather than by
    // whatever this machine happens to have: the answer decides whether Node is
    // installed from NodeSource, and a test that installs an apt repository on
    // the machine it runs on — or does not, depending on the developer's npm —
    // is no test at all. A new enough one here; §1b's other branch is
    // `an_npm_too_old_to_install_a_module_is_replaced_from_nodesource`.
    record("npm", "echo 11.12.1\n");
    record("systemctl", "exit 0\n");
    record("chown", "exit 0\n");
    record("ln", "exit 0\n");
    // The browser `view_app` drives, and the service account it is checked
    // under — stubs for the reason npm is one: what this machine has installed
    // must not decide what the script does. `runuser` drops `-u NAME --`.
    record(
        "chromium",
        "case \"$*\" in *--dump-dom*) echo '<html><head></head><body></body></html>' ;; esac\n",
    );
    record("runuser", "shift 3\nexec \"$@\"\n");
    write_executable(
        &bin.join("sudo"),
        &format!(
            "#!/bin/sh\nprintf 'sudo %s\\n' \"$*\" >> {log}\n\
             [ \"${{1:-}}\" = -u ] && shift 2\nexec \"$@\"\n",
            log = log.display()
        ),
    );
    (bin, log)
}

#[test]
fn a_static_install_writes_the_config_and_a_unit_pointing_at_the_deployed_binary() {
    let dir = scratch("static");
    let (bin, log) = stub_dir(&dir);
    let prefix = dir.join("opt/feldspar");
    let config = dir.join("etc/feldspar/feldspar.toml");
    let unit = dir.join("etc/systemd/system/feldspar.service");

    // The binary build-static.sh --deploy would have left; here, something that
    // exists and is executable, which is all the script looks for.
    write_executable(&prefix.join("bin/feldspar"), "#!/bin/sh\nexit 0\n");

    let out = Command::new("sh")
        .arg(script())
        .args(["--static", "--domain", "example.com"])
        .arg("--database-url")
        .arg("postgres://feldspar:pw@db.internal/feldspar")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--config")
        .arg(&config)
        .arg("--unit")
        .arg(&unit)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .expect("run setup-host.sh");
    let stdout = stdout_of(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the static setup failed ({})\n{stdout}\n{stderr}",
        out.status
    );

    // The configuration file: the remote database as a url, the base domain, and
    // 0640 because a file like this may hold a password.
    let toml = fs::read_to_string(&config).expect("the configuration file should exist");
    assert!(
        toml.contains(r#"url = "postgres://feldspar:pw@db.internal/feldspar""#),
        "--database-url should go in as a url:\n{toml}"
    );
    assert!(toml.contains(r#"base_domain = "example.com""#), "{toml}");
    assert!(
        toml.contains(r#"default_environment = "production""#),
        "{toml}"
    );
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o640,
        "the configuration file must not be readable outside the service's group"
    );

    // The unit: the deployed binary, the config file this run wrote, and the
    // hardening that makes /var/lib/feldspar the one writable path.
    let service = fs::read_to_string(&unit).expect("the unit should exist");
    for line in [
        &format!(
            "ExecStart={}/bin/feldspar serve --environment production",
            prefix.display()
        ),
        &format!("Environment=FELDSPAR_CONFIG={}", config.display()),
        "Type=notify",
        "User=feldspar",
        "StateDirectory=feldspar",
        "ProtectSystem=strict",
        "ReadWritePaths=/var/lib/feldspar",
        "Environment=HOME=/var/lib/feldspar",
        "WorkingDirectory=/var/lib/feldspar",
        "AmbientCapabilities=CAP_NET_BIND_SERVICE",
    ] {
        assert!(
            service.contains(line),
            "the unit should contain {line}:\n{service}"
        );
    }

    // What it did to the machine: no toolchain, no PostgreSQL (the database is
    // elsewhere), the unit reloaded and started because the binary is there.
    let commands = fs::read_to_string(&log).expect("the stubs should have logged");
    let install = commands
        .lines()
        .find(|l| l.starts_with("apt-get") && l.contains("install"))
        .unwrap_or_else(|| panic!("no apt-get install in:\n{commands}"));
    for package in ["ca-certificates", "curl"] {
        assert!(
            install.contains(package),
            "{package} should be installed: {install}"
        );
    }
    for package in ["build-essential", "libclang-dev", "postgresql"] {
        assert!(
            !install.contains(package),
            "a --static install with a remote database should not install {package}: {install}"
        );
    }
    // Node is not an apt package here: this machine's npm — the stub's 11.12.1
    // — is new enough, so §1b leaves the toolchain alone rather than adding an
    // apt repository to a host that does not need one.
    assert!(
        !commands.contains("deb.nodesource.com"),
        "a host with a new enough npm should not get the NodeSource repository:\n{commands}"
    );
    assert!(
        stdout.contains("leaving Node alone"),
        "it should say the npm that is there is kept:\n{stdout}"
    );
    // The browser on PATH is kept, and started once as the service account.
    assert!(
        stdout.contains("a headless-capable browser is already here:"),
        "{stdout}"
    );
    assert!(
        commands
            .lines()
            .any(|l| l.starts_with("runuser -u feldspar -- ")
                && l.contains("--headless")
                && l.contains("--dump-dom about:blank")),
        "the browser should be checked as the service account:\n{commands}"
    );
    assert!(
        stdout.contains("runs headless as feldspar\n    it does"),
        "{stdout}"
    );
    assert!(commands.contains("systemctl daemon-reload"), "{commands}");
    assert!(
        commands.contains("systemctl enable --now feldspar.service"),
        "the unit should be enabled and started when the binary is there:\n{commands}"
    );
    assert!(
        stdout.contains("feldspar is running"),
        "it should report the service is up:\n{stdout}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// The other order of the two-step install: this script first, the deploy after.
/// Nothing may be started, and the operator has to be told what is missing.
#[test]
fn a_static_install_without_a_binary_yet_enables_but_does_not_start() {
    let dir = scratch("static-no-binary");
    let (bin, log) = stub_dir(&dir);
    let prefix = dir.join("opt/feldspar");

    let out = Command::new("sh")
        .arg(script())
        .args(["--static", "--database-url", "postgres://x/y"])
        .arg("--prefix")
        .arg(&prefix)
        .arg("--config")
        .arg(dir.join("etc/feldspar.toml"))
        .arg("--unit")
        .arg(dir.join("feldspar.service"))
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .expect("run setup-host.sh");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let commands = fs::read_to_string(&log).unwrap();
    assert!(
        commands.contains("systemctl enable feldspar.service"),
        "the unit should still be enabled for the next boot:\n{commands}"
    );
    assert!(
        !commands.contains("--now"),
        "a server whose binary is not there yet must not be started:\n{commands}"
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("build-static.sh --deploy"),
        "it should name the command that puts a binary there:\n{stdout}"
    );
    // And no --domain: the operator is told what that costs, rather than getting
    // a server that silently serves nothing but the admin UI.
    assert!(
        stdout.contains("no --domain"),
        "a missing --domain should be called out:\n{stdout}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// §1b: an npm that cannot install a module is replaced, and the distribution's
/// packages are not how Node gets here.
///
/// Debian 12, Debian 13 and Ubuntu 24.04 all package npm 9.2.0, which fails *every*
/// module install with `Invalid comparator: file:…` — the modules directory
/// depends on the v1 API stubs at a `file:` path and overrides the same names,
/// and npm before 9.3.0 hands that path to semver. A host set up by this script
/// must not land there, so `nodejs`/`npm` are not in the apt package list and
/// NodeSource is added instead. Checked as a plan, because adding an apt
/// repository is not something a test may do to the machine it runs on.
#[test]
fn an_npm_too_old_to_install_a_module_is_replaced_from_nodesource() {
    let dir = scratch("nodesource");
    let (bin, _log) = stub_dir(&dir);
    // Debian 12's and Ubuntu 24.04's npm, in front of whatever this machine has.
    write_executable(&bin.join("npm"), "#!/bin/sh\necho 9.2.0\n");
    let config = dir.join("feldspar.toml");
    let unit = dir.join("feldspar.service");

    let out = Command::new("sh")
        .arg(script())
        .args(["--dry-run", "--static", "--domain", "example.com"])
        .arg("--config")
        .arg(&config)
        .arg("--unit")
        .arg(&unit)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .expect("run setup-host.sh");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan = stdout_of(&out);

    // Why, in the version the admin has and the one that is needed.
    assert!(plan.contains("npm 9.2.0 is too old"), "{plan}");
    assert!(plan.contains("9.3.0"), "{plan}");
    // And how: the key, the repository, the pin that keeps a distribution
    // package from taking it back, and the install.
    for step in [
        "deb.nodesource.com/gpgkey/nodesource-repo.gpg.key",
        "signed-by=/etc/apt/keyrings/nodesource.asc",
        "https://deb.nodesource.com/node_26.x nodistro main",
        "Pin-Priority: 600",
        "apt-get install -y nodejs",
    ] {
        assert!(
            plan.contains(step),
            "the plan should include `{step}`:\n{plan}"
        );
    }
    // Not from the distribution, whose npm is the whole problem. NodeSource's
    // `nodejs` carries its own and conflicts with the `npm` package.
    let packages = plan
        .lines()
        .find(|line| line.contains("installing packages:"))
        .unwrap_or_else(|| panic!("no package list in:\n{plan}"));
    assert!(!packages.contains("nodejs"), "{packages}");
    assert!(!packages.contains("npm"), "{packages}");

    fs::remove_dir_all(&dir).ok();
}

/// What a test user cannot run — installing PostgreSQL, creating a role,
/// rustup and a release build — is checked as a plan.
#[test]
fn the_plan_differs_between_a_source_install_and_a_static_one() {
    let dir = scratch("plan");
    let config = dir.join("feldspar.toml");
    let unit = dir.join("feldspar.service");
    let common = [
        "--dry-run",
        "--domain",
        "example.com",
        "--config",
        config.to_str().unwrap(),
        "--unit",
        unit.to_str().unwrap(),
    ];

    let mut source_args = common.to_vec();
    source_args.extend(["--src", "/opt/feldspar/src"]);
    let source = run_plain(&source_args);
    assert!(source.status.success());
    let source = stdout_of(&source);
    for expected in [
        "build-essential",
        "libclang-dev",
        "postgresql",
        "createuser feldspar",
        "createdb -O feldspar feldspar",
        "git clone --branch main",
        "cargo build --release -p sc-cli",
        "install -m 0755 /opt/feldspar/src/target/release/feldspar /usr/local/bin/feldspar",
        "ExecStart=/usr/local/bin/feldspar serve --environment production",
    ] {
        assert!(
            source.contains(expected),
            "a source install should plan `{expected}`:\n{source}"
        );
    }

    let mut static_args = common.to_vec();
    static_args.push("--static");
    let statically = run_plain(&static_args);
    assert!(statically.status.success());
    let statically = stdout_of(&statically);
    for absent in ["cargo build", "git clone", "libclang-dev", "rustup"] {
        assert!(
            !statically.contains(absent),
            "--static must not plan `{absent}`:\n{statically}"
        );
    }
    // It still accounts for npm, which is a *run-time* dependency: the server
    // shells out to it to build an application or install a module. Which of
    // §1b's two branches this is depends on the machine the test runs on — the
    // npm here is kept, or NodeSource's replaces it — and the plan has to say
    // one of them either way.
    assert!(
        statically.contains("leaving Node alone") || statically.contains("from NodeSource"),
        "the plan should account for the Node toolchain:\n{statically}"
    );
    assert!(
        statically.contains("ExecStart=/opt/feldspar/bin/feldspar"),
        "the unit should point at the deployed artifact:\n{statically}"
    );
    // The dry run must not have written the files it printed.
    assert!(!config.exists() && !unit.exists(), "--dry-run wrote a file");

    fs::remove_dir_all(&dir).ok();
}

/// Nothing is silently overwritten on a second run, and a `--dry-run` is honest
/// about what it would keep.
#[test]
fn an_existing_config_and_unit_are_kept_unless_forced() {
    let dir = scratch("keep");
    let config = dir.join("feldspar.toml");
    let unit = dir.join("feldspar.service");
    fs::write(&config, "# hand-edited\n").unwrap();
    fs::write(&unit, "# hand-edited\n").unwrap();

    let args = |force: bool| {
        let mut a = vec![
            "--dry-run".to_string(),
            "--static".to_string(),
            "--database-url".to_string(),
            "postgres://x/y".to_string(),
            "--config".to_string(),
            config.to_str().unwrap().to_string(),
            "--unit".to_string(),
            unit.to_str().unwrap().to_string(),
        ];
        if force {
            a.push("--force".to_string());
        }
        a
    };

    let kept = Command::new("sh")
        .arg(script())
        .args(args(false))
        .output()
        .unwrap();
    let kept = stdout_of(&kept);
    assert_eq!(
        kept.matches("keeping the existing").count(),
        2,
        "both files should be kept:\n{kept}"
    );
    assert!(
        !kept.contains("default_environment"),
        "nothing should be rewritten"
    );

    let forced = Command::new("sh")
        .arg(script())
        .args(args(true))
        .output()
        .unwrap();
    let forced = stdout_of(&forced);
    assert!(
        forced.contains("default_environment") && forced.contains("[Install]"),
        "--force should plan to replace both files:\n{forced}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// The header is the interface for someone who has never seen the repository:
/// it has to carry a working `curl … | sh` line with the raw URL in it, and
/// --help has to list the flag that makes the two-step install two steps.
/// Run from inside an installed artifact — `/opt/feldspar/setup-host.sh`, beside
/// the binary the tarball's install.sh put there — the script must not offer to
/// install rustup and build a second copy. Its own location picks `--static`, and
/// the prefix follows it, so the tarball workflow (unpack, install.sh,
/// setup-host.sh) needs no flags at all.
#[test]
fn a_copy_beside_an_installed_binary_defaults_to_static() {
    let dir = scratch("in-prefix");
    let prefix = dir.join("opt/feldspar");
    write_executable(&prefix.join("bin/feldspar"), "#!/bin/sh\necho feldspar\n");
    let installed = prefix.join("setup-host.sh");
    fs::copy(script(), &installed).unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new("sh")
        .arg(&installed)
        .args(["--dry-run", "--domain", "example.com"])
        .output()
        .expect("run the installed setup-host.sh");
    let plan = stdout_of(&out);
    assert!(out.status.success(), "{plan}");

    assert!(
        plan.contains("static install"),
        "a copy inside the prefix should default to --static:\n{plan}"
    );
    // The prefix followed the script, so the unit points at the binary beside it
    // even though this artifact is not at /opt/feldspar.
    assert!(
        plan.contains(&format!("binary {}", prefix.join("bin/feldspar").display())),
        "the prefix should follow the script's own location:\n{plan}"
    );
    assert!(
        !plan.contains("rustup") && !plan.contains("git clone") && !plan.contains("cargo build"),
        "nothing is built when the binary is already here:\n{plan}"
    );
    assert!(
        !plan.contains("libclang-dev"),
        "a static install needs no build dependencies:\n{plan}"
    );

    // And the choice is a default, not a decision: --from-source still wins.
    let out = Command::new("sh")
        .arg(&installed)
        .args(["--dry-run", "--from-source"])
        .output()
        .unwrap();
    let plan = stdout_of(&out);
    assert!(
        plan.contains("source install") && plan.contains("libclang-dev"),
        "--from-source must override the location:\n{plan}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// The older Debian 13 + Python artifact leaves `VARIANT` beside its binary,
/// and the host it is on then gets the library it links and the interpreter
/// pip needs. The `linux` artifact brings its own CPython, so its host gets no
/// Python package — and its embedded interpreter's HTTPS is checked instead
/// (R2.3).
#[test]
fn only_the_older_python_artifact_gets_python_packages() {
    let dir = scratch("python-variant");
    let prefix = dir.join("opt/feldspar");
    write_executable(&prefix.join("bin/feldspar"), "#!/bin/sh\necho feldspar\n");
    let installed = prefix.join("setup-host.sh");
    fs::copy(script(), &installed).unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();

    let plan = |variant: &str| {
        fs::write(prefix.join("VARIANT"), format!("{variant}\n")).unwrap();
        let out = Command::new("sh")
            .arg(&installed)
            .args(["--dry-run", "--domain", "example.com"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stdout_of(&out));
        stdout_of(&out)
    };
    let python = plan("debian13-python");
    assert!(
        python.contains("libpython3.13 python3 python3-venv"),
        "the python artifact's host needs its packages:\n{python}"
    );
    let plain = plan("static");
    assert!(
        !plain.contains("python3-venv"),
        "the static artifact needs no Python:\n{plain}"
    );
    assert!(!plain.contains("python check"), "{plain}");
    let linux = plan("linux");
    assert!(
        !linux.contains("python3"),
        "the linux artifact brings its own Python:\n{linux}"
    );
    assert!(
        linux.contains("embedded Python makes a verified HTTPS request")
            && linux.contains(&format!(
                "runuser -u feldspar -- {} python check",
                prefix.join("bin/feldspar").display()
            )),
        "the linux artifact's HTTPS check should be planned:\n{linux}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// The same file *outside* an installed tree — a checkout, or piped from curl —
/// keeps building from source as the default.
#[test]
fn a_checkout_copy_still_defaults_to_source() {
    let out = run_plain(&["--dry-run", "--domain", "example.com"]);
    let plan = stdout_of(&out);
    assert!(
        plan.contains("source install"),
        "the repository's own copy should still default to a source build:\n{plan}"
    );
}

#[test]
fn the_header_documents_the_curl_and_wget_invocations() {
    let text = fs::read_to_string(script()).expect("read setup-host.sh");
    let raw = "https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh";
    assert!(text.contains(raw), "the header should carry the raw URL");
    assert!(
        text.contains("curl -fsSL") && text.contains("wget -qO-"),
        "both fetch commands should be shown"
    );
    assert!(
        text.contains("| sh -s --"),
        "the pipe form has to pass options after `sh -s --`"
    );
    assert!(
        text.contains("scripts/build-static.sh --deploy"),
        "the header should name the other half of the two-step install"
    );

    let help = run_plain(&["--help"]);
    assert!(help.status.success());
    let help = stdout_of(&help);
    for option in [
        "--static",
        "--domain",
        "--database-url",
        "--dry-run",
        "--force",
    ] {
        assert!(help.contains(option), "--help should document {option}");
    }

    // An option that cannot work is refused, and refused with a status a caller
    // can tell from a failed installation.
    let bad = run_plain(&["--config", "relative.toml"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--config must be absolute"));
    let missing = run_plain(&["--domain"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--domain needs a value"));
}

/// There are now two copies of the systemd unit — README §2.6 and this script —
/// and a deployment set up by the script has to be the one the README describes.
/// The script's defaults are the quick start's paths, so with them the README's
/// unit should appear in the generated one line for line.
#[test]
fn the_generated_unit_agrees_with_the_readme() {
    let readme = fs::read_to_string(workspace_root().join("README.md")).expect("README.md");
    let documented = readme
        .split_once("sudo tee /etc/systemd/system/feldspar.service >/dev/null <<'UNIT'\n")
        .expect("§2.6 should write the unit with a heredoc")
        .1
        .split_once("\nUNIT\n")
        .expect("the heredoc should be terminated")
        .0
        .to_owned();

    // --force, so a machine that already has these files (this one may) still
    // prints the plan; --dry-run, so nothing is written either way.
    let planned = run_plain(&["--dry-run", "--force", "--domain", "example.com"]);
    assert!(planned.status.success());
    let planned = stdout_of(&planned);
    // The dry run indents file content with `      | `.
    let planned: String = planned
        .lines()
        .filter_map(|l| l.strip_prefix("      | "))
        .collect::<Vec<_>>()
        .join("\n");

    for line in documented.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            planned.contains(line),
            "the unit the script writes is missing the README's `{line}`:\n{planned}"
        );
    }
}

/// TODO 6b.1: with no browser on `PATH` — and Ubuntu's snap shim does not
/// count — the plan installs one (or says why it cannot), and checks it as the
/// service account; `--no-browser` does neither.
#[test]
fn a_host_without_a_browser_gets_one_and_a_snap_shim_does_not_count() {
    let dir = scratch("browser");
    let (bin, _log) = stub_dir(&dir);
    // No `chromium` stub here: only Ubuntu's shim.
    fs::remove_file(bin.join("chromium")).unwrap();
    write_executable(
        &bin.join("chromium-browser"),
        "#!/bin/sh\nexec /snap/bin/chromium \"$@\"\n",
    );
    // A PATH with the stubs and the few tools the plan needs, and none of this
    // machine's browsers.
    let tools = dir.join("tools");
    fs::create_dir_all(&tools).unwrap();
    for tool in [
        "sh", "id", "dirname", "basename", "cat", "sed", "grep", "head", "tail", "readlink",
        "dpkg", "uname", "tr", "cut",
    ] {
        if let Ok(found) = which(tool) {
            std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
        }
    }
    let path = format!("{}:{}", bin.display(), tools.display());

    let plan = |extra: &[&str]| {
        let out = Command::new(tools.join("sh"))
            .arg(script())
            .args(["--dry-run", "--static", "--domain", "example.com"])
            .args(extra)
            .env("PATH", &path)
            .output()
            .expect("run setup-host.sh");
        assert!(
            out.status.success(),
            "{}\n{}",
            stdout_of(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        stdout_of(&out)
    };

    let stdout = plan(&[]);
    assert!(
        !stdout.contains("already here: "),
        "the shim is not a browser:\n{stdout}"
    );
    let installs = stdout.contains("apt-get install -y chromium")
        || stdout.contains("apt-get install -y google-chrome-stable");
    let cannot = stdout.contains("no non-snap Chromium is known");
    assert!(installs || cannot, "{stdout}");
    if installs {
        assert!(stdout.contains("runs headless as feldspar"), "{stdout}");
    }

    let stdout = plan(&["--no-browser"]);
    assert!(
        stdout.contains("--no-browser: view_app will be unavailable"),
        "{stdout}"
    );
    assert!(!stdout.contains("apt-get install -y chromium"), "{stdout}");
    assert!(!stdout.contains("runs headless"), "{stdout}");
}

/// Where `tool` is on this machine's PATH.
fn which(tool: &str) -> Result<PathBuf, ()> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
        .ok_or(())
}

/// The account owns what it writes and nothing it executes.
///
/// Three things that have to hold together, and did not before: the service
/// account's home is the state directory rather than the program prefix, so
/// `sudo -u feldspar git config --global ...` has somewhere to write and
/// `/opt/feldspar` stays root's; the configuration file is `root:feldspar`, so a
/// server that can read its database password still cannot rewrite the file that
/// decides what the next restart connects to; and the unit closes off the kernel
/// surface — without the four settings that would break the JIT or Chromium's
/// sandbox.
#[test]
fn the_service_account_owns_its_state_and_nothing_it_runs() {
    let dir = scratch("ownership");
    let (bin, log) = stub_dir(&dir);
    let prefix = dir.join("opt/feldspar");
    let config = dir.join("etc/feldspar/feldspar.toml");
    let unit = dir.join("etc/systemd/system/feldspar.service");
    write_executable(&prefix.join("bin/feldspar"), "#!/bin/sh\nexit 0\n");

    // Not `feldspar`: an account that already exists is left alone, and the
    // machine running these tests may well be running the service. A name nothing
    // can have is what takes the branch under test.
    let account = "feldspar-nonexistent-acct";
    assert!(
        std::process::Command::new("id")
            .arg(account)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true),
        "{account} should not exist on this machine"
    );

    let out = Command::new("sh")
        .arg(script())
        .args(["--static", "--domain", "example.com", "--user", account])
        .arg("--database-url")
        .arg("postgres://feldspar:pw@db.internal/feldspar")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--config")
        .arg(&config)
        .arg("--unit")
        .arg(&unit)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .expect("run setup-host.sh");
    assert!(
        out.status.success(),
        "setup failed ({})\n{}\n{}",
        out.status,
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    let commands = fs::read_to_string(&log).expect("the stubs should have logged");

    // The home is the state directory, not ${PREFIX}: a service account that owned
    // the program tree could rewrite the binary it is about to execute.
    let adduser = commands
        .lines()
        .find(|l| l.starts_with("adduser"))
        .unwrap_or_else(|| panic!("no adduser in:\n{commands}"));
    assert!(
        adduser.contains("--home /var/lib/feldspar"),
        "the account's home should be its state directory: {adduser}"
    );
    assert!(
        !adduser.contains(&prefix.display().to_string()),
        "the account's home must not be the program prefix: {adduser}"
    );

    // Owned by root, readable by the service: `chown` is a stub here, so the
    // assertion is on how it was called.
    let chown = commands
        .lines()
        .rfind(|l| l.starts_with("chown") && l.contains("feldspar.toml"))
        .unwrap_or_else(|| panic!("the config file should be chowned:\n{commands}"));
    assert!(
        chown.contains(&format!("root:{account}")),
        "the config file should be root's, read by the service's group: {chown}"
    );

    // The mode is on the file before a password is written into it — `tee` alone
    // would create it at the umask's 0644 and leave it world-readable until the
    // chmod caught up.
    let (created, wrote) = (
        commands
            .lines()
            .position(|l| l.starts_with("sudo install -m 0640") && l.contains("feldspar.toml")),
        commands
            .lines()
            .position(|l| l.starts_with("sudo tee") && l.contains("feldspar.toml")),
    );
    let created = created.unwrap_or_else(|| panic!("no mode-first create:\n{commands}"));
    let wrote = wrote.unwrap_or_else(|| panic!("no tee of the config:\n{commands}"));
    assert!(
        created < wrote,
        "the config file should reach its final mode before the content goes in:\n{commands}"
    );

    let service = fs::read_to_string(&unit).expect("the unit should exist");
    for line in [
        "ProtectKernelTunables=true",
        "ProtectKernelModules=true",
        "ProtectKernelLogs=true",
        "ProtectControlGroups=true",
        "ProtectClock=true",
        "ProtectHostname=true",
        "ProtectProc=invisible",
        "RestrictSUIDSGID=true",
        "RestrictRealtime=true",
        "LockPersonality=true",
        "UMask=0077",
    ] {
        assert!(
            service.contains(line),
            "the unit should have {line}:\n{service}"
        );
    }

    // glibc's getaddrinfo asks netlink what is configured before resolving.
    assert!(
        service.contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK"),
        "netlink is needed by the resolver:\n{service}"
    );

    // The four that would break the module runtime or view_app. Each is named in
    // a comment explaining the omission, so the check is on the directive.
    for forbidden in [
        "MemoryDenyWriteExecute=",
        "SystemCallFilter=",
        "RestrictNamespaces=",
        "ProcSubset=",
    ] {
        assert!(
            !service
                .lines()
                .any(|l| l.trim_start().starts_with(forbidden)),
            "{forbidden} breaks the JIT or Chromium's sandbox and must stay out:\n{service}"
        );
    }
}

/// A dry run as if on another distribution: `os_release` stands in for
/// /etc/os-release, the package managers are stubs, npm is too old (so Node
/// is installed) and no browser is on PATH (so one is). Answers the plan.
fn plan_on(os_release: &str, extra: &[&str]) -> (Output, String) {
    let dir = scratch("family");
    let (bin, _log) = stub_dir(&dir);
    fs::remove_file(bin.join("chromium")).unwrap();
    write_executable(&bin.join("npm"), "#!/bin/sh\necho 9.2.0\n");
    for tool in ["dnf", "zypper", "pacman"] {
        write_executable(&bin.join(tool), "#!/bin/sh\nexit 0\n");
    }
    let tools = dir.join("tools");
    fs::create_dir_all(&tools).unwrap();
    for tool in [
        "sh", "id", "dirname", "basename", "cat", "sed", "grep", "head", "tail", "readlink",
        "uname", "tr", "cut",
    ] {
        if let Ok(found) = which(tool) {
            std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
        }
    }
    let osr = dir.join("os-release");
    fs::write(&osr, os_release).unwrap();
    let out = Command::new(tools.join("sh"))
        .arg(script())
        .args(["--dry-run", "--static", "--domain", "example.com"])
        // A prefix of its own: /opt/feldspar on the machine running the tests
        // may hold an artifact whose VARIANT changes the plan.
        .arg("--prefix")
        .arg(dir.join("opt/feldspar"))
        .arg("--config")
        .arg(dir.join("feldspar.toml"))
        .arg("--unit")
        .arg(dir.join("feldspar.service"))
        .args(extra)
        .env("PATH", format!("{}:{}", bin.display(), tools.display()))
        .env("FELDSPAR_OS_RELEASE", &osr)
        .output()
        .expect("run setup-host.sh");
    let plan = stdout_of(&out);
    fs::remove_dir_all(&dir).ok();
    (out, plan)
}

/// D9: each family gets the table's packages under its own names, its own
/// PostgreSQL first start, Node and a browser from its own repositories, and a
/// system account made with its own tool. Derivatives come by ID_LIKE.
#[test]
fn each_family_gets_its_own_packages_database_setup_and_account() {
    let fedora = [
        "dnf install -y ca-certificates curl postgresql-server postgresql",
        "postgresql-setup --initdb",
        "systemctl enable --now postgresql",
        "dnf install -y nodejs npm",
        "dnf install -y chromium",
        "useradd --system --user-group --create-home --home-dir /var/lib/feldspar --shell /usr/sbin/nologin feldspar",
    ];
    let cases: [(&str, &[&str], &[&str]); 7] = [
        ("ID=fedora\nVERSION_ID=43\n", &fedora, &["epel-release"]),
        (
            "ID=almalinux\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"10.0\"\n",
            &[
                "dnf install -y epel-release",
                "dnf install -y chromium",
                "postgresql-setup --initdb",
            ],
            &[],
        ),
        (
            "ID=rhel\nID_LIKE=fedora\nVERSION_ID=\"10.1\"\n",
            &[
                "dnf install -y https://dl.fedoraproject.org/pub/epel/epel-release-latest-10.noarch.rpm",
            ],
            &[],
        ),
        (
            "ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n",
            &[
                "zypper --non-interactive install ca-certificates ca-certificates-mozilla curl postgresql-server postgresql",
                "systemctl enable --now postgresql",
                "zypper --non-interactive install nodejs-default npm-default",
                "zypper --non-interactive install chromium",
                "useradd --system",
            ],
            &["initdb", "postgresql-setup"],
        ),
        (
            "ID=opensuse-leap\nID_LIKE=\"suse opensuse\"\nVERSION_ID=\"16.0\"\n",
            &["zypper --non-interactive install nodejs22 npm22"],
            &[],
        ),
        (
            "ID=cachyos\nID_LIKE=arch\n",
            &[
                "pacman -Syu --needed --noconfirm ca-certificates curl postgresql",
                "initdb --locale=C.UTF-8 --encoding=UTF8 --auth-local=peer --auth-host=scram-sha-256 -D /var/lib/postgres/data",
                "systemctl enable --now postgresql",
                "pacman -Syu --needed --noconfirm nodejs npm",
                "pacman -Syu --needed --noconfirm chromium",
            ],
            &["postgresql-setup"],
        ),
        (
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n",
            &[
                "apt-get install -y ca-certificates curl postgresql postgresql-client",
                "deb.nodesource.com",
                "adduser --system --group --home /var/lib/feldspar feldspar",
            ],
            &["useradd", "systemctl enable --now postgresql", "dnf"],
        ),
    ];
    for (os_release, present, absent) in cases {
        let (out, plan) = plan_on(os_release, &[]);
        assert!(
            out.status.success(),
            "{os_release}\n{plan}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for expected in present {
            assert!(
                plan.contains(expected),
                "{os_release}: the plan should have `{expected}`:\n{plan}"
            );
        }
        for unexpected in absent {
            assert!(
                !plan.contains(unexpected),
                "{os_release}: the plan should not have `{unexpected}`:\n{plan}"
            );
        }
        assert!(
            !plan.contains("python3"),
            "{os_release}: no Python packages:\n{plan}"
        );
    }

    // A source build gets the family's toolchain.
    let (_, plan) = plan_on("ID=fedora\n", &["--from-source"]);
    assert!(
        plan.contains("gcc gcc-c++ make pkgconf-pkg-config git clang-devel"),
        "{plan}"
    );
    let (_, plan) = plan_on("ID=arch\n", &["--from-source"]);
    assert!(plan.contains("base-devel pkgconf git clang"), "{plan}");
}

/// Alpine runs OpenRC, so it is refused by name and pointed at what works
/// there; a distribution no family covers is refused naming those that are.
#[test]
fn alpine_and_unknown_distributions_are_refused_with_a_reason() {
    let (out, _) = plan_on("ID=alpine\n", &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("OpenRC") && stderr.contains("install.sh"),
        "{stderr}"
    );

    let (out, _) = plan_on("ID=gentoo\n", &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Fedora, RHEL, openSUSE and Arch") && stderr.contains("'gentoo'"),
        "{stderr}"
    );
}

/// R2.3 / D10: where the embedded interpreter's verified HTTPS request fails
/// on its own and passes with the distribution's bundle in SSL_CERT_FILE, the
/// unit sets it; where it passes, the unit does not.
#[test]
fn ssl_cert_file_goes_into_the_unit_only_where_https_needs_it() {
    let bundle = "/etc/ssl/certs/ca-certificates.crt";
    if !Path::new(bundle).is_file() {
        eprintln!("skipped: no {bundle} on this machine");
        return;
    }
    let setup = |needs_it: bool| {
        let dir = scratch("ssl-cert");
        let (bin, _log) = stub_dir(&dir);
        let prefix = dir.join("opt/feldspar");
        let unit = dir.join("feldspar.service");
        // A binary whose self-check's HTTPS request passes only with
        // SSL_CERT_FILE set, or always.
        let check = if needs_it {
            "[ -n \"${SSL_CERT_FILE:-}\" ] && exit 0; echo 'FAIL https certificate verify failed'; exit 1"
        } else {
            "exit 0"
        };
        write_executable(
            &prefix.join("bin/feldspar"),
            &format!(
                "#!/bin/sh\nif [ \"${{1:-}} ${{2:-}}\" = 'python check' ]; then {check}; fi\nexit 0\n"
            ),
        );
        fs::write(prefix.join("VARIANT"), "linux\n").unwrap();
        let out = Command::new("sh")
            .arg(script())
            .args([
                "--static",
                "--database-url",
                "postgres://x/y",
                "--no-browser",
            ])
            .arg("--prefix")
            .arg(&prefix)
            .arg("--config")
            .arg(dir.join("feldspar.toml"))
            .arg("--unit")
            .arg(&unit)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .expect("run setup-host.sh");
        let stdout = stdout_of(&out);
        assert!(
            out.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let service = fs::read_to_string(&unit).unwrap();
        fs::remove_dir_all(&dir).ok();
        (stdout, service)
    };

    let (stdout, service) = setup(true);
    assert!(
        service.contains(&format!("Environment=SSL_CERT_FILE={bundle}\n")),
        "{service}"
    );
    assert!(
        stdout.contains(&format!(
            "only with SSL_CERT_FILE={bundle}; the unit sets it"
        )),
        "{stdout}"
    );
    let (stdout, service) = setup(false);
    assert!(!service.contains("SSL_CERT_FILE"), "{service}");
    assert!(
        stdout.contains("verified HTTPS request as feldspar\n    it does"),
        "{stdout}"
    );
}
