//! `feldspar cmdstan …`: CmdStan installation.

use sc_error::Result;

/// `feldspar cmdstan status | install` (TODO "Bayesian models with Stan" §20).
///
/// No database: `status` reports what is on this machine, and `install` is a
/// download and a build the operator asked for.
pub(crate) async fn cmdstan_command(args: &[String]) -> Result<()> {
    use sc_cli::cmdstan::{CmdStanArgs, install, status};

    match CmdStanArgs::parse(args)? {
        CmdStanArgs::Status { cmdstan } => status(cmdstan),
        CmdStanArgs::Install { version, dir, jobs } => install(version, dir, jobs).await,
    }
}
