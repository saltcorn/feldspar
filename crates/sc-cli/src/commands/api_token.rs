//! `feldspar api-token …`: an application's users' API tokens (TODO AT1).

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar api-token create|list|revoke`. See `sc_cli::api_token`.
pub(crate) async fn api_token_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => api_token_create_command(&args[1..]).await,
        Some("list") => api_token_list_command(&args[1..]).await,
        Some("revoke") => api_token_revoke_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown api-token subcommand `{other}`; there are create, list and revoke"
        ))),
        None => Err(sc_error::Error::config(
            "api-token needs a subcommand: create, list or revoke",
        )),
    }
}

/// The email of `user`, for a line an operator reads.
fn email_of(user: &sc_auth::User) -> Option<String> {
    user.extra()
        .get(sc_auth::COL_EMAIL)
        .and_then(|e| e.as_text())
        .map(str::to_owned)
}

/// `feldspar api-token create --app SUBDOMAIN --label L (--email E | --admin)
/// [--expires-in-days N] [--url ORIGIN]`: the secret alone on stdout, the rest
/// on stderr.
async fn api_token_create_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::api_token::parse_create(&rest)?;
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    sc_log::set_log_sql(false);

    let app = sc_app::load_application_by_subdomain(&catalog, &parsed.app)
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!(
                "no application with the subdomain `{}`; `feldspar app list` shows them",
                parsed.app
            ))
        })?;
    let user = sc_cli::auth::resolve_user(&catalog, &parsed.user).await?;
    let email = email_of(&user).unwrap_or_else(|| "the selected user".to_owned());

    let minted = sc_auth::mint_app_api_token(
        &catalog,
        sc_auth::NewAppApiToken {
            application_id: app.id.0,
            user_id: user.id(),
            label: parsed.label.clone(),
            expires_at: sc_cli::mcp_token::expiry(parsed.expires_in_days, chrono::Utc::now()),
        },
    )
    .await?;

    println!("{}", minted.secret);
    let token = &minted.token;
    eprintln!(
        "feldspar: minted API token `{}` (id {}) for {} running as {email} (role {})",
        token.label,
        token.id,
        app.subdomain,
        user.role()
    );
    match token.expires_at {
        Some(at) => eprintln!("feldspar: expires {at}"),
        None => eprintln!("feldspar: it does not expire; revoke it when you are done with it"),
    }
    let origin = parsed.url.clone().unwrap_or_else(|| {
        sc_cli::api_token::default_origin(&app.subdomain, db.serving().port().unwrap_or(3032))
    });
    eprintln!("feldspar: the token is shown once. Call the application with it:");
    eprintln!(
        "  {}",
        sc_cli::api_token::curl_line(&origin, &minted.secret)
    );
    if !app.api_tokens_enabled() {
        eprintln!(
            "feldspar: {} does not accept API tokens yet: turn them on in its Security \
             settings, or the token is refused",
            app.subdomain
        );
    }
    Ok(())
}

/// `feldspar api-token list [--app SUBDOMAIN] [--json]`: every application's
/// tokens (or one's), newest first, revoked and lapsed ones included. Never
/// the secret.
async fn api_token_list_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::api_token::parse_list(&rest)?;
    let catalog = connect_catalog(&db).await?;
    sc_log::set_log_sql(false);
    let apps = match &parsed.app {
        Some(subdomain) => vec![
            sc_app::load_application_by_subdomain(&catalog, subdomain)
                .await?
                .ok_or_else(|| {
                    sc_error::Error::not_found(format!(
                        "no application with the subdomain `{subdomain}`"
                    ))
                })?,
        ],
        None => sc_app::list_applications(&catalog).await?,
    };
    let now = chrono::Utc::now();
    let mut out = Vec::new();
    for app in &apps {
        for token in sc_auth::list_app_api_tokens(&catalog, app.id.0, None).await? {
            let email = sc_auth::load_user(&catalog, token.user_id)
                .await?
                .and_then(|user| email_of(&user));
            if parsed.json {
                out.push(sc_cli::api_token::token_json(
                    &token,
                    &app.subdomain,
                    email.as_deref(),
                ));
                continue;
            }
            println!(
                "{}  {:<7}  {}  {}  {}{}",
                token.id,
                sc_cli::mcp_token::state(&token, now),
                app.subdomain,
                email.as_deref().unwrap_or("(deleted user)"),
                token.label,
                token
                    .last_used_at
                    .map(|at| format!("  last used {at}"))
                    .unwrap_or_default()
            );
        }
    }
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    }
    Ok(())
}

/// `feldspar api-token revoke ID`: the row stays, marked revoked; the next
/// request under it is refused.
async fn api_token_revoke_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let id = sc_cli::api_token::parse_revoke(&rest)?;
    let catalog = connect_catalog(&db).await?;
    let token = sc_auth::load_api_token(&catalog, id)
        .await?
        .ok_or_else(|| sc_error::Error::not_found(format!("no API token with id {id}")))?;
    let Some(application) = token.application_id else {
        return Err(sc_error::Error::invalid(format!(
            "`{}` ({id}) is an MCP token; `feldspar mcp-token revoke {id}` revokes it",
            token.label
        )));
    };
    match sc_auth::revoke_app_api_token(&catalog, application, id, None).await? {
        true => eprintln!("feldspar: revoked API token `{}` ({id})", token.label),
        false => eprintln!(
            "feldspar: API token `{}` ({id}) was already revoked",
            token.label
        ),
    }
    Ok(())
}
