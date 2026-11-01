//! Which tables a row's read rule reads (TODO.md "Live updates" §3 rule 6, L2.6).
//!
//! A `row` subscription was authorised by its row's read rule — the role floor
//! or the table's ownership formula. The floor moves only with the caller's
//! role, which the socket re-reads on its clock. The formula is a function of
//! the row **and of every row it reaches**: `ownerⱵteam` reads the `teams` row
//! the key points at, `sharesↃnote.some(s => s.user === user.id)` reads the
//! `shares` rows pointing back. A write to any of those can change the verdict,
//! so those are the tables a subscription watches: the topic's own table (for
//! its row) plus everything [`read_rule_tables`] names.
//!
//! The answer comes from the formula's own analysis — the same Ⱶ paths and
//! aggregation uses the reified evaluator prefetches — so a table that matters
//! is never missed because somebody forgot to list it.

use std::collections::BTreeSet;

use sc_catalog::{Catalog, DataFieldKind, Table};

/// The tables `table`'s ownership formula reads, other than `table` itself —
/// empty for a table with no formula, and for one whose formula no longer
/// validates (it grants nothing, so nothing it reads can change a verdict).
pub fn read_rule_tables(catalog: &Catalog, table: &Table) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(formula) = &table.ownership else {
        return out;
    };
    let Ok(shape) = catalog.schema_shape() else {
        return out;
    };
    let Ok(analysis) = formula.validate(&shape, &table.name) else {
        return out;
    };
    for path in &analysis.join_paths {
        // Every segment but the last is a Key field to traverse; each target
        // table along the way is read.
        let mut current = table.clone();
        for segment in path
            .segments
            .iter()
            .take(path.segments.len().saturating_sub(1))
        {
            let Some(field) = current.field(segment) else {
                break;
            };
            let DataFieldKind::Key { target_table, .. } = &field.kind else {
                break;
            };
            out.insert(target_table.clone());
            match catalog.get(target_table) {
                Ok(Some(next)) => current = next,
                _ => break,
            }
        }
    }
    for agg in &analysis.agg_uses {
        out.insert(agg.child_table.clone());
    }
    out.remove(&table.name);
    out
}
