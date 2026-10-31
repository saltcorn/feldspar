//! [`ViewSnapshot`]: an application's views, pages and library as the worker
//! sees them (TODO "Saltcorn UI" §4, TODO "The builder" §8).
//!
//! v1's `View.findOne` is synchronous, and so are `getState().getConfig(…)` and
//! `getState().roles`, so none of them can be a host call. They are answered from
//! a snapshot instead — the rule the previous milestone made for `Table` —
//! serialised **once** per [`ViewSet`] generation and sent to a worker only when
//! the worker does not already hold that generation.
//!
//! The registries a `getState()` exposes (`types`, `viewtemplates`, …) are not in
//! it: they are the bundle's own objects and never change while a worker lives.

use std::sync::Arc;

use sc_app::{AppId, Application};
use sc_auth::RoleDef;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::library::LibraryItem;
use crate::view::{Page, View};
use crate::view_set::ViewSet;

/// The framework setting the application's menu is kept in — v1's own config
/// key, so a restored backup's menu crosses unrenamed.
pub const MENU_CONFIG_KEY: &str = "menu_items";

/// One application's views, pages, library and settings, serialised at one
/// generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSnapshot {
    application: AppId,
    generation: u64,
    json: Arc<str>,
}

impl ViewSnapshot {
    /// The snapshot of `application`'s `set`, with the server's `roles` and the
    /// application's own origin.
    ///
    /// The set must be the application's: a snapshot mixing one application's
    /// row with another's views would render views under the wrong name, menu
    /// and settings, so it is refused rather than built.
    pub fn build(
        application: &Application,
        set: &ViewSet,
        roles: &[RoleDef],
        base_url: &str,
    ) -> Result<ViewSnapshot> {
        ViewSnapshot::build_with_trigger_actions(application, set, roles, base_url, &|_| None)
    }

    /// [`build`](ViewSnapshot::build), with what each of the application's
    /// triggers runs: `action_of(name)` is the trigger's action (`run_js_code`,
    /// `Workflow` for a workflow body), or `None` for a trigger this server does
    /// not have.
    ///
    /// v1's `run_action_column` reads a trigger's `action` to decide how to run
    /// it (§12.2), so a trigger whose action is unknown to the snapshot is found
    /// by `Trigger.findOne` but cannot be run from a view.
    pub fn build_with_trigger_actions(
        application: &Application,
        set: &ViewSet,
        roles: &[RoleDef],
        base_url: &str,
        action_of: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ViewSnapshot> {
        if set.application != application.id {
            return Err(Error::msg(format!(
                "the view set of application {} cannot be the snapshot of `{}` ({})",
                set.application.0, application.name, application.id.0
            )));
        }
        let config = &application.framework.config;
        let value = json!({
            "application": {
                "id": application.id.0.to_string(),
                "name": application.name,
                "description": application.description,
                "subdomain": application.subdomain,
                "base_url": base_url,
                // The tables a view may name (§11): checked on save, and again
                // on every run in the worker, because the subset can shrink
                // under a view already saved (7.4).
                "tables": application.tables.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
                // The stores the builder's image picker lists (TODO "The
                // builder" 5.4). Names only: what is in them is listed when the
                // builder asks, through the call's file surface.
                "file_stores": application.file_stores.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            },
            "generation": set.generation,
            "config": config,
            "menu": config.get(MENU_CONFIG_KEY).filter(|m| m.is_array()).cloned().unwrap_or_else(|| json!([])),
            // v1's `getState().roles` shape: the number is the id.
            "roles": roles.iter().map(|r| json!({ "id": r.role, "role": r.name })).collect::<Vec<_>>(),
            // The triggers the application declares (§12.2): what v1's
            // synchronous `Trigger.findOne` finds, and nothing else.
            "triggers": application.triggers.iter().map(|t| json!({ "name": t.0, "action": action_of(&t.0) })).collect::<Vec<_>>(),
            "views": set.views.iter().map(view_json).collect::<Vec<_>>(),
            "pages": set.pages.iter().map(page_json).collect::<Vec<_>>(),
            // What v1's `Library.find` reads (§8): the application's items only.
            "library": set.library.iter().map(library_json).collect::<Vec<_>>(),
        });
        let json = serde_json::to_string(&value)
            .map_err(|e| Error::serde(format!("serialising the view snapshot: {e}")))?;
        Ok(ViewSnapshot::from_json(
            application.id,
            set.generation,
            json,
        ))
    }

    /// A snapshot of already-serialised JSON — what a test that wants to say
    /// exactly what the worker is handed uses.
    pub fn from_json(
        application: AppId,
        generation: u64,
        json: impl Into<Arc<str>>,
    ) -> ViewSnapshot {
        ViewSnapshot {
            application,
            generation,
            json: json.into(),
        }
    }

    /// The application it is of.
    pub fn application(&self) -> AppId {
        self.application
    }

    /// The [`ViewSet`] generation it was built at — what a call carries, and
    /// what a worker compares against what it already holds.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The serialised snapshot, as the worker parses it.
    pub fn json(&self) -> &str {
        &self.json
    }
}

/// A view as v1's `View` is shaped. `table_id` is the table's **name**, because
/// that is what the v1 `Table` over the schema snapshot is keyed by.
fn view_json(view: &View) -> Json {
    json!({
        "id": view.id.0.to_string(),
        "name": view.name,
        "description": view.description,
        "viewtemplate": view.viewpattern,
        "table_name": view.table_name,
        "table_id": view.table_name,
        "configuration": view.configuration,
        "min_role": view.min_role,
        "slug": view.slug,
        "attributes": view.attributes,
    })
}

/// A page as v1's `Page` is shaped.
fn page_json(page: &Page) -> Json {
    json!({
        "id": page.id.0.to_string(),
        "name": page.name,
        "title": page.title,
        "description": page.description,
        "layout": page.layout,
        "min_role": page.min_role,
        "attributes": page.attributes,
    })
}

/// A library item as v1's `_sc_library` row is shaped. `id` is the UUID that a
/// `library` segment's `library_id` holds.
fn library_json(item: &LibraryItem) -> Json {
    json!({
        "id": item.id.0.to_string(),
        "name": item.name,
        "icon": item.icon,
        "layout": item.layout,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sc_app::FrameworkRef;
    use sc_types::Role;

    fn app() -> Application {
        let mut framework = FrameworkRef::new(crate::SALTCORN_UI_FRAMEWORK);
        framework.config.insert(
            MENU_CONFIG_KEY.into(),
            json!([{ "label": "Books", "type": "Page" }]),
        );
        framework
            .config
            .insert("site_name".into(), json!("BooksDB"));
        Application::new("BooksDB", "books", framework)
            .with_table(sc_catalog::TableId("books".into()))
    }

    fn role(role: Role, name: &str) -> RoleDef {
        RoleDef {
            id: uuid::Uuid::new_v4(),
            role,
            name: name.into(),
            description: String::new(),
            attributes: Default::default(),
        }
    }

    #[test]
    fn the_snapshot_carries_what_a_synchronous_v1_lookup_needs() {
        let mut app = app();
        app.triggers.push(sc_app::TriggerRef::new("notify_author"));
        let set = ViewSet {
            application: app.id,
            generation: 7,
            views: vec![View::new(app.id, "List Books", "List", "books").min_role(Role::ADMIN)],
            pages: vec![Page::new(app.id, "BooksOverview").title("Books")],
            library: vec![
                LibraryItem::new(app.id, "Book header")
                    .icon("fas fa-heading")
                    .layout(json!({ "type": "blank", "contents": "Header" })),
            ],
        };
        let snapshot = ViewSnapshot::build(
            &app,
            &set,
            &[role(Role::ADMIN, "admin"), role(Role::PUBLIC, "public")],
            "https://books.example",
        )
        .unwrap();
        assert_eq!(snapshot.generation(), 7);
        assert_eq!(snapshot.application(), app.id);

        let value: Json = serde_json::from_str(snapshot.json()).unwrap();
        assert_eq!(value["application"]["name"], json!("BooksDB"));
        assert_eq!(
            value["application"]["base_url"],
            json!("https://books.example")
        );
        assert_eq!(value["application"]["tables"], json!(["books"]));
        assert_eq!(value["config"]["site_name"], json!("BooksDB"));
        assert_eq!(value["menu"][0]["label"], json!("Books"));
        assert_eq!(
            value["roles"],
            json!([{ "id": 1, "role": "admin" }, { "id": 100, "role": "public" }])
        );
        // A trigger this server does not have has no action to run.
        assert_eq!(
            value["triggers"],
            json!([{ "name": "notify_author", "action": null }])
        );
        let view = &value["views"][0];
        assert_eq!(view["name"], json!("List Books"));
        // v1's own field names, because v1's own code reads them.
        assert_eq!(view["viewtemplate"], json!("List"));
        assert_eq!(view["table_id"], json!("books"));
        assert_eq!(view["min_role"], json!(1));
        assert_eq!(value["pages"][0]["title"], json!("Books"));
        let item = &value["library"][0];
        assert_eq!(item["id"], json!(set.library[0].id.0.to_string()));
        assert_eq!(item["name"], json!("Book header"));
        assert_eq!(item["icon"], json!("fas fa-heading"));
        assert_eq!(item["layout"]["contents"], json!("Header"));
    }

    #[test]
    fn the_snapshot_says_what_each_trigger_runs() {
        let mut app = app();
        app.triggers.push(sc_app::TriggerRef::new("TrimPages"));
        app.triggers.push(sc_app::TriggerRef::new("Gone"));
        let set = ViewSet {
            application: app.id,
            generation: 1,
            views: Vec::new(),
            pages: Vec::new(),
            library: Vec::new(),
        };
        let snapshot = ViewSnapshot::build_with_trigger_actions(&app, &set, &[], "", &|name| {
            (name == "TrimPages").then(|| "run_js_code".to_owned())
        })
        .unwrap();
        let value: Json = serde_json::from_str(snapshot.json()).unwrap();
        assert_eq!(
            value["triggers"],
            json!([
                { "name": "TrimPages", "action": "run_js_code" },
                { "name": "Gone", "action": null },
            ])
        );
    }

    #[test]
    fn a_snapshot_of_another_applications_views_is_refused() {
        let app = app();
        let set = ViewSet {
            application: AppId::new(),
            generation: 1,
            views: Vec::new(),
            pages: Vec::new(),
            library: Vec::new(),
        };
        let msg = ViewSnapshot::build(&app, &set, &[], "")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("BooksDB"), "{msg}");
    }
}
