//! Stable locators against the accessibility tree (TODO-checklists C2.2).
//!
//! A checklist step names an element by role and accessible name, by label or
//! by visible text ([`Locator`]); this module finds those in the same tree
//! `view_app`'s snapshot renders (`Accessibility.getFullAXTree`), so "visible"
//! means the same thing to a step as to a model reading a snapshot: an element
//! that is hidden is not in the tree, or is in it as ignored, and is not found.
//! `data-testid` and CSS locators are DOM queries, done by the driver; their
//! elements are described from the tree here when they are in it.
//!
//! Pure functions over [`AxNode`]s, so they are tested without a browser.

use std::collections::HashMap;

use sc_agent::{ElementInfo, Locator};
use serde_json::Value as Json;

use super::snapshot::{AxNode, text_of};

/// The roles a label names: form controls.
const LABELLED: &[&str] = &[
    "textbox",
    "searchbox",
    "combobox",
    "listbox",
    "checkbox",
    "radio",
    "switch",
    "spinbutton",
    "slider",
];

/// The roles whose checked state an [`ElementInfo`] reports.
const CHECKABLE: &[&str] = &[
    "checkbox",
    "radio",
    "switch",
    "menuitemcheckbox",
    "menuitemradio",
];

/// The tree, indexed.
pub struct Tree<'a> {
    nodes: &'a [AxNode],
    by_id: HashMap<&'a str, &'a AxNode>,
    by_backend: HashMap<i64, &'a AxNode>,
}

impl<'a> Tree<'a> {
    pub fn new(nodes: &'a [AxNode]) -> Tree<'a> {
        let by_id = nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();
        let by_backend = nodes
            .iter()
            .filter_map(|n| n.backend_dom_node_id.map(|id| (id, n)))
            .collect();
        Tree {
            nodes,
            by_id,
            by_backend,
        }
    }

    /// The DOM nodes `locator` names, in document order, each once. Empty for
    /// the locators the DOM answers (`testid`, `css`).
    pub fn find(&self, locator: &Locator) -> Vec<i64> {
        let mut out: Vec<i64> = Vec::new();
        for node in self.nodes.iter().filter(|n| !n.ignored) {
            let role = text_of(node.role.as_ref());
            let name = text_of(node.name.as_ref());
            let target = match locator {
                Locator::Role {
                    role: want,
                    name: want_name,
                } => (role.eq_ignore_ascii_case(want)
                    && want_name.as_ref().is_none_or(|m| m.matches(&name)))
                .then_some(node.backend_dom_node_id)
                .flatten(),
                Locator::Label(text) => (LABELLED.contains(&role.as_str()) && text.matches(&name))
                    .then_some(node.backend_dom_node_id)
                    .flatten(),
                // A run of text names the element it is in.
                Locator::Text(text) => (role == "StaticText" && text.matches(&name))
                    .then(|| self.element_of(node))
                    .flatten(),
                Locator::TestId(_) | Locator::Css(_) => None,
            };
            if let Some(id) = target
                && !out.contains(&id)
            {
                out.push(id);
            }
        }
        out
    }

    /// The nearest element at or above a text node: the node itself is a text
    /// node, which cannot be clicked or laid out on its own.
    fn element_of(&self, node: &AxNode) -> Option<i64> {
        let mut current = node.parent_id.as_deref().and_then(|p| self.by_id.get(p));
        while let Some(parent) = current {
            let role = text_of(parent.role.as_ref());
            if !matches!(role.as_str(), "StaticText" | "InlineTextBox")
                && let Some(id) = parent.backend_dom_node_id
            {
                return Some(id);
            }
            current = parent.parent_id.as_deref().and_then(|p| self.by_id.get(p));
        }
        None
    }

    /// The DOM node `backend` described: from the tree when it is shown there,
    /// and as not visible when it is not.
    pub fn describe(&self, backend: i64) -> ElementInfo {
        let Some(node) = self.by_backend.get(&backend).filter(|n| !n.ignored) else {
            return ElementInfo {
                enabled: true,
                ..ElementInfo::default()
            };
        };
        let role = text_of(node.role.as_ref());
        let value = text_of(node.value.as_ref());
        let flag = |key: &str| {
            node.properties
                .iter()
                .find(|p| p.name == key)
                .and_then(|p| p.value.value.as_ref())
                .map(|v| matches!(v, Json::Bool(true)) || v.as_str() == Some("true"))
        };
        ElementInfo {
            checked: CHECKABLE
                .contains(&role.as_str())
                .then(|| flag("checked").unwrap_or(false)),
            enabled: !flag("disabled").unwrap_or(false),
            name: text_of(node.name.as_ref()).trim().to_owned(),
            value: (!value.is_empty()).then_some(value),
            visible: true,
            role,
        }
    }

    /// What a person can read: every shown node's accessible name, one per
    /// line, a text run that only repeats its parent's name left out.
    pub fn visible_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for node in self.nodes.iter().filter(|n| !n.ignored) {
            let role = text_of(node.role.as_ref());
            if matches!(role.as_str(), "RootWebArea" | "WebArea" | "InlineTextBox") {
                continue;
            }
            let name = collapse(&text_of(node.name.as_ref()));
            if name.is_empty() {
                continue;
            }
            let repeats = role == "StaticText"
                && node
                    .parent_id
                    .as_deref()
                    .and_then(|p| self.by_id.get(p))
                    .is_some_and(|p| collapse(&text_of(p.name.as_ref())) == name);
            if !repeats {
                lines.push(name);
            }
        }
        lines.join("\n")
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nodes() -> Vec<AxNode> {
        serde_json::from_value(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Orders"},
             "childIds": ["2", "3", "5", "7", "9", "11", "12"], "backendDOMNodeId": 1},
            {"nodeId": "2", "role": {"value": "heading"}, "name": {"value": "Orders"},
             "parentId": "1", "childIds": ["2a"], "backendDOMNodeId": 2},
            {"nodeId": "2a", "role": {"value": "StaticText"}, "name": {"value": "Orders"},
             "parentId": "2", "backendDOMNodeId": 20},
            {"nodeId": "3", "role": {"value": "button"}, "name": {"value": "Save"},
             "parentId": "1", "childIds": ["4"], "backendDOMNodeId": 3},
            {"nodeId": "4", "role": {"value": "StaticText"}, "name": {"value": "Save"},
             "parentId": "3", "backendDOMNodeId": 40},
            {"nodeId": "5", "role": {"value": "button"}, "name": {"value": "Save draft"},
             "parentId": "1", "childIds": ["6"], "backendDOMNodeId": 5,
             "properties": [{"name": "disabled", "value": {"value": true}}]},
            {"nodeId": "6", "role": {"value": "StaticText"}, "name": {"value": "Save draft"},
             "parentId": "5", "backendDOMNodeId": 60},
            {"nodeId": "7", "role": {"value": "textbox"}, "name": {"value": "Customer email"},
             "value": {"value": "a@b.c"}, "parentId": "1", "backendDOMNodeId": 7},
            {"nodeId": "9", "role": {"value": "checkbox"}, "name": {"value": "Paid"},
             "parentId": "1", "backendDOMNodeId": 9,
             "properties": [{"name": "checked", "value": {"value": "true"}}]},
            {"nodeId": "11", "role": {"value": "generic"}, "parentId": "1",
             "childIds": ["11a"], "backendDOMNodeId": 11},
            {"nodeId": "11a", "role": {"value": "StaticText"}, "name": {"value": "Total:  42 EUR"},
             "parentId": "11", "backendDOMNodeId": 110},
            {"nodeId": "12", "ignored": true, "role": {"value": "StaticText"},
             "name": {"value": "Secret"}, "parentId": "1", "backendDOMNodeId": 12}
        ]))
        .unwrap()
    }

    fn find(raw: &str) -> Vec<i64> {
        let nodes = nodes();
        Tree::new(&nodes).find(&raw.parse().unwrap())
    }

    #[test]
    fn a_role_and_name_find_the_element_exactly_or_by_part() {
        assert_eq!(find(r#"button "Save""#), vec![3]);
        assert_eq!(find("button save"), vec![3, 5]);
        assert_eq!(find("role=button"), vec![3, 5]);
        assert_eq!(find(r#"heading "Orders""#), vec![2]);
        assert!(find(r#"link "Save""#).is_empty());
    }

    #[test]
    fn a_label_finds_only_form_controls() {
        assert_eq!(find("label=email"), vec![7]);
        assert_eq!(find("label=Paid"), vec![9]);
        assert!(find("label=Save").is_empty());
    }

    #[test]
    fn text_finds_the_element_it_is_in_and_never_hidden_text() {
        assert_eq!(find(r#"text="Save""#), vec![3]);
        assert_eq!(find("text=total: 42"), vec![11]);
        assert!(find("text=Secret").is_empty());
    }

    #[test]
    fn an_element_is_described_from_the_tree() {
        let nodes = nodes();
        let tree = Tree::new(&nodes);
        let save_draft = tree.describe(5);
        assert_eq!(save_draft.role, "button");
        assert!(!save_draft.enabled);
        assert_eq!(save_draft.checked, None);
        let email = tree.describe(7);
        assert_eq!(email.value.as_deref(), Some("a@b.c"));
        assert!(email.enabled && email.visible);
        assert_eq!(tree.describe(9).checked, Some(true));
        // Not in the tree, or ignored there: not visible.
        assert!(!tree.describe(999).visible);
        assert!(!tree.describe(12).visible);
    }

    #[test]
    fn the_visible_text_leaves_out_hidden_and_repeated_text() {
        let nodes = nodes();
        let text = Tree::new(&nodes).visible_text();
        assert_eq!(
            text,
            "Orders\nSave\nSave draft\nCustomer email\nPaid\nTotal: 42 EUR"
        );
        // What `browser.expect_text` matches against: text split over two
        // elements is found, hidden text is not.
        let has = |t: &str| sc_agent::TextMatch::contains(t).matches(&text);
        assert!(has("total: 42 eur"));
        assert!(has("Save draft Customer"));
        assert!(!has("Secret"));
    }
}
