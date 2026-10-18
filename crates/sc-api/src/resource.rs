//! The **resource model**: what a table looks like to a generated consumer
//! (design §13.1).
//!
//! An [`Endpoint`](crate::Endpoint) says how a request is made and what its body
//! is *shaped* like; it deliberately says nothing about the table behind it,
//! because most endpoints have none. A table-backed API has four endpoints per
//! table which are only meaningful together — list, create, update, delete are
//! one *thing* with four verbs, over rows whose columns are known — and a client
//! generated from the endpoints alone can only type those rows as `unknown`.
//!
//! A [`ResourceModel`] is that missing half, carried in the
//! [`EndpointSet`](crate::EndpointSet) beside the endpoints and serialized with
//! them: the table's columns and their wire types, which of them a write may
//! set, which are keys into another resource (so an embedded `?select=` can be
//! typed), and the names of the endpoints each operation is performed by. The
//! TypeScript generator turns one resource into one row interface and one client
//! object with methods, rather than four loose methods with `unknown` in their
//! signatures.
//!
//! Nothing here restates what an endpoint already knows: the model *names* its
//! endpoints rather than copying their paths or auth, so the two cannot drift —
//! an operation whose endpoint the projection did not register (a keyless table
//! has no `update`) is simply absent.

use crate::schema::ValueType;
use serde::{Deserialize, Serialize};

/// One column of a [`ResourceModel`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceField {
    /// The column name — the JSON key and the generated property name.
    pub name: String,
    /// The type its values are carried as on the wire.
    pub ty: ValueType,
    /// The column is `NOT NULL`: it is never `null` in a row, and an insert that
    /// omits it fails unless the database fills it in
    /// ([`has_default`](ResourceField::has_default)).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// The database fills the column in when a write omits it (an identity key,
    /// a `DEFAULT`), so an insert may leave it out even when it is `required`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_default: bool,
    /// The column is (part of) the primary key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primary_key: bool,
    /// The column is **read-only**: a calculated field, which the row layer
    /// refuses to be written (`reject_calc_writes`), so it appears in a row and
    /// in no write body.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The resource this column is a foreign key into, when it is one *and* that
    /// table is projected too. A key into a table the API does not expose is
    /// left `None`: its rows have no type here, and an embed of it cannot be
    /// typed as anything but unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<String>,
}

impl ResourceField {
    /// A plain, nullable, writable column.
    pub fn new(name: impl Into<String>, ty: ValueType) -> ResourceField {
        ResourceField {
            name: name.into(),
            ty,
            required: false,
            has_default: false,
            primary_key: false,
            read_only: false,
            references: None,
        }
    }

    /// Whether a write body may set this column.
    pub fn writable(&self) -> bool {
        !self.read_only
    }

    /// Whether an insert may omit this column.
    pub fn optional_on_insert(&self) -> bool {
        !self.required || self.has_default
    }
}

/// A file-valued column and the two endpoints that move its bytes (§4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceFile {
    /// The column holding the file's path.
    pub field: String,
    /// The endpoint name that reads the bytes.
    pub download: String,
    /// The endpoint name that writes them.
    pub upload: String,
}

/// The endpoints one resource's operations are performed by, each `None` when
/// the projection registered no endpoint for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceOps {
    /// Read many rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    /// Insert a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<String>,
    /// Replace a row, addressed by primary key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    /// Delete a row, addressed by primary key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<String>,
    /// The file columns, with their download/upload endpoints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ResourceFile>,
}

/// A table as a generated client sees it: its rows' shape, plus the endpoints
/// that read and write them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceModel {
    /// The table name. Also the property the resource appears under on the
    /// generated client, and the cache key the generated hooks use.
    pub name: String,
    /// The columns, in table order.
    pub fields: Vec<ResourceField>,
    /// The single-column primary key rows are addressed by, when the table has
    /// one. A table without it gets no row-addressed operations at all — the
    /// same rule the projection applies to its endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<String>,
    /// The endpoints behind each operation.
    pub ops: ResourceOps,
    /// The table is synced (§14.11): a client created with `offline` keeps it on
    /// the device.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synced: bool,
}

impl ResourceModel {
    /// An empty model for `name`, to be filled in with the builder methods.
    pub fn new(name: impl Into<String>) -> ResourceModel {
        ResourceModel {
            name: name.into(),
            fields: Vec::new(),
            primary_key: None,
            ops: ResourceOps::default(),
            synced: false,
        }
    }

    /// Add a column.
    pub fn field(mut self, field: ResourceField) -> ResourceModel {
        self.fields.push(field);
        self
    }

    /// Look a column up by name.
    pub fn get(&self, name: &str) -> Option<&ResourceField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The primary key column, when the table has a single-column one.
    pub fn key_field(&self) -> Option<&ResourceField> {
        self.primary_key.as_deref().and_then(|pk| self.get(pk))
    }

    /// Every endpoint name this resource speaks for — what the client generator
    /// must *not* also emit as a loose method, since it appears as one of the
    /// resource's own.
    pub fn endpoint_names(&self) -> impl Iterator<Item = &str> {
        let ops = &self.ops;
        [&ops.list, &ops.create, &ops.update, &ops.delete]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain(
                ops.files
                    .iter()
                    .flat_map(|f| [f.download.as_str(), f.upload.as_str()]),
            )
    }
}
