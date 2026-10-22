//! Fields and the identifiers that tie them together.
//!
//! The technical design (§6.2) separates a **database** field ([`DataField`] —
//! a column in a table) from a [**form** field](sc_types::FormField), and shares
//! their common shape in [`BaseField`]. The MVP models `DataField` fully —
//! including the `Key` and `File` kinds, even though the admin UI only exercises
//! `Plain` — so the data model is right from the start (calculated fields are
//! post-MVP; see the crate root).
//!
//! Only `DataField` and the identifiers live here. `BaseField` and `FormField`
//! are in `sc-types` beside [`Attrs`], which they carry and describe. What keeps
//! `DataField` up here is its [`Key`](DataFieldKind::Key)/[`File`](DataFieldKind::File)
//! kinds, which reference catalog identifiers, and its `Column`/`ColumnDef`
//! bridge — none of which `BaseField` ever needed. All three are re-exported
//! from this crate, so a caller reaching for `sc_catalog::BaseField` still finds
//! it.

use sc_db::{Column, ColumnDef, ColumnGenerator};
use sc_types::TypeRef;

/// Type-specific attributes carried by a field or table, always a JSON object
/// (technical design §6, §9: "a sparse value goes into `attributes`").
///
/// Defined in `sc-types` beside [`FormField`](sc_types::FormField), which
/// describes one entry of it (§6.1), and re-exported here because fields and
/// tables are where the catalog meets it.
pub use sc_types::Attrs;

/// Properties shared by every field (technical design §6.2).
///
/// Defined in `sc-types` beside [`FormField`](sc_types::FormField), the other
/// half of §6.2's split, and re-exported here beside [`DataField`].
pub use sc_types::BaseField;

/// Identifies a table within the catalog. For the MVP — which stores no metadata
/// beyond `information_schema` — a table's stable identity is simply its name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableId(pub String);

/// Identifies a field within a table. As with [`TableId`], the MVP identity is
/// the field (column) name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldId(pub String);

/// Identifies a connected database. The MVP has a single **primary** database
/// (see [`DbId::primary`]); the newtype exists so multi-database support slots in
/// later without changing field signatures.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DbId(pub String);

/// Identifies a connected file store, referenced by [`DataFieldKind::File`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileStoreId(pub String);

impl DbId {
    /// The single primary database of the MVP: the one that additionally hosts
    /// the `users` and (post-MVP) `_fd_*` metadata tables.
    pub fn primary() -> DbId {
        DbId("primary".to_owned())
    }
}

/// A column in a database table (technical design §6.2).
#[derive(Debug, Clone, PartialEq)]
pub struct DataField {
    /// The shared field properties.
    pub base: BaseField,
    /// Whether the column is `NOT NULL`.
    pub required: bool,
    /// Whether the column carries a `UNIQUE` constraint.
    pub unique: bool,
    /// Whether the column is part of the table's primary key (possibly
    /// composite).
    pub primary_key: bool,
    /// How the column fills itself in when a write omits it, if it does.
    ///
    /// A database fact and never an overlay one — introspection reads it back
    /// off the column ([`from_column`](DataField::from_column)), so what the
    /// admin UI says about a key ("this one numbers itself") is what the column
    /// actually does rather than what creating it was supposed to do.
    pub generated: Option<ColumnGenerator>,
    /// A JavaScript expression that supplies the value when an insert leaves
    /// this field out (or sends `null`), if one is set.
    ///
    /// An overlay fact, unlike [`generated`](DataField::generated): the database
    /// knows nothing of it, and the row layer evaluates it before the `INSERT`,
    /// with `user` (the writing user) and the row's other values in scope. Only
    /// a `NOT NULL` field may carry one (`schema_edit` refuses it elsewhere).
    pub default_expression: Option<String>,
    /// What the field references, if anything.
    pub kind: DataFieldKind,
}

/// What a [`DataField`] holds beyond a plain scalar (technical design §6.2).
///
/// `Eq` as well as `PartialEq`: every variant's fields are identifiers and
/// strings, so two kinds compare exactly. (The enclosing [`DataField`] is only
/// `PartialEq`, because its [`BaseField`] carries an [`Attrs`] bag — but the kind
/// itself has no such value, and [`FieldMeta`](crate::FieldMeta) relies on this
/// to be `Eq`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataFieldKind {
    /// An ordinary scalar column.
    Plain,
    /// A foreign key: holds the value of a referenced field (not necessarily the
    /// target's primary key), with an optional summary field used as the default
    /// label when selecting.
    Key {
        /// The referenced table.
        target_table: TableId,
        /// The referenced field.
        target_field: FieldId,
        /// A field of the target used as a human label, if chosen.
        summary_field: Option<FieldId>,
    },
    /// A file reference: a relative path within a named file store, optionally
    /// restricted to a folder and/or MIME types.
    File {
        /// The file store the path is relative to.
        store: FileStoreId,
        /// A folder within the store the file must live under, if restricted.
        folder: Option<String>,
        /// Allowed MIME types (empty = any).
        mime_allow: Vec<String>,
        /// What happens to the stored file when its row is deleted.
        on_delete: FileOnDelete,
        /// What an upload does when a file of the same name is already there.
        on_conflict: FileOnConflict,
    },
    /// A **non-stored calculated field** (TODO Phase 8): no column on disk, only
    /// an `sc-expr` expression computed on read over the row's own fields,
    /// Ⱶ-join paths, Ↄ-aggregations and *other calculated fields on the same
    /// table* (never `user`/operation flags — a calc field has no caller). Such
    /// a field exists purely in the overlay: [`Table::apply_field_overlay`] adds
    /// it as a virtual field (there is no introspected column to overlay), and it
    /// emits no DDL — it is not writable.
    Calc {
        /// The JavaScript expression, validated at merge time.
        expression: String,
    },
    /// A **secret** (TODO-imap M2): a `Text` column whose values are stored
    /// encrypted under the instance key ([`crate::secrets`]), as envelopes.
    ///
    /// Server-side code — actions, `{{ }}` templates, code bodies, module
    /// actions and services — reads the plaintext. Every surface a client
    /// reads — REST, GraphQL, live updates, the admin data editor — gets
    /// [`SECRET_SENTINEL`](sc_types::SECRET_SENTINEL) when there is a value
    /// and `null` when there is none, and may write a new value (write-only).
    /// A secret can be no part of a key, an index, a unique constraint, a
    /// filter or an ordering.
    Secret,
}

/// What a `File` field does with its stored file when the row is deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileOnDelete {
    /// The file stays in its store (the default): other rows, or nothing at all,
    /// may still want it.
    #[default]
    Keep,
    /// The file is removed from its store once the delete has committed, unless
    /// another row of the table still references the same path.
    RemoveFile,
}

impl FileOnDelete {
    /// The stored and wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            FileOnDelete::Keep => "keep",
            FileOnDelete::RemoveFile => "remove_file",
        }
    }

    /// Parse the stored and wire spelling; an empty string is the default.
    pub fn parse(s: &str) -> Option<FileOnDelete> {
        match s.trim() {
            "" | "keep" => Some(FileOnDelete::Keep),
            "remove_file" => Some(FileOnDelete::RemoveFile),
            _ => None,
        }
    }
}

/// What an upload into a `File` field does when the path it would land at is
/// already taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileOnConflict {
    /// The upload replaces the file that is there (the default).
    #[default]
    Overwrite,
    /// The upload gets the first free name of `name_1.ext`, `name_2.ext`, ….
    Suffix,
}

impl FileOnConflict {
    /// The stored and wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            FileOnConflict::Overwrite => "overwrite",
            FileOnConflict::Suffix => "suffix",
        }
    }

    /// Parse the stored and wire spelling; an empty string is the default.
    pub fn parse(s: &str) -> Option<FileOnConflict> {
        match s.trim() {
            "" | "overwrite" => Some(FileOnConflict::Overwrite),
            "suffix" => Some(FileOnConflict::Suffix),
            _ => None,
        }
    }
}

impl DataField {
    /// A plain, nullable, non-unique, non-key column of the given name and type —
    /// the base to tweak with the builder methods below.
    pub fn plain(name: impl Into<String>, type_: TypeRef) -> DataField {
        DataField {
            base: BaseField::new(name, type_),
            required: false,
            unique: false,
            primary_key: false,
            generated: None,
            default_expression: None,
            kind: DataFieldKind::Plain,
        }
    }

    /// Set the human label.
    pub fn label(mut self, label: impl Into<String>) -> DataField {
        self.base.label = label.into();
        self
    }

    /// Mark the column `NOT NULL`.
    pub fn required(mut self) -> DataField {
        self.required = true;
        self
    }

    /// Attach a `UNIQUE` constraint.
    pub fn unique(mut self) -> DataField {
        self.unique = true;
        self
    }

    /// Make the column part of the primary key.
    pub fn primary_key(mut self) -> DataField {
        self.primary_key = true;
        self
    }

    /// Have the column fill itself in — see [`DataField::generated`].
    pub fn generated(mut self, generator: ColumnGenerator) -> DataField {
        self.generated = Some(generator);
        self
    }

    /// Give the field a default-value expression — see
    /// [`DataField::default_expression`].
    pub fn default_expression(mut self, expression: impl Into<String>) -> DataField {
        self.default_expression = Some(expression.into());
        self
    }

    /// Whether an insert may leave this field out and still get a value: the
    /// column fills itself in, or a default expression supplies one.
    pub fn has_default(&self) -> bool {
        self.generated.is_some() || self.default_expression.is_some()
    }

    /// This field's calculated-field expression, if it is one (Phase 8).
    pub fn calc_expression(&self) -> Option<&str> {
        match &self.kind {
            DataFieldKind::Calc { expression } => Some(expression),
            _ => None,
        }
    }

    /// Whether this is a [`Secret`](DataFieldKind::Secret) field.
    pub fn is_secret(&self) -> bool {
        matches!(self.kind, DataFieldKind::Secret)
    }

    /// Whether this is a non-stored calculated field.
    pub fn is_calc(&self) -> bool {
        matches!(self.kind, DataFieldKind::Calc { .. })
    }

    /// Whether this field holds a collaborative document (`collab_doc`, live
    /// updates L5.1): never read or written through an API, never named by a
    /// filter, an ordering or a formula.
    pub fn is_collab_doc(&self) -> bool {
        sc_types::is_collab_doc(&self.base.type_)
    }

    /// The column definition this field emits when the table or column is
    /// created via the catalog. Primary-key membership is applied at the table
    /// level (in the `CREATE TABLE`), not here.
    ///
    /// A [`Key`](DataFieldKind::Key) field emits a real foreign key. That is the
    /// direction that closes the loop: introspection *derives* `Key` from a
    /// foreign key ([`Table::from_physical`](crate::Table::from_physical)), so a
    /// `Key` field that created a plain column would be silently downgraded to
    /// `Plain` the moment the catalog reloaded — the field would claim a
    /// relationship the database did not have, and nothing would enforce it.
    ///
    /// A [`File`](DataFieldKind::File) field emits none: its target is a path in
    /// a file store, which is not a table and has nothing to constrain against.
    pub fn to_column_def(&self) -> ColumnDef {
        let mut col = ColumnDef::new(self.base.name.clone(), self.base.type_.sql_type());
        if self.required {
            col = col.not_null();
        }
        col.generated = self.generated.clone();
        if self.unique {
            col = col.unique();
        }
        if let DataFieldKind::Key {
            target_table,
            target_field,
            ..
        } = &self.kind
        {
            col = col.references(target_table.0.clone(), target_field.0.clone());
        }
        col
    }

    /// Build a data field from an introspected [`Column`]. Nullability comes from
    /// the column; uniqueness is not reported by MVP introspection and defaults
    /// to `false`. The caller supplies primary-key membership and the kind, both
    /// of which are table-level facts.
    pub fn from_column(col: &Column, primary_key: bool, kind: DataFieldKind) -> DataField {
        DataField {
            base: BaseField {
                name: col.name.clone(),
                label: col.name.clone(),
                type_: TypeRef::from_sql_type(&col.sql_type),
                attributes: Attrs::new(),
            },
            required: !col.nullable,
            unique: false,
            primary_key,
            generated: col.generated.clone(),
            default_expression: None,
            kind,
        }
    }
}
