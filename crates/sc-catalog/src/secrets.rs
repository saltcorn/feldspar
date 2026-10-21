//! The instance key, and the envelope a [`Secret`](crate::DataFieldKind::Secret)
//! field's value is stored in (TODO-imap M2).
//!
//! A Secret field is a `Text` column whose every non-null value is an
//! **envelope**: `v1:<nonce>:<ciphertext>`, both parts standard base64, sealed
//! with AES-256-GCM under the instance's one key. The column is text on both
//! backends, so Postgres and SQLite need nothing of their own.
//!
//! The key is 32 random bytes in `<data dir>/secrets.key` ([`secret_key_path`]),
//! created with mode 0600 the first time a value is sealed. It is kept on disk
//! beside the rest of the data directory, not in the database, so a copy of the
//! database alone — a dump, a replica — holds nothing it can decrypt. A backup
//! is the one thing that carries both, on purpose: restoring one after a
//! disaster must not need every password typed in again.
//!
//! The envelope names no table or field (no associated data), so a table or
//! field rename leaves every value readable. Moving a ciphertext from one row
//! into another is possible for whoever can write the database directly, and
//! such a writer could as well write a value of their own; it is not a threat
//! this layer can answer.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sc_error::{Error, Result};
use sc_query::{Expr, Projection, Select, Source, Statement, UnOp, Value};
use sc_types::SECRET_SENTINEL;
use serde_json::Value as Json;
use std::collections::BTreeMap;

use crate::{Catalog, DataField, Table};

/// The key file's name inside the data directory.
pub const SECRET_KEY_FILE: &str = "secrets.key";

/// What every envelope starts with: the format's version.
pub const ENVELOPE_PREFIX: &str = "v1:";

/// The length of the key, in bytes (AES-256).
const KEY_LEN: usize = 32;
/// The length of an AES-GCM nonce, in bytes.
const NONCE_LEN: usize = 12;

/// The instance key.
///
/// Its `Debug` prints nothing of the key, so a struct holding one can derive
/// `Debug` without a log line ever carrying it.
#[derive(Clone)]
pub struct SecretKey {
    bytes: [u8; KEY_LEN],
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(…)")
    }
}

impl PartialEq for SecretKey {
    fn eq(&self, other: &SecretKey) -> bool {
        self.bytes == other.bytes
    }
}

impl SecretKey {
    /// A new random key.
    pub fn generate() -> Result<SecretKey> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes)
            .map_err(|e| Error::msg(format!("cannot draw a random key: {e}")))?;
        Ok(SecretKey { bytes })
    }

    /// A key from its 32 bytes — what the key file and a backup's
    /// `keys/secrets.key` entry hold.
    pub fn from_bytes(bytes: &[u8]) -> Result<SecretKey> {
        let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| {
            Error::invalid(format!(
                "a secrets key is {KEY_LEN} bytes, and this one is {}",
                bytes.len()
            ))
        })?;
        Ok(SecretKey { bytes })
    }

    /// The key's bytes, for writing it into a backup.
    pub fn to_bytes(&self) -> [u8; KEY_LEN] {
        self.bytes
    }

    /// Seal `plaintext` into an envelope. Every call draws a fresh nonce, so
    /// the same value sealed twice gives two different envelopes.
    pub fn seal(&self, plaintext: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce)
            .map_err(|e| Error::msg(format!("cannot draw a random nonce: {e}")))?;
        let ciphertext = self
            .cipher()
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| Error::msg("a secret value could not be encrypted"))?;
        Ok(format!(
            "{ENVELOPE_PREFIX}{}:{}",
            STANDARD.encode(nonce),
            STANDARD.encode(ciphertext)
        ))
    }

    /// Open an envelope [`seal`](SecretKey::seal) made with this key.
    ///
    /// The error never quotes the envelope or anything decrypted: it says only
    /// what kind of failure it was.
    pub fn open(&self, envelope: &str) -> Result<String> {
        let rest = envelope
            .strip_prefix(ENVELOPE_PREFIX)
            .ok_or_else(|| Error::invalid("a secret value is not an encrypted envelope"))?;
        let (nonce, ciphertext) = rest
            .split_once(':')
            .ok_or_else(|| Error::invalid("a secret value's envelope is damaged"))?;
        let nonce = STANDARD
            .decode(nonce)
            .ok()
            .filter(|n| n.len() == NONCE_LEN)
            .ok_or_else(|| Error::invalid("a secret value's envelope is damaged"))?;
        let ciphertext = STANDARD
            .decode(ciphertext)
            .map_err(|_| Error::invalid("a secret value's envelope is damaged"))?;
        let plaintext = self
            .cipher()
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_slice())
            .map_err(|_| {
                Error::invalid(
                    "a secret value cannot be decrypted with this instance's key: it was \
                     encrypted with another one, or it is damaged",
                )
            })?;
        String::from_utf8(plaintext)
            .map_err(|_| Error::invalid("a secret value decrypted to something that is not text"))
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(&self.bytes.into())
    }

    /// Read the key at `path`, or `None` when there is no file there.
    pub fn load(path: &Path) -> Result<Option<SecretKey>> {
        match std::fs::read(path) {
            Ok(bytes) => SecretKey::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| Error::config(format!("`{}`: {}", path.display(), e.causes()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::config(format!(
                "cannot read the secrets key `{}`: {e}",
                path.display()
            ))),
        }
    }

    /// Read the key at `path`, creating it (and its directory) when there is
    /// none.
    ///
    /// The new key is written to a temporary file with mode 0600 and then
    /// hard-linked into place, which fails when another process got there
    /// first — and then that process's key is the one read back, so two
    /// servers starting at once never end up with different keys.
    pub fn load_or_create(path: &Path) -> Result<SecretKey> {
        if let Some(key) = SecretKey::load(path)? {
            return Ok(key);
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::config(format!(
                "cannot create `{}` for the secrets key: {e}",
                dir.display()
            ))
        })?;
        let key = SecretKey::generate()?;
        let temp = dir.join(format!(
            ".{SECRET_KEY_FILE}.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let written =
            write_private(&temp, &key.bytes).and_then(|()| match std::fs::hard_link(&temp, path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e),
            });
        let _ = std::fs::remove_file(&temp);
        written.map_err(|e| {
            Error::config(format!(
                "cannot write the secrets key `{}`: {e}",
                path.display()
            ))
        })?;
        // Whichever key won the race is the one that is there now.
        SecretKey::load(path)?.ok_or_else(|| {
            Error::config(format!(
                "the secrets key `{}` vanished as it was written",
                path.display()
            ))
        })
    }
}

/// Write `bytes` to a new file at `path` that only its owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Whether `value` is spelt as an envelope (it says nothing of whether it opens).
pub fn is_envelope(value: &str) -> bool {
    value.starts_with(ENVELOPE_PREFIX)
}

/// Where this instance's key lives: `<data dir>/secrets.key`.
pub fn secret_key_path() -> Result<PathBuf> {
    Ok(sc_files::data_dir()?.join(SECRET_KEY_FILE))
}

// --- a table's secret values -------------------------------------------------
//
// The row layer stores and reads envelopes and nothing else: a row read from a
// table holds its secrets as the ciphertext the column holds. That is the
// fail-safe direction — a surface that forgets to treat them shows a client an
// envelope, which is useless without the key, never the plaintext. The
// plaintext exists only where server-side code asks for it
// ([`reveal_values`], [`reveal_json`]), and a client surface shows the
// redaction sentinel instead ([`redact_values`], [`redact_json`]).

/// Whether `table` has a secret field at all — the fast path every helper
/// below takes first.
pub fn has_secrets(table: &Table) -> bool {
    table.fields.iter().any(DataField::is_secret)
}

/// Whether `column` of `table` is a secret.
pub fn is_secret_column(table: &Table, column: &str) -> bool {
    table.field(column).is_some_and(DataField::is_secret)
}

/// What a write stores in `column` for `value`, or `None` when the column is
/// to be left out of the write.
///
/// Not a secret column: `value` as it is. A secret column:
///
/// - `null` stays `null`;
/// - the [`SECRET_SENTINEL`] is left out, so a client that sends back what it
///   was shown keeps the stored value (and an insert leaves the column empty);
/// - an envelope that opens with this instance's key is kept as it is, so
///   server-side code that writes back a row it never revealed does not seal
///   the ciphertext a second time;
/// - any other text is sealed.
pub fn seal_for_write(
    catalog: &Catalog,
    table: &Table,
    column: &str,
    value: Value,
) -> Result<Option<Value>> {
    if !is_secret_column(table, column) {
        return Ok(Some(value));
    }
    match value {
        Value::Null => Ok(Some(Value::Null)),
        Value::Text(text) if text == SECRET_SENTINEL => Ok(None),
        Value::Text(text) => {
            let key = catalog.secret_key()?;
            if is_envelope(&text) && key.open(&text).is_ok() {
                return Ok(Some(Value::Text(text)));
            }
            Ok(Some(Value::Text(key.seal(&text)?)))
        }
        other => Err(Error::invalid(format!(
            "`{}.{column}` is a secret, which holds text, not {}",
            table.name,
            other.kind()
        ))),
    }
}

/// A secret's value as a client sees it: `null` for none, the sentinel for
/// any value at all.
pub fn redacted(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        _ => Value::Text(SECRET_SENTINEL.to_owned()),
    }
}

/// Replace every secret of `table` in `values` with what a client sees.
pub fn redact_values(table: &Table, values: &mut BTreeMap<String, Value>) {
    if !has_secrets(table) {
        return;
    }
    for (name, value) in values.iter_mut() {
        if is_secret_column(table, name) {
            *value = redacted(value);
        }
    }
}

/// [`redact_values`] for a row as JSON, or for every row of an array of them.
/// Anything else is left alone.
pub fn redact_json(table: &Table, row: &mut Json) {
    if !has_secrets(table) {
        return;
    }
    if let Json::Array(rows) = row {
        for row in rows {
            redact_json(table, row);
        }
        return;
    }
    let Json::Object(obj) = row else {
        return;
    };
    for (name, value) in obj.iter_mut() {
        if is_secret_column(table, name) && !value.is_null() {
            *value = Json::String(SECRET_SENTINEL.to_owned());
        }
    }
}

/// Decrypt every secret of `table` in `values`, for server-side code.
///
/// A value that is not an envelope, or that does not open with this
/// instance's key, is an error naming the field — never the value.
pub fn reveal_values(
    catalog: &Catalog,
    table: &Table,
    values: &mut BTreeMap<String, Value>,
) -> Result<()> {
    if !has_secrets(table) {
        return Ok(());
    }
    for (name, value) in values.iter_mut() {
        if !is_secret_column(table, name) {
            continue;
        }
        if let Value::Text(envelope) = value {
            *value = Value::Text(open_field(catalog, table, name, envelope)?);
        }
    }
    Ok(())
}

/// [`reveal_values`] for a row as JSON. Anything but an object is left alone.
pub fn reveal_json(catalog: &Catalog, table: &Table, row: &mut Json) -> Result<()> {
    if !has_secrets(table) {
        return Ok(());
    }
    let Json::Object(obj) = row else {
        return Ok(());
    };
    for (name, value) in obj.iter_mut() {
        if !is_secret_column(table, name) {
            continue;
        }
        if let Json::String(envelope) = value {
            *value = Json::String(open_field(catalog, table, name, envelope)?);
        }
    }
    Ok(())
}

/// Open one stored value of `table.column`, naming the field when it fails.
fn open_field(catalog: &Catalog, table: &Table, column: &str, envelope: &str) -> Result<String> {
    if envelope == SECRET_SENTINEL {
        // Already what a client sees: a row that went through a redaction and
        // came back. There is nothing under it to reveal.
        return Ok(envelope.to_owned());
    }
    catalog
        .secret_key()?
        .open(envelope)
        .map_err(|e| Error::invalid(format!("`{}.{column}`: {}", table.name, e.causes())))
}

/// Refuse to start when this instance holds Secret values and the key that
/// opens them is missing (TODO-imap M2.2).
///
/// A missing key with no values to open is no problem: the key is created the
/// first time one is written. But a missing key with values is a data
/// directory that was lost or moved, and creating a new key then would make
/// every stored secret unreadable for good — and every write after it would
/// seal under a key the old values never knew. So the server stops instead,
/// and says which file to put back.
pub async fn check_key_on_start(catalog: &Catalog) -> Result<()> {
    if catalog.has_secret_key() {
        return Ok(());
    }
    check_key_at(catalog, &secret_key_path()?).await
}

/// [`check_key_on_start`] against the key file at `path`.
pub async fn check_key_at(catalog: &Catalog, path: &Path) -> Result<()> {
    if SecretKey::load(path)?.is_some() {
        return Ok(());
    }
    let mut holding = Vec::new();
    for table in catalog.tables()? {
        let secrets: Vec<&DataField> = table.fields.iter().filter(|f| f.is_secret()).collect();
        let Some(first) = secrets.first() else {
            continue;
        };
        let held = secrets.iter().skip(1).fold(
            Expr::unary(UnOp::IsNotNull, Expr::col(first.base.name.clone())),
            |any, f| any.or(Expr::unary(UnOp::IsNotNull, Expr::col(f.base.name.clone()))),
        );
        let mut select = Select::from(Source::table(table.name.clone()))
            .columns(vec![Projection::expr(Expr::col(first.base.name.clone()))])
            .filter(held);
        select.limit = Some(1);
        let rows: Vec<sc_db::Row> = catalog
            .driver_for(&table)?
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await?;
        if !rows.is_empty() {
            holding.push(format!("`{}`", table.name));
        }
    }
    if holding.is_empty() {
        return Ok(());
    }
    Err(Error::config(format!(
        "the secrets key `{}` is missing, and {} hold encrypted values that only it can \
         open. Restore the file from the data directory this server ran with (or from a \
         backup's `keys/secrets.key`) before starting; a new key would leave those values \
         unreadable",
        path.display(),
        holding.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_opens_with_its_key_and_only_with_its_key() {
        let key = SecretKey::generate().unwrap();
        let envelope = key.seal("Grüße-€-pässwort").unwrap();
        assert!(is_envelope(&envelope));
        assert!(!envelope.contains("pässwort"));
        assert_eq!(key.open(&envelope).unwrap(), "Grüße-€-pässwort");
        // A fresh nonce each time.
        assert_ne!(key.seal("Grüße-€-pässwort").unwrap(), envelope);

        let other = SecretKey::generate().unwrap();
        let message = other.open(&envelope).unwrap_err().causes();
        assert!(message.contains("another one"), "{message}");
        assert!(!message.contains(&envelope));
        assert!(key.open("plain").is_err());
        assert!(key.open("v1:AAAA:BBBB").is_err());
    }

    #[test]
    fn the_key_file_is_created_once_private_and_read_back() {
        let dir = std::env::temp_dir().join(format!("sc-secrets-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join(SECRET_KEY_FILE);
        assert_eq!(SecretKey::load(&path).unwrap(), None);
        let first = SecretKey::load_or_create(&path).unwrap();
        let again = SecretKey::load_or_create(&path).unwrap();
        assert_eq!(first, again, "a second call reads the key the first wrote");
        assert_eq!(std::fs::read(&path).unwrap().len(), KEY_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing but the key is left in the directory.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(SECRET_KEY_FILE)]);

        std::fs::write(&path, b"short").unwrap();
        let message = SecretKey::load(&path).unwrap_err().causes();
        assert!(message.contains("32 bytes"), "{message}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_never_prints() {
        let key = SecretKey::from_bytes(&[7u8; 32]).unwrap();
        assert_eq!(format!("{key:?}"), "SecretKey(…)");
    }
}
