//! Saving a layout from the builder (TODO "The builder" §6), and what names a
//! page.
//!
//! The builder writes two things: a view's `{ columns, layout }` for one of its
//! pattern's builder steps, and a page's layout. Either may carry
//! `libraryUpdates`, v1's in-place edits to the shared components the layout
//! places. A layout save is a save:
//!
//! - **A view's layout** is merged into its configuration by v1's rule
//!   ([`merge_view_layout`]) and saved through [`save_view`](crate::save_view)'s
//!   checks; the pattern's other steps are replayed by the caller, which holds
//!   the view runtime.
//! - **A page's layout** replaces the page's, and is checked as a page is
//!   ([`validate_page`]): its actions are v1's page actions or the
//!   application's triggers, and every view it embeds or links to is one of the
//!   application's.
//! - **Both** refuse a `library` segment naming an item this application does
//!   not have, and write the library edits **in the same transaction** as the
//!   view or page, so a refused save leaves none of them applied. v1 writes
//!   them one after another, last write wins; the transaction is the one
//!   improvement taken, because this server has one.
//!
//! Nothing is normalised in either direction: what the builder's `storage.js`
//! wrote is what is stored.

use std::collections::BTreeSet;

use sc_app::Application;
use sc_catalog::{Catalog, SharedTx};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::library::{
    LibraryItem, LibraryUpdate, apply_library_updates_in, collect_library_ids, list_library,
};
use crate::snapshot::MENU_CONFIG_KEY;
use crate::store::{
    check_role, list_views, require_application, save_page_on, save_view_on, validate_view,
};
use crate::validate::{check_name, configured_actions, referenced_views};
use crate::view::{Page, View};

/// v1's page actions: the built-ins its page builder offers (`pageBuilderData`'s
/// `builtInActions`). Anything else an `action` segment on a page runs is a
/// trigger the application declares.
pub const PAGE_ACTIONS: [&str; 1] = ["GoBack"];

/// Merge what the builder wrote for one step into a view's configuration: under
/// the step's `contextField` when it has one, else at the top level. That is the
/// `step.builder` branch of v1's `Workflow.run` (`models/workflow.ts`).
///
/// v1's `viewedit/savebuilder` spreads the whole body into the configuration
/// instead, which is the same thing for Show, Edit, List and Filter, none of
/// whose builder steps has a `contextField`, so one rule serves both.
///
/// Refused, naming the view and the key, when the `contextField` holds
/// something other than an object: v1 would spread its characters.
pub fn merge_view_layout(
    view: &str,
    configuration: &mut Attrs,
    context_field: Option<&str>,
    columns: Json,
    layout: Json,
) -> Result<()> {
    let target = match context_field {
        None => configuration,
        Some(key) => {
            let slot = configuration
                .entry(key.to_owned())
                .or_insert_with(|| Json::Object(Attrs::new()));
            if slot.is_null() {
                *slot = Json::Object(Attrs::new());
            }
            slot.as_object_mut().ok_or_else(|| {
                Error::invalid(format!(
                    "view `{view}` keeps its layout under `{key}`, which holds something other \
                     than an object"
                ))
            })?
        }
    };
    target.insert("columns".to_owned(), columns);
    target.insert("layout".to_owned(), layout);
    Ok(())
}

/// Refuse a `library` segment anywhere in `roots` whose `library_id` is not one
/// of `library`'s items, naming `what` placed it and the id it named.
pub fn check_library_placements<'a>(
    what: &str,
    roots: impl IntoIterator<Item = &'a Json>,
    application: &str,
    library: &[LibraryItem],
) -> Result<()> {
    let mut placed = BTreeSet::new();
    for root in roots {
        collect_library_ids(root, &mut placed);
    }
    for id in placed {
        let known = uuid::Uuid::parse_str(&id)
            .ok()
            .is_some_and(|id| library.iter().any(|item| item.id.0 == id));
        if !known {
            return Err(Error::invalid(format!(
                "{what} places the library item `{id}`, which is not an item of application \
                 `{application}`"
            )));
        }
    }
    Ok(())
}

/// Refuse a page whose layout runs an action this server will not run on a
/// page: every action is one of v1's [`PAGE_ACTIONS`] or a trigger the
/// application declares.
pub fn check_page_actions(page: &Page, app: &Application) -> Result<()> {
    let mut wrapped = Attrs::new();
    wrapped.insert("layout".to_owned(), page.layout.clone());
    for name in configured_actions(&wrapped) {
        if PAGE_ACTIONS.contains(&name.as_str()) || app.triggers.iter().any(|t| t.0 == name) {
            continue;
        }
        let declared: Vec<&str> = app.triggers.iter().map(|t| t.0.as_str()).collect();
        return Err(Error::invalid(format!(
            "page `{}` runs the action `{name}`, which is neither one of v1's page actions ({}) \
             nor a trigger of application `{}` ({}); add a trigger of that name to the \
             application, or remove the action from the page",
            page.name,
            PAGE_ACTIONS.join(", "),
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

/// Refuse a page whose layout embeds or links to a view `views` does not have,
/// naming the view.
pub fn check_page_views(page: &Page, application: &str, views: &[View]) -> Result<()> {
    match referenced_views(&page.layout)
        .into_iter()
        .find(|name| !views.iter().any(|v| &v.name == name))
    {
        None => Ok(()),
        Some(missing) => Err(Error::invalid(format!(
            "page `{}` shows the view `{missing}`, which application `{application}` does not \
             have",
            page.name
        ))),
    }
}

/// The checks a page is saved through from the admin API, answering its
/// application: a name that is a URL path segment, an application that exists,
/// a role that exists, and a layout whose actions ([`check_page_actions`]) and
/// views ([`check_page_views`]) are the application's.
///
/// [`save_page`](crate::save_page) itself makes only the first three, because a
/// restore saves a page before it can know whether the views it names came too,
/// and reports the ones that did not rather than refusing the page.
pub async fn validate_page(catalog: &Catalog, page: &Page) -> Result<Application> {
    check_name("page", &page.name)?;
    let app = require_application(catalog, page.application, "page", &page.name).await?;
    check_role(catalog, "page", &page.name, page.min_role).await?;
    check_page_actions(page, &app)?;
    check_page_views(
        page,
        &app.name,
        &list_views(catalog, page.application).await?,
    )?;
    Ok(app)
}

/// Save a view whose layout the builder changed, and the library edits the
/// builder made on the way, in one transaction.
///
/// Refused before anything is written: whatever [`save_view`](crate::save_view)
/// refuses, and a `library` segment — in the view or in an edited item — naming
/// an item this application does not have. An update naming such an item is
/// refused by [`apply_library_updates_in`]. Anything refused after the edits
/// were written rolls them back.
pub async fn save_view_with_library_updates(
    catalog: &Catalog,
    view: &View,
    updates: &[LibraryUpdate],
) -> Result<View> {
    let app = validate_view(catalog, view).await?;
    let library = list_library(catalog, view.application).await?;
    check_library_placements(
        &format!("view `{}`", view.name),
        view.configuration.values(),
        &app.name,
        &library,
    )?;
    check_update_placements(updates, &app.name, &library)?;
    let tx = SharedTx::begin_primary(catalog)?;
    let saved = async {
        if !updates.is_empty() {
            apply_library_updates_in(&tx, catalog, view.application, updates).await?;
        }
        save_view_on(catalog, Some(&tx), view).await
    }
    .await;
    tx.finish(saved).await
}

/// Save a page whose layout the builder changed, and the library edits it
/// carries, in one transaction: [`validate_page`]'s checks, then as
/// [`save_view_with_library_updates`].
pub async fn save_page_with_library_updates(
    catalog: &Catalog,
    page: &Page,
    updates: &[LibraryUpdate],
) -> Result<Page> {
    let app = validate_page(catalog, page).await?;
    let library = list_library(catalog, page.application).await?;
    check_library_placements(
        &format!("page `{}`", page.name),
        std::iter::once(&page.layout),
        &app.name,
        &library,
    )?;
    check_update_placements(updates, &app.name, &library)?;
    let tx = SharedTx::begin_primary(catalog)?;
    let saved = async {
        if !updates.is_empty() {
            apply_library_updates_in(&tx, catalog, page.application, updates).await?;
        }
        save_page_on(catalog, Some(&tx), page).await
    }
    .await;
    tx.finish(saved).await
}

/// Refuse an edited item's layout that places an item the application does not
/// have. An item placing itself is not refused: it renders blank, as in v1.
pub fn check_update_placements(
    updates: &[LibraryUpdate],
    application: &str,
    library: &[LibraryItem],
) -> Result<()> {
    for update in updates {
        let name = library
            .iter()
            .find(|i| i.id == update.library_id)
            .map_or_else(|| update.library_id.0.to_string(), |i| i.name.clone());
        check_library_placements(
            &format!("the library item `{name}`"),
            std::iter::once(&update.layout),
            application,
            library,
        )?;
    }
    Ok(())
}

/// Every page a view's configuration or a page's layout names, sorted and
/// without repeats: a `page` segment's `page` (v1's `Page.js`, embedding one
/// page in another) and a `link` segment whose `link_src` is `Page`, by the last
/// segment of its URL, as v1's `extractFromLayout` reads it.
pub fn referenced_pages(value: &Json) -> Vec<String> {
    fn walk(out: &mut BTreeSet<String>, value: &Json) {
        match value {
            Json::Object(item) => {
                let text = |key: &str| item.get(key).and_then(Json::as_str);
                match text("type") {
                    Some("page") => {
                        if let Some(name) = text("page").filter(|n| !n.is_empty()) {
                            out.insert(name.to_owned());
                        }
                    }
                    Some("link") if text("link_src") == Some("Page") => {
                        if let Some(name) = text("url")
                            .and_then(|url| url.rsplit('/').next())
                            .filter(|n| !n.is_empty())
                        {
                            out.insert(percent_decoded(name));
                        }
                    }
                    _ => {}
                }
                item.values().for_each(|child| walk(out, child));
            }
            Json::Array(items) => items.iter().for_each(|i| walk(out, i)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(&mut out, value);
    out.into_iter().collect()
}

/// A URL segment with its `%XX` escapes decoded, or as written when they do not
/// decode to UTF-8.
fn percent_decoded(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| segment.to_owned())
}

/// What names one page, so a rename or a delete can say beforehand what it
/// will leave pointing at a name that no longer exists (TODO "The builder" §10).
/// Nothing is rewritten.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageReferences {
    /// The labels of the menu entries that open it, nested entries included.
    pub menu: Vec<String>,
    /// The roles whose home page it is, by name: the application's
    /// `root_pages` setting and the page's own `root_page_for_roles`.
    pub home_page_for: Vec<String>,
    /// The views whose configuration embeds or links to it.
    pub views: Vec<String>,
    /// The other pages whose layout embeds or links to it.
    pub pages: Vec<String>,
    /// The library items whose layout embeds or links to it.
    pub library: Vec<String>,
}

/// The labels of the entries of v1's `menu_items` that open the page `name`,
/// walking `Header` entries' `subitems`. An entry with no label is its page's
/// name.
pub(crate) fn menu_entries_for(menu: Option<&Json>, name: &str) -> Vec<String> {
    fn walk(out: &mut Vec<String>, items: &Json, name: &str) {
        for item in items.as_array().into_iter().flatten() {
            let text = |key: &str| item.get(key).and_then(Json::as_str);
            if text("type") == Some("Page") && text("pagename") == Some(name) {
                out.push(
                    text("label")
                        .filter(|l| !l.is_empty())
                        .unwrap_or(name)
                        .to_owned(),
                );
            }
            if let Some(sub) = item.get("subitems") {
                walk(out, sub, name);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(menu) = menu {
        walk(&mut out, menu, name);
    }
    out
}

/// The application's menu, as its settings hold it.
pub(crate) fn application_menu(app: &Application) -> Option<&Json> {
    app.framework.config.get(MENU_CONFIG_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{LibraryItemId, LibraryUpdate};
    use sc_app::{FrameworkRef, TriggerRef};
    use serde_json::json;

    fn attrs(value: Json) -> Attrs {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn a_layout_lands_at_the_top_level_or_under_the_steps_context_field() {
        let mut configuration = attrs(json!({ "columns": [1], "layout": {}, "keep": true }));
        merge_view_layout(
            "Show Books",
            &mut configuration,
            None,
            json!([{ "type": "Field" }]),
            json!({ "above": [] }),
        )
        .unwrap();
        assert_eq!(
            Json::Object(configuration),
            json!({ "columns": [{ "type": "Field" }], "layout": { "above": [] }, "keep": true })
        );

        // v1 spreads what the context field held and overwrites the two keys.
        let mut configuration = attrs(json!({ "inner": { "columns": [], "other": 1 } }));
        merge_view_layout(
            "Show Books",
            &mut configuration,
            Some("inner"),
            json!([]),
            json!({ "type": "blank" }),
        )
        .unwrap();
        assert_eq!(
            Json::Object(configuration),
            json!({ "inner": { "columns": [], "other": 1, "layout": { "type": "blank" } } })
        );
        let mut fresh = Attrs::new();
        merge_view_layout("V", &mut fresh, Some("inner"), json!([]), json!({})).unwrap();
        assert_eq!(fresh["inner"], json!({ "columns": [], "layout": {} }));

        let mut wrong = attrs(json!({ "inner": [1] }));
        let err = merge_view_layout("V", &mut wrong, Some("inner"), json!([]), json!({}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("view `V`") && err.contains("`inner`"), "{err}");
    }

    #[test]
    fn a_library_segment_must_name_an_item_of_the_application() {
        let app = sc_app::AppId(uuid::Uuid::new_v4());
        let item = LibraryItem::new(app, "Book header");
        let layout = json!({ "above": [
            { "type": "library", "library_id": item.id.0.to_string(), "slots": [] },
        ]});
        check_library_placements(
            "page `Home`",
            [&layout],
            "Books",
            std::slice::from_ref(&item),
        )
        .unwrap();

        let stray = LibraryItemId::new().0.to_string();
        for (bad, named) in [
            (
                json!({ "type": "library", "library_id": stray }),
                stray.clone(),
            ),
            (
                json!({ "type": "library", "library_id": 3 }),
                "3".to_owned(),
            ),
        ] {
            let err = check_library_placements(
                "page `Home`",
                [&bad],
                "Books",
                std::slice::from_ref(&item),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("page `Home`")
                    && err.contains(&format!("`{named}`"))
                    && err.contains("application `Books`"),
                "{err}"
            );
        }

        // An edited item placing an unknown one is refused naming the item.
        let update = LibraryUpdate {
            library_id: item.id,
            layout: json!({ "type": "library", "library_id": stray }),
        };
        let err = check_update_placements(&[update], "Books", &[item])
            .unwrap_err()
            .to_string();
        assert!(err.contains("library item `Book header`"), "{err}");
    }

    #[test]
    fn a_page_runs_the_page_actions_and_the_applications_triggers_and_shows_its_views() {
        let app = Application::new("Books", "books", FrameworkRef::new("saltcorn-ui"))
            .with_trigger(TriggerRef::new("TrimPages"));
        let mut page = Page::new(app.id, "Home").layout(json!({ "besides": [
            { "type": "action", "action_name": "GoBack", "rndid": "a" },
            { "contents": { "type": "action", "action_name": "TrimPages", "rndid": "b" } },
            { "type": "view", "view": "List Books", "state": "shared" },
            { "type": "view_link", "view": "Own:Show Books" },
        ]}));
        check_page_actions(&page, &app).unwrap();
        let views = [
            View::new(app.id, "List Books", "List", "books"),
            View::new(app.id, "Show Books", "Show", "books"),
        ];
        check_page_views(&page, &app.name, &views).unwrap();

        // A view action is not a page action.
        page.layout["besides"][0]["action_name"] = json!("Delete");
        let err = check_page_actions(&page, &app).unwrap_err().to_string();
        assert!(
            err.contains("page `Home`") && err.contains("`Delete`") && err.contains("GoBack"),
            "{err}"
        );

        let err = check_page_views(&page, &app.name, &views[..1])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`Show Books`"), "{err}");
    }

    #[test]
    fn the_pages_a_layout_names_are_its_page_segments_and_page_links() {
        let layout = json!({ "above": [
            { "type": "page", "page": "Welcome" },
            { "type": "link", "link_src": "Page", "url": "/page/Book%20list", "text": "Books" },
            { "type": "link", "link_src": "URL", "url": "/page/Elsewhere" },
            { "type": "container", "contents": { "type": "page", "page": "Welcome" } },
        ]});
        assert_eq!(referenced_pages(&layout), ["Book list", "Welcome"]);
    }

    #[test]
    fn a_menu_entry_is_found_under_a_header() {
        let menu = json!([
            { "type": "Page", "label": "Home", "pagename": "Home" },
            { "type": "Header", "label": "More", "subitems": [
                { "type": "Page", "label": "", "pagename": "Home" },
                { "type": "View", "label": "Home", "viewname": "Home" },
            ]},
        ]);
        assert_eq!(menu_entries_for(Some(&menu), "Home"), ["Home", "Home"]);
        assert!(menu_entries_for(None, "Home").is_empty());
    }
}
