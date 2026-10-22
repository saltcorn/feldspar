//! Non-stored calculated fields: catalog-wide validation and dependency
//! ordering (TODO Phase 8).
//!
//! A calc field is a virtual [`DataField`](crate::DataField) the `_fd_fields`
//! overlay introduces (there is no column) whose value is an `sc-expr`
//! expression computed on read. This module runs once per
//! [`Catalog::reload`](crate::Catalog::reload), after every overlay has merged,
//! and does two things that need the *whole* schema:
//!
//! - **Validation** — each expression is validated like an ownership formula but
//!   with **no `user` and no operation flags** (a calc field has no caller). A
//!   parse error, an unknown reference, or a `user`/flag use makes the field
//!   invalid.
//! - **Dependency ordering** — a calc field may read another calc field on the
//!   same table, so the fields form a graph. A cycle, or a dependency on an
//!   invalid field, makes a field invalid in turn. The survivors are left in
//!   **topological order** (each after the fields it reads) so read-time
//!   evaluation and SQL inlining can walk them in one pass.
//!
//! Invalid calc fields are **dropped** (fail closed — they neither appear nor
//! resolve) and each is reported as a [`FieldMergeIssue`], exactly like a
//! dangling field overlay.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sc_expr::{Formula, SchemaShape};

use crate::field::TableId;
use crate::table::{FieldMergeIssue, Table};

/// Validate and order every table's calculated fields against `shape` (which
/// already includes them as fields), dropping the invalid ones and leaving the
/// survivors in dependency order. Returns one issue per dropped field.
pub(crate) fn merge_calc_fields(
    map: &mut HashMap<TableId, Table>,
    shape: &SchemaShape,
) -> Vec<FieldMergeIssue> {
    let mut issues = Vec::new();
    let ids: Vec<TableId> = map.keys().cloned().collect();
    for id in ids {
        let table = &map[&id];
        if table.is_system() {
            continue;
        }
        let calc_names: BTreeSet<String> = table
            .fields
            .iter()
            .filter(|f| f.is_calc())
            .map(|f| f.base.name.clone())
            .collect();
        if calc_names.is_empty() {
            continue;
        }

        let mut invalid: BTreeSet<String> = BTreeSet::new();
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for field in table.fields.iter().filter(|f| f.is_calc()) {
            let name = &field.base.name;
            let expr = field.calc_expression().unwrap_or_default();
            match Formula::parse(expr) {
                Err(e) => {
                    invalid.insert(name.clone());
                    issues.push(issue(&table.name, name, &e.to_string()));
                }
                Ok(formula) => {
                    // Same-table calc fields this one reads (self excluded).
                    let d: BTreeSet<String> = formula
                        .free_vars()
                        .idents
                        .iter()
                        .filter(|i| calc_names.contains(*i) && *i != name)
                        .cloned()
                        .collect();
                    deps.insert(name.clone(), d);
                    match formula.validate(shape, &table.name) {
                        Err(e) => {
                            invalid.insert(name.clone());
                            issues.push(issue(&table.name, name, &e.to_string()));
                        }
                        Ok(analysis) => {
                            if analysis.uses_caller() || !analysis.flags.is_empty() {
                                invalid.insert(name.clone());
                                issues.push(issue(
                                    &table.name,
                                    name,
                                    "a calculated field cannot use `user`, `session` or the \
                                     operation flags",
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Dependency order + cycle detection in one Kahn pass over the deps
        // graph. Whatever cannot be ordered is in a cycle.
        let (order, cyclic) = topo_order(&calc_names, &deps);
        for name in &cyclic {
            if invalid.insert(name.clone()) {
                issues.push(issue(
                    &table.name,
                    name,
                    "is part of a calculated-field dependency cycle",
                ));
            }
        }

        // A field that reads an invalid field is invalid too — propagate to a
        // fixpoint.
        loop {
            let mut changed = false;
            for (name, d) in &deps {
                if !invalid.contains(name) && d.iter().any(|x| invalid.contains(x)) {
                    invalid.insert(name.clone());
                    issues.push(issue(
                        &table.name,
                        name,
                        "reads a calculated field that is not valid",
                    ));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Rebuild the field list: the real columns keep their order; the
        // surviving calc fields follow in dependency order.
        if let Some(table) = map.get_mut(&id) {
            let mut real: Vec<_> = table
                .fields
                .iter()
                .filter(|f| !f.is_calc())
                .cloned()
                .collect();
            for name in order {
                if invalid.contains(&name) {
                    continue;
                }
                if let Some(f) = table.fields.iter().find(|f| f.base.name == name) {
                    real.push(f.clone());
                }
            }
            table.fields = real;
        }
    }
    issues
}

/// A Kahn topological sort of `nodes` by the `deps` graph (`A ∈ deps[B]` means B
/// reads A, so A must come first). Returns the orderable nodes in evaluation
/// order and the leftover nodes, which are exactly those on a cycle.
fn topo_order(
    nodes: &BTreeSet<String>,
    deps: &BTreeMap<String, BTreeSet<String>>,
) -> (Vec<String>, BTreeSet<String>) {
    let mut remaining: BTreeSet<String> = nodes.clone();
    let mut order = Vec::new();
    loop {
        // A node is ready when every dependency it has is already emitted (i.e.
        // no longer remaining). Missing/parse-error deps are treated as empty.
        let ready: Vec<String> = remaining
            .iter()
            .filter(|n| {
                deps.get(*n)
                    .map(|d| d.iter().all(|x| !remaining.contains(x)))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            break;
        }
        for n in ready {
            remaining.remove(&n);
            order.push(n);
        }
    }
    (order, remaining)
}

fn issue(table: &str, field: &str, what: &str) -> FieldMergeIssue {
    FieldMergeIssue {
        table: table.to_owned(),
        field: field.to_owned(),
        message: format!("calculated field `{table}.{field}`: {what}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_expr::TableShape;
    use sc_types::{BaseField, BasicType, TypeRef};

    use crate::field::{DataField, DataFieldKind, DbId};
    use crate::table::{AccessRules, TableSource};

    fn calc(name: &str, expr: &str) -> DataField {
        DataField {
            base: BaseField {
                name: name.into(),
                label: name.into(),
                type_: TypeRef::Basic(BasicType::Text),
                attributes: Default::default(),
            },
            required: false,
            unique: false,
            primary_key: false,
            generated: None,
            default_expression: None,
            kind: DataFieldKind::Calc {
                expression: expr.into(),
            },
        }
    }

    fn plain(name: &str) -> DataField {
        DataField::plain(name, TypeRef::Basic(BasicType::Int))
    }

    fn table(name: &str, fields: Vec<DataField>) -> Table {
        Table {
            id: TableId(name.into()),
            name: name.into(),
            database: DbId::primary(),
            source: TableSource::Database,
            fields,
            primary_key: vec!["id".into()],
            label: name.into(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Default::default(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    /// Build a shape that includes every field of `t` as a plain field (calc
    /// fields resolve against it too), so validation can run without a database.
    fn shape_of(t: &Table) -> SchemaShape {
        let mut ts = TableShape::new().primary_key("id");
        for f in &t.fields {
            ts = ts.field(&f.base.name);
        }
        SchemaShape::new().table(&t.name, ts)
    }

    fn run(mut t: Table) -> (Table, Vec<FieldMergeIssue>) {
        let shape = shape_of(&t);
        let mut map = HashMap::new();
        let id = t.id.clone();
        map.insert(id.clone(), std::mem::replace(&mut t, table("_tmp", vec![])));
        let issues = merge_calc_fields(&mut map, &shape);
        (map.remove(&id).unwrap(), issues)
    }

    #[test]
    fn dependent_calc_fields_are_ordered_after_what_they_read() {
        // gross reads tax reads net; the read-time order must be net, tax, gross.
        let t = table(
            "sales",
            vec![
                plain("id"),
                plain("net"),
                calc("gross", "net + tax"),
                calc("tax", "net * 2"),
            ],
        );
        let (t, issues) = run(t);
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        let order: Vec<&str> = t.calc_fields().map(|f| f.base.name.as_str()).collect();
        assert_eq!(order, vec!["tax", "gross"]);
    }

    #[test]
    fn a_cycle_drops_the_fields_and_reports_it() {
        let t = table(
            "t",
            vec![plain("id"), calc("a", "b + 1"), calc("b", "a + 1")],
        );
        let (t, issues) = run(t);
        assert_eq!(t.calc_fields().count(), 0, "cyclic calc fields dropped");
        assert!(
            issues.iter().any(|i| i.message.contains("cycle")),
            "got: {issues:?}"
        );
    }

    #[test]
    fn user_and_flags_are_refused_in_a_calc_field() {
        let t = table(
            "t",
            vec![
                plain("id"),
                plain("owner"),
                calc("mine", "owner === user.id"),
            ],
        );
        let (t, issues) = run(t);
        assert_eq!(t.calc_fields().count(), 0);
        assert!(
            issues.iter().any(|i| i.message.contains("`user`")),
            "got: {issues:?}"
        );
    }

    #[test]
    fn a_dependent_of_an_invalid_field_is_dropped_too() {
        // `bad` references a nonexistent column → invalid; `worse` reads `bad`.
        let t = table(
            "t",
            vec![
                plain("id"),
                calc("bad", "nonexistent + 1"),
                calc("worse", "bad + 1"),
            ],
        );
        let (t, issues) = run(t);
        assert_eq!(t.calc_fields().count(), 0);
        assert!(
            issues
                .iter()
                .any(|i| i.field == "worse" && i.message.contains("reads")),
            "got: {issues:?}"
        );
    }

    #[test]
    fn a_valid_calc_field_survives() {
        let t = table(
            "t",
            vec![plain("id"), plain("net"), calc("doubled", "net * 2")],
        );
        let (t, issues) = run(t);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            t.calc_fields()
                .map(|f| f.base.name.clone())
                .collect::<Vec<_>>(),
            vec!["doubled"]
        );
    }
}
