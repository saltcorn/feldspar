//! The `_fd_llm_providers` and `_fd_llm_models` tables: their schemas,
//! bootstrap, and the [`LlmProviderDef`] and [`LlmModelDef`] ⇄ row mappings
//! (design §9, §11.1, TODO §3a).
//!
//! Follows `_fd_file_stores` in every respect that matters, because it is the
//! same kind of thing: a provider, like a store and like an application, has
//! **nothing to introspect it from**, so its row *is* its definition rather than
//! an overlay on something the database already knows.
//!
//! **Reading is strict.** A column that is missing or of the wrong shape is an
//! [`Error::invalid`] naming the provider and the column, not a silently
//! defaulted field: a half-understood row is a misconfigured provider the admin
//! needs told about, and one that silently defaults its `base_url` would send an
//! API key to the wrong host.
//!
//! **The key is not encrypted at rest** (§11.1, and out of scope for this
//! milestone by decision): it sits in the primary database like every other
//! configuration value. Saying so is better than implying a protection a
//! database dump would disprove. What *is* guaranteed is that it does not leave
//! through the API — see [`sc_types::redact_attrs`] and the admin handlers.
//!
//! **The host's provider is merged in.** The provider `feldspar.toml` supplies
//! (`crate::host`) has no row, but every reader here answers with it as if it
//! had one — listed, found by id and by name, with its models — and every
//! writer refuses it. Callers never need to know which kind they hold.

use sc_catalog::{
    Catalog, ConstraintKind, DataField, DataFieldKind, FieldId, SchemaStep, SharedTx, Table,
    TableConstraint, TableId,
};
use sc_db::{Row, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::def::{LlmProviderDef, LlmProviderDefId, validate_provider_config};
use crate::host::{host_llm_provider, is_host_llm_provider, read_only};
use crate::model::{
    LlmModelDef, LlmModelDefId, ModelKind, normalise_model_config, validate_model_config,
};

/// Name of the providers table in the primary database.
pub const LLM_PROVIDERS_TABLE: &str = "_fd_llm_providers";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The provider's name — what an agent references.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The backend serving the provider (`anthropic`, `openai_responses`).
pub const COL_BACKEND: &str = "backend";
/// The backend's settings (JSON object), including the API key.
pub const COL_CONFIG: &str = "config";
/// The sparse per-provider values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The fields of the `_fd_llm_providers` table, in declaration order.
///
/// `name` carries the `UNIQUE` constraint for the same reason a file store's
/// does: it is the key an agent resolves through, so two definitions claiming
/// one name is not a state the system can serve, and the database is the
/// authority because two admins saving concurrently cannot see each other's
/// transaction.
fn provider_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_BACKEND, text()).required(),
        DataField::plain(COL_CONFIG, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure the `_fd_llm_providers` table exists, and then `_fd_llm_models`
/// beside it ([`bootstrap_llm_models`]), creating either if absent.
///
/// One call for both, because a provider with no models table is not a state
/// anything can use, and the models' foreign key needs the providers first.
///
/// Idempotent and safe against a database that has never seen Saltcorn — the
/// same contract `bootstrap_file_stores` has. Call once at startup, after the
/// [`Catalog`] is initialised.
pub async fn bootstrap_llm_providers(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(LLM_PROVIDERS_TABLE, &provider_fields())
        .await?;
    bootstrap_llm_models(catalog).await?;
    Ok(table)
}

/// Save a provider definition: insert its row, or update it in place if a row
/// with its [`LlmProviderDefId`] already exists.
///
/// Settings are checked against the backend's spec first — a missing key is the
/// admin's to fix and the admin is standing in front of the form, whereas the
/// same mistake found at chat time is an agent that fails in a transcript.
///
/// The name is unique, and a clash is reported as an [`Error::invalid`] naming
/// the conflict rather than a raw constraint violation. The database's `UNIQUE`
/// constraint remains the authority: this check and the write are not one
/// transaction.
pub async fn save_llm_provider(catalog: &Catalog, def: &LlmProviderDef) -> Result<()> {
    check_provider_saveable(catalog, def).await?;

    let columns = provider_columns();
    let values = provider_values(def);

    if load_llm_provider(catalog, def.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(LLM_PROVIDERS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            LLM_PROVIDERS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(())
}

/// Everything [`save_llm_provider`] checks before it writes: a name, a backend,
/// settings matching that backend's spec, and no other provider holding the
/// name.
pub async fn check_provider_saveable(catalog: &Catalog, def: &LlmProviderDef) -> Result<()> {
    if is_host_llm_provider(catalog, def.id) {
        return Err(read_only(&format!("LLM provider `{}`", def.name.trim())));
    }
    let name = def.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("an LLM provider needs a name"));
    }
    if host_llm_provider(catalog).is_some_and(|host| host.def().name == name) {
        return Err(Error::invalid(format!(
            "LLM provider name `{name}` is used by the provider the server's \
             configuration file supplies; choose another"
        )));
    }
    if def.backend.trim().is_empty() {
        return Err(Error::invalid(format!(
            "LLM provider `{name}` needs a backend"
        )));
    }
    validate_provider_config(def)?;

    if let Some(other) = load_llm_provider_by_name(catalog, name).await?
        && other.id != def.id
    {
        return Err(Error::invalid(format!(
            "LLM provider name `{name}` is already used; \
             each provider is referenced by its own name"
        )));
    }
    Ok(())
}

/// Load the definition with this id, if it exists.
pub async fn load_llm_provider(
    catalog: &Catalog,
    id: LlmProviderDefId,
) -> Result<Option<LlmProviderDef>> {
    if let Some(host) = host_llm_provider(catalog)
        && host.def().id == id
    {
        return Ok(Some(host.def().clone()));
    }
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the definition named `name`, if any — the lookup an agent's `provider`
/// resolves through.
pub async fn load_llm_provider_by_name(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<LlmProviderDef>> {
    if let Some(host) = host_llm_provider(catalog)
        && host.def().name == name
    {
        return Ok(Some(host.def().clone()));
    }
    load_stored_provider_by_name(catalog, name).await
}

/// The provider **the admin added** named `name`, leaving out the host's — what
/// the host's own name is checked against.
pub(crate) async fn load_stored_provider_by_name(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<LlmProviderDef>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// The definition named `name`, or an error saying it does not exist — what an
/// agent's validation calls, so a provider that was deleted names itself in the
/// reason the agent is invalid (§11.2).
pub async fn require_llm_provider(catalog: &Catalog, name: &str) -> Result<LlmProviderDef> {
    load_llm_provider_by_name(catalog, name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no LLM provider named `{name}`")))
}

/// Every definition, the host's included, ordered by name — what the admin UI
/// lists.
pub async fn list_llm_providers(catalog: &Catalog) -> Result<Vec<LlmProviderDef>> {
    let select = Select::from(Source::table(LLM_PROVIDERS_TABLE));
    let mut defs: Vec<LlmProviderDef> = rows(catalog, select)
        .await?
        .iter()
        .map(provider_from_row)
        .collect::<Result<_>>()?;
    if let Some(host) = host_llm_provider(catalog) {
        defs.push(host.def().clone());
    }
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(defs)
}

/// Delete a provider definition **and its models**, returning whether one was
/// there to delete.
///
/// The models go in the same transaction, first, because their foreign key
/// points at the provider and the schema layer has no `ON DELETE`. Either both
/// go or neither does.
///
/// `extra_referents` is how references this crate cannot see are supplied: an
/// **agent** names a provider and its models, and `_fd_agents` lives in
/// `sc-agent`, a layer above this one. Callers that know about agents collect
/// those first and pass them in; a caller passing an empty slice gets no
/// reference check at all, which is correct for a system with no agents and
/// wrong for one with them. The same arrangement `delete_file_store` uses for
/// applications, and for the same layering reason.
pub async fn delete_llm_provider(
    catalog: &Catalog,
    id: LlmProviderDefId,
    extra_referents: &[String],
) -> Result<bool> {
    let Some(def) = load_llm_provider(catalog, id).await? else {
        return Ok(false);
    };
    if is_host_llm_provider(catalog, id) {
        return Err(read_only(&format!("LLM provider `{}`", def.name)));
    }

    if !extra_referents.is_empty() {
        return Err(Error::invalid(format!(
            "LLM provider `{}` is still used by {}; \
             remove those references before deleting it",
            def.name,
            extra_referents.join(", ")
        )));
    }

    let models =
        Delete::from(LLM_MODELS_TABLE).filter(Expr::col(COL_PROVIDER_ID).eq(Expr::lit(def.id.0)));
    let provider =
        Delete::from(LLM_PROVIDERS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)));
    in_transaction(
        catalog,
        vec![Statement::from(models), Statement::from(provider)],
    )
    .await?;
    Ok(true)
}

/// The row's columns, in the order [`provider_values`] produces them.
fn provider_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_BACKEND,
        COL_CONFIG,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The definition serialised to its row's values, in [`provider_columns`] order.
fn provider_values(def: &LlmProviderDef) -> Vec<Value> {
    vec![
        Value::Uuid(def.id.0),
        Value::Text(def.name.trim().to_owned()),
        Value::Text(def.description.clone()),
        Value::Text(def.backend.trim().to_owned()),
        Value::Json(Json::Object(def.config.clone())),
        Value::Json(Json::Object(def.attributes.clone())),
    ]
}

/// Rebuild an [`LlmProviderDef`] from its row. The strictness note in the module
/// docs applies throughout.
fn provider_from_row(row: &Row) -> Result<LlmProviderDef> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => LlmProviderDefId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };

    // A NULL description is "none given", not a broken row.
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_DESCRIPTION, "text", other)),
    };

    Ok(LlmProviderDef {
        id,
        name: text(row, COL_NAME)?,
        description,
        backend: text(row, COL_BACKEND)?,
        config: object(row, COL_CONFIG)?,
        attributes: object(row, COL_ATTRIBUTES)?,
    })
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{LLM_PROVIDERS_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{LLM_PROVIDERS_TABLE}.{column} should be {expected}, got {}",
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

/// Load the single definition matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<LlmProviderDef>> {
    let select = Select::from(Source::table(LLM_PROVIDERS_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(provider_from_row(row)?)),
        None => Ok(None),
    }
}

// --- models ------------------------------------------------------------------

/// Name of the models table in the primary database. Not `_fd_models`, which
/// is the predictive models' table.
pub const LLM_MODELS_TABLE: &str = "_fd_llm_models";

/// The provider a model belongs to: a foreign key onto the providers' `id`.
pub const COL_PROVIDER_ID: &str = "provider_id";
/// Whether the model is its provider's default of its kind.
pub const COL_IS_DEFAULT: &str = "is_default";
/// The model's kind (`chat`, `embedding`, `image`, `speech`). Nullable,
/// because it was added to a table that has rows; `NULL` is chat.
pub const COL_KIND: &str = "kind";

/// The fields of the `_fd_llm_models` table, in declaration order.
///
/// `name` is **not** unique on its own: the same model name under two providers
/// is two rows. (`provider_id`, `name`) is, as a table constraint added by
/// [`bootstrap_llm_models`].
fn model_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField {
            kind: DataFieldKind::Key {
                target_table: TableId(LLM_PROVIDERS_TABLE.to_owned()),
                target_field: FieldId(COL_ID.to_owned()),
                summary_field: Some(FieldId(COL_NAME.to_owned())),
            },
            ..DataField::plain(COL_PROVIDER_ID, uuid()).required()
        },
        DataField::plain(COL_NAME, text()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_KIND, text()),
        DataField::plain(COL_IS_DEFAULT, TypeRef::Basic(BasicType::Bool)).required(),
        DataField::plain(COL_CONFIG, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// The jointly-unique key: one row per model name per provider.
fn model_key() -> ConstraintKind {
    ConstraintKind::Unique {
        fields: vec![COL_PROVIDER_ID.to_owned(), COL_NAME.to_owned()],
    }
}

/// Ensure `_fd_llm_models` exists, with its key, and return it.
///
/// **Runs after the providers table**, because its foreign key points there.
/// [`bootstrap_llm_providers`] calls it, so a caller bootstrapping providers
/// gets both.
pub async fn bootstrap_llm_models(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(LLM_MODELS_TABLE, &model_fields())
        .await?;
    let key = model_key();
    if table.constraints.iter().any(|c| c.kind == key) {
        return Ok(table);
    }
    let name = TableConstraint::derived_name(LLM_MODELS_TABLE, &key, "");
    catalog
        .apply_schema_batch(&[SchemaStep::Change(SchemaChange::AddUniqueConstraint {
            table: LLM_MODELS_TABLE.to_owned(),
            name,
            columns: vec![COL_PROVIDER_ID.to_owned(), COL_NAME.to_owned()],
        })])
        .await?;
    catalog.reload().await?;
    catalog.require(LLM_MODELS_TABLE)
}

/// Save a model row: insert it, or update it in place if a row with its id
/// exists.
///
/// Checked first: a name, a provider that exists, settings matching the
/// provider's backend, and no other row of the same provider holding the name.
/// Blank settings are dropped before the row is written, so a row records only
/// where it differs from the built-in defaults.
///
/// **At most one default per provider and kind**, enforced in the same
/// transaction as the write: saving a row as the default clears the flag on
/// every other row of its provider and kind, so two concurrent saves cannot
/// both leave a default behind.
pub async fn save_llm_model(catalog: &Catalog, model: &LlmModelDef) -> Result<LlmModelDef> {
    let mut model = model.clone();
    model.name = model.name.trim().to_owned();
    model.config = normalise_model_config(&model.config);
    check_model_saveable(catalog, &model).await?;

    let columns = model_columns();
    let values = model_values(&model);
    let mut statements = Vec::new();
    if model.is_default {
        let mut same_kind = Expr::col(COL_KIND).eq(Expr::lit(model.kind.as_str()));
        if model.kind == ModelKind::Chat {
            same_kind = same_kind.or(Expr::unary(sc_query::UnOp::IsNull, Expr::col(COL_KIND)));
        }
        let clear = Update::new(
            LLM_MODELS_TABLE,
            vec![Assignment::new(
                COL_IS_DEFAULT.to_owned(),
                Expr::Lit(Value::Bool(false)),
            )],
        )
        .filter(
            Expr::col(COL_PROVIDER_ID)
                .eq(Expr::lit(model.provider_id.0))
                .and(Expr::binary(
                    sc_query::BinOp::Ne,
                    Expr::col(COL_ID),
                    Expr::lit(model.id.0),
                ))
                .and(same_kind),
        );
        statements.push(Statement::from(clear));
    }
    if load_llm_model(catalog, model.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(LLM_MODELS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(model.id.0)));
        statements.push(Statement::from(update));
    } else {
        let insert = Insert::row(
            LLM_MODELS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        statements.push(Statement::from(insert));
    }
    in_transaction(catalog, statements).await?;
    Ok(model)
}

/// Everything [`save_llm_model`] checks before it writes.
pub async fn check_model_saveable(catalog: &Catalog, model: &LlmModelDef) -> Result<()> {
    let name = model.name.trim();
    if let Some(host) = host_llm_provider(catalog)
        && host.def().id == model.provider_id
    {
        return Err(read_only(&format!(
            "the models of LLM provider `{}`",
            host.def().name
        )));
    }
    if name.is_empty() {
        return Err(Error::invalid("an LLM model needs a name"));
    }
    let provider = load_llm_provider(catalog, model.provider_id)
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "LLM model `{name}`: its provider {} does not exist",
                model.provider_id.0
            ))
        })?;
    // A row cannot move to another provider: its prices and capabilities were
    // entered for this one. Nor change kind: what names it expects that kind.
    if let Some(stored) = load_llm_model(catalog, model.id).await? {
        if stored.provider_id != model.provider_id {
            return Err(Error::invalid(format!(
                "LLM model `{name}` belongs to another provider and cannot be moved"
            )));
        }
        if stored.kind != model.kind {
            return Err(Error::invalid(format!(
                "LLM model `{name}` is {} model and cannot become {} model; add a new \
                 model instead",
                stored.kind.with_article(),
                model.kind.with_article()
            )));
        }
    }
    validate_model_config(&provider.backend, model)?;
    if let Some(other) = load_llm_model_by_name(catalog, &provider, name).await?
        && other.id != model.id
    {
        return Err(Error::invalid(format!(
            "LLM provider `{}` already has a model named `{name}`",
            provider.name
        )));
    }
    Ok(())
}

/// Load the model row with this id, if it exists.
pub async fn load_llm_model(catalog: &Catalog, id: LlmModelDefId) -> Result<Option<LlmModelDef>> {
    if let Some(model) = host_model(catalog, id) {
        return Ok(Some(model));
    }
    let select = Select::from(Source::table(LLM_MODELS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)))
        .limit(1);
    let Some(row) = rows(catalog, select).await?.into_iter().next() else {
        return Ok(None);
    };
    let model = model_from_row(&row)?;
    // Read strictly, like the providers: a row whose settings no longer match
    // its backend is reported, not half-used.
    if let Some(provider) = load_llm_provider(catalog, model.provider_id).await? {
        validate_model_config(&provider.backend, &model)?;
    }
    Ok(Some(model))
}

/// The model of `provider` named `name`, if any.
pub async fn load_llm_model_by_name(
    catalog: &Catalog,
    provider: &LlmProviderDef,
    name: &str,
) -> Result<Option<LlmModelDef>> {
    Ok(list_llm_models(catalog, provider)
        .await?
        .into_iter()
        .find(|m| m.name == name.trim()))
}

/// Every model of `provider`, ordered by name — what the admin UI lists.
pub async fn list_llm_models(
    catalog: &Catalog,
    provider: &LlmProviderDef,
) -> Result<Vec<LlmModelDef>> {
    if let Some(host) = host_llm_provider(catalog)
        && host.def().id == provider.id
    {
        return Ok(host.models().to_vec());
    }
    let select = Select::from(Source::table(LLM_MODELS_TABLE))
        .filter(Expr::col(COL_PROVIDER_ID).eq(Expr::lit(provider.id.0)));
    let mut models: Vec<LlmModelDef> = rows(catalog, select)
        .await?
        .iter()
        .map(model_from_row)
        .collect::<Result<_>>()?;
    for model in &models {
        validate_model_config(&provider.backend, model)?;
    }
    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models)
}

/// The model an agent's (`provider`, `model`) pair names: the chat model row
/// called `model`, or the provider's default chat model when `model` is blank.
///
/// The error says which is missing — no such model, or no default — because
/// the two are fixed in different places.
pub async fn require_llm_model(
    catalog: &Catalog,
    provider: &LlmProviderDef,
    model: Option<&str>,
) -> Result<LlmModelDef> {
    // A chat model is always a row: its capabilities and window are the
    // loop's to know.
    if let Some(name) = model.map(str::trim).filter(|m| !m.is_empty())
        && load_llm_model_by_name(catalog, provider, name)
            .await?
            .is_none()
    {
        return Err(Error::not_found(format!(
            "LLM provider `{}` has no model named `{name}`",
            provider.name
        )));
    }
    require_model_of_kind(catalog, provider, model, ModelKind::Chat).await
}

/// The model of `kind` that (`provider`, `model`) names (TODO-agent-skills
/// AS3.1): the row called `model`, or the provider's default of that kind when
/// `model` is blank.
///
/// A row of another kind is refused by name. A non-chat model named with no row
/// is used by its name, with no prices: a provider serves more embedding,
/// image and speech models than anybody will enter, and the name is all the
/// call needs. ([`require_llm_model`] refuses a chat name with no row.)
pub async fn require_model_of_kind(
    catalog: &Catalog,
    provider: &LlmProviderDef,
    model: Option<&str>,
    kind: ModelKind,
) -> Result<LlmModelDef> {
    let models = list_llm_models(catalog, provider).await?;
    match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(name) => match models.into_iter().find(|m| m.name == name) {
            Some(found) if found.kind == kind => Ok(found),
            Some(found) => Err(Error::invalid(format!(
                "LLM model `{name}` of provider `{}` is {} model, not {} model",
                provider.name,
                found.kind.with_article(),
                kind.with_article()
            ))),
            None => Ok(LlmModelDef::new(provider.id, name).kind(kind)),
        },
        None => models
            .into_iter()
            .find(|m| m.kind == kind && m.is_default)
            .ok_or_else(|| {
                // A chat model is what "model" means to most readers.
                let kind = match kind {
                    ModelKind::Chat => String::new(),
                    other => format!("{other} "),
                };
                Error::not_found(format!(
                    "LLM provider `{}` has no default {kind}model; mark one of its {kind}models \
                     as the default, or name a model",
                    provider.name
                ))
            }),
    }
}

/// Delete a model row, returning whether one was there to delete.
///
/// Refused while `extra_referents` is non-empty, naming them — the agents that
/// call this model, collected by a caller that can see agents, exactly as for
/// [`delete_llm_provider`].
pub async fn delete_llm_model(
    catalog: &Catalog,
    id: LlmModelDefId,
    extra_referents: &[String],
) -> Result<bool> {
    if let Some(model) = host_model(catalog, id) {
        return Err(read_only(&format!("LLM model `{}`", model.name)));
    }
    let select = Select::from(Source::table(LLM_MODELS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)))
        .limit(1);
    let Some(row) = rows(catalog, select).await?.into_iter().next() else {
        return Ok(false);
    };
    let model = model_from_row(&row)?;
    if !extra_referents.is_empty() {
        return Err(Error::invalid(format!(
            "LLM model `{}` is still used by {}; \
             remove those references before deleting it",
            model.name,
            extra_referents.join(", ")
        )));
    }
    let delete = Delete::from(LLM_MODELS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The host provider's model with this id, if it is one.
fn host_model(catalog: &Catalog, id: LlmModelDefId) -> Option<LlmModelDef> {
    host_llm_provider(catalog)?
        .models()
        .iter()
        .find(|m| m.id == id)
        .cloned()
}

/// The model row's columns, in the order [`model_values`] produces them.
fn model_columns() -> Vec<String> {
    [
        COL_ID,
        COL_PROVIDER_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_KIND,
        COL_IS_DEFAULT,
        COL_CONFIG,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The model serialised to its row's values, in [`model_columns`] order.
fn model_values(model: &LlmModelDef) -> Vec<Value> {
    vec![
        Value::Uuid(model.id.0),
        Value::Uuid(model.provider_id.0),
        Value::Text(model.name.trim().to_owned()),
        Value::Text(model.description.clone()),
        Value::Text(model.kind.as_str().to_owned()),
        Value::Bool(model.is_default),
        Value::Json(Json::Object(model.config.clone())),
        Value::Json(Json::Object(model.attributes.clone())),
    ]
}

/// Rebuild an [`LlmModelDef`] from its row, strictly.
fn model_from_row(row: &Row) -> Result<LlmModelDef> {
    let uuid = |column: &str| match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad_model_column(column, "a uuid", other)),
    };
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_model_column(COL_DESCRIPTION, "text", other)),
    };
    let kind = match row.get(COL_KIND) {
        Some(Value::Text(t)) => ModelKind::parse(t)
            .map_err(|e| Error::invalid(format!("{LLM_MODELS_TABLE}.{COL_KIND}: {e}")))?,
        Some(Value::Null) | None => ModelKind::Chat,
        other => return Err(bad_model_column(COL_KIND, "text", other)),
    };
    let is_default = match row.get(COL_IS_DEFAULT) {
        Some(Value::Bool(b)) => *b,
        other => return Err(bad_model_column(COL_IS_DEFAULT, "a bool", other)),
    };
    let name = match row.get(COL_NAME) {
        Some(Value::Text(t)) => t.clone(),
        other => return Err(bad_model_column(COL_NAME, "text", other)),
    };
    let object = |column: &str| match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        other => Err(bad_model_column(column, "a json object", other)),
    };
    Ok(LlmModelDef {
        id: LlmModelDefId(uuid(COL_ID)?),
        provider_id: LlmProviderDefId(uuid(COL_PROVIDER_ID)?),
        name,
        description,
        kind,
        is_default,
        config: object(COL_CONFIG)?,
        attributes: object(COL_ATTRIBUTES)?,
    })
}

fn bad_model_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{LLM_MODELS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("{LLM_MODELS_TABLE} row has no `{column}` column")),
    }
}

/// Run `statements` in one transaction on the primary database: all of them,
/// or none.
async fn in_transaction(catalog: &Catalog, statements: Vec<Statement>) -> Result<()> {
    let tx = SharedTx::begin_primary(catalog)?;
    for statement in &statements {
        if let Err(e) = tx.run(None, statement).await {
            // The rollback's own failure would hide the reason; the statement's
            // error is the one worth reporting.
            let _ = tx.rollback().await;
            return Err(e);
        }
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::ANTHROPIC_BACKEND;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = provider_fields();
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
    }

    #[test]
    fn the_tables_are_hidden_system_tables() {
        assert!(LLM_PROVIDERS_TABLE.starts_with("_fd_"));
        assert!(LLM_MODELS_TABLE.starts_with("_fd_"));
        assert_ne!(LLM_MODELS_TABLE, "_fd_models");
    }

    #[test]
    fn a_model_belongs_to_its_provider_by_a_foreign_key_and_is_unique_within_it() {
        let fields = model_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();
        assert!(matches!(
            &by_name(COL_PROVIDER_ID).kind,
            DataFieldKind::Key { target_table, .. } if target_table.0 == LLM_PROVIDERS_TABLE
        ));
        assert!(
            !by_name(COL_NAME).unique,
            "unique per provider, not globally"
        );
        assert!(by_name(COL_IS_DEFAULT).required);
        assert_eq!(
            model_key(),
            ConstraintKind::Unique {
                fields: vec![COL_PROVIDER_ID.to_owned(), COL_NAME.to_owned()]
            }
        );
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        // The insert pairs these two positionally, so a column added to one and
        // not the other would write an API key into the wrong column.
        let def = LlmProviderDef::new("main", ANTHROPIC_BACKEND).with("api_key", "sk-x");
        assert_eq!(provider_columns().len(), provider_values(&def).len());
        assert_eq!(provider_columns().len(), provider_fields().len());
        let model = LlmModelDef::new(def.id, "claude-sonnet-5");
        assert_eq!(model_columns().len(), model_values(&model).len());
        assert_eq!(model_columns().len(), model_fields().len());
    }

    #[test]
    fn a_definitions_config_reaches_the_row_whole() {
        let def = LlmProviderDef::new("main", ANTHROPIC_BACKEND).with("api_key", "sk-x");
        let idx = provider_columns()
            .iter()
            .position(|c| c == COL_CONFIG)
            .unwrap();
        let Value::Json(Json::Object(config)) = &provider_values(&def)[idx] else {
            panic!("config should be a json object");
        };
        assert_eq!(config.get("api_key"), Some(&Json::from("sk-x")));
        assert_eq!(
            provider_values(&def)[provider_columns()
                .iter()
                .position(|c| c == COL_BACKEND)
                .unwrap()],
            Value::Text(ANTHROPIC_BACKEND.to_owned())
        );
    }
}
