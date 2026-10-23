//! `grant_role` — give a user a role for a while (TODO-payments P3.4, design
//! C8).
//!
//! A card-less trial, a comp, a promotion: none of them involves a payment, and
//! each is a **role grant** (`role_grants`, §7.4) — a role the user holds
//! beside their base role until it ends. A trigger on a user's insert runs
//!
//! ```text
//! user: row.id   role: 70   duration_days: 14   key: "trial"   once: true
//! ```
//!
//! and the new account is served at role 70 for a fortnight, then at its base
//! role again with nothing written.
//!
//! # The key
//!
//! A formula naming the grant. It is **per user**: the grant is stored under
//! `<user id>:<key>`, so `"trial"` is each user's own trial, and a key that
//! forgets to mention the user cannot move one user's grant onto another. Run
//! again with the same key, the grant is **extended** (its role and period
//! replaced) — unless `once` is set, when nothing happens if a grant with that
//! key has *ever* existed for the user, expired and revoked ones included. A
//! second trial is never granted.
//!
//! # Authority
//!
//! Like `update_rows`, it runs with the trigger's authority: an admin configured
//! it, and the role is the admin's choice, not the caller's. It never grants the
//! admin role — an admin does that by hand, on the user page — so a trigger an
//! application exposes cannot make one.

use chrono::{DateTime, Duration, Utc};
use sc_action::{
    Action, ActionContext, ConfigCheck, EVENT_SCOPE, check_formula, config_flag,
    event_formula_value, optional_formula, required_formula,
};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

/// The user formula: a user's id, or a row with an `id`.
const CFG_USER: &str = "user";
/// The role, a number.
const CFG_ROLE: &str = "role";
/// How long, in days. Blank, with no `expires_at`: until revoked.
const CFG_DURATION_DAYS: &str = "duration_days";
/// When it ends, as a formula — instead of a duration.
const CFG_EXPIRES_AT: &str = "expires_at";
/// The key formula: what names the grant, per user.
const CFG_KEY: &str = "key";
/// Grant only if no grant with this key ever existed for the user.
const CFG_ONCE: &str = "once";
/// A word for whoever reads the grant.
const CFG_NOTE: &str = "note";

/// The source every grant this action makes carries.
pub const GRANT_ROLE_SOURCE: &str = "grant_role";

/// Give a user a role for a while.
pub struct GrantRole;

#[async_trait::async_trait]
impl Action for GrantRole {
    fn name(&self) -> &str {
        GRANT_ROLE_SOURCE
    }

    fn description(&self) -> &str {
        "Give a user a role for a while: a trial, a comp or a promotion, beside their base role"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_USER, BasicType::Text)
                .label("User")
                .required(),
            FormField::new(CFG_ROLE, BasicType::Int)
                .label("Role")
                .required(),
            FormField::new(CFG_DURATION_DAYS, BasicType::Float).label("Duration (days)"),
            FormField::new(CFG_EXPIRES_AT, BasicType::Text).label("Expires at"),
            FormField::new(CFG_KEY, BasicType::Text)
                .label("Key")
                .required(),
            FormField::new(CFG_ONCE, BasicType::Bool).label("Only once per user"),
            FormField::new(CFG_NOTE, BasicType::Text).label("Note"),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let role = role(check.config)?;
        if sc_auth::load_role(check.catalog, role).await?.is_none() {
            return Err(Error::invalid(format!(
                "role {role} does not exist; create it before granting it"
            )));
        }
        for key in [CFG_USER, CFG_KEY] {
            let formula = required_formula(check.config, key)?;
            check_formula(check.shape, EVENT_SCOPE, &formula, &format!("`{key}`"))?;
        }
        if let Some(formula) = optional_formula(check.config, CFG_EXPIRES_AT)? {
            check_formula(
                check.shape,
                EVENT_SCOPE,
                &formula,
                &format!("`{CFG_EXPIRES_AT}`"),
            )?;
        }
        if duration(check.config)?.is_some()
            && optional_formula(check.config, CFG_EXPIRES_AT)?.is_some()
        {
            return Err(Error::invalid(format!(
                "give a grant a `{CFG_DURATION_DAYS}` or an `{CFG_EXPIRES_AT}`, not both"
            )));
        }
        config_flag(check.config, CFG_ONCE)?;
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let role = role(ctx.config)?;
        let user = required_formula(ctx.config, CFG_USER)?;
        let user = user_id(&event_formula_value(ctx, &user, &format!("`{CFG_USER}`")).await?)?;
        let key = required_formula(ctx.config, CFG_KEY)?;
        let key = match event_formula_value(ctx, &key, &format!("`{CFG_KEY}`")).await? {
            Json::String(k) if !k.trim().is_empty() => k.trim().to_owned(),
            Json::Number(n) => n.to_string(),
            other => {
                return Err(Error::invalid(format!(
                    "trigger `{}`: `{CFG_KEY}` gave {other}; it names the grant, so it is text",
                    ctx.trigger
                )));
            }
        };
        let expires_at = match (
            duration(ctx.config)?,
            optional_formula(ctx.config, CFG_EXPIRES_AT)?,
        ) {
            (Some(days), _) => Some(Utc::now() + days),
            (None, Some(formula)) => instant(
                &event_formula_value(ctx, &formula, &format!("`{CFG_EXPIRES_AT}`")).await?,
                ctx.trigger,
            )?,
            (None, None) => None,
        };
        let grant = sc_auth::NewRoleGrant {
            user_id: user,
            role,
            source: GRANT_ROLE_SOURCE.to_owned(),
            source_ref: format!("{user}:{key}"),
            starts_at: None,
            expires_at,
            note: match ctx.config.get(CFG_NOTE) {
                Some(Json::String(note)) if !note.trim().is_empty() => Some(note.trim().to_owned()),
                _ => None,
            },
        };
        let granted = if config_flag(ctx.config, CFG_ONCE)? {
            sc_auth::grant_role_once(ctx.catalog, grant).await?
        } else {
            Some(sc_auth::grant_role(ctx.catalog, grant).await?)
        };
        Ok(match granted {
            Some(grant) => json!({
                "granted": true,
                "id": grant.id.to_string(),
                "user": grant.user_id.to_string(),
                "role": grant.role,
                "expires_at": grant.expires_at.map(|t| t.to_rfc3339()),
            }),
            None => json!({ "granted": false, "user": user.to_string() }),
        })
    }
}

/// The configured role: one that can be granted, never the admin's.
fn role(config: &Attrs) -> Result<u8> {
    let role = config
        .get(CFG_ROLE)
        .and_then(|v| match v {
            Json::Number(n) => n.as_u64(),
            Json::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .and_then(|r| u8::try_from(r).ok())
        .ok_or_else(|| Error::invalid(format!("`{CFG_ROLE}` is a role number")))?;
    if role <= sc_auth::ROLE_ADMIN {
        return Err(Error::invalid(
            "grant_role does not grant the admin role; an admin does that by hand, on the user \
             page",
        ));
    }
    if role >= sc_auth::ROLE_PUBLIC {
        return Err(Error::invalid(format!(
            "role {role} cannot be granted: the public role is everybody's already"
        )));
    }
    Ok(role)
}

/// The configured duration, if any: a positive number of days.
fn duration(config: &Attrs) -> Result<Option<Duration>> {
    let days = match config.get(CFG_DURATION_DAYS) {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Json::Number(n)) => n.as_f64(),
        Some(Json::String(s)) => s.trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    match days {
        Some(days) if days > 0.0 && days.is_finite() => {
            Ok(Some(Duration::milliseconds((days * 86_400_000.0) as i64)))
        }
        _ => Err(Error::invalid(format!(
            "`{CFG_DURATION_DAYS}` is a positive number of days; leave it blank for a grant that \
             lasts until revoked"
        ))),
    }
}

/// The user a formula named: an id, or a row with one.
fn user_id(value: &Json) -> Result<uuid::Uuid> {
    let id = match value {
        Json::String(id) => Some(id.as_str()),
        Json::Object(row) => row.get("id").and_then(Json::as_str),
        _ => None,
    };
    id.and_then(|id| id.parse().ok()).ok_or_else(|| {
        Error::invalid(format!(
            "`{CFG_USER}` gave {value}, which is not a user; it is a user's id (`row.id` on a \
             new user, `user.id` for the caller)"
        ))
    })
}

/// When a grant ends, as an `expires_at` formula gave it: an ISO date and time,
/// or milliseconds since the epoch. Null is "until revoked".
fn instant(value: &Json, trigger: &str) -> Result<Option<DateTime<Utc>>> {
    let parsed = match value {
        Json::Null => return Ok(None),
        Json::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        Json::Number(n) => n.as_i64().and_then(DateTime::<Utc>::from_timestamp_millis),
        _ => None,
    };
    parsed.map(Some).ok_or_else(|| {
        Error::invalid(format!(
            "trigger `{trigger}`: `{CFG_EXPIRES_AT}` gave {value}, which is not a date and time"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(entries: &[(&str, Json)]) -> Attrs {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn the_role_is_grantable_and_never_the_admins() {
        assert_eq!(role(&config(&[(CFG_ROLE, json!(70))])).unwrap(), 70);
        assert_eq!(role(&config(&[(CFG_ROLE, json!("70"))])).unwrap(), 70);
        for bad in [json!(1), json!(100), json!(0), json!("pro"), Json::Null] {
            assert!(role(&config(&[(CFG_ROLE, bad.clone())])).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_duration_is_positive_days_or_nothing() {
        assert_eq!(
            duration(&config(&[(CFG_DURATION_DAYS, json!(14))])).unwrap(),
            Some(Duration::days(14))
        );
        assert_eq!(
            duration(&config(&[(CFG_DURATION_DAYS, json!(0.5))])).unwrap(),
            Some(Duration::hours(12))
        );
        assert_eq!(duration(&config(&[])).unwrap(), None);
        assert_eq!(
            duration(&config(&[(CFG_DURATION_DAYS, json!(""))])).unwrap(),
            None
        );
        assert!(duration(&config(&[(CFG_DURATION_DAYS, json!(-1))])).is_err());
    }

    #[test]
    fn a_user_is_an_id_or_a_row() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(user_id(&json!(id.to_string())).unwrap(), id);
        assert_eq!(user_id(&json!({ "id": id.to_string() })).unwrap(), id);
        assert!(user_id(&Json::Null).is_err());
        assert!(user_id(&json!(7)).is_err());
    }
}
