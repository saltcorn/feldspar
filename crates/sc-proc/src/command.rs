//! [`HostCommand`], the [`Child`] it starts, and [`ProcError`].

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::authority::{OperatorCli, SpawnAuthority};
use crate::words::{EnvName, Operand, Program, Timeout};

/// Why a process did not run to an exit status.
#[derive(Debug)]
pub enum ProcError {
    /// It could not be started (the program is not there, or not executable).
    Spawn {
        /// The program.
        program: String,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// It was still running at its [`Timeout`], and its process group was
    /// killed.
    TimedOut {
        /// The program.
        program: String,
        /// The limit it ran into.
        timeout: Timeout,
    },
    /// Waiting for it, or reading its output, failed.
    Io {
        /// The program.
        program: String,
        /// What the operating system said.
        source: std::io::Error,
    },
}

impl ProcError {
    /// Whether the process ran into its timeout.
    pub fn is_timeout(&self) -> bool {
        matches!(self, ProcError::TimedOut { .. })
    }

    /// Whether the program was not found.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ProcError::Spawn { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound)
    }
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcError::Spawn { program, source } => {
                write!(f, "could not run `{program}`: {source}")
            }
            ProcError::TimedOut { program, timeout } => {
                write!(
                    f,
                    "`{program}` was still running after {timeout}, and was stopped"
                )
            }
            ProcError::Io { program, source } => write!(f, "running `{program}`: {source}"),
        }
    }
}

impl std::error::Error for ProcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProcError::Spawn { source, .. } | ProcError::Io { source, .. } => Some(source),
            ProcError::TimedOut { .. } => None,
        }
    }
}

impl From<ProcError> for sc_error::Error {
    fn from(e: ProcError) -> sc_error::Error {
        sc_error::Error::config(e.to_string())
    }
}

/// How a child's stdin is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stdin {
    Null,
    Piped,
    Inherit,
}

/// A program to run on the host, configured as the crate docs say: cleared
/// environment, mandatory timeout, operands that cannot be options, a process
/// group of its own, and the authority it runs on.
///
/// Built with `&mut self` methods, then run by one of the consuming ones:
/// [`output`](Self::output), [`output_bounded`](Self::output_bounded),
/// [`status`](Self::status), [`spawn`](Self::spawn), or their `_blocking`
/// forms for synchronous code.
pub struct HostCommand {
    program: Program,
    args: Vec<OsString>,
    separated: bool,
    env: Vec<(OsString, OsString)>,
    /// The names in `env` whose values are credentials, which `Debug` leaves
    /// out.
    secret: Vec<OsString>,
    cwd: Option<PathBuf>,
    stdin: Stdin,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    timeout: Timeout,
    operator: bool,
    leave_descendants: bool,
    audit: String,
}

impl std::fmt::Debug for HostCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<(&OsStr, &OsStr)> = self
            .env
            .iter()
            .map(|(name, value)| match self.secret.contains(name) {
                true => (name.as_os_str(), OsStr::new("<redacted>")),
                false => (name.as_os_str(), value.as_os_str()),
            })
            .collect();
        f.debug_struct("HostCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("separated", &self.separated)
            .field("env", &env)
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("timeout", &self.timeout)
            .field("operator", &self.operator)
            .field("leave_descendants", &self.leave_descendants)
            .field("audit", &self.audit)
            .finish()
    }
}

impl HostCommand {
    /// `program`, run on `who`'s authority, stopped after `timeout`.
    pub fn new(
        program: impl Into<Program>,
        who: &impl SpawnAuthority,
        timeout: Timeout,
    ) -> HostCommand {
        HostCommand {
            program: program.into(),
            args: Vec::new(),
            separated: false,
            env: Vec::new(),
            secret: Vec::new(),
            cwd: None,
            stdin: Stdin::Null,
            stdout: None,
            stderr: None,
            timeout,
            operator: false,
            leave_descendants: false,
            audit: who.audit(),
        }
    }

    /// An argument the program spells itself: a sub-command or a flag.
    pub fn arg(&mut self, arg: &'static str) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Several of [`arg`](Self::arg).
    pub fn args<I: IntoIterator<Item = &'static str>>(&mut self, args: I) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// `flag` and then `value`, as two arguments: the value is what the flag
    /// consumes, so it is read as a value whatever it starts with (a commit
    /// message after `-m`, a script after `-c`).
    pub fn flag_value(&mut self, flag: &'static str, value: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(flag.into());
        self.args.push(value.as_ref().to_owned());
        self
    }

    /// `flag=value`, as one argument (`--prefix=web/`, `--user-data-dir=…`).
    pub fn flag_eq(&mut self, flag: &'static str, value: impl AsRef<OsStr>) -> &mut Self {
        let mut arg = OsString::from(flag);
        arg.push("=");
        arg.push(value.as_ref());
        self.args.push(arg);
        self
    }

    /// An operand, after a `--` emitted before the first one.
    pub fn operand(&mut self, operand: Operand) -> &mut Self {
        if !self.separated {
            self.args.push("--".into());
            self.separated = true;
        }
        self.args.push(operand.as_os_str().to_owned());
        self
    }

    /// Several of [`operand`](Self::operand); the `--` is emitted even when
    /// there are none, which is what `git add -- ` with no paths needs.
    pub fn operands<I: IntoIterator<Item = Operand>>(&mut self, operands: I) -> &mut Self {
        if !self.separated {
            self.args.push("--".into());
            self.separated = true;
        }
        for operand in operands {
            self.operand(operand);
        }
        self
    }

    /// An operand with no `--` before it, for a program where `--` means
    /// something else: `npm run <script> --` passes what follows to the script,
    /// `git checkout -- <x>` reads `x` as a path, and `make` takes `VAR=value`.
    /// [`Operand`] already refuses a leading `-`.
    pub fn positional(&mut self, operand: Operand) -> &mut Self {
        self.args.push(operand.as_os_str().to_owned());
        self
    }

    /// Arguments an administrator or operator configured as a whole command
    /// line, flags included (a build command's arguments, a target's, a setup
    /// step's). Not for anything a request, a model or a row supplied.
    pub fn configured_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Give the child `name=value`, on top of [`BASE_ENV`](crate::BASE_ENV).
    pub fn env(&mut self, name: EnvName, value: impl AsRef<OsStr>) -> &mut Self {
        self.env
            .push((name.as_str().into(), value.as_ref().to_owned()));
        self
    }

    /// [`env`](Self::env) for a credential: given to the child the same way,
    /// and never in `Debug`.
    pub fn secret_env(&mut self, name: EnvName, value: impl AsRef<OsStr>) -> &mut Self {
        self.secret.push(name.as_str().into());
        self.env(name, value)
    }

    /// Give the child the server's own value of `name`, when it has one.
    pub fn inherit(&mut self, name: EnvName) -> &mut Self {
        if let Some(value) = std::env::var_os(name.as_str()) {
            self.env.push((name.as_str().into(), value));
        }
        self
    }

    /// Give the child every variable of the server's whose name starts with
    /// `prefix` (`STAN_`, `npm_config_`), except those [`EnvName`] refuses.
    pub fn inherit_prefix(&mut self, prefix: &'static str) -> &mut Self {
        self.env.extend(crate::env::with_prefix(prefix));
        self
    }

    /// Run in `dir`.
    pub fn cwd(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }

    /// Give the child a pipe for stdin, instead of `/dev/null`.
    pub fn stdin_piped(&mut self) -> &mut Self {
        self.stdin = Stdin::Piped;
        self
    }

    /// Where the child's stdout goes. [`output`](Self::output) pipes it when
    /// this is not set; the others leave it as the server's.
    pub fn stdout(&mut self, stdout: impl Into<Stdio>) -> &mut Self {
        self.stdout = Some(stdout.into());
        self
    }

    /// Where the child's stderr goes, as [`stdout`](Self::stdout).
    pub fn stderr(&mut self, stderr: impl Into<Stdio>) -> &mut Self {
        self.stderr = Some(stderr.into());
        self
    }

    /// Run as a step of the operator's own terminal session: the operator's
    /// whole environment, their stdin, and their process group, so `sudo` can
    /// ask for a password and Ctrl-C reaches the child. Only the CLI can.
    pub fn operator_terminal(&mut self, _: &OperatorCli) -> &mut Self {
        self.operator = true;
        self.stdin = Stdin::Inherit;
        self
    }

    /// Leave what the program started running once it has exited normally
    /// (`docker run -d`, whose container outlives the CLI): its process group
    /// is still killed at the timeout and when the [`Child`] is dropped before
    /// the program exits.
    pub fn leave_descendants(&mut self) -> &mut Self {
        self.leave_descendants = true;
        self
    }

    /// The program and its arguments, for a message. Arguments can hold what
    /// a user typed; not for a log line.
    pub fn command_line(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The program, for a message.
    fn label(&self) -> String {
        self.program.to_string()
    }

    /// The configured `std` command; stdout and stderr are taken.
    fn std_command(&mut self) -> std::process::Command {
        #[allow(
            clippy::disallowed_methods,
            reason = "sc-proc is the one place a process is started; this is it"
        )]
        let mut command = std::process::Command::new(self.program.as_os_str());
        command.args(&self.args);
        if !self.operator {
            command.env_clear().envs(crate::env::base());
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        command.stdin(match self.stdin {
            Stdin::Null => Stdio::null(),
            Stdin::Piped => Stdio::piped(),
            Stdin::Inherit => Stdio::inherit(),
        });
        if let Some(stdout) = self.stdout.take() {
            command.stdout(stdout);
        }
        if let Some(stderr) = self.stderr.take() {
            command.stderr(stderr);
        }
        #[cfg(unix)]
        if !self.operator {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        die_with_server(&mut command);
        command
    }

    fn log(&self) {
        let dir = self
            .cwd
            .as_deref()
            .map(|d| format!(" in {}", d.display()))
            .unwrap_or_default();
        sc_log::log_verbose!(
            "spawn `{}`{dir} on {}, {}",
            self.program,
            self.audit,
            self.timeout
        );
    }

    /// Start the program. The [`Child`] kills its process group when dropped.
    pub fn spawn(mut self) -> Result<Child, ProcError> {
        self.log();
        let mut command = tokio::process::Command::from(self.std_command());
        command.kill_on_drop(true);
        let child = command.spawn().map_err(|source| ProcError::Spawn {
            program: self.label(),
            source,
        })?;
        let pgid = match self.operator {
            true => None,
            false => child.id().and_then(|id| i32::try_from(id).ok()),
        };
        Ok(Child {
            inner: child,
            pgid,
            deadline: self.timeout.duration().map(|d| Instant::now() + d),
            program: self.label(),
            timeout: self.timeout,
            leave_descendants: self.leave_descendants,
            exited: false,
        })
    }

    /// Run to the end, with stdout and stderr captured (unless set
    /// otherwise). Running into the timeout is [`ProcError::TimedOut`].
    pub async fn output(self) -> Result<Output, ProcError> {
        let (program, timeout) = (self.label(), self.timeout);
        self.output_bounded()
            .await?
            .ok_or(ProcError::TimedOut { program, timeout })
    }

    /// [`output`](Self::output), with `None` for a process that ran into its
    /// timeout and was killed.
    pub async fn output_bounded(mut self) -> Result<Option<Output>, ProcError> {
        if self.stdout.is_none() {
            self.stdout = Some(Stdio::piped());
        }
        if self.stderr.is_none() {
            self.stderr = Some(Stdio::piped());
        }
        let mut child = self.spawn()?;
        let mut stdout = child.inner.stdout.take();
        let mut stderr = child.inner.stderr.take();
        let pgid = child.pgid;
        let leave = child.leave_descendants;
        let deadline = child.deadline;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let run = async {
            let read_out = async {
                match stdout.as_mut() {
                    Some(s) => s.read_to_end(&mut out).await.map(drop),
                    None => Ok(()),
                }
            };
            let read_err = async {
                match stderr.as_mut() {
                    Some(s) => s.read_to_end(&mut err).await.map(drop),
                    None => Ok(()),
                }
            };
            let exited = async {
                let status = child.inner.wait().await;
                // What the program left running would hold the pipes open, and
                // the reads would wait for it.
                if !leave && let Some(pgid) = pgid {
                    kill_group(pgid);
                }
                status
            };
            let (a, b, status) = tokio::join!(read_out, read_err, exited);
            a?;
            b?;
            status
        };
        let finished = match deadline {
            None => Some(run.await),
            Some(at) => tokio::time::timeout_at(tokio::time::Instant::from_std(at), run)
                .await
                .ok(),
        };
        match finished {
            Some(Ok(status)) => {
                child.exited = true;
                Ok(Some(Output {
                    status,
                    stdout: out,
                    stderr: err,
                }))
            }
            Some(Err(source)) => Err(ProcError::Io {
                program: child.program.clone(),
                source,
            }),
            None => {
                child.kill().await;
                Ok(None)
            }
        }
    }

    /// Run to the end and return its exit status. Running into the timeout is
    /// [`ProcError::TimedOut`].
    pub async fn status(self) -> Result<ExitStatus, ProcError> {
        let mut child = self.spawn()?;
        child.wait().await
    }

    /// [`output`](Self::output) for synchronous code, on this thread.
    pub fn output_blocking(mut self) -> Result<Output, ProcError> {
        if self.stdout.is_none() {
            self.stdout = Some(Stdio::piped());
        }
        if self.stderr.is_none() {
            self.stderr = Some(Stdio::piped());
        }
        let (status, stdout, stderr) = self.run_blocking()?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// [`status`](Self::status) for synchronous code, on this thread.
    pub fn status_blocking(self) -> Result<ExitStatus, ProcError> {
        Ok(self.run_blocking()?.0)
    }

    fn run_blocking(mut self) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), ProcError> {
        self.log();
        let program = self.label();
        let mut child = self
            .std_command()
            .spawn()
            .map_err(|source| ProcError::Spawn {
                program: program.clone(),
                source,
            })?;
        let pgid = match self.operator {
            true => None,
            false => child.id().try_into().ok(),
        };
        let mut group = GroupGuard(pgid);
        let reader = |pipe: Option<Box<dyn Read + Send>>| {
            pipe.map(|mut pipe| {
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = pipe.read_to_end(&mut bytes);
                    bytes
                })
            })
        };
        let out = reader(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let err = reader(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let deadline = self.timeout.duration().map(|d| Instant::now() + d);
        let mut pause = Duration::from_millis(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(source) => return Err(ProcError::Io { program, source }),
            }
            if deadline.is_some_and(|at| Instant::now() >= at) {
                group.kill();
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcError::TimedOut {
                    program,
                    timeout: self.timeout,
                });
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        };
        if self.leave_descendants {
            group.0 = None;
        }
        // As in `output_bounded`: what it left running would hold the pipes.
        drop(group);
        let join = |t: Option<std::thread::JoinHandle<Vec<u8>>>| {
            t.and_then(|t| t.join().ok()).unwrap_or_default()
        };
        Ok((status, join(out), join(err)))
    }

    /// Start a process meant to **outlive this one** — the browser's watchdog,
    /// which ends the browser after a server that was killed. In a process
    /// group of its own, but not killed when the server dies, nor when the
    /// handle is dropped; the timeout is not enforced.
    pub fn spawn_detached(mut self) -> Result<std::process::Child, ProcError> {
        self.log();
        let program = self.label();
        let mut command = self.std_command();
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `prctl` is async-signal-safe, which is all a `pre_exec`
            // closure may call. Runs after `die_with_server`'s, undoing it.
            unsafe {
                command.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, 0);
                    Ok(())
                });
            }
        }
        command
            .spawn()
            .map_err(|source| ProcError::Spawn { program, source })
    }
}

/// On Linux, the child is killed when the thread that started it ends — the
/// server dying, in practice: `PR_SET_PDEATHSIG`.
fn die_with_server(command: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `prctl` is async-signal-safe, which is all a `pre_exec`
        // closure may call.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// Send `SIGKILL` to the process group `pgid`. A group that is gone is fine.
pub(crate) fn kill_group(pgid: i32) {
    #[cfg(unix)]
    if pgid > 0 {
        // SAFETY: `killpg` takes plain integers and touches no memory.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Kills a process group when dropped.
struct GroupGuard(Option<i32>);

impl GroupGuard {
    fn kill(&self) {
        if let Some(pgid) = self.0 {
            kill_group(pgid);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A running process [`HostCommand::spawn`] started. Dropping it kills its
/// whole process group.
#[derive(Debug)]
pub struct Child {
    inner: tokio::process::Child,
    pgid: Option<i32>,
    deadline: Option<Instant>,
    program: String,
    timeout: Timeout,
    leave_descendants: bool,
    exited: bool,
}

impl Child {
    /// The process id, while it has not been waited for.
    pub fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    /// Its process group, which is its pid (`None` for an
    /// [operator's](HostCommand::operator_terminal) step, which shares the
    /// CLI's).
    pub fn pgid(&self) -> Option<i32> {
        self.pgid
    }

    /// Its stdin, when [piped](HostCommand::stdin_piped).
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.inner.stdin.take()
    }

    /// Its stdout, when piped.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.inner.stdout.take()
    }

    /// Its stderr, when piped.
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.inner.stderr.take()
    }

    /// Wait for it to exit. Running into the timeout kills the group and is
    /// [`ProcError::TimedOut`].
    pub async fn wait(&mut self) -> Result<ExitStatus, ProcError> {
        let (program, timeout) = (self.program.clone(), self.timeout);
        self.wait_bounded()
            .await?
            .ok_or(ProcError::TimedOut { program, timeout })
    }

    /// [`wait`](Self::wait), with `None` for a process that ran into its
    /// timeout and was killed.
    pub async fn wait_bounded(&mut self) -> Result<Option<ExitStatus>, ProcError> {
        let waited = match self.deadline {
            None => Some(self.inner.wait().await),
            Some(at) => {
                tokio::time::timeout_at(tokio::time::Instant::from_std(at), self.inner.wait())
                    .await
                    .ok()
            }
        };
        match waited {
            Some(Ok(status)) => {
                self.exited = true;
                Ok(Some(status))
            }
            Some(Err(source)) => Err(ProcError::Io {
                program: self.program.clone(),
                source,
            }),
            None => {
                self.kill().await;
                Ok(None)
            }
        }
    }

    /// Whether it has exited, without waiting.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcError> {
        let status = self.inner.try_wait().map_err(|source| ProcError::Io {
            program: self.program.clone(),
            source,
        })?;
        self.exited |= status.is_some();
        Ok(status)
    }

    /// Kill the process group now, without waiting.
    pub fn kill_group(&self) {
        if let Some(pgid) = self.pgid {
            kill_group(pgid);
        }
    }

    /// Kill the process group and reap the program.
    pub async fn kill(&mut self) {
        self.kill_group();
        let _ = self.inner.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.wait()).await;
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !(self.exited && self.leave_descendants) {
            self.kill_group();
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::authority::SystemTask;

    const TEST: SystemTask = SystemTask::new("sc-proc test");

    #[tokio::test]
    async fn the_child_sees_only_the_base_environment_and_what_was_given() {
        let mut cmd = HostCommand::new("sh", &TEST, Timeout::secs(10));
        cmd.flag_value("-c", "env")
            .env(crate::env_name!("FELDSPAR_PROC_GIVEN"), "yes");
        let out = cmd.output().await.unwrap();
        let env = String::from_utf8_lossy(&out.stdout);
        assert!(env.contains("FELDSPAR_PROC_GIVEN=yes"), "{env}");
        assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
        // The test binary's own environment has CARGO_* and RUST_* variables:
        // none of them reaches the child.
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
        assert!(!env.contains("CARGO_PKG_NAME"), "{env}");
    }

    #[tokio::test]
    async fn the_timeout_kills_the_whole_group() {
        let marker = std::env::temp_dir().join(format!("sc-proc-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut cmd = HostCommand::new("sh", &TEST, Timeout::after(Duration::from_millis(300)));
        // A grandchild that would write the marker after the timeout.
        cmd.flag_value(
            "-c",
            format!("(sleep 1; touch {}) & sleep 30", marker.display()),
        );
        let started = Instant::now();
        let err = cmd.output().await.unwrap_err();
        assert!(err.is_timeout(), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the grandchild outlived the timeout");
    }

    #[test]
    fn the_blocking_form_times_out_too() {
        let mut cmd = HostCommand::new("sleep", &TEST, Timeout::after(Duration::from_millis(200)));
        cmd.positional(Operand::number(30));
        let started = Instant::now();
        assert!(cmd.output_blocking().unwrap_err().is_timeout());
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut ok = HostCommand::new("echo", &TEST, Timeout::secs(10));
        ok.positional(Operand::new("hi").unwrap());
        assert_eq!(ok.output_blocking().unwrap().stdout, b"hi\n");
    }

    #[tokio::test]
    async fn operands_follow_a_single_separator() {
        let mut cmd = HostCommand::new("printf", &TEST, Timeout::secs(10));
        cmd.arg("%s|")
            .operand(Operand::new("a").unwrap())
            .operand(Operand::path("-b"));
        let out = cmd.output().await.unwrap();
        // printf takes `--` as the end of its options.
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a|./-b|");
        assert_eq!(cmd_line(), "printf %s| -- a ./-b");
    }

    fn cmd_line() -> String {
        let mut cmd = HostCommand::new("printf", &TEST, Timeout::secs(10));
        cmd.arg("%s|")
            .operand(Operand::new("a").unwrap())
            .operand(Operand::path("-b"));
        cmd.command_line()
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let cmd = HostCommand::new("feldspar-no-such-program", &TEST, Timeout::secs(10));
        let err = cmd.output().await.unwrap_err();
        assert!(err.is_not_found(), "{err}");
    }
}
