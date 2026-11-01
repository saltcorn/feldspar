//! Signing in and the first user.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    reg.register("authStatus", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let exists = any_user_exists(&catalog).await?;
                // Whether the create-first-user screen needs the setup link's
                // token. Not a secret, and only said while it can matter.
                let token_required =
                    !exists && !sc_config::first_user_tokens(&catalog).await?.is_empty();
                let current = ctx.user().map_or(Json::Null, user_summary_json);
                // Asked only of an admin: nobody else is shown the dialogs, and
                // a signed-out caller has no business learning the answer.
                let onboarding_pending = match ctx.user() {
                    Some(user) if user.role().is_admin() => {
                        sc_config::config_value(&catalog, sc_config::ONBOARDING_DONE).await?
                            != json!(true)
                    }
                    _ => false,
                };
                let locales = sc_i18n::active();
                Ok(HandlerResponse::ok(json!({
                    "any_user_exists": exists,
                    "first_user_token_required": token_required,
                    "current_user": current,
                    "locales": {
                        "default": locales.default_locale().as_str(),
                        "current": ctx.locale.as_str(),
                        "enabled": locales
                            .enabled()
                            .iter()
                            .map(|l| l.as_str())
                            .collect::<Vec<_>>(),
                    },
                    "onboarding_pending": onboarding_pending,
                })))
            }
        }
    });

    reg.register_admin("finishOnboarding", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                sc_config::set_config(&catalog, sc_config::ONBOARDING_DONE, json!(true)).await?;
                Ok(HandlerResponse::ok(json!({ "done": true })))
            }
        }
    });

    reg.register("createFirstUser", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let (email, password) = credentials(&ctx.body)?;
                // An installation set up with a first-user token is claimed
                // only through the link that carries it (security: otherwise
                // whoever reaches a fresh instance first becomes its admin).
                let expected = sc_config::first_user_tokens(&catalog).await?;
                if !expected.is_empty() {
                    let supplied = ctx.body.get("token").and_then(Json::as_str);
                    if !sc_auth::first_user_token_matches(&expected, supplied) {
                        return Ok(HandlerResponse::ok(json!({
                            "error": "this installation's first user can only be created \
                                      through the setup link it was given",
                        }))
                        .with_status(403));
                    }
                }
                let user = create_first_user(&catalog, &email, &password).await?;
                // Log the new admin straight in — they came from the bootstrap
                // screen and expect to land in the app.
                let body = user_summary_json(&user);
                Ok(HandlerResponse::start_verified_session(user, body))
            }
        }
    });

    /// A throttled sign-in (TODO F4.4): `429 {error, retry_after}`, which the
    /// router also sends as `Retry-After`.
    fn too_many_requests(limited: sc_ratelimit::Limited) -> HandlerResponse {
        HandlerResponse::ok(json!({
            "error": "too many requests",
            "retry_after": limited.retry_after_secs(),
        }))
        .with_status(429)
    }

    reg.register("login", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let (email, password) = credentials(&ctx.body)?;
                // Throttled per IP and per email, as an application's login
                // is (TODO F4.4), before the account is looked up.
                if let Err(limited) = sc_api::throttle::login_attempt(ctx.client_ip, &email) {
                    return Ok(too_many_requests(limited));
                }
                // The admin host signs in admins. An Analytics application
                // (analytics TODO A9.3) signs in anybody whose password is
                // right, on its own host: its role floor is what decides what
                // they reach, and it is applied to every call after this one.
                let user = match ctx.analytics_app {
                    Some(_) => sc_auth::authenticate(&catalog, &email, &password).await?,
                    None => authenticate_admin(&catalog, &email, &password).await?,
                };
                match user {
                    Some(user) => {
                        let body = user_summary_json(&user);
                        let remember = sc_api::auth::remember(&ctx.body);
                        Ok(HandlerResponse::start_session_remembering(
                            user, body, remember,
                        ))
                    }
                    None => {
                        sc_api::throttle::login_failed(&email);
                        Err(Error::auth(t!(ctx.locale, "invalid credentials")))
                    }
                }
            }
        }
    });

    reg.register_signed_in("logout", |_ctx| async {
        Ok(HandlerResponse::end_session(json!({ "ok": true })))
    });
}
