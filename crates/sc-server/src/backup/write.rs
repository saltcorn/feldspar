//! Writing a backup: the selection in, a zip out.
//!
//! Everything here is a read, so nothing in this file can damage an
//! installation — which is why it is worth keeping the writing separate from the
//! restoring even though they share a layout.
//!
//! Two things it does *not* do, deliberately:
//!
//! - **It does not redact.** A file store's backend settings, an LLM provider's
//!   API key and the SSL private key are written as they are stored. A backup with the private key replaced by
//!   the redaction sentinel would restore an installation that cannot serve HTTPS,
//!   and one with a store's credentials removed would restore a store that cannot
//!   connect. The zip is therefore as sensitive as the database, and the dialog
//!   says so where the admin chooses.
//!
//! And one thing it does, which shapes the rest: **it streams, into a file.**
//! Memory is bounded by about one page of rows plus the I/O buffers, whatever
//! the size of the installation. That rules out the obvious arrangement — read a
//! table, serialise it, add it to a zip in memory — and is what a 43 MB `bytea`
//! table taking 3.4 GB to back up taught. So:
//!
//! - The zip is written to a [`std::fs::File`] ([`write_backup_to`]), never to a
//!   buffer. The callers spool it to disk and serve, rename or send that file.
//! - A table's rows are read a **page** at a time ([`write_table_rows`]) and
//!   written one compact JSON line each as they arrive. The page size adapts to
//!   the rows, so a table of large blobs is read a few rows at a time.
//! - A file store's files are copied a chunk at a time, and a file that is
//!   already compressed (a video, an image, a zip) is stored rather than
//!   deflated again ([`already_compressed`]).
//! - On Postgres every row is read through **one snapshot**
//!   ([`sc_db::DatabaseDriver::begin_snapshot`]), so a table read last is
//!   consistent with the one read first and the foreign keys between them line
//!   up — `pg_dump`'s arrangement, and the answer to "what if a table changes
//!   underneath a half-written file".

use std::io::{Seek, Write};

use sc_api::rows;
use sc_catalog::{Catalog, FIELD_META_TABLE, Table, list_field_meta_for_table, list_file_stores};
use sc_db::{Row, Transaction};
use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, OrderBy, Projection, Select, Source, Statement, Value};
use serde_json::{Map, Value as Json, json};
use tokio::io::AsyncReadExt;
use zip::write::SimpleFileOptions;

use super::values;
use super::{
    Available, Item, MANIFEST_FILE, MANIFEST_SECRETS_KEY, SECRETS_KEY_FILE, SSL_SECTION, Selection,
};
use crate::handlers::{
    agent_json, application_json, backup_db_connection_json, backup_file_meta_json,
    backup_llm_provider_json, backup_model_json, backup_module_json, backup_store_def_json,
    backup_stream_json, constraint_json, field_json, library_item_json, page_json, role_json,
    table_json, trigger_json, trigger_table, view_json,
};

/// What can be included in a backup of this server, right now.
///
/// The `users` table is **not** among the tables: user accounts are the Users
/// choice, which carries the table's rows (password hashes included) along with
/// the roles they point at and the role grants they hold (`role_grants`, which
/// is left out of the tables for the same reason). Offering it twice would let an admin tick "users" and
/// untick the `users` table and get something incoherent.
///
/// Tables on another database connection are not among them either: the
/// connection is.
pub async fn available(catalog: &Catalog) -> Result<Available> {
    let mut tables = Vec::new();
    for table in catalog.tables()? {
        if table.is_system()
            || table.name == sc_auth::USERS_TABLE
            || table.name == sc_auth::ROLE_GRANTS_TABLE
        {
            continue;
        }
        // A table on another database is that database's to back up: the
        // connection travels (passwords and all) and brings its tables back
        // with it, and a restore that wrote this archive's rows into a live
        // external database would be writing somewhere the admin never chose.
        if table.database != sc_catalog::DbId::primary() {
            continue;
        }
        let count = rows::count_rows(catalog, &table, None).await.unwrap_or(0);
        tables.push(
            Item::new(table.name.clone())
                .labelled(table.label.clone())
                .counting(count),
        );
    }

    let apps = sc_app::list_applications(catalog).await?;
    let applications = apps
        .iter()
        .map(|app| Item::new(app.subdomain.clone()).labelled(app.name.clone()))
        .collect();
    // Over every application: views and pages are one choice each, and travel
    // inside whichever applications are chosen.
    let (mut views, mut pages) = (0, 0);
    for app in &apps {
        views += views_of(catalog, app).await?.len();
        pages += pages_of(catalog, app).await?.len();
    }

    // Defined stores, not merely connected ones: a store whose directory is not
    // mounted right now is still a definition worth backing up, and its files
    // simply come out empty. The count is left unknown — answering it would mean
    // walking every store every time the dialog opens.
    let file_stores = list_file_stores(catalog)
        .await?
        .iter()
        .map(|def| Item::new(def.name.clone()).labelled(def.description.clone()))
        .collect();

    let users = match catalog.get(sc_auth::USERS_TABLE)? {
        Some(users) => rows::count_rows(catalog, &users, None).await.unwrap_or(0),
        None => 0,
    };

    let modules = if has_table(catalog, sc_module::MODULES_TABLE)? {
        count(sc_module::list_modules(catalog).await?.len())
    } else {
        0
    };
    let db_connections = if has_table(catalog, sc_catalog::DB_CONNECTIONS_TABLE)? {
        count(sc_catalog::list_db_connections(catalog).await?.len())
    } else {
        0
    };
    let streams = if has_table(catalog, sc_stream::STREAMS_TABLE)? {
        count(sc_stream::list_streams(catalog).await?.len())
    } else {
        0
    };
    let datasets = if has_table(catalog, sc_dataset::DATASETS_TABLE)? {
        count(sc_dataset::list_datasets(catalog).await?.len())
    } else {
        0
    };
    let workspaces = if has_table(catalog, sc_analytics::WORKSPACES_TABLE)? {
        count(sc_analytics::list_workspaces(catalog).await?.len())
    } else {
        0
    };
    let (models, fits) = if has_table(catalog, sc_model::MODELS_TABLE)? {
        let models = sc_model::list_models(catalog).await?;
        let mut fits = 0;
        for model in &models {
            fits += kept_fits(catalog, model).await?.len();
        }
        (count(models.len()), count(fits))
    } else {
        (0, 0)
    };

    Ok(Available {
        tables,
        applications,
        file_stores,
        users,
        modules,
        db_connections,
        streams,
        datasets,
        models,
        workspaces,
        fits,
        llm_providers: if has_table(catalog, sc_llm::LLM_PROVIDERS_TABLE)? {
            count(stored_llm_providers(catalog).await?.len())
        } else {
            0
        },
        agents: i64::try_from(sc_agent::list_agents(catalog).await?.len()).unwrap_or(i64::MAX),
        code_libraries: if has_table(catalog, sc_catalog::CODE_LIBRARIES_TABLE)? {
            count(sc_catalog::list_code_libraries(catalog).await?.len())
        } else {
            0
        },
        // Bootstrapped by `serve` rather than at connect, so a database no server
        // has run on yet — one a `feldspar backup` can be pointed at — has none.
        triggers: if has_table(catalog, sc_action::TRIGGERS_TABLE)? {
            count(sc_action::list_triggers(catalog).await?.len())
        } else {
            0
        },
        views: i64::try_from(views).unwrap_or(i64::MAX),
        pages: i64::try_from(pages).unwrap_or(i64::MAX),
        // There is always an SSL section to include, even when every value in it
        // is the default — "serve plain HTTP" is a setting an admin may well want
        // restored onto a copy of a production server.
        ssl: true,
        // Likewise every other section: "no SMTP server" is a setting too.
        settings: true,
    })
}

/// A length as a count.
fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Whether a system table has been bootstrapped on this server. A server that
/// never ran the feature has no rows to back up, rather than an error.
fn has_table(catalog: &Catalog, name: &str) -> Result<bool> {
    Ok(catalog.get(name)?.is_some())
}

/// The fits of `model` a backup carries: every one that finished, fitted or
/// failed. One still fitting has no result yet, and the boot reap would mark
/// it failed on the server it was restored to anyway.
async fn kept_fits(
    catalog: &Catalog,
    model: &sc_model::Model,
) -> Result<Vec<sc_model::ModelInstance>> {
    Ok(sc_model::list_model_instances(catalog, model.id)
        .await?
        .into_iter()
        .filter(|i| i.status != sc_model::FitStatus::Fitting)
        .collect())
}

/// Write the zip for `selection` into `file`, returning its length.
///
/// The file is the whole answer: what went into it is *in* it, in the
/// manifest, which is the same thing a restore reads back with
/// [`inspect`](super::inspect). A second copy of that record travelling beside the
/// file would be a second copy to keep true.
///
/// The zip writes are synchronous calls into a buffered file — the page cache —
/// made from async code, as the in-memory writes they replaced were.
pub async fn write_backup_to(
    catalog: &Catalog,
    selection: &Selection,
    file: std::fs::File,
) -> Result<u64> {
    let mut zip = ZipBuilder::new(std::io::BufWriter::with_capacity(256 * 1024, file));
    let mut source = RowSource::open(catalog).await?;
    write_entries(catalog, selection, &mut source, &mut zip).await?;
    source.finish().await;
    let file = zip
        .finish()?
        .into_inner()
        .map_err(|e| zip_error("the archive", &e.error().to_string()))?;
    // On disk before anyone is told it is there: the CLI and the scheduler
    // rename it into place next, and a rename can reach the disk first.
    file.sync_all()
        .map_err(|e| zip_error("the archive", &e.to_string()))?;
    file.metadata()
        .map(|m| m.len())
        .map_err(|e| zip_error("the archive", &e.to_string()))
}

async fn write_entries<W: Write + Seek>(
    catalog: &Catalog,
    selection: &Selection,
    source: &mut RowSource<'_>,
    zip: &mut ZipBuilder<W>,
) -> Result<()> {
    let mut contents = Available::default();
    // Whether a table's rows went in with Secret values in them, which is
    // what decides whether the key goes in too.
    let mut sealed_rows = false;

    // --- tables: the overlay, the columns, and the rows ---------------------
    let rls = catalog.primary().capabilities().row_level_security;
    let has_field_meta = catalog.get(FIELD_META_TABLE)?.is_some();
    for name in &selection.tables {
        let table = catalog.require(name)?;
        let metas = if has_field_meta {
            list_field_meta_for_table(catalog, &table.name).await?
        } else {
            Vec::new()
        };
        let fields: Vec<Json> = table
            .fields
            .iter()
            .map(|f| {
                let description = metas
                    .iter()
                    .find(|m| m.field_name == f.base.name)
                    .map(|m| m.description.clone())
                    .unwrap_or_default();
                field_json(f, &description)
            })
            .collect();
        // The constraints go in the backup for the reason the fields do: a
        // restore that recreated the columns and not the rules would hand back a
        // table that accepts what the original refused, and say nothing about it
        // (§5.1). They are *read* off the table, so what is written is what the
        // database has — including a constraint somebody added by hand.
        let constraints: Vec<Json> = table.constraints.iter().map(constraint_json).collect();
        zip.json(
            &format!("tables/{name}/table.json"),
            &json!({
                // A backup is an archive, not a screen: its `table.json` is
                // read back by a restore and by a person grepping it, so it is
                // written in the source language whatever the admin who pressed
                // the button reads (§16.1).
                "table": table_json(catalog, &table, rls, &sc_i18n::Locale::source()),
                "fields": fields,
                "constraints": constraints,
            }),
        )?;

        let mut count = None;
        if selection.includes_data(name) {
            sealed_rows |= sc_catalog::secrets::has_secrets(&table);
            count = Some(
                write_table_rows(
                    source,
                    &table,
                    None,
                    zip,
                    &format!("tables/{name}/rows.ndjson"),
                    true,
                )
                .await?,
            );
        }
        contents.tables.push({
            let item = Item::new(table.name.clone()).labelled(table.label.clone());
            match count {
                Some(count) => item.counting(count),
                None => item,
            }
        });
    }

    // --- applications -------------------------------------------------------
    for app in sc_app::list_applications(catalog).await? {
        if !selection.includes_application(&app.subdomain) {
            continue;
        }
        zip.json(
            &format!("applications/{}.json", app.subdomain),
            &application_json(&app),
        )?;
        contents
            .applications
            .push(Item::new(app.subdomain.clone()).labelled(app.name.clone()));
        // A Saltcorn UI application's content is rows rather than a source tree,
        // so without these a restored one would serve nothing.
        if selection.views {
            // The library travels with the views, the choice that places its
            // items, and only a Saltcorn UI application has one (TODO "The
            // builder" §8).
            if app.framework.name == sc_viewpattern::SALTCORN_UI_FRAMEWORK {
                let library: Vec<Json> = library_of(catalog, &app)
                    .await?
                    .iter()
                    .map(library_item_json)
                    .collect();
                zip.json(
                    &format!("applications/{}/library.json", app.subdomain),
                    &Json::Array(library),
                )?;
            }
            let views: Vec<Json> = views_of(catalog, &app)
                .await?
                .iter()
                .map(view_json)
                .collect();
            contents.views += i64::try_from(views.len()).unwrap_or(i64::MAX);
            zip.json(
                &format!("applications/{}/views.json", app.subdomain),
                &Json::Array(views),
            )?;
        }
        if selection.pages {
            let pages: Vec<Json> = pages_of(catalog, &app)
                .await?
                .iter()
                .map(page_json)
                .collect();
            contents.pages += i64::try_from(pages.len()).unwrap_or(i64::MAX);
            zip.json(
                &format!("applications/{}/pages.json", app.subdomain),
                &Json::Array(pages),
            )?;
        }
        // A Saltcorn UI application's translations are rows, so they travel
        // here; a code application's are files in its repository, and travel
        // with its file store.
        if let Some(catalogues) = row_translations(catalog, &app).await? {
            zip.json(
                &format!("applications/{}/translations.json", app.subdomain),
                &catalogues,
            )?;
        }
    }

    // --- file stores: the definition, the metadata, and the bytes -----------
    for def in list_file_stores(catalog).await? {
        if !selection.includes_store(&def.name) {
            continue;
        }
        let dir = format!("file-stores/{}", def.name);
        let mut files = Vec::new();
        // A defined store that is not connected right now backs up as its
        // definition and no files, rather than failing the whole backup: the
        // definition is the part that is hard to recreate by hand.
        if let Ok(store) = catalog.require_file_store(&def.name) {
            for (path, size) in walk(store.as_ref(), "").await? {
                let written = copy_file(
                    store.as_ref(),
                    &path,
                    size,
                    zip,
                    &format!("{dir}/files/{path}"),
                )
                .await?;
                let meta = store.get_meta(&path).await.unwrap_or_default();
                let mode = file_mode(store.as_ref(), &path);
                files.push(backup_file_meta_json(&path, Some(written), &meta, mode));
            }
        }
        let count = i64::try_from(files.len()).unwrap_or(i64::MAX);
        zip.json(
            &format!("{dir}/store.json"),
            &json!({ "definition": backup_store_def_json(&def), "files": files }),
        )?;
        contents.file_stores.push(
            Item::new(def.name.clone())
                .labelled(def.description.clone())
                .counting(count),
        );
    }

    // --- users and the roles they point at ----------------------------------
    //
    // One directory, because a user without their role is a foreign key pointing
    // at nothing: `users.json` for the roles and the admin-added columns,
    // `rows.ndjson` beside it for the accounts. The rows are the table's own —
    // **including `password_hash`**, which is the difference between a restored
    // installation people can sign in to and one where every account needs a
    // new password. A hash is what the database holds and what a restore has to
    // put back.
    if selection.users {
        let users_table = catalog.require(sc_auth::USERS_TABLE)?;
        contents.users =
            write_table_rows(source, &users_table, None, zip, super::USER_ROWS_FILE, true).await?;
        // The grants the accounts hold: a subscription's, a trial's, an
        // admin's comp. Theirs, so with them.
        if let Some(grants) = catalog.get(sc_auth::ROLE_GRANTS_TABLE)? {
            write_table_rows(
                source,
                &grants,
                None,
                zip,
                super::ROLE_GRANT_ROWS_FILE,
                false,
            )
            .await?;
        }
        let roles: Vec<Json> = sc_auth::list_roles(catalog)
            .await?
            .iter()
            .map(role_json)
            .collect();
        // The columns an admin added to the users table (§7.1 invites them to),
        // and only those: the five the system owns are created by the bootstrap on
        // any server worth restoring onto. Without this the rows would arrive
        // carrying a `nickname` no column accepts, and the restore would drop that
        // value silently — a row is inserted with the columns the table has.
        let metas = if has_field_meta {
            list_field_meta_for_table(catalog, &users_table.name).await?
        } else {
            Vec::new()
        };
        let fields: Vec<Json> = users_table
            .fields
            .iter()
            .filter(|f| !sc_auth::is_system_user_column(&f.base.name))
            .map(|f| {
                let description = metas
                    .iter()
                    .find(|m| m.field_name == f.base.name)
                    .map(|m| m.description.clone())
                    .unwrap_or_default();
                field_json(f, &description)
            })
            .collect();
        zip.json(
            super::USERS_FILE,
            &json!({ "roles": roles, "fields": fields }),
        )?;
    }

    // --- modules -----------------------------------------------------------
    //
    // The rows, not the packages: a restore reinstalls each from where it came,
    // which is what an install already does, and a `node_modules` tree in a zip
    // would be built for the machine that took the backup.
    if selection.modules && has_table(catalog, sc_module::MODULES_TABLE)? {
        let modules: Vec<Json> = sc_module::list_modules(catalog)
            .await?
            .iter()
            .map(backup_module_json)
            .collect();
        contents.modules = count(modules.len());
        zip.json("modules.json", &Json::Array(modules))?;
    }

    // --- connections to other databases ------------------------------------
    if selection.db_connections && has_table(catalog, sc_catalog::DB_CONNECTIONS_TABLE)? {
        let connections: Vec<Json> = sc_catalog::list_db_connections(catalog)
            .await?
            .iter()
            .map(backup_db_connection_json)
            .collect();
        contents.db_connections = count(connections.len());
        zip.json("db-connections.json", &Json::Array(connections))?;
    }

    // --- streams -----------------------------------------------------------
    if selection.streams && has_table(catalog, sc_stream::STREAMS_TABLE)? {
        let streams: Vec<Json> = sc_stream::list_streams(catalog)
            .await?
            .iter()
            .map(backup_stream_json)
            .collect();
        contents.streams = count(streams.len());
        zip.json("streams.json", &Json::Array(streams))?;
    }

    // --- analytics: datasets, models, workspaces, and the fits -------------
    if selection.analytics {
        write_analytics(catalog, selection, source, zip, &mut contents).await?;
    }

    // --- LLM providers, each with its models --------------------------------
    //
    // One entry rather than two, because a model is a row *of* its provider —
    // the same model name under two providers is two models — and a model
    // restored without its provider has nothing to belong to. The API keys are
    // in it (see the module comment): an agent is only restored if its provider
    // validates, and a provider without its key does not.
    if selection.llm_providers && has_table(catalog, sc_llm::LLM_PROVIDERS_TABLE)? {
        let mut providers = Vec::new();
        for def in stored_llm_providers(catalog).await? {
            let models = sc_llm::list_llm_models(catalog, &def).await?;
            providers.push(backup_llm_provider_json(&def, &models));
        }
        contents.llm_providers = i64::try_from(providers.len()).unwrap_or(i64::MAX);
        zip.json("llm-providers.json", &Json::Array(providers))?;
    }

    // --- agents -------------------------------------------------------------
    if selection.agents {
        let agents: Vec<Json> = sc_agent::list_agents(catalog)
            .await?
            .iter()
            .map(|agent| agent_json(agent, None))
            .collect();
        contents.agents = i64::try_from(agents.len()).unwrap_or(i64::MAX);
        zip.json("agents.json", &Json::Array(agents))?;
    }

    // --- code libraries (TODO F3) ---------------------------------------------
    // The admin list's own shape, without the "used by" that is recomputed
    // wherever it is restored.
    if selection.code_libraries {
        let libraries: Vec<Json> = sc_catalog::list_code_libraries(catalog)
            .await?
            .iter()
            .map(|library| sc_app::code_library_json(library, None, None))
            .collect();
        contents.code_libraries = i64::try_from(libraries.len()).unwrap_or(i64::MAX);
        zip.json("code-libraries.json", &Json::Array(libraries))?;
    }

    // --- triggers ----------------------------------------------------------
    if selection.triggers {
        let has_versions = has_table(catalog, sc_workflow::VERSIONS_TABLE)?;
        let mut triggers = Vec::new();
        for trigger in sc_action::list_triggers(catalog).await? {
            if !selection.includes_trigger(trigger_table(&trigger)) {
                continue;
            }
            let mut value = trigger_json(&trigger, None);
            // A workflow's steps are not on the trigger row: they are its
            // current version (§10.3), and a workflow restored without them is
            // one that refuses to run. Only the current one — the history is
            // there for runs to pin to, and runs are not backed up.
            if has_versions
                && let Some(workflow) = sc_workflow::current_workflow(catalog, trigger.id).await?
                && let Json::Object(map) = &mut value
            {
                map.insert(
                    "workflow".to_owned(),
                    serde_json::to_value(&workflow)
                        .map_err(|e| Error::msg(format!("workflow `{}`: {e}", trigger.name)))?,
                );
            }
            triggers.push(value);
        }
        contents.triggers = i64::try_from(triggers.len()).unwrap_or(i64::MAX);
        zip.json("triggers.json", &Json::Array(triggers))?;
    }

    // --- the SSL settings ---------------------------------------------------
    if selection.ssl {
        let stored = sc_config::all_config(catalog).await?;
        let mut values = Map::new();
        for field in sc_config::config_sections()
            .iter()
            .filter(|section| section.name == SSL_SECTION)
            .flat_map(|section| section.fields.iter())
        {
            if let Some(value) = stored.get(field.key()) {
                values.insert(field.key().to_owned(), value.clone());
            }
        }
        contents.ssl = true;
        zip.json("settings/ssl.json", &Json::Object(values))?;
    }

    // --- every other settings section, one file each ------------------------
    if selection.settings {
        let stored = sc_config::all_config(catalog).await?;
        for section in sc_config::config_sections()
            .iter()
            .filter(|section| section.name != SSL_SECTION)
        {
            let mut values = Map::new();
            // Where the admin UI is served is this host's, as its TLS settings
            // are: restored onto another host it would move that host's admin
            // UI to a name nobody pointed at it.
            for field in section
                .fields
                .iter()
                .filter(|field| field.key() != sc_config::ADMIN_SUBDOMAIN)
            {
                if let Some(value) = stored.get(field.key()) {
                    values.insert(field.key().to_owned(), value.clone());
                }
            }
            zip.json(
                &format!("settings/{}.json", section.name),
                &Json::Object(values),
            )?;
        }
        contents.settings = true;
    }

    // The key the Secret values were sealed with, so a restore — onto this
    // server after a disaster, or onto another — opens them without anyone
    // typing a password in again. Which means **this file can decrypt them**:
    // a backup holding Secret fields is as sensitive as the secrets are.
    if sealed_rows {
        let key = catalog.secret_key()?;
        zip.start_file(SECRETS_KEY_FILE, "secrets.key", Some(32))?;
        zip.write_chunk(SECRETS_KEY_FILE, &key.to_bytes())?;
    }

    // The manifest goes in last so it can describe what was actually written —
    // a store that turned out to be unreachable, a table whose rows were left
    // out — rather than what was asked for.
    zip.json(
        MANIFEST_FILE,
        &json!({
            MANIFEST_SECRETS_KEY: sealed_rows.then_some(SECRETS_KEY_FILE),
            "format": super::FORMAT,
            "version": super::FORMAT_VERSION,
            // What wrote it, as Saltcorn 1 records `saltcorn_version` in its own
            // `backup-info.json`: the layout version says how to read the file,
            // this says which build produced it, which is the question asked of an
            // archive that turns up later with something unexpected in it.
            "feldspar_version": super::PRODUCT_VERSION,
            "created_at": chrono::Utc::now().to_rfc3339(),
            // One line for a person — the restore dialog shows it, so an admin
            // about to press Restore can see what they picked up.
            "source": format!("Feldspar {}", super::PRODUCT_VERSION),
            "contents": contents.to_json(),
        }),
    )?;
    Ok(())
}

/// Datasets, models and workspaces — and, when chosen, the models' fits.
///
/// Datasets are written in the order a restore can save them in, every base
/// before the datasets built on it, so the restore does not have to sort.
async fn write_analytics<W: Write + Seek>(
    catalog: &Catalog,
    selection: &Selection,
    source: &mut RowSource<'_>,
    zip: &mut ZipBuilder<W>,
    contents: &mut Available,
) -> Result<()> {
    if has_table(catalog, sc_dataset::DATASETS_TABLE)? {
        let datasets = bases_first(sc_dataset::list_datasets(catalog).await?);
        contents.datasets = count(datasets.len());
        let datasets: Vec<Json> = datasets
            .iter()
            .map(|def| serde_json::to_value(def).map_err(|e| Error::msg(e.to_string())))
            .collect::<Result<_>>()?;
        zip.json("analytics/datasets.json", &Json::Array(datasets))?;
    }
    if has_table(catalog, sc_model::MODELS_TABLE)? {
        let models = sc_model::list_models(catalog).await?;
        let mut out = Vec::with_capacity(models.len());
        for model in &models {
            out.push(backup_model_json(catalog, model).await?);
        }
        contents.models = count(out.len());
        zip.json("analytics/models.json", &Json::Array(out))?;

        if selection.fits {
            let mut instances = Vec::new();
            let mut outputs = Vec::new();
            for model in &models {
                for fit in kept_fits(catalog, model).await? {
                    let draws = catalog.require(sc_model::DRAWS_TABLE)?;
                    instances
                        .extend(rows_of(source, sc_model::INSTANCES_TABLE, "id", fit.id.0).await?);
                    outputs.extend(
                        rows_of(source, sc_model::OUTPUTS_TABLE, "instance", fit.id.0).await?,
                    );
                    // One entry per fit, because a posterior's draws are the
                    // one thing here that can be large: they are paged out as
                    // a table's rows are, and read back a batch at a time.
                    write_table_rows(
                        source,
                        &draws,
                        Some(Expr::col("instance").eq(Expr::lit(fit.id.0))),
                        zip,
                        &format!("analytics/draws/{}.ndjson", fit.id.0),
                        false,
                    )
                    .await?;
                }
            }
            contents.fits = count(instances.len());
            zip.json(
                "analytics/fits.json",
                &json!({ "instances": instances, "outputs": outputs }),
            )?;
        }
    }
    if has_table(catalog, sc_analytics::WORKSPACES_TABLE)? {
        let workspaces: Vec<Json> = sc_analytics::list_workspaces(catalog)
            .await?
            .iter()
            .map(crate::analytics::workspace_json)
            .collect();
        contents.workspaces = count(workspaces.len());
        zip.json("analytics/workspaces.json", &Json::Array(workspaces))?;
    }
    Ok(())
}

/// Datasets ordered so that each comes after the dataset it is built on. A
/// dataset whose base is missing, or in a cycle, goes at the end, where the
/// restore will report it.
fn bases_first(mut pending: Vec<sc_dataset::DatasetDef>) -> Vec<sc_dataset::DatasetDef> {
    let mut ordered: Vec<sc_dataset::DatasetDef> = Vec::with_capacity(pending.len());
    loop {
        let before = pending.len();
        let mut rest = Vec::new();
        for def in pending {
            let ready = match &def.base {
                sc_dataset::Base::Table { .. } => true,
                sc_dataset::Base::Dataset { dataset } => ordered.iter().any(|d| d.id == *dataset),
            };
            if ready {
                ordered.push(def);
            } else {
                rest.push(def);
            }
        }
        pending = rest;
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }
    ordered.extend(pending);
    ordered
}

/// The rows of a system table whose `column` is `id`, as a backup writes them —
/// the fits' rows, which have no admin-API shape to borrow.
async fn rows_of(
    source: &mut RowSource<'_>,
    table: &str,
    column: &str,
    id: uuid::Uuid,
) -> Result<Vec<Json>> {
    let select = Select::from(Source::table(table.to_owned()))
        .columns(vec![Projection::all()])
        .filter(Expr::col(column).eq(Expr::lit(id)));
    let fetched = source.query(&Statement::from(select)).await?;
    Ok(fetched.iter().map(values::row_to_json).collect())
}

/// A Saltcorn UI application's translations, `{ locale: catalogue }` — `None`
/// for a code application, whose catalogues are files in its repository, and
/// for a server that has never stored a translation.
async fn row_translations(catalog: &Catalog, app: &sc_app::Application) -> Result<Option<Json>> {
    if sc_app::app_source_from_config(&app.framework).is_ok()
        || !has_table(catalog, sc_app::TRANSLATIONS_TABLE)?
    {
        return Ok(None);
    }
    use sc_app::CatalogStore;
    let store = sc_app::RowCatalogStore::new(app.id);
    let mut out = Map::new();
    for locale in store.locales(catalog).await? {
        if let Some(messages) = store.load(catalog, &locale).await? {
            out.insert(locale.as_str().to_owned(), messages.to_json());
        }
    }
    Ok(Some(Json::Object(out)))
}

/// An application's views — none on a database whose views table was never
/// bootstrapped, which is a server that has never served a Saltcorn UI app.
async fn views_of(
    catalog: &Catalog,
    app: &sc_app::Application,
) -> Result<Vec<sc_viewpattern::View>> {
    if catalog.get(sc_viewpattern::VIEWS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    sc_viewpattern::list_views(catalog, app.id).await
}

/// An application's pages, with [`views_of`]'s rule.
async fn pages_of(
    catalog: &Catalog,
    app: &sc_app::Application,
) -> Result<Vec<sc_viewpattern::Page>> {
    if catalog.get(sc_viewpattern::PAGES_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    sc_viewpattern::list_pages(catalog, app.id).await
}

/// An application's library items, with [`views_of`]'s rule.
async fn library_of(
    catalog: &Catalog,
    app: &sc_app::Application,
) -> Result<Vec<sc_viewpattern::LibraryItem>> {
    if catalog.get(sc_viewpattern::LIBRARY_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    sc_viewpattern::list_library(catalog, app.id).await
}

/// A file's permission bits, where the store keeps it on disk and the platform
/// has them.
///
/// Only the `rwx` bits: setuid, setgid and sticky are not something a restore
/// should hand back to whoever restores the file.
#[cfg(unix)]
fn file_mode(store: &dyn sc_files::FileStore, path: &str) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let local = store.local_path(path).ok()??;
    let meta = std::fs::metadata(local).ok()?;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_store: &dyn sc_files::FileStore, _path: &str) -> Option<u32> {
    None
}

/// Every file in a store, depth-first, as store-relative paths with the size
/// the listing gave (where it gave one).
///
/// Directories are not recorded: an empty directory carries no information a
/// restore could not recreate, and every non-empty one is implied by the paths of
/// the files in it. A `node_modules` is not descended into
/// ([`super::is_installed_dependency`]).
async fn walk(store: &dyn sc_files::FileStore, dir: &str) -> Result<Vec<(String, Option<u64>)>> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in store.list(&dir).await? {
            if super::is_installed_dependency(&entry.path) {
                continue;
            }
            if entry.is_dir {
                pending.push(entry.path);
            } else {
                out.push((entry.path, entry.size));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Where a backup reads rows from: one snapshot on a backend that has one, the
/// database itself otherwise ([`sc_db::DatabaseDriver::begin_snapshot`]).
enum RowSource<'a> {
    Snapshot(Box<dyn Transaction>),
    Direct(&'a Catalog),
}

impl<'a> RowSource<'a> {
    async fn open(catalog: &'a Catalog) -> Result<RowSource<'a>> {
        Ok(match catalog.primary().begin_snapshot().await? {
            Some(tx) => RowSource::Snapshot(tx),
            None => RowSource::Direct(catalog),
        })
    }

    async fn query(&mut self, statement: &Statement) -> Result<Vec<Row>> {
        let rows = match self {
            RowSource::Snapshot(tx) => tx.query(statement).await?,
            RowSource::Direct(catalog) => catalog.primary().query(statement).await?,
        };
        rows.try_collect().await
    }

    /// End the snapshot. It wrote nothing, so there is nothing to commit, and a
    /// failure to end it is the connection's problem, not the backup's.
    async fn finish(self) {
        if let RowSource::Snapshot(tx) = self {
            let _ = tx.rollback().await;
        }
    }
}

/// The most rows one page of a table is read in.
const PAGE_ROWS: u64 = 1000;

/// How many rows the first page of a table is: one, because nothing is known
/// yet about how large a row is, and a first page of a thousand 40 MB rows is
/// the very thing paging is for. It costs one extra query per table.
const FIRST_PAGE_ROWS: u64 = 1;

/// The size one page of rows aims at, measured as the lines they become. The
/// Postgres driver holds a whole result in memory before handing back the
/// first row, so this — not the table — is what a backup's memory is bounded
/// by.
const PAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Write the rows of `table` matching `filter` as one NDJSON entry at `path`,
/// returning how many there were.
///
/// With a single primary key the rows are read in pages, by **keyset**: ordered
/// by the key, each page starting after the last key of the one before, which
/// costs the same however far into the table it is (an `OFFSET` would rescan
/// everything it skips) and is what makes two backups of an unchanged table the
/// same file. The page size adapts: it starts at [`FIRST_PAGE_ROWS`] and after
/// every page is rescaled so the next one comes to about [`PAGE_BYTES`] (at
/// most [`PAGE_ROWS`]), so a table of large blobs is read a few rows at a time
/// and a table of small rows a thousand at a time.
///
/// A table with no single key has no order to page by and is read with one
/// query, still written a row at a time.
///
/// `always` writes the entry even when there are no rows — a table's rows file,
/// which says "this table was empty" — rather than leaving it out, as a fit with
/// no draws does.
async fn write_table_rows<W: Write + Seek>(
    source: &mut RowSource<'_>,
    table: &Table,
    filter: Option<Expr>,
    zip: &mut ZipBuilder<W>,
    path: &str,
    always: bool,
) -> Result<i64> {
    let pk = rows::single_pk(table).ok();
    let mut limit = FIRST_PAGE_ROWS;
    let mut after: Option<Value> = None;
    let mut count: i64 = 0;
    let mut started = false;
    if always {
        zip.start_rows(path)?;
        started = true;
    }
    loop {
        let mut select =
            Select::from(Source::table(table.name.clone())).columns(vec![Projection::all()]);
        let mut condition = filter.clone();
        if let Some(pk) = &pk {
            if let Some(last) = &after {
                let past = Expr::binary(BinOp::Gt, Expr::col(pk.clone()), Expr::lit(last.clone()));
                condition = Some(match condition {
                    Some(filter) => Expr::binary(BinOp::And, filter, past),
                    None => past,
                });
            }
            select.order = vec![OrderBy::asc(Expr::col(pk.clone()))];
            select.limit = Some(limit);
        }
        select.filter = condition;
        let page = source.query(&Statement::from(select)).await?;
        if !started && !page.is_empty() {
            zip.start_rows(path)?;
            started = true;
        }
        let mut bytes: u64 = 0;
        for row in &page {
            bytes += zip.row(&values::row_to_json(row))?;
            count += 1;
        }
        let Some(pk) = &pk else {
            break;
        };
        let read = page.len() as u64;
        if read < limit {
            break;
        }
        after = match page.last().and_then(|row| row.get(pk)) {
            Some(Value::Null) | None => {
                return Err(Error::msg(format!(
                    "`{}` has a row with no `{pk}`, so its rows cannot be read in order",
                    table.name
                )));
            }
            Some(last) => Some(last.clone()),
        };
        limit = (PAGE_BYTES / (bytes / read).max(1)).clamp(1, PAGE_ROWS);
    }
    Ok(count)
}

/// The size one read of a file store's file copies.
const CHUNK: usize = 64 * 1024;

/// Copy one store file into the zip a chunk at a time, returning its length.
async fn copy_file<W: Write + Seek>(
    store: &dyn sc_files::FileStore,
    path: &str,
    size: Option<u64>,
    zip: &mut ZipBuilder<W>,
    entry: &str,
) -> Result<u64> {
    let mut reader = store.open_read(path, None).await?;
    zip.start_file(entry, path, size)?;
    let mut buffer = vec![0u8; CHUNK];
    let mut written = 0;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|e| Error::file(format!("reading `{path}` from its store: {e}")))?;
        if read == 0 {
            break;
        }
        zip.write_chunk(entry, &buffer[..read])?;
        written += read as u64;
    }
    Ok(written)
}

/// Whether a file's name says its bytes are compressed already — an image, a
/// recording, an archive, a web font, an office document (which is a zip).
///
/// Such a file is stored in the backup as it is: deflating a video costs CPU
/// for a few bytes at best. Everything else is deflated.
pub(super) fn already_compressed(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        // images
        "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "jxl",
        // audio and video
        "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "mp4", "m4v", "mov", "mkv", "webm",
        "avi", "wmv", "mpg", "mpeg", "3gp", // archives
        "zip", "gz", "tgz", "xz", "txz", "zst", "bz2", "7z", "rar", "br", "lz4",
        // fonts
        "woff", "woff2", // office formats, which are zips
        "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "jar", "apk",
    ];
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known)),
        _ => false,
    }
}

/// Past this size an entry is written with Zip64 sizes: a 32-bit size field
/// overflows at 4 GiB, and a writer that finds out half-way through cannot go
/// back. A little under, for the deflate stream's own overhead.
const ZIP64_FROM: u64 = 0xFFFF_0000;

/// A zip being written to a file.
///
/// Thin on purpose: it exists so the entry options are stated once and so the
/// callers above read as a list of what goes into a backup rather than as zip
/// plumbing.
struct ZipBuilder<W: Write + Seek> {
    zip: zip::ZipWriter<W>,
    /// One row's line, reused from row to row.
    line: Vec<u8>,
}

impl<W: Write + Seek> ZipBuilder<W> {
    fn new(out: W) -> ZipBuilder<W> {
        ZipBuilder {
            zip: zip::ZipWriter::new(out).set_auto_large_file(),
            line: Vec::new(),
        }
    }

    fn deflated() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
    }

    /// Add a JSON entry, pretty-printed: a backup is a thing people open and
    /// read, and `git diff` on an unpacked one is a real way to answer "what
    /// changed".
    fn json(&mut self, path: &str, value: &Json) -> Result<()> {
        self.zip
            .start_file(path, Self::deflated())
            .map_err(|e| zip_error(path, &e.to_string()))?;
        serde_json::to_writer_pretty(&mut self.zip, value)
            .map_err(|e| zip_error(path, &e.to_string()))
    }

    /// Start an NDJSON entry of rows. How long it will be is not known, so it
    /// is written with Zip64 sizes in case it passes 4 GiB.
    fn start_rows(&mut self, path: &str) -> Result<()> {
        self.zip
            .start_file(path, Self::deflated().large_file(true))
            .map_err(|e| zip_error(path, &e.to_string()))
    }

    /// One row as one compact line of the entry started last, returning the
    /// line's length. A UTF-8 string never contains a raw newline in JSON, so
    /// the newline is always the end of the row.
    fn row(&mut self, row: &Json) -> Result<u64> {
        self.line.clear();
        serde_json::to_writer(&mut self.line, row)
            .map_err(|e| zip_error("a row", &e.to_string()))?;
        self.line.push(b'\n');
        self.zip
            .write_all(&self.line)
            .map_err(|e| zip_error("a row", &e.to_string()))?;
        let length = self.line.len() as u64;
        // A page of large rows must not leave its largest line behind.
        if self.line.capacity() > 1024 * 1024 {
            self.line = Vec::new();
        }
        Ok(length)
    }

    /// Start the entry for a store file named `name`, of `size` bytes where
    /// the store said.
    fn start_file(&mut self, path: &str, name: &str, size: Option<u64>) -> Result<()> {
        let options = if already_compressed(name) {
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
        } else {
            Self::deflated()
        };
        let large = size.is_none_or(|size| size >= ZIP64_FROM);
        self.zip
            .start_file(path, options.large_file(large))
            .map_err(|e| zip_error(path, &e.to_string()))
    }

    fn write_chunk(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.zip
            .write_all(bytes)
            .map_err(|e| zip_error(path, &e.to_string()))
    }

    fn finish(self) -> Result<W> {
        self.zip
            .finish()
            .map_err(|e| zip_error("the archive", &e.to_string()))
    }
}

fn zip_error(path: &str, message: &str) -> Error {
    Error::msg(format!("could not write {path} into the backup: {message}"))
}

/// The LLM providers the admin added — not the one the configuration file
/// supplies, whose key is the operator's and stays in that file. An agent
/// naming it is restored onto an installation whose file supplies the same
/// name, and is reported unrestorable elsewhere, like any agent whose provider
/// is missing.
async fn stored_llm_providers(catalog: &Catalog) -> Result<Vec<sc_llm::LlmProviderDef>> {
    Ok(sc_llm::list_llm_providers(catalog)
        .await?
        .into_iter()
        .filter(|def| !sc_llm::is_host_llm_provider(catalog, def.id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::archive::Archive;
    use crate::backup::spool::SpoolFile;

    #[test]
    fn a_file_is_stored_when_its_name_says_it_is_compressed_already() {
        for path in [
            "clips/intro.mp4",
            "photos/B.JPG",
            "dist/app.tar.gz",
            "fonts/inter.woff2",
            "report.docx",
            "archive.7z",
        ] {
            assert!(already_compressed(path), "{path} is stored");
        }
        for path in [
            "notes.txt",
            "data.csv",
            "index.html",
            "mp4",
            "dir/.gz",
            "dir.zip/readme",
            "app.js",
        ] {
            assert!(!already_compressed(path), "{path} is deflated");
        }
    }

    /// The central directory's general-purpose flags of the entry `name`.
    fn flags_of(zip: &[u8], name: &str) -> u16 {
        let mut at = 0;
        while let Some(offset) = zip[at..].windows(4).position(|w| w == b"PK\x01\x02") {
            let header = at + offset;
            let flags = u16::from_le_bytes([zip[header + 8], zip[header + 9]]);
            let length = usize::from(u16::from_le_bytes([zip[header + 28], zip[header + 29]]));
            if &zip[header + 46..header + 46 + length] == name.as_bytes() {
                return flags;
            }
            at = header + 4;
        }
        panic!("no central directory entry for {name}");
    }

    #[tokio::test]
    async fn what_the_builder_writes_the_archive_reads_back() {
        const NOTE: &str = "file-stores/s/files/Ærø/日本.txt";
        const VIDEO: &str = "file-stores/s/files/clip.mp4";
        let spool = SpoolFile::new("backup");
        let mut zip = ZipBuilder::new(std::io::BufWriter::new(spool.create().unwrap()));
        zip.json("manifest.json", &json!({ "a": 1 })).unwrap();
        zip.start_rows("tables/t/rows.ndjson").unwrap();
        let mut lengths = 0;
        for i in 0..3 {
            lengths += zip.row(&json!({ "id": i, "name": "Grüße" })).unwrap();
        }
        zip.start_file(NOTE, "Ærø/日本.txt", Some(5)).unwrap();
        zip.write_chunk(NOTE, b"hello").unwrap();
        zip.start_file(VIDEO, "clip.mp4", None).unwrap();
        zip.write_chunk(VIDEO, b"not really a video").unwrap();
        zip.finish().unwrap().into_inner().unwrap();

        // A non-ASCII name is flagged as UTF-8, so every unzip reads it as written.
        let bytes = std::fs::read(spool.path()).unwrap();
        assert_ne!(flags_of(&bytes, NOTE) & 0x0800, 0, "the UTF-8 flag");
        assert_eq!(flags_of(&bytes, VIDEO) & 0x0800, 0, "ASCII needs no flag");

        let archive = Archive::open_zip(spool.path()).unwrap();
        assert_eq!(archive.json("manifest.json").unwrap()["a"], 1);
        assert_eq!(
            archive.names_under("file-stores/s/files/"),
            vec![VIDEO.to_owned(), NOTE.to_owned()]
        );
        let mut reader = archive.rows("tables/t/rows.ndjson");
        let batch = reader.next_batch().await.unwrap().unwrap();
        assert_eq!(batch.rows.len(), 3);
        assert_eq!(batch.rows[2], json!({ "id": 2, "name": "Grüße" }));
        assert_eq!(
            lengths,
            batch
                .rows
                .iter()
                .map(|r| r.to_string().len() as u64 + 1)
                .sum::<u64>()
        );
        assert!(reader.next_batch().await.is_none());

        let mut zip = zip::ZipArchive::new(std::fs::File::open(spool.path()).unwrap()).unwrap();
        assert_eq!(
            zip.by_name(VIDEO).unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        assert_eq!(
            zip.by_name(NOTE).unwrap().compression(),
            zip::CompressionMethod::Deflated
        );
    }
}
