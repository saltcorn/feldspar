//! Every migration, oldest first.

use crate::{Body, BoxFuture, Migration, MigrationCtx, Result};

/// Every migration Feldspar has, in the order they run — which is the order of
/// their ids, and [`check_registry`](crate::check_registry) refuses a list that
/// is not.
///
/// Append new ones at the end; never edit or remove one that has been released,
/// because a database that applied it has recorded its id. See the crate
/// documentation for how to write one.
pub static MIGRATIONS: &[Migration] = &[APP_API_TABLES, ADMIN_SESSIONS_RESCOPED];

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

/// Sessions now carry the host they were issued on, and the admin host refuses
/// an application's session admin authority (security review #15). A session
/// from before that carries no scope and so reads as an admin-host one, which
/// would let an administrator's session started on an application's host keep
/// admin authority on the admin host until it expired. Ending every
/// administrator's session closes that window; administrators sign in once more.
/// Nobody else's session can carry admin authority, so theirs are left alone.
pub const ADMIN_SESSIONS_RESCOPED: Migration = Migration {
    id: "2026-11-01-admin-sessions-rescoped",
    description: "end administrators' sessions, which carry no host scope yet",
    body: Body::Rust(admin_sessions_rescoped),
};

fn admin_sessions_rescoped(ctx: &mut MigrationCtx) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        if !ctx.table_exists("_fd_sessions").await? || !ctx.table_exists("users").await? {
            return Ok(());
        }
        // Role 1 is the administrator; only an administrator's session reaches
        // admin routes.
        ctx.batch(
            r#"DELETE FROM "_fd_sessions"
               WHERE "user_id" IN (SELECT "id" FROM "users" WHERE "role" = 1)"#,
        )
        .await
    })
}
