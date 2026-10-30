//! `scripts/build-static.sh --release`: the upload step that runs after the
//! tarball is written.
//!
//! Same shape as `build_static_deploy.rs` and for the same reason — compiling V8
//! to check two `wrangler` invocations would be absurd. The script runs inside a
//! throwaway "repository" (itself, plus the `Cargo.toml` it reads the version
//! from) with a `PATH` in front of it holding stubs for `cargo`, `rustup` and
//! `npx`. The `npx` stub records its argv and keeps a copy of every file it was
//! asked to upload, so the assertions are about what would actually have landed
//! in the bucket: the object names, and the contents of the checksum file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET: &str = "x86_64-unknown-linux-gnu";

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
    // The packaging step puts this in the artifact, so the throwaway repository
    // has to have one.
    fs::copy(
        root.join("scripts/setup-host.sh"),
        repo.join("scripts/setup-host.sh"),
    )
    .expect("copy setup-host.sh");
    // And the curl-to-sh installer, which --release publishes beside it.
    fs::copy(
        root.join("scripts/install.sh"),
        repo.join("scripts/install.sh"),
    )
    .expect("copy install.sh");
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
    // would have left one.
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
            .args(self.variant(extra))
            .args(extra)
            .env("PATH", &path)
            .output()
            .expect("run build-static.sh");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output, stdout, stderr)
    }

    /// `--variant static` unless the test chose one: the tests below that predate
    /// the Python artifact are about one tarball and two uploads.
    fn variant(&self, extra: &[&str]) -> &'static [&'static str] {
        if extra.contains(&"--variant") {
            &[]
        } else {
            &["--variant", "static"]
        }
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

#[test]
fn release_uploads_the_tarball_and_checksum_under_version_less_names() {
    let f = fixture("ok", 0);
    let (output, stdout, stderr) = f.release(&[]);
    assert!(
        output.status.success(),
        "--release failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let calls = f.npx_calls();
    assert_eq!(
        calls.len(),
        3,
        "expected one upload each for the tarball, the checksum and the installer, got {calls:?}"
    );

    // The tarball, sent under a name that carries no version.
    let tarball = f.tarball();
    assert!(tarball.is_file(), "the build wrote no {tarball:?}");
    assert_eq!(
        calls[0],
        format!(
            "wrangler r2 object put feldspar-latest-static/feldspar.tar.gz -f {} --remote",
            tarball.display()
        ),
    );
    assert!(
        calls[1].starts_with(
            "wrangler r2 object put feldspar-latest-static/feldspar.tar.gz.sha256 -f "
        )
    );
    assert!(calls[1].ends_with(" --remote"), "{}", calls[1]);
    // Last, the installer that fetches it — byte for byte the checkout's.
    assert!(
        calls[2].starts_with("wrangler r2 object put feldspar-latest-static/install.sh -f "),
        "{}",
        calls[2]
    );
    assert_eq!(
        fs::read(f.uploads.join("install.sh")).expect("the uploaded installer"),
        fs::read(f.repo.join("scripts/install.sh")).unwrap(),
    );
    assert!(
        stdout.contains("curl -fsSL https://feldspar-latest-static.saltcorn.com/install.sh | sh"),
        "a publish should print the one-line install:\n{stdout}"
    );

    // What is inside it: the host installer, so the machine that downloads this
    // from the bucket needs no checkout to set itself up.
    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(&tarball)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing.lines().any(|l| l.ends_with("/setup-host.sh")),
        "the published tarball carries no setup-host.sh:\n{listing}"
    );

    // What the bucket now holds: the tarball itself, byte for byte...
    assert_eq!(
        fs::read(f.uploads.join("feldspar.tar.gz")).expect("the uploaded tarball"),
        fs::read(&tarball).unwrap(),
    );
    // ...and a checksum file naming the *published* object rather than the
    // versioned one, so `sha256sum -c` works on what was downloaded.
    let published = fs::read_to_string(f.uploads.join("feldspar.tar.gz.sha256"))
        .expect("the uploaded checksum");
    let local = fs::read_to_string(format!("{}.sha256", tarball.display())).unwrap();
    let digest = local.split_whitespace().next().unwrap();
    assert_eq!(published, format!("{digest}  feldspar.tar.gz\n"));

    assert!(
        stdout.contains("published") && stdout.contains("feldspar-latest-static/feldspar.tar.gz"),
        "the run said nothing about publishing:\n{stdout}"
    );
    // And it names the URL that artifact is now served at — the one README §2
    // tells an operator to download, so the two cannot drift apart unnoticed.
    let public = "https://feldspar-latest-static.saltcorn.com/feldspar.tar.gz";
    assert!(
        stdout.contains(public),
        "a publish should print where the artifact can now be fetched:\n{stdout}"
    );
    let readme = fs::read_to_string(workspace_root().join("README.md")).expect("README.md");
    assert!(
        readme.contains(public) && readme.contains(&format!("{public}.sha256")),
        "README should document the published download URLs"
    );
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn bucket_overrides_the_destination() {
    let f = fixture("bucket", 0);
    let (output, stdout, stderr) = f.release(&["--bucket", "feldspar-staging"]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let calls = f.npx_calls();
    assert!(
        calls[0].contains("feldspar-staging/feldspar.tar.gz -f")
            && calls[1].contains("feldspar-staging/feldspar.tar.gz.sha256 -f"),
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

#[test]
fn the_default_release_builds_and_publishes_the_static_and_the_python_artifact() {
    let f = fixture("both", 0);
    // The native Python build checks the interpreter it will link against; this
    // one answers as Debian 13's does, whatever the test machine has.
    write_executable(&f.bin.join("python3"), "#!/bin/sh\necho 3.13\n");
    let (output, stdout, stderr) = f.release(&["--variant", "both"]);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let static_tarball = f.tarball();
    let python_tarball = PathBuf::from(
        static_tarball
            .display()
            .to_string()
            .replace(".tar.gz", "-debian13-python.tar.gz"),
    );
    assert!(static_tarball.is_file() && python_tarball.is_file());

    // Five uploads: each tarball and its checksum, under version-less names that
    // tell the two apart, and the one installer.
    let objects: Vec<String> = f
        .npx_calls()
        .iter()
        .map(|c| c.split_whitespace().nth(4).unwrap().to_owned())
        .collect();
    assert_eq!(
        objects,
        [
            "feldspar-latest-static/feldspar.tar.gz",
            "feldspar-latest-static/feldspar.tar.gz.sha256",
            "feldspar-latest-static/feldspar-debian13-python.tar.gz",
            "feldspar-latest-static/feldspar-debian13-python.tar.gz.sha256",
            "feldspar-latest-static/install.sh",
        ]
    );
    assert_eq!(
        fs::read(f.uploads.join("feldspar-debian13-python.tar.gz")).unwrap(),
        fs::read(&python_tarball).unwrap(),
    );
    let published =
        fs::read_to_string(f.uploads.join("feldspar-debian13-python.tar.gz.sha256")).unwrap();
    assert!(
        published.ends_with("  feldspar-debian13-python.tar.gz\n"),
        "{published}"
    );
    assert!(
        stdout.contains(
            "https://feldspar-latest-static.saltcorn.com/feldspar-debian13-python.tar.gz"
        ),
        "{stdout}"
    );

    // Each artifact says which it is, for setup-host.sh to read...
    assert_eq!(member(&static_tarball, "VARIANT"), "static\n");
    assert_eq!(member(&python_tarball, "VARIANT"), "debian13-python\n");
    // ...and only the Python one's install.sh sees to the library it links.
    let install = member(&python_tarball, "install.sh");
    assert!(
        install.contains("libpython3.13.so.1.0")
            && install.contains("apt-get install -y libpython3.13"),
        "{install}"
    );
    assert!(!member(&static_tarball, "install.sh").contains("libpython"));
    assert!(member(&python_tarball, "README").contains("Debian 13"));

    // The download URL the operations manual gives is the one published.
    let ops = fs::read_to_string(workspace_root().join("docs/OPERATIONS.md")).unwrap();
    assert!(
        ops.contains("https://feldspar-latest-static.saltcorn.com/feldspar-debian13-python.tar.gz"),
        "docs/OPERATIONS.md should document the Python artifact's download URL"
    );
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn a_native_python_build_against_the_wrong_python_is_refused_before_the_build() {
    let f = fixture("wrong-python", 0);
    write_executable(&f.bin.join("python3"), "#!/bin/sh\necho 3.11\n");
    let (output, _stdout, stderr) = f.release(&["--variant", "python"]);
    assert!(!output.status.success());
    assert!(stderr.contains("links libpython 3.13"), "{stderr}");
    assert!(f.npx_calls().is_empty());
    assert!(!f.dir.join("dist").exists(), "the build ran anyway");
    fs::remove_dir_all(&f.dir).ok();
}
