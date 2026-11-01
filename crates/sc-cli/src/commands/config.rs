//! `feldspar get-cfg`, `feldspar set-cfg` and `feldspar send-email`: the stored settings.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar get-cfg [KEY] [database flags]` — the stored configuration values
/// (§13.5).
///
/// With a key it prints that value and nothing else, so a script can capture it:
/// `port=$(feldspar get-cfg smtp_port)`. With no key it prints every declared
/// setting as `key=value`, one per line, which is the answer to "what is this
/// installation actually configured to do" — the question that otherwise needs a
/// browser and a session.
///
/// **What it prints is what the server acts on**: the stored value where there is
/// one, the declared default where there is not — the same
/// [`sc_config::config_value`] the boot path reads, so the two cannot disagree.
/// A key with neither prints nothing, and says so on stderr rather than printing
/// an empty line that a caller would have to tell apart from an empty value.
///
/// It prints **nothing else**: the SQL echo is turned off for this command, so a
/// value can be captured out of it whatever Settings → Development says.
///
/// **Secrets.** A listing redacts them, because a listing is asked for by
/// somebody who wants an overview and it ends up in scrollback, in a CI log and
/// in a pasted issue. Naming the key prints it in full: that caller asked for
/// that secret, and this command already holds the database, which is more
/// authority than any of them buys.
pub(crate) async fn get_cfg_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::config::parse_get(&rest)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // This command's stdout **is** the value, so nothing else may go there.
    // The SQL echo is a stored setting meant for a *server's* stdout, which
    // nobody captures an answer out of (see `sc-log`); leaving it on here would
    // mean `port=$(feldspar get-cfg smtp_port)` picking up the select that
    // found the port, because of a checkbox somebody ticked days ago in another
    // process. It stays on for every other command, `set-cfg` included, where
    // stdout is not an answer.
    sc_log::set_log_sql(false);

    let Some(key) = parsed.key else {
        return print_all_config(&catalog, parsed.json).await;
    };
    // `config_value` refuses an undeclared key, naming the ones there are — the
    // typo is fixed from the message rather than from a second command.
    let value = sc_config::config_value(&catalog, &key).await?;
    if parsed.json {
        // `null` for a key with no value, which JSON can say outright.
        println!("{value}");
        return Ok(());
    }
    if value.is_null() {
        eprintln!("feldspar: `{key}` is not set, and has no default");
        return Ok(());
    }
    println!("{}", sc_cli::config::render(&value));
    Ok(())
}

/// Print every declared key as `key=value`, secrets redacted.
///
/// Every *declared* key, not every stored one: a setting that is unset is still
/// a setting this installation has, and leaving it out would make the listing
/// double as a claim that the key does not exist. An unset key with no default
/// prints as `key=` — present, plainly empty.
///
/// Keys stored under a name no declaration describes are reported on **stderr**
/// afterwards. Nothing reads such a row, so it is not part of the answer; but it
/// is a setting somebody believes is in force, which is worth a sentence.
///
/// With `--json` the same listing is one JSON object, key to value, an unset
/// key `null` and a secret that is set its redaction.
pub(crate) async fn print_all_config(catalog: &sc_catalog::Catalog, json: bool) -> Result<()> {
    let values = sc_config::all_config(catalog).await?;
    let fields = sc_config::config_spec()
        .into_iter()
        .chain(sc_config::internal_defs().iter().map(|d| d.field.clone()));
    if json {
        let mut out = serde_json::Map::new();
        for field in fields {
            let key = field.name();
            let value = match values.get(key) {
                Some(value) if !value.is_null() && field.secret => {
                    serde_json::Value::from(sc_types::SECRET_SENTINEL)
                }
                Some(value) => value.clone(),
                None => serde_json::Value::Null,
            };
            out.insert(key.to_owned(), value);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap_or_default()
        );
    } else {
        for field in fields {
            print_config_line(&field, &values);
        }
    }
    for stray in sc_config::stray_config_keys(catalog).await? {
        eprintln!(
            "feldspar: `{stray}` is stored but is not a configuration key this \
             version has, so nothing reads it"
        );
    }
    Ok(())
}

/// One `key=value` line of the listing.
pub(crate) fn print_config_line(field: &sc_types::FormField, values: &sc_types::Attrs) {
    {
        let key = field.name();
        let line = match values.get(key) {
            Some(value) if value.is_null() => String::new(),
            Some(_) if field.secret => sc_types::SECRET_SENTINEL.to_owned(),
            Some(value) => sc_cli::config::render_inline(value),
            None => String::new(),
        };
        println!("{key}={line}");
    }
}

/// `feldspar set-cfg KEY [VALUE] [database flags]` — write one configuration
/// value.
///
/// The value comes from the argument when there is one and from **stdin** when
/// there is not, which is how a certificate is set without quoting a PEM block
/// into a shell:
///
/// ```text
/// feldspar set-cfg ssl_certificate < fullchain.pem
/// feldspar set-cfg smtp_port 2525
/// ```
///
/// A terminal has only strings, so the *declaration* decides the type
/// ([`sc_cli::config::value_for`]) and [`sc_config::set_config`] checks the
/// result against that same declaration — the one check every writer goes
/// through, so `smtp_port = "yes"` is refused here exactly as it is in the
/// admin UI, and nothing is written.
///
/// It does not restart anything. A stored setting is read by the server at the
/// point it needs it — the TLS settings at boot, the SMTP transport per message
/// — so what a write takes effect on, and when, is the setting's own business
/// and not this command's to guess at.
///
/// Two more forms, for a script that owns a whole settings section:
///
/// - `set-cfg --json < settings.json` reads a JSON object of keys, checks
///   **every** value against its declaration first, and writes them in one
///   transaction — so a refused port leaves the SMTP settings as they were
///   rather than half changed. A `null` value unsets that key.
/// - `set-cfg --unset KEY` deletes the stored value; the key reads as its
///   declared default again.
pub(crate) async fn set_cfg_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = match sc_cli::config::parse_set_command(&rest)? {
        sc_cli::config::SetCommand::One(parsed) => parsed,
        sc_cli::config::SetCommand::Many => {
            let read = std::io::read_to_string(std::io::stdin()).map_err(|e| {
                sc_error::Error::config(format!("reading the settings from stdin: {e}"))
            })?;
            let document: serde_json::Value = serde_json::from_str(&read).map_err(|e| {
                sc_error::Error::invalid(format!("stdin is not a JSON object of settings: {e}"))
            })?;
            let values = sc_cli::config::values_from_json(&document)?;
            if let Some(source) = db.source() {
                eprintln!("feldspar: database configured from {source}");
            }
            let catalog = connect_catalog(&db).await?;
            sc_config::set_config_many(&catalog, &values).await?;
            for (key, value) in &values {
                let secret = sc_config::definition(key).is_some_and(|f| f.secret);
                eprintln!(
                    "feldspar: {key} = {}",
                    match (value.is_null(), secret) {
                        (true, _) => "(unset)".to_owned(),
                        (false, true) => sc_types::SECRET_SENTINEL.to_owned(),
                        (false, false) => sc_cli::config::render_inline(value),
                    }
                );
            }
            return Ok(());
        }
        sc_cli::config::SetCommand::Unset(key) => {
            if let Some(source) = db.source() {
                eprintln!("feldspar: database configured from {source}");
            }
            let catalog = connect_catalog(&db).await?;
            // Through `set_config`, whose `null` is a delete — so the key is
            // checked against the declarations as a write would be.
            let was_set = sc_config::stored_config(&catalog, &key).await?.is_some();
            sc_config::set_config(&catalog, &key, serde_json::Value::Null).await?;
            eprintln!(
                "feldspar: {key} {}",
                if was_set {
                    "unset; it reads as its default again"
                } else {
                    "was not set; nothing changed"
                }
            );
            return Ok(());
        }
    };

    // The key is checked against the declarations *before* stdin is read: a typo
    // should not leave the command sitting on a terminal waiting for a value
    // nobody can store.
    let field = sc_config::definition(&parsed.key).ok_or_else(|| {
        sc_error::Error::invalid(format!(
            "no configuration key `{}`; known keys are {}",
            parsed.key,
            sc_config::known_keys().join(", ")
        ))
    })?;

    let raw = match parsed.value {
        Some(value) => value,
        None => {
            let read = std::io::read_to_string(std::io::stdin()).map_err(|e| {
                sc_error::Error::config(format!("reading the value from stdin: {e}"))
            })?;
            // One trailing newline is the pipe's, not the value's.
            sc_cli::config::strip_final_newline(&read).to_owned()
        }
    };
    let value = sc_cli::config::value_for(&field, &raw)?;
    sc_cli::config::refuse_sentinel(&parsed.key, &value)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    sc_config::set_config(&catalog, &parsed.key, value.clone()).await?;

    // What was written, echoed back — but never a secret's value, which is the
    // one thing a terminal should not be made to hold a copy of by a command
    // that was given it on the way in.
    eprintln!(
        "feldspar: {} = {}",
        parsed.key,
        if field.secret {
            sc_types::SECRET_SENTINEL.to_owned()
        } else {
            sc_cli::config::render_inline(&value)
        }
    );
    Ok(())
}

/// `feldspar send-email --to ADDR --subject TEXT [--html] [database flags] < body`
/// — one message through the stored SMTP settings and the server's own
/// transport. See `sc_cli::email`.
///
/// It connects nothing but the database (no catalog bootstrap beyond what
/// every command does, no modules), and sends through `SmtpMailer`, which is
/// what the Email settings' *Send test message* sends through.
pub(crate) async fn send_email_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::email::parse_send(&rest)?;
    // Before the database and stdin: an address that will not parse is a typo.
    let recipients = sc_email::parse_recipients(&parsed.to)?;
    let body = std::io::read_to_string(std::io::stdin())
        .map_err(|e| sc_error::Error::config(format!("reading the body from stdin: {e}")))?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    sc_dns::init()?;
    let settings = sc_config::EmailSettings::load(&catalog)
        .await?
        .ok_or_else(|| {
            sc_error::Error::invalid(
                "no SMTP server is configured: set smtp_host (Settings → Email, or \
                 `feldspar set-cfg`) first",
            )
        })?;
    let mut email = sc_email::Email::new(settings.from.clone());
    email.to = recipients;
    email.subject = parsed.subject;
    match parsed.html {
        true => email.html = Some(body),
        false => email.text = Some(body),
    }
    email.check()?;
    sc_email::Mailer::send(&sc_email::SmtpMailer::new(&settings)?, &email).await?;
    eprintln!(
        "feldspar: sent to {} via {}:{}",
        parsed.to, settings.host, settings.port
    );
    Ok(())
}
