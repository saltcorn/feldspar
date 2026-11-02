//! The accessibility snapshot `view_app` returns by default (TODO 6b.8).
//!
//! A page as a model reads it: CDP's accessibility tree
//! (`Accessibility.getFullAXTree`), with everything a reader of the page would
//! not notice taken out, one line per node, indented by depth. Interactive
//! nodes carry a short **ref** (`@e3`) that `click` and `fill` name. Refs are
//! handed out in document order, so an unchanged page renders byte-identically
//! and keeps its refs.
//!
//! ```text
//! page "Tasks"
//!   heading "Tasks" [level=1]
//!   textbox "New task" @e1
//!   button "Add" @e2
//!   list
//!     listitem
//!       checkbox "Buy milk" [checked] @e3
//! ```
//!
//! Text costs tokens, so the rendering is capped. A page past the cap ends with
//! a line saying how much was left out and how to narrow it.
//!
//! The renderer reads its own [`AxNode`], deserialised from the protocol's JSON,
//! rather than the generated type, so it can be tested without a browser.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value as Json;

/// The most lines a snapshot renders.
pub const MAX_SNAPSHOT_LINES: usize = 400;

/// The most characters a snapshot renders.
pub const MAX_SNAPSHOT_CHARS: usize = 16_000;

/// The most characters of one node's name or value shown.
const MAX_TEXT: usize = 120;

/// One node of CDP's accessibility tree, as far as the snapshot reads it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxNode {
    pub node_id: String,
    #[serde(default)]
    pub ignored: bool,
    #[serde(default)]
    pub role: Option<AxValue>,
    #[serde(default)]
    pub name: Option<AxValue>,
    #[serde(default)]
    pub value: Option<AxValue>,
    #[serde(default)]
    pub properties: Vec<AxProperty>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub child_ids: Vec<String>,
    #[serde(rename = "backendDOMNodeId", default)]
    pub backend_dom_node_id: Option<i64>,
}

/// A value in the accessibility tree: only the value itself is read.
#[derive(Debug, Clone, Deserialize)]
pub struct AxValue {
    #[serde(default)]
    pub value: Option<Json>,
}

/// A named property of a node (`checked`, `level`, `disabled`, …).
#[derive(Debug, Clone, Deserialize)]
pub struct AxProperty {
    pub name: String,
    pub value: AxValue,
}

/// A rendered snapshot: the text, and the DOM node each ref names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub text: String,
    pub refs: HashMap<String, i64>,
}

/// Roles a model can act on, and so get a ref.
const INTERACTIVE: &[&str] = &[
    "button",
    "checkbox",
    "combobox",
    "link",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "textbox",
    "treeitem",
];

/// Roles that say nothing on their own: shown only when named, and otherwise
/// replaced by their children.
const STRUCTURAL: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "group",
    "LineBreak",
    "InlineTextBox",
    "paragraph",
    "section",
    "div",
];

/// Properties worth a word, in the order they are shown.
const SHOWN: &[&str] = &[
    "level", "checked", "pressed", "selected", "expanded", "disabled", "required", "invalid",
    "focused",
];

/// Render `nodes` (the whole tree, in the protocol's order).
pub fn render(nodes: &[AxNode]) -> Snapshot {
    let by_id: HashMap<&str, &AxNode> = nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();
    let root = nodes.iter().find(|n| {
        n.parent_id
            .as_deref()
            .is_none_or(|p| !by_id.contains_key(p))
    });
    let mut out = Renderer {
        by_id: &by_id,
        lines: Vec::new(),
        refs: HashMap::new(),
        chars: 0,
        dropped: 0,
    };
    if let Some(root) = root {
        out.node(root, 0);
    }
    let mut text = out.lines.join("\n");
    if out.dropped > 0 {
        text.push_str(&format!(
            "\n[{} more lines not shown: wait_for the part you need, or scroll to it]",
            out.dropped
        ));
    }
    Snapshot {
        text,
        refs: out.refs,
    }
}

struct Renderer<'a> {
    by_id: &'a HashMap<&'a str, &'a AxNode>,
    lines: Vec<String>,
    refs: HashMap<String, i64>,
    chars: usize,
    dropped: usize,
}

impl Renderer<'_> {
    fn node(&mut self, node: &AxNode, depth: usize) {
        let role = text_of(node.role.as_ref());
        let name = clip(text_of(node.name.as_ref()).trim());
        let shown = !node.ignored && self.shows(node, &role, &name);
        let child_depth = if shown {
            self.line(node, &role, &name, depth);
            depth + 1
        } else {
            depth
        };
        // A node whose name is its text has nothing more to say below it.
        if shown && role == "StaticText" {
            return;
        }
        for child in &node.child_ids {
            if let Some(child) = self.by_id.get(child.as_str()) {
                self.node(child, child_depth);
            }
        }
    }

    fn shows(&self, node: &AxNode, role: &str, name: &str) -> bool {
        match role {
            "" | "InlineTextBox" | "LineBreak" | "ListMarker" => false,
            "StaticText" => !name.is_empty() && !self.repeats_parent(node, name),
            role if STRUCTURAL.contains(&role) => !name.is_empty(),
            _ => true,
        }
    }

    /// A text node whose parent is already named by exactly this text: a
    /// button's label, a link's.
    fn repeats_parent(&self, node: &AxNode, name: &str) -> bool {
        let Some(parent) = node.parent_id.as_deref().and_then(|p| self.by_id.get(p)) else {
            return false;
        };
        clip(text_of(parent.name.as_ref()).trim()) == name
    }

    fn line(&mut self, node: &AxNode, role: &str, name: &str, depth: usize) {
        let mut line = "  ".repeat(depth);
        line.push_str(match role {
            "RootWebArea" | "WebArea" => "page",
            "StaticText" => "text",
            other => other,
        });
        if !name.is_empty() {
            line.push_str(&format!(" {}", quote(name)));
        }
        let value = clip(text_of(node.value.as_ref()).trim());
        if !value.is_empty() && value != name {
            line.push_str(&format!(" = {}", quote(&value)));
        }
        let mut flags = Vec::new();
        for key in SHOWN {
            let Some(prop) = node.properties.iter().find(|p| p.name == *key) else {
                continue;
            };
            // A heading's level says something; a list item's does not.
            if *key == "level" && !matches!(role, "heading" | "treeitem") {
                continue;
            }
            match prop.value.value.as_ref() {
                Some(Json::Bool(true)) => flags.push((*key).to_owned()),
                Some(Json::String(s)) if s == "true" || s == "mixed" => {
                    flags.push(if s == "true" {
                        (*key).to_owned()
                    } else {
                        format!("{key}=mixed")
                    })
                }
                Some(Json::Number(n)) => flags.push(format!("{key}={n}")),
                _ => {}
            }
        }
        if !flags.is_empty() {
            line.push_str(&format!(" [{}]", flags.join(", ")));
        }
        if INTERACTIVE.contains(&role)
            && let Some(backend) = node.backend_dom_node_id
        {
            let reference = format!("@e{}", self.refs.len() + 1);
            line.push_str(&format!(" {reference}"));
            self.refs.insert(reference, backend);
        }
        if self.lines.len() >= MAX_SNAPSHOT_LINES || self.chars + line.len() > MAX_SNAPSHOT_CHARS {
            self.dropped += 1;
            return;
        }
        self.chars += line.len() + 1;
        self.lines.push(line);
    }
}

/// A value's text: a string as it is, anything else as JSON, nothing as empty.
pub(crate) fn text_of(value: Option<&AxValue>) -> String {
    match value.and_then(|v| v.value.as_ref()) {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Whitespace collapsed, and cut at [`MAX_TEXT`] characters.
fn clip(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_TEXT {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(MAX_TEXT).collect();
    format!("{cut}…")
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> Vec<AxNode> {
        serde_json::from_value(json!([
            {"nodeId": "1", "ignored": false, "role": {"value": "RootWebArea"},
             "name": {"value": "Tasks"}, "childIds": ["2"], "backendDOMNodeId": 1},
            {"nodeId": "2", "ignored": false, "role": {"value": "generic"}, "parentId": "1",
             "childIds": ["3", "4", "6", "8"], "backendDOMNodeId": 2},
            {"nodeId": "3", "ignored": false, "role": {"value": "heading"},
             "name": {"value": "Tasks"}, "parentId": "2", "childIds": ["3a"],
             "properties": [{"name": "level", "value": {"value": 1}}], "backendDOMNodeId": 3},
            {"nodeId": "3a", "ignored": false, "role": {"value": "StaticText"},
             "name": {"value": "Tasks"}, "parentId": "3", "backendDOMNodeId": 30},
            {"nodeId": "4", "ignored": false, "role": {"value": "textbox"},
             "name": {"value": "New task"}, "value": {"value": "milk"}, "parentId": "2",
             "properties": [{"name": "focused", "value": {"value": true}}],
             "childIds": ["5"], "backendDOMNodeId": 4},
            {"nodeId": "5", "ignored": true, "role": {"value": "none"}, "parentId": "4"},
            {"nodeId": "6", "ignored": false, "role": {"value": "button"},
             "name": {"value": "Add"}, "parentId": "2", "childIds": ["7"],
             "backendDOMNodeId": 6},
            {"nodeId": "7", "ignored": false, "role": {"value": "StaticText"},
             "name": {"value": "Add"}, "parentId": "6", "backendDOMNodeId": 7},
            {"nodeId": "8", "ignored": false, "role": {"value": "list"}, "parentId": "2",
             "childIds": ["9"], "backendDOMNodeId": 8},
            {"nodeId": "9", "ignored": false, "role": {"value": "listitem"}, "parentId": "8",
             "childIds": ["10", "11"], "backendDOMNodeId": 9},
            {"nodeId": "10", "ignored": false, "role": {"value": "checkbox"},
             "name": {"value": "Buy milk"}, "parentId": "9",
             "properties": [{"name": "checked", "value": {"value": "true"}},
                            {"name": "disabled", "value": {"value": false}}],
             "backendDOMNodeId": 10},
            {"nodeId": "11", "ignored": false, "role": {"value": "StaticText"},
             "name": {"value": "  due   today "}, "parentId": "9", "backendDOMNodeId": 11}
        ]))
        .unwrap()
    }

    #[test]
    fn a_page_renders_as_a_compact_tree_with_refs_on_what_can_be_used() {
        let snapshot = render(&tree());
        assert_eq!(
            snapshot.text,
            "page \"Tasks\"\n\
             \x20 heading \"Tasks\" [level=1]\n\
             \x20 textbox \"New task\" = \"milk\" [focused] @e1\n\
             \x20 button \"Add\" @e2\n\
             \x20 list\n\
             \x20   listitem\n\
             \x20     checkbox \"Buy milk\" [checked] @e3\n\
             \x20     text \"due today\""
        );
        assert_eq!(snapshot.refs.get("@e1"), Some(&4));
        assert_eq!(snapshot.refs.get("@e2"), Some(&6));
        assert_eq!(snapshot.refs.get("@e3"), Some(&10));
        assert_eq!(snapshot.refs.len(), 3);
    }

    #[test]
    fn an_unchanged_page_renders_identically() {
        assert_eq!(render(&tree()), render(&tree()));
    }

    #[test]
    fn a_long_page_is_capped_with_a_hint() {
        let mut nodes = vec![json!({"nodeId": "root", "role": {"value": "RootWebArea"},
            "childIds": (0..1000).map(|i| format!("b{i}")).collect::<Vec<_>>()})];
        for i in 0..1000 {
            nodes.push(
                json!({"nodeId": format!("b{i}"), "role": {"value": "button"},
                "name": {"value": format!("Button {i}")}, "parentId": "root",
                "backendDOMNodeId": i + 100}),
            );
        }
        let nodes: Vec<AxNode> = serde_json::from_value(Json::Array(nodes)).unwrap();
        let snapshot = render(&nodes);
        let lines: Vec<&str> = snapshot.text.lines().collect();
        assert_eq!(lines.len(), MAX_SNAPSHOT_LINES + 1);
        assert_eq!(
            lines.last().copied(),
            Some("[601 more lines not shown: wait_for the part you need, or scroll to it]")
        );
        assert!(snapshot.text.len() <= MAX_SNAPSHOT_CHARS + 100);
    }

    #[test]
    fn nothing_renders_as_nothing() {
        assert_eq!(render(&[]), Snapshot::default());
    }
}
