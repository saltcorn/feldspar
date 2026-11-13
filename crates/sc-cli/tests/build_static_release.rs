//! `scripts/build-static.sh --release X.Y.Z`: the version checks, commit and
//! tag before the build, and the push and uploads after it.
//!
//! Compiling V8 to check a few `wrangler` invocations would be absurd, so the
//! script runs inside a throwaway git repository (itself, plus the
//! `Cargo.toml` and `Cargo.lock` it versions), with a bare repository as its
//! `origin`, and a `PATH` in front of it holding stubs for `cargo`, `rustup` and
//! `npx`. The `npx` stub records its argv and stands in for the bucket: a
//! directory that keeps every file it was asked to upload, and that `get`
//! reads back, so the assertions are about what would actually have landed in
//! the bucket: the object names, and the contents of each file.
//!
//! The bundle step runs for real (`scripts/lib/python-bundle.sh`)
//! over a fake python-build-standalone tarball, with `curl`, `ldd` and
//! `patchelf` stubbed: what is under test is the tree it assembles and the
//! commands it would have run, not CPython or the dynamic linker.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET: &str = "x86_64-unknown-linux-gnu";
const AARCH64: &str = "aarch64-unknown-linux-gnu";
/// The workspace version in the throwaway checkout, whatever the real one is.
const VERSION: &str = "0.4.0";
/// A lock file with two workspace packages and a registry one at the same
/// version: a bump must change the first two and leave the third.
const LOCK: &str = "version = 4\n\n\
[[package]]\nname = \"sc-cli\"\nversion = \"0.4.0\"\ndependencies = [\n \"sc-error\",\n \"tinyreg\",\n]\n\n\
[[package]]\nname = \"sc-error\"\nversion = \"0.4.0\"\n\n\
[[package]]\nname = \"tinyreg\"\nversion = \"0.4.0\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"00\"\n";

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

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feldspar-release-test-{name}-{}-{}",
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

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
    /// Every npx argv, one per line.
    log: PathBuf,
    /// A copy of each uploaded file, named after the object key it was sent as.
    uploads: PathBuf,
}

fn fixture(name: &str, npx_status: i32) -> Fixture {
    let root = workspace_root();
    let dir = scratch(name);

    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("scripts")).unwrap();
    fs::copy(
        root.join("scripts/build-static.sh"),
        repo.join("scripts/build-static.sh"),
    )
    .expect("copy build-static.sh");
    // The curl-to-sh installer, which --release publishes beside it.
    fs::copy(
        root.join("scripts/install.sh"),
        repo.join("scripts/install.sh"),
    )
    .expect("copy install.sh");
    // And the bundle step, the tree installer the artifact ships as its
    // install.sh, and the upload half of --release.
    fs::create_dir_all(repo.join("scripts/lib")).unwrap();
    for lib in ["python-bundle.sh", "install-tree.sh", "release.sh"] {
        fs::copy(
            root.join("scripts/lib").join(lib),
            repo.join("scripts/lib").join(lib),
        )
        .unwrap_or_else(|e| panic!("copy {lib}: {e}"));
    }
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let real = manifest
        .lines()
        .skip_while(|l| *l != "[workspace.package]")
        .find(|l| l.starts_with("version = "))
        .expect("the workspace version")
        .to_owned();
    fs::write(
        repo.join("Cargo.toml"),
        manifest.replacen(&real, &format!("version = \"{VERSION}\""), 1),
    )
    .unwrap();
    fs::write(repo.join("Cargo.lock"), LOCK).unwrap();
    // And the bundled modules, which the packaging step also puts in the
    // artifact.
    let plugins = repo.join("plugins/rss");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(plugins.join("feldspar-module.json"), "{}").unwrap();

    let bin = dir.join("bin");
    let log = dir.join("npx.log");
    let uploads = dir.join("uploads");
    fs::create_dir_all(&uploads).unwrap();

    // `cargo build`, minus the compiler: leave an executable where the real build
    // would have left one (for the `--target` it was given), and note the flags
    // and the PyO3 configuration it was given.
    write_executable(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\nset -eu\ntarget={TARGET}\n\
             while [ $# -gt 0 ]; do [ \"$1\" = --target ] && target=\"$2\"; shift; done\n\
             out=\"${{CARGO_TARGET_DIR:-$PWD/target}}/$target/release\"\n\
             printf 'RUSTFLAGS=%s\\n' \"${{RUSTFLAGS:-}}\" >> '{log}'\n\
             if [ -n \"${{PYO3_CONFIG_FILE:-}}\" ]; then cat \"$PYO3_CONFIG_FILE\" >> '{log}'; fi\n\
             mkdir -p \"$out\"\n\
             printf '#!/bin/sh\\necho feldspar usage\\n' > \"$out/feldspar\"\n\
             chmod +x \"$out/feldspar\"\n",
            log = dir.join("cargo.log").display(),
        ),
    );
    write_executable(
        &bin.join("rustup"),
        &format!(
            "#!/bin/sh\n[ \"${{1:-}}\" = target ] && echo {TARGET} && echo {AARCH64}\nexit 0\n"
        ),
    );
    // `npx wrangler r2 object put <bucket>/<key> -f <file> ...`: record the
    // argv, then keep the file under the key it was uploaded as, so the test can
    // look at what the bucket would hold. `... get <bucket>/<key> --file <file>`
    // answers from those files, and as wrangler does for a key that is not there.
    // `npx_status` is what every `put` exits with.
    write_executable(
        &bin.join("npx"),
        &format!(
            "#!/bin/sh\nset -eu\n\
             printf '%s\\n' \"$*\" >> {log}\n\
             key=\"$(basename \"${{5:-}}\")\"\n\
             case \"${{4:-}}\" in\n\
             get) if [ -f '{uploads}'/\"$key\" ]; then cp '{uploads}'/\"$key\" \"$7\"; exit 0; fi\n\
                  echo 'X [ERROR] The specified key does not exist.'; exit 1 ;;\n\
             put) cp \"$7\" '{uploads}'/\"$key\"; exit {npx_status} ;;\n\
             esac\n",
            log = log.display(),
            uploads = uploads.display(),
        ),
    );

    Fixture {
        dir,
        repo,
        bin,
        log,
        uploads,
    }
}

impl Fixture {
    /// Stand in for python-build-standalone and the tools the bundle step runs:
    /// a `curl` that answers every download with a small tarball shaped like
    /// PBS's (`python/` with a libpython, an interpreter, `sysconfig` holding
    /// the `/install` placeholder, and a Tcl library to trim), an `ldd` that
    /// names a fake system glibc, and a `patchelf` that records its argv.
    ///
    /// `pin` writes that tarball's SHA-256 into the throwaway checkout's copy of
    /// the script, as a release bump would; without it the pin is the real
    /// one, which the fake does not match.
    fn fake_python_build_standalone(&self, pin: bool) {
        self.fake_python_build_standalone_for("X86_64", pin);
    }

    /// The same, pinned (with `pin`) as `PBS_SHA256_<arch>`.
    fn fake_python_build_standalone_for(&self, arch: &str, pin: bool) {
        let pbs = self.dir.join("pbs/python");
        fs::create_dir_all(pbs.join("bin")).unwrap();
        fs::create_dir_all(pbs.join("lib/python3.13")).unwrap();
        write_executable(&pbs.join("bin/python3.13"), "#!/bin/sh\n");
        std::os::unix::fs::symlink("python3.13", pbs.join("bin/python3")).unwrap();
        fs::write(pbs.join("lib/libpython3.13.so.1.0"), "ELF?").unwrap();
        std::os::unix::fs::symlink("libpython3.13.so.1.0", pbs.join("lib/libpython3.13.so"))
            .unwrap();
        fs::write(pbs.join("lib/libtcl9.0.so"), "tcl").unwrap();
        fs::write(
            pbs.join("lib/python3.13/_sysconfigdata__linux_x86_64-linux-gnu.py"),
            "build_time_vars = {\"LIBDIR\": \"/install/lib\", \"CFLAGS\": \"-I/install/include \
             -L/install/lib\", \"INSTALL\": \"/usr/bin/install -c\", \"prefix\": \"/install\"}\n",
        )
        .unwrap();
        let tarball = self.dir.join("pbs.tar.gz");
        let packed = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(self.dir.join("pbs"))
            .arg("python")
            .status()
            .unwrap();
        assert!(packed.success());

        let log = self.dir.join("tools.log");
        write_executable(
            &self.bin.join("curl"),
            &format!(
                "#!/bin/sh\nset -eu\nout=\"\"\nwhile [ $# -gt 0 ]; do\n\
                 case \"$1\" in -o) out=\"$2\"; shift 2 ;; -*) shift ;; *) echo \"curl $1\" >> '{log}'; shift ;; esac\n\
                 done\ncp '{tarball}' \"$out\"\n",
                log = log.display(),
                tarball = tarball.display(),
            ),
        );
        let syslib = self.dir.join("syslib");
        fs::create_dir_all(&syslib).unwrap();
        for lib in ["libc.so.6", "ld-linux-x86-64.so.2", "libnss_compat.so.2"] {
            fs::write(syslib.join(lib), lib).unwrap();
        }
        write_executable(
            &self.bin.join("ldd"),
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then echo 'ldd (Debian GLIBC 2.36-9) 2.36'; exit 0; fi\n\
                 case \"$1\" in */bin/feldspar) \
                 printf '\\tlibpython3.13.so.1.0 => %s/../python/lib/libpython3.13.so.1.0 (0x1)\\n' \"$(dirname \"$1\")\" ;; esac\n\
                 printf '\\tlinux-vdso.so.1 (0x0)\\n'\n\
                 printf '\\tlibc.so.6 => {syslib}/libc.so.6 (0x2)\\n'\n\
                 printf '\\t{syslib}/ld-linux-x86-64.so.2 (0x3)\\n'\n",
                syslib = syslib.display(),
            ),
        );
        write_executable(
            &self.bin.join("patchelf"),
            &format!(
                "#!/bin/sh\nprintf 'patchelf %s\\n' \"$*\" >> '{log}'\n",
                log = log.display()
            ),
        );

        if pin {
            self.pin_python_build_standalone(arch);
        }
    }

    /// Pin the fake python-build-standalone tarball as `PBS_SHA256_<arch>`; it
    /// is the same file for either architecture. Before the first release: the
    /// script is then committed as it is.
    fn pin_python_build_standalone(&self, arch: &str) {
        let tarball = self.dir.join("pbs.tar.gz");
        let sha = Command::new("sha256sum").arg(&tarball).output().unwrap();
        let sha = String::from_utf8_lossy(&sha.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        let script = self.repo.join("scripts/build-static.sh");
        let text = fs::read_to_string(&script).unwrap();
        let var = format!("readonly PBS_SHA256_{arch}=");
        let pinned: Vec<&str> = text.lines().filter(|l| l.starts_with(&var)).collect();
        assert_eq!(pinned.len(), 1, "one {arch} pin in the script");
        let text = text.replace(pinned[0], &format!("{var}\"{sha}\""));
        fs::write(&script, text).unwrap();
    }

    /// Make `uname -m` answer `arch`, as on a build machine of that
    /// architecture; everything else `uname` is asked goes to the real one.
    fn pretend_machine(&self, arch: &str) {
        let real = Command::new("sh")
            .args(["-c", "command -v uname"])
            .output()
            .unwrap();
        let real = String::from_utf8_lossy(&real.stdout).trim().to_owned();
        assert!(!real.is_empty(), "no uname on PATH");
        write_executable(
            &self.bin.join("uname"),
            &format!(
                "#!/bin/sh\n[ \"$*\" = -m ] && {{ echo {arch}; exit 0; }}\nexec {real} \"$@\"\n"
            ),
        );
    }

    /// The bucket keys the npx stub was asked to upload, in order.
    fn objects(&self) -> Vec<String> {
        self.npx_calls()
            .iter()
            .filter(|c| c.split_whitespace().nth(3) == Some("put"))
            .map(|c| c.split_whitespace().nth(4).unwrap().to_owned())
            .collect()
    }

    /// Put a file in the bucket, as an earlier release would have.
    fn seed(&self, key: &str, contents: &str) {
        fs::write(self.uploads.join(key), contents).unwrap();
    }

    /// What the bucket holds under `key`.
    fn bucket(&self, key: &str) -> String {
        fs::read_to_string(self.uploads.join(key)).unwrap_or_else(|e| panic!("{key}: {e}"))
    }

    /// `git` in the checkout (`repo`) or the remote (`remote.git`), with no
    /// configuration but this test's.
    fn git(&self, dir: &str, args: &[&str]) -> String {
        let out = git_command(&self.dir.join(dir))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Make the checkout a repository with everything in it committed on
    /// `main`, pushed to a bare `origin` that `main` tracks. Once, before the
    /// first release.
    fn commit(&self) {
        if self.repo.join(".git").exists() {
            return;
        }
        let remote = self.dir.join("remote.git");
        let out = git_command(&self.dir)
            .args(["init", "--quiet", "--bare", "-b", "main"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(out.status.success());
        self.git("repo", &["init", "--quiet", "-b", "main"]);
        self.git("repo", &["add", "-A"]);
        self.git("repo", &["commit", "--quiet", "-m", "the checkout"]);
        self.git(
            "repo",
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        self.git("repo", &["push", "--quiet", "-u", "origin", "main"]);
    }

    /// What the `curl` and `patchelf` stubs recorded.
    fn tool_calls(&self) -> String {
        fs::read_to_string(self.dir.join("tools.log")).unwrap_or_default()
    }

    /// Run the script with `--release` of the checkout's own version and
    /// whatever extra options a test wants.
    fn release(&self, extra: &[&str]) -> (std::process::Output, String, String) {
        self.release_as(VERSION, extra)
    }

    /// Run the script with `--release version`, committing the checkout first.
    fn release_as(&self, version: &str, extra: &[&str]) -> (std::process::Output, String, String) {
        self.commit();
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = git_env(Command::new("bash"))
            .arg(self.repo.join("scripts/build-static.sh"))
            .args([
                "--native",
                "--no-ui",
                "--no-strip",
                "--no-verify",
                "--release",
                version,
            ])
            .arg("--output")
            .arg(self.dir.join("dist"))
            .args(extra)
            .env("PATH", &path)
            .output()
            .expect("run build-static.sh");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output, stdout, stderr)
    }

    /// What the npx stub recorded, in the order it was called.
    fn npx_calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The x86_64 tarball the build wrote (named after the version and commit
    /// it was built at, which a release has since moved on from), or a path
    /// that is not there.
    fn tarball(&self) -> PathBuf {
        let dist = self.dir.join("dist");
        fs::read_dir(&dist)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.to_string_lossy().ends_with(&format!("-{TARGET}.tar.gz")))
            .unwrap_or_else(|| dist.join("no-tarball"))
    }

    /// The workspace version in the checkout's Cargo.toml.
    fn version(&self) -> String {
        fs::read_to_string(self.repo.join("Cargo.toml"))
            .unwrap()
            .lines()
            .skip_while(|l| *l != "[workspace.package]")
            .find_map(|l| {
                l.strip_prefix("version = ")
                    .map(|v| v.trim_matches('"').to_owned())
            })
            .expect("the workspace version")
    }

    /// Start the checkout at `version` instead of [`VERSION`] (before the
    /// first release): Cargo.toml and the lock file's workspace packages.
    fn start_at(&self, version: &str) {
        let from = format!("version = \"{VERSION}\"");
        let to = format!("version = \"{version}\"");
        let manifest = self.repo.join("Cargo.toml");
        let text = fs::read_to_string(&manifest)
            .unwrap()
            .replacen(&from, &to, 1);
        fs::write(&manifest, text).unwrap();
        fs::write(self.repo.join("Cargo.lock"), LOCK.replacen(&from, &to, 2)).unwrap();
    }
}

/// `cmd` with git configured by the test alone: no global or system
/// configuration (no signing, no hooks, no default branch of the machine's),
/// and an author for the release commit and tag.
fn git_env(mut cmd: Command) -> Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Release Test")
        .env("GIT_AUTHOR_EMAIL", "release@example.com")
        .env("GIT_COMMITTER_NAME", "Release Test")
        .env("GIT_COMMITTER_EMAIL", "release@example.com");
    cmd
}

fn git_command(dir: &Path) -> Command {
    let mut cmd = git_env(Command::new("git"));
    cmd.current_dir(dir);
    cmd
}

/// The x86_64 tarball goes up under its versioned name, then its own platform
/// name and the name the first releases had, each with a checksum naming that
/// object; then the list of versions, and the installer last.
#[test]
fn release_uploads_the_tarball_and_checksum_under_versioned_and_version_less_names() {
    let f = fixture("ok", 0);
    f.fake_python_build_standalone(true);
    let (output, stdout, stderr) = f.release(&[]);
    assert!(
        output.status.success(),
        "--release failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let tarball = f.tarball();
    assert!(tarball.is_file(), "the build wrote no {tarball:?}");
    assert_eq!(
        f.objects(),
        [
            "feldspar-latest-static/feldspar-0.4.0-linux-x86_64.tar.gz",
            "feldspar-latest-static/feldspar-0.4.0-linux-x86_64.tar.gz.sha256",
            "feldspar-latest-static/feldspar-linux-x86_64.tar.gz",
            "feldspar-latest-static/feldspar-linux-x86_64.tar.gz.sha256",
            "feldspar-latest-static/feldspar.tar.gz",
            "feldspar-latest-static/feldspar.tar.gz.sha256",
            "feldspar-latest-static/versions.txt",
            "feldspar-latest-static/latest.txt",
            "feldspar-latest-static/install.sh",
        ]
    );
    let calls = f.npx_calls();
    assert_eq!(
        calls.iter().find(|c| c.contains(" put ")).unwrap(),
        &format!(
            "wrangler r2 object put feldspar-latest-static/feldspar-0.4.0-linux-x86_64.tar.gz -f {} --remote",
            tarball.display()
        ),
    );
    // The first release of the version in Cargo.toml: no release commit, a
    // tag on the checkout's own commit; then the next development version
    // committed on top, and both pushed; and the version listed.
    assert_eq!(
        f.git("repo", &["log", "--format=%s", "-2"]),
        "Start 0.4.1-dev\nthe checkout"
    );
    assert_eq!(
        f.git("remote.git", &["rev-parse", "v0.4.0^{commit}"]),
        f.git("repo", &["rev-parse", "HEAD~1"])
    );
    assert_eq!(
        f.git("remote.git", &["rev-parse", "main"]),
        f.git("repo", &["rev-parse", "HEAD"])
    );
    assert_eq!(f.version(), "0.4.1-dev");
    assert!(
        f.tarball().to_string_lossy().contains("/feldspar-0.4.0-"),
        "built as 0.4.0, not as what comes after: {:?}",
        f.tarball()
    );
    assert_eq!(f.bucket("versions.txt"), "0.4.0\n");
    assert_eq!(f.bucket("latest.txt"), "0.4.0\n");
    assert!(
        calls
            .iter()
            .filter(|c| c.starts_with("wrangler r2 "))
            .all(|c| c.ends_with(" --remote")),
        "{calls:?}"
    );
    // The installer that fetches it — byte for byte the checkout's.
    assert_eq!(
        fs::read(f.uploads.join("install.sh")).expect("the uploaded installer"),
        fs::read(f.repo.join("scripts/install.sh")).unwrap(),
    );
    assert!(
        stdout.contains("curl -fsSL https://feldspar-latest-static.saltcorn.com/install.sh | sh"),
        "a publish should print the one-line install:\n{stdout}"
    );

    // Both names hold the same tarball, and each checksum file names its own
    // object rather than the versioned file, so `sha256sum -c` works on
    // either download.
    let local = fs::read_to_string(format!("{}.sha256", tarball.display())).unwrap();
    let digest = local.split_whitespace().next().unwrap();
    for object in [
        "feldspar-0.4.0-linux-x86_64.tar.gz",
        "feldspar-linux-x86_64.tar.gz",
        "feldspar.tar.gz",
    ] {
        assert_eq!(
            fs::read(f.uploads.join(object)).unwrap(),
            fs::read(&tarball).unwrap(),
            "{object}"
        );
        assert_eq!(
            fs::read_to_string(f.uploads.join(format!("{object}.sha256"))).unwrap(),
            format!("{digest}  {object}\n"),
        );
        assert!(
            stdout.contains(&format!(
                "https://feldspar-latest-static.saltcorn.com/{object}"
            )),
            "{stdout}"
        );
    }
    // The curl installer that was published fetches the platform's name, with
    // the architecture filled in (`install_script` checks it resolves).
    let installer = fs::read_to_string(f.uploads.join("install.sh")).unwrap();
    assert!(
        installer.contains("OBJECT=\"feldspar-linux-${ARCH}.tar.gz\""),
        "{installer}"
    );

    // The tarball has no host setup script any more: `feldspar setup` is in the
    // binary.
    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(&tarball)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing.lines().any(|l| l.ends_with("/setup-host.sh")),
        "{listing}"
    );

    // README §2 and the operations manual tell an operator to download the
    // published URLs, so the two cannot drift apart unnoticed.
    let public = "https://feldspar-latest-static.saltcorn.com/feldspar-linux-x86_64.tar.gz";
    for doc in ["README.md", "docs/OPERATIONS.md"] {
        let text = fs::read_to_string(workspace_root().join(doc)).unwrap();
        assert!(
            text.contains(public) && text.contains(&format!("{public}.sha256")),
            "{doc} should document the published download URLs"
        );
    }
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn bucket_overrides_the_destination() {
    let f = fixture("bucket", 0);
    f.fake_python_build_standalone(true);
    let (output, stdout, stderr) = f.release(&["--bucket", "feldspar-staging"]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let objects = f.objects();
    assert_eq!(
        objects[..4],
        [
            "feldspar-staging/feldspar-0.4.0-linux-x86_64.tar.gz",
            "feldspar-staging/feldspar-0.4.0-linux-x86_64.tar.gz.sha256",
            "feldspar-staging/feldspar-linux-x86_64.tar.gz",
            "feldspar-staging/feldspar-linux-x86_64.tar.gz.sha256",
        ],
        "{objects:?}"
    );
    // The versions it reads are that bucket's too.
    assert!(
        f.npx_calls()
            .iter()
            .filter(|c| c.contains(" get "))
            .all(|c| c.contains(" feldspar-staging/")),
        "{:?}",
        f.npx_calls()
    );
    // A bucket somewhere else is not served at the public URL, so it is not
    // offered one: printing it would be pointing at the *last* release.
    assert!(
        !stdout.contains("saltcorn.com"),
        "a --bucket elsewhere must not be reported as the public download:\n{stdout}"
    );
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn a_failed_upload_fails_the_run() {
    // wrangler exiting non-zero must not be reported as a published release.
    let f = fixture("fail", 1);
    f.fake_python_build_standalone(true);
    let (output, stdout, stderr) = f.release(&[]);
    assert!(
        !output.status.success(),
        "a failed upload was reported as success"
    );
    assert!(!stdout.contains("published"), "{stdout}");
    assert!(
        f.tarball().is_file(),
        "the tarball should still be on disk after a failed upload"
    );
    let _ = stderr;
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn release_without_npx_is_refused_before_the_build() {
    // The check has to happen before the compile, not after it.
    let f = fixture("no-npx", 0);
    fs::remove_file(f.bin.join("npx")).unwrap();
    // Dropping the stub is not enough on a machine that has node installed: the
    // real npx would be found further down `PATH` and the build would run. So
    // `PATH` becomes a single directory of links to everything it held *except*
    // npx — dropping the directories that contain one would take `bash` and the
    // coreutils with them.
    let sandbox = f.dir.join("path-without-npx");
    fs::create_dir_all(&sandbox).unwrap();
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == "npx" || sandbox.join(&name).exists() {
                continue;
            }
            std::os::unix::fs::symlink(entry.path(), sandbox.join(&name)).ok();
        }
    }
    let path = format!("{}:{}", f.bin.display(), sandbox.display());
    let output = Command::new("bash")
        .arg(f.repo.join("scripts/build-static.sh"))
        .args([
            "--native",
            "--no-ui",
            "--no-strip",
            "--no-verify",
            "--release",
            VERSION,
        ])
        .arg("--output")
        .arg(f.dir.join("dist"))
        .env("PATH", &path)
        .output()
        .expect("run build-static.sh");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--release needs npx"), "{stderr}");
    assert!(
        !f.dir.join("dist").exists(),
        "the build ran before the option was refused"
    );

    // An empty bucket name is refused the same way.
    let output = Command::new("bash")
        .arg(f.repo.join("scripts/build-static.sh"))
        .args(["--native", "--release", VERSION, "--bucket", ""])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--bucket must not be empty"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(&f.dir).ok();
}

/// The tarball's member at `path` (below its top-level directory), as text.
fn member(tarball: &Path, path: &str) -> String {
    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(tarball)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    let name = listing
        .lines()
        .find(|l| l.ends_with(&format!("/{path}")))
        .unwrap_or_else(|| panic!("{tarball:?} has no {path}:\n{listing}"))
        .to_owned();
    let out = Command::new("tar")
        .arg("-xzOf")
        .arg(tarball)
        .arg(&name)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The tree that ships: the pinned CPython, linked beside the binary, the
/// bundled glibc and the `.glibc` copies that use it, the tree installer, and
/// a README that says what it needs from the host.
#[test]
fn the_release_bundles_python_and_glibc() {
    let f = fixture("tree", 0);
    f.fake_python_build_standalone(true);
    let (output, stdout, stderr) = f.release(&[]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let linux_tarball = f.tarball();
    assert_eq!(member(&linux_tarball, "VARIANT"), "linux\n");

    // The pinned CPython was fetched for this target...
    let tools = f.tool_calls();
    assert!(
        tools.contains(
            "curl https://github.com/astral-sh/python-build-standalone/releases/download/\
             20261026/cpython-3.13.18+20261026-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz"
        ),
        "{tools}"
    );
    // ...linked through a RUNPATH beside the binary, against that copy...
    let cargo = fs::read_to_string(f.dir.join("cargo.log")).unwrap();
    assert!(
        cargo.contains("-C link-arg=-Wl,-rpath,$ORIGIN/../python/lib"),
        "{cargo}"
    );
    assert!(cargo.contains("lib_name=python3.13"), "{cargo}");
    assert!(cargo.contains("/python/lib\n"), "{cargo}");
    // ...trimmed, with its sysconfig naming where it will be installed...
    let sysconfig = member(
        &linux_tarball,
        "python/lib/python3.13/_sysconfigdata__linux_x86_64-linux-gnu.py",
    );
    assert!(
        sysconfig.contains("\"LIBDIR\": \"/opt/feldspar/python/lib\"")
            && sysconfig.contains("-I/opt/feldspar/python/include -L/opt/feldspar/python/lib")
            && sysconfig.contains("\"prefix\": \"/opt/feldspar/python\"")
            && sysconfig.contains("/usr/bin/install -c"),
        "{sysconfig}"
    );
    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(&linux_tarball)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    for path in [
        "/bin/feldspar",
        "/bin/feldspar.glibc",
        "/python/bin/python3.glibc",
        "/python/lib/libpython3.13.so.1.0",
        "/glibc/libc.so.6",
        "/glibc/ld-linux-x86-64.so.2",
        "/glibc/libnss_compat.so.2",
    ] {
        assert!(
            listing.lines().any(|l| l.ends_with(path)),
            "no {path} in:\n{listing}"
        );
    }
    assert!(
        !listing.contains("libtcl"),
        "Tcl/Tk was not trimmed:\n{listing}"
    );
    // ...and both copies point at the bundled loader, through a DT_RPATH.
    let patched = tools.matches(
        "patchelf --set-interpreter /opt/feldspar/glibc/ld-linux-x86-64.so.2 \
         --force-rpath --set-rpath /opt/feldspar/glibc:/opt/feldspar/python/lib ",
    );
    assert_eq!(patched.count(), 2, "{tools}");
    assert!(
        stderr.contains("libnss_db.so.2 is not on this machine"),
        "{stderr}"
    );

    // install.sh is the tree installer (R2.6) — it swaps the whole prefix in
    // rather than writing over a mapped libpython, and chooses the libc —
    // with this build's prefix and glibc filled in. It asks the host for no
    // Python package at all.
    let installer = fs::read_to_string(workspace_root().join("scripts/lib/install-tree.sh"))
        .unwrap()
        .replace("@PREFIX@", "/opt/feldspar")
        .replace("@BUILD_GLIBC@", "2.36");
    let install = member(&linux_tarball, "install.sh");
    assert_eq!(install, installer);
    assert!(!install.contains("apt-get"), "{install}");
    // Owned by root in the archive, whoever built it: install.sh keeps what it
    // copies as it is (`cp -p`), so a tree unpacked as root would otherwise
    // belong to the build account's uid.
    let verbose = Command::new("tar")
        .args(["-tvzf"])
        .arg(&linux_tarball)
        .arg("--numeric-owner")
        .output()
        .unwrap();
    let verbose = String::from_utf8_lossy(&verbose.stdout);
    assert!(
        verbose
            .lines()
            .all(|l| l.split_whitespace().nth(1) == Some("0/0")),
        "every member should be 0/0:\n{verbose}"
    );
    let readme = member(&linux_tarball, "README");
    assert!(readme.contains("glibc 2.36 or newer"), "{readme}");
    assert!(readme.contains("feldspar python check"), "{readme}");
    assert!(
        readme.contains("https://feldspar-latest-static.saltcorn.com/feldspar-linux-x86_64.tar.gz"),
        "{readme}"
    );
    assert!(readme.contains("sudo feldspar setup --server"), "{readme}");
    fs::remove_dir_all(&f.dir).ok();
}

/// The options that chose between two artifacts, or pushed one over ssh, are
/// gone; a script that still passes one is told why before anything is built.
#[test]
fn removed_options_are_refused_with_a_reason() {
    let f = fixture("removed", 0);
    for option in [
        &["--deploy", "root@vm"][..],
        &["--variant", "linux"],
        &["--deploy-variant", "linux"],
        &["--ssh-opt", "-p2222"],
        &["--remote-tmp", "/tmp"],
    ] {
        let (output, _stdout, stderr) = f.release(option);
        assert_eq!(output.status.code(), Some(2), "{option:?}: {stderr}");
        assert!(
            stderr.contains(&format!("{} is gone", option[0])) && stderr.contains("OPERATIONS.md"),
            "{option:?}: {stderr}"
        );
    }
    assert!(f.npx_calls().is_empty());
    assert!(!f.dir.join("cargo.log").exists(), "cargo ran anyway");
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn a_python_download_that_does_not_match_the_pin_is_refused_before_the_build() {
    let f = fixture("wrong-pbs", 0);
    // The real pin, which the fake tarball does not match.
    f.fake_python_build_standalone(false);
    let (output, _stdout, stderr) = f.release(&[]);
    assert!(!output.status.success());
    assert!(stderr.contains("not the pinned"), "{stderr}");
    assert!(f.objects().is_empty(), "{:?}", f.objects());
    assert!(
        !f.dir.join("cargo.log").exists(),
        "cargo ran after a bad download"
    );
    assert!(!f.dir.join("dist").exists(), "a tarball was written anyway");
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn an_aarch64_release_publishes_only_its_own_name() {
    // On an aarch64 build machine the release goes up under its own name
    // only: the old name has always been the x86_64 tarball's.
    let f = fixture("aarch64", 0);
    f.pretend_machine("aarch64");
    f.fake_python_build_standalone_for("AARCH64", true);
    let (output, stdout, stderr) = f.release(&["--target", AARCH64]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        f.objects(),
        [
            "feldspar-latest-static/feldspar-0.4.0-linux-aarch64.tar.gz",
            "feldspar-latest-static/feldspar-0.4.0-linux-aarch64.tar.gz.sha256",
            "feldspar-latest-static/feldspar-linux-aarch64.tar.gz",
            "feldspar-latest-static/feldspar-linux-aarch64.tar.gz.sha256",
            "feldspar-latest-static/versions.txt",
            "feldspar-latest-static/latest.txt",
            "feldspar-latest-static/install.sh",
        ]
    );
    let published =
        fs::read_to_string(f.uploads.join("feldspar-linux-aarch64.tar.gz.sha256")).unwrap();
    assert!(
        published.ends_with("  feldspar-linux-aarch64.tar.gz\n"),
        "{published}"
    );
    assert!(
        f.tool_calls().contains(&format!(
            "cpython-3.13.18+20261026-{AARCH64}-install_only_stripped"
        )),
        "{}",
        f.tool_calls()
    );
    fs::remove_dir_all(&f.dir).ok();
}

/// The usual release, from a development version: X.Y.Z set in Cargo.toml
/// and in the lock file's workspace packages, committed and tagged, built;
/// then X.Y.(Z+1)-dev committed on top, and both pushed, with the tag,
/// before anything is uploaded; and the version added to those listed.
#[test]
fn a_new_version_is_committed_tagged_pushed_and_listed() {
    let f = fixture("bump", 0);
    f.fake_python_build_standalone(true);
    f.start_at("0.4.1-dev");
    f.seed("versions.txt", "0.3.9\n0.4.0\n");
    let (output, stdout, stderr) = f.release_as("0.4.1", &[]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The release commit: both files, the workspace version only, and in the
    // lock file the workspace packages only.
    assert_eq!(
        f.git("repo", &["log", "--format=%s", "-1", "v0.4.1"]),
        "Release 0.4.1"
    );
    assert_eq!(
        f.git("repo", &["show", "--name-only", "--format=", "v0.4.1^{commit}"]),
        "Cargo.lock\nCargo.toml"
    );
    let manifest = f.git("repo", &["show", "v0.4.1:Cargo.toml"]);
    let workspace = manifest
        .split("[workspace.package]")
        .nth(1)
        .unwrap()
        .split("\n[")
        .next()
        .unwrap();
    assert!(workspace.contains("version = \"0.4.1\""), "{workspace}");
    assert_eq!(
        manifest.matches("version = \"0.4.1\"").count(),
        1,
        "{manifest}"
    );
    assert_eq!(
        format!("{}\n", f.git("repo", &["show", "v0.4.1:Cargo.lock"])),
        LOCK.replacen("version = \"0.4.0\"", "version = \"0.4.1\"", 2),
        "the registry package keeps its version"
    );
    assert!(
        f.tarball().to_string_lossy().contains("/feldspar-0.4.1-"),
        "{:?}",
        f.tarball()
    );

    // Then the next development version, on top of it, in both files.
    assert_eq!(
        f.git("repo", &["log", "--format=%s", "-3"]),
        "Start 0.4.2-dev\nRelease 0.4.1\nthe checkout"
    );
    assert_eq!(f.version(), "0.4.2-dev");
    assert_eq!(
        fs::read_to_string(f.repo.join("Cargo.lock")).unwrap(),
        LOCK.replacen("version = \"0.4.0\"", "version = \"0.4.2-dev\"", 2),
    );
    assert_eq!(
        f.git("repo", &["status", "--porcelain", "--untracked-files=no"]),
        ""
    );

    // On the remote: main at the -dev commit, the annotated tag at the release.
    assert_eq!(
        f.git("remote.git", &["rev-parse", "main"]),
        f.git("repo", &["rev-parse", "HEAD"])
    );
    assert_eq!(
        f.git("remote.git", &["rev-parse", "v0.4.1^{commit}"]),
        f.git("repo", &["rev-parse", "HEAD~1"])
    );
    assert_eq!(f.git("remote.git", &["cat-file", "-t", "v0.4.1"]), "tag");

    // The push came before the first upload: the tag was on the remote by
    // then. (The stub cannot see the remote, so the order is read from the
    // log lines, which the script prints as it goes.)
    let pushed = stdout
        .find("pushing main and v0.4.1 to origin")
        .expect(&stdout);
    let uploaded = stdout.find("uploading").expect(&stdout);
    assert!(pushed < uploaded, "{stdout}");

    assert_eq!(
        fs::read(f.uploads.join("feldspar-0.4.1-linux-x86_64.tar.gz")).unwrap(),
        fs::read(f.uploads.join("feldspar-linux-x86_64.tar.gz")).unwrap()
    );
    assert_eq!(f.bucket("versions.txt"), "0.3.9\n0.4.0\n0.4.1\n");
    assert_eq!(f.bucket("latest.txt"), "0.4.1\n");

    // The next release from there: 0.4.1 again is older than 0.4.2-dev; a
    // minor release, 0.5.0, is fine from it.
    let (output, _stdout, stderr) = f.release_as("0.4.1", &[]);
    assert!(!output.status.success());
    assert!(
        stderr.contains("older than the version in Cargo.toml, 0.4.2-dev"),
        "{stderr}"
    );
    let (output, stdout, stderr) = f.release_as("0.5.0", &[]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(f.version(), "0.5.1-dev");
    assert_eq!(f.bucket("versions.txt"), "0.3.9\n0.4.0\n0.4.1\n0.5.0\n");
    fs::remove_dir_all(&f.dir).ok();
}

/// A push the remote refuses uploads nothing and takes the -dev commit back
/// off; once the remote takes pushes again, the same command finishes the
/// release from the tag it left: the build again, the -dev commit, the push.
#[test]
fn a_refused_push_uploads_nothing_and_the_same_command_finishes_the_release() {
    let f = fixture("refused-push", 0);
    f.fake_python_build_standalone(true);
    f.start_at("0.4.1-dev");
    f.commit();
    let hook = f.dir.join("remote.git/hooks/pre-receive");
    write_executable(&hook, "#!/bin/sh\necho 'refused by the test' >&2\nexit 1\n");

    let (output, stdout, stderr) = f.release_as("0.4.1", &[]);
    assert!(!output.status.success(), "{stdout}");
    assert!(
        stderr.contains("could not push the release to origin"),
        "{stderr}"
    );
    assert!(f.objects().is_empty(), "{:?}", f.objects());
    assert_eq!(
        f.git("repo", &["rev-parse", "HEAD"]),
        f.git("repo", &["rev-parse", "v0.4.1^{commit}"]),
        "the -dev commit should be gone, HEAD at the tag"
    );
    assert_eq!(f.version(), "0.4.1");
    assert_eq!(
        f.git("remote.git", &["log", "--format=%s", "-1", "main"]),
        "the checkout"
    );

    fs::remove_file(&hook).unwrap();
    let (output, stdout, stderr) = f.release_as("0.4.1", &[]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("not pushed yet: finishing the release"),
        "{stdout}"
    );
    assert_eq!(
        f.git("remote.git", &["log", "--format=%s", "-3", "main"]),
        "Start 0.4.2-dev\nRelease 0.4.1\nthe checkout"
    );
    assert_eq!(f.bucket("latest.txt"), "0.4.1\n");
    fs::remove_dir_all(&f.dir).ok();
}

/// The second architecture of a version: built at its tag, it commits and
/// tags nothing, publishes its own names, and leaves the version listed once.
/// The same run again is refused: a published version is not replaced.
#[test]
fn another_architecture_publishes_at_the_tag_and_a_version_is_never_replaced() {
    let f = fixture("second-arch", 0);
    f.fake_python_build_standalone(true);
    f.pin_python_build_standalone("AARCH64");
    let (output, stdout, stderr) = f.release_as("0.5.0", &[]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let tagged = f.git("repo", &["rev-parse", "v0.5.0^{commit}"]);
    let main = f.git("repo", &["rev-parse", "HEAD"]);

    // The aarch64 machine: a detached checkout of the tag.
    f.git("repo", &["checkout", "--quiet", "v0.5.0"]);
    f.pretend_machine("aarch64");
    let before = f.objects().len();
    let (output, stdout, stderr) = f.release_as("0.5.0", &["--target", AARCH64]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("v0.5.0 is this commit"), "{stdout}");
    assert_eq!(
        f.git("repo", &["rev-parse", "HEAD"]),
        tagged,
        "no new commit"
    );
    assert_eq!(f.version(), "0.5.0");
    assert_eq!(
        f.git("remote.git", &["rev-parse", "main"]),
        main,
        "main left at the first run's -dev commit"
    );
    assert_eq!(
        f.objects()[before..],
        [
            "feldspar-latest-static/feldspar-0.5.0-linux-aarch64.tar.gz",
            "feldspar-latest-static/feldspar-0.5.0-linux-aarch64.tar.gz.sha256",
            "feldspar-latest-static/feldspar-linux-aarch64.tar.gz",
            "feldspar-latest-static/feldspar-linux-aarch64.tar.gz.sha256",
            "feldspar-latest-static/versions.txt",
            "feldspar-latest-static/latest.txt",
            "feldspar-latest-static/install.sh",
        ]
    );
    assert_eq!(f.bucket("versions.txt"), "0.5.0\n");

    // Again: refused before the build, and nothing uploaded.
    let before = f.objects().len();
    fs::remove_file(f.dir.join("cargo.log")).unwrap();
    let (output, _stdout, stderr) = f.release_as("0.5.0", &["--target", AARCH64]);
    assert!(!output.status.success());
    assert!(
        stderr.contains("feldspar-0.5.0-linux-aarch64.tar.gz is already published"),
        "{stderr}"
    );
    assert_eq!(f.objects().len(), before);
    assert!(!f.dir.join("cargo.log").exists(), "cargo ran anyway");
    fs::remove_dir_all(&f.dir).ok();
}

/// Each version that may not be published is refused before the build, with
/// nothing committed, tagged or uploaded.
#[test]
fn versions_that_may_not_be_published_are_refused_before_the_build() {
    let refused = |name: &str, setup: &dyn Fn(&Fixture), version: &str, why: &str| {
        let f = fixture(name, 0);
        f.fake_python_build_standalone(true);
        f.commit();
        setup(&f);
        let tags = f.git("repo", &["tag", "-l"]);
        let (output, stdout, stderr) = f.release_as(version, &[]);
        assert!(!output.status.success(), "{name}: was published\n{stdout}");
        assert!(stderr.contains(why), "{name}: {stderr}");
        assert!(!f.dir.join("cargo.log").exists(), "{name}: cargo ran");
        assert!(f.objects().is_empty(), "{name}: {:?}", f.objects());
        assert_eq!(f.git("repo", &["tag", "-l"]), tags, "{name}: tagged");
        assert_eq!(
            f.git("repo", &["log", "--format=%s", "-1"]),
            "the checkout",
            "{name}: committed"
        );
        fs::remove_dir_all(&f.dir).ok();
    };
    let nothing = |_: &Fixture| {};
    refused("format", &nothing, "0.5", "X.Y.Z");
    refused("prefix", &nothing, "v0.5.0", "X.Y.Z");
    refused(
        "older-than-cargo",
        &nothing,
        "0.3.9",
        "older than the version in Cargo.toml, 0.4.0",
    );
    refused(
        "older-than-listed",
        &|f| f.seed("versions.txt", "0.4.0\n0.4.10\n"),
        "0.4.9",
        "older than the newest published release, 0.4.10",
    );
    refused(
        "older-than-tag",
        &|f| {
            f.git("repo", &["tag", "v0.6.0"]);
        },
        "0.5.0",
        "older than the newest release tag, v0.6.0",
    );
    refused(
        "published",
        &|f| f.seed("feldspar-0.4.0-linux-x86_64.tar.gz.sha256", "x"),
        "0.4.0",
        "is already published",
    );
    refused(
        "dirty",
        &|f| fs::write(f.repo.join("Cargo.lock"), "edited").unwrap(),
        "0.4.1",
        "uncommitted changes",
    );
    refused(
        "tagged-elsewhere",
        &|f| {
            f.git("repo", &["tag", "-a", "v0.4.1", "-m", "elsewhere"]);
            fs::write(f.repo.join("NEW"), "").unwrap();
            f.git("repo", &["add", "NEW"]);
            f.git("repo", &["commit", "--quiet", "-m", "the checkout"]);
        },
        "0.4.1",
        "git checkout v0.4.1",
    );
    refused(
        "behind",
        &|f| {
            // Someone else's commit on the remote's main.
            let other = f.dir.join("other");
            let out = git_command(&f.dir)
                .args(["clone", "--quiet"])
                .arg(f.dir.join("remote.git"))
                .arg(&other)
                .output()
                .unwrap();
            assert!(out.status.success());
            fs::write(other.join("THEIRS"), "").unwrap();
            f.git("other", &["add", "THEIRS"]);
            f.git("other", &["commit", "--quiet", "-m", "theirs"]);
            f.git("other", &["push", "--quiet", "origin", "main"]);
        },
        "0.4.1",
        "main is behind origin/main",
    );
    refused(
        "detached",
        &|f| {
            f.git("repo", &["checkout", "--quiet", "--detach"]);
        },
        "0.4.1",
        "HEAD is detached",
    );

    // Older than a development version's release: 0.4.0 from 0.4.1-dev.
    let f = fixture("older-than-dev", 0);
    f.fake_python_build_standalone(true);
    f.start_at("0.4.1-dev");
    let (output, _stdout, stderr) = f.release_as("0.4.0", &[]);
    assert!(!output.status.success());
    assert!(
        stderr.contains("0.4.0 is older than the version in Cargo.toml, 0.4.1-dev"),
        "{stderr}"
    );
    assert!(f.objects().is_empty());
    fs::remove_dir_all(&f.dir).ok();

    // And with no version at all.
    let f = fixture("no-version", 0);
    let (output, _stdout, stderr) = f.release_as("--no-ui", &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr.contains("--release takes the version to publish"),
        "{stderr}"
    );
    fs::remove_dir_all(&f.dir).ok();
}

/// `scripts/lib/release.sh` on its own, sourced by a script that is not
/// build-static.sh — as the macOS build will (TODO-portable-release D15, R5):
/// the checksum, an upload under two names with a `.sha256` naming each, the
/// installer last, and the preflight's refusals.
#[test]
fn the_release_library_publishes_for_any_build_script() {
    let dir = scratch("lib");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("scripts/lib")).unwrap();
    fs::copy(
        workspace_root().join("scripts/lib/release.sh"),
        repo.join("scripts/lib/release.sh"),
    )
    .unwrap();
    fs::write(repo.join("scripts/install.sh"), "#!/bin/sh\n").unwrap();
    let dist = dir.join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(dist.join("feldspar-0.1.0-macos.tar.gz"), "a tarball").unwrap();

    let bin = dir.join("bin");
    let log = dir.join("npx.log");
    let uploads = dir.join("uploads");
    fs::create_dir_all(&uploads).unwrap();
    // Records each `wrangler r2 object put BUCKET/KEY -f FILE` and keeps FILE.
    write_executable(
        &bin.join("npx"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n[ \"${{4:-}}\" = put ] || exit 0\nkey=\"${{5#*/}}\"\ncp \"$7\" '{up}'/\"$key\"\n",
            log = log.display(),
            up = uploads.display()
        ),
    );
    // By absolute path: a test PATH without npx has no bash on it either.
    let bash = Command::new("sh")
        .args(["-c", "command -v bash"])
        .output()
        .unwrap();
    let bash = String::from_utf8_lossy(&bash.stdout).trim().to_owned();
    let run = |body: &str, path: &str| {
        Command::new(&bash)
            .arg("-c")
            .arg(format!(
                "set -euo pipefail\nREPO_ROOT='{repo}'\nlog() {{ echo \"==> $*\"; }}\n\
                 source \"$REPO_ROOT/scripts/lib/release.sh\"\nR2_BUCKET=\"${{BUCKET-$DEFAULT_R2_BUCKET}}\"\n{body}",
                repo = repo.display()
            ))
            .env("PATH", path)
            .output()
            .unwrap()
    };
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let tarball = dist.join("feldspar-0.1.0-macos.tar.gz");
    let out = run(
        &format!(
            "release_preflight\nrelease_checksum '{t}'\nrelease_warn_if_dirty abc123-dirty\n\
             release_upload '{t}' '{d}' feldspar-macos-arm64.tar.gz feldspar-other.tar.gz\n\
             release_upload_installer",
            t = tarball.display(),
            d = dir.display()
        ),
        &path,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("warning: publishing a build from a dirty checkout (abc123-dirty)"));
    assert!(
        stdout.contains("curl -fsSL https://feldspar-latest-static.saltcorn.com/install.sh | sh")
    );

    // The checksum beside the tarball names it by its own name...
    let local = fs::read_to_string(dist.join("feldspar-0.1.0-macos.tar.gz.sha256")).unwrap();
    let digest = local.split_whitespace().next().unwrap().to_owned();
    assert_eq!(local, format!("{digest}  feldspar-0.1.0-macos.tar.gz\n"));
    // ...and each published one by its published name; the installer goes last.
    let keys: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| l.split_whitespace().nth(3) == Some("put"))
        .map(|l| l.split_whitespace().nth(4).unwrap().to_owned())
        .collect();
    assert_eq!(
        keys,
        [
            "feldspar-latest-static/feldspar-macos-arm64.tar.gz",
            "feldspar-latest-static/feldspar-macos-arm64.tar.gz.sha256",
            "feldspar-latest-static/feldspar-other.tar.gz",
            "feldspar-latest-static/feldspar-other.tar.gz.sha256",
            "feldspar-latest-static/install.sh",
        ]
    );
    for object in ["feldspar-macos-arm64.tar.gz", "feldspar-other.tar.gz"] {
        assert_eq!(
            fs::read_to_string(uploads.join(object)).unwrap(),
            "a tarball"
        );
        assert_eq!(
            fs::read_to_string(uploads.join(format!("{object}.sha256"))).unwrap(),
            format!("{digest}  {object}\n")
        );
    }

    // Versions compare as numbers, field by field.
    let out = run(
        "release_version_ge 0.1.10 0.1.9 && release_version_ge 1.0.0 0.99.99 && \
         release_version_ge 0.2.0 0.2.0 && ! release_version_ge 0.1.9 0.1.10 && \
         release_version_is_valid 10.0.1 && ! release_version_is_valid 01.0.0 && \
         ! release_version_is_valid 1.0.0-rc1 && \
         [[ $(release_next_dev 0.1.1) == 0.1.2-dev && $(release_next_dev 1.9.99) == 1.9.100-dev ]] && \
         [[ $(release_version_base 0.1.2-dev) == 0.1.2 && $(release_version_base 0.1.2) == 0.1.2 ]] && \
         [[ \"$(printf '0.2.0\\njunk\\n0.10.0\\n0.9.1\\n' | release_newest)\" == 0.10.0 ]]",
        &path,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The preflight refuses an empty bucket, and a PATH without npx.
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "REPO_ROOT='{}'; source \"$REPO_ROOT/scripts/lib/release.sh\"; R2_BUCKET=''; release_preflight",
            repo.display()
        ))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--bucket must not be empty"));
    let no_npx = dir.join("empty-path");
    fs::create_dir_all(&no_npx).unwrap();
    let out = run("release_preflight", no_npx.to_str().unwrap());
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs npx"));
    fs::remove_dir_all(&dir).ok();
}

/// `npx wrangler` with no wrangler installed asks "Ok to proceed? (y)" before
/// downloading it. Where the release captures wrangler's output (reading
/// versions.txt) that question was never seen, and the release sat silent.
/// The preflight now downloads it in view, and every call says yes in advance.
#[test]
fn a_wrangler_npx_must_download_is_fetched_in_view_and_never_asks() {
    let dir = scratch("wrangler-download");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("scripts/lib")).unwrap();
    fs::copy(
        workspace_root().join("scripts/lib/release.sh"),
        repo.join("scripts/lib/release.sh"),
    )
    .unwrap();
    fs::write(repo.join("scripts/install.sh"), "#!/bin/sh\n").unwrap();

    // npx as it behaves: with wrangler absent, `npm_config_yes=false` fails,
    // `true` downloads it, and neither asks; unset, it asks and waits for an
    // answer (here: hangs, and says so on a file the test reads).
    let bin = dir.join("bin");
    let installed = dir.join("installed");
    let asked = dir.join("asked");
    write_executable(
        &bin.join("npx"),
        &format!(
            "#!/bin/sh\n\
             if [ ! -f '{installed}' ]; then\n\
               case \"${{npm_config_yes:-}}\" in\n\
               true) echo 'npm: downloading wrangler@4.0.0' >&2; touch '{installed}' ;;\n\
               false) echo 'npm error npx canceled due to missing packages and no YES option' >&2; exit 1 ;;\n\
               *) echo 'Ok to proceed? (y)'; touch '{asked}'; sleep 30; exit 1 ;;\n\
               esac\n\
             fi\n\
             case \"$*\" in\n\
             'wrangler --version') echo '4.0.0' ;;\n\
             *' get '*) echo 'X [ERROR] The specified key does not exist.'; exit 1 ;;\n\
             esac\n",
            installed = installed.display(),
            asked = asked.display(),
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = || {
        let started = std::time::Instant::now();
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -euo pipefail\nREPO_ROOT='{}'\nlog() {{ echo \"==> $*\"; }}\n\
                 source \"$REPO_ROOT/scripts/lib/release.sh\"\nR2_BUCKET=b\n\
                 release_preflight\nrelease_published_versions '{}'",
                repo.display(),
                dir.display()
            ))
            .env("PATH", &path)
            .env_remove("npm_config_yes")
            .output()
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "it waited on a prompt"
        );
        out
    };

    let out = run();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(!asked.exists(), "npx asked to install wrangler: {stdout}");
    // The download is announced, its output is in view, and it is over before
    // the bucket is read.
    let downloading = stdout
        .find("==> wrangler is not installed: npx is downloading it")
        .expect(&stdout);
    let reading = stdout.find("==> reading r2://b/versions.txt").expect(&stdout);
    assert!(downloading < reading, "{stdout}");
    assert!(stderr.contains("npm: downloading wrangler@4.0.0"), "{stderr}");

    // Installed now: the preflight names the version and downloads nothing.
    let out = run();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("==> using wrangler 4.0.0"), "{stdout}");
    assert!(!stdout.contains("downloading"), "{stdout}");

    // A wrangler that cannot be had stops the release before anything else.
    fs::remove_file(&installed).unwrap();
    write_executable(&bin.join("npx"), "#!/bin/sh\necho 'npm error 404' >&2\nexit 1\n");
    let out = run();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not run wrangler"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::remove_dir_all(&dir).ok();
}
