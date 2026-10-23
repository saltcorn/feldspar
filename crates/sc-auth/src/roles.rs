//! The `_fd_roles` table: roles as objects rather than bare integers.
//!
//! A role has always been an integer on the fixed `1..=100` scale (§7.1), and
//! for a while an integer was all it was: `1` meant admin because
//! [`ROLE_ADMIN`] said so, `100` meant public, and everything between meant
//! whatever an installation's users made it mean. That stops working as soon as
//! a role needs to *carry* anything — a name to show in a pick-list, and
//! settings that apply to everyone holding it. A row can carry those; an integer
//! cannot.
//!
//! So a role is a row here, and [`users.role`](crate::COL_ROLE) is a **foreign
//! key onto it**. That is the part worth being deliberate about: the reference
//! is a real database constraint, not a convention. A user whose role names
//! nothing is a user whose privileges cannot be described, and the database is
//! the only thing that can rule that state out under concurrency.
//!
//! **Not an overlay** (§9). A table exists whether or not `_fd_tables` has a row
//! for it; a role does not exist without its row, exactly as an application or a
//! file store does not. That is why this table can hold the authoritative list
//! rather than merely adding to one.
//!
//! **`attributes` is where role-specific settings live** — §9's rule that a
//! sparse value goes in the JSON bag. Whatever the first such setting turns out
//! to be, it needs no schema change to arrive.

use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, Table, TableId};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::users::{ROLE_ADMIN, ROLE_PUBLIC, USERS_TABLE, role_in_range};

/// Name of the roles table in the primary database.
pub const ROLES_TABLE: &str = "_fd_roles";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The role number, `1..=100` — the value `users.role` and every `min_role`
/// holds, and the target of the users table's foreign key.
pub const COL_ROLE: &str = "role";
/// The role's human name (§9's required `name`), e.g. `Admin`.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The sparse per-role settings column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// One role (technical design §7.1, §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    /// The row's identity.
    pub id: Uuid,
    /// The role number on the `1..=100` scale — lower is more privileged.
    pub role: u8,
    /// The human name shown wherever a role is chosen or displayed.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// Role-specific settings (§9).
    pub attributes: Attrs,
}

impl Role {
    /// A role with the given number and name, no description and no settings.
    pub fn new(role: u8, name: impl Into<String>) -> Role {
        Role {
            id: Uuid::new_v4(),
            role,
            name: name.into(),
            description: String::new(),
            attributes: Attrs::new(),
        }
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Role {
        self.description = description.into();
        self
    }

    /// Whether this role is one of the two the system itself depends on.
    ///
    /// [`ROLE_ADMIN`] is what `User::is_admin` tests and what every admin
    /// endpoint requires; [`ROLE_PUBLIC`] is the role an unauthenticated caller
    /// is treated as. Neither is a policy choice an installation gets to delete
    /// — without the first nobody can administer anything, and without the
    /// second an anonymous request has no role at all.
    pub fn is_builtin(&self) -> bool {
        self.role == ROLE_ADMIN || self.role == ROLE_PUBLIC
    }
}

/// The fields of the `_fd_roles` table, in declaration order.
///
/// `role` carries the `UNIQUE` constraint — not `name` — because `role` is what
/// everything else holds: `users.role`, every `min_role`, every access rule.
/// A foreign key needs its target unique, and this is that target. `name` is
/// unique too, since two roles displaying the same name would make a pick-list
/// ambiguous, but nothing references a role *by* name.
fn roles_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_ROLE, int()).required().unique(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// The `users.role` field: a foreign key onto [`ROLES_TABLE`].[`COL_ROLE`], with
/// the role's name as the summary field.
///
/// Lives here rather than beside the rest of the users schema because this is
/// the end that knows what is being referenced, and because the reference is the
/// whole reason this module exists.
pub(crate) fn user_role_field() -> DataField {
    DataField {
        kind: DataFieldKind::Key {
            target_table: TableId(ROLES_TABLE.to_owned()),
            target_field: FieldId(COL_ROLE.to_owned()),
            summary_field: Some(FieldId(COL_NAME.to_owned())),
        },
        ..DataField::plain(crate::users::COL_ROLE, TypeRef::Basic(BasicType::Int)).required()
    }
}

/// Ensure the `_fd_roles` table exists and holds the two built-in roles.
///
/// **Must run before the users table is created**, because `users.role`
/// references this table: a foreign key onto a table that does not exist is not
/// a constraint the database will accept. [`bootstrap`](crate::bootstrap) does
/// them in that order.
///
/// Idempotent, and safe against a database that has never seen Saltcorn. The
/// two built-ins are inserted only when absent, so an admin who renames `Admin`
/// to `Owner` keeps that name across every subsequent boot — the bootstrap
/// establishes the roles, it does not enforce their spelling.
///
/// Only two roles are created. It is tempting to seed a plausible middle —
/// staff, editor, member — but an invented role that nobody uses is a role every
/// admin has to read, understand and decide to delete, and the two seeded here
/// are the only two the *system* itself depends on.
pub async fn bootstrap_roles(catalog: &Catalog) -> Result<Table> {
    let table = match catalog.get(ROLES_TABLE)? {
        Some(existing) => existing,
        None => catalog.create_table(ROLES_TABLE, &roles_fields()).await?,
    };
    for (role, name, description) in [
        (ROLE_ADMIN, "Admin", "Full access to everything."),
        (
            ROLE_PUBLIC,
            "Public",
            "Anyone at all, including callers who are not logged in.",
        ),
    ] {
        if load_role(catalog, role).await?.is_none() {
            save_role(catalog, &Role::new(role, name).description(description)).await?;
        }
    }
    Ok(table)
}

/// Save a role: insert it, or update the row with its id in place.
///
/// The role number and the name are both checked for a clash with a *different*
/// row first, so the admin gets an error naming the conflict rather than a raw
/// constraint violation. The database's `UNIQUE` constraints remain the
/// authority: these checks and the write are not one transaction.
pub async fn save_role(catalog: &Catalog, role: &Role) -> Result<()> {
    let name = role.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a role needs a name"));
    }
    if !role_in_range(role.role) {
        return Err(Error::invalid(format!(
            "role {} is not in 1..=100",
            role.role
        )));
    }
    if let Some(other) = load_role(catalog, role.role).await?
        && other.id != role.id
    {
        return Err(Error::invalid(format!(
            "role {} is already used by `{}`",
            role.role, other.name
        )));
    }
    if let Some(other) = load_role_by_name(catalog, name).await?
        && other.id != role.id
    {
        return Err(Error::invalid(format!(
            "the name `{name}` is already used by role {}",
            other.role
        )));
    }

    let columns = role_columns();
    let values = role_values(role);
    if load_role_by_id(catalog, role.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(ROLES_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(role.id)));
        run(catalog, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            ROLES_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await
    }
}

/// The role with this number, if it exists.
pub async fn load_role(catalog: &Catalog, role: u8) -> Result<Option<Role>> {
    load_one(catalog, Expr::col(COL_ROLE).eq(Expr::lit(i64::from(role)))).await
}

/// The role with this name, if any.
pub async fn load_role_by_name(catalog: &Catalog, name: &str) -> Result<Option<Role>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// The role with this row id, if any.
async fn load_role_by_id(catalog: &Catalog, id: Uuid) -> Result<Option<Role>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id))).await
}

/// Every role, ordered by role number — most privileged first, which is the
/// order a pick-list wants.
pub async fn list_roles(catalog: &Catalog) -> Result<Vec<Role>> {
    let select = Select::from(Source::table(ROLES_TABLE));
    let mut roles: Vec<Role> = rows(catalog, select)
        .await?
        .iter()
        .map(role_from_row)
        .collect::<Result<_>>()?;
    roles.sort_by_key(|r| r.role);
    Ok(roles)
}

/// Delete the role with this number, returning whether one was there to delete.
///
/// Refused in two cases, both because the deletion would leave something in a
/// state it cannot describe:
///
/// - **A built-in role** ([`Role::is_builtin`]). Deleting `Admin` would leave an
///   installation nobody can administer; deleting `Public` would leave an
///   anonymous request with no role to be.
/// - **A role users still hold.** The foreign key would refuse this anyway; the
///   check is here so the answer names the count instead of surfacing a
///   constraint violation, and so the admin knows what to do about it.
///
/// Nothing cascades. A user's role is not something to silently reassign.
pub async fn delete_role(catalog: &Catalog, role: u8) -> Result<bool> {
    let Some(existing) = load_role(catalog, role).await? else {
        return Ok(false);
    };
    if existing.is_builtin() {
        return Err(Error::invalid(format!(
            "role {role} (`{}`) is built in and cannot be deleted",
            existing.name
        )));
    }
    let holders = users_with_role(catalog, role).await?;
    if holders > 0 {
        return Err(Error::invalid(format!(
            "role {role} (`{}`) is held by {holders} user(s); \
             move them to another role before deleting it",
            existing.name
        )));
    }
    crate::grants::clear_ended_grants_of_role(catalog, role).await?;
    let delete =
        Delete::from(ROLES_TABLE).filter(Expr::col(COL_ROLE).eq(Expr::lit(i64::from(role))));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// How many users hold `role`.
async fn users_with_role(catalog: &Catalog, role: u8) -> Result<usize> {
    let select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(crate::users::COL_ROLE).eq(Expr::lit(i64::from(role))));
    Ok(rows(catalog, select).await?.len())
}

/// The row's columns, in the order [`role_values`] produces them.
fn role_columns() -> Vec<String> {
    [COL_ID, COL_ROLE, COL_NAME, COL_DESCRIPTION, COL_ATTRIBUTES]
        .iter()
        .map(|c| (*c).to_owned())
        .collect()
}

/// A role serialised to its row's values, in [`role_columns`] order.
fn role_values(role: &Role) -> Vec<Value> {
    vec![
        Value::Uuid(role.id),
        Value::Int(i64::from(role.role)),
        Value::Text(role.name.trim().to_owned()),
        Value::Text(role.description.clone()),
        Value::Json(Json::Object(role.attributes.clone())),
    ]
}

/// Rebuild a [`Role`] from its `_fd_roles` row.
///
/// Strict, as every `_fd_*` read is: a missing or ill-typed column is an error
/// naming it, never a silent default. A role that fails to parse is a role whose
/// privileges cannot be stated, and guessing is not available.
fn role_from_row(row: &Row) -> Result<Role> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => *u,
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let role = match row.get(COL_ROLE) {
        Some(Value::Int(i)) => u8::try_from(*i)
            .ok()
            .filter(|r| role_in_range(*r))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "{ROLES_TABLE}.{COL_ROLE} should be a role between 1 and 100, got {i}"
                ))
            })?,
        other => return Err(bad_column(COL_ROLE, "an integer role", other)),
    };
    let name = match row.get(COL_NAME) {
        Some(Value::Text(t)) => t.clone(),
        other => return Err(bad_column(COL_NAME, "text", other)),
    };
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_DESCRIPTION, "text", other)),
    };
    let attributes = match row.get(COL_ATTRIBUTES) {
        Some(Value::Json(Json::Object(o))) => o.clone(),
        Some(Value::Json(_)) => {
            return Err(Error::invalid(format!(
                "{ROLES_TABLE}.{COL_ATTRIBUTES} should be a json object"
            )));
        }
        other => return Err(bad_column(COL_ATTRIBUTES, "json", other)),
    };
    Ok(Role {
        id,
        role,
        name,
        description,
        attributes,
    })
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{ROLES_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// Load the single role matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Role>> {
    let select = Select::from(Source::table(ROLES_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(role_from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = roles_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required);
        assert!(!by_name(COL_DESCRIPTION).required);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
    }

    #[test]
    fn the_role_number_is_unique_because_it_is_the_key_everything_holds() {
        // `users.role` and every `min_role` hold this number, and a foreign key
        // needs its target unique. The name is unique too, but only so a
        // pick-list is unambiguous — nothing references a role by name.
        let fields = roles_fields();
        let role = fields.iter().find(|f| f.base.name == COL_ROLE).unwrap();
        assert!(role.required && role.unique);
        assert_eq!(role.base.type_, TypeRef::Basic(BasicType::Int));
        assert!(
            fields
                .iter()
                .find(|f| f.base.name == COL_NAME)
                .unwrap()
                .unique
        );
    }

    #[test]
    fn the_user_role_field_is_a_real_foreign_key() {
        let field = user_role_field();
        assert!(field.required);
        assert_eq!(
            field.kind,
            DataFieldKind::Key {
                target_table: TableId(ROLES_TABLE.to_owned()),
                target_field: FieldId(COL_ROLE.to_owned()),
                summary_field: Some(FieldId(COL_NAME.to_owned())),
            }
        );
        // And it renders as one, which is what makes the reference enforced
        // rather than merely described.
        let col = field.to_column_def();
        let target = col.references.expect("a key field references something");
        assert_eq!(target.table, ROLES_TABLE);
        assert_eq!(target.column, COL_ROLE);
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(ROLES_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let role = Role::new(40, "Staff");
        assert_eq!(role_columns().len(), role_values(&role).len());
        assert_eq!(role_columns().len(), roles_fields().len());
    }

    #[test]
    fn the_two_roles_the_system_depends_on_are_builtin() {
        assert!(Role::new(ROLE_ADMIN, "Admin").is_builtin());
        assert!(Role::new(ROLE_PUBLIC, "Public").is_builtin());
        assert!(!Role::new(40, "Staff").is_builtin());
    }
}
