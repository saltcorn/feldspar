//! `feldspar mcp-token …`: MCP access tokens.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar mcp-token create|list|revoke` — the administration MCP server's
/// bearer tokens, without the admin UI (§13.6). See `sc_cli::mcp_token`.
pub(crate) async fn mcp_token_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => mcp_token_create_command(&args[1..]).await,
        Some("list") => mcp_token_list_command(&args[1..]).await,
        Some("revoke") => mcp_token_revoke_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown mcp-token subcommand `{other}`; there are create, list and revoke"
        ))),
        None => Err(sc_error::Error::config(
            "mcp-token needs a subcommand: create, list or revoke",
        )),
    }
}

/// `feldspar mcp-token create --label L (--email E | --admin) [--expires-in-days N]
/// [--allow-all] [--allow-…|--no-allow-…]… [--url ORIGIN] [--name NAME]`.
///
/// Prints the secret, alone, on stdout — the one time it exists anywhere — and
/// everything else on stderr.
pub(crate) async fn mcp_token_create_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::mcp_token::parse_create(&rest)?;
    for warning in &parsed.warnings {
        eprintln!("feldspar: {warning}");
    }
    // Before the database: a grant that will not validate is a typo, and the
    // message should not wait on a connection.
    let grants = match parsed.routes_only {
        true => sc_cli::mcp_token::routes_only_grants(),
        false => sc_cli::mcp_token::normalised_grants(&parsed.grants)?,
    };

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // stdout is the secret and nothing else.
    sc_log::set_log_sql(false);

    let user = sc_cli::auth::resolve_user(&catalog, &parsed.user).await?;
    let email = user
        .extra()
        .get(sc_auth::COL_EMAIL)
        .and_then(|e| e.as_text())
        .unwrap_or("the selected user")
        .to_owned();
    // A token for a non-admin would be minted and then refused on every call —
    // unless it is a routes-only one, which runs as a user of any role.
    if !user.role().is_admin() && !parsed.routes_only {
        return Err(sc_error::Error::invalid(format!(
            "{email} is not an administrator, and an MCP token only works for one"
        )));
    }

    let minted = sc_auth::mint_api_token(
        &catalog,
        sc_auth::NewApiToken {
            user_id: user.id(),
            label: parsed.label.clone(),
            grants,
            expires_at: sc_cli::mcp_token::expiry(parsed.expires_in_days, chrono::Utc::now()),
        },
    )
    .await?;

    println!("{}", minted.secret);
    let token = &minted.token;
    eprintln!(
        "feldspar: minted MCP token `{}` (id {}) running as {email}",
        token.label, token.id
    );
    eprintln!(
        "feldspar: grants: {}",
        sc_cli::mcp_token::describe_grants(&token.grants)
    );
    match token.expires_at {
        Some(at) => eprintln!("feldspar: expires {at}"),
        None => eprintln!("feldspar: it does not expire; revoke it when you are done with it"),
    }
    let origin = parsed
        .url
        .clone()
        .unwrap_or_else(|| format!("http://localhost:{}", db.serving().port().unwrap_or(3032)));
    if parsed.routes_only {
        eprintln!(
            "feldspar: the token is shown once. It reaches module routes only, as {email} \
             (role {}):",
            user.role()
        );
        eprintln!(
            "  curl -H \"Authorization: Bearer {}\" {}{}/<module>/<route>",
            minted.secret,
            origin.trim_end_matches('/'),
            sc_module::ROUTES_PREFIX
        );
        return Ok(());
    }
    eprintln!("feldspar: the token is shown once. Register it with:");
    eprintln!(
        "  {}",
        sc_cli::mcp_token::claude_mcp_add_line(&origin, &parsed.name, &minted.secret)
    );
    if !sc_config::mcp_settings(&catalog).await?.enabled {
        eprintln!(
            "feldspar: the MCP server is off; `feldspar set-cfg mcp_enabled true` turns it on"
        );
    }
    Ok(())
}

/// `feldspar mcp-token list [--json]` — every token, newest first, revoked and
/// lapsed ones included. Never the secret, which nothing can read back.
pub(crate) async fn mcp_token_list_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let as_json = match rest.as_slice() {
        [] => false,
        [flag] if flag == "--json" => true,
        [other, ..] => {
            return Err(sc_error::Error::config(format!(
                "unknown mcp-token list argument `{other}`; it takes --json"
            )));
        }
    };
    let catalog = connect_catalog(&db).await?;
    sc_log::set_log_sql(false);
    let now = chrono::Utc::now();
    let tokens = sc_auth::list_api_tokens(&catalog).await?;
    if as_json {
        let out: Vec<serde_json::Value> = tokens
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "user_id": t.user_id,
                    "label": t.label,
                    "grants": t.grants,
                    "created_at": t.created_at,
                    "expires_at": t.expires_at,
                    "last_used_at": t.last_used_at,
                    "revoked_at": t.revoked_at,
                    "state": sc_cli::mcp_token::state(t, now),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }
    for t in &tokens {
        println!(
            "{}  {:<7}  {}  [{}]{}",
            t.id,
            sc_cli::mcp_token::state(t, now),
            t.label,
            sc_cli::mcp_token::describe_grants(&t.grants),
            t.last_used_at
                .map(|at| format!("  last used {at}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// `feldspar mcp-token revoke ID` — the row stays, marked revoked; the next call
/// under it fails.
pub(crate) async fn mcp_token_revoke_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let id = sc_cli::mcp_token::parse_revoke(&rest)?;
    let catalog = connect_catalog(&db).await?;
    let token = sc_auth::load_api_token(&catalog, id)
        .await?
        .ok_or_else(|| sc_error::Error::not_found(format!("no MCP token with id {id}")))?;
    match sc_auth::revoke_api_token(&catalog, id).await? {
        true => eprintln!("feldspar: revoked MCP token `{}` ({id})", token.label),
        false => eprintln!(
            "feldspar: MCP token `{}` ({id}) was already revoked",
            token.label
        ),
    }
    Ok(())
}
