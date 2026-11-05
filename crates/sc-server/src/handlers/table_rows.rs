//! Rows: CRUD, row history, restore and undo, and CSV export and import.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // A page of rows, filtered and ordered by the query string — the same
    // vocabulary an application's REST read takes, parsed by the same code
    // (`sc_api::query_string`). The admin grid sends `order`, `limit`, `offset`
    // and one `column=op.value` pair per box its filter row has typed in.
    reg.handle(sc_api::handler_ids::LIST_ROWS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let query =
                    sc_api::query_string::row_query(&table, &ctx.query, sc_api::ROW_PAGE_CAP)?;
                let mut rows = rows::list_rows_query(
                    &catalog,
                    &table,
                    &query,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                // The data editor is a client like any other: it may write a
                // secret, and never reads one back (TODO-imap M2).
                sc_catalog::secrets::redact_json(&table, &mut rows);
                Ok(HandlerResponse::ok(rows))
            }
        }
    });

    reg.handle(sc_api::handler_ids::COUNT_ROWS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                // The count is of the rows the *same* filters leave: the grid
                // sizes its scroller by this and pages through `listRows`, so a
                // count that ignored the filter row would leave the last screen
                // of a filtered table empty.
                let filter = sc_api::query_string::filter_predicate(&table, &ctx.query)?;
                let count = rows::count_rows_where(
                    &catalog,
                    &table,
                    filter,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                Ok(HandlerResponse::ok(json!({ "count": count })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_ROW, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let row = rows::create_row(
                    &catalog,
                    &table,
                    &ctx.body,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                sc_api::metadata_tables::after_row_write(&catalog, &table).await?;
                let mut row = row;
                sc_catalog::secrets::redact_json(&table, &mut row);
                Ok(HandlerResponse::ok(row).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_ROW, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let id = ctx.path_param("id")?;
                let row = rows::update_row(
                    &catalog,
                    &table,
                    id,
                    &ctx.body,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                sc_api::metadata_tables::after_row_write(&catalog, &table).await?;
                let mut row = row;
                sc_catalog::secrets::redact_json(&table, &mut row);
                Ok(HandlerResponse::ok(row))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_ROW, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let id = ctx.path_param("id")?;
                // The row layer hands back the row it removed; the admin API
                // acknowledges, as it does for every other delete it serves.
                rows::delete_row(
                    &catalog,
                    &table,
                    id,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                sc_api::metadata_tables::after_row_write(&catalog, &table).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_ALL_ROWS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let deleted = rows::delete_all_rows(
                    &catalog,
                    &table,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                if deleted > 0 {
                    sc_api::metadata_tables::after_row_write(&catalog, &table).await?;
                }
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    //
    // Admin-only routes, read through `sc_api::history` all the same, so the
    // table must keep history and secrets are the sentinel here as everywhere.

    reg.handle(sc_api::handler_ids::GET_ROW_HISTORY, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let evaluator = apps.evaluator();
                let reader = HistoryReader::new(&catalog, ctx.user(), evaluator.as_ref());
                let out = reader
                    .row_history(ctx.path_param("table")?, ctx.path_param("id")?)
                    .await?;
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_DELETED_ROWS, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let evaluator = apps.evaluator();
                let reader = HistoryReader::new(&catalog, ctx.user(), evaluator.as_ref());
                let limit = optional_query_u64(&ctx, "limit")?;
                let offset = optional_query_u64(&ctx, "offset")?.unwrap_or(0);
                let out = reader
                    .deleted_rows(ctx.path_param("table")?, limit, offset)
                    .await?;
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_ROW_AS_OF, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let evaluator = apps.evaluator();
                let reader = HistoryReader::new(&catalog, ctx.user(), evaluator.as_ref());
                let as_of = ctx.query_get("as_of").ok_or_else(|| {
                    Error::invalid("`as_of` is required: a time or an entry's number")
                })?;
                let out = reader
                    .row_as_of(ctx.path_param("table")?, ctx.path_param("id")?, as_of)
                    .await?;
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_CHANGESETS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let reader = HistoryReader::new(&catalog, ctx.user(), None);
                let filter = sc_api::history::changeset_filter(&ctx.query)?;
                Ok(HandlerResponse::ok(reader.list_changesets(filter).await?))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_CHANGESET, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let reader = HistoryReader::new(&catalog, ctx.user(), None);
                Ok(HandlerResponse::ok(
                    reader.changeset(ctx.path_param("id")?).await?,
                ))
            }
        }
    });

    //
    // Writes as the caller through `sc_api::undo`, each in one transaction and
    // a changeset of its own.

    reg.handle(sc_api::handler_ids::RESTORE_ROW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let evaluator = apps.evaluator();
                let reverser = Reverser::new(&catalog, ctx.user(), evaluator.as_ref());
                let restored = reverser
                    .restore_row(ctx.path_param("table")?, ctx.path_param("id")?)
                    .await?;
                Ok(HandlerResponse::ok(restored.to_json()))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REVERT_ROW, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let evaluator = apps.evaluator();
                let reverser = Reverser::new(&catalog, ctx.user(), evaluator.as_ref());
                let version = int_field(require_object(&ctx.body)?, "version")?;
                let restored = reverser
                    .revert_row(ctx.path_param("table")?, ctx.path_param("id")?, version)
                    .await?;
                Ok(HandlerResponse::ok(restored.to_json()))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UNDO_CHANGESET, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let evaluator = apps.evaluator();
                let reverser = Reverser::new(&catalog, ctx.user(), evaluator.as_ref());
                let id = ctx.path_param("id")?;
                let id: uuid::Uuid = id
                    .parse()
                    .map_err(|_| Error::not_found(format!("there is no changeset `{id}`")))?;
                // Both fields are optional, so an empty body is an undo of all.
                let empty = Map::new();
                let obj = match &ctx.body {
                    Json::Null => &empty,
                    body => require_object(body)?,
                };
                let table = match obj.get("table") {
                    None | Some(Json::Null) => None,
                    Some(Json::String(t)) if t.is_empty() => None,
                    Some(Json::String(t)) => Some(t.as_str()),
                    Some(_) => return Err(Error::invalid("`table` must be a table's name")),
                };
                let force = optional_bool(obj, "force")?;
                let outcome = reverser.undo_changeset(id, table, force).await?;
                Ok(HandlerResponse::ok(outcome.to_json()))
            }
        }
    });

    reg.handle(sc_api::handler_ids::EXPORT_TABLE_CSV, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let document = csv_rows::export_table(
                    &catalog,
                    &table,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                // The browser saves the file, so the server says what to call
                // it: the table's own name is the only thing that makes three
                // downloads distinguishable in a downloads folder.
                Ok(HandlerResponse::ok(json!({
                    "filename": format!("{}.csv", table.name),
                    "csv": document,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::IMPORT_TABLE_CSV, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                let document = require_object(&ctx.body)?
                    .get("csv")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::invalid("`csv` must be the CSV document as text"))?
                    .to_owned();
                let outcome = csv_rows::import_table(
                    &catalog,
                    &table,
                    &document,
                    sc_access::admin_access(&admin_caller(&ctx.who)),
                )
                .await?;
                // An import is row by row, so any row that went in counts.
                if outcome.inserted + outcome.updated > 0 {
                    sc_api::metadata_tables::after_row_write(&catalog, &table).await?;
                }
                Ok(HandlerResponse::ok(json!({
                    "inserted": outcome.inserted,
                    "updated": outcome.updated,
                    "errors": outcome.errors,
                })))
            }
        }
    });
}

/// An optional whole-number query parameter; absent or empty is `None`.
fn optional_query_u64<W>(ctx: &crate::HandlerCtx<W>, name: &str) -> Result<Option<u64>> {
    match ctx.query_get(name) {
        None | Some("") => Ok(None),
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| Error::invalid(format!("`{name}` must be a whole number"))),
    }
}
