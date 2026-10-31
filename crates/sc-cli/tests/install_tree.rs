//! `scripts/lib/install-tree.sh`: the release tarball's own `install.sh`
//! (TODO-portable-release R2.1, R2.6).
//!
//! Run for real, as `build-static.sh` ships it — with `@PREFIX@` filled in —
//! against fake release trees and a prefix inside the test's temporary
//! directory. The executables in a fake tree are shell scripts whose second line
//! says which release and which copy they are, which is how the assertions tell
//! what ended up where. `systemctl` is **always** a stub first on `PATH`: the
//! installer stops running `feldspar@*` units, and the machine running these
//! tests may well have some.

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

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feldspar-install-tree-{name}-{}-{}",
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

/// A fake executable: line 2 names it, `--version` prints that name, `serve`
/// stays up, and anything else is "usage" — unless `starts` is false, when it
/// fails the way a binary linked against a newer glibc does.
fn fake_binary(path: &Path, name: &str, starts: bool) {
    let otherwise = if starts {
        "echo usage >&2; exit 0"
    } else {
        "echo \"$0: /lib/x86_64-linux-gnu/libc.so.6: version 'GLIBC_2.99' not found\" >&2; exit 1"
    };
    write_executable(
        path,
        &format!(
            "#!/bin/sh\n# {name}\ncase \"${{1:-}}\" in\n\
             --version) echo \"feldspar {name}\" ;;\n\
             serve) while :; do sleep 1; done ;;\n\
             *) {otherwise} ;;\nesac\n"
        ),
    );
}

struct Fixture {
    dir: PathBuf,
    prefix: PathBuf,
    stubs: PathBuf,
    /// What the systemctl stub was asked to stop and start, one per line,
    /// with the installed binary's name at that moment.
    units: PathBuf,
}

fn fixture(name: &str) -> Fixture {
    let dir = scratch(name);
    let prefix = dir.join("opt/feldspar");
    fs::create_dir_all(prefix.parent().unwrap()).unwrap();
    let stubs = dir.join("stubs");
    let units = dir.join("units.log");
    write_executable(
        &stubs.join("systemctl"),
        &format!(
            "#!/bin/sh\n\
             seen=\"$(sed -n 2p '{prefix}/bin/feldspar' 2>/dev/null | cut -c3-)\"\n\
             case \"$1\" in\n\
             list-units) cat '{dir}/active' 2>/dev/null ;;\n\
             stop) echo \"stop $2 [$seen]\" >> '{units}'; [ ! -e '{dir}/stop-fails' ] ;;\n\
             start) echo \"start $2 [$seen]\" >> '{units}' ;;\n\
             esac\n",
            prefix = prefix.display(),
            dir = dir.display(),
            units = units.display(),
        ),
    );
    Fixture {
        dir,
        prefix,
        stubs,
        units,
    }
}

impl Fixture {
    /// A release tree named `name` (the binaries' marker). `linux` adds what
    /// the `linux` artifact has: python/, glibc/ and the `.glibc` copies.
    fn tree(&self, name: &str, linux: bool) -> PathBuf {
        let tree = self.dir.join(format!("unpacked/feldspar-{name}"));
        fake_binary(&tree.join("bin/feldspar"), name, true);
        fs::create_dir_all(tree.join("ui/admin/dist")).unwrap();
        fs::write(tree.join("ui/admin/dist/index.html"), name).unwrap();
        fs::create_dir_all(tree.join("plugins/rss")).unwrap();
        fs::write(tree.join("plugins/rss/feldspar-module.json"), "{}").unwrap();
        fs::write(
            tree.join("VARIANT"),
            if linux { "linux\n" } else { "static\n" },
        )
        .unwrap();
        if linux {
            fake_binary(
                &tree.join("bin/feldspar.glibc"),
                &format!("{name} on bundled glibc"),
                true,
            );
            write_executable(
                &tree.join("python/bin/python3.13"),
                &format!("#!/bin/sh\n# python {name}\n"),
            );
            std::os::unix::fs::symlink("python3.13", tree.join("python/bin/python3")).unwrap();
            write_executable(
                &tree.join("python/bin/python3.glibc"),
                &format!("#!/bin/sh\n# python {name} on bundled glibc\n"),
            );
            fs::create_dir_all(tree.join("python/lib/python3.13/__pycache__")).unwrap();
            fs::write(tree.join("python/lib/python3.13/os.py"), "").unwrap();
            fs::write(
                tree.join("python/lib/python3.13/__pycache__/os.cpython-313.pyc"),
                "",
            )
            .unwrap();
            fs::create_dir_all(tree.join("glibc")).unwrap();
            fs::write(tree.join("glibc/libc.so.6"), "libc").unwrap();
        }
        let installer = fs::read_to_string(workspace_root().join("scripts/lib/install-tree.sh"))
            .unwrap()
            .replace("@PREFIX@", &self.prefix.display().to_string())
            .replace("@BUILD_GLIBC@", "2.36");
        write_executable(&tree.join("install.sh"), &installer);
        tree
    }

    /// Run `tree`'s install.sh, with the stubs first on PATH and `extra_path`
    /// before them (to stand in for the host's `ldd`).
    fn install(&self, tree: &Path, args: &[&str], extra_path: Option<&Path>) -> Output {
        let mut path = format!(
            "{}:{}",
            self.stubs.display(),
            std::env::var("PATH").unwrap()
        );
        if let Some(extra) = extra_path {
            path = format!("{}:{path}", extra.display());
        }
        Command::new("sh")
            .arg(tree.join("install.sh"))
            .args(args)
            .env("PATH", path)
            .env("TMPDIR", &self.dir)
            .env("FELDSPAR_LINK", self.link())
            .output()
            .expect("run install.sh")
    }

    /// Where the installer links the binary onto PATH, inside the test's
    /// directory.
    fn link(&self) -> PathBuf {
        self.dir.join("usr/local/bin/feldspar")
    }

    fn installed(&self, path: &str) -> String {
        fs::read_to_string(self.prefix.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn units(&self) -> Vec<String> {
        fs::read_to_string(&self.units)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `ldd` answering as musl's does, for a musl host.
    fn musl_ldd(&self) -> PathBuf {
        let bin = self.dir.join("musl");
        write_executable(
            &bin.join("ldd"),
            "#!/bin/sh\necho 'musl libc (x86_64)' >&2\necho 'Version 1.2.5' >&2\nexit 1\n",
        );
        bin
    }
}

fn ok(out: &Output) -> (String, String) {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "install.sh failed ({})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    (stdout, stderr)
}

/// Every path under `root` with its contents (or link target), in order: what
/// "changes nothing" is measured against.
fn snapshot(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.push(format!(
                    "{rel} -> {}",
                    fs::read_link(&path).unwrap().display()
                ));
            } else if meta.is_dir() {
                out.push(format!("{rel}/"));
                walk(root, &path, out);
            } else {
                out.push(format!(
                    "{rel} {:o} {:?}",
                    meta.permissions().mode(),
                    fs::read(&path).unwrap()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn a_first_install_puts_the_tree_at_the_prefix_and_lists_it() {
    let f = fixture("fresh");
    let tree = f.tree("2.0", true);
    let (stdout, _) = ok(&f.install(&tree, &[], None));

    assert!(f.installed("bin/feldspar").contains("# 2.0\n"));
    // On a glibc host whose glibc runs the binary, the normal executables stay,
    // and so do the .glibc copies and glibc/ — the tree is the tarball.
    assert!(f.prefix.join("bin/feldspar.glibc").is_file());
    assert!(f.prefix.join("glibc/libc.so.6").is_file());
    let libc = f.installed("LIBC");
    assert!(libc.starts_with("host: the host's glibc"), "{libc}");
    assert!(
        stdout.contains("this install runs on the host's glibc"),
        "{stdout}"
    );

    // The manifest names everything the install put there, itself included,
    // and nothing else.
    let manifest = f.installed("MANIFEST");
    for path in [
        "MANIFEST",
        "LIBC",
        "VARIANT",
        "bin",
        "bin/feldspar",
        "bin/feldspar.glibc",
        "install.sh",
        "plugins/rss/feldspar-module.json",
        "python/bin/python3",
        "ui/admin/dist/index.html",
    ] {
        assert!(
            manifest.lines().any(|l| l == path),
            "MANIFEST lacks {path}:\n{manifest}"
        );
    }
    assert!(!manifest.contains(&f.dir.display().to_string()));

    // Nothing to keep yet, nothing left beside it, and the next step named.
    assert!(!f.dir.join("opt/feldspar.old").exists());
    assert!(!f.dir.join("opt/feldspar.new").exists());
    assert!(
        stdout.contains("sudo feldspar setup --server --domain example.com"),
        "{stdout}"
    );
    assert!(stdout.contains("installed"), "{stdout}");
    // On PATH, through the prefix, so an upgrade's swap keeps it pointing at
    // the installed release.
    assert_eq!(
        fs::read_link(f.link()).unwrap(),
        f.prefix.join("bin/feldspar")
    );
    // No unit was running, so none was touched.
    assert!(f.units().is_empty(), "{:?}", f.units());
    fs::remove_dir_all(&f.dir).ok();
}

/// D11's tests, run here as well as in the smoke test (R2.4): the upgrade
/// succeeds under a running process, swaps rather than overwrites, drops what
/// only the old release had, and keeps what an operator added.
#[test]
fn an_upgrade_swaps_the_tree_under_a_running_server_and_keeps_an_operators_files() {
    let f = fixture("upgrade");
    let old = f.tree("1.0", true);
    fs::write(old.join("old-only.txt"), "gone in 2.0").unwrap();
    ok(&f.install(&old, &[], None));

    // A server running out of the old tree.
    let mut server = Command::new(f.prefix.join("bin/feldspar"))
        .arg("serve")
        .spawn()
        .unwrap();

    // What an operator, a root process and a module build left in the prefix.
    fs::write(f.prefix.join("local.conf"), "mine").unwrap();
    fs::create_dir_all(f.prefix.join("notes")).unwrap();
    fs::write(f.prefix.join("notes/a.txt"), "a").unwrap();
    fs::write(f.prefix.join("notes/b.txt"), "b").unwrap();
    fs::write(
        f.prefix
            .join("python/lib/python3.13/__pycache__/x.cpython-313.pyc"),
        "",
    )
    .unwrap();
    fs::create_dir_all(f.prefix.join("plugins/rss/node_modules/left")).unwrap();
    fs::create_dir_all(f.prefix.join("plugins/rss/feldspar_rss.egg-info")).unwrap();
    // A file no install listed, at a path the new release ships: the new one wins.
    fs::write(f.prefix.join("README"), "an operator's notes").unwrap();

    let new = f.tree("2.0", true);
    fs::write(new.join("README"), "the release's README").unwrap();
    let (stdout, stderr) = ok(&f.install(&new, &["--no-restart"], None));

    // The server kept running through it.
    assert!(
        server.try_wait().unwrap().is_none(),
        "the server running from the old tree died during the install"
    );
    server.kill().ok();
    server.wait().ok();

    // The new release is installed, the old one is whole beside it.
    assert!(f.installed("bin/feldspar").contains("# 2.0\n"));
    let old_tree = f.dir.join("opt/feldspar.old");
    assert!(
        fs::read_to_string(old_tree.join("bin/feldspar"))
            .unwrap()
            .contains("# 1.0\n")
    );
    assert!(old_tree.join("old-only.txt").is_file());
    assert!(
        !f.prefix.join("old-only.txt").exists(),
        "a file only the old release had is still installed"
    );

    // The operator's files came along, each named once — a directory as a
    // whole — and caches and build output did not.
    assert_eq!(f.installed("local.conf"), "mine");
    assert_eq!(f.installed("notes/b.txt"), "b");
    assert!(
        stderr.contains("carrying over") && stderr.contains("/local.conf: no install put it there"),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("/notes").count(),
        1,
        "a foreign directory should be named once:\n{stderr}"
    );
    for dropped in [
        "python/lib/python3.13/__pycache__/x.cpython-313.pyc",
        "plugins/rss/node_modules",
        "plugins/rss/feldspar_rss.egg-info",
    ] {
        assert!(
            !f.prefix.join(dropped).exists(),
            "{dropped} should not have been carried over"
        );
        assert!(
            stdout.contains(&format!(
                "not carrying over {}/{dropped}",
                f.prefix.display()
            )),
            "{stdout}"
        );
    }
    assert_eq!(f.installed("README"), "the release's README");
    assert!(
        stderr.contains("/README was not put there by an install"),
        "{stderr}"
    );
    // ...and are not the install's: the manifest leaves them out.
    let manifest = f.installed("MANIFEST");
    assert!(
        !manifest
            .lines()
            .any(|l| l == "local.conf" || l.starts_with("notes"))
    );

    // What it said: both versions, how to go back, the dump, and --no-restart.
    assert!(
        stdout.contains("upgrading") && stdout.contains("feldspar 1.0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("installed") && stdout.contains("feldspar 2.0"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "mv {p} {p}.bad && mv {p}.old {p}",
            p = f.prefix.display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("database dump"), "{stdout}");
    assert!(stdout.contains("--no-restart"), "{stdout}");
    assert!(
        !stdout.contains("feldspar setup --server"),
        "an upgrade is not told to set the host up again:\n{stdout}"
    );
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn installing_the_same_tree_again_changes_nothing_but_the_old_tree() {
    let f = fixture("again");
    let tree = f.tree("2.0", true);
    ok(&f.install(&tree, &[], None));
    fs::write(f.prefix.join("local.conf"), "mine").unwrap();
    ok(&f.install(&tree, &[], None));
    let before = snapshot(&f.prefix);

    ok(&f.install(&tree, &[], None));
    assert_eq!(
        snapshot(&f.prefix),
        before,
        "a second install of the same tree changed the prefix"
    );
    assert_eq!(
        snapshot(&f.dir.join("opt/feldspar.old")),
        before,
        "the old tree should be the one this install replaced"
    );
    fs::remove_dir_all(&f.dir).ok();
}

/// A prefix installed before MANIFEST existed is read against the layout every
/// earlier tarball had — and the `.new` an interrupted run left is cleared.
#[test]
fn a_prefix_from_before_the_manifest_is_read_against_the_old_layout() {
    let f = fixture("legacy");
    for path in [
        "bin/feldspar",
        "ui/admin/dist/old.js",
        "plugins/rss/old.js",
        "python/x",
        "python.old/y",
    ] {
        fs::create_dir_all(f.prefix.join(path).parent().unwrap()).unwrap();
        fs::write(f.prefix.join(path), "old").unwrap();
    }
    fs::write(f.prefix.join("VARIANT"), "debian13-python\n").unwrap();
    fs::write(f.prefix.join("setup-host.sh"), "old").unwrap();
    fs::create_dir_all(f.prefix.join("src/.git")).unwrap();
    fs::write(f.prefix.join("src/.git/HEAD"), "ref").unwrap();
    fs::create_dir_all(f.dir.join("opt/feldspar.new/half")).unwrap();

    let tree = f.tree("2.0", false);
    let (stdout, stderr) = ok(&f.install(&tree, &[], None));
    assert!(
        stdout.contains("removing") && stdout.contains("interrupted install"),
        "{stdout}"
    );
    // The old tarball's files are the install's: replaced, not carried.
    assert!(!f.prefix.join("ui/admin/dist/old.js").exists());
    assert!(!f.prefix.join("plugins/rss/old.js").exists());
    assert!(!f.prefix.join("python.old").exists());
    // Older trees shipped setup-host.sh; this one does not, and it goes.
    assert!(!f.prefix.join("setup-host.sh").exists());
    assert!(
        !f.prefix.join("python").exists(),
        "the static tree has no python/"
    );
    assert_eq!(f.installed("VARIANT"), "static\n");
    // A source checkout is not: carried, and named.
    assert_eq!(f.installed("src/.git/HEAD"), "ref");
    assert_eq!(stderr.matches("carrying over").count(), 1, "{stderr}");
    assert!(stderr.contains("/src: no install put it there"), "{stderr}");
    // A static tree has no libc to choose.
    assert!(!f.prefix.join("LIBC").exists());
    fs::remove_dir_all(&f.dir).ok();
}

/// D6: a musl host gets the bundled glibc, whatever loader paths it has — the
/// `.glibc` copies replace the files the paths name.
#[test]
fn a_musl_host_runs_the_bundled_glibc_copies() {
    let f = fixture("musl");
    let tree = f.tree("2.0", true);
    let (stdout, _) = ok(&f.install(&tree, &[], Some(&f.musl_ldd())));
    assert!(
        f.installed("bin/feldspar")
            .contains("# 2.0 on bundled glibc\n"),
        "bin/feldspar should be the .glibc copy"
    );
    assert!(!f.prefix.join("bin/feldspar.glibc").exists());
    // python3 is still the link; the file it names is the copy.
    assert_eq!(
        fs::read_link(f.prefix.join("python/bin/python3")).unwrap(),
        PathBuf::from("python3.13")
    );
    assert!(
        f.installed("python/bin/python3.13")
            .contains("on bundled glibc")
    );
    assert!(!f.prefix.join("python/bin/python3.glibc").exists());
    assert_eq!(f.installed("LIBC"), "bundled: this is a musl host\n");
    assert!(stdout.contains("runs on the bundled glibc"), "{stdout}");
    assert!(!f.installed("MANIFEST").contains("feldspar.glibc"));
    fs::remove_dir_all(&f.dir).ok();
}

/// A glibc too old for the normal binary is found by starting it, and the
/// reason is recorded.
#[test]
fn a_binary_the_hosts_glibc_cannot_start_falls_back_to_the_bundled_one() {
    let f = fixture("old-glibc");
    let tree = f.tree("2.0", true);
    fake_binary(&tree.join("bin/feldspar"), "2.0", false);
    let (stdout, _) = ok(&f.install(&tree, &[], None));
    assert!(f.installed("bin/feldspar").contains("on bundled glibc"));
    let libc = f.installed("LIBC");
    assert!(
        libc.starts_with("bundled: the normal binary does not start")
            && libc.contains("glibc 2.36 or newer")
            && libc.contains("GLIBC_2.99"),
        "{libc}"
    );
    assert!(stdout.contains("bundled glibc"), "{stdout}");
    fs::remove_dir_all(&f.dir).ok();
}

/// The bundled-glibc binary can only be started once it is at the prefix its
/// loader path names, so it is checked after the swap — and a failure there
/// swaps back.
#[test]
fn a_bundled_glibc_binary_that_does_not_start_is_swapped_back_out() {
    let f = fixture("rollback");
    let old = f.tree("1.0", true);
    ok(&f.install(&old, &[], None));
    fs::write(
        f.dir.join("active"),
        "feldspar@main.service loaded active running x\n",
    )
    .unwrap();

    let new = f.tree("2.0", true);
    fake_binary(&new.join("bin/feldspar.glibc"), "2.0 broken", false);
    let out = f.install(&new, &["--libc", "bundled"], None);
    assert!(
        !out.status.success(),
        "a binary that does not start was installed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not start on its bundled glibc"),
        "{stderr}"
    );
    assert!(
        f.installed("bin/feldspar").contains("# 1.0\n"),
        "the old tree should be back"
    );
    assert!(f.dir.join("opt/feldspar.new/bin/feldspar").is_file());
    // The unit stopped for the swap is running the old release again.
    assert_eq!(
        f.units(),
        [
            "stop feldspar@main.service [1.0]",
            "start feldspar@main.service [1.0]"
        ]
    );
    fs::remove_dir_all(&f.dir).ok();
}

/// Running units are stopped for the swap and no longer: the stop sees the old
/// binary, the start the new one. `--no-restart` asks systemd nothing, and a
/// unit that would not stop is not started.
#[test]
fn running_units_are_stopped_for_the_swap_and_started_after_it() {
    let f = fixture("units");
    ok(&f.install(&f.tree("1.0", false), &[], None));
    fs::write(
        f.dir.join("active"),
        "feldspar.service loaded active running Saltcorn Feldspar\n\
         feldspar@main.service loaded active running feldspar instance main\n",
    )
    .unwrap();
    let (stdout, _) = ok(&f.install(&f.tree("2.0", false), &[], None));
    assert_eq!(
        f.units(),
        [
            "stop feldspar.service [1.0]",
            "stop feldspar@main.service [1.0]",
            "start feldspar.service [2.0]",
            "start feldspar@main.service [2.0]",
        ]
    );
    assert!(
        stdout.contains("restarted: feldspar.service feldspar@main.service"),
        "{stdout}"
    );

    fs::remove_file(&f.units).unwrap();
    ok(&f.install(&f.tree("3.0", false), &["--no-restart"], None));
    assert!(f.units().is_empty(), "--no-restart touched {:?}", f.units());

    fs::write(f.dir.join("stop-fails"), "").unwrap();
    let (_, stderr) = ok(&f.install(&f.tree("4.0", false), &[], None));
    assert!(
        stderr.contains("could not stop feldspar.service"),
        "{stderr}"
    );
    assert!(
        !f.units().iter().any(|u| u.starts_with("start")),
        "a unit that would not stop must not be started: {:?}",
        f.units()
    );
    assert!(f.installed("bin/feldspar").contains("# 4.0\n"));
    fs::remove_dir_all(&f.dir).ok();
}

#[test]
fn a_tree_whose_binary_does_not_start_leaves_the_prefix_alone() {
    let f = fixture("broken");
    ok(&f.install(&f.tree("1.0", false), &[], None));
    let tree = f.tree("2.0", false);
    fake_binary(&tree.join("bin/feldspar"), "2.0", false);
    let out = f.install(&tree, &[], None);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not start here"));
    assert!(f.installed("bin/feldspar").contains("# 1.0\n"));
    assert!(!f.dir.join("opt/feldspar.old").exists());
    fs::remove_dir_all(&f.dir).ok();
}

/// The installed copy is not a release tree to install from: running it would
/// copy the prefix onto itself, with the libc choice already applied.
#[test]
fn the_installed_copy_refuses_to_install_itself() {
    let f = fixture("self");
    ok(&f.install(&f.tree("1.0", true), &[], None));
    let out = f.install(&f.prefix, &[], None);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("this is an installed copy"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.dir.join("opt/feldspar.old").exists());
    fs::remove_dir_all(&f.dir).ok();
}

/// Alpine runs it: POSIX sh, and clean under shellcheck where that is here.
#[test]
fn the_tree_installer_is_posix_sh() {
    let path = workspace_root().join("scripts/lib/install-tree.sh");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("#!/bin/sh\n"));
    assert!(
        !text.contains("[["),
        "a bashism ([[) in a script Alpine's sh runs"
    );
    assert!(
        Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    if let Ok(out) = Command::new("shellcheck")
        .args(["-s", "sh"])
        .arg(&path)
        .output()
    {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
