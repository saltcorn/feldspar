//! Saltcorn UI views and pages, and the builder.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // Every write goes through `view_sets()`, the cache the mounted application
    // renders from, so a save or a delete is live on the app's next request with
    // no build and no remount (TODO "Saltcorn UI" §1, §4).

    reg.handle(sc_api::handler_ids::LIST_VIEWS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let views = sc_viewpattern::list_views(&catalog, app.id).await?;
                Ok(HandlerResponse::ok(Json::Array(
                    views.iter().map(view_json).collect(),
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_VIEW, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                let view = sc_viewpattern::load_view(&catalog, app.id, name)
                    .await?
                    .ok_or_else(|| no_such("view", &app, name))?;
                Ok(HandlerResponse::ok(view_json(&view)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SAVE_VIEW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                // The stored view's id when the path names one — so a body with
                // another name renames it rather than adding a second view.
                let id = sc_viewpattern::load_view(&catalog, app.id, name)
                    .await?
                    .map_or_else(sc_viewpattern::ViewId::new, |v| v.id);
                let view = view_from_body(id, app.id, &ctx.body)?;
                // What the store refuses is refused first, in the store's
                // words; then the configuration is replayed through the
                // pattern's steps (10.1), so a value a step's form would not
                // accept is refused naming the step and the field. A server
                // with no view runtime can replay no step — and render no view —
                // and saves what the store accepts.
                sc_viewpattern::validate_view(&catalog, &view).await?;
                if sc_viewpattern::view_runtime().is_ok() {
                    view_configurer(&catalog, &app, &ctx.who, &apps)
                        .await?
                        .check(&view)
                        .await?;
                }
                let saved = sc_viewpattern::view_sets()
                    .save_view(&catalog, &view)
                    .await?;
                Ok(HandlerResponse::ok(view_json(&saved)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_VIEW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let mut view = view_from_body(sc_viewpattern::ViewId::new(), app.id, &ctx.body)?;
                if sc_viewpattern::load_view(&catalog, app.id, &view.name)
                    .await?
                    .is_some()
                {
                    return Err(Error::invalid(format!(
                        "application `{}` already has a view named `{}`; choose another name, \
                         or configure that view",
                        app.name, view.name
                    )));
                }
                // Refused by the store's sentence before the pattern is asked
                // anything about a table it may not have.
                sc_viewpattern::validate_view(&catalog, &view).await?;
                view.configuration = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .initial_config(
                        &view.viewpattern,
                        view.table_name.as_deref(),
                        Some(&view.name),
                    )
                    .await?;
                let saved = sc_viewpattern::view_sets()
                    .save_view(&catalog, &view)
                    .await?;
                Ok(HandlerResponse::ok(view_json(&saved)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::VIEW_CONFIG_STEP, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let pattern = obj.non_empty_str_field("viewpattern")?;
                let table = obj
                    .get("table_name")
                    .and_then(Json::as_str)
                    .filter(|t| !t.is_empty());
                let name = obj
                    .get("name")
                    .and_then(Json::as_str)
                    .filter(|n| !n.is_empty());
                let index = step_member(obj)?;
                let context = match obj.get("context") {
                    None | Some(Json::Null) => json!({}),
                    Some(context @ Json::Object(_)) => context.clone(),
                    Some(_) => return Err(Error::invalid("`context` must be an object")),
                };
                // The subset bounds what a step may be asked about, as it bounds
                // what a view may be saved over (§11).
                if let Some(table) = table
                    && !app.tables.iter().any(|t| t == table)
                {
                    return Err(Error::invalid(format!(
                        "the table `{table}` is not in application `{}`'s table subset",
                        app.name
                    )));
                }
                let step = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .step(pattern, table, name, index, &context)
                    .await?;
                Ok(HandlerResponse::ok(config_step_json(
                    index,
                    &step,
                    &ctx.locale,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::VIEW_REFERENCES, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                if sc_viewpattern::load_view(&catalog, app.id, name)
                    .await?
                    .is_none()
                {
                    return Err(no_such("view", &app, name));
                }
                let references = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .references(name)
                    .await?;
                // The library, both ways (TODO "The builder" §8): the items
                // whose layouts show the view, and the items the view places.
                let set = sc_viewpattern::view_sets().get(&catalog, app.id).await?;
                let library: Vec<&str> = set
                    .library
                    .iter()
                    .filter(|i| {
                        sc_viewpattern::referenced_views(&i.layout)
                            .iter()
                            .any(|v| v == name)
                    })
                    .map(|i| i.name.as_str())
                    .collect();
                let places = set
                    .view(name)
                    .map(|view| item_names(set.library_placed_by_view(view)))
                    .unwrap_or_default();
                Ok(HandlerResponse::ok(json!({
                    "embedded_in": references.embedded_in,
                    "linked_from": references.linked_from,
                    "pages": references.pages,
                    "library": library,
                    "places": places,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_VIEW, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                if !sc_viewpattern::view_sets()
                    .delete_view(&catalog, app.id, name)
                    .await?
                {
                    return Err(no_such("view", &app, name));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_PAGES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let pages = sc_viewpattern::list_pages(&catalog, app.id).await?;
                Ok(HandlerResponse::ok(Json::Array(
                    pages.iter().map(page_json).collect(),
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_PAGE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                let page = sc_viewpattern::load_page(&catalog, app.id, name)
                    .await?
                    .ok_or_else(|| no_such("page", &app, name))?;
                Ok(HandlerResponse::ok(page_json(&page)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SAVE_PAGE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                let id = sc_viewpattern::load_page(&catalog, app.id, name)
                    .await?
                    .map_or_else(sc_viewpattern::PageId::new, |p| p.id);
                let page = page_from_body(id, app.id, &ctx.body)?;
                // A page's layout names actions and views, and they are the
                // application's (TODO "The builder" §6); the store checks the
                // rest.
                sc_viewpattern::validate_page(&catalog, &page).await?;
                let saved = sc_viewpattern::view_sets()
                    .save_page(&catalog, &page)
                    .await?;
                Ok(HandlerResponse::ok(page_json(&saved)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_PAGE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                if !sc_viewpattern::view_sets()
                    .delete_page(&catalog, app.id, name)
                    .await?
                {
                    return Err(no_such("page", &app, name));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    // A layout from a view's builder step. The step says where the layout lands
    // and that it is a layout; then every check `saveView` makes runs over the
    // merged configuration, and the view and its library edits are written in
    // one transaction.
    reg.handle(sc_api::handler_ids::SAVE_VIEW_LAYOUT, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                let mut view = sc_viewpattern::load_view(&catalog, app.id, name)
                    .await?
                    .ok_or_else(|| no_such("view", &app, name))?;
                let obj = require_object(&ctx.body)?;
                let index = step_member(obj)?;
                let columns = required_member(obj, "columns", "the columns the builder wrote")?;
                let layout = required_member(obj, "layout", "the layout the builder wrote")?;
                let updates = library_updates_member(obj)?;
                let configurer = view_configurer(&catalog, &app, &ctx.who, &apps).await?;
                let step = configurer
                    .step(
                        &view.viewpattern,
                        view.table_name.as_deref(),
                        Some(&view.name),
                        index,
                        &Json::Object(view.configuration.clone()),
                    )
                    .await?;
                if !step.builder {
                    return Err(Error::invalid(format!(
                        "step {index} of view `{}`, {}, is a form rather than a layout; its \
                         settings are saved with saveView",
                        view.name, step.name
                    )));
                }
                sc_viewpattern::merge_view_layout(
                    &view.name,
                    &mut view.configuration,
                    step.context_field.as_deref(),
                    columns,
                    layout,
                )?;
                sc_viewpattern::validate_view(&catalog, &view).await?;
                configurer.check(&view).await?;
                let saved = sc_viewpattern::view_sets()
                    .save_view_layout(&catalog, &view, &updates)
                    .await?;
                Ok(HandlerResponse::ok(view_json(&saved)))
            }
        }
    });

    // A page's layout, replacing the page's: checked as a page is, and written
    // with its library edits in one transaction.
    reg.handle(sc_api::handler_ids::SAVE_PAGE_LAYOUT, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                let mut page = sc_viewpattern::load_page(&catalog, app.id, name)
                    .await?
                    .ok_or_else(|| no_such("page", &app, name))?;
                let obj = require_object(&ctx.body)?;
                page.layout = required_member(obj, "layout", "the layout the builder wrote")?;
                let updates = library_updates_member(obj)?;
                let saved = sc_viewpattern::view_sets()
                    .save_page_layout(&catalog, &page, &updates)
                    .await?;
                Ok(HandlerResponse::ok(page_json(&saved)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::PAGE_REFERENCES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let name = ctx.path_param("name")?;
                let set = sc_viewpattern::view_sets().get(&catalog, app.id).await?;
                let page = set.page(name).ok_or_else(|| no_such("page", &app, name))?;
                let roles: Vec<(u8, String)> = sc_auth::list_roles(&catalog)
                    .await?
                    .into_iter()
                    .map(|r| (r.role.get(), r.name))
                    .collect();
                let references = set.page_references(&app, name, &roles);
                Ok(HandlerResponse::ok(json!({
                    "menu": references.menu,
                    "home_page_for": references.home_page_for,
                    "views": references.views,
                    "pages": references.pages,
                    "library": references.library,
                    "places": item_names(set.library_placed_by_page(page)),
                })))
            }
        }
    });

    // The library (§8). Reads answer from the view set, which every write here
    // reloads; `getLibraryItem` reads the row, because a placed instance must
    // start from the latest layout.

    reg.handle(sc_api::handler_ids::LIST_LIBRARY, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let set = sc_viewpattern::view_sets().get(&catalog, app.id).await?;
                let items = set
                    .library
                    .iter()
                    .map(|item| {
                        let mut listed = library_item_json(item);
                        listed["used_by"] =
                            library_references_json(&set.library_references(item.id));
                        listed
                    })
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(items)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_LIBRARY_ITEM, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let id = parse_library_id(ctx.path_param("item")?)?;
                let item = sc_viewpattern::load_library_item(&catalog, app.id, id)
                    .await?
                    .ok_or_else(|| no_library_item(&app, id))?;
                Ok(HandlerResponse::ok(library_item_json(&item)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_LIBRARY_ITEM, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let item = library_item_from_body(
                    sc_viewpattern::LibraryItemId::new(),
                    app.id,
                    &ctx.body,
                )?;
                let library = sc_viewpattern::list_library(&catalog, app.id).await?;
                sc_viewpattern::check_library_placements(
                    &format!("the library item `{}`", item.name),
                    std::iter::once(&item.layout),
                    &app.name,
                    &library,
                )?;
                let saved = sc_viewpattern::view_sets()
                    .save_library_item(&catalog, &item)
                    .await?;
                Ok(HandlerResponse::ok(library_item_json(&saved)).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SAVE_LIBRARY_ITEM, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let id = parse_library_id(ctx.path_param("item")?)?;
                let mut item = sc_viewpattern::load_library_item(&catalog, app.id, id)
                    .await?
                    .ok_or_else(|| no_library_item(&app, id))?;
                let obj = require_object(&ctx.body)?;
                obj.non_empty_str_field("name")?.clone_into(&mut item.name);
                if let Some(icon) = obj.get("icon").and_then(Json::as_str) {
                    icon.clone_into(&mut item.icon);
                }
                if let Some(description) = obj.get("description").and_then(Json::as_str) {
                    description.clone_into(&mut item.description);
                }
                let saved = sc_viewpattern::view_sets()
                    .save_library_item(&catalog, &item)
                    .await?;
                Ok(HandlerResponse::ok(library_item_json(&saved)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SAVE_LIBRARY_UPDATES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let updates = library_updates_member(require_object(&ctx.body)?)?;
                let library = sc_viewpattern::list_library(&catalog, app.id).await?;
                sc_viewpattern::check_update_placements(&updates, &app.name, &library)?;
                sc_viewpattern::view_sets()
                    .apply_library_updates(&catalog, app.id, &updates)
                    .await?;
                Ok(HandlerResponse::ok(json!({ "updated": updates.len() })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_LIBRARY_ITEM, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let id = parse_library_id(ctx.path_param("item")?)?;
                let item = sc_viewpattern::load_library_item(&catalog, app.id, id)
                    .await?
                    .ok_or_else(|| no_library_item(&app, id))?;
                let references = sc_viewpattern::view_sets()
                    .get(&catalog, app.id)
                    .await?
                    .library_references(id);
                if !references.is_empty() && ctx.query_get("confirm") != Some("true") {
                    return Ok(HandlerResponse::ok(json!({
                        "error": format!(
                            "the library item `{}` is placed by {}; delete it with confirm=true \
                             to leave those places blank",
                            item.name,
                            describe_library_references(&references)
                        ),
                        "references": library_references_json(&references),
                    }))
                    .with_status(409));
                }
                sc_viewpattern::view_sets()
                    .delete_library_item(&catalog, app.id, id)
                    .await?;
                Ok(HandlerResponse::ok(json!({
                    "deleted": true,
                    "references": library_references_json(&references),
                })))
            }
        }
    });

    // The canvas's calls: v1's server routes, run in the worker as the admin by
    // the configuration calls' `Configurer`. What they may name — a table in the
    // subset, a view or page of the application — is checked here first, so a
    // refusal names it rather than failing inside v1's code.

    reg.handle(sc_api::handler_ids::BUILDER_FIELD_PREVIEW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let table = obj.non_empty_str_field("table")?;
                let field = obj.non_empty_str_field("field")?;
                let fieldview = obj.non_empty_str_field("fieldview")?;
                require_subset_table(&app, table)?;
                // v1's body, with only what was sent: `{ ...undefined }` and
                // `{ ...null }` differ nowhere, but a fieldview may read either.
                let body: Map<String, Json> = ["configuration", "row_id"]
                    .into_iter()
                    .filter_map(|key| {
                        obj.get(key)
                            .filter(|v| !v.is_null())
                            .map(|v| (key.to_owned(), v.clone()))
                    })
                    .collect();
                let html = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .field_preview(table, field, fieldview, &Json::Object(body))
                    .await?;
                Ok(HandlerResponse::ok(json!({ "html": html })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::BUILDER_FIELDVIEW_CONFIG_FORM, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let table = obj.non_empty_str_field("table")?;
                require_subset_table(&app, table)?;
                let form = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .fieldview_config(table, &ctx.body)
                    .await?;
                Ok(HandlerResponse::ok(form))
            }
        }
    });

    reg.handle(sc_api::handler_ids::BUILDER_VIEW_PREVIEW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("view")?;
                if sc_viewpattern::load_view(&catalog, app.id, name)
                    .await?
                    .is_none()
                {
                    return Err(no_such("view", &app, name));
                }
                let state = match obj.get("state") {
                    None | Some(Json::Null) => json!({}),
                    Some(state @ Json::Object(_)) => state.clone(),
                    Some(_) => return Err(Error::invalid("`state` must be an object")),
                };
                let html = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .view_preview(name, &state)
                    .await?;
                Ok(HandlerResponse::ok(json!({ "html": html })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::BUILDER_PAGE_PREVIEW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("page")?;
                if sc_viewpattern::load_page(&catalog, app.id, name)
                    .await?
                    .is_none()
                {
                    return Err(no_such("page", &app, name));
                }
                let html = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .page_preview(name)
                    .await?;
                Ok(HandlerResponse::ok(json!({ "html": html })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::BUILDER_DISTINCT_VALUES, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_views_app(&catalog, ctx.path_param("id")?).await?;
                let table = ctx.path_param("table")?;
                let field = ctx.path_param("field")?;
                require_subset_table(&app, table)?;
                let values = view_configurer(&catalog, &app, &ctx.who, &apps)
                    .await?
                    .distinct_values(table, field)
                    .await?;
                Ok(HandlerResponse::ok(values))
            }
        }
    });

    // The registry is what a save is checked against, so it is the list; the
    // runtime's manifest only describes its entries. A server with no view
    // runtime running (built without the bundle) still lists the names, since
    // they are still what a view may be saved with.
    reg.handle(sc_api::handler_ids::LIST_VIEW_PATTERNS, |_ctx| async move {
        let manifests = match sc_viewpattern::view_runtime() {
            Ok(runtime) => runtime.patterns().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let out: Vec<Json> = sc_viewpattern::registered_patterns()
            .into_iter()
            .map(|pattern| {
                let manifest = manifests.iter().find(|m| m.name == pattern.name);
                json!({
                    "name": pattern.name,
                    "label": manifest
                        .map(|m| m.label.as_str())
                        .filter(|l| !l.is_empty())
                        .unwrap_or(&pattern.name),
                    "description": manifest.map(|m| m.description.as_str()).unwrap_or(""),
                    "table_required": !pattern.tableless,
                    "view_quantity": manifest.and_then(|m| m.view_quantity.clone()),
                    "routes": manifest.map(|m| m.routes.clone()).unwrap_or_default(),
                    "steps": manifest.map(|m| m.steps.clone()).unwrap_or_default(),
                    "module": pattern.module,
                })
            })
            .collect();
        Ok(HandlerResponse::ok(Json::Array(out)))
    });
}

/// The view runtime's configuration calls for `app`, made as the signed-in
/// admin `user` (TODO "Saltcorn UI" Phase 10). The sentence saying there is no
/// view runtime when this server has none.
async fn view_configurer<'a>(
    catalog: &'a Catalog,
    app: &Application,
    admin: &crate::admin_session::Admin,
    apps: &AppMounts,
) -> Result<sc_viewpattern::Configurer<'a>> {
    sc_viewpattern::Configurer::new(
        sc_viewpattern::view_runtime()?,
        catalog,
        app,
        &admin.principal(),
        apps.triggers().map(|d| d.as_ref()),
    )
    .await
}

/// One configuration step on the wire (`view_config_step_schema`).
fn config_step_json(index: usize, step: &sc_viewpattern::ConfigStep, locale: &Locale) -> Json {
    json!({
        "index": index,
        "name": step.name,
        "count": step.count,
        "builder": step.builder,
        "builder_options": step.builder_options,
        "skip": step.skip,
        "context_field": step.context_field,
        "blurb": step.blurb,
        "fields": spec_json(&step.fields, locale),
        "values": Json::Object(step.values.clone()),
        "issues": step.issues,
    })
}

/// [`require_app`], refusing an application whose framework has no views: a
/// view saved into a React application would be a row nothing ever serves.
async fn require_views_app(catalog: &Catalog, raw_id: &str) -> Result<Application> {
    let app = require_app(catalog, raw_id).await?;
    if app.framework.name != sc_viewpattern::SALTCORN_UI_FRAMEWORK {
        return Err(Error::invalid(format!(
            "application `{}` uses the `{}` framework; only a `{}` application has views and \
             pages",
            app.name,
            app.framework.name,
            sc_viewpattern::SALTCORN_UI_FRAMEWORK
        )));
    }
    Ok(app)
}

/// The `step` of a configuration call's body: a step number, counting from 0.
fn step_member(obj: &Map<String, Json>) -> Result<usize> {
    obj.get("step")
        .and_then(Json::as_u64)
        .and_then(|s| usize::try_from(s).ok())
        .ok_or_else(|| Error::invalid("`step` must be a step number, counting from 0"))
}

/// A member a body must carry, `null` counting as absent; `what` says what it
/// is in the refusal.
fn required_member(obj: &Map<String, Json>, key: &str, what: &str) -> Result<Json> {
    obj.present(key)
        .cloned()
        .ok_or_else(|| Error::invalid(format!("`{key}` is required: {what}")))
}

/// v1's `libraryUpdates`: `[{ library_id, layout }]`, absent meaning none. Each
/// refusal names the update by its place in the list.
fn library_updates_member(obj: &Map<String, Json>) -> Result<Vec<sc_viewpattern::LibraryUpdate>> {
    let updates = match obj.get("libraryUpdates") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(updates)) => updates,
        Some(_) => {
            return Err(Error::invalid(
                "`libraryUpdates` must be a list of `{ library_id, layout }`",
            ));
        }
    };
    updates
        .iter()
        .enumerate()
        .map(|(n, update)| {
            let raw = match update.get("library_id") {
                Some(Json::String(id)) => id.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let library_id = uuid::Uuid::parse_str(&raw)
                .map(sc_viewpattern::LibraryItemId)
                .map_err(|_| {
                    Error::invalid(format!(
                        "library update {} names `{raw}`, which is not a library item id",
                        n + 1
                    ))
                })?;
            let layout = update
                .get("layout")
                .filter(|l| !l.is_null())
                .cloned()
                .ok_or_else(|| {
                    Error::invalid(format!("library update {} has no `layout`", n + 1))
                })?;
            Ok(sc_viewpattern::LibraryUpdate { library_id, layout })
        })
        .collect()
}

/// A library item id from a path.
fn parse_library_id(raw: &str) -> Result<sc_viewpattern::LibraryItemId> {
    uuid::Uuid::parse_str(raw)
        .map(sc_viewpattern::LibraryItemId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a library item id")))
}

/// The 404 for a library item `id` that `app` does not have.
fn no_library_item(app: &Application, id: sc_viewpattern::LibraryItemId) -> Error {
    Error::not_found(format!(
        "application `{}` has no library item {}",
        app.name, id.0
    ))
}

/// Refuse a table outside `app`'s subset, as a configuration step does (§11).
fn require_subset_table(app: &Application, table: &str) -> Result<()> {
    if app.tables.iter().any(|t| t == table) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "the table `{table}` is not in application `{}`'s table subset",
        app.name
    )))
}

/// Library items' names, in the order given.
fn item_names(items: Vec<&sc_viewpattern::LibraryItem>) -> Vec<String> {
    items.into_iter().map(|i| i.name.clone()).collect()
}

/// What places a library item, on the wire.
fn library_references_json(references: &sc_viewpattern::LibraryReferences) -> Json {
    json!({
        "views": references.views,
        "pages": references.pages,
        "library": references.library,
    })
}

/// What places a library item, as a clause: "the views `A` and the page `B`".
fn describe_library_references(references: &sc_viewpattern::LibraryReferences) -> String {
    let group = |one: &str, many: &str, names: &[String]| match names {
        [] => None,
        [name] => Some(format!("the {one} `{name}`")),
        names => Some(format!(
            "the {many} {}",
            names
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    };
    [
        group("view", "views", &references.views),
        group("page", "pages", &references.pages),
        group("library item", "library items", &references.library),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" and ")
}

/// The 404 for a view or page `name` that `app` does not have.
fn no_such(kind: &str, app: &Application, name: &str) -> Error {
    Error::not_found(format!(
        "application `{}` has no {kind} named `{name}`",
        app.name
    ))
}
