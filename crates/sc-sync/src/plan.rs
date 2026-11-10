//! The planner: from live and a target tree, the operations that turn one into
//! the other (APP_SYNC_PROPOSAL §5, §6 step 8).
//!
//! Deploy plans the branch tip; a discard plans the base's version of what it
//! discards (S3.6); a registry save plans the instance tree with one entity
//! replaced (§7.6). All three are this one function, because each is "make live
//! look like this tree", and each is classified the same way (§5).
//!
//! **Every operation is classified**, and its class sets its default tick:
//!
//! | Class | Default |
//! |---|---|
//! | [`Safe`](OpClass::Safe): create, update, add a field, relax a constraint, rename by id | ticked |
//! | [`MetadataDelete`](OpClass::MetadataDelete): a view, a trigger, a store file… | ticked |
//! | [`CanFail`](OpClass::CanFail): make a field required, add a unique constraint, change a key's target | ticked, dry-run against live data ([`crate::validate()`]) |
//! | [`DataDestructive`](OpClass::DataDestructive): drop a table or field, delete a store or connection | **unticked** |
//! | [`Unsupported`](OpClass::Unsupported): change a column's SQL type or a primary key | **blocked** |
//!
//! **The order is §6 step 8's**: modules, roles, connections and stores (and
//! their files), tables and fields, LLM providers and MCP servers, triggers,
//! streams and agents, analytics, applications and what is in them, settings,
//! and then every delete, in the reverse of that order so that what refers to
//! something goes before it.
//!
//! The planner reads only the two trees (and the base, for a git store's pin).
//! What needs the database — the dry runs, whether a role is held, whether a
//! store has unpushed work — is [`crate::validate()`]'s.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value as Json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use sc_api::schema_edit::{FieldSettings, GivenIds, Operation};
use sc_backup::codec::{
    constraint_from_body, field_spec_from_body, parse_field_kind, table_settings_from_body,
};
use sc_catalog::DataFieldKind;
use sc_files::{CFG_PUBLIC_URL, CFG_URL, derive_https_url};
use sc_types::SECRET_SENTINEL;

use crate::diff::{is_git_store, match_sides, pin};
use crate::entity::{Entities, Entity, Kind};
use crate::format::check_format;
use crate::tree::StructureTree;

/// Where in §6 step 8 an operation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// 8.1: modules.
    Modules = 1,
    /// 8.2: roles.
    Roles = 2,
    /// 8.3: database connections, file stores and code libraries; then each
    /// git store's pin, and local stores' synced files.
    Stores = 3,
    /// 8.4: tables and fields, through `schema_edit::apply`.
    Tables = 4,
    /// 8.5: LLM providers and MCP servers.
    Providers = 5,
    /// 8.6: triggers, streams and agents.
    Automations = 6,
    /// 8.7: analytics.
    Analytics = 7,
    /// 8.8: applications, then their views, pages, library and translations.
    Applications = 8,
    /// 8.9: portable settings.
    Settings = 9,
    /// 8.10: deletes, in reverse order.
    Deletes = 10,
}

/// Which step an entity of `kind` is created or updated in.
pub fn step_of(kind: Kind) -> Step {
    match kind {
        Kind::Module => Step::Modules,
        Kind::Role => Step::Roles,
        Kind::DbConnection | Kind::FileStore | Kind::StoreFile | Kind::CodeLibrary => Step::Stores,
        Kind::Table | Kind::Users => Step::Tables,
        Kind::LlmProvider | Kind::McpServer => Step::Providers,
        Kind::Trigger | Kind::Stream | Kind::Agent => Step::Automations,
        Kind::Dataset | Kind::Model | Kind::Workspace => Step::Analytics,
        Kind::Application
        | Kind::View
        | Kind::Page
        | Kind::LibraryItem
        | Kind::Translation
        | Kind::Checklist
        | Kind::Other => Step::Applications,
        Kind::Setting => Step::Settings,
    }
}

/// How dangerous an operation is (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpClass {
    /// Nothing is lost.
    Safe,
    /// A definition is deleted; it is recoverable from git history.
    MetadataDelete,
    /// It may be refused by the data already there; validated by a dry run.
    CanFail,
    /// Data is lost. Opt-in, per operation.
    DataDestructive,
    /// Not something a deploy can do.
    Unsupported,
}

impl OpClass {
    /// Whether an operation of this class is ticked unless the operator
    /// unticks it.
    pub fn ticked_by_default(self) -> bool {
        matches!(
            self,
            OpClass::Safe | OpClass::MetadataDelete | OpClass::CanFail
        )
    }
}

/// What applying an operation does.
#[derive(Debug, Clone)]
pub enum OpBody {
    /// One `schema_edit::apply` batch.
    Schema {
        /// The batch, naming tables and fields as they are when it runs.
        batch: Vec<Operation>,
        /// The ids the tables and fields it creates are given (§3.2).
        ids: GivenIds,
        /// The same change against live as it is now, for a can-fail
        /// operation's dry run: live's table and field names, since the renames
        /// before it have not happened yet.
        check: Vec<Operation>,
    },
    /// Create or update an entity through its own save path.
    Put {
        /// What is there now.
        live: Option<Box<Entity>>,
        /// What it becomes. A name match with another id carries live's id
        /// (§3.2), and a git store's pin is the one to reset to.
        target: Box<Entity>,
    },
    /// Delete an entity.
    Delete {
        /// What is there now.
        live: Box<Entity>,
    },
}

/// One operation of a plan.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedOp {
    /// Stable across re-plans of the same trees: the action and the entity
    /// (`drop_field:table:tickets#status`). A confirm ticks operations by it.
    pub id: String,
    /// Where it runs.
    pub step: Step,
    /// How dangerous it is.
    pub class: OpClass,
    /// Whether it is ticked.
    pub ticked: bool,
    /// The kind of entity.
    pub kind: Kind,
    /// The entity's key.
    pub entity: String,
    /// What it does: `create`, `update`, `delete`, `rename_table`,
    /// `add_field`, `drop_field`, `rename_field`, ….
    pub action: &'static str,
    /// What it does, in a sentence.
    pub summary: String,
    /// How to apply it.
    #[serde(skip)]
    pub body: OpBody,
    #[serde(skip)]
    order: (Step, u8, u32),
}

/// Something that stops the plan being applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    /// The operation it is about, if one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// The entity it is about, if one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// Why, in a sentence that says what to do.
    pub message: String,
}

/// A plan: the operations in order, what stops them, and a digest a confirm
/// carries back.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// Every operation, in the order it is applied.
    pub operations: Vec<PlannedOp>,
    /// What stops it; Deploy is disabled while there is any.
    pub blockers: Vec<Blocker>,
    /// Entities the plan creates with a secret this instance does not have
    /// yet (§3.3), for the receipt.
    pub needs_secrets: Vec<String>,
    /// New git stores this instance cannot clone without credentials (§3.5).
    pub needs_credentials: Vec<String>,
    /// A digest of what the plan was made from: it changes exactly when live,
    /// the target or the base does (§6 step 6).
    pub digest: String,
}

impl Plan {
    /// The operation with this id.
    pub fn op(&self, id: &str) -> Option<&PlannedOp> {
        self.operations.iter().find(|op| op.id == id)
    }

    /// Whether nothing would be done.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// How many operations are in each class.
    pub fn counts(&self) -> BTreeMap<OpClass, usize> {
        let mut counts = BTreeMap::new();
        for op in &self.operations {
            *counts.entry(op.class).or_insert(0) += 1;
        }
        counts
    }
}

/// The digest of a plan made from these trees.
pub fn plan_digest(
    live: &StructureTree,
    target: &StructureTree,
    base: Option<&StructureTree>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"feldspar-plan/1\n");
    for tree in [Some(live), Some(target), base] {
        hasher.update(tree.map_or_else(|| "-".to_owned(), StructureTree::digest));
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// Plan turning `live` into `target`.
///
/// `base` is the base tree, when there is one: a git store whose pin is the
/// same on the target as in the base keeps live's pin, because a pin that
/// moved here is not something a deploy undoes (§3.5).
pub fn plan(live: &StructureTree, target: &StructureTree, base: Option<&StructureTree>) -> Plan {
    let mut planner = Planner::default();
    if let Err(e) = check_format(target) {
        planner.block(None, None, e.to_string());
    }
    let mut sides = vec![Entities::of(live), Entities::of(target)];
    if let Some(base) = base {
        sides.push(Entities::of(base));
    }
    let groups = match_sides(&sides);
    let pick = |g: &crate::diff::Group, side: usize| -> Option<&Entity> {
        g.members
            .get(side)
            .copied()
            .flatten()
            .map(|i| &sides[side].list[i])
    };
    let live_tables = live_module_tables(&sides[0]);
    for group in &groups {
        let (l, t, b) = (pick(group, 0), pick(group, 1), pick(group, 2));
        match group.kind {
            Kind::Table => planner.table(l, t),
            Kind::Users => planner.users(l, t),
            Kind::Other => {
                if let Some(t) = t
                    && l.is_none_or(|l| !l.same_as(t))
                {
                    planner.block(
                        None,
                        Some(t.key()),
                        format!(
                            "`{}` is not something this version of Feldspar knows how to \
                             deploy; upgrade this instance, or remove the file from the \
                             repository",
                            t.path
                        ),
                    );
                }
            }
            _ => planner.entity(l, t, b, &live_tables),
        }
    }
    planner.secrets(&sides[0], &sides[1], &groups);
    planner.git_clones(&sides[1], &groups);

    let mut operations = planner.ops;
    operations.sort_by(|a, b| (a.order, &a.id).cmp(&(b.order, &b.id)));
    Plan {
        operations,
        blockers: planner.blockers,
        needs_secrets: planner.needs_secrets,
        needs_credentials: planner.needs_credentials,
        digest: plan_digest(live, target, base),
    }
}

/// Which module provides each live table that a module provides.
fn live_module_tables(live: &Entities) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in live.list.iter().filter(|e| e.kind == Kind::Table) {
        if let Some(module) = e
            .json()
            .and_then(|v| v.get("module"))
            .and_then(Json::as_str)
        {
            out.entry(module.to_owned())
                .or_default()
                .push(e.name.clone());
        }
    }
    out
}

#[derive(Default)]
struct Planner {
    ops: Vec<PlannedOp>,
    blockers: Vec<Blocker>,
    needs_secrets: Vec<String>,
    needs_credentials: Vec<String>,
}

/// The words for a kind in a summary.
fn noun(kind: Kind) -> &'static str {
    match kind {
        Kind::Module => "module",
        Kind::Role => "role",
        Kind::Setting => "setting",
        Kind::Users => "users table",
        Kind::Table => "table",
        Kind::DbConnection => "database connection",
        Kind::FileStore => "file store",
        Kind::StoreFile => "file",
        Kind::CodeLibrary => "code library",
        Kind::LlmProvider => "LLM provider",
        Kind::McpServer => "MCP server",
        Kind::Trigger => "trigger",
        Kind::Stream => "stream",
        Kind::Agent => "agent",
        Kind::Dataset => "dataset",
        Kind::Model => "model",
        Kind::Workspace => "workspace",
        Kind::Application => "application",
        Kind::View => "view",
        Kind::Page => "page",
        Kind::LibraryItem => "library item",
        Kind::Translation => "translation",
        Kind::Checklist => "checklist",
        Kind::Other => "file",
    }
}

/// An entity's display name in a summary: a child with its parent's segment.
fn shown(e: &Entity) -> String {
    e.key()
        .split_once(':')
        .map_or_else(|| e.name.clone(), |(_, n)| n.to_owned())
}

/// The order a delete of `kind` runs in: the reverse of the order it would be
/// created in, children before parents.
fn delete_rank(kind: Kind) -> u32 {
    10_000 - (step_of(kind) as u32 * 100 + kind_rank(kind))
}

fn kind_rank(kind: Kind) -> u32 {
    let index = Kind::ALL.iter().position(|k| *k == kind).unwrap_or(0);
    u32::try_from(index).unwrap_or(u32::MAX)
}

impl Planner {
    fn block(&mut self, op: Option<String>, entity: Option<String>, message: String) {
        self.blockers.push(Blocker {
            op,
            entity,
            message,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: Kind,
        entity: &str,
        action: &'static str,
        sub: Option<&str>,
        class: OpClass,
        summary: String,
        body: OpBody,
        sub_order: u8,
    ) -> String {
        let id = match sub {
            Some(sub) => format!("{action}:{entity}#{sub}"),
            None => format!("{action}:{entity}"),
        };
        let deleting =
            matches!(body, OpBody::Delete { .. }) || matches!(action, "drop_table" | "drop_field");
        let (step, rank) = if deleting {
            (Step::Deletes, delete_rank(kind))
        } else {
            (step_of(kind), kind_rank(kind))
        };
        if class == OpClass::Unsupported {
            self.block(Some(id.clone()), Some(entity.to_owned()), summary.clone());
        }
        self.ops.push(PlannedOp {
            id: id.clone(),
            step,
            class,
            ticked: class.ticked_by_default(),
            kind,
            entity: entity.to_owned(),
            action,
            summary,
            body,
            order: (step, sub_order, rank),
        });
        id
    }

    // --- every kind but tables --------------------------------------------------

    fn entity(
        &mut self,
        live: Option<&Entity>,
        target: Option<&Entity>,
        base: Option<&Entity>,
        module_tables: &BTreeMap<String, Vec<String>>,
    ) {
        match (live, target) {
            (None, Some(t)) => {
                self.push(
                    t.kind,
                    &t.key(),
                    "create",
                    None,
                    OpClass::Safe,
                    format!("create the {} `{}`", noun(t.kind), shown(t)),
                    OpBody::Put {
                        live: None,
                        target: Box::new(t.clone()),
                    },
                    0,
                );
            }
            (Some(l), Some(t)) => {
                let mut target = t.clone();
                keep_live_id(l, &mut target);
                keep_live_pin(l, &mut target, base);
                // The same — or differing only by an id, and the instance's is
                // kept (§3.2).
                if l.same_as(&target) {
                    return;
                }
                let summary = if l.name != t.name && t.kind.has_id() {
                    format!(
                        "rename the {} `{}` to `{}` and update it",
                        noun(t.kind),
                        shown(l),
                        t.name
                    )
                } else if t.kind == Kind::FileStore && pin(Some(l)) != pin(Some(&target)) {
                    format!(
                        "update the file store `{}` and check out commit {}",
                        shown(t),
                        short(pin(Some(&target)).unwrap_or("(none)"))
                    )
                } else {
                    format!("update the {} `{}`", noun(t.kind), shown(t))
                };
                self.push(
                    t.kind,
                    &l.key(),
                    "update",
                    None,
                    OpClass::Safe,
                    summary,
                    OpBody::Put {
                        live: Some(Box::new(l.clone())),
                        target: Box::new(target),
                    },
                    0,
                );
            }
            (Some(l), None) => {
                if l.kind == Kind::Role
                    && l.json()
                        .and_then(|v| v.get("builtin"))
                        .and_then(Json::as_bool)
                        == Some(true)
                {
                    // Every instance has the built-in roles.
                    return;
                }
                let (class, summary) = delete_class(l, module_tables);
                self.push(
                    l.kind,
                    &l.key(),
                    "delete",
                    None,
                    class,
                    summary,
                    OpBody::Delete {
                        live: Box::new(l.clone()),
                    },
                    0,
                );
            }
            (None, None) => {}
        }
    }

    // --- tables -------------------------------------------------------------------

    fn table(&mut self, live: Option<&Entity>, target: Option<&Entity>) {
        let module = |e: &Entity| {
            e.json()
                .and_then(|v| v.get("module"))
                .is_some_and(|m| !m.is_null())
        };
        let provided = |e: &Entity| {
            e.json()
                .and_then(|v| v.get("provider"))
                .is_some_and(|m| !m.is_null())
        };
        // A module's tables are its own: installing, upgrading or removing it
        // is what makes and changes them.
        if live.is_some_and(module) || target.is_some_and(module) {
            return;
        }
        if live.is_some_and(provided) || target.is_some_and(provided) {
            // A provided table is a definition the provider reads, saved whole.
            return match (live, target) {
                (Some(l), None) => {
                    self.push(
                        Kind::Table,
                        &l.key(),
                        "delete",
                        None,
                        OpClass::MetadataDelete,
                        format!(
                            "delete the provided table `{}` (its rows are its provider's)",
                            l.name
                        ),
                        OpBody::Delete {
                            live: Box::new(l.clone()),
                        },
                        0,
                    );
                }
                _ => self.entity(live, target, None, &BTreeMap::new()),
            };
        }
        match (live, target) {
            (None, Some(t)) => self.create_table(t),
            (Some(l), None) => {
                self.push(
                    Kind::Table,
                    &l.key(),
                    "drop_table",
                    None,
                    OpClass::DataDestructive,
                    format!("drop the table `{}` and every row in it", l.name),
                    schema(vec![Operation::DropTable {
                        table: l.name.clone(),
                    }]),
                    0,
                );
            }
            (Some(l), Some(t)) => self.alter_table(l, t),
            (None, None) => {}
        }
    }

    fn create_table(&mut self, t: &Entity) {
        let key = t.key();
        let Some(obj) = t.json().and_then(Json::as_object) else {
            return self.block(
                None,
                Some(key),
                format!("`{}` is not a JSON object", t.path),
            );
        };
        let name = t.name.clone();
        let settings = match table_settings_from_body(obj) {
            Ok(s) => s,
            Err(e) => return self.block(None, Some(key), format!("`{}`: {e}", t.path)),
        };
        let database = match obj.get("database").and_then(Json::as_str) {
            None | Some("primary") => String::new(),
            Some(db) => db.to_owned(),
        };
        let mut ids = GivenIds::default();
        if let Some(id) = uuid_of(obj) {
            ids.tables.insert(name.clone(), id);
        }
        let mut plain = Vec::new();
        let mut later = Vec::new();
        for field in fields_of(obj) {
            let spec = match field_spec_from_body(field) {
                Ok(spec) => spec,
                Err(e) => return self.block(None, Some(key), format!("`{}`: {e}", t.path)),
            };
            if let Some(id) = uuid_of(field) {
                ids.fields.insert((name.clone(), spec.name.clone()), id);
            }
            match spec.kind {
                DataFieldKind::Key { .. } => later.push((4, spec)),
                DataFieldKind::Calc { .. } => later.push((5, spec)),
                _ => plain.push(spec),
            }
        }
        self.push(
            Kind::Table,
            &key,
            "create_table",
            None,
            OpClass::Safe,
            format!("create the table `{name}`"),
            OpBody::Schema {
                batch: vec![Operation::CreateTable {
                    name: name.clone(),
                    database,
                    settings,
                    fields: plain,
                }],
                ids: ids.clone(),
                check: Vec::new(),
            },
            1,
        );
        // Keys after every table exists; calculated fields after the keys they
        // may read through.
        for (order, spec) in later {
            let field = spec.name.clone();
            self.push(
                Kind::Table,
                &key,
                "add_field",
                Some(&field),
                OpClass::Safe,
                format!("add the field `{name}.{field}`"),
                OpBody::Schema {
                    batch: vec![Operation::AddField {
                        table: name.clone(),
                        field: spec,
                    }],
                    ids: ids.clone(),
                    check: Vec::new(),
                },
                order,
            );
        }
        for c in managed_constraints(obj) {
            self.add_constraint(&key, &name, None, c);
        }
    }

    fn alter_table(&mut self, l: &Entity, t: &Entity) {
        if l.same_as(t) {
            return;
        }
        let key = l.key();
        let (Some(lv), Some(tv)) = (
            l.json().and_then(Json::as_object),
            t.json().and_then(Json::as_object),
        ) else {
            return;
        };
        let (live_name, name) = (l.name.clone(), t.name.clone());
        let database = |v: &Map<String, Json>| match v.get("database").and_then(Json::as_str) {
            None | Some("") => "primary".to_owned(),
            Some(db) => db.to_owned(),
        };
        if database(lv) != database(tv) {
            self.push(
                Kind::Table,
                &key,
                "move_table",
                None,
                OpClass::Unsupported,
                format!(
                    "the table `{live_name}` would move from the database `{}` to `{}`, which \
                     a deploy cannot do; create a new table there, copy the rows across, then \
                     drop this one",
                    database(lv),
                    database(tv)
                ),
                schema(Vec::new()),
                2,
            );
            return;
        }
        if live_name != name {
            self.push(
                Kind::Table,
                &key,
                "rename_table",
                None,
                OpClass::Safe,
                format!("rename the table `{live_name}` to `{name}`"),
                schema(vec![Operation::RenameTable {
                    table: live_name.clone(),
                    to: name.clone(),
                }]),
                0,
            );
        }
        const SETTINGS: [&str; 10] = [
            "label",
            "description",
            "min_role_read",
            "min_role_write",
            "ownership_formula",
            "rls_enabled",
            "sync_enabled",
            "history_enabled",
            "history_min_role",
            "history_keep_days",
        ];
        if SETTINGS.iter().any(|k| lv.get(*k) != tv.get(*k)) {
            match table_settings_from_body(tv) {
                Ok(settings) => {
                    self.push(
                        Kind::Table,
                        &key,
                        "alter_table",
                        None,
                        OpClass::Safe,
                        format!("change the settings of the table `{name}`"),
                        schema(vec![Operation::AlterTable {
                            table: name.clone(),
                            settings,
                        }]),
                        2,
                    );
                }
                Err(e) => self.block(None, Some(key.clone()), format!("`{}`: {e}", t.path)),
            }
        }
        self.fields(&key, &live_name, &name, lv, tv);
        self.constraints(&key, &live_name, &name, lv, tv);
    }

    fn users(&mut self, live: Option<&Entity>, target: Option<&Entity>) {
        let empty = Map::new();
        let obj = |e: Option<&Entity>| {
            e.and_then(Entity::json)
                .and_then(Json::as_object)
                .cloned()
                .unwrap_or_else(|| empty.clone())
        };
        let (lv, tv) = (obj(live), obj(target));
        if lv == tv {
            return;
        }
        let key = live
            .or(target)
            .map_or_else(|| "users:users".to_owned(), Entity::key);
        let users = sc_auth::USERS_TABLE;
        self.fields(&key, users, users, &lv, &tv);
    }

    /// The field operations turning live's fields into the target's.
    fn fields(
        &mut self,
        key: &str,
        live_table: &str,
        table: &str,
        lv: &Map<String, Json>,
        tv: &Map<String, Json>,
    ) {
        let (lf, tf) = (fields_of(lv), fields_of(tv));
        let pairs = match_fields(&lf, &tf);
        for (li, ti) in pairs {
            match (li.map(|i| lf[i]), ti.map(|i| tf[i])) {
                (None, Some(t)) => self.add_field(key, live_table, table, t),
                (Some(l), None) => {
                    let field = text(l, "name");
                    if flag(l, "primary_key") {
                        self.push(
                            Kind::Table,
                            key,
                            "drop_field",
                            Some(&field),
                            OpClass::Unsupported,
                            format!(
                                "`{table}.{field}` is part of the primary key, which a deploy \
                                 does not change; drop the table instead, or keep the field"
                            ),
                            schema(Vec::new()),
                            0,
                        );
                        continue;
                    }
                    self.push(
                        Kind::Table,
                        key,
                        "drop_field",
                        Some(&field),
                        OpClass::DataDestructive,
                        format!("drop the field `{table}.{field}` and the values in it"),
                        schema(vec![Operation::DropField {
                            table: table.to_owned(),
                            field: field.clone(),
                        }]),
                        0,
                    );
                }
                (Some(l), Some(t)) => self.alter_field(key, live_table, table, l, t),
                (None, None) => {}
            }
        }
    }

    fn add_field(&mut self, key: &str, live_table: &str, table: &str, t: &Map<String, Json>) {
        let spec = match field_spec_from_body(t) {
            Ok(spec) => spec,
            Err(e) => return self.block(None, Some(key.to_owned()), format!("`{table}`: {e}")),
        };
        let field = spec.name.clone();
        let mut ids = GivenIds::default();
        if let Some(id) = uuid_of(t) {
            ids.fields.insert((table.to_owned(), field.clone()), id);
        }
        let is_calc = matches!(spec.kind, DataFieldKind::Calc { .. });
        let order = match spec.kind {
            DataFieldKind::Key { .. } => 4,
            DataFieldKind::Calc { .. } => 5,
            _ => 3,
        };
        let can_fail = (spec.required && !is_calc) || spec.unique;
        let check = if can_fail {
            vec![Operation::AddField {
                table: live_table.to_owned(),
                field: spec.clone(),
            }]
        } else {
            Vec::new()
        };
        self.push(
            Kind::Table,
            key,
            "add_field",
            Some(&field),
            if can_fail {
                OpClass::CanFail
            } else {
                OpClass::Safe
            },
            format!("add the field `{table}.{field}`"),
            OpBody::Schema {
                batch: vec![Operation::AddField {
                    table: table.to_owned(),
                    field: spec,
                }],
                ids,
                check,
            },
            order,
        );
    }

    fn alter_field(
        &mut self,
        key: &str,
        live_table: &str,
        table: &str,
        l: &Map<String, Json>,
        t: &Map<String, Json>,
    ) {
        let strip = |v: &Map<String, Json>| {
            let mut v = v.clone();
            v.remove("id");
            v.remove("nullable");
            v
        };
        if strip(l) == strip(t) {
            return;
        }
        let (live_field, field) = (text(l, "name"), text(t, "name"));
        let kind_type = |v: &Map<String, Json>| kind_type(v).to_owned();
        let unsupported = unsupported_change(table, &field, l, t);
        if let Some(message) = unsupported {
            self.push(
                Kind::Table,
                key,
                "alter_field",
                Some(&live_field),
                OpClass::Unsupported,
                message,
                schema(Vec::new()),
                3,
            );
            return;
        }
        if live_field != field {
            self.push(
                Kind::Table,
                key,
                "rename_field",
                Some(&live_field),
                OpClass::Safe,
                format!("rename the field `{table}.{live_field}` to `{field}`"),
                schema(vec![Operation::AlterField {
                    table: table.to_owned(),
                    field: live_field.clone(),
                    settings: FieldSettings {
                        name: Some(field.clone()),
                        ..FieldSettings::default()
                    },
                }]),
                2,
            );
        }

        let mut settings = FieldSettings::default();
        let mut class = OpClass::Safe;
        let mut what: Vec<&str> = Vec::new();
        let differs = |k: &str| l.get(k) != t.get(k);
        if differs("label") {
            settings.label = Some(text(t, "label"));
            what.push("label");
        }
        if differs("description") {
            settings.description = Some(text(t, "description"));
            what.push("description");
        }
        if differs("type") {
            settings.type_name = Some(text(t, "type"));
            what.push("type");
        }
        if differs("attributes") {
            match sc_backup::codec::attributes_field(t) {
                Ok(attrs) => settings.attributes = Some(attrs),
                Err(e) => {
                    return self.block(
                        None,
                        Some(key.to_owned()),
                        format!("`{table}.{field}`: {e}"),
                    );
                }
            }
            what.push("attributes");
        }
        if differs("default_expression") {
            settings.default_expression = Some(text(t, "default_expression"));
            what.push("default");
        }
        if differs("kind") {
            match parse_field_kind(t) {
                Ok(kind) => {
                    if kind_type(l) == "key" || kind_type(t) == "key" {
                        // A key's target is the database's foreign key.
                        class = OpClass::CanFail;
                    }
                    settings.kind = Some(kind);
                }
                Err(e) => {
                    return self.block(
                        None,
                        Some(key.to_owned()),
                        format!("`{table}.{field}`: {e}"),
                    );
                }
            }
            what.push("kind");
        }
        if flag(l, "required") != flag(t, "required") {
            settings.required = Some(flag(t, "required"));
            if flag(t, "required") {
                class = OpClass::CanFail;
                what.push("now required");
            } else {
                what.push("now optional");
            }
        }
        if flag(l, "unique") != flag(t, "unique") {
            settings.unique = Some(flag(t, "unique"));
            if flag(t, "unique") {
                class = OpClass::CanFail;
                what.push("now unique");
            } else {
                what.push("no longer unique");
            }
        }
        if what.is_empty() {
            return;
        }
        let check = if class == OpClass::CanFail {
            vec![Operation::AlterField {
                table: live_table.to_owned(),
                field: live_field.clone(),
                settings: settings.clone(),
            }]
        } else {
            Vec::new()
        };
        self.push(
            Kind::Table,
            key,
            "alter_field",
            Some(&live_field),
            class,
            format!("change the field `{table}.{field}` ({})", what.join(", ")),
            OpBody::Schema {
                batch: vec![Operation::AlterField {
                    table: table.to_owned(),
                    field: field.clone(),
                    settings,
                }],
                ids: GivenIds::default(),
                check,
            },
            3,
        );
    }

    fn constraints(
        &mut self,
        key: &str,
        live_table: &str,
        table: &str,
        lv: &Map<String, Json>,
        tv: &Map<String, Json>,
    ) {
        let by_name = |v: &Map<String, Json>| -> BTreeMap<String, Map<String, Json>> {
            managed_constraints(v)
                .into_iter()
                .map(|c| (text(&c, "name"), c))
                .collect()
        };
        let (lc, tc) = (by_name(lv), by_name(tv));
        let names: BTreeSet<&String> = lc.keys().chain(tc.keys()).collect();
        for name in names {
            match (lc.get(name), tc.get(name)) {
                (None, Some(t)) => self.add_constraint(key, table, Some(live_table), t.clone()),
                (Some(_), None) => {
                    self.push(
                        Kind::Table,
                        key,
                        "drop_constraint",
                        Some(name),
                        OpClass::Safe,
                        format!("drop the constraint `{name}` on `{table}`"),
                        schema(vec![Operation::DropConstraint {
                            table: table.to_owned(),
                            name: name.clone(),
                        }]),
                        6,
                    );
                }
                (Some(l), Some(t)) if l != t => match constraint_from_body(t) {
                    Ok(constraint) => {
                        let drop = |table: &str| Operation::DropConstraint {
                            table: table.to_owned(),
                            name: name.clone(),
                        };
                        let add = |table: &str| Operation::AddConstraint {
                            table: table.to_owned(),
                            given_name: String::new(),
                            constraint: constraint.clone(),
                        };
                        self.push(
                            Kind::Table,
                            key,
                            "replace_constraint",
                            Some(name),
                            OpClass::CanFail,
                            format!("change the constraint `{name}` on `{table}`"),
                            OpBody::Schema {
                                batch: vec![drop(table), add(table)],
                                ids: GivenIds::default(),
                                check: vec![drop(live_table), add(live_table)],
                            },
                            7,
                        );
                    }
                    Err(e) => self.block(None, Some(key.to_owned()), format!("`{table}`: {e}")),
                },
                _ => {}
            }
        }
    }

    /// Add a constraint; `live_table` is the table's live name when it exists
    /// already, so the dry run can check the rows there.
    fn add_constraint(
        &mut self,
        key: &str,
        table: &str,
        live_table: Option<&str>,
        c: Map<String, Json>,
    ) {
        let name = text(&c, "name");
        let constraint = match constraint_from_body(&c) {
            Ok(constraint) => constraint,
            Err(e) => return self.block(None, Some(key.to_owned()), format!("`{table}`: {e}")),
        };
        let can_fail =
            live_table.is_some() && matches!(text(&c, "type").as_str(), "unique" | "formula");
        let add = |table: &str| Operation::AddConstraint {
            table: table.to_owned(),
            given_name: String::new(),
            constraint: constraint.clone(),
        };
        self.push(
            Kind::Table,
            key,
            "add_constraint",
            Some(&name),
            if can_fail {
                OpClass::CanFail
            } else {
                OpClass::Safe
            },
            format!("add the constraint `{name}` on `{table}`"),
            OpBody::Schema {
                batch: vec![add(table)],
                ids: GivenIds::default(),
                check: match (can_fail, live_table) {
                    (true, Some(live)) => vec![add(live)],
                    _ => Vec::new(),
                },
            },
            7,
        );
    }

    // --- what a plan depends on ------------------------------------------------------

    /// Entities created with a secret this instance has not got, and the tables
    /// that cannot work without one (§3.3).
    fn secrets(&mut self, live: &Entities, target: &Entities, groups: &[crate::diff::Group]) {
        for op in &self.ops {
            if let OpBody::Put {
                live: None,
                target: t,
            } = &op.body
                && t.json().is_some_and(holds_sentinel)
            {
                self.needs_secrets.push(t.key());
            }
        }
        // A connection whose password the target has (as the sentinel) and
        // this instance does not.
        let mut missing: BTreeSet<String> = BTreeSet::new();
        for g in groups.iter().filter(|g| g.kind == Kind::DbConnection) {
            let Some(t) = g.members[1].map(|i| &target.list[i]) else {
                continue;
            };
            let password = |e: &Entity| {
                e.json()
                    .and_then(|v| v.get("password"))
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let here = g.members[0].map(|i| password(&live.list[i]));
            if password(t) == SECRET_SENTINEL && here.is_none_or(|p| p.is_empty()) {
                missing.insert(t.name.clone());
            }
        }
        if missing.is_empty() {
            return;
        }
        let touched: BTreeSet<&str> = self
            .ops
            .iter()
            .filter(|op| op.kind == Kind::Table)
            .map(|op| op.entity.as_str())
            .collect();
        let mut found = Vec::new();
        for g in groups.iter().filter(|g| g.kind == Kind::Table) {
            let Some(t) = g.members[1].map(|i| &target.list[i]) else {
                continue;
            };
            let key = g.members[0].map_or_else(|| t.key(), |i| live.list[i].key());
            let db = t
                .json()
                .and_then(|v| v.get("database"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            if missing.contains(db) && touched.contains(key.as_str()) {
                found.push((key, t.name.clone(), db.to_owned()));
            }
        }
        for (key, table, db) in found {
            self.block(
                None,
                Some(key),
                format!(
                    "the table `{table}` is on the database connection `{db}`, which has no \
                     password on this instance yet; set it under Database connections, then \
                     deploy again"
                ),
            );
        }
    }

    /// New git stores this instance cannot clone, and the applications built
    /// from one (§3.5).
    fn git_clones(&mut self, target: &Entities, groups: &[crate::diff::Group]) {
        let mut uncloneable = BTreeSet::new();
        for g in groups.iter().filter(|g| g.kind == Kind::FileStore) {
            if g.members[0].is_some() {
                continue;
            }
            let Some(t) = g.members[1].map(|i| &target.list[i]) else {
                continue;
            };
            if !is_git_store(t) {
                continue;
            }
            let config = t.json().and_then(|v| v.get("config"));
            let setting = |k: &str| {
                config
                    .and_then(|c| c.get(k))
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            };
            if !setting(CFG_PUBLIC_URL).is_empty() || anonymous(&setting(CFG_URL)) {
                continue;
            }
            self.needs_credentials.push(t.key());
            uncloneable.insert(t.name.clone());
        }
        if uncloneable.is_empty() {
            return;
        }
        for app in target.list.iter().filter(|e| e.kind == Kind::Application) {
            let Some(v) = app.json() else { continue };
            let frameworks = std::iter::once(v.get("framework")).chain(
                v.get("extra_frameworks")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .map(Some),
            );
            for framework in frameworks.flatten() {
                let store = framework
                    .get("config")
                    .and_then(|c| c.get("store"))
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                if uncloneable.contains(store) {
                    self.block(
                        None,
                        Some(app.key()),
                        format!(
                            "the application `{}` is built from the git store `{store}`, which \
                             this instance cannot clone: it has no public URL, and its URL \
                             needs a key this instance does not have. Give the store a \
                             public URL, or create it here with a deploy key first",
                            app.name
                        ),
                    );
                }
            }
        }
    }
}

/// Whether a clone of `url` needs no credentials: an `https://` (or plain
/// HTTP) URL, one of the well-known hosts' SSH URLs (cloned over HTTPS
/// instead), or a local repository.
fn anonymous(url: &str) -> bool {
    url.starts_with("http://")
        || url.starts_with("file://")
        || url.starts_with('/')
        || derive_https_url(url).is_some()
}

/// The class and words of deleting `l`.
fn delete_class(l: &Entity, module_tables: &BTreeMap<String, Vec<String>>) -> (OpClass, String) {
    let name = shown(l);
    match l.kind {
        Kind::DbConnection => (
            OpClass::DataDestructive,
            format!("delete the database connection `{name}`"),
        ),
        Kind::FileStore => (
            OpClass::DataDestructive,
            format!("delete the file store `{name}` (its files are not deleted)"),
        ),
        Kind::Model => (
            OpClass::DataDestructive,
            format!("delete the model `{name}` and its fits"),
        ),
        Kind::Module => match module_tables.get(&l.name) {
            Some(tables) => (
                OpClass::DataDestructive,
                format!(
                    "remove the module `{name}`, which provides the table{} {}",
                    if tables.len() == 1 { "" } else { "s" },
                    tables
                        .iter()
                        .map(|t| format!("`{t}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            None => (
                OpClass::MetadataDelete,
                format!("remove the module `{name}`"),
            ),
        },
        Kind::Setting => (
            OpClass::Safe,
            format!("reset the setting `{name}` to its default"),
        ),
        kind => (
            OpClass::MetadataDelete,
            format!("delete the {} `{name}`", noun(kind)),
        ),
    }
}

/// A name match with another id keeps the instance's (§3.2).
fn keep_live_id(live: &Entity, target: &mut Entity) {
    if !target.kind.has_id() || live.id == target.id {
        return;
    }
    if let (Some(id), crate::entity::Content::Json { value }) = (&live.id, &mut target.content)
        && let Json::Object(map) = value
    {
        map.insert("id".to_owned(), Json::String(id.clone()));
        target.id = Some(id.clone());
    }
}

/// A git store whose pin is the same on the target as in the base keeps live's
/// pin: the pin moved here, and a deploy does not undo that (§3.5).
fn keep_live_pin(live: &Entity, target: &mut Entity, base: Option<&Entity>) {
    if !is_git_store(target) || base.is_none() || pin(base) != pin(Some(target)) {
        return;
    }
    let live_pin = pin(Some(live)).map(str::to_owned);
    if let crate::entity::Content::Json { value } = &mut target.content
        && let Json::Object(map) = value
    {
        map.insert(
            "commit".to_owned(),
            live_pin.map_or(Json::Null, Json::String),
        );
    }
}

fn holds_sentinel(value: &Json) -> bool {
    match value {
        Json::String(s) => s == SECRET_SENTINEL,
        Json::Array(items) => items.iter().any(holds_sentinel),
        Json::Object(map) => map.values().any(holds_sentinel),
        _ => false,
    }
}

fn schema(batch: Vec<Operation>) -> OpBody {
    OpBody::Schema {
        batch,
        ids: GivenIds::default(),
        check: Vec::new(),
    }
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn fields_of(v: &Map<String, Json>) -> Vec<&Map<String, Json>> {
    v.get("fields")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_object)
        .collect()
}

/// The constraints a table's JSON lists that Saltcorn made and so a deploy
/// makes: the others (a unique field's own, a key's) follow from the fields.
fn managed_constraints(v: &Map<String, Json>) -> Vec<Map<String, Json>> {
    v.get("constraints")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_object)
        .filter(|c| c.get("managed").and_then(Json::as_bool) == Some(true))
        .cloned()
        .collect()
}

fn uuid_of(v: &Map<String, Json>) -> Option<Uuid> {
    v.get("id")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

fn text(v: &Map<String, Json>, key: &str) -> String {
    match v.get(key) {
        Some(Json::String(s)) => s.clone(),
        None | Some(Json::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

fn flag(v: &Map<String, Json>, key: &str) -> bool {
    v.get(key).and_then(Json::as_bool).unwrap_or(false)
}

/// A field's kind, as its JSON names it.
fn kind_type(v: &Map<String, Json>) -> &str {
    v.get("kind")
        .and_then(|k| k.get("type"))
        .and_then(Json::as_str)
        .unwrap_or("plain")
}

/// What a deploy cannot do to a column, in words that say what to do instead.
fn unsupported_change(
    table: &str,
    field: &str,
    l: &Map<String, Json>,
    t: &Map<String, Json>,
) -> Option<String> {
    let calc = |v: &Map<String, Json>| kind_type(v) == "calc";
    if calc(l) != calc(t) {
        Some(format!(
            "`{table}.{field}` would change between a calculated field and a stored one, \
             which a deploy cannot do; add a new field, then drop the old one"
        ))
    } else if !calc(l) && l.get("sql_type") != t.get("sql_type") {
        Some(format!(
            "`{table}.{field}` would change its SQL type from `{}` to `{}`, which a \
             deploy cannot do; add a new field, copy the values across, then drop the \
             old one",
            text(l, "sql_type"),
            text(t, "sql_type")
        ))
    } else if flag(l, "primary_key") != flag(t, "primary_key") {
        Some(format!(
            "`{table}.{field}` would {} the primary key, which a deploy does not \
             change; change the table's key on each instance by hand",
            if flag(t, "primary_key") {
                "join"
            } else {
                "leave"
            }
        ))
    } else if flag(l, "generated") != flag(t, "generated") {
        Some(format!(
            "`{table}.{field}` would change whether the database generates its values, \
             which a deploy cannot do"
        ))
    } else {
        None
    }
}

/// Pair live's fields with the target's, by id and then by name.
fn match_fields(
    live: &[&Map<String, Json>],
    target: &[&Map<String, Json>],
) -> Vec<(Option<usize>, Option<usize>)> {
    let id = |f: &Map<String, Json>| f.get("id").and_then(Json::as_str).map(str::to_owned);
    let mut paired: Vec<Option<usize>> = vec![None; target.len()];
    let mut taken = vec![false; live.len()];
    for (ti, t) in target.iter().enumerate() {
        if let Some(tid) = id(t)
            && let Some(li) =
                (0..live.len()).find(|&li| !taken[li] && id(live[li]).as_ref() == Some(&tid))
        {
            paired[ti] = Some(li);
            taken[li] = true;
        }
    }
    for (ti, t) in target.iter().enumerate() {
        if paired[ti].is_some() {
            continue;
        }
        if let Some(li) =
            (0..live.len()).find(|&li| !taken[li] && text(live[li], "name") == text(t, "name"))
        {
            paired[ti] = Some(li);
            taken[li] = true;
        }
    }
    let mut out: Vec<(Option<usize>, Option<usize>)> = paired
        .iter()
        .enumerate()
        .map(|(ti, li)| (*li, Some(ti)))
        .collect();
    out.extend(
        (0..live.len())
            .filter(|&li| !taken[li])
            .map(|li| (Some(li), None)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T: &str = "6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f01";
    const F_ID: &str = "6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f10";
    const F_TITLE: &str = "6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f11";
    const F_STATUS: &str = "6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f12";

    fn field(id: &str, name: &str) -> Json {
        json!({
            "attributes": {}, "default_expression": "", "description": "",
            "generated": false, "id": id, "kind": { "type": "plain" },
            "label": name, "name": name, "nullable": true, "primary_key": false,
            "required": false, "sql_type": "text", "type": "string", "unique": false,
        })
    }

    fn key_field() -> Json {
        let mut f = field(F_ID, "id");
        f["generated"] = json!(true);
        f["primary_key"] = json!(true);
        f["required"] = json!(true);
        f["nullable"] = json!(false);
        f["sql_type"] = json!("int8");
        f["type"] = json!("integer");
        f
    }

    /// The `tickets` table as an export writes it, with these fields after
    /// its key.
    fn tickets(name: &str, fields: Vec<Json>) -> Json {
        let mut all = vec![key_field()];
        all.extend(fields);
        json!({
            "constraints": [], "database": "primary", "description": "",
            "fields": all, "history_enabled": false, "history_keep_days": null,
            "history_min_role": 1, "id": T, "label": name, "min_role_read": 1,
            "min_role_write": 1, "module": null, "name": name, "ownership_formula": "",
            "provider": null, "rls_enabled": false, "sync_enabled": false,
        })
    }

    fn with_table(table: Json) -> StructureTree {
        let mut tree = StructureTree::new();
        let name = table["name"].as_str().unwrap().to_owned();
        tree.insert_json(format!("tables/{name}.json"), table);
        tree
    }

    fn base_tickets() -> StructureTree {
        with_table(tickets(
            "tickets",
            vec![field(F_TITLE, "title"), field(F_STATUS, "status")],
        ))
    }

    fn actions(plan: &Plan) -> Vec<(&str, OpClass, bool)> {
        plan.operations
            .iter()
            .map(|op| (op.action, op.class, op.ticked))
            .collect()
    }

    #[test]
    fn nothing_to_do_is_an_empty_plan() {
        let tree = base_tickets();
        let plan = plan(&tree, &tree, None);
        assert!(plan.is_empty(), "{:#?}", plan.operations);
        assert!(plan.blockers.is_empty());
    }

    #[test]
    fn a_new_table_is_created_with_its_ids() {
        let plan = plan(&StructureTree::new(), &base_tickets(), None);
        assert_eq!(actions(&plan), [("create_table", OpClass::Safe, true)]);
        let OpBody::Schema { batch, ids, .. } = &plan.operations[0].body else {
            panic!("{:?}", plan.operations[0].body);
        };
        let Operation::CreateTable { name, fields, .. } = &batch[0] else {
            panic!("{batch:?}");
        };
        assert_eq!(name, "tickets");
        assert_eq!(fields.len(), 3);
        assert!(fields[0].primary_key);
        assert_eq!(ids.tables["tickets"].to_string(), T);
        assert_eq!(
            ids.fields[&("tickets".to_owned(), "status".to_owned())].to_string(),
            F_STATUS
        );
    }

    #[test]
    fn a_field_renamed_by_id_is_a_rename_not_a_drop_and_a_create() {
        let mut state = field(F_STATUS, "state");
        state["label"] = json!("status");
        let target = with_table(tickets("tickets", vec![field(F_TITLE, "title"), state]));
        let plan = plan(&base_tickets(), &target, None);
        assert_eq!(actions(&plan), [("rename_field", OpClass::Safe, true)]);
        assert_eq!(plan.operations[0].id, "rename_field:table:tickets#status");
        let OpBody::Schema { batch, .. } = &plan.operations[0].body else {
            panic!();
        };
        assert!(matches!(
            &batch[0],
            Operation::AlterField { table, field, settings }
                if table == "tickets" && field == "status"
                    && settings.name.as_deref() == Some("state")
        ));
    }

    #[test]
    fn a_table_renamed_by_id_is_renamed_first_and_alone() {
        let target = with_table(tickets(
            "issues",
            vec![field(F_TITLE, "title"), field(F_STATUS, "status")],
        ));
        let plan = plan(&base_tickets(), &target, None);
        // Its label followed its name in the fixture: a settings change, after.
        assert_eq!(
            actions(&plan),
            [
                ("rename_table", OpClass::Safe, true),
                ("alter_table", OpClass::Safe, true),
            ]
        );
        let OpBody::Schema { batch, .. } = &plan.operations[1].body else {
            panic!();
        };
        assert!(matches!(&batch[0], Operation::AlterTable { table, .. } if table == "issues"));
    }

    #[test]
    fn dropping_a_table_or_a_field_is_data_destructive_and_unticked() {
        let target = with_table(tickets("tickets", vec![field(F_TITLE, "title")]));
        let plan = plan(&base_tickets(), &target, None);
        assert_eq!(
            actions(&plan),
            [("drop_field", OpClass::DataDestructive, false)]
        );
        assert_eq!(plan.operations[0].step, Step::Deletes);

        let plan = super::plan(&base_tickets(), &StructureTree::new(), None);
        assert_eq!(
            actions(&plan),
            [("drop_table", OpClass::DataDestructive, false)]
        );
        assert!(plan.blockers.is_empty(), "deferring a drop is allowed");
    }

    #[test]
    fn making_a_field_required_can_fail_and_is_checked_against_live_names() {
        let mut status = field(F_STATUS, "state");
        status["required"] = json!(true);
        status["nullable"] = json!(false);
        let target = with_table(tickets("tickets", vec![field(F_TITLE, "title"), status]));
        let plan = plan(&base_tickets(), &target, None);
        assert_eq!(
            actions(&plan),
            [
                ("rename_field", OpClass::Safe, true),
                ("alter_field", OpClass::CanFail, true),
            ]
        );
        let OpBody::Schema { batch, check, .. } = &plan.operations[1].body else {
            panic!();
        };
        // Applied after the rename, checked before it.
        assert!(matches!(&batch[0], Operation::AlterField { field, .. } if field == "state"));
        assert!(
            matches!(&check[0], Operation::AlterField { field, settings, .. }
            if field == "status" && settings.required == Some(true))
        );
    }

    #[test]
    fn an_sql_type_change_is_unsupported_and_blocks() {
        let mut status = field(F_STATUS, "status");
        status["sql_type"] = json!("int8");
        status["type"] = json!("integer");
        let target = with_table(tickets("tickets", vec![field(F_TITLE, "title"), status]));
        let plan = plan(&base_tickets(), &target, None);
        assert_eq!(
            actions(&plan),
            [("alter_field", OpClass::Unsupported, false)]
        );
        assert_eq!(plan.blockers.len(), 1);
        let message = &plan.blockers[0].message;
        assert!(message.contains("SQL type"), "{message}");
        assert!(
            message.contains("add a new field, copy the values across, then drop the old one"),
            "{message}"
        );
    }

    fn app_tree(views: &[&str]) -> StructureTree {
        let mut tree = StructureTree::new();
        tree.insert_json(
            "applications/desk/application.json",
            json!({ "id": "a1", "name": "desk", "subdomain": "desk" }),
        );
        for (i, v) in views.iter().enumerate() {
            tree.insert_json(
                format!("applications/desk/views/{v}.json"),
                json!({ "id": format!("v{i}"), "name": v }),
            );
        }
        tree
    }

    #[test]
    fn deleting_a_view_is_a_ticked_metadata_delete() {
        let plan = plan(&app_tree(&["list", "edit"]), &app_tree(&["list"]), None);
        assert_eq!(actions(&plan), [("delete", OpClass::MetadataDelete, true)]);
        assert_eq!(plan.operations[0].entity, "view:desk/edit");
        assert_eq!(plan.operations[0].step, Step::Deletes);
    }

    #[test]
    fn deleting_a_store_a_connection_or_a_module_with_tables_is_data_destructive() {
        let mut live = StructureTree::new();
        live.insert_json(
            "file-stores/assets.json",
            json!({ "id": "s1", "name": "assets", "backend": "local", "config": {} }),
        );
        live.insert_json(
            "db-connections/reporting.json",
            json!({ "id": "c1", "name": "reporting", "password": "" }),
        );
        live.insert_json(
            "modules.json",
            json!([{ "name": "crm", "version": "1.0.0" }, { "name": "theme", "version": "1" }]),
        );
        let mut provided = tickets("contacts", Vec::new());
        provided["module"] = json!("crm");
        live.insert_json("tables/contacts.json", provided);
        let plan = plan(&live, &StructureTree::new(), None);
        let by_entity: BTreeMap<&str, OpClass> = plan
            .operations
            .iter()
            .map(|op| (op.entity.as_str(), op.class))
            .collect();
        assert_eq!(by_entity["file_store:assets"], OpClass::DataDestructive);
        assert_eq!(
            by_entity["db_connection:reporting"],
            OpClass::DataDestructive
        );
        assert_eq!(by_entity["module:crm"], OpClass::DataDestructive);
        assert_eq!(by_entity["module:theme"], OpClass::MetadataDelete);
        // The module's table is the module's to drop.
        assert!(!by_entity.contains_key("table:contacts"));
    }

    #[test]
    fn operations_run_in_deploy_order_and_deletes_in_reverse() {
        let mut target = app_tree(&["list"]);
        target.insert_json("modules.json", json!([{ "name": "crm", "version": "1" }]));
        target.insert_json("roles.json", json!([{ "role": 40, "name": "staff" }]));
        target.insert_json(
            "settings/localisation.json",
            json!({ "default_locale": "fr" }),
        );
        target.insert_json(
            "triggers/notify.json",
            json!({ "id": "t1", "name": "notify" }),
        );
        // `tickets` is new; `old_notes` and the application `old` are gone.
        target.insert_json(
            "tables/tickets.json",
            base_tickets().json("tables/tickets.json").unwrap().clone(),
        );
        let mut live = StructureTree::new();
        live.insert_json(
            "applications/old/application.json",
            json!({ "id": "a2", "name": "old", "subdomain": "old" }),
        );
        live.insert_json(
            "applications/old/views/v.json",
            json!({ "id": "v9", "name": "v" }),
        );
        live.insert_json("tables/old_notes.json", {
            let mut t = tickets("old_notes", Vec::new());
            t["id"] = json!("6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f99");
            for f in t["fields"].as_array_mut().unwrap() {
                f["id"] = json!("6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f98");
            }
            t
        });
        let plan = plan(&live, &target, None);
        let order: Vec<&str> = plan
            .operations
            .iter()
            .map(|op| op.entity.as_str())
            .collect();
        assert_eq!(
            order,
            [
                "module:crm",
                "role:40",
                "table:tickets",
                "trigger:notify",
                "application:desk",
                "view:desk/list",
                "setting:localisation.default_locale",
                // Deletes: the view before its application, both before the
                // table.
                "view:old/v",
                "application:old",
                "table:old_notes",
            ]
        );
    }

    #[test]
    fn the_digest_changes_with_live_or_the_target_and_not_otherwise() {
        let live = base_tickets();
        let target = app_tree(&["list"]);
        let first = plan(&live, &target, None).digest;
        assert_eq!(plan(&live, &target, None).digest, first);
        assert_eq!(plan(&live.clone(), &target.clone(), None).digest, first);

        let mut edited = live.clone();
        edited.insert_json(
            "tables/tickets.json",
            tickets("tickets", vec![field(F_TITLE, "title")]),
        );
        assert_ne!(plan(&edited, &target, None).digest, first);

        let moved = app_tree(&["list", "edit"]);
        assert_ne!(plan(&live, &moved, None).digest, first);
    }

    #[test]
    fn a_name_match_with_another_id_keeps_the_instances_id() {
        let mut live = StructureTree::new();
        live.insert_json(
            "triggers/notify.json",
            json!({ "id": "live-id", "name": "notify", "when": "insert" }),
        );
        let mut target = StructureTree::new();
        target.insert_json(
            "triggers/notify.json",
            json!({ "id": "repo-id", "name": "notify", "when": "update" }),
        );
        let plan = plan(&live, &target, None);
        let OpBody::Put { target, .. } = &plan.operations[0].body else {
            panic!();
        };
        assert_eq!(target.json().unwrap()["id"], "live-id");
        assert_eq!(target.json().unwrap()["when"], "update");

        // Differing only by id is nothing to do.
        let mut same = StructureTree::new();
        same.insert_json(
            "triggers/notify.json",
            json!({ "id": "repo-id", "name": "notify", "when": "insert" }),
        );
        assert!(super::plan(&live, &same, None).is_empty());
    }

    fn git_store(commit: &str, branch: &str) -> StructureTree {
        let mut tree = StructureTree::new();
        tree.insert_json(
            "file-stores/web.json",
            json!({
                "id": "s1", "name": "web", "backend": "git",
                "config": { "url": "git@example.org:o/web.git", "branch": branch },
                "commit": commit,
            }),
        );
        tree
    }

    #[test]
    fn a_pin_moved_here_is_kept_and_an_incoming_pin_is_applied() {
        let base = git_store("aaa", "main");
        let live = git_store("bbb", "main");
        // The tip has the base's pin: nothing to do for the store.
        assert!(plan(&live, &base, Some(&base)).is_empty());
        // The tip changed the branch: the update keeps live's pin.
        let plan_ = plan(&live, &git_store("aaa", "dev"), Some(&base));
        let OpBody::Put { target, .. } = &plan_.operations[0].body else {
            panic!();
        };
        assert_eq!(target.json().unwrap()["commit"], "bbb");
        // The tip moved the pin: it is applied.
        let plan_ = plan(&live, &git_store("ccc", "main"), Some(&base));
        let OpBody::Put { target, .. } = &plan_.operations[0].body else {
            panic!();
        };
        assert_eq!(target.json().unwrap()["commit"], "ccc");
    }

    #[test]
    fn a_table_on_a_connection_without_a_password_blocks() {
        let mut target = StructureTree::new();
        target.insert_json(
            "db-connections/reporting.json",
            json!({ "id": "c1", "name": "reporting", "password": SECRET_SENTINEL }),
        );
        let mut report = tickets("report", Vec::new());
        report["database"] = json!("reporting");
        target.insert_json("tables/report.json", report);
        let plan = plan(&StructureTree::new(), &target, None);
        assert_eq!(plan.needs_secrets, ["db_connection:reporting"]);
        assert_eq!(plan.blockers.len(), 1, "{:#?}", plan.blockers);
        assert!(plan.blockers[0].message.contains("`reporting`"));

        // Without a table on it, the new connection is only "needs a secret".
        target.remove("tables/report.json");
        let plan = super::plan(&StructureTree::new(), &target, None);
        assert!(plan.blockers.is_empty());
        assert_eq!(plan.needs_secrets, ["db_connection:reporting"]);
    }

    #[test]
    fn an_application_built_from_an_uncloneable_new_git_store_blocks() {
        let mut target = git_store("aaa", "main");
        target.insert_json(
            "applications/web/application.json",
            json!({
                "id": "a1", "subdomain": "web",
                "framework": { "name": "code", "config": { "store": "web" } },
            }),
        );
        let plan = plan(&StructureTree::new(), &target, None);
        assert_eq!(plan.needs_credentials, ["file_store:web"]);
        assert_eq!(plan.blockers.len(), 1);
        assert!(plan.blockers[0].message.contains("`web`"));

        // A well-known host's SSH URL is cloned over HTTPS instead.
        let mut github = target.clone();
        let mut store = github.json("file-stores/web.json").unwrap().clone();
        store["config"]["url"] = json!("git@github.com:o/web.git");
        github.insert_json("file-stores/web.json", store);
        assert!(
            super::plan(&StructureTree::new(), &github, None)
                .blockers
                .is_empty()
        );
    }

    #[test]
    fn a_newer_format_blocks() {
        let mut target = StructureTree::new();
        target.insert_json(
            crate::format::FORMAT_FILE,
            json!({ "format": crate::format::FORMAT_VERSION + 1, "writer": "9.9.9" }),
        );
        let plan = plan(&StructureTree::new(), &target, None);
        assert_eq!(plan.blockers.len(), 1);
        assert!(
            plan.blockers[0].message.contains("9.9.9"),
            "{:?}",
            plan.blockers
        );
    }
}
