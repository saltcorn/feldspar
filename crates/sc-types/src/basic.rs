//! Basic types: the MVP type system.
//!
//! A [`BasicType`] is a database type Saltcorn knows how to move a [`Value`]
//! through, but which carries no rich attributes or validation beyond "the value
//! is of the right scalar family" (technical design §6.1). The milestone ships
//! **only** basic types — rich types (typed attributes, dedicated fieldviews)
//! are added incrementally later.
//!
//! Each known variant corresponds one-to-one with a [`Value`] variant, so the
//! driver's `sql_type` string (a Postgres `udt_name` such as `int8`, `text`,
//! `timestamptz`) can be resolved to the [`Value`] family it round-trips as, and
//! back to a canonical SQL type for DDL. A DB type we do not recognise is still
//! *usable* — it becomes [`BasicType::Other`] and flows through the catch-all
//! display/edit path (see [`crate::catchall`]) as text, exactly as the design
//! requires ("a Basic type is any DB type not mapped to a rich type").

use sc_error::{Error, Result};
use sc_query::Value;

use crate::geometry::{self, GeometryKind};

/// A database type the MVP understands, as one of a fixed set of scalar families
/// (each mirroring a [`Value`] variant) plus an [`Other`](BasicType::Other)
/// catch-all for unrecognised backend types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasicType {
    /// Boolean (`bool`) — [`Value::Bool`].
    Bool,
    /// 64-bit signed integer (`int2`/`int4`/`int8`) — [`Value::Int`].
    Int,
    /// IEEE-754 floating point (`float4`/`float8`) — [`Value::Float`].
    Float,
    /// Exact fixed-point decimal (`numeric`) — [`Value::Decimal`].
    Decimal,
    /// UTF-8 text (`text`/`varchar`/`bpchar`) — [`Value::Text`].
    Text,
    /// Raw bytes (`bytea`) — [`Value::Bytes`].
    Bytes,
    /// JSON document (`json`/`jsonb`) — [`Value::Json`].
    Json,
    /// UUID (`uuid`) — [`Value::Uuid`].
    Uuid,
    /// Calendar date (`date`) — [`Value::Date`].
    Date,
    /// Wall-clock time (`time`) — [`Value::Time`].
    Time,
    /// Instant in time (`timestamptz`/`timestamp`) — [`Value::Timestamp`].
    Timestamp,
    /// A geometry in WGS84 (PostGIS `geometry(<kind>,4326)`), carried as a
    /// GeoJSON geometry object in a [`Value::Json`] (analytics TODO A5.1, see
    /// [`crate::geometry`]).
    Geometry(GeometryKind),
    /// A backend type not mapped to any of the above. Still usable, but only
    /// through the catch-all display/edit path, where it is treated as text. The
    /// wrapped string is the backend's own type name so it survives round-trips.
    Other(String),
}

impl BasicType {
    /// Resolve a backend SQL type name (a Postgres `udt_name`) to a basic type.
    ///
    /// The match is case-insensitive and covers the common Postgres aliases. Any
    /// type not in the table becomes [`BasicType::Other`] carrying the original
    /// name — never an error, because every column must remain usable.
    pub fn from_sql_type(sql_type: &str) -> BasicType {
        if let Some(kind) = GeometryKind::of_sql_type(sql_type) {
            return BasicType::Geometry(kind);
        }
        match sql_type.trim().to_ascii_lowercase().as_str() {
            "bool" | "boolean" => BasicType::Bool,
            "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" | "serial"
            | "bigserial" | "smallserial" => BasicType::Int,
            "float4" | "float8" | "real" | "double precision" => BasicType::Float,
            "numeric" | "decimal" => BasicType::Decimal,
            "text" | "varchar" | "character varying" | "bpchar" | "char" | "character" | "name"
            | "citext" => BasicType::Text,
            "bytea" => BasicType::Bytes,
            "json" | "jsonb" => BasicType::Json,
            "uuid" => BasicType::Uuid,
            "date" => BasicType::Date,
            "time" | "timetz" | "time without time zone" | "time with time zone" => BasicType::Time,
            "timestamp"
            | "timestamptz"
            | "timestamp without time zone"
            | "timestamp with time zone" => BasicType::Timestamp,
            other => BasicType::Other(other.to_owned()),
        }
    }

    /// The canonical Postgres type this maps to for DDL (`apply_schema`).
    ///
    /// Recognised variants collapse their aliases to one preferred name (e.g.
    /// every integer width becomes `int8`); [`Other`](BasicType::Other) yields
    /// the backend name it was constructed from.
    pub fn sql_type(&self) -> &str {
        match self {
            BasicType::Bool => "bool",
            BasicType::Int => "int8",
            BasicType::Float => "float8",
            BasicType::Decimal => "numeric",
            BasicType::Text => "text",
            BasicType::Bytes => "bytea",
            BasicType::Json => "jsonb",
            BasicType::Uuid => "uuid",
            BasicType::Date => "date",
            BasicType::Time => "time",
            BasicType::Timestamp => "timestamptz",
            BasicType::Geometry(kind) => kind.sql_type(),
            BasicType::Other(name) => name,
        }
    }

    /// A stable, human-readable name. For recognised variants this equals the
    /// corresponding [`Value::kind`]; [`Other`](BasicType::Other) reports its
    /// backend type name.
    pub fn name(&self) -> &str {
        match self {
            BasicType::Bool => "bool",
            BasicType::Int => "int",
            BasicType::Float => "float",
            BasicType::Decimal => "decimal",
            BasicType::Text => "text",
            BasicType::Bytes => "bytes",
            BasicType::Json => "json",
            BasicType::Uuid => "uuid",
            BasicType::Date => "date",
            BasicType::Time => "time",
            BasicType::Timestamp => "timestamp",
            BasicType::Geometry(kind) => kind.type_name(),
            BasicType::Other(name) => name,
        }
    }

    /// The inverse of [`name`](BasicType::name): the type a stable name means.
    ///
    /// [`name`](BasicType::name) is what a declaration written as *data* carries
    /// — a stream's element key says `"float"`, not `"float8"` — and until this
    /// existed there was no way back. [`from_sql_type`](BasicType::from_sql_type)
    /// is not that inverse: it answers a backend's `udt_name`, so `"int"` and
    /// `"float"` fall through it into [`Other`](BasicType::Other), which is the
    /// quiet wrong answer rather than an error.
    ///
    /// The SQL aliases are still accepted, so one function reads either
    /// vocabulary, and an unrecognised name still becomes
    /// [`Other`](BasicType::Other) carrying it — a name is never rejected here,
    /// because whoever asked is the one who knows whether an unknown type is a
    /// problem.
    pub fn from_name(name: &str) -> BasicType {
        let lower = name.trim().to_ascii_lowercase();
        if let Some(kind) = GeometryKind::of_type_name(&lower) {
            return BasicType::Geometry(kind);
        }
        match lower.as_str() {
            "int" => BasicType::Int,
            "float" => BasicType::Float,
            "bytes" => BasicType::Bytes,
            other => BasicType::from_sql_type(other),
        }
    }

    /// The [`Value::kind`] a non-null value of this type must have, or `None`
    /// for [`Other`](BasicType::Other) (which accepts text only via the
    /// catch-all path but imposes no `Value`-family constraint here).
    pub fn value_kind(&self) -> Option<&'static str> {
        Some(match self {
            BasicType::Bool => "bool",
            BasicType::Int => "int",
            BasicType::Float => "float",
            BasicType::Decimal => "decimal",
            BasicType::Text => "text",
            BasicType::Bytes => "bytes",
            BasicType::Json => "json",
            BasicType::Uuid => "uuid",
            BasicType::Date => "date",
            BasicType::Time => "time",
            BasicType::Timestamp => "timestamp",
            BasicType::Geometry(_) => "json",
            BasicType::Other(_) => return None,
        })
    }

    /// The basic type a [`Value`] belongs to, or `None` for [`Value::Null`]
    /// (which carries no type of its own). This is the reverse of the SQL-type
    /// mapping: it answers "what column type could hold this value".
    pub fn of_value(value: &Value) -> Option<BasicType> {
        Some(match value {
            Value::Null => return None,
            Value::Bool(_) => BasicType::Bool,
            Value::Int(_) => BasicType::Int,
            Value::Float(_) => BasicType::Float,
            Value::Decimal(_) => BasicType::Decimal,
            Value::Text(_) => BasicType::Text,
            Value::Bytes(_) => BasicType::Bytes,
            Value::Json(_) => BasicType::Json,
            Value::Uuid(_) => BasicType::Uuid,
            Value::Date(_) => BasicType::Date,
            Value::Time(_) => BasicType::Time,
            Value::Timestamp(_) => BasicType::Timestamp,
        })
    }

    /// Whether a value is compatible with this type. [`Value::Null`] is always
    /// accepted (nullability is a field-level concern, not a type one); an
    /// [`Other`](BasicType::Other) type accepts only text.
    pub fn accepts(&self, value: &Value) -> bool {
        if value.is_null() {
            return true;
        }
        if let (BasicType::Geometry(kind), Value::Json(json)) = (self, value) {
            return geometry::check_geojson(*kind, json).is_ok();
        }
        match self.value_kind() {
            Some(kind) => value.kind() == kind,
            // `Other` imposes no family constraint beyond "not a structured
            // value"; the catch-all path renders/edits it as text.
            None => matches!(value, Value::Text(_)),
        }
    }

    /// Whether a **JSON** value is compatible with this type — the check for a
    /// value that arrived as JSON rather than as a [`Value`]: an [`Attrs`](crate::Attrs)
    /// entry, which is what a [`FormField`](crate::FormField) describes when it
    /// declares a configurable extension's settings (§13.3).
    ///
    /// JSON represents three of the families natively (bool, number, string), so
    /// those are matched strictly: a `Bool` setting wants `true`, not `"true"`.
    /// The rest have no JSON form and travel as strings, so they are checked by
    /// parsing them exactly the way [`catchall::parse`](crate::catchall::parse)
    /// parses a form input. A [`Json`](BasicType::Json) setting takes any shape at
    /// all — that is what asking for JSON means.
    ///
    /// `null` is accepted by every type, as [`Value::Null`] is by
    /// [`accepts`](BasicType::accepts): it means "no value", and whether that is
    /// allowed is a requiredness question, not a type one.
    pub fn accepts_json(&self, json: &serde_json::Value) -> bool {
        use serde_json::Value as Json;
        if matches!(self, BasicType::Json) {
            return true;
        }
        if let BasicType::Geometry(kind) = self {
            return json.is_null() || geometry::check_geojson(*kind, json).is_ok();
        }
        match json {
            Json::Null => true,
            Json::Bool(_) => matches!(self, BasicType::Bool),
            Json::Number(n) => match self {
                BasicType::Int => n.is_i64(),
                BasicType::Float | BasicType::Decimal => true,
                _ => false,
            },
            Json::String(s) => match self {
                // Text takes any string; `Other` is edited as text (see
                // `accepts`).
                BasicType::Text | BasicType::Other(_) => true,
                // Natively representable in JSON, so a string is the wrong
                // shape — not something to coerce.
                BasicType::Bool | BasicType::Int | BasicType::Float => false,
                // No JSON form: a string encoding, checked by parsing it.
                _ => crate::catchall::parse(self, s).is_ok(),
            },
            // No basic type is a list or an object; that is what `Json` is for.
            Json::Array(_) | Json::Object(_) => false,
        }
    }

    /// Validate that `value` is compatible with this type, returning a
    /// descriptive [`Error::Invalid`] when it is not (principle 5: no silent
    /// coercion).
    pub fn validate(&self, value: &Value) -> Result<()> {
        // A geometry says what is wrong with it, not only that it is wrong.
        if let (BasicType::Geometry(kind), Value::Json(json)) = (self, value) {
            return geometry::check_geojson(*kind, json);
        }
        if self.accepts(value) {
            Ok(())
        } else {
            Err(Error::invalid(format!(
                "value of kind `{}` is not compatible with type `{}`",
                value.kind(),
                self.name()
            )))
        }
    }
}
