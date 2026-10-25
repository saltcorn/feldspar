//! The `_fd_agents` table: its schema, bootstrap, and the [`Agent`] ⇄ row
//! mapping (design §9, §11.2).
//!
//! An agent, like a trigger, has **nothing to introspect it from**: no column in
//! `information_schema` says "answer questions about books using this prompt". So
//! by §9's rule its row *is* its definition — this is not an overlay, and without
//! the row there is no agent at all.
//!
//! **Reading is strict**: a column that is missing or of the wrong shape is an
//! [`Error::invalid`] naming the agent and the column, never a silently defaulted
//! field. A half-understood agent is one that would answer with the wrong tools,
//! or as the wrong provider, and the admin needs telling.
//!
//! What is *not* here: validation (see [`validate`](crate::validate), which
//! [`save_agent`] calls).

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::agent::{Agent, AgentId, EnabledTrait};

/// Name of the agents table in the primary database.
pub const AGENTS_TABLE: &str = "_fd_agents";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The agent's unique name — what a trigger's `run_agent` and the chat address.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The `_fd_llm_providers` name this agent calls through.
pub const COL_PROVIDER: &str = "provider";
/// The model, overriding the provider's default, or NULL for the provider's own.
pub const COL_MODEL: &str = "model";
/// The system prompt.
pub const COL_SYSTEM_PROMPT: &str = "system_prompt";
/// The enabled traits: a JSON **array** of `{trait, config}` objects.
///
/// An array rather than an object keyed by trait name, because a trait may be
/// enabled more than once (§11.2) — and because the order is the order its tools
/// are offered in, which an object would not preserve.
pub const COL_TRAITS: &str = "traits";
/// The role floor for chatting with this agent, or NULL for admin-only.
pub const COL_MIN_ROLE: &str = "min_role";
/// The sparse per-agent values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The fields of the `_fd_agents` table, in declaration order.
fn agent_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        // Unique for the same reason a trigger's name is: it is the key a
        // `run_agent` action and a chat client resolve through, so two agents
        // claiming one name is not a state the system can serve.
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_PROVIDER, text()).required(),
        // Nullable: "the provider's default model" is a real answer, and the
        // common one.
        DataField::plain(COL_MODEL, text()),
        DataField::plain(COL_SYSTEM_PROMPT, text()),
        DataField::plain(COL_TRAITS, json()).required(),
        DataField::plain(COL_MIN_ROLE, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        // When it was last saved (`sc_catalog::edited`): bookkeeping for the
        // entities list, written by every save and read by nothing else.
        sc_catalog::updated_at_field(),
    ]
}

/// Ensure the `_fd_agents` table exists, creating it if absent, and return it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as `bootstrap_triggers`. Call once at startup, after the [`Catalog`] is initialised.
pub async fn bootstrap_agents(catalog: &Catalog) -> Result<Table> {
    catalog.bootstrap_table(AGENTS_TABLE, &agent_fields()).await
}

/// Save an agent: insert its row, or update it in place if a row with its
/// [`AgentId`] already exists.
///
/// Validation runs **first** ([`validate_agent`](crate::validate_agent)), so an
/// agent that could never answer correctly — an unknown provider, a trait
/// configured against a table that is gone, two tools with one name — is refused
/// while the admin is still looking at the form rather than discovered inside a
/// conversation.
pub async fn save_agent(
    catalog: &Catalog,
    registry: &crate::AgentRegistry,
    agent: &Agent,
) -> Result<()> {
    crate::validate_agent(catalog, registry, agent).await?;

    let name = agent.name.trim();
    if let Some(other) = load_agent_by_name(catalog, name).await?
        && other.id != agent.id
    {
        return Err(Error::invalid(format!(
            "agent name `{name}` is already used; each agent is referenced by its own name"
        )));
    }

    let columns = agent_columns();
    let values = agent_values(agent);

    if load_agent(catalog, agent.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(AGENTS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(agent.id.0)));
        run(catalog, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            AGENTS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await
    }
}

/// Load the agent with this id, if it exists.
pub async fn load_agent(catalog: &Catalog, id: AgentId) -> Result<Option<Agent>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the agent named `name`, if any — the lookup a chat client and the
/// `run_agent` action both resolve through.
pub async fn load_agent_by_name(catalog: &Catalog, name: &str) -> Result<Option<Agent>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// The agent named `name`, or a not-found error naming it.
pub async fn require_agent(catalog: &Catalog, name: &str) -> Result<Agent> {
    load_agent_by_name(catalog, name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no agent named `{name}`")))
}

/// Every stored agent, ordered by name — what the admin UI lists.
///
/// Every *stored* one, including those that no longer validate: an agent that
/// fails validation stays listed and editable, because editing it is the repair
/// (§11.2). Sorting out which ones are usable is
/// [`Agents::load`](crate::Agents::load)'s job, not this one's.
pub async fn list_agents(catalog: &Catalog) -> Result<Vec<Agent>> {
    let select = Select::from(Source::table(AGENTS_TABLE));
    let mut out: Vec<Agent> = rows(catalog, select)
        .await?
        .iter()
        .map(agent_from_row)
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete an agent, returning whether one was there to delete.
///
/// Its runs are **not** deleted with it: a transcript is a record of what
/// happened, and what happened does not stop having happened because the agent
/// was removed. [`Run::subject`](crate::Run::subject) is the agent's name for
/// exactly this reason.
pub async fn delete_agent(catalog: &Catalog, id: AgentId) -> Result<bool> {
    if load_agent(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(AGENTS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The row's columns, in the order [`agent_values`] produces them.
fn agent_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_PROVIDER,
        COL_MODEL,
        COL_SYSTEM_PROMPT,
        COL_TRAITS,
        COL_MIN_ROLE,
        COL_ATTRIBUTES,
        sc_catalog::COL_UPDATED_AT,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The agent serialised to its row's values, in [`agent_columns`] order.
fn agent_values(agent: &Agent) -> Vec<Value> {
    vec![
        Value::Uuid(agent.id.0),
        Value::Text(agent.name.trim().to_owned()),
        Value::Text(agent.description.clone()),
        Value::Text(agent.provider.trim().to_owned()),
        match agent
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(model) => Value::Text(model.to_owned()),
            None => Value::Null,
        },
        Value::Text(agent.system_prompt.clone()),
        Value::Json(Json::Array(
            agent
                .traits
                .iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Json::Null))
                .collect(),
        )),
        match agent.min_role {
            Some(role) => Value::Int(i64::from(role)),
            None => Value::Null,
        },
        Value::Json(Json::Object(agent.attributes.clone())),
        // Every save is an edit (`sc_catalog::edited`).
        sc_catalog::updated_now(),
    ]
}

/// Rebuild an [`Agent`] from its `_fd_agents` row. The strictness note in the
/// module docs applies throughout.
fn agent_from_row(row: &Row) -> Result<Agent> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => AgentId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let name = text(row, COL_NAME)?;

    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_DESCRIPTION, "text", other)),
    };
    let system_prompt = match row.get(COL_SYSTEM_PROMPT) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_SYSTEM_PROMPT, "text", other)),
    };

    // A value outside 1–100 is not clamped: reading a corrupt role as the nearest
    // valid one would quietly change who may talk to the agent.
    let min_role = match row.get(COL_MIN_ROLE) {
        Some(Value::Null) | None => None,
        Some(Value::Int(i)) => Some(
            u8::try_from(*i)
                .ok()
                .filter(|r| (1..=100).contains(r))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "agent `{name}`: {AGENTS_TABLE}.{COL_MIN_ROLE} should be a role \
                         between 1 and 100, got {i}"
                    ))
                })?,
        ),
        other => return Err(bad_column(COL_MIN_ROLE, "an integer role", other)),
    };

    Ok(Agent {
        id,
        name: name.clone(),
        description,
        provider: text(row, COL_PROVIDER)?,
        model: optional_text(row, COL_MODEL)?,
        system_prompt,
        traits: traits(row, &name)?,
        min_role,
        attributes: object(row, COL_ATTRIBUTES)?,
    })
}

/// The enabled traits: a JSON array of `{trait, config}`.
///
/// An entry that does not parse names the agent and its position, because "the
/// third trait is malformed" is something an admin can act on and "invalid JSON"
/// is not.
fn traits(row: &Row, agent: &str) -> Result<Vec<EnabledTrait>> {
    let array = match row.get(COL_TRAITS) {
        Some(Value::Json(Json::Array(a))) => a,
        Some(Value::Json(_)) => {
            return Err(Error::invalid(format!(
                "agent `{agent}`: {AGENTS_TABLE}.{COL_TRAITS} should be a json array"
            )));
        }
        other => return Err(bad_column(COL_TRAITS, "json", other)),
    };
    array
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            serde_json::from_value(entry.clone()).map_err(|e| {
                Error::invalid(format!(
                    "agent `{agent}`: trait {} is not readable: {e}",
                    i + 1
                ))
            })
        })
        .collect()
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A nullable text column: NULL and empty both read as `None`.
fn optional_text(row: &Row, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) if t.trim().is_empty() => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{AGENTS_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{AGENTS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Load the single agent matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Agent>> {
    let select = Select::from(Source::table(AGENTS_TABLE)).filter(filter);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(agent_from_row(row)?)),
        None => Ok(None),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = agent_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required && by_name(COL_NAME).unique);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        assert!(!by_name(COL_DESCRIPTION).required);
        // A provider is what an agent talks through; there is no agent without
        // one. A model override is optional, because the provider has a default.
        assert!(by_name(COL_PROVIDER).required);
        assert!(!by_name(COL_MODEL).required);
        // The trait list is required *as a column* — an agent with no traits
        // stores an empty array, which is a real configuration (it can still
        // chat), whereas NULL would be an unreadable one.
        assert!(by_name(COL_TRAITS).required);
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let agent = Agent::new("a", "p").with_trait(EnabledTrait::new("query_table"));
        assert_eq!(agent_columns().len(), agent_values(&agent).len());
        let declared: Vec<String> = agent_fields().iter().map(|f| f.base.name.clone()).collect();
        assert_eq!(agent_columns(), declared);
    }

    #[test]
    fn the_traits_column_is_an_array_of_pairs() {
        // The shape matters: the same trait twice is two entries, and the order
        // is the order its tools are offered in.
        let agent = Agent::new("a", "p")
            .with_trait(EnabledTrait::new("query_table").config("table", "books"))
            .with_trait(EnabledTrait::new("query_table").config("table", "orders"));
        let values = agent_values(&agent);
        let Value::Json(Json::Array(entries)) = &values[6] else {
            panic!("the traits column is a json array");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["trait"], Json::from("query_table"));
        assert_eq!(entries[0]["config"]["table"], Json::from("books"));
        assert_eq!(entries[1]["config"]["table"], Json::from("orders"));
    }

    #[test]
    fn a_cleared_model_override_stores_as_null() {
        // "the provider's default" and "the admin emptied the box" are the same
        // state and must store identically.
        let mut agent = Agent::new("a", "p");
        agent.model = Some("   ".to_owned());
        assert_eq!(agent_values(&agent)[4], Value::Null);
        agent.model = Some(" gpt-5 ".to_owned());
        assert_eq!(agent_values(&agent)[4], Value::Text("gpt-5".to_owned()));
    }
}
