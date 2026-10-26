//! Whether a database can hold vectors (TODO-agent-skills AS2.2).
//!
//! On PostgreSQL a vector field needs the pgvector extension, which, like
//! PostGIS, is a fact about **one database** rather than a backend: a driver
//! finds it out by asking
//! ([`DatabaseDriver::detect_vector`](crate::DatabaseDriver::detect_vector)),
//! remembers the answer, and says why when the answer is no — the sentence a
//! refused vector field shows, naming the package to install. SQLite always
//! can: a vector is a blob there, and its distance a function the driver
//! registers.

use serde::{Deserialize, Serialize};

/// Whether vectors can be stored in a database, and why not when they cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VectorSupport {
    /// Vectors can be stored. `version` is pgvector's, or `builtin` where the
    /// driver stores them itself (SQLite).
    Available {
        /// pgvector's version, as `pg_extension` reports it.
        version: String,
    },
    /// They cannot, with the sentence saying why.
    Unavailable {
        /// Why vectors cannot be stored, as a sentence for the admin.
        reason: String,
    },
}

impl VectorSupport {
    /// The answer for a backend that stores vectors itself.
    pub fn builtin() -> VectorSupport {
        VectorSupport::Available {
            version: "builtin".to_owned(),
        }
    }

    /// Whether vectors can be stored.
    pub fn is_available(&self) -> bool {
        matches!(self, VectorSupport::Available { .. })
    }

    /// `Ok` when vectors can be stored, else the sentence saying why not.
    pub fn require(&self) -> std::result::Result<(), String> {
        match self {
            VectorSupport::Available { .. } => Ok(()),
            VectorSupport::Unavailable { reason } => Err(reason.clone()),
        }
    }
}
