//! The one way this system starts a process on its host (security review
//! 2026-11 §4, TODO-type-hardening H4).
//!
//! Before this crate there were 33 spawn sites, and each chose its own
//! environment, timeout and argument quoting: most passed the server's whole
//! environment (`DATABASE_URL`, `PGPASSWORD`, `FELDSPAR_BACKUP_PASSWORD`, cloud
//! credentials) to `npm install` and every dependency's lifecycle script, most
//! had no timeout, and several put a value a model or an admin typed where the
//! program reads options. [`HostCommand`] makes the safe choice the only one
//! that compiles:
//!
//! - **a cleared environment**. The child gets [`BASE_ENV`] (the locale, the
//!   search path, the home and temporary directories, proxy and certificate
//!   settings) and what [`HostCommand::env`] adds, under an [`EnvName`] that
//!   refuses the variables that change how a program loads (`LD_*`,
//!   `NODE_OPTIONS`, `BASH_ENV`, `PYTHON*`, `GIT_*`, …). There is no "inherit
//!   everything", except for the operator's own terminal
//!   ([`HostCommand::operator_terminal`]).
//! - **a timeout**. [`HostCommand::new`] takes a [`Timeout`], which has no
//!   default; "as long as it takes" is spelled
//!   [`Timeout::unbounded`] with a reason. At the timeout the whole process
//!   group is killed.
//! - **operands that cannot be options**. Arguments the program wrote itself
//!   are `&'static str` ([`HostCommand::arg`]); anything else is an [`Operand`],
//!   which refuses a leading `-`, after a `--` that [`HostCommand::operand`]
//!   emits once, or the value of a named flag ([`HostCommand::flag_value`]). A
//!   command line an administrator or operator configured as a whole is
//!   [`HostCommand::configured_args`], greppable.
//! - **a process group of its own**, killed as a whole when the child is
//!   dropped, at the timeout, and (on Linux, `PR_SET_PDEATHSIG`) when the
//!   server dies; stdin is `/dev/null` unless asked for.
//! - **an authority**. [`HostCommand::new`] takes a [`SpawnAuthority`], a
//!   sealed trait with three implementations — [`AdminProof`] (an
//!   administrator, from `sc_catalog::ConsoleAdmin::spawn_proof` or a named
//!   check), [`OperatorCli`] (the `feldspar` CLI) and [`SystemTask`] (the
//!   server's own probes and watchdogs) — and every spawn is logged with it.
//!
//! [`Git`] is `git` over a [`RepoRoot`]: the repository named explicitly
//! (`GIT_DIR`, `GIT_WORK_TREE`), never discovered from a directory users below
//! admin can write, with hooks, `core.fsmonitor` and bare-repository discovery
//! off (#1 / C1).
//!
//! `clippy.toml` forbids `std::process::Command::new`,
//! `tokio::process::Command::new`, `pty_process::Command::new` and
//! `chromiumoxide::Browser::launch` everywhere; this crate is where they are
//! allowed, once each. A test fixture outside it that starts a program says so
//! with an `#[allow]` and a reason.
//!
//! May use `unsafe` (the workspace denies it) for the two `libc` calls a child
//! process needs and std has no safe form of: `prctl(PR_SET_PDEATHSIG)` in
//! `pre_exec`, and `killpg`.
#![allow(unsafe_code)]

mod authority;
mod command;
mod env;
mod git;
#[cfg(feature = "pty")]
mod pty;
mod words;

pub use authority::{AdminProof, OperatorCli, SpawnAuthority, SystemTask};
pub use command::{Child, HostCommand, ProcError};
pub use env::{BASE_ENV, BASE_ENV_PREFIXES};
pub use git::{Git, HARDENING_FLAGS, RepoRoot};
#[cfg(feature = "pty")]
pub use pty::spawn_on_pty;
pub use words::{EnvName, Operand, Program, Timeout};

/// An [`EnvName`] checked at compile time: `env_name!("TERM")`. A name
/// [`EnvName::new`] would refuse is a compile error here.
#[macro_export]
macro_rules! env_name {
    ($name:literal) => {
        const { $crate::EnvName::fixed($name) }
    };
}
