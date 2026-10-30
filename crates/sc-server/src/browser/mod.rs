//! The headless browser behind the coding agent's `view_app` (TODO §7b) and
//! the checklists' `browser.*` steps (TODO-checklists C2).
//!
//! - [`detect`] finds the binary at boot.
//! - [`snapshot`] renders a page's accessibility tree for the model.
//! - [`locate`] finds the elements a stable locator names in that tree.

mod detect;
mod driver;
pub mod locate;
pub mod snapshot;

pub use driver::{ChromiumDriver, DriverConfig};

pub use detect::{BROWSER_NAMES, check_browser_sandbox, detect_browser};
