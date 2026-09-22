//! Persisting applications: `_fd_applications` row ⇄ [`Application`] (design
//! §13.2).
//!
//! An application exists only as its stored row, so this module is the whole of
//! its lifecycle-on-disk: [`save_application`] writes one (inserting or
//! updating), [`load_application`] / [`load_application_by_subdomain`] /
//! [`list_applications`] read them back, and [`delete_application`] removes one.
//! Mounting what is loaded is the server's job (§13.2 "the mount registry is
//! live"), and it is deliberately *not* here: saving the configuration and
//! building/mounting it are distinct operations with distinct outcomes, and an
//! app that is saved but unbuilt is a normal state rather than an error.
//!
//! The row is the [`Application`] value serialised (§13.2): one column per field
//! every app has, with sparse values in `attributes`. The columns that are lists
//! or maps are JSON — see [`applications`](crate::applications) for why.
//!
//! **Reading is strict.** A column that is missing or of the wrong shape is an
//! [`Error::invalid`] naming the app and the column rather than a silently
//! defaulted field: an app is its row, so a half-understood row is a
//! misconfigured app the admin needs told about, not one to serve approximately.

use sc_catalog::{Attrs, Catalog, FileStoreId, TableId};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use serde_json::{Value as Json, json};

use crate::api::{
    describe_api_queries, validate_api_config, validate_api_mounts, validate_static_dirs,
};
use crate::application::{
    ApiConfig, AppId, Application, CspPolicy, FrameworkRef, StaticDir, StreamRef, TriggerRef,
};
use crate::applications::{
    APPLICATIONS_TABLE, COL_APIS, COL_ATTRIBUTES, COL_CSP, COL_DESCRIPTION, COL_EXTRA_FRAMEWORKS,
    COL_FILE_STORES, COL_FRAMEWORK, COL_ID, COL_NAME, COL_STATIC_DIRS, COL_STREAMS, COL_SUBDOMAIN,
    COL_TABLES, COL_TRIGGERS,
};
use crate::framework::{CFG_STORE, validate_framework_config};

/// Save an application: insert its row, or update it in place if a row with its
/// [`AppId`] already exists.
///
/// The subdomain is unique (§13.2 — it is the routing key), and this checks for
/// a clash first so the admin gets an [`Error::invalid`] naming the app that
/// already holds it, rather than a raw constraint violation. The database's
/// `UNIQUE` constraint remains the authority: this check and the write are not
/// one transaction, so a concurrent save is still caught — just less prettily.
///
/// Saving does **not** build or mount anything. An app can be saved and unbuilt;
/// that is what a newly created app is until its first build.
///
/// **Returns the application as it was stored**, which is not always the one that
/// arrived: a custom SQL query is described on the way in, so what comes back
/// carries the result columns the database reported. A caller that answers with
/// the value it sent instead would be telling the admin their query returns
/// nothing — which is the drift §13.1 exists to prevent, in miniature.
pub async fn save_application(catalog: &Catalog, app: &Application) -> Result<Application> {
    let subdomain = app.subdomain.trim();
    if subdomain.is_empty() {
        return Err(Error::invalid("an application needs a subdomain"));
    }
    if app.name.trim().is_empty() {
        return Err(Error::invalid("an application needs a name"));
    }

    // Check the framework settings against the spec the framework declares
    // (§13.3). This is the point of doing it on save: a missing or ill-typed
    // setting is the admin's to fix and the admin is standing in front of the
    // form, whereas the same mistake found at build time is a bundler error and
    // at serve time is a broken app.
    validate_framework_config(catalog, &app.framework).await?;
    for extra in &app.extra_frameworks {
        validate_framework_config(catalog, extra).await?;
    }
    // An API mounted at `/` claims every path, so an app with a UI would never
    // serve it. Same reasoning as the framework config above: caught on save,
    // where the admin can fix it, rather than at build or serve time.
    validate_api_mounts(app)?;
    // …and the static directories, against the same two things: the store subset
    // this app declares, and the mounts the APIs above have just claimed (§13.2).
    validate_static_dirs(app)?;
    // …and each provider's own settings, against the spec that provider
    // declares. An unknown key is refused rather than stored and ignored: the
    // form renders exactly the spec, so a key outside it is a typo or a stale
    // config, and a switch that silently does nothing is the failure this
    // milestone's aggregation setting would otherwise be.
    for api in &app.apis {
        validate_api_config(app, api)?;
    }
    // …and each custom SQL query is **prepared** against the database, which is
    // both the last validation and the typing: a statement that will not prepare
    // cannot be saved (it comes back carrying Postgres's own message), and one
    // that will is stored with the result columns the database reported, so the
    // shape the generated client promises is the shape the query returns. The
    // application written below is therefore the one that came back from this,
    // not the one that arrived.
    let described = Application {
        apis: {
            let mut apis = Vec::with_capacity(app.apis.len());
            for api in &app.apis {
                apis.push(describe_api_queries(catalog, api).await?);
            }
            apis
        },
        ..app.clone()
    };
    let app = &described;

    if let Some(other) = load_application_by_subdomain(catalog, subdomain).await?
        && other.id != app.id
    {
        return Err(Error::invalid(format!(
            "subdomain `{subdomain}` is already used by application `{}`; \
             each application is served on its own subdomain",
            other.name
        )));
    }

    let columns = app_columns();
    let values = app_values(app)?;

    if load_application(catalog, app.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(APPLICATIONS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(app.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            APPLICATIONS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(described)
}

/// Load the application with this id, if it exists.
pub async fn load_application(catalog: &Catalog, id: AppId) -> Result<Option<Application>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the application served on `subdomain`, if any — the lookup routing needs
/// (§13.2: the subdomain is the routing key).
pub async fn load_application_by_subdomain(
    catalog: &Catalog,
    subdomain: &str,
) -> Result<Option<Application>> {
    load_one(catalog, Expr::col(COL_SUBDOMAIN).eq(Expr::lit(subdomain))).await
}

/// Every stored application, ordered by subdomain — what `sc-server` mounts at
/// boot (§13.2).
pub async fn list_applications(catalog: &Catalog) -> Result<Vec<Application>> {
    let select = Select::from(Source::table(APPLICATIONS_TABLE));
    let mut apps: Vec<Application> = rows(catalog, select)
        .await?
        .iter()
        .map(application_from_row)
        .collect::<Result<_>>()?;
    apps.sort_by(|a, b| a.subdomain.cmp(&b.subdomain));
    Ok(apps)
}

/// Delete an application's row, returning whether one was there to delete.
///
/// Unmounting the app — so its subdomain stops resolving — is the server's job;
/// this only removes the definition.
pub async fn delete_application(catalog: &Catalog, id: AppId) -> Result<bool> {
    let existed = load_application(catalog, id).await?.is_some();
    let delete = Delete::from(APPLICATIONS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    // `_fd_translations.application` is by value (§16.1), so the cascade is
    // ours to perform — the same arrangement views, pages and the library have.
    crate::i18n::delete_application_translations(catalog, id).await?;
    Ok(existed)
}

/// Every application that references the file store named `name`, described as
/// ``application `Name` `` — the application-level half of the reference check
/// [`delete_file_store`](sc_catalog::delete_file_store) performs.
///
/// This lives here rather than beside that function because `sc-catalog` sits
/// *below* this crate: it can scan its own tables for `File` fields but has no
/// idea applications exist. So the check is split, and a caller deleting a store
/// in a system that has applications must collect these and pass them in as
/// `extra_referents`. The admin API is the one place that knows about both, and
/// is where they are composed.
///
/// An application references a store three ways, and all three count — a store
/// still serving an app's source is exactly the one an admin must not be able to
/// delete out from under it:
///
/// 1. its declared store subset ([`Application::file_stores`]),
/// 2. a statically-served directory ([`StaticDir::store`]),
/// 3. a framework's `store` setting — where a code framework's source lives.
///
/// The third is matched on the setting *name*, which is a known limitation: a
/// framework declares its settings as data but has no way to mark one as "this
/// is a file-store reference", so a framework using some other name for it would
/// not be seen here. TODO §1.6 fixes this properly by making the setting a
/// server-query pick-list of stores, which does carry that meaning.
pub async fn applications_using_file_store(catalog: &Catalog, name: &str) -> Result<Vec<String>> {
    let mut refs: Vec<String> = list_applications(catalog)
        .await?
        .into_iter()
        .filter(|app| {
            let in_subset = app.file_stores.iter().any(|s| s.0 == name);
            let in_static = app.static_dirs.iter().any(|d| d.store.0 == name);
            let in_framework = std::iter::once(&app.framework)
                .chain(&app.extra_frameworks)
                .any(|fw| fw.config.get(CFG_STORE).and_then(Json::as_str) == Some(name));
            in_subset || in_static || in_framework
        })
        .map(|app| format!("application `{}`", app.name))
        .collect();
    refs.sort();
    Ok(refs)
}

/// The row's columns, in the order [`app_values`] produces them.
fn app_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_SUBDOMAIN,
        COL_FRAMEWORK,
        COL_EXTRA_FRAMEWORKS,
        COL_TABLES,
        COL_FILE_STORES,
        COL_TRIGGERS,
        COL_STREAMS,
        COL_APIS,
        COL_STATIC_DIRS,
        COL_CSP,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The application serialised to its row's values, in [`app_columns`] order.
fn app_values(app: &Application) -> Result<Vec<Value>> {
    Ok(vec![
        Value::Uuid(app.id.0),
        Value::Text(app.name.trim().to_owned()),
        Value::Text(app.description.clone()),
        Value::Text(app.subdomain.trim().to_owned()),
        Value::Json(framework_to_json(&app.framework)),
        Value::Json(Json::Array(
            app.extra_frameworks.iter().map(framework_to_json).collect(),
        )),
        Value::Json(names_to_json(app.tables.iter().map(|t| &t.0))),
        Value::Json(names_to_json(app.file_stores.iter().map(|s| &s.0))),
        // Written as `[]` rather than left NULL, so a row this version saves is
        // never one the tolerant read below has to forgive.
        Value::Json(names_to_json(app.triggers.iter().map(|t| &t.0))),
        // …and the exposed streams beside them, written the same way.
        Value::Json(names_to_json(app.streams.iter().map(|s| &s.0))),
        Value::Json(Json::Array(
            app.apis
                .iter()
                .map(|a| {
                    json!({
                        "provider": a.provider,
                        "mount": a.mount,
                        "config": Json::Object(a.config.clone()),
                    })
                })
                .collect(),
        )),
        Value::Json(Json::Array(
            app.static_dirs
                .iter()
                .map(|d| json!({ "mount": d.mount, "store": d.store.0, "path": d.path }))
                .collect(),
        )),
        Value::Json(csp_to_json(&app.csp)),
        Value::Json(Json::Object(app.attributes.clone())),
    ])
}

fn framework_to_json(fw: &FrameworkRef) -> Json {
    json!({ "name": fw.name, "config": Json::Object(fw.config.clone()) })
}

fn names_to_json<'a>(names: impl Iterator<Item = &'a String>) -> Json {
    Json::Array(names.map(|n| Json::String(n.clone())).collect())
}

fn csp_to_json(csp: &CspPolicy) -> Json {
    Json::Object(
        csp.directives
            .iter()
            .map(|(name, sources)| (name.clone(), names_to_json(sources.iter())))
            .collect(),
    )
}

/// Rebuild an [`Application`] from its `_fd_applications` row.
///
/// Public within the crate so the server can hydrate an app from a row it has
/// already read; the strictness note in the module docs applies throughout.
pub(crate) fn application_from_row(row: &Row) -> Result<Application> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => AppId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let name = text(row, COL_NAME)?;
    let subdomain = text(row, COL_SUBDOMAIN)?;

    // A NULL description is "none given", not a broken row.
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_DESCRIPTION, "text", other)),
    };

    Ok(Application {
        id,
        name,
        description,
        subdomain,
        framework: framework_from_json(json_column(row, COL_FRAMEWORK)?, COL_FRAMEWORK)?,
        extra_frameworks: json_array(row, COL_EXTRA_FRAMEWORKS)?
            .iter()
            .map(|v| framework_from_json(v.clone(), COL_EXTRA_FRAMEWORKS))
            .collect::<Result<_>>()?,
        tables: names_from_json(row, COL_TABLES)?
            .into_iter()
            .map(TableId)
            .collect(),
        file_stores: names_from_json(row, COL_FILE_STORES)?
            .into_iter()
            .map(FileStoreId)
            .collect(),
        // The one column read leniently, and only for `NULL`/absent: this is the
        // column the additive bootstrap adds to rows that already exist, and
        // "written before triggers existed" is not a broken row. A value of the
        // wrong *shape* is still refused, like every other column.
        triggers: optional_names_from_json(row, COL_TRIGGERS)?
            .into_iter()
            .map(TriggerRef)
            .collect(),
        // Read leniently for `NULL`/absent for the same reason, and only that
        // reason: the column arrived after the table did.
        streams: optional_names_from_json(row, COL_STREAMS)?
            .into_iter()
            .map(StreamRef)
            .collect(),
        apis: json_array(row, COL_APIS)?
            .iter()
            .map(|v| {
                let provider = member_str(v, "provider", COL_APIS)?;
                let config = member_config(v, &provider)?;
                Ok(ApiConfig::new(provider, member_str(v, "mount", COL_APIS)?).with_config(config))
            })
            .collect::<Result<_>>()?,
        static_dirs: json_array(row, COL_STATIC_DIRS)?
            .iter()
            .map(|v| {
                Ok(StaticDir::new(
                    member_str(v, "mount", COL_STATIC_DIRS)?,
                    FileStoreId(member_str(v, "store", COL_STATIC_DIRS)?),
                    member_str(v, "path", COL_STATIC_DIRS)?,
                ))
            })
            .collect::<Result<_>>()?,
        csp: csp_from_json(json_column(row, COL_CSP)?)?,
        attributes: object(json_column(row, COL_ATTRIBUTES)?, COL_ATTRIBUTES)?,
    })
}

/// An API row's `config` object — absent or `null` meaning "no settings", the
/// same reading a framework's config gets below, and anything else refused.
fn member_config(value: &Json, provider: &str) -> Result<Attrs> {
    match value.get("config") {
        Some(Json::Object(o)) => Ok(o.clone()),
        None | Some(Json::Null) => Ok(Attrs::new()),
        Some(_) => Err(Error::invalid(format!(
            "{APPLICATIONS_TABLE}.{COL_APIS}: API provider `{provider}` has a non-object `config`"
        ))),
    }
}

fn framework_from_json(value: Json, column: &str) -> Result<FrameworkRef> {
    let name = member_str(&value, "name", column)?;
    let config = match value.get("config") {
        Some(Json::Object(o)) => o.clone(),
        // A framework with no settings may have been stored without a config.
        None | Some(Json::Null) => Attrs::new(),
        Some(_) => {
            return Err(Error::invalid(format!(
                "{APPLICATIONS_TABLE}.{column}: framework `{name}` has a non-object `config`"
            )));
        }
    };
    Ok(FrameworkRef { name, config })
}

fn csp_from_json(value: Json) -> Result<CspPolicy> {
    let object = object(value, COL_CSP)?;
    let mut directives = std::collections::BTreeMap::new();
    for (name, sources) in object {
        let Json::Array(items) = sources else {
            return Err(Error::invalid(format!(
                "{APPLICATIONS_TABLE}.{COL_CSP}: directive `{name}` should be a list of sources"
            )));
        };
        let sources = items
            .iter()
            .map(|s| {
                s.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::invalid(format!(
                        "{APPLICATIONS_TABLE}.{COL_CSP}: directive `{name}` has a non-string source"
                    ))
                })
            })
            .collect::<Result<_>>()?;
        directives.insert(name, sources);
    }
    Ok(CspPolicy { directives })
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column's value.
fn json_column(row: &Row, column: &str) -> Result<Json> {
    match row.get(column) {
        Some(Value::Json(j)) => Ok(j.clone()),
        other => Err(bad_column(column, "json", other)),
    }
}

/// A JSON column holding an array.
fn json_array(row: &Row, column: &str) -> Result<Vec<Json>> {
    match json_column(row, column)? {
        Json::Array(items) => Ok(items),
        _ => Err(Error::invalid(format!(
            "{APPLICATIONS_TABLE}.{column} should be a json array"
        ))),
    }
}

/// A JSON column holding an array of strings (the table/store subsets).
fn names_from_json(row: &Row, column: &str) -> Result<Vec<String>> {
    json_array(row, column)?
        .iter()
        .map(|v| {
            v.as_str().map(str::to_owned).ok_or_else(|| {
                Error::invalid(format!(
                    "{APPLICATIONS_TABLE}.{column} should be a json array of names"
                ))
            })
        })
        .collect()
}

/// A JSON column holding an array of strings, where `NULL` (or a column the row
/// does not carry at all) means the empty list — the read a column added by
/// [`bootstrap`](crate::bootstrap) after rows existed needs.
fn optional_names_from_json(row: &Row, column: &str) -> Result<Vec<String>> {
    match row.get(column) {
        Some(Value::Null) | None => Ok(Vec::new()),
        _ => names_from_json(row, column),
    }
}

/// A JSON value that must be an object.
fn object(value: Json, column: &str) -> Result<Attrs> {
    match value {
        Json::Object(o) => Ok(o),
        _ => Err(Error::invalid(format!(
            "{APPLICATIONS_TABLE}.{column} should be a json object"
        ))),
    }
}

/// A required string member of a JSON object inside `column`.
fn member_str(value: &Json, member: &str, column: &str) -> Result<String> {
    value
        .get(member)
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::invalid(format!(
                "{APPLICATIONS_TABLE}.{column} entry is missing the string member `{member}`"
            ))
        })
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{APPLICATIONS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// Load the single application matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Application>> {
    let select = Select::from(Source::table(APPLICATIONS_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(application_from_row(row)?)),
        None => Ok(None),
    }
}
