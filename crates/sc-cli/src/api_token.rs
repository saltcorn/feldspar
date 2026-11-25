//! `feldspar api-token create|list|revoke`: the API tokens an application's
//! users call its API with (TODO AT1), from a terminal.
//!
//! A user mints their own on the application (`/_feldspar/api-tokens`); this is
//! the operator's way to mint one **for** somebody — a service account, a
//! script set up before its owner has signed in — over the same
//! `sc_auth::tokens` functions, so a token minted here is listed and revoked on
//! the application's Security tab and its owner's page like any other.
//!
//! **Who it runs as.** `--email` names the user exactly, `--admin` the first
//! administrator. Unlike an MCP token, any role will do: the application's own
//! access settings and its tables' rules decide what the token may do.
//!
//! **The secret is the only thing on stdout**, as with `mcp-token create`, so
//! `token=$(feldspar api-token create …)` captures it and nothing else.

use sc_auth::ApiToken;
use sc_error::{Error, Result};
use uuid::Uuid;

use crate::auth::UserSelector;

/// What `api-token create` was asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateArgs {
    /// `--app`: the subdomain of the application the token is for.
    pub app: String,
    /// `--label`: what the token is called in the lists and the audit line.
    pub label: String,
    /// `--email` / `--admin`: who the token runs as.
    pub user: UserSelector,
    /// `--expires-in-days`: `None` for a token that does not lapse.
    pub expires_in_days: Option<i64>,
    /// `--url`: the application's origin, for the example line.
    pub url: Option<String>,
}

/// Parse `api-token create`'s flags. Unknown ones are refused by name.
pub fn parse_create(args: &[String]) -> Result<CreateArgs> {
    let mut app = None;
    let mut label = None;
    let mut email = None;
    let mut admin = false;
    let mut expires = None;
    let mut url = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--admin" {
            admin = true;
            continue;
        }
        let slot = match arg.as_str() {
            "--app" => &mut app,
            "--label" => &mut label,
            "--email" => &mut email,
            "--expires-in-days" => &mut expires,
            "--url" => &mut url,
            other => {
                return Err(Error::config(format!(
                    "unknown api-token create argument `{other}`; it takes --app, --label, \
                     --email or --admin, --expires-in-days and --url"
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
        Error::config("api-token create needs --app SUBDOMAIN: the application the token calls")
    })?;
    let label = label.ok_or_else(|| {
        Error::config(
            "api-token create needs --label: what the token is called in the lists, and \
             in every log line a call under it writes",
        )
    })?;
    let user = match (email, admin) {
        (Some(_), true) => {
            return Err(Error::config(
                "api-token create takes one of --email and --admin, not both",
            ));
        }
        (Some(email), false) => UserSelector::Email(email),
        (None, true) => UserSelector::Admin,
        (None, false) => {
            return Err(Error::config(
                "api-token create needs --email EMAIL or --admin: the token runs as that user",
            ));
        }
    };
    let expires_in_days = expires
        .map(|raw| match raw.trim().parse::<i64>() {
            Ok(days) if days > 0 => Ok(days),
            _ => Err(Error::config(format!(
                "--expires-in-days takes a positive number of days, not `{raw}`; \
                 leave it out for a token that does not expire"
            ))),
        })
        .transpose()?;
    Ok(CreateArgs {
        app,
        label,
        user,
        expires_in_days,
        url,
    })
}

/// What `api-token list` was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListArgs {
    /// `--app`: one application's tokens; every application's without it.
    pub app: Option<String>,
    /// `--json`.
    pub json: bool,
}

/// Parse `api-token list`'s flags.
pub fn parse_list(args: &[String]) -> Result<ListArgs> {
    let mut out = ListArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => out.json = true,
            "--app" => {
                out.app = Some(
                    it.next()
                        .ok_or_else(|| Error::config("--app needs a value"))?
                        .clone(),
                );
            }
            other => {
                return Err(Error::config(format!(
                    "unknown api-token list argument `{other}`; it takes --app SUBDOMAIN and --json"
                )));
            }
        }
    }
    Ok(out)
}

/// Parse `api-token revoke ID`.
pub fn parse_revoke(args: &[String]) -> Result<Uuid> {
    match args {
        [id] => Uuid::parse_str(id).map_err(|_| {
            Error::config(format!(
                "`{id}` is not a token id; `feldspar api-token list` shows them"
            ))
        }),
        [] => Err(Error::config(
            "api-token revoke needs the token's id; `feldspar api-token list` shows them",
        )),
        _ => Err(Error::config("api-token revoke takes one id")),
    }
}

/// The example a minted token is shown with: who it is, from a script.
pub fn curl_line(origin: &str, secret: &str) -> String {
    format!(
        "curl -H \"Authorization: Bearer {secret}\" {}/api/whoami",
        origin.trim_end_matches('/')
    )
}

/// The origin an application is reached at when `--url` does not say: its
/// subdomain of `localhost`, on the port the configuration serves.
pub fn default_origin(subdomain: &str, port: u16) -> String {
    format!("http://{subdomain}.localhost:{port}")
}

/// One token as `api-token list --json` prints it: never the secret.
pub fn token_json(token: &ApiToken, app: &str, email: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": token.id,
        "app": app,
        "user_id": token.user_id,
        "email": email,
        "label": token.label,
        "created_at": token.created_at,
        "expires_at": token.expires_at,
        "last_used_at": token.last_used_at,
        "revoked_at": token.revoked_at,
        "state": crate::mcp_token::state(token, chrono::Utc::now()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn create_needs_an_app_a_label_and_a_user() {
        let ok = parse_create(&args(&[
            "--app",
            "crm",
            "--label",
            "export",
            "--email",
            "a@b.c",
            "--expires-in-days",
            "7",
        ]))
        .unwrap();
        assert_eq!(ok.app, "crm");
        assert_eq!(ok.user, UserSelector::Email("a@b.c".into()));
        assert_eq!(ok.expires_in_days, Some(7));

        for (bad, says) in [
            (&["--label", "x", "--admin"][..], "--app"),
            (&["--app", "crm", "--admin"][..], "--label"),
            (&["--app", "crm", "--label", "x"][..], "--email"),
            (
                &["--app", "crm", "--label", "x", "--admin", "--email", "a"][..],
                "not both",
            ),
            (
                &[
                    "--app",
                    "crm",
                    "--label",
                    "x",
                    "--admin",
                    "--expires-in-days",
                    "0",
                ][..],
                "positive",
            ),
            (
                &["--app", "crm", "--label", "x", "--admin", "--allow-drop"][..],
                "unknown",
            ),
        ] {
            let err = parse_create(&args(bad)).unwrap_err().to_string();
            assert!(err.contains(says), "{bad:?}: {err}");
        }
    }

    #[test]
    fn list_and_revoke_parse() {
        assert_eq!(
            parse_list(&args(&["--app", "crm", "--json"])).unwrap(),
            ListArgs {
                app: Some("crm".into()),
                json: true
            }
        );
        assert!(parse_list(&args(&["--all"])).is_err());
        let id = Uuid::new_v4();
        assert_eq!(parse_revoke(&args(&[&id.to_string()])).unwrap(), id);
        assert!(parse_revoke(&args(&["nope"])).is_err());
        assert!(parse_revoke(&[]).is_err());
    }

    #[test]
    fn the_example_calls_the_application_as_the_user() {
        assert_eq!(
            curl_line("http://crm.localhost:3032/", "fsat_x"),
            "curl -H \"Authorization: Bearer fsat_x\" http://crm.localhost:3032/api/whoami"
        );
        assert_eq!(default_origin("crm", 3032), "http://crm.localhost:3032");
    }
}
