//! `feldspar cmdstan …`: CmdStan installation.

use sc_error::Result;

/// `feldspar cmdstan status | install` (TODO "Bayesian models with Stan" §20).
///
/// No database: `status` reports what is on this machine, and `install` is a
/// download and a build the operator asked for.
pub(crate) async fn cmdstan_command(args: &[String]) -> Result<()> {
    use sc_cli::cmdstan::{CmdStanArgs, install, status, warm};

    match CmdStanArgs::parse(args)? {
        CmdStanArgs::Status { cmdstan } => status(cmdstan),
        CmdStanArgs::Install { version, dir, jobs } => install(version, dir, jobs).await,
        CmdStanArgs::Warm {
            cmdstan,
            cache_dir,
            families,
            list,
        } => {
            let dir = cache_dir.unwrap_or_else(sc_server::stan_cache_dir);
            warm(cmdstan, dir, &families, list).await
        }
    }
}
