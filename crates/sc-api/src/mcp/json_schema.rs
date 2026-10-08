//! [`TypeSchema`] → JSON Schema, for the tools projected from tagged endpoints
//! (§13.6).
//!
//! The deliberate sibling of [`crate::typescript`]'s `ts_type`: one function
//! that walks the same four-variant schema and renders it in the vocabulary its
//! consumer reads. The TypeScript generator's consumer is `tsc`; this one's is a
//! model choosing arguments, and JSON Schema is what MCP declares a tool's
//! `inputSchema` in.
//!
//! Three decisions the mapping makes, each of which a reader might otherwise
//! take for a slip:
//!
//! - **`Optional` is two things at once**: absent from `required` *and*
//!   nullable. A model given `"expires_at": {"type": "string"}` and no `required`
//!   entry will still sometimes send `null`, and a schema that then rejects it
//!   has spent a turn on a distinction the wire does not make.
//! - **A struct is closed** — `additionalProperties: false`. The projection
//!   refuses an argument it did not declare
//!   ([`arguments`](super::arguments) does the same for the hand-written tools),
//!   so declaring the schema open would be advertising a latitude the call does
//!   not have.
//! - **`Json` is the empty schema `{}`**, which is JSON Schema for *anything*.
//!   `TypeSchema::Value(Json)` is the escape hatch for a shape no schema
//!   describes — a workflow document, a GraphQL response — and the honest
//!   rendering of "any value" is the schema that accepts any value.
//!
//! The `format` annotations are advisory in JSON Schema and are emitted anyway:
//! a model told a string is a `uuid` writes a UUID, and one told only `string`
//! writes a name.

use crate::schema::{TypeSchema, ValueType};
use serde_json::{Map, Value as Json, json};

/// The JSON Schema for one [`TypeSchema`].
///
/// A `Struct` becomes an `object` whose non-[`Optional`](TypeSchema::Optional)
/// fields are `required`; an `Array` becomes an `array` with `items`; an
/// `Optional` becomes its inner schema, made nullable; a `Value` becomes the
/// scalar mapping below.
pub fn json_schema(schema: &TypeSchema) -> Json {
    match schema {
        TypeSchema::Value(v) => scalar_schema(*v),
        TypeSchema::Struct(fields) => {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in fields {
                properties.insert(field.name.clone(), json_schema(&field.schema));
                if !matches!(field.schema, TypeSchema::Optional(_)) {
                    required.push(Json::String(field.name.clone()));
                }
            }
            object_schema(properties, required)
        }
        TypeSchema::Array(inner) => json!({ "type": "array", "items": json_schema(inner) }),
        TypeSchema::Optional(inner) => nullable(json_schema(inner)),
    }
}

/// An `object` schema over these properties, closed and with this required list.
///
/// The one place the shape is written, because the projection assembles an
/// arguments object out of three sources (path, query, body) and must produce
/// the same kind of object a struct does.
pub fn object_schema(properties: Map<String, Json>, required: Vec<Json>) -> Json {
    let mut out = Map::new();
    out.insert("type".to_owned(), Json::String("object".to_owned()));
    out.insert("properties".to_owned(), Json::Object(properties));
    // An empty `required` is omitted rather than written as `[]`: the two mean
    // the same thing and the shorter one is one less line in every tool
    // description a model reads.
    if !required.is_empty() {
        out.insert("required".to_owned(), Json::Array(required));
    }
    out.insert("additionalProperties".to_owned(), Json::Bool(false));
    Json::Object(out)
}

/// The scalar mapping: what one [`ValueType`] looks like to a model.
///
/// It follows [`ValueType::ts_type`]'s reasoning about what travels as a string
/// — a decimal keeps its precision through a string round-trip that a JSON
/// number would corrupt — and adds the `format` the TypeScript type has no room
/// for.
pub fn scalar_schema(ty: ValueType) -> Json {
    match ty {
        ValueType::Bool => json!({ "type": "boolean" }),
        ValueType::Int => json!({ "type": "integer" }),
        ValueType::Float => json!({ "type": "number" }),
        ValueType::Text => json!({ "type": "string" }),
        ValueType::Decimal => {
            json!({ "type": "string", "description": "an exact decimal, as a string" })
        }
        ValueType::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
        ValueType::Uuid => json!({ "type": "string", "format": "uuid" }),
        ValueType::Date => json!({ "type": "string", "format": "date" }),
        ValueType::Time => json!({ "type": "string", "format": "time" }),
        ValueType::Timestamp => json!({ "type": "string", "format": "date-time" }),
        // Anything, which is what `Value(Json)` means.
        ValueType::Json => json!({}),
        ValueType::Geometry => json!({
            "type": "object",
            "description": "a GeoJSON geometry in WGS84 longitude and latitude, such as \
                            {\"type\": \"Point\", \"coordinates\": [-0.12, 51.5]}",
            "required": ["type"],
        }),
    }
}

/// The same schema, admitting `null`.
///
/// Written as a widened `type` where there is a single one to widen — the form
/// every model has seen a thousand times — and as an `anyOf` otherwise. A schema
/// that already accepts anything (`{}`, which is `Json`'s) accepts `null`
/// already and is left alone, because `{"anyOf": [{}, {"type": "null"}]}` says
/// exactly as much in three times the tokens.
fn nullable(schema: Json) -> Json {
    let Json::Object(mut map) = schema else {
        return json!({ "anyOf": [schema, { "type": "null" }] });
    };
    match map.get("type") {
        None => Json::Object(map),
        Some(Json::String(single)) => {
            let widened = json!([single, "null"]);
            map.insert("type".to_owned(), widened);
            Json::Object(map)
        }
        Some(_) => json!({ "anyOf": [Json::Object(map), { "type": "null" }] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::StructField;

    #[test]
    fn a_struct_declares_its_required_fields_and_admits_no_others() {
        let schema = TypeSchema::struct_of([
            StructField::new("label", TypeSchema::text()),
            StructField::new("expires_in_days", TypeSchema::optional(TypeSchema::int())),
        ]);
        let json = json_schema(&schema);
        assert_eq!(json["type"], json!("object"));
        assert_eq!(json["additionalProperties"], json!(false));
        // Only the field that is not `Optional`.
        assert_eq!(json["required"], json!(["label"]));
        assert_eq!(json["properties"]["label"], json!({ "type": "string" }));
        // …and the optional one is nullable as well as absent from `required`.
        assert_eq!(
            json["properties"]["expires_in_days"],
            json!({ "type": ["integer", "null"] })
        );
    }

    #[test]
    fn the_no_value_schema_is_an_object_with_nothing_in_it() {
        // `TypeSchema::empty()` is how an endpoint says it takes no body, and a
        // tool that takes no arguments still declares an object — every MCP
        // client expects `inputSchema.type` to be `"object"`.
        let json = json_schema(&TypeSchema::empty());
        assert_eq!(json["type"], json!("object"));
        assert_eq!(json["properties"], json!({}));
        assert!(json.get("required").is_none(), "{json}");
    }

    #[test]
    fn an_array_carries_the_shape_of_its_items() {
        let json = json_schema(&TypeSchema::array(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::uuid()),
        ])));
        assert_eq!(json["type"], json!("array"));
        assert_eq!(json["items"]["required"], json!(["id"]));
        assert_eq!(
            json["items"]["properties"]["id"],
            json!({ "type": "string", "format": "uuid" })
        );
    }

    #[test]
    fn every_scalar_maps_to_something_a_model_can_act_on() {
        for ty in ValueType::ALL {
            let json = scalar_schema(ty);
            assert!(json.is_object(), "{ty:?} produced {json}");
            match ty {
                // The one that is deliberately unconstrained: `Json` is the
                // escape hatch for a shape no schema describes.
                ValueType::Json => assert_eq!(json, json!({})),
                _ => assert!(json.get("type").is_some(), "{ty:?} produced {json}"),
            }
        }
    }

    #[test]
    fn an_optional_json_value_stays_the_schema_that_accepts_anything() {
        // Widening "anything" with `null` is three times the tokens for no
        // change in what is accepted.
        assert_eq!(
            json_schema(&TypeSchema::optional(TypeSchema::json())),
            json!({})
        );
    }

    #[test]
    fn an_optional_array_says_null_beside_its_own_type() {
        let json = json_schema(&TypeSchema::optional(TypeSchema::array(TypeSchema::text())));
        assert_eq!(json["type"], json!(["array", "null"]));
        assert_eq!(json["items"], json!({ "type": "string" }));
    }
}
