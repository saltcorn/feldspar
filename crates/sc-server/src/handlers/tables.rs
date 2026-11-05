//! Tables: listing, creating (from scratch, a CSV or a geo file), renaming and dropping them, the
//! metadata tables, provided tables and orphaned table settings.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    reg.handle(sc_api::handler_ids::LIST_TABLES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let rls = catalog.primary_info().capabilities().row_level_security;
                let tables = catalog.tables()?;
                let out: Vec<Json> = tables
                    .iter()
                    .filter(|t| !t.is_hidden())
                    .map(|t| table_json(&catalog, t, rls, &ctx.locale))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("name")?.trim().to_owned();
                let database = optional_str(obj, "database");
                // One operation through the shared schema editor (§3.3): the
                // identifier check and the live re-projection are its, so the
                // admin API and an agent create a table the same way. With no
                // fields, this makes an empty table with **no primary key** —
                // the admin adds the key as a field, like any other (GOALS).
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::CreateTable {
                        name: name.clone(),
                        database,
                        settings: schema_edit::TableSettings::default(),
                        fields: Vec::new(),
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(HandlerResponse::ok(table_json(
                    &catalog,
                    &catalog.require(&name)?,
                    rls,
                    &ctx.locale,
                ))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_TABLE_FROM_CSV, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("name")?.trim().to_owned();
                let document = obj
                    .get("csv")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::invalid("`csv` must be the CSV document as text"))?
                    .to_owned();
                // The fields, the table and the rows are all `sc-api`'s: this
                // endpoint chooses nothing the CLI or an agent doing the same
                // thing would choose differently. A file the rows will not go
                // into leaves no table behind, which is why the failure here is
                // an ordinary error and not a half-made table plus a warning.
                let (table, outcome) = csv_rows::create_table_from_csv(
                    &catalog,
                    &name,
                    &optional_str(obj, "database"),
                    &document,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(HandlerResponse::ok(json!({
                    "table": table_json(&catalog, &table, rls, &ctx.locale),
                    "inserted": outcome.inserted,
                }))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_TABLE_FROM_GEO_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                use base64::Engine as _;
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("name")?.trim().to_owned();
                let file_name = obj.non_empty_str_field("file_name")?.to_owned();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(obj.str_field("content_base64")?.trim())
                    .map_err(|e| Error::invalid(format!("`content_base64` is not base64 ({e})")))?;
                let layer = optional_str(obj, "layer");
                let (table, outcome) = sc_api::geo_import::create_table_from_geo_file(
                    &catalog,
                    &name,
                    &optional_str(obj, "database"),
                    &file_name,
                    &bytes,
                    Some(layer.trim()).filter(|l| !l.is_empty()),
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(HandlerResponse::ok(json!({
                    "table": table_json(&catalog, &table, rls, &ctx.locale),
                    "inserted": outcome.inserted,
                    "warnings": outcome.warnings,
                }))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DROP_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // Deliberately **not** `deleteTableSettings`: that forgets a
                // configuration, this destroys the table, its columns and every
                // row in it. Both delete the overlay row; only this one issues
                // DDL, and only this one refuses by name what another table's
                // key still points at.
                let table = ctx.path_param("table")?.to_owned();
                // A **provided** table (§8.3) is deleted, not dropped: there is
                // no table in any database to issue DDL against, and its
                // `_fd_tables` row is its whole definition. One verb in the UI,
                // because "delete this table" is one thing an admin means.
                if catalog.get(&table)?.is_some_and(|t| t.provider().is_some()) {
                    sc_api::provided_tables::forget(&catalog, &table).await?;
                    return Ok(HandlerResponse::ok(json!({ "dropped": table })));
                }
                // A **metadata** table is Saltcorn's and is never dropped:
                // "delete" takes it off the tables list, and nothing else.
                if catalog.get(&table)?.is_some_and(|t| t.is_metadata()) {
                    sc_api::metadata_tables::remove(&catalog, &table).await?;
                    return Ok(HandlerResponse::ok(json!({ "dropped": table })));
                }
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::DropTable {
                        table: table.clone(),
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                Ok(HandlerResponse::ok(json!({ "dropped": table })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::RENAME_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = ctx.path_param("table")?.to_owned();
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("name")?.trim().to_owned();
                let applied = schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::RenameTable {
                        table,
                        to: name.clone(),
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(HandlerResponse::ok(json!({
                    "table": table_json(&catalog, &catalog.require(&name)?, rls, &ctx.locale),
                    "notes": applied.notes,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // The table must exist: this configures a table the admin is
                // looking at. A row for a table that is *not* there is an
                // orphan, which is cleaned up rather than edited — see
                // `deleteTableSettings`.
                let table = catalog.require(ctx.path_param("table")?)?;
                let obj = require_object(&ctx.body)?;

                // **Every** setting is passed, which is this endpoint's
                // whole-object contract (§13.1) rather than the schema editor's
                // omitted-means-leave default: the admin UI edits a table it has
                // loaded, so an omitted value here means the admin cleared it.
                // `alter_table` in an agent's batch is the other case, and the
                // difference is spelled at exactly these two call sites.
                let settings = table_settings_from_body(obj)?;
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::AlterTable {
                        table: table.name.clone(),
                        settings,
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(HandlerResponse::ok(table_json(
                    &catalog,
                    &catalog.require(&table.name)?,
                    rls,
                    &ctx.locale,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_METADATA_TABLES, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let names = sc_api::metadata_tables::available(&catalog)?;
                Ok(HandlerResponse::ok(json!(names)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_METADATA_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("name")?.to_owned();
                // No DDL and no settings: the table is already there, and this
                // writes the row that puts it in the list.
                let table = sc_api::metadata_tables::add(&catalog, &name).await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(
                    HandlerResponse::ok(table_json(&catalog, &table, rls, &ctx.locale))
                        .with_status(201),
                )
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_TABLE_PROVIDERS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // A property of the **installed modules**, not of any table, and
                // the empty list is the ordinary answer: a server with no
                // modules supplies no providers, and the New table dialog offers
                // the option only when there is one.
                let out: Vec<Json> = catalog
                    .table_provider_kinds()
                    .iter()
                    .map(|kind| {
                        json!({
                            "module": kind.module,
                            "provider": kind.provider,
                            "config_spec": spec_json(&kind.config_spec, &ctx.locale),
                        })
                    })
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_PROVIDED_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let name = obj.non_empty_str_field("name")?.trim().to_owned();
                let module = obj.non_empty_str_field("module")?.to_owned();
                let provider = obj.non_empty_str_field("provider")?.to_owned();
                // No DDL: this writes the definition row and reloads, which is
                // what makes the table exist and what asks the module for its
                // columns.
                let table = sc_api::provided_tables::create(
                    &catalog,
                    &name,
                    &module,
                    &provider,
                    object_field(obj, "configuration")?,
                )
                .await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(
                    HandlerResponse::ok(table_json(&catalog, &table, rls, &ctx.locale))
                        .with_status(201),
                )
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_PROVIDED_TABLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("table")?.to_owned();
                let table = catalog.require(&name)?;
                let obj = require_object(&ctx.body)?;
                let submitted = object_field(obj, "configuration")?;
                // A provider's `password` setting is a secret like any other:
                // the form was shown the sentinel, and submitting it back
                // unchanged keeps what is stored — the same merge a module's own
                // settings go through, against the spec the provider declared.
                let spec = table
                    .provider()
                    .and_then(|(module, provider)| {
                        catalog
                            .table_provider_kinds()
                            .into_iter()
                            .find(|k| k.module == module && k.provider == provider)
                    })
                    .map(|kind| kind.config_spec)
                    .unwrap_or_default();
                let stored = match table.attributes.get(sc_catalog::ATTR_PROVIDER_CONFIG) {
                    Some(Json::Object(map)) => map.clone(),
                    _ => sc_types::Attrs::new(),
                };
                let configuration = sc_types::merge_secrets(&spec, &stored, &submitted);
                let table =
                    sc_api::provided_tables::configure(&catalog, &name, configuration).await?;
                let rls = catalog.primary_info().capabilities().row_level_security;
                Ok(HandlerResponse::ok(table_json(
                    &catalog,
                    &table,
                    rls,
                    &ctx.locale,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_TABLE_SETTINGS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // Addressed by name and looked up in the *rows*, not the
                // catalog: this is also how an orphan is cleaned up, and an
                // orphan by definition has no table to look up. Dropping the
                // policies a forgotten `rls_enabled` had turned on is the schema
                // editor's rule, not this handler's.
                let name = ctx.path_param("table")?.to_owned();
                let deleted = schema_edit::forget_table_settings(&catalog, &name).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_ORPHAN_TABLE_SETTINGS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let out: Vec<Json> = orphan_table_meta(&catalog)
                    .await?
                    .iter()
                    .map(|meta| {
                        json!({
                            "name": meta.table_name,
                            "label": meta.label,
                            "description": meta.description,
                            "min_role_read": meta.access.min_role_read,
                            "min_role_write": meta.access.min_role_write,
                            "ownership_formula": meta.ownership_formula().unwrap_or(""),
                            "rls_enabled": meta.rls_enabled(),
                            "sync_enabled": sc_catalog::sync_enabled(&meta.attributes),
                            "history_enabled": sc_catalog::history_enabled(&meta.attributes),
                            "history_min_role": sc_catalog::history::history_min_role(&meta.attributes),
                            "history_keep_days": sc_catalog::retention::history_keep_days(&meta.attributes),
                        })
                    })
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });
}
