//! Password hashing with argon2id (technical design §7.1).
//!
//! This module owns the *write* side of credentials — turning a plaintext
//! password into a stored hash — which the create-first-user flow needs to
//! populate [`password_hash`](super::COL_PASSWORD_HASH). Verification (the read
//! side, used at login) lands with the login flow in a later Phase 5 item.
//!
//! Hashes are stored as PHC strings (`$argon2id$v=19$m=…,t=…,p=…$salt$hash`),
//! which embed the algorithm, version, parameters, and a random per-password
//! salt, so a stored hash is self-describing and future parameter changes stay
//! backward-compatible.
//!
//! **Verification decides the scheme from the stored hash itself**
//! ([`HashScheme::of`]): argon2id is what this system writes, and bcrypt
//! (`$2a$…`, `$2b$…`, `$2y$…`) is what Saltcorn 1 wrote — so an account imported
//! from a v1 backup signs in with the password it always had. Nothing new is ever
//! hashed with bcrypt; a login that verifies against one is told to replace it
//! ([`needs_rehash`]), so imported hashes become argon2id one sign-in at a time.

use argon2::Argon2;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use sc_error::{Error, Result};

/// How many characters [`random_password`] produces.
///
/// Twenty characters of the alphabet below is a little over 116 bits, which is
/// past the point where the argon2 cost matters: this password is never guessed,
/// it is stolen or it is not.
pub const RANDOM_PASSWORD_LENGTH: usize = 20;

/// The characters a generated password is drawn from: ASCII letters and digits,
/// minus the four that a human reading one out or retyping it confuses —
/// `l`/`I`, `O`/`0`. A generated password is *transcribed* (into a chat message,
/// out of one), so the alphabet is chosen for that rather than for maximum
/// entropy per character; the length covers the difference.
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ123456789";

/// A fresh random password, for an admin creating a user without choosing one or
/// resetting somebody out of their account (§7.1).
///
/// Drawn from the operating system's CSPRNG — the same source the argon2 salt
/// comes from — with **rejection sampling** rather than `byte % len`, since the
/// alphabet's length does not divide 256 and a modulo would quietly make the
/// first few characters likelier than the rest.
pub fn random_password() -> String {
    let len = ALPHABET.len();
    // The largest multiple of the alphabet that fits in a byte; anything at or
    // above it is redrawn rather than folded.
    let limit = (256 / len * len) as u32;
    let mut out = String::with_capacity(RANDOM_PASSWORD_LENGTH);
    let mut buffer = [0u8; RANDOM_PASSWORD_LENGTH];
    while out.len() < RANDOM_PASSWORD_LENGTH {
        OsRng.fill_bytes(&mut buffer);
        for byte in buffer {
            if u32::from(byte) >= limit {
                continue;
            }
            out.push(ALPHABET[usize::from(byte) % len] as char);
            if out.len() == RANDOM_PASSWORD_LENGTH {
                break;
            }
        }
    }
    out
}

/// Hash a plaintext password with argon2id (a fresh random salt each call) and
/// return the PHC string to store in the `password_hash` column.
///
/// [`Argon2::default`] is argon2id at version 0x13 with the crate's recommended
/// default parameters.
pub fn hash_password(plaintext: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| Error::auth(format!("hashing password failed: {e}")))?;
    Ok(hash.to_string())
}

/// The algorithm a stored password hash was made with, read off the hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashScheme {
    /// A PHC string — argon2id, what [`hash_password`] writes.
    Phc,
    /// A modular-crypt bcrypt hash, `$2a$`/`$2b$`/`$2x$`/`$2y$` — Saltcorn 1's.
    Bcrypt,
}

impl HashScheme {
    /// The scheme `hash` is in, or `None` for a string that is neither.
    pub fn of(hash: &str) -> Option<HashScheme> {
        if is_bcrypt(hash) {
            Some(HashScheme::Bcrypt)
        } else if PasswordHash::new(hash).is_ok() {
            Some(HashScheme::Phc)
        } else {
            None
        }
    }
}

/// `$2?$NN$` and 53 characters of bcrypt's base64 (22 of salt, 31 of hash):
/// the one shape every bcrypt implementation writes.
fn is_bcrypt(hash: &str) -> bool {
    let bytes = hash.as_bytes();
    bytes.len() == 60
        && ["$2a$", "$2b$", "$2x$", "$2y$"]
            .iter()
            .any(|prefix| hash.starts_with(prefix))
        && bytes[4..6].iter().all(u8::is_ascii_digit)
        && bytes[6] == b'$'
        && bytes[7..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'/')
}

/// Whether `hash` is a well-formed stored password hash in a scheme this
/// system can verify. Used to reject obviously-corrupt stored hashes before
/// they reach verification.
pub fn is_valid_hash(hash: &str) -> bool {
    HashScheme::of(hash).is_some()
}

/// Whether a hash that just verified should be replaced by a fresh
/// [`hash_password`] of the same password — true for every scheme but the one
/// this system writes.
pub fn needs_rehash(hash: &str) -> bool {
    HashScheme::of(hash) != Some(HashScheme::Phc)
}

/// Verify a plaintext password against a stored hash, in whichever scheme the
/// hash says it is ([`HashScheme::of`]).
///
/// Returns `Ok(true)` on a match and `Ok(false)` on a mismatch — the ordinary
/// "wrong password" outcome, not an error. Only a genuinely broken stored hash
/// or hasher failure yields [`Err`].
pub fn verify_password(hash: &str, plaintext: &str) -> Result<bool> {
    if HashScheme::of(hash) == Some(HashScheme::Bcrypt) {
        // bcrypt reads at most 72 bytes of the password, as Saltcorn 1's
        // bcryptjs did when it made the hash; truncating is what verifies it.
        return bcrypt::verify(plaintext, hash)
            .map_err(|e| Error::auth(format!("invalid stored password hash: {e}")));
    }
    let parsed = PasswordHash::new(hash)
        .map_err(|e| Error::auth(format!("invalid stored password hash: {e}")))?;
    match Argon2::default().verify_password(plaintext.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(Error::auth(format!("verifying password failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_argon2id_phc_and_parseable() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"), "got {hash}");
        assert!(is_valid_hash(&hash));
    }

    #[test]
    fn hashing_uses_a_random_salt() {
        // The same password hashed twice yields different strings (distinct salts).
        let a = hash_password("hunter2").unwrap();
        let b = hash_password("hunter2").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn garbage_is_not_a_valid_hash() {
        assert!(!is_valid_hash("not-a-hash"));
        assert!(!is_valid_hash(""));
    }

    #[test]
    fn verify_accepts_the_right_password_and_rejects_others() {
        let hash = hash_password("s3cret-pw").unwrap();
        assert!(verify_password(&hash, "s3cret-pw").unwrap());
        assert!(!verify_password(&hash, "wrong").unwrap());
        assert!(!verify_password(&hash, "").unwrap());
    }

    /// Saltcorn 1 hashed with bcryptjs, which writes `$2a$`. This is the
    /// OpenBSD test vector for `U*U`, in that spelling.
    const V1_BCRYPT: &str = "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW";

    #[test]
    fn a_saltcorn_1_bcrypt_hash_verifies_and_asks_to_be_replaced() {
        assert_eq!(HashScheme::of(V1_BCRYPT), Some(HashScheme::Bcrypt));
        assert!(is_valid_hash(V1_BCRYPT));
        assert!(verify_password(V1_BCRYPT, "U*U").unwrap());
        assert!(!verify_password(V1_BCRYPT, "U*V").unwrap());
        assert!(needs_rehash(V1_BCRYPT));

        let ours = hash_password("U*U").unwrap();
        assert_eq!(HashScheme::of(&ours), Some(HashScheme::Phc));
        assert!(!needs_rehash(&ours));
        assert_eq!(HashScheme::of("not-a-hash"), None);
        assert_eq!(HashScheme::of("$2a$10$notabcrypthash"), None);
    }

    #[test]
    fn verify_errors_on_a_broken_stored_hash() {
        assert!(verify_password("$2a$05$tooshort", "whatever").is_err());
        assert!(verify_password("not-a-hash", "whatever").is_err());
    }

    #[test]
    fn a_generated_password_is_fresh_transcribable_and_usable() {
        let pw = random_password();
        assert_eq!(pw.len(), RANDOM_PASSWORD_LENGTH);
        assert_ne!(pw, random_password());
        // Nothing outside the alphabet, and in particular none of the four
        // characters a human reading it out would get wrong.
        assert!(pw.bytes().all(|b| ALPHABET.contains(&b)), "got {pw}");
        assert!(!pw.contains(['l', 'I', 'O', '0']), "got {pw}");
        // And it is a password like any other: it round-trips through argon2.
        let hash = hash_password(&pw).unwrap();
        assert!(verify_password(&hash, &pw).unwrap());
    }
}
