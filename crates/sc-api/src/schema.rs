//! The type schema describing endpoint inputs and outputs (design §13.1).
//!
//! [`TypeSchema`] is the reified, serializable description of the shape of an
//! endpoint's request body and response value. It is deliberately small — just
//! enough to describe arguments and results and to emit TypeScript types — and
//! mirrors the "data, not fluent calls" philosophy of the query layer: a schema
//! is a plain value that can be inspected, stored, or turned into a TypeScript
//! declaration by the generator in [`crate::typescript`].
//!
//! The leaf of the schema is [`ValueType`], the scalar type set. It mirrors the
//! non-null variants of the query layer's `Value`, so a schema built from the
//! data layer's columns lines up one-to-one with the values that actually flow
//! over the wire.

use sc_types::BasicType;
use serde::{Deserialize, Serialize};

/// A scalar type: the leaf of a [`TypeSchema`]. The variants mirror the non-null
/// families of the query layer's `Value`, so every column/argument type maps to
/// exactly one of these and to exactly one TypeScript type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    /// Boolean — TS `boolean`.
    Bool,
    /// 64-bit signed integer — TS `number`.
    Int,
    /// IEEE-754 floating point — TS `number`.
    Float,
    /// Exact fixed-point decimal — TS `string` (arbitrary precision survives a
    /// string round-trip that a JS `number` would silently corrupt).
    Decimal,
    /// UTF-8 text — TS `string`.
    Text,
    /// Raw bytes, carried as base64 — TS `string`.
    Bytes,
    /// Arbitrary embedded JSON — TS `unknown`.
    Json,
    /// UUID, carried as its canonical string form — TS `string`.
    Uuid,
    /// Calendar date (ISO 8601) — TS `string`.
    Date,
    /// Wall-clock time (ISO 8601) — TS `string`.
    Time,
    /// Instant in time (RFC 3339) — TS `string`.
    Timestamp,
    /// A geometry, carried as a GeoJSON geometry object (analytics TODO A5.1) —
    /// TS an object with `type` and `coordinates`. Not in [`ALL`](ValueType::ALL): a custom query's
    /// parameter is not declared as one.
    Geometry,
}

impl ValueType {
    /// The wire type a column's [`BasicType`] is carried as.
    ///
    /// The two enums are deliberately 1:1 — [`BasicType`] is what a column *is*
    /// in the database, [`ValueType`] is what it looks like on the wire — except
    /// for [`BasicType::Other`], the unmapped-backend-type escape hatch, which
    /// travels as text exactly as its own docs prescribe.
    pub fn from_basic(basic: &BasicType) -> ValueType {
        match basic {
            BasicType::Bool => ValueType::Bool,
            BasicType::Int => ValueType::Int,
            BasicType::Float => ValueType::Float,
            BasicType::Decimal => ValueType::Decimal,
            BasicType::Text => ValueType::Text,
            BasicType::Bytes => ValueType::Bytes,
            BasicType::Json => ValueType::Json,
            BasicType::Uuid => ValueType::Uuid,
            BasicType::Date => ValueType::Date,
            BasicType::Time => ValueType::Time,
            BasicType::Timestamp => ValueType::Timestamp,
            BasicType::Geometry(_) => ValueType::Geometry,
            BasicType::Other(_) => ValueType::Text,
        }
    }

    /// The column type this wire type is carried for — the inverse of
    /// [`from_basic`](ValueType::from_basic), and lossless in this direction
    /// because every variant here names one [`BasicType`].
    ///
    /// What a value has to be coerced *to* before it can be bound: a custom SQL
    /// query's parameter is declared as a [`ValueType`] by the admin and reaches
    /// the database as a value of the corresponding basic type, through the same
    /// `json_to_value` a row write goes through.
    pub fn to_basic(self) -> BasicType {
        match self {
            ValueType::Bool => BasicType::Bool,
            ValueType::Int => BasicType::Int,
            ValueType::Float => BasicType::Float,
            ValueType::Decimal => BasicType::Decimal,
            ValueType::Text => BasicType::Text,
            ValueType::Bytes => BasicType::Bytes,
            ValueType::Json => BasicType::Json,
            ValueType::Uuid => BasicType::Uuid,
            ValueType::Date => BasicType::Date,
            ValueType::Time => BasicType::Time,
            ValueType::Timestamp => BasicType::Timestamp,
            ValueType::Geometry => BasicType::Geometry(sc_types::GeometryKind::Any),
        }
    }

    /// The wire name of this type — what it serialises as, and what an admin
    /// writes when they declare a custom query's parameter.
    pub fn name(self) -> &'static str {
        match self {
            ValueType::Bool => "bool",
            ValueType::Int => "int",
            ValueType::Float => "float",
            ValueType::Decimal => "decimal",
            ValueType::Text => "text",
            ValueType::Bytes => "bytes",
            ValueType::Json => "json",
            ValueType::Uuid => "uuid",
            ValueType::Date => "date",
            ValueType::Time => "time",
            ValueType::Timestamp => "timestamp",
            ValueType::Geometry => "geometry",
        }
    }

    /// Every type a custom query's parameter may be declared as, in the order a
    /// chooser should offer them — so a command line and a form can both name
    /// the whole set without either holding a list of its own that a new
    /// variant would not reach. Everything but [`Geometry`](ValueType::Geometry),
    /// which is a column's type and never a parameter's.
    pub const ALL: [ValueType; 11] = [
        ValueType::Text,
        ValueType::Int,
        ValueType::Float,
        ValueType::Decimal,
        ValueType::Bool,
        ValueType::Date,
        ValueType::Timestamp,
        ValueType::Time,
        ValueType::Uuid,
        ValueType::Json,
        ValueType::Bytes,
    ];

    /// The type written under this name, if it is one — the inverse of
    /// [`name`](ValueType::name).
    pub fn from_name(name: &str) -> Option<ValueType> {
        ValueType::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The TypeScript type this scalar serializes as. See the per-variant docs
    /// for why non-string scalars (decimal, bytes) are carried as strings.
    pub fn ts_type(self) -> &'static str {
        match self {
            ValueType::Bool => "boolean",
            ValueType::Int | ValueType::Float => "number",
            ValueType::Json => "unknown",
            // A GeoJSON geometry object, spelled inline so a generated client
            // needs no declaration of its own.
            ValueType::Geometry => {
                "{ type: string; coordinates?: unknown; geometries?: unknown[] }"
            }
            // Everything else is carried as a JSON string.
            ValueType::Decimal
            | ValueType::Text
            | ValueType::Bytes
            | ValueType::Uuid
            | ValueType::Date
            | ValueType::Time
            | ValueType::Timestamp => "string",
        }
    }
}

/// A named member of a [`TypeSchema::Struct`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructField {
    /// The field name — used verbatim as the JSON key and the TS property name.
    pub name: String,
    /// The field's type.
    pub schema: TypeSchema,
}

impl StructField {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, schema: TypeSchema) -> StructField {
        StructField {
            name: name.into(),
            schema,
        }
    }
}

/// The shape of an endpoint's input or output value (design §13.1).
///
/// This is enough to describe arguments and results and to emit TypeScript
/// types: a scalar, a record of named fields, a homogeneous array, or an
/// optional (nullable) wrapper. Richer shapes (unions, maps) are added when a
/// concrete endpoint needs them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeSchema {
    /// A scalar leaf.
    Value(ValueType),
    /// A record of named fields. An empty struct is the canonical "no value"
    /// shape (see [`TypeSchema::empty`]).
    Struct(Vec<StructField>),
    /// A homogeneous array.
    Array(Box<TypeSchema>),
    /// An optional value: present-or-`null`.
    Optional(Box<TypeSchema>),
}

impl TypeSchema {
    /// A scalar schema.
    pub fn value(t: ValueType) -> TypeSchema {
        TypeSchema::Value(t)
    }

    /// A struct schema from an iterator of fields.
    pub fn struct_of(fields: impl IntoIterator<Item = StructField>) -> TypeSchema {
        TypeSchema::Struct(fields.into_iter().collect())
    }

    /// An array of `inner`.
    pub fn array(inner: TypeSchema) -> TypeSchema {
        TypeSchema::Array(Box::new(inner))
    }

    /// An optional (nullable) `inner`.
    pub fn optional(inner: TypeSchema) -> TypeSchema {
        TypeSchema::Optional(Box::new(inner))
    }

    /// The canonical "no value" schema — an empty struct. Used for endpoints
    /// with no request body or no meaningful response payload.
    pub fn empty() -> TypeSchema {
        TypeSchema::Struct(Vec::new())
    }

    /// Whether this schema carries no value (an empty [`Struct`](TypeSchema::Struct)).
    pub fn is_empty(&self) -> bool {
        matches!(self, TypeSchema::Struct(fields) if fields.is_empty())
    }

    // --- scalar shortcuts, for readable endpoint definitions ----------------

    /// Shortcut for `Value(Text)`.
    pub fn text() -> TypeSchema {
        TypeSchema::Value(ValueType::Text)
    }

    /// Shortcut for `Value(Uuid)`.
    pub fn uuid() -> TypeSchema {
        TypeSchema::Value(ValueType::Uuid)
    }

    /// Shortcut for `Value(Int)`.
    pub fn int() -> TypeSchema {
        TypeSchema::Value(ValueType::Int)
    }

    /// Shortcut for `Value(Bool)`.
    pub fn bool() -> TypeSchema {
        TypeSchema::Value(ValueType::Bool)
    }

    /// Shortcut for `Value(Json)`.
    pub fn json() -> TypeSchema {
        TypeSchema::Value(ValueType::Json)
    }

    /// Shortcut for `Value(Timestamp)` — an instant, carried on the wire as an
    /// RFC 3339 string (which is why it types as `string` in the client).
    pub fn timestamp() -> TypeSchema {
        TypeSchema::Value(ValueType::Timestamp)
    }
}
