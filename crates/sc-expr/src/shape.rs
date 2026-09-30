//! How the catalog describes tables *to* this crate.
//!
//! `sc-expr` is a generic library below `sc-catalog` in the dependency graph, so
//! it cannot see `Table` or `DataFieldKind`. Instead the caller projects the
//! little a formula needs — which fields exist, which of them are keys and where
//! they point, what fields the user object carries — into a [`SchemaShape`].
//! Phase 4 builds one from the catalog; tests build them by hand.

use std::collections::{BTreeMap, BTreeSet};

use crate::analyze::Ambient;

/// The tables a formula may reach: the formula's own table plus every table
/// reachable through Ⱶ-join paths, keyed by table name — plus which ambient
/// objects are in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaShape {
    /// Per-table field shapes.
    pub tables: BTreeMap<String, TableShape>,
    /// The [`Ambient`] objects in scope, each with its fields when the caller
    /// knows them (`None` = "the caller does not know", which skips membership
    /// checks rather than rejecting every access).
    ///
    /// Presence is scope: a shape declares `user` always (every formula has a
    /// caller — [`Default`] puts it here), and `row`/`old` only where a trigger's
    /// formula may see them. An ambient object that is *not* declared is not a
    /// special identifier at all, so an ownership formula naming `row` gets the
    /// unknown-identifier error it deserves.
    pub ambient: BTreeMap<Ambient, Option<BTreeSet<String>>>,
    /// The **module functions** in scope (§4b), keyed by the function's own
    /// name and carrying every module that supplies it.
    ///
    /// A `Vec` rather than a `String` because v1 lets two modules supply
    /// `geocode_lat` and nothing here pretends otherwise: a formula has no
    /// spelling for "the one from that module", so an ambiguous name is
    /// **refused on save** naming both — which it can only do if both are here.
    ///
    /// Empty by default, which is a server with no modules: a formula naming
    /// `md_to_html` there is the unknown identifier it has always been.
    pub module_functions: BTreeMap<String, Vec<String>>,
}

impl Default for SchemaShape {
    /// No tables, and `user` in scope with unknown fields — the base language.
    fn default() -> SchemaShape {
        SchemaShape {
            tables: BTreeMap::new(),
            ambient: BTreeMap::from([(Ambient::User, None)]),
            module_functions: BTreeMap::new(),
        }
    }
}

/// One table's fields, keyed by field name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableShape {
    /// Field name → shape.
    pub fields: BTreeMap<String, FieldShape>,
    /// The table's primary-key column, when known. Aggregations (Phase 7) need
    /// it in two places: `length` counts a non-null column, and `maxBy`/`minBy`
    /// break selector ties on it so both evaluators pick the same child row.
    /// `None` means the caller did not declare it — an aggregation that needs
    /// it then fails to translate rather than guessing.
    pub primary_key: Option<String>,
    /// Whose rows these are, when this "table" is not a table of the database
    /// but a query whose rows **correspond** to one: a dataset stage (analytics
    /// TODO A1.2). `None` for every real table.
    ///
    /// What it buys is aggregations over child tables. `ordersↃcustomer` on a
    /// stage aggregated by `customer` has no `customers` table to resolve
    /// against, but each of its rows *is* a customer, identified by the stage's
    /// `customer` column. [`RowsOf`] says exactly that, and an inverse relation
    /// whose key targets `customers.id` then correlates on the stage's
    /// `customer` column instead of on a column the stage does not have.
    pub rows_of: Option<RowsOf>,
}

/// That a [`TableShape`]'s rows are rows of another table: `table`'s rows,
/// identified by the value of `table.field`, which this shape holds in its
/// `column`. See [`TableShape::rows_of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsOf {
    /// The table the rows correspond to.
    pub table: String,
    /// The field of `table` that identifies a row — what a child's key targets.
    pub field: String,
    /// The column of *this* shape holding that field's value.
    pub column: String,
}

/// What a formula needs to know about one field. A struct rather than a bare
/// `Option<KeyShape>` so later phases can add to it without touching every
/// constructor. (Phase 2's GUC casts ended up not needing a type here — the
/// user field→type map rides in `UserEnv::Guc`, since only that mode wants
/// types and only for the user object.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldShape {
    /// `Some` when this is a Key field a Ⱶ-path may traverse.
    pub key: Option<KeyShape>,
}

/// Where a Key field points: the link a Ⱶ-path segment follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyShape {
    /// The referenced table.
    pub target_table: String,
    /// The referenced field on that table.
    pub target_field: String,
}

impl SchemaShape {
    /// An empty shape, to add tables to.
    pub fn new() -> SchemaShape {
        SchemaShape::default()
    }

    /// Add a table, replacing any previous shape under the same name.
    pub fn table(mut self, name: impl Into<String>, shape: TableShape) -> SchemaShape {
        self.tables.insert(name.into(), shape);
        self
    }

    /// Declare the user object's fields (enables `user.x` validation).
    pub fn user_fields<I, S>(self, fields: I) -> SchemaShape
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ambient_fields(Ambient::User, Some(fields))
    }

    /// Put `ambient` in scope, with its fields when they are known.
    ///
    /// `None` declares the object in scope but its fields unchecked;
    /// `Some(fields)` also enables the membership check on every `ambient.x`.
    pub fn ambient_fields<I, S>(mut self, ambient: Ambient, fields: Option<I>) -> SchemaShape
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ambient.insert(
            ambient,
            fields.map(|f| f.into_iter().map(Into::into).collect()),
        );
        self
    }

    /// Declare that `module` supplies a function called `name`.
    ///
    /// Additive, and the order modules are declared in is the order an
    /// ambiguity names them in — which is the module set's own order, so the
    /// message an admin reads twice reads the same twice.
    pub fn module_function(
        mut self,
        name: impl Into<String>,
        module: impl Into<String>,
    ) -> SchemaShape {
        self.module_functions
            .entry(name.into())
            .or_default()
            .push(module.into());
        self
    }

    /// The modules supplying `name`, or an empty slice when nothing does.
    pub fn modules_supplying(&self, name: &str) -> &[String] {
        self.module_functions
            .get(name)
            .map_or(&[] as &[String], Vec::as_slice)
    }

    /// Whether `ambient` is in scope for a formula validated against this shape.
    pub fn declares_ambient(&self, ambient: Ambient) -> bool {
        self.ambient.contains_key(&ambient)
    }

    /// The declared fields of `ambient`, or `None` when it is out of scope or its
    /// fields are unknown — either way, there is nothing to check a member
    /// against.
    pub fn ambient_field_set(&self, ambient: Ambient) -> Option<&BTreeSet<String>> {
        self.ambient.get(&ambient)?.as_ref()
    }

    /// The key fields pointing *at* `table`: every `(child_table, key_field)`
    /// pair in the shape whose Key targets `table`. This is what an inverse
    /// relation `childↃkey` (Phase 7) resolves against — derived from the child
    /// tables' [`KeyShape`]s rather than a parallel index that could drift, so
    /// the caller need only *include* the candidate child tables in the shape.
    /// The result is sorted (by child table, then key field) for a stable order.
    pub fn incoming(&self, table: &str) -> Vec<(&str, &str)> {
        let mut out = Vec::new();
        for (child, shape) in &self.tables {
            for (name, field) in &shape.fields {
                if let Some(key) = &field.key
                    && key.target_table == table
                {
                    out.push((child.as_str(), name.as_str()));
                }
            }
        }
        out
    }
}

impl TableShape {
    /// An empty table shape, to add fields to.
    pub fn new() -> TableShape {
        TableShape::default()
    }

    /// Add a plain (non-key) field.
    pub fn field(mut self, name: impl Into<String>) -> TableShape {
        self.fields.insert(name.into(), FieldShape::default());
        self
    }

    /// Declare the primary-key column (needed for aggregations over this table
    /// as a child — `length`'s count and `maxBy`/`minBy`'s tie-break).
    pub fn primary_key(mut self, name: impl Into<String>) -> TableShape {
        self.primary_key = Some(name.into());
        self
    }

    /// Declare that this shape's rows are rows of `table`, identified by
    /// `table.field` whose value this shape holds in `column` (see
    /// [`RowsOf`]).
    pub fn rows_of(
        mut self,
        table: impl Into<String>,
        field: impl Into<String>,
        column: impl Into<String>,
    ) -> TableShape {
        self.rows_of = Some(RowsOf {
            table: table.into(),
            field: field.into(),
            column: column.into(),
        });
        self
    }

    /// Add a Key field pointing at `target_table.target_field`.
    pub fn key_field(
        mut self,
        name: impl Into<String>,
        target_table: impl Into<String>,
        target_field: impl Into<String>,
    ) -> TableShape {
        self.fields.insert(
            name.into(),
            FieldShape {
                key: Some(KeyShape {
                    target_table: target_table.into(),
                    target_field: target_field.into(),
                }),
            },
        );
        self
    }
}
