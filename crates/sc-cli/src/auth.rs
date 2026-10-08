//! `feldspar auth token`: a signed-in browser session, written to a file.
//!
//! An application's screens are behind a sign-in (design §13.3 — a scaffolded
//! project's routes require a user unless they say otherwise), so a screenshot
//! taken by a script is a screenshot of the sign-in page. What the script needs
//! is the cookie a browser would have got by signing in, and what it has is a
//! shell on the machine.
//!
//! **It asks no password, because it is not a caller who should have one.** This
//! command runs where the server runs, from a shell holding the primary
//! database's credentials — the authority that can already read every password
//! hash, rewrite any of them, and grant itself any role. Demanding a user's
//! password on top of that protected nothing and cost the operator a secret to
//! keep, so what it does instead is name a *user* — `--email`, `--admin`, or
//! `--role NAME` — and start a session for them.
//!
//! **It writes the session itself, and needs no server at all.** A session is a
//! row in [`_fd_sessions`](sc_auth::SESSIONS_TABLE) (§7.2) — that is what lets
//! two application servers share one — so the authority that can write that
//! table can start a session, and this command is holding exactly that
//! authority. It used to have to ask a *running* server, through a one-time
//! grant, because the store it had to reach lived in that process's memory; with
//! the store in the database there is nothing left to ask for. So `auth token`
//! works against a stopped server, and a screenshot script no longer has to
//! sequence itself behind one coming up.
//!
//! It still forges nothing. The session it makes is
//! [`create_session`](sc_auth::create_session)'s, the same one a sign-in makes,
//! doing exactly what that account can do and no more — so giving an agent a
//! low-privilege account of its own is a real limit and not a gesture.
//!
//! Two files come out, and which one depends on what will read it:
//!
//! - **Playwright's `storageState`** (the default) — a JSON document
//!   `browser.newContext({ storageState })` restores a logged-in browser from.
//! - **A Netscape `cookies.txt`** — what `curl --cookie` and `wget` read.
//!
//! Both are **credentials**, so both are written `0600` and both default names
//! are in the `.gitignore` a scaffolded project ships with.
//!
//! **Two cookies are written, not one.** Mutating requests are refused unless
//! the `x-csrf-token` header echoes the `sc_csrf` cookie (§7.2's double-submit
//! check). That check compares a cookie with a header and nothing else — it is
//! not bound to the session — so the value can be minted here alongside the
//! session, and a `curl` restored from the file can POST. A browser restored
//! from it would be handed one anyway on its first page load; `curl` would not,
//! and its first POST would 403.

use std::path::{Path, PathBuf};

use chrono::Duration;
use sc_auth::{DEFAULT_TTL_HOURS, ROLE_ADMIN, Role, User, create_session, list_roles};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};
use uuid::Uuid;

/// The default file name for [`SessionFormat::Playwright`].
pub const DEFAULT_PLAYWRIGHT_FILE: &str = ".feldspar-session.json";
/// The default file name for [`SessionFormat::Netscape`].
pub const DEFAULT_NETSCAPE_FILE: &str = ".feldspar-cookies.txt";

/// The names of the two cookies a signed-in browser carries.
///
/// Spelled here rather than imported from `sc-server` because the server that
/// reads this file may be a different build: what matters is the wire contract,
/// and the wire contract is these two names. The server's own constants are
/// asserted equal to these in a test, so a rename that broke this cannot land
/// quietly.
const SESSION_COOKIE: &str = "sc_session";
const CSRF_COOKIE: &str = "sc_csrf";

/// Which file to write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionFormat {
    /// Playwright's `storageState` JSON — a browser context restored from a
    /// file. The default: the command exists for screenshots, and this is what
    /// takes them.
    #[default]
    Playwright,
    /// A Netscape cookie file, for `curl --cookie` and friends.
    Netscape,
}

impl SessionFormat {
    /// The format named on the command line.
    pub fn parse(name: &str) -> Result<SessionFormat> {
        match name {
            "playwright" | "json" => Ok(SessionFormat::Playwright),
            "netscape" | "curl" | "txt" => Ok(SessionFormat::Netscape),
            other => Err(Error::config(format!(
                "unknown --format `{other}`; there are `playwright` (a browser's \
                 storageState) and `netscape` (a cookies.txt for curl)"
            ))),
        }
    }

    /// The file name used when `--out` says nothing.
    pub fn default_file(&self) -> &'static str {
        match self {
            SessionFormat::Playwright => DEFAULT_PLAYWRIGHT_FILE,
            SessionFormat::Netscape => DEFAULT_NETSCAPE_FILE,
        }
    }
}

/// Who the session file is *for*: the host the application answers on, and the
/// origin a browser reaches it at.
///
/// The two routinely differ. A server routes an application by the request's
/// `Host` header (§13.2), and a development machine with no DNS for
/// `blog.example.com` reaches it by connecting to the loopback and *saying* that
/// name — which is what `--url` is for.
///
/// [`host`](Self::host) is the load-bearing one: it is the domain the cookies
/// are written for, and the one thing this command must not do is write a
/// session file for the wrong origin. [`url`](Self::url) is now only the origin
/// reported back to the caller (and the port in it), because nothing is
/// connected to any more.
#[derive(Debug, Clone)]
pub struct Target {
    /// The origin the application is reached at, e.g. `http://127.0.0.1:3032`.
    pub url: String,
    /// The `Host` header to send, e.g. `blog.example.com`.
    pub host: String,
    /// Whether the cookies should be marked `secure` — i.e. whether the browser
    /// that will use them is talking to an `https` origin.
    pub secure: bool,
}

impl Target {
    /// The URL a browser should open, which is the app's own host rather than
    /// whatever `--url` connected to.
    pub fn browser_url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        match port_of(&self.url) {
            Some(port) => format!("{scheme}://{}:{port}", self.host),
            None => format!("{scheme}://{}", self.host),
        }
    }
}

/// The port in an origin like `http://127.0.0.1:3032`, when it carries one.
fn port_of(url: &str) -> Option<u16> {
    url.rsplit(':').next()?.trim_end_matches('/').parse().ok()
}

/// One cookie of the pair a signed-in browser carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    /// The cookie's name.
    pub name: String,
    /// Its value — the secret, when the name is `sc_session`.
    pub value: String,
    /// Its path, defaulting to `/`.
    pub path: String,
    /// Whether it carries `HttpOnly`.
    pub http_only: bool,
    /// Whether it carries `Secure`.
    pub secure: bool,
    /// Its `SameSite`, as the server spelled it.
    pub same_site: String,
}

/// The cookies a browser signed in as `token`'s owner would carry, scoped for
/// `target`.
///
/// They are built to match what the server sets, attribute for attribute
/// (`security.rs`'s `build_cookie`): path `/`, `SameSite=Strict`, `HttpOnly` on
/// the session and not on the CSRF cookie — which the SPA has to read — and
/// `Secure` iff the browser will be talking `https`. A cookie written with the
/// wrong attributes is one the browser silently declines to send, which looks
/// exactly like a session that did not work.
fn cookies_for(target: &Target, token: String) -> Vec<Cookie> {
    let cookie = |name: &str, value: String, http_only: bool| Cookie {
        name: name.to_owned(),
        value,
        path: "/".to_owned(),
        http_only,
        secure: target.secure,
        same_site: "Strict".to_owned(),
    };
    vec![
        cookie(SESSION_COOKIE, token, true),
        // Not read back by anything server-side — the double-submit check only
        // compares this cookie with the header echoing it — so any unguessable
        // value is a valid one, and this is the same 256 bits the server mints.
        cookie(CSRF_COOKIE, new_csrf_token(), false),
    ]
}

/// A fresh CSRF token: 256 bits from two v4 UUIDs, hex-encoded, exactly as the
/// server's own `new_csrf_token` makes one.
fn new_csrf_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// What the command reports back about who it signed in as — the same three
/// fields the server's `login` answers with, so the message reads identically
/// whichever made the session.
fn user_summary(user: &User) -> Json {
    json!({
        "id": user.id.to_string(),
        "role": user.role,
        "email": user.get(sc_auth::COL_EMAIL).and_then(|v| match v {
            sc_query::Value::Text(t) => Some(t.clone()),
            _ => None,
        }),
    })
}

/// Render the cookies in `format`, for `host`.
pub fn render(cookies: &[Cookie], host: &str, format: SessionFormat) -> String {
    match format {
        SessionFormat::Playwright => playwright(cookies, host),
        SessionFormat::Netscape => netscape(cookies, host),
    }
}

/// Playwright's `storageState`: the cookies, and no origin-scoped storage.
///
/// `expires: -1` is Playwright's spelling of a **session cookie**, which is what
/// these are: the server sets no `Expires`, and the lifetime that actually
/// governs them is the server-side session's.
fn playwright(cookies: &[Cookie], host: &str) -> String {
    let cookies: Vec<Json> = cookies
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "value": c.value,
                // Host-only, as the server set it: no leading dot, so it is not
                // sent to any other subdomain of the base domain.
                "domain": host,
                "path": c.path,
                "expires": -1,
                "httpOnly": c.http_only,
                "secure": c.secure,
                "sameSite": same_site(&c.same_site),
            })
        })
        .collect();
    let state = json!({ "cookies": cookies, "origins": [] });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&state).unwrap_or_else(|_| state.to_string())
    )
}

/// Playwright accepts exactly `Strict`, `Lax` and `None`.
fn same_site(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "strict" => "Strict",
        "none" => "None",
        _ => "Lax",
    }
}

/// A Netscape cookie file: `curl --cookie`, `wget --load-cookies`.
///
/// The `#HttpOnly_` prefix is curl's convention for an `HttpOnly` cookie, and
/// the session cookie is one — without it curl reads the line as a comment and
/// silently sends no session, which is the failure mode this whole file exists
/// to avoid.
fn netscape(cookies: &[Cookie], host: &str) -> String {
    let mut out = String::from(
        "# Netscape HTTP Cookie File\n\
         # Written by `feldspar auth token`. This is a live session — treat it as a password.\n",
    );
    for c in cookies {
        out.push_str(&format!(
            "{}{host}\tFALSE\t{}\t{}\t0\t{}\t{}\n",
            if c.http_only { "#HttpOnly_" } else { "" },
            c.path,
            if c.secure { "TRUE" } else { "FALSE" },
            c.name,
            c.value,
        ));
    }
    out
}

/// Write `contents` to `path`, readable by nobody else.
///
/// The mode is set **before** the bytes go in, so the file is never briefly
/// world-readable with a session in it.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| Error::file(format!("could not write {}: {e}", path.display())))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| Error::file(format!("could not write {}: {e}", path.display())))?;
    Ok(())
}

/// Where to write, given `--out` and the format's default.
pub fn out_path(out: Option<&str>, format: SessionFormat) -> PathBuf {
    PathBuf::from(out.unwrap_or_else(|| format.default_file()))
}

/// Which user the session is for — the three ways of saying it.
///
/// Three rather than one because the caller is usually a script that does not
/// know the installation's users, and the two questions it can actually answer
/// are "the admin" and "somebody who can see this screen". `--email` remains for
/// the case where the answer is a particular person, which is the one a shared
/// deployment wants: give the agent its own account and name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserSelector {
    /// `--email EMAIL`: exactly this user.
    Email(String),
    /// `--admin`: the first user holding the admin role.
    Admin,
    /// `--role NAME`: the first user holding the role with this name.
    Role(String),
}

impl UserSelector {
    /// How the selection reads in a message about it.
    fn describe(&self) -> String {
        match self {
            UserSelector::Email(email) => format!("--email {email}"),
            UserSelector::Admin => "--admin".to_owned(),
            UserSelector::Role(name) => format!("--role {name}"),
        }
    }
}

/// Resolve a selector against the database, or say why it names nobody.
///
/// Every refusal names what was asked for and what exists instead — an unknown
/// role lists the roles, a role nobody holds says so and gives its number — because
/// the caller is at a shell with no other way to find out, and "not found" on its
/// own would send them to the admin UI to answer a question this command could
/// have answered.
pub async fn resolve_user(catalog: &Catalog, selector: &UserSelector) -> Result<User> {
    match selector {
        UserSelector::Email(email) => sc_auth::load_user_by_email(catalog, email)
            .await?
            .ok_or_else(|| Error::not_found(format!("no user with the email `{email}`"))),
        UserSelector::Admin => {
            let role = role_by_number(catalog, ROLE_ADMIN).await?;
            first_holder(catalog, &role).await
        }
        UserSelector::Role(name) => {
            let role = role_by_name(catalog, name).await?;
            first_holder(catalog, &role).await
        }
    }
}

/// The role `spec` names — by number (`1`) or by name (`admin`, any case) — or an
/// error listing the roles there are.
pub async fn resolve_role(catalog: &Catalog, spec: &str) -> Result<Role> {
    match spec.trim().parse::<u8>() {
        Ok(number) => {
            let roles = list_roles(catalog).await?;
            roles
                .iter()
                .find(|r| r.role == number)
                .cloned()
                .ok_or_else(|| {
                    Error::not_found(format!(
                        "no role {number}. The roles on this server are: {}",
                        role_list(&roles)
                    ))
                })
        }
        Err(_) => role_by_name(catalog, spec).await,
    }
}

/// The role with this name, or an error listing the roles there are.
async fn role_by_name(catalog: &Catalog, name: &str) -> Result<Role> {
    let roles = list_roles(catalog).await?;
    roles
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case(name.trim()))
        .cloned()
        .ok_or_else(|| {
            Error::not_found(format!(
                "no role named `{name}`. The roles on this server are: {}",
                role_list(&roles)
            ))
        })
}

/// The role with this number — used for `--admin`, so its name is the
/// installation's own even when an admin has renamed it.
async fn role_by_number(catalog: &Catalog, number: u8) -> Result<Role> {
    list_roles(catalog)
        .await?
        .into_iter()
        .find(|r| r.role == number)
        .ok_or_else(|| Error::not_found(format!("this database has no role {number}")))
}

/// The first user holding `role`, or an error saying that nobody does.
async fn first_holder(catalog: &Catalog, role: &Role) -> Result<User> {
    sc_auth::first_user_with_role(catalog, role.role)
        .await?
        .ok_or_else(|| {
            Error::not_found(format!(
                "no user has the role `{}` ({}), so there is no session to mint for it",
                role.name, role.role
            ))
        })
}

/// The roles, as an error message lists them: `Admin (1), Public (100)`.
pub fn role_list(roles: &[Role]) -> String {
    roles
        .iter()
        .map(|r| format!("{} ({})", r.name, r.role))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `auth token` was asked for.
#[derive(Debug, Clone)]
pub struct TokenArgs {
    /// `--app`: which application's session this is.
    pub app: String,
    /// `--email` / `--admin` / `--role`: who the session is for.
    pub user: UserSelector,
    /// `--url`: the origin to connect to, when it is not the application's own
    /// (a loopback address on a machine with no DNS for the base domain).
    pub url: Option<String>,
    /// `--base-domain`: overrides the configuration file's.
    pub base_domain: Option<String>,
    /// `--out`: where to write.
    pub out: Option<String>,
    /// `--format`: which file to write.
    pub format: SessionFormat,
}

/// Parse `auth token`'s flags. Unknown ones are refused by name, like every
/// other command's.
///
/// The three ways of naming a user are mutually exclusive and one is required:
/// two of them together is a caller who has not decided, and defaulting to
/// either would sign them in as somebody they did not ask for.
pub fn parse_token_args(args: &[String]) -> Result<TokenArgs> {
    let mut app = None;
    let mut email = None;
    let mut role = None;
    let mut admin = false;
    let mut url = None;
    let mut base_domain = None;
    let mut out = None;
    let mut format = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        // `--admin` is the one flag that takes no value; everything else reads
        // the next argument, and a flag left dangling at the end is an error
        // rather than an empty string.
        if arg == "--admin" {
            admin = true;
            continue;
        }
        let slot = match arg.as_str() {
            "--app" => &mut app,
            "--email" => &mut email,
            "--role" => &mut role,
            "--url" => &mut url,
            "--base-domain" => &mut base_domain,
            "--out" => &mut out,
            "--format" => &mut format,
            other => {
                return Err(Error::config(format!(
                    "unknown auth token argument `{other}`"
                )));
            }
        };
        *slot = Some(
            it.next()
                .ok_or_else(|| Error::config(format!("{arg} needs a value")))?
                .clone(),
        );
    }

    let app = app.ok_or_else(|| {
        Error::config("auth token needs --app: which application's session to mint")
    })?;
    let user = select_user(email, admin, role)?;
    Ok(TokenArgs {
        app,
        user,
        url,
        base_domain,
        out,
        format: match format {
            Some(name) => SessionFormat::parse(&name)?,
            None => SessionFormat::Playwright,
        },
    })
}

/// Exactly one of the three ways of naming a user.
fn select_user(email: Option<String>, admin: bool, role: Option<String>) -> Result<UserSelector> {
    let mut chosen: Vec<UserSelector> = [
        email.map(UserSelector::Email),
        admin.then_some(UserSelector::Admin),
        role.map(UserSelector::Role),
    ]
    .into_iter()
    .flatten()
    .collect();
    if chosen.len() > 1 {
        return Err(Error::config(format!(
            "auth token takes one of --email, --admin and --role, not {}",
            chosen
                .iter()
                .map(UserSelector::describe)
                .collect::<Vec<_>>()
                .join(" and ")
        )));
    }
    chosen.pop().ok_or_else(|| {
        Error::config(
            "auth token needs to know who the session is for: --email EMAIL, \
             --admin (the first admin user), or --role NAME (the first user \
             holding that role)",
        )
    })
}

/// Start a session for `user` and return the cookies that carry it, scoped for
/// `target`, with a summary of who was signed in.
///
/// The one half of the command that needs something other than a file: the
/// database, which is both what authorises this and — since §7.2 put sessions
/// in a table — where the session goes. No server is contacted, so a server that
/// is not running is not an error.
pub async fn session_for(
    catalog: &Catalog,
    target: &Target,
    user: &User,
) -> Result<(Vec<Cookie>, Json)> {
    let token = create_session(catalog, user.id, Duration::hours(DEFAULT_TTL_HOURS)).await?;
    Ok((cookies_for(target, token), user_summary(user)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insecure_target() -> Target {
        Target {
            url: "http://blog.example.com:3032".to_owned(),
            host: "blog.example.com".to_owned(),
            secure: false,
        }
    }

    fn session() -> Cookie {
        cookies_for(&insecure_target(), "abc123".to_owned())
            .into_iter()
            .find(|c| c.name == "sc_session")
            .expect("the session cookie")
    }

    #[test]
    fn the_cookies_carry_the_attributes_the_server_sets() {
        let jar = cookies_for(&insecure_target(), "abc123".to_owned());

        let session = jar
            .iter()
            .find(|c| c.name == "sc_session")
            .expect("session");
        assert_eq!(session.value, "abc123");
        assert_eq!(session.path, "/");
        assert_eq!(session.same_site, "Strict");
        // `HttpOnly`, exactly as the server sets it: nothing in a page has any
        // business reading a session token.
        assert!(session.http_only);
        assert!(!session.secure, "http, so not Secure");

        // The CSRF cookie is the one the SPA has to read, so it must *not* be
        // HttpOnly — a jar that got this backwards would 403 every mutation.
        let csrf = jar.iter().find(|c| c.name == "sc_csrf").expect("csrf");
        assert!(!csrf.http_only);
        assert_eq!(csrf.value.len(), 64);
        assert_ne!(csrf.value, session.value);

        // `Secure` follows the scheme the browser will use, not this process's.
        let secure = Target {
            secure: true,
            ..insecure_target()
        };
        assert!(
            cookies_for(&secure, "abc123".to_owned())
                .iter()
                .all(|c| c.secure)
        );
    }

    #[test]
    fn the_playwright_file_is_a_storage_state_a_browser_can_restore() {
        let text = playwright(&[session()], "blog.example.com");
        let state: Json = serde_json::from_str(&text).expect("valid JSON");
        let cookie = &state["cookies"][0];
        assert_eq!(cookie["name"], "sc_session");
        assert_eq!(cookie["value"], "abc123");
        // Host-only: no leading dot, so the session is not offered to a sibling
        // application on another subdomain.
        assert_eq!(cookie["domain"], "blog.example.com");
        assert_eq!(cookie["httpOnly"], true);
        assert_eq!(cookie["sameSite"], "Strict");
        // Playwright's spelling of "session cookie".
        assert_eq!(cookie["expires"], -1);
        assert!(state["origins"].is_array());
    }

    #[test]
    fn the_netscape_file_marks_an_httponly_cookie_the_way_curl_reads_it() {
        let text = netscape(&[session()], "blog.example.com");
        let line = text
            .lines()
            .find(|l| l.contains("sc_session"))
            .expect("the session line");
        assert!(
            line.starts_with("#HttpOnly_blog.example.com\t"),
            "curl needs the prefix or it reads the line as a comment: {line}"
        );
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 7, "{line}");
        assert_eq!(fields[1], "FALSE", "host-only, not a domain cookie");
        assert_eq!(fields[2], "/");
        assert_eq!(fields[3], "FALSE", "not secure over http");
        assert_eq!(fields[4], "0", "a session cookie");
        assert_eq!(fields[5], "sc_session");
        assert_eq!(fields[6], "abc123");
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_user_is_named_one_of_three_ways() {
        let by_email = parse_token_args(&args(&["--app", "blog", "--email", "a@b.c"]))
            .expect("an email names a user");
        assert_eq!(by_email.user, UserSelector::Email("a@b.c".to_owned()));

        // `--admin` takes no value, and what follows it is still parsed.
        let by_admin =
            parse_token_args(&args(&["--app", "blog", "--admin", "--format", "netscape"]))
                .expect("--admin names a user");
        assert_eq!(by_admin.user, UserSelector::Admin);
        assert_eq!(by_admin.format, SessionFormat::Netscape);

        let by_role =
            parse_token_args(&args(&["--app", "blog", "--role", "Editor"])).expect("a role");
        assert_eq!(by_role.user, UserSelector::Role("Editor".to_owned()));
    }

    #[test]
    fn naming_no_user_or_two_is_refused_by_name() {
        // None of the three: the error lists all three rather than naming the
        // one that used to be mandatory.
        let err = parse_token_args(&args(&["--app", "blog"])).expect_err("no user");
        let message = err.to_string();
        for flag in ["--email", "--admin", "--role"] {
            assert!(message.contains(flag), "{message}");
        }

        // Two of them: a caller who has not decided, and there is no sensible
        // precedence to invent.
        let err = parse_token_args(&args(&["--app", "blog", "--admin", "--email", "a@b.c"]))
            .expect_err("two selectors");
        assert!(err.to_string().contains("--email a@b.c"), "{err}");
        assert!(err.to_string().contains("--admin"), "{err}");
    }

    #[test]
    fn the_roles_are_listed_the_way_an_error_shows_them() {
        let roles = [Role::new(ROLE_ADMIN, "Admin"), Role::new(100, "Public")];
        assert_eq!(role_list(&roles), "Admin (1), Public (100)");
    }

    #[test]
    fn the_output_path_defaults_per_format() {
        assert_eq!(
            out_path(None, SessionFormat::Playwright),
            PathBuf::from(DEFAULT_PLAYWRIGHT_FILE)
        );
        assert_eq!(
            out_path(None, SessionFormat::Netscape),
            PathBuf::from(DEFAULT_NETSCAPE_FILE)
        );
        assert_eq!(
            out_path(Some("/tmp/s.json"), SessionFormat::Playwright),
            PathBuf::from("/tmp/s.json")
        );
    }

    #[test]
    fn an_unknown_format_names_the_two_there_are() {
        let err = SessionFormat::parse("har").expect_err("unknown");
        assert!(err.to_string().contains("playwright"), "{err}");
        assert!(err.to_string().contains("netscape"), "{err}");
    }

    #[test]
    fn the_browser_url_is_the_apps_host_not_the_connection_target() {
        // `--url` points at the loopback; the browser must still be told the
        // name the server routes on.
        let target = Target {
            url: "http://127.0.0.1:3032".to_owned(),
            host: "blog.example.com".to_owned(),
            secure: false,
        };
        assert_eq!(target.browser_url(), "http://blog.example.com:3032");
    }
}
