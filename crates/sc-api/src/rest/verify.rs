//! **Email verification** for an application's self-service sign-ups (TODO
//! A2): `POST {mount}/verify-email` and `POST {mount}/resend-verification`,
//! and the link `signup` emails.
//!
//! An application turns it on with [`CFG_VERIFY_EMAIL`]. Then:
//!
//! - `signup` makes the account as before (with the role
//!   [`CFG_NEW_USER_ROLE`](super::CFG_NEW_USER_ROLE) names, address unproven,
//!   signed in) and **emails a link** to the address. If the message cannot be
//!   sent, the account is removed again and the caller is told, so they can
//!   simply try again.
//! - Until the address is proven, the application **serves the account as**
//!   [`CFG_UNVERIFIED_ROLE`] — the cap `max_role` applies, applied to this
//!   account only (`sc_app::cap_role`). The account's own role is never
//!   changed, so anything that proves the address gives full access at once:
//!   this link, a password reset or invitation link, an identity provider
//!   sign-in. Turning the setting off serves every account as its role again.
//! - `verify-email` spends the link's token
//!   ([`sc_auth::redeem_email_verification`]) and signs its holder in. Public,
//!   and one of the sign-in endpoints: the token is the credential, and the
//!   link may be opened on a phone that has never signed in.
//! - `resend-verification` sends a fresh link to the signed-in caller's own
//!   address, at most once a [`RESEND_THROTTLE_SECONDS`].
//!
//! The link is `{origin}/verify-email#token=…`, built like the password links
//! ([`super::password`]): the application's own origin, the token in the
//! fragment so no server log or `Referer` sees it. The page at `/verify-email`
//! is the application's; it reads the fragment and calls `verify-email`.

use std::sync::Arc;

use sc_auth::{
    PasswordTokenPurpose, User, issue_password_token, password_token_issued_within,
    redeem_email_verification, user_email,
};
use sc_catalog::Catalog;
use sc_email::Mailer;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::password::{LINK_PLACEHOLDER, Template, link_origin, ok_schema};
use crate::auth::{user_summary_json, user_summary_schema};
use crate::provider::{ApiRequest, ApiResponse};
use crate::rows::require_object;
use crate::schema::{StructField, TypeSchema};

/// The setting that turns verification on. Off unless an admin turns it on:
/// an application that never asked for it must not start sending mail, nor
/// serve anybody as somebody less.
pub const CFG_VERIFY_EMAIL: &str = "verify_email";

/// The setting naming the role an account whose address is unproven is served
/// as.
pub const CFG_UNVERIFIED_ROLE: &str = "unverified_role";

/// What [`CFG_UNVERIFIED_ROLE`] defaults to: the public role, so an unverified
/// account sees what somebody who is not signed in sees — and can still ask
/// for another link, being signed in.
pub const DEFAULT_UNVERIFIED_ROLE: u8 = sc_auth::ROLE_PUBLIC;

/// The setting for the message's subject.
pub const CFG_VERIFY_EMAIL_SUBJECT: &str = "verify_email_subject";

/// The setting for the message's text, which must carry [`LINK_PLACEHOLDER`].
pub const CFG_VERIFY_EMAIL_BODY: &str = "verify_email_body";

/// The path the emailed link opens in the application.
pub const VERIFY_EMAIL_PAGE: &str = "/verify-email";

/// How often one account may be sent a verification link.
pub const RESEND_THROTTLE_SECONDS: i64 = 60;

const VERIFY_SUBJECT: &str = "Confirm your email address";
const VERIFY_TEXT: &str = "Thank you for signing up with {{email}}.\n\n\
    To confirm that this address is yours, open this link:\n\n{{link}}\n\n\
    The link works once, and expires in 48 hours. If you did not sign up, \
    you can ignore this email.\n";

/// What an application's verification settings say, read once at mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailVerification {
    /// The role an unproven account is served as.
    pub unverified_role: u8,
    /// The message's subject, when the settings name one.
    pub subject: Option<String>,
    /// The message's text, when the settings give one.
    pub body: Option<String>,
}

impl EmailVerification {
    /// The message, before the link is known.
    fn template(&self) -> Template {
        Template {
            from: None,
            subject: self
                .subject
                .clone()
                .unwrap_or_else(|| VERIFY_SUBJECT.to_owned()),
            text: Some(self.body.clone().unwrap_or_else(|| VERIFY_TEXT.to_owned())),
            html: None,
        }
    }

    /// `user` as the application serves them: capped at
    /// [`unverified_role`](EmailVerification::unverified_role) while their
    /// address is unproven.
    pub fn serve_as(&self, user: &User) -> u8 {
        if user.email_unproven() {
            user.role.max(self.unverified_role)
        } else {
            user.role
        }
    }
}

/// An application's verification settings, or `None` where it does not
/// require verification.
///
/// A role outside `2..=100` is the public role rather than an error: the
/// configuration was checked on save ([`check_verify_config`]), and failing
/// closed serves an unproven account as nobody.
pub fn rest_email_verification(config: &Attrs) -> Option<EmailVerification> {
    if !config
        .get(CFG_VERIFY_EMAIL)
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let unverified_role = match config.get(CFG_UNVERIFIED_ROLE).filter(|r| !r.is_null()) {
        None => DEFAULT_UNVERIFIED_ROLE,
        Some(role) => role
            .as_u64()
            .and_then(|r| u8::try_from(r).ok())
            .filter(|r| unverified_role_allowed(*r))
            .unwrap_or(sc_auth::ROLE_PUBLIC),
    };
    let text = |key: &str| {
        config
            .get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .filter(|s| !s.trim().is_empty())
    };
    Some(EmailVerification {
        unverified_role,
        subject: text(CFG_VERIFY_EMAIL_SUBJECT),
        body: text(CFG_VERIFY_EMAIL_BODY),
    })
}

/// The role an application serves an unproven account as, where it requires
/// verification — what `sc_app::cap_role` reads.
pub fn rest_unverified_role(config: &Attrs) -> Option<u8> {
    rest_email_verification(config).map(|v| v.unverified_role)
}

/// Refuse verification settings that cannot mean anything: an unverified role
/// that is the administrator's, or a message without the link.
pub(crate) fn check_verify_config(config: &Attrs) -> Result<()> {
    if let Some(role) = config.get(CFG_UNVERIFIED_ROLE).filter(|r| !r.is_null())
        && !role
            .as_u64()
            .and_then(|r| u8::try_from(r).ok())
            .is_some_and(unverified_role_allowed)
    {
        return Err(Error::invalid(format!(
            "`{CFG_UNVERIFIED_ROLE}` must be a role from 2 to 100: an account \
             nobody has verified cannot be served as an administrator"
        )));
    }
    if let Some(body) = config.get(CFG_VERIFY_EMAIL_BODY).and_then(Json::as_str)
        && !body.trim().is_empty()
        && !body.contains(LINK_PLACEHOLDER)
    {
        return Err(Error::invalid(format!(
            "`{CFG_VERIFY_EMAIL_BODY}` must contain `{LINK_PLACEHOLDER}`, where the \
             link that confirms the address goes"
        )));
    }
    Ok(())
}

fn unverified_role_allowed(role: u8) -> bool {
    (sc_auth::ROLE_ADMIN + 1..=sc_auth::ROLE_PUBLIC).contains(&role)
}

pub(crate) fn verify_email_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("token", TypeSchema::text()),
        crate::auth::remember_field(),
    ])
}

pub(crate) fn verify_output_schema() -> TypeSchema {
    user_summary_schema()
}

pub(crate) fn resend_output_schema() -> TypeSchema {
    ok_schema()
}

/// The link a verification token travels in.
fn link(origin: &str, token: &str) -> String {
    format!("{origin}{VERIFY_EMAIL_PAGE}#token={token}")
}

/// Issue a verification token for `user` and email the link to their address.
///
/// Everything that can be wrong before a token exists — no mail transport, no
/// application to link to, an address the transport will not take — is found
/// first, so a failed send leaves at most a token nobody holds.
pub(crate) async fn send_verification(
    req: &ApiRequest,
    cat: &Catalog,
    user: &User,
    mailer: Option<&Arc<dyn Mailer>>,
    settings: &EmailVerification,
) -> Result<()> {
    let mailer = mailer.ok_or_else(no_mailer)?;
    let origin = link_origin(req, None)?;
    let to = user_email(user)
        .ok_or_else(|| Error::invalid("the account has no email address to verify"))?
        .to_owned();
    let sender = mailer.sender().await?;
    let token = issue_password_token(cat, user.id, PasswordTokenPurpose::Verify).await?;
    let message = settings
        .template()
        .render(sender, &to, &link(&origin, &token))?;
    mailer.send(&message).await
}

/// Check, before an account is made, that a verification link could be sent
/// for it: a sign-up whose link cannot go anywhere would make an account that
/// can never get its access.
pub(crate) fn check_can_send(req: &ApiRequest, mailer: Option<&Arc<dyn Mailer>>) -> Result<()> {
    mailer.ok_or_else(no_mailer)?;
    link_origin(req, None).map(|_| ())
}

/// `POST {mount}/verify-email`: spend the token, prove the address, sign in.
pub(crate) async fn verify_email(req: &ApiRequest, cat: &Catalog) -> Result<ApiResponse> {
    let obj = require_object(&req.body)?;
    let token = obj.get("token").and_then(Json::as_str).unwrap_or_default();
    match redeem_email_verification(cat, token).await? {
        Some(user) => {
            let summary = user_summary_json(&user);
            Ok(ApiResponse::start_session_remembering(
                user,
                summary,
                crate::auth::remember(&req.body),
            ))
        }
        None => Ok(ApiResponse::error(
            400,
            "this link does not work any more: it may have expired or been used \
             already. Sign in and ask for a new one",
        )),
    }
}

/// `POST {mount}/resend-verification`: a fresh link to the caller's own
/// address.
///
/// `200 {"ok": true}` when a link was sent, and also when there is nothing to
/// do — the address is already proven, or a link went less than
/// [`RESEND_THROTTLE_SECONDS`] ago. The caller is the account's owner, so a
/// message that could not be sent is an error they are told about.
pub(crate) async fn resend_verification(
    req: &ApiRequest,
    cat: &Catalog,
    user: Option<&User>,
    mailer: Option<&Arc<dyn Mailer>>,
    settings: &EmailVerification,
) -> Result<ApiResponse> {
    // The endpoint's `LoggedIn` already refused an anonymous caller.
    let user = user.ok_or_else(|| Error::auth("sign in to ask for a verification link"))?;
    let ok = || Ok(ApiResponse::ok(json!({ "ok": true })));
    if !user.email_unproven() {
        return ok();
    }
    let throttle = chrono::Duration::seconds(RESEND_THROTTLE_SECONDS);
    if password_token_issued_within(cat, user.id, PasswordTokenPurpose::Verify, throttle).await? {
        return ok();
    }
    send_verification(req, cat, user, mailer, settings)
        .await
        .map_err(|e| {
            Error::msg(format!(
                "the verification link could not be sent: {}",
                sc_error::format_chain(&e)
            ))
        })?;
    ok()
}

fn no_mailer() -> Error {
    Error::config(
        "this server has no mail transport installed, so a verification link \
         cannot be sent; configure Settings → Email, or turn off email \
         verification for this application",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_is_off_unless_turned_on() {
        let mut config = Attrs::new();
        assert_eq!(rest_email_verification(&config), None);
        config.insert(CFG_VERIFY_EMAIL.into(), json!(true));
        let on = rest_email_verification(&config).unwrap();
        assert_eq!(on.unverified_role, 100, "the public role by default");
        assert_eq!(on.template().subject, VERIFY_SUBJECT);
        config.insert(CFG_UNVERIFIED_ROLE.into(), json!(90));
        assert_eq!(rest_unverified_role(&config), Some(90));
    }

    #[test]
    fn the_unverified_role_is_never_the_administrators() {
        let mut config = Attrs::new();
        config.insert(CFG_UNVERIFIED_ROLE.into(), json!(1));
        assert!(check_verify_config(&config).is_err());
        config.insert(CFG_VERIFY_EMAIL.into(), json!(true));
        assert_eq!(
            rest_unverified_role(&config),
            Some(100),
            "fails closed when an unchecked value gets through"
        );
        config.insert(CFG_UNVERIFIED_ROLE.into(), json!(2));
        assert!(check_verify_config(&config).is_ok());
    }

    #[test]
    fn a_custom_message_must_carry_the_link() {
        let mut config = Attrs::new();
        config.insert(CFG_VERIFY_EMAIL_BODY.into(), json!("Welcome aboard!"));
        let err = check_verify_config(&config).unwrap_err().to_string();
        assert!(err.contains("{{link}}"), "{err}");
        config.insert(CFG_VERIFY_EMAIL_BODY.into(), json!("Confirm: {{link}}"));
        assert!(check_verify_config(&config).is_ok());
        config.insert(CFG_VERIFY_EMAIL.into(), json!(true));
        config.insert(CFG_VERIFY_EMAIL_SUBJECT.into(), json!("Hi {{email}}"));
        let settings = rest_email_verification(&config).unwrap();
        let sender = sc_email::parse_mailbox("system@example.com").unwrap();
        let email = settings
            .template()
            .render(
                sender,
                "pat@example.com",
                &link("https://a.example.com", "tok"),
            )
            .unwrap();
        assert_eq!(email.subject, "Hi pat@example.com");
        assert_eq!(
            email.text.as_deref(),
            Some("Confirm: https://a.example.com/verify-email#token=tok")
        );
    }
}
