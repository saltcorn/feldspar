//! `scripts/build-static.sh --release`: the upload step that runs after the
//! tarball is written.
//!
//! Compiling V8 to check a few `wrangler` invocations would be absurd, so the
//! script runs inside a
//! throwaway "repository" (itself, plus the `Cargo.toml` it reads the version
//! from) with a `PATH` in front of it holding stubs for `cargo`, `rustup` and
//! `npx`. The `npx` stub records its argv and keeps a copy of every file it was
//! asked to upload, so the assertions are about what would actually have landed
//! in the bucket: the object names, and the contents of the checksum file.
//!
//! The bundle step runs for real (`scripts/lib/python-bundle.sh`)
//! over a fake python-build-standalone tarball, with `curl`, `ldd` and
//! `patchelf` stubbed: what is under test is the tree it assembles and the
//! commands it would have run, not CPython or the dynamic linker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET: &str = "x86_64-unknown-linux-gnu";
const AARCH64: &str = "aarch64-unknown-linux-gnu";

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
    fs::copy(root.join("Cargo.toml"), repo.join("Cargo.toml")).unwrap();
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
    // `npx wrangler r2 object put <bucket>/<key> -f <file> --remote`: record the
    // argv, then keep the file under the key it was uploaded as, so the test can
    // look at what the bucket would hold.
    write_executable(
        &bin.join("npx"),
        &format!(
            "#!/bin/sh\nset -eu\n\
             printf '%s\\n' \"$*\" >> {log}\n\
             key=\"${{5:-}}\"\n\
             if [ \"${{6:-}}\" = -f ]; then cp \"$7\" '{uploads}'/\"$(basename \"$key\")\"; fi\n\
             exit {npx_status}\n",
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
            .map(|c| c.split_whitespace().nth(4).unwrap().to_owned())
            .collect()
    }

    /// What the `curl` and `patchelf` stubs recorded.
    fn tool_calls(&self) -> String {
        fs::read_to_string(self.dir.join("tools.log")).unwrap_or_default()
    }

    /// Run the script with `--release` and whatever extra options a test wants.
    fn release(&self, extra: &[&str]) -> (std::process::Output, String, String) {
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
                "--release",
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

    fn tarball(&self) -> PathBuf {
        let version = fs::read_to_string(self.repo.join("Cargo.toml"))
            .unwrap()
            .lines()
            .find_map(|l| {
                l.strip_prefix("version = ")
                    .map(|v| v.trim_matches('"').to_owned())
            })
            .expect("the workspace version");
        self.dir
            .join("dist")
            .join(format!("feldspar-{version}-{TARGET}.tar.gz"))
    }
}

/// The x86_64 tarball goes up under its own platform name and under the name
/// the first releases had, each with a checksum naming that object, and the
/// installer goes up last.
#[test]
fn release_uploads_the_tarball_and_checksum_under_version_less_names() {
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
            "feldspar-latest-static/feldspar-linux-x86_64.tar.gz",
            "feldspar-latest-static/feldspar-linux-x86_64.tar.gz.sha256",
            "feldspar-latest-static/feldspar.tar.gz",
            "feldspar-latest-static/feldspar.tar.gz.sha256",
            "feldspar-latest-static/install.sh",
        ]
    );
    let calls = f.npx_calls();
    assert_eq!(
        calls[0],
        format!(
            "wrangler r2 object put feldspar-latest-static/feldspar-linux-x86_64.tar.gz -f {} --remote",
            tarball.display()
        ),
    );
    assert!(calls.iter().all(|c| c.ends_with(" --remote")), "{calls:?}");
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
    for object in ["feldspar-linux-x86_64.tar.gz", "feldspar.tar.gz"] {
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

    let calls = f.npx_calls();
    assert!(
        calls[0].contains("feldspar-staging/feldspar-linux-x86_64.tar.gz -f")
            && calls[1].contains("feldspar-staging/feldspar-linux-x86_64.tar.gz.sha256 -f"),
        "{calls:?}"
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
        .args(["--native", "--release", "--bucket", ""])
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
    assert!(f.npx_calls().is_empty());
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
            "feldspar-latest-static/feldspar-linux-aarch64.tar.gz",
            "feldspar-latest-static/feldspar-linux-aarch64.tar.gz.sha256",
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
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\nkey=\"${{5#*/}}\"\ncp \"$7\" '{up}'/\"$key\"\n",
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
