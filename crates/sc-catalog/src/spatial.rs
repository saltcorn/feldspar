//! PostGIS in the primary database (analytics TODO A5.1).

use sc_db::SpatialSupport;
use sc_error::Result;

use crate::Catalog;

/// Install PostGIS in the primary database where the role may, and say whether
/// geometry can be stored there.
///
/// Called once on a server's boot, beside the other bootstraps. A role that may
/// not install the extension is not an error: geometry fields are then refused
/// with the sentence this returns, and everything else works as before.
/// [`Catalog::init`] only *asks*, so a test or a command that never boots a
/// server never tries to change the database.
pub async fn bootstrap_spatial(catalog: &Catalog) -> Result<SpatialSupport> {
    catalog.primary().enable_spatial().await
}
