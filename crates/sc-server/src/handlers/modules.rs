//! Saltcorn v1 JavaScript modules.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // A module is somebody else's npm package, so every one of these handlers
    // does the same two-part act: change the disk or the row, then **reload** —
    // rebuild the action registry from the built-ins plus every installed
    // module, swap it into the dispatcher, and revalidate the triggers against
    // it (`ModuleServices::reload`). That is what makes a module's actions
    // usable on a running server, which is the whole point of installing one
    // from a form rather than from a deploy.

    reg.handle(sc_api::handler_ids::LIST_MODULES, {
        let apps = apps.clone();
        move |ctx| {
            let apps = apps.clone();
            async move {
                let services = modules_of(&apps)?;
                Ok(HandlerResponse::ok(
                    list_modules_json(&services, &ctx.locale).await,
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::INSTALL_MODULE, {
        let apps = apps.clone();
        move |ctx| {
            let apps = apps.clone();
            async move {
                let services = modules_of(&apps)?;
                let installed = install_module_json(&services, &ctx.body, &ctx.locale).await?;
                Ok(HandlerResponse::ok(installed).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_MODULE, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let services = modules_of(&apps)?;
                let id = parse_module_id(ctx.path_param("id")?)?;
                let mut module = sc_module::require_module(&catalog, id).await?;
                let obj = require_object(&ctx.body)?;

                // Two independent edits behind one endpoint, and each takes
                // effect only when it was **sent**: the settings form and the
                // permissions form are separate saves, and a save of one must
                // not clear the other.
                if let Some(submitted) = obj.get("configuration").filter(|v| !v.is_null()) {
                    let Json::Object(submitted) = submitted.clone() else {
                        return Err(Error::invalid("field `configuration` must be an object"));
                    };
                    // A module's `password` setting is a secret like any other:
                    // what the form was shown is the sentinel, and submitting it
                    // back unchanged keeps what is stored.
                    let set = services.modules();
                    module.configuration = match set.get(&module.name) {
                        Some(loaded) => sc_types::merge_secrets(
                            &loaded.config_spec,
                            &module.configuration,
                            &submitted,
                        ),
                        None => submitted,
                    };
                    // Not the full spec check (a v1 module's settings are not
                    // held to it), but a pattern the module declares is: a
                    // malformed key is refused in front of the form.
                    if let Some(loaded) = set.get(&module.name) {
                        sc_types::validate_patterns(&loaded.config_spec, &module.configuration)?;
                    }
                }
                if let Some(submitted) = obj.get("permissions").filter(|v| !v.is_null()) {
                    // Checked here rather than at the worker: a permission an
                    // admin typed wrong should be refused in front of the form,
                    // not discovered as a module that stopped working.
                    module.permissions = sc_module::ModulePermissions::from_json(submitted)?;
                }
                // The configuration is what v1's `actions(cfg)` is called with,
                // so it only takes effect when the module is loaded again — and
                // a permission change moves the module onto a worker built with
                // the new set, which the same reload does.
                services.update_module(&module).await?;
                let set = services.modules();
                let loaded = set
                    .get(&module.name)
                    .ok_or_else(|| Error::msg("the saved module is not in the loaded set"))?;
                let statuses = services.service_statuses(&module.name);
                Ok(HandlerResponse::ok(module_json(
                    loaded,
                    &statuses,
                    &ctx.locale,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_MODULE, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let services = modules_of(&apps)?;
                let id = parse_module_id(ctx.path_param("id")?)?;
                let Some(module) = load_module(&catalog, id).await? else {
                    return Ok(HandlerResponse::ok(json!({ "deleted": false })));
                };
                let deleted = services.remove_module(&module).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::RELOAD_MODULES, {
        let apps = apps.clone();
        move |_ctx| {
            let apps = apps.clone();
            async move {
                let services = modules_of(&apps)?;
                services.reload().await?;
                let count = services.modules().modules().len();
                Ok(HandlerResponse::ok(json!({ "modules": count })))
            }
        }
    });
}

/// Parse a module id from a path parameter.
fn parse_module_id(raw: &str) -> Result<sc_module::ModuleId> {
    uuid::Uuid::parse_str(raw)
        .map(sc_module::ModuleId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid module id")))
}
