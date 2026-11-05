//! Settings, external identity providers, the admin address, the test email, system information and
//! help.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    reg.handle(sc_api::handler_ids::GET_HELP, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let topic = ctx.path_param("topic")?.to_owned();
                let context = help_context(
                    &catalog,
                    ctx.user(),
                    ctx.query_get("table"),
                    ctx.query_get("event"),
                )
                .await?;
                let page = sc_docs::help::render(&topic, &context)?;
                Ok(HandlerResponse::ok(
                    json!({ "title": say(&page.title, &ctx.locale), "html": page.html }),
                ))
            }
        }
    });

    //
    // `_fd_config` (§9), rendered from its declarations: the response carries
    // the sections and their fields alongside the values, so the screen is
    // generic over what a setting is — the same arrangement the file-store and
    // provider forms have. Two things happen *here* rather than in the store:
    // secrets are redacted on the way out and the sentinel merged back on the
    // way in (§11.1), and a certificate is parsed before it is saved, because a
    // key that does not match its chain must fail in front of the admin rather
    // than at the next restart (§13.5).
    reg.handle(sc_api::handler_ids::GET_SETTINGS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                Ok(HandlerResponse::ok(
                    settings_json(&catalog, &ctx.locale).await?,
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_SETTINGS, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let submitted = match ctx.body.get("values") {
                    Some(Json::Object(values)) => values.clone(),
                    Some(Json::Null) | None => Attrs::new(),
                    Some(other) => {
                        return Err(Error::invalid(format!(
                            "`values` should be an object of settings, got {other}"
                        )));
                    }
                };
                let stored = sc_config::all_config(&catalog).await?;
                let spec = sc_config::config_spec();
                // Only the form's own keys. An internal one (the base maps, the
                // backup selection, …) is declared, so the store would take it,
                // but it is written by the screen that owns it, through checks
                // this save does not run.
                if let Some(key) = submitted
                    .keys()
                    .find(|key| !spec.iter().any(|f| f.name() == key.as_str()))
                {
                    return Err(Error::invalid(format!(
                        "`{key}` is not a setting on this screen"
                    )));
                }
                // The secret the form sent back untouched is the sentinel it was
                // shown, not the key: restore what is stored.
                let mut values = sc_types::merge_secrets(&spec, &stored, &submitted);
                // The admin subdomain is stored as the router reads it — one
                // lower-cased label, or nothing — so what the screen shows after
                // the save is the host the admin UI is now on.
                if let Some(Json::String(raw)) = values.get(sc_config::ADMIN_SUBDOMAIN) {
                    let normalised = sc_config::check_admin_subdomain(raw)?;
                    values.insert(
                        sc_config::ADMIN_SUBDOMAIN.to_owned(),
                        normalised.map_or(Json::Null, Json::String),
                    );
                }

                // What the settings *mean together* is checked before anything
                // is written: `custom` with no certificate, or a certificate its
                // key does not match, is refused as one act rather than saved as
                // half a configuration.
                let merged = {
                    let mut merged = stored.clone();
                    for (key, value) in &values {
                        merged.insert(key.clone(), value.clone());
                    }
                    merged
                };
                let ssl = sc_config::ssl_settings_from(&merged)?;
                ssl.check()?;
                if ssl.mode == sc_config::SslMode::Custom {
                    crate::tls::check_certificate(&ssl.certificate, ssl.private_key.expose())?;
                }
                // The email section's own cross-field rules, on the same
                // footing: a username with no password, or credentials over an
                // unencrypted connection, is refused here rather than at the
                // first message that fails to send. Reading is the check —
                // `from_config` runs it — so the result is dropped.
                sc_config::EmailSettings::from_config(&merged)?;
                // The Development section's own reading, on the same footing: a
                // verbosity that is not a level is refused here rather than
                // stored and discovered at the next boot.
                let development = sc_config::development_settings_from(&merged)?;
                // The Localisation section's own reading, on the same footing: a
                // tag that is not a language tag is refused here, where the
                // admin can see which box they typed it into (§16.1).
                let localisation = sc_config::localisation_settings_from(&merged)?;
                // Where the admin UI is served: a move is checked against what
                // is served there before anything is written, because the
                // router would otherwise have to choose between the admin UI
                // and an application on one host.
                let admin_subdomain = sc_config::admin_subdomain_from(&merged)?;
                let moving = admin_subdomain != apps.admin().subdomain();
                if moving {
                    check_admin_move(&catalog, &apps, admin_subdomain.as_deref()).await?;
                }
                // Whether one sign-in is good for every application. A cookie
                // already in a browser keeps the scope it was set with, so a
                // change ends every session, this admin's included: everyone
                // signs in again and gets a cookie scoped the new way.
                let shared_session_cookie = sc_config::shared_session_cookie_from(&merged);
                let rescoping = shared_session_cookie != apps.shared_session_cookie();
                // How long a session lasts: refused here when it is not a
                // number of hours, and live for the next sign-in after the save.
                let session_ttl_hours = sc_config::session_ttl_hours_from(&merged)?;
                let persistent_session_cookie = sc_config::persistent_session_cookie_from(&merged);

                sc_config::set_config_many(&catalog, &values).await?;
                // The admin UI moves now, and the new name's certificate is
                // ordered now: the screen that saved this is about to wait for
                // it (`getAdminAddress`) and then follow it there.
                if moving {
                    apps.set_admin_subdomain(admin_subdomain);
                }
                apps.set_shared_session_cookie(shared_session_cookie);
                apps.set_session_ttl_hours(session_ttl_hours);
                apps.set_persistent_session_cookie(persistent_session_cookie);
                // The two switches this process runs under move **now**, not at
                // the next restart: an admin ticks "Log SQL" precisely because
                // something is happening in the server they are looking at, and
                // a switch that needed a restart would have thrown that away.
                // Only a save that names one of them moves them: a save of
                // another section has nothing to say about the process's logging.
                if values.contains_key(sc_config::LOG_SQL)
                    || values.contains_key(sc_config::LOG_VERBOSITY)
                {
                    development.apply();
                }
                // And the enabled locales move now too, for the same reason:
                // an admin who has just turned French on reloads the page to
                // see it, not the server.
                sc_i18n::set_active(localisation);
                let body = settings_json(&catalog, &ctx.locale).await?;
                Ok(if rescoping {
                    HandlerResponse::end_all_sessions(body)
                } else {
                    HandlerResponse::ok(body)
                })
            }
        }
    });

    //
    // A list of records under one internal key. Each is checked against
    // `oauth_provider_spec` on save; its client secret is redacted on the way
    // out (`oauth_provider_json`) and the sentinel merged back on the way in,
    // as an LLM provider's key is.

    reg.handle(sc_api::handler_ids::LIST_O_AUTH_PROVIDERS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let out: Vec<Json> = sc_config::oauth_provider_records(&catalog)
                    .await?
                    .iter()
                    .map(oauth_provider_json)
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(
        sc_api::handler_ids::GET_O_AUTH_PROVIDER_SPEC,
        |ctx| async move {
            Ok(HandlerResponse::ok(json!({
                "fields": spec_json(&sc_config::oauth_provider_spec(), &ctx.locale),
                "callback_path": crate::oauth::CALLBACK_ROUTE,
            })))
        },
    );

    reg.handle(sc_api::handler_ids::CREATE_O_AUTH_PROVIDER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let submitted = oauth_provider_body(&ctx.body)?;
                let spec = sc_config::oauth_provider_spec();
                // Nothing is stored to merge a sentinel against, so one is
                // dropped rather than saved as the secret.
                let record = sc_types::merge_secrets(&spec, &Attrs::new(), &submitted);
                let mut records = sc_config::oauth_provider_records(&catalog).await?;
                records.push(record.clone());
                sc_config::save_oauth_provider_records(&catalog, &records).await?;
                Ok(HandlerResponse::ok(oauth_provider_json(&record)).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_O_AUTH_PROVIDER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("name")?.to_owned();
                let submitted = oauth_provider_body(&ctx.body)?;
                let mut records = sc_config::oauth_provider_records(&catalog).await?;
                let at = records
                    .iter()
                    .position(|r| oauth_record_name(r) == name)
                    .ok_or_else(|| {
                        Error::not_found(format!("no identity provider called `{name}`"))
                    })?;
                let spec = sc_config::oauth_provider_spec();
                let record = sc_types::merge_secrets(&spec, &records[at], &submitted);
                records[at] = record.clone();
                sc_config::save_oauth_provider_records(&catalog, &records).await?;
                Ok(HandlerResponse::ok(oauth_provider_json(&record)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_O_AUTH_PROVIDER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("name")?.to_owned();
                let mut records = sc_config::oauth_provider_records(&catalog).await?;
                let before = records.len();
                records.retain(|r| oauth_record_name(r) != name);
                let deleted = records.len() != before;
                if deleted {
                    sc_config::save_oauth_provider_records(&catalog, &records).await?;
                }
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    // Where the admin UI is served, and whether a browser can follow it there
    // yet (Settings → Development → Admin subdomain). Polled by the dialog a
    // move opens: the certificate for a new name is a minute of ACME traffic
    // away, and the admin is not let go to an address that would refuse them.
    reg.handle(sc_api::handler_ids::GET_ADMIN_ADDRESS, {
        let apps = apps.clone();
        move |_ctx| {
            let apps = apps.clone();
            async move { Ok(HandlerResponse::ok(admin_address_json(&apps))) }
        }
    });

    // A single-use token that gives the signed-in admin a session on the admin
    // UI's current host. The session cookie is host-only, so without this
    // following a move would mean logging in again (`crate::admin_host`).
    reg.handle(sc_api::handler_ids::CREATE_ADMIN_HANDOFF, {
        let apps = apps.clone();
        move |ctx| {
            let apps = apps.clone();
            async move {
                let user = ctx
                    .user()
                    .cloned()
                    .ok_or_else(|| Error::invalid("there is no session to hand off"))?;
                let host = apps.admin_host().ok_or_else(|| {
                    Error::invalid(
                        "this server has no base domain, so the admin UI has no address to \
                         move to",
                    )
                })?;
                let token = apps.admin().mint_handoff(user, host.clone());
                Ok(HandlerResponse::ok(json!({
                    "host": host,
                    "path": format!("{}?token={token}", crate::admin_host::HANDOFF_ROUTE),
                })))
            }
        }
    });

    // One message through the **stored** email settings, so an admin can find
    // out whether they work while they are still on the screen that sets them.
    //
    // Everything about it is deliberate. The settings come from `_fd_config`
    // rather than the request, so this tests what is saved and the note beside
    // the button says to save first. The recipient defaults to the signed-in
    // admin's own address, because they are the one person who can go and look.
    // And the transport's error is returned **verbatim**: "connection refused",
    // "authentication failed" and "relay access denied" are three different
    // problems with three different fixes, and the whole value of a test button
    // is learning which one you have.
    reg.handle(sc_api::handler_ids::SEND_TEST_EMAIL, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let settings = sc_config::EmailSettings::load(&catalog).await?.ok_or_else(|| {
                    Error::invalid(
                        "no SMTP server is configured: set an SMTP host in Settings → Email and \
                         save before sending a test message",
                    )
                })?;

                // The signed-in admin's own address, unless they named one.
                let to = match ctx.body.get("to") {
                    Some(Json::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
                    _ => ctx
                        .user()
                        .and_then(|user| user.get(COL_EMAIL))
                        .and_then(Value::as_text)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            Error::invalid(
                                "there is nobody to send the test message to: give an address, \
                                 or set one on your own account",
                            )
                        })?,
                };
                let recipients = sc_email::parse_recipients(&to)?;

                let mailer = sc_email::SmtpMailer::new(&settings)?;
                let mut email = sc_email::Email::new(settings.from.clone());
                email.to = recipients;
                email.subject = "Saltcorn test message".to_owned();
                // Both bodies, because both are what a real message will carry
                // and a transport that mangles `multipart/alternative` should
                // fail here rather than on the first receipt.
                email.text = Some(TEST_EMAIL_TEXT.to_owned());
                email.html = Some(TEST_EMAIL_HTML.to_owned());
                mailer.send(&email).await?;

                Ok(HandlerResponse::ok(json!({ "sent_to": to })))
            }
        }
    });

    // What this process can do about Python, on the screen that already carries
    // the other "what is this server doing right now" readings (§7, phase 4.2).
    //
    // Read-only and cheap. It is on the Development tab rather than beside the
    // modules because none of it is a *setting* — every number here is a fact
    // about the running process, and the two that are configurable are flags
    // that need a restart, which is precisely the thing an admin needs told.
    reg.handle(sc_api::handler_ids::GET_PYTHON_STATUS, {
        let apps = apps.clone();
        move |_ctx| {
            let apps = apps.clone();
            async move {
                let status = match apps.python() {
                    Some(python) => python.status(),
                    // A server booted with no adapters at all — a test, or an
                    // admin-only process. Nothing about Python is available
                    // here, which is a different sentence from every other one
                    // below and is said as such.
                    None => {
                        return Ok(HandlerResponse::ok(json!({
                            "state": "unavailable",
                            "version": Json::Null,
                            "explanation": "this process has no Python runtime installed, so \
                                            nothing here can say what it would do.",
                            "dir": Json::Null,
                            "site_packages": Json::Null,
                            "bin": Json::Null,
                            "packages": [],
                            "max_inflight": 0,
                            "resident": 0,
                            "threads": 0,
                            "stuck": 0,
                            "max_stuck": 0,
                            "env_error": Json::Null,
                        })));
                    }
                };
                Ok(HandlerResponse::ok(python_status_json(&status)))
            }
        }
    });

    // Settings → System: which build this is, its database, and what the
    // process is doing to the machine (`crate::system`).
    reg.handle(sc_api::handler_ids::GET_SYSTEM_INFO, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                Ok(HandlerResponse::ok(
                    crate::system::system_info(&catalog).await,
                ))
            }
        }
    });

    // Exit for whatever manages the process to start it again. The answer goes
    // out first; the exit follows it (`crate::system::schedule_restart`).
    reg.handle(
        sc_api::handler_ids::RESTART_SERVER,
        move |_ctx| async move {
            crate::system::schedule_restart();
            Ok(HandlerResponse::ok(json!({ "restarting": true })))
        },
    );
}

/// The `text/plain` half of the Email tab's test message.
///
/// It says what it is and where it came from, because the person who receives it
/// may not be the person who pressed the button — a shared `postmaster@` address
/// is a common answer to "who do I send this to" — and an unexplained email from
/// a machine is a support ticket.
const TEST_EMAIL_TEXT: &str = "This is a test message from Saltcorn.\n\nIf you received it, the \
                               SMTP settings saved under Settings \u{2192} Email work.";

/// The `text/html` half. The same sentences: a test message is not the place to
/// find out whether the two bodies say different things.
const TEST_EMAIL_HTML: &str = "<p>This is a test message from Saltcorn.</p><p>If you received \
                               it, the SMTP settings saved under Settings \u{2192} Email \
                               work.</p>";

/// What a help topic is filled in with (`sc_docs::help`'s module docs list the keys):
/// the screen's table with its key fields' targets and the keys pointing back
/// at it, the trigger's event, the admin's own user object, the users table's
/// fields, and the tables and triggers a code body can reach.
///
/// A `table` that does not exist is left out rather than refused: the form the
/// help was opened from may be about a table that is still being named.
async fn help_context(
    catalog: &Catalog,
    user: Option<&User>,
    table: Option<&str>,
    event: Option<&str>,
) -> Result<Json> {
    let field_summary = |f: &DataField| json!({ "name": f.base.name, "label": f.base.label, "type": f.base.type_.name() });
    let tables = catalog.tables()?;
    let mut context = serde_json::Map::new();
    if let Some(table) = table
        .filter(|t| !t.is_empty())
        .and_then(|t| tables.iter().find(|candidate| candidate.name == t))
    {
        let fields: Vec<Json> = table
            .fields
            .iter()
            .filter(|f| !sc_auth::is_api_hidden_column(&table.name, &f.base.name))
            .map(|f| {
                let key_to = match &f.kind {
                    DataFieldKind::Key { target_table, .. } => {
                        let target = tables.iter().find(|t| &t.name == target_table);
                        json!({
                            "table": target_table,
                            "fields": target.map_or_else(Vec::new, |t| {
                                t.fields
                                    .iter()
                                    .filter(|f| !sc_auth::is_api_hidden_column(&t.name, &f.base.name))
                                    .map(field_summary)
                                    .collect()
                            }),
                        })
                    }
                    _ => Json::Null,
                };
                json!({
                    "name": f.base.name,
                    "label": f.base.label,
                    "type": f.base.type_.name(),
                    "required": f.required,
                    "key_to": key_to,
                })
            })
            .collect();
        let inbound: Vec<Json> = SchemaProjection::new(tables.clone())
            .referencing_fields(&table.name)
            .into_iter()
            .map(|(t, f)| json!({ "table": t, "field": f }))
            .collect();
        context.insert(
            "table".into(),
            json!({ "name": table.name, "label": table.label, "fields": fields, "inbound": inbound }),
        );
    }
    if let Some(event) = event.filter(|e| !e.is_empty()) {
        context.insert("event".into(), json!(event));
    }
    if let Some(user) = user {
        let mut object: serde_json::Map<String, Json> = user
            .extra()
            .iter()
            .map(|(k, v)| (k.clone(), sc_types::value_to_json(v)))
            .collect();
        object.insert("id".into(), json!(user.id()));
        object.insert("role".into(), json!(user.role()));
        context.insert("user".into(), Json::Object(object));
    }
    if let Some(users) = tables.iter().find(|t| t.name == USERS_TABLE) {
        let fields: Vec<Json> = users
            .fields
            .iter()
            .filter(|f| !sc_auth::is_api_hidden_column(USERS_TABLE, &f.base.name))
            .filter(|f| f.base.name != "id" && f.base.name != "role")
            .map(field_summary)
            .collect();
        context.insert("user_fields".into(), Json::Array(fields));
    }
    let reachable: Vec<Json> = tables
        .iter()
        .filter(|t| !t.is_hidden())
        .map(|t| json!({ "name": t.name, "label": t.label }))
        .collect();
    context.insert("tables".into(), Json::Array(reachable));
    // Only once a trigger has been saved: the table is created by the first.
    if catalog.get(sc_action::TRIGGERS_TABLE)?.is_some() {
        let triggers: Vec<Json> = list_triggers(catalog)
            .await?
            .into_iter()
            .map(|t| json!(t.name))
            .collect();
        context.insert("triggers".into(), Json::Array(triggers));
    }
    Ok(Json::Object(context))
}

/// Refuse moving the admin UI to `to` (`None`: back to the base domain) when
/// an application is served there, or when there is no base domain to be a
/// subdomain of.
async fn check_admin_move(catalog: &Catalog, apps: &AppMounts, to: Option<&str>) -> Result<()> {
    if to.is_some() && apps.base_domain().is_none() {
        return Err(Error::invalid(
            "this server has no base domain (`--base-domain`), so the admin UI has nothing to \
             be a subdomain of",
        ));
    }
    let wanted = to.unwrap_or(sc_config::ROOT_SUBDOMAIN);
    if let Some(app) = list_applications(catalog)
        .await?
        .into_iter()
        .find(|app| app.subdomain.trim() == wanted)
    {
        return Err(Error::invalid(match to {
            Some(subdomain) => format!(
                "the application `{}` is served on `{subdomain}`; choose another admin subdomain",
                app.name
            ),
            None => format!(
                "the application `{}` is served on the base domain (`@`); give it a subdomain \
                 before moving the admin UI back there",
                app.name
            ),
        }));
    }
    Ok(())
}

/// Where the admin UI is served and whether its certificate is ready: the body
/// of `getAdminAddress`.
fn admin_address_json(apps: &AppMounts) -> Json {
    let host = apps.admin_host();
    let certificate = host.as_deref().map(|host| apps.certificate_status(host));
    json!({
        "base_domain": apps.base_domain(),
        "admin_subdomain": apps.admin().subdomain(),
        "admin_host": host,
        "ready": certificate.as_ref().is_none_or(|status| status.is_ready()),
        "certificate": certificate.map(|status| json!({
            "state": status.state,
            "message": status.message,
        })),
    })
}

/// A stored identity provider as the Authentication tab lists it: the client
/// secret redacted.
fn oauth_provider_json(record: &Attrs) -> Json {
    let text = |key: &str| record.get(key).and_then(Json::as_str).unwrap_or_default();
    let config = sc_types::redact_attrs(&sc_config::oauth_provider_spec(), record);
    json!({
        "name": text(sc_config::authentication::PROVIDER_NAME),
        "label": text(sc_config::authentication::PROVIDER_LABEL),
        "preset": match text(sc_config::authentication::PROVIDER_PRESET) {
            "" => sc_config::authentication::PRESET_CUSTOM,
            preset => preset,
        },
        "enabled": record
            .get(sc_config::authentication::PROVIDER_ENABLED)
            .and_then(Json::as_bool)
            .unwrap_or(true),
        "config": Json::Object(config),
    })
}

/// The `name` a stored provider record is addressed by.
fn oauth_record_name(record: &Attrs) -> &str {
    record
        .get(sc_config::authentication::PROVIDER_NAME)
        .and_then(Json::as_str)
        .unwrap_or_default()
}

/// The `config` object of a create or update body, without its blanks (a
/// cleared box means the declared default).
fn oauth_provider_body(body: &Json) -> Result<Attrs> {
    match body.get("config") {
        Some(Json::Object(config)) => Ok(config
            .iter()
            .filter(|(_, v)| !v.is_null() && v.as_str() != Some(""))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()),
        _ => Err(Error::invalid(
            "`config` should be an object of the provider's settings",
        )),
    }
}

/// The settings screen's whole payload: what may be set, and what is set
/// (matching `settings_schema`).
///
/// The values are [`redacted`](sc_types::redact_attrs) here, at the one place
/// they are serialised, so a secret setting cannot leak through a second reader
/// added later — the rule §11.1 states for an API key, applied to the private
/// key an admin pastes into the TLS section.
async fn settings_json(catalog: &Catalog, locale: &Locale) -> Result<Json> {
    // Three kinds of English on this screen and all three are the server's, so
    // all three are translated here (§16.1, D5): the section's heading, its one
    // sentence, and each setting's label and help text. The help is the one
    // `translate_spec` cannot reach — it hangs off `ConfigDef` rather than off
    // `FormField` — so it is translated where it is serialised, which is here.
    //
    // A section the host has taken out of the admin's hands is not sent at all:
    // with `ssl_mode = "proxy"` pinned, the certificates are the proxy's, and a
    // tab of settings that do nothing is worse than none. The screen posts back
    // only the fields it was sent, so the stored TLS rows are left as they are.
    let hidden = sc_config::hidden_sections(catalog);
    let sections: Vec<Json> = sc_config::config_sections()
        .iter()
        .filter(|section| !hidden.contains(&section.name))
        .map(|section| {
            json!({
                "name": section.name,
                // Not `t!`: the heading is *data* in `sc-config`, so the key is
                // the declared English and the lookup is the same one
                // `translate_spec` makes. The scanner reads `label:` and
                // `description:` where they are declared, which is what puts
                // them in the catalogue in the first place.
                "label": sc_i18n::core::translate(locale, section.label, &[]),
                "description": sc_i18n::core::translate(locale, section.description, &[]),
                "fields": section
                    .fields
                    .iter()
                    .map(|def| {
                        let mut declared = def.field.clone();
                        sc_types::translate_field(&mut declared, locale);
                        let mut field = form_field_json(&declared);
                        if let Json::Object(map) = &mut field {
                            let help = sc_i18n::core::translate(locale, def.help, &[]);
                            map.insert("help".to_owned(), Json::from(help));
                        }
                        field
                    })
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let values = sc_types::redact_attrs(
        &sc_config::config_spec(),
        &sc_config::all_config(catalog).await?,
    );
    Ok(json!({
        "sections": sections,
        "values": Json::Object(values),
        // The keys this host's `feldspar.toml` pins (§13.5): shown read-only,
        // because a save cannot change them.
        "host_keys": sc_config::host_config_keys(catalog),
    }))
}

/// One [`sc_python::PythonStatus`] as the API returns it.
///
/// **The explanation is written here rather than in the SPA**, because which
/// sentence is true depends on how this binary was built and how this process
/// was started — and only the server knows either. Each one names its own
/// remedy, because the three states of §7 have three different ones and an
/// admin who cannot tell them apart goes looking for the wrong fix.
fn python_status_json(status: &sc_python::PythonStatus) -> Json {
    let explanation = match &status.state {
        sc_python::PythonState::NotBuilt => {
            "This server was built without Python support, so it has no interpreter linked \
             in and no setting or flag can add one. A Python-capable server is a separate \
             build (`cargo build -p sc-server --features python`), which needs libpython on \
             the host."
        }
        sc_python::PythonState::Off => {
            "This server has Python support but was started with `--python off`, so it will \
             not start an interpreter. Restart it with `--python auto` (the default) to run \
             Python code."
        }
        sc_python::PythonState::NotInitialised => {
            "This server has Python support and has not needed it yet. The interpreter \
             starts with the first Python trigger that fires, which is why a server that \
             runs no Python pays nothing for it."
        }
        sc_python::PythonState::Running { .. } => {
            "The interpreter is running. Everything Python in this process — every code \
             body — runs in this one interpreter, which is what lets one body wait on a \
             query while the others carry on."
        }
    };
    json!({
        "state": status.state.as_str(),
        "version": status.state.version(),
        "explanation": explanation,
        "dir": status.dir.as_ref().map(|p| p.display().to_string()),
        "site_packages": status.site_packages.as_ref().map(|p| p.display().to_string()),
        "bin": status.bin.as_ref().map(|p| p.display().to_string()),
        "packages": status
            .packages
            .iter()
            .map(|package| json!({ "name": package.name, "version": package.version }))
            .collect::<Vec<_>>(),
        "max_inflight": status.max_inflight,
        "resident": status.resident,
        "threads": status.threads,
        "stuck": status.stuck,
        "max_stuck": status.max_stuck,
        // Null on the ordinary server. When it is not, it is the one thing on
        // the panel that needs acting on: an environment built by a different
        // Python is left off `sys.path` entirely, so everything installed in it
        // is invisible to every body (§9).
        "env_error": status.env_error,
    })
}
