//! CmdStan on this machine: which one, and how to get one (TODO §20).
//!
//! - [`discover`] finds a CmdStan: `--cmdstan <dir>`, else `$CMDSTAN`, else the
//!   newest `~/.cmdstan/cmdstan-*` (cmdstanpy's convention, so an existing
//!   install is picked up). It reads the version and refuses anything older than
//!   [`MIN_VERSION`].
//! - [`Toolchain::find`] looks for `make` and a C++ compiler, which compiling a
//!   Stan program needs as much as it needs CmdStan.
//! - [`install`] downloads a release tarball from GitHub and builds it. That is
//!   a download and a build an admin runs on purpose from a shell
//!   (`feldspar cmdstan install`); the server never calls it.

mod discover;
mod install;
mod version;

pub use discover::{CmdStan, Locations, Source, Toolchain, default_root, discover};
pub use install::{
    InstallEvent, InstallOptions, Installed, LATEST_RELEASE_API, install, latest_release,
    release_url, target_dir,
};
pub use version::{MIN_VERSION, Version};
