//! An application's catalogue, through both halves of the [`CatalogStore`]
//! seam (§16.1, D4): files for an app with a project tree, `_fd_translations`
//! rows for one without.
//!
//! The point of the seam is that everything above it — the admin API, the
//! Translations screen, the LLM fill — is written once, so what this asserts is
//! that the two implementations behave the same through the same trait object:
//! the same round trip, the same listing, the same delete. Then the two things
//! that differ and matter: a file catalogue is a file in the admin's repository
//! (so it is pretty-printed and newline-terminated, and a stray file in
//! `locales/` is somebody else's), and a row catalogue is deleted with its
//! application.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_app::i18n::{CatalogStore, FileCatalogStore, RowCatalogStore, app_catalog_store};
use sc_app::{
    AppId, Application, FrameworkRef, TRANSLATIONS_TABLE, bootstrap, delete_application,
    save_application,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_i18n::{Catalog as MessageCatalog, Locale, Message};
use sc_test_harness::TestDb;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<TempDir> {
        let dir = std::env::temp_dir().join(format!(
            "sc-app-i18n-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::save_file_store(&catalog, &sc_files::FileStoreDef::local("apps", "/srv/x")).await?;
    bootstrap(&catalog).await?;
    Ok(catalog)
}

/// The French catalogue of a small application: a plain message, one with a
/// placeholder, and a plural.
fn french() -> MessageCatalog {
    let mut cat = MessageCatalog::new(Locale::parse("fr").unwrap());
    cat.insert(
        "Add a task",
        Message::Simple("Ajouter une tâche".to_owned()),
    );
    cat.insert(
        "Delete {name}?",
        Message::Simple("Supprimer {name} ?".to_owned()),
    );
    cat.insert(
        "{count} rows",
        Message::from_json(
            "{count} rows",
            &serde_json::json!({ "one": "{count} ligne", "other": "{count} lignes" }),
        )
        .unwrap(),
    );
    cat
}

/// The same assertions against whichever store is handed in: a locale that is
/// not there, a round trip, a listing, an overwrite, and a delete.
async fn round_trip(cat: &Catalog, store: &dyn CatalogStore) -> Result<()> {
    let fr = Locale::parse("fr")?;
    let de = Locale::parse("de")?;

    // A store that holds nothing says so, rather than failing.
    assert!(store.locales(cat).await?.is_empty(), "{}", store.describe());
    assert!(store.load(cat, &fr).await?.is_none());
    assert!(!store.delete(cat, &fr).await?);

    store.save(cat, &french()).await?;
    let back = store.load(cat, &fr).await?.expect("the catalogue we saved");
    assert_eq!(back.len(), 3);
    assert_eq!(
        back.get("Add a task").map(Message::forms),
        Some(vec!["Ajouter une tâche"])
    );
    // The plural survived as a plural, not as one of its forms.
    assert_eq!(
        back.get("{count} rows")
            .unwrap()
            .categories()
            .unwrap()
            .len(),
        2
    );

    // A second locale is a second catalogue, listed beside the first.
    let mut german = MessageCatalog::new(de.clone());
    german.insert("Add a task", Message::Simple("Aufgabe".to_owned()));
    store.save(cat, &german).await?;
    let tags: Vec<String> = store
        .locales(cat)
        .await?
        .iter()
        .map(|l| l.as_str().to_owned())
        .collect();
    assert_eq!(tags, vec!["de".to_owned(), "fr".to_owned()]);

    // `load_all` is the domain, assembled from the two.
    let all = store.load_all(cat).await?;
    assert_eq!(all.locales().len(), 2);
    assert_eq!(all.translate(&fr, "Add a task", &[]), "Ajouter une tâche");
    // A key the domain has not got renders its own English (D1).
    assert_eq!(all.translate(&fr, "Untranslated", &[]), "Untranslated");

    // Saving replaces rather than merging: the catalogue is the whole file.
    let mut shorter = MessageCatalog::new(fr.clone());
    shorter.insert("Add a task", Message::Simple("Ajouter".to_owned()));
    store.save(cat, &shorter).await?;
    let back = store.load(cat, &fr).await?.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(
        back.get("Add a task").map(Message::forms),
        Some(vec!["Ajouter"])
    );

    assert!(store.delete(cat, &fr).await?);
    assert!(store.load(cat, &fr).await?.is_none());
    let tags: Vec<String> = store
        .locales(cat)
        .await?
        .iter()
        .map(|l| l.as_str().to_owned())
        .collect();
    assert_eq!(tags, vec!["de".to_owned()]);
    assert!(store.delete(cat, &de).await?);
    Ok(())
}

#[tokio::test]
async fn a_code_applications_catalogues_are_files_in_its_project() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let tmp = TempDir::new("files")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    let store = FileCatalogStore::new("apps", "tasks");
    round_trip(&cat, &store).await?;

    // What the admin's repository actually gains: `tasks/locales/fr.json`,
    // pretty-printed and newline-terminated, because it is a file somebody will
    // read in a diff.
    store.save(&cat, &french()).await?;
    let path = tmp.path().join("tasks/locales/fr.json");
    let text = std::fs::read_to_string(&path)?;
    assert!(text.ends_with("}\n"), "{text}");
    assert!(text.contains("\n  \"Add a task\""), "{text}");
    // A message id is the English source text (D1), so the file is readable.
    assert!(text.contains("Delete {name}?"), "{text}");

    // A file in `locales/` that is not a locale belongs to somebody else and is
    // not mistaken for a catalogue.
    std::fs::write(tmp.path().join("tasks/locales/README.md"), "notes\n")?;
    std::fs::write(tmp.path().join("tasks/locales/_shared.json"), "{}\n")?;
    let tags: Vec<String> = store
        .locales(&cat)
        .await?
        .iter()
        .map(|l| l.as_str().to_owned())
        .collect();
    assert_eq!(tags, vec!["fr".to_owned()]);

    Ok(())
}

#[tokio::test]
async fn an_application_without_a_tree_keeps_its_catalogues_in_rows() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // No application row is needed to hold a catalogue: `application` is a value
    // (§16.1), not a key, for the reason `_fd_views.application` is not one.
    let app = AppId::new();
    let store = RowCatalogStore::new(app);
    round_trip(&cat, &store).await?;

    // Another application's catalogue is not this one's, even under the same
    // locale tag: the key is (application, name).
    let other = RowCatalogStore::new(AppId::new());
    store.save(&cat, &french()).await?;
    assert!(other.load(&cat, &Locale::parse("fr")?).await?.is_none());

    // The table is the one §9 describes.
    let table = cat.require(TRANSLATIONS_TABLE)?;
    assert!(table.fields.iter().any(|f| f.base.name == "messages"));
    assert!(table.fields.iter().any(|f| f.base.name == "application"));

    Ok(())
}

#[tokio::test]
async fn deleting_an_application_deletes_its_catalogues() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let app = save_application(
        &cat,
        &Application::new(
            "Blog",
            "blog",
            FrameworkRef::new("code")
                .with("store", "apps")
                .with("source", "web")
                .with("output", "web/dist")
                .with("command", "npm run build"),
        ),
    )
    .await?;
    let kept = save_application(
        &cat,
        &Application::new(
            "Shop",
            "shop",
            FrameworkRef::new("code")
                .with("store", "apps")
                .with("source", "web")
                .with("output", "web/dist")
                .with("command", "npm run build"),
        ),
    )
    .await?;

    let doomed = RowCatalogStore::new(app.id);
    let survivor = RowCatalogStore::new(kept.id);
    doomed.save(&cat, &french()).await?;
    survivor.save(&cat, &french()).await?;

    assert!(delete_application(&cat, app.id).await?);
    assert!(doomed.load(&cat, &Locale::parse("fr")?).await?.is_none());
    assert!(survivor.load(&cat, &Locale::parse("fr")?).await?.is_some());

    Ok(())
}

#[tokio::test]
async fn the_store_is_chosen_by_whether_the_framework_has_a_tree() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let tmp = TempDir::new("choice")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // A `react` app: files, under its project directory.
    let react = Application::new(
        "Tasks",
        "tasks",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "tasks"),
    );
    let store = app_catalog_store(&react)?;
    store.save(&cat, &french()).await?;
    assert!(tmp.path().join("tasks/locales/fr.json").is_file());

    // A framework with no build step — Saltcorn UI, whose definition is rows —
    // gets rows. Nothing is written to the file store.
    let ui = Application::new("Books", "books", FrameworkRef::new("saltcorn-ui"));
    let store = app_catalog_store(&ui)?;
    store.save(&cat, &french()).await?;
    assert!(store.load(&cat, &Locale::parse("fr")?).await?.is_some());
    assert!(!tmp.path().join("books").exists());

    Ok(())
}

// ---------------------------------------------------------------------------
// The instance catalogue (TODO F6.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_instance_catalogue_is_rows_with_no_application() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // CRUD per locale, through the same trait the app stores answer.
    let shared = sc_app::instance_catalog_store();
    round_trip(&cat, &shared).await?;

    // Saving twice updates the one row: a unique key does not stop two `NULL`
    // applications, so the store must not insert a second.
    shared.save(&cat, &french()).await?;
    shared.save(&cat, &french()).await?;
    let rows: Vec<sc_db::Row> = cat
        .primary()
        .query(&sc_query::Statement::from(sc_query::Select::from(
            sc_query::Source::table(TRANSLATIONS_TABLE),
        )))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("application").is_some_and(|v| v.is_null()));

    // The instance's catalogue is not an application's, and the reverse.
    let app = RowCatalogStore::new(AppId::new());
    let fr = Locale::parse("fr")?;
    assert!(app.load(&cat, &fr).await?.is_none());
    let mut own = MessageCatalog::new(Locale::parse("de")?);
    own.insert("Copy", Message::Simple("Kopie".to_owned()));
    app.save(&cat, &own).await?;
    assert!(shared.load(&cat, &Locale::parse("de")?).await?.is_none());
    assert_eq!(
        shared
            .locales(&cat)
            .await?
            .iter()
            .map(|l| l.as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["fr".to_owned()]
    );
    Ok(())
}

#[tokio::test]
async fn deleting_an_application_keeps_the_instance_catalogue() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let app = save_application(
        &cat,
        &Application::new(
            "Blog",
            "blog",
            FrameworkRef::new("code")
                .with("store", "apps")
                .with("source", "web")
                .with("output", "web/dist")
                .with("command", "npm run build"),
        ),
    )
    .await?;
    RowCatalogStore::new(app.id).save(&cat, &french()).await?;
    sc_app::instance_catalog_store()
        .save(&cat, &french())
        .await?;

    assert!(delete_application(&cat, app.id).await?);
    assert!(
        sc_app::instance_catalog_store()
            .load(&cat, &Locale::parse("fr")?)
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn an_app_catalogue_is_served_over_the_instance_one() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let tmp = TempDir::new("merge")?;
    cat.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    // A file-backed app: the merge is the same whichever store the app has.
    let app = Application::new(
        "Tasks",
        "tasks",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "tasks"),
    );
    let de = Locale::parse("de")?;

    // Nothing anywhere: nothing to merge.
    assert!(
        sc_app::merged_app_catalogue(&cat, &app, &de)
            .await?
            .is_none()
    );

    let mut shared = MessageCatalog::new(de.clone());
    shared.insert("Sign in", Message::Simple("Anmelden".to_owned()));
    shared.insert("Copy", Message::Simple("Kopieren".to_owned()));
    sc_app::instance_catalog_store().save(&cat, &shared).await?;
    let mut own = MessageCatalog::new(de.clone());
    own.insert("Copy", Message::Simple("Duplizieren".to_owned()));
    app_catalog_store(&app)?.save(&cat, &own).await?;

    let merged = sc_app::merged_app_catalogue(&cat, &app, &de)
        .await?
        .expect("both have one");
    assert_eq!(
        merged.get("Copy").map(Message::forms),
        Some(vec!["Duplizieren"])
    );
    assert_eq!(
        merged.get("Sign in").map(Message::forms),
        Some(vec!["Anmelden"])
    );
    Ok(())
}

#[tokio::test]
async fn a_table_from_before_the_instance_catalogue_is_loosened_by_bootstrap() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    // The shape the table had before F6: `application` NOT NULL.
    cat.apply_schema_batch(&[sc_catalog::SchemaStep::Change(
        sc_db::SchemaChange::SetColumnNullable {
            table: TRANSLATIONS_TABLE.to_owned(),
            column: "application".to_owned(),
            nullable: false,
        },
    )])
    .await?;
    cat.reload().await?;
    assert!(
        cat.require(TRANSLATIONS_TABLE)?
            .field("application")
            .unwrap()
            .required
    );

    sc_app::bootstrap_translations(&cat).await?;
    assert!(
        !cat.require(TRANSLATIONS_TABLE)?
            .field("application")
            .unwrap()
            .required
    );
    sc_app::instance_catalog_store()
        .save(&cat, &french())
        .await?;
    Ok(())
}
