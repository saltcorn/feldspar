//! **Role grants a module makes** (TODO-payments P3.3, design C4): the `roles`
//! surface behind `ctx.roles` in a service or a webhook and `roles` in an
//! action.
//!
//! ```js
//! await ctx.roles.grant({ user, role: 70, ref: "stripe:sub_123", expires_at });
//! await ctx.roles.revoke("stripe:sub_123");
//! const grant = await ctx.roles.get("stripe:sub_123");
//! ```
//!
//! A module's grants are rows of `role_grants` whose `source` is the module's
//! own name, so `ref` only has to be unique within the module, and a module can
//! neither see nor touch a grant another module, a trigger or an admin made.
//! `grant` **upserts** by the reference — a subscription's one grant is
//! extended by each renewal ([`sc_auth::grant_role`]).
//!
//! # The ceiling
//!
//! Granting a role is a permission the admin gives, like a host the module may
//! reach: [`ModulePermissions::roles`], the most privileged role the module may
//! grant ("may grant roles 70 to 99"). A grant above it is refused by name, so
//! is one when nothing was granted, and the admin role (1) is refused whatever
//! the ceiling says — no module may make an admin. The ceiling is read when the
//! call is made, from the set the module was loaded with.

use chrono::{DateTime, Utc};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::host::RoleAsks;
use crate::permissions::ModulePermissions;

/// One module's role grants, over one call.
pub struct ModuleRoles<'a> {
    catalog: &'a Catalog,
    module: &'a str,
    ceiling: Option<u8>,
}

impl<'a> ModuleRoles<'a> {
    /// The grants `module` may make, under the role ceiling it was granted
    /// ([`ModulePermissions::roles`]).
    pub fn new(catalog: &'a Catalog, module: &'a str, ceiling: Option<u8>) -> ModuleRoles<'a> {
        ModuleRoles {
            catalog,
            module,
            ceiling,
        }
    }

    fn permissions(&self) -> ModulePermissions {
        ModulePermissions {
            roles: self.ceiling,
            ..ModulePermissions::closed()
        }
    }

    /// Refuse a role this module may not grant, saying what it may.
    fn check_role(&self, role: u8) -> Result<()> {
        if role == sc_auth::ROLE_ADMIN {
            return Err(Error::invalid(format!(
                "module `{}` asked to grant the admin role (1); no module may grant it",
                self.module
            )));
        }
        if self.permissions().may_grant_role(role) {
            return Ok(());
        }
        Err(Error::invalid(match self.ceiling {
            None => format!(
                "module `{}` asked to grant role {role}, but it has not been allowed to grant \
                 roles; an admin allows it on the Modules tab (\"Roles it may grant\")",
                self.module
            ),
            Some(ceiling) => format!(
                "module `{}` asked to grant role {role}, but it may only grant roles \
                 {ceiling} to 99; an admin can change that on the Modules tab",
                self.module
            ),
        }))
    }

    async fn grant(&self, plan: &Map<String, Json>) -> Result<Json> {
        let user = match plan.get("user") {
            Some(Json::String(id)) => id.parse::<Uuid>().ok(),
            // A user row, as a module has one from `db.users…` or an event.
            Some(Json::Object(row)) => row
                .get("id")
                .and_then(Json::as_str)
                .and_then(|id| id.parse::<Uuid>().ok()),
            _ => None,
        }
        .ok_or_else(|| {
            Error::invalid("roles.grant needs `user`: a user's id, or a row with an `id`")
        })?;
        let role = plan
            .get("role")
            .and_then(Json::as_u64)
            .and_then(|r| u8::try_from(r).ok())
            .ok_or_else(|| Error::invalid("roles.grant needs `role`, a role number"))?;
        self.check_role(role)?;
        let grant = sc_auth::grant_role(
            self.catalog,
            sc_auth::NewRoleGrant {
                user_id: user,
                role,
                source: self.module.to_owned(),
                source_ref: reference(plan)?,
                starts_at: instant(plan, "starts_at")?,
                expires_at: instant(plan, "expires_at")?,
                note: plan.get("note").and_then(Json::as_str).map(str::to_owned),
            },
        )
        .await?;
        Ok(grant_json(&grant))
    }
}

#[async_trait::async_trait]
impl RoleAsks for ModuleRoles<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let Json::Object(plan) = plan else {
            return Err(Error::invalid("a roles ask is an object with an `op`"));
        };
        match plan.get("op").and_then(Json::as_str).unwrap_or_default() {
            "grant" => self.grant(&plan).await,
            "revoke" => {
                let revoked =
                    sc_auth::revoke_role_grant(self.catalog, self.module, &reference(&plan)?)
                        .await?;
                Ok(json!({ "revoked": revoked }))
            }
            "get" => Ok(
                sc_auth::find_role_grant(self.catalog, self.module, &reference(&plan)?)
                    .await?
                    .as_ref()
                    .map_or(Json::Null, grant_json),
            ),
            other => Err(Error::invalid(format!(
                "`{other}` is not a roles operation; there are grant, revoke and get"
            ))),
        }
    }
}

/// The module's own reference for a grant: what makes it one grant rather than
/// another.
fn reference(plan: &Map<String, Json>) -> Result<String> {
    match plan.get("ref") {
        Some(Json::String(r)) if !r.trim().is_empty() => Ok(r.trim().to_owned()),
        Some(Json::Number(n)) => Ok(n.to_string()),
        _ => Err(Error::invalid(
            "a role grant needs `ref`, the module's own name for it (a subscription's id, say), \
             which a renewal or a revocation names again",
        )),
    }
}

/// An instant as JavaScript sends one: an ISO string (`Date` through JSON) or
/// milliseconds since the epoch. Absent or null is none.
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

/// A grant as the module is answered it.
fn grant_json(grant: &sc_auth::RoleGrant) -> Json {
    json!({
        "id": grant.id.to_string(),
        "user": grant.user_id.to_string(),
        "role": grant.role,
        "ref": grant.source_ref,
        "starts_at": grant.starts_at.to_rfc3339(),
        "expires_at": grant.expires_at.map(|t| t.to_rfc3339()),
        "revoked_at": grant.revoked_at.map(|t| t.to_rfc3339()),
        "note": grant.note,
        "in_force": grant.in_force_at(Utc::now()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_are_read_as_javascript_sends_them() {
        let plan = |v: Json| {
            let mut m = Map::new();
            m.insert("at".into(), v);
            m
        };
        let iso = instant(&plan(json!("2026-11-01T00:00:00.000Z")), "at").unwrap();
        let ms = instant(&plan(json!(1_793_491_200_000_i64)), "at").unwrap();
        assert_eq!(iso, ms);
        assert_eq!(instant(&plan(Json::Null), "at").unwrap(), None);
        assert!(instant(&plan(json!("next week")), "at").is_err());
        assert!(reference(&plan(json!(1))).is_err(), "no `ref` key");
    }
}
