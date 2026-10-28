//! The [`User`] value: an authenticated (or looked-up) row of the `users` table.
//!
//! Per technical design §7.1 the model is deliberately minimal and makes only
//! two assumptions about the table's shape — the non-deletable UUID
//! [`id`](super::COL_ID) and the [`role`](super::COL_ROLE) integer. Everything
//! else the admin has added (including [`email`](super::COL_EMAIL)) is carried
//! opaquely in [`extra`](User::extra), so code never hard-codes the presence of
//! any other field.
//!
//! The password hash is a system column, not an admin field, and is
//! **deliberately excluded** from `extra`: a `User` is the object passed around
//! the request/session, so keeping the hash out of it avoids leaking it into
//! logs, session state, or API responses.

use std::collections::BTreeMap;

use sc_db::Row;
use sc_error::{Error, Result};
use sc_expr::value_from_json;
use sc_query::Value;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::users::{
    COL_DISABLED, COL_ID, COL_LANGUAGE, COL_PASSWORD_HASH, COL_ROLE, ROLE_ADMIN, role_in_range,
};

/// A user of the system: the two guaranteed fields plus an opaque bag of the
/// admin-defined columns (technical design §7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// The non-deletable UUID primary key.
    pub id: Uuid,
    /// The role, `1..=100` (1 = admin, 100 = public).
    pub role: u8,
    /// Every other column of the row keyed by column name — e.g. `email` and any
    /// fields the admin has added. Excludes `id`, `role`, and the password hash.
    pub extra: BTreeMap<String, Value>,
}

impl User {
    /// A user with the two guaranteed fields and no extra columns.
    ///
    /// Returns [`Error::invalid`] if `role` is outside `1..=100`.
    pub fn new(id: Uuid, role: u8) -> Result<User> {
        if !role_in_range(role) {
            return Err(Error::invalid(format!("role {role} is not in 1..=100")));
        }
        Ok(User {
            id,
            role,
            extra: BTreeMap::new(),
        })
    }

    /// Build a user from a `users`-table [`Row`].
    ///
    /// Reads the UUID `id` and integer `role`, validates the role range, and
    /// collects every remaining column into [`extra`](User::extra) — except the
    /// password hash, which is never carried on a `User`. A missing or
    /// wrongly-typed `id`/`role` is an [`Error::invalid`].
    pub fn from_row(row: &Row) -> Result<User> {
        let id = match row.get(COL_ID) {
            Some(Value::Uuid(u)) => *u,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "users.{COL_ID} should be a uuid, got {}",
                    other.kind()
                )));
            }
            None => return Err(Error::invalid(format!("row has no `{COL_ID}` column"))),
        };
        let role = match row.get(COL_ROLE) {
            Some(Value::Int(i)) => role_from_i64(*i)?,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "users.{COL_ROLE} should be an integer, got {}",
                    other.kind()
                )));
            }
            None => return Err(Error::invalid(format!("row has no `{COL_ROLE}` column"))),
        };

        let mut extra = BTreeMap::new();
        for (name, value) in row.columns().iter().zip(row.values()) {
            if name == COL_ID || name == COL_ROLE || name == COL_PASSWORD_HASH {
                continue;
            }
            extra.insert(name.clone(), value.clone());
        }

        Ok(User { id, role, extra })
    }

    /// Build a user from the **JSON object an event carries as its caller** —
    /// the shape `sc_api::caller_context` renders a user's fields into, which is
    /// also what the formula language binds `user` to.
    ///
    /// The inverse of that rendering, as far as JSON can express it: `id` is a
    /// uuid string (a user has no other kind of key), and every other key rides
    /// in [`extra`](User::extra) typed by its own JSON shape, because there is no
    /// column behind it — [`sc_expr::value_from_json`], the same reading a
    /// trigger's own bindings get. The `role` is the **event's**, passed in
    /// rather than read out of the object: it is the authority the request that
    /// caused the event was served at, and that is what a delegated operation
    /// must be checked against.
    ///
    /// `None` in, `None` out. An event with no caller — a scheduled or startup
    /// trigger — has nobody to act as, and the honest answer to "on whose
    /// behalf" is then the public role rather than an error.
    ///
    /// A caller object with no usable `id` **is** an error: acting as somebody
    /// requires knowing who, and guessing is not one of the options.
    pub fn from_json(role: u8, fields: Option<&Json>) -> Result<Option<User>> {
        let Some(fields) = fields else {
            return Ok(None);
        };
        let Json::Object(map) = fields else {
            return Err(Error::invalid(format!(
                "a caller is an object of user fields, not {fields}"
            )));
        };
        let id = match map.get(COL_ID) {
            Some(Json::String(s)) => s.parse::<Uuid>().map_err(|e| {
                Error::invalid(format!("a caller's `{COL_ID}` should be a uuid: {e}"))
            })?,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "a caller's `{COL_ID}` should be a uuid string, got {other}"
                )));
            }
            None => {
                return Err(Error::invalid(format!(
                    "this caller carries no `{COL_ID}`, so there is no user to act as"
                )));
            }
        };
        let mut user = User::new(id, role)?;
        user.extra = map
            .iter()
            .filter(|(name, _)| {
                name.as_str() != COL_ID
                    && name.as_str() != COL_ROLE
                    && name.as_str() != COL_PASSWORD_HASH
            })
            .map(|(name, json)| (name.clone(), value_from_json(json)))
            .collect();
        Ok(Some(user))
    }

    /// Whether this user is an administrator (`role == 1`).
    pub fn is_admin(&self) -> bool {
        self.role == ROLE_ADMIN
    }

    /// Whether this account has been disabled by an administrator.
    ///
    /// `NULL` and `false` both mean enabled — the column says something only
    /// about accounts somebody has acted on. Unlike the password hash the flag
    /// *is* carried in [`extra`](User::extra), because the two places that must
    /// ask (credential [`authenticate`](crate::authenticate)ion and session
    /// resolution) both hold a `User` rather than a row.
    pub fn is_disabled(&self) -> bool {
        matches!(self.get(COL_DISABLED), Some(Value::Bool(true)))
    }

    /// Whether this account was made by a self-service signup whose address
    /// nobody has proven since ([`COL_EMAIL_VERIFIED`](crate::COL_EMAIL_VERIFIED)
    /// is `false`).
    pub fn email_unproven(&self) -> bool {
        matches!(
            self.get(crate::users::COL_EMAIL_VERIFIED),
            Some(Value::Bool(false))
        )
    }

    /// Whether this account is a **check user**: one a checklist may run as,
    /// signed in by the runner without a password (TODO-checklists C1.5). Set
    /// by an admin ([`set_check_user`](crate::set_check_user)) and never by an
    /// agent, which may only point a checklist at an account that already is
    /// one.
    pub fn is_check_user(&self) -> bool {
        match self.get(crate::users::COL_ATTRIBUTES) {
            Some(Value::Json(attrs)) => attrs
                .get(crate::ATTR_CHECK_USER)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Whether this user meets a `min_role` gate. Roles run 1–100 with **lower =
    /// more privileged** (matching [`AccessRules`](sc_catalog::AccessRules)), so a
    /// user passes when `role <= min_role`.
    pub fn meets_role(&self, min_role: u8) -> bool {
        self.role <= min_role
    }

    /// The language this user has chosen to read the product in, if they have
    /// chosen one (§16.1, D8).
    ///
    /// A tag, not a [`Locale`](sc_i18n::Locale): it is stored text that an older
    /// configuration may no longer enable, and the negotiation is the one place
    /// that decides whether a stated preference is servable. Carried on the
    /// `User` for [`is_disabled`](User::is_disabled)'s reason — the router holds
    /// a user, not a row, at the moment it has to ask.
    pub fn language(&self) -> Option<&str> {
        match self.get(COL_LANGUAGE) {
            Some(Value::Text(tag)) if !tag.trim().is_empty() => Some(tag.as_str()),
            _ => None,
        }
    }

    /// The value of an extra (admin-defined) field, if present.
    pub fn get(&self, field: &str) -> Option<&Value> {
        self.extra.get(field)
    }
}

/// Narrow a raw `role` integer to the validated `1..=100` byte.
fn role_from_i64(i: i64) -> Result<u8> {
    let role: u8 = i
        .try_into()
        .map_err(|_| Error::invalid(format!("role {i} is out of range")))?;
    if !role_in_range(role) {
        return Err(Error::invalid(format!("role {role} is not in 1..=100")));
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::users::COL_EMAIL;

    fn row(columns: &[&str], values: Vec<Value>) -> Row {
        let cols = Arc::new(columns.iter().map(|c| c.to_string()).collect());
        Row::new(cols, values).unwrap()
    }

    #[test]
    fn new_validates_role_range() {
        assert!(User::new(Uuid::new_v4(), 0).is_err());
        assert!(User::new(Uuid::new_v4(), 101).is_err());
        let u = User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap();
        assert!(u.is_admin());
        assert!(u.extra.is_empty());
    }

    #[test]
    fn meets_role_gates_on_privilege() {
        let admin = User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap();
        let editor = User::new(Uuid::new_v4(), 40).unwrap();
        // Admin (role 1) passes every gate; a lower-privilege user only passes
        // gates at or below its own role number.
        assert!(admin.meets_role(ROLE_ADMIN));
        assert!(admin.meets_role(100));
        assert!(!editor.meets_role(ROLE_ADMIN)); // 40 <= 1 is false
        assert!(editor.meets_role(40));
        assert!(editor.meets_role(100));
    }

    #[test]
    fn from_row_reads_id_role_and_extra_but_not_hash() {
        let id = Uuid::new_v4();
        let r = row(
            &[COL_ID, COL_ROLE, COL_EMAIL, COL_PASSWORD_HASH],
            vec![
                Value::Uuid(id),
                Value::Int(1),
                Value::Text("admin@example.com".into()),
                Value::Text("$argon2id$secret".into()),
            ],
        );
        let user = User::from_row(&r).unwrap();
        assert_eq!(user.id, id);
        assert_eq!(user.role, 1);
        assert!(user.is_admin());
        // email is carried in extra; the password hash is not.
        assert_eq!(
            user.get(COL_EMAIL),
            Some(&Value::Text("admin@example.com".into()))
        );
        assert!(!user.extra.contains_key(COL_PASSWORD_HASH));
        assert_eq!(user.extra.len(), 1);
    }

    #[test]
    fn disabled_is_read_from_the_row_and_null_means_enabled() {
        let id = Uuid::new_v4();
        let with = |disabled: Value| {
            User::from_row(&row(
                &[COL_ID, COL_ROLE, COL_DISABLED],
                vec![Value::Uuid(id), Value::Int(40), disabled],
            ))
            .unwrap()
        };
        assert!(with(Value::Bool(true)).is_disabled());
        assert!(!with(Value::Bool(false)).is_disabled());
        // A column nobody has ever written is an ordinary, enabled account.
        assert!(!with(Value::Null).is_disabled());
        assert!(!User::new(id, 40).unwrap().is_disabled());
    }

    #[test]
    fn from_json_reads_the_caller_an_event_carries() {
        let id = Uuid::new_v4();
        // The object an event carries is what `caller_context` rendered: the id
        // as a uuid string, the role, and every other field of the row.
        let caller = serde_json::json!({
            "id": id.to_string(),
            "role": 40,
            "email": "ada@example.com",
            "quota": 3,
        });
        let user = User::from_json(40, Some(&caller)).unwrap().unwrap();
        assert_eq!(user.id, id);
        // The event's role, not the object's — they are the same number here, and
        // where they differ the event's is the authority it was served at.
        assert_eq!(user.role, 40);
        assert_eq!(
            user.get(COL_EMAIL),
            Some(&Value::Text("ada@example.com".into()))
        );
        assert_eq!(user.get("quota"), Some(&Value::Int(3)));
        // `id` and `role` are the struct's own fields and are not repeated in
        // `extra`, exactly as `from_row` leaves them out.
        assert_eq!(user.extra.len(), 2);

        // An event with no caller has nobody to act as, and says so by being
        // absent rather than by failing: a scheduled trigger delegates to public.
        assert!(User::from_json(100, None).unwrap().is_none());

        // Acting as somebody requires knowing who.
        for bad in [
            serde_json::json!({ "email": "ada@example.com" }),
            serde_json::json!({ "id": 7 }),
            serde_json::json!({ "id": "not-a-uuid" }),
            serde_json::json!("ada"),
        ] {
            assert!(
                User::from_json(40, Some(&bad)).is_err(),
                "{bad} is not a caller"
            );
        }
        // And the role is validated as it is everywhere else.
        let ok = serde_json::json!({ "id": id.to_string() });
        assert!(User::from_json(0, Some(&ok)).is_err());
    }

    #[test]
    fn from_row_rejects_bad_role_and_missing_columns() {
        let id = Uuid::new_v4();
        // role out of range
        let bad_role = row(&[COL_ID, COL_ROLE], vec![Value::Uuid(id), Value::Int(200)]);
        assert!(User::from_row(&bad_role).is_err());
        // wrong id type
        let bad_id = row(&[COL_ID, COL_ROLE], vec![Value::Int(5), Value::Int(1)]);
        assert!(User::from_row(&bad_id).is_err());
        // missing role column
        let no_role = row(&[COL_ID], vec![Value::Uuid(id)]);
        assert!(User::from_row(&no_role).is_err());
    }
}
