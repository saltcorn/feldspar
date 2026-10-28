//! The admin UI's **help topics** — v1's `packages/server/help/*.tmd`.
//!
//! A setting that needs more than its sublabel can say names a topic
//! ([`FormField::help_topic`](https://docs.rs/sc-types) on a declared setting, or a
//! screen's own control), and the admin UI shows a question-mark beside it that
//! opens the topic in a popup. A topic is a **markdown template**: it is filled
//! in with what the screen is about — the table's fields, the key fields' joins,
//! the trigger's event, the admin's own user object — so the help for an
//! ownership formula lists *this* table's `Ⱶ`-paths rather than describing
//! `Ⱶ`-paths in general.
//!
//! ## The template language
//!
//! v1's templates were `underscore` templates, which run arbitrary JavaScript
//! with the server's models in scope. These are [MiniJinja] templates instead: a
//! template can print, loop and branch over the context it is given and nothing
//! else, so a topic is a document and not a program.
//!
//! ```text
//! {% if table %}`{{ table.name }}` has these fields:
//! {% for f in table.fields %}
//! - `{{ f.name }}` ({{ f.type }})
//! {%- endfor %}{% endif %}
//! ```
//!
//! An unknown name is undefined and prints as nothing (also through a chain of
//! attributes), so a topic opened from a screen that has no table still renders.
//!
//! ## The context
//!
//! Plain JSON, built by the server from the request (`getHelp`), so this crate
//! knows nothing of the catalog. Every key is optional:
//!
//! | key | what it is |
//! |---|---|
//! | `table` | `{ name, label, fields, inbound }`: the screen's table. Each field is `{ name, label, type, required, key_to }`, `key_to` being `{ table, fields: [{ name, label, type }] }` for a key field and null otherwise; `inbound` is `[{ table, field }]`, the keys of other tables that point here. |
//! | `event` | the trigger's event (`insert`, `update`, `none`, …) |
//! | `user` | the logged-in admin's user object, as a formula sees it |
//! | `user_fields` | `[{ name, label, type }]`: the users table's fields |
//! | `tables` | `[{ name, label }]`: every table a code body can reach as `db.<name>` |
//! | `triggers` | the names of the triggers `trigger("name")` can run |
//!
//! ## Rendering
//!
//! The filled-in markdown is rendered to HTML with tables and strikethrough.
//! **Raw HTML is not passed through** — it is printed as text — and a link may
//! only go to `http(s):`, `mailto:`, a path on this server, or another topic
//! (`help:<topic>`, which the popup opens in place). The context carries values
//! an admin or a user typed (a field label, the admin's own name), and neither
//! should be able to put a script or a `javascript:` link into the admin UI.
//!
//! [MiniJinja]: https://docs.rs/minijinja

use minijinja::{Environment, UndefinedBehavior, Value};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
use sc_error::{Error, Result};
use serde_json::Value as Json;

/// One help topic: its id (what a [`FormField`](https://docs.rs/sc-types)
/// names), its title, and its markdown template.
#[derive(Debug, Clone, Copy)]
pub struct Topic {
    /// The id a setting or a screen names the topic by, e.g. `ownership-formula`.
    pub id: &'static str,
    /// The popup's title.
    pub title: &'static str,
    /// The markdown template (see the module docs).
    pub template: &'static str,
}

/// Every topic. Compiled in, so a single binary has its help with it.
pub const TOPICS: &[Topic] = &[
    Topic {
        id: "ownership-formula",
        title: "Ownership formula",
        template: include_str!("../topics/ownership-formula.md"),
    },
    Topic {
        id: "javascript-code",
        title: "JavaScript code",
        template: include_str!("../topics/javascript-code.md"),
    },
];

/// The topic named `id`, if there is one.
pub fn topic(id: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|t| t.id == id)
}

/// A rendered topic: the popup's title and its body as HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpPage {
    /// The topic's title.
    pub title: String,
    /// The topic's body, as HTML.
    pub html: String,
}

/// Render topic `id` with `context` (see the module docs for its keys).
///
/// An unknown topic is [`Error::not_found`]. A template that does not render is
/// [`Error::invalid`] naming the topic and the line — a bug in the topic, which
/// the tests render every topic to catch.
pub fn render(id: &str, context: &Json) -> Result<HelpPage> {
    let topic = topic(id).ok_or_else(|| Error::not_found(format!("no help topic `{id}`")))?;
    let markdown = fill(topic, context)?;
    Ok(HelpPage {
        title: topic.title.to_owned(),
        html: markdown_to_html(&markdown),
    })
}

/// Fill `topic`'s template in with `context`, giving markdown.
fn fill(topic: &Topic, context: &Json) -> Result<String> {
    let mut env = Environment::new();
    // Undefined all the way down a chain: `table.fields` with no `table` is
    // nothing rather than an error, so one template serves every screen.
    env.set_undefined_behavior(UndefinedBehavior::Chainable);
    // Kept: a topic is laid out as markdown, where a line break can matter.
    env.set_keep_trailing_newline(true);
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    env.render_str(topic.template, Value::from_serialize(context))
        .map_err(|e| {
            Error::invalid(format!(
                "help topic `{}` does not render (line {}): {e}",
                topic.id,
                e.line().unwrap_or(0)
            ))
        })
}

/// Render markdown to HTML, raw HTML printed as text and links limited to the
/// schemes in the module docs.
pub fn markdown_to_html(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        other => other,
    });
    let mut html = String::with_capacity(markdown.len() * 3 / 2);
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

/// `url` when it is one a topic may link to, `#` otherwise.
fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    let lower = url.trim_start().to_ascii_lowercase();
    // A scheme is whatever comes before the first `:` that precedes any `/`,
    // `?` or `#`; no scheme at all is a path on this server or an anchor.
    let scheme = lower
        .split(['/', '?', '#'])
        .next()
        .and_then(|head| head.split_once(':'))
        .map(|(scheme, _)| scheme.to_owned());
    match scheme.as_deref() {
        None | Some("http" | "https" | "mailto" | "help") => url,
        Some(_) => CowStr::Borrowed("#"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A table with a key to the users table, a key to another table, and a
    /// child table pointing back — everything the topics branch on.
    fn full_context() -> Json {
        json!({
            "table": {
                "name": "expenses",
                "label": "Expenses",
                "fields": [
                    { "name": "id", "label": "ID", "type": "int", "required": true, "key_to": null },
                    { "name": "amount", "label": "Amount", "type": "float", "required": true, "key_to": null },
                    { "name": "filed_by", "label": "Filed by", "type": "uuid", "required": false,
                      "key_to": { "table": "users", "fields": [
                          { "name": "id", "label": "ID", "type": "uuid" },
                          { "name": "email", "label": "Email", "type": "text" } ] } },
                    { "name": "project", "label": "Project", "type": "int", "required": false,
                      "key_to": { "table": "projects", "fields": [
                          { "name": "id", "label": "ID", "type": "int" },
                          { "name": "manager", "label": "Manager", "type": "uuid" } ] } }
                ],
                "inbound": [ { "table": "receipts", "field": "expense" } ]
            },
            "event": "update",
            "user": { "id": "00000000-0000-0000-0000-000000000001", "role": 1, "email": "admin@example.com" },
            "user_fields": [ { "name": "email", "label": "Email", "type": "text" },
                             { "name": "department", "label": "Department", "type": "text" } ],
            "tables": [ { "name": "expenses", "label": "Expenses" }, { "name": "projects", "label": "Projects" } ],
            "triggers": [ "send_invoice" ]
        })
    }

    #[test]
    fn every_topic_renders_with_and_without_a_context() {
        for t in TOPICS {
            let bare = render(t.id, &json!({})).unwrap();
            assert_eq!(bare.title, t.title);
            assert!(!bare.html.is_empty(), "{} renders empty", t.id);
            let full = render(t.id, &full_context()).unwrap();
            assert!(
                full.html.len() > bare.html.len(),
                "{} ignores its context",
                t.id
            );
        }
    }

    #[test]
    fn the_ownership_topic_lists_this_tables_fields_joins_and_children() {
        let page = render("ownership-formula", &full_context()).unwrap();
        assert!(page.html.contains("<code>amount</code>"), "{}", page.html);
        assert!(page.html.contains("<code>projectⱵmanager</code>"));
        assert!(page.html.contains("receiptsↃexpense"));
        assert!(page.html.contains("<code>user.department</code>"));
        // A key to the users table makes the obvious formula the first example.
        assert!(page.html.contains("filed_by === user.id"));
        // The admin's own user object, as v1 showed it.
        assert!(page.html.contains("admin@example.com"));
    }

    #[test]
    fn the_javascript_topic_follows_the_event_and_the_tables() {
        let page = render("javascript-code", &full_context()).unwrap();
        assert!(
            page.html.contains("<code>row.amount</code>"),
            "{}",
            page.html
        );
        assert!(page.html.contains("db.expenses"));
        assert!(page.html.contains("send_invoice"));
        // An update has an `old` row; a `none` trigger reads `payload` instead.
        assert!(page.html.contains("<code>old</code>"));
        let mut none = full_context();
        none["event"] = json!("none");
        let page = render("javascript-code", &none).unwrap();
        assert!(!page.html.contains("<code>row.amount</code>"));
    }

    #[test]
    fn an_unknown_topic_is_not_found() {
        assert!(render("no-such-topic", &json!({})).is_err());
    }

    #[test]
    fn raw_html_and_unsafe_links_from_the_context_are_neutralised() {
        let html = markdown_to_html(
            "<img src=x onerror=alert(1)> [a](javascript:alert(1)) [b](https://x.org) \
             [c](/admin/tables) [d](help:javascript-code)",
        );
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("&lt;img"));
        assert!(html.contains(r##"<a href="#">a</a>"##));
        assert!(html.contains(r#"href="https://x.org""#));
        assert!(html.contains(r#"href="/admin/tables""#));
        assert!(html.contains(r#"href="help:javascript-code""#));
    }
}
