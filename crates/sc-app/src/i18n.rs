//! An application's own catalogue: where it lives, and the one trait that hides
//! which of the two places that is (§16.1, D4/D7).
//!
//! Type **B** strings are the admin's — the labels in the application they
//! built. Unlike ours (type A, in `crates/sc-i18n/locales` and the SPA bundles)
//! they are written after the release, so their catalogue cannot ship with the
//! binary: it belongs to the application, and it goes wherever the rest of that
//! application's definition already is.
//!
//! There are two answers to that, and the split is not a preference:
//!
//! - **A code application's definition is its git repository**, so its
//!   catalogues are files in it — `<project>/locales/{locale}.json`, through the
//!   app's own file store ([`FileCatalogStore`]). A clone carries them, the
//!   coding agent reads them with the file tools it already has, and the backup
//!   story is the repository's.
//! - **A Saltcorn UI application's definition is rows** — `_fd_views`,
//!   `_fd_pages`, `_fd_library` — so its catalogues are rows too:
//!   [`TRANSLATIONS_TABLE`], one per (application, locale), deleted with the
//!   application ([`RowCatalogStore`]).
//!
//! [`app_catalog_store`] picks between them by asking whether the app's
//! framework builds from a source tree, which is the same question
//! [`app_source_from_config`] already answers for the build. Everything above
//! this module — the admin API, the Translations screen, the LLM fill, the
//! `{mount}/i18n/{locale}.json` route — is written once against
//! [`CatalogStore`].
//!
//! ## The locales an application has
//!
//! Which locales an app serves, and which one it falls back to, are sparse
//! values in [`Application::attributes`] (§9's column-vs-attributes rule):
//! [`ATTR_LOCALES`] and [`ATTR_DEFAULT_LOCALE`]. Most applications have
//! neither, and D11's promise is that those cost nothing — [`app_locales`]
//! answers an empty vector without touching a file store or the database.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sc_catalog::{Catalog, ConstraintKind, DataField, SchemaStep, Table, TableConstraint};
use sc_db::{Row, SchemaChange};
use sc_error::{Error, Result};
use sc_files::FileStore;
use sc_i18n::{Catalog as MessageCatalog, Catalogs as MessageCatalogs, Locale, Message};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, UnOp, Update, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::application::{AppId, Application};
use crate::build::app_source_from_config;
use crate::react::project_path;

/// Name of the per-application translations table in the primary database.
pub const TRANSLATIONS_TABLE: &str = "_fd_translations";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The owning application's id. By value, like `_fd_views.application`: deleting
/// an application deletes its translations, and a foreign key would make that
/// deletion impossible rather than tidy. `NULL` is the **instance** catalogue
/// (TODO F6): the strings every application shares, read after its own.
pub const COL_APPLICATION: &str = "application";
/// The locale tag — a translations row's `name` (§9), unique per application.
pub const COL_NAME: &str = "name";
/// The description (§9). Nullable; `NULL` reads back as the empty string.
pub const COL_DESCRIPTION: &str = "description";
/// The catalogue itself: the flat JSON object of §16.1.
pub const COL_MESSAGES: &str = "messages";
/// The sparse per-row values column (§9).
pub const COL_ATTRIBUTES: &str = "attributes";

/// The application attribute holding the locale tags it serves, a JSON array of
/// strings. Absent — the usual case — means the application is not translated.
pub const ATTR_LOCALES: &str = "locales";
/// The application attribute holding the locale it falls back to. Absent means
/// the server's `default_locale`.
pub const ATTR_DEFAULT_LOCALE: &str = "default_locale";

/// The directory an application's catalogues live in, relative to its project.
pub const LOCALES_DIR: &str = "locales";

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}
fn json_type() -> TypeRef {
    TypeRef::Basic(BasicType::Json)
}
fn uuid_type() -> TypeRef {
    TypeRef::Basic(BasicType::Uuid)
}

/// The fields of `_fd_translations`, in declaration order.
pub(crate) fn translations_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_ID, uuid_type())
            .required()
            .primary_key(),
        // Nullable: `NULL` is the instance catalogue (TODO F6).
        DataField::plain(COL_APPLICATION, uuid_type()),
        DataField::plain(COL_NAME, text()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_MESSAGES, json_type()).required(),
        DataField::plain(COL_ATTRIBUTES, json_type()).required(),
    ]
}

/// The jointly-unique key (`application`, `name`): one catalogue per locale per
/// application, exactly as a view's name is unique per application.
///
/// A unique key treats `NULL`s as distinct, so it does not stop two instance
/// rows for one locale; [`InstanceCatalogStore::save`] updates in place rather
/// than inserting a second, and that is the only writer.
pub(crate) fn name_key() -> ConstraintKind {
    ConstraintKind::Unique {
        fields: vec![COL_APPLICATION.to_owned(), COL_NAME.to_owned()],
    }
}

/// Ensure `_fd_translations` exists with its (`application`, `name`) key.
///
/// Idempotent and additively reconciled, like every other bootstrap. Called by
/// [`bootstrap`](crate::bootstrap), because an application's translations are
/// part of what an application is: a deployment that can hold an app can hold
/// its catalogue.
pub async fn bootstrap_translations(catalog: &Catalog) -> Result<Table> {
    let mut table = catalog
        .bootstrap_table(TRANSLATIONS_TABLE, &translations_fields())
        .await?;
    // A table made before the instance catalogue (TODO F6) has `application`
    // `NOT NULL`. Loosening a constraint rewrites no data and cannot fail on
    // the rows already there, so it is the one re-constraining a bootstrap
    // does; `TABLES_RENAME.sql` §14 has the same change by hand.
    if table.field(COL_APPLICATION).is_some_and(|f| f.required) {
        catalog
            .apply_schema_batch(&[SchemaStep::Change(SchemaChange::SetColumnNullable {
                table: TRANSLATIONS_TABLE.to_owned(),
                column: COL_APPLICATION.to_owned(),
                nullable: true,
            })])
            .await?;
        catalog.reload().await?;
        table = catalog.require(TRANSLATIONS_TABLE)?;
    }
    let key = name_key();
    if table.constraints.iter().any(|c| c.kind == key) {
        return Ok(table);
    }
    let constraint = TableConstraint::derived_name(TRANSLATIONS_TABLE, &key, "");
    catalog
        .apply_schema_batch(&[SchemaStep::Change(SchemaChange::AddUniqueConstraint {
            table: TRANSLATIONS_TABLE.to_owned(),
            name: constraint,
            columns: vec![COL_APPLICATION.to_owned(), COL_NAME.to_owned()],
        })])
        .await?;
    catalog.reload().await?;
    catalog.require(TRANSLATIONS_TABLE)
}

// ---------------------------------------------------------------------------
// The locales an application serves
// ---------------------------------------------------------------------------

/// The locales `app` serves, parsed. An application with no `locales` attribute
/// answers an empty vector without any work at all (D11).
///
/// A tag that is not a locale is an [`Error::invalid`] naming it: the attribute
/// was written by the admin API, so a bad one is a misconfigured application to
/// be told about rather than one to serve approximately.
pub fn app_locales(app: &Application) -> Result<Vec<Locale>> {
    let Some(value) = app.attributes.get(ATTR_LOCALES) else {
        return Ok(Vec::new());
    };
    match value {
        Json::Null => Ok(Vec::new()),
        Json::Array(items) => items
            .iter()
            .map(|item| {
                let tag = item.as_str().ok_or_else(|| {
                    Error::invalid(format!(
                        "application `{}`: `{ATTR_LOCALES}` should be a list of locale tags",
                        app.name
                    ))
                })?;
                Locale::parse(tag)
            })
            .collect(),
        _ => Err(Error::invalid(format!(
            "application `{}`: `{ATTR_LOCALES}` should be a list of locale tags",
            app.name
        ))),
    }
}

/// The locale `app` falls back to, if it named one.
pub fn app_default_locale(app: &Application) -> Result<Option<Locale>> {
    match app.attributes.get(ATTR_DEFAULT_LOCALE) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(tag)) => Locale::parse(tag).map(Some),
        Some(_) => Err(Error::invalid(format!(
            "application `{}`: `{ATTR_DEFAULT_LOCALE}` should be a locale tag",
            app.name
        ))),
    }
}

/// Set the locales an application serves, and optionally its default — the
/// write half of [`app_locales`], so the attribute's shape is spelled once.
pub fn set_app_locales(app: &mut Application, locales: &[Locale], default: Option<&Locale>) {
    if locales.is_empty() {
        app.attributes.remove(ATTR_LOCALES);
    } else {
        app.attributes.insert(
            ATTR_LOCALES.to_owned(),
            Json::Array(
                locales
                    .iter()
                    .map(|l| Json::String(l.as_str().to_owned()))
                    .collect(),
            ),
        );
    }
    match default {
        Some(locale) => {
            app.attributes.insert(
                ATTR_DEFAULT_LOCALE.to_owned(),
                Json::String(locale.as_str().to_owned()),
            );
        }
        None => {
            app.attributes.remove(ATTR_DEFAULT_LOCALE);
        }
    }
}

/// Whether `app` is translated at all — the one check every caller on the
/// serving path makes first, so an untranslated application costs a map lookup
/// (D11).
pub fn app_is_translated(app: &Application) -> bool {
    matches!(app.attributes.get(ATTR_LOCALES), Some(Json::Array(a)) if !a.is_empty())
}

// ---------------------------------------------------------------------------
// Where the catalogue is served
// ---------------------------------------------------------------------------

/// The path segment an application's catalogues are served under.
pub const I18N_SEGMENT: &str = "i18n";

fn api_mount(app: &Application) -> &str {
    app.apis
        .first()
        .map(|api| api.mount.trim_end_matches('/'))
        .unwrap_or("")
}

/// Where `locale`'s catalogue is served: `{mount}/i18n/{locale}.json`.
///
/// **Beside** the endpoint set rather than in it, for the reason the stream
/// observe socket is (§13.2): an `EndpointSet` is a typed request/response
/// model and a catalogue is a file. The generated runtime and the server's
/// router both go through this function, so the two cannot disagree about a
/// path.
pub fn i18n_catalog_path(app: &Application, locale: &Locale) -> String {
    format!("{}/{I18N_SEGMENT}/{}.json", api_mount(app), locale.as_str())
}

/// The catalogue path with a literal `{locale}` where the tag goes — what the
/// generated runtime interpolates at request time, so the bundle and the router
/// compute the same path from the same function.
pub fn i18n_catalog_path_template(app: &Application) -> String {
    format!("{}/{I18N_SEGMENT}/{{locale}}.json", api_mount(app))
}

/// The locale tag `path` names on `app`, if it is a catalogue's path at all —
/// the inverse of [`i18n_catalog_path`], and the router's half of it.
///
/// It says nothing about whether the tag is a locale or whether the application
/// serves it: those are two different refusals, and the caller makes them.
pub fn i18n_locale_in_path<'a>(app: &Application, path: &'a str) -> Option<&'a str> {
    let rest = path.strip_prefix(api_mount(app))?;
    let rest = rest.strip_prefix('/')?;
    let rest = rest.strip_prefix(I18N_SEGMENT)?;
    let rest = rest.strip_prefix('/')?;
    let tag = rest.strip_suffix(".json")?;
    (!tag.is_empty() && !tag.contains('/')).then_some(tag)
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// Where one application's catalogues are kept.
///
/// Two implementations, chosen by [`app_catalog_store`]: [`FileCatalogStore`]
/// for an application with a project tree, [`RowCatalogStore`] for one without.
/// Every method takes the [`Catalog`] because the row implementation needs the
/// database and the catalog is not `Clone`; the file implementation ignores it.
#[async_trait]
pub trait CatalogStore: Send + Sync {
    /// Where these catalogues live, in a sentence, for an error or a screen.
    fn describe(&self) -> String;

    /// Every locale this application has a catalogue for, sorted by tag.
    ///
    /// This is what the store *holds*, which is not the same as
    /// [`app_locales`]: a locale can be enabled before anything is translated
    /// into it, and a catalogue can outlive the locale being turned off.
    async fn locales(&self, cat: &Catalog) -> Result<Vec<Locale>>;

    /// The catalogue for `locale`, or `None` if there is not one.
    async fn load(&self, cat: &Catalog, locale: &Locale) -> Result<Option<MessageCatalog>>;

    /// Write `messages` as the catalogue for its own locale, replacing whatever
    /// was there.
    async fn save(&self, cat: &Catalog, messages: &MessageCatalog) -> Result<()>;

    /// Remove a locale's catalogue, answering whether one was there.
    async fn delete(&self, cat: &Catalog, locale: &Locale) -> Result<bool>;

    /// Every catalogue this store holds, as one domain.
    ///
    /// Provided rather than required: both implementations list and then load,
    /// and a store that can do better can say so.
    async fn load_all(&self, cat: &Catalog) -> Result<MessageCatalogs> {
        let mut catalogs = MessageCatalogs::new();
        for locale in self.locales(cat).await? {
            if let Some(messages) = self.load(cat, &locale).await? {
                catalogs.insert(messages);
            }
        }
        Ok(catalogs)
    }
}

/// The catalogue store for `app`: files if its framework builds from a source
/// tree, rows if it does not.
///
/// The question is asked of [`app_source_from_config`] rather than of the
/// framework's name, so a framework a module declares gets files for the same
/// reason the built-in code frameworks do — it has a repository to put them in.
pub fn app_catalog_store(app: &Application) -> Result<Box<dyn CatalogStore>> {
    match app_source_from_config(&app.framework) {
        Ok(source) => Ok(Box::new(FileCatalogStore {
            store: source.store.0,
            project: source.build.source_dir,
        })),
        // No build step, so no tree: the definition is rows and so is the
        // catalogue. The error is not a failure here — it is the answer.
        Err(_) => Ok(Box::new(RowCatalogStore {
            application: Some(app.id),
        })),
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Catalogues as `<project>/locales/{locale}.json` in the application's file
/// store — the home for an application whose definition is a repository.
#[derive(Debug, Clone)]
pub struct FileCatalogStore {
    /// The file store the project tree lives in.
    store: String,
    /// The project directory within that store; empty for the store root.
    project: String,
}

impl FileCatalogStore {
    /// Catalogues under `project` in the file store named `store`.
    pub fn new(store: impl Into<String>, project: impl Into<String>) -> FileCatalogStore {
        FileCatalogStore {
            store: store.into(),
            project: project.into(),
        }
    }

    /// The store-relative path of `locale`'s catalogue.
    pub fn path(&self, locale: &Locale) -> String {
        project_path(
            &self.project,
            &format!("{LOCALES_DIR}/{}.json", locale.as_str()),
        )
    }

    /// The store-relative directory the catalogues are in.
    pub fn dir(&self) -> String {
        project_path(&self.project, LOCALES_DIR)
    }

    fn store(&self, cat: &Catalog) -> Result<Arc<dyn FileStore>> {
        cat.require_file_store(&self.store)
    }
}

#[async_trait]
impl CatalogStore for FileCatalogStore {
    fn describe(&self) -> String {
        format!("{} of file store `{}`", self.dir(), self.store)
    }

    async fn locales(&self, cat: &Catalog) -> Result<Vec<Locale>> {
        let store = self.store(cat)?;
        // A project that has never been translated has no `locales/` at all,
        // and that is not an error — it is the answer.
        let entries = match store.list(&self.dir()).await {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        let mut locales = Vec::new();
        for entry in entries {
            let name = entry.name.rsplit('/').next().unwrap_or(&entry.name);
            let Some(tag) = name.strip_suffix(".json") else {
                continue;
            };
            // A file whose name is not a locale is somebody else's; skipping it
            // is right, because this directory is in the admin's repository.
            if let Ok(locale) = Locale::parse(tag) {
                locales.push(locale);
            }
        }
        locales.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        locales.dedup_by(|a, b| a.as_str() == b.as_str());
        Ok(locales)
    }

    async fn load(&self, cat: &Catalog, locale: &Locale) -> Result<Option<MessageCatalog>> {
        let store = self.store(cat)?;
        let path = self.path(locale);
        let Ok(bytes) = store.read(&path).await else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
            Error::invalid(format!("{path} is not UTF-8, so it is not a catalogue"))
        })?;
        MessageCatalog::parse(locale.clone(), &text).map(Some)
    }

    async fn save(&self, cat: &Catalog, messages: &MessageCatalog) -> Result<()> {
        let store = self.store(cat)?;
        // The directory is the admin's repository, so create it rather than
        // refusing: the first translation of an application is the normal way
        // `locales/` comes into existence.
        store.mkdir(&self.dir()).await?;
        let mut text = serde_json::to_string_pretty(&messages.to_json())
            .map_err(|e| Error::msg(format!("serialising a catalogue: {e}")))?;
        // It is a file in a repository, and a repository's files end in a
        // newline.
        text.push('\n');
        store
            .write(
                &self.path(messages.locale()),
                Bytes::from(text.into_bytes()),
            )
            .await
    }

    async fn delete(&self, cat: &Catalog, locale: &Locale) -> Result<bool> {
        let store = self.store(cat)?;
        store.delete(&self.path(locale)).await
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Catalogues as `_fd_translations` rows — the home for an application whose
/// definition is rows (Saltcorn UI), and for the instance catalogue (TODO F6).
#[derive(Debug, Clone, Copy)]
pub struct RowCatalogStore {
    /// `None` is the instance catalogue: the rows whose `application` is `NULL`.
    application: Option<AppId>,
}

impl RowCatalogStore {
    /// The catalogues of `application`.
    pub fn new(application: AppId) -> RowCatalogStore {
        RowCatalogStore {
            application: Some(application),
        }
    }

    /// The instance catalogue: strings every application shares, read after the
    /// application's own (TODO F6, [`merged_app_catalogue`]).
    pub fn instance() -> RowCatalogStore {
        RowCatalogStore { application: None }
    }

    /// Whether this is the instance catalogue rather than one application's.
    pub fn is_instance(&self) -> bool {
        self.application.is_none()
    }

    fn of_application(&self) -> Expr {
        match self.application {
            Some(app) => Expr::col(COL_APPLICATION).eq(Expr::lit(app.0)),
            None => Expr::unary(UnOp::IsNull, Expr::col(COL_APPLICATION)),
        }
    }

    fn application_value(&self) -> Expr {
        match self.application {
            Some(app) => Expr::lit(app.0),
            None => Expr::Lit(Value::Null),
        }
    }

    fn for_locale(&self, locale: &Locale) -> Expr {
        self.of_application()
            .and(Expr::col(COL_NAME).eq(Expr::lit(locale.as_str())))
    }
}

#[async_trait]
impl CatalogStore for RowCatalogStore {
    fn describe(&self) -> String {
        match self.application {
            Some(app) => format!("`{TRANSLATIONS_TABLE}` rows of application {}", app.0),
            None => format!("the instance's shared `{TRANSLATIONS_TABLE}` rows"),
        }
    }

    async fn locales(&self, cat: &Catalog) -> Result<Vec<Locale>> {
        let select = Select::from(Source::table(TRANSLATIONS_TABLE)).filter(self.of_application());
        let mut locales: Vec<Locale> = rows(cat, select)
            .await?
            .iter()
            .map(|row| Locale::parse(&row_text(row, COL_NAME)?))
            .collect::<Result<_>>()?;
        locales.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(locales)
    }

    async fn load(&self, cat: &Catalog, locale: &Locale) -> Result<Option<MessageCatalog>> {
        let select = Select::from(Source::table(TRANSLATIONS_TABLE))
            .filter(self.for_locale(locale))
            .limit(1);
        let found = rows(cat, select).await?;
        let Some(row) = found.first() else {
            return Ok(None);
        };
        let messages = match row.get(COL_MESSAGES) {
            Some(Value::Json(j)) => j.clone(),
            other => {
                return Err(Error::invalid(format!(
                    "{TRANSLATIONS_TABLE}.{COL_MESSAGES} should be json, got {}",
                    other.map(Value::kind).unwrap_or("nothing")
                )));
            }
        };
        MessageCatalog::from_json(locale.clone(), &messages).map(Some)
    }

    async fn save(&self, cat: &Catalog, messages: &MessageCatalog) -> Result<()> {
        let locale = messages.locale();
        let select = Select::from(Source::table(TRANSLATIONS_TABLE))
            .filter(self.for_locale(locale))
            .limit(1);
        let existing = rows(cat, select).await?;
        let body = messages.to_json();
        if !existing.is_empty() {
            let update = Update::new(
                TRANSLATIONS_TABLE,
                vec![Assignment::new(
                    COL_MESSAGES.to_owned(),
                    Expr::Lit(Value::Json(body)),
                )],
            )
            .filter(self.for_locale(locale));
            run(cat, Statement::from(update)).await
        } else {
            let insert = Insert::row(
                TRANSLATIONS_TABLE,
                vec![
                    COL_ID.to_owned(),
                    COL_APPLICATION.to_owned(),
                    COL_NAME.to_owned(),
                    COL_DESCRIPTION.to_owned(),
                    COL_MESSAGES.to_owned(),
                    COL_ATTRIBUTES.to_owned(),
                ],
                vec![
                    Expr::lit(Uuid::new_v4()),
                    self.application_value(),
                    Expr::lit(locale.as_str()),
                    Expr::lit(""),
                    Expr::Lit(Value::Json(body)),
                    Expr::Lit(Value::Json(json!({}))),
                ],
            );
            run(cat, Statement::from(insert)).await
        }
    }

    async fn delete(&self, cat: &Catalog, locale: &Locale) -> Result<bool> {
        let select = Select::from(Source::table(TRANSLATIONS_TABLE))
            .filter(self.for_locale(locale))
            .limit(1);
        let existed = !rows(cat, select).await?.is_empty();
        let delete = Delete::from(TRANSLATIONS_TABLE).filter(self.for_locale(locale));
        run(cat, Statement::from(delete)).await?;
        Ok(existed)
    }
}

// ---------------------------------------------------------------------------
// The instance catalogue, under an application's own (TODO F6)
// ---------------------------------------------------------------------------

/// The instance catalogue's store: `_fd_translations` rows with a `NULL`
/// application.
pub fn instance_catalog_store() -> RowCatalogStore {
    RowCatalogStore::instance()
}

/// Whether `message` says nothing — an empty string, or plural forms that are
/// all empty. An application's catalogue may hold one (a grid cell somebody
/// cleared), and it counts as **missing**, so the instance's translation shows
/// through rather than a blank label.
pub fn message_is_blank(message: &Message) -> bool {
    message.forms().iter().all(|form| form.trim().is_empty())
}

/// Lay `shared` under `own`: every key `own` lacks, or holds blank, is taken
/// from `shared`. A key `own` translates wins, whatever `shared` says.
pub fn merge_catalogues(
    locale: &Locale,
    own: Option<MessageCatalog>,
    shared: Option<&MessageCatalog>,
) -> MessageCatalog {
    let mut merged = own.unwrap_or_else(|| MessageCatalog::new(locale.clone()));
    let Some(shared) = shared else {
        return merged;
    };
    for (key, message) in shared.messages() {
        if message_is_blank(message) {
            continue;
        }
        if merged.get(key).is_none_or(message_is_blank) {
            merged.insert(key.clone(), message.clone());
        }
    }
    merged
}

/// The instance catalogue for `locale`, or `None` — also for a database whose
/// translations table was never bootstrapped, which has no shared strings.
pub async fn load_instance_catalogue(
    cat: &Catalog,
    locale: &Locale,
) -> Result<Option<MessageCatalog>> {
    if cat.get(TRANSLATIONS_TABLE)?.is_none() {
        return Ok(None);
    }
    instance_catalog_store().load(cat, locale).await
}

/// What `app` serves for `locale`: its own catalogue over the instance's
/// (lookup is **app → instance → key**). `None` when neither has one.
///
/// This is the one place the merge happens, so the `{mount}/i18n/{locale}.json`
/// route and a server-rendered framework cannot disagree about which
/// translation wins.
pub async fn merged_app_catalogue(
    cat: &Catalog,
    app: &Application,
    locale: &Locale,
) -> Result<Option<MessageCatalog>> {
    let own = app_catalog_store(app)?.load(cat, locale).await?;
    let shared = load_instance_catalogue(cat, locale).await?;
    if own.is_none() && shared.is_none() {
        return Ok(None);
    }
    Ok(Some(merge_catalogues(locale, own, shared.as_ref())))
}

/// Delete every translation of an application — what
/// [`delete_application`](crate::delete_application) calls, so the catalogue
/// goes with the application it belongs to.
pub async fn delete_application_translations(cat: &Catalog, application: AppId) -> Result<()> {
    if cat.get(TRANSLATIONS_TABLE)?.is_none() {
        // A deployment whose bootstrap has not run has nothing to delete, and
        // failing the application's deletion over it would be worse.
        return Ok(());
    }
    let delete = Delete::from(TRANSLATIONS_TABLE)
        .filter(Expr::col(COL_APPLICATION).eq(Expr::lit(application.0)));
    run(cat, Statement::from(delete)).await
}

async fn run(cat: &Catalog, statement: Statement) -> Result<()> {
    cat.primary().query(&statement).await?.try_collect().await?;
    Ok(())
}

async fn rows(cat: &Catalog, select: Select) -> Result<Vec<Row>> {
    cat.primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

fn row_text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(Error::invalid(format!(
            "{TRANSLATIONS_TABLE}.{column} should be text, got {}",
            other.map(Value::kind).unwrap_or("nothing")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::FrameworkRef;

    fn react_app() -> Application {
        Application::new(
            "Tasks",
            "tasks",
            FrameworkRef {
                name: crate::react::REACT_FRAMEWORK.to_owned(),
                config: [
                    ("store".to_owned(), json!("apps")),
                    ("project".to_owned(), json!("tasks")),
                ]
                .into_iter()
                .collect(),
            },
        )
    }

    fn rows_app() -> Application {
        Application::new(
            "Books",
            "books",
            FrameworkRef {
                name: "saltcorn-ui".to_owned(),
                config: sc_catalog::Attrs::new(),
            },
        )
    }

    #[test]
    fn the_table_has_the_section_9_columns_and_an_application() {
        let fields = translations_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();
        assert!(by_name(COL_ID).primary_key && by_name(COL_ID).required);
        assert!(by_name(COL_NAME).required);
        assert!(!by_name(COL_DESCRIPTION).required);
        assert_eq!(by_name(COL_ATTRIBUTES).base.type_, json_type());
        assert_eq!(by_name(COL_APPLICATION).base.type_, uuid_type());
        assert_eq!(by_name(COL_MESSAGES).base.type_, json_type());
        // The locale tag is unique per application, not globally.
        assert!(!by_name(COL_NAME).unique);
        assert_eq!(
            name_key(),
            ConstraintKind::Unique {
                fields: vec![COL_APPLICATION.to_owned(), COL_NAME.to_owned()]
            }
        );
        assert!(TRANSLATIONS_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn an_untranslated_application_has_no_locales_and_costs_nothing() {
        let app = react_app();
        assert!(!app_is_translated(&app));
        assert!(app_locales(&app).unwrap().is_empty());
        assert!(app_default_locale(&app).unwrap().is_none());
    }

    #[test]
    fn locales_round_trip_through_the_attributes() {
        let mut app = react_app();
        let fr = Locale::parse("fr").unwrap();
        let de = Locale::parse("de").unwrap();
        set_app_locales(&mut app, &[fr.clone(), de.clone()], Some(&fr));
        assert!(app_is_translated(&app));
        let tags: Vec<String> = app_locales(&app)
            .unwrap()
            .iter()
            .map(|l| l.as_str().to_owned())
            .collect();
        assert_eq!(tags, vec!["fr".to_owned(), "de".to_owned()]);
        assert_eq!(
            app_default_locale(&app).unwrap().unwrap().as_str(),
            fr.as_str()
        );

        // Turning it off removes the attributes rather than storing empties:
        // §9's sparse rule, and what D11 checks.
        set_app_locales(&mut app, &[], None);
        assert!(!app.attributes.contains_key(ATTR_LOCALES));
        assert!(!app.attributes.contains_key(ATTR_DEFAULT_LOCALE));
        let _ = de;
    }

    #[test]
    fn a_locales_attribute_that_is_not_a_list_of_tags_is_refused_by_name() {
        let mut app = react_app();
        app.attributes
            .insert(ATTR_LOCALES.to_owned(), json!("fr, de"));
        let err = app_locales(&app).unwrap_err().to_string();
        assert!(err.contains(ATTR_LOCALES), "{err}");
        assert!(err.contains("Tasks"), "{err}");

        let mut app = react_app();
        app.attributes
            .insert(ATTR_LOCALES.to_owned(), json!(["not a locale!"]));
        assert!(app_locales(&app).is_err());
    }

    #[test]
    fn an_app_with_a_tree_gets_files_and_one_without_gets_rows() {
        let files = app_catalog_store(&react_app()).unwrap();
        assert!(
            files.describe().contains("tasks/locales"),
            "{}",
            files.describe()
        );
        assert!(files.describe().contains("apps"), "{}", files.describe());

        let store = app_catalog_store(&rows_app()).unwrap();
        assert!(
            store.describe().contains(TRANSLATIONS_TABLE),
            "{}",
            store.describe()
        );
    }

    #[test]
    fn a_catalogue_is_served_beside_the_endpoint_set_and_read_back_from_there() {
        let fr = Locale::parse("fr").unwrap();

        // An app with no API provider has no mount to sit beside.
        let app = react_app();
        assert_eq!(i18n_catalog_path(&app, &fr), "/i18n/fr.json");
        assert_eq!(i18n_locale_in_path(&app, "/i18n/fr.json"), Some("fr"));

        // One with a provider at `/api`: beside it, where the provider's own
        // "no such endpoint" would otherwise answer.
        let mut app = react_app();
        app.apis = vec![crate::application::ApiConfig::new("rest", "/api")];
        assert_eq!(i18n_catalog_path(&app, &fr), "/api/i18n/fr.json");
        assert_eq!(i18n_locale_in_path(&app, "/api/i18n/fr.json"), Some("fr"));
        assert_eq!(
            i18n_locale_in_path(&app, "/api/i18n/zh-Hans.json"),
            Some("zh-Hans")
        );

        // Everything that is not one of those paths.
        for not_one in [
            "/api/i18n/fr",
            "/api/i18n/.json",
            "/api/i18n/a/b.json",
            "/i18n/fr.json",
            "/api/tasks",
        ] {
            assert_eq!(i18n_locale_in_path(&app, not_one), None, "{not_one}");
        }
    }

    #[test]
    fn an_apps_key_wins_and_a_blank_one_lets_the_instance_show_through() {
        let de = Locale::parse("de").unwrap();
        let mut own = MessageCatalog::new(de.clone());
        own.insert("Save", Message::Simple("Sichern".to_owned()));
        own.insert("Copy", Message::Simple("  ".to_owned()));
        let mut shared = MessageCatalog::new(de.clone());
        shared.insert("Save", Message::Simple("Speichern".to_owned()));
        shared.insert("Copy", Message::Simple("Kopieren".to_owned()));
        shared.insert("Password", Message::Simple("Passwort".to_owned()));
        shared.insert("Blank", Message::Simple(String::new()));

        let merged = merge_catalogues(&de, Some(own), Some(&shared));
        let text = |k: &str| merged.get(k).map(|m| m.forms().join("/"));
        assert_eq!(text("Save").as_deref(), Some("Sichern"));
        assert_eq!(text("Copy").as_deref(), Some("Kopieren"));
        assert_eq!(text("Password").as_deref(), Some("Passwort"));
        // A blank shared value is not a translation either.
        assert_eq!(text("Blank"), None);

        // No app catalogue at all: the instance's, whole.
        let only = merge_catalogues(&de, None, Some(&shared));
        assert_eq!(only.len(), 3);
        assert!(merge_catalogues(&de, None, None).is_empty());
    }

    #[test]
    fn the_instance_store_says_so() {
        let store = instance_catalog_store();
        assert!(store.is_instance());
        assert!(
            store.describe().contains("instance"),
            "{}",
            store.describe()
        );
        assert!(!RowCatalogStore::new(AppId::new()).is_instance());
    }

    #[test]
    fn a_file_catalogues_path_is_the_locale_under_the_projects_locales_dir() {
        let store = FileCatalogStore::new("apps", "tasks");
        let fr = Locale::parse("fr").unwrap();
        assert_eq!(store.path(&fr), "tasks/locales/fr.json");
        assert_eq!(store.dir(), "tasks/locales");

        // A project at the store root has no prefix to add.
        let root = FileCatalogStore::new("apps", "");
        assert_eq!(root.path(&fr), "locales/fr.json");
    }
}
