//! What the elements of a stream *are* (TODO §4, tasks 1.2 and 1.3).
//!
//! A stream declares its [`ElementType`] and every element that arrives is
//! shaped by it: an object with known keys, a string in a named encoding, or
//! bytes. The declaration is not decoration — it is what the Observe screen
//! renders against, what a trigger's `only_if` reads, and what the generated
//! TypeScript client types an application's subscription from. So it is checked
//! where it is declared ([`ElementType::validate`]) and enforced where an
//! element is decoded ([`ElementType::decode`]), and neither of those guesses.
//!
//! ## GOALS asks "(which encoding?)" and this is the answer
//!
//! **The declaration carries one, the runtime implements UTF-8, and a non-UTF-8
//! declaration is refused at save time.** Guessing an encoding is the failure
//! mode that produces a stream of replacement characters nobody notices for a
//! week; refusing `latin1` while the admin is looking at the form is a sentence
//! they can act on. When a second encoding is worth decoding it is added here
//! and the refusal narrows — which is a change to one list, not a migration.
//!
//! ## Unknown keys are carried, a declared key that is absent is null
//!
//! A publisher that adds a field must not break a stream that did not ask for
//! it, and a stream that declared five keys must produce five keys per element
//! or the table on the Observe screen is ragged. Both rules are §4's, and
//! together they mean a declared shape is a *floor* on what an element has and
//! not a ceiling.

use base64::Engine as _;
use sc_error::{Error, Result};
use sc_types::BasicType;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

/// The encoding a [`Text`](ElementType::Text) element is decoded with, and the
/// only one this milestone implements.
pub const UTF8: &str = "utf8";

/// One declared key of a [`Json`](ElementType::Json) element (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementField {
    /// The key's name in the object.
    pub name: String,
    /// What a value of it is. Carried over the wire as
    /// [`BasicType::name`] — `"float"`, not `"float8"` — because this
    /// declaration is read by the admin UI and by a module in JavaScript, and
    /// neither speaks Postgres.
    #[serde(rename = "type", with = "basic_type")]
    pub r#type: BasicType,
    /// Whether an element missing this key (or carrying `null` in it) is
    /// **refused** rather than delivered with a null.
    ///
    /// Sparse on the wire: a key that is not required does not say so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// For a `json` key whose value is an object of a known shape, that
    /// shape: the keys of the object, each declared the same way.
    ///
    /// What a `table_changes` stream's `row` key is — the table's row, one
    /// sub-key per column — so a generated client types `element.row.title`
    /// rather than handing over an `unknown` (TODO.md "Live updates" L3.4).
    /// Descriptive only: [`ElementType::decode`] checks the key is JSON, not
    /// that the object has these keys, because a declared shape is a floor
    /// (see the module docs). Sparse on the wire: a key with none says nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ElementField>,
}

impl ElementField {
    /// An optional key of this name and type.
    pub fn new(name: impl Into<String>, r#type: BasicType) -> ElementField {
        ElementField {
            name: name.into(),
            r#type,
            required: false,
            keys: Vec::new(),
        }
    }

    /// Declare the shape of this `json` key's object value.
    pub fn with_keys(mut self, keys: impl IntoIterator<Item = ElementField>) -> ElementField {
        self.keys = keys.into_iter().collect();
        self
    }

    /// Mark the key required: an element without it is malformed.
    pub fn required(mut self) -> ElementField {
        self.required = true;
        self
    }
}

/// What the elements of a stream are, as a function of the stream's
/// configuration (§4, [`StreamProvider::element_type`](crate::StreamProvider::element_type)).
///
/// Tagged `kind` on the wire, because this JSON is read by the admin UI, by the
/// generated client and by a module — an externally tagged enum would make
/// every consumer unwrap a one-key object first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ElementType {
    /// An object with known keys, each of a known basic type (GOALS).
    ///
    /// Unknown keys are carried through rather than dropped; a declared key
    /// that is absent is `null`.
    Json {
        /// The declared keys, in the order they are shown.
        keys: Vec<ElementField>,
    },
    /// Characters, in a named encoding.
    ///
    /// [`UTF8`] is the default and the only one this milestone decodes;
    /// anything else is refused at save time rather than mis-decoded at 3am.
    Text {
        /// The encoding's name, lowercase.
        encoding: String,
    },
    /// Bytes. `value` is base64 in the envelope, and the Observe screen shows a
    /// hex head rather than pretending it is text.
    Binary,
}

impl ElementType {
    /// An object element with these declared keys.
    pub fn json(keys: impl IntoIterator<Item = ElementField>) -> ElementType {
        ElementType::Json {
            keys: keys.into_iter().collect(),
        }
    }

    /// A UTF-8 text element.
    pub fn text() -> ElementType {
        ElementType::Text {
            encoding: UTF8.to_owned(),
        }
    }

    /// The stable wire name of the variant — what the UI switches on.
    pub fn kind(&self) -> &'static str {
        match self {
            ElementType::Json { .. } => "json",
            ElementType::Text { .. } => "text",
            ElementType::Binary => "binary",
        }
    }

    /// The declared keys, or an empty slice for the two that have none.
    pub fn keys(&self) -> &[ElementField] {
        match self {
            ElementType::Json { keys } => keys,
            _ => &[],
        }
    }

    /// Check that this declaration is one the runtime can actually honour.
    ///
    /// Four refusals, each naming what is wrong and where, because every one of
    /// them is a thing an admin is looking at a form full of when it happens:
    ///
    /// - a [`Json`](ElementType::Json) type with **no keys**, which declares
    ///   nothing and would make the Observe screen a table with no columns;
    /// - a **duplicate key**, which would put two meanings in one column;
    /// - a key of a type that has no JSON form ([`BasicType::Bytes`]) or that
    ///   this system does not know ([`BasicType::Other`]) — "a known basic
    ///   type" is what GOALS asks for, and `Other` is by definition the
    ///   unknown one;
    /// - a [`Text`](ElementType::Text) encoding that is not [`UTF8`].
    ///
    /// This runs on save (task 2.2) and, for the providers that build a type
    /// out of admin-supplied settings, inside `element_type` itself.
    pub fn validate(&self) -> Result<()> {
        match self {
            ElementType::Json { keys } => {
                if keys.is_empty() {
                    return Err(Error::invalid(
                        "a json element type must declare at least one key: an element of no \
                         declared keys has no shape for a trigger, a client or the Observe \
                         screen to read",
                    ));
                }
                let mut seen: Vec<&str> = Vec::with_capacity(keys.len());
                for key in keys {
                    let name = key.name.trim();
                    if name.is_empty() {
                        return Err(Error::invalid("an element key must have a name"));
                    }
                    if seen.contains(&name) {
                        return Err(Error::invalid(format!(
                            "the element key `{name}` is declared twice"
                        )));
                    }
                    seen.push(name);
                    validate_sub_keys(key)?;
                    match &key.r#type {
                        BasicType::Bytes => {
                            return Err(Error::invalid(format!(
                                "the element key `{name}` is `bytes`, which has no JSON form; a \
                                 stream of bytes is the `binary` element type"
                            )));
                        }
                        BasicType::Other(other) => {
                            return Err(Error::invalid(format!(
                                "the element key `{name}` has the unknown type `{other}`"
                            )));
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            ElementType::Text { encoding } => {
                if encoding.trim().to_ascii_lowercase() == UTF8 {
                    Ok(())
                } else {
                    Err(Error::invalid(format!(
                        "the encoding `{encoding}` is not one this server decodes; `{UTF8}` is. \
                         Declaring an encoding nothing implements would deliver replacement \
                         characters rather than an error"
                    )))
                }
            }
            ElementType::Binary => Ok(()),
        }
    }

    /// Turn what a provider actually received into the envelope's `value`
    /// (task 1.3).
    ///
    /// The one place a payload becomes an element, so it is the one place that
    /// decides what "malformed" means. Every failure is an [`Error::invalid`]
    /// naming the key or the reason, because the caller counts it, logs it once
    /// a minute and **does not deliver it** (§11): a broker with one misbehaving
    /// publisher must not fill the log or the trigger queue.
    pub fn decode(&self, raw: RawPayload) -> Result<Json> {
        match self {
            ElementType::Json { keys } => {
                let value = match raw {
                    RawPayload::Json(json) => json,
                    RawPayload::Bytes(bytes) => {
                        let text = String::from_utf8(bytes).map_err(|_| {
                            Error::invalid("a json element's payload is not valid UTF-8")
                        })?;
                        serde_json::from_str::<Json>(&text).map_err(|e| {
                            Error::invalid(format!("a json element's payload is not JSON: {e}"))
                        })?
                    }
                };
                let Json::Object(mut object) = value else {
                    return Err(Error::invalid(format!(
                        "a json element must be an object, got {}",
                        json_kind(&value)
                    )));
                };
                // The declared keys first, then whatever else arrived,
                // carried rather than dropped. The *order* is not part of the
                // contract — an Observe column and a generated client's type
                // both come from the declaration, which is a list — but the
                // presence of every declared key is: §4 says an absent one is
                // null, so a consumer never has to ask whether a key exists.
                let mut out = Map::with_capacity(object.len().max(keys.len()));
                for key in keys {
                    let value = object.remove(&key.name).unwrap_or(Json::Null);
                    if value.is_null() {
                        if key.required {
                            return Err(Error::invalid(format!(
                                "the element key `{}` is required and the payload has no value \
                                 for it",
                                key.name
                            )));
                        }
                        out.insert(key.name.clone(), Json::Null);
                        continue;
                    }
                    if !key.r#type.accepts_json(&value) {
                        return Err(Error::invalid(format!(
                            "the element key `{}` should be {}, got {}",
                            key.name,
                            key.r#type.name(),
                            json_kind(&value)
                        )));
                    }
                    out.insert(key.name.clone(), value);
                }
                out.extend(object);
                Ok(Json::Object(out))
            }
            ElementType::Text { encoding } => {
                if encoding.trim().to_ascii_lowercase() != UTF8 {
                    return Err(Error::invalid(format!(
                        "the encoding `{encoding}` is not one this server decodes"
                    )));
                }
                match raw {
                    RawPayload::Bytes(bytes) => String::from_utf8(bytes)
                        .map(Json::String)
                        .map_err(|_| Error::invalid("a text element's payload is not valid UTF-8")),
                    RawPayload::Json(Json::String(text)) => Ok(Json::String(text)),
                    RawPayload::Json(other) => Err(Error::invalid(format!(
                        "a text element must be a string, got {}",
                        json_kind(&other)
                    ))),
                }
            }
            ElementType::Binary => match raw {
                RawPayload::Bytes(bytes) => Ok(Json::String(BASE64.encode(bytes))),
                // A provider that already has base64 (a module's poll returns
                // JSON, and JSON has no bytes) says so by handing over a
                // string — checked by decoding it, because a `value` the client
                // cannot decode is worse than a refusal here.
                RawPayload::Json(Json::String(encoded)) => match BASE64.decode(&encoded) {
                    Ok(_) => Ok(Json::String(encoded)),
                    Err(e) => Err(Error::invalid(format!(
                        "a binary element's payload is a string but not base64: {e}"
                    ))),
                },
                RawPayload::Json(other) => Err(Error::invalid(format!(
                    "a binary element must be bytes or base64 text, got {}",
                    json_kind(&other)
                ))),
            },
        }
    }
}

/// Check the declared shape of one key's object value, if it has one: only a
/// `json` key can have one, and within it every name is non-empty, used once,
/// and of a type this system knows. A sub-key may be `bytes` — a row's `bytea`
/// column travels as base64 text, as it does over REST.
fn validate_sub_keys(key: &ElementField) -> Result<()> {
    if key.keys.is_empty() {
        return Ok(());
    }
    if key.r#type != BasicType::Json {
        return Err(Error::invalid(format!(
            "the element key `{}` declares the keys of an object, so it must be `json`, not `{}`",
            key.name,
            key.r#type.name()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(key.keys.len());
    for sub in &key.keys {
        let name = sub.name.trim();
        if name.is_empty() {
            return Err(Error::invalid(format!(
                "a key inside the element key `{}` must have a name",
                key.name
            )));
        }
        if seen.contains(&name) {
            return Err(Error::invalid(format!(
                "the key `{name}` is declared twice inside the element key `{}`",
                key.name
            )));
        }
        seen.push(name);
        if let BasicType::Other(other) = &sub.r#type {
            return Err(Error::invalid(format!(
                "the key `{name}` inside the element key `{}` has the unknown type `{other}`",
                key.name
            )));
        }
        validate_sub_keys(sub)?;
    }
    Ok(())
}

/// The base64 alphabet the envelope carries bytes in: standard, padded — what
/// `atob` in a browser and `Buffer.from(s, "base64")` in a module both read.
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// What a provider actually received, before an [`ElementType`] shapes it.
///
/// Two variants because there are two kinds of provider and they genuinely
/// differ: a broker hands over **bytes** and has no idea what is in them, while
/// a module's poll returns **JSON**, because the module seam is JSON and cannot
/// carry anything else. Making one of them convert to the other before this
/// point would mean either parsing bytes nobody asked to parse, or re-encoding
/// a value that was never encoded.
#[derive(Debug, Clone, PartialEq)]
pub enum RawPayload {
    /// Bytes as they arrived — what a broker hands over.
    Bytes(Vec<u8>),
    /// A value the provider has already parsed — what a module's poll returns.
    Json(Json),
}

impl RawPayload {
    /// The bytes of a payload that arrived as bytes.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> RawPayload {
        RawPayload::Bytes(bytes.into())
    }

    /// A payload that is already a value.
    pub fn json(value: impl Into<Json>) -> RawPayload {
        RawPayload::Json(value.into())
    }
}

/// What a JSON value *is*, for an error message: the word an admin reading the
/// failure needs, rather than the value itself (which may be the payload of
/// something they should not be shown in a log).
fn json_kind(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "a list",
        Json::Object(_) => "an object",
    }
}

/// [`BasicType`] on the wire, as its [`name`](BasicType::name).
///
/// `BasicType` has no `Serialize` of its own, and giving it one would fix a
/// single encoding for every consumer in the tree. Here the encoding is this
/// declaration's: the stable name, which is what the admin UI's picker lists
/// and what a module writes in its `element_type`.
mod basic_type {
    use sc_types::BasicType;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ty: &BasicType, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(ty.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BasicType, D::Error> {
        let name = String::deserialize(d)?;
        Ok(BasicType::from_name(&name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn boiler() -> ElementType {
        ElementType::json([
            ElementField::new("temperature", BasicType::Float).required(),
            ElementField::new("unit", BasicType::Text),
        ])
    }

    #[test]
    fn an_element_type_round_trips_through_json_under_its_stable_names() {
        let ty = boiler();
        let wire = serde_json::to_value(&ty).unwrap();
        assert_eq!(
            wire,
            json!({
                "kind": "json",
                "keys": [
                    { "name": "temperature", "type": "float", "required": true },
                    { "name": "unit", "type": "text" },
                ]
            })
        );
        assert_eq!(serde_json::from_value::<ElementType>(wire).unwrap(), ty);

        let text = ElementType::text();
        let wire = serde_json::to_value(&text).unwrap();
        assert_eq!(wire, json!({ "kind": "text", "encoding": "utf8" }));
        assert_eq!(serde_json::from_value::<ElementType>(wire).unwrap(), text);

        let wire = serde_json::to_value(ElementType::Binary).unwrap();
        assert_eq!(wire, json!({ "kind": "binary" }));
        assert_eq!(
            serde_json::from_value::<ElementType>(wire).unwrap(),
            ElementType::Binary
        );
    }

    #[test]
    fn a_json_type_with_no_keys_is_refused() {
        let err = ElementType::json([]).validate().unwrap_err().to_string();
        assert!(err.contains("at least one key"), "{err}");
    }

    #[test]
    fn a_duplicate_key_is_refused_naming_it() {
        let ty = ElementType::json([
            ElementField::new("temperature", BasicType::Float),
            ElementField::new("temperature", BasicType::Text),
        ]);
        let err = ty.validate().unwrap_err().to_string();
        assert!(
            err.contains("`temperature`") && err.contains("twice"),
            "{err}"
        );
    }

    #[test]
    fn a_key_of_a_type_with_no_json_form_is_refused_naming_it() {
        let ty = ElementType::json([ElementField::new("blob", BasicType::Bytes)]);
        let err = ty.validate().unwrap_err().to_string();
        assert!(err.contains("`blob`") && err.contains("binary"), "{err}");

        let ty = ElementType::json([ElementField::new("where", BasicType::Other("geo".into()))]);
        let err = ty.validate().unwrap_err().to_string();
        assert!(err.contains("`where`") && err.contains("geo"), "{err}");
    }

    #[test]
    fn a_text_encoding_that_is_not_utf8_is_refused_naming_it() {
        let ty = ElementType::Text {
            encoding: "latin1".into(),
        };
        let err = ty.validate().unwrap_err().to_string();
        assert!(err.contains("latin1") && err.contains("utf8"), "{err}");
        // The declared one is normalised on the way in, not silently accepted
        // in any casing: `UTF8` is the same encoding.
        assert!(
            ElementType::Text {
                encoding: "UTF8".into()
            }
            .validate()
            .is_ok()
        );
        assert!(boiler().validate().is_ok());
        assert!(ElementType::Binary.validate().is_ok());
    }

    #[test]
    fn a_json_payload_keeps_every_declared_key_and_carries_the_rest() {
        let value = boiler()
            .decode(RawPayload::bytes(
                br#"{"extra": 1, "unit": "C", "temperature": 31.2}"#.to_vec(),
            ))
            .unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 3);
        assert_eq!(value["temperature"], json!(31.2));
        assert_eq!(value["unit"], json!("C"));
        // The undeclared key is carried rather than dropped (§4): a publisher
        // that adds a field must not break a stream that did not ask for it.
        assert_eq!(value["extra"], json!(1));
    }

    #[test]
    fn a_declared_key_that_is_absent_is_null_unless_it_is_required() {
        let value = boiler()
            .decode(RawPayload::json(json!({ "temperature": 20 })))
            .unwrap();
        assert_eq!(value["unit"], Json::Null);

        let err = boiler()
            .decode(RawPayload::json(json!({ "unit": "C" })))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`temperature`") && err.contains("required"),
            "{err}"
        );
    }

    #[test]
    fn a_payload_of_the_wrong_shape_is_refused_rather_than_coerced() {
        let err = boiler()
            .decode(RawPayload::json(json!({ "temperature": "warm" })))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`temperature`") && err.contains("float"),
            "{err}"
        );

        let err = boiler()
            .decode(RawPayload::json(json!([1, 2])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be an object"), "{err}");

        let err = boiler()
            .decode(RawPayload::bytes(b"not json".to_vec()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not JSON"), "{err}");

        let err = boiler()
            .decode(RawPayload::bytes(vec![0xff, 0xfe]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn a_json_key_may_declare_the_shape_of_its_object_and_round_trips() {
        let ty = ElementType::json([
            ElementField::new("op", BasicType::Text).required(),
            ElementField::new("row", BasicType::Json).with_keys([
                ElementField::new("id", BasicType::Int).required(),
                ElementField::new("photo", BasicType::Bytes),
            ]),
        ]);
        ty.validate().unwrap();
        let wire = serde_json::to_value(&ty).unwrap();
        assert_eq!(
            wire["keys"][1]["keys"][0],
            json!({ "name": "id", "type": "int", "required": true })
        );
        // A key with no shape says nothing about one.
        assert!(wire["keys"][0].get("keys").is_none());
        assert_eq!(serde_json::from_value::<ElementType>(wire).unwrap(), ty);

        for (bad, says) in [
            (
                ElementField::new("row", BasicType::Text)
                    .with_keys([ElementField::new("id", BasicType::Int)]),
                "must be `json`",
            ),
            (
                ElementField::new("row", BasicType::Json).with_keys([
                    ElementField::new("id", BasicType::Int),
                    ElementField::new("id", BasicType::Text),
                ]),
                "declared twice inside",
            ),
            (
                ElementField::new("row", BasicType::Json)
                    .with_keys([ElementField::new("x", BasicType::Other("money".into()))]),
                "unknown type `money`",
            ),
        ] {
            let err = ElementType::json([bad]).validate().unwrap_err().to_string();
            assert!(err.contains(says), "{err}");
        }
    }

    #[test]
    fn a_text_element_is_decoded_as_utf8_and_refused_when_it_is_not() {
        let ty = ElementType::text();
        assert_eq!(
            ty.decode(RawPayload::bytes("héllo".as_bytes().to_vec()))
                .unwrap(),
            json!("héllo")
        );
        assert_eq!(
            ty.decode(RawPayload::json(json!("already text"))).unwrap(),
            json!("already text")
        );
        let err = ty
            .decode(RawPayload::bytes(vec![0x80]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"), "{err}");
        let err = ty
            .decode(RawPayload::json(json!(7)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a string"), "{err}");
    }

    #[test]
    fn a_binary_element_is_base64_in_the_envelope() {
        let value = ElementType::Binary
            .decode(RawPayload::bytes(vec![0xde, 0xad, 0xbe, 0xef]))
            .unwrap();
        assert_eq!(value, json!("3q2+7w=="));
        // Already-encoded bytes are taken as they are, but only if they decode.
        assert_eq!(
            ElementType::Binary
                .decode(RawPayload::json(json!("3q2+7w==")))
                .unwrap(),
            json!("3q2+7w==")
        );
        let err = ElementType::Binary
            .decode(RawPayload::json(json!("not base64!!")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64"), "{err}");
    }

    #[test]
    fn the_kind_and_keys_are_what_a_screen_switches_on() {
        assert_eq!(boiler().kind(), "json");
        assert_eq!(boiler().keys().len(), 2);
        assert_eq!(ElementType::text().kind(), "text");
        assert!(ElementType::text().keys().is_empty());
        assert_eq!(ElementType::Binary.kind(), "binary");
    }
}
