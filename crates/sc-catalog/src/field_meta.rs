//! The `_fd_fields` overlay: its schema, bootstrap, and the [`FieldMeta`] ⇄ row
//! mapping (design §9).
//!
//! The field-level twin of [`table_meta`](crate::table_meta). Everything the
//! module docs there say about an **overlay** holds here, sharper: a column
//! exists in the database whether or not this table has a row for it, so
//! everything stored here *adds* to what introspection yields — a rich type, a
//! label, a field kind, sparse attributes — and nothing restates a fact the
//! column already carries (its SQL type, its nullability). That is what keeps
//! §9's "legacy databases just work" true one level down.
//!
//! Three shape decisions distinguish this table from `_fd_tables`:
//!
//! - **The natural key is composite.** A table overlay is keyed by one name; a
//!   field overlay by the pair `(table_name, field_name)`, since a field name is
//!   only unique within its table. That pair is the **primary key**, which is
//!   what enforces "one overlay row per field" at the database — the authority,
//!   because two admins saving concurrently cannot see each other's transaction.
//!   §9 requires an `id` (UUID) column but does not require it to *be* the key,
//!   so `id` is kept as a required, unique, stable row handle (used by
//!   [`save_field_meta`]/[`load_field_meta`]/[`delete_field_meta`]) while the
//!   composite key does the uniqueness work.
//! - **The field's kind is a discriminant plus sparse parameters.** A `kind`
//!   text column says `plain`/`key`/`file`; the kind's parameters (a `File`'s
//!   store, folder and MIME list; a `Key`'s target) live in the `attributes`
//!   column, not in a column apiece. `Key` and `File` have disjoint parameters
//!   and more kinds are expected, so a column per parameter would grow a nullable
//!   column forever — exactly what §9's "a sparse value goes into `attributes`"
//!   rule exists to avoid. On read the kind's parameter keys are lifted back out
//!   of the bag and whatever remains is the field's own (rich-type) attributes;
//!   those key names are therefore reserved within a field's attributes, which is
//!   harmless in practice (a `File`/`Key` field carries a basic type with no rich
//!   attributes to collide).
//! - **The rich type is a name, resolved elsewhere.** A `type` column holds a
//!   registered rich type's name or NULL; it is *not* validated against the
//!   registry here (storage stays inert — a type supplied later by a plugin must
//!   still round-trip). §3.2's merge resolves it and reports a mismatch; §3.3's
//!   API validates it on create.
//!
//! **Strict reads**, as in `_fd_tables`: a missing or ill-typed column — or a
//! `kind` that is not a known discriminant — is an [`Error::invalid`] naming the
//! field and what was wrong, never a silent default.

use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::catalog::Catalog;
use crate::field::{
    DataField, DataFieldKind, FieldId, FileOnConflict, FileOnDelete, FileStoreId, TableId,
};
use crate::file_stores::QUERY_FILE_STORES;
use crate::table::Table;

/// Name of the field-overlay table in the primary database.
pub const FIELD_META_TABLE: &str = "_fd_fields";

/// The UUID row-handle column (§9).
pub const COL_ID: &str = "id";
/// The name of the table the overlaid field belongs to. Part of the key.
pub const COL_TABLE: &str = "table_name";
/// The overlaid field's own name — §9's required `name` column. Part of the key.
pub const COL_NAME: &str = "name";
/// The human label shown instead of the field name, when one is given.
pub const COL_LABEL: &str = "label";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The registered rich type's name, or NULL for a basic field.
pub const COL_TYPE: &str = "type";
/// The field-kind discriminant: [`KIND_PLAIN`], [`KIND_KEY`] or [`KIND_FILE`].
pub const COL_KIND: &str = "kind";
/// The sparse per-field values column (§9) — JSON, always an object. Holds the
/// kind's parameters alongside the field's rich-type attributes.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The `kind` discriminant for a plain scalar field.
pub const KIND_PLAIN: &str = "plain";
/// The `kind` discriminant for a foreign-key field.
pub const KIND_KEY: &str = "key";
/// The `kind` discriminant for a file-reference field.
pub const KIND_FILE: &str = "file";
/// The `kind` discriminant for a non-stored calculated field (Phase 8).
pub const KIND_CALC: &str = "calc";
/// The `kind` discriminant for a secret field (TODO-imap M2): a text column
/// holding encrypted values. It has no parameters.
pub const KIND_SECRET: &str = "secret";

// Reserved keys a kind's parameters occupy within the `attributes` bag.
const KEY_TARGET_TABLE: &str = "target_table";
const KEY_TARGET_FIELD: &str = "target_field";
const KEY_SUMMARY_FIELD: &str = "summary_field";
const KEY_STORE: &str = "store";
const KEY_FOLDER: &str = "folder";
const KEY_MIME_ALLOW: &str = "mime_allow";
const KEY_ON_DELETE: &str = "on_delete";
const KEY_ON_CONFLICT: &str = "on_conflict";
const KEY_EXPRESSION: &str = "expression";
// Not a kind's parameter, but stored the same way: any kind but `calc` may have
// one, and it is the overlay's, never the rich type's.
const KEY_DEFAULT_EXPRESSION: &str = "default_expression";

/// The settings a `File` field kind takes, as the same [`FormField`] vocabulary a
/// rich type's attributes and a framework's config use (§6.2) — so the admin UI
/// (§3.4) can render the kind's parameter form from a spec it need not understand.
///
/// The `store` is a [`server_query`](FormField::server_query) over the connected
/// stores ([`QUERY_FILE_STORES`]), resolved to a pick-list before the spec
/// reaches the UI; folder and MIME restrictions are plain inputs. The field names
/// match the keys [`FieldMeta`] folds a `File` kind's parameters into, so a value
/// the UI collects under `store` lands where the storage reads it.
pub fn file_kind_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(KEY_STORE, BasicType::Text)
            .label("File store")
            .required()
            .server_query(QUERY_FILE_STORES),
        FormField::new(KEY_FOLDER, BasicType::Text).label("Folder (optional)"),
        FormField::new(KEY_MIME_ALLOW, BasicType::Json).label("Allowed MIME types (optional)"),
        FormField::new(KEY_ON_DELETE, BasicType::Text)
            .label("When the row is deleted")
            .options([
                FileOnDelete::Keep.as_str(),
                FileOnDelete::RemoveFile.as_str(),
            ])
            .default_value(FileOnDelete::Keep.as_str())
            .sublabel("remove_file deletes the stored file too, unless another row uses it"),
        FormField::new(KEY_ON_CONFLICT, BasicType::Text)
            .label("When an upload's name is taken")
            .options([
                FileOnConflict::Overwrite.as_str(),
                FileOnConflict::Suffix.as_str(),
            ])
            .default_value(FileOnConflict::Overwrite.as_str())
            .sublabel("suffix stores it as name_1.ext, name_2.ext, … instead of replacing"),
    ]
}

/// The settings a `Key` field kind takes (§6.2), as [`FormField`]s. Mirrors
/// [`file_kind_config_spec`]; the field names match the keys [`FieldMeta`] folds a
/// `Key` kind's parameters into.
///
/// Unlike a `File`'s, this spec is what a `Key`'s parameters *are*, not the form
/// the admin UI renders for them: the three depend on each other — the target and
/// summary fields must be fields of the chosen table — and the `FormField`
/// vocabulary has no way to say that one setting's options come from another's
/// value. So the admin UI renders a Key by hand (`ui/admin/src/keyField.ts`) and
/// this stays the machine-readable description of what the kind accepts.
pub fn key_kind_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(KEY_TARGET_TABLE, BasicType::Text)
            .label("Target table")
            .required(),
        FormField::new(KEY_TARGET_FIELD, BasicType::Text)
            .label("Target field")
            .required(),
        FormField::new(KEY_SUMMARY_FIELD, BasicType::Text).label("Summary field (optional)"),
    ]
}

/// Identifies a stored field-overlay row.
///
/// Distinct from a field's `(table, name)` identity, which is what everything
/// else references. This id is a stable handle the row keeps across edits, so an
/// edit updates the existing row rather than racing to create a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldMetaId(pub Uuid);

impl FieldMetaId {
    /// A fresh random id, for an overlay that has never been saved.
    pub fn new() -> FieldMetaId {
        FieldMetaId(Uuid::new_v4())
    }
}

impl Default for FieldMetaId {
    fn default() -> FieldMetaId {
        FieldMetaId::new()
    }
}

/// The stored overlay for one field (design §9).
///
/// Deliberately **not** a [`DataField`]: a `DataField` is the merged view the
/// rest of the system sees — its SQL type, nullability and key membership all
/// come from introspection — while this is only the part an admin configured.
/// Keeping them apart is what makes "no row" expressible, and what stops the
/// overlay from restating a fact the column already knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMeta {
    /// The row's stable handle.
    pub id: FieldMetaId,
    /// The table the overlaid field belongs to.
    pub table_name: String,
    /// The overlaid field's own name. Unique within its table.
    pub field_name: String,
    /// A human label; empty when none was given (the field's name is the label).
    pub label: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// The registered rich type's name, if the overlay gives the field one.
    /// `None` leaves the field its introspected basic type (§2.1: a rich type is
    /// never guessed from the database).
    pub type_name: Option<String>,
    /// The field's kind and the kind's parameters. `Plain` for an ordinary
    /// column; `Key`/`File` carry their references.
    pub kind: DataFieldKind,
    /// The field's own (rich-type) attributes — a `String`'s `max_length`, an
    /// `Integer`'s `min`/`max`. The kind's parameters are stored beside these but
    /// are modelled in [`kind`](FieldMeta::kind), not here.
    pub attributes: Attrs,
    /// A JavaScript expression supplying the value when an insert leaves the
    /// field out — see [`DataField::default_expression`]. Stored in the
    /// attributes bag under a reserved key, like a kind's parameters.
    pub default_expression: Option<String>,
}

impl FieldMeta {
    /// A plain overlay for `table_name.field_name` carrying nothing else — the
    /// base to add settings to.
    pub fn new(table_name: impl Into<String>, field_name: impl Into<String>) -> FieldMeta {
        FieldMeta {
            id: FieldMetaId::new(),
            table_name: table_name.into(),
            field_name: field_name.into(),
            label: String::new(),
            description: String::new(),
            type_name: None,
            kind: DataFieldKind::Plain,
            attributes: Attrs::new(),
            default_expression: None,
        }
    }

    /// Set the human label.
    pub fn label(mut self, label: impl Into<String>) -> FieldMeta {
        self.label = label.into();
        self
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> FieldMeta {
        self.description = description.into();
        self
    }

    /// Give the field a registered rich type by name.
    pub fn rich_type(mut self, name: impl Into<String>) -> FieldMeta {
        self.type_name = Some(name.into());
        self
    }

    /// Set the field's kind (and its parameters).
    pub fn kind(mut self, kind: DataFieldKind) -> FieldMeta {
        self.kind = kind;
        self
    }

    /// Give the field a default-value expression.
    pub fn default_expression(mut self, expression: impl Into<String>) -> FieldMeta {
        self.default_expression = Some(expression.into());
        self
    }

    /// Give the row a specific id — for a caller updating a known row.
    pub fn id(mut self, id: FieldMetaId) -> FieldMeta {
        self.id = id;
        self
    }
}

/// The fields of the `_fd_fields` table, in declaration order.
///
/// `(table_name, name)` is the composite **primary key**: it is what enforces one
/// overlay row per field, and the merge (§3.2) joins fields on the pair. `id` is
/// a separate required, unique row handle (see the module docs on why it is not
/// the key here, unlike `_fd_tables`).
fn field_meta_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    vec![
        DataField::plain(COL_ID, uuid()).required().unique(),
        DataField::plain(COL_TABLE, text()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().primary_key(),
        DataField::plain(COL_LABEL, text()),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_TYPE, text()),
        DataField::plain(COL_KIND, text()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure the `_fd_fields` table exists, creating it if absent, and return it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as [`bootstrap_table_meta`](crate::bootstrap_table_meta). Not
/// wired into the boot path here: like `_fd_tables` before §1.2, nothing reads
/// these rows until §3.2's merge exists, so wiring it in belongs there.
pub async fn bootstrap_field_meta(catalog: &Catalog) -> Result<Table> {
    if let Some(existing) = catalog.get(FIELD_META_TABLE)? {
        return Ok(existing);
    }
    catalog
        .create_table(FIELD_META_TABLE, &field_meta_fields())
        .await
}

/// Save a field overlay: insert its row, or update it in place if a row with its
/// [`FieldMetaId`] already exists.
///
/// Checked here (each for the same reason the twin check exists in
/// [`save_table_meta`](crate::save_table_meta)):
///
/// - **A table and a field name** — an overlay of nothing has no meaning.
/// - **Not a system table** — `_fd_*` tables are hidden from users (§9); their
///   fields are not the admin's to configure.
/// - **The `(table, field)` pair is not already claimed by another row** — the
///   composite primary key is the authority, but this names the conflict rather
///   than surfacing a raw constraint violation.
///
/// Deliberately **not** checked: that the field (or its table) exists, nor that
/// `type_name` names a registered rich type. An overlay may legitimately outlive
/// its column (a restore, a migration run outside Saltcorn), and a rich type may
/// arrive later from a plugin — storage stays inert, and §3.2's merge is where a
/// dangling row or an unknown type is reported.
///
/// **Saving reloads the catalog cache**, exactly as a schema change does: the
/// merged fields are read from the cached [`Table`], so without the reload an
/// admin would set a type and watch the running server go on serving the old one.
pub async fn save_field_meta(catalog: &Catalog, meta: &FieldMeta) -> Result<()> {
    save_field_meta_row(catalog, meta).await?;
    catalog.reload().await
}

/// Write the row **without reloading** — the half of [`save_field_meta`] a
/// batch uses, which reloads once at the end rather than once per operation
/// (Phase 7).
pub async fn save_field_meta_row(catalog: &Catalog, meta: &FieldMeta) -> Result<()> {
    let table = meta.table_name.trim();
    let field = meta.field_name.trim();
    if table.is_empty() || field.is_empty() {
        return Err(Error::invalid(
            "a field overlay needs both a table name and a field name",
        ));
    }
    if table.starts_with("_fd_") {
        return Err(Error::invalid(format!(
            "`{table}` is a system table; its fields are not configurable"
        )));
    }

    if let Some(other) = load_field_meta_by_field(catalog, table, field).await?
        && other.id != meta.id
    {
        return Err(Error::invalid(format!(
            "field `{table}.{field}` already has an overlay row; a field has at most one"
        )));
    }

    let columns = meta_columns();
    let values = meta_values(meta);
    if load_field_meta(catalog, meta.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's handle, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(FIELD_META_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(meta.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            FIELD_META_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(())
}

/// Load the overlay with this id, if it exists.
pub async fn load_field_meta(catalog: &Catalog, id: FieldMetaId) -> Result<Option<FieldMeta>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the overlay for `table.field`, if any — the lookup the merge (§3.2)
/// performs.
pub async fn load_field_meta_by_field(
    catalog: &Catalog,
    table: &str,
    field: &str,
) -> Result<Option<FieldMeta>> {
    load_one(
        catalog,
        Expr::col(COL_TABLE)
            .eq(Expr::lit(table))
            .and(Expr::col(COL_NAME).eq(Expr::lit(field))),
    )
    .await
}

/// Every stored overlay, ordered by table then field name.
pub async fn list_field_meta(catalog: &Catalog) -> Result<Vec<FieldMeta>> {
    let select = Select::from(Source::table(FIELD_META_TABLE));
    let mut metas: Vec<FieldMeta> = rows(catalog, select)
        .await?
        .iter()
        .map(field_meta_from_row)
        .collect::<Result<_>>()?;
    metas.sort_by(|a, b| {
        a.table_name
            .cmp(&b.table_name)
            .then_with(|| a.field_name.cmp(&b.field_name))
    });
    Ok(metas)
}

/// Every overlay for the fields of `table`, ordered by field name — what §3.2's
/// merge loads for one table.
pub async fn list_field_meta_for_table(catalog: &Catalog, table: &str) -> Result<Vec<FieldMeta>> {
    let select = Select::from(Source::table(FIELD_META_TABLE))
        .filter(Expr::col(COL_TABLE).eq(Expr::lit(table)));
    let mut metas: Vec<FieldMeta> = rows(catalog, select)
        .await?
        .iter()
        .map(field_meta_from_row)
        .collect::<Result<_>>()?;
    metas.sort_by(|a, b| a.field_name.cmp(&b.field_name));
    Ok(metas)
}

/// Delete a field overlay, returning whether one was there to delete.
///
/// **This removes the row and nothing else** — the column and its data are
/// untouched. Deleting an overlay is "forget what I configured", returning the
/// field to its introspected basic type. Reloads the cache for the same reason
/// [`save_field_meta`] does.
pub async fn delete_field_meta(catalog: &Catalog, id: FieldMetaId) -> Result<bool> {
    let deleted = delete_field_meta_row(catalog, id).await?;
    if deleted {
        catalog.reload().await?;
    }
    Ok(deleted)
}

/// Delete the row **without reloading** — the half of [`delete_field_meta`] a
/// batch uses, which reloads once at the end rather than once per row (Phase 7).
pub(crate) async fn delete_field_meta_row(catalog: &Catalog, id: FieldMetaId) -> Result<bool> {
    if load_field_meta(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(FIELD_META_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The row's columns, in the order [`meta_values`] produces them.
fn meta_columns() -> Vec<String> {
    [
        COL_ID,
        COL_TABLE,
        COL_NAME,
        COL_LABEL,
        COL_DESCRIPTION,
        COL_TYPE,
        COL_KIND,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The overlay serialised to its row's values, in [`meta_columns`] order. The
/// kind's discriminant goes to `kind`; its parameters are folded into the
/// `attributes` object beside the field's own attributes.
fn meta_values(meta: &FieldMeta) -> Vec<Value> {
    vec![
        Value::Uuid(meta.id.0),
        Value::Text(meta.table_name.trim().to_owned()),
        Value::Text(meta.field_name.trim().to_owned()),
        Value::Text(meta.label.clone()),
        Value::Text(meta.description.clone()),
        match &meta.type_name {
            Some(name) => Value::Text(name.clone()),
            None => Value::Null,
        },
        Value::Text(kind_discriminant(&meta.kind).to_owned()),
        Value::Json(Json::Object(attributes_for_storage(meta))),
    ]
}

/// The `kind` discriminant for a [`DataFieldKind`].
fn kind_discriminant(kind: &DataFieldKind) -> &'static str {
    match kind {
        DataFieldKind::Plain => KIND_PLAIN,
        DataFieldKind::Key { .. } => KIND_KEY,
        DataFieldKind::File { .. } => KIND_FILE,
        DataFieldKind::Calc { .. } => KIND_CALC,
        DataFieldKind::Secret => KIND_SECRET,
    }
}

/// The stored `attributes` object: the field's own attributes with the kind's
/// parameters folded in under their reserved keys. Optional/empty parameters are
/// omitted rather than stored as null, keeping the bag sparse (§9).
fn attributes_for_storage(meta: &FieldMeta) -> Attrs {
    let mut obj = meta.attributes.clone();
    if let Some(expression) = &meta.default_expression {
        obj.insert(
            KEY_DEFAULT_EXPRESSION.to_owned(),
            Json::String(expression.clone()),
        );
    }
    match &meta.kind {
        DataFieldKind::Plain | DataFieldKind::Secret => {}
        DataFieldKind::Key {
            target_table,
            target_field,
            summary_field,
        } => {
            obj.insert(
                KEY_TARGET_TABLE.to_owned(),
                Json::String(target_table.0.clone()),
            );
            obj.insert(
                KEY_TARGET_FIELD.to_owned(),
                Json::String(target_field.0.clone()),
            );
            if let Some(summary) = summary_field {
                obj.insert(
                    KEY_SUMMARY_FIELD.to_owned(),
                    Json::String(summary.0.clone()),
                );
            }
        }
        DataFieldKind::File {
            store,
            folder,
            mime_allow,
            on_delete,
            on_conflict,
        } => {
            obj.insert(KEY_STORE.to_owned(), Json::String(store.0.clone()));
            if *on_delete != FileOnDelete::default() {
                obj.insert(KEY_ON_DELETE.to_owned(), json_str(on_delete.as_str()));
            }
            if *on_conflict != FileOnConflict::default() {
                obj.insert(KEY_ON_CONFLICT.to_owned(), json_str(on_conflict.as_str()));
            }
            if let Some(folder) = folder {
                obj.insert(KEY_FOLDER.to_owned(), Json::String(folder.clone()));
            }
            if !mime_allow.is_empty() {
                let list = mime_allow.iter().map(|m| Json::String(m.clone())).collect();
                obj.insert(KEY_MIME_ALLOW.to_owned(), Json::Array(list));
            }
        }
        DataFieldKind::Calc { expression } => {
            obj.insert(KEY_EXPRESSION.to_owned(), Json::String(expression.clone()));
        }
    }
    obj
}

/// Rebuild a [`FieldMeta`] from its `_fd_fields` row. The strictness note in the
/// module docs applies throughout: every column is read for its exact shape.
fn field_meta_from_row(row: &Row) -> Result<FieldMeta> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => FieldMetaId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let table_name = text(row, COL_TABLE)?;
    let field_name = text(row, COL_NAME)?;
    let discriminant = text(row, COL_KIND)?;
    let mut stored_attributes = object(row, COL_ATTRIBUTES)?;
    let default_expression = take_optional_text(
        &mut stored_attributes,
        KEY_DEFAULT_EXPRESSION,
        &table_name,
        &field_name,
    )?;
    let (kind, attributes) =
        kind_and_attributes(&discriminant, stored_attributes, &table_name, &field_name)?;

    Ok(FieldMeta {
        id,
        label: optional_text(row, COL_LABEL)?,
        description: optional_text(row, COL_DESCRIPTION)?,
        type_name: nullable_text(row, COL_TYPE)?,
        kind,
        attributes,
        default_expression,
        table_name,
        field_name,
    })
}

/// Split a stored `attributes` object into the field's kind (with its parameters
/// lifted out) and the residual attributes that are the field's own.
fn kind_and_attributes(
    discriminant: &str,
    mut attrs: Attrs,
    table: &str,
    field: &str,
) -> Result<(DataFieldKind, Attrs)> {
    let kind = match discriminant {
        KIND_PLAIN => DataFieldKind::Plain,
        KIND_KEY => DataFieldKind::Key {
            target_table: TableId(take_text(&mut attrs, KEY_TARGET_TABLE, table, field)?),
            target_field: FieldId(take_text(&mut attrs, KEY_TARGET_FIELD, table, field)?),
            summary_field: take_optional_text(&mut attrs, KEY_SUMMARY_FIELD, table, field)?
                .map(FieldId),
        },
        KIND_FILE => DataFieldKind::File {
            store: FileStoreId(take_text(&mut attrs, KEY_STORE, table, field)?),
            folder: take_optional_text(&mut attrs, KEY_FOLDER, table, field)?,
            mime_allow: take_string_array(&mut attrs, KEY_MIME_ALLOW, table, field)?,
            on_delete: take_choice(&mut attrs, KEY_ON_DELETE, table, field, FileOnDelete::parse)?,
            on_conflict: take_choice(
                &mut attrs,
                KEY_ON_CONFLICT,
                table,
                field,
                FileOnConflict::parse,
            )?,
        },
        KIND_CALC => DataFieldKind::Calc {
            expression: take_text(&mut attrs, KEY_EXPRESSION, table, field)?,
        },
        KIND_SECRET => DataFieldKind::Secret,
        other => {
            return Err(Error::invalid(format!(
                "{FIELD_META_TABLE} row for `{table}.{field}`: `{other}` is not a known field \
                 kind (expected {KIND_PLAIN}, {KIND_KEY}, {KIND_FILE}, {KIND_CALC} or {KIND_SECRET})"
            )));
        }
    };
    Ok((kind, attrs))
}

/// Remove a required string parameter from the attributes bag, naming the field
/// and parameter if it is missing or not a string.
fn take_text(attrs: &mut Attrs, key: &str, table: &str, field: &str) -> Result<String> {
    match attrs.remove(key) {
        Some(Json::String(s)) => Ok(s),
        Some(other) => Err(bad_param(table, field, key, "a string", Some(&other))),
        None => Err(bad_param(table, field, key, "a string", None)),
    }
}

/// Remove an optional enumerated parameter, its default when absent.
fn take_choice<T: Default>(
    attrs: &mut Attrs,
    key: &str,
    table: &str,
    field: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<T> {
    match take_optional_text(attrs, key, table, field)? {
        None => Ok(T::default()),
        Some(text) => parse(&text).ok_or_else(|| {
            bad_param(
                table,
                field,
                key,
                "a known option",
                Some(&Json::String(text)),
            )
        }),
    }
}

fn json_str(s: &str) -> Json {
    Json::String(s.to_owned())
}

/// Remove an optional string parameter: `None` when absent or null, an error when
/// present but not a string.
fn take_optional_text(
    attrs: &mut Attrs,
    key: &str,
    table: &str,
    field: &str,
) -> Result<Option<String>> {
    match attrs.remove(key) {
        Some(Json::String(s)) => Ok(Some(s)),
        Some(Json::Null) | None => Ok(None),
        Some(other) => Err(bad_param(table, field, key, "a string", Some(&other))),
    }
}

/// Remove an optional array-of-strings parameter: empty when absent or null, an
/// error when present but not an array of strings.
fn take_string_array(
    attrs: &mut Attrs,
    key: &str,
    table: &str,
    field: &str,
) -> Result<Vec<String>> {
    match attrs.remove(key) {
        Some(Json::Null) | None => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Json::String(s) => Ok(s),
                other => Err(bad_param(
                    table,
                    field,
                    key,
                    "an array of strings",
                    Some(&other),
                )),
            })
            .collect(),
        Some(other) => Err(bad_param(
            table,
            field,
            key,
            "an array of strings",
            Some(&other),
        )),
    }
}

/// An error for a kind parameter of the wrong shape (or missing).
fn bad_param(table: &str, field: &str, key: &str, expected: &str, got: Option<&Json>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{FIELD_META_TABLE} row for `{table}.{field}`: kind parameter `{key}` should be \
             {expected}, got {value}"
        )),
        None => Error::invalid(format!(
            "{FIELD_META_TABLE} row for `{table}.{field}`: kind parameter `{key}` is required \
             and missing"
        )),
    }
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A text column where NULL means "none given", read as an empty string.
fn optional_text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A text column where NULL means "none given", read as `None`.
fn nullable_text(row: &Row, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{FIELD_META_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{FIELD_META_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// Load the single overlay matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<FieldMeta>> {
    let select = Select::from(Source::table(FIELD_META_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(field_meta_from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = field_meta_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        // §9: every system metadata table MUST have id (UUID), name, description,
        // attributes (JSON object).
        let id = by_name(COL_ID);
        assert!(id.required && id.unique, "id is a required, unique handle");
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required);
        assert!(!by_name(COL_DESCRIPTION).required);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
    }

    #[test]
    fn the_natural_key_is_the_composite_of_table_and_field() {
        // One overlay row per field is enforced by the primary key on the pair,
        // not by a single-column unique — a field name is unique only in its
        // table.
        let fields = field_meta_fields();
        let pk: Vec<&str> = fields
            .iter()
            .filter(|f| f.primary_key)
            .map(|f| f.base.name.as_str())
            .collect();
        assert_eq!(pk, [COL_TABLE, COL_NAME]);
        // And id, though required and unique, is not part of that key.
        assert!(
            !fields
                .iter()
                .find(|f| f.base.name == COL_ID)
                .unwrap()
                .primary_key
        );
    }

    #[test]
    fn the_overlay_carries_nothing_the_database_already_knows() {
        // The precedence rule (§1.2/§3.2) holds only while the overlay's columns
        // name nothing introspection yields — no sql type, no nullability. The
        // set is asserted in full so a stray column would fail here.
        let names: Vec<String> = field_meta_fields()
            .iter()
            .map(|f| f.base.name.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                COL_ID,
                COL_TABLE,
                COL_NAME,
                COL_LABEL,
                COL_DESCRIPTION,
                COL_TYPE,
                COL_KIND,
                COL_ATTRIBUTES,
            ]
        );
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(FIELD_META_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let meta = FieldMeta::new("books", "title");
        assert_eq!(meta_columns().len(), meta_values(&meta).len());
        assert_eq!(meta_columns().len(), field_meta_fields().len());
    }

    #[test]
    fn a_file_kind_round_trips_through_the_attributes_bag() {
        // The parameters go into `attributes` on the way out and come back into
        // the structured kind on the way in, with the residual attributes empty.
        let file = DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: Some("covers".into()),
            mime_allow: vec!["image/png".into(), "image/jpeg".into()],
            on_delete: Default::default(),
            on_conflict: Default::default(),
        };
        let meta = FieldMeta::new("books", "cover").kind(file.clone());

        let stored = attributes_for_storage(&meta);
        assert_eq!(stored.get(KEY_STORE), Some(&Json::String("uploads".into())));
        assert_eq!(stored.get(KEY_FOLDER), Some(&Json::String("covers".into())));

        let (kind, residual) = kind_and_attributes(KIND_FILE, stored, "books", "cover").unwrap();
        assert_eq!(kind, file);
        assert!(
            residual.is_empty(),
            "no attributes beyond the kind's params"
        );
    }

    #[test]
    fn a_file_kinds_delete_and_conflict_options_round_trip_and_default_sparse() {
        let file = DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: None,
            mime_allow: Vec::new(),
            on_delete: FileOnDelete::RemoveFile,
            on_conflict: FileOnConflict::Suffix,
        };
        let stored = attributes_for_storage(&FieldMeta::new("books", "cover").kind(file.clone()));
        assert_eq!(stored.get(KEY_ON_DELETE), Some(&json_str("remove_file")));
        assert_eq!(stored.get(KEY_ON_CONFLICT), Some(&json_str("suffix")));
        let (kind, residual) = kind_and_attributes(KIND_FILE, stored, "books", "cover").unwrap();
        assert_eq!(kind, file);
        assert!(residual.is_empty());

        // The defaults are not stored at all, and an absent option reads back
        // as its default.
        let plain = DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: None,
            mime_allow: Vec::new(),
            on_delete: FileOnDelete::Keep,
            on_conflict: FileOnConflict::Overwrite,
        };
        let stored = attributes_for_storage(&FieldMeta::new("books", "cover").kind(plain.clone()));
        assert!(!stored.contains_key(KEY_ON_DELETE) && !stored.contains_key(KEY_ON_CONFLICT));
        let (kind, _) = kind_and_attributes(KIND_FILE, stored, "books", "cover").unwrap();
        assert_eq!(kind, plain);

        // An unknown option is a named error, not a silent default.
        let mut bad = Attrs::new();
        bad.insert(KEY_STORE.into(), json_str("uploads"));
        bad.insert(KEY_ON_DELETE.into(), json_str("shred"));
        let err = kind_and_attributes(KIND_FILE, bad, "books", "cover").unwrap_err();
        assert!(err.to_string().contains(KEY_ON_DELETE), "{err}");
    }

    #[test]
    fn a_plain_rich_field_keeps_its_attributes_and_reserves_no_keys() {
        // A plain field's whole attributes bag is its own — nothing is lifted out.
        let mut meta = FieldMeta::new("people", "age").rich_type("integer");
        meta.attributes.insert("min".into(), Json::from(0));
        meta.attributes.insert("max".into(), Json::from(120));

        let stored = attributes_for_storage(&meta);
        let (kind, residual) = kind_and_attributes(KIND_PLAIN, stored, "people", "age").unwrap();
        assert_eq!(kind, DataFieldKind::Plain);
        assert_eq!(residual, meta.attributes);
    }

    #[test]
    fn an_unknown_kind_is_rejected_by_name() {
        let err = kind_and_attributes("blob", Attrs::new(), "books", "cover").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("blob"), "names the bad kind: {message}");
        assert!(
            message.contains("books.cover"),
            "names the field: {message}"
        );
    }

    #[test]
    fn a_file_row_missing_its_store_is_a_named_error() {
        // Strict read: the required `store` parameter is not optional.
        let err = kind_and_attributes(KIND_FILE, Attrs::new(), "books", "cover").unwrap_err();
        let message = err.to_string();
        assert!(message.contains(KEY_STORE), "{message}");
        assert!(message.contains("books.cover"), "{message}");
    }
}
