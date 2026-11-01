//! `feldspar auth …`: authentication tokens.

use sc_app::load_application_by_subdomain;
use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar auth SUBCOMMAND …` — sessions for driving an application without a
/// browser to sign in with.
pub(crate) async fn auth_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("token") => auth_token_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown auth subcommand `{other}`; there is `token`"
        ))),
        None => Err(sc_error::Error::config(
            "auth needs a subcommand: token (a signed-in session, written to a file)",
        )),
    }
}

/// `feldspar auth token --app SUBDOMAIN (--email EMAIL | --admin | --role NAME)
/// [--url …] [--out PATH] [--format playwright|netscape]`.
///
/// Mints a session for a user of the **running server** and writes the cookies a
/// browser would have got, so a script can screenshot the screens behind the
/// sign-in page (§13.3).
///
/// No password, because the caller already holds something stronger: the primary
/// database's credentials. What it does with them is write a one-time grant
/// (§7.2) that the server exchanges for an ordinary session — it still creates
/// no user, resets no password and forges no cookie, because the only session
/// the server accepts is one the server itself made.
pub(crate) async fn auth_token_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::auth::parse_token_args(&rest)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    let app = load_application_by_subdomain(&catalog, &parsed.app)
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!("no application with subdomain `{}`", parsed.app))
        })?;
    // Resolved before anything is written or any request is made, so "there is
    // no such user" arrives before "the server is unreachable" — the two are
    // fixed in different places.
    let user = sc_cli::auth::resolve_user(&catalog, &parsed.user).await?;

    // Where the application answers, and where to connect to reach it. The two
    // differ whenever a development machine has no DNS for the base domain,
    // which is most of them.
    let origin = db
        .serving()
        .public_origin(parsed.base_domain.as_deref())
        .ok_or_else(|| {
            sc_error::Error::config(
                "no base domain: an application is served at \
                 `<subdomain>.<base-domain>`, so pass --base-domain, or set \
                 `base_domain` in the feldspar.toml environment this is \
                 connecting with",
            )
        })?;
    let target = sc_cli::auth::Target {
        url: parsed
            .url
            .clone()
            .unwrap_or_else(|| origin.url_for(&app.subdomain)),
        host: origin.host_for(&app.subdomain),
        secure: origin.secure,
    };

    let (cookies, signed_in) = sc_cli::auth::session_for(&catalog, &target, &user).await?;
    let path = sc_cli::auth::out_path(parsed.out.as_deref(), parsed.format);
    sc_cli::auth::write_private(
        &path,
        &sc_cli::auth::render(&cookies, &target.host, parsed.format),
    )?;

    // What was written, for whom, and the URL to point a browser at — the three
    // things the caller's next command needs. The identity is the *server's*
    // answer, not the flag's: `--admin` and `--role` name a user this command
    // chose, and the caller should be told which one it got.
    eprintln!(
        "feldspar: session for {}{} — written to {}",
        signed_in
            .get("email")
            .and_then(|e| e.as_str())
            .unwrap_or("the selected user"),
        match signed_in.get("role").and_then(|r| r.as_i64()) {
            Some(role) => format!(" (role {role})"),
            None => String::new(),
        },
        path.display()
    );
    eprintln!("feldspar: the application is at {}", target.browser_url());
    Ok(())
}
