//! **The Python environment**: one virtual environment this server owns, and
//! `pip` (specification §9; TODO phase 5).
//!
//! ```text
//! <--python-dir>/pyvenv.cfg                        # written by `python3 -m venv`
//! <--python-dir>/bin/python                        # the interpreter pip runs under
//! <--python-dir>/lib/python3.x/site-packages/…     # what pip put there, and what
//!                                                  # `interp::isolate_path` puts on sys.path
//! ```
//!
//! Everything here is a **subprocess**, so none of it is behind the
//! `python-host` feature: creating an environment, installing into it and
//! listing what is there are the same acts whether or not this binary has an
//! interpreter linked in. That matters for the screen — the Modules tab asks
//! [`have_python`] and [`have_pip`] the way it already asks `have_npm`, and a
//! build without the feature must be able to answer.
//!
//! # The trap this module exists to close
//!
//! `pip` runs under an **external** interpreter (`--python-bin`, default
//! `python3`) and the code runs under the **embedded** one, which PyO3 linked
//! in. They are not the same program and they need not be the same version. A
//! pure-Python package does not care; a C extension built for 3.12 and imported
//! into 3.11 is a **segfault**, not an `ImportError` — the server goes down and
//! the crash is attributed to whatever ran next. So every path into the
//! environment goes through [`PythonEnvironment::ensure`], which compares the
//! two and refuses with both versions named ([`abi_refusal`]), and the boot
//! path makes the same comparison against an environment that already exists
//! before it puts anything on `sys.path`.
//!
//! # What an install is
//!
//! `pip install <specifier>` for a `pypi` module and `pip install <directory>`
//! for a local one — a **copy**, not `-e`, for the reason npm's
//! `--install-links` is used for JavaScript locals: an editable install is a
//! pointer at a checkout, and a checkout that moves or changes under a running
//! server is a module that stops being the one that was installed. Pressing
//! Install again is how a local package is updated.
//!
//! **What was installed is read from pip**, never from what the admin typed: a
//! specifier may carry a version or an extra (`httpx[http2]>=0.27`) and a
//! directory says its name only in its own metadata. `pip install --report`
//! writes exactly that — the requested distribution's name and version, as pip
//! resolved them — and where a pip too old to write one leaves nothing, the
//! answer is recovered by diffing the environment across the install.

use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use tokio::process::Command;

use crate::{PythonEnv, PythonPackage};

/// The interpreter `pip` runs under when `--python-bin` was not given.
pub const DEFAULT_PYTHON_BIN: &str = "python3";

/// The directory name under the platform's data directory, and the environment's
/// own name under that.
///
/// Beside the modules root rather than inside it: they are installed by
/// different package managers and one deleting the other's tree would be a very
/// confusing bug report.
const APP_DIR: &str = "feldspar";
const PYTHON_DIR: &str = "python";

/// Where a Python module comes from — the `source` of its `_fd_modules` row,
/// narrowed to the two a Python module can have.
///
/// The JavaScript pair is `npm` and `local`; this is `pypi` and `local`, and
/// `local` means the same thing in both: a directory on this server's disk,
/// copied in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonSource {
    /// A specifier for the Python Package Index — `httpx`, `httpx>=0.27`.
    Pypi,
    /// A directory on this server's disk holding a Python project.
    Local,
}

/// What an install turned out to be — pip's answer, not the admin's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDistribution {
    /// The distribution's own name, as its metadata declares it.
    pub name: String,
    /// The version installed.
    pub version: String,
    /// pip's output, for the admin who wants to see what happened.
    pub log: String,
}

/// Whether an interpreter is there to build the environment with.
///
/// Asked by the Modules tab before an admin types a package name, exactly as
/// `have_npm` is: "install failed: could not run python3" after filling in a
/// form is a worse way to learn that a server has no Python toolchain than a
/// sentence above the form. It is **not** the same question as whether this
/// binary has an interpreter linked in — that one is
/// [`PythonRuntime::state`](crate::PythonRuntime::state), and the two answers
/// are independent.
pub async fn have_python(bin: &Path) -> bool {
    succeeds(Command::new(bin).arg("--version")).await
}

/// Whether that interpreter has `pip`.
///
/// A separate question on the distributions that ship one without the other:
/// Debian's `python3` is a `python3-pip` away from being able to install
/// anything, and the sentence an admin needs names the package rather than the
/// exit code.
pub async fn have_pip(bin: &Path) -> bool {
    succeeds(Command::new(bin).args(["-m", "pip", "--version"])).await
}

async fn succeeds(command: &mut Command) -> bool {
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// The default environment directory for this machine, or an error saying why
/// there is none.
///
/// The rule `sc_module::default_modules_root` states, one directory over: the
/// operator's word first (`--python-dir`, or a `python_dir` in the chosen
/// `feldspar.toml` environment, both of which arrive here as an explicit path),
/// and the platform's **data** directory last, because installed packages are
/// state rather than configuration.
///
/// | | environment |
/// |---|---|
/// | Linux/BSD | `$XDG_DATA_HOME/feldspar/python` (else `~/.local/share/feldspar/python`) |
/// | macOS | `~/Library/Application Support/feldspar/python` |
/// | Windows | `%APPDATA%\feldspar\python` |
pub fn default_python_dir() -> Result<PathBuf> {
    user_data_dir()
        .map(|dir| dir.join(APP_DIR).join(PYTHON_DIR))
        .ok_or_else(|| {
            Error::config(
                "cannot tell where to put this server's Python environment: no user data \
                 directory could be determined (set XDG_DATA_HOME or HOME, or pass \
                 --python-dir)",
            )
        })
}

/// The platform's per-user data directory.
fn user_data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return env_var("APPDATA").map(PathBuf::from);
    }
    if let Some(xdg) = env_var("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg));
    }
    let home = PathBuf::from(env_var("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library").join("Application Support"))
    } else {
        Some(home.join(".local").join("share"))
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The directory a release tree keeps its CPython in, beside `bin/`.
const BUNDLED_DIR: &str = "python";

/// The **bundled** CPython's home, for the binary at `exe`: `<exe>/../python`
/// when that directory exists (TODO-portable-release §3).
///
/// The release tarball ships python-build-standalone there, and nothing on the
/// host says where its standard library is — CPython's own search starts from
/// the `python3` on the `PATH`, which on a host with no Python finds nothing and
/// on one with a different Python finds the wrong one. A developer build and
/// `cargo test` have no such directory and keep CPython's own search.
#[must_use]
pub fn bundled_python_home_for(exe: &Path) -> Option<PathBuf> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let home = exe.parent()?.parent()?.join(BUNDLED_DIR);
    home.is_dir().then_some(home)
}

/// [`bundled_python_home_for`] this process's own executable, symlinks
/// resolved — so `/usr/local/bin/feldspar` pointing into `/opt/feldspar/bin`
/// finds `/opt/feldspar/python`.
#[must_use]
pub fn bundled_python_home() -> Option<PathBuf> {
    bundled_python_home_for(&std::env::current_exe().ok()?)
}

/// The bundled interpreter under a home: `<home>/bin/python3`, if it is there.
#[must_use]
pub fn bundled_python_bin_in(home: &Path) -> Option<PathBuf> {
    let bin = home.join("bin").join("python3");
    bin.is_file().then_some(bin)
}

/// The interpreter `pip` runs under when `--python-bin` was not given: the
/// bundled one when this binary has one beside it, so pip and the embedded
/// interpreter are the same CPython and the ABI check cannot trip on a fresh
/// host; `python3` on the `PATH` otherwise.
#[must_use]
pub fn default_python_bin() -> PathBuf {
    bundled_python_home()
        .as_deref()
        .and_then(bundled_python_bin_in)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_PYTHON_BIN))
}

impl PythonEnv {
    /// The environment directory this server will use: `--python-dir`, else the
    /// platform's default.
    ///
    /// `None` only on a machine with neither a home nor an `XDG_DATA_HOME`,
    /// where there is nothing truthful to say — and where saying "." would put
    /// a server's packages wherever it happened to be started.
    #[must_use]
    pub fn directory(&self) -> Option<PathBuf> {
        match &self.dir {
            Some(dir) => Some(dir.clone()),
            None => default_python_dir().ok(),
        }
    }

    /// The external interpreter `pip` runs under: `--python-bin`, else
    /// [`default_python_bin`] — the bundled one, or `python3` on the PATH.
    #[must_use]
    pub fn interpreter(&self) -> PathBuf {
        match &self.bin {
            Some(bin) => bin.clone(),
            None => default_python_bin(),
        }
    }
}

/// The directory an installed package lives in, under a virtual environment.
///
/// Computed from a `(major, minor)` rather than looked up, because the caller
/// that matters computes it from the **embedded** interpreter's version: that is
/// the one that will import what is there, and the version of the `python3` that
/// built the environment may not be it — which is the whole of the ABI check.
#[must_use]
pub fn site_packages(dir: &Path, major: u32, minor: u32) -> PathBuf {
    if cfg!(windows) {
        dir.join("Lib").join("site-packages")
    } else {
        dir.join("lib")
            .join(format!("python{major}.{minor}"))
            .join("site-packages")
    }
}

/// The version of the interpreter a virtual environment was built with, from
/// its own `pyvenv.cfg`, or `None` if there is no environment there.
///
/// Read rather than executed: this is asked on the boot path, before anything
/// has been put on `sys.path`, and a boot that ran a subprocess to find out
/// whether it may use a directory would be paying for the answer on every
/// start. `venv` writes `version = 3.13.2` and newer ones also write
/// `version_info = 3.13.2.final.0`; either is enough.
#[must_use]
pub fn venv_version(dir: &Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(dir.join("pyvenv.cfg")).ok()?;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !matches!(key.trim(), "version" | "version_info") {
            continue;
        }
        if let Some(parts) = parse_version(value.trim()) {
            return Some(parts);
        }
    }
    None
}

/// The directory of the interpreter a virtual environment was built from — the
/// `home` line `venv` writes into `pyvenv.cfg` — or `None` without one.
#[must_use]
pub fn venv_home(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join("pyvenv.cfg")).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "home").then(|| PathBuf::from(value.trim()))
    })
}

/// Whether two paths name one directory, symlinks resolved where they can be.
fn same_dir(a: &Path, b: &Path) -> bool {
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    resolve(a) == resolve(b)
}

/// `3.13.2` → `(3, 13)`. The patch and anything after it is not this module's
/// business: the ABI is `major.minor`.
#[must_use]
pub fn parse_version(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    Some((major, minor))
}

/// The sentence a version mismatch is refused with (§9).
///
/// **Both versions and the flag that fixes it**, because every other phrasing
/// leaves the admin to guess which of the two interpreters is the wrong one —
/// and because the alternative to refusing is a segfault, which is not a
/// diagnosis anybody can act on.
#[must_use]
pub fn abi_refusal(embedded: (u32, u32), external: (u32, u32), dir: &Path, bin: &Path) -> Error {
    Error::config(format!(
        "this server's own Python is {}.{} but the environment at {} is built with Python \
         {}.{} ({}). A package compiled for one is not importable by the other — a C \
         extension crashes the server rather than failing to import — so the environment is \
         not used. Point --python-bin at a python{}.{} and remove {}, or run a server built \
         against Python {}.{}.",
        embedded.0,
        embedded.1,
        dir.display(),
        external.0,
        external.1,
        bin.display(),
        embedded.0,
        embedded.1,
        dir.display(),
        external.0,
        external.1,
    ))
}

/// The packages installed under one `site-packages` directory.
///
/// A directory read of `*.dist-info` rather than a `pip list`: a screen should
/// not need a subprocess to render, and a missing or unreadable directory is an
/// empty listing rather than an error — an environment nobody has installed
/// into yet is the ordinary case.
#[must_use]
pub fn packages_in(site_packages: &Path) -> Vec<PythonPackage> {
    let Ok(entries) = std::fs::read_dir(site_packages) else {
        return Vec::new();
    };
    let mut found: Vec<PythonPackage> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".dist-info")
                .or_else(|| name.strip_suffix(".egg-info"))?;
            // `numpy-2.1.0.dist-info`, and `-` is legal in neither half after
            // the normalisation a distribution's metadata directory carries — so
            // the last one is the separator.
            Some(match stem.rsplit_once('-') {
                Some((name, version)) => PythonPackage {
                    name: name.to_owned(),
                    version: Some(version.to_owned()),
                },
                None => PythonPackage {
                    name: stem.to_owned(),
                    version: None,
                },
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup();
    found
}

/// A distribution name reduced to the one spelling everything can be compared
/// in: PEP 503's, lower-cased with every run of `-`, `_` and `.` collapsed to a
/// single `-`.
///
/// `sc-fixture`, `sc_fixture` and `SC.Fixture` are one distribution, and the
/// admin who typed one of them must find the module that was stored under
/// another.
#[must_use]
pub fn normalise_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !last_was_separator && !out.is_empty() {
                out.push('-');
            }
            last_was_separator = true;
        } else {
            out.extend(ch.to_lowercase());
            last_was_separator = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The virtual environment this server installs Python packages into.
///
/// Constructed from the runtime rather than by hand
/// ([`PythonRuntime::environment`](crate::PythonRuntime::environment)), because
/// the one thing it must know beyond the two paths is the **embedded**
/// interpreter's version — and asking the runtime for that is what makes sure
/// there is one.
#[derive(Debug, Clone)]
pub struct PythonEnvironment {
    dir: PathBuf,
    bin: PathBuf,
    /// The embedded interpreter's `(major, minor)`, where this process has one
    /// running. `None` means the ABI check cannot be made and is therefore not
    /// claimed — a build without the feature can still list what is installed.
    embedded: Option<(u32, u32)>,
    /// The interpreter the release tree bundles beside this binary, if it has
    /// one: what an environment built by the host's `python3` is moved onto
    /// ([`PythonEnvironment::ensure`]).
    bundled: Option<PathBuf>,
}

impl PythonEnvironment {
    /// The environment `env` names, for a process whose embedded interpreter is
    /// `embedded`.
    ///
    /// Fails only when there is no directory to use and none can be defaulted,
    /// which is [`default_python_dir`]'s error and names the flag.
    pub fn new(env: &PythonEnv, embedded: Option<(u32, u32)>) -> Result<PythonEnvironment> {
        // `directory()` is `--python-dir` or the default, and the only way it
        // answers `None` is the default's own failure — so that is the error to
        // report, because it is the one that names the flag.
        let dir = match env.directory() {
            Some(dir) => dir,
            None => default_python_dir()?,
        };
        Ok(PythonEnvironment {
            dir,
            bin: env.interpreter(),
            embedded,
            bundled: bundled_python_home()
                .as_deref()
                .and_then(bundled_python_bin_in),
        })
    }

    /// Where the environment is (`--python-dir`, or the platform default).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The external interpreter it is built with (`--python-bin`).
    #[must_use]
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// The environment's **own** interpreter — what `pip` is run as.
    ///
    /// Not `--python-bin` with a `--target`: a venv's `python` is how pip is
    /// told which environment it is installing into, and it is the same program
    /// the environment was built from.
    #[must_use]
    pub fn python(&self) -> PathBuf {
        if cfg!(windows) {
            self.dir.join("Scripts").join("python.exe")
        } else {
            self.dir.join("bin").join("python")
        }
    }

    /// Whether the environment has been created.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.dir.join("pyvenv.cfg").is_file()
    }

    /// The version the environment was built with, if it exists.
    #[must_use]
    pub fn version(&self) -> Option<(u32, u32)> {
        venv_version(&self.dir)
    }

    /// Where a package lands inside it — the environment's own version, or the
    /// embedded interpreter's where there is no environment yet to ask.
    #[must_use]
    pub fn site_packages(&self) -> Option<PathBuf> {
        let (major, minor) = self.version().or(self.embedded)?;
        Some(site_packages(&self.dir, major, minor))
    }

    /// What is installed in it.
    #[must_use]
    pub fn packages(&self) -> Vec<PythonPackage> {
        self.site_packages()
            .as_deref()
            .map(packages_in)
            .unwrap_or_default()
    }

    /// The version of one installed distribution, by any spelling of its name.
    #[must_use]
    pub fn installed_version(&self, name: &str) -> Option<String> {
        let wanted = normalise_name(name);
        self.packages()
            .into_iter()
            .find(|package| normalise_name(&package.name) == wanted)
            .and_then(|package| package.version)
    }

    /// Whether a distribution by this name is installed.
    #[must_use]
    pub fn is_installed(&self, name: &str) -> bool {
        let wanted = normalise_name(name);
        self.packages()
            .iter()
            .any(|package| normalise_name(&package.name) == wanted)
    }

    /// Create the environment if it is not there, and check that what is there
    /// is a version this process can import from (§9).
    ///
    /// Idempotent, and called before every install for the reason
    /// `Installer::ensure_project` is: an environment an operator deleted
    /// between two installs should be rebuilt rather than reported.
    ///
    /// An environment that exists but was built by another interpreter of the
    /// **same** minor version — Debian's `python3.13` on a host upgraded to the
    /// release that bundles CPython 3.13 — is moved onto the bundled one first
    /// ([`PythonEnvironment::repoint`]), so it stops depending on a `python3`
    /// the operator may now remove. A different minor version is not moved: its
    /// C extensions are built for that one, and the ABI check refuses it.
    ///
    /// The ABI check is made against the environment's **own** interpreter and
    /// not against `--python-bin`, because that is the one pip will run and the
    /// one whose `site-packages` the embedded interpreter will import from. An
    /// operator who repoints `--python-bin` at a different version has not
    /// changed the environment, and the refusal has to be about what is on the
    /// disk.
    pub async fn ensure(&self) -> Result<()> {
        if !self.exists() {
            // The parent, so a `--python-dir` two levels below anything that
            // exists is created rather than reported by `venv` as a missing
            // path.
            if let Some(parent) = self.dir.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::config(format!("creating {}: {e}", parent.display())))?;
            }
            let output = Command::new(&self.bin)
                .arg("-m")
                .arg("venv")
                .arg(&self.dir)
                .output()
                .await
                .map_err(|e| {
                    Error::config(format!(
                        "could not run `{} -m venv {}`: {e}. Installing a Python module needs \
                         a Python 3 interpreter on this server's PATH (--python-bin names \
                         which one)",
                        self.bin.display(),
                        self.dir.display()
                    ))
                })?;
            if !output.status.success() {
                // `venv`'s own words: on Debian the useful half of this is
                // "ensurepip is not available … apt install python3-venv",
                // which an exit code would throw away (§16).
                return Err(Error::config(format!(
                    "could not create this server's Python environment at {} with {}: {}\n{}",
                    self.dir.display(),
                    self.bin.display(),
                    output.status,
                    combined(&output)
                )));
            }
        } else {
            self.repoint().await?;
        }
        self.check_abi()
    }

    /// Whether this environment should be moved onto the bundled interpreter:
    /// there is one, it is the interpreter pip runs under (an operator's own
    /// `--python-bin` is their choice and is left alone), the environment was
    /// built by the same `major.minor` as the embedded interpreter, and by an
    /// interpreter somewhere else — or its `bin/python` is a link to one, which
    /// is what an interrupted move leaves, and an environment whose interpreter
    /// the operator has since removed.
    fn needs_repoint(&self) -> bool {
        let Some(bundled) = &self.bundled else {
            return false;
        };
        if *bundled != self.bin {
            return false;
        }
        let (Some(embedded), Some(built)) = (self.embedded, self.version()) else {
            return false;
        };
        if embedded != built {
            return false;
        }
        let moved = match (venv_home(&self.dir), bundled.parent()) {
            (Some(home), Some(bundled_home)) => same_dir(&home, bundled_home),
            _ => true,
        };
        // A copied interpreter (`venv --copies`) is the environment's own, and
        // never resolves to the bundle: only a link is followed.
        let python = self.python();
        let linked_elsewhere = python.is_symlink()
            && match (std::fs::canonicalize(&python), std::fs::canonicalize(bundled)) {
                (Ok(target), Ok(bundled)) => target != bundled,
                _ => true,
            };
        !moved || linked_elsewhere
    }

    /// Remove the environment's interpreter links — `bin/python`, `python3`,
    /// `python3.N` — so `venv --upgrade` makes them again: it leaves a link that
    /// already exists alone, so without this the environment keeps running the
    /// host's `python3` after its `pyvenv.cfg` has been moved.
    fn unlink_interpreters(&self) -> Result<()> {
        let bin = self.dir.join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let interpreter = name
                .strip_prefix("python")
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'));
            if interpreter && entry.path().is_symlink() {
                std::fs::remove_file(entry.path()).map_err(|e| {
                    Error::config(format!(
                        "could not remove {} to move this server's Python environment onto {}: {e}",
                        entry.path().display(),
                        self.bin.display()
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// `<bundled python3> -m venv --upgrade <dir>`, where
    /// [`needs_repoint`](Self::needs_repoint) says so (TODO-portable-release
    /// R1.6).
    ///
    /// `--upgrade` rewrites `pyvenv.cfg`, makes the `bin/python` links
    /// [`unlink_interpreters`](Self::unlink_interpreters) removed, and leaves
    /// `site-packages` alone, so every installed module stays
    /// installed: what changes is only which program pip runs as. The embedded
    /// interpreter imports from `site-packages` directly, so a server that
    /// never installs again would not need this — but the first install after
    /// the operator removes the host's `python3` would.
    async fn repoint(&self) -> Result<()> {
        if !self.needs_repoint() {
            return Ok(());
        }
        self.unlink_interpreters()?;
        let output = Command::new(&self.bin)
            .arg("-m")
            .arg("venv")
            .arg("--upgrade")
            .arg(&self.dir)
            .output()
            .await
            .map_err(|e| {
                Error::config(format!(
                    "could not run `{} -m venv --upgrade {}`: {e}",
                    self.bin.display(),
                    self.dir.display()
                ))
            })?;
        if !output.status.success() {
            return Err(Error::config(format!(
                "could not move this server's Python environment at {} onto {}: {}\n{}",
                self.dir.display(),
                self.bin.display(),
                output.status,
                combined(&output)
            )));
        }
        Ok(())
    }

    /// Compare the embedded interpreter with the environment's, and refuse a
    /// mismatch.
    ///
    /// A process with no interpreter of its own — a build without the feature,
    /// listing what is installed for a screen — has nothing to compare and
    /// says nothing, rather than inventing an answer from the version that
    /// happens to be on the PATH.
    pub fn check_abi(&self) -> Result<()> {
        let (Some(embedded), Some(external)) = (self.embedded, self.version()) else {
            return Ok(());
        };
        if embedded == external {
            return Ok(());
        }
        Err(abi_refusal(embedded, external, &self.dir, &self.bin))
    }

    /// Install `location` into the environment, and report what it turned out
    /// to be.
    ///
    /// `source` decides only how `location` is read: a specifier goes to pip as
    /// it stands, and a local directory is canonicalised first — pip records
    /// the path it is given, and a relative one would be resolved against
    /// wherever this process was started.
    pub async fn install(
        &self,
        source: PythonSource,
        location: &str,
    ) -> Result<InstalledDistribution> {
        // What the admin typed is read before the environment is built: a typo
        // in a path should not cost a `python3 -m venv` first.
        let spec = match source {
            PythonSource::Pypi => {
                let spec = location.trim();
                if spec.is_empty() {
                    return Err(Error::invalid(
                        "a Python module needs a package name to install from PyPI",
                    ));
                }
                spec.to_owned()
            }
            PythonSource::Local => {
                let path = PathBuf::from(location.trim());
                let path = tokio::fs::canonicalize(&path).await.map_err(|e| {
                    Error::invalid(format!(
                        "no directory at {} to install a Python module from: {e}",
                        path.display()
                    ))
                })?;
                if !project_file(&path) {
                    return Err(Error::invalid(format!(
                        "{} is not a Python project: it has no pyproject.toml, setup.py or \
                         setup.cfg",
                        path.display()
                    )));
                }
                path.display().to_string()
            }
        };

        self.ensure().await?;

        // pip's report is the authority on what was asked for; the listing
        // either side of the install is the fallback for a pip too old to write
        // one. Both are needed, and the second costs two directory reads.
        let report = self.dir.join(REPORT_FILE);
        let _ = tokio::fs::remove_file(&report).await;
        let before = self.packages();
        let base = ["install", "--disable-pip-version-check", "--no-input"];
        let mut args: Vec<&str> = base.to_vec();
        let report_path = report.display().to_string();
        args.extend(["--report", &report_path, &spec]);
        let log = match self.pip(&args).await {
            Ok(log) => log,
            // A pip older than 22.2 does not know `--report` and says so before
            // it does anything, so the install is retried without it and the
            // diff below answers what was installed. Any other failure is the
            // install's own and is reported as it stands.
            Err(e) if e.to_string().contains("--report") => {
                let mut args: Vec<&str> = base.to_vec();
                args.push(&spec);
                self.pip(&args).await?
            }
            Err(e) => return Err(e),
        };
        let after = self.packages();
        let requested = requested_distribution(&report);
        let _ = tokio::fs::remove_file(&report).await;

        let (name, version) = match requested {
            Some(found) => found,
            None => installed_name(&before, &after, source, &spec)?,
        };
        // From the disk rather than from the report where both are available:
        // what a body will import is what is in `site-packages`, and a report
        // that disagreed with it would be describing a different install.
        let version = self.installed_version(&name).unwrap_or(version);
        Ok(InstalledDistribution { name, version, log })
    }

    /// Remove a distribution from the environment.
    ///
    /// Best effort by design, exactly as npm's is: the row is what makes a
    /// module *exist* to this server, so a package pip declines to remove must
    /// not keep the module installed. The caller deletes the row either way and
    /// this reports what pip said.
    pub async fn uninstall(&self, name: &str) -> Result<String> {
        if !self.exists() {
            // Nothing was ever installed here, and creating an environment in
            // order to uninstall from it would be absurd.
            return Ok(String::new());
        }
        // pip runs as the environment's own interpreter, which may be the
        // host's `python3` an operator has since removed.
        self.repoint().await?;
        self.pip(&["uninstall", "--disable-pip-version-check", "-y", name])
            .await
    }

    /// Run pip in this environment, returning its output.
    async fn pip(&self, args: &[&str]) -> Result<String> {
        let python = self.python();
        let line = format!("{} -m pip {}", python.display(), args.join(" "));
        let output = Command::new(&python)
            .arg("-m")
            .arg("pip")
            .args(args)
            .output()
            .await
            .map_err(|e| {
                Error::config(format!(
                    "could not run `{line}`: {e}. Installing a Python module needs pip in \
                     this server's Python environment"
                ))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            // pip's own diagnostics are the only useful part of this (§16): the
            // resolver's "no matching distribution", the build backend's
            // traceback or the proxy that would not answer is what the admin
            // acts on, and the exit code is not.
            return Err(Error::config(format!(
                "`{line}` failed with {}\n{}",
                output.status,
                tail(&stderr, &stdout)
            )));
        }
        Ok(tail(&stdout, &stderr))
    }
}

/// The report file pip is asked to write, inside the environment it is
/// installing into — a directory this server owns and can write, which the
/// system temporary directory is not always.
const REPORT_FILE: &str = ".saltcorn-install-report.json";

/// Whether a directory looks like something pip can build.
fn project_file(dir: &Path) -> bool {
    ["pyproject.toml", "setup.py", "setup.cfg"]
        .iter()
        .any(|name| dir.join(name).is_file())
}

/// The name and version of the distribution the admin actually asked for, from
/// pip's `--report`.
///
/// `install[]` holds the whole resolved set — the requested package and every
/// dependency it dragged in — and exactly one entry carries `requested: true`.
/// That is the distinction the npm installer gets from the project file's
/// dependency diff, and it is why the report is worth reading rather than
/// parsing "Successfully installed a-1 b-2", whose order means nothing.
fn requested_distribution(report: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(report).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let entries = json.get("install")?.as_array()?;
    let requested = entries
        .iter()
        .find(|entry| entry.get("requested").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| entries.first())?;
    let metadata = requested.get("metadata")?;
    Some((
        metadata.get("name")?.as_str()?.to_owned(),
        metadata
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("0.0.0")
            .to_owned(),
    ))
}

/// Which distribution the install added, when pip wrote no report.
///
/// The one that appeared, or whose version changed. A specifier that resolved
/// to something already installed at the same version changes nothing, and then
/// the name is recovered from the specifier itself — which is the last resort
/// and the only one that can be wrong, because a specifier's name is what the
/// admin typed rather than what pip resolved.
fn installed_name(
    before: &[PythonPackage],
    after: &[PythonPackage],
    source: PythonSource,
    spec: &str,
) -> Result<(String, String)> {
    let mut changed = after.iter().filter(|package| !before.contains(package));
    if let (Some(only), None) = (changed.next(), changed.next()) {
        return Ok((
            only.name.clone(),
            only.version.clone().unwrap_or_else(|| "0.0.0".to_owned()),
        ));
    }
    if source == PythonSource::Local {
        return Err(Error::msg(format!(
            "pip installed from {spec} but this server could not tell which distribution that \
             was: pip wrote no install report (pip 22.2 or newer writes one) and the \
             environment gained more than one package"
        )));
    }
    let name = pypi_spec_name(spec)?;
    let version = after
        .iter()
        .find(|package| normalise_name(&package.name) == normalise_name(&name))
        .and_then(|package| package.version.clone())
        .unwrap_or_else(|| "0.0.0".to_owned());
    Ok((name, version))
}

/// The distribution name in a PyPI specifier: `httpx[http2]>=0.27` is `httpx`.
///
/// Everything from the first character that cannot be in a name onwards is a
/// version, an extra or a marker, and none of them is part of what the package
/// is called.
pub fn pypi_spec_name(spec: &str) -> Result<String> {
    let spec = spec.trim();
    let end = spec
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(spec.len());
    let name = spec[..end].trim();
    if name.is_empty() {
        return Err(Error::invalid(format!(
            "`{spec}` does not name a Python package"
        )));
    }
    Ok(name.to_owned())
}

/// A process's combined output, for a failure whose useful half could be in
/// either stream.
fn combined(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    tail(&stderr, &stdout)
}

/// The tail of a command's output: the first stream if it said anything, else
/// the second, capped so an error message stays readable.
fn tail(first: &str, second: &str) -> String {
    let text = if first.trim().is_empty() {
        second
    } else {
        first
    };
    const MAX: usize = 4000;
    if text.len() <= MAX {
        return text.trim().to_owned();
    }
    let start = text.len() - MAX;
    let start = (start..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    format!("…\n{}", text[start..].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_specifiers_name_stops_at_the_version_or_the_extra() {
        assert_eq!(pypi_spec_name("httpx").unwrap(), "httpx");
        assert_eq!(pypi_spec_name("httpx>=0.27").unwrap(), "httpx");
        assert_eq!(pypi_spec_name("httpx[http2]>=0.27").unwrap(), "httpx");
        assert_eq!(
            pypi_spec_name("saltcorn-mqtt==0.2.0").unwrap(),
            "saltcorn-mqtt"
        );
        assert!(pypi_spec_name("==1.0").is_err());
    }

    #[test]
    fn one_distribution_has_one_spelling() {
        assert_eq!(normalise_name("SC_Fixture"), "sc-fixture");
        assert_eq!(normalise_name("sc.fixture"), "sc-fixture");
        assert_eq!(normalise_name("sc--fixture"), "sc-fixture");
        assert_eq!(normalise_name("sc-fixture"), normalise_name("sc_fixture"));
    }

    #[test]
    fn the_refusal_names_both_versions_and_the_flag() {
        let said = abi_refusal(
            (3, 11),
            (3, 14),
            Path::new("/srv/python"),
            Path::new("/usr/bin/python3"),
        )
        .to_string();
        assert!(said.contains("3.11"), "{said}");
        assert!(said.contains("3.14"), "{said}");
        assert!(said.contains("--python-bin"), "{said}");
        assert!(said.contains("/srv/python"), "{said}");
    }

    #[test]
    fn an_environment_directory_is_where_a_venv_puts_its_packages() {
        let dir = Path::new("/srv/python");
        let packages = site_packages(dir, 3, 13);
        if cfg!(windows) {
            assert!(packages.ends_with("Lib/site-packages"), "{packages:?}");
        } else {
            assert_eq!(packages, dir.join("lib/python3.13/site-packages"));
        }
    }

    #[test]
    fn the_default_environment_is_beside_the_modules_root() {
        let Ok(dir) = default_python_dir() else {
            return;
        };
        assert!(
            dir.ends_with(PathBuf::from(APP_DIR).join(PYTHON_DIR)),
            "{dir:?}"
        );
        assert!(dir.is_absolute(), "{dir:?}");
    }

    #[test]
    fn a_pyvenv_cfg_says_which_interpreter_built_the_environment() {
        let dir = std::env::temp_dir().join(format!("sc-python-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(venv_version(&dir), None, "no environment, no version");
        std::fs::write(
            dir.join("pyvenv.cfg"),
            "home = /usr/bin\ninclude-system-site-packages = false\nversion = 3.13.2\n",
        )
        .unwrap();
        assert_eq!(venv_version(&dir), Some((3, 13)));
        assert_eq!(venv_home(&dir), Some(PathBuf::from("/usr/bin")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-python-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_release_tree_has_its_python_home_beside_bin() {
        let root = scratch("bundled-home");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/feldspar"), "").unwrap();
        let exe = root.join("bin/feldspar");
        assert_eq!(bundled_python_home_for(&exe), None, "no python/ yet");

        std::fs::create_dir_all(root.join("python/bin")).unwrap();
        let home = bundled_python_home_for(&exe).expect("python/ beside bin/");
        assert!(same_dir(&home, &root.join("python")), "{home:?}");
        assert_eq!(bundled_python_bin_in(&home), None, "no interpreter yet");
        std::fs::write(root.join("python/bin/python3"), "").unwrap();
        assert!(bundled_python_bin_in(&home).is_some_and(|b| b.ends_with("bin/python3")));

        // A link to the binary finds the tree it points into.
        #[cfg(unix)]
        {
            let link = root.join("feldspar-link");
            std::os::unix::fs::symlink(&exe, &link).unwrap();
            assert!(bundled_python_home_for(&link).is_some_and(|h| same_dir(&h, &home)));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cargo_test_binary_has_no_bundled_python() {
        // `target/debug/deps/<test>` → `target/debug/python`, which no build makes:
        // a developer build starts CPython as it always has and pip runs `python3`.
        assert_eq!(bundled_python_home(), None);
        assert_eq!(default_python_bin(), PathBuf::from(DEFAULT_PYTHON_BIN));
        assert_eq!(
            PythonEnv::default().interpreter(),
            PathBuf::from(DEFAULT_PYTHON_BIN)
        );
    }

    /// A stand-in for the bundled `python3`: records its arguments, and does
    /// what `-m venv --upgrade` does — points `pyvenv.cfg`'s `home` at itself,
    /// and links `bin/python*` to itself, but only where no link is already
    /// there (CPython's `symlink_or_copy`).
    #[cfg(unix)]
    fn fake_bundled(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = root.join("bundle/bin/python3");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 cfg=\"$4/pyvenv.cfg\"\n\
                 sed 's,^home = .*,home = {home},' \"$cfg\" > \"$cfg.new\" && mv \"$cfg.new\" \"$cfg\"\n\
                 for n in python python3 python3.13; do\n\
                 [ -L \"$4/bin/$n\" ] || ln -s '{bin}' \"$4/bin/$n\"\n\
                 done\n",
                log = root.join("calls").display(),
                home = bin.parent().unwrap().display(),
                bin = bin.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// An environment as `<home>/python3 -m venv` leaves it: `pyvenv.cfg`, and
    /// `bin/python*` linked to a stand-in for that interpreter.
    #[cfg(unix)]
    fn environment_built_by(root: &Path, home: &str, version: &str) -> PathBuf {
        let dir = root.join("venv");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join("pyvenv.cfg"),
            format!("home = {home}\ninclude-system-site-packages = false\nversion = {version}\n"),
        )
        .unwrap();
        let host = root.join("host/python3");
        std::fs::create_dir_all(host.parent().unwrap()).unwrap();
        std::fs::write(&host, "").unwrap();
        std::os::unix::fs::symlink(&host, dir.join("bin/python")).unwrap();
        std::os::unix::fs::symlink("python", dir.join("bin/python3")).unwrap();
        std::os::unix::fs::symlink("python", dir.join("bin/python3.13")).unwrap();
        dir
    }

    /// Where the environment's `bin/python` leads.
    #[cfg(unix)]
    fn interpreter_of(dir: &Path) -> PathBuf {
        std::fs::canonicalize(dir.join("bin/python")).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_environment_from_the_hosts_python_of_the_same_minor_is_moved_onto_the_bundled_one()
    {
        let root = scratch("repoint");
        let bundled = fake_bundled(&root);
        let dir = environment_built_by(&root, "/usr/bin", "3.13.5");
        let environment = PythonEnvironment {
            dir: dir.clone(),
            bin: bundled.clone(),
            embedded: Some((3, 13)),
            bundled: Some(bundled.clone()),
        };
        environment.ensure().await.expect("repointed and accepted");
        let calls = std::fs::read_to_string(root.join("calls")).unwrap();
        assert_eq!(calls, format!("-m venv --upgrade {}\n", dir.display()));
        assert!(same_dir(
            &venv_home(&dir).unwrap(),
            bundled.parent().unwrap()
        ));
        // And pip runs as the bundled interpreter: the host's links are gone.
        for name in ["python", "python3", "python3.13"] {
            assert_eq!(
                std::fs::canonicalize(dir.join("bin").join(name)).unwrap(),
                std::fs::canonicalize(&bundled).unwrap(),
                "bin/{name}"
            );
        }

        // Once moved, it is left alone — by an install and by an uninstall.
        environment.ensure().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("calls")).unwrap(),
            calls,
            "an environment already on the bundled interpreter is not upgraded again"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What an earlier move left behind: `pyvenv.cfg` names the bundle, but
    /// `bin/python` is still the host's, because `venv --upgrade` keeps a link
    /// that is already there. And the same with the host's `python3` removed,
    /// so the link dangles.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_environment_whose_interpreter_still_links_to_the_host_is_moved() {
        let root = scratch("half-moved");
        let bundled = fake_bundled(&root);
        let home = bundled.parent().unwrap().display().to_string();
        for remove_host in [false, true] {
            let dir = environment_built_by(&root, &home, "3.13.18");
            if remove_host {
                std::fs::remove_file(root.join("host/python3")).unwrap();
            }
            let environment = PythonEnvironment {
                dir: dir.clone(),
                bin: bundled.clone(),
                embedded: Some((3, 13)),
                bundled: Some(bundled.clone()),
            };
            assert!(environment.needs_repoint(), "host removed: {remove_host}");
            environment.ensure().await.expect("repointed");
            assert_eq!(
                interpreter_of(&dir),
                std::fs::canonicalize(&bundled).unwrap(),
                "host removed: {remove_host}"
            );
            assert!(!environment.needs_repoint());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_different_minor_or_an_operators_own_interpreter_is_not_moved() {
        let root = scratch("no-repoint");
        let bundled = fake_bundled(&root);

        // Built by a 3.12: its C extensions are 3.12's, so it is refused as before.
        let dir = environment_built_by(&root, "/usr/bin", "3.12.3");
        let environment = PythonEnvironment {
            dir: dir.clone(),
            bin: bundled.clone(),
            embedded: Some((3, 13)),
            bundled: Some(bundled.clone()),
        };
        let refused = environment.ensure().await.unwrap_err().to_string();
        assert!(
            refused.contains("3.12") && refused.contains("3.13"),
            "{refused}"
        );
        assert!(
            !root.join("calls").exists(),
            "a 3.12 environment was upgraded"
        );

        // `--python-bin` naming another interpreter is the operator's choice.
        let dir = environment_built_by(&root, "/usr/bin", "3.13.5");
        let environment = PythonEnvironment {
            dir: dir.clone(),
            bin: PathBuf::from("/usr/bin/python3.13"),
            embedded: Some((3, 13)),
            bundled: Some(bundled),
        };
        environment.ensure().await.unwrap();
        assert!(
            !root.join("calls").exists(),
            "an explicit --python-bin was overridden"
        );
        assert_eq!(venv_home(&dir), Some(PathBuf::from("/usr/bin")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
