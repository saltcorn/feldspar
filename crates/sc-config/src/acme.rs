//! `_fd_acme_cache`: where the ACME account key and the issued certificates
//! live (design §13.5).
//!
//! Not configuration — nobody types this and nobody reads it — but it belongs
//! beside configuration because it answers the same question: what does this
//! deployment serve TLS with? It is stored in the **primary database** rather
//! than in a directory so that a renewal survives a restart *and* a second node
//! serves the same certificate: an ACME client that cached to local disk would
//! have each node order its own, which is a rate-limit incident waiting for the
//! deployment that scales out.
//!
//! An entry is opaque bytes under a key derived from what it is *for* — the
//! domain list and the CA's directory URL — so pointing a deployment at the
//! staging directory, or adding a domain, looks for a different entry rather
//! than serving the wrong certificate. The bytes are base64 in a text column,
//! since a private key is not text and the column is.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{BasicType, TypeRef};
use sha2::{Digest, Sha256};

/// Name of the ACME cache table in the primary database.
pub const ACME_CACHE_TABLE: &str = "_fd_acme_cache";

/// The cache key: `cert:<digest>` or `account:<digest>`.
pub const COL_KEY: &str = "key";
/// The cached bytes, base64-encoded.
pub const COL_DATA: &str = "data";

fn cache_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_KEY, TypeRef::Basic(BasicType::Text))
            .required()
            .primary_key(),
        DataField::plain(COL_DATA, TypeRef::Basic(BasicType::Text)).required(),
    ]
}

/// Ensure `_fd_acme_cache` exists. Idempotent, like every other bootstrap.
pub async fn bootstrap_acme_cache(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(ACME_CACHE_TABLE, &cache_fields())
        .await
}

/// The database-backed store the ACME client caches through.
///
/// Deliberately **not** the `rustls-acme` cache traits: those live a layer up,
/// in `sc-server`, which is where the ACME client is. This is two methods over a
/// table, and the adapter that spells them as somebody's trait is that layer's
/// business (§2's layering: a crate does not name a dependency of the crate
/// above it).
#[derive(Clone)]
pub struct AcmeCache {
    catalog: std::sync::Arc<Catalog>,
}

impl std::fmt::Debug for AcmeCache {
    /// The catalog is not printable and the entries are key material, so this
    /// says what the value *is* and nothing about what it holds.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AcmeCache")
    }
}

impl AcmeCache {
    /// A cache over this catalog's primary database.
    pub fn new(catalog: std::sync::Arc<Catalog>) -> AcmeCache {
        AcmeCache { catalog }
    }

    /// The cached certificate for these domains from this directory, if any.
    pub async fn load_cert(
        &self,
        domains: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.load(&cert_key(domains, directory_url)).await
    }

    /// Cache a freshly issued certificate.
    pub async fn store_cert(
        &self,
        domains: &[String],
        directory_url: &str,
        data: &[u8],
    ) -> Result<()> {
        self.store(&cert_key(domains, directory_url), data).await
    }

    /// The cached ACME account for this contact set and directory, if any.
    pub async fn load_account(
        &self,
        contacts: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.load(&account_key(contacts, directory_url)).await
    }

    /// Cache a newly registered ACME account.
    pub async fn store_account(
        &self,
        contacts: &[String],
        directory_url: &str,
        data: &[u8],
    ) -> Result<()> {
        self.store(&account_key(contacts, directory_url), data)
            .await
    }

    /// Read one entry.
    pub async fn load(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let select = Select::from(Source::table(ACME_CACHE_TABLE))
            .filter(Expr::col(COL_KEY).eq(Expr::lit(key)))
            .limit(1);
        let Some(row) = self.rows(select).await?.into_iter().next() else {
            return Ok(None);
        };
        let encoded = match row.get(COL_DATA) {
            Some(Value::Text(t)) => t.clone(),
            Some(Value::Null) | None => return Ok(None),
            other => {
                return Err(Error::invalid(format!(
                    "{ACME_CACHE_TABLE}.{COL_DATA} should be text, got {}",
                    other.map(|v| v.kind()).unwrap_or("nothing")
                )));
            }
        };
        BASE64
            .decode(encoded.as_bytes())
            .map(Some)
            .map_err(|e| Error::invalid(format!("cached ACME entry `{key}` is not base64: {e}")))
    }

    /// Write one entry, replacing whatever was there.
    pub async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let encoded = BASE64.encode(data);
        let exists = {
            let select = Select::from(Source::table(ACME_CACHE_TABLE))
                .filter(Expr::col(COL_KEY).eq(Expr::lit(key)))
                .limit(1);
            !self.rows(select).await?.is_empty()
        };
        let statement = if exists {
            Statement::from(
                Update::new(
                    ACME_CACHE_TABLE,
                    vec![Assignment::new(COL_DATA, Expr::lit(encoded))],
                )
                .filter(Expr::col(COL_KEY).eq(Expr::lit(key))),
            )
        } else {
            Statement::from(Insert::row(
                ACME_CACHE_TABLE,
                vec![COL_KEY.to_owned(), COL_DATA.to_owned()],
                vec![Expr::lit(key), Expr::lit(encoded)],
            ))
        };
        self.run(statement).await
    }

    /// Forget one entry — what an admin does when a cached certificate is for a
    /// domain set the deployment no longer serves.
    pub async fn forget(&self, key: &str) -> Result<()> {
        self.run(Statement::from(
            Delete::from(ACME_CACHE_TABLE).filter(Expr::col(COL_KEY).eq(Expr::lit(key))),
        ))
        .await
    }

    async fn rows(&self, select: Select) -> Result<Vec<Row>> {
        self.catalog
            .primary(crate::INTERNAL_TABLES)
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await
    }

    async fn run(&self, statement: Statement) -> Result<()> {
        self.catalog
            .primary(crate::INTERNAL_TABLES)
            .query(&statement)
            .await?
            .try_collect()
            .await?;
        Ok(())
    }
}

/// The cache key for a certificate covering `domains`, issued by
/// `directory_url`.
pub fn cert_key(domains: &[String], directory_url: &str) -> String {
    format!("cert:{}", digest(domains, directory_url))
}

/// The cache key for the ACME account registered with `contacts` at
/// `directory_url`.
pub fn account_key(contacts: &[String], directory_url: &str) -> String {
    format!("account:{}", digest(contacts, directory_url))
}

/// A stable digest of a list plus a URL.
///
/// The list is **not** sorted: `rustls-acme` orders a certificate for the
/// domains in the order it was given them, and two orders of the same names are
/// two different certificates as far as the CA is concerned. Hashing what we
/// were handed keeps the key and the artifact in step.
fn digest(parts: &[String], directory_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(directory_url.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(ACME_CACHE_TABLE.starts_with("_fd_"));
        assert!(cache_fields()[0].primary_key);
    }

    /// The key is what stops a staging certificate being served in production,
    /// and what stops a stale certificate outliving a domain being added.
    #[test]
    fn the_key_is_what_the_entry_is_for() {
        let a = vec!["example.com".to_owned()];
        let b = vec!["example.com".to_owned(), "www.example.com".to_owned()];
        let prod = crate::ssl::LETSENCRYPT_PRODUCTION;
        let staging = crate::ssl::LETSENCRYPT_STAGING;

        assert_eq!(cert_key(&a, prod), cert_key(&a, prod));
        assert_ne!(cert_key(&a, prod), cert_key(&b, prod));
        assert_ne!(cert_key(&a, prod), cert_key(&a, staging));
        assert_ne!(cert_key(&a, prod), account_key(&a, prod));

        // Two names cannot be one by being joined differently.
        let joined = vec!["example.comwww.example.com".to_owned()];
        assert_ne!(cert_key(&b, prod), cert_key(&joined, prod));
    }
}
