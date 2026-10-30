//! Every migration, oldest first.

use crate::Migration;

/// Every migration Feldspar has, in the order they run — which is the order of
/// their ids, and [`check_registry`](crate::check_registry) refuses a list that
/// is not.
///
/// Append new ones at the end; never edit or remove one that has been released,
/// because a database that applied it has recorded its id. See the crate
/// documentation for how to write one.
pub static MIGRATIONS: &[Migration] = &[];
