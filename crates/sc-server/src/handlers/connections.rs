//! Secondary database connections.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    // The other databases whose tables share the tables list. Every response
    // goes through `db_connection_json`, which is where the password becomes the
    // sentinel — the same discipline `llm_provider_json` keeps, and for the same
    // reason: an endpoint added later would have to build the JSON by hand to
    // leak one.

    reg.handle(sc_api::handler_ids::LIST_DATABASE_CONNECTIONS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let defs = list_db_connections(&catalog).await?;
                let out: Vec<Json> = defs
                    .iter()
                    .map(|def| db_connection_json(&catalog, def))
                    .collect::<Result<_>>()?;
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_DATABASE_CONNECTION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let def = db_connection_from_body(DbConnectionId::new(), &ctx.body, None)?;
                // Checked before dialling so a clashing name fails fast, and
                // dialled before saving so a create that could never work leaves
                // no row behind — the same order, for the same reason, as
                // `createFileStore`.
                check_db_connection_saveable(&catalog, &def).await?;
                save_db_connection(&catalog, &def).await?;
                // A failure here is deliberately *not* an error: the connection
                // was created, and the reason travels in the response so the UI
                // can show "saved, but not connected: <why>".
                let _ = connect_db_connection(&catalog, &def).await;
                catalog.reload().await?;
                Ok(HandlerResponse::ok(db_connection_json(&catalog, &def)?).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_DATABASE_CONNECTION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_db_connection_id(ctx.path_param("id")?)?;
                let existing = load_db_connection(&catalog, id).await?.ok_or_else(|| {
                    Error::not_found(format!("no database connection with id {id:?}"))
                })?;
                // The id is the path's, not the body's. A password sent back as
                // the sentinel is the sentinel the form was shown, not the
                // password, so what is stored is restored (§11.1).
                let def = db_connection_from_body(id, &ctx.body, Some(&existing))?;
                // Attributes are server-managed and the edit form neither shows
                // nor sends them, so they are carried across rather than reset.
                let def = DbConnectionDef {
                    attributes: existing.attributes.clone(),
                    ..def
                };
                save_db_connection(&catalog, &def).await?;

                // A rename leaves the old driver registered under the old name,
                // still contributing tables stamped with a connection that no
                // longer exists. This is the one place that knows both the
                // before and the after, so it is where the old one goes.
                if existing.name != def.name {
                    catalog.disconnect_database(&existing.name)?;
                }
                // Re-dial under the current settings so an edited host takes
                // effect immediately, with no restart.
                catalog.disconnect_database(&def.name)?;
                let _ = connect_db_connection(&catalog, &def).await;
                catalog.reload().await?;
                Ok(HandlerResponse::ok(db_connection_json(&catalog, &def)?))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_DATABASE_CONNECTION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_db_connection_id(ctx.path_param("id")?)?;
                let existing = load_db_connection(&catalog, id).await?.ok_or_else(|| {
                    Error::not_found(format!("no database connection with id {id:?}"))
                })?;
                let deleted = delete_db_connection(&catalog, id).await?;
                // Removing the row is the definition's end; disconnecting and
                // reloading is what takes its tables out of the catalog. Both
                // are needed, or the tables list would go on offering tables
                // from a connection the admin had just removed.
                if deleted {
                    catalog.disconnect_database(&existing.name)?;
                    catalog.reload().await?;
                }
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::TEST_DATABASE_CONNECTION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // A test of an *existing* connection may carry the sentinel
                // instead of the password — the form shows the sentinel — so the
                // stored one is looked up by name and merged back in. Without
                // this, Test would fail on every connection the admin had not
                // retyped the password for.
                let name = require_object(&ctx.body)?
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                let existing = if name.is_empty() {
                    None
                } else {
                    load_db_connection_by_name(&catalog, &name).await?
                };
                let def =
                    db_connection_from_body(DbConnectionId::new(), &ctx.body, existing.as_ref())?;
                Ok(HandlerResponse::ok(
                    match sc_catalog::dial(&catalog, &def).await {
                        Ok(driver) => json!({
                            "connected": true,
                            "error": Json::Null,
                            "tables": driver.introspect().await?.len(),
                        }),
                        // A failed test is an answer, not a request that went wrong:
                        // 200 with the reason, so the form shows it beside the
                        // button rather than as a red banner about the API.
                        Err(e) => json!({
                            "connected": false,
                            "error": sc_error::format_causes(&e),
                            "tables": 0,
                        }),
                    },
                ))
            }
        }
    });
}

/// A database connection as the API returns it (matching
/// `db_connection_schema`), with the password replaced by the sentinel and the
/// live state the running server knows added.
///
/// **The redaction is here and nowhere else**, for the reason
/// [`llm_provider_json`] keeps its: every response carrying a connection goes
/// through this, so an endpoint added later cannot return a password without
/// building the JSON by hand.
fn db_connection_json(catalog: &Catalog, def: &DbConnectionDef) -> Result<Json> {
    let connected = catalog.is_connected(&def.name)?;
    let error = catalog.database_error(&def.name)?;
    // How many of the catalog's tables this connection actually contributed —
    // the number that answers "did it work", where `connected` only answers "did
    // it dial".
    let tables = catalog
        .tables()?
        .iter()
        .filter(|t| t.database.0 == def.name)
        .count();
    Ok(json!({
        "id": def.id.0,
        "backend": def.backend,
        "name": def.name,
        "description": def.description,
        "host": def.host,
        "port": def.port,
        "database": def.database,
        "username": def.username,
        // Never the stored password. An empty one stays empty rather than
        // becoming the sentinel: "no password" is a real state, and showing dots
        // for it would tell the admin a password exists that does not.
        "password": if def.password.is_empty() { "" } else { SECRET_SENTINEL },
        "schema": def.schema,
        "file_store": def.file_store,
        "file_path": def.file_path,
        "connected": connected,
        "error": error,
        "tables": tables,
        "shadowed": catalog.shadowed_tables(&def.name)?,
    }))
}

/// Parse a database connection id from a path segment.
fn parse_db_connection_id(raw: &str) -> Result<DbConnectionId> {
    uuid::Uuid::parse_str(raw)
        .map(DbConnectionId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid database connection id")))
}
