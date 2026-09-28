//! `feldspar cmdstan install`: a CmdStan release, downloaded and built.
//!
//! The steps are cmdstanpy's `install_cmdstan`, which is what most Stan users
//! on this machine will have run before: the release tarball from GitHub,
//! unpacked into `~/.cmdstan/cmdstan-<version>`, then `make build` inside it.
//! The build compiles Stan's math library and fetches the matching `stanc`
//! binary, so it needs `make`, a C++ compiler and the network.
//!
//! **`--jobs` defaults to 1.** Each job is a C++ compile of 1–2 GB, and a
//! machine that runs out of memory halfway through a build does not always
//! fail politely (TODO §13).
//!
//! **A half-finished install is removed.** Whatever this function created —
//! the partial download, the unpacking directory, the install directory itself
//! — is removed when it fails *or is dropped*, so a build interrupted with
//! Ctrl-C does not leave a `cmdstan-*` directory behind that discovery would
//! later find and try to use. `make` runs in a process group of its own, and
//! that whole group is killed first, so its compilers are not still writing
//! into the directory being removed.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sc_error::{Context, Error, Result};
use tokio::io::AsyncWriteExt;

use super::discover::{CmdStan, Source};
use super::version::{MIN_VERSION, Version};
use crate::process::{KillGroupOnDrop, TAIL_LINES, forward_lines};

/// GitHub's description of CmdStan's latest release; its `tag_name` is the
/// version.
pub const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/stan-dev/cmdstan/releases/latest";

/// Release assets live under `<this>/v<version>/`.
const DOWNLOAD_BASE: &str = "https://github.com/stan-dev/cmdstan/releases/download";

/// The tarball for `version` on a machine of this `os` and `arch` (Rust's
/// `std::env::consts` names).
///
/// One source tarball serves every platform except Linux on a non-x86
/// processor, which has its own asset carrying a `stanc` built for it — the
/// mapping cmdstanpy uses.
pub fn release_url(version: &Version, os: &str, arch: &str) -> String {
    let linux_arch = match (os, arch) {
        ("linux", "aarch64") => Some("arm64"),
        ("linux", "arm") => Some("armhf"),
        ("linux", "powerpc64") => Some("ppc64el"),
        ("linux", "s390x") => Some("s390x"),
        _ => None,
    };
    match linux_arch {
        Some(arch) => format!("{DOWNLOAD_BASE}/v{version}/cmdstan-{version}-linux-{arch}.tar.gz"),
        None => format!("{DOWNLOAD_BASE}/v{version}/cmdstan-{version}.tar.gz"),
    }
}

/// Where `version` is installed under `root`: `cmdstan-<version>`, the name
/// discovery looks for.
pub fn target_dir(root: &Path, version: &Version) -> PathBuf {
    root.join(format!("cmdstan-{version}"))
}

/// What to install, and where.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// `None` is the latest release.
    pub version: Option<Version>,
    /// The directory the `cmdstan-<version>` directory goes in (`~/.cmdstan`).
    pub root: PathBuf,
    /// `make -j`.
    pub jobs: u32,
}

/// What an install is doing, for the terminal.
#[derive(Debug)]
pub enum InstallEvent<'a> {
    /// The version is known, and so are where it comes from and goes.
    Resolved {
        version: &'a Version,
        url: &'a str,
        dir: &'a Path,
    },
    /// Some of the tarball has arrived. Reported every few percent, not per
    /// chunk.
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Unpacking,
    Building {
        jobs: u32,
    },
    /// One line of `make`'s output (stdout and stderr interleaved).
    Output(&'a str),
}

/// What an install ended with.
#[derive(Debug)]
pub enum Installed {
    /// Downloaded and built just now.
    Fresh(CmdStan),
    /// That version was already installed and built there; nothing was done.
    Already(CmdStan),
}

impl Installed {
    pub fn cmdstan(&self) -> &CmdStan {
        match self {
            Installed::Fresh(c) | Installed::Already(c) => c,
        }
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // GitHub's API refuses a request with no User-Agent.
        .user_agent(concat!(
            "feldspar/",
            env!("CARGO_PKG_VERSION"),
            " (cmdstan install)"
        ))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("building the HTTP client")
}

/// The version of CmdStan's latest release, from GitHub.
pub async fn latest_release() -> Result<Version> {
    let response = client()?
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| {
            format!("asking GitHub for the latest CmdStan release ({LATEST_RELEASE_API})")
        })?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("reading GitHub's answer about the latest CmdStan release")?;
    if !status.is_success() {
        return Err(Error::config(format!(
            "GitHub answered {status} when asked for the latest CmdStan release; name a version \
             with --version instead"
        )));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&body).context("parsing GitHub's latest-release answer")?;
    let tag = json
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::serde("GitHub's latest-release answer has no tag_name"))?;
    tag.parse()
}

/// Download, unpack and build a CmdStan release (see the module
/// documentation). `on` hears what it is doing.
pub async fn install(
    options: &InstallOptions,
    on: &mut (dyn FnMut(InstallEvent<'_>) + Send),
) -> Result<Installed> {
    if options.jobs == 0 {
        return Err(Error::invalid("--jobs must be at least 1"));
    }
    let version = match &options.version {
        Some(version) => version.clone(),
        None => latest_release().await?,
    };
    if !version.supported() {
        return Err(Error::invalid(format!(
            "CmdStan {version} is too old: {}.{} or newer is required",
            MIN_VERSION.major, MIN_VERSION.minor
        )));
    }
    let root = &options.root;
    let target = target_dir(root, &version);
    if target.exists() {
        return match CmdStan::at(&target, Source::Default) {
            Ok(found) if found.built() => Ok(Installed::Already(found)),
            _ => Err(Error::config(format!(
                "{} already exists but is not a built CmdStan {version}; remove it and run the \
                 install again",
                target.display()
            ))),
        };
    }
    let url = release_url(&version, std::env::consts::OS, std::env::consts::ARCH);
    on(InstallEvent::Resolved {
        version: &version,
        url: &url,
        dir: &target,
    });

    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let tarball = root.join(format!(".cmdstan-{version}.tar.gz.part"));
    let staging = root.join(format!(".cmdstan-{version}.unpacking"));
    // Declared before anything is created, so every early return below —
    // and a drop of this future — removes all three.
    let mut cleanup = RemoveOnDrop(vec![tarball.clone(), staging.clone(), target.clone()]);

    download(&url, &tarball, on).await?;

    on(InstallEvent::Unpacking);
    unpack(&tarball, &staging, &target).await?;
    let _ = std::fs::remove_file(&tarball);
    let _ = std::fs::remove_dir_all(&staging);

    on(InstallEvent::Building { jobs: options.jobs });
    build(&target, options.jobs, on).await?;

    let found = CmdStan::at(&target, Source::Default)?;
    if let Some(missing) = found.build_products().into_iter().find(|p| !p.is_file()) {
        return Err(Error::msg(format!(
            "`make build` succeeded but {} is not there",
            missing.display()
        )));
    }
    cleanup.0.clear();
    Ok(Installed::Fresh(found))
}

/// Stream `url` into `to`, reporting progress every 5 % (every 10 MB when the
/// server does not say how big it is).
async fn download(
    url: &str,
    to: &Path,
    on: &mut (dyn FnMut(InstallEvent<'_>) + Send),
) -> Result<()> {
    let mut response = client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::config(format!(
            "downloading {url}: the server answered {status}{}",
            match status.as_u16() {
                404 => " (is that a released CmdStan version?)",
                _ => "",
            }
        )));
    }
    let total = response.content_length();
    let step = total.map_or(10 << 20, |t| (t / 20).max(1));
    let mut file = tokio::fs::File::create(to)
        .await
        .with_context(|| format!("creating {}", to.display()))?;
    let (mut received, mut reported) = (0u64, 0u64);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("downloading {url}"))?
    {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", to.display()))?;
        received += chunk.len() as u64;
        if received - reported >= step {
            reported = received;
            on(InstallEvent::Downloading { received, total });
        }
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", to.display()))?;
    if reported != received {
        on(InstallEvent::Downloading { received, total });
    }
    match total {
        Some(total) if received != total => Err(Error::file(format!(
            "downloading {url}: the connection closed after {received} of {total} bytes"
        ))),
        _ => Ok(()),
    }
}

/// Unpack `tarball` into `staging`, then move the one directory it holds to
/// `target`.
///
/// With the system `tar`: a machine that can build CmdStan has one, and the
/// build itself shells out to `curl` and `make` in the same spirit. Unpacking
/// beside the target and renaming means `target` only ever appears whole.
async fn unpack(tarball: &Path, staging: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(staging).with_context(|| format!("creating {}", staging.display()))?;
    let output = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(staging)
        .kill_on_drop(true)
        .output()
        .await
        .context("running `tar` to unpack the CmdStan release")?;
    if !output.status.success() {
        return Err(Error::file(format!(
            "`tar` could not unpack {} ({}): {}",
            tarball.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut dirs = std::fs::read_dir(staging)
        .with_context(|| format!("reading {}", staging.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir());
    let (Some(unpacked), None) = (dirs.next(), dirs.next()) else {
        return Err(Error::file(format!(
            "{} did not unpack to a single directory",
            tarball.display()
        )));
    };
    std::fs::rename(&unpacked, target)
        .with_context(|| format!("moving {} to {}", unpacked.display(), target.display()))
}

/// `make build -j<jobs>` in `dir`, every line of its output passed to `on`.
async fn build(dir: &Path, jobs: u32, on: &mut (dyn FnMut(InstallEvent<'_>) + Send)) -> Result<()> {
    let mut command = tokio::process::Command::new("make");
    command
        .arg("build")
        .arg(format!("-j{jobs}"))
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("running `make build` in the CmdStan directory (is `make` on the PATH?)")?;
    let _group = KillGroupOnDrop(child.id());

    // Both streams into one channel, in the order the lines arrive.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        forward_lines(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_lines(stderr, tx.clone());
    }
    drop(tx);
    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    while let Some(line) = rx.recv().await {
        on(InstallEvent::Output(&line));
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    let status = child.wait().await.context("waiting for `make build`")?;
    if status.success() {
        return Ok(());
    }
    Err(Error::config(format!(
        "`make build -j{jobs}` in {} failed ({status}); the last lines of its output:\n{}",
        dir.display(),
        Vec::from(tail).join("\n")
    )))
}

/// Removes its paths when dropped; clear the list to keep them.
struct RemoveOnDrop(Vec<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        for path in &self.0 {
            // A compiler killed a moment ago may still be closing a file in
            // there, which makes one pass of `remove_dir_all` fail on a
            // directory that is no longer empty; a few passes settle it.
            for _ in 0..5 {
                let removed = match path.is_dir() {
                    true => std::fs::remove_dir_all(path),
                    false => std::fs::remove_file(path),
                };
                if removed.is_ok() || !path.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn the_release_url_is_the_source_tarball_except_on_linux_off_x86() {
        let base = "https://github.com/stan-dev/cmdstan/releases/download/v2.40.0";
        assert_eq!(
            release_url(&v("2.40.0"), "linux", "x86_64"),
            format!("{base}/cmdstan-2.40.0.tar.gz")
        );
        assert_eq!(
            release_url(&v("2.40.0"), "macos", "aarch64"),
            format!("{base}/cmdstan-2.40.0.tar.gz")
        );
        assert_eq!(
            release_url(&v("2.40.0"), "linux", "aarch64"),
            format!("{base}/cmdstan-2.40.0-linux-arm64.tar.gz")
        );
        assert_eq!(
            release_url(&v("2.40.0"), "linux", "powerpc64"),
            format!("{base}/cmdstan-2.40.0-linux-ppc64el.tar.gz")
        );
        assert!(
            release_url(&v("2.36.0-rc1"), "linux", "x86_64")
                .ends_with("/v2.36.0-rc1/cmdstan-2.36.0-rc1.tar.gz")
        );
    }

    #[test]
    fn the_target_directory_is_named_for_discovery_to_find() {
        let root = Path::new("/home/someone/.cmdstan");
        assert_eq!(
            target_dir(root, &v("2.40.0")),
            PathBuf::from("/home/someone/.cmdstan/cmdstan-2.40.0")
        );
    }

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sc-stan-install-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn an_old_version_or_zero_jobs_is_refused_before_anything_is_fetched() {
        let root = scratch("refused");
        let mut quiet = |_: InstallEvent<'_>| {};
        let old = InstallOptions {
            version: Some(v("2.32.2")),
            root: root.clone(),
            jobs: 1,
        };
        let err = install(&old, &mut quiet).await.unwrap_err().to_string();
        assert!(err.contains("2.32.2") && err.contains("too old"), "{err}");
        let zero = InstallOptions {
            version: Some(v("2.40.0")),
            jobs: 0,
            ..old
        };
        let err = install(&zero, &mut quiet).await.unwrap_err().to_string();
        assert!(err.contains("--jobs"), "{err}");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn an_existing_install_is_left_alone() {
        let root = scratch("existing");
        let mut heard = 0;
        let mut on = |_: InstallEvent<'_>| heard += 1;
        let options = InstallOptions {
            version: Some(v("2.40.0")),
            root: root.clone(),
            jobs: 1,
        };
        let target = target_dir(&root, &v("2.40.0"));

        // Something is there, but it is not a built CmdStan: refused, and kept.
        std::fs::create_dir_all(target.join("bin")).unwrap();
        std::fs::write(target.join("makefile"), "CMDSTAN_VERSION := 2.40.0\n").unwrap();
        let err = install(&options, &mut on).await.unwrap_err().to_string();
        assert!(
            err.contains("already exists") && err.contains("remove it"),
            "{err}"
        );
        assert!(target.join("makefile").exists());

        // Built: nothing to do.
        let existing = CmdStan::at(&target, Source::Default).unwrap();
        for product in existing.build_products() {
            std::fs::write(product, "").unwrap();
        }
        let installed = install(&options, &mut on).await.unwrap();
        assert!(matches!(installed, Installed::Already(_)), "{installed:?}");
        assert_eq!(installed.cmdstan().dir, target);
        assert_eq!(heard, 0, "nothing was downloaded or built");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_half_finished_install_is_removed_on_drop() {
        let root = scratch("cleanup");
        let (file, dir) = (root.join("a.part"), root.join("cmdstan-2.40.0"));
        std::fs::write(&file, "x").unwrap();
        std::fs::create_dir_all(dir.join("stan/src")).unwrap();
        drop(RemoveOnDrop(vec![
            file.clone(),
            dir.clone(),
            root.join("never-created"),
        ]));
        assert!(!file.exists() && !dir.exists());

        std::fs::create_dir_all(&dir).unwrap();
        let mut kept = RemoveOnDrop(vec![dir.clone()]);
        kept.0.clear();
        drop(kept);
        assert!(dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
