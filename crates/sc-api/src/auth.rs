//! The authentication vocabulary shared by every API surface (design §7.2,
//! §13.4).
//!
//! Logging in looks the same whichever API is asked: the same credentials go in,
//! the same public view of a user comes back, and the same [`SessionAction`]
//! describes what happened. The admin API's `login` and an application's `login`
//! are the same operation on the same `sc-auth`, so the schemas, the wire shape,
//! and the body parsing live here once rather than once per surface.
//!
//! [`SessionAction`]: crate::SessionAction

use sc_auth::{COL_EMAIL, User};
use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::{Value as Json, json};

use crate::rows::require_object;
use crate::schema::{StructField, TypeSchema};

/// Name of the CSRF double-submit cookie the server hands every browser client
/// (non-`HttpOnly`, so the client can read it).
///
/// Declared here, below every API surface, because it is part of the **wire
/// contract** a client of any of them must satisfy: a session cookie
/// authenticates the request, so a mutating request must also echo this cookie in
/// [`CSRF_HEADER`] or it is refused. The server enforces it and
/// [`generate_client`](crate::generate_client) emits code that satisfies it —
/// naming it twice is how the two drift.
pub const CSRF_COOKIE: &str = "sc_csrf";
/// Header a mutating request must echo the [`CSRF_COOKIE`] value in.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Header a client sends, with the value [`NATIVE_CLIENT`], when it is **not a
/// browser** — the generated client running in a React Native app, where there
/// is no `document`. A native app's login gets a session cookie that outlives
/// the app being closed; a browser's keeps ending with the browser.
pub const CLIENT_KIND_HEADER: &str = "x-feldspar-client";
/// The [`CLIENT_KIND_HEADER`] value a native app sends.
pub const NATIVE_CLIENT: &str = "native";

/// Email + password, the body of a login (and of first-user creation).
pub fn credentials_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("email", TypeSchema::text()),
        StructField::new("password", TypeSchema::text()),
    ])
}

/// The public view of a user — **never** the password hash.
pub fn user_summary_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("email", TypeSchema::text()),
        StructField::new("role", TypeSchema::int()),
    ])
}

/// A user rendered for the wire, matching [`user_summary_schema`].
///
/// Only the id, the identifying email, and the role: a user row may carry any
/// admin-added column (§7.1) and the password hash, and none of that belongs in
/// an API response.
pub fn user_summary_json(user: &User) -> Json {
    json!({
        "id": user.id.to_string(),
        "email": user.get(COL_EMAIL).and_then(Value::as_text).unwrap_or_default(),
        "role": i64::from(user.role),
    })
}

/// The **administrator's** view of a user: the summary, plus whether the account
/// is disabled and every admin-added column of the row.
///
/// A second shape rather than a wider [`user_summary_schema`], because the two
/// answer different questions. The summary says who the caller is and travels on
/// every login and `authStatus`; this one is the users screen's row, and the
/// users screen is the one place that has to show a table whose columns the
/// admin themselves added (§7.1). Widening the summary would put those columns —
/// which may be anything at all — into every login response.
pub fn user_row_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("email", TypeSchema::text()),
        StructField::new("role", TypeSchema::int()),
        StructField::new("disabled", TypeSchema::bool()),
        // The language this account reads the product in — a system column with
        // its own select on the form, so it is a named field rather than one of
        // `extra`'s (§16.1). Optional: most accounts have never stated one.
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        // Keyed by column name, since only the admin's own schema knows what is
        // in here — `listFields` on the users table is what names them.
        StructField::new("extra", TypeSchema::json()),
    ])
}

/// One role grant as the admin user page shows it (TODO-payments P3.2).
pub fn role_grant_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("role", TypeSchema::int()),
        // `admin`, `grant_role`, or the module that made it (`payments`).
        StructField::new("source", TypeSchema::text()),
        StructField::new("source_ref", TypeSchema::text()),
        StructField::new("starts_at", TypeSchema::timestamp()),
        StructField::new("expires_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new("revoked_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new("note", TypeSchema::optional(TypeSchema::text())),
        // Whether it counts now: started, not ended, not revoked.
        StructField::new("in_force", TypeSchema::bool()),
    ])
}

/// What the admin user page shows about a user's role: the base role an admin
/// sets, the role they are served at, and every grant they have had.
pub fn user_role_grants_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("base_role", TypeSchema::int()),
        StructField::new("effective_role", TypeSchema::int()),
        StructField::new("grants", TypeSchema::array(role_grant_schema())),
    ])
}

/// A grant rendered to [`role_grant_schema`].
pub fn role_grant_json(grant: &sc_auth::RoleGrant) -> Json {
    json!({
        "id": grant.id.to_string(),
        "role": i64::from(grant.role),
        "source": grant.source,
        "source_ref": grant.source_ref,
        "starts_at": grant.starts_at.to_rfc3339(),
        "expires_at": grant.expires_at.map(|t| t.to_rfc3339()),
        "revoked_at": grant.revoked_at.map(|t| t.to_rfc3339()),
        "note": grant.note,
        "in_force": grant.in_force_at(chrono::Utc::now()),
    })
}

/// A user rendered for the admin users screen, matching [`user_row_schema`].
///
/// The password hash is not in [`User::extra`] to begin with (it is dropped when
/// the row is read), so there is nothing to filter here beyond splitting the
/// columns the system owns out of the ones the admin added.
pub fn user_row_json(user: &User) -> Json {
    let extra: serde_json::Map<String, Json> = user
        .extra
        .iter()
        .filter(|(column, _)| !sc_auth::is_system_user_column(column))
        .map(|(column, value)| (column.clone(), crate::convert::value_to_json(value)))
        .collect();
    json!({
        "id": user.id.to_string(),
        "email": user.get(COL_EMAIL).and_then(Value::as_text).unwrap_or_default(),
        "role": i64::from(user.role),
        "disabled": user.is_disabled(),
        "language": user.language(),
        "extra": Json::Object(extra),
    })
}

/// Email + password out of a login body, matching [`credentials_schema`].
///
/// Both must be present and non-blank; an empty password is a malformed request,
/// not a login attempt to be checked against the hash.
pub fn credentials(body: &Json) -> Result<(String, String)> {
    let obj = require_object(body)?;
    let field = |key: &str| -> Result<String> {
        let value = obj
            .get(key)
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid(format!("missing or non-string field `{key}`")))?;
        if value.trim().is_empty() {
            return Err(Error::invalid(format!("field `{key}` must not be empty")));
        }
        Ok(value.to_owned())
    };
    Ok((field("email")?, field("password")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_require_both_fields_non_blank() {
        let (email, password) = credentials(&json!({"email": "a@b.c", "password": "pw"})).unwrap();
        assert_eq!(email, "a@b.c");
        assert_eq!(password, "pw");

        // Missing, non-string, and blank are each rejected rather than treated
        // as an empty credential.
        assert!(credentials(&json!({"email": "a@b.c"})).is_err());
        assert!(credentials(&json!({"email": 1, "password": "pw"})).is_err());
        assert!(credentials(&json!({"email": "a@b.c", "password": "  "})).is_err());
        assert!(credentials(&json!("not an object")).is_err());
    }

    #[test]
    fn user_summary_never_leaks_extra_columns() {
        let mut user = User::new(uuid::Uuid::new_v4(), 1).unwrap();
        user.extra
            .insert(COL_EMAIL.to_owned(), Value::Text("a@b.c".to_owned()));
        // An admin-added column, and something that must never travel.
        user.extra.insert(
            "password_hash".to_owned(),
            Value::Text("$argon2id$".to_owned()),
        );

        let json = user_summary_json(&user);
        assert_eq!(json["email"], json!("a@b.c"));
        assert_eq!(json["role"], json!(1));
        // Only the three declared fields, whatever else the row carries.
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert!(json.get("password_hash").is_none());
    }
}
