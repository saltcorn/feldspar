//! **Tables a module reads because its settings name them** (TODO-payments
//! P4.1).
//!
//! A module's services and webhooks reach only the tables the module declared
//! ([`crate::services`]'s `ScopedTables`). Some modules also need to read a
//! table of the application's: the payments module's plan binding says "plans
//! are the rows of `plans`", and its webhook has to look up which plan a
//! subscription's price is, to know which role to grant. A module says so with
//! a `reads` key, a list of table names or a function of its configuration
//! returning one:
//!
//! ```js
//! reads: (configuration) => (configuration.plans_table ? [configuration.plans_table] : []),
//! ```
//!
//! What the module asks for is not what it gets. A table is **admitted only
//! when the module's stored configuration names it** — when one of the values
//! an admin saved in its settings is that table's name. Module code cannot
//! widen its own reach: a `reads` that answers `users` when nothing in the
//! settings says `users` is an issue on the card, and reading it is refused.
//! What is admitted is read-only, at the module's authority; a write is refused
//! by name.

use std::collections::BTreeSet;

use serde_json::Value as Json;

/// What a module asked to read, split into what its configuration admits and
/// the issues for the rest, in the order asked.
pub fn admitted(asked: &[String], configuration: &sc_types::Attrs) -> (Vec<String>, Vec<String>) {
    let mut named = BTreeSet::new();
    for value in configuration.values() {
        strings_in(value, &mut named);
    }
    let mut tables = Vec::new();
    let mut issues = Vec::new();
    for table in asked {
        let table = table.trim();
        if table.is_empty() || tables.iter().any(|t| t == table) {
            continue;
        }
        if named.contains(table) {
            tables.push(table.to_owned());
        } else {
            issues.push(format!(
                "it asks to read the table `{table}`, which its settings do not name, so it may \
                 not: a module reads only the tables an admin names in its settings"
            ));
        }
    }
    (tables, issues)
}

/// Every string in `value`, trimmed, at any depth.
fn strings_in(value: &Json, out: &mut BTreeSet<String>) {
    match value {
        Json::String(s) => {
            let s = s.trim();
            if !s.is_empty() {
                out.insert(s.to_owned());
            }
        }
        Json::Array(items) => items.iter().for_each(|v| strings_in(v, out)),
        Json::Object(map) => map.values().for_each(|v| strings_in(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(value: Json) -> sc_types::Attrs {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn only_a_table_the_settings_name_is_admitted() {
        let configuration = attrs(json!({
            "plans_table": " plans ",
            "nested": { "more": ["addons"] },
            "grace_days": 3,
        }));
        let asked = ["plans", "addons", "users", "", "plans"].map(str::to_owned);
        let (tables, issues) = admitted(&asked, &configuration);
        assert_eq!(tables, ["plans", "addons"]);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("`users`, which its settings do not name"), "{issues:?}");
    }

    #[test]
    fn nothing_is_admitted_from_empty_settings() {
        let (tables, issues) = admitted(&["plans".to_owned()], &sc_types::Attrs::new());
        assert!(tables.is_empty());
        assert_eq!(issues.len(), 1);
    }
}
