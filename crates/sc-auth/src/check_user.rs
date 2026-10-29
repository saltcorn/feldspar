//! Check users (TODO-checklists C1.5): the accounts a checklist runs as.
//!
//! A flag in the user's [`attributes`](crate::COL_ATTRIBUTES), set only by an
//! admin. The checklist runner signs a check user in without a password — it
//! makes the session itself — so the flag is what an agent's checklist may
//! point at: an agent may set a checklist's `run_as` to a check user and to
//! nobody else (docs/CHECKLISTS.md §4 rule 4), which keeps an agent with
//! narrow grants from writing a checklist that deletes rows as an admin.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, Statement, Update, Value};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::lookup::load_user;
use crate::user::User;
use crate::users::{COL_ATTRIBUTES, COL_ID, USERS_TABLE};

/// The key in a user's attributes that makes it a check user.
pub const ATTR_CHECK_USER: &str = "check_user";

/// Make the user with this id a check user, or stop it being one, and return
/// it. The caller is responsible for being an admin: this is reached from the
/// admin API's user editor and the command line, never from an agent.
pub async fn set_check_user(catalog: &Catalog, id: Uuid, on: bool) -> Result<User> {
    let user = load_user(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no user with id {id}")))?;
    let mut attrs = match user.get(COL_ATTRIBUTES) {
        Some(Value::Json(Json::Object(map))) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if on {
        attrs.insert(ATTR_CHECK_USER.to_owned(), Json::Bool(true));
    } else {
        attrs.remove(ATTR_CHECK_USER);
    }
    let value = if attrs.is_empty() {
        Value::Null
    } else {
        Value::Json(Json::Object(attrs))
    };
    let update = Update::new(
        USERS_TABLE,
        vec![Assignment::new(COL_ATTRIBUTES, Expr::Lit(value))],
    )
    .filter(Expr::col(COL_ID).eq(Expr::lit(id)));
    catalog
        .primary()
        .query(&Statement::from(update))
        .await?
        .try_collect()
        .await?;
    load_user(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no user with id {id}")))
}
