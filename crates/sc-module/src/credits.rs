//! **LLM credits a module grants** (TODO-payments P6.4, design C4): the
//! `credits` surface behind `ctx.credits` in a service or a webhook and
//! `credits` in an action.
//!
//! ```js
//! await ctx.credits.grant({ user, amount: 100, key: "invoice:in_123",
//!                           expires_at: periodEnd, kind: "allowance" });
//! const { balance } = await ctx.credits.balance(user);
//! await ctx.credits.setThreshold(user, 10);
//! ```
//!
//! A module's grants are buckets whose `source` is the module's own name, so
//! `key` only has to be unique within the module — and a replayed webhook that
//! grants the same key again does nothing ([`sc_auth::grant_credits`]).
//!
//! # No debit
//!
//! There is deliberately no `debit`: the only spender of credits is an agent
//! run's model calls (§3.8), and the agent loop charges those itself. A module
//! that wants to charge for something else keeps its own tables.
//!
//! # No ceiling
//!
//! Unlike [`crate::roles`], granting credits needs no permission of its own. A
//! role is a privilege — what a user may read and write — so the admin says how
//! far a module may raise one. Credits are an allowance of model calls through
//! agents the admin configured, which buys no access the agent did not already
//! have.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::host::CreditAsks;

/// One module's credit grants, over one call.
pub struct ModuleCredits<'a> {
    catalog: &'a Catalog,
    module: &'a str,
}

impl<'a> ModuleCredits<'a> {
    /// The credits `module` may grant and read.
    pub fn new(catalog: &'a Catalog, module: &'a str) -> ModuleCredits<'a> {
        ModuleCredits { catalog, module }
    }

    async fn grant(&self, plan: &Map<String, Json>) -> Result<Json> {
        let user = user(plan, "credits.grant")?;
        let amount = amount(plan.get("amount"))?.ok_or_else(|| {
            Error::invalid("credits.grant needs `amount`, the credits to grant (more than 0)")
        })?;
        let key = match plan.get("key") {
            Some(Json::String(k)) if !k.trim().is_empty() => k.trim().to_owned(),
            Some(Json::Number(n)) => n.to_string(),
            _ => {
                return Err(Error::invalid(
                    "credits.grant needs `key`, the module's own name for the grant (an \
                     invoice's id, say): granting the same key again does nothing",
                ));
            }
        };
        let grant = sc_auth::grant_credits(
            self.catalog,
            sc_auth::NewCreditGrant {
                user_id: user,
                amount,
                source: self.module.to_owned(),
                key,
                kind: plan
                    .get("kind")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                expires_at: instant(plan, "expires_at")?,
                note: plan.get("note").and_then(Json::as_str).map(str::to_owned),
            },
            None,
        )
        .await?;
        Ok(json!({
            "bucket": grant.bucket.id.to_string(),
            "amount": number(grant.bucket.amount),
            "remaining": number(grant.bucket.remaining),
            "expires_at": grant.bucket.expires_at.map(|t| t.to_rfc3339()),
            "repaid": number(grant.repaid),
            "balance": number(grant.balance),
            "replayed": grant.replayed,
        }))
    }
}

#[async_trait::async_trait]
impl CreditAsks for ModuleCredits<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let Json::Object(plan) = plan else {
            return Err(Error::invalid("a credits ask is an object with an `op`"));
        };
        match plan.get("op").and_then(Json::as_str).unwrap_or_default() {
            "grant" => self.grant(&plan).await,
            "balance" => {
                let balance =
                    sc_auth::credit_balance(self.catalog, user(&plan, "credits.balance")?).await?;
                Ok(balance_json(&balance))
            }
            "threshold" => {
                let user = user(&plan, "credits.setThreshold")?;
                let threshold = amount(plan.get("threshold"))?;
                let balance =
                    sc_auth::set_low_balance_threshold(self.catalog, user, threshold, None).await?;
                Ok(balance_json(&balance))
            }
            other => Err(Error::invalid(format!(
                "`{other}` is not a credits operation; there are grant, balance and \
                 setThreshold (credits are spent only by agent runs)"
            ))),
        }
    }
}

/// A balance as the module is answered it. Numbers, as JavaScript reads them:
/// a module shows a balance or compares it, and six decimal places fit a
/// double exactly enough for both.
fn balance_json(balance: &sc_auth::CreditBalance) -> Json {
    json!({
        "balance": number(balance.balance),
        "available": number(balance.available),
        "debt": number(balance.debt),
        "next_expiry": balance.next_expiry.map(|t| t.to_rfc3339()),
        "low_balance_threshold": balance.low_balance_threshold.map(number),
    })
}

fn number(d: Decimal) -> Json {
    serde_json::Number::from_f64(f64::try_from(d).unwrap_or(f64::NAN))
        .map_or(Json::Null, Json::Number)
}

/// The user a plan names: an id, or a row with an `id`.
fn user(plan: &Map<String, Json>, what: &str) -> Result<Uuid> {
    match plan.get("user") {
        Some(Json::String(id)) => id.parse::<Uuid>().ok(),
        Some(Json::Object(row)) => row
            .get("id")
            .and_then(Json::as_str)
            .and_then(|id| id.parse::<Uuid>().ok()),
        _ => None,
    }
    .ok_or_else(|| {
        Error::invalid(format!(
            "{what} needs `user`: a user's id, or a row with an `id`"
        ))
    })
}

/// An amount as JavaScript sends one: a number or a decimal string. Absent or
/// null is none.
fn amount(value: Option<&Json>) -> Result<Option<Decimal>> {
    let parsed = match value {
        None | Some(Json::Null) => return Ok(None),
        // A JSON number's own text, not its double: `0.1` stays 0.1.
        Some(Json::Number(n)) => n.to_string().parse::<Decimal>().ok(),
        Some(Json::String(s)) => s.trim().parse::<Decimal>().ok(),
        Some(_) => None,
    };
    parsed
        .map(Some)
        .ok_or_else(|| Error::invalid("an amount of credits is a number, like 100 or 2.5"))
}

/// An instant as JavaScript sends one: an ISO string or milliseconds since the
/// epoch. Absent or null is none.
fn instant(plan: &Map<String, Json>, key: &str) -> Result<Option<DateTime<Utc>>> {
    match plan.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => DateTime::parse_from_rfc3339(s)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|e| Error::invalid(format!("`{key}` is not a date and time: {e}"))),
        Some(Json::Number(n)) => n
            .as_i64()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .map(Some)
            .ok_or_else(|| Error::invalid(format!("`{key}` is not a time in milliseconds"))),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a date, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_read_exactly() {
        assert_eq!(amount(Some(&json!(0.1))).unwrap(), Some(Decimal::new(1, 1)));
        assert_eq!(
            amount(Some(&json!("2.50"))).unwrap(),
            Some(Decimal::new(250, 2))
        );
        assert_eq!(amount(None).unwrap(), None);
        assert!(amount(Some(&json!("lots"))).is_err());
        assert!(amount(Some(&json!(true))).is_err());
    }

    #[test]
    fn a_user_is_an_id_or_a_row() {
        let id = Uuid::new_v4();
        let plan = |v: Json| {
            let mut m = Map::new();
            m.insert("user".into(), v);
            m
        };
        assert_eq!(user(&plan(json!(id.to_string())), "x").unwrap(), id);
        assert_eq!(
            user(&plan(json!({ "id": id.to_string() })), "x").unwrap(),
            id
        );
        assert!(user(&plan(json!(7)), "x").is_err());
    }
}
