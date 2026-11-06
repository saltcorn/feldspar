//! Translations of an application and of the instance.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    //
    // The screen is one table, so the read is one call: the keys the source
    // actually uses, what each enabled locale has for them, what the lint found
    // that nobody wrapped, and the orphans. Every write goes through the
    // `CatalogStore` — files for an application with a project tree, rows for
    // one without — and then drops the mount's cached catalogues, so the
    // running application serves the change on the next reload with no build
    // (D7).

    reg.handle(
        sc_api::handler_ids::GET_TRANSLATIONS,
        typed(state, get_translations),
    );

    reg.handle(sc_api::handler_ids::SET_APPLICATION_LOCALES, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let tags: Vec<String> = match obj.get("locales") {
                    Some(Json::Array(items)) => items
                        .iter()
                        .map(|v| {
                            v.as_str().map(str::to_owned).ok_or_else(|| {
                                Error::invalid("`locales` should be a list of locale tags")
                            })
                        })
                        .collect::<Result<_>>()?,
                    _ => {
                        return Err(Error::invalid("`locales` should be a list of locale tags"));
                    }
                };
                let default = obj.get("default_locale").and_then(Json::as_str);
                Ok(HandlerResponse::ok(
                    crate::translations::set_locales(&catalog, &apps, id, &tags, default).await?,
                ))
            }
        }
    });

    reg.handle(
        sc_api::handler_ids::SAVE_TRANSLATIONS,
        typed(state, save_translations),
    );

    reg.handle(sc_api::handler_ids::TRANSLATE_MISSING, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let locale = sc_i18n::Locale::parse(ctx.path_param("locale")?)?;
                let translator = crate::translations::configured_translator(&catalog, None).await?;
                Ok(HandlerResponse::ok(
                    crate::translations::fill_missing(&catalog, &apps, &app, &locale, &translator)
                        .await?,
                ))
            }
        }
    });

    //
    // A save or fill drops every mount's cached catalogues, because every
    // application serves the shared catalogue under its own.

    reg.handle(
        sc_api::handler_ids::GET_INSTANCE_TRANSLATIONS,
        typed(state, get_instance_translations),
    );

    reg.handle(
        sc_api::handler_ids::SAVE_INSTANCE_TRANSLATIONS,
        typed(state, save_instance_translations),
    );

    reg.handle(sc_api::handler_ids::TRANSLATE_INSTANCE_MISSING, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let locale = sc_i18n::Locale::parse(ctx.path_param("locale")?)?;
                let translator = crate::translations::configured_translator(&catalog, None).await?;
                Ok(HandlerResponse::ok(
                    crate::translations::fill_instance_missing(
                        &catalog,
                        Some(&apps),
                        &locale,
                        &translator,
                    )
                    .await?,
                ))
            }
        }
    });
}

/// The `getTranslations` endpoint.
pub(crate) async fn get_translations(
    state: HandlerState,
    ctx: HandlerCtx<Admin>,
    _body: Json,
) -> Result<HandlerResponse> {
    let catalog = state.catalog;
    let app = require_app(&catalog, ctx.path_param("id")?).await?;
    Ok(HandlerResponse::ok(
        crate::translations::translations_json(&catalog, &app).await?,
    ))
}

/// The `saveTranslations` endpoint.
pub(crate) async fn save_translations(
    state: HandlerState,
    ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    let app = require_app(&catalog, ctx.path_param("id")?).await?;
    let locale = sc_i18n::Locale::parse(ctx.path_param("locale")?)?;
    let obj = require_object(&body)?;
    let messages = obj
        .get("messages")
        .ok_or_else(|| Error::invalid("`messages` is required"))?;
    Ok(HandlerResponse::ok(
        crate::translations::save_catalogue(&catalog, &apps, &app, &locale, messages).await?,
    ))
}

/// The `getInstanceTranslations` endpoint.
pub(crate) async fn get_instance_translations(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    _body: Json,
) -> Result<HandlerResponse> {
    let catalog = state.catalog;
    Ok(HandlerResponse::ok(
        crate::translations::instance_translations_json(&catalog).await?,
    ))
}

/// The `saveInstanceTranslations` endpoint.
pub(crate) async fn save_instance_translations(
    state: HandlerState,
    ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    let locale = sc_i18n::Locale::parse(ctx.path_param("locale")?)?;
    let obj = require_object(&body)?;
    let messages = obj
        .get("messages")
        .ok_or_else(|| Error::invalid("`messages` is required"))?;
    Ok(HandlerResponse::ok(
        crate::translations::save_instance_catalogue(&catalog, Some(&apps), &locale, messages)
            .await?,
    ))
}
