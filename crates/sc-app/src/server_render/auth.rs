//! **Signing in on a server-rendered site** (TODO W4.2): the three form posts
//! `<SignIn />`, `<SignUp />` and `<SignOut />` (`feldspar:components`) send,
//! answered here rather than by the site's code body.
//!
//! | route | what it does |
//! |---|---|
//! | `GET /_auth/sign-in` | a plain sign-in page, for a site that admits nobody signed out |
//! | `POST /_auth/sign-in` | email and password, then on to `next` signed in |
//! | `POST /_auth/sign-up` | an account, where the application's REST API offers sign-up |
//! | `POST /_auth/sign-out` | the session ends |
//!
//! **The one session path.** A sign-in here is a [`SessionAction`] on the
//! response, exactly as the Saltcorn UI framework's `/auth/login` and an
//! application's REST `login` are: the router applies it, so the second
//! factor, an account the application does not let in (F9), the shared or
//! per-host cookie and "Remember me" (A1) are all decided where they are for
//! every other way of signing in, and nothing is checked twice. The page's
//! code never sees the session cookie — the runtime keeps `sc_*` cookies away
//! from pages — so it cannot set one either.
//!
//! **Before the site's own code.** These routes are answered for a caller the
//! application does not admit too, as nobody: they are its sign-in surface, the
//! one thing such a caller may reach. The posts have passed the CSRF check
//! before they arrive, so a form on another site cannot sign a browser in.
//!
//! **Where to go next.** `next` (where the person was going) and `back` (the
//! page the form is on, for a failure) must be paths on this host
//! ([`RedirectTarget`]); anything else is `/`. A failure goes back to the form's
//! page with `?sign_in=…` or `?sign_up=…`, which the component reads to say
//! what went wrong — so the form works with no JavaScript at all.
//!
//! **Sign-up** follows the application's REST API settings ("Allow sign-up",
//! "Role of a signed-up account"): one switch for every way an account is made.
//! An application whose REST API requires email verification is refused here
//! (`?sign_up=verify`): the verification email is the REST API's to send.

use bytes::Bytes;
use sc_api::SessionAction;
use sc_catalog::Catalog;
use sc_error::{ErrorKind, Result};
use sc_http::RedirectTarget;
use sc_types::Role;
use serde_json::Value as Json;

use crate::framework::RequestBody;
use crate::{AppRequest, AppResponse};
use sc_api::Method;

/// The paths this module answers start with this.
pub const AUTH_PREFIX: &str = "/_auth/";

/// The sign-in route.
pub const SIGN_IN: &str = "/_auth/sign-in";
/// The sign-up route.
pub const SIGN_UP: &str = "/_auth/sign-up";
/// The sign-out route.
pub const SIGN_OUT: &str = "/_auth/sign-out";

/// The hidden field a form sends beside its "Remember me" box, so an unticked
/// box — which a form does not send at all — is told apart from a form that
/// never offered one.
const REMEMBER_SHOWN: &str = "remember_shown";

/// What a site's sign-up is, read from the application when it is mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SignUp {
    /// The role a new account gets, or `None` when sign-up is off.
    pub role: Option<Role>,
    /// The REST API requires email verification, which this route cannot send.
    pub verification: bool,
}

impl SignUp {
    /// The application's REST settings, as its `signup` endpoint reads them.
    pub fn of(app: &crate::Application) -> SignUp {
        let rest = app
            .apis
            .iter()
            .find(|api| api.provider == sc_api::REST_PROVIDER);
        SignUp {
            role: rest.and_then(|api| sc_api::rest_signup_role(&api.config)),
            verification: app.unverified_role().is_some(),
        }
    }

    /// Whether a `<SignUp />` form is shown at all.
    pub fn offered(self) -> bool {
        self.role.is_some() && !self.verification
    }
}

/// Answer one request under [`AUTH_PREFIX`].
pub async fn handle(req: &AppRequest, cat: &Catalog, sign_up: SignUp) -> Result<AppResponse> {
    let out = match (req.path.as_str(), &req.method) {
        (SIGN_IN, Method::Get) => sign_in_page(req),
        (SIGN_IN, Method::Post) => sign_in(req, cat).await?,
        (SIGN_UP, Method::Post) => self::sign_up(req, cat, sign_up).await?,
        (SIGN_OUT, Method::Post) => {
            let mut out = see_other(&target(req, "next"));
            out.session = SessionAction::End;
            out
        }
        (SIGN_IN | SIGN_UP | SIGN_OUT, _) => AppResponse::method_not_allowed(),
        _ => AppResponse::not_found(),
    };
    // One browser's answer, always.
    Ok(out.header("cache-control", "no-store"))
}

/// `POST /_auth/sign-in`: the credentials through `sc_auth::authenticate` —
/// the check every other sign-in makes, for any role — throttled under the
/// same keys, and a wrong password, an unknown email and a disabled account
/// the one answer.
async fn sign_in(req: &AppRequest, cat: &Catalog) -> Result<AppResponse> {
    let email = field(req, "email");
    let password = field(req, "password");
    if sc_api::throttle::login_attempt(req.client_ip, &email).is_err() {
        return Ok(back(req, "sign_in", "throttled"));
    }
    match sc_auth::authenticate(cat, &email, &password).await? {
        Some(user) => {
            let mut out = see_other(&target(req, "next"));
            out.session = SessionAction::start(user, remembered(req));
            Ok(out)
        }
        None => {
            sc_api::throttle::login_failed(&email);
            Ok(back(req, "sign_in", "failed"))
        }
    }
}

/// `POST /_auth/sign-up`: an account with the role the application's REST
/// settings give one, under the rules its `signup` endpoint has — throttled
/// per address, a password that is not blank, an address with no account —
/// and signed in.
async fn sign_up(req: &AppRequest, cat: &Catalog, settings: SignUp) -> Result<AppResponse> {
    let Some(role) = settings.role else {
        return Ok(back(req, "sign_up", "closed"));
    };
    if settings.verification {
        return Ok(back(req, "sign_up", "verify"));
    }
    if sc_api::throttle::signup_attempt(req.client_ip).is_err() {
        return Ok(back(req, "sign_up", "throttled"));
    }
    let email = field(req, "email").trim().to_owned();
    let password = field(req, "password");
    if email.is_empty() || password.is_empty() {
        return Ok(back(req, "sign_up", "missing"));
    }
    if has_field(req, "password_repeat") && field(req, "password_repeat") != password {
        return Ok(back(req, "sign_up", "mismatch"));
    }
    if sc_auth::load_user_by_email(cat, &email).await?.is_some() {
        return Ok(back(req, "sign_up", "exists"));
    }
    // Recorded as unproven: the visitor chose the address (security review #41).
    match sc_auth::sign_up_user(cat, &email, &password, role).await {
        Ok(user) => {
            let signing_in =
                sc_auth::PasswordVerified::by(cat, user, sc_auth::FirstFactor::SignUp).await?;
            let mut out = see_other(&target(req, "next"));
            out.session = SessionAction::start(signing_in, remembered(req));
            Ok(out)
        }
        // An address the users table refuses, say.
        Err(e) if e.kind() == ErrorKind::Application => Ok(back(req, "sign_up", "invalid")),
        Err(e) => Err(e),
    }
}

/// `GET /_auth/sign-in`: the form `<SignIn />` renders, as a page of its own —
/// what a site that admits nobody signed out links to, since none of its own
/// pages can be shown to such a visitor.
fn sign_in_page(req: &AppRequest) -> AppResponse {
    let next = RedirectTarget::or_root(req.query.get("next").map(String::as_str));
    let problem = match req.query.get("sign_in").map(String::as_str) {
        Some("failed") => "<p class=\"error\" role=\"alert\">Incorrect email or password.</p>",
        Some("throttled") => {
            "<p class=\"error\" role=\"alert\">Too many sign-in attempts. Try again in a few minutes.</p>"
        }
        _ => "",
    };
    let back = format!("{SIGN_IN}?next={}", form_encode(next.as_str()));
    AppResponse::html(
        200,
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <meta name=\"robots\" content=\"noindex\"><title>Sign in</title></head><body>\
             <main><h1>Sign in</h1>{problem}\
             <form method=\"post\" action=\"{SIGN_IN}\">\
             <input type=\"hidden\" name=\"_csrf\" value=\"{csrf}\">\
             <input type=\"hidden\" name=\"next\" value=\"{next}\">\
             <input type=\"hidden\" name=\"back\" value=\"{back}\">\
             <p><label>Email <input type=\"email\" name=\"email\" autocomplete=\"username\" required autofocus></label></p>\
             <p><label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\" required></label></p>\
             <p><label><input type=\"checkbox\" name=\"remember\" value=\"1\" checked> Remember me</label>\
             <input type=\"hidden\" name=\"{REMEMBER_SHOWN}\" value=\"1\"></p>\
             <p><button type=\"submit\">Sign in</button></p></form></main></body></html>",
            csrf = escape(&req.csrf_token),
            next = escape(next.as_str()),
            back = escape(&back),
        ),
    )
}

/// A `303 See Other` to `target`: the answer to a form post.
fn see_other(target: &RedirectTarget) -> AppResponse {
    AppResponse::with_status(303, "text/plain; charset=utf-8", Bytes::new())
        .header("location", target.as_str())
}

/// Back to the form's page, saying what went wrong as `key=why`.
fn back(req: &AppRequest, key: &str, why: &str) -> AppResponse {
    see_other(&target(req, "back").with_param(key, why))
}

/// The form field `name` as a path on this host, or `/`.
fn target(req: &AppRequest, name: &str) -> RedirectTarget {
    let value = field(req, name);
    RedirectTarget::or_root(Some(value.as_str()).filter(|v| !v.is_empty()))
}

/// Whether the sign-in asks to stay signed in after the browser closes: the
/// form's box, ticked, or yes for a form that offered no box.
fn remembered(req: &AppRequest) -> bool {
    if has_field(req, "remember") {
        return !matches!(field(req, "remember").as_str(), "" | "0" | "off" | "false");
    }
    !has_field(req, REMEMBER_SHOWN)
}

/// The request body's field `name` (the first, for a form), or `""`.
fn field(req: &AppRequest, name: &str) -> String {
    match &req.body {
        RequestBody::Form(pairs) => pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default(),
        RequestBody::Json(body) => match body.get(name) {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Bool(b)) => b.to_string(),
            Some(Json::Number(n)) => n.to_string(),
            _ => String::new(),
        },
        RequestBody::Empty => String::new(),
    }
}

/// Whether the request body has a field `name` at all.
fn has_field(req: &AppRequest, name: &str) -> bool {
    match &req.body {
        RequestBody::Form(pairs) => pairs.iter().any(|(k, _)| k == name),
        RequestBody::Json(body) => body.get(name).is_some(),
        RequestBody::Empty => false,
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `text` as a form-encoded query value.
fn form_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(path: &str, fields: &[(&str, &str)]) -> AppRequest {
        let mut req = AppRequest::new(Method::Post, path);
        req.body = RequestBody::Form(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        req
    }

    fn location(out: &AppResponse) -> &str {
        out.headers
            .iter()
            .find(|(k, _)| k == "location")
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[test]
    fn next_and_back_stay_on_this_host() {
        let req = post(
            SIGN_IN,
            &[
                ("next", "https://evil.example/"),
                ("back", "//evil.example"),
            ],
        );
        assert_eq!(target(&req, "next").as_str(), "/");
        assert_eq!(
            location(&back(&req, "sign_in", "failed")),
            "/?sign_in=failed"
        );
        let req = post(
            SIGN_IN,
            &[("next", "/feed?tab=new"), ("back", "/login?x=1")],
        );
        assert_eq!(target(&req, "next").as_str(), "/feed?tab=new");
        assert_eq!(
            location(&back(&req, "sign_in", "failed")),
            "/login?x=1&sign_in=failed"
        );
    }

    #[test]
    fn remember_me_is_the_box_when_the_form_has_one() {
        assert!(remembered(&post(SIGN_IN, &[])), "no box offered");
        assert!(
            !remembered(&post(SIGN_IN, &[(REMEMBER_SHOWN, "1")])),
            "unticked"
        );
        assert!(remembered(&post(
            SIGN_IN,
            &[(REMEMBER_SHOWN, "1"), ("remember", "1")]
        )));
    }

    #[test]
    fn sign_up_follows_the_rest_settings() {
        let mut app = crate::Application::new("Site", "site", crate::FrameworkRef::new("astro"));
        assert!(!SignUp::of(&app).offered(), "no REST API, no sign-up");
        let mut rest = crate::ApiConfig::new("rest", "/api");
        rest.config
            .insert(sc_api::REST_CFG_ALLOW_SIGNUP.to_owned(), Json::Bool(true));
        app = app.with_api(rest);
        let sign_up = SignUp::of(&app);
        assert_eq!(
            sign_up.role,
            Some(Role::of(80)),
            "the REST API's default role"
        );
        assert!(sign_up.offered());
    }

    #[test]
    fn the_sign_in_page_carries_the_token_and_where_to_go() {
        let mut req = AppRequest::get(SIGN_IN);
        req.csrf_token = "tok\"".to_owned();
        req.query.insert("next".to_owned(), "/feed".to_owned());
        req.query.insert("sign_in".to_owned(), "failed".to_owned());
        let page = sign_in_page(&req);
        let html = String::from_utf8_lossy(&page.body);
        assert!(
            html.contains("name=\"_csrf\" value=\"tok&quot;\""),
            "{html}"
        );
        assert!(html.contains("name=\"next\" value=\"/feed\""), "{html}");
        assert!(
            html.contains("name=\"back\" value=\"/_auth/sign-in?next=/feed\""),
            "{html}"
        );
        assert!(html.contains("Incorrect email or password."), "{html}");
    }
}
