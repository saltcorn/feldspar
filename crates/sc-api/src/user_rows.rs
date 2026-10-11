//! The `users` table as an application's API sees it (design §7.1, §7.3).
//!
//! The users table is the one table an admin is invited to extend and then
//! expose to an app — a therapist's screen lists their patients' accounts, a
//! profile page edits the caller's own. Exposed as an ordinary table, though, two
//! of its columns are not ordinary, and the row layer treats them here, once,
//! for every surface that reads or writes through it (REST, GraphQL, an agent's
//! tools, a trigger's actions):
//!
//! - **`password_hash` is never read and never written.** Reads project every
//!   column *but* it ([`projection`]), so it is absent from a list, a returned
//!   row, an event's row and an embed alike, and a filter or ordering that names
//!   it is refused as naming no field ([`visible_field`]). A write naming it is
//!   refused by name: a password is set by its owner, through a login flow.
//! - **`role` is an escalation path**, so a write below admin is held to one
//!   rule: *nobody hands out power they do not have*. A role written into a row
//!   must be less powerful than the caller's own (a greater number), except that
//!   a caller editing their own row may keep their role; and the rows a caller
//!   may update or delete at all are their own and those of less powerful
//!   accounts ([`update_guard`], [`delete_guard`]). The last part rides in the statement's `WHERE`,
//!   so a row out of reach is the same not-found an absent one is.
//!
//! A write with **no caller context** (a system path) or at **admin** role is
//! not held to the role rule: an admin can make anybody anything in the admin
//! UI already. The table's own access rules and ownership formula still decide
//! whether a caller may write the table at all; this narrows, it never grants.

use sc_auth::{COL_ID, COL_PASSWORD_HASH, COL_ROLE, ROLE_ADMIN, USERS_TABLE};
use sc_catalog::{CallerContext, DataField, Table};
use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, Projection, Value};
use serde_json::{Map, Value as Json};

/// Whether `table` is the users table.
pub(crate) fn is_users(table: &Table) -> bool {
    table.name == USERS_TABLE
}

/// Whether `column` of `table` is one no API reads or writes — sc-auth's
/// [`is_api_hidden_column`](sc_auth::is_api_hidden_column), the one statement
/// of the rule, and any `collab_doc` field (live updates L5.1), which is left
/// out of a read altogether rather than sent as null.
pub(crate) fn is_hidden_column(table: &Table, column: &str) -> bool {
    sc_auth::is_api_hidden_column(&table.name, column)
        || table.field(column).is_some_and(DataField::is_collab_doc)
}

/// `table`'s field `name`, unless it is hidden from APIs — the lookup a caller's
/// `select`, filter or ordering resolves a column name through, so naming the
/// password hash is exactly as good as naming a column that is not there.
pub(crate) fn visible_field<'a>(table: &'a Table, name: &str) -> Option<&'a DataField> {
    if is_hidden_column(table, name) {
        return None;
    }
    table.field(name)
}

/// The columns a row read projects: all of them, or — for the users table, and
/// a table with a `collab_doc` field — every stored column but the hidden ones,
/// named one by one.
pub(crate) fn projection(table: &Table) -> Vec<Projection> {
    if !is_users(table) && !table.fields.iter().any(DataField::is_collab_doc) {
        return vec![Projection::all()];
    }
    table
        .fields
        .iter()
        .filter(|f| !f.is_calc() && !is_hidden_column(table, &f.base.name))
        .map(|f| Projection::expr(Expr::col(f.base.name.clone())))
        .collect()
}

/// A row as an application's API returns it: the table's own columns that
/// [`visible_field`] lets through, in the table's field order — anything else
/// the JSON carries (a projected join value, a column REST hides) is dropped.
///
/// What the live hub sends a page for a `table_changes` element (TODO.md
/// "Live updates" L3.5): the row came off the write, not off a read, and this
/// is how it is held to the read's projection rather than to a second list of
/// what to leave out. A column the row does not carry stays absent rather than
/// becoming null — a delete's row, read back by its `RETURNING`, has no
/// calculated fields, and saying they are null would be saying something
/// false.
pub fn api_row(table: &Table, row: &Json) -> Json {
    let Json::Object(obj) = row else {
        return Json::Null;
    };
    let mut out = Map::with_capacity(obj.len());
    for field in &table.fields {
        let name = &field.base.name;
        if visible_field(table, name).is_none() {
            continue;
        }
        if let Some(value) = obj.get(name) {
            out.insert(name.clone(), value.clone());
        }
    }
    Json::Object(out)
}

/// The caller the role rule applies to, or `None` when it does not (not the
/// users table, a system write, or an admin).
fn restricted_caller<'a>(
    table: &Table,
    context: Option<&'a CallerContext>,
) -> Option<&'a CallerContext> {
    context.filter(|c| is_users(table) && c.role != ROLE_ADMIN)
}

/// The caller's own user id, as the context carries it.
fn caller_id(context: &CallerContext) -> Option<&str> {
    context.user.as_ref()?.get(COL_ID)?.as_str()
}

/// Refuse a write body that names the password hash — on the users table,
/// whoever is asking, when there is a caller at all.
fn reject_password_hash(table: &Table, obj: &Map<String, Json>) -> Result<()> {
    if is_users(table) && obj.contains_key(COL_PASSWORD_HASH) {
        return Err(Error::auth(format!(
            "`{COL_PASSWORD_HASH}` is not written through an API: a password is \
             chosen by its owner, through set-password"
        )));
    }
    Ok(())
}

/// The role a write body names, if it names one.
fn written_role(table: &Table, obj: &Map<String, Json>) -> Result<Option<i64>> {
    let Some(json) = obj.get(COL_ROLE) else {
        return Ok(None);
    };
    match crate::rows::column_value(table, COL_ROLE, json)? {
        Value::Int(role) => Ok(Some(role)),
        Value::Null => Ok(None),
        other => Err(Error::invalid(format!(
            "`{COL_ROLE}` must be a role number, not {}",
            other.kind()
        ))),
    }
}

fn too_powerful(role: i64, caller_role: u8) -> Error {
    Error::auth(format!(
        "you cannot give an account role {role}: a role you assign must be less \
         powerful than your own (a number greater than {caller_role})"
    ))
}

/// Check an insert into the users table: no password hash, and a role less
/// powerful than the caller's.
pub(crate) fn check_insert(
    table: &Table,
    obj: &Map<String, Json>,
    context: Option<&CallerContext>,
) -> Result<()> {
    if context.is_some() {
        reject_password_hash(table, obj)?;
    }
    let Some(caller) = restricted_caller(table, context) else {
        return Ok(());
    };
    match written_role(table, obj)? {
        Some(role) if role > i64::from(caller.role) => Ok(()),
        Some(role) => Err(too_powerful(role, caller.role)),
        None => Err(Error::invalid(format!(
            "a new account needs a `{COL_ROLE}` less powerful than your own"
        ))),
    }
}

/// Check an update of user `id` and return the predicate that confines it to
/// the rows the caller may change — their own, and less powerful accounts'.
pub(crate) fn update_guard(
    table: &Table,
    id: &str,
    obj: &Map<String, Json>,
    context: Option<&CallerContext>,
) -> Result<Option<Expr>> {
    if context.is_some() {
        reject_password_hash(table, obj)?;
    }
    let Some(caller) = restricted_caller(table, context) else {
        return Ok(None);
    };
    if let Some(role) = written_role(table, obj)? {
        let own_row = caller_id(caller).is_some_and(|me| me.eq_ignore_ascii_case(id.trim()));
        // Keeping your own role is not handing anybody anything.
        let allowed = match own_row {
            true => role >= i64::from(caller.role),
            false => role > i64::from(caller.role),
        };
        if !allowed {
            return Err(too_powerful(role, caller.role));
        }
    }
    reachable(table, caller).map(Some)
}

/// The predicate that confines a delete of a user to the rows the caller may
/// remove — their own, and less powerful accounts'.
pub(crate) fn delete_guard(table: &Table, context: Option<&CallerContext>) -> Result<Option<Expr>> {
    match restricted_caller(table, context) {
        Some(caller) => reachable(table, caller).map(Some),
        None => Ok(None),
    }
}

/// `role > caller.role OR id = caller.id`.
fn reachable(table: &Table, caller: &CallerContext) -> Result<Expr> {
    let weaker = Expr::binary(
        BinOp::Gt,
        Expr::col(COL_ROLE),
        Expr::lit(i64::from(caller.role)),
    );
    Ok(match caller_id(caller) {
        Some(me) => {
            let me = crate::rows::column_value(table, COL_ID, &Json::String(me.to_owned()))?;
            weaker.or(Expr::col(COL_ID).eq(Expr::lit(me)))
        }
        None => weaker,
    })
}

/// AND an extra predicate onto an optional guard.
pub(crate) fn and_guard(guard: Option<Expr>, extra: Option<Expr>) -> Option<Expr> {
    match (guard, extra) {
        (Some(g), Some(e)) => Some(g.and(e)),
        (g, e) => g.or(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_catalog::{AccessRules, DbId, TableId, TableSource};
    use sc_types::{BasicType, TypeRef};
    use serde_json::json;

    fn users() -> Table {
        Table {
            id: TableId(USERS_TABLE.to_owned()),
            name: USERS_TABLE.to_owned(),
            database: DbId::primary(),
            source: TableSource::Database,
            fields: vec![
                DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid)).primary_key(),
                DataField::plain(COL_ROLE, TypeRef::Basic(BasicType::Int)),
                DataField::plain("email", TypeRef::Basic(BasicType::Text)),
                DataField::plain(COL_PASSWORD_HASH, TypeRef::Basic(BasicType::Text)),
            ],
            primary_key: vec![COL_ID.to_owned()],
            label: USERS_TABLE.to_owned(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Default::default(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    const ME: &str = "6f1c1f0e-1d7b-4c47-9a35-7a3f3c1f2b10";

    fn therapist() -> CallerContext {
        CallerContext::new(40, Some(json!({ "id": ME, "role": 40 })))
    }

    fn obj(v: Json) -> Map<String, Json> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn the_hash_is_neither_projected_nor_resolvable() {
        let t = users();
        let cols = projection(&t);
        assert_eq!(cols.len(), 3, "{cols:?}");
        assert!(visible_field(&t, COL_PASSWORD_HASH).is_none());
        assert!(visible_field(&t, "email").is_some());
        // Another table's column of the same name is an ordinary column.
        let mut other = users();
        other.name = "patients".into();
        assert!(visible_field(&other, COL_PASSWORD_HASH).is_some());
        assert_eq!(projection(&other), vec![Projection::all()]);
    }

    #[test]
    fn an_insert_may_only_hand_out_less_power() {
        let t = users();
        let ctx = therapist();
        assert!(check_insert(&t, &obj(json!({"email": "p@x", "role": 80})), Some(&ctx)).is_ok());
        assert!(check_insert(&t, &obj(json!({"email": "p@x", "role": 40})), Some(&ctx)).is_err());
        assert!(check_insert(&t, &obj(json!({"email": "p@x", "role": 1})), Some(&ctx)).is_err());
        assert!(check_insert(&t, &obj(json!({"email": "p@x"})), Some(&ctx)).is_err());
        let hash = obj(json!({"email": "p@x", "role": 80, "password_hash": "x"}));
        assert!(check_insert(&t, &hash, Some(&ctx)).is_err());
        // An admin is not held to the role rule, but still cannot write a hash.
        let admin = CallerContext::new(ROLE_ADMIN, None);
        assert!(check_insert(&t, &obj(json!({"email": "a@x", "role": 1})), Some(&admin)).is_ok());
        assert!(check_insert(&t, &hash, Some(&admin)).is_err());
        // A system write has no caller to hold to anything.
        assert!(check_insert(&t, &hash, None).is_ok());
    }

    #[test]
    fn an_update_keeps_your_own_role_but_grants_nobody_more() {
        let t = users();
        let ctx = therapist();
        let other = "0b5d9a52-54d4-4bde-8b2a-6d0f6f7a1a11";
        let keep = obj(json!({"role": 40}));
        assert!(update_guard(&t, ME, &keep, Some(&ctx)).is_ok());
        assert!(update_guard(&t, other, &keep, Some(&ctx)).is_err());
        assert!(update_guard(&t, ME, &obj(json!({"role": 1})), Some(&ctx)).is_err());
        let guard = update_guard(&t, other, &obj(json!({"email": "e"})), Some(&ctx))
            .unwrap()
            .expect("a restricted caller is confined");
        let sql = format!("{guard:?}");
        assert!(sql.contains("role") && sql.contains("id"), "{sql}");
        assert!(delete_guard(&t, Some(&ctx)).unwrap().is_some());
        assert!(delete_guard(&t, None).unwrap().is_none());
    }
}
