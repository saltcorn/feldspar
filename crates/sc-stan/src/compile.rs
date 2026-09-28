//! The compile cache (TODO §13): a Stan program compiled once per node, and
//! found again by what it is rather than where it came from.
//!
//! **The key** is the SHA-256 of the program's files (each one's store path
//! and hash, the main file first), the CmdStan version and
//! [`COMPILE_OPTIONS`]. Editing a program, including a file it `#include`s,
//! is a new key; so is upgrading CmdStan. Two models running the same program
//! share one executable.
//!
//! **The layout**, under the cache directory:
//!
//! ```text
//! <key>/model           the executable
//! <key>/src/…           the program as it was compiled, at its store paths
//! <key>/manifest.json   the main path, the hashes, the CmdStan version, the source directory
//! <key>/compile.log     make's output
//! ```
//!
//! A compile happens in `<key>.building-<pid>-<n>/` and is renamed into place
//! when it has finished, so a half-built directory is never taken for a
//! cached one, and one left by a server that died is simply never read.
//!
//! **One compile at a time per node.** A Stan compile is a C++ compile — a
//! minute of CPU and 1–2 GB of memory — and two at once is how a small server
//! runs out of memory. A second fit of the same program waits for the first
//! one's compile and then finds it cached.
//!
//! **Nothing admin-supplied reaches the compiler.** `make` is given the target
//! and `STANCFLAGS=--include-paths=<src>`, both ours; there is no
//! `--allow-undefined` (a program cannot bring C++ of its own) and no
//! `CXXFLAGS`. The environment is the scrubbed one of every child (§13).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sc_error::{Context, Error, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::cmdstan::CmdStan;
use crate::process::{self, KillGroupOnDrop, Stopped, Tail, Watch};
use crate::program::Program;

/// The fixed options every program is compiled with, folded into the key so a
/// change to them is a recompile. There are none beyond CmdStan's defaults:
/// no threads (one process per chain needs none), no `--allow-undefined`.
pub const COMPILE_OPTIONS: &str = "stanc: --include-paths=<src>; make: defaults";

/// The file the executable is at, inside a cache entry.
const EXE: &str = "model";

/// Taken for the length of every compile on this node.
static COMPILING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A compiled program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    /// The cache key.
    pub key: String,
    /// The executable.
    pub exe: PathBuf,
    /// The directory the program was compiled from, which the model's own
    /// error messages name — mapped back to store paths in a failure's
    /// sentence.
    pub src: PathBuf,
    /// Whether it was already in the cache.
    pub cached: bool,
}

/// Where compiled programs are kept, and the `make` that builds them.
#[derive(Debug, Clone)]
pub struct CompileCache {
    dir: PathBuf,
    make: PathBuf,
}

impl CompileCache {
    /// A cache in `dir`, compiling with the `make` at `make`.
    pub fn new(dir: impl Into<PathBuf>, make: impl Into<PathBuf>) -> CompileCache {
        CompileCache {
            dir: dir.into(),
            make: make.into(),
        }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Remove the half-finished compiles of server processes that are no
    /// longer running — boot's cleanup (§13). Answers how many were removed.
    pub fn clean_stale(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pid) = name
                .split_once(".building-")
                .and_then(|(_, rest)| rest.split('-').next())
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            if pid != std::process::id()
                && !crate::run_dir::alive(pid)
                && std::fs::remove_dir_all(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
        removed
    }

    /// The key `program` is compiled under by `cmdstan`.
    pub fn key(program: &Program, cmdstan: &CmdStan) -> String {
        let mut hash = Sha256::new();
        hash.update(b"feldspar stan compile 1\n");
        hash.update(format!("cmdstan {}\n", cmdstan.version).as_bytes());
        hash.update(format!("options {COMPILE_OPTIONS}\n").as_bytes());
        for file in &program.files {
            hash.update(format!("file {} {}\n", file.path, file.sha256).as_bytes());
        }
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The cached executable for `key`, if it is there.
    pub fn lookup(&self, key: &str) -> Option<Compiled> {
        let entry = self.dir.join(key);
        let exe = entry.join(EXE);
        if !exe.is_file() {
            return None;
        }
        let src = std::fs::read(entry.join("manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|m| m.get("src").and_then(|s| s.as_str()).map(PathBuf::from))
            .unwrap_or_else(|| entry.join("src"));
        Some(Compiled {
            key: key.to_owned(),
            exe,
            src,
            cached: true,
        })
    }

    /// The executable for `program`: from the cache, or compiled now (calling
    /// `compiling` first, so the fit can say so) — one compile at a time on
    /// this node, stopped when `watch` says so.
    pub async fn compile(
        &self,
        cmdstan: &CmdStan,
        program: &Program,
        watch: &Watch<'_>,
        compiling: &(dyn Fn() + Sync),
    ) -> Result<Compiled> {
        let key = Self::key(program, cmdstan);
        if let Some(found) = self.lookup(&key) {
            return Ok(found);
        }
        compiling();
        let _one_at_a_time = tokio::select! {
            guard = COMPILING.lock() => guard,
            why = watch.until_stopped() => return Err(stop_error(why, watch)),
        };
        // Somebody else may have compiled it while this fit waited.
        if let Some(found) = self.lookup(&key) {
            return Ok(found);
        }
        self.build(cmdstan, program, &key, watch).await
    }

    async fn build(
        &self,
        cmdstan: &CmdStan,
        program: &Program,
        key: &str,
        watch: &Watch<'_>,
    ) -> Result<Compiled> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating the Stan compile cache {}", self.dir.display()))?;
        let dir = self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone());
        if dir.to_string_lossy().chars().any(char::is_whitespace) {
            return Err(Error::config(format!(
                "the Stan compile cache {} has a space in its path, which CmdStan's makefiles \
                 cannot handle: choose another directory",
                dir.display()
            )));
        }
        let building = dir.join(format!(
            "{key}.building-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _cleanup = RemoveOnDrop(building.clone());
        let src = building.join("src");
        std::fs::create_dir_all(&src).with_context(|| format!("creating {}", src.display()))?;
        let main = program.layout(&src)?;
        let Some(stem) = main.strip_suffix(".stan") else {
            return Err(Error::invalid(format!(
                "the program `{main}` must be a `.stan` file to be compiled"
            )));
        };
        let target = src.join(stem);

        let mut command = process::command(&self.make);
        command
            .arg(format!("STANCFLAGS=--include-paths={}", src.display()))
            .arg(&target)
            .current_dir(&cmdstan.dir);
        let mut child = command.spawn().with_context(|| {
            format!(
                "running `{}` in the CmdStan directory {}",
                self.make.display(),
                cmdstan.dir.display()
            )
        })?;
        let group = KillGroupOnDrop(child.id());
        let mut lines = process::output_lines(&mut child);
        let mut log = String::new();
        let mut tail = Tail::default();
        loop {
            tokio::select! {
                line = lines.recv() => match line {
                    Some(line) => {
                        log.push_str(&line);
                        log.push('\n');
                        tail.push(&line);
                    }
                    None => break,
                },
                why = watch.until_stopped() => {
                    group.kill();
                    return Err(stop_error(why, watch));
                }
            }
        }
        let status = tokio::select! {
            status = child.wait() => status.context("waiting for make")?,
            why = watch.until_stopped() => {
                group.kill();
                return Err(stop_error(why, watch));
            }
        };
        std::fs::write(building.join("compile.log"), &log)
            .with_context(|| format!("writing {}", building.join("compile.log").display()))?;
        if !status.success() || !target.is_file() {
            return Err(compile_failure(&main, &status, &log, &tail, &src));
        }

        std::fs::rename(&target, building.join(EXE))
            .with_context(|| format!("moving the compiled {main} into the cache"))?;
        let manifest = json!({
            "main": main,
            "files": program.hashes(),
            "cmdstan": cmdstan.version.to_string(),
            "options": COMPILE_OPTIONS,
            "src": src.display().to_string(),
        });
        std::fs::write(
            building.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
        )
        .context("writing the compile cache manifest")?;
        let entry = dir.join(key);
        if std::fs::rename(&building, &entry).is_err() && !entry.join(EXE).is_file() {
            return Err(Error::msg(format!(
                "the compiled program could not be moved into the cache at {}",
                entry.display()
            )));
        }
        // The manifest names the source directory the model was compiled
        // from, which is `building`'s and not the entry's: that is the path
        // CmdStan's messages carry.
        Ok(Compiled {
            key: key.to_owned(),
            exe: entry.join(EXE),
            src,
            cached: false,
        })
    }
}

/// What a stopped fit says.
pub(crate) fn stop_error(why: Stopped, watch: &Watch<'_>) -> Error {
    match why {
        Stopped::Cancelled => Error::msg(format!(
            "the fit was cancelled after {}",
            process::duration_words(watch.elapsed())
        )),
        Stopped::TimedOut => Error::msg(format!(
            "the fit was stopped after {}, because it ran longer than its \
             `max_runtime_minutes` ({})",
            process::duration_words(watch.elapsed()),
            watch.limit().as_secs() / 60
        )),
    }
}

/// `text` with the compile's source directory taken out of every path, so
/// what is left is the store path.
pub(crate) fn map_src(text: &str, src: &Path) -> String {
    let mut text = text.to_owned();
    for root in [src.to_path_buf(), src.canonicalize().unwrap_or_default()] {
        let root = root.display().to_string();
        if !root.is_empty() {
            text = text.replace(&format!("{root}/"), "");
        }
    }
    text
}

/// The sentence for a compile that failed: `stanc`'s own error, quoted with
/// store paths, when it was `stanc` that refused; the end of `make`'s output
/// otherwise.
fn compile_failure(
    main: &str,
    status: &std::process::ExitStatus,
    log: &str,
    tail: &Tail,
    src: &Path,
) -> Error {
    let log = map_src(log, src);
    let lines: Vec<&str> = log.lines().collect();
    if let Some(at) = lines
        .iter()
        .position(|l| l.contains("Syntax error") || l.contains("Semantic error"))
    {
        let error: Vec<&str> = lines[at..]
            .iter()
            .copied()
            .filter(|l| !l.starts_with("make:") && !l.starts_with("make["))
            .collect();
        return Error::invalid(format!(
            "stanc refused the program `{main}`:\n{}",
            error.join("\n").trim_end()
        ));
    }
    Error::msg(format!(
        "compiling the program `{main}` failed (make {status}); the last lines of its output:\n{}",
        map_src(&tail.text(), src)
    ))
}

/// Removes its directory when dropped — a compile that failed or was stopped
/// leaves nothing behind, and one that succeeded has been renamed away.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmdstan::{Source, Version};
    use crate::program::ProgramFile;

    fn cmdstan(version: &str) -> CmdStan {
        CmdStan {
            dir: PathBuf::from("/opt/cmdstan"),
            version: version.parse::<Version>().expect("version"),
            source: Source::Flag,
        }
    }

    fn program(body: &str, lib: &str) -> Program {
        Program::from_files(
            "store",
            vec![
                ProgramFile::new("m.stan", body),
                ProgramFile::new("lib.stan", lib),
            ],
        )
        .expect("program")
    }

    #[test]
    fn the_key_changes_with_any_file_and_with_cmdstan_and_nothing_else() {
        let a = CompileCache::key(&program("x", "y"), &cmdstan("2.36.0"));
        assert_eq!(a.len(), 64);
        assert_eq!(a, CompileCache::key(&program("x", "y"), &cmdstan("2.36.0")));
        assert_ne!(a, CompileCache::key(&program("x", "z"), &cmdstan("2.36.0")));
        assert_ne!(a, CompileCache::key(&program("w", "y"), &cmdstan("2.36.0")));
        assert_ne!(a, CompileCache::key(&program("x", "y"), &cmdstan("2.37.0")));
    }

    #[cfg(unix)]
    #[test]
    fn a_stanc_error_is_quoted_with_store_paths_and_without_makes_lines() {
        let src = Path::new("/cache/abc.building-1-0/src");
        let log = "--- Translating Stan model to C++ code ---\n\
                   bin/stanc --include-paths=/cache/abc.building-1-0/src --o=x.hpp /cache/abc.building-1-0/src/m.stan\n\
                   Semantic error in '/cache/abc.building-1-0/src/m.stan', line 3, column 2 to column 9:\n\
                   Identifier 'sigmaa' not in scope.\n\
                   make: *** [make/program:50: /cache/abc.building-1-0/src/m.hpp] Error 1\n";
        let mut tail = Tail::default();
        for l in log.lines() {
            tail.push(l);
        }
        let status = exit_status(2);
        let e = compile_failure("m.stan", &status, log, &tail, src);
        assert_eq!(
            e.to_string(),
            "invalid: stanc refused the program `m.stan`:\n\
             Semantic error in 'm.stan', line 3, column 2 to column 9:\n\
             Identifier 'sigmaa' not in scope."
        );
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }
}
