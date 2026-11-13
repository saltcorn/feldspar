//! Identity (APP_SYNC_PROPOSAL §3.2): every table and field in the tree has an
//! id, and it is the same id on every instance.
//!
//! Tables and fields are overlays: a column exists whether or not an `_fd_fields`
//! row describes it, and one added by hand — raw DDL, an imported database —
//! has none, so it has no id to keep. The export therefore **creates the
//! missing rows first** ([`ensure_overlays`]). The write is additive: a new row
//! carries the defaults a table or field with no row already has (admin-only
//! access, the column's own kind), so nothing about how the table behaves
//! changes, and from then on the id is stable.
//!
//! The other half — an entity created on another instance keeps the id it came
//! with — is each create path's: `schema_edit`'s `GivenIds` for tables and
//! fields, and the `*_from_body(id, …)` codecs for everything else.

use sc_catalog::{
    Catalog, DataFieldKind, FIELD_META_TABLE, FieldMeta, TABLE_META_TABLE, Table, TableMeta,
    list_field_meta_for_table, save_field_meta_row, save_table_meta_row,
};
use sc_error::Result;

/// Whether `table` is one the tree carries as `tables/<name>.json`: not a
/// system table, and not one of the tables whose rows are the users' own and
/// whose shape is the system's (the users table is `users.json`, and its grants
/// and credits are never in the repository).
pub fn is_structure_table(table: &Table) -> bool {
    const USER_TABLES: [&str; 5] = [
        sc_auth::USERS_TABLE,
        sc_auth::ROLE_GRANTS_TABLE,
        sc_auth::CREDIT_ACCOUNTS_TABLE,
        sc_auth::CREDIT_BUCKETS_TABLE,
        sc_auth::CREDIT_ENTRIES_TABLE,
    ];
    !table.is_system() && !USER_TABLES.contains(&table.name.as_str())
}

/// Give every table and field the tree carries an overlay row, so each has an
/// id. Returns how many rows were created; the catalog is reloaded when any
/// were.
///
/// The users table's own columns are the system's and are not in the tree, so
/// only the fields an admin added to it are given rows, and the table itself is
/// not: its access is the system's to decide. A provided table's fields are the
/// provider's declaration, not columns, so they are left as they are too.
pub async fn ensure_overlays(catalog: &Catalog) -> Result<usize> {
    if catalog.get(TABLE_META_TABLE)?.is_none() || catalog.get(FIELD_META_TABLE)?.is_none() {
        return Ok(0);
    }
    let mut created = 0;
    for table in catalog.tables()? {
        let is_users = table.name == sc_auth::USERS_TABLE;
        if !is_structure_table(&table) && !is_users {
            continue;
        }
        if !is_users && table.overlay.is_none() {
            save_table_meta_row(catalog, &TableMeta::new(table.name.clone())).await?;
            created += 1;
        }
        if table.provider().is_some() {
            continue;
        }
        let metas = list_field_meta_for_table(catalog, &table.name).await?;
        for field in &table.fields {
            let name = &field.base.name;
            if is_users && sc_auth::is_system_user_column(name) {
                continue;
            }
            if metas.iter().any(|m| &m.field_name == name) {
                continue;
            }
            // The kind the column already reads as — a key the database's
            // foreign key gives it — so the new row restates it rather than
            // turning the field plain.
            let mut meta = FieldMeta::new(table.name.clone(), name.clone());
            meta.kind = match &field.kind {
                DataFieldKind::Calc { .. } => continue,
                other => other.clone(),
            };
            save_field_meta_row(catalog, &meta).await?;
            created += 1;
        }
    }
    if created > 0 {
        catalog.reload().await?;
    }
    Ok(created)
}
