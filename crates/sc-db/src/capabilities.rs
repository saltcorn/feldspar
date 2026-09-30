//! What a connected database can do — advertised by
//! [`DatabaseDriver::capabilities`](crate::DatabaseDriver::capabilities).
//!
//! Capabilities are queried, never assumed. The authorization layer (§7) picks
//! its enforcement strategy from these flags (e.g. row-level security only when
//! the backend supports it), and the message bus only wires up a pg-notify
//! driver when `listen_notify` is advertised (§16).

use serde::{Deserialize, Serialize};

/// Feature flags describing what a particular [`DatabaseDriver`] backend
/// supports (technical design §5).
///
/// The set is intentionally small and grows only as a real decision hangs off a
/// flag. Construct via [`DbCapabilities::none`] and enable the relevant fields,
/// so adding a field never silently flips existing drivers to "supported".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DbCapabilities {
    /// The backend enforces row-level security policies. Drives the authz
    /// strategy in §7.
    pub row_level_security: bool,
    /// Tables may have a primary key spanning more than one column. Required by
    /// the goals; Postgres supports it.
    pub composite_pk: bool,
    /// The backend offers a `LISTEN`/`NOTIFY`-style channel, enabling the
    /// pg-notify message-bus driver (§16).
    pub listen_notify: bool,
    /// Data-modifying statements can return affected rows (`RETURNING`), so an
    /// insert/update/delete yields the resulting row without a follow-up query.
    pub returning: bool,
    /// An identity column is numbered by a **separate sequence object**, which
    /// can therefore fall behind the rows in the table.
    ///
    /// Postgres's is: rows written with explicit keys (a restored backup, an
    /// imported CSV that carries its own `id` column) do not move the sequence,
    /// so the next insert collides unless something winds it past the largest
    /// key — which is what `sc_api::csv` does after an import. A backend that
    /// derives the next key from the table itself (SQLite's rowid) has nothing
    /// to wind and must not be sent the statement that would do it.
    pub identity_sequences: bool,
    /// A table can be created without write-ahead logging — Postgres's
    /// `UNLOGGED`. Drives
    /// [`SchemaChange::CreateTable::unlogged`](crate::SchemaChange::CreateTable),
    /// which the session store (§7.2) asks for: session rows are worth sharing
    /// between nodes and not worth a WAL record, and losing them all to an
    /// unclean shutdown costs a re-login.
    pub unlogged_tables: bool,
    /// Dates, times, timestamps and UUIDs are types of their own, so
    /// `CAST(x AS date)` makes a date.
    ///
    /// Postgres's are. SQLite stores them as text and gives a cast a numeric
    /// affinity by its type's name, so there `CAST('2024-01-05' AS date)` is
    /// `2024`. A dataset's compiled query (analytics TODO A1.4) casts the
    /// literals it generates, and casts those to `text` on a backend without
    /// them.
    pub native_temporal_types: bool,
}

impl DbCapabilities {
    /// A capability set with every feature disabled — the safe starting point a
    /// driver enables fields from.
    pub const fn none() -> Self {
        DbCapabilities {
            row_level_security: false,
            composite_pk: false,
            listen_notify: false,
            returning: false,
            identity_sequences: false,
            unlogged_tables: false,
            native_temporal_types: false,
        }
    }
}

impl Default for DbCapabilities {
    fn default() -> Self {
        DbCapabilities::none()
    }
}
