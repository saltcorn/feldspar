//! Save-time validation of views and pages (TODO "Saltcorn UI" §1, §11).
//!
//! The checks that need nothing but values are pure functions here, so they are
//! asserted without a database; the store resolves the application, the roles
//! and the registry and hands them in.

use sc_app::Application;
use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::patterns::{PatternInfo, find_pattern};
use crate::view::View;

/// Characters a name may not contain, because the name is a path segment
/// (`/view/:name`, `/page/:name`) and each of these either ends a segment or
/// changes what the URL means even when escaped by a careless link.
const RESERVED: [char; 5] = ['/', '\\', '?', '#', '%'];

/// Refuse a view or page name that cannot be its own URL path segment.
///
/// **Spaces are allowed**, and deliberately: v1 names views `List Books` and
/// `Filter books`, and a restored backup is exactly the case this has to accept.
/// A space percent-encodes to one unambiguous segment; a `/` does not.
pub fn check_name(kind: &str, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::invalid(format!("a {kind} needs a name")));
    }
    if name.trim() != name {
        return Err(Error::invalid(format!(
            "{kind} name `{name}` starts or ends with whitespace"
        )));
    }
    if name == "." || name == ".." {
        return Err(Error::invalid(format!(
            "`{name}` is not a {kind} name: it is a relative path segment"
        )));
    }
    if let Some(c) = name
        .chars()
        .find(|c| RESERVED.contains(c) || c.is_control())
    {
        return Err(Error::invalid(format!(
            "{kind} name `{}` contains {}, which cannot appear in the URL the {kind} is served at",
            name.escape_debug(),
            match c {
                c if c.is_control() => "a control character".to_owned(),
                c => format!("`{c}`"),
            }
        )));
    }
    Ok(())
}

/// Refuse a view whose pattern is not registered, or whose table is missing or
/// outside the application's subset.
pub(crate) fn check_view_shape(
    view: &View,
    app: &Application,
    patterns: &[PatternInfo],
) -> Result<()> {
    let Some(pattern) = find_pattern(patterns, &view.viewpattern) else {
        let names: Vec<&str> = patterns.iter().map(|p| p.name.as_str()).collect();
        return Err(Error::invalid(format!(
            "view `{}` uses the view pattern `{}`, which is not registered; \
             the registered patterns are {}",
            view.name,
            view.viewpattern,
            names.join(", ")
        )));
    };
    match (&view.table_name, pattern.tableless) {
        (None, false) => Err(Error::invalid(format!(
            "view `{}` has no table, and the `{}` pattern needs one",
            view.name, pattern.name
        ))),
        (Some(table), _) if !app.tables.iter().any(|t| t.0 == *table) => {
            Err(Error::invalid(format!(
                "view `{}` is over the table `{table}`, which is not in application `{}`'s \
                 table subset; add it to the application first",
                view.name, app.name
            )))
        }
        _ => Ok(()),
    }
}

/// v1's own view actions (§12.1): run by the pattern that renders them — a
/// form's submit, a link to `/delete/…` — and never by name through a trigger.
/// The set is fixed; a trigger of the same name cannot take one out of it.
///
/// `Clear` is Filter's: its builder offers it as a built-in action
/// (`filter.ts`'s `builtInActions`), and the pattern renders it as a link or a
/// button calling `clear_state`, in the browser.
pub const VIEW_ACTIONS: [&str; 12] = [
    "Delete",
    "Save",
    "SaveAndContinue",
    "UpdateMatchingRows",
    "SubmitWithAjax",
    "Reset",
    "GoBack",
    "Cancel",
    "Login",
    "Sign up",
    "Logout",
    "Clear",
];

/// v1's name for an action column that runs its steps in order; each step is
/// checked in its place.
const MULTI_STEP: &str = "Multi-step action";

/// Every action a view's configuration names, in the order it names them: the
/// action columns (and a multi-step column's steps), the `action` segments and
/// container `click_action`s anywhere in its layout, and a row-click action.
pub fn configured_actions(configuration: &sc_types::Attrs) -> Vec<String> {
    fn push(out: &mut Vec<String>, name: Option<&Json>) {
        if let Some(name) = name.and_then(Json::as_str).filter(|n| !n.is_empty())
            && !out.iter().any(|seen| seen == name)
        {
            out.push(name.to_owned());
        }
    }
    fn action(out: &mut Vec<String>, item: &serde_json::Map<String, Json>) {
        let name = item.get("action_name");
        if name.and_then(Json::as_str) == Some(MULTI_STEP) {
            for step in item
                .get("step_action_names")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                push(out, Some(step));
            }
        } else {
            push(out, name);
        }
    }
    fn walk(out: &mut Vec<String>, value: &Json) {
        match value {
            Json::Object(item) => {
                if item.get("type").and_then(Json::as_str) == Some("action") {
                    action(out, item);
                }
                push(out, item.get("click_action"));
                for child in item.values() {
                    walk(out, child);
                }
            }
            Json::Array(items) => items.iter().for_each(|i| walk(out, i)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for column in configuration
        .get("columns")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_object)
    {
        if column.get("type").and_then(Json::as_str) == Some("Action") {
            action(&mut out, column);
        }
    }
    if let Some(layout) = configuration.get("layout") {
        walk(&mut out, layout);
    }
    push(
        &mut out,
        configuration
            .get("default_state")
            .and_then(|d| d.get("_row_click_action")),
    );
    out
}

/// The relation prefixes a v1 view reference may carry — `Own:Show Books`,
/// `ChildList:List Books.books.author` — ahead of the view's name. `Own` and
/// `Independent` are followed by the name alone; the others by the name, a dot,
/// and the path the related rows are found along.
const RELATION_PREFIXES: [&str; 5] = [
    "Own",
    "Independent",
    "ChildList",
    "ParentShow",
    "OneToOneShow",
];

/// The keys under which a v1 configuration or layout names another view: an
/// embedded view segment or a view link (`view`, `view_name`), a List's
/// `view_to_create`, an Edit's `view_when_done`, and the views a Feed or a
/// ListShowList shows (`show_view`, `list_view`).
const VIEW_REFERENCE_KEYS: [&str; 6] = [
    "view",
    "view_name",
    "view_to_create",
    "view_when_done",
    "show_view",
    "list_view",
];

/// Every view a view's configuration or a page's layout names, by name, sorted
/// and without repeats — what a restore checks against the views it brought, so
/// a link to a view that did not come is reported rather than found by clicking
/// it (TODO "Saltcorn UI" 8.4).
pub fn referenced_views(value: &Json) -> Vec<String> {
    fn name_of(raw: &str) -> Option<String> {
        let raw = raw.trim();
        let name = match raw.split_once(':') {
            Some((prefix @ ("Own" | "Independent"), rest))
                if RELATION_PREFIXES.contains(&prefix) =>
            {
                rest
            }
            Some((prefix, rest)) if RELATION_PREFIXES.contains(&prefix) => {
                rest.split('.').next().unwrap_or(rest)
            }
            _ => raw,
        };
        (!name.is_empty()).then(|| name.to_owned())
    }
    fn walk(out: &mut std::collections::BTreeSet<String>, value: &Json) {
        match value {
            Json::Object(item) => {
                for (key, child) in item {
                    if VIEW_REFERENCE_KEYS.contains(&key.as_str())
                        && let Some(name) = child.as_str().and_then(name_of)
                    {
                        out.insert(name);
                    }
                    walk(out, child);
                }
            }
            Json::Array(items) => items.iter().for_each(|i| walk(out, i)),
            _ => {}
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(&mut out, value);
    out.into_iter().collect()
}

/// The keys whose text names a field of the view's own table: a column's or a
/// layout item's field, a feed's sort order, and a join's first hop.
const FIELD_REFERENCE_KEYS: [&str; 3] = ["field_name", "order_field", "join_field"];

/// Every field of its own table a view's configuration names, sorted and
/// without repeats — a column's or layout item's `field_name`, a feed's
/// `order_field`, and the key a `join_field` (`author.last_name`) starts from.
///
/// What a checklist's `saltcorn_ui.views_render` checks against the table as
/// it is now (TODO-checklists C3.3): v1's patterns render a field that has gone
/// as an empty cell or a missing column, not as an error, so a view broken by a
/// field's deletion still answers 200.
pub fn referenced_fields(configuration: &Json) -> Vec<String> {
    fn walk(out: &mut std::collections::BTreeSet<String>, value: &Json) {
        match value {
            Json::Object(item) => {
                for (key, child) in item {
                    if FIELD_REFERENCE_KEYS.contains(&key.as_str())
                        && let Some(raw) = child.as_str()
                        && let Some(first) = raw.split('.').next().map(str::trim)
                        && !first.is_empty()
                    {
                        out.insert(first.to_owned());
                    }
                    walk(out, child);
                }
            }
            Json::Array(items) => items.iter().for_each(|i| walk(out, i)),
            _ => {}
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(&mut out, configuration);
    out.into_iter().collect()
}

/// Refuse a view that names an action this server will not run (§12.3): every
/// action it names is one of v1's [`VIEW_ACTIONS`] or a trigger in the
/// application's declared subset. A v1 state action, a plugin's action, a
/// `Toggle` column, a trigger the application does not declare — each is
/// refused here, naming it, rather than failing when somebody clicks it.
pub fn check_view_actions(view: &View, app: &Application) -> Result<()> {
    for name in configured_actions(&view.configuration) {
        if VIEW_ACTIONS.contains(&name.as_str()) || app.triggers.iter().any(|t| t.0 == name) {
            continue;
        }
        let declared: Vec<&str> = app.triggers.iter().map(|t| t.0.as_str()).collect();
        return Err(Error::invalid(format!(
            "view `{}` runs the action `{name}`, which is neither one of v1's view actions ({}) \
             nor a trigger of application `{}` ({}); add a trigger of that name to the \
             application, or remove the action from the view",
            view.name,
            VIEW_ACTIONS.join(", "),
            app.name,
            if declared.is_empty() {
                "it declares none".to_owned()
            } else {
                format!("it declares {}", declared.join(", "))
            }
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::builtin_patterns;
    use sc_app::FrameworkRef;
    use sc_catalog::TableId;

    #[test]
    fn a_v1_name_with_spaces_is_a_name() {
        assert!(check_name("view", "List Books").is_ok());
        assert!(check_name("page", "BooksOverview").is_ok());
    }

    #[test]
    fn a_name_that_is_not_one_path_segment_is_refused_by_what_is_wrong() {
        let msg = |n: &str| check_name("view", n).unwrap_err().to_string();
        assert!(msg("").contains("needs a name"));
        assert!(msg(" books").contains("whitespace"));
        assert!(msg("..").contains("relative path segment"));
        assert!(msg("books/authors").contains("`/`"));
        assert!(msg("50%").contains("`%`"));
        assert!(msg("a\nb").contains("control character"));
    }

    #[test]
    fn a_view_may_run_the_view_actions_and_the_applications_triggers_and_nothing_else() {
        let app = Application::new("Books", "books", FrameworkRef::new("saltcorn-ui"))
            .with_table(TableId("books".to_owned()))
            .with_trigger(sc_app::TriggerRef::new("TrimPages"));
        let configuration = serde_json::json!({
            "columns": [
                { "type": "Action", "action_name": "Delete" },
                { "type": "Action", "action_name": "TrimPages" },
                { "type": "Field", "field_name": "title" },
            ],
            "layout": { "above": [
                { "type": "action", "action_name": "Save" },
                { "type": "container", "click_action": "TrimPages", "contents": [] },
            ]},
        });
        let mut view = View::new(app.id, "List Books", "List", "books")
            .configuration(configuration.as_object().cloned().unwrap());
        assert_eq!(
            configured_actions(&view.configuration),
            ["Delete", "TrimPages", "Save"]
        );
        check_view_actions(&view, &app).unwrap();

        // A v1 state action, in a multi-step column's second step.
        view.configuration["columns"] = serde_json::json!([{
            "type": "Action",
            "action_name": "Multi-step action",
            "step_action_names": ["TrimPages", "run_js_code"],
        }]);
        let msg = check_view_actions(&view, &app).unwrap_err().to_string();
        assert!(msg.contains("`run_js_code`"), "{msg}");
        assert!(msg.contains("it declares TrimPages"), "{msg}");

        // A trigger the application does not declare, deep in a layout.
        view.configuration.remove("columns");
        view.configuration["layout"] = serde_json::json!({ "besides": [{ "contents": { "type": "action", "action_name": "Notify" } }] });
        let msg = check_view_actions(&view, &app).unwrap_err().to_string();
        assert!(
            msg.contains("`Notify`") && msg.contains("application `Books`"),
            "{msg}"
        );
    }

    /// The ways BooksDB's views name one another: a List's view links, in both
    /// the relation-prefixed and the plain spelling, and a Filter's embedded List.
    #[test]
    fn the_views_a_configuration_names_are_found_wherever_v1_puts_them() {
        let list = serde_json::json!({
            "columns": [
                { "type": "ViewLink", "view": "Own:Show Authors", "view_name": "Show Authors" },
                { "type": "ViewLink", "view": "ChildList:List Books.Books.author" },
                { "type": "Action", "action_name": "Delete" },
            ],
            "view_to_create": "Edit Authors",
            "view_when_done": "",
        });
        assert_eq!(
            referenced_views(&list),
            ["Edit Authors", "List Books", "Show Authors"]
        );
        let filter = serde_json::json!({
            "layout": { "above": [{ "type": "view", "view": "List Books", "state": "shared" }] },
        });
        assert_eq!(referenced_views(&filter), ["List Books"]);
        // A name that merely contains a colon is a name.
        assert_eq!(
            referenced_views(&serde_json::json!({ "show_view": "Books: detail" })),
            ["Books: detail"]
        );
    }

    #[test]
    fn a_tableless_view_of_a_pattern_that_needs_a_table_is_refused() {
        let app = Application::new("Books", "books", FrameworkRef::new("saltcorn-ui"))
            .with_table(TableId("books".to_owned()));
        let mut view = View::new(app.id, "List Books", "List", "books");
        assert!(check_view_shape(&view, &app, &builtin_patterns()).is_ok());
        view.table_name = None;
        let err = check_view_shape(&view, &app, &builtin_patterns()).unwrap_err();
        assert!(err.to_string().contains("needs one"), "{err}");
    }
    #[test]
    fn the_fields_a_configuration_names_are_its_own_tables() {
        let configuration = serde_json::json!({
            "columns": [
                { "type": "Field", "field_name": "pages" },
                { "type": "JoinField", "join_field": "author.last_name" },
                { "type": "ViewLink", "view": "Show Books" },
            ],
            "layout": { "above": [{ "type": "field", "field_name": "title" }, { "field_name": "" }] },
            "order_field": "title",
        });
        assert_eq!(
            referenced_fields(&configuration),
            ["author", "pages", "title"]
        );
    }
}
