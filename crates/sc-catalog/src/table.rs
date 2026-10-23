//! [`Table`]: a table as the catalog presents it.
//!
//! A `Table` is pure data — its id, name, database, fields, primary key, access
//! rules, and attributes — built either from introspection
//! ([`Table::from_physical`]) or from a create request. The *behaviour* of
//! serving rows lives in a [`TableProvider`](crate::TableProvider), which the
//! [`Catalog`](crate::Catalog) constructs on demand; [`TableSource`] records
//! which kind of provider backs the table so that identity stays serialisable
//! and comparable (technical design §8.2).

use std::collections::BTreeSet;

use sc_db::PhysicalTable;
use sc_types::{BaseField, BasicType, RichTypeRef, TypeRef};

use crate::constraint::TableConstraint;
use crate::field::{Attrs, DataField, DataFieldKind, DbId, FieldId, TableId};
use crate::field_meta::FieldMeta;
use crate::provider::ProvidedWrites;
use crate::table_meta::{TableMeta, TableMetaId};

/// A way in which a stored `_fd_fields` overlay row did **not** cleanly apply to
/// the introspected field it names (design §3.2).
///
/// The merge never fails and never silently downgrades: a row that cannot be
/// honoured leaves the field as introspection produced it — still usable — and
/// records why here, for the admin UI to surface. The two reasons are a dangling
/// row (the column, or its table, is gone) and a rich type that does not fit the
/// column's SQL type. Each is exactly what a hand-edited database or a restored
/// dump can produce, and each is the admin's to fix, not a bug to crash on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMergeIssue {
    /// The table the overlay row named.
    pub table: String,
    /// The field the overlay row named.
    pub field: String,
    /// A human-readable explanation of why the row did not apply.
    pub message: String,
}

/// Which kind of provider serves a table's rows (technical design §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableSource {
    /// Backed directly by a table in the connected database `database`.
    Database,
    /// Served by a **table provider** a module supplies: the rows come from
    /// JavaScript rather than from a database, and the table exists because a
    /// `_fd_tables` row says so rather than because introspection found it
    /// (see [`ProvidedTableDef`](crate::ProvidedTableDef)).
    ///
    /// Both names are carried because a call has to reach the worker that
    /// *module* was loaded on; the provider name alone is not an address.
    Provider {
        /// The package name — `@saltcorn/rss`.
        module: String,
        /// The provider's own name within it — `RSS feed`.
        provider: String,
        /// Which of v1's three write methods `get_table` answered for *this*
        /// table's configuration, asked at the last catalog reload.
        ///
        /// Carried on the table rather than asked at write time because the
        /// admin UI draws its buttons from it, and a button with nothing behind
        /// it is a screen that lies. The write path checks again on the far
        /// side; see [`ProvidedWrites`](crate::ProvidedWrites).
        writes: ProvidedWrites,
    },
}

impl TableSource {
    /// Which writes the provider serving this table answers — none at all for a
    /// database table, whose writes do not go through a provider's methods.
    pub fn provided_writes(&self) -> ProvidedWrites {
        match self {
            TableSource::Database => ProvidedWrites::NONE,
            TableSource::Provider { writes, .. } => *writes,
        }
    }

    /// The provider serving this table, or `None` for a database table.
    pub fn provider(&self) -> Option<(&str, &str)> {
        match self {
            TableSource::Database => None,
            TableSource::Provider {
                module, provider, ..
            } => Some((module, provider)),
        }
    }
}

/// Per-CRUD access control for a table (technical design §8.2). Roles run 1–100
/// with **lower = more privileged** (1 = admin, 100 = public), so a `min_role` is
/// the least-privileged role number still allowed: a user with `role <= min_role`
/// passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRules {
    /// Least-privileged role that may read rows.
    pub min_role_read: u8,
    /// Least-privileged role that may create/update/delete rows.
    pub min_role_write: u8,
}

impl Default for AccessRules {
    /// Admin-only, matching the MVP admin UI: only role 1 (admin) may read or
    /// write. An introspected table carries no overlay metadata, so this default
    /// applies until `_fd_tables` overlays arrive (post-MVP).
    fn default() -> Self {
        AccessRules {
            min_role_read: 1,
            min_role_write: 1,
        }
    }
}

/// A table as the catalog knows it (technical design §8.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Stable identity (the table name in the MVP).
    pub id: TableId,
    /// The (unqualified) table name.
    pub name: String,
    /// The database that hosts it.
    pub database: DbId,
    /// Which kind of provider serves its rows.
    pub source: TableSource,
    /// Its fields, in declaration order. A composite or absent primary key is
    /// allowed.
    pub fields: Vec<DataField>,
    /// Names of the primary-key columns, in key order (empty for none; more than
    /// one for a composite key).
    pub primary_key: Vec<String>,
    /// A human label. Defaults to the table's own name; an overlay row may
    /// replace it.
    pub label: String,
    /// A human description; empty unless an overlay row gives one.
    pub description: String,
    /// Per-CRUD access rules.
    pub access: AccessRules,
    /// Table-level attributes (JSON object).
    pub attributes: Attrs,
    /// The overlay row this table's configuration came from, if it has one.
    ///
    /// `None` is the ordinary case and the important one: it means "nobody has
    /// configured this table", which is exactly how a freshly connected database
    /// looks and must keep looking (§9). It is recorded rather than inferred so
    /// that saving a change updates the existing row instead of racing to create
    /// a second one for the same table.
    pub overlay: Option<TableMetaId>,
    /// The table's ownership formula (§7.3), parsed at merge time and — when it
    /// passed the catalog-wide validation in [`Catalog::reload`] — ready for
    /// the evaluators. `None` when no formula is stored **or** when the stored
    /// one failed to parse or validate: a broken formula **grants nothing**
    /// (fail closed), and [`ownership_error`](Table::ownership_error) says why.
    ///
    /// [`Catalog::reload`]: crate::Catalog::reload
    pub ownership: Option<sc_expr::Formula>,
    /// Why the stored ownership formula is not in effect, when it is not — a
    /// parse or validation failure reported on the table itself, so the admin
    /// UI can show it exactly where the formula is edited. `None` when there is
    /// no formula or the formula is live.
    pub ownership_error: Option<String>,
    /// Whether row-level security is enabled for this table (§7.3; enforcement
    /// is Phase 6's — until then the flag is stored and surfaced, not acted on).
    pub rls_enabled: bool,
    /// The table's constraints: its jointly-unique keys, its indexes and its row
    /// constraints (see [`constraint`](crate::constraint)).
    ///
    /// Read back from the database rather than stored, which is why it sits here
    /// beside `primary_key` and not in the overlay: a constraint *is* a fact of
    /// the schema (§9's rule), so a `UNIQUE` added in `psql` is in this list and
    /// nothing Saltcorn stores can disagree with it.
    pub constraints: Vec<TableConstraint>,
}

impl Table {
    /// Build a catalog table from an introspected [`PhysicalTable`].
    ///
    /// Primary-key membership is taken from the physical key; a column that is
    /// the sole local column of a foreign key becomes a
    /// [`Key`](DataFieldKind::Key) field (with no summary field, which is overlay
    /// metadata), and every other column is [`Plain`](DataFieldKind::Plain).
    pub fn from_physical(database: DbId, physical: &PhysicalTable) -> Table {
        let pk: BTreeSet<&str> = physical.primary_key.iter().map(String::as_str).collect();
        let fields = physical
            .columns
            .iter()
            .map(|col| {
                let is_pk = pk.contains(col.name.as_str());
                let kind = single_column_fk_target(physical, &col.name)
                    .map(|(table, field)| DataFieldKind::Key {
                        target_table: TableId(table),
                        target_field: FieldId(field),
                        summary_field: None,
                    })
                    .unwrap_or(DataFieldKind::Plain);
                DataField::from_column(col, is_pk, kind)
            })
            .collect();
        Table {
            id: TableId(physical.name.clone()),
            name: physical.name.clone(),
            database,
            source: TableSource::Database,
            fields,
            primary_key: physical.primary_key.clone(),
            label: physical.name.clone(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: physical
                .constraints
                .iter()
                .filter_map(TableConstraint::from_physical)
                .collect(),
        }
    }

    /// A table that does not exist yet, as a schema edit **projects** it (Phase
    /// 7): the shape the batch will leave in the database, before any DDL has
    /// been issued.
    ///
    /// This is the same value [`from_physical`](Table::from_physical) will
    /// produce once the batch commits and the catalog reloads — which is the
    /// point: an operation later in the batch resolves against it, so a key
    /// pointing at a table created three operations earlier validates without a
    /// round trip to a database that has not seen either yet.
    pub fn projected(
        database: DbId,
        name: impl Into<String>,
        fields: Vec<DataField>,
        primary_key: Vec<String>,
    ) -> Table {
        let name = name.into();
        Table {
            id: TableId(name.clone()),
            label: name.clone(),
            name,
            database,
            source: TableSource::Database,
            fields,
            primary_key,
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    /// A **provided** table: one whose rows come from a module's table provider
    /// rather than from a database (§8.3).
    ///
    /// There is no `from_physical` for one of these, because there is no
    /// physical table — the `_fd_tables` row *is* the table, and the fields are
    /// whatever the provider answered when it was asked. `database` is still
    /// the primary: a provided table is not in any database, but every
    /// [`DbId`](crate::DbId) in the system names a connection an admin
    /// configured, and inventing a fictional one would put a name in the
    /// tables list that no Connections screen could explain.
    ///
    /// The caller applies the overlay afterwards, exactly as it does to an
    /// introspected table — the same row carries the label, the description and
    /// the access rules.
    pub fn provided(
        database: DbId,
        name: impl Into<String>,
        module: impl Into<String>,
        provider: impl Into<String>,
        fields: Vec<DataField>,
        writes: ProvidedWrites,
    ) -> Table {
        let name = name.into();
        let primary_key = fields
            .iter()
            .filter(|f| f.primary_key)
            .map(|f| f.base.name.clone())
            .collect();
        Table {
            id: TableId(name.clone()),
            label: name.clone(),
            name,
            database,
            source: TableSource::Provider {
                module: module.into(),
                provider: provider.into(),
                writes,
            },
            fields,
            primary_key,
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    /// The module and provider serving this table's rows, or `None` when a
    /// database does.
    pub fn provider(&self) -> Option<(&str, &str)> {
        self.source.provider()
    }

    /// Which of v1's three write methods this table's provider answers.
    /// [`NONE`](ProvidedWrites::NONE) for a table in a database — its writes are
    /// the driver's, not a provider's.
    pub fn provided_writes(&self) -> ProvidedWrites {
        self.source.provided_writes()
    }

    /// Apply an overlay row to this table (technical design §9, TODO §1.2).
    ///
    /// **The precedence rule, stated once:** the database is the authority on
    /// everything it knows — columns, types, nullability, keys, the primary key
    /// — and the overlay is the authority on everything *it* knows — access
    /// rules, label, description, attributes. The two sets do not intersect, so
    /// there is no contested value and no conflict semantics to get wrong. That
    /// is a constraint on what may ever be added to `_fd_tables`, not merely a
    /// description of what it holds today: a `nullable` column there would break
    /// this method's correctness, which is why `table_meta`'s column list is
    /// asserted in full by a test rather than spot-checked.
    ///
    /// A **system table takes an overlay only as a metadata table** — a row an
    /// admin wrote to show it (see [`table_meta`](crate::table_meta)). Any other
    /// row for one is refused by `save_table_meta`, so it only exists if it was
    /// inserted behind the API's back, and the answer there is to ignore it.
    ///
    /// An empty label or description in the row means "none given", so the
    /// table's own name stays its label rather than becoming blank.
    pub fn apply_overlay(&mut self, meta: &TableMeta) {
        if self.is_system() && !meta.is_metadata_table() {
            return;
        }
        if !meta.label.is_empty() {
            self.label = meta.label.clone();
        }
        if !meta.description.is_empty() {
            self.description = meta.description.clone();
        }
        self.access = meta.access.clone();
        self.attributes = meta.attributes.clone();
        self.overlay = Some(meta.id);
        self.rls_enabled = meta.rls_enabled();

        // Parse the stored ownership formula here; *validate* it in
        // `Catalog::reload`, which sees the whole schema (a Ⱶ-path crosses
        // tables, so one table cannot check its own formula). A source that
        // does not parse fails closed now: no formula, and the reason on the
        // table for the admin UI to show.
        match meta.ownership_formula().map(sc_expr::Formula::parse) {
            None => {
                self.ownership = None;
                self.ownership_error = None;
            }
            Some(Ok(formula)) => {
                self.ownership = Some(formula);
                self.ownership_error = None;
            }
            Some(Err(e)) => {
                self.ownership = None;
                self.ownership_error = Some(e.to_string());
            }
        }
    }

    /// Apply a field overlay row to the matching field of this table (design
    /// §3.2), returning any way in which it did not cleanly apply.
    ///
    /// **Same precedence rule as [`apply_overlay`](Table::apply_overlay), one
    /// level down:** the database is the authority on what it knows (the column's
    /// SQL type, its nullability, whether a foreign key stands behind it), and the
    /// overlay is the authority on what *it* knows (a label, a rich type, a field
    /// kind's extra parameters, attributes). Where the two would contradict, the
    /// database wins and the contradiction is *reported*, never resolved by
    /// downgrading the column or by crashing — the table stays usable.
    ///
    /// The rules that make that concrete:
    ///
    /// - A **rich type** applies only if its `sql_types()` includes the column's
    ///   actual SQL type. `text` configured as `Integer` is reported and the
    ///   column stays its basic type. An unregistered type name is likewise
    ///   reported (it may belong to a plugin not loaded here).
    /// - A **`Key`** overlay's target is the database's whenever the database has
    ///   one: on a column introspection made a key from a foreign key, the
    ///   overlay may add only its `summary_field` and can never repoint the
    ///   reference. On a column with *no* foreign key, the overlay supplies the
    ///   whole reference — the case the database cannot enforce, such as a key
    ///   onto a field in another database (a future capability), where the
    ///   overlay is the only place the target can come from.
    /// - A **`File`** overlay applies to any column (its storage is `text`); it
    ///   is a reference the database does not model, so there is nothing to
    ///   contradict.
    /// - `label` and `attributes` are the overlay's to set and are applied even
    ///   when a type or kind issue is also reported — the field stays usable and
    ///   the admin is told what did not take.
    ///
    /// A **system table never takes a field overlay**, matching
    /// [`apply_overlay`](Table::apply_overlay): `save_field_meta` refuses to write
    /// one, so this only fires on a row inserted behind the API, and the answer is
    /// to ignore it.
    pub fn apply_field_overlay(&mut self, meta: &FieldMeta) -> Vec<FieldMergeIssue> {
        if self.is_system() {
            return Vec::new();
        }
        let mut issues = Vec::new();
        let existing = self
            .fields
            .iter()
            .position(|f| f.base.name == meta.field_name);
        let idx = match (existing, &meta.kind) {
            // A calculated field has no column: the overlay *introduces* it as a
            // virtual field (Phase 8). Its expression is validated catalog-wide
            // in `Catalog::reload`, like an ownership formula.
            (None, DataFieldKind::Calc { .. }) => {
                self.fields.push(virtual_calc_field(meta));
                return issues;
            }
            // A calc overlay whose name collides with a real column would shadow
            // it — refused, the column stays as introspected.
            (Some(_), DataFieldKind::Calc { .. }) => {
                issues.push(self.field_issue(
                    meta,
                    "is a calculated field but a real column of that name exists",
                ));
                return issues;
            }
            (Some(idx), _) => idx,
            (None, _) => {
                issues.push(self.field_issue(meta, "refers to a column that does not exist"));
                return issues;
            }
        };

        // Overlay-owned, always applied: a label (empty means "none given") and
        // the field's own attributes.
        if !meta.label.is_empty() {
            self.fields[idx].base.label = meta.label.clone();
        }
        self.fields[idx].base.attributes = meta.attributes.clone();

        // A default-value expression, if it parses. Validating it against the
        // schema is the save's job (`schema_edit`); one that no longer parses is
        // reported and dropped, so an insert fails on the column rather than on
        // a formula nobody can fix from the row.
        self.fields[idx].default_expression = None;
        if let Some(expression) = &meta.default_expression {
            match sc_expr::Formula::parse(expression) {
                Ok(_) => self.fields[idx].default_expression = Some(expression.clone()),
                Err(e) => issues.push(
                    self.field_issue(meta, &format!("has a default expression that fails: {e}")),
                ),
            }
        }

        // A rich type, if it fits the column's SQL type.
        if let Some(type_name) = &meta.type_name {
            let column_sql = self.fields[idx].base.type_.sql_type().to_owned();
            match RichTypeRef::resolve(type_name) {
                Ok(rich) if rich.rich_type().sql_types().contains(&column_sql.as_str()) => {
                    self.fields[idx].base.type_ = TypeRef::Rich(rich);
                }
                Ok(_) => issues.push(self.field_issue(
                    meta,
                    &format!(
                        "is configured as rich type `{type_name}`, which does not apply to a \
                         `{column_sql}` column"
                    ),
                )),
                Err(_) => issues.push(self.field_issue(
                    meta,
                    &format!("names rich type `{type_name}`, which is not registered"),
                )),
            }
        }

        // A field kind. `File` applies freely; `Plain` changes nothing; `Key`
        // depends on whether the database enforces the reference.
        match &meta.kind {
            DataFieldKind::Plain => {}
            // Handled above (a calc overlay on an existing column is refused), so
            // this is unreachable — named for exhaustiveness.
            DataFieldKind::Calc { .. } => {}
            DataFieldKind::File { .. } => self.fields[idx].kind = meta.kind.clone(),
            // A secret holds envelopes, which are text: on any other column the
            // overlay is refused rather than applied, so a value is never read
            // as a secret that could not have been written as one.
            DataFieldKind::Secret => {
                if self.fields[idx].base.type_.as_basic() == Some(&sc_types::BasicType::Text) {
                    self.fields[idx].kind = DataFieldKind::Secret;
                } else {
                    issues.push(self.field_issue(
                        meta,
                        "is configured as a secret, which applies only to a text column",
                    ));
                }
            }
            DataFieldKind::Key { summary_field, .. } => {
                // Read the database's target first, so the assignment below does
                // not borrow the field it writes.
                let db_target = match &self.fields[idx].kind {
                    DataFieldKind::Key {
                        target_table,
                        target_field,
                        ..
                    } => Some((target_table.clone(), target_field.clone())),
                    _ => None,
                };
                self.fields[idx].kind = match db_target {
                    // The database enforces this foreign key, so it is the
                    // authority on the target: the overlay may add only the
                    // summary field, never repoint the reference.
                    Some((target_table, target_field)) => DataFieldKind::Key {
                        target_table,
                        target_field,
                        summary_field: summary_field.clone(),
                    },
                    // No foreign key behind the column: the overlay supplies the
                    // whole reference. This is the case the database *cannot*
                    // enforce — a key onto a field in another database (a future
                    // capability) — so the overlay is the only place its target
                    // can come from.
                    None => meta.kind.clone(),
                };
            }
        }
        issues
    }

    /// Build a [`FieldMergeIssue`] naming this table and the overlay's field.
    fn field_issue(&self, meta: &FieldMeta, what: &str) -> FieldMergeIssue {
        FieldMergeIssue {
            table: self.name.clone(),
            field: meta.field_name.clone(),
            message: format!("field `{}.{}` {what}", self.name, meta.field_name),
        }
    }

    /// Whether this is a system table (`_fd_*`), hidden from users (technical
    /// design §9). No such tables exist in the MVP, but the check is defined so
    /// callers can filter consistently.
    pub fn is_system(&self) -> bool {
        self.name.starts_with("_fd_")
    }

    /// Whether this is a system table an admin has added to the tables list as
    /// a **metadata table**: its rows and settings are theirs to edit, its
    /// schema is still Saltcorn's.
    pub fn is_metadata(&self) -> bool {
        self.is_system() && crate::table_meta::is_metadata_flag(&self.attributes)
    }

    /// Whether offline sync records this table's changes (§14.11): its
    /// overlay's `attributes.sync.enabled`.
    pub fn sync_enabled(&self) -> bool {
        crate::sync::sync_enabled(&self.attributes)
    }

    /// Whether row history records this table's changes (§14.12): its
    /// overlay's `attributes.history.enabled`.
    pub fn history_enabled(&self) -> bool {
        crate::change_log::history_enabled(&self.attributes)
    }

    /// Whether this table has the change-log trigger: sync or history is on.
    pub fn change_log_enabled(&self) -> bool {
        crate::change_log::change_log_enabled(&self.attributes)
    }

    /// The `File` fields whose file goes with the row
    /// (`on_delete: remove_file`), as `(column, store)`.
    pub fn files_removed_on_delete(&self) -> Vec<(&str, &str)> {
        self.fields
            .iter()
            .filter_map(|field| match &field.kind {
                crate::DataFieldKind::File {
                    store,
                    on_delete: crate::FileOnDelete::RemoveFile,
                    ..
                } => Some((field.base.name.as_str(), store.0.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Whether this table is kept out of the user-facing lists: a system table
    /// nobody has added as a metadata table. What [`is_system`](Table::is_system)
    /// alone still decides is that the schema is not editable.
    pub fn is_hidden(&self) -> bool {
        self.is_system() && !self.is_metadata()
    }

    /// The field with the given name, if present.
    pub fn field(&self, name: &str) -> Option<&DataField> {
        self.fields.iter().find(|f| f.base.name == name)
    }

    /// This table's non-stored calculated fields (Phase 8), in dependency order
    /// — each after the calc fields it reads, as [`Catalog::reload`] left them.
    /// The read path fills them in this order; the write path refuses them.
    ///
    /// [`Catalog::reload`]: crate::Catalog::reload
    pub fn calc_fields(&self) -> impl Iterator<Item = &DataField> {
        self.fields.iter().filter(|f| f.is_calc())
    }

    /// This table's calc-field expressions parsed into a map, for the translator
    /// to **inline** where an ownership formula (or another calc field) names one
    /// (Phase 8). The sources validated at merge time, so a re-parse cannot fail
    /// in practice; a stray failure simply omits that field from the map.
    pub fn calc_formulas(&self) -> sc_expr::CalcFields {
        self.calc_fields()
            .filter_map(|f| {
                let expr = f.calc_expression()?;
                sc_expr::Formula::parse(expr)
                    .ok()
                    .map(|formula| (f.base.name.clone(), formula))
            })
            .collect()
    }
}

/// A virtual [`DataField`] for a non-stored calculated overlay: no column, no
/// key, not required. Its declared type is the overlay's rich type when one is
/// given (a calc field has no column to check `sql_types()` against, so it is
/// taken as-is) and `Text` otherwise — the value's real type comes from the
/// expression at read time.
fn virtual_calc_field(meta: &FieldMeta) -> DataField {
    let type_ = match &meta.type_name {
        Some(name) => RichTypeRef::resolve(name)
            .map(TypeRef::Rich)
            .unwrap_or(TypeRef::Basic(BasicType::Text)),
        None => TypeRef::Basic(BasicType::Text),
    };
    let label = if meta.label.is_empty() {
        meta.field_name.clone()
    } else {
        meta.label.clone()
    };
    DataField {
        base: BaseField {
            name: meta.field_name.clone(),
            label,
            type_,
            attributes: meta.attributes.clone(),
        },
        required: false,
        unique: false,
        primary_key: false,
        // No column, so nothing fills it in: a calculated field is computed on
        // read.
        generated: None,
        default_expression: None,
        kind: meta.kind.clone(),
    }
}

/// If `column` is the single local column of some foreign key on `physical`,
/// return the `(referenced_table, referenced_column)` it points at.
fn single_column_fk_target(physical: &PhysicalTable, column: &str) -> Option<(String, String)> {
    physical.foreign_keys.iter().find_map(|fk| {
        match (fk.columns.as_slice(), fk.referenced_columns.as_slice()) {
            ([local], [referenced]) if local == column => {
                Some((fk.referenced_table.clone(), referenced.clone()))
            }
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::FileStoreId;
    use sc_types::BasicType;

    /// A two-column `books` table: a `text` `title` and a `text` `cover`.
    fn books() -> Table {
        Table {
            id: TableId("books".into()),
            name: "books".into(),
            database: DbId::primary(),
            source: TableSource::Database,
            fields: vec![
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("cover", TypeRef::Basic(BasicType::Text)),
            ],
            primary_key: vec![],
            label: "books".into(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    #[test]
    fn an_overlay_with_an_ownership_formula_parses_it() {
        let mut table = books();
        let mut meta = TableMeta::new("books");
        meta.set_ownership_formula(Some("title === user.id"));
        meta.set_rls_enabled(true);
        table.apply_overlay(&meta);
        assert_eq!(
            table.ownership.as_ref().map(|f| f.source()),
            Some("title === user.id")
        );
        assert!(table.ownership_error.is_none());
        assert!(table.rls_enabled);

        // Clearing the formula clears it on the next merge too.
        meta.set_ownership_formula(None);
        meta.set_rls_enabled(false);
        table.apply_overlay(&meta);
        assert!(table.ownership.is_none());
        assert!(!table.rls_enabled);
    }

    #[test]
    fn an_unparseable_stored_formula_fails_closed_with_the_reason() {
        // What a hand-edited attributes blob can hold. The formula grants
        // nothing (None) and the reason rides on the table for the admin UI.
        let mut table = books();
        let mut meta = TableMeta::new("books");
        meta.attributes.insert(
            crate::table_meta::ATTR_OWNERSHIP_FORMULA.into(),
            serde_json::Value::String("owner === ((".into()),
        );
        table.apply_overlay(&meta);
        assert!(table.ownership.is_none());
        let err = table.ownership_error.as_deref().unwrap();
        assert!(err.contains("parse error"), "got: {err}");
    }

    #[test]
    fn a_fitting_rich_type_and_attributes_apply() {
        let mut table = books();
        let mut meta = FieldMeta::new("books", "title")
            .label("Title")
            .rich_type("string");
        meta.attributes.insert("max_length".into(), 200.into());

        let issues = table.apply_field_overlay(&meta);
        assert!(issues.is_empty(), "{issues:?}");

        let field = table.field("title").unwrap();
        assert_eq!(field.base.label, "Title");
        assert_eq!(field.base.type_.name(), "string");
        assert_eq!(field.base.attributes.get("max_length"), Some(&200.into()));
    }

    #[test]
    fn a_rich_type_that_does_not_fit_the_column_is_reported_and_the_column_stays_basic() {
        // `text` configured as `Integer` — what a hand-edited database produces.
        let mut table = books();
        let meta = FieldMeta::new("books", "title").rich_type("integer");

        let issues = table.apply_field_overlay(&meta);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("integer"), "{:?}", issues[0]);
        assert!(issues[0].message.contains("text"), "{:?}", issues[0]);
        // The field is still usable, still its basic type.
        assert_eq!(
            table.field("title").unwrap().base.type_,
            TypeRef::Basic(BasicType::Text)
        );
    }

    #[test]
    fn an_unregistered_rich_type_is_reported() {
        let mut table = books();
        let meta = FieldMeta::new("books", "title").rich_type("colour");
        let issues = table.apply_field_overlay(&meta);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("colour"), "{:?}", issues[0]);
        assert!(
            issues[0].message.contains("not registered"),
            "{:?}",
            issues[0]
        );
    }

    #[test]
    fn a_file_kind_applies_to_any_column() {
        let mut table = books();
        let meta = FieldMeta::new("books", "cover").kind(DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: Some("covers".into()),
            mime_allow: vec![],
            on_delete: Default::default(),
            on_conflict: Default::default(),
        });
        let issues = table.apply_field_overlay(&meta);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(matches!(
            table.field("cover").unwrap().kind,
            DataFieldKind::File { .. }
        ));
    }

    #[test]
    fn a_key_overlay_on_a_plain_column_supplies_the_whole_reference() {
        // No foreign key behind the column: the overlay is the only place a
        // target can come from — the case the database cannot enforce (e.g. a key
        // into another database). It applies rather than being rejected.
        let mut table = books();
        let key = DataFieldKind::Key {
            target_table: TableId("authors".into()),
            target_field: FieldId("id".into()),
            summary_field: Some(FieldId("name".into())),
        };
        let meta = FieldMeta::new("books", "title").kind(key.clone());
        let issues = table.apply_field_overlay(&meta);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(table.field("title").unwrap().kind, key);
    }

    #[test]
    fn a_key_overlay_adds_only_the_summary_field_atop_a_real_foreign_key() {
        // The column already is a key from introspection; the overlay keeps the
        // database's target and adds only the summary field.
        let mut table = books();
        table.fields[0].kind = DataFieldKind::Key {
            target_table: TableId("authors".into()),
            target_field: FieldId("id".into()),
            summary_field: None,
        };
        let meta = FieldMeta::new("books", "title").kind(DataFieldKind::Key {
            // A different target here must be ignored — the database is authority.
            target_table: TableId("wrong".into()),
            target_field: FieldId("wrong".into()),
            summary_field: Some(FieldId("name".into())),
        });

        let issues = table.apply_field_overlay(&meta);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            table.field("title").unwrap().kind,
            DataFieldKind::Key {
                target_table: TableId("authors".into()),
                target_field: FieldId("id".into()),
                summary_field: Some(FieldId("name".into())),
            }
        );
    }

    #[test]
    fn an_overlay_for_a_missing_column_is_reported() {
        let mut table = books();
        let meta = FieldMeta::new("books", "ghost").rich_type("string");
        let issues = table.apply_field_overlay(&meta);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("does not exist"),
            "{:?}",
            issues[0]
        );
    }

    #[test]
    fn a_system_table_ignores_a_field_overlay() {
        let mut table = books();
        table.name = "_fd_secret".into();
        let meta = FieldMeta::new("_fd_secret", "title").rich_type("string");
        assert!(table.apply_field_overlay(&meta).is_empty());
        assert_eq!(
            table.field("title").unwrap().base.type_,
            TypeRef::Basic(BasicType::Text)
        );
    }
}
