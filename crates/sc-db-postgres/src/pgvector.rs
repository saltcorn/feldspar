//! Finding pgvector in a database and installing it where the role may
//! (TODO-agent-skills AS2.2).
//!
//! pgvector is what a vector field is stored as on Postgres (`vector(n)`), and
//! what answers a nearest read (`<=>`, with an HNSW index). It is not a
//! *trusted* extension, so only a superuser — or the database's owner, where
//! the server marks it trusted — can install it; a server that cannot is told
//! which package to install and what to run.

use sc_db::VectorSupport;
use sc_error::{Error, Result};
use tokio_postgres::Client;

/// The Debian/Ubuntu package that carries pgvector for a server of `major`
/// version, which is what the refusal names.
fn package(major: Option<u32>) -> String {
    match major {
        Some(major) => format!("postgresql-{major}-pgvector"),
        None => "postgresql-<version>-pgvector".to_owned(),
    }
}

/// Whether pgvector is installed in the database `client` is connected to.
pub async fn detect(client: &Client) -> Result<VectorSupport> {
    let rows = client
        .query(
            "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
            &[],
        )
        .await
        .map_err(|e| Error::database(format!("looking for pgvector: {e}")))?;
    if let Some(row) = rows.first() {
        return Ok(VectorSupport::Available {
            version: row.get(0),
        });
    }
    let on_server = client
        .query(
            "SELECT 1 FROM pg_available_extensions WHERE name = 'vector'",
            &[],
        )
        .await
        .map_err(|e| Error::database(format!("looking for pgvector: {e}")))?;
    let major = client
        .query_one(
            "SELECT current_setting('server_version_num')::int / 10000",
            &[],
        )
        .await
        .ok()
        .and_then(|row| row.try_get::<_, i32>(0).ok())
        .and_then(|n| u32::try_from(n).ok());
    Ok(VectorSupport::Unavailable {
        reason: if on_server.is_empty() {
            format!(
                "vector fields need the pgvector extension, which is not installed on this \
                 PostgreSQL server; install the `{}` package (or build pgvector from source) \
                 and restart, and Feldspar adds the extension on its next start where its \
                 role may (OPERATIONS.md, \"Vectors with pgvector\")",
                package(major)
            )
        } else {
            "vector fields need the pgvector extension, which this PostgreSQL server has but \
             this database does not; a superuser can add it with CREATE EXTENSION vector"
                .to_owned()
        },
    })
}

/// Install pgvector where the role may, then [`detect`].
pub async fn enable(client: &Client) -> Result<VectorSupport> {
    let found = detect(client).await?;
    if found.is_available() {
        return Ok(found);
    }
    match client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
        .await
    {
        Ok(()) => detect(client).await,
        // Not permitted, or not on the server: `detect`'s sentence says which.
        Err(_) => Ok(found),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_names_the_package_for_the_servers_version() {
        assert_eq!(package(Some(17)), "postgresql-17-pgvector");
        assert_eq!(package(None), "postgresql-<version>-pgvector");
    }
}
