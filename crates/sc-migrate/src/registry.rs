//! Every migration, oldest first.

use crate::{Body, BoxFuture, Migration, MigrationCtx, Result};

/// Every migration Feldspar has, in the order they run — which is the order of
/// their ids, and [`check_registry`](crate::check_registry) refuses a list that
/// is not.
///
/// Append new ones at the end; never edit or remove one that has been released,
/// because a database that applied it has recorded its id. See the crate
/// documentation for how to write one.
pub static MIGRATIONS: &[Migration] = &[APP_API_TABLES];

/// Applications gain `api_tables`: the connected tables whose generic row API
/// (REST list/read/create/update/delete, GraphQL type, generated hooks) they
/// serve. A new application serves none; one that existed before keeps serving
/// every table it is connected to, as it did.
pub const APP_API_TABLES: Migration = Migration {
    id: "2026-10-30-app-api-tables",
    description: "existing applications keep the row API of every table they are connected to",
    body: Body::Rust(app_api_tables),
};

fn app_api_tables(ctx: &mut MigrationCtx) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        if !ctx.table_exists("_fd_applications").await? {
            return Ok(());
        }
        // The bootstrap adds the column too, but only after migrations run.
        if !ctx.column_exists("_fd_applications", "api_tables").await? {
            ctx.batch(r#"ALTER TABLE "_fd_applications" ADD COLUMN "api_tables" jsonb"#)
                .await?;
        }
        ctx.batch(
            r#"UPDATE "_fd_applications" SET "api_tables" = "tables" WHERE "api_tables" IS NULL"#,
        )
        .await
    })
}
