//! The host half of a code body's `crypto` global (TODO F1.2, F1.3; design
//! §10.1 "Globals").
//!
//! A body used to hand-write SHA-256 and base64 in JavaScript because the
//! isolate has no `crypto`. These ops are what `SETUP`'s `crypto` object calls:
//!
//! - `op_sc_random` fills a buffer from the operating system's entropy, for
//!   `crypto.getRandomValues` and `crypto.randomUUID`;
//! - `op_sc_digest`, `op_sc_hmac` and `op_sc_hmac_verify` are SHA-2 and HMAC
//!   over the same `sha2` the session tokens use. They are **synchronous**:
//!   hashing runs at hundreds of megabytes a second, the bytes are already in
//!   the isolate's heap (so they are bounded by it), and a sync op needs no
//!   resumption mark. The JavaScript still answers with a promise, as the Web
//!   Crypto API does, so a body written against the web runs unchanged;
//! - `op_sc_password_hash` and `op_sc_password_verify` are argon2id with
//!   `sc-auth`'s parameters (`Argon2::default()`), **asynchronous** on tokio's
//!   blocking pool. A hash is deliberately slow — tens to hundreds of
//!   milliseconds — and done on the isolate's thread it would be charged to the
//!   body's 1 s JavaScript slice and stall every body resident beside it.
//!
//! Nothing here touches a run's authority: every op is a pure function of its
//! arguments, so none takes a run token. The resumption mark the password ops
//! need is made on the JavaScript side (`SETUP`'s `settle`).

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha384, Sha512};

/// The longest password `crypto.password` will hash, in bytes. Argon2 accepts
/// far more, but a megabyte "password" is a request to burn a blocking thread,
/// not a credential.
const MAX_PASSWORD_BYTES: usize = 4096;

/// Fill `out` from the operating system's CSPRNG. `false` when the OS refused,
/// which the JavaScript turns into a thrown error rather than handing back a
/// buffer of zeroes that looks random to nobody but its author.
#[deno_core::op2(fast)]
pub(crate) fn op_sc_random(#[buffer] out: &mut [u8]) -> bool {
    getrandom::fill(out).is_ok()
}

/// The digest of `data` under `algorithm` (`SHA-256`, `SHA-384`, `SHA-512`;
/// the JavaScript has already normalised and checked the name). Empty for any
/// other name, which the JavaScript never sends.
#[deno_core::op2]
#[buffer]
pub(crate) fn op_sc_digest(#[string] algorithm: &str, #[buffer] data: &[u8]) -> Vec<u8> {
    digest(algorithm, data).unwrap_or_default()
}

/// HMAC of `data` with `key` under `algorithm`'s hash. Empty for an unknown
/// hash, as [`op_sc_digest`] is.
#[deno_core::op2]
#[buffer]
pub(crate) fn op_sc_hmac(
    #[string] algorithm: &str,
    #[buffer] key: &[u8],
    #[buffer] data: &[u8],
) -> Vec<u8> {
    hmac_sign(algorithm, key, data).unwrap_or_default()
}

/// Whether `signature` is the HMAC of `data` with `key`, compared in constant
/// time — the reason `verify` is an op and not `sign` plus `===`.
#[deno_core::op2(fast)]
pub(crate) fn op_sc_hmac_verify(
    #[string] algorithm: &str,
    #[buffer] key: &[u8],
    #[buffer] signature: &[u8],
    #[buffer] data: &[u8],
) -> bool {
    hmac_verify(algorithm, key, signature, data)
}

/// `crypto.password.hash(pw)`: a PHC-format argon2id hash, as JSON
/// `{"ok": "$argon2id$…"}` or `{"error": "…"}` (the envelope every other async
/// op answers with).
#[deno_core::op2]
#[string]
pub(crate) async fn op_sc_password_hash(#[string] password: String) -> String {
    let outcome = match tokio::task::spawn_blocking(move || password_hash(&password)).await {
        Ok(outcome) => outcome,
        Err(e) => Err(format!("the password hash did not finish: {e}")),
    };
    envelope(outcome.map(serde_json::Value::String))
}

/// `crypto.password.verify(pw, hash)`: `{"ok": true|false}`, or `{"error": …}`
/// when `hash` is not a password hash at all — a corrupt column is a bug to
/// surface, not a wrong password.
#[deno_core::op2]
#[string]
pub(crate) async fn op_sc_password_verify(
    #[string] password: String,
    #[string] hash: String,
) -> String {
    let outcome = match tokio::task::spawn_blocking(move || password_verify(&password, &hash)).await
    {
        Ok(outcome) => outcome,
        Err(e) => Err(format!("the password check did not finish: {e}")),
    };
    envelope(outcome.map(serde_json::Value::Bool))
}

fn envelope(outcome: Result<serde_json::Value, String>) -> String {
    match outcome {
        Ok(value) => serde_json::json!({ "ok": value }).to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

pub(crate) fn digest(algorithm: &str, data: &[u8]) -> Option<Vec<u8>> {
    Some(match algorithm {
        "SHA-256" => Sha256::digest(data).to_vec(),
        "SHA-384" => Sha384::digest(data).to_vec(),
        "SHA-512" => Sha512::digest(data).to_vec(),
        _ => return None,
    })
}

/// One body for the three hashes: `Hmac<H>` is a different type per hash.
macro_rules! with_hmac {
    ($algorithm:expr, $key:expr, |$mac:ident| $body:expr) => {
        match $algorithm {
            "SHA-256" => Hmac::<Sha256>::new_from_slice($key)
                .ok()
                .map(|mut $mac| $body),
            "SHA-384" => Hmac::<Sha384>::new_from_slice($key)
                .ok()
                .map(|mut $mac| $body),
            "SHA-512" => Hmac::<Sha512>::new_from_slice($key)
                .ok()
                .map(|mut $mac| $body),
            _ => None,
        }
    };
}

pub(crate) fn hmac_sign(algorithm: &str, key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    with_hmac!(algorithm, key, |mac| {
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    })
}

pub(crate) fn hmac_verify(algorithm: &str, key: &[u8], signature: &[u8], data: &[u8]) -> bool {
    with_hmac!(algorithm, key, |mac| {
        mac.update(data);
        mac.verify_slice(signature).is_ok()
    })
    .unwrap_or(false)
}

/// argon2id at `sc-auth`'s parameters: [`Argon2::default`], a fresh 16-byte
/// salt per hash.
pub(crate) fn password_hash(password: &str) -> Result<String, String> {
    check_password(password)?;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("the password could not be hashed: {e}"))
}

pub(crate) fn password_verify(password: &str, hash: &str) -> Result<bool, String> {
    check_password(password)?;
    let parsed = PasswordHash::new(hash).map_err(|_| {
        "crypto.password.verify: the second argument is not a password hash — pass \
         what crypto.password.hash returned"
            .to_owned()
    })?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(format!("the password could not be checked: {e}")),
    }
}

fn check_password(password: &str) -> Result<(), String> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(format!(
            "a password may be at most {MAX_PASSWORD_BYTES} bytes long"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn digests_match_the_nist_vectors() {
        assert_eq!(
            hex(&digest("SHA-256", b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest("SHA-384", b"abc").unwrap()),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
             8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            hex(&digest("SHA-512", b"abc").unwrap()),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert!(digest("MD5", b"abc").is_none());
    }

    #[test]
    fn hmac_matches_rfc_4231_test_case_2() {
        let mac = hmac_sign("SHA-256", b"Jefe", b"what do ya want for nothing?").unwrap();
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(hmac_verify(
            "SHA-256",
            b"Jefe",
            &mac,
            b"what do ya want for nothing?"
        ));
        assert!(!hmac_verify(
            "SHA-256",
            b"Jefe",
            &mac,
            b"what do ya want for something?"
        ));
    }

    #[test]
    fn a_password_hash_verifies_and_a_wrong_password_does_not() {
        let hash = password_hash("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert_eq!(password_verify("correct horse", &hash), Ok(true));
        assert_eq!(password_verify("battery staple", &hash), Ok(false));
        assert!(password_verify("x", "not a hash").is_err());
        // The same parameters `sc-auth` hashes account passwords with.
        assert!(hash.contains("m=19456,t=2,p=1"), "{hash}");
    }
}

/// The globals as a body sees them: run through a real isolate.
#[cfg(test)]
mod js_tests {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::code::{CodeCall, CodeRuntime};

    fn call(code: &str) -> CodeCall<'static> {
        CodeCall {
            code: code.to_owned(),
            timeout: Some(Duration::from_secs(60)),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn btoa_atob_and_the_text_codecs_round_trip_known_vectors() {
        let rt = CodeRuntime::with_workers(1);
        let out = rt
            .run(call(
                r#"
                const enc = new TextEncoder();
                const dec = new TextDecoder();
                const bytes = enc.encode("naïve 名前 🎉");
                const binary = String.fromCharCode(0, 1, 127, 128, 254, 255);
                let fatal = null;
                try { new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xc3, 0x28])); }
                catch (e) { fatal = e.name; }
                let wide = null;
                try { btoa("名"); } catch (e) { wide = e.name; }
                let bad = null;
                try { atob("a"); } catch (e) { bad = e.name; }
                return {
                    hi: btoa("hi"),
                    back: atob("aGk="),
                    rfc: ["", "f", "fo", "foo", "foob", "fooba", "foobar"].map(btoa),
                    binary: btoa(binary),
                    binaryBack: atob(btoa(binary)) === binary,
                    utf8: Array.from(bytes),
                    text: dec.decode(bytes),
                    viaBuffer: dec.decode(bytes.buffer),
                    bom: dec.decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41])),
                    lenient: dec.decode(new Uint8Array([0xc3, 0x28])),
                    loneSurrogate: Array.from(enc.encode("\ud800")),
                    spaces: atob(" aG k= "),
                    fatal, wide, bad,
                    encoding: [enc.encoding, dec.encoding],
                };
                "#,
            ))
            .await
            .unwrap();
        assert_eq!(out["hi"], json!("aGk="));
        assert_eq!(out["back"], json!("hi"));
        // RFC 4648 §10.
        assert_eq!(
            out["rfc"],
            json!([
                "", "Zg==", "Zm8=", "Zm9v", "Zm9vYg==", "Zm9vYmE=", "Zm9vYmFy"
            ])
        );
        assert_eq!(out["binary"], json!("AAF/gP7/"));
        assert_eq!(out["binaryBack"], json!(true));
        assert_eq!(
            out["utf8"],
            json!([
                110, 97, 195, 175, 118, 101, 32, 229, 144, 141, 229, 137, 141, 32, 240, 159, 142,
                137
            ])
        );
        assert_eq!(out["text"], json!("naïve 名前 🎉"));
        assert_eq!(out["viaBuffer"], json!("naïve 名前 🎉"));
        assert_eq!(out["bom"], json!("A"));
        assert_eq!(out["lenient"], json!("\u{fffd}("));
        assert_eq!(out["loneSurrogate"], json!([0xef, 0xbf, 0xbd]));
        assert_eq!(out["spaces"], json!("hi"));
        assert_eq!(out["fatal"], json!("TypeError"));
        assert_eq!(out["wide"], json!("InvalidCharacterError"));
        assert_eq!(out["bad"], json!("InvalidCharacterError"));
        assert_eq!(out["encoding"], json!(["utf-8", "utf-8"]));
    }

    #[tokio::test]
    async fn get_random_values_fills_and_refuses_more_than_64k() {
        let rt = CodeRuntime::with_workers(1);
        let out = rt
            .run(call(
                r#"
                const a = crypto.getRandomValues(new Uint8Array(64));
                const words = crypto.getRandomValues(new Uint32Array(16384));
                let quota = null;
                try { crypto.getRandomValues(new Uint8Array(65537)); } catch (e) { quota = e.name; }
                let mismatch = null;
                try { crypto.getRandomValues(new Float64Array(4)); } catch (e) { mismatch = e.name; }
                const ids = Array.from({ length: 50 }, () => crypto.randomUUID());
                return {
                    filled: a.some((b) => b !== 0),
                    same: crypto.getRandomValues(a) === a,
                    wordsFilled: words.some((w) => w !== 0),
                    quota, mismatch,
                    uuid: ids[0],
                    distinct: new Set(ids).size,
                    shaped: ids.every((id) => /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id)),
                };
                "#,
            ))
            .await
            .unwrap();
        assert_eq!(out["filled"], json!(true));
        assert_eq!(out["same"], json!(true), "it returns the array it filled");
        assert_eq!(
            out["wordsFilled"],
            json!(true),
            "exactly 65 536 bytes is allowed"
        );
        assert_eq!(out["quota"], json!("QuotaExceededError"));
        assert_eq!(out["mismatch"], json!("TypeMismatchError"));
        assert_eq!(out["distinct"], json!(50));
        assert_eq!(out["shaped"], json!(true), "{}", out["uuid"]);
    }

    #[tokio::test]
    async fn subtle_digest_and_hmac_match_the_nist_and_rfc_4231_vectors() {
        let rt = CodeRuntime::with_workers(1);
        let out = rt
            .run(call(
                r#"
                const hex = (buf) => Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, "0")).join("");
                const enc = new TextEncoder();
                const abc = enc.encode("abc");
                const key = await crypto.subtle.importKey(
                    "raw", enc.encode("Jefe"), { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
                const data = enc.encode("what do ya want for nothing?");
                const sig = await crypto.subtle.sign("HMAC", key, data);
                // RFC 4231 test case 1, SHA-512.
                const key1 = await crypto.subtle.importKey(
                    "raw", new Uint8Array(20).fill(0x0b), { name: "HMAC", hash: { name: "SHA-512" } }, true, ["sign"]);
                const sig1 = await crypto.subtle.sign({ name: "HMAC" }, key1, enc.encode("Hi There"));
                let unsupported = null;
                try { await crypto.subtle.digest("MD5", abc); } catch (e) { unsupported = e.name; }
                let signOnly = null;
                try { await crypto.subtle.verify("HMAC", key1, sig1, enc.encode("Hi There")); } catch (e) { signOnly = e.name; }
                return {
                    sha256: hex(await crypto.subtle.digest("SHA-256", abc)),
                    sha384: hex(await crypto.subtle.digest({ name: "SHA-384" }, abc)),
                    sha512: hex(await crypto.subtle.digest("sha-512", abc.buffer)),
                    empty: hex(await crypto.subtle.digest("SHA-256", new Uint8Array(0))),
                    hmac256: hex(sig),
                    verified: await crypto.subtle.verify("HMAC", key, sig, data),
                    tampered: await crypto.subtle.verify("HMAC", key, sig, enc.encode("what do ya want for something?")),
                    hmac512: hex(sig1),
                    algorithm: key.algorithm,
                    exported: hex(await crypto.subtle.exportKey("raw", key1)),
                    unsupported, signOnly,
                };
                "#,
            ))
            .await
            .unwrap();
        assert_eq!(
            out["sha256"],
            json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            out["sha384"],
            json!(
                "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
            )
        );
        assert_eq!(
            out["sha512"],
            json!(
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
            )
        );
        assert_eq!(
            out["empty"],
            json!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            out["hmac256"],
            json!("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        assert_eq!(out["verified"], json!(true));
        assert_eq!(out["tampered"], json!(false));
        assert_eq!(
            out["hmac512"],
            json!(
                "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
            )
        );
        assert_eq!(
            out["algorithm"],
            json!({ "name": "HMAC", "hash": { "name": "SHA-256" }, "length": 32 })
        );
        assert_eq!(out["exported"], json!("0b".repeat(20)));
        assert_eq!(out["unsupported"], json!("NotSupportedError"));
        assert_eq!(out["signOnly"], json!("InvalidAccessError"));
    }

    #[tokio::test]
    async fn a_password_hash_verifies_and_hashing_does_not_trip_the_slice() {
        let rt = CodeRuntime::with_workers(1);
        // Four argon2id hashes in a row would be well past the 1 s slice in an
        // unoptimised build if they ran on the isolate's thread; on the
        // blocking pool they cost the body's JavaScript nothing.
        let started = Instant::now();
        let out = rt
            .run(call(
                r#"
                const hashes = [];
                for (let i = 0; i < 4; i++) hashes.push(await crypto.password.hash("pw" + i));
                let corrupt = null;
                try { await crypto.password.verify("pw0", "plain text"); } catch (e) { corrupt = e.message; }
                return {
                    prefix: hashes.every((h) => h.startsWith("$argon2id$")),
                    salted: new Set(hashes).size,
                    right: await crypto.password.verify("pw2", hashes[2]),
                    wrong: await crypto.password.verify("pw1", hashes[2]),
                    corrupt,
                };
                "#,
            ))
            .await
            .unwrap();
        assert_eq!(out["prefix"], json!(true));
        assert_eq!(out["salted"], json!(4));
        assert_eq!(out["right"], json!(true));
        assert_eq!(out["wrong"], json!(false));
        assert!(
            out["corrupt"]
                .as_str()
                .is_some_and(|m| m.contains("not a password hash")),
            "{out}"
        );
        eprintln!("4 hashes and 2 checks took {:?}", started.elapsed());

        // The watchdog is still pointed at the run once a hash comes back: a
        // body that spins afterwards is stopped at the slice, not left running.
        let mut c = call("await crypto.password.hash('x'); while (true) {}");
        c.timeout = Some(Duration::from_secs(30));
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("without awaiting anything"), "{err}");
        assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
    }
}
