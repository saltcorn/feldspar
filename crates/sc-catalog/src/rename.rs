//! Renaming a table: the overlay half.
//!
//! A table is known by its name everywhere (§3.2: its identity *is* the
//! name), so the DDL that renames it in its database leaves every
//! `_fd_*` row that names it pointing at a table that is no longer there. This
//! moves the rows the catalog owns — the table's settings, its fields' settings,
//! the other tables' keys onto it and the formulas that aggregate over it — and
//! nothing else. What other crates store (views, triggers, applications…) is
//! theirs to follow, through the [`TableRenameHook`](crate::TableRenameHook).

use sc_error::Result;

use crate::catalog::Catalog;
use crate::field::DataFieldKind;
use crate::field_meta::{FIELD_META_TABLE, list_field_meta, save_field_meta_row};
use crate::table_meta::{
    TABLE_META_TABLE, list_table_meta, load_table_meta_by_name, save_table_meta_row,
};

/// Move every overlay row that names table `from` to name `to`, after the DDL
/// that renamed it committed.
///
/// - its `_fd_tables` row, and its recently-edited time;
/// - its `_fd_fields` rows;
/// - every key field, in any table, whose target is `from`;
/// - every calculated field's expression, default expression and ownership
///   formula that aggregates over `from` (`fromↃkey`).
///
/// Does not reload the catalog: a batch reloads once at the end.
pub async fn rename_table_meta(catalog: &Catalog, from: &str, to: &str) -> Result<()> {
    if catalog.get(TABLE_META_TABLE)?.is_some() {
        if let Some(mut meta) = load_table_meta_by_name(catalog, from).await? {
            meta.table_name = to.to_owned();
            save_table_meta_row(catalog, &meta).await?;
        }
        for mut meta in list_table_meta(catalog).await? {
            if let Some(formula) = meta
                .ownership_formula()
                .and_then(|f| sc_expr::rename_relation_table(f, from, to))
            {
                meta.set_ownership_formula(Some(&formula));
                save_table_meta_row(catalog, &meta).await?;
            }
        }
    }
    if catalog.get(FIELD_META_TABLE)?.is_some() {
        for mut meta in list_field_meta(catalog).await? {
            let mut changed = false;
            if meta.table_name == from {
                meta.table_name = to.to_owned();
                changed = true;
            }
            match &mut meta.kind {
                DataFieldKind::Key { target_table, .. } if target_table == from => {
                    *target_table = to.to_owned();
                    changed = true;
                }
                DataFieldKind::Calc { expression } => {
                    if let Some(renamed) = sc_expr::rename_relation_table(expression, from, to) {
                        *expression = renamed;
                        changed = true;
                    }
                }
                _ => {}
            }
            if let Some(renamed) = meta
                .default_expression
                .as_deref()
                .and_then(|e| sc_expr::rename_relation_table(e, from, to))
            {
                meta.default_expression = Some(renamed);
                changed = true;
            }
            if changed {
                save_field_meta_row(catalog, &meta).await?;
            }
        }
    }
    crate::edited::forget_table_edit(catalog, from).await?;
    crate::edited::touch_tables(catalog, &[to]).await
}
