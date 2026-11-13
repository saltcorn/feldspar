//! The second factor's secrets as types (security review 2026-11, §2 and B5;
//! TODO-type-hardening H2.5).
//!
//! [`TotpSecret`], [`RecoveryCode`] and [`ChallengeToken`] each wrap the value
//! they hold so that it cannot be printed by accident (`Debug` is redacted and
//! there is no `Display`), it is compared in constant time, and it is read
//! only through an explicit `expose…` at the one place that must send it.
//!
//! A [`RecoveryCode`] is 18 symbols from a 31-letter alphabet, drawn by
//! rejection sampling so every symbol is equally likely: 89 bits, past the
//! 80 the review asks for (the codes before were ten symbols, about 49 bits
//! with a bias). It is stored as an HMAC-SHA-256 keyed with a key derived from
//! the instance's [`SecretKey`] ([`RecoveryCodeHash`]), so a database dump
//! without the key file is no help in finding one. Codes made before this keep
//! their unkeyed hash until they are spent or replaced (`spend` checks both
//! forms), since a hash cannot be re-keyed without the code it hides.

use std::fmt;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use sc_catalog::SecretKey;
use sc_error::{Error, Result};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Whether `a` and `b` are the same bytes, in time that depends only on their
/// lengths.
pub(crate) fn bytes_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// --- the authenticator secret -------------------------------------------------

/// The length of a secret, in bytes: 160 bits, as RFC 4226 recommends.
pub(crate) const SECRET_BYTES: usize = 20;

/// An authenticator app's shared secret (module docs).
pub struct TotpSecret(Vec<u8>);

impl TotpSecret {
    /// A new random secret.
    pub(crate) fn generate() -> TotpSecret {
        let mut bytes = vec![0u8; SECRET_BYTES];
        OsRng.fill_bytes(&mut bytes);
        TotpSecret(bytes)
    }

    /// A secret written in base32, read leniently: case, spaces and padding are
    /// what a person copying one by hand gets wrong.
    pub fn from_base32(secret: &str) -> Result<TotpSecret> {
        let cleaned: String = secret
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '=')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        BASE32_NOPAD
            .decode(cleaned.as_bytes())
            .map(TotpSecret)
            .map_err(|_| Error::invalid("a two-factor secret is not valid base32"))
    }

    /// The secret in base32: what the app is given in its QR code, and what
    /// is sealed at rest. The one way to read it.
    pub fn expose_base32(&self) -> String {
        BASE32_NOPAD.encode(&self.0)
    }

    /// The raw bytes, for computing a code.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for TotpSecret {
    fn eq(&self, other: &TotpSecret) -> bool {
        bytes_match(&self.0, &other.0)
    }
}

impl Eq for TotpSecret {}

impl fmt::Debug for TotpSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TotpSecret([redacted])")
    }
}

// --- recovery codes ------------------------------------------------------------

/// The letters a recovery code is written in: no `0`/`o`, `1`/`l`/`i`, so a
/// code copied by hand off paper is copied right. 31 of them.
const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// Symbols in a new recovery code: log2(31^18) is about 89 bits.
pub(crate) const RECOVERY_CODE_SYMBOLS: usize = 18;
/// Symbols per dash-separated group, as a code is shown.
const RECOVERY_GROUP: usize = 6;
/// Symbols in a code made before codes were lengthened (and hashed unkeyed).
const LEGACY_RECOVERY_CODE_SYMBOLS: usize = 10;
/// What a keyed recovery-code hash starts with, which an unkeyed one does not.
const KEYED_PREFIX: &str = "k1:";

/// One single-use recovery code (module docs).
pub struct RecoveryCode(String);

impl RecoveryCode {
    /// A new code: [`RECOVERY_CODE_SYMBOLS`] symbols drawn without bias,
    /// shown as `xxxxxx-xxxxxx-xxxxxx`.
    pub(crate) fn generate() -> RecoveryCode {
        // The largest multiple of the alphabet's size that fits in a byte: a
        // byte at or past it is drawn again, so every symbol is as likely.
        let alphabet = RECOVERY_ALPHABET.len();
        let limit = 256 - (256 % alphabet);
        let mut symbols = String::with_capacity(RECOVERY_CODE_SYMBOLS + 2);
        let mut drawn = 0;
        let mut byte = [0u8; 1];
        while drawn < RECOVERY_CODE_SYMBOLS {
            OsRng.fill_bytes(&mut byte);
            let value = usize::from(byte[0]);
            if value >= limit {
                continue;
            }
            if drawn > 0 && drawn % RECOVERY_GROUP == 0 {
                symbols.push('-');
            }
            if let Some(symbol) = RECOVERY_ALPHABET.get(value % alphabet) {
                symbols.push(char::from(*symbol));
            }
            drawn += 1;
        }
        RecoveryCode(symbols)
    }

    /// A code as somebody typed it.
    pub fn presented(code: &str) -> RecoveryCode {
        RecoveryCode(code.to_owned())
    }

    /// The code, as it is shown the one time it is made.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The code in the form it is hashed in: lower case, letters and digits
    /// only.
    pub(crate) fn normalised(&self) -> String {
        self.0
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }

    /// Whether this could be a code made before codes were lengthened.
    pub(crate) fn could_be_legacy(&self) -> bool {
        self.normalised().len() == LEGACY_RECOVERY_CODE_SYMBOLS
    }
}

impl PartialEq for RecoveryCode {
    fn eq(&self, other: &RecoveryCode) -> bool {
        bytes_match(self.normalised().as_bytes(), other.normalised().as_bytes())
    }
}

impl Eq for RecoveryCode {}

impl fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryCode([redacted])")
    }
}

/// What is stored for a recovery code: the only way to make the value of the
/// `code_hash` column (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryCodeHash(String);

impl RecoveryCodeHash {
    /// The keyed hash of `code` for `user`: HMAC-SHA-256 under a key derived
    /// from the instance key, over the account and the normalised code.
    pub(crate) fn new(
        key: &SecretKey,
        user: Uuid,
        code: &RecoveryCode,
    ) -> Result<RecoveryCodeHash> {
        let mut derive = <Hmac<Sha256> as Mac>::new_from_slice(&key.to_bytes())
            .map_err(|_| Error::msg("the instance key cannot key an HMAC"))?;
        derive.update(b"feldspar recovery codes v1");
        let subkey = derive.finalize().into_bytes();
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&subkey)
            .map_err(|_| Error::msg("a derived key cannot key an HMAC"))?;
        mac.update(user.as_bytes());
        mac.update(code.normalised().as_bytes());
        Ok(RecoveryCodeHash(format!(
            "{KEYED_PREFIX}{}",
            hex(&mac.finalize().into_bytes())
        )))
    }

    /// The unkeyed SHA-256 a code made before keyed hashes was stored under,
    /// so it can still be spent once.
    pub(crate) fn legacy(user: Uuid, code: &RecoveryCode) -> RecoveryCodeHash {
        let mut hasher = Sha256::new();
        hasher.update(user.as_bytes());
        hasher.update(code.normalised().as_bytes());
        RecoveryCodeHash(hex(&hasher.finalize()))
    }

    /// The column value.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

// --- the challenge -------------------------------------------------------------

/// The token a sign-in is given in place of a session while it waits for its
/// second factor (module docs). Stored only as a hash.
pub struct ChallengeToken(String);

impl ChallengeToken {
    /// A token as a request presented it.
    pub fn presented(token: impl Into<String>) -> ChallengeToken {
        ChallengeToken(token.into())
    }

    /// The token, for the cookie and the JSON it is handed out in.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl PartialEq for ChallengeToken {
    fn eq(&self, other: &ChallengeToken) -> bool {
        bytes_match(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl Eq for ChallengeToken {}

impl fmt::Debug for ChallengeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChallengeToken([redacted])")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_recovery_code_has_at_least_80_bits() {
        let code = RecoveryCode::generate();
        let normalised = code.normalised();
        assert_eq!(normalised.len(), RECOVERY_CODE_SYMBOLS);
        let bits = (RECOVERY_CODE_SYMBOLS as f64) * (RECOVERY_ALPHABET.len() as f64).log2();
        assert!(bits >= 80.0, "{bits}");
        assert_eq!(code.expose().len(), RECOVERY_CODE_SYMBOLS + 2);
        assert_eq!(code.expose().matches('-').count(), 2);
        assert!(normalised.bytes().all(|b| RECOVERY_ALPHABET.contains(&b)));
    }

    #[test]
    fn every_symbol_is_drawn() {
        // 400 codes are 7200 draws: a symbol the sampler could not reach would
        // be missing.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..400 {
            seen.extend(RecoveryCode::generate().normalised().into_bytes());
        }
        assert_eq!(seen.len(), RECOVERY_ALPHABET.len());
    }

    #[test]
    fn the_keyed_hash_needs_the_key() {
        let user = Uuid::new_v4();
        let code = RecoveryCode::generate();
        let key = SecretKey::generate().unwrap();
        let other = SecretKey::generate().unwrap();
        let typed = RecoveryCode::presented(&format!(" {} ", code.expose().to_uppercase()));
        assert_eq!(
            RecoveryCodeHash::new(&key, user, &code).unwrap(),
            RecoveryCodeHash::new(&key, user, &typed).unwrap(),
            "however it is typed"
        );
        assert_ne!(
            RecoveryCodeHash::new(&key, user, &code).unwrap(),
            RecoveryCodeHash::new(&other, user, &code).unwrap(),
        );
        assert_ne!(
            RecoveryCodeHash::new(&key, user, &code).unwrap(),
            RecoveryCodeHash::new(&key, Uuid::new_v4(), &code).unwrap(),
            "another account's row"
        );
        assert!(
            RecoveryCodeHash::new(&key, user, &code)
                .unwrap()
                .as_str()
                .starts_with(KEYED_PREFIX)
        );
        assert!(
            !RecoveryCodeHash::legacy(user, &code)
                .as_str()
                .starts_with(KEYED_PREFIX)
        );
    }

    #[test]
    fn secrets_do_not_print() {
        let secret = TotpSecret::generate();
        let code = RecoveryCode::generate();
        let token = ChallengeToken::presented("abc123");
        let shown = format!("{secret:?} {code:?} {token:?}");
        assert!(!shown.contains(&secret.expose_base32()), "{shown}");
        assert!(!shown.contains(code.expose()), "{shown}");
        assert!(!shown.contains("abc123"), "{shown}");
    }

    #[test]
    fn equality_is_by_value() {
        let secret = TotpSecret::generate();
        assert_eq!(
            TotpSecret::from_base32(&secret.expose_base32().to_lowercase()).unwrap(),
            secret
        );
        assert_ne!(TotpSecret::generate(), secret);
        assert_eq!(
            ChallengeToken::presented("a"),
            ChallengeToken::presented("a")
        );
        assert_ne!(
            ChallengeToken::presented("a"),
            ChallengeToken::presented("ab")
        );
        assert!(bytes_match(b"", b""));
        assert!(!bytes_match(b"x", b"y"));
    }
}
