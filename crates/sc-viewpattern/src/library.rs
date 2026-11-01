//! The library: named, reusable layout fragments, stored per application
//! (TODO "The builder" §8).
//!
//! A library item is v1's `_sc_library` row, `{ name, icon, layout }` — what
//! current v1 calls a **shared component**. Placing one in a view's or a page's
//! layout writes `{ type: "library", library_id, slots }`, and v1's own
//! `Library.resolveSegment` swaps the reference for the item's layout at render
//! time. The item's layout is v1-shaped and stored untouched, like a view's
//! configuration.
//!
//! Two departures from v1, both for the reasons views have them:
//!
//! - **An item belongs to one application**, because its layout names fields,
//!   join paths, views and actions that mean something only inside one
//!   application's table subset and view set. `_fd_library.application` is a
//!   column and a name is unique per application.
//! - **Its id is a UUID**, not v1's serial, so `library_id` in a layout is the
//!   item's UUID as a string.
//!
//! **Only a Saltcorn UI application has a library.** Every write here refuses
//! any other application, naming its framework. Reads answer an empty library
//! for one, because there is nothing wrong with asking.
//!
//! What is here: [`LibraryItem`] and its id; the row path
//! ([`save_library_item`], [`load_library_item`], [`list_library`],
//! [`delete_library_item`]); [`apply_library_updates`], v1's in-place edits to
//! placed items, in one transaction; and the layout walk references are built
//! from ([`placed_library_ids`], with the set-level answers on
//! [`ViewSet`](crate::ViewSet)).

use std::collections::BTreeSet;

use sc_app::{AppId, Application, load_application};
use sc_catalog::{Catalog, SharedTx};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, Statement, Update, Value};
use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::bundle::SALTCORN_UI_FRAMEWORK;
use crate::store::{
    bad_column, delete_where, load_one, object, optional_text, rows, text, uuid, write_row,
};
use crate::tables::{
    COL_APPLICATION, COL_ATTRIBUTES, COL_DESCRIPTION, COL_ICON, COL_ID, COL_LAYOUT, COL_NAME,
    LIBRARY_TABLE,
};

/// Identifies a library item: the UUID primary key of its `_fd_library` row,
/// and what a `library` segment's `library_id` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LibraryItemId(pub Uuid);

impl LibraryItemId {
    /// Mint an id for a new item.
    pub fn new() -> LibraryItemId {
        LibraryItemId(Uuid::new_v4())
    }
}

impl Default for LibraryItemId {
    fn default() -> Self {
        Self::new()
    }
}

/// One library item: a named layout fragment in one application.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryItem {
    /// The row's identity.
    pub id: LibraryItemId,
    /// The application the item belongs to.
    pub application: AppId,
    /// The name, unique within the application.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// v1's icon class (`fas fa-heading`); empty when none was chosen.
    pub icon: String,
    /// The layout, exactly as v1's builder wrote it.
    pub layout: Json,
    /// Sparse per-item settings (§9).
    pub attributes: Attrs,
}

impl LibraryItem {
    /// An item named `name` in `application`, with no icon and an empty layout.
    pub fn new(application: AppId, name: impl Into<String>) -> LibraryItem {
        LibraryItem {
            id: LibraryItemId::new(),
            application,
            name: name.into(),
            description: String::new(),
            icon: String::new(),
            layout: Json::Object(Attrs::new()),
            attributes: Attrs::new(),
        }
    }

    /// Set the icon.
    pub fn icon(mut self, icon: impl Into<String>) -> LibraryItem {
        self.icon = icon.into();
        self
    }

    /// Set the layout.
    pub fn layout(mut self, layout: Json) -> LibraryItem {
        self.layout = layout;
        self
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> LibraryItem {
        self.description = description.into();
        self
    }
}

/// One of v1's in-place edits to a placed item (`save-updates`): the item's new
/// layout.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryUpdate {
    /// The item edited.
    pub library_id: LibraryItemId,
    /// Its whole new layout.
    pub layout: Json,
}

/// What places a library item: the views, pages and other items whose layouts
/// place it, directly or through an item they place. Names, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryReferences {
    /// The views that place it.
    #[serde(default)]
    pub views: Vec<String>,
    /// The pages that place it.
    #[serde(default)]
    pub pages: Vec<String>,
    /// The other library items that place it.
    #[serde(default)]
    pub library: Vec<String>,
}

impl LibraryReferences {
    /// Whether nothing places the item.
    pub fn is_empty(&self) -> bool {
        self.views.is_empty() && self.pages.is_empty() && self.library.is_empty()
    }
}

/// Every `library_id` a layout's `library` segments name, as written, sorted and
/// without repeats — slots and nested containers included, and not resolved
/// through the items they name.
///
/// Raw strings rather than [`LibraryItemId`]s, because a layout may name an id
/// that is not a UUID (an untranslated v1 serial), and a save that refuses it has
/// to be able to say what it named. A number is rendered as its digits.
pub fn placed_library_ids(value: &Json) -> Vec<String> {
    let mut out = BTreeSet::new();
    collect_library_ids(value, &mut out);
    out.into_iter().collect()
}

pub(crate) fn collect_library_ids(value: &Json, out: &mut BTreeSet<String>) {
    match value {
        Json::Object(item) => {
            if item.get("type").and_then(Json::as_str) == Some("library") {
                match item.get("library_id") {
                    Some(Json::String(id)) if !id.is_empty() => {
                        out.insert(id.clone());
                    }
                    Some(Json::Number(n)) => {
                        out.insert(n.to_string());
                    }
                    _ => {}
                }
            }
            item.values()
                .for_each(|child| collect_library_ids(child, out));
        }
        Json::Array(items) => items.iter().for_each(|i| collect_library_ids(i, out)),
        _ => {}
    }
}

/// Save a library item: insert its row, or update it in place if a row with
/// its [`LibraryItemId`] already exists.
///
/// Refused, each naming what is wrong: an empty or whitespace-padded name; an
/// application that does not exist or is not a Saltcorn UI application; a name
/// another item of the application already has; and an id that belongs to an
/// item of a different application.
pub async fn save_library_item(catalog: &Catalog, item: &LibraryItem) -> Result<LibraryItem> {
    if item.name.trim().is_empty() {
        return Err(Error::invalid("a library item needs a name"));
    }
    if item.name.trim() != item.name {
        return Err(Error::invalid(format!(
            "library item name `{}` starts or ends with whitespace",
            item.name
        )));
    }
    let app = require_library_application(catalog, item.application).await?;

    let existing = load_one(catalog, LIBRARY_TABLE, by_id(item.id))
        .await?
        .map(|row| item_from_row(&row))
        .transpose()?;
    if let Some(existing) = &existing
        && existing.application != item.application
    {
        return Err(Error::invalid(format!(
            "library item id {} belongs to another application; a library item cannot move \
             between applications",
            item.id.0
        )));
    }
    let same_name =
        of_application(item.application).and(Expr::col(COL_NAME).eq(Expr::lit(item.name.as_str())));
    if let Some(other) = load_one(catalog, LIBRARY_TABLE, same_name).await?
        && uuid(&other, LIBRARY_TABLE, COL_ID)? != item.id.0
    {
        return Err(Error::invalid(format!(
            "application `{}` already has a library item named `{}`",
            app.name, item.name
        )));
    }

    let columns = [
        COL_ID,
        COL_APPLICATION,
        COL_NAME,
        COL_DESCRIPTION,
        COL_ICON,
        COL_LAYOUT,
        COL_ATTRIBUTES,
    ];
    let values = vec![
        Value::Uuid(item.id.0),
        Value::Uuid(item.application.0),
        Value::Text(item.name.clone()),
        Value::Text(item.description.clone()),
        Value::Text(item.icon.clone()),
        Value::Json(item.layout.clone()),
        Value::Json(Json::Object(item.attributes.clone())),
    ];
    write_row(
        catalog,
        None,
        LIBRARY_TABLE,
        &columns,
        values,
        existing.is_some(),
    )
    .await?;
    Ok(item.clone())
}

/// The item `id` of `application`, if it has one. An item of another
/// application is not found.
pub async fn load_library_item(
    catalog: &Catalog,
    application: AppId,
    id: LibraryItemId,
) -> Result<Option<LibraryItem>> {
    load_one(catalog, LIBRARY_TABLE, scoped(application, id))
        .await?
        .map(|row| item_from_row(&row))
        .transpose()
}

/// Every library item of `application`, ordered by name.
pub async fn list_library(catalog: &Catalog, application: AppId) -> Result<Vec<LibraryItem>> {
    let mut items: Vec<LibraryItem> = rows(catalog, LIBRARY_TABLE, of_application(application))
        .await?
        .iter()
        .map(item_from_row)
        .collect::<Result<_>>()?;
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

/// Delete the item `id` of `application`, returning whether one was there to
/// delete. Refused for an application that is not a Saltcorn UI application.
///
/// Nothing that places the item is changed: a `library` segment naming a
/// deleted item renders blank, which is v1's `resolveSegment` behaviour. Asking
/// before deleting an item with references is the caller's business.
pub async fn delete_library_item(
    catalog: &Catalog,
    application: AppId,
    id: LibraryItemId,
) -> Result<bool> {
    require_library_application(catalog, application).await?;
    delete_where(catalog, LIBRARY_TABLE, scoped(application, id)).await
}

/// Apply v1's in-place edits to placed items, all or none, in a transaction of
/// its own.
///
/// Refused before anything is written: an application that is not a Saltcorn UI
/// application, and an update naming an id that is not an item of this
/// application (named in the refusal).
pub async fn apply_library_updates(
    catalog: &Catalog,
    application: AppId,
    updates: &[LibraryUpdate],
) -> Result<()> {
    let tx = SharedTx::begin_primary(catalog)?;
    let outcome = apply_library_updates_in(&tx, catalog, application, updates).await;
    tx.finish(outcome).await
}

/// [`apply_library_updates`] inside a transaction the caller owns, so a view's
/// or a page's save and the library edits it carries commit together (§6).
/// Neither commits nor rolls back.
pub async fn apply_library_updates_in(
    tx: &SharedTx,
    catalog: &Catalog,
    application: AppId,
    updates: &[LibraryUpdate],
) -> Result<()> {
    let app = require_library_application(catalog, application).await?;
    let items = list_library(catalog, application).await?;
    if let Some(unknown) = updates
        .iter()
        .find(|u| !items.iter().any(|i| i.id == u.library_id))
    {
        return Err(Error::invalid(format!(
            "library item {} is not an item of application `{}`",
            unknown.library_id.0, app.name
        )));
    }
    for update in updates {
        let statement = Update::new(
            LIBRARY_TABLE,
            vec![Assignment::new(
                COL_LAYOUT.to_owned(),
                Expr::Lit(Value::Json(update.layout.clone())),
            )],
        )
        .filter(scoped(application, update.library_id));
        tx.run(
            sc_catalog::Unchecked::system("apply library updates"),
            &Statement::from(statement),
        )
        .await?;
    }
    Ok(())
}

/// The application a library write is for, refused unless it exists and is a
/// Saltcorn UI application.
async fn require_library_application(catalog: &Catalog, id: AppId) -> Result<Application> {
    let app = load_application(catalog, id).await?.ok_or_else(|| {
        Error::invalid(format!(
            "a library item belongs to application {}, which does not exist",
            id.0
        ))
    })?;
    if app.framework.name != SALTCORN_UI_FRAMEWORK {
        return Err(Error::invalid(format!(
            "application `{}` uses the framework `{}`; only a `{SALTCORN_UI_FRAMEWORK}` \
             application has a library",
            app.name, app.framework.name
        )));
    }
    Ok(app)
}

fn by_id(id: LibraryItemId) -> Expr {
    Expr::col(COL_ID).eq(Expr::lit(id.0))
}

fn of_application(application: AppId) -> Expr {
    Expr::col(COL_APPLICATION).eq(Expr::lit(application.0))
}

fn scoped(application: AppId, id: LibraryItemId) -> Expr {
    of_application(application).and(by_id(id))
}

fn item_from_row(row: &Row) -> Result<LibraryItem> {
    let t = LIBRARY_TABLE;
    Ok(LibraryItem {
        id: LibraryItemId(uuid(row, t, COL_ID)?),
        application: AppId(uuid(row, t, COL_APPLICATION)?),
        name: text(row, t, COL_NAME)?,
        description: optional_text(row, t, COL_DESCRIPTION)?.unwrap_or_default(),
        icon: optional_text(row, t, COL_ICON)?.unwrap_or_default(),
        layout: match row.get(COL_LAYOUT) {
            Some(Value::Json(j)) => j.clone(),
            other => return Err(bad_column(t, COL_LAYOUT, "json", other)),
        },
        attributes: object(row, t, COL_ATTRIBUTES)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn library_ids_are_found_in_slots_and_containers_and_kept_as_written() {
        let layout = json!({ "above": [
            { "type": "library", "library_id": "b", "slots": [
                { "type": "library", "library_id": "a" },
            ]},
            { "type": "container", "contents": { "type": "library", "library_id": 7 } },
            { "type": "library", "library_id": "b" },
            // Not a library segment: a key of that name elsewhere is not a placement.
            { "type": "blank", "library_id": "c" },
            { "type": "library", "library_id": "" },
        ]});
        assert_eq!(placed_library_ids(&layout), ["7", "a", "b"]);
    }
}
