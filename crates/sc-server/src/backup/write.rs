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
//! - **It does not stream.** The zip is built in memory and handed to the
//!   response whole. A backup is bounded by the installation's own size and the
//!   admin asked for it; streaming would mean deciding what to do about a table
//!   changing underneath a half-written file, which is a bigger question than the
//!   memory it saves.

use std::io::{Cursor, Write};

use sc_api::rows;
use sc_catalog::{Catalog, FIELD_META_TABLE, Table, list_field_meta_for_table, list_file_stores};
use sc_error::{Error, Result};
use sc_query::{Expr, OrderBy, Projection, Select, Source, Statement};
use serde_json::{Map, Value as Json, json};
use zip::write::SimpleFileOptions;

use super::{Available, Item, MANIFEST_FILE, SSL_SECTION, Selection};
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
/// the roles they point at. Offering it twice would let an admin tick "users" and
/// untick the `users` table and get something incoherent.
///
/// Tables on another database connection are not among them either: the
/// connection is.
pub async fn available(catalog: &Catalog) -> Result<Available> {
    let mut tables = Vec::new();
    for table in catalog.tables()? {
        if table.is_system() || table.name == sc_auth::USERS_TABLE {
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
            count(sc_llm::list_llm_providers(catalog).await?.len())
        } else {
            0
        },
        agents: i64::try_from(sc_agent::list_agents(catalog).await?.len()).unwrap_or(i64::MAX),
        triggers: i64::try_from(sc_action::list_triggers(catalog).await?.len()).unwrap_or(i64::MAX),
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

/// Build the zip for `selection`.
///
/// The bytes are the whole answer: what went into them is *in* them, in the
/// manifest, which is the same thing a restore reads back with
/// [`inspect`](super::inspect). A second copy of that record travelling beside the
/// file would be a second copy to keep true.
pub async fn write_backup(catalog: &Catalog, selection: &Selection) -> Result<Vec<u8>> {
    let mut zip = ZipBuilder::new();
    let mut contents = Available::default();

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
            let rows = table_rows(catalog, &table).await?;
            count = Some(i64::try_from(rows.len()).unwrap_or(i64::MAX));
            zip.json(&format!("tables/{name}/rows.json"), &Json::Array(rows))?;
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
            for path in walk(store.as_ref(), "").await? {
                let bytes = store.read(&path).await?;
                let meta = store.get_meta(&path).await.unwrap_or_default();
                zip.bytes(&format!("{dir}/files/{path}"), &bytes)?;
                let mode = file_mode(store.as_ref(), &path);
                files.push(backup_file_meta_json(&path, &meta, mode));
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
    // One entry, because a user without their role is a foreign key pointing at
    // nothing. The rows are the table's own — **including `password_hash`**,
    // which is the difference between a restored installation people can sign in
    // to and one where every account needs a new password. A hash is what the
    // database holds and what a restore has to put back.
    if selection.users {
        let users_table = catalog.require(sc_auth::USERS_TABLE)?;
        let users = table_rows(catalog, &users_table).await?;
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
        contents.users = i64::try_from(users.len()).unwrap_or(i64::MAX);
        zip.json(
            "users.json",
            &json!({ "roles": roles, "fields": fields, "users": Json::Array(users) }),
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
        write_analytics(catalog, selection, &mut zip, &mut contents).await?;
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
        for def in sc_llm::list_llm_providers(catalog).await? {
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
            for field in &section.fields {
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

    // The manifest goes in last so it can describe what was actually written —
    // a store that turned out to be unreachable, a table whose rows were left
    // out — rather than what was asked for.
    zip.json(
        MANIFEST_FILE,
        &json!({
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

    zip.finish()
}

/// Datasets, models and workspaces — and, when chosen, the models' fits.
///
/// Datasets are written in the order a restore can save them in, every base
/// before the datasets built on it, so the restore does not have to sort.
async fn write_analytics(
    catalog: &Catalog,
    selection: &Selection,
    zip: &mut ZipBuilder,
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
                    instances
                        .extend(rows_of(catalog, sc_model::INSTANCES_TABLE, "id", fit.id.0).await?);
                    outputs.extend(
                        rows_of(catalog, sc_model::OUTPUTS_TABLE, "instance", fit.id.0).await?,
                    );
                    // One entry per fit, because a posterior's draws are the
                    // one thing here that can be large: a restore reads them a
                    // fit at a time rather than as one document.
                    let draws =
                        rows_of(catalog, sc_model::DRAWS_TABLE, "instance", fit.id.0).await?;
                    if !draws.is_empty() {
                        zip.json(
                            &format!("analytics/draws/{}.json", fit.id.0),
                            &Json::Array(draws),
                        )?;
                    }
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

/// The rows of a system table whose `column` is `id`, as JSON — the fits'
/// rows, which have no admin-API shape to borrow.
async fn rows_of(
    catalog: &Catalog,
    table: &str,
    column: &str,
    id: uuid::Uuid,
) -> Result<Vec<Json>> {
    let select = Select::from(Source::table(table.to_owned()))
        .columns(vec![Projection::all()])
        .filter(Expr::col(column).eq(Expr::lit(id)));
    let fetched = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(fetched.iter().map(rows::row_to_json).collect())
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

/// Every row of a table as JSON, ordered by its primary key where it has a
/// single one so two backups of an unchanged table are the same file.
async fn table_rows(catalog: &Catalog, table: &Table) -> Result<Vec<Json>> {
    let mut select =
        Select::from(Source::table(table.name.clone())).columns(vec![Projection::all()]);
    if let Ok(pk) = rows::single_pk(table) {
        select.order = vec![OrderBy::asc(Expr::col(pk))];
    }
    let fetched = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(fetched.iter().map(rows::row_to_json).collect())
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

/// Every file in a store, depth-first, as store-relative paths.
///
/// Directories are not recorded: an empty directory carries no information a
/// restore could not recreate, and every non-empty one is implied by the paths of
/// the files in it. A `node_modules` is not descended into
/// ([`super::is_installed_dependency`]).
async fn walk(store: &dyn sc_files::FileStore, dir: &str) -> Result<Vec<String>> {
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
                out.push(entry.path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A zip being built in memory.
///
/// Thin on purpose: it exists so the entry options are stated once and so the
/// callers above read as a list of what goes into a backup rather than as zip
/// plumbing.
struct ZipBuilder {
    zip: zip::ZipWriter<Cursor<Vec<u8>>>,
    options: SimpleFileOptions,
}

impl ZipBuilder {
    fn new() -> ZipBuilder {
        ZipBuilder {
            zip: zip::ZipWriter::new(Cursor::new(Vec::new())),
            options: SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        }
    }

    /// Add a JSON entry, pretty-printed: a backup is a thing people open and
    /// read, and `git diff` on an unpacked one is a real way to answer "what
    /// changed".
    fn json(&mut self, path: &str, value: &Json) -> Result<()> {
        let text = serde_json::to_vec_pretty(value)
            .map_err(|e| Error::msg(format!("could not serialise {path}: {e}")))?;
        self.bytes(path, &text)
    }

    fn bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.zip
            .start_file(path, self.options)
            .map_err(|e| zip_error(path, &e.to_string()))?;
        self.zip
            .write_all(bytes)
            .map_err(|e| zip_error(path, &e.to_string()))?;
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>> {
        Ok(self
            .zip
            .finish()
            .map_err(|e| zip_error("the archive", &e.to_string()))?
            .into_inner())
    }
}

fn zip_error(path: &str, message: &str) -> Error {
    Error::msg(format!("could not write {path} into the backup: {message}"))
}
