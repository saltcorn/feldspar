//! Persisting views and pages: `_fd_views` / `_fd_pages` row ⇄ [`View`] /
//! [`Page`] (TODO "Saltcorn UI" §1).
//!
//! Every read and write is **scoped by application**: a view is looked up by
//! (`application`, `name`), never by name alone, because two applications may
//! each hold a view of that name.
//!
//! **Reading is strict**, as every `_fd_*` read is: a missing or ill-typed column
//! is an error naming the table and the column, never a silently defaulted field.

use sc_app::{AppId, Application, load_application};
use sc_catalog::{Catalog, SharedTx};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::Attrs;
use sc_types::Role;
use serde_json::Value as Json;

use crate::patterns::registered_patterns;
use crate::tables::{
    COL_APPLICATION, COL_ATTRIBUTES, COL_CONFIGURATION, COL_DESCRIPTION, COL_ID, COL_LAYOUT,
    COL_MIN_ROLE, COL_NAME, COL_SLUG, COL_TABLE_NAME, COL_TITLE, COL_VIEWPATTERN, LIBRARY_TABLE,
    PAGES_TABLE, VIEWS_TABLE,
};
use crate::validate::{check_name, check_view_actions, check_view_shape};
use crate::view::{Page, PageId, View, ViewId};

/// The checks [`save_view`] makes before it writes, without the write, answering
/// the view's application.
///
/// What the admin API runs before it asks a pattern for a first configuration
/// or replays one through the pattern's steps (TODO "Saltcorn UI" 10.1, 10.2),
/// so a view the store would refuse is refused with the store's sentence rather
/// than with whatever the pattern made of a table it was never meant to see.
pub async fn validate_view(catalog: &Catalog, view: &View) -> Result<Application> {
    check_name("view", &view.name)?;
    let app = require_application(catalog, view.application, "view", &view.name).await?;
    check_view_shape(view, &app, &registered_patterns())?;
    check_view_actions(view, &app)?;
    check_role(catalog, "view", &view.name, view.min_role).await?;
    Ok(app)
}

/// Save a view: insert its row, or update it in place if a row with its
/// [`ViewId`] already exists.
///
/// Refused, each with a sentence naming what is wrong: a name that is not a URL
/// path segment; an application that does not exist; a pattern that is not
/// registered; a table missing or outside the application's subset; a
/// `min_role` no role defines; a name another view of the application already
/// has; and an id that belongs to a view of a different application.
pub async fn save_view(catalog: &Catalog, view: &View) -> Result<View> {
    save_view_on(catalog, None, view).await
}

/// [`save_view`], writing inside `tx` when one is given, so a layout save and
/// the library edits it carries commit together (TODO "The builder" §6). The
/// checks read outside it.
pub(crate) async fn save_view_on(
    catalog: &Catalog,
    tx: Option<&SharedTx>,
    view: &View,
) -> Result<View> {
    let app = validate_view(catalog, view).await?;

    let existing = load_one(
        catalog,
        VIEWS_TABLE,
        Expr::col(COL_ID).eq(Expr::lit(view.id.0)),
    )
    .await?
    .map(|row| view_from_row(&row))
    .transpose()?;
    if let Some(existing) = &existing
        && existing.application != view.application
    {
        return Err(Error::invalid(format!(
            "view id {} belongs to another application; a view cannot move between applications",
            view.id.0
        )));
    }
    if let Some(other) = load_view(catalog, view.application, &view.name).await?
        && other.id != view.id
    {
        return Err(Error::invalid(format!(
            "application `{}` already has a view named `{}`",
            app.name, view.name
        )));
    }

    let columns = [
        COL_ID,
        COL_APPLICATION,
        COL_NAME,
        COL_DESCRIPTION,
        COL_VIEWPATTERN,
        COL_TABLE_NAME,
        COL_CONFIGURATION,
        COL_MIN_ROLE,
        COL_SLUG,
        COL_ATTRIBUTES,
    ];
    let values = vec![
        Value::Uuid(view.id.0),
        Value::Uuid(view.application.0),
        Value::Text(view.name.clone()),
        Value::Text(view.description.clone()),
        Value::Text(view.viewpattern.clone()),
        view.table_name
            .as_ref()
            .map_or(Value::Null, |t| Value::Text(t.clone())),
        Value::Json(Json::Object(view.configuration.clone())),
        Value::Int(i64::from(view.min_role)),
        view.slug
            .as_ref()
            .filter(|s| !s.is_null())
            .map_or(Value::Null, |s| Value::Json(s.clone())),
        Value::Json(Json::Object(view.attributes.clone())),
    ];
    write_row(
        catalog,
        tx,
        VIEWS_TABLE,
        &columns,
        values,
        existing.is_some(),
    )
    .await?;
    Ok(view.clone())
}

/// The view named `name` in `application`, if any.
pub async fn load_view(catalog: &Catalog, application: AppId, name: &str) -> Result<Option<View>> {
    load_one(catalog, VIEWS_TABLE, scoped(application, name))
        .await?
        .map(|row| view_from_row(&row))
        .transpose()
}

/// Every view of `application`, ordered by name.
pub async fn list_views(catalog: &Catalog, application: AppId) -> Result<Vec<View>> {
    let mut views: Vec<View> = rows(catalog, VIEWS_TABLE, of_application(application))
        .await?
        .iter()
        .map(view_from_row)
        .collect::<Result<_>>()?;
    views.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(views)
}

/// Delete the view named `name` in `application`, returning whether one was
/// there to delete.
pub async fn delete_view(catalog: &Catalog, application: AppId, name: &str) -> Result<bool> {
    delete_scoped(catalog, VIEWS_TABLE, application, name).await
}

/// Save a page: insert its row, or update it in place if a row with its
/// [`PageId`] already exists. Refused for the same reasons a view is, less the
/// two that are about a pattern and a table.
pub async fn save_page(catalog: &Catalog, page: &Page) -> Result<Page> {
    save_page_on(catalog, None, page).await
}

/// [`save_page`], writing inside `tx` when one is given (see [`save_view_on`]).
pub(crate) async fn save_page_on(
    catalog: &Catalog,
    tx: Option<&SharedTx>,
    page: &Page,
) -> Result<Page> {
    check_name("page", &page.name)?;
    let app = require_application(catalog, page.application, "page", &page.name).await?;
    check_role(catalog, "page", &page.name, page.min_role).await?;

    let existing = load_one(
        catalog,
        PAGES_TABLE,
        Expr::col(COL_ID).eq(Expr::lit(page.id.0)),
    )
    .await?
    .map(|row| page_from_row(&row))
    .transpose()?;
    if let Some(existing) = &existing
        && existing.application != page.application
    {
        return Err(Error::invalid(format!(
            "page id {} belongs to another application; a page cannot move between applications",
            page.id.0
        )));
    }
    if let Some(other) = load_page(catalog, page.application, &page.name).await?
        && other.id != page.id
    {
        return Err(Error::invalid(format!(
            "application `{}` already has a page named `{}`",
            app.name, page.name
        )));
    }

    let columns = [
        COL_ID,
        COL_APPLICATION,
        COL_NAME,
        COL_TITLE,
        COL_DESCRIPTION,
        COL_LAYOUT,
        COL_MIN_ROLE,
        COL_ATTRIBUTES,
    ];
    let values = vec![
        Value::Uuid(page.id.0),
        Value::Uuid(page.application.0),
        Value::Text(page.name.clone()),
        Value::Text(page.title.clone()),
        Value::Text(page.description.clone()),
        Value::Json(page.layout.clone()),
        Value::Int(i64::from(page.min_role)),
        Value::Json(Json::Object(page.attributes.clone())),
    ];
    write_row(
        catalog,
        tx,
        PAGES_TABLE,
        &columns,
        values,
        existing.is_some(),
    )
    .await?;
    Ok(page.clone())
}

/// The page named `name` in `application`, if any.
pub async fn load_page(catalog: &Catalog, application: AppId, name: &str) -> Result<Option<Page>> {
    load_one(catalog, PAGES_TABLE, scoped(application, name))
        .await?
        .map(|row| page_from_row(&row))
        .transpose()
}

/// Every page of `application`, ordered by name.
pub async fn list_pages(catalog: &Catalog, application: AppId) -> Result<Vec<Page>> {
    let mut pages: Vec<Page> = rows(catalog, PAGES_TABLE, of_application(application))
        .await?
        .iter()
        .map(page_from_row)
        .collect::<Result<_>>()?;
    pages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pages)
}

/// Delete the page named `name` in `application`, returning whether one was
/// there to delete.
pub async fn delete_page(catalog: &Catalog, application: AppId, name: &str) -> Result<bool> {
    delete_scoped(catalog, PAGES_TABLE, application, name).await
}

/// Delete every view, page and library item of `application` — what deleting
/// the application does, since `application` is not a foreign key (see
/// [`tables`](crate::tables)).
///
/// A database whose tables were never bootstrapped has nothing to delete, and
/// that is not an error: an application can be deleted from a server that has
/// never served a Saltcorn UI app.
pub async fn delete_application_views_pages_and_library(
    catalog: &Catalog,
    application: AppId,
) -> Result<()> {
    for table in [VIEWS_TABLE, PAGES_TABLE, LIBRARY_TABLE] {
        if catalog.get(table)?.is_none() {
            continue;
        }
        let delete = Delete::from(table).filter(of_application(application));
        run(catalog, Statement::from(delete)).await?;
    }
    Ok(())
}

/// The application a view or page is being saved into, or the refusal naming it.
pub(crate) async fn require_application(
    catalog: &Catalog,
    id: AppId,
    kind: &str,
    name: &str,
) -> Result<Application> {
    load_application(catalog, id).await?.ok_or_else(|| {
        Error::invalid(format!(
            "{kind} `{name}` belongs to application {}, which does not exist",
            id.0
        ))
    })
}

/// Refuse a `min_role` that no role defines.
pub(crate) async fn check_role(
    catalog: &Catalog,
    kind: &str,
    name: &str,
    role: Role,
) -> Result<()> {
    if sc_auth::load_role(catalog, role).await?.is_some() {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "{kind} `{name}` requires role {role}, which no role defines"
    )))
}

fn of_application(application: AppId) -> Expr {
    Expr::col(COL_APPLICATION).eq(Expr::lit(application.0))
}

fn scoped(application: AppId, name: &str) -> Expr {
    of_application(application).and(Expr::col(COL_NAME).eq(Expr::lit(name)))
}

async fn delete_scoped(
    catalog: &Catalog,
    table: &str,
    application: AppId,
    name: &str,
) -> Result<bool> {
    delete_where(catalog, table, scoped(application, name)).await
}

/// Delete the rows of `table` matching `filter`, returning whether there were
/// any.
pub(crate) async fn delete_where(catalog: &Catalog, table: &str, filter: Expr) -> Result<bool> {
    let existed = load_one(catalog, table, filter.clone()).await?.is_some();
    let delete = Delete::from(table).filter(filter);
    run(catalog, Statement::from(delete)).await?;
    Ok(existed)
}

/// Insert a row, or update the one with the same id — inside `tx` when one is
/// given.
pub(crate) async fn write_row(
    catalog: &Catalog,
    tx: Option<&SharedTx>,
    table: &str,
    columns: &[&str],
    values: Vec<Value>,
    exists: bool,
) -> Result<()> {
    if exists {
        let mut id = None;
        let mut assignments = Vec::new();
        for (col, value) in columns.iter().zip(values) {
            if *col == COL_ID {
                id = Some(value);
            } else {
                assignments.push(Assignment::new((*col).to_owned(), Expr::Lit(value)));
            }
        }
        let id = id.ok_or_else(|| Error::msg(format!("a `{table}` write has no id")))?;
        let update =
            sc_query::Update::new(table, assignments).filter(Expr::col(COL_ID).eq(Expr::Lit(id)));
        run_on(catalog, tx, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            table,
            columns.iter().map(|c| (*c).to_owned()).collect(),
            values.into_iter().map(Expr::Lit).collect(),
        );
        run_on(catalog, tx, Statement::from(insert)).await
    }
}

/// [`run`], or the same statement inside `tx`.
async fn run_on(catalog: &Catalog, tx: Option<&SharedTx>, statement: Statement) -> Result<()> {
    match tx {
        Some(tx) => tx
            .run(
                sc_catalog::Unchecked::system("write view-pattern definitions"),
                &statement,
            )
            .await
            .map(|_| ()),
        None => run(catalog, statement).await,
    }
}

fn view_from_row(row: &Row) -> Result<View> {
    let t = VIEWS_TABLE;
    Ok(View {
        id: ViewId(uuid(row, t, COL_ID)?),
        application: AppId(uuid(row, t, COL_APPLICATION)?),
        name: text(row, t, COL_NAME)?,
        description: optional_text(row, t, COL_DESCRIPTION)?.unwrap_or_default(),
        viewpattern: text(row, t, COL_VIEWPATTERN)?,
        table_name: optional_text(row, t, COL_TABLE_NAME)?,
        configuration: object(row, t, COL_CONFIGURATION)?,
        min_role: role(row, t)?,
        slug: match row.get(COL_SLUG) {
            Some(Value::Json(Json::Null)) | Some(Value::Null) | None => None,
            Some(Value::Json(j)) => Some(j.clone()),
            other => return Err(bad_column(t, COL_SLUG, "json", other)),
        },
        attributes: object(row, t, COL_ATTRIBUTES)?,
    })
}

fn page_from_row(row: &Row) -> Result<Page> {
    let t = PAGES_TABLE;
    Ok(Page {
        id: PageId(uuid(row, t, COL_ID)?),
        application: AppId(uuid(row, t, COL_APPLICATION)?),
        name: text(row, t, COL_NAME)?,
        title: text(row, t, COL_TITLE)?,
        description: optional_text(row, t, COL_DESCRIPTION)?.unwrap_or_default(),
        layout: match row.get(COL_LAYOUT) {
            Some(Value::Json(j)) => j.clone(),
            other => return Err(bad_column(t, COL_LAYOUT, "json", other)),
        },
        min_role: role(row, t)?,
        attributes: object(row, t, COL_ATTRIBUTES)?,
    })
}

pub(crate) fn uuid(row: &Row, table: &str, column: &str) -> Result<uuid::Uuid> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad_column(table, column, "a uuid", other)),
    }
}

pub(crate) fn text(row: &Row, table: &str, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(table, column, "text", other)),
    }
}

pub(crate) fn optional_text(row: &Row, table: &str, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(table, column, "text", other)),
    }
}

pub(crate) fn object(row: &Row, table: &str, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        other => Err(bad_column(table, column, "a json object", other)),
    }
}

fn role(row: &Row, table: &str) -> Result<Role> {
    match row.get(COL_MIN_ROLE) {
        Some(Value::Int(i)) => Role::try_from(*i).ok().ok_or_else(|| {
            Error::invalid(format!(
                "{table}.{COL_MIN_ROLE} should be a role between 1 and 100, got {i}"
            ))
        }),
        other => Err(bad_column(table, COL_MIN_ROLE, "an integer role", other)),
    }
}

pub(crate) fn bad_column(table: &str, column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{table}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("a `{table}` row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// The rows of `table` matching `filter`.
pub(crate) async fn rows(catalog: &Catalog, table: &str, filter: Expr) -> Result<Vec<Row>> {
    let select = Select::from(Source::table(table)).filter(filter);
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// The single row of `table` matching `filter`, if any.
pub(crate) async fn load_one(catalog: &Catalog, table: &str, filter: Expr) -> Result<Option<Row>> {
    let select = Select::from(Source::table(table)).filter(filter).limit(1);
    Ok(catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?
        .into_iter()
        .next())
}
