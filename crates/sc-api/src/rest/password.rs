//! An application's password links: **invite**, **forgot-password** and
//! **set-password** (design §7.2, §13.4).
//!
//! Three endpoints, one mechanism. Each of the first two issues a single-use
//! token ([`sc_auth::issue_password_token`]) and emails a link carrying it to
//! the address the account holds; the third spends the token on a password the
//! holder chooses, and signs them in.
//!
//! - `POST {mount}/invite` — a signed-in user makes an account for somebody
//!   else, **less powerful than themselves**, and the new owner is emailed a
//!   link to choose its password. Projected only where the application's
//!   settings turn it on ([`CFG_ALLOW_INVITE`]), for callers at or above
//!   [`CFG_INVITE_MIN_ROLE`]. Everything about one invitation — the role, the
//!   application the link opens, the message's subject, body and sender — is
//!   the *call's*, because a therapists' app inviting patients to the patients'
//!   app writes that message itself.
//! - `POST {mount}/forgot-password` — public, and always the same answer, so it
//!   cannot be used to learn which addresses have accounts. The message is the
//!   system's own: a caller who is not signed in does not get to write mail.
//! - `POST {mount}/set-password` — public: the token is the credential.
//!
//! # The link
//!
//! `{origin}/set-password#token=…`. The **origin** is the application's, as
//! the request reached it ([`AppDirectory`](crate::AppDirectory)) — an
//! invitation may name another application on the same server, and nothing
//! else: a link is never built to a host a caller typed. The token rides in the
//! **fragment**, which a browser never sends to a server, so it is in no access
//! log and no `Referer`. The page at `/set-password` is the application's own; it
//! reads the fragment and calls `set-password`.

use std::sync::Arc;

use sc_auth::{
    InviteOutcome, NewUser, PasswordTokenPurpose, User, invite_user, issue_password_token,
    load_user_by_email, password_token_issued_within, redeem_password_token, user_email,
};
use sc_catalog::Catalog;
use sc_email::{Email, Mailer, parse_mailbox};
use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::{Map, Value as Json, json};

use crate::auth::{user_summary_json, user_summary_schema};
use crate::provider::{ApiRequest, ApiResponse};
use crate::rows::{column_value, require_object};
use crate::schema::{StructField, TypeSchema};

/// The setting that projects `POST {mount}/invite`. Off unless an admin turns
/// it on: making accounts for other people, and sending them mail, is a
/// decision.
pub const CFG_ALLOW_INVITE: &str = "allow_invite";

/// The setting naming the least powerful role that may invite — a role number,
/// so a caller must have that role *or a more powerful one* (a smaller number).
pub const CFG_INVITE_MIN_ROLE: &str = "invite_min_role";

/// What [`CFG_INVITE_MIN_ROLE`] defaults to: admins only, until somebody says
/// who else.
pub const DEFAULT_INVITE_MIN_ROLE: u8 = sc_auth::ROLE_ADMIN;

/// The path the emailed link opens in the application.
pub const SET_PASSWORD_PAGE: &str = "/set-password";

/// How often one account may be sent a reset link. The endpoint is public, so
/// this is what stops it filling somebody's inbox.
pub const RESET_THROTTLE_SECONDS: i64 = 60;

/// The placeholder a message's subject and bodies are given the link through.
pub const LINK_PLACEHOLDER: &str = "{{link}}";

/// The placeholder for the account's own address.
pub const EMAIL_PLACEHOLDER: &str = "{{email}}";

const INVITE_SUBJECT: &str = "Your new account";
const INVITE_TEXT: &str = "An account has been created for you ({{email}}).\n\n\
    To choose your password and sign in, open this link:\n\n{{link}}\n\n\
    The link works once, and expires in 7 days.\n";
const RESET_SUBJECT: &str = "Reset your password";
const RESET_TEXT: &str = "Somebody asked to reset the password for {{email}}.\n\n\
    To choose a new password, open this link:\n\n{{link}}\n\n\
    The link works once, and expires in an hour. If you did not ask for this, \
    you can ignore this email: your password has not changed.\n";

/// The keys an invitation body may carry. Anything else is refused by name —
/// a misspelt `subjct` silently sending the default message is the failure a
/// caller would never see.
const INVITE_KEYS: [&str; 9] = [
    "email", "role", "app", "fields", "language", "subject", "body", "html", "from",
];

/// Whether an application's stored REST configuration offers invitations, and
/// if so the least powerful role that may send one.
///
/// A role outside `1..=99` is `None` rather than a default: the configuration
/// was checked on save ([`check_invite_config`]), so one out of range now was
/// never accepted, and failing closed projects nothing.
pub fn rest_invite_min_role(config: &sc_types::Attrs) -> Option<u8> {
    if !config
        .get(CFG_ALLOW_INVITE)
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    match config.get(CFG_INVITE_MIN_ROLE).filter(|r| !r.is_null()) {
        None => Some(DEFAULT_INVITE_MIN_ROLE),
        Some(role) => role
            .as_u64()
            .and_then(|r| u8::try_from(r).ok())
            .filter(|r| invite_min_role_allowed(*r)),
    }
}

/// Refuse an invitation role floor that is not a signed-in role.
pub(crate) fn check_invite_config(config: &sc_types::Attrs) -> Result<()> {
    if let Some(role) = config.get(CFG_INVITE_MIN_ROLE).filter(|r| !r.is_null())
        && !role
            .as_u64()
            .and_then(|r| u8::try_from(r).ok())
            .is_some_and(invite_min_role_allowed)
    {
        return Err(Error::invalid(format!(
            "`{CFG_INVITE_MIN_ROLE}` must be a role from 1 to 99: the public role \
             is nobody signed in, and nobody signed in cannot invite"
        )));
    }
    Ok(())
}

fn invite_min_role_allowed(role: u8) -> bool {
    (sc_auth::ROLE_ADMIN..sc_auth::ROLE_PUBLIC).contains(&role)
}

/// The shape of an invitation, as a generated client types it.
pub(crate) fn invite_schema() -> TypeSchema {
    let text = || TypeSchema::optional(TypeSchema::text());
    TypeSchema::struct_of([
        StructField::new("email", TypeSchema::text()),
        StructField::new("role", TypeSchema::int()),
        StructField::new("app", text()),
        StructField::new("fields", TypeSchema::optional(TypeSchema::json())),
        StructField::new("language", text()),
        StructField::new("subject", text()),
        StructField::new("body", text()),
        StructField::new("html", text()),
        StructField::new("from", text()),
    ])
}

/// What `invite` answers with: the account, and whether it was new.
pub(crate) fn invite_output_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("user", user_summary_schema()),
        StructField::new("created", TypeSchema::bool()),
    ])
}

pub(crate) fn forgot_password_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new("email", TypeSchema::text())])
}

pub(crate) fn set_password_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("token", TypeSchema::text()),
        StructField::new("password", TypeSchema::text()),
    ])
}

pub(crate) fn ok_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new("ok", TypeSchema::bool())])
}

/// A message's content before the link is known: subject, bodies, sender.
struct Template {
    from: Option<sc_email::Mailbox>,
    subject: String,
    text: Option<String>,
    html: Option<String>,
}

impl Template {
    fn render(&self, sender: sc_email::Mailbox, to: &str, link: &str) -> Result<Email> {
        let fill = |s: &str| {
            s.replace(LINK_PLACEHOLDER, link)
                .replace(EMAIL_PLACEHOLDER, to)
        };
        let mut email = Email::new(self.from.clone().unwrap_or(sender));
        email.to = vec![parse_mailbox(to).map_err(|e| {
            Error::invalid(format!("the account's address `{to}` is not usable: {e}"))
        })?];
        email.subject = fill(&self.subject);
        email.text = self.text.as_deref().map(fill);
        email.html = self.html.as_deref().map(fill);
        email.check()?;
        Ok(email)
    }
}

/// `POST {mount}/invite`.
pub(crate) async fn invite(
    req: &ApiRequest,
    cat: &Catalog,
    inviter: Option<&User>,
    mailer: Option<&Arc<dyn Mailer>>,
) -> Result<ApiResponse> {
    // The endpoint's `MinRole` already refused an anonymous caller.
    let inviter = inviter.ok_or_else(|| Error::auth("sign in to invite somebody"))?;
    let obj = require_object(&req.body)?;
    if let Some(unknown) = obj.keys().find(|k| !INVITE_KEYS.contains(&k.as_str())) {
        return Err(Error::invalid(format!(
            "an invitation has no field `{unknown}`; it takes {}",
            INVITE_KEYS.join(", ")
        )));
    }
    let email = required_text(obj, "email")?;
    let role = obj
        .get("role")
        .and_then(Json::as_u64)
        .and_then(|r| u8::try_from(r).ok())
        .ok_or_else(|| Error::invalid("`role` must be the new account's role number"))?;
    let language = optional_text(obj, "language")?;
    let extra = user_fields(cat, obj.get("fields"), inviter.role)?;

    // Everything that can be wrong with the *message* is found before an
    // account exists, so a bad template never leaves one behind.
    let origin = link_origin(req, optional_text(obj, "app")?.as_deref())?;
    let template = invite_template(obj)?;
    let mailer = mailer.ok_or_else(no_mailer)?;
    let sender = mailer.sender().await?;

    let new = NewUser {
        email,
        password: String::new(),
        role,
        language,
        extra,
    };
    let (user, token, created) = match invite_user(cat, new, inviter.role).await? {
        InviteOutcome::Created { user, token } => (user, token, true),
        InviteOutcome::Resent { user, token } => (user, token, false),
        InviteOutcome::AlreadyActive => {
            return Ok(ApiResponse::error(
                409,
                "there is already an account with this email address, and it is in \
                 use: its owner can reset their password from the sign-in page",
            ));
        }
    };
    let to = user_email(&user).unwrap_or_default().to_owned();
    let sent = match template.render(sender, &to, &link(&origin, &token)) {
        Ok(message) => mailer.send(&message).await,
        Err(e) => Err(e),
    };
    if let Err(e) = sent {
        // The account is only worth having if its owner can reach it: one
        // made a moment ago goes again, and the caller can simply retry. A
        // resent invitation's account was already there and stays.
        if created {
            sc_auth::delete_password_tokens_for_user(cat, user.id).await?;
            sc_auth::delete_user(cat, user.id).await?;
        }
        return Err(Error::msg(format!(
            "the invitation could not be sent, so no account was made: {e}"
        )));
    }
    Ok(ApiResponse::with_status(
        if created { 201 } else { 200 },
        json!({ "user": user_summary_json(&user), "created": created }),
    ))
}

/// `POST {mount}/forgot-password`.
///
/// Always `200 {"ok": true}`: an unknown address, a disabled account, a second
/// request inside [`RESET_THROTTLE_SECONDS`] and a mail server that is down all
/// look like success, because telling them apart tells a stranger whether an
/// address has an account here. The message itself is sent **after** the
/// answer is decided, off the request, so how long the answer took says
/// nothing either; a failure to send is logged for the admin.
///
/// The one other answer is a `429` (TODO F4.4): too many requests from this
/// IP, or for this address, counted before the address is looked up — so it
/// comes the same for an address with an account and one without.
pub(crate) async fn forgot_password(
    req: &ApiRequest,
    cat: &Catalog,
    mailer: Option<&Arc<dyn Mailer>>,
) -> Result<ApiResponse> {
    let obj = require_object(&req.body)?;
    let email = required_text(obj, "email")?;
    if let Err(limited) = crate::throttle::reset_attempt(req.client_ip, &email) {
        return Ok(ApiResponse::too_many_requests(limited));
    }
    let origin = link_origin(req, None)?;
    let ok = || Ok(ApiResponse::ok(json!({ "ok": true })));
    let Some(mailer) = mailer else {
        eprintln!("feldspar: a password reset was asked for, but no mailer is installed");
        return ok();
    };
    let user = match load_user_by_email(cat, email.trim()).await? {
        Some(user) if !user.is_disabled() => user,
        _ => return ok(),
    };
    let throttle = chrono::Duration::seconds(RESET_THROTTLE_SECONDS);
    if password_token_issued_within(cat, user.id, PasswordTokenPurpose::Reset, throttle).await? {
        return ok();
    }
    let Some(to) = user_email(&user).map(str::to_owned) else {
        return ok();
    };
    let token = issue_password_token(cat, user.id, PasswordTokenPurpose::Reset).await?;
    let template = Template {
        from: None,
        subject: RESET_SUBJECT.to_owned(),
        text: Some(RESET_TEXT.to_owned()),
        html: None,
    };
    let mailer = Arc::clone(mailer);
    let link = link(&origin, &token);
    tokio::spawn(async move {
        let sent = match mailer.sender().await {
            Ok(sender) => match template.render(sender, &to, &link) {
                Ok(message) => mailer.send(&message).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            eprintln!(
                "feldspar: sending a password reset link: {}",
                sc_error::format_chain(&e)
            );
        }
    });
    ok()
}

/// `POST {mount}/set-password`: spend the token, set the password, sign in.
pub(crate) async fn set_password(req: &ApiRequest, cat: &Catalog) -> Result<ApiResponse> {
    let obj = require_object(&req.body)?;
    let token = obj.get("token").and_then(Json::as_str).unwrap_or_default();
    let password = obj
        .get("password")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid("missing or non-string field `password`"))?;
    match redeem_password_token(cat, token, password).await? {
        Some(user) => {
            let summary = user_summary_json(&user);
            Ok(ApiResponse::start_session(user, summary))
        }
        None => Ok(ApiResponse::error(
            400,
            "this link does not work any more: it may have expired or been used \
             already. Ask for a new one",
        )),
    }
}

/// The link a token travels in.
fn link(origin: &str, token: &str) -> String {
    format!("{origin}{SET_PASSWORD_PAGE}#token={token}")
}

/// The origin a link opens: this application's, or the one served on `app`.
fn link_origin(req: &ApiRequest, app: Option<&str>) -> Result<String> {
    let links = req.links.as_ref().ok_or_else(|| {
        Error::config(
            "this request did not arrive at an application, so there is no \
             address to build a link to",
        )
    })?;
    match app.map(str::trim).filter(|a| !a.is_empty()) {
        None => Ok(links.0.own_origin()),
        Some(app) => links.0.app_origin(app).ok_or_else(|| {
            Error::invalid(format!(
                "`app`: no application is served on the subdomain `{app}`"
            ))
        }),
    }
}

/// The invitation's message, from the call where it says so and the default
/// where it does not.
fn invite_template(obj: &Map<String, Json>) -> Result<Template> {
    let from = optional_text(obj, "from")?
        .map(|from| {
            parse_mailbox(&from)
                .map_err(|e| Error::invalid(format!("`from`: `{from}` is not usable: {e}")))
        })
        .transpose()?;
    let subject = optional_text(obj, "subject")?.unwrap_or_else(|| INVITE_SUBJECT.to_owned());
    let mut text = optional_text(obj, "body")?;
    let html = optional_text(obj, "html")?;
    if text.is_none() && html.is_none() {
        text = Some(INVITE_TEXT.to_owned());
    }
    // A message without the link is an account nobody can reach.
    let carries_link =
        |s: &Option<String>| s.as_deref().is_some_and(|s| s.contains(LINK_PLACEHOLDER));
    if !carries_link(&text) && !carries_link(&html) {
        return Err(Error::invalid(format!(
            "the invitation's `body` (or `html`) must contain `{LINK_PLACEHOLDER}`, \
             where the link to choose a password goes"
        )));
    }
    Ok(Template {
        from,
        subject,
        text,
        html,
    })
}

/// The admin-added columns an invitation sets on the new account, coerced to
/// the users table's own types. System columns are refused further down, by
/// the same check every other way of making an account has.
///
/// A column flagged [`ADMIN_ONLY_WRITE`](crate::user_rows::ADMIN_ONLY_WRITE)
/// — the `org_id` a tenancy formula reads — is set by an admin's invitation
/// only. The account is written directly, not through the row layer that
/// holds every other write to that rule, so it is held to it here: otherwise a
/// manager could invite an address of their own into another tenant.
fn user_fields(
    cat: &Catalog,
    fields: Option<&Json>,
    inviter_role: u8,
) -> Result<std::collections::BTreeMap<String, Value>> {
    let mut out = std::collections::BTreeMap::new();
    let Some(fields) = fields.filter(|f| !f.is_null()) else {
        return Ok(out);
    };
    let obj = fields
        .as_object()
        .ok_or_else(|| Error::invalid("`fields` must be an object of the account's columns"))?;
    let users = cat.require(sc_auth::USERS_TABLE)?;
    for (column, json) in obj {
        if sc_auth::is_system_user_column(column) {
            return Err(Error::invalid(format!(
                "`fields`: `{column}` is set by the invitation itself, not as a field"
            )));
        }
        if inviter_role > sc_auth::ROLE_ADMIN
            && crate::user_rows::is_admin_only_write(&users, column)
        {
            return Err(Error::auth(format!(
                "`fields`: `{column}` of an account is set by an admin only"
            )));
        }
        out.insert(column.clone(), column_value(&users, column, json)?);
    }
    Ok(out)
}

fn required_text(obj: &Map<String, Json>, key: &str) -> Result<String> {
    optional_text(obj, key)?
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| Error::invalid(format!("missing or blank field `{key}`")))
}

fn optional_text(obj: &Map<String, Json>, key: &str) -> Result<Option<String>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Error::invalid(format!("field `{key}` must be a string"))),
    }
}

fn no_mailer() -> Error {
    Error::config(
        "this server has no mail transport installed, so an invitation cannot be \
         sent; configure Settings → Email",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Json) -> Map<String, Json> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn an_invitation_template_must_carry_the_link() {
        let t = invite_template(&obj(json!({}))).expect("the default carries it");
        assert!(t.text.unwrap().contains(LINK_PLACEHOLDER));
        assert_eq!(t.subject, INVITE_SUBJECT);

        let err = invite_template(&obj(json!({ "body": "Welcome!" })))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("{{link}}"), "{err}");

        let html = invite_template(&obj(json!({ "html": "<a href=\"{{link}}\">go</a>" })));
        assert!(html.is_ok());

        let from = invite_template(&obj(json!({ "from": "not an address" })));
        assert!(from.is_err());
    }

    #[test]
    fn a_rendered_message_fills_both_placeholders_and_goes_only_to_the_account() {
        let t = invite_template(&obj(json!({
            "subject": "Hello {{email}}",
            "body": "Open {{link}} now",
            "from": "Clinic <clinic@example.com>",
        })))
        .unwrap();
        let sender = parse_mailbox("system@example.com").unwrap();
        let email = t
            .render(
                sender,
                "pat@example.com",
                "https://p.example.com/set-password#token=abc",
            )
            .unwrap();
        assert_eq!(email.subject, "Hello pat@example.com");
        assert_eq!(
            email.text.as_deref(),
            Some("Open https://p.example.com/set-password#token=abc now")
        );
        assert_eq!(email.to.len(), 1);
        assert_eq!(email.from.address, "clinic@example.com");
        assert_eq!(email.from.name.as_deref(), Some("Clinic"));
    }

    #[test]
    fn the_invite_role_floor_is_a_signed_in_role() {
        let mut config = sc_types::Attrs::new();
        assert_eq!(rest_invite_min_role(&config), None, "off by default");
        config.insert(CFG_ALLOW_INVITE.into(), json!(true));
        assert_eq!(
            rest_invite_min_role(&config),
            Some(1),
            "admins only by default"
        );
        config.insert(CFG_INVITE_MIN_ROLE.into(), json!(40));
        assert_eq!(rest_invite_min_role(&config), Some(40));
        assert!(check_invite_config(&config).is_ok());
        config.insert(CFG_INVITE_MIN_ROLE.into(), json!(100));
        assert!(check_invite_config(&config).is_err());
        assert_eq!(rest_invite_min_role(&config), None);
    }
}
