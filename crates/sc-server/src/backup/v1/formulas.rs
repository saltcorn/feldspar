//! v1's calculated fields, stored or not, as this system's calculated fields
//! — which are never stored (§3.4).
//!
//! A v1 formula is a JavaScript expression and so is an `sc-expr` formula, so a
//! plain calculation (`qty*unit_price`) and a Ⱶ-join (`orderⱵfor_export`, the
//! same character in both systems) cross as written. Two spellings do not:
//!
//! - **A join with a dot.** v1 lets a non-stored formula reach through a key as
//!   `publisher.name`; here a dot is a member access on the key's *value*, and
//!   the join is `publisherⱵname`. Each chain whose head is a key field becomes
//!   the Ⱶ-path it means, through as many keys as it follows.
//! - **An aggregation.** v1 has no formula for one: the field's expression is
//!   `__aggregation` and the attributes say what to aggregate. Here it is a
//!   formula over the incoming relation (docs/AGG_EXPRS.md) —
//!   `order_lineↃorder.filter(r => r.gift === true).length` — whose `where`
//!   is v1's own, read in the child row's scope.
//!
//! An aggregation's arrow reads the child row's **columns**: a calculated child
//! field has none, so where v1 aggregated (or filtered on) one, its own formula
//! is written into the arrow in its place — `order_lineↃorder.sum(r =>
//! r.qty*r.unit_price)` for v1's sum of the line's calculated `sum`.
//!
//! What comes out is checked by the schema editor when the restore adds the
//! field, exactly as one typed into the table editor is, so a v1 formula this
//! system cannot read is a refusal naming the field rather than a column that
//! fails on every read.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_expr::{INVERSE, JOIN, rewrite_free_refs};
use serde_json::{Map, Value as Json};

use super::{array, name_of, text};

/// The expression v1 writes for a field whose value is an aggregation.
const AGGREGATION: &str = "__aggregation";
/// The arrow parameter a child row is bound to in a translated aggregation.
const CHILD_ROW: &str = "r";

/// Every table of the pack, as the field names it has and, for each key, the
/// table it points at — what a dot-join and an aggregation are resolved against.
#[derive(Default)]
pub(super) struct V1Schema {
    tables: BTreeMap<String, BTreeMap<String, V1FieldInfo>>,
}

/// What a formula needs to know about one v1 field.
#[derive(Default)]
struct V1FieldInfo {
    /// The table a `Key` points at.
    key_target: Option<String>,
    /// A calculated field's v1 expression (`__aggregation` for an aggregation).
    calc: Option<String>,
}

/// How deep calculated fields are inlined into each other before giving up —
/// a guard against v1 formulas that read each other in a circle.
const MAX_INLINE: usize = 8;

impl V1Schema {
    pub(super) fn of(pack: &Map<String, Json>) -> V1Schema {
        let mut schema = V1Schema::default();
        for table in array(pack, "tables") {
            let Some(table) = table.as_object() else {
                continue;
            };
            let fields = array(table, "fields")
                .iter()
                .filter_map(Json::as_object)
                .map(|field| {
                    let info = V1FieldInfo {
                        key_target: (text(field, "type") == "Key")
                            .then(|| key_target(field))
                            .flatten(),
                        calc: (field.get("calculated").and_then(Json::as_bool) == Some(true))
                            .then(|| text(field, "expression")),
                    };
                    (name_of(field), info)
                })
                .collect();
            schema.tables.insert(name_of(table), fields);
        }
        schema
    }

    fn has_field(&self, table: &str, field: &str) -> bool {
        self.tables
            .get(table)
            .is_some_and(|fields| fields.contains_key(field))
    }

    fn key_target(&self, table: &str, field: &str) -> Option<&str> {
        self.tables.get(table)?.get(field)?.key_target.as_deref()
    }

    fn calc(&self, table: &str, field: &str) -> Option<&str> {
        self.tables.get(table)?.get(field)?.calc.as_deref()
    }
}

/// The table a v1 `Key` field points at, wherever that v1 version wrote it.
pub(super) fn key_target(field: &Map<String, Json>) -> Option<String> {
    field
        .get("attributes")
        .and_then(|a| a.get("reftable_name"))
        .or_else(|| field.get("reftable_name"))
        .and_then(Json::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// The `sc-expr` formula for the v1 calculated field `field` of `table`.
pub(super) fn calc_formula(
    schema: &V1Schema,
    table: &str,
    field: &Map<String, Json>,
) -> Result<String> {
    let expression = text(field, "expression");
    if expression.trim() == AGGREGATION {
        let attrs = field
            .get("attributes")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default();
        return aggregation_formula(schema, table, &attrs);
    }
    if expression.trim().is_empty() {
        return Err(Error::invalid("it is calculated but has no formula"));
    }
    in_scope(schema, table, &expression, None, 0)
}

/// `source`, a formula over a row of `table`, with each dot-join spelled as the
/// Ⱶ-path it means — and, when `row` is given, every field of `table` read off
/// that name (`gift` → `r.gift`), for a formula inside an aggregation's arrow.
/// There a calculated field is replaced by its own formula ([`inline`]), since
/// the arrow reads columns.
///
/// Only what names a field of `table` is touched: `user`, `Math` and anything
/// else this does not know cross as written, for the formula's own validation
/// to judge.
fn in_scope(
    schema: &V1Schema,
    table: &str,
    source: &str,
    row: Option<&str>,
    depth: usize,
) -> Result<String> {
    let mut failed = None;
    let out = rewrite_free_refs(source, &mut |path| {
        let head = &path[0];
        // Already a Ⱶ-path: v1's stored-join spelling, which is this system's.
        let joined = head.contains(JOIN);
        if !joined && !schema.has_field(table, head) {
            return None;
        }
        let mut through = table;
        let mut keys = 0;
        while !joined && keys + 1 < path.len() {
            match schema.key_target(through, &path[keys]) {
                Some(target) => {
                    through = target;
                    keys += 1;
                }
                None => break,
            }
        }
        if let Some(row) = row
            && !joined
            && keys == 0
            && schema.calc(table, head).is_some()
        {
            return match inline(schema, table, head, row, depth) {
                Ok(formula) => Some((1, format!("({formula})"))),
                Err(e) => {
                    failed = Some(e);
                    None
                }
            };
        }
        let reference = path[..=keys].join(&JOIN.to_string());
        match row {
            Some(row) => Some((keys + 1, format!("{row}.{reference}"))),
            None if keys > 0 => Some((keys + 1, reference)),
            None => None,
        }
    })?;
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// The calculated field `field` of `table` as its formula over `row` — what an
/// aggregation's arrow reads in its place.
fn inline(schema: &V1Schema, table: &str, field: &str, row: &str, depth: usize) -> Result<String> {
    let expression = schema.calc(table, field).unwrap_or_default();
    if expression.trim() == AGGREGATION {
        return Err(Error::invalid(format!(
            "it reads `{table}.{field}`, which is itself an aggregation"
        )));
    }
    if depth >= MAX_INLINE {
        return Err(Error::invalid(format!(
            "`{table}.{field}` reads calculated fields more than {MAX_INLINE} deep"
        )));
    }
    in_scope(schema, table, expression, Some(row), depth + 1)
}

/// A v1 aggregation field's attributes as a formula over the incoming relation.
///
/// v1 says it in four attributes: `table` and `ref` (the child table and its key
/// onto this one), `agg_field` (`field@Type`), `aggregate` and an optional
/// `aggwhere` over the child row.
fn aggregation_formula(
    schema: &V1Schema,
    table: &str,
    attrs: &Map<String, Json>,
) -> Result<String> {
    let child = text(attrs, "table");
    let key = text(attrs, "ref");
    if child.is_empty() || key.is_empty() {
        return Err(Error::invalid(
            "it is an aggregation that names no child table or key",
        ));
    }
    match schema.key_target(&child, &key) {
        Some(target) if target == table => {}
        _ => {
            return Err(Error::invalid(format!(
                "it aggregates over `{child}.{key}`, which is not a key onto `{table}` in \
                 this backup"
            )));
        }
    }
    let field = text(attrs, "agg_field")
        .split('@')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let aggregate = text(attrs, "aggregate");
    let mut formula = format!("{child}{INVERSE}{key}");
    let condition = text(attrs, "aggwhere");
    if !condition.trim().is_empty() {
        let condition = in_scope(schema, &child, &condition, Some(CHILD_ROW), 0)
            .map_err(|e| Error::invalid(format!("its where `{condition}`: {}", e.causes())))?;
        formula.push_str(&format!(".filter({CHILD_ROW} => {condition})"));
    }
    let needs_field = || {
        if field.is_empty() {
            Err(Error::invalid(format!(
                "its `{aggregate}` aggregation names no field"
            )))
        } else {
            Ok(())
        }
    };
    // What the aggregate reads off each child row: the column by name, or —
    // for a calculated field, which has no column — its formula in an arrow.
    let calculated = if field.is_empty() || schema.calc(&child, &field).is_none() {
        None
    } else {
        Some(inline(schema, &child, &field, CHILD_ROW, 0)?)
    };
    let selector = match &calculated {
        Some(formula) => format!("{CHILD_ROW} => {formula}"),
        None => format!("\"{field}\""),
    };
    let value = match &calculated {
        Some(formula) => format!("({formula})"),
        None => format!("{CHILD_ROW}.{field}"),
    };
    let tail = match aggregate.trim() {
        // v1 counts the rows where the field is not null — every row, for the
        // key it nearly always counts.
        "Count" if field.is_empty() || field == "id" => ".length".to_owned(),
        "Count" => format!(".filter({CHILD_ROW} => {value} !== null).length"),
        "CountUnique" => {
            needs_field()?;
            format!(".distinct({selector}).length")
        }
        aggregate @ ("Sum" | "Avg" | "Max" | "Min") => {
            needs_field()?;
            format!(".{}({selector})", aggregate.to_ascii_lowercase())
        }
        "Array_Agg" => {
            needs_field()?;
            format!(".map({CHILD_ROW} => {value})")
        }
        other => {
            let ordered = other
                .strip_prefix("Latest ")
                .map(|by| ("maxBy", by))
                .or_else(|| other.strip_prefix("Earliest ").map(|by| ("minBy", by)));
            match ordered {
                Some(_) if calculated.is_some() => {
                    return Err(Error::invalid(format!(
                        "its `{other}` reads `{child}.{field}`, which is calculated and \
                         has no column to select"
                    )));
                }
                Some((method, by)) if !by.trim().is_empty() => {
                    needs_field()?;
                    format!(".{method}(\"{}\").{field}", by.trim())
                }
                _ => {
                    return Err(Error::invalid(format!(
                        "its aggregate `{other}` has no counterpart here"
                    )));
                }
            }
        }
    };
    formula.push_str(&tail);
    Ok(formula)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> V1Schema {
        let pack = json!({ "tables": [
            { "name": "order", "fields": [
                { "name": "id", "type": "Integer" },
                { "name": "name", "type": "String" },
                { "name": "customer", "type": "Key", "reftable_name": "customer" },
            ] },
            { "name": "customer", "fields": [
                { "name": "id", "type": "Integer" },
                { "name": "name", "type": "String" },
                { "name": "region", "type": "Key", "reftable_name": "region" },
            ] },
            { "name": "region", "fields": [ { "name": "label", "type": "String" } ] },
            { "name": "order_line", "fields": [
                { "name": "id", "type": "Integer" },
                { "name": "qty", "type": "Integer" },
                { "name": "gift", "type": "Bool" },
                { "name": "price", "type": "Float" },
                { "name": "sum", "type": "Float", "calculated": true, "stored": true,
                  "expression": "qty*price" },
                { "name": "at", "type": "Date" },
                { "name": "order", "type": "Key", "reftable_name": "order" },
            ] },
        ] });
        V1Schema::of(pack.as_object().unwrap())
    }

    fn calc(table: &str, field: Json) -> Result<String> {
        calc_formula(&schema(), table, field.as_object().unwrap())
    }

    fn agg(aggregate: &str, field: &str, aggwhere: &str) -> Result<String> {
        calc(
            "order",
            json!({ "expression": "__aggregation", "attributes": {
                "table": "order_line", "ref": "order", "agg_field": field,
                "aggregate": aggregate, "aggwhere": aggwhere } }),
        )
    }

    #[test]
    fn a_plain_calculation_and_a_stored_join_cross_as_written() {
        assert_eq!(
            calc("order_line", json!({ "expression": "qty*unit_price" })).unwrap(),
            "qty*unit_price"
        );
        assert_eq!(
            calc("order_line", json!({ "expression": "order\u{2c75}name" })).unwrap(),
            "orderⱵname"
        );
    }

    #[test]
    fn a_dot_join_becomes_the_path_it_means_through_every_key() {
        assert_eq!(
            calc("order", json!({ "expression": "customer.name + '!'" })).unwrap(),
            "customerⱵname + '!'"
        );
        assert_eq!(
            calc(
                "order",
                json!({ "expression": "customer.region.label.length" })
            )
            .unwrap(),
            "customerⱵregionⱵlabel.length"
        );
        // A member access on a field that is not a key is left alone.
        assert_eq!(
            calc("order", json!({ "expression": "name.toUpperCase()" })).unwrap(),
            "name.toUpperCase()"
        );
    }

    #[test]
    fn an_aggregation_is_a_formula_over_the_incoming_relation() {
        assert_eq!(
            agg("Sum", "qty@Integer", "").unwrap(),
            "order_lineↃorder.sum(\"qty\")"
        );
        // A calculated child field has no column: its formula is read instead.
        assert_eq!(
            agg("Sum", "sum@Float", "sum > 2").unwrap(),
            "order_lineↃorder.filter(r => (r.qty*r.price) > 2).sum(r => r.qty*r.price)"
        );
        assert_eq!(
            agg("Count", "id@Integer", "gift === true").unwrap(),
            "order_lineↃorder.filter(r => r.gift === true).length"
        );
        assert_eq!(
            agg("Count", "qty@Integer", "").unwrap(),
            "order_lineↃorder.filter(r => r.qty !== null).length"
        );
        assert_eq!(
            agg("CountUnique", "qty@Integer", "").unwrap(),
            "order_lineↃorder.distinct(\"qty\").length"
        );
        assert_eq!(
            agg("Latest at", "qty@Integer", "").unwrap(),
            "order_lineↃorder.maxBy(\"at\").qty"
        );
        // The where reads the child row, joins and all.
        assert_eq!(
            agg("Avg", "qty@Integer", "gift && order.name === 'x'").unwrap(),
            "order_lineↃorder.filter(r => r.gift && r.orderⱵname === 'x').avg(\"qty\")"
        );
    }

    #[test]
    fn what_cannot_be_said_here_is_an_error_naming_it() {
        assert!(
            agg("Percent true", "gift@Bool", "")
                .unwrap_err()
                .to_string()
                .contains("Percent true")
        );
        let wrong_key = calc(
            "customer",
            json!({ "expression": "__aggregation", "attributes": {
                "table": "order_line", "ref": "order", "agg_field": "id@Integer",
                "aggregate": "Count" } }),
        );
        assert!(wrong_key.is_err());
    }
}
