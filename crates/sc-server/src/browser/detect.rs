//! Finding the headless browser `view_app` drives (TODO 6b.2).
//!
//! The `browser` setting names one outright. Otherwise the server looks on
//! `PATH` for [`BROWSER_NAMES`], in order, and **skips a snap**: Ubuntu's apt
//! `chromium-browser` is a transitional package whose binary is a shell script
//! that execs `/snap/bin/chromium`, and `/snap/bin/chromium` is a link to
//! `snap` itself — neither starts under a systemd service user with
//! `ProtectHome` and `PrivateTmp`. A skipped candidate is named in the reason,
//! so "no browser" on a machine that visibly has one explains itself.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The names looked for on `PATH`, in order.
pub const BROWSER_NAMES: [&str; 3] = ["chromium", "chromium-browser", "google-chrome"];

/// Where the browser is, or why there is none.
pub fn detect_browser(configured: Option<&Path>) -> Result<PathBuf, String> {
    detect_in(configured, std::env::var_os("PATH"))
}

/// [`detect_browser`] over a given `PATH`, for tests.
pub(crate) fn detect_in(
    configured: Option<&Path>,
    path_var: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(path) = configured {
        if !is_executable(path) {
            return Err(format!(
                "the `browser` setting names `{}`, which is not an executable file",
                path.display()
            ));
        }
        if let Some(why) = snap(path) {
            return Err(format!(
                "the `browser` setting names `{}`, which {why}; a snap does not run under \
                 a service user, so install a non-snap Chromium (`feldspar setup --browser` does)",
                path.display()
            ));
        }
        return Ok(path.to_owned());
    }
    let dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut skipped = Vec::new();
    for name in BROWSER_NAMES {
        for dir in &dirs {
            let candidate = dir.join(name);
            if !is_executable(&candidate) {
                continue;
            }
            match snap(&candidate) {
                Some(why) => skipped.push(format!("{} {why}", candidate.display())),
                None => return Ok(candidate),
            }
        }
    }
    let mut reason = format!("no {} on PATH", BROWSER_NAMES.join(", "));
    if !skipped.is_empty() {
        reason.push_str(&format!(
            " that is not a snap (skipped: {})",
            skipped.join("; ")
        ));
    }
    reason.push_str(
        "; install Chromium (`feldspar setup --browser` does) or set `browser` in feldspar.toml",
    );
    Err(reason)
}

/// What Chromium prints when it can neither create a user namespace nor use a
/// setuid helper — inside a container under Docker's default seccomp profile,
/// or on a host whose AppArmor forbids unprivileged user namespaces.
const NO_SANDBOX: &str = "No usable sandbox";

/// How long the boot waits for the browser's one page.
const SANDBOX_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Start the browser once, sandboxed, and say whether its sandbox works.
///
/// `view_app` would find out at the agent's first call, as an error in the
/// middle of a run; this finds out at boot and says what to change. Only a
/// browser that names its sandbox as the reason is refused: one that is slow,
/// or fails for some other reason, is left for `view_app` to report in full,
/// because a browser this check could not judge is not one it should take away.
/// The browser is never retried without its sandbox — that is the operator's
/// decision (`--no-browser-sandbox`), not this check's.
pub fn check_browser_sandbox(browser: &Path) -> Result<(), String> {
    let profile = std::env::temp_dir().join(format!(
        "feldspar-sandbox-check-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = run_sandbox_check(browser, &profile);
    let _ = std::fs::remove_dir_all(&profile);
    result
}

fn run_sandbox_check(browser: &Path, profile: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut child = match Command::new(browser)
        .args(["--headless", "--disable-gpu", "--no-first-run"])
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--dump-dom", "about:blank"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        // Not this check's finding: `view_app` will say the same, in context.
        Err(_) => return Ok(()),
    };
    // The pipe is drained on a thread so a chatty browser cannot fill it and
    // stall while this waits for it to exit.
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut out = String::new();
            let _ = pipe.read_to_string(&mut out);
            out
        })
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < SANDBOX_CHECK_TIMEOUT => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }
    };
    let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    if status.success() || !stderr.contains(NO_SANDBOX) {
        return Ok(());
    }
    Err(format!(
        "the browser at {} cannot start its sandbox (\"{NO_SANDBOX}\"), and it is not \
         started without one. In a container, run it with the seccomp profile that \
         `feldspar seccomp-profile` prints (`--security-opt seccomp=<file>`; the Docker \
         image's compose.yml does this); on an Ubuntu host, AppArmor's restriction on \
         unprivileged user namespaces is the usual cause",
        browser.display()
    ))
}

/// Why `path` is a snap, or `None`.
fn snap(path: &Path) -> Option<&'static str> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if path.starts_with("/snap") || real.starts_with("/snap") {
        return Some("is a snap");
    }
    if real.file_name().is_some_and(|n| n == "snap") {
        return Some("is a link to snap");
    }
    let mut head = Vec::with_capacity(4096);
    let read = std::fs::File::open(&real)
        .and_then(|f| f.take(4096).read_to_end(&mut head))
        .is_ok();
    if read && head.starts_with(b"#!") && String::from_utf8_lossy(&head).contains("/snap/") {
        return Some("is a script that runs a snap");
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A browser that says its sandbox is unusable is refused with the remedy;
    /// one that renders, or fails for any other reason, is left to `view_app`.
    #[test]
    fn a_browser_without_a_sandbox_is_refused_at_boot_with_the_remedy() {
        let dir = scratch("sandbox");
        let refused = executable(
            &dir,
            "no-sandbox",
            "#!/bin/sh\necho '[1:1:ERROR:zygote_host_impl_linux.cc] No usable sandbox! \
             If this is a Debian system...' >&2\nexit 1\n",
        );
        let err = check_browser_sandbox(&refused).unwrap_err();
        assert!(err.contains("feldspar seccomp-profile"), "{err}");
        assert!(err.contains("cannot start its sandbox"), "{err}");

        let fine = executable(&dir, "fine", "#!/bin/sh\necho '<html></html>'\n");
        assert_eq!(check_browser_sandbox(&fine), Ok(()));
        let other = executable(
            &dir,
            "other",
            "#!/bin/sh\necho 'out of memory' >&2\nexit 1\n",
        );
        assert_eq!(check_browser_sandbox(&other), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_path_is_searched_in_order_and_a_snap_shim_is_skipped_by_name() {
        let dir = scratch("path");
        executable(
            &dir,
            "chromium-browser",
            "#!/bin/sh\nexec /snap/bin/chromium \"$@\"\n",
        );
        let path = Some(dir.clone().into_os_string());
        let err = detect_in(None, path.clone()).unwrap_err();
        assert!(
            err.contains("chromium-browser is a script that runs a snap"),
            "{err}"
        );
        assert!(err.contains("set `browser` in feldspar.toml"), "{err}");

        let chrome = executable(&dir, "google-chrome", "\x7fELF");
        assert_eq!(detect_in(None, path.clone()), Ok(chrome));
        // `chromium` comes first.
        let chromium = executable(&dir, "chromium", "\x7fELF");
        assert_eq!(detect_in(None, path), Ok(chromium));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_setting_wins_and_is_checked() {
        let dir = scratch("setting");
        let mine = executable(&dir, "my-chrome", "\x7fELF");
        assert_eq!(detect_in(Some(&mine), None), Ok(mine.clone()));
        let missing = dir.join("nope");
        let err = detect_in(Some(&missing), None).unwrap_err();
        assert!(err.contains("not an executable file"), "{err}");
        let shim = executable(&dir, "shim", "#!/bin/sh\nexec /snap/bin/chromium\n");
        assert!(detect_in(Some(&shim), None).unwrap_err().contains("snap"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nothing_on_the_path_says_what_was_looked_for() {
        let err = detect_in(None, None).unwrap_err();
        assert!(
            err.starts_with("no chromium, chromium-browser, google-chrome on PATH"),
            "{err}"
        );
    }
}
