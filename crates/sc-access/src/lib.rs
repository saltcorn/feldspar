//! The row-access layer (code-quality review §2, §4.4): reading and writing a
//! table's rows, and deciding who may.
//!
//! Every API surface — the admin API's row endpoints, an application's REST and
//! GraphQL providers, offline sync, a code body's `db`, an action, an agent's
//! tools, a live socket — reads and writes rows through this crate, so the rule
//! of who may is written once and audited here.
//!
//! - [`Scoped`] is the handle a caller is given: a catalog with an
//!   [`Authority`](sc_catalog::Authority) bound to it, exposing only the row
//!   operations that authority may make. A caller's reads and writes go through
//!   the ownership rule ([`ownership`]); the system's are unchecked and
//!   inventoried.
//! - [`rows`] is the row CRUD itself. Its operations decide nothing about the
//!   table's role floor or ownership formula, so each takes an
//!   [`Unchecked`](sc_catalog::Unchecked): proof that the server's own authority
//!   is acting, that the caller is an administrator, or that a named check
//!   already decided.
//! - [`ownership`] is §7.3's rule — the table's floors, the ownership formula,
//!   row-level security — as the guarded `*_as` functions, and [`write_as`] the
//!   REST API's write rule built from it.
//! - [`filter`], [`search`] and [`vector`] are the filtering vocabulary every
//!   syntax lowers through; [`user_rows`] the users table's own rules
//!   (the bridge from JSON to the query layer's `Value` is `sc_types::json`'s);
//!   and [`calc_read`] calculated fields on the read path.

pub mod calc_read;
pub mod filter;
pub mod ownership;
pub mod rows;
mod scoped;
pub mod search;
pub mod user_rows;
pub mod vector;
pub mod write_as;

#[cfg(test)]
mod testing;

/// The row layer's own reads that decide nothing for anybody: the row count a
/// search or a nearest-neighbour read checks against its scan limit before it
/// runs as its caller.
pub(crate) const SCAN_COUNT: sc_catalog::Unchecked<'static> =
    sc_catalog::Unchecked::system("row layer: count the rows a scan would read");

pub use ownership::{
    RowImage, delete_row_as, insert_row_as, principal_at, principal_of, read_row_values_as,
    read_rows_as, row_deletable_as, row_insertable_as, row_readable_as, row_updatable_as,
    update_row_as,
};
pub use scoped::Scoped;

/// `principal`'s [`Unchecked`] access, refused when it is not an
/// administrator's: what code that runs as "admin, in somebody's name" (an
/// action's write, a code body's `asAdmin()` handle, an admin's own row
/// endpoints) hands the row layer.
pub fn admin_access(
    principal: &sc_catalog::Principal,
) -> sc_error::Result<sc_catalog::Unchecked<'_>> {
    sc_catalog::Unchecked::admin(principal).ok_or_else(|| {
        sc_error::Error::auth("only an administrator may read or write rows unchecked")
    })
}
pub use user_rows::{ADMIN_ONLY_WRITE, api_row};
