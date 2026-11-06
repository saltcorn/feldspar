//! The IDE terminal's program on a pseudo-terminal (`pty` feature): the one
//! spawn that is not a pipe, and an administrator's alone.

use std::ffi::OsString;
use std::path::Path;

use crate::authority::{AdminProof, SpawnAuthority};
use crate::words::{EnvName, Program, Timeout};

/// Start `program` with `args` in `dir` on the terminal `pts`, with exactly
/// `env` — not even [`BASE_ENV`](crate::BASE_ENV): the terminal says what an
/// interactive session gets. The program leads a new session with `pts` as
/// its controlling terminal; ending it, and what it started, is the caller's
/// (it outlives no socket: killed on drop as well).
///
/// `args` is configured as a whole by the server (the coding agent's flags),
/// as [`HostCommand::configured_args`](crate::HostCommand::configured_args) is.
pub fn spawn_on_pty(
    program: &Path,
    who: &AdminProof,
    args: &[OsString],
    dir: &Path,
    env: &[(EnvName, String)],
    pts: pty_process::Pts,
) -> pty_process::Result<tokio::process::Child> {
    sc_log::log_verbose!(
        "spawn `{}` on a terminal in {} on {}, {}",
        Program::path(program),
        dir.display(),
        who.audit(),
        Timeout::unbounded("an IDE terminal lives as long as its socket")
    );
    #[allow(
        clippy::disallowed_methods,
        reason = "sc-proc is the one place a process is started; this is the terminal's"
    )]
    let command = pty_process::Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v)))
        .kill_on_drop(true)
        .spawn(pts)
}
