//! `feldspar mcp-token create|list|revoke`: the administration MCP server's
//! bearer tokens (design §13.6), from a terminal.
//!
//! The same three things the **Settings → Development** panel does, over the
//! same `sc_auth::tokens` functions, with the same seven grants (and
//! `allow_all`, which holds every grant a later version adds) read and written
//! through `sc_api::mcp` — so a token minted here is indistinguishable from one
//! minted at the screen, and is listed and revoked there like any other.
//!
//! **Who the token runs as.** The screen mints for the admin standing at it.
//! A terminal has no signed-in user, but it holds the primary database, which is
//! more authority than any admin session — so, like `auth token`, it names one:
//! `--email` for that admin exactly, or `--admin` for the first one. The user must
//! be an administrator, because a token for anybody else would be minted and then
//! refused on its first call.
//!
//! **The secret is the only thing on stdout**, so it can be captured
//! (`token=$(feldspar mcp-token create …)`) and nothing else ends up beside it.
//! Everything an operator reads — the id to revoke it by, the grants, the
//! `claude mcp add` line — goes to stderr.

use chrono::{DateTime, Duration, Utc};
use sc_auth::ApiToken;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::auth::UserSelector;

/// The command-line flag for one of the seven grant keys: `allow_drop` is
/// `--allow-drop`, and `--no-allow-drop` turns it off.
pub fn grant_flag(key: &str) -> String {
    format!("--{}", key.replace('_', "-"))
}

/// What `mcp-token create` was asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateArgs {
    /// `--label`: what the token is called in the list and in the audit line.
    pub label: String,
    /// `--email` / `--admin`: whose authority the token runs with.
    pub user: UserSelector,
    /// `--expires-in-days`: `None` for a token that does not lapse.
    pub expires_in_days: Option<i64>,
    /// The grant flags that were given, keyed as stored. A flag left out is
    /// absent here and defaults on (`sc_api::mcp::token_flags_from_submitted`).
    pub grants: Attrs,
    /// `--url`: the server origin the `claude mcp add` line points at.
    pub url: Option<String>,
    /// `--name`: what the server is registered as in Claude Code.
    pub name: String,
}

/// Parse `mcp-token create`'s flags. Unknown ones are refused by name.
pub fn parse_create(args: &[String]) -> Result<CreateArgs> {
    let mut label = None;
    let mut email = None;
    let mut admin = false;
    let mut expires = None;
    let mut url = None;
    let mut name = None;
    let mut grants = Attrs::new();
    let mut it = args.iter();
    'args: while let Some(arg) = it.next() {
        if arg == "--admin" {
            admin = true;
            continue;
        }
        // Every grant this binary knows and every one a later version adds,
        // stored on the token as `allow_all`; `--no-allow-…` still takes single
        // grants back out.
        if arg == "--allow-all" {
            let key = sc_api::mcp::ALLOW_ALL;
            if grants.insert(key.to_owned(), Json::Bool(true)).is_some() {
                return Err(Error::config("--allow-all is given more than once"));
            }
            continue;
        }
        for key in sc_api::mcp::FLAG_KEYS {
            let on = grant_flag(key);
            let set = if *arg == on {
                true
            } else if *arg == format!("--no-{}", &on[2..]) {
                false
            } else {
                continue;
            };
            if grants.insert(key.to_owned(), Json::Bool(set)).is_some() {
                return Err(Error::config(format!(
                    "{on} is given more than once; say it once, as {on} or --no-{}",
                    &on[2..]
                )));
            }
            continue 'args;
        }
        let slot = match arg.as_str() {
            "--label" => &mut label,
            "--email" => &mut email,
            "--expires-in-days" => &mut expires,
            "--url" => &mut url,
            "--name" => &mut name,
            other => {
                return Err(Error::config(format!(
                    "unknown mcp-token create argument `{other}`; the grants are --allow-all, {}",
                    sc_api::mcp::FLAG_KEYS
                        .iter()
                        .map(|k| grant_flag(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        };
        *slot = Some(
            it.next()
                .ok_or_else(|| Error::config(format!("{arg} needs a value")))?
                .clone(),
        );
    }

    let label = label.ok_or_else(|| {
        Error::config(
            "mcp-token create needs --label: what the token is called in the list, \
             and in every log line a call under it writes",
        )
    })?;
    let user = match (email, admin) {
        (Some(_), true) => {
            return Err(Error::config(
                "mcp-token create takes one of --email and --admin, not both",
            ));
        }
        (Some(email), false) => UserSelector::Email(email),
        (None, true) => UserSelector::Admin,
        (None, false) => {
            return Err(Error::config(
                "mcp-token create needs --email EMAIL or --admin: the token runs with \
                 that administrator's authority",
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
        label,
        user,
        expires_in_days,
        grants,
        url,
        name: name.unwrap_or_else(|| "feldspar".to_owned()),
    })
}

/// The grants as they will be stored: every key explicit, a flag not given
/// defaulting **on**, and `allow_all` if `--allow-all` was given — by the same
/// function the admin API uses.
pub fn normalised_grants(submitted: &Attrs) -> Result<Attrs> {
    sc_api::mcp::validate_token_flags(submitted)?;
    Ok(sc_api::mcp::token_flags_from_submitted(submitted))
}

/// When a token minted now for `days` days lapses.
pub fn expiry(days: Option<i64>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    days.map(|d| now + Duration::days(d))
}

/// The grants a token holds, as the flags that would give them: `allow_create
/// allow_edit` and so on, with the ones it does not hold left out — and
/// `allow_all` first when the token also holds every grant added later.
pub fn describe_grants(grants: &Attrs) -> String {
    let flags = sc_api::mcp::token_flags(grants);
    let held: Vec<&str> = std::iter::once(sc_api::mcp::ALLOW_ALL)
        .chain(sc_api::mcp::FLAG_KEYS)
        .filter(|key| flags.get(*key).and_then(Json::as_bool) == Some(true))
        .collect();
    match held.is_empty() {
        true => "(none)".to_owned(),
        false => held.join(" "),
    }
}

/// The line that registers the server with Claude Code. The same line the
/// admin screen builds (`ui/admin/src/mcpTokens.ts`).
pub fn claude_mcp_add_line(origin: &str, name: &str, secret: &str) -> String {
    format!(
        "claude mcp add --transport http {name} {}/mcp --header \"Authorization: Bearer {secret}\"",
        origin.trim_end_matches('/')
    )
}

/// What a token's state is, in a word: `live`, `revoked` or `expired`.
pub fn state(token: &ApiToken, now: DateTime<Utc>) -> &'static str {
    match (token.revoked_at, token.is_live(now)) {
        (Some(_), _) => "revoked",
        (None, true) => "live",
        (None, false) => "expired",
    }
}

/// Parse the id `mcp-token revoke` names.
pub fn parse_revoke(args: &[String]) -> Result<Uuid> {
    match args {
        [id] => Uuid::parse_str(id.trim()).map_err(|_| {
            Error::config(format!(
                "`{id}` is not a token id; `feldspar mcp-token list` shows them"
            ))
        }),
        [] => Err(Error::config(
            "mcp-token revoke needs the token's id; `feldspar mcp-token list` shows them",
        )),
        [_, extra, ..] => Err(Error::config(format!(
            "unknown mcp-token revoke argument `{extra}`; it takes one id"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn every_grant_has_a_flag_and_its_negation() {
        let parsed = parse_create(&args(&[
            "--label",
            "laptop",
            "--admin",
            "--allow-drop",
            "--no-allow-triggers",
            "--expires-in-days",
            "90",
        ]))
        .unwrap();
        assert_eq!(parsed.label, "laptop");
        assert_eq!(parsed.user, UserSelector::Admin);
        assert_eq!(parsed.expires_in_days, Some(90));
        assert_eq!(parsed.name, "feldspar");

        let stored = normalised_grants(&parsed.grants).unwrap();
        // Given: drop on, triggers off. Left out: on, every one of them.
        assert_eq!(stored["allow_drop"], Json::Bool(true));
        assert_eq!(stored["allow_triggers"], Json::Bool(false));
        assert_eq!(stored["allow_create"], Json::Bool(true));
        assert_eq!(stored["allow_edit"], Json::Bool(true));
        assert_eq!(stored["allow_access_changes"], Json::Bool(true));
        assert_eq!(stored["allow_applications"], Json::Bool(true));
        assert_eq!(stored["allow_modules"], Json::Bool(true));
        assert_eq!(
            describe_grants(&stored),
            "allow_create allow_edit allow_drop allow_access_changes \
             allow_applications allow_modules allow_history"
        );
    }

    #[test]
    fn allow_all_is_stored_and_single_grants_can_still_be_taken_out() {
        let parsed = parse_create(&args(&[
            "--label",
            "agent",
            "--admin",
            "--allow-all",
            "--no-allow-drop",
        ]))
        .unwrap();
        let stored = normalised_grants(&parsed.grants).unwrap();
        assert_eq!(stored["allow_all"], Json::Bool(true));
        assert_eq!(stored["allow_drop"], Json::Bool(false));
        assert_eq!(stored["allow_modules"], Json::Bool(true));
        assert_eq!(
            describe_grants(&stored),
            "allow_all allow_create allow_edit allow_access_changes allow_triggers \
             allow_applications allow_modules allow_history"
        );
        // Without it, nothing claims the grants a later version adds.
        let plain = parse_create(&args(&["--label", "x", "--admin"])).unwrap();
        assert_eq!(
            normalised_grants(&plain.grants).unwrap()["allow_all"],
            Json::Bool(false)
        );
        // Twice is refused, like any other flag.
        assert!(
            parse_create(&args(&[
                "--label",
                "x",
                "--admin",
                "--allow-all",
                "--allow-all"
            ]))
            .is_err()
        );
    }

    #[test]
    fn create_refuses_what_it_cannot_mint() {
        // No user.
        assert!(parse_create(&args(&["--label", "x"])).is_err());
        // Two users.
        assert!(parse_create(&args(&["--label", "x", "--admin", "--email", "a@b"])).is_err());
        // No label.
        assert!(parse_create(&args(&["--admin"])).is_err());
        // A grant twice, even the same way.
        assert!(
            parse_create(&args(&[
                "--label",
                "x",
                "--admin",
                "--allow-drop",
                "--no-allow-drop"
            ]))
            .is_err()
        );
        // An expiry that is not a positive number of days.
        assert!(
            parse_create(&args(&[
                "--label",
                "x",
                "--admin",
                "--expires-in-days",
                "0"
            ]))
            .is_err()
        );
        // A misspelt grant is named.
        let err = parse_create(&args(&["--label", "x", "--admin", "--allow-delete"]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--allow-delete") && err.contains("--allow-drop"),
            "{err}"
        );
    }

    #[test]
    fn revoke_takes_exactly_one_id() {
        let id = Uuid::new_v4();
        assert_eq!(parse_revoke(&args(&[&id.to_string()])).unwrap(), id);
        assert!(parse_revoke(&[]).is_err());
        assert!(parse_revoke(&args(&["not-a-uuid"])).is_err());
    }

    #[test]
    fn the_registration_line_matches_the_screens() {
        assert_eq!(
            claude_mcp_add_line("http://localhost:3032/", "feldspar", "fspk_x"),
            "claude mcp add --transport http feldspar http://localhost:3032/mcp \
             --header \"Authorization: Bearer fspk_x\""
        );
    }
}
