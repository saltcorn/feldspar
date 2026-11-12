//! Vectors spelt compactly, for a backup (bug report BK1.6).
//!
//! A vector is a JSON array of numbers above the driver, and that is what the
//! API and a formula see. In a backup it is the bulk of a table that has one:
//! a 1024-dimension `halfvec` is 2 KB in its column and some 12 KB as decimal
//! text. So a backup writes each one as **a tagged string**, the elements'
//! bytes in base64:
//!
//! | value | spelling |
//! |---|---|
//! | `vector(n)` | `f32le:<base64 of n little-endian f32s>` |
//! | `halfvec(n)` | `f16le:<base64 of n little-endian IEEE 754 halves>` |
//! | `sparsevec(n)` | `sparse:<n>:<base64 of the k 1-based u32le indices, then the k f32le values>` |
//!
//! The tag says what the bytes are, so a value is read back by what it says it
//! is, not by guessing from its length — `halfvec(4)` and `vector(2)` are both
//! eight bytes. Nothing is lost: an `f32` and a half are written as they are,
//! and read back as the same JSON numbers (or the same `sparsevec` text) the
//! column gives.
//!
//! A restore reads this spelling **and the ones before it**: a vector written
//! as an array of numbers, or a `sparsevec` as its text.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sc_error::{Error, Result};
use serde_json::Value as Json;

const F32: &str = "f32le:";
const F16: &str = "f16le:";
const SPARSE: &str = "sparse:";

/// The elements of a vector as its packed spelling: `f16le:` when `half`
/// (each element rounded to the nearest half, as the column already holds it),
/// `f32le:` otherwise. `None` for anything that is not an array of finite
/// numbers, which the caller writes as it is.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a vector element is single precision, as its column holds it"
)]
pub fn pack(json: &Json, half: bool) -> Option<String> {
    let Json::Array(items) = json else {
        return None;
    };
    let mut bytes = Vec::with_capacity(items.len() * if half { 2 } else { 4 });
    for item in items {
        let x = item.as_f64().filter(|x| x.is_finite())?;
        if half {
            let h = half::f16::from_f64(x);
            if !h.is_finite() {
                return None;
            }
            bytes.extend_from_slice(&h.to_le_bytes());
        } else {
            bytes.extend_from_slice(&(x as f32).to_le_bytes());
        }
    }
    Some(format!(
        "{}{}",
        if half { F16 } else { F32 },
        BASE64.encode(bytes)
    ))
}

/// A packed vector ([`pack`]) as the JSON array of numbers it was. `None` for a
/// string that is not one (the caller reads it some other way); an error for
/// one that says it is and is damaged.
pub fn unpack(text: &str) -> Option<Result<Json>> {
    let (body, width) = if let Some(body) = text.strip_prefix(F32) {
        (body, 4)
    } else {
        (text.strip_prefix(F16)?, 2)
    };
    Some((|| {
        let bytes = decode(body)?;
        if bytes.len() % width != 0 {
            return Err(Error::invalid(format!(
                "a packed vector of {} bytes is not a whole number of elements",
                bytes.len()
            )));
        }
        let floats: Vec<f32> = bytes
            .chunks_exact(width)
            .map(|c| match c {
                [a, b] => half::f16::from_le_bytes([*a, *b]).to_f32(),
                [a, b, c, d] => f32::from_le_bytes([*a, *b, *c, *d]),
                _ => f32::NAN,
            })
            .collect();
        Ok(crate::vector::to_json(&floats))
    })())
}

/// A `sparsevec`'s text (`{1:1.5,4:2}/5`, as Postgres prints it) as its packed
/// spelling, or `None` when it is not one — the caller then writes the text.
pub fn pack_sparse(text: &str) -> Option<String> {
    let (elements, dimensions) = text.trim().rsplit_once('/')?;
    let dimensions: u32 = dimensions.trim().parse().ok()?;
    let inner = elements.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut indices = Vec::new();
    let mut values = Vec::new();
    for pair in inner.split(',').filter(|p| !p.trim().is_empty()) {
        let (i, x) = pair.split_once(':')?;
        indices.extend_from_slice(&i.trim().parse::<u32>().ok()?.to_le_bytes());
        let x: f32 = x.trim().parse().ok().filter(|x: &f32| x.is_finite())?;
        values.extend_from_slice(&x.to_le_bytes());
    }
    indices.extend_from_slice(&values);
    Some(format!("{SPARSE}{dimensions}:{}", BASE64.encode(indices)))
}

/// A packed `sparsevec` ([`pack_sparse`]) as the text the column reads, or
/// `None` for a string that is not one.
pub fn unpack_sparse(text: &str) -> Option<Result<String>> {
    let rest = text.strip_prefix(SPARSE)?;
    Some((|| {
        let (dimensions, body) = rest
            .split_once(':')
            .ok_or_else(|| Error::invalid("a packed sparse vector has no dimensions"))?;
        let dimensions: u32 = dimensions.parse().map_err(|_| {
            Error::invalid(format!(
                "{dimensions:?} is not a sparse vector's dimensions"
            ))
        })?;
        let bytes = decode(body)?;
        if bytes.len() % 8 != 0 {
            return Err(Error::invalid(format!(
                "a packed sparse vector of {} bytes is not a whole number of elements",
                bytes.len()
            )));
        }
        let (indices, values) = bytes.split_at(bytes.len() / 2);
        let pairs: Vec<String> = indices
            .chunks_exact(4)
            .zip(values.chunks_exact(4))
            .map(|(i, x)| {
                let i = u32::from_le_bytes([i[0], i[1], i[2], i[3]]);
                let x = f32::from_le_bytes([x[0], x[1], x[2], x[3]]);
                format!("{i}:{x}")
            })
            .collect();
        Ok(format!("{{{}}}/{dimensions}", pairs.join(",")))
    })())
}

fn decode(body: &str) -> Result<Vec<u8>> {
    BASE64
        .decode(body)
        .map_err(|e| Error::invalid(format!("a packed vector is not base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_vector_packs_and_unpacks_to_the_same_numbers() {
        let v = json!([0.1, -2.5, 3.0e-5, 1.0]);
        let packed = pack(&v, false).unwrap();
        assert!(packed.starts_with("f32le:"), "{packed}");
        assert_eq!(unpack(&packed).unwrap().unwrap(), v);
    }

    #[test]
    fn a_half_vector_packs_to_two_bytes_an_element() {
        // What a halfvec column gives back for [0.1, 1, -2]: the nearest halves.
        let v = json!([0.099975586, 1.0, -2.0]);
        let packed = pack(&v, true).unwrap();
        assert_eq!(
            packed,
            format!("f16le:{}", BASE64.encode([0x66, 0x2e, 0, 0x3c, 0, 0xc0]))
        );
        assert_eq!(unpack(&packed).unwrap().unwrap(), v);
        // Past the half range there is no half to write.
        assert_eq!(pack(&json!([70000]), true), None);
    }

    #[test]
    fn what_is_not_a_vector_is_left_alone_or_refused() {
        assert_eq!(pack(&json!("[1,2]"), false), None);
        assert_eq!(pack(&json!([1, "x"]), false), None);
        assert!(unpack("[1,2]").is_none());
        assert!(unpack("f32le:!!").unwrap().is_err());
        assert!(
            unpack(&format!("f32le:{}", BASE64.encode([1, 2, 3])))
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn a_sparse_vector_packs_and_unpacks_to_the_same_text() {
        let text = "{1:1.5,4:2}/5";
        let packed = pack_sparse(text).unwrap();
        assert!(packed.starts_with("sparse:5:"), "{packed}");
        assert_eq!(unpack_sparse(&packed).unwrap().unwrap(), text);
        assert_eq!(
            unpack_sparse(&pack_sparse("{}/3").unwrap())
                .unwrap()
                .unwrap(),
            "{}/3"
        );
        assert_eq!(pack_sparse("not a vector"), None);
        assert!(unpack_sparse(text).is_none());
    }
}
