//! An application as an OpenID Connect identity provider (TODO IP1.8): its
//! clients, the grants its users made to them, and the instance's signing
//! keys. The switch itself is the application's `identity_provider` field,
//! saved with the rest of it. The storage is `sc_auth`'s (`oidc.rs` there).

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    // An application's users' API tokens (TODO AT1): listed with whose they
    // are, and revoked; never minted here.
    reg.handle(sc_api::handler_ids::LIST_APPLICATION_API_TOKENS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let mut out = Vec::new();
                for token in sc_auth::list_app_api_tokens(&catalog, app.id.0, None).await? {
                    let email =
                        sc_auth::load_user(&catalog, token.user_id)
                            .await?
                            .and_then(|user| match user.get(sc_auth::COL_EMAIL) {
                                Some(sc_query::Value::Text(t)) => Some(t.clone()),
                                _ => None,
                            });
                    let mut row = crate::api_tokens::token_json(&token);
                    row["user_id"] = json!(token.user_id);
                    row["email"] = json!(email);
                    out.push(row);
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REVOKE_APPLICATION_API_TOKEN, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let id = uuid::Uuid::parse_str(ctx.path_param("token_id")?)
                    .map_err(|_| Error::invalid("not a token id"))?;
                let revoked = sc_auth::revoke_app_api_token(&catalog, app.id.0, id, None).await?;
                Ok(HandlerResponse::ok(json!({ "revoked": revoked })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_IDENTITY_PROVIDER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let clients: Vec<Json> = sc_auth::list_oauth_clients(&catalog, app.id.0)
                    .await?
                    .iter()
                    .map(client_json)
                    .collect();
                let mut grants = Vec::new();
                for grant in sc_auth::list_oauth_grants(&catalog, app.id.0).await? {
                    let email =
                        sc_auth::load_user(&catalog, grant.user_id)
                            .await?
                            .and_then(|user| match user.get(sc_auth::COL_EMAIL) {
                                Some(sc_query::Value::Text(t)) => Some(t.clone()),
                                _ => None,
                            });
                    grants.push(json!({
                        "id": grant.id,
                        "user_id": grant.user_id,
                        "email": email,
                        "client_id": grant.client_id,
                        "scopes": grant.scopes,
                        "created_at": grant.created_at.to_rfc3339(),
                        "updated_at": grant.updated_at.to_rfc3339(),
                    }));
                }
                let keys = signing_keys_json(&catalog).await?;
                Ok(HandlerResponse::ok(json!({
                    "enabled": app.identity_provider,
                    "clients": clients,
                    "grants": grants,
                    "signing_keys": keys,
                    "claim_sources": claim_sources(&catalog)?,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_OAUTH_CLIENT, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let settings = client_settings(obj)?;
                let registered = sc_auth::register_oauth_client(
                    &catalog,
                    sc_auth::NewOAuthClient {
                        app_id: app.id.0,
                        name: settings.name,
                        confidential: obj
                            .get("confidential")
                            .and_then(Json::as_bool)
                            .unwrap_or(true),
                        redirect_uris: settings.redirect_uris,
                        scopes: settings.scopes,
                        skip_consent: settings.skip_consent,
                        created_by: ctx.user().map(sc_auth::User::id),
                        claim_fields: settings.claim_fields,
                        post_logout_redirect_uris: settings.post_logout_redirect_uris,
                    },
                )
                .await?;
                Ok(HandlerResponse::ok(json!({
                    "client": client_json(&registered.client),
                    "secret": registered.secret,
                }))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_OAUTH_CLIENT, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let client_id = ctx.path_param("client_id")?;
                let settings = client_settings(require_object(&ctx.body)?)?;
                let client = sc_auth::update_oauth_client(&catalog, app.id.0, client_id, settings)
                    .await?
                    .ok_or_else(|| no_client(client_id))?;
                Ok(HandlerResponse::ok(client_json(&client)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_OAUTH_CLIENT, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let deleted =
                    sc_auth::delete_oauth_client(&catalog, app.id.0, ctx.path_param("client_id")?)
                        .await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REGENERATE_OAUTH_CLIENT_SECRET, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let client_id = ctx.path_param("client_id")?;
                let client = sc_auth::load_oauth_client(&catalog, app.id.0, client_id)
                    .await?
                    .ok_or_else(|| no_client(client_id))?;
                if !client.confidential {
                    return Err(Error::invalid(
                        "a public client has no secret: it relies on PKCE alone. Register a \
                         confidential client instead",
                    ));
                }
                let secret = sc_auth::regenerate_oauth_client_secret(&catalog, app.id.0, client_id)
                    .await?
                    .ok_or_else(|| no_client(client_id))?;
                Ok(HandlerResponse::ok(json!({ "secret": secret })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REVOKE_OAUTH_GRANT, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let raw = ctx.path_param("grant_id")?;
                let grant_id = uuid::Uuid::parse_str(raw)
                    .map_err(|_| Error::invalid(format!("`{raw}` is not a grant id")))?;
                let revoked = sc_auth::revoke_oauth_grant(&catalog, app.id.0, grant_id).await?;
                Ok(HandlerResponse::ok(json!({ "revoked": revoked })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::ROTATE_OIDC_SIGNING_KEYS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                sc_auth::rotate_signing_keys(&catalog).await?;
                Ok(HandlerResponse::ok(Json::Array(
                    signing_keys_json(&catalog).await?,
                )))
            }
        }
    });
}

fn no_client(client_id: &str) -> Error {
    Error::not_found(format!(
        "there is no client `{client_id}` on this application"
    ))
}

/// A client on the wire (`oauth_client_schema`): never its secret.
fn client_json(client: &sc_auth::OAuthClient) -> Json {
    json!({
        "client_id": client.client_id,
        "name": client.name,
        "confidential": client.confidential,
        "redirect_uris": client.redirect_uris,
        "scopes": client.scopes,
        "skip_consent": client.skip_consent,
        "claim_fields": client.claim_fields,
        "post_logout_redirect_uris": client.post_logout_redirect_uris,
        "created_at": client.created_at.to_rfc3339(),
    })
}

async fn signing_keys_json(catalog: &Catalog) -> Result<Vec<Json>> {
    Ok(sc_auth::list_signing_keys(catalog)
        .await?
        .iter()
        .map(|key| {
            json!({
                "kid": key.kid,
                "alg": key.alg.name(),
                "created_at": key.created_at.to_rfc3339(),
                "retired_at": key.retired_at.map(|at| at.to_rfc3339()),
            })
        })
        .collect())
}

/// What a claim may be mapped onto: `role`, then the fields an administrator
/// added to the users table.
fn claim_sources(catalog: &Catalog) -> Result<Vec<String>> {
    let users = catalog.require(sc_auth::USERS_TABLE)?;
    Ok(std::iter::once(sc_auth::ROLE_CLAIM_FIELD.to_owned())
        .chain(
            users
                .fields
                .iter()
                .map(|f| f.base.name.clone())
                .filter(|name| !sc_auth::is_system_user_column(name)),
        )
        .collect())
}

/// A client's settings from the form's body.
fn client_settings(obj: &serde_json::Map<String, Json>) -> Result<sc_auth::OAuthClientUpdate> {
    let claim_fields = match obj.get("claim_fields") {
        None | Some(Json::Null) => Vec::new(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| Error::invalid("`claim_fields` should be a list of { claim, field }"))?,
    };
    let strings = |key: &str| -> Result<Vec<String>> {
        match obj.get(key) {
            None | Some(Json::Null) => Ok(Vec::new()),
            Some(_) => string_array_field(obj, key),
        }
    };
    Ok(sc_auth::OAuthClientUpdate {
        name: obj.str_field("name")?.to_owned(),
        redirect_uris: strings("redirect_uris")?,
        scopes: strings("scopes")?,
        skip_consent: obj
            .get("skip_consent")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        claim_fields,
        post_logout_redirect_uris: strings("post_logout_redirect_uris")?,
    })
}
