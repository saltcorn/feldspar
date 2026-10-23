//! The physical schema as the database sees it, plus the schema-change requests
//! that mutate it.
//!
//! Two directions:
//!
//! - [`PhysicalTable`] and friends are the *result* of
//!   [`introspect`](crate::DatabaseDriver::introspect): the live shape of a
//!   table read from `information_schema`. Per the goals there is no table
//!   discovery step — as soon as a database is connected, every table is usable,
//!   so introspection is the source of truth and stored metadata is only an
//!   optional overlay (§5, §9).
//! - [`SchemaChange`] is the *input* to
//!   [`apply_schema`](crate::DatabaseDriver::apply_schema): a single create/drop
//!   table or add/drop column. Notably, creating a table does **not** invent an
//!   `id` column — key fields are declared explicitly like any other (§5).
//!
//! Both model composite primary keys and foreign keys to non-primary-key
//! columns, which the goals require, so keys are always `Vec<String>` and never
//! "the id column".

use serde::{Deserialize, Serialize};

/// A table exactly as it exists in the database, as returned by introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalTable {
    /// The table name (unqualified).
    pub name: String,
    /// The containing schema namespace (e.g. `public`), when the backend has
    /// one. `None` means the driver's default schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Columns in declaration order.
    pub columns: Vec<Column>,
    /// Names of the columns forming the primary key, in key order. Empty when
    /// the table has no primary key; more than one entry is a composite key.
    #[serde(default)]
    pub primary_key: Vec<String>,
    /// Foreign-key constraints declared on this table.
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    /// The unique constraints, indexes and row-constraint triggers on this
    /// table, as the database holds them.
    ///
    /// Read back rather than stored, for the reason the primary key and the
    /// foreign keys are: these are facts the database knows, and a second copy
    /// in an overlay row would be a second answer to the same question (§9). A
    /// constraint somebody added in `psql` is therefore one of these, and shows
    /// up wherever the others do.
    #[serde(default)]
    pub constraints: Vec<PhysicalConstraint>,
}

/// One unique constraint, index or row-constraint trigger, exactly as the
/// database holds it.
///
/// The `comment` is what makes this more than a name: Postgres has nowhere to
/// put the *error message* an admin wrote for a violated constraint, nor the
/// *formula* a row-constraint trigger was generated from, and both are
/// Saltcorn's rather than the backend's. They ride in the object's comment,
/// which is attached to it, dropped with it and carried by `pg_dump` — the one
/// place a comment is the natural store rather than a hiding place. The driver
/// reads it as opaque text; what the text *means* is the catalog's (§9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalConstraint {
    /// The constraint's, index's or trigger's name, unqualified.
    pub name: String,
    /// Which of the three it is.
    pub kind: PhysicalConstraintKind,
    /// The object's comment, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// The three shapes a backend has for what an admin can add to a table.
///
/// Deliberately three rather than four: a full-text search index *is* an index,
/// distinguished only by being over an expression rather than columns, and a
/// backend that reported it as a fourth kind would be deciding a question that
/// belongs to the layer above (§6's rule for types, applied to indexes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PhysicalConstraintKind {
    /// A `UNIQUE` constraint over one or more columns, in key order.
    Unique {
        /// The constrained columns.
        columns: Vec<String>,
    },
    /// An index that no constraint owns — a primary key's and a unique
    /// constraint's own indexes are reported as those constraints, not twice.
    Index {
        /// The indexed columns, in index order; empty for an expression index.
        #[serde(default)]
        columns: Vec<String>,
        /// The indexed expression, for an index that is over one rather than
        /// over columns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
        /// The access method (`btree`, `gin`, …).
        method: String,
    },
    /// A row-level trigger that is not one of the backend's own internal ones —
    /// what a row constraint is implemented as (see the catalog's constraint
    /// module).
    RowTrigger,
}

/// One column of a [`PhysicalTable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// The column name.
    pub name: String,
    /// The backend's own type name (e.g. `text`, `int8`, `timestamptz`). Mapping
    /// this to a rich/basic Saltcorn type is the type layer's job (§6), not the
    /// driver's.
    pub sql_type: String,
    /// Whether the column accepts `NULL`.
    pub nullable: bool,
    /// How the column fills itself in when a write omits it, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<ColumnGenerator>,
}

/// How a column supplies a value when a write does not give it one.
///
/// One enum rather than a `default: Option<String>` beside an `identity: bool`,
/// because a column has **one** of these and never both: Postgres refuses an
/// identity column that also carries a default. The two are the same fact —
/// "the database fills this in" — spelled differently, and the pair that can
/// express nonsense is the pair somebody eventually writes.
///
/// This is what a primary key needs to be usable. A key column nobody can supply
/// a value for is a table no form can insert into, and the goals are explicit
/// that no `id` column is invented — so the key the admin declares has to
/// generate itself, or "create the key field yourself" would mean "and now type
/// a key by hand for every row".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "generator", content = "sql")]
pub enum ColumnGenerator {
    /// The backend numbers the column itself — Postgres's `GENERATED BY DEFAULT
    /// AS IDENTITY`.
    ///
    /// *By default*, never *always*: an explicit value is still accepted, which
    /// is what restoring a backup and importing a CSV that carries its own `id`
    /// column both depend on.
    Identity,
    /// A column default, rendered as backend SQL (`gen_random_uuid()`, `now()`,
    /// `false`).
    Default(String),
}

/// One column of a prepared statement's result, as reported by
/// [`describe`](crate::DatabaseDriver::describe).
///
/// Deliberately not a [`Column`]: a result column has no nullability the backend
/// will commit to and no default — an expression is not a column of a table.
/// What it has is a name and a type, which is exactly what typing a custom
/// query's response needs. As with [`Column::sql_type`], the type is the
/// backend's own name for it; mapping it to a Saltcorn type is the type layer's
/// job (§6), not the driver's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribedColumn {
    /// The column's output name (an alias, or whatever the backend derived).
    pub name: String,
    /// The backend's own type name (e.g. `text`, `int8`, `numeric`).
    pub sql_type: String,
}

/// A foreign-key constraint. Local `columns` reference `referenced_columns` of
/// `referenced_table`, positionally. The referenced columns need not be a
/// primary key (the goals require FKs to non-PK columns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    /// Local columns constrained by this key, in order.
    pub columns: Vec<String>,
    /// The referenced table's (unqualified) name.
    pub referenced_table: String,
    /// The referenced columns, positionally matching `columns`.
    pub referenced_columns: Vec<String>,
}

/// A single schema mutation applied by
/// [`apply_schema`](crate::DatabaseDriver::apply_schema).
///
/// Kept deliberately minimal for the MVP: create/drop a table and add/drop a
/// column. Indexes, constraints, and alterations arrive as the design demands
/// them. Creating a table adds **no** implicit primary-key column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum SchemaChange {
    /// Create a new table with the given columns and optional primary key.
    CreateTable {
        /// The new table's name.
        name: String,
        /// Column definitions, in the order they should be declared.
        columns: Vec<ColumnDef>,
        /// Names of columns forming the primary key (empty for none; more than
        /// one for a composite key). Each must appear in `columns`.
        #[serde(default)]
        primary_key: Vec<String>,
        /// Create the table **without write-ahead logging** — Postgres's
        /// `UNLOGGED` (see
        /// [`unlogged_tables`](crate::DbCapabilities::unlogged_tables)).
        ///
        /// Only ever a request: a backend that does not advertise the capability
        /// creates an ordinary table, because the flag is a performance
        /// property and never a correctness one. Ask for it only where losing
        /// every row on an unclean shutdown is acceptable — the session table
        /// (§7.2) is the case it exists for.
        #[serde(default)]
        unlogged: bool,
    },
    /// Drop an existing table.
    DropTable {
        /// The table to drop.
        name: String,
        /// Suppress an error when the table does not exist.
        #[serde(default)]
        if_exists: bool,
    },
    /// Add a column to an existing table.
    AddColumn {
        /// The table to alter.
        table: String,
        /// The column to add.
        column: ColumnDef,
    },
    /// Drop a column from an existing table.
    DropColumn {
        /// The table to alter.
        table: String,
        /// The column to drop.
        column: String,
        /// Suppress an error when the column does not exist.
        #[serde(default)]
        if_exists: bool,
    },
    /// Rename a column, keeping its data, type and constraints.
    RenameColumn {
        /// The table to alter.
        table: String,
        /// The column's current name.
        from: String,
        /// Its new name.
        to: String,
    },
    /// Make these columns the table's primary key, replacing the key it has.
    ///
    /// A table is created with the key its columns declare, and a table created
    /// with none has none — the goals are explicit that no `id` column is
    /// invented (§5). So the key has to be settable **afterwards**, or a table
    /// would be stuck without one for as long as it existed: the admin adds the
    /// key field to a table that has no key, exactly as they add any other
    /// field, and this is the half of that which the columns cannot express.
    ///
    /// Replacing rather than adding, because "add a second key" is not a thing a
    /// table can have: a composite key grown one column at a time is a sequence
    /// of whole keys, each replacing the last.
    SetPrimaryKey {
        /// The table to alter.
        table: String,
        /// The columns forming the new key, in key order. Each must be a column
        /// of the table, and each must be `NOT NULL`.
        columns: Vec<String>,
    },
    /// Give an existing column a [generator](ColumnGenerator), or take away the
    /// one it has.
    ///
    /// The other half of [`SetPrimaryKey`](SchemaChange::SetPrimaryKey). A key
    /// declared when the column is created gets its generator in the
    /// [`ColumnDef`]; a key switched on afterwards — which is the ordinary way a
    /// table gets one, since none is invented — has a column that already exists,
    /// and this is what turns it into a key that fills itself in. Without it,
    /// ticking the box would produce a key every writer has to type by hand.
    SetColumnGenerator {
        /// The table to alter.
        table: String,
        /// The column to alter.
        column: String,
        /// The generator to give it; `None` removes whatever it has.
        generator: Option<ColumnGenerator>,
    },
    /// Make an existing column accept nulls, or reject them.
    ///
    /// Making a column `NOT NULL` is a check of the data as well as a change of
    /// the declaration: a backend refuses it while any row holds a null there,
    /// and the change fails rather than inventing a value to put in its place.
    SetColumnNullable {
        /// The table to alter.
        table: String,
        /// The column to alter.
        column: String,
        /// `true` drops the `NOT NULL`; `false` adds it.
        nullable: bool,
    },
    /// Make an existing column a foreign key onto another column, repoint the
    /// one it is, or stop it being one.
    ///
    /// The [`ColumnDef::references`] of a column that already exists: every
    /// single-column foreign key on `column` is dropped, whatever it was named,
    /// and — unless `references` is `None` — the new one is added. Replacing
    /// rather than adding, for the reason [`SetPrimaryKey`](SchemaChange::SetPrimaryKey)
    /// replaces: a column that is a `Key` points at one thing, and a second
    /// reference beside the first would be a column no row could satisfy.
    ///
    /// Adding the key is a check of the data as well as a declaration: a row
    /// whose value is not in the new target makes the backend refuse the change.
    SetColumnReference {
        /// The table to alter.
        table: String,
        /// The referencing column.
        column: String,
        /// What it references from now on, or `None` for nothing.
        references: Option<ColumnRef>,
    },
    /// Add a `UNIQUE` constraint over one or more columns — a table-level
    /// constraint, which is what "jointly unique" needs and what
    /// [`ColumnDef::unique`] cannot express.
    AddUniqueConstraint {
        /// The table to alter.
        table: String,
        /// The constraint's name. Given rather than left to the backend,
        /// because it is the handle the constraint is later dropped by and the
        /// name a violation reports.
        name: String,
        /// The columns that are jointly unique, in order.
        columns: Vec<String>,
    },
    /// Drop a named table constraint.
    DropConstraint {
        /// The table to alter.
        table: String,
        /// The constraint to drop.
        name: String,
        /// Suppress an error when it does not exist.
        #[serde(default)]
        if_exists: bool,
    },
    /// Create an index over columns or over an expression.
    CreateIndex {
        /// The table to index.
        table: String,
        /// The index's name.
        name: String,
        /// What is indexed.
        on: IndexOn,
        /// The access method, when it is not the backend's default (`gin` for a
        /// full-text index). `None` leaves the backend to pick.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    /// Drop an index by name.
    DropIndex {
        /// The index to drop.
        name: String,
        /// Suppress an error when it does not exist.
        #[serde(default)]
        if_exists: bool,
    },
    /// Set or remove the comment on a constraint, an index or a trigger.
    ///
    /// The comment is where a constraint's Saltcorn metadata lives — the error
    /// message and, for a row constraint, the formula it was generated from (see
    /// [`PhysicalConstraint::comment`]). It is a schema change like any other so
    /// that it joins the same transaction as the object it describes: a
    /// constraint that committed without its message would be a constraint
    /// whose violation says the wrong thing.
    SetComment {
        /// What is being commented on.
        target: CommentTarget,
        /// The comment text, or `None` to remove it.
        comment: Option<String>,
    },
}

/// What a [`SchemaChange::CreateIndex`] indexes.
///
/// The expression form is what a full-text index needs (`to_tsvector(…)` over
/// several columns at once). As with [`ColumnDef::sql_type`], it is a
/// **structural SQL fragment** built by trusted code from the catalog, never
/// text a caller sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "on", content = "value")]
pub enum IndexOn {
    /// One or more columns, in index order.
    Columns(Vec<String>),
    /// A single expression.
    Expression(String),
}

/// The object a [`SchemaChange::SetComment`] is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "on")]
pub enum CommentTarget {
    /// A table constraint, which is named relative to its table.
    Constraint {
        /// The table the constraint is on.
        table: String,
        /// The constraint's name.
        name: String,
    },
    /// An index, which is named in its own right.
    Index {
        /// The index's name.
        name: String,
    },
    /// A trigger, which is named relative to its table.
    Trigger {
        /// The table the trigger is on.
        table: String,
        /// The trigger's name.
        name: String,
    },
}

/// The definition of a column to create, used inside a [`SchemaChange`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    /// The column name.
    pub name: String,
    /// The backend SQL type for the column (e.g. `text`, `uuid`, `int8`).
    pub sql_type: String,
    /// Whether the column accepts `NULL`.
    pub nullable: bool,
    /// How the column should fill itself in when a write omits it, if at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<ColumnGenerator>,
    /// Whether a `UNIQUE` constraint should be attached to the column.
    #[serde(default)]
    pub unique: bool,
    /// The column this one references, if it is a foreign key.
    ///
    /// Single-column only, which is what a `REFERENCES` clause on the column
    /// itself can express. A composite foreign key is a table-level constraint
    /// and will need its own [`SchemaChange`] when something needs one; nothing
    /// does yet, and inventing the general form here would mean two ways to
    /// declare the simple case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<ColumnRef>,
}

/// A reference to one column of one table — the target of a single-column
/// foreign key (see [`ColumnDef::references`]).
///
/// The target need not be a primary key; it must only be unique, which the
/// goals require (a `Key` field "holds the value of a referenced field, not
/// necessarily the target PK").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnRef {
    /// The referenced table's (unqualified) name.
    pub table: String,
    /// The referenced column.
    pub column: String,
}

impl ColumnDef {
    /// A nullable column of the given name and SQL type, with no default and no
    /// unique constraint — the base to tweak fields on.
    pub fn new(name: impl Into<String>, sql_type: impl Into<String>) -> Self {
        ColumnDef {
            name: name.into(),
            sql_type: sql_type.into(),
            nullable: true,
            generated: None,
            unique: false,
            references: None,
        }
    }

    /// Mark this column `NOT NULL`.
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Attach a `UNIQUE` constraint to this column.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Set the column default (backend SQL).
    pub fn default(mut self, sql: impl Into<String>) -> Self {
        self.generated = Some(ColumnGenerator::Default(sql.into()));
        self
    }

    /// Have the backend number the column — see
    /// [`ColumnGenerator::Identity`].
    pub fn identity(mut self) -> Self {
        self.generated = Some(ColumnGenerator::Identity);
        self
    }

    /// Make this column a foreign key onto `table`.`column`.
    pub fn references(mut self, table: impl Into<String>, column: impl Into<String>) -> Self {
        self.references = Some(ColumnRef {
            table: table.into(),
            column: column.into(),
        });
        self
    }
}
