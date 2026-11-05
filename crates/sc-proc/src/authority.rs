//! On whose authority a process is started: the sealed [`SpawnAuthority`].

use std::fmt;

mod sealed {
    /// Only this crate's three authorities are [`SpawnAuthority`](super::SpawnAuthority).
    pub trait Sealed {}
}

/// Who a process is started for. Sealed: the three kinds below are all there
/// is, so a spawn names one of them, and every spawn's log line says which.
///
/// - [`AdminProof`]: an administrator. Made from the console's proof
///   (`sc_catalog::ConsoleAdmin::spawn_proof`), or, where the proof does not
///   reach the spawn, by [`AdminProof::vouched`] naming the check that stands
///   behind it (listed in the system-authority inventory,
///   docs/SECURITY_REVIEW_2026-10.md).
/// - [`OperatorCli`]: the `feldspar` command line, run by whoever has a shell
///   on the machine.
/// - [`SystemTask`]: the server's own work — a version probe, a watchdog, a
///   read-only `git log` — whose arguments the program writes itself.
pub trait SpawnAuthority: sealed::Sealed {
    /// The authority, as the spawn's log line names it.
    fn audit(&self) -> String;
}

/// An administrator's authority to run a program on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminProof {
    user: Option<String>,
    by: &'static str,
}

impl AdminProof {
    /// An administrator's authority — `user`'s, or no user for a system-run
    /// admin tool — which the check named `by` stands behind.
    ///
    /// `sc_catalog::ConsoleAdmin::spawn_proof` is the usual way to make one.
    /// This constructor is for a spawn the proof does not reach (a file-store
    /// operation behind the admin route, a build an admin started): the `by`
    /// names the check, and the call site is listed in the system-authority
    /// inventory.
    pub fn vouched(user: Option<String>, by: &'static str) -> AdminProof {
        AdminProof { user, by }
    }

    /// The administrator, when there is one.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The check that stands behind it.
    pub fn vouched_by(&self) -> &'static str {
        self.by
    }
}

impl sealed::Sealed for AdminProof {}
impl SpawnAuthority for AdminProof {
    fn audit(&self) -> String {
        match &self.user {
            Some(user) => format!("admin {user} ({})", self.by),
            None => format!("admin ({})", self.by),
        }
    }
}

/// The operator's authority: the `feldspar` command line, which runs as
/// whoever typed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorCli {
    what: &'static str,
}

impl OperatorCli {
    /// The CLI, doing `what` (`"setup"`, `"cmdstan install"`).
    pub const fn new(what: &'static str) -> OperatorCli {
        OperatorCli { what }
    }
}

impl sealed::Sealed for OperatorCli {}
impl SpawnAuthority for OperatorCli {
    fn audit(&self) -> String {
        format!("operator CLI ({})", self.what)
    }
}

/// The server's own work, for `reason`: a probe, a watchdog, a read-only
/// query whose arguments the program writes itself. Not for running a
/// program, or arguments, that somebody configured or sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTask(&'static str);

impl SystemTask {
    /// The server's own work, for `reason`.
    pub const fn new(reason: &'static str) -> SystemTask {
        SystemTask(reason)
    }

    /// The reason.
    pub fn reason(&self) -> &'static str {
        self.0
    }
}

impl sealed::Sealed for SystemTask {}
impl SpawnAuthority for SystemTask {
    fn audit(&self) -> String {
        format!("system task ({})", self.0)
    }
}

impl fmt::Display for AdminProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.audit())
    }
}
