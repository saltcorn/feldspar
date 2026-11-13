//! `feldspar add-user`, `modify-user`, `delete-user` and `role`.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar add-user EMAIL --role ROLE [--password [VALUE] | --random-password]`
/// — create an account from a terminal. With no password value, it is asked
/// for; with no `--password` at all and no terminal, it is refused.
pub(crate) async fn add_user_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::users::parse_add_user(&rest)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // Before the prompt: a role or email that will be refused should not cost a
    // password typed twice first.
    let role = sc_cli::auth::resolve_role(&catalog, &parsed.role).await?;
    if sc_auth::load_user_by_email(&catalog, parsed.email.trim())
        .await?
        .is_some()
    {
        return Err(sc_error::Error::invalid(format!(
            "there is already a user with the email `{}`; use modify-user to change it",
            parsed.email.trim()
        )));
    }
    let password = match parsed.random_password {
        true => sc_auth::random_password(),
        false => sc_cli::users::password_from(parsed.password.as_ref())?,
    };
    let user = sc_auth::create_user(&catalog, &parsed.email, &password, role.role).await?;
    eprintln!(
        "feldspar: created {} with role {} ({}), id {}",
        parsed.email.trim(),
        role.name,
        role.role,
        user.id()
    );
    if parsed.random_password {
        // Printed once, alone on stdout, so a script can capture it.
        println!("{password}");
    }
    Ok(())
}

/// `feldspar modify-user EMAIL [--role ROLE] [--password [VALUE]]` — change an
/// account's role or password from a terminal: the way back in for an admin who
/// has forgotten their password on a server that cannot email a reset link.
pub(crate) async fn modify_user_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::users::parse_modify_user(&rest)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    let user = sc_auth::load_user_by_email(&catalog, parsed.email.trim())
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!("no user with the email `{}`", parsed.email.trim()))
        })?;
    let role = match &parsed.role {
        Some(spec) => Some(sc_cli::auth::resolve_role(&catalog, spec).await?),
        None => None,
    };
    let password = match &parsed.password {
        Some(arg) => Some(sc_cli::users::password_from(Some(arg))?),
        None if parsed.random_password => Some(sc_auth::random_password()),
        None => None,
    };
    // First, so a refusal (the last enabled admin) changes nothing else.
    let sessions = sc_auth::SessionStore::database(catalog.clone());
    if let Some(disabled) = parsed.disabled {
        let changed = sc_auth::set_user_disabled(&catalog, user.id(), disabled).await?;
        // A disabled account stops resolving from a session it holds; the
        // rows are dropped too, as the admin UI's "Disable" drops them.
        if let Some(credentials) = changed.credentials {
            sessions.bump_epoch(credentials, None).await?;
        }
        eprintln!(
            "feldspar: {} is now {}",
            parsed.email.trim(),
            if disabled { "disabled" } else { "enabled" }
        );
    }
    if parsed.reset_second_factor {
        let removed = sc_auth::disable_second_factor(&catalog, user.id()).await?;
        if let Some(credentials) = removed.changed {
            sessions.bump_epoch(credentials, None).await?;
        }
        // Whoever is signed in to it now may be why it is being reset.
        sessions.end_user_sessions(user.id()).await?;
        eprintln!(
            "feldspar: two-factor authentication is off for {}; its sessions have ended",
            parsed.email.trim()
        );
    }
    if role.is_none() && password.is_none() {
        return Ok(());
    }
    let changed = sc_auth::update_user(
        &catalog,
        user.id(),
        sc_auth::UserUpdate {
            role: role.as_ref().map(|r| r.role),
            password: password.clone().map(sc_error::Secret::new),
            ..Default::default()
        },
    )
    .await?;
    if let Some(credentials) = changed.credentials {
        sessions.bump_epoch(credentials, None).await?;
    }
    if let Some(role) = &role {
        eprintln!(
            "feldspar: {} now has role {} ({})",
            parsed.email.trim(),
            role.name,
            role.role
        );
    }
    if let Some(password) = &password {
        eprintln!("feldspar: password changed for {}", parsed.email.trim());
        if parsed.random_password {
            println!("{password}");
        }
    }
    Ok(())
}

/// `feldspar delete-user EMAIL` — delete an account from a terminal (TODO
/// F1.8). The last enabled admin is refused, as it is in the admin UI.
pub(crate) async fn delete_user_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let email = sc_cli::users::parse_delete_user(&rest)?;
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    let user = sc_auth::load_user_by_email(&catalog, email.trim())
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!("no user with the email `{}`", email.trim()))
        })?;
    sc_auth::delete_user(&catalog, user.id()).await?;
    sc_auth::SessionStore::database(catalog.clone())
        .end_user_sessions(user.id())
        .await?;
    eprintln!("feldspar: deleted {}", email.trim());
    Ok(())
}

/// `feldspar role list|add|delete` — the roles users and endpoints are
/// assigned, from a terminal (TODO F1.8), over `sc-auth`'s own functions.
pub(crate) async fn role_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let command = sc_cli::users::parse_role(&rest)?;
    let catalog = connect_catalog(&db).await?;
    sc_log::set_log_sql(false);
    match command {
        sc_cli::users::RoleCommand::List { json } => {
            let roles = sc_auth::list_roles(&catalog).await?;
            if json {
                let out: Vec<serde_json::Value> = roles
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "role": r.role, "name": r.name, "description": r.description,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
                return Ok(());
            }
            for r in &roles {
                println!("{:>3}  {:<20} {}", r.role, r.name, r.description);
            }
            Ok(())
        }
        sc_cli::users::RoleCommand::Add {
            name,
            level,
            description,
        } => {
            let role = sc_auth::RoleDef::new(level, name.trim()).description(description.trim());
            sc_auth::save_role(&catalog, &role).await?;
            eprintln!("feldspar: created role {} ({level})", name.trim());
            Ok(())
        }
        sc_cli::users::RoleCommand::Delete { role } => {
            let found = sc_cli::auth::resolve_role(&catalog, &role).await?;
            sc_auth::delete_role(&catalog, found.role).await?;
            eprintln!("feldspar: deleted role {} ({})", found.name, found.role);
            Ok(())
        }
    }
}
