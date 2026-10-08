//! Whether a database can hold geometry (analytics TODO A5.1).
//!
//! Geometry needs PostgreSQL with the PostGIS extension. Unlike the flags of
//! [`DbCapabilities`](crate::DbCapabilities), which are facts about a backend,
//! this is a fact about **one database**: the same server may have PostGIS in
//! one database and not in the next, and an extension can be installed while
//! the server runs. So a driver finds it out by asking
//! ([`DatabaseDriver::detect_spatial`](crate::DatabaseDriver::detect_spatial)),
//! remembers the answer, and says why when the answer is no — the sentence a
//! refused geometry field shows.

use serde::{Deserialize, Serialize};

/// Whether geometry can be stored in a database, and why not when it cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SpatialSupport {
    /// PostGIS is installed in the database.
    Available {
        /// PostGIS's version, as `pg_extension` reports it.
        version: String,
    },
    /// It is not, with the sentence saying why.
    Unavailable {
        /// Why geometry cannot be stored, as a sentence for the admin.
        reason: String,
    },
}

impl SpatialSupport {
    /// The answer for a backend that is not PostgreSQL.
    pub fn not_postgres(backend: &str) -> SpatialSupport {
        SpatialSupport::Unavailable {
            reason: format!(
                "geometry needs PostgreSQL with the PostGIS extension, and this database is \
                 {backend}"
            ),
        }
    }

    /// Whether geometry can be stored.
    pub fn is_available(&self) -> bool {
        matches!(self, SpatialSupport::Available { .. })
    }

    /// `Ok` when geometry can be stored, else the sentence saying why not.
    pub fn require(&self) -> std::result::Result<(), String> {
        match self {
            SpatialSupport::Available { .. } => Ok(()),
            SpatialSupport::Unavailable { reason } => Err(reason.clone()),
        }
    }
}
