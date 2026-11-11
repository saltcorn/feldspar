//! Users, roles, role grants, credits and API tokens.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    reg.handle(sc_api::handler_ids::LIST_ROLES, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let out: Vec<Json> = list_roles(&catalog).await?.iter().map(role_json).collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_ROLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let number = role_field(obj, "role")?;
                let mut role = RoleDef::new(number, obj.non_empty_str_field("name")?.trim());
                role.description = obj.str_field("description")?.trim().to_owned();
                save_role(&catalog, &role).await?;
                Ok(HandlerResponse::ok(role_json(&role)).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SET_ROLE_SECOND_FACTOR, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let raw = ctx.path_param("role")?;
                let number = raw
                    .parse::<u8>()
                    .ok()
                    .and_then(Role::new)
                    .filter(|r| !r.is_public())
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`{raw}` is not a role between 1 and 99: nobody signs in as the \
                             public role, so it cannot require anything of them"
                        ))
                    })?;
                let required = matches!(ctx.body.get("required"), Some(Json::Bool(true)));
                let mut role = sc_auth::load_role(&catalog, number)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no role {}", number.get())))?;
                if required {
                    role.attributes
                        .insert(sc_auth::ATTR_REQUIRE_SECOND_FACTOR.to_owned(), json!(true));
                } else {
                    role.attributes.remove(sc_auth::ATTR_REQUIRE_SECOND_FACTOR);
                }
                save_role(&catalog, &role).await?;
                let mut out = HandlerResponse::ok(role_json(&role));
                if required {
                    // A session started without the second factor the role now
                    // requires ends, and its next sign-in sets one up (security
                    // review 2026-11, #11) — but not the admin's making the
                    // change, who is here and is asked at their next sign-in.
                    let changed = sc_auth::delete_sessions_lacking_second_factor(
                        &catalog,
                        number,
                        ctx.user().map(User::id),
                    )
                    .await?;
                    out.session = sc_api::SessionAction::CredentialsChanged(changed);
                }
                Ok(out)
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_ROLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let raw = ctx.path_param("role")?;
                let number = raw.parse::<u8>().ok().and_then(Role::new).ok_or_else(|| {
                    Error::invalid(format!("`{raw}` is not a role between 1 and 100"))
                })?;
                let deleted = delete_role(&catalog, number).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_USERS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                // `SELECT *`, not the three columns it used to be: the users
                // table is the one table an admin adds their own columns to
                // (§7.1), and the users screen is where they expect to see them.
                // `User::from_row` drops the password hash on the way past, so
                // "everything" still never includes that.
                let mut select =
                    Select::from(Source::table(USERS_TABLE)).columns(vec![Projection::all()]);
                select.order = vec![OrderBy::asc(Expr::col(COL_EMAIL))];
                let rows = catalog
                    .primary(crate::ADMIN_TASKS)
                    .query(&Statement::from(select))
                    .await?
                    .try_collect()
                    .await?;
                let second_factor = sc_auth::second_factor_users(&catalog).await?;
                let mut out = Vec::with_capacity(rows.len());
                for row in &rows {
                    let user = User::from_row(row)?;
                    let mut json = user_row_json(&user);
                    json["second_factor"] = Json::Bool(second_factor.contains(&user.id()));
                    out.push(json);
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_USER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let users = catalog.require(USERS_TABLE)?;
                let created = create_user_with(
                    &catalog,
                    NewUser {
                        email: obj.non_empty_str_field("email")?.to_owned(),
                        // Absent or blank asks for a generated password, which
                        // comes back in this response and nowhere else.
                        password: optional_str(obj, "password").into(),
                        role: user_role_field(obj)?,
                        language: user_language_field(obj)?,
                        extra: user_extra_values(&users, obj)?,
                    },
                )
                .await?;
                Ok(HandlerResponse::ok(json!({
                    "user": user_row_json(&created.user),
                    "generated_password": created.generated_password,
                }))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_USER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let users = catalog.require(USERS_TABLE)?;
                let changed = update_user(
                    &catalog,
                    id,
                    UserUpdate {
                        email: Some(obj.non_empty_str_field("email")?.to_owned()),
                        role: Some(user_role_field(obj)?),
                        // Blank here means "leave it alone" — the form's password
                        // box is empty because the admin is not changing it, not
                        // because they want the account to have no password.
                        password: Some(optional_str(obj, "password"))
                            .filter(|p| !p.is_empty())
                            .map(sc_error::Secret::new),
                        // Absent leaves it alone; present-and-empty is the
                        // form's "Site default" option, which is a change.
                        language: match obj.contains_key("language") {
                            true => Some(user_language_field(obj)?),
                            false => None,
                        },
                        extra: user_extra_values(&users, obj)?,
                    },
                )
                .await?;
                // A new password ends the sessions the old one started
                // (security review 2026-11, #11), the editing admin's own
                // included when it is their account; so does a role that now
                // requires a second factor the account lacks.
                let body = user_row_json(&changed.user);
                Ok(HandlerResponse::credentials_changed(
                    changed.credentials,
                    body,
                ))
            }
        }
    });

    // Role grants (TODO-payments P3.2). The page shows the base role beside
    // the effective one, every grant the user has had, and lets an admin add a
    // time-limited one by hand or revoke one — a subscription's included, which
    // the provider's next renewal would put back.
    reg.handle(sc_api::handler_ids::LIST_USER_ROLE_GRANTS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let user = sc_auth::load_base_user(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no user with id {id}")))?;
                let effective = sc_auth::effective_role(&catalog, id, user.role()).await?;
                let grants = sc_auth::list_role_grants(&catalog, id).await?;
                Ok(HandlerResponse::ok(json!({
                    "base_role": i64::from(user.role()),
                    "effective_role": i64::from(effective.role),
                    "grants": grants.iter().map(sc_api::auth::role_grant_json).collect::<Vec<_>>(),
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GRANT_USER_ROLE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                if sc_auth::load_base_user(&catalog, id).await?.is_none() {
                    return Err(Error::not_found(format!("no user with id {id}")));
                }
                let role = int_field(obj, "role")?;
                let role = Role::try_from(role)
                    .map_err(|_| Error::invalid(format!("role {role} is not in 1..=99")))?;
                let expires_at = match obj.get("expires_in_days") {
                    None | Some(Json::Null) => None,
                    Some(_) => {
                        let days = int_field(obj, "expires_in_days")?;
                        if days < 1 {
                            return Err(Error::invalid(
                                "`expires_in_days` must be a positive number of days; \
                                 leave it out for a grant that lasts until it is revoked",
                            ));
                        }
                        Some(chrono::Utc::now() + chrono::Duration::days(days))
                    }
                };
                let note = optional_str(obj, "note");
                let grant = sc_auth::grant_role(
                    &catalog,
                    sc_auth::NewRoleGrant {
                        user_id: id,
                        role,
                        source: sc_auth::GRANT_SOURCE_ADMIN.to_owned(),
                        source_ref: uuid::Uuid::new_v4().to_string(),
                        starts_at: None,
                        expires_at,
                        note: (!note.trim().is_empty()).then(|| note.trim().to_owned()),
                    },
                )
                .await?;
                Ok(HandlerResponse::ok(sc_api::auth::role_grant_json(&grant)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REVOKE_USER_ROLE_GRANT, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let grant_id = parse_user_id(ctx.path_param("grant")?)?;
                let not_found = || Error::not_found(format!("user {id} has no grant {grant_id}"));
                let grant = sc_auth::load_role_grant(&catalog, grant_id)
                    .await?
                    .filter(|g| g.user_id == id)
                    .ok_or_else(not_found)?;
                if grant.revoked_at.is_none() {
                    sc_auth::revoke_role_grant_by_id(&catalog, grant.id).await?;
                }
                let grant = sc_auth::load_role_grant(&catalog, grant_id)
                    .await?
                    .ok_or_else(not_found)?;
                Ok(HandlerResponse::ok(sc_api::auth::role_grant_json(&grant)))
            }
        }
    });

    // LLM credits (TODO-payments P6.2). Every answer is the whole view — the
    // balance, the buckets and the latest entries — so the page redraws from
    // what the server now holds rather than guessing at what changed.
    reg.handle(sc_api::handler_ids::GET_USER_CREDITS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                Ok(HandlerResponse::ok(user_credits(&catalog, id).await?))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GRANT_USER_CREDITS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let amount = credit_amount(obj.get("amount"), "amount")?
                    .ok_or_else(|| Error::invalid("`amount` is required: the credits to grant"))?;
                let expires_at = match obj.get("expires_in_days") {
                    None | Some(Json::Null) => None,
                    Some(_) => {
                        let days = int_field(obj, "expires_in_days")?;
                        if days < 1 {
                            return Err(Error::invalid(
                                "`expires_in_days` must be a positive number of days; \
                                 leave it out for credits that never expire",
                            ));
                        }
                        Some(chrono::Utc::now() + chrono::Duration::days(days))
                    }
                };
                let note = optional_str(obj, "note");
                let caller = admin_caller(&ctx.who);
                sc_auth::grant_credits(
                    &catalog,
                    sc_auth::NewCreditGrant {
                        user_id: id,
                        amount,
                        source: sc_auth::CREDIT_SOURCE_ADMIN.to_owned(),
                        key: uuid::Uuid::new_v4().to_string(),
                        kind: sc_auth::CREDIT_SOURCE_ADMIN.to_owned(),
                        expires_at,
                        note: (!note.trim().is_empty()).then(|| note.trim().to_owned()),
                        rollover: None,
                    },
                    Authority::Caller(&caller),
                )
                .await?;
                Ok(HandlerResponse::ok(user_credits(&catalog, id).await?))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SET_USER_CREDIT_THRESHOLD, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let threshold = credit_amount(obj.get("threshold"), "threshold")?;
                let caller = admin_caller(&ctx.who);
                sc_auth::set_low_balance_threshold(
                    &catalog,
                    id,
                    threshold,
                    Authority::Caller(&caller),
                )
                .await?;
                Ok(HandlerResponse::ok(user_credits(&catalog, id).await?))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_USER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                refuse_self(&ctx, id, "delete")?;
                if !delete_user(&catalog, id).await? {
                    return Err(Error::not_found(format!("no user with id {id}")));
                }
                // The account is gone, so the sessions holding it must go with
                // it — not because they would still work (a session resolves by
                // reading the user, and there is no user), but because leaving
                // rows that resolve to nobody is leaving litter.
                Ok(HandlerResponse::end_user_sessions(
                    id,
                    json!({ "deleted": true }),
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SET_USER_DISABLED, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let disabled = bool_field(obj, "disabled")?;
                if disabled {
                    refuse_self(&ctx, id, "disable")?;
                }
                let changed = set_user_disabled(&catalog, id, disabled).await?;
                let body = user_row_json(&changed.user);
                // Disabling stops them signing in again; ending the sessions is
                // what stops the one they are in the middle of.
                Ok(HandlerResponse::credentials_changed(
                    changed.credentials,
                    body,
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::FORCE_LOGOUT_USER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                // Read it first, so forcing a logout on somebody who is not there
                // is a 404 rather than a cheerful "ended 0 sessions".
                if load_user(&catalog, id).await?.is_none() {
                    return Err(Error::not_found(format!("no user with id {id}")));
                }
                Ok(HandlerResponse::end_user_sessions(
                    id,
                    json!({ "ok": true }),
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::BECOME_USER, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let user = load_user(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no user with id {id}")))?;
                // A disabled account cannot be signed into by its owner, so it
                // cannot be signed into over their head either.
                if user.is_disabled() {
                    return Err(Error::invalid(
                        "that account is disabled; enable it before becoming it",
                    ));
                }
                let body = user_summary_json(&user);
                // A swap, not an addition: starting a session ends the one the
                // request arrived with, so the admin session does not stay live
                // behind the one it just became. Past the second factor, named:
                // the admin signed in with theirs, and could reset this one's.
                let admin = ctx.user().map(User::id).unwrap_or_default();
                Ok(HandlerResponse::start_verified_session(
                    sc_auth::FullyAuthenticated::waived(
                        user,
                        sc_auth::SecondFactorWaiver::BecomeUser { admin },
                    ),
                    body,
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::RESET_USER_SECOND_FACTOR, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                if load_user(&catalog, id).await?.is_none() {
                    return Err(Error::not_found(format!("no user with id {id}")));
                }
                let removed = sc_auth::disable_second_factor(&catalog, id).await?;
                // Whoever is signed in to it now may be why it is being reset:
                // its sessions end whether or not one was on yet.
                let _turned_off = removed.changed;
                Ok(HandlerResponse::end_user_sessions(
                    id,
                    json!({ "reset": removed.existed }),
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SET_RANDOM_PASSWORD, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let password = random_password();
                let changed = set_user_password(&catalog, id, &password).await?;
                // The one response that carries a readable password. A new
                // password ends the sessions the old one started (security
                // review 2026-11, #11): `set_user_password` raised the epoch
                // and dropped their rows, and this drops this node's cached
                // copies and live sockets.
                let body = json!({
                    "email": changed.user.get(COL_EMAIL).and_then(sc_query::Value::as_text).unwrap_or_default(),
                    "password": password,
                });
                Ok(HandlerResponse::credentials_changed(changed.credentials, body))
            }
        }
    });

    // Mail a user the password an admin just set. The recipient is the
    // account's own stored address, never one from the request, so the
    // endpoint cannot be pointed at anybody else; the password and the sign-in
    // URL come from the caller because the server holds only the hash. The
    // transport's error is returned verbatim, as the test message's is.
    reg.handle(sc_api::handler_ids::SEND_CREDENTIALS_EMAIL, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_user_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let password = obj.non_empty_str_field("password")?.to_owned();
                let url = obj.non_empty_str_field("url")?.trim().to_owned();
                let user = load_user(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no user with id {id}")))?;
                let to = user
                    .get(COL_EMAIL)
                    .and_then(sc_query::Value::as_text)
                    .map(str::to_owned)
                    .ok_or_else(|| Error::invalid("this user has no email address to send to"))?;
                let settings = sc_config::EmailSettings::load(&catalog).await?.ok_or_else(|| {
                    Error::invalid(
                        "no SMTP server is configured: set an SMTP host in Settings → Email \
                         before sending a password by email",
                    )
                })?;
                let mailer = sc_email::SmtpMailer::new(&settings)?;
                let mut email = sc_email::Email::new(settings.from.clone());
                email.to = sc_email::parse_recipients(&to)?;
                email.subject = "Your sign-in details".to_owned();
                email.text = Some(credentials_email_text(&url, &to, &password));
                mailer.send(&email).await?;
                Ok(HandlerResponse::ok(json!({ "sent_to": to })))
            }
        }
    });

    //
    // The credential the administration MCP server authenticates with. Three
    // handlers, and every decision in them is `sc_auth::tokens`'s already: the
    // plaintext exists in one response and nowhere else, the list is made of a
    // value that has no field for it, and a revoked row stays.
    //
    // What is decided *here* is that a mint is for the **calling admin**. The
    // row could name anybody; this API will not, because minting a credential
    // that runs as somebody else hands out their authority without their
    // knowledge.

    reg.handle(sc_api::handler_ids::LIST_API_TOKENS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let out: Vec<Json> = list_api_tokens(&catalog)
                    .await?
                    .iter()
                    .map(api_token_json)
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_API_TOKEN, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                // The endpoint is `admin()`, so there is a user; saying so out
                // loud rather than defaulting, because "which user does this
                // token run as?" is the whole of §13.6's authentication.
                let user = ctx
                    .user()
                    .ok_or_else(|| {
                        Error::auth(t!(
                            ctx.locale,
                            "minting an API token needs a signed-in admin"
                        ))
                    })?;

                // The seven flags, read and written back through `sc-api::mcp` —
                // the one place that knows what they are called. A token
                // defaults every flag on; a stored `grants` is explicit in all
                // seven, so the row records what was agreed to rather than what
                // a later default would say.
                let submitted = match obj.get("grants") {
                    None | Some(Json::Null) => Attrs::new(),
                    Some(Json::Object(map)) => map.clone(),
                    Some(_) => return Err(Error::invalid("`grants` must be an object")),
                };
                sc_api::mcp::validate_token_flags(&submitted)?;
                let grants = sc_api::mcp::token_flags_from_submitted(&submitted);

                let expires_at = match obj.get("expires_in_days") {
                    None | Some(Json::Null) => None,
                    Some(_) => {
                        let days = int_field(obj, "expires_in_days")?;
                        if days <= 0 {
                            return Err(Error::invalid(
                                "`expires_in_days` must be a positive number of days;                                  leave it out for a token that does not expire",
                            ));
                        }
                        Some(chrono::Utc::now() + chrono::Duration::days(days))
                    }
                };

                let minted = mint_api_token(
                    &catalog,
                    NewApiToken {
                        user_id: user.id(),
                        label: obj.str_field("label")?.to_owned(),
                        grants,
                        expires_at,
                    },
                )
                .await?;
                // The one response that carries the credential — the sibling of
                // `setRandomPassword`, and for the same reason.
                Ok(HandlerResponse::ok(json!({
                    "token": api_token_json(&minted.token),
                    "secret": minted.secret,
                }))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REVOKE_API_TOKEN, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_api_token_id(ctx.path_param("id")?)?;
                let revoked = revoke_api_token(&catalog, id).await?;
                Ok(HandlerResponse::ok(json!({ "revoked": revoked })))
            }
        }
    });
}

/// One stored API token on the wire (§13.6).
///
/// Made of [`sc_auth::ApiToken`], which has no field for the token and none for
/// its hash — so "shown once" survives this function without this function
/// having to remember it.
fn api_token_json(token: &sc_auth::ApiToken) -> Json {
    json!({
        "id": token.id,
        "user_id": token.user_id,
        "label": token.label,
        "grants": Json::Object(token.grants.clone()),
        "created_at": token.created_at,
        "expires_at": token.expires_at,
        "last_used_at": token.last_used_at,
        "revoked_at": token.revoked_at,
        // Computed here rather than in the screen: "would this authenticate
        // right now?" is clock arithmetic over two nullable columns, and one
        // answer is better than one per client.
        "live": token.is_live(chrono::Utc::now()),
    })
}

fn parse_api_token_id(raw: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid API token id")))
}

fn parse_user_id(raw: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid user id")))
}

/// The `role` of a user body, narrowed to the `1..=100` byte.
fn user_role_field(obj: &Map<String, Json>) -> Result<Role> {
    let role = int_field(obj, "role")?;
    Role::try_from(role).map_err(|_| Error::invalid(format!("role {role} is not in 1..=100")))
}

/// The `language` of a user body: a BCP-47 tag, canonicalised, or `None` for
/// "whatever the request negotiates" (§16.1).
///
/// Parsed rather than trusted, so `pt-br` is stored as `pt-BR` and a value that
/// is not a language tag is refused where the admin can fix it. It is **not**
/// checked against the enabled set: an admin setting up an account for somebody
/// who reads French before enabling French is doing the two halves of one job in
/// the order they thought of them, and a stored tag nothing enables simply falls
/// through the negotiation.
fn user_language_field(obj: &Map<String, Json>) -> Result<Option<String>> {
    let tag = match obj.get("language") {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(tag)) => tag.trim(),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`language` should be a language tag, got {other}"
            )));
        }
    };
    if tag.is_empty() {
        return Ok(None);
    }
    Ok(Some(sc_i18n::Locale::parse(tag)?.as_str().to_owned()))
}

/// The `extra` bag of a user body: the columns the admin has added to the users
/// table, coerced to each column's type.
///
/// Unknown columns and system columns are both refused — the first by
/// [`rows::column_value`], which is the same coercion every row write goes
/// through, and the second by `sc-auth`, since each system column has its own way
/// in. A body with no `extra` at all is an empty bag, not an error: a users table
/// nobody has added a column to is the ordinary case.
fn user_extra_values(users: &Table, obj: &Map<String, Json>) -> Result<BTreeMap<String, Value>> {
    let Some(extra) = obj.get("extra") else {
        return Ok(BTreeMap::new());
    };
    if extra.is_null() {
        return Ok(BTreeMap::new());
    }
    let extra = require_object(extra)?;
    let mut out = BTreeMap::new();
    for (column, json) in extra {
        if sc_auth::is_system_user_column(column) {
            return Err(Error::invalid(format!(
                "`{column}` is not an admin-defined field of the users table"
            )));
        }
        out.insert(column.clone(), rows::column_value(users, column, json)?);
    }
    Ok(out)
}

/// Refuse an operation an admin is aiming at their own account.
///
/// Disabling or deleting yourself is a request to lock yourself out mid-session,
/// and the users screen is reached only *through* that session. Nothing about it
/// is unsafe for the system — an installation can have another admin do it — so
/// this is a guard rail, not an access rule, and it names the account rather than
/// pretending the operation does not exist.
fn refuse_self<W: crate::handler::Who>(
    ctx: &crate::handler::HandlerCtx<W>,
    id: uuid::Uuid,
    verb: &str,
) -> Result<()> {
    if ctx.user().is_some_and(|u| u.id() == id) {
        return Err(Error::invalid(format!(
            "you cannot {verb} the account you are signed in as"
        )));
    }
    Ok(())
}

/// A user's LLM credits as the admin user page shows them: the balance, every
/// bucket, and the latest entries.
async fn user_credits(catalog: &Catalog, id: uuid::Uuid) -> Result<Json> {
    /// The entries the page lists; older ones are a REST read of the table.
    const RECENT_ENTRIES: u64 = 50;
    if sc_auth::load_base_user(catalog, id).await?.is_none() {
        return Err(Error::not_found(format!("no user with id {id}")));
    }
    let balance = sc_auth::credit_balance(catalog, id).await?;
    let buckets = sc_auth::list_credit_buckets(catalog, id).await?;
    let entries = sc_auth::list_credit_entries(catalog, id, RECENT_ENTRIES).await?;
    Ok(sc_api::auth::user_credits_json(
        &balance, &buckets, &entries,
    ))
}

/// An amount of credits from a body: a decimal string (what the admin client
/// sends, exactly) or a number. Absent or null is `None`.
fn credit_amount(value: Option<&Json>, key: &str) -> Result<Option<rust_decimal::Decimal>> {
    let parsed = match value {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(text)) if text.trim().is_empty() => return Ok(None),
        Some(Json::String(text)) => text.trim().parse::<rust_decimal::Decimal>().ok(),
        Some(Json::Number(n)) => n.to_string().parse::<rust_decimal::Decimal>().ok(),
        Some(_) => None,
    };
    parsed
        .map(Some)
        .ok_or_else(|| Error::invalid(format!("`{key}` must be an amount of credits, like 12.5")))
}

/// The body of the message `sendCredentialsEmail` sends: the same three lines
/// the admin UI's dialog shows and copies (`credentialsText`), under one
/// sentence saying what they are. Plain text only, so nothing in the password
/// or the URL needs escaping.
fn credentials_email_text(url: &str, email: &str, password: &str) -> String {
    format!(
        "A password has been set for your account. Sign in with these details:\n\n\
         Site:     {url}\nEmail:    {email}\nPassword: {password}\n"
    )
}
