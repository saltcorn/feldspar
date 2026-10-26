//! Long-term memory: the `_fd_agent_memory` store, the `memory` trait and the
//! `consolidate_memory` action (TODO-agent-skills AS8.3–AS8.5).
//!
//! v1 keeps an agent's memories in an `AgentLongTermMemory` table the skill
//! creates in the application's own schema. Here they are a **system table**
//! (§3's open question, decided as proposed): it is not part of anybody's
//! application, a backup of the application does not carry somebody's chat
//! preferences into another instance, and the admin reads and deletes them on
//! a screen of their own rather than through the table editor.
//!
//! ## Who may read a memory
//!
//! A memory is **personal** or **shared**. A shared memory is something the
//! agent learned that anybody it talks to may be told ("the office is closed
//! on the 24th"). A personal one is something one person said about
//! themselves, and the whole design hangs on one rule: **a personal memory is
//! only ever read back for its own user**. The rule is in one place —
//! [`MemoryReader::visibility`] — and every read in this module goes through
//! it; a reader who is nobody in particular (a triggered run, an anonymous
//! visitor) sees shared memories only, and nothing personal is ever stored for
//! a run that has no user to file it under.
//!
//! ## Search
//!
//! The table has a full-text constraint (AS1), created with it, so
//! `search_memory` is a ranked full-text search on both backends — an index on
//! Postgres, an FTS5 table on SQLite — over the memory's topic and contents.
//!
//! ## Preferences
//!
//! `consolidate_memory` (an action, meant for a scheduled trigger) has the
//! agent's cheap role read one user's personal memories and write a single
//! `preferences` memory summarising them, which the trait puts in the session
//! header of that user's next run.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sc_action::{Action, ActionContext, ConfigCheck};
use sc_agent::{
    AgentRegistry, AgentTrait, ModelRole, ProviderConnector, SessionContext, ToolsContext,
    TraitCheck, TraitContext,
};
use sc_catalog::{
    Catalog, ConstraintKind, DataField, SchemaProjection, Table, TableConstraint,
    create_constraint_steps, full_text_columns, full_text_constraint, indexed_text_search,
};
use sc_db::{ColumnGenerator, Row};
use sc_error::{Error, Result};
use sc_llm::{LlmMessage, LlmRequest, ToolSpec};
use sc_query::{BinOp, Delete, Expr, Insert, OrderBy, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

// --- the store ----------------------------------------------------------------

/// The memory table in the primary database.
pub const MEMORY_TABLE: &str = "_fd_agent_memory";

/// The integer key — an integer because SQLite's full-text index is keyed by it.
pub const COL_MEMORY_ID: &str = "id";
/// The name of the agent that wrote it, and the only agent that reads it.
pub const COL_MEMORY_AGENT: &str = "agent";
/// The user it is about (personal) or who said it (shared); NULL for a run
/// with no user, which only ever writes shared memories.
pub const COL_MEMORY_USER: &str = "user_id";
/// The run that wrote it, NULL for one the consolidation wrote.
pub const COL_MEMORY_RUN: &str = "run_id";
/// When it was written.
pub const COL_MEMORY_WRITTEN_AT: &str = "written_at";
/// [`KIND_MEMORY`] or [`KIND_PREFERENCES`].
pub const COL_MEMORY_KIND: &str = "kind";
/// Whether only its own user may read it back.
pub const COL_MEMORY_PERSONAL: &str = "personal";
/// A few words saying what it is about, for the search and the admin's list.
pub const COL_MEMORY_TOPIC: &str = "topic";
/// What is remembered.
pub const COL_MEMORY_CONTENTS: &str = "contents";

/// A memory the model stored.
pub const KIND_MEMORY: &str = "memory";
/// The one consolidated summary of a user's personal memories.
pub const KIND_PREFERENCES: &str = "preferences";

/// The language the full-text constraint indexes in.
const MEMORY_LANGUAGE: &str = "english";

fn memory_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    vec![
        DataField::plain(COL_MEMORY_ID, TypeRef::Basic(BasicType::Int))
            .required()
            .primary_key()
            .generated(ColumnGenerator::Identity),
        DataField::plain(COL_MEMORY_AGENT, text()).required(),
        DataField::plain(COL_MEMORY_USER, TypeRef::Basic(BasicType::Uuid)),
        DataField::plain(COL_MEMORY_RUN, TypeRef::Basic(BasicType::Uuid)),
        DataField::plain(COL_MEMORY_WRITTEN_AT, TypeRef::Basic(BasicType::Timestamp)).required(),
        DataField::plain(COL_MEMORY_KIND, text()).required(),
        DataField::plain(COL_MEMORY_PERSONAL, TypeRef::Basic(BasicType::Bool)).required(),
        DataField::plain(COL_MEMORY_TOPIC, text()),
        DataField::plain(COL_MEMORY_CONTENTS, text()).required(),
    ]
}

/// Ensure the memory table and its full-text constraint exist, and return the
/// table. Idempotent, like every other system table's bootstrap.
pub async fn bootstrap_agent_memory(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(MEMORY_TABLE, &memory_fields())
        .await?;
    if full_text_constraint(&table).is_some() {
        return Ok(table);
    }
    let kind = ConstraintKind::FullTextSearch {
        language: MEMORY_LANGUAGE.to_owned(),
    };
    let constraint =
        TableConstraint::new(TableConstraint::derived_name(MEMORY_TABLE, &kind, ""), kind);
    let dialect = catalog.primary().dialect();
    let steps = create_constraint_steps(
        dialect,
        &SchemaProjection::new(vec![table.clone()]),
        &table,
        &constraint,
    )?;
    catalog.apply_schema_batch(&steps).await?;
    catalog.reload().await?;
    catalog.require(MEMORY_TABLE)
}

/// One stored memory.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    pub id: i64,
    pub agent: String,
    pub user: Option<Uuid>,
    pub run: Option<Uuid>,
    pub written_at: DateTime<Utc>,
    pub kind: String,
    pub personal: bool,
    pub topic: String,
    pub contents: String,
}

impl MemoryEntry {
    /// The entry as the admin API returns it.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "agent": self.agent,
            "user_id": self.user.map(|u| u.to_string()),
            "run_id": self.run.map(|r| r.to_string()),
            "written_at": self.written_at.to_rfc3339(),
            "kind": self.kind,
            "personal": self.personal,
            "topic": self.topic,
            "contents": self.contents,
        })
    }

    /// The entry as the model reads it: what, about what, and when — not
    /// whose, which it has no use for.
    fn for_model(&self) -> Json {
        let mut out = json!({
            "contents": self.contents,
            "written_at": self.written_at.format("%Y-%m-%d").to_string(),
            "personal": self.personal,
        });
        if !self.topic.is_empty() {
            out["topic"] = json!(self.topic);
        }
        out
    }
}

/// A memory to write.
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub agent: String,
    pub user: Option<Uuid>,
    pub run: Option<Uuid>,
    pub kind: String,
    pub personal: bool,
    pub topic: String,
    pub contents: String,
}

/// Write one memory.
///
/// Refuses a personal memory with no user: it would belong to nobody, and the
/// read rule could then never give it back to anyone — or, worse, a later
/// change to the rule could give it to everyone.
pub async fn store_memory(catalog: &Catalog, memory: &NewMemory) -> Result<()> {
    insert_memory(catalog, memory, Utc::now()).await
}

/// Write a memory brought in from elsewhere, keeping when it was written —
/// what a restore does with v1's memories (TODO-agent-skills AS12.4).
/// `Ok(false)` when the same memory (agent, user, kind and contents) is
/// already here, so restoring the same file twice adds nothing the second time.
pub async fn import_memory(
    catalog: &Catalog,
    memory: &NewMemory,
    written_at: DateTime<Utc>,
) -> Result<bool> {
    let same = Expr::col(COL_MEMORY_AGENT)
        .eq(Expr::lit(memory.agent.as_str()))
        .and(Expr::binary(
            BinOp::IsNotDistinct,
            Expr::col(COL_MEMORY_USER),
            Expr::lit(Value::from(memory.user)),
        ))
        .and(Expr::col(COL_MEMORY_KIND).eq(Expr::lit(memory.kind.as_str())))
        .and(Expr::col(COL_MEMORY_CONTENTS).eq(Expr::lit(memory.contents.trim())));
    let select = Select::from(Source::table(MEMORY_TABLE))
        .filter(same)
        .limit(1);
    if !rows(catalog, select).await?.is_empty() {
        return Ok(false);
    }
    insert_memory(catalog, memory, written_at).await?;
    Ok(true)
}

async fn insert_memory(
    catalog: &Catalog,
    memory: &NewMemory,
    written_at: DateTime<Utc>,
) -> Result<()> {
    if memory.personal && memory.user.is_none() {
        return Err(Error::invalid(
            "a personal memory needs a user to belong to, and this run has none",
        ));
    }
    if memory.contents.trim().is_empty() {
        return Err(Error::invalid(
            "there is nothing to remember: `contents` is empty",
        ));
    }
    let insert = Insert::row(
        MEMORY_TABLE,
        [
            COL_MEMORY_AGENT,
            COL_MEMORY_USER,
            COL_MEMORY_RUN,
            COL_MEMORY_WRITTEN_AT,
            COL_MEMORY_KIND,
            COL_MEMORY_PERSONAL,
            COL_MEMORY_TOPIC,
            COL_MEMORY_CONTENTS,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::lit(memory.agent.as_str()),
            Expr::lit(Value::from(memory.user)),
            Expr::lit(Value::from(memory.run)),
            Expr::lit(written_at),
            Expr::lit(memory.kind.as_str()),
            Expr::lit(memory.personal),
            Expr::lit(memory.topic.trim()),
            Expr::lit(memory.contents.trim()),
        ],
    );
    exec(catalog, Statement::from(insert)).await
}

/// Who is reading the store — the access rule's one input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryReader {
    /// The admin's screen: every memory.
    Admin,
    /// A run on behalf of this user: shared memories and their own.
    User(Uuid),
    /// A run with no user (triggered, anonymous): shared memories only.
    Nobody,
}

impl MemoryReader {
    /// The reader a run's caller is.
    pub fn of(caller: &sc_agent::RunCaller) -> MemoryReader {
        match &caller.user {
            Some(user) => MemoryReader::User(user.id),
            None => MemoryReader::Nobody,
        }
    }

    /// **The** rule: what this reader may see, as a predicate — `None` for
    /// everything. Every read of the store is filtered by it.
    pub fn visibility(&self) -> Option<Expr> {
        let shared = Expr::col(COL_MEMORY_PERSONAL).eq(Expr::lit(false));
        match self {
            MemoryReader::Admin => None,
            MemoryReader::Nobody => Some(shared),
            MemoryReader::User(id) => Some(
                shared.or(Expr::col(COL_MEMORY_PERSONAL)
                    .eq(Expr::lit(true))
                    .and(Expr::col(COL_MEMORY_USER).eq(Expr::lit(*id)))),
            ),
        }
    }
}

/// What to list.
#[derive(Debug, Clone)]
pub struct MemoryFilter {
    pub reader: MemoryReader,
    /// Only this agent's.
    pub agent: Option<String>,
    /// Only those about (or written by) this user.
    pub user: Option<Uuid>,
    /// Only this kind.
    pub kind: Option<String>,
    /// Full-text search, ranked; otherwise newest first.
    pub search: Option<String>,
    pub limit: u64,
}

impl MemoryFilter {
    /// Everything `reader` may see, newest first, up to `limit`.
    pub fn new(reader: MemoryReader, limit: u64) -> MemoryFilter {
        MemoryFilter {
            reader,
            agent: None,
            user: None,
            kind: None,
            search: None,
            limit,
        }
    }
}

/// The memories `filter` selects, best match (or newest) first.
///
/// An empty list on an installation that never created the table: nothing has
/// been remembered there.
pub async fn list_memories(catalog: &Catalog, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
    let Some(table) = catalog.get(MEMORY_TABLE)? else {
        return Ok(Vec::new());
    };
    let mut conditions: Vec<Expr> = Vec::new();
    if let Some(rule) = filter.reader.visibility() {
        conditions.push(rule);
    }
    if let Some(agent) = &filter.agent {
        conditions.push(Expr::col(COL_MEMORY_AGENT).eq(Expr::lit(agent.as_str())));
    }
    if let Some(user) = filter.user {
        conditions.push(Expr::col(COL_MEMORY_USER).eq(Expr::lit(user)));
    }
    if let Some(kind) = &filter.kind {
        conditions.push(Expr::col(COL_MEMORY_KIND).eq(Expr::lit(kind.as_str())));
    }
    let mut order = Vec::new();
    if let Some(phrase) = filter
        .search
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let search = memory_search(&table, phrase);
        conditions.push(Expr::TextMatch(Box::new(search.clone())));
        order.push(OrderBy::desc(Expr::TextRank(Box::new(search))));
    }
    order.push(OrderBy::desc(Expr::col(COL_MEMORY_WRITTEN_AT)));
    order.push(OrderBy::desc(Expr::col(COL_MEMORY_ID)));
    let mut select = Select::from(Source::table(MEMORY_TABLE)).limit(filter.limit.max(1));
    if let Some(all) = conditions.into_iter().reduce(Expr::and) {
        select = select.filter(all);
    }
    select.order = order;
    rows(catalog, select)
        .await?
        .iter()
        .map(memory_from_row)
        .collect()
}

/// The search over the table's text, through its full-text constraint where
/// it has one (it always should: the bootstrap creates it).
fn memory_search(table: &Table, phrase: &str) -> sc_query::TextSearch {
    let columns = full_text_columns(table);
    indexed_text_search(table, columns.clone(), phrase).unwrap_or(sc_query::TextSearch {
        table: table.name.clone(),
        columns,
        language: MEMORY_LANGUAGE.to_owned(),
        phrase: phrase.to_owned(),
        fts5: None,
    })
}

/// One memory by id, whoever it belongs to — the admin's view.
pub async fn load_memory(catalog: &Catalog, id: i64) -> Result<Option<MemoryEntry>> {
    if catalog.get(MEMORY_TABLE)?.is_none() {
        return Ok(None);
    }
    let select = Select::from(Source::table(MEMORY_TABLE))
        .filter(Expr::col(COL_MEMORY_ID).eq(Expr::lit(id)));
    rows(catalog, select)
        .await?
        .first()
        .map(memory_from_row)
        .transpose()
}

/// Delete one memory. `false` when there was none with that id.
pub async fn delete_memory(catalog: &Catalog, id: i64) -> Result<bool> {
    if load_memory(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(MEMORY_TABLE).filter(Expr::col(COL_MEMORY_ID).eq(Expr::lit(id)));
    exec(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// A user's consolidated preferences for `agent`, if there are any.
pub async fn preferences_of(
    catalog: &Catalog,
    agent: &str,
    user: Uuid,
) -> Result<Option<MemoryEntry>> {
    let mut filter = MemoryFilter::new(MemoryReader::User(user), 1);
    filter.agent = Some(agent.to_owned());
    filter.user = Some(user);
    filter.kind = Some(KIND_PREFERENCES.to_owned());
    Ok(list_memories(catalog, &filter)
        .await?
        .into_iter()
        // Belt and braces: the visibility rule already admits only this
        // user's personal ones, and a shared "preferences" row is nobody's.
        .find(|m| m.personal && m.user == Some(user)))
}

/// Replace a user's preferences for `agent` with `contents`.
pub async fn replace_preferences(
    catalog: &Catalog,
    agent: &str,
    user: Uuid,
    contents: &str,
) -> Result<()> {
    let delete = Delete::from(MEMORY_TABLE).filter(
        Expr::col(COL_MEMORY_AGENT)
            .eq(Expr::lit(agent))
            .and(Expr::col(COL_MEMORY_USER).eq(Expr::lit(user)))
            .and(Expr::col(COL_MEMORY_KIND).eq(Expr::lit(KIND_PREFERENCES))),
    );
    exec(catalog, Statement::from(delete)).await?;
    store_memory(
        catalog,
        &NewMemory {
            agent: agent.to_owned(),
            user: Some(user),
            run: None,
            kind: KIND_PREFERENCES.to_owned(),
            personal: true,
            topic: "preferences".to_owned(),
            contents: contents.to_owned(),
        },
    )
    .await
}

/// Every user with a personal memory of `agent`'s, in id order.
pub async fn users_with_memories(catalog: &Catalog, agent: &str) -> Result<Vec<Uuid>> {
    if catalog.get(MEMORY_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let select = Select::from(Source::table(MEMORY_TABLE)).filter(
        Expr::col(COL_MEMORY_AGENT)
            .eq(Expr::lit(agent))
            .and(Expr::col(COL_MEMORY_PERSONAL).eq(Expr::lit(true)))
            .and(Expr::col(COL_MEMORY_KIND).eq(Expr::lit(KIND_MEMORY))),
    );
    let users: BTreeSet<Uuid> = rows(catalog, select)
        .await?
        .iter()
        .filter_map(|row| match row.get(COL_MEMORY_USER) {
            Some(Value::Uuid(id)) => Some(*id),
            _ => None,
        })
        .collect();
    Ok(users.into_iter().collect())
}

fn memory_from_row(row: &Row) -> Result<MemoryEntry> {
    let text = |column: &str| -> String {
        match row.get(column) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        }
    };
    let uuid = |column: &str| -> Option<Uuid> {
        match row.get(column) {
            Some(Value::Uuid(id)) => Some(*id),
            Some(Value::Text(s)) => s.parse().ok(),
            _ => None,
        }
    };
    let id = match row.get(COL_MEMORY_ID) {
        Some(Value::Int(id)) => *id,
        other => {
            return Err(Error::invalid(format!(
                "{MEMORY_TABLE}.{COL_MEMORY_ID} should be an integer, got {other:?}"
            )));
        }
    };
    let written_at = match row.get(COL_MEMORY_WRITTEN_AT) {
        Some(Value::Timestamp(at)) => *at,
        Some(Value::Text(s)) => DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default(),
        _ => DateTime::<Utc>::default(),
    };
    let personal = match row.get(COL_MEMORY_PERSONAL) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Int(i)) => *i != 0,
        // Unreadable is personal: the safe reading of a flag that guards
        // somebody's privacy.
        _ => true,
    };
    Ok(MemoryEntry {
        id,
        agent: text(COL_MEMORY_AGENT),
        user: uuid(COL_MEMORY_USER),
        run: uuid(COL_MEMORY_RUN),
        written_at,
        kind: text(COL_MEMORY_KIND),
        personal,
        topic: text(COL_MEMORY_TOPIC),
        contents: text(COL_MEMORY_CONTENTS),
    })
}

async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

// --- the trait ------------------------------------------------------------------

/// Whether what this agent stores is personal (the default) or shared.
pub const CFG_PERSONAL: &str = "personal";
/// Whether the user's consolidated preferences go into the session header.
pub const CFG_PREFERENCES_IN_HEADER: &str = "preferences_in_header";
/// The most memories one search returns.
pub const CFG_MEMORY_MAX_RESULTS: &str = "max_results";
/// The default for [`CFG_MEMORY_MAX_RESULTS`].
pub const DEFAULT_MEMORY_RESULTS: u64 = 10;

/// The tool that writes a memory.
pub const TOOL_STORE_MEMORY: &str = "store_in_memory";
/// The tool that searches them.
pub const TOOL_SEARCH_MEMORY: &str = "search_memory";

/// The two fixed tool names. Fixed rather than derived, as `admin_copilot`'s
/// are: an agent has one memory, so enabling the trait twice collides — which
/// is the intended outcome.
pub fn tool_names() -> [&'static str; 2] {
    [TOOL_STORE_MEMORY, TOOL_SEARCH_MEMORY]
}

/// Long-term memory: `store_in_memory` and `search_memory` over the agent's
/// memories, personal or shared.
pub struct AgentMemory;

fn flag(config: &Attrs, key: &str, default: bool) -> bool {
    config.get(key).and_then(Json::as_bool).unwrap_or(default)
}

fn max_results(config: &Attrs) -> Result<u64> {
    crate::files::config_count(config, CFG_MEMORY_MAX_RESULTS, DEFAULT_MEMORY_RESULTS)
}

#[async_trait::async_trait]
impl AgentTrait for AgentMemory {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Remember things across conversations, per user or shared, and search them"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_PERSONAL, BasicType::Bool)
                .label("Memories are personal")
                .sublabel(
                    "On: what is stored is only ever read back in the same user's \
                     conversations. Off: every user of this agent can be told it.",
                )
                .default_value(true),
            FormField::new(CFG_PREFERENCES_IN_HEADER, BasicType::Bool)
                .label("Start each conversation with the user's consolidated preferences")
                .sublabel("Written by a `consolidate_memory` trigger")
                .default_value(true),
            FormField::new(CFG_MEMORY_MAX_RESULTS, BasicType::Int)
                .label("Most memories one search returns")
                .default_value(DEFAULT_MEMORY_RESULTS as i64),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        if max_results(check.config)? == 0 {
            return Err(Error::invalid(format!(
                "`{CFG_MEMORY_MAX_RESULTS}` must be at least 1"
            )));
        }
        if check.catalog.get(MEMORY_TABLE)?.is_none() {
            return Err(Error::config(format!(
                "the memory store (`{MEMORY_TABLE}`) is not installed on this server"
            )));
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let personal = flag(config, CFG_PERSONAL, true);
        let ceiling = max_results(config).unwrap_or(DEFAULT_MEMORY_RESULTS);
        let whose = if personal {
            "It is personal: it will be remembered in later conversations with this same \
             user, and never told to anybody else."
        } else {
            "It is shared: any user of this agent may be told it later, so never store \
             anything private about one person."
        };
        vec![
            ToolSpec::new(
                TOOL_STORE_MEMORY,
                format!(
                    "Remember something for later conversations: a fact, a decision, or \
                     what the user prefers. Store one self-contained statement per call, \
                     written so it makes sense without this conversation. {whose}"
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "contents": {
                            "type": "string",
                            "description": "What to remember, as a complete sentence."
                        },
                        "topic": {
                            "type": "string",
                            "description": "A few words saying what it is about."
                        }
                    },
                    "required": ["contents"],
                    "additionalProperties": false,
                }),
            ),
            ToolSpec::new(
                TOOL_SEARCH_MEMORY,
                format!(
                    "Search what you have remembered from earlier conversations, best \
                     match first, at most {ceiling} memories. The query is words to look \
                     for: `\"quoted phrases\"`, `-excluded` words and `or` work."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Words to look for."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false,
                }),
            ),
        ]
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let empty = Map::new();
        let args = args.as_object().unwrap_or(&empty);
        let text = |key: &str| {
            args.get(key)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        match tool {
            TOOL_STORE_MEMORY => {
                let personal = flag(config, CFG_PERSONAL, true);
                let user = ctx.caller.user.as_ref().map(|u| u.id);
                if personal && user.is_none() {
                    return Err(Error::invalid(
                        "nobody is signed in to this conversation, so there is nobody to \
                         remember this for; it was not stored",
                    ));
                }
                store_memory(
                    ctx.catalog,
                    &NewMemory {
                        agent: ctx.agent.to_owned(),
                        user,
                        run: Some(ctx.run.0),
                        kind: KIND_MEMORY.to_owned(),
                        personal,
                        topic: text("topic"),
                        contents: text("contents"),
                    },
                )
                .await?;
                Ok(json!({ "stored": true, "personal": personal }))
            }
            TOOL_SEARCH_MEMORY => {
                let query = text("query");
                if query.is_empty() {
                    return Err(Error::invalid("`query` is required: the words to look for"));
                }
                let mut filter =
                    MemoryFilter::new(MemoryReader::of(ctx.caller), max_results(config)?);
                filter.agent = Some(ctx.agent.to_owned());
                filter.search = Some(query);
                let found = list_memories(ctx.catalog, &filter).await?;
                Ok(json!({
                    "memories": found.iter().map(MemoryEntry::for_model).collect::<Vec<_>>(),
                    "count": found.len(),
                }))
            }
            other => Err(Error::invalid(format!(
                "`memory` has no tool `{other}`; it offers {}",
                tool_names().join(" and ")
            ))),
        }
    }

    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        if !flag(config, CFG_PREFERENCES_IN_HEADER, true) {
            return Ok(None);
        }
        let Some(user) = cx.caller.user.as_ref() else {
            return Ok(None);
        };
        let Some(preferences) = preferences_of(cx.catalog, cx.agent, user.id).await? else {
            return Ok(None);
        };
        Ok(Some(format!(
            "What you remember about this user's preferences from earlier conversations:\n\n{}",
            preferences.contents.trim()
        )))
    }
}

// --- the consolidation action ----------------------------------------------------

/// The agent whose memories are consolidated.
pub const CFG_MEMORY_AGENT: &str = "agent";
/// The most memories read per user, newest first.
pub const CFG_MAX_MEMORIES: &str = "max_memories";
const DEFAULT_MAX_MEMORIES: u64 = 200;

/// What the cheap role is told.
const CONSOLIDATE_PROMPT: &str = "You maintain a short profile of one user's preferences \
for an assistant. You are given the profile so far (possibly empty) and the things the \
assistant remembered about the user in conversations. Write the updated profile: a concise \
list of the user's stable preferences, facts about them and how they like to be answered. \
Later statements override earlier ones. Leave out one-off details. Answer with the profile \
only.";

/// `consolidate_memory`: summarise each user's personal memories of one agent
/// into a single `preferences` memory, through the agent's cheap role.
///
/// Meant for a scheduled trigger, where the event has no user and every user
/// with memories is consolidated. An event that carries a user (a trigger run
/// by a person) consolidates only theirs.
pub struct ConsolidateMemory {
    traits: Arc<AgentRegistry>,
    providers: Arc<dyn ProviderConnector>,
}

impl ConsolidateMemory {
    pub fn new(traits: Arc<AgentRegistry>, providers: Arc<dyn ProviderConnector>) -> Self {
        ConsolidateMemory { traits, providers }
    }
}

#[async_trait::async_trait]
impl Action for ConsolidateMemory {
    fn name(&self) -> &str {
        "consolidate_memory"
    }

    fn description(&self) -> &str {
        "Summarise each user's personal agent memories into their preferences"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_MEMORY_AGENT, BasicType::Text)
                .label("Agent")
                .required(),
            FormField::new(CFG_MAX_MEMORIES, BasicType::Int)
                .label("Most memories read per user")
                .default_value(DEFAULT_MAX_MEMORIES as i64),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let name = sc_action::config_str(check.config, CFG_MEMORY_AGENT)?;
        if sc_agent::load_agent_by_name(check.catalog, &name)
            .await?
            .is_none()
        {
            return Err(Error::invalid(format!("no agent named `{name}`")));
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let name = sc_action::config_str(ctx.config, CFG_MEMORY_AGENT).map_err(named)?;
        let limit = crate::files::config_count(ctx.config, CFG_MAX_MEMORIES, DEFAULT_MAX_MEMORIES)
            .map_err(named)?
            .max(1);
        let agents = sc_agent::Agents::load(ctx.catalog, &self.traits)
            .await
            .map_err(named)?;
        let agent = agents.require(&name).map_err(named)?;
        let event_user = ctx
            .event
            .user
            .as_ref()
            .and_then(|u| u.get("id"))
            .and_then(Json::as_str)
            .and_then(|s| s.parse::<Uuid>().ok());
        let users = match event_user {
            Some(user) => vec![user],
            None => users_with_memories(ctx.catalog, &name)
                .await
                .map_err(named)?,
        };
        let mut consolidated = Vec::new();
        let mut model = None;
        for user in users {
            let mut filter = MemoryFilter::new(MemoryReader::User(user), limit);
            filter.agent = Some(name.clone());
            filter.user = Some(user);
            filter.kind = Some(KIND_MEMORY.to_owned());
            let memories: Vec<MemoryEntry> = list_memories(ctx.catalog, &filter)
                .await
                .map_err(named)?
                .into_iter()
                .filter(|m| m.personal && m.user == Some(user))
                .collect();
            if memories.is_empty() {
                continue;
            }
            let before = preferences_of(ctx.catalog, &name, user)
                .await
                .map_err(named)?
                .map(|p| p.contents)
                .unwrap_or_default();
            if model.is_none() {
                model = Some(
                    self.providers
                        .connect(ctx.catalog, agent, ModelRole::Cheap)
                        .await
                        .map_err(named)?,
                );
            }
            let Some(connected) = model.as_ref() else {
                continue;
            };
            let request = LlmRequest {
                system: Some(CONSOLIDATE_PROMPT.to_owned()),
                messages: vec![LlmMessage::user(consolidation_brief(&before, &memories))],
                ..LlmRequest::default()
            };
            let answer = connected
                .provider
                .stream(request)
                .await
                .map_err(named)?
                .collect()
                .await
                .map_err(named)?;
            let profile = answer.content.trim();
            if profile.is_empty() {
                return Err(named(Error::msg(format!(
                    "agent `{name}`: the summary of a user's memories came back empty"
                ))));
            }
            replace_preferences(ctx.catalog, &name, user, profile)
                .await
                .map_err(named)?;
            consolidated.push(json!({ "user_id": user.to_string(), "memories": memories.len() }));
        }
        Ok(json!({ "agent": name, "consolidated": consolidated }))
    }
}

/// What the cheap role reads for one user: the profile so far, then the
/// memories oldest first, so "later overrides earlier" reads top to bottom.
fn consolidation_brief(before: &str, memories: &[MemoryEntry]) -> String {
    let mut out = String::from("Profile so far:\n");
    out.push_str(if before.trim().is_empty() {
        "(none)"
    } else {
        before.trim()
    });
    out.push_str("\n\nRemembered, oldest first:\n");
    for memory in memories.iter().rev() {
        out.push_str(&format!(
            "- [{}]{} {}\n",
            memory.written_at.format("%Y-%m-%d"),
            if memory.topic.is_empty() {
                String::new()
            } else {
                format!(" ({})", memory.topic)
            },
            memory.contents.trim()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_sees_shared_memories_and_only_their_own_personal_ones() {
        let id = Uuid::new_v4();
        // Rendered, the rule says exactly that: shared, or personal and theirs.
        let rule = format!("{:?}", MemoryReader::User(id).visibility().unwrap());
        assert!(rule.contains(&id.to_string()), "{rule}");
        assert!(rule.contains(COL_MEMORY_PERSONAL), "{rule}");
        // Nobody in particular sees only shared ones; the admin, everything.
        let nobody = format!("{:?}", MemoryReader::Nobody.visibility().unwrap());
        assert!(!nobody.contains(COL_MEMORY_USER), "{nobody}");
        assert!(MemoryReader::Admin.visibility().is_none());
    }

    #[test]
    fn the_brief_reads_oldest_first_after_the_profile() {
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let memory = |contents: &str, when: &str| MemoryEntry {
            id: 1,
            agent: "a".into(),
            user: None,
            run: None,
            written_at: at(when),
            kind: KIND_MEMORY.into(),
            personal: true,
            topic: String::new(),
            contents: contents.into(),
        };
        // Newest first, as the store lists them.
        let brief = consolidation_brief(
            "",
            &[
                memory("prefers tea", "2026-02-01T00:00:00Z"),
                memory("prefers coffee", "2026-01-01T00:00:00Z"),
            ],
        );
        assert!(brief.contains("(none)"));
        assert!(
            brief.find("coffee").unwrap() < brief.find("tea").unwrap(),
            "{brief}"
        );
    }
}
