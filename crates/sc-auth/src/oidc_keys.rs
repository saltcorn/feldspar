//! The identity provider's signing keys (TODO IP1.2): what an `id_token` is
//! signed with, and the JWKS a relying party checks it against.
//!
//! One key set serves the whole instance. There is an **RS256** key, which is
//! OpenID Connect's default `id_token_signed_response_alg` and the one every
//! relying party can check, and an **ES256** key for those that prefer it.
//! Each is made the first time it is needed ([`ensure_signing_keys`]).
//!
//! # At rest
//!
//! A row of [`OIDC_KEYS_TABLE`] holds the public JWK as it is published and
//! the private key as PKCS#8 PEM **sealed with the instance key**
//! (`sc_catalog::secrets`): a copy of the database alone signs nothing.
//!
//! # Rotation
//!
//! [`rotate_signing_keys`] retires every live key and makes new ones. A new
//! key signs from then on; a retired one stays in the JWKS for
//! [`RETIRED_KEY_PUBLISHED_HOURS`], far longer than any token it signed
//! lasts, so a relying party holding one can still check it.
//!
//! # A key this instance cannot open
//!
//! A database restored onto an instance with another instance key has keys it
//! cannot open. Such a key is never served: it is left out of the JWKS, and
//! [`ensure_signing_keys`] retires it, says so in the log and in its answer,
//! and makes a new one, so the provider keeps working with keys it can use.

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::jwk::{Jwk, KeyAlgorithm, PublicKeyUse, ThumbprintHash};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, Insert, Select, Source, Statement, UnOp, Update, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;

/// The signing keys.
pub const OIDC_KEYS_TABLE: &str = "_fd_oidc_keys";

/// How long a retired key stays in the JWKS: far longer than any `id_token`
/// it signed lives.
pub const RETIRED_KEY_PUBLISHED_HOURS: i64 = 24;

const COL_KID: &str = "kid";
const COL_ALG: &str = "alg";
const COL_PUBLIC_JWK: &str = "public_jwk";
const COL_PRIVATE_KEY: &str = "private_key";
const COL_CREATED_AT: &str = "created_at";
const COL_RETIRED_AT: &str = "retired_at";

/// An algorithm the provider signs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SigningAlg {
    /// RSASSA-PKCS1-v1_5 with SHA-256 over a 2048-bit key: OIDC's default.
    Rs256,
    /// ECDSA over P-256 with SHA-256.
    Es256,
}

impl SigningAlg {
    /// Both, RS256 first: the order discovery lists them in.
    pub const ALL: [SigningAlg; 2] = [SigningAlg::Rs256, SigningAlg::Es256];

    /// Its JOSE name.
    pub fn name(self) -> &'static str {
        match self {
            SigningAlg::Rs256 => "RS256",
            SigningAlg::Es256 => "ES256",
        }
    }

    /// The algorithm a JOSE name means, if it is one of these.
    pub fn from_name(name: &str) -> Option<SigningAlg> {
        SigningAlg::ALL.into_iter().find(|alg| alg.name() == name)
    }

    fn jose(self) -> Algorithm {
        match self {
            SigningAlg::Rs256 => Algorithm::RS256,
            SigningAlg::Es256 => Algorithm::ES256,
        }
    }

    fn encoding_key(self, pem: &str) -> Result<EncodingKey> {
        let key = match self {
            SigningAlg::Rs256 => EncodingKey::from_rsa_pem(pem.as_bytes()),
            SigningAlg::Es256 => EncodingKey::from_ec_pem(pem.as_bytes()),
        };
        key.map_err(|e| Error::msg(format!("an {} signing key will not load: {e}", self.name())))
    }
}

fn key_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_KID, text()).required().primary_key(),
        DataField::plain(COL_ALG, text()).required(),
        DataField::plain(COL_PUBLIC_JWK, TypeRef::Basic(BasicType::Json)).required(),
        DataField::plain(COL_PRIVATE_KEY, text()).required(),
        DataField::plain(COL_CREATED_AT, timestamp()).required(),
        DataField::plain(COL_RETIRED_AT, timestamp()),
    ]
}

pub(crate) async fn bootstrap_oidc_keys(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(OIDC_KEYS_TABLE, &key_fields())
        .await
}

/// One stored key, as the JWKS and the admin see it: no private half.
#[derive(Debug, Clone, PartialEq)]
pub struct SigningKey {
    /// Its key id: the RFC 7638 thumbprint of the public key.
    pub kid: String,
    /// What it signs with.
    pub alg: SigningAlg,
    /// The public JWK as it is published.
    pub public_jwk: Json,
    /// When it was made.
    pub created_at: DateTime<Utc>,
    /// When it stopped signing, or `None` while it signs.
    pub retired_at: Option<DateTime<Utc>>,
}

/// What [`ensure_signing_keys`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyCheck {
    /// The key ids made now.
    pub created: Vec<String>,
    /// The live keys this instance could not open, retired now: a restore
    /// from an instance with another instance key.
    pub unreadable: Vec<String>,
}

/// Make sure there is a live key this instance can open for each algorithm,
/// making one where there is none. Cheap when there are: one read and an
/// AES-GCM open per live key.
pub async fn ensure_signing_keys(catalog: &Catalog) -> Result<KeyCheck> {
    let mut check = KeyCheck::default();
    let key = catalog.secret_key()?;
    let rows = stored(catalog).await?;
    for alg in SigningAlg::ALL {
        let mut usable = false;
        for (stored, sealed) in rows
            .iter()
            .filter(|(k, _)| k.alg == alg && k.retired_at.is_none())
        {
            if key.open(sealed).is_ok() {
                usable = true;
            } else {
                sc_log::log_warn!(
                    "the identity provider's {} signing key `{}` cannot be opened with this \
                     instance's key (it was sealed by another instance, as after restoring a \
                     backup without its key); it is retired and a new key is made",
                    alg.name(),
                    stored.kid
                );
                retire(catalog, Some(&stored.kid)).await?;
                check.unreadable.push(stored.kid.clone());
            }
        }
        if !usable {
            check.created.push(create_key(catalog, alg).await?);
        }
    }
    Ok(check)
}

/// Retire every live key and make new ones (module docs).
pub async fn rotate_signing_keys(catalog: &Catalog) -> Result<Vec<String>> {
    retire(catalog, None).await?;
    let mut made = Vec::new();
    for alg in SigningAlg::ALL {
        made.push(create_key(catalog, alg).await?);
    }
    Ok(made)
}

/// Every key, newest first, live and retired: what an admin sees.
pub async fn list_signing_keys(catalog: &Catalog) -> Result<Vec<SigningKey>> {
    let mut keys: Vec<SigningKey> = stored(catalog).await?.into_iter().map(|(k, _)| k).collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.created_at));
    Ok(keys)
}

/// The JWKS: `{"keys": [...]}` with every live key and every key retired in
/// the last [`RETIRED_KEY_PUBLISHED_HOURS`] — but only keys this instance
/// can open, so a key restored from another instance is never served.
pub async fn published_jwks(catalog: &Catalog) -> Result<Json> {
    let key = catalog.secret_key()?;
    let horizon = Utc::now() - Duration::hours(RETIRED_KEY_PUBLISHED_HOURS);
    let mut keys = Vec::new();
    for (stored, sealed) in stored(catalog).await? {
        let recent = stored.retired_at.is_none_or(|at| at > horizon);
        if recent && key.open(&sealed).is_ok() {
            keys.push(stored.public_jwk);
        }
    }
    Ok(serde_json::json!({ "keys": keys }))
}

/// Sign `claims` as a JWT with the newest live key for `alg`.
pub async fn sign_jwt(catalog: &Catalog, alg: SigningAlg, claims: &Json) -> Result<String> {
    let rows = stored(catalog).await?;
    let Some((stored, sealed)) = rows
        .iter()
        .filter(|(k, _)| k.alg == alg && k.retired_at.is_none())
        .max_by_key(|(k, _)| k.created_at)
    else {
        return Err(Error::invalid(format!(
            "the identity provider has no {} signing key yet",
            alg.name()
        )));
    };
    let pem = catalog.secret_key()?.open(sealed).map_err(|_| {
        Error::invalid(format!(
            "the identity provider's {} signing key `{}` cannot be opened with this \
             instance's key; rotate the signing keys to make new ones",
            alg.name(),
            stored.kid
        ))
    })?;
    let mut header = Header::new(alg.jose());
    header.kid = Some(stored.kid.clone());
    header.typ = Some("JWT".to_owned());
    jsonwebtoken::encode(&header, claims, &alg.encoding_key(&pem)?)
        .map_err(|e| Error::msg(format!("could not sign an {} token: {e}", alg.name())))
}

/// The left half of the SHA-256 of `token`, base64url: an `at_hash` or a
/// `c_hash` for an `id_token` signed with RS256 or ES256 (OIDC Core §3.1.3.6).
pub fn left_half_sha256(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let half = digest.get(..16).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(half)
}

/// Every signing key as a backup carries it: the public JWK and the private
/// half **still sealed** with this instance's key. The backup carries that key
/// too (`keys/secrets.key`), which is what lets a restore open them.
pub async fn export_signing_keys(catalog: &Catalog) -> Result<Vec<Json>> {
    Ok(stored(catalog)
        .await?
        .into_iter()
        .map(|(key, sealed)| {
            serde_json::json!({
                "kid": key.kid,
                "alg": key.alg.name(),
                "public_jwk": key.public_jwk,
                "private_key": sealed,
                "created_at": key.created_at.to_rfc3339(),
                "retired_at": key.retired_at.map(|at| at.to_rfc3339()),
            })
        })
        .collect())
}

/// What [`import_signing_keys`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyImport {
    /// The key ids restored.
    pub restored: Vec<String>,
    /// Already here, and kept as they are.
    pub kept: Vec<String>,
    /// Ones the backup's key could not open: not restored, and so never served.
    pub unreadable: Vec<String>,
}

/// Restore signing keys a backup carried. `open` opens a private half sealed
/// with the backup's key (`None` when it cannot); each is sealed again with
/// this instance's key. A key that is already here is kept, and one that will
/// not open is left out and named, rather than stored where it could never
/// sign.
pub async fn import_signing_keys(
    catalog: &Catalog,
    keys: &[Json],
    open: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<KeyImport> {
    let mut report = KeyImport::default();
    let here: Vec<String> = stored(catalog)
        .await?
        .into_iter()
        .map(|(key, _)| key.kid)
        .collect();
    let instance = catalog.secret_key()?;
    for item in keys {
        let text = |name: &str| item.get(name).and_then(Json::as_str);
        let (Some(kid), Some(alg), Some(sealed)) = (text("kid"), text("alg"), text("private_key"))
        else {
            return Err(Error::invalid(
                "a signing key in the backup is missing its kid, alg or private key",
            ));
        };
        let alg = SigningAlg::from_name(alg)
            .ok_or_else(|| Error::invalid(format!("signing key `{kid}`: unknown alg `{alg}`")))?;
        if here.iter().any(|k| k == kid) {
            report.kept.push(kid.to_owned());
            continue;
        }
        let Some(pem) = open(sealed) else {
            report.unreadable.push(kid.to_owned());
            continue;
        };
        // It must load as what it says it is before it is trusted to sign.
        alg.encoding_key(&pem)?;
        let when = |name: &str| {
            text(name)
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc))
        };
        let insert = Insert::row(
            OIDC_KEYS_TABLE,
            key_fields().iter().map(|f| f.base.name.clone()).collect(),
            vec![
                Expr::lit(kid),
                Expr::lit(alg.name()),
                Expr::Lit(Value::Json(
                    item.get("public_jwk").cloned().unwrap_or(Json::Null),
                )),
                Expr::lit(instance.seal(&pem)?),
                Expr::Lit(Value::Timestamp(
                    when("created_at").unwrap_or_else(Utc::now),
                )),
                Expr::Lit(when("retired_at").map_or(Value::Null, Value::Timestamp)),
            ],
        );
        run(catalog, Statement::from(insert)).await?;
        report.restored.push(kid.to_owned());
    }
    Ok(report)
}

/// Make one key for `alg`, store it sealed, and return its key id.
async fn create_key(catalog: &Catalog, alg: SigningAlg) -> Result<String> {
    let der = match alg {
        SigningAlg::Rs256 => aws_lc_rs::rsa::KeyPair::generate(KeySize::Rsa2048)
            .and_then(|pair| pair.as_der())
            .map(|der| der.as_ref().to_vec()),
        SigningAlg::Es256 => EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING)
            .and_then(|pair| pair.to_pkcs8v1())
            .map(|doc| doc.as_ref().to_vec()),
    }
    .map_err(|_| Error::msg(format!("could not make an {} signing key", alg.name())))?;
    let pem = pkcs8_pem(&der);
    let mut jwk = Jwk::from_encoding_key(&alg.encoding_key(&pem)?, alg.jose())
        .map_err(|e| Error::msg(format!("could not describe an {} key: {e}", alg.name())))?;
    let kid = jwk.thumbprint(ThumbprintHash::SHA256);
    jwk.common.key_id = Some(kid.clone());
    jwk.common.public_key_use = Some(PublicKeyUse::Signature);
    jwk.common.key_algorithm = Some(match alg {
        SigningAlg::Rs256 => KeyAlgorithm::RS256,
        SigningAlg::Es256 => KeyAlgorithm::ES256,
    });
    let public_jwk = serde_json::to_value(&jwk)
        .map_err(|e| Error::msg(format!("could not write an {} key: {e}", alg.name())))?;
    let sealed = catalog.secret_key()?.seal(&pem)?;
    let insert = Insert::row(
        OIDC_KEYS_TABLE,
        key_fields().iter().map(|f| f.base.name.clone()).collect(),
        vec![
            Expr::lit(kid.as_str()),
            Expr::lit(alg.name()),
            Expr::Lit(Value::Json(public_jwk)),
            Expr::lit(sealed),
            Expr::Lit(Value::Timestamp(Utc::now())),
            Expr::Lit(Value::Null),
        ],
    );
    run(catalog, Statement::from(insert)).await?;
    Ok(kid)
}

/// PKCS#8 DER as PEM: what `jsonwebtoken` reads a private key from.
fn pkcs8_pem(der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem
}

/// Retire one live key, or every live key.
async fn retire(catalog: &Catalog, kid: Option<&str>) -> Result<()> {
    let live = Expr::unary(UnOp::IsNull, Expr::col(COL_RETIRED_AT));
    let filter = match kid {
        Some(kid) => live.and(Expr::col(COL_KID).eq(Expr::lit(kid))),
        None => live,
    };
    let update = Update::new(
        OIDC_KEYS_TABLE,
        vec![Assignment::new(
            COL_RETIRED_AT.to_owned(),
            Expr::Lit(Value::Timestamp(Utc::now())),
        )],
    )
    .filter(filter);
    run(catalog, Statement::from(update)).await
}

/// Every row: the key, and its sealed private half.
async fn stored(catalog: &Catalog) -> Result<Vec<(SigningKey, String)>> {
    let select = Select::from(Source::table(OIDC_KEYS_TABLE));
    let found: Vec<Row> = catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    found.iter().map(key_from_row).collect()
}

fn key_from_row(row: &Row) -> Result<(SigningKey, String)> {
    let bad = |column: &str| Error::invalid(format!("{OIDC_KEYS_TABLE}.{column} is malformed"));
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        _ => Err(bad(column)),
    };
    let alg = text(COL_ALG)?;
    Ok((
        SigningKey {
            kid: text(COL_KID)?,
            alg: SigningAlg::from_name(&alg).ok_or_else(|| bad(COL_ALG))?,
            public_jwk: match row.get(COL_PUBLIC_JWK) {
                Some(Value::Json(j)) => j.clone(),
                _ => return Err(bad(COL_PUBLIC_JWK)),
            },
            created_at: match row.get(COL_CREATED_AT) {
                Some(Value::Timestamp(t)) => *t,
                _ => return Err(bad(COL_CREATED_AT)),
            },
            retired_at: match row.get(COL_RETIRED_AT) {
                Some(Value::Timestamp(t)) => Some(*t),
                Some(Value::Null) | None => None,
                _ => return Err(bad(COL_RETIRED_AT)),
            },
        },
        text(COL_PRIVATE_KEY)?,
    ))
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    let _: Vec<Row> = catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_hash_is_the_left_half_of_sha_256() {
        // SHA-256("") starts e3b0c44298fc1c149afbf4c8996fb924.
        assert_eq!(left_half_sha256(""), "47DEQpj8HBSa-_TImW-5JA");
    }

    #[test]
    fn the_algorithms_round_trip_their_names() {
        for alg in SigningAlg::ALL {
            assert_eq!(SigningAlg::from_name(alg.name()), Some(alg));
        }
        assert_eq!(SigningAlg::from_name("HS256"), None);
        assert_eq!(SigningAlg::from_name("none"), None);
    }
}
