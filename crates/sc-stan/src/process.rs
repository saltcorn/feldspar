//! The child processes a fit starts — `make` for a compile, the compiled model
//! once per chain — and the three things every one of them gets (TODO §13):
//!
//! - **a scrubbed environment**: `PATH`, `HOME`, `TMPDIR` and CmdStan's own
//!   variables, nothing else, so a model never sees the server's secrets;
//! - **a process group of its own**, killed as a whole — `make` is a tree of
//!   compilers, and killing the leader alone leaves them running;
//! - **death with the server**: `kill_on_drop`, a group kill when the handle is
//!   dropped, and on Linux `PR_SET_PDEATHSIG`, so a server that dies takes its
//!   chains with it rather than leaving them sampling for an hour into a
//!   directory nobody will read.
//!
//! And [`Watch`]: what stops a running child early — a cancel, read from the
//! fit's context, or the fit's deadline — polled while its output is read.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};

/// The variables a child keeps from the server's environment. `CXX` is the
/// operator's choice of compiler (the one `feldspar cmdstan status` reports),
/// not an admin's: nothing an admin configures reaches a child's environment
/// or command line.
const KEPT_ENV: [&str; 5] = ["PATH", "HOME", "TMPDIR", "CMDSTAN", "CXX"];

/// How often a running child is checked for a cancel or the deadline.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// How many lines of a child's output a failure quotes (TODO §13).
pub(crate) const TAIL_LINES: usize = 40;

/// A command for `program`, configured as every child of a fit is: scrubbed
/// environment, no stdin, both streams piped, its own process group, killed
/// on drop and, on Linux, when the server dies.
pub(crate) fn command(program: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in KEPT_ENV {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    // CmdStan's own variables (`STAN_THREADS`, `STAN_NUM_THREADS`, …).
    for (var, value) in std::env::vars_os() {
        if var.to_string_lossy().starts_with("STAN_") {
            command.env(var, value);
        }
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(target_os = "linux")]
    // SAFETY: `prctl` is async-signal-safe, which is all a `pre_exec` closure
    // may call.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    command
}

/// Kills the process group led by the given process when dropped. After the
/// group has exited normally the signal finds nothing, which is harmless.
pub(crate) struct KillGroupOnDrop(pub(crate) Option<u32>);

impl KillGroupOnDrop {
    /// Kill the group now.
    pub(crate) fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: `killpg` takes plain integers and has no memory effects;
            // `pid` leads the group because the child was spawned with
            // `process_group(0)`.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Every line `stream` produces, sent to `tx` as it arrives.
pub(crate) fn forward_lines(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

/// A child's stdout and stderr as one stream of lines, in arrival order.
pub(crate) fn output_lines(
    child: &mut tokio::process::Child,
) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        forward_lines(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_lines(stderr, tx);
    }
    rx
}

/// The last [`TAIL_LINES`] lines of a child's output.
#[derive(Debug, Default)]
pub(crate) struct Tail(VecDeque<String>);

impl Tail {
    pub(crate) fn push(&mut self, line: &str) {
        if self.0.len() == TAIL_LINES {
            self.0.pop_front();
        }
        self.0.push_back(line.to_owned());
    }

    pub(crate) fn lines(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub(crate) fn text(&self) -> String {
        self.lines().collect::<Vec<_>>().join("\n")
    }
}

/// Why a child was stopped before it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The fit was cancelled.
    Cancelled,
    /// The fit ran past its `max_runtime_minutes`.
    TimedOut,
}

/// What stops a fit's children early: a cancel flag, polled, and a deadline.
pub struct Watch<'a> {
    started: Instant,
    limit: Duration,
    cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl<'a> Watch<'a> {
    /// A watch that has been running since now, stops at `limit`, and asks
    /// `cancelled` whether to stop sooner.
    pub fn new(limit: Duration, cancelled: &'a (dyn Fn() -> bool + Sync)) -> Watch<'a> {
        Watch {
            started: Instant::now(),
            limit,
            cancelled,
        }
    }

    /// Whether to stop, and why.
    pub fn stopped(&self) -> Option<Stopped> {
        if (self.cancelled)() {
            Some(Stopped::Cancelled)
        } else if self.started.elapsed() >= self.limit {
            Some(Stopped::TimedOut)
        } else {
            None
        }
    }

    /// How long the fit has been running.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The limit.
    pub fn limit(&self) -> Duration {
        self.limit
    }

    /// Wait until `stopped` would answer something — for a `select!` beside
    /// work that might finish first.
    pub(crate) async fn until_stopped(&self) -> Stopped {
        loop {
            if let Some(why) = self.stopped() {
                return why;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// "3 minutes 12 seconds" — how long something ran, for a sentence.
pub(crate) fn duration_words(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let unit = |n: u64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (h, m) {
        (0, 0) => unit(s, "second"),
        (0, _) => format!("{} {}", unit(m, "minute"), unit(s, "second")),
        _ => format!("{} {}", unit(h, "hour"), unit(m, "minute")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tail_keeps_the_last_forty_lines() {
        let mut tail = Tail::default();
        for i in 0..100 {
            tail.push(&format!("line {i}"));
        }
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line 60");
        assert_eq!(lines[39], "line 99");
    }

    #[test]
    fn durations_read_as_words() {
        assert_eq!(duration_words(Duration::from_secs(1)), "1 second");
        assert_eq!(
            duration_words(Duration::from_secs(192)),
            "3 minutes 12 seconds"
        );
        assert_eq!(duration_words(Duration::from_secs(3660)), "1 hour 1 minute");
    }

    #[test]
    fn a_watch_says_cancelled_before_timed_out() {
        let yes = || true;
        let no = || false;
        assert_eq!(
            Watch::new(Duration::ZERO, &yes).stopped(),
            Some(Stopped::Cancelled)
        );
        assert_eq!(
            Watch::new(Duration::ZERO, &no).stopped(),
            Some(Stopped::TimedOut)
        );
        assert_eq!(Watch::new(Duration::from_secs(60), &no).stopped(), None);
    }
}
