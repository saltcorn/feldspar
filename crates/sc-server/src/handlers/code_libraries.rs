//! Code libraries a code body may import.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    reg.register_admin("listCodeLibraries", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let listed = sc_app::describe_code_libraries(&catalog).await?;
                Ok(HandlerResponse::ok(Json::Array(listed)))
            }
        }
    });

    reg.register_admin("createCodeLibrary", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let library = sc_app::code_library_from_json(uuid::Uuid::new_v4(), &ctx.body)?;
                sc_app::save_code_library_checked(&catalog, &library).await?;
                Ok(
                    HandlerResponse::ok(code_library_answer(&catalog, &library).await?)
                        .with_status(201),
                )
            }
        }
    });

    reg.register_admin("updateCodeLibrary", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_code_library_id(ctx.path_param("id")?)?;
                if sc_catalog::load_code_library(&catalog, id).await?.is_none() {
                    return Err(Error::not_found(format!("no code library with id {id}")));
                }
                // The id is the path's, not the body's.
                let library = sc_app::code_library_from_json(id, &ctx.body)?;
                sc_app::save_code_library_checked(&catalog, &library).await?;
                Ok(HandlerResponse::ok(
                    code_library_answer(&catalog, &library).await?,
                ))
            }
        }
    });

    reg.register_admin("deleteCodeLibrary", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_code_library_id(ctx.path_param("id")?)?;
                if !sc_catalog::delete_code_library(&catalog, id).await? {
                    return Err(Error::not_found(format!("no code library with id {id}")));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });
}

/// A code library id from the path.
fn parse_code_library_id(raw: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a code library id")))
}

/// A library just saved, as the list shows it — with its application's
/// subdomain and who uses it.
async fn code_library_answer(catalog: &Catalog, library: &sc_catalog::CodeLibrary) -> Result<Json> {
    let listed = sc_app::describe_code_libraries(catalog).await?;
    Ok(listed
        .into_iter()
        .find(|l| l["id"] == json!(library.id))
        .unwrap_or_else(|| sc_app::code_library_json(library, None, None)))
}
