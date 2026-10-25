//! **Role grants** (TODO-payments P3, design C2): a role a user holds for a
//! while, beside the base role an admin sets on `users.role`.
//!
//! v1's subscription module wrote `users.role` directly. A missed cancellation
//! webhook left the role in place for good, an admin's change was overwritten
//! by the next renewal, and a user could not hold a role from a subscription
//! and from an admin's free upgrade at once. Here each of those is a **row**:
//! `(user, role, source, source_ref, starts_at, expires_at)`, and the role a
//! user is served at — their **effective role** — is the most privileged of the
//! base role and the grants in force ([`effective_role`]).
//!
//! # Lapsing is a read, never a write
//!
//! A grant is in force from `starts_at` until `expires_at` (none: until
//! revoked). Nothing runs at the expiry: the next read of the user simply finds
//! no grant in force. So a grant whose renewal never arrived ends on time with
//! no job, no webhook and no write — missing an event makes access lapse, it
//! never extends it. The session cache ([`crate::SessionStore`]) holds a
//! resolved user no later than the next moment its effective role can change
//! ([`EffectiveRole::changes_at`]), so a lapse lands at once rather than up to a
//! freshness TTL later.
//!
//! # One grant per source and reference
//!
//! `(source, source_ref)` is jointly unique. A module's grant carries the
//! module's package name and its own reference (`("@feldspar/payments",
//! "<account>:<subscription>")`), an admin's by hand is `("admin", <uuid>)`, a
//! trial from the `grant_role` action is `("grant_role", "<user id>:trial")`. [`grant_role`] **upserts** on it, so each
//! renewal extends the one grant rather than adding another; [`grant_role_once`]
//! inserts only if no grant with that key has *ever* existed, expired and
//! revoked ones included — a second trial is never granted. A revocation keeps
//! the row (`revoked_at`) for exactly that reason.
//!
//! # An ordinary table
//!
//! [`ROLE_GRANTS_TABLE`] has no `_fd_` prefix: an application reads it like any
//! other table ("your trial ends on …"), and a daily trigger over it sends the
//! reminders. Its owner may read their own rows (`_read && user_id === user.id`)
//! and only an admin may write them; core writes them through the functions
//! here, never through the row API.

use chrono::{DateTime, Utc};
use sc_catalog::{
    Catalog, DataField, DataFieldKind, FieldId, Table, TableConstraint, TableId, TableMeta,
    load_table_meta_by_name, save_table_meta_row,
};
use sc_db::{Row, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, OrderBy, Projection, Select, Source, Statement, UnOp,
    Update, Value,
};
use sc_types::{BasicType, TypeRef};
use serde::Serialize;
use uuid::Uuid;

use crate::roles::{COL_NAME as COL_ROLE_NAME, ROLES_TABLE, load_role};
use crate::users::{COL_EMAIL, COL_ID as COL_USER_ID_TARGET, ROLE_ADMIN, ROLE_PUBLIC, USERS_TABLE};

/// The grants, an ordinary table in the primary database.
pub const ROLE_GRANTS_TABLE: &str = "role_grants";

/// The grant's own id.
pub const COL_GRANT_ID: &str = "id";
/// The user it is for (`→ users`).
pub const COL_GRANT_USER: &str = "user_id";
/// The role it gives (`→ _fd_roles`).
pub const COL_GRANT_ROLE: &str = "role";
/// Who made it: `admin`, `grant_role`, or a module's name.
pub const COL_GRANT_SOURCE: &str = "source";
/// The source's own reference for it — a subscription, a trial key. Unique with
/// `source`.
pub const COL_GRANT_SOURCE_REF: &str = "source_ref";
/// When it starts to count.
pub const COL_GRANT_STARTS_AT: &str = "starts_at";
/// When it stops; null for "until revoked".
pub const COL_GRANT_EXPIRES_AT: &str = "expires_at";
/// When it was revoked, if it was. A revoked grant is kept, so a `once` grant
/// knows it existed.
pub const COL_GRANT_REVOKED_AT: &str = "revoked_at";
/// A word for whoever reads the row: "14-day trial", "comped by Ada".
pub const COL_GRANT_NOTE: &str = "note";
/// When the row was first written.
pub const COL_GRANT_CREATED_AT: &str = "created_at";

/// The source an admin's grant by hand carries.
pub const GRANT_SOURCE_ADMIN: &str = "admin";

/// The ownership formula the table is created with: the owner reads their own
/// grants; nobody but an admin writes.
pub const ROLE_GRANTS_OWNERSHIP: &str = "_read && user_id === user.id";

/// A grant, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleGrant {
    /// The row's id.
    pub id: Uuid,
    /// The user it is for.
    pub user_id: Uuid,
    /// The role it gives.
    pub role: u8,
    /// Who made it.
    pub source: String,
    /// The source's reference.
    pub source_ref: String,
    /// When it starts to count.
    pub starts_at: DateTime<Utc>,
    /// When it stops, if ever.
    pub expires_at: Option<DateTime<Utc>>,
    /// When it was revoked, if it was.
    pub revoked_at: Option<DateTime<Utc>>,
    /// The note, if any.
    pub note: Option<String>,
    /// When it was first written.
    pub created_at: DateTime<Utc>,
}

impl RoleGrant {
    /// Whether the grant counts at `now`: started, not expired, not revoked.
    pub fn in_force_at(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none()
            && self.starts_at <= now
            && self.expires_at.is_none_or(|end| end > now)
    }
}

/// A grant to make. `starts_at` defaults to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoleGrant {
    /// The user it is for.
    pub user_id: Uuid,
    /// The role it gives.
    pub role: u8,
    /// Who makes it.
    pub source: String,
    /// The source's reference for it.
    pub source_ref: String,
    /// When it starts; `None` is now.
    pub starts_at: Option<DateTime<Utc>>,
    /// When it stops; `None` is "until revoked".
    pub expires_at: Option<DateTime<Utc>>,
    /// A word for whoever reads the row.
    pub note: Option<String>,
}

/// A user's effective role, and the next moment it can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveRole {
    /// The most privileged of the base role and the grants in force.
    pub role: u8,
    /// The soonest a grant that could matter starts or ends — when a cached
    /// copy of this answer stops being true. `None`: not until something is
    /// written.
    pub changes_at: Option<DateTime<Utc>>,
}

fn grant_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_GRANT_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField {
            kind: DataFieldKind::Key {
                target_table: TableId(USERS_TABLE.to_owned()),
                target_field: FieldId(COL_USER_ID_TARGET.to_owned()),
                summary_field: Some(FieldId(COL_EMAIL.to_owned())),
            },
            ..DataField::plain(COL_GRANT_USER, TypeRef::Basic(BasicType::Uuid)).required()
        },
        DataField {
            kind: DataFieldKind::Key {
                target_table: TableId(ROLES_TABLE.to_owned()),
                target_field: FieldId(crate::users::COL_ROLE.to_owned()),
                summary_field: Some(FieldId(COL_ROLE_NAME.to_owned())),
            },
            ..DataField::plain(COL_GRANT_ROLE, TypeRef::Basic(BasicType::Int)).required()
        },
        DataField::plain(COL_GRANT_SOURCE, text()).required(),
        DataField::plain(COL_GRANT_SOURCE_REF, text()).required(),
        DataField::plain(COL_GRANT_STARTS_AT, timestamp()).required(),
        DataField::plain(COL_GRANT_EXPIRES_AT, timestamp()),
        DataField::plain(COL_GRANT_REVOKED_AT, timestamp()),
        DataField::plain(COL_GRANT_NOTE, text()),
        DataField::plain(COL_GRANT_CREATED_AT, timestamp()).required(),
    ]
}

/// The jointly-unique `(source, source_ref)`: one grant per thing that grants.
fn source_key() -> sc_catalog::ConstraintKind {
    sc_catalog::ConstraintKind::Unique {
        fields: vec![COL_GRANT_SOURCE.to_owned(), COL_GRANT_SOURCE_REF.to_owned()],
    }
}

/// Ensure `role_grants` exists, with its `(source, source_ref)` key and, when
/// the table is new, its access settings. Idempotent.
///
/// **Not** part of [`crate::bootstrap`]: a new installation has no grants
/// table. A module that grants roles declares it (`core_tables`, created when
/// the module loads), and the first grant made without one creates it.
///
/// **After the users and roles tables**, both of which it references.
/// The access settings are the creation's, like a module table's (§15.1b): an
/// admin who changes them afterwards keeps the change.
pub async fn bootstrap_role_grants(catalog: &Catalog) -> Result<Table> {
    sc_catalog::bootstrap_table_meta(catalog).await?;
    let created = catalog.get(ROLE_GRANTS_TABLE)?.is_none();
    let mut table = catalog
        .bootstrap_table(ROLE_GRANTS_TABLE, &grant_fields())
        .await?;
    let key = source_key();
    if !table.constraints.iter().any(|c| c.kind == key) {
        catalog
            .apply_schema_batch(&[sc_catalog::SchemaStep::Change(
                SchemaChange::AddUniqueConstraint {
                    table: ROLE_GRANTS_TABLE.to_owned(),
                    name: TableConstraint::derived_name(ROLE_GRANTS_TABLE, &key, ""),
                    columns: vec![COL_GRANT_SOURCE.to_owned(), COL_GRANT_SOURCE_REF.to_owned()],
                },
            )])
            .await?;
        catalog.reload().await?;
        table = catalog.require(ROLE_GRANTS_TABLE)?;
    }
    if created
        && load_table_meta_by_name(catalog, ROLE_GRANTS_TABLE)
            .await?
            .is_none()
    {
        let mut meta = TableMeta::new(ROLE_GRANTS_TABLE)
            .label("Role grants")
            .description(
                "Roles users hold for a while, beside their base role: subscriptions, trials \
                 and grants made by hand. Written by Feldspar; each user reads their own.",
            )
            .access(ROLE_ADMIN, ROLE_ADMIN);
        meta.set_ownership_formula(Some(ROLE_GRANTS_OWNERSHIP));
        save_table_meta_row(catalog, &meta).await?;
        catalog.reload().await?;
        table = catalog.require(ROLE_GRANTS_TABLE)?;
    }
    Ok(table)
}

/// `user`'s effective role: the most privileged (lowest) of `base` and the
/// grants in force now (§7.4, design C2).
///
/// An installation without the table — a test catalog that never bootstrapped
/// it — answers the base role.
pub async fn effective_role(catalog: &Catalog, user: Uuid, base: u8) -> Result<EffectiveRole> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(EffectiveRole {
            role: base,
            changes_at: None,
        });
    }
    let now = Utc::now();
    // Every unrevoked grant that has not ended: the ones in force decide the
    // role, and the ones still to start decide when it next changes.
    let select = Select::from(Source::table(ROLE_GRANTS_TABLE)).filter(
        Expr::col(COL_GRANT_USER)
            .eq(Expr::lit(user))
            .and(Expr::unary(UnOp::IsNull, Expr::col(COL_GRANT_REVOKED_AT)))
            .and(
                Expr::unary(UnOp::IsNull, Expr::col(COL_GRANT_EXPIRES_AT)).or(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_GRANT_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(now)),
                )),
            ),
    );
    let grants = rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(grant_from_row)
        .collect::<Result<Vec<_>>>()?;
    Ok(resolve(base, &grants, now))
}

/// The pure part of [`effective_role`]: given the unrevoked, unexpired grants.
fn resolve(base: u8, grants: &[RoleGrant], now: DateTime<Utc>) -> EffectiveRole {
    let role = grants
        .iter()
        .filter(|g| g.in_force_at(now))
        .map(|g| g.role)
        .fold(base, u8::min);
    // A grant no more privileged than the base cannot change anything by
    // starting or ending, so only the others' moments count.
    let changes_at = grants
        .iter()
        .filter(|g| g.role < base)
        .filter_map(|g| {
            if g.starts_at > now {
                Some(g.starts_at)
            } else {
                g.expires_at
            }
        })
        .min();
    EffectiveRole { role, changes_at }
}

/// `user` served at their effective role, and when that next changes.
pub(crate) async fn with_grants(
    catalog: &Catalog,
    mut user: crate::User,
) -> Result<(crate::User, Option<DateTime<Utc>>)> {
    let effective = effective_role(catalog, user.id, user.role).await?;
    user.role = effective.role;
    Ok((user, effective.changes_at))
}

/// Grant a role, or update the grant `(source, source_ref)` already names —
/// the user, role, period and note are replaced, and a revoked grant is in
/// force again. What a subscription's renewal does to extend its one grant.
pub async fn grant_role(catalog: &Catalog, new: NewRoleGrant) -> Result<RoleGrant> {
    check_new(catalog, &new).await?;
    ensure_table(catalog).await?;
    if let Some(updated) = update_by_key(catalog, &new).await? {
        return Ok(updated);
    }
    match insert(catalog, &new).await {
        Ok(grant) => Ok(grant),
        // Somebody inserted the same key between the update and the insert:
        // theirs is the row, so update it.
        Err(e) => match update_by_key(catalog, &new).await? {
            Some(updated) => Ok(updated),
            None => Err(e),
        },
    }
}

/// Grant a role only if no grant with this `(source, source_ref)` has ever
/// existed — expired and revoked ones count. `None` when one has. What a
/// trial on sign-up uses, so a second one is never granted.
pub async fn grant_role_once(catalog: &Catalog, new: NewRoleGrant) -> Result<Option<RoleGrant>> {
    check_new(catalog, &new).await?;
    ensure_table(catalog).await?;
    if find(catalog, &new.source, &new.source_ref).await?.is_some() {
        return Ok(None);
    }
    match insert(catalog, &new).await {
        Ok(grant) => Ok(Some(grant)),
        Err(e) => match find(catalog, &new.source, &new.source_ref).await? {
            Some(_) => Ok(None),
            None => Err(e),
        },
    }
}

/// Revoke the grant `(source, source_ref)` names. `false` when there is none,
/// or it was already revoked. The row stays, revoked.
pub async fn revoke_role_grant(catalog: &Catalog, source: &str, source_ref: &str) -> Result<bool> {
    revoke_where(
        catalog,
        Expr::col(COL_GRANT_SOURCE)
            .eq(Expr::lit(source))
            .and(Expr::col(COL_GRANT_SOURCE_REF).eq(Expr::lit(source_ref))),
    )
    .await
}

/// Revoke one grant by its id. `false` when there is none, or it was already
/// revoked.
pub async fn revoke_role_grant_by_id(catalog: &Catalog, id: Uuid) -> Result<bool> {
    revoke_where(catalog, Expr::col(COL_GRANT_ID).eq(Expr::lit(id))).await
}

/// The grant `(source, source_ref)` names, in whatever state.
pub async fn find_role_grant(
    catalog: &Catalog,
    source: &str,
    source_ref: &str,
) -> Result<Option<RoleGrant>> {
    find(catalog, source, source_ref).await
}

/// One grant by id.
pub async fn load_role_grant(catalog: &Catalog, id: Uuid) -> Result<Option<RoleGrant>> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(None);
    }
    let select = Select::from(Source::table(ROLE_GRANTS_TABLE))
        .filter(Expr::col(COL_GRANT_ID).eq(Expr::lit(id)))
        .limit(1);
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(grant_from_row)
        .transpose()
}

/// Every grant a user has had, newest start first: in force, ended and revoked.
pub async fn list_role_grants(catalog: &Catalog, user: Uuid) -> Result<Vec<RoleGrant>> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let mut select = Select::from(Source::table(ROLE_GRANTS_TABLE))
        .filter(Expr::col(COL_GRANT_USER).eq(Expr::lit(user)));
    select.order = vec![OrderBy::desc(Expr::col(COL_GRANT_STARTS_AT))];
    rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(grant_from_row)
        .collect()
}

/// Delete every grant a user has had — what deleting the user does first,
/// since the rows reference them.
pub(crate) async fn delete_grants_for_user(catalog: &Catalog, user: Uuid) -> Result<()> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(());
    }
    let delete =
        Delete::from(ROLE_GRANTS_TABLE).filter(Expr::col(COL_GRANT_USER).eq(Expr::lit(user)));
    rows(catalog, Statement::from(delete)).await?;
    Ok(())
}

/// Before deleting `role`: refuse while a grant of it is in force (or still to
/// start), and delete the ones that have ended — history that would otherwise
/// keep a role nobody holds from ever being deleted.
pub(crate) async fn clear_ended_grants_of_role(catalog: &Catalog, role: u8) -> Result<()> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(());
    }
    let now = Utc::now();
    let of_role = || Expr::col(COL_GRANT_ROLE).eq(Expr::lit(i64::from(role)));
    let ended = Expr::unary(UnOp::IsNotNull, Expr::col(COL_GRANT_REVOKED_AT)).or(Expr::binary(
        BinOp::Le,
        Expr::col(COL_GRANT_EXPIRES_AT),
        Expr::Lit(Value::Timestamp(now)),
    ));
    let live = Select::from(Source::table(ROLE_GRANTS_TABLE))
        .filter(of_role().and(Expr::unary(UnOp::Not, ended.clone())));
    let live = rows(catalog, Statement::from(live)).await?.len();
    if live > 0 {
        return Err(Error::invalid(format!(
            "role {role} is held through {live} grant(s) that have not ended; \
             revoke them before deleting it"
        )));
    }
    let delete = Delete::from(ROLE_GRANTS_TABLE).filter(of_role().and(ended));
    rows(catalog, Statement::from(delete)).await?;
    Ok(())
}

/// Refuse a grant that cannot mean anything: a role that does not exist, the
/// public role (everybody's already), an empty source or reference, or an end
/// before its start.
async fn check_new(catalog: &Catalog, new: &NewRoleGrant) -> Result<()> {
    if new.role >= ROLE_PUBLIC || new.role < ROLE_ADMIN {
        return Err(Error::invalid(format!(
            "role {} cannot be granted: grants run from 1 to 99, and the public role is \
             everybody's already",
            new.role
        )));
    }
    if load_role(catalog, new.role).await?.is_none() {
        return Err(Error::invalid(format!(
            "role {} does not exist; create it before granting it",
            new.role
        )));
    }
    if new.source.trim().is_empty() || new.source_ref.trim().is_empty() {
        return Err(Error::invalid(
            "a role grant needs a source and a reference, which together name it",
        ));
    }
    if let (Some(end), start) = (new.expires_at, new.starts_at.unwrap_or_else(Utc::now))
        && end <= start
    {
        return Err(Error::invalid(format!(
            "a role grant that ends at {end} has ended before it starts at {start}"
        )));
    }
    Ok(())
}

/// Create `role_grants` if it is not there yet: what the first grant does on an
/// installation where no module declared it (an admin's grant by hand, the
/// `grant_role` action). Once created, it stays.
async fn ensure_table(catalog: &Catalog) -> Result<()> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_some() {
        return Ok(());
    }
    let _creating = crate::CREATING_TABLES.lock().await;
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        bootstrap_role_grants(catalog).await?;
    }
    Ok(())
}

async fn find(catalog: &Catalog, source: &str, source_ref: &str) -> Result<Option<RoleGrant>> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(None);
    }
    let select = Select::from(Source::table(ROLE_GRANTS_TABLE))
        .filter(
            Expr::col(COL_GRANT_SOURCE)
                .eq(Expr::lit(source))
                .and(Expr::col(COL_GRANT_SOURCE_REF).eq(Expr::lit(source_ref))),
        )
        .limit(1);
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(grant_from_row)
        .transpose()
}

async fn insert(catalog: &Catalog, new: &NewRoleGrant) -> Result<RoleGrant> {
    let now = Utc::now();
    let insert = Insert::row(
        ROLE_GRANTS_TABLE,
        [
            COL_GRANT_ID,
            COL_GRANT_USER,
            COL_GRANT_ROLE,
            COL_GRANT_SOURCE,
            COL_GRANT_SOURCE_REF,
            COL_GRANT_STARTS_AT,
            COL_GRANT_EXPIRES_AT,
            COL_GRANT_NOTE,
            COL_GRANT_CREATED_AT,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::lit(Uuid::new_v4()),
            Expr::lit(new.user_id),
            Expr::lit(i64::from(new.role)),
            Expr::lit(new.source.as_str()),
            Expr::lit(new.source_ref.as_str()),
            Expr::Lit(Value::Timestamp(new.starts_at.unwrap_or(now))),
            optional_time(new.expires_at),
            optional_text(new.note.as_deref()),
            Expr::Lit(Value::Timestamp(now)),
        ],
    )
    .returning(vec![Projection::all()]);
    let written = rows(catalog, Statement::from(insert)).await?;
    written
        .first()
        .map(grant_from_row)
        .transpose()?
        .ok_or_else(|| Error::database("inserting a role grant returned no row"))
}

async fn update_by_key(catalog: &Catalog, new: &NewRoleGrant) -> Result<Option<RoleGrant>> {
    let update = Update {
        table: ROLE_GRANTS_TABLE.to_owned(),
        assignments: vec![
            Assignment::new(COL_GRANT_USER, Expr::lit(new.user_id)),
            Assignment::new(COL_GRANT_ROLE, Expr::lit(i64::from(new.role))),
            Assignment::new(
                COL_GRANT_STARTS_AT,
                Expr::Lit(Value::Timestamp(new.starts_at.unwrap_or_else(Utc::now))),
            ),
            Assignment::new(COL_GRANT_EXPIRES_AT, optional_time(new.expires_at)),
            Assignment::new(COL_GRANT_REVOKED_AT, Expr::Lit(Value::Null)),
            Assignment::new(COL_GRANT_NOTE, optional_text(new.note.as_deref())),
        ],
        filter: Some(
            Expr::col(COL_GRANT_SOURCE)
                .eq(Expr::lit(new.source.as_str()))
                .and(Expr::col(COL_GRANT_SOURCE_REF).eq(Expr::lit(new.source_ref.as_str()))),
        ),
        returning: vec![Projection::all()],
    };
    rows(catalog, Statement::from(update))
        .await?
        .first()
        .map(grant_from_row)
        .transpose()
}

async fn revoke_where(catalog: &Catalog, filter: Expr) -> Result<bool> {
    if catalog.get(ROLE_GRANTS_TABLE)?.is_none() {
        return Ok(false);
    }
    let update = Update {
        table: ROLE_GRANTS_TABLE.to_owned(),
        assignments: vec![Assignment::new(
            COL_GRANT_REVOKED_AT,
            Expr::Lit(Value::Timestamp(Utc::now())),
        )],
        filter: Some(filter.and(Expr::unary(UnOp::IsNull, Expr::col(COL_GRANT_REVOKED_AT)))),
        returning: vec![Projection::expr(Expr::col(COL_GRANT_ID))],
    };
    Ok(!rows(catalog, Statement::from(update)).await?.is_empty())
}

fn optional_time(t: Option<DateTime<Utc>>) -> Expr {
    Expr::Lit(t.map_or(Value::Null, Value::Timestamp))
}

fn optional_text(t: Option<&str>) -> Expr {
    match t.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Expr::lit(t),
        None => Expr::Lit(Value::Null),
    }
}

fn grant_from_row(row: &Row) -> Result<RoleGrant> {
    let bad = |column: &str| Error::invalid(format!("{ROLE_GRANTS_TABLE}.{column} is malformed"));
    let uuid = |column: &str| match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        _ => Err(bad(column)),
    };
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        _ => Err(bad(column)),
    };
    let time = |column: &str| match row.get(column) {
        Some(Value::Timestamp(t)) => Ok(Some(*t)),
        Some(Value::Null) | None => Ok(None),
        _ => Err(bad(column)),
    };
    let role = match row.get(COL_GRANT_ROLE) {
        Some(Value::Int(i)) => u8::try_from(*i).map_err(|_| bad(COL_GRANT_ROLE))?,
        _ => return Err(bad(COL_GRANT_ROLE)),
    };
    Ok(RoleGrant {
        id: uuid(COL_GRANT_ID)?,
        user_id: uuid(COL_GRANT_USER)?,
        role,
        source: text(COL_GRANT_SOURCE)?,
        source_ref: text(COL_GRANT_SOURCE_REF)?,
        starts_at: time(COL_GRANT_STARTS_AT)?.ok_or_else(|| bad(COL_GRANT_STARTS_AT))?,
        expires_at: time(COL_GRANT_EXPIRES_AT)?,
        revoked_at: time(COL_GRANT_REVOKED_AT)?,
        note: match row.get(COL_GRANT_NOTE) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        },
        created_at: time(COL_GRANT_CREATED_AT)?.ok_or_else(|| bad(COL_GRANT_CREATED_AT))?,
    })
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn grant(role: u8, starts: i64, ends: Option<i64>) -> RoleGrant {
        let now = Utc::now();
        RoleGrant {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            role,
            source: "test".into(),
            source_ref: Uuid::new_v4().to_string(),
            starts_at: now + Duration::seconds(starts),
            expires_at: ends.map(|e| now + Duration::seconds(e)),
            revoked_at: None,
            note: None,
            created_at: now,
        }
    }

    #[test]
    fn the_most_privileged_grant_in_force_wins_and_never_lowers_the_base() {
        let now = Utc::now();
        // A grant less privileged than the base changes nothing.
        let r = resolve(40, &[grant(70, -10, Some(100))], now);
        assert_eq!(r.role, 40);
        assert_eq!(r.changes_at, None);
        // Two grants in force: the more privileged one, until it ends.
        let g60 = grant(60, -10, Some(100));
        let g50 = grant(50, -10, Some(50));
        let r = resolve(80, &[g60.clone(), g50.clone()], now);
        assert_eq!(r.role, 50);
        assert_eq!(r.changes_at, g50.expires_at);
        // A grant still to start does not count yet, but its start is a change.
        let later = grant(30, 20, None);
        let r = resolve(80, std::slice::from_ref(&later), now);
        assert_eq!(r.role, 80);
        assert_eq!(r.changes_at, Some(later.starts_at));
        // A grant until revoked has no end to wait for.
        let r = resolve(80, &[grant(60, -10, None)], now);
        assert_eq!((r.role, r.changes_at), (60, None));
    }

    #[test]
    fn the_key_is_the_source_and_its_reference() {
        let names: Vec<String> = grant_fields().into_iter().map(|f| f.base.name).collect();
        assert!(names.contains(&COL_GRANT_SOURCE.to_owned()));
        assert_eq!(
            source_key(),
            sc_catalog::ConstraintKind::Unique {
                fields: vec!["source".into(), "source_ref".into()]
            }
        );
    }
}
