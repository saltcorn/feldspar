//! **Schema edits a module makes** (TODO ST1.2): the `schema` surface behind
//! `schema` in an action's argument.
//!
//! ```js
//! await schema.apply([
//!   { op: "create_table", table: "ut_recipes",
//!     fields: [{ field: "id", type: "Integer", primary_key: true },
//!              { field: "title", type: "String", required: true }] },
//! ]);
//! ```
//!
//! An application whose users define their own tables changes the schema from
//! a custom query, on behalf of an ordinary user. The path is the custom query
//! → `trigger("…")` (a trigger only an admin may run, reached by the query's
//! `asAdmin()`) → a module's action → this ask → [`schema_edit::apply`], so the
//! batch is validated, applied in one transaction and refused whole exactly as
//! the admin API's and MCP `edit_schema`'s are. The operations are MCP
//! `edit_schema`'s operation JSON ([`sc_api::mcp::parse_operations`]).
//!
//! # What bounds it
//!
//! The application decides **who** may (its own rules: the owner, the members,
//! a quota), in the custom query. The server bounds **what** can be done,
//! because a check inside the module's JavaScript or the query would be on the
//! wrong side of the boundary:
//!
//! - **The grant** ([`SchemaGrant`]): which of create, edit and drop the admin
//!   allowed the module, on the Modules tab or by installing a bundled module
//!   whose manifest requests it.
//! - **The prefix**: every table an operation creates, alters, renames (both
//!   names) or drops must start with the grant's prefix. A key may *point at* a
//!   table outside it (`users`, the application's own tables), because that
//!   changes only the table the key is on.
//! - **By name, whatever the prefix**: this server's own tables (`_fd_*`,
//!   `users`, the role and credit tables), a table a module declared, a table a
//!   provider serves and a table in another database.
//! - **Who ran the action**: the ask is served only when the trigger that ran
//!   the action is one only an admin may run (no `min_role`, or the admin's) —
//!   reached from a code body's or another trigger's elevation. A trigger an
//!   application exposes and opens to a lower role cannot reach it, because
//!   then its caller, not configuration, chose to run it.
//!
//! The editor is [`Editor::Delegated`]: on a table with row-level security only
//! a person changes the access rules, disables RLS or drops it.
//!
//! # After the batch
//!
//! A table the batch creates is connected to the application whose request ran
//! the action (TODO ST1.3) and a table it drops is disconnected; the row API is
//! not opened. The answer carries the catalog's generation after the reload. A
//! code body's `db.<table>` resolves names against the live catalog, so the
//! rest of the run can read and write a table it just created; v1's
//! `Table.findOne` answers from the schema snapshot the run started with, so it
//! sees the table from the next run (TODO ST1.4).

use sc_action::ActionContext;
use sc_api::schema_edit::{self, ApplyOptions, Grants, Operation};
use sc_catalog::{Catalog, DbId, Editor};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::host::Ask;
use crate::permissions::SchemaGrant;

/// Who a module's schema edit is made by: the module, for the user whose
/// request ran its trigger, within the grant an admin gave it. Named in the
/// log line every applied batch writes, and listed in the system-authority
/// inventory.
pub const MODULE_SCHEMA_EDIT: &str = "a module's schema ask (sc-module schema.rs)";

/// This server's own tables that are not `_fd_*`: refused by name, whatever a
/// prefix happens to match.
const CORE_TABLES: [&str; 5] = [
    sc_auth::USERS_TABLE,
    sc_auth::ROLE_GRANTS_TABLE,
    sc_auth::CREDIT_BUCKETS_TABLE,
    sc_auth::CREDIT_ENTRIES_TABLE,
    sc_auth::CREDIT_ACCOUNTS_TABLE,
];

/// One module's schema edits, over one action's call.
pub struct ModuleSchema<'a> {
    catalog: &'a Catalog,
    module: &'a str,
    grant: &'a SchemaGrant,
    /// Why this call may not change the schema, when it may not: the trigger
    /// that ran the action is open to a role below the admin's.
    refused: Option<String>,
    /// Who caused the event, for the log line.
    user: Option<Json>,
    /// The application whose request ran the action, if one did.
    app: Option<Uuid>,
}

impl<'a> ModuleSchema<'a> {
    /// The schema edits `module` may make under `grant`, for the action
    /// running in `ctx`.
    pub fn for_action(
        ctx: &ActionContext<'a>,
        module: &'a str,
        grant: &'a SchemaGrant,
    ) -> ModuleSchema<'a> {
        ModuleSchema {
            catalog: ctx.catalog,
            module,
            grant,
            refused: admin_only_trigger(ctx).err(),
            user: ctx.event.user.clone(),
            app: ctx
                .event
                .app
                .as_ref()
                .map(|app| app.id)
                .or_else(|| ctx.application()),
        }
    }

    /// The same edits, with no trigger to check and no application — for a
    /// test that drives the ask directly.
    pub fn new(catalog: &'a Catalog, module: &'a str, grant: &'a SchemaGrant) -> ModuleSchema<'a> {
        ModuleSchema {
            catalog,
            module,
            grant,
            refused: None,
            user: None,
            app: None,
        }
    }

    /// The same, as run for the application `app` — what connects the tables a
    /// batch creates.
    #[must_use]
    pub fn for_application(mut self, app: Option<Uuid>) -> ModuleSchema<'a> {
        self.app = app;
        self
    }

    async fn apply(&self, plan: &Map<String, Json>) -> Result<Json> {
        if let Some(why) = &self.refused {
            return Err(Error::auth(why.clone()));
        }
        let items = match plan.get("operations") {
            Some(Json::Array(items)) if !items.is_empty() => items.as_slice(),
            Some(Json::Array(_)) | None => {
                return Err(Error::invalid(
                    "schema.apply needs `operations`: a list of at least one operation, as \
                     MCP edit_schema takes them",
                ));
            }
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`operations` should be a list of operations, got {other}"
                )));
            }
        };
        let dry_run = match plan.get("dry_run") {
            None | Some(Json::Null) => false,
            Some(Json::Bool(b)) => *b,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`dry_run` should be true or false, got {other}"
                )));
            }
        };
        let operations = sc_api::mcp::parse_operations(self.catalog, items)?;
        for (index, op) in operations.iter().enumerate() {
            self.check(op)
                .map_err(|e| Error::invalid(format!("operation {index}: {e}")))?;
        }
        if !dry_run {
            // Who changed the schema, under which grant, for whom: the
            // inventory's promise, kept where every applied batch passes.
            eprintln!(
                "feldspar: {MODULE_SCHEMA_EDIT}: module `{}` ({}) applies {} \
                 operation{} for user {}",
                self.module,
                self.grant.sentence(),
                operations.len(),
                if operations.len() == 1 { "" } else { "s" },
                self.user
                    .as_ref()
                    .and_then(|u| u.get("id"))
                    .map_or_else(|| "none".to_owned(), ToString::to_string),
            );
        }
        let applied = schema_edit::apply(
            self.catalog,
            &operations,
            &ApplyOptions {
                grants: grants_of(self.grant),
                dry_run,
                editor: Editor::Delegated(MODULE_SCHEMA_EDIT),
                ids: schema_edit::GivenIds::default(),
                database_types: false,
            },
        )
        .await?;
        let mut answer = sc_api::mcp::applied_json(&applied);
        let mut notes: Vec<Json> = answer["notes"].as_array().cloned().unwrap_or_default();
        answer["connected"] = if applied.dry_run {
            Json::Null
        } else {
            self.connect(&applied, &mut notes).await?
        };
        answer["generation"] = json!(self.catalog.generation());
        if !applied.dry_run && !applied.tables_created.is_empty() {
            notes.push(json!(
                "the rest of this run reaches the new tables through `db` (and v1's \
                 `insertRow` and friends on a table found by name); v1's synchronous \
                 `Table.findOne` and `Table.find` answer from the schema the run started \
                 with, so they see the new tables from the next run"
            ));
        }
        answer["notes"] = Json::Array(notes);
        Ok(answer)
    }

    /// Connect what the batch created to the calling application, and
    /// disconnect what it dropped (TODO ST1.3). The row API stays off.
    async fn connect(&self, applied: &schema_edit::Applied, notes: &mut Vec<Json>) -> Result<Json> {
        let added = applied.tables_created.clone();
        let removed = applied.tables_dropped.clone();
        if added.is_empty() && removed.is_empty() {
            return Ok(Json::Null);
        }
        let Some(app) = self.app else {
            if !added.is_empty() {
                notes.push(json!(
                    "no application's request ran this action, so the new tables are not \
                     connected to any application; connect them in the application's settings"
                ));
            }
            return Ok(Json::Null);
        };
        let change = sc_app::TableChange {
            add: added.clone(),
            remove: removed.clone(),
            set: None,
        };
        let Some((stored, changed)) =
            sc_app::change_stored_app_tables(self.catalog, sc_app::AppId(app), &change).await?
        else {
            notes.push(json!(format!(
                "the application {app} that ran this action no longer exists, so the new \
                 tables are not connected to it"
            )));
            return Ok(Json::Null);
        };
        if changed {
            match self.catalog.notify_application_changed(app).await {
                Ok(Some(more)) => notes.extend(more.into_iter().map(Json::String)),
                Ok(None) => notes.push(json!(format!(
                    "application `{}` is not served by this process, so its new tables are \
                     served from its next start",
                    stored.name
                ))),
                Err(e) => notes.push(json!(format!(
                    "application `{}` could not be served again with its new tables: {e}",
                    stored.name
                ))),
            }
        }
        Ok(json!({
            "application": app,
            "name": stored.name,
            "added": added,
            "removed": removed,
        }))
    }

    /// Refuse an operation the grant does not allow, or that names a table the
    /// module may not change.
    fn check(&self, op: &Operation) -> Result<()> {
        let (granted, what, switch) = match op {
            Operation::CreateTable { .. } => (self.grant.create, "create a table", "create"),
            Operation::AlterTable { .. } => (self.grant.edit, "change a table's settings", "edit"),
            Operation::AddField { .. } => (self.grant.edit, "add a field", "edit"),
            Operation::AlterField { settings, .. } if settings.unique == Some(false) => (
                self.grant.edit && self.grant.drop,
                "drop a unique constraint",
                "edit and drop",
            ),
            Operation::AlterField { .. } => (self.grant.edit, "change a field", "edit"),
            Operation::RenameTable { .. } => (self.grant.edit, "rename a table", "edit"),
            Operation::AddConstraint { .. } => (self.grant.edit, "add a constraint", "edit"),
            Operation::DropField { .. } => (self.grant.drop, "drop a field", "drop"),
            Operation::DropTable { .. } => (self.grant.drop, "drop a table", "drop"),
            Operation::DropConstraint { .. } => (self.grant.drop, "drop a constraint", "drop"),
        };
        if !granted {
            return Err(Error::auth(format!(
                "module `{}` may not {what} ({}); the whole batch was refused and nothing was \
                 applied. An admin can allow `{switch}` on the Modules tab (\"Tables it may \
                 change\")",
                self.module,
                self.grant.sentence()
            )));
        }
        if let Operation::CreateTable { database, .. } = op
            && !(database.is_empty() || *database == DbId::primary().0)
        {
            return Err(Error::auth(format!(
                "module `{}` may only create tables in this server's own database, not in \
                 `{database}`",
                self.module
            )));
        }
        self.check_table(op.table())?;
        if let Operation::RenameTable { to, .. } = op {
            self.check_table(to)?;
        }
        Ok(())
    }

    /// Refuse a table the module may not change, by name.
    fn check_table(&self, table: &str) -> Result<()> {
        let refuse = |why: String| {
            Err(Error::auth(format!(
                "module `{}` may not change `{table}`: {why}; the whole batch was refused and \
                 nothing was applied",
                self.module
            )))
        };
        if table.starts_with('_') || CORE_TABLES.contains(&table) {
            return refuse("it is one of this server's own tables".to_owned());
        }
        if !self.grant.covers(table) {
            return refuse(format!(
                "it may only change tables whose names start with `{}`, and a key may point \
                 at another table but not change it",
                self.grant.prefix
            ));
        }
        if let Some(owner) = self.catalog.module_tables().module_of(table) {
            return refuse(format!("it is a table the module `{owner}` declares"));
        }
        if let Some(existing) = self.catalog.get(table)? {
            if existing.provider().is_some() {
                return refuse("its rows are served by a table provider".to_owned());
            }
            if existing.database != DbId::primary() {
                return refuse(format!(
                    "it is in the connected database `{}`",
                    existing.database.0
                ));
            }
        }
        Ok(())
    }
}

/// The grants a batch is applied with: the module's three switches.
fn grants_of(grant: &SchemaGrant) -> Grants {
    Grants {
        create: grant.create,
        edit: grant.edit,
        drop: grant.drop,
    }
}

/// Whether the trigger running this action is one only an admin may run — so
/// that what reached it was configuration (another trigger, a code body's
/// `asAdmin()`, an event), not a caller an application let run it.
fn admin_only_trigger(ctx: &ActionContext<'_>) -> std::result::Result<(), String> {
    let name = ctx.trigger;
    let Some(dispatcher) = ctx.triggers() else {
        return Err(format!(
            "`{name}` was not run by this server's trigger dispatcher, so whether only an \
             admin may run it cannot be checked, and a schema edit is refused"
        ));
    };
    let triggers = dispatcher
        .triggers()
        .map_err(|e| format!("the trigger set could not be read: {e}"))?;
    let Some(trigger) = triggers.all().iter().find(|t| t.name == name) else {
        return Err(format!(
            "`{name}` is not a stored trigger, and a module changes the schema only from a \
             trigger that only an admin may run"
        ));
    };
    match trigger.min_role {
        None => Ok(()),
        Some(role) if role.is_admin() => Ok(()),
        Some(role) => Err(format!(
            "trigger `{name}` may be run by role {role}, so its caller and not this server's \
             configuration chose to run it; a module changes the schema only from a trigger \
             only an admin may run (no minimum role), reached from a custom query's \
             `trigger(\"{name}\").asAdmin().run(…)` or another trigger"
        )),
    }
}

#[async_trait::async_trait]
impl Ask for ModuleSchema<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let Json::Object(plan) = plan else {
            return Err(Error::invalid("a schema ask is an object with an `op`"));
        };
        match plan.get("op").and_then(Json::as_str).unwrap_or_default() {
            "apply" => self.apply(&plan).await,
            other => Err(Error::invalid(format!(
                "`{other}` is not a schema operation; there is apply"
            ))),
        }
    }
}
