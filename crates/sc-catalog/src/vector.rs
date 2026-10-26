//! Vector fields in the catalog (TODO-agent-skills AS2).
//!
//! Three small things the layers above share: whether the primary database can
//! hold vectors ([`bootstrap_vector`]), how a value reaches a vector column
//! ([`write_expr`]), and the bound on a nearest read that has no index to use
//! ([`VECTOR_SCAN_LIMIT`]).

use sc_db::VectorSupport;
use sc_error::Result;
use sc_query::{Expr, Value};
use sc_types::BasicType;

use crate::{Catalog, DataField};

/// The most rows a nearest read reads when no index answers it — every SQLite
/// table, and a Postgres one without a vector index. Each row's distance is
/// computed, so this is the work a single read may do.
pub const VECTOR_SCAN_LIMIT: u64 = 50_000;

/// Install pgvector in the primary database where the role may, and say whether
/// vector fields can be stored there.
///
/// Called once on a server's boot, beside [`bootstrap_spatial`](crate::bootstrap_spatial).
/// A role that may not install the extension is not an error: vector fields are
/// then refused with the sentence this returns.
pub async fn bootstrap_vector(catalog: &Catalog) -> Result<VectorSupport> {
    catalog.primary().enable_vector().await
}

/// The expression a value is written to `field` as: the value itself, except
/// for a vector, which is cast to the column's `vector(n)` — a cast on
/// Postgres, and on SQLite the driver's function that makes the stored blob.
pub fn write_expr(field: Option<&DataField>, value: Value) -> Expr {
    match field.and_then(|f| f.base.type_.as_basic()) {
        Some(ty @ BasicType::Vector(n)) if *n > 0 && !value.is_null() => Expr::Cast {
            expr: Box::new(Expr::lit(value)),
            type_name: ty.sql_type().to_owned(),
        },
        _ => Expr::lit(value),
    }
}
