//! `shell` — R§3's `bash` tool, behind `coding`'s `may_use_shell` grant (TODO
//! §7a, Phase 6a).
//!
//! **A shell is every other grant at once.** It can edit files, run scripts and
//! reach the network, and it runs as the server's OS user, which can read
//! `feldspar.toml` and the database credentials in it. So the grant is off by
//! default, implies none of the other checkboxes, and is offered only to a run
//! whose caller is an **admin** — checked when the tools are listed and again
//! when one is called, for a transcript that carried the call from an earlier
//! caller.
//!
//! ## What a call is
//!
//! - One stateless `bash -c` in the scope's directory. Nothing carries over
//!   between calls, so the model `cd`s inside its command.
//! - A non-interactive environment ([`SHELL_ENV`]), with stdin closed.
//! - A timeout: [`CFG_SHELL_TIMEOUT`] by default, and the model may ask for more,
//!   up to [`CFG_SHELL_TIMEOUT_MAX`]. The command runs in a process group of its
//!   own, and the whole group is killed at the timeout, and after the command
//!   exits, so nothing it started in the background outlives the call.
//! - stdout and stderr through one pipe, so they interleave as a terminal shows
//!   them, truncated to their **head and tail** with the elided byte count.
//! - The exit code, always. A non-zero exit is a result, not an error.
//!
//! A command ending in `&` is refused with a pointer to `process_<slug>`
//! ([`super::process`]), which runs servers and watchers as named processes the
//! run owns.
//!
//! ## The sandbox
//!
//! [`CFG_SHELL_SANDBOX`] is `none` (the default: the command runs on the host,
//! and the admin-only rule is the only protection) or `container`: each command
//! runs in a fresh container of [`CFG_SHELL_IMAGE`], through `docker` or
//! `podman`, with only the scope's directory mounted (at the same path, so
//! diagnostics name the paths the file tools use), as the server's own user, and
//! with no network unless [`CFG_SHELL_NETWORK`] is on.
//!
//! ## The ledger still sees it
//!
//! Before each call the scope is snapshotted, and after it the snapshot is
//! compared ([`super::snapshot`]): every changed path enters the change ledger
//! with its pre-image, the model's read of it is forgotten (so the next edit
//! asks for a re-read), and the result lists the paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sc_agent::TraitContext;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use super::snapshot::{Snapshot, record_changes};
use super::state::CodingState;
use super::{CFG_MAY_USE_SHELL, may};
use crate::files::{FileScope, config_count, string_arg};
use crate::table::{arguments, config_str};

/// The default timeout of one shell command, in seconds.
pub const CFG_SHELL_TIMEOUT: &str = "shell_timeout";
/// The longest timeout the model may ask for, in seconds.
pub const CFG_SHELL_TIMEOUT_MAX: &str = "shell_timeout_max";
/// Where commands run: `none` (on the host) or `container`.
pub const CFG_SHELL_SANDBOX: &str = "shell_sandbox";
/// The container image commands run in, under the `container` sandbox.
pub const CFG_SHELL_IMAGE: &str = "shell_image";
/// The container runtime: `auto`, `docker` or `podman`.
pub const CFG_SHELL_RUNTIME: &str = "shell_runtime";
/// Whether a sandboxed command may reach the network.
pub const CFG_SHELL_NETWORK: &str = "shell_network";

/// [`CFG_SHELL_TIMEOUT`] when the admin sets none.
pub const DEFAULT_SHELL_TIMEOUT: u64 = 120;
/// [`CFG_SHELL_TIMEOUT_MAX`] when the admin sets none.
pub const DEFAULT_SHELL_TIMEOUT_MAX: u64 = 600;

/// The sandbox values.
pub const SANDBOX_NONE: &str = "none";
/// Each command in a container.
pub const SANDBOX_CONTAINER: &str = "container";

/// The runtime values.
pub const RUNTIME_AUTO: &str = "auto";

/// The environment every command gets on top of the server's: nothing waits
/// for a person at a terminal.
pub const SHELL_ENV: [(&str, &str); 5] = [
    ("CI", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("TERM", "dumb"),
];

/// The bytes of output kept from the start of a command's output.
pub const OUTPUT_HEAD_BYTES: usize = 8_000;
/// The bytes of output kept from its end, where a failure says why.
pub const OUTPUT_TAIL_BYTES: usize = 12_000;

/// How long the output pipe is drained after the command's process group is
/// gone. Only a process that left the group (`setsid`) can hold it open longer.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The command to run.
pub const ARG_COMMAND: &str = "command";
/// A timeout for this call, in seconds.
const ARG_TIMEOUT: &str = "timeout";

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("shell_{}", scope.slug())
}

/// The settings, in the order the form shows them: last, after every other
/// grant, because this one is all of them.
pub fn config_fields() -> Vec<FormField> {
    vec![
        FormField::new(CFG_MAY_USE_SHELL, BasicType::Bool)
            .label(
                "May use a shell (admins only). This is every other permission at once: \
                 it can change any file, run anything and read the server user's files",
            )
            .default_value(false),
        FormField::new(CFG_SHELL_TIMEOUT, BasicType::Int)
            .label("Shell command timeout (seconds)")
            .default_value(DEFAULT_SHELL_TIMEOUT as i64),
        FormField::new(CFG_SHELL_TIMEOUT_MAX, BasicType::Int)
            .label("Longest shell timeout the model may ask for (seconds)")
            .default_value(DEFAULT_SHELL_TIMEOUT_MAX as i64),
        FormField::new(CFG_SHELL_SANDBOX, BasicType::Text)
            .label(
                "Shell sandbox: `none` runs commands directly on the server as its own user; \
                 `container` runs each in a container with only this directory mounted",
            )
            .options([SANDBOX_NONE, SANDBOX_CONTAINER].map(str::to_owned))
            .default_value(SANDBOX_NONE),
        FormField::new(CFG_SHELL_IMAGE, BasicType::Text)
            .label("Container image (with bash), for the container sandbox"),
        FormField::new(CFG_SHELL_RUNTIME, BasicType::Text)
            .label("Container runtime")
            .options([RUNTIME_AUTO, "docker", "podman"].map(str::to_owned))
            .default_value(RUNTIME_AUTO),
        FormField::new(CFG_SHELL_NETWORK, BasicType::Bool)
            .label("Sandboxed commands may use the network")
            .default_value(false),
    ]
}

/// The two timeouts: the default and the ceiling.
pub fn timeouts(config: &Attrs) -> Result<(u64, u64)> {
    let default = config_count(config, CFG_SHELL_TIMEOUT, DEFAULT_SHELL_TIMEOUT)?;
    let max = config_count(config, CFG_SHELL_TIMEOUT_MAX, DEFAULT_SHELL_TIMEOUT_MAX)?;
    Ok((default, max.max(default)))
}

/// A container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    /// `docker`.
    Docker,
    /// `podman`.
    Podman,
}

impl Runtime {
    /// The program.
    pub fn program(self) -> &'static str {
        match self {
            Runtime::Docker => "docker",
            Runtime::Podman => "podman",
        }
    }

    /// Whether the program runs here.
    fn installed(self) -> bool {
        std::process::Command::new(self.program())
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The runtime a setting names, if it is installed: `auto` prefers
    /// `podman`, which runs rootless.
    pub fn detect(setting: &str) -> Result<Runtime> {
        let candidates: &[Runtime] = match setting {
            "" | RUNTIME_AUTO => &[Runtime::Podman, Runtime::Docker],
            "docker" => &[Runtime::Docker],
            "podman" => &[Runtime::Podman],
            other => {
                return Err(Error::invalid(format!(
                    "`{CFG_SHELL_RUNTIME}` must be auto, docker or podman, got `{other}`"
                )));
            }
        };
        candidates
            .iter()
            .copied()
            .find(|runtime| runtime.installed())
            .ok_or_else(|| {
                Error::config(format!(
                    "the container sandbox needs {} on this server's PATH, and it was not found",
                    candidates
                        .iter()
                        .map(|r| format!("`{}`", r.program()))
                        .collect::<Vec<_>>()
                        .join(" or ")
                ))
            })
    }
}

/// Where a command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sandbox {
    /// On the host, as the server's user.
    None,
    /// In a container.
    Container {
        /// The runtime.
        runtime: Runtime,
        /// The image.
        image: String,
        /// Whether the network is reachable.
        network: bool,
    },
}

/// The sandbox a configuration names, with its runtime found.
pub fn sandbox(config: &Attrs) -> Result<Sandbox> {
    match config_str(config, CFG_SHELL_SANDBOX).as_str() {
        "" | SANDBOX_NONE => Ok(Sandbox::None),
        SANDBOX_CONTAINER => {
            let image = config_str(config, CFG_SHELL_IMAGE);
            if image.is_empty() {
                return Err(Error::invalid(format!(
                    "the container sandbox needs `{CFG_SHELL_IMAGE}`: an image with bash in it"
                )));
            }
            Ok(Sandbox::Container {
                runtime: Runtime::detect(&config_str(config, CFG_SHELL_RUNTIME))?,
                image,
                network: may(config, CFG_SHELL_NETWORK),
            })
        }
        other => Err(Error::invalid(format!(
            "`{CFG_SHELL_SANDBOX}` must be none or container, got `{other}`"
        ))),
    }
}

/// The save-and-load check of the shell settings. The timeouts and the sandbox
/// value always; with the grant on, that the store has a local path, and for
/// the container sandbox that the runtime is installed and has the image.
pub async fn validate(catalog: &Catalog, scope: &FileScope, config: &Attrs) -> Result<()> {
    let default = config_count(config, CFG_SHELL_TIMEOUT, DEFAULT_SHELL_TIMEOUT)?;
    let max = config_count(config, CFG_SHELL_TIMEOUT_MAX, DEFAULT_SHELL_TIMEOUT_MAX)?;
    if max < default {
        return Err(Error::invalid(format!(
            "`{CFG_SHELL_TIMEOUT_MAX}` ({max}) is less than `{CFG_SHELL_TIMEOUT}` ({default})"
        )));
    }
    match config_str(config, CFG_SHELL_SANDBOX).as_str() {
        "" | SANDBOX_NONE | SANDBOX_CONTAINER => {}
        other => {
            return Err(Error::invalid(format!(
                "`{CFG_SHELL_SANDBOX}` must be none or container, got `{other}`"
            )));
        }
    }
    match config_str(config, CFG_SHELL_RUNTIME).as_str() {
        "" | RUNTIME_AUTO | "docker" | "podman" => {}
        other => {
            return Err(Error::invalid(format!(
                "`{CFG_SHELL_RUNTIME}` must be auto, docker or podman, got `{other}`"
            )));
        }
    }
    if !may(config, CFG_MAY_USE_SHELL) {
        return Ok(());
    }
    scope_dir(scope, catalog).await?;
    if let Sandbox::Container { runtime, image, .. } = sandbox(config)? {
        let found = tokio::process::Command::new(runtime.program())
            .args(["image", "inspect", &image])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !found {
            return Err(Error::invalid(format!(
                "`{}` has no image `{image}`; pull it on the server first (`{} pull {image}`)",
                runtime.program(),
                runtime.program()
            )));
        }
    }
    Ok(())
}

/// The scope's directory on disk.
pub async fn scope_dir(scope: &FileScope, catalog: &Catalog) -> Result<PathBuf> {
    let (store, _) = scope.connect(catalog).await?;
    store.local_path(&scope.resolve("")?)?.ok_or_else(|| {
        Error::config(format!(
            "{} has no local path, so no shell can run in it; a shell needs a local \
             file store",
            scope.label()
        ))
    })
}

/// The `shell` tool.
pub fn spec(scope: &FileScope, config: &Attrs) -> ToolSpec {
    let (default, max) = timeouts(config).unwrap_or((DEFAULT_SHELL_TIMEOUT, DEFAULT_SHELL_TIMEOUT));
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Run a bash command in {} and return its exit code and output. Each call is a \
             fresh non-interactive shell starting in that directory: `cd` inside the command \
             when needed. Times out after {default}s unless `{ARG_TIMEOUT}` asks for more \
             (at most {max}s). Do not end a command with `&`: start servers and watchers \
             with `{}`.",
            scope.label(),
            super::process::tool_name(scope)
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_COMMAND: {"type": "string", "description": "The bash command."},
                ARG_TIMEOUT: {"type": "integer", "minimum": 1, "description": "Seconds before it is killed."},
            },
            "required": [ARG_COMMAND],
            "additionalProperties": false,
        }),
    )
}

/// The short usage note `act` mode's prompt carries when the shell is offered.
pub fn prompt_note(scope: &FileScope, config: &Attrs) -> String {
    let (default, max) = timeouts(config).unwrap_or((DEFAULT_SHELL_TIMEOUT, DEFAULT_SHELL_TIMEOUT));
    format!(
        "Shell: `{shell}` runs one stateless `bash -c` in {label}; nothing (not even `cd`) \
         carries over between calls. Prefer the file tools for reading, searching and \
         editing, and `check` for verifying. Commands time out after {default}s (ask for up \
         to {max}s). Never background a command with `&`: use `{process}` to start, read \
         and stop servers and watchers. Files the shell changes must be read again before \
         they are edited.",
        shell = tool_name(scope),
        label = scope.label(),
        process = super::process::tool_name(scope),
    )
}

/// What makes two shell calls the same: the command, whitespace normalised.
pub fn fingerprint(args: &Json) -> Json {
    let command = args
        .get(ARG_COMMAND)
        .and_then(Json::as_str)
        .unwrap_or_default();
    json!({ ARG_COMMAND: normalise_command(command) })
}

/// A command with every run of whitespace folded to one space.
pub fn normalise_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a command puts something in the background at the end of a line:
/// the `&` that would leave a process running after the call returns.
pub fn backgrounds(command: &str) -> bool {
    command.lines().any(|line| {
        let line = line.trim_end();
        line.ends_with('&') && !line.ends_with("&&") && !line.ends_with("|&")
    })
}

/// Run one command.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let args = arguments(args, &[ARG_COMMAND, ARG_TIMEOUT])?;
    let command = string_arg(&args, ARG_COMMAND)?;
    if command.trim().is_empty() {
        return Err(Error::invalid(format!("`{ARG_COMMAND}` is empty")));
    }
    if backgrounds(&command) {
        return Err(Error::invalid(format!(
            "that command ends with `&`, which would leave a process running after the call. \
             Start long-running processes with `{}` (action `start`) instead, then read their \
             output with action `logs`.",
            super::process::tool_name(scope)
        )));
    }
    let (default, max) = timeouts(config)?;
    let (timeout, capped) = requested_timeout(&args, default, max)?;
    let dir = scope_dir(scope, ctx.catalog).await?;
    let sandbox = sandbox(config)?;

    let before = Snapshot::take_async(dir.clone(), true).await;
    let ran = run_command(&sandbox, &dir, &command, timeout).await?;
    let after = Snapshot::take_async(dir.clone(), false).await;

    let mut state = CodingState::load(ctx.state());
    let changed = record_changes(scope, &dir, &before, &after, &mut state).await;
    state.store(ctx.state());

    let mut out = match ran.exit {
        Exit::Code(code) => format!("exit code {code}"),
        Exit::Signal => "killed by a signal (no exit code)".to_owned(),
        Exit::TimedOut => format!("timed out after {timeout}s and was killed (no exit code)"),
    };
    out.push_str(&format!(" in {:.1}s", ran.elapsed.as_secs_f64()));
    if capped {
        out.push_str(&format!(
            " (the timeout asked for was more than the {max}s allowed)"
        ));
    }
    out.push('\n');
    match ran.output.trim_end() {
        "" => out.push_str("(no output)\n"),
        text => {
            out.push_str(text);
            out.push('\n');
        }
    }
    if !changed.is_empty() {
        out.push_str("\nfiles changed by this command (read them again before editing):\n");
        for line in &changed {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(Json::String(out.trim_end().to_owned()))
}

/// The call's timeout, clamped to the ceiling, and whether it was.
fn requested_timeout(args: &Map<String, Json>, default: u64, max: u64) -> Result<(u64, bool)> {
    match args.get(ARG_TIMEOUT) {
        None | Some(Json::Null) => Ok((default, false)),
        Some(Json::Number(n)) => match n.as_u64() {
            Some(0) | None => Err(Error::invalid(format!(
                "`{ARG_TIMEOUT}` should be a whole number of seconds, at least 1"
            ))),
            Some(n) if n > max => Ok((max, true)),
            Some(n) => Ok((n, false)),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_TIMEOUT}` should be a number of seconds, got {other}"
        ))),
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this code.
    Code(i32),
    /// A signal ended it.
    Signal,
    /// It was killed at the timeout.
    TimedOut,
}

/// One finished command.
#[derive(Debug, Clone)]
pub struct Ran {
    /// How it ended.
    pub exit: Exit,
    /// Its output, head and tail.
    pub output: String,
    /// How long it took.
    pub elapsed: Duration,
}

/// Run `command` under `sandbox` in `dir`, bounded by `timeout` seconds.
pub async fn run_command(
    sandbox: &Sandbox,
    dir: &Path,
    command: &str,
    timeout: u64,
) -> Result<Ran> {
    let started = Instant::now();
    let container = format!("feldspar-sh-{}", uuid::Uuid::new_v4().simple());
    let (program, args) = match sandbox {
        Sandbox::None => ("bash".to_owned(), vec!["-c".to_owned(), command.to_owned()]),
        Sandbox::Container {
            runtime,
            image,
            network,
        } => {
            let mut args = container_run_args(*runtime, dir, &container, *network);
            args.extend([
                image.clone(),
                "bash".to_owned(),
                "-c".to_owned(),
                command.to_owned(),
            ]);
            (runtime.program().to_owned(), args)
        }
    };
    let mut child = Spawned::spawn(
        &program,
        &args,
        dir,
        Capture::new(OUTPUT_HEAD_BYTES, OUTPUT_TAIL_BYTES),
    )?;
    // A call dropped mid-command — the run aborted, or its drive stopped —
    // takes the command's group with it, as a timeout does.
    let mut dropped = KillOnDrop(child.pgid);
    let status = tokio::time::timeout(Duration::from_secs(timeout), child.child.wait()).await;
    // Whatever the command left in its group goes with it.
    child.kill_group();
    dropped.0 = None;
    let exit = match status {
        Err(_) => {
            if let Sandbox::Container { runtime, .. } = sandbox {
                remove_container(*runtime, &container).await;
            }
            let _ = child.child.wait().await;
            Exit::TimedOut
        }
        Ok(Err(e)) => {
            return Err(Error::config(format!(
                "could not wait for `{program}`: {e}"
            )));
        }
        Ok(Ok(status)) => status.code().map_or(Exit::Signal, Exit::Code),
    };
    let output = child.finish(DRAIN_GRACE).await;
    Ok(Ran {
        exit,
        output,
        elapsed: started.elapsed(),
    })
}

/// Kills a process group when dropped, unless disarmed by clearing it.
struct KillOnDrop(Option<i32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            kill_group(pgid);
        }
    }
}

/// `run` arguments for a container over `dir`, up to (not including) the image.
pub fn container_run_args(runtime: Runtime, dir: &Path, name: &str, network: bool) -> Vec<String> {
    let dir = dir.to_string_lossy().into_owned();
    let mut args: Vec<String> = ["run", "--rm", "--pull=never", "--name", name]
        .map(str::to_owned)
        .to_vec();
    if !network {
        args.push("--network=none".to_owned());
    }
    // Files the command writes belong to the server's user, as the host's do.
    match runtime {
        Runtime::Docker => {
            let (uid, gid) = ids();
            args.push(format!("--user={uid}:{gid}"));
        }
        Runtime::Podman => args.push("--userns=keep-id".to_owned()),
    }
    args.extend([
        "-v".to_owned(),
        format!("{dir}:{dir}"),
        "-w".to_owned(),
        dir,
        "-e".to_owned(),
        "HOME=/tmp".to_owned(),
    ]);
    for (key, value) in SHELL_ENV {
        args.push("-e".to_owned());
        args.push(format!("{key}={value}"));
    }
    args
}

/// Remove a container, whatever it is doing. Best effort.
pub async fn remove_container(runtime: Runtime, name: &str) {
    let removed = tokio::process::Command::new(runtime.program())
        .args(["rm", "-f", name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = tokio::time::timeout(Duration::from_secs(30), removed).await;
}

/// The server's user and group ids.
#[cfg(unix)]
fn ids() -> (u32, u32) {
    // SAFETY: `getuid` and `getgid` cannot fail and touch no memory.
    unsafe { (libc::getuid(), libc::getgid()) }
}

#[cfg(not(unix))]
fn ids() -> (u32, u32) {
    (0, 0)
}

/// A process's output as it arrives: the first `head` bytes and the last
/// `tail`, and how many there were in all.
#[derive(Debug, Default)]
pub struct Capture {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
}

impl Capture {
    /// An empty capture keeping `head` and `tail` bytes.
    pub fn new(head: usize, tail: usize) -> Capture {
        Capture {
            head_cap: head,
            tail_cap: tail,
            ..Capture::default()
        }
    }

    /// Take in more output.
    pub fn push(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if self.head.len() < self.head_cap {
            let take = bytes.len().min(self.head_cap - self.head.len());
            self.head.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        self.tail.extend(bytes);
        while self.tail.len() > self.tail_cap {
            self.tail.pop_front();
        }
    }

    /// The kept output as text, saying how much of the middle was dropped.
    pub fn render(&self) -> String {
        let kept = (self.head.len() + self.tail.len()) as u64;
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        if self.total > kept {
            out.push_str(&format!(
                "\n[… {} bytes of output elided …]\n",
                self.total - kept
            ));
        }
        out.push_str(&String::from_utf8_lossy(&tail));
        out
    }
}

/// A child process in its own process group, its stdout and stderr read into
/// one [`Capture`] by a thread of its own.
pub struct Spawned {
    /// The process.
    pub child: tokio::process::Child,
    /// Its process group, which is its pid.
    pub pgid: Option<i32>,
    /// Its output so far.
    pub output: Arc<Mutex<Capture>>,
    /// Resolves when the pipe reaches end of file.
    drained: tokio::sync::oneshot::Receiver<()>,
}

impl Spawned {
    /// Start `program args…` in `dir`, with [`SHELL_ENV`], stdin closed, in a
    /// new process group, killed if the server dies.
    #[cfg(unix)]
    pub fn spawn(program: &str, args: &[String], dir: &Path, capture: Capture) -> Result<Spawned> {
        use std::io::Read;
        use std::os::fd::FromRawFd;

        let (mut reader, writer) = {
            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: `fds` has room for the two descriptors `pipe2` writes.
            #[cfg(target_os = "linux")]
            let failed = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0;
            // macOS has no `pipe2`: create the pipe, then mark both ends
            // close-on-exec. (Not atomic with respect to a concurrent fork.)
            // SAFETY: as above, and `fcntl` only touches the descriptors just
            // created.
            #[cfg(not(target_os = "linux"))]
            let failed = unsafe {
                libc::pipe(fds.as_mut_ptr()) != 0
                    || fds
                        .iter()
                        .any(|&fd| libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) != 0)
            };
            if failed {
                return Err(Error::config(format!(
                    "could not create a pipe: {}",
                    std::io::Error::last_os_error()
                )));
            }
            // SAFETY: both descriptors were just created and are owned by
            // nothing else.
            unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            }
        };
        let child = {
            let mut command = tokio::process::Command::new(program);
            command
                .args(args)
                .current_dir(dir)
                .envs(SHELL_ENV)
                .stdin(std::process::Stdio::null())
                .stdout(
                    writer
                        .try_clone()
                        .map_err(|e| Error::config(format!("could not duplicate a pipe: {e}")))?,
                )
                .stderr(writer)
                .process_group(0)
                .kill_on_drop(true);
            #[cfg(target_os = "linux")]
            // SAFETY: `prctl` is async-signal-safe, which is all a `pre_exec`
            // closure may call.
            unsafe {
                command.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
            // The command, and with it this process's copies of the pipe's
            // write end, is dropped here: the reader sees end of file once the
            // child's copies are gone.
            command.spawn().map_err(|e| {
                Error::config(format!(
                    "could not run `{program}` in {}: {e}",
                    dir.display()
                ))
            })?
        };
        let pgid = child.id().map(|id| id as i32);
        let output = Arc::new(Mutex::new(capture));
        let (done, drained) = tokio::sync::oneshot::channel();
        let sink = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => lock(&sink).push(&buf[..n]),
                }
            }
            let _ = done.send(());
        });
        Ok(Spawned {
            child,
            pgid,
            output,
            drained,
        })
    }

    #[cfg(not(unix))]
    pub fn spawn(program: &str, _: &[String], _: &Path, _: Capture) -> Result<Spawned> {
        Err(Error::config(format!(
            "`{program}` cannot be run: the coding shell needs a Unix server"
        )))
    }

    /// Kill the process group, whatever is left in it.
    pub fn kill_group(&self) {
        if let Some(pgid) = self.pgid {
            kill_group(pgid);
        }
    }

    /// The output, once the pipe is drained or `grace` has passed.
    pub async fn finish(self, grace: Duration) -> String {
        let _ = tokio::time::timeout(grace, self.drained).await;
        lock(&self.output).render()
    }
}

/// Send `SIGKILL` to a process group. A group that is already gone is fine.
#[cfg(unix)]
pub fn kill_group(pgid: i32) {
    if pgid > 0 {
        // SAFETY: `killpg` takes plain integers and touches no memory.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub fn kill_group(_: i32) {}

/// A mutex's guard, through a poisoning: the data is output bytes, which a
/// panicked writer cannot leave inconsistent.
pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_ampersand_is_backgrounding_and_a_conjunction_is_not() {
        assert!(backgrounds("npm run dev &"));
        assert!(backgrounds("npm run dev &  \nsleep 2"));
        assert!(!backgrounds("npm test && echo ok"));
        assert!(!backgrounds("make 2>&1"));
        assert!(!backgrounds("npm test &&\n echo ok"));
        assert!(!backgrounds("ls |&"));
    }

    #[test]
    fn a_fingerprint_ignores_whitespace() {
        assert_eq!(
            fingerprint(&json!({"command": "ls   -la\n src", "timeout": 5})),
            fingerprint(&json!({"command": " ls -la src "}))
        );
    }

    #[test]
    fn a_long_output_keeps_head_and_tail_and_counts_the_rest() {
        let mut capture = Capture::new(4, 6);
        capture.push(b"abcdef");
        capture.push(b"ghijklmnop");
        assert_eq!(
            capture.render(),
            "abcd\n[… 6 bytes of output elided …]\nklmnop"
        );
        // Short output is kept whole.
        let mut short = Capture::new(4, 6);
        short.push(b"abcdefgh");
        assert_eq!(short.render(), "abcdefgh");
    }

    #[test]
    fn the_timeout_asked_for_is_capped_at_the_ceiling() {
        let args = |v: Json| arguments(&v, &[ARG_COMMAND, ARG_TIMEOUT]).unwrap();
        assert_eq!(
            requested_timeout(&args(json!({})), 120, 600).unwrap(),
            (120, false)
        );
        assert_eq!(
            requested_timeout(&args(json!({"timeout": 300})), 120, 600).unwrap(),
            (300, false)
        );
        assert_eq!(
            requested_timeout(&args(json!({"timeout": 9000})), 120, 600).unwrap(),
            (600, true)
        );
        assert!(requested_timeout(&args(json!({"timeout": 0})), 120, 600).is_err());
    }

    #[test]
    fn a_container_mounts_only_the_scope_and_has_no_network_by_default() {
        let args = container_run_args(Runtime::Docker, Path::new("/srv/app"), "c1", false);
        assert!(args.contains(&"--network=none".to_owned()), "{args:?}");
        assert!(args.contains(&"/srv/app:/srv/app".to_owned()), "{args:?}");
        assert_eq!(args.iter().filter(|a| *a == "-v").count(), 1);
        let open = container_run_args(Runtime::Podman, Path::new("/srv/app"), "c1", true);
        assert!(!open.iter().any(|a| a.starts_with("--network")), "{open:?}");
        assert!(open.contains(&"--userns=keep-id".to_owned()), "{open:?}");
    }
}
