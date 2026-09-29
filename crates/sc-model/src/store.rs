//! The `_fd_models` table: its schema, bootstrap, and the [`Model`] ⇄ row
//! mapping (TODO §15; design §9).
//!
//! A model, like a trigger or an agent, has **nothing to introspect it from**:
//! no column in `information_schema` says "predict `price` from these four
//! formulas with a linear regression". So by §9's rule its row *is* its
//! definition — this is not an overlay, and without the row there is no model at
//! all.
//!
//! **Reading is strict**: a column that is missing or of the wrong shape is an
//! [`Error::invalid`] naming the model and the column, never a silently
//! defaulted field. The failure this prevents is the one this whole milestone is
//! careful about — a model read with half a dataset would be *fitted*, and a fit
//! that quietly dropped a feature produces a number that looks exactly like the
//! right one.
//!
//! The judgements §9 asks for, made out loud:
//!
//! - **`dataset`, `configuration`, `hyperparameters` and `split` are JSON
//!   columns**, because each is a structured value with no independent identity
//!   (§3 for the dataset in particular) and nothing queries into them.
//! - **`table_name` is a column even though the dataset already carries it**,
//!   because "the models on this table" is a question two things ask — the model
//!   list's filter and `fit_model`'s `config_spec_for` — and answering it by
//!   reading every dataset would be a scan. It is *derived* on the way out and
//!   *checked* on the way in ([`Model::table`]), so the duplication cannot drift.
//!
//! What is *not* here: validation (see [`validate`](crate::validate), which
//! [`save_model`] calls).

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::dataset::{Dataset, DatasetShape};
use crate::model::{Model, ModelId, NamedDataset};
use crate::registry::ModelRegistry;
use crate::split::Split;

/// Name of the models table in the primary database.
pub const MODELS_TABLE: &str = "_fd_models";

/// The [`OptionsSource::ServerQuery`](sc_types::OptionsSource) name meaning "the
/// models over this table" — what `fit_model`'s model picker declares.
///
/// A query name rather than a resolved list, because the answer is *rows*: a
/// `config_spec_for` is synchronous and cannot read them, so the declaration
/// says what it wants and the endpoint that serves it fills the list in. The
/// same arrangement a File field's file-store picker already has.
pub const MODELS_QUERY: &str = "models_for_table";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The model's unique name — what `predict()` and the admin screen address.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The table the dataset is over, derived from it (see the module docs).
pub const COL_TABLE_NAME: &str = "table_name";
/// The registered model-provider name.
pub const COL_PROVIDER: &str = "provider";
/// The dataset: table, columns and filter, as JSON (§3).
pub const COL_DATASET: &str = "dataset";
/// The provider's configuration (JSON object).
pub const COL_CONFIGURATION: &str = "configuration";
/// The hyperparameter space (JSON object): a value or a list per name (§11).
pub const COL_HYPERPARAMETERS: &str = "hyperparameters";
/// The split fractions and seed, as JSON (§5).
pub const COL_SPLIT: &str = "split";
/// The sparse per-model values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";
/// The related datasets, as a JSON array of [`NamedDataset`](crate::NamedDataset)s
/// (Stan TODO §7).
///
/// **Nullable**, and last, so that `bootstrap_table` can add it to an
/// installation whose `_fd_models` already has rows — a required column could
/// not be added there without a value for each. NULL reads as "none".
pub const COL_RELATED: &str = "related";

/// The fields of the `_fd_models` table, in declaration order.
fn model_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        // Unique for the reason a trigger's and an agent's names are: it is the
        // key a `predict("…")` formula resolves through, so two models claiming
        // one name is not a state the system can serve.
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_TABLE_NAME, text()).required(),
        DataField::plain(COL_PROVIDER, text()).required(),
        DataField::plain(COL_DATASET, json()).required(),
        DataField::plain(COL_CONFIGURATION, json()).required(),
        DataField::plain(COL_HYPERPARAMETERS, json()).required(),
        DataField::plain(COL_SPLIT, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        DataField::plain(COL_RELATED, json()),
    ]
}

/// Ensure the `_fd_models` table exists, creating it if absent, and return it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as `bootstrap_triggers` and `bootstrap_agents`. Call once at
/// startup, after the [`Catalog`] is initialised.
pub async fn bootstrap_models(catalog: &Catalog) -> Result<Table> {
    catalog.bootstrap_table(MODELS_TABLE, &model_fields()).await
}

/// Save a model: insert its row, or update it in place if a row with its
/// [`ModelId`] already exists.
///
/// Validation runs **first**
/// ([`validate_model`](crate::validate_model)), so a model that could never be
/// fitted — an unknown provider, a dataset column whose formula no longer
/// resolves, split fractions that do not sum to 1 — is refused while the admin
/// is still looking at the form rather than discovered inside a fit that has
/// already returned an instance id.
///
/// `shape` is the dataset's columns and their types, and it is optional for the
/// reason [`validate_model`](crate::validate_model) documents: only a read knows
/// it. A caller that has previewed the dataset (which the admin UI's builder
/// has) passes it and gets the configuration checked against the provider's real
/// form; one that has not still gets every check that does not need the data.
pub async fn save_model(
    catalog: &Catalog,
    registry: &ModelRegistry,
    model: &Model,
    shape: Option<&DatasetShape>,
) -> Result<()> {
    crate::validate_model(catalog, registry, model, shape).await?;

    let name = model.name.trim();
    if let Some(other) = load_model_by_name(catalog, name).await?
        && other.id != model.id
    {
        return Err(Error::invalid(format!(
            "model name `{name}` is already used; each model is referenced by its own name"
        )));
    }

    let columns = model_columns();
    let values = model_values(model)?;

    if load_model(catalog, model.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(MODELS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(model.id.0)));
        exec(catalog, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            MODELS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        exec(catalog, Statement::from(insert)).await
    }
}

/// Load the model with this id, if it exists.
pub async fn load_model(catalog: &Catalog, id: ModelId) -> Result<Option<Model>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the model named `name`, if any — the lookup `predict()` resolves
/// through.
pub async fn load_model_by_name(catalog: &Catalog, name: &str) -> Result<Option<Model>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// The model named `name`, or a not-found error naming it.
pub async fn require_model(catalog: &Catalog, name: &str) -> Result<Model> {
    load_model_by_name(catalog, name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no model named `{name}`")))
}

/// Every stored model, ordered by name — what the Models tab lists.
///
/// Every *stored* one, including those that no longer validate: a model whose
/// dataset stopped resolving because a column was dropped stays listed and
/// editable, because editing it is the repair. Sorting out which ones are usable
/// is [`Models::load`](crate::Models::load)'s job, not this one's.
pub async fn list_models(catalog: &Catalog) -> Result<Vec<Model>> {
    let select = Select::from(Source::table(MODELS_TABLE));
    let mut out: Vec<Model> = rows(catalog, select)
        .await?
        .iter()
        .map(model_from_row)
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The models over `table`, ordered by name — what `fit_model`'s
/// `config_spec_for` offers when its trigger has a table, and the reason
/// [`COL_TABLE_NAME`] is a column.
pub async fn models_for_table(catalog: &Catalog, table: &str) -> Result<Vec<Model>> {
    let select = Select::from(Source::table(MODELS_TABLE))
        .filter(Expr::col(COL_TABLE_NAME).eq(Expr::lit(table)));
    let mut out: Vec<Model> = rows(catalog, select)
        .await?
        .iter()
        .map(model_from_row)
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete a model, returning whether one was there to delete.
///
/// **Its instances go with it** — the opposite of what deleting an agent does to
/// its runs, and for a reason worth stating. A run is a transcript: a record of
/// something that happened, which does not stop having happened because the
/// agent was removed. An instance is a fitted *object*, whose parameters mean
/// nothing without the dataset they were fitted over and which nothing can list,
/// read or apply once its model is gone. Leaving them would be leaving rows
/// nobody can reach.
///
/// The model, its instances and their draws go in **one transaction**; then the
/// provider [`discard`](crate::ModelProvider::discard)s what each fit kept
/// outside the database, which is why the registry is a parameter.
pub async fn delete_model(
    catalog: &Catalog,
    registry: &ModelRegistry,
    id: ModelId,
) -> Result<bool> {
    let Some(model) = load_model(catalog, id).await? else {
        return Ok(false);
    };
    let states: Vec<Json> = crate::list_model_instances(catalog, id)
        .await?
        .into_iter()
        .map(|i| i.state)
        .collect();
    let mut tx = catalog.primary().begin().await?;
    let deleted = async {
        crate::instance_store::delete_instances_on(catalog, tx.as_mut(), id).await?;
        let delete = Delete::from(MODELS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
        crate::instance_store::run(tx.as_mut(), Statement::from(delete))
            .await
            .map(drop)
    }
    .await;
    crate::instance_store::finish(tx, deleted).await?;
    // The model row is gone, so the provider is asked for by the name it had.
    if let Some(provider) = registry.get(model.provider.trim()) {
        crate::instance_store::discard_with(provider.as_ref(), &states).await?;
    }
    Ok(true)
}

/// The row's columns, in the order [`model_values`] produces them.
fn model_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_TABLE_NAME,
        COL_PROVIDER,
        COL_DATASET,
        COL_CONFIGURATION,
        COL_HYPERPARAMETERS,
        COL_SPLIT,
        COL_ATTRIBUTES,
        COL_RELATED,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The model serialised to its row's values, in [`model_columns`] order.
fn model_values(model: &Model) -> Result<Vec<Value>> {
    Ok(vec![
        Value::Uuid(model.id.0),
        Value::Text(model.name.trim().to_owned()),
        Value::Text(model.description.clone()),
        // Derived, never separately edited — see the module docs.
        Value::Text(model.table().to_owned()),
        Value::Text(model.provider.trim().to_owned()),
        Value::Json(to_json(&model.dataset, "dataset")?),
        Value::Json(Json::Object(model.configuration.clone())),
        Value::Json(Json::Object(model.hyperparameters.clone())),
        Value::Json(to_json(&model.split, "split")?),
        Value::Json(Json::Object(model.attributes.clone())),
        // None is SQL NULL rather than `[]`, so a model that never had related
        // datasets reads the same whether it was written before the column
        // existed or after.
        if model.related.is_empty() {
            Value::Null
        } else {
            Value::Json(to_json(&model.related, "related datasets")?)
        },
    ])
}

/// Serialise one structured column, naming it if it will not go.
fn to_json<T: serde::Serialize>(value: &T, what: &str) -> Result<Json> {
    serde_json::to_value(value)
        .map_err(|e| Error::msg(format!("a model's {what} could not be stored: {e}")))
}

/// Rebuild a [`Model`] from its `_fd_models` row. The strictness note in the
/// module docs applies throughout.
fn model_from_row(row: &Row) -> Result<Model> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => ModelId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let name = text(row, COL_NAME)?;
    let at = |e: String| Error::invalid(format!("model `{name}`: {e}"));

    let dataset: Dataset = structured(row, COL_DATASET).map_err(|e| at(e.to_string()))?;
    let split: Split = structured(row, COL_SPLIT).map_err(|e| at(e.to_string()))?;
    // Strict like every other column: a NULL (or an absent column, which is a
    // table `bootstrap_models` has not yet reached) is "none", and anything that
    // is not an array of named datasets is refused by name — a posterior bound
    // against half its datasets would be sampled.
    let related: Vec<NamedDataset> = match row.get(COL_RELATED) {
        None | Some(Value::Null) | Some(Value::Json(Json::Null)) => Vec::new(),
        Some(Value::Json(Json::Array(_))) => {
            structured(row, COL_RELATED).map_err(|e| at(e.to_string()))?
        }
        Some(Value::Json(other)) => {
            return Err(at(format!(
                "{COL_RELATED} should be a json array, got {}",
                kind_of(other)
            )));
        }
        other => return Err(at(bad_column(COL_RELATED, "json", other).to_string())),
    };

    // The derived column and the dataset must agree. They cannot drift through
    // this code — `model_values` writes one from the other — so a disagreement
    // means the row was edited by something else, and reading it either way
    // would make one of the two questions ("which models are on `houses`" and
    // "what is this model fitted over") answer wrong.
    let table_name = text(row, COL_TABLE_NAME)?;
    if table_name != dataset.table {
        return Err(at(format!(
            "{MODELS_TABLE}.{COL_TABLE_NAME} is `{table_name}` but its dataset is over \
             `{}`",
            dataset.table
        )));
    }

    Ok(Model {
        id,
        name: name.clone(),
        description: optional_text(row, COL_DESCRIPTION)?,
        provider: text(row, COL_PROVIDER)?,
        dataset,
        related,
        configuration: object(row, COL_CONFIGURATION).map_err(|e| at(e.to_string()))?,
        hyperparameters: object(row, COL_HYPERPARAMETERS).map_err(|e| at(e.to_string()))?,
        split,
        attributes: object(row, COL_ATTRIBUTES).map_err(|e| at(e.to_string()))?,
    })
}

/// A required text column.
pub(crate) fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A text column whose NULL means "none given".
pub(crate) fn optional_text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
pub(crate) fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(other)) => Err(Error::invalid(format!(
            "{column} should be a json object, got {}",
            kind_of(other)
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

/// A JSON column read into the value it should be.
///
/// The error names the column and what serde said, because "the split is
/// `{\"train\": \"most\"}`" is something an admin can act on and "invalid type"
/// is not.
pub(crate) fn structured<T: serde::de::DeserializeOwned>(row: &Row, column: &str) -> Result<T> {
    match row.get(column) {
        Some(Value::Json(value)) => serde_json::from_value(value.clone())
            .map_err(|e| Error::invalid(format!("{column} is not readable: {e}"))),
        other => Err(bad_column(column, "json", other)),
    }
}

/// What a JSON value is, in a word, for an error message.
fn kind_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

pub(crate) fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Load the single model matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Model>> {
    let select = Select::from(Source::table(MODELS_TABLE)).filter(filter);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(model_from_row(row)?)),
        None => Ok(None),
    }
}

/// Run a statement that returns no rows of interest.
pub(crate) async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
pub(crate) async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
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
        let fields = model_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required && by_name(COL_NAME).unique);
        assert!(!by_name(COL_DESCRIPTION).required);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        // A model with no dataset, no provider or no split is not a model that
        // can be read at all, so all three are required *as columns* — an empty
        // configuration is `{}`, which is a real value, and NULL is not.
        for column in [COL_DATASET, COL_PROVIDER, COL_SPLIT, COL_CONFIGURATION] {
            assert!(by_name(column).required, "{column}");
        }
        // Added after installations existed, so nullable: `bootstrap_table`
        // can add it to a table with rows.
        assert!(!by_name(COL_RELATED).required);
    }

    #[test]
    fn no_related_datasets_is_sql_null_and_some_is_an_array() {
        let model = Model::new("m", "stan", Dataset::new("homes"));
        let at = model_columns()
            .iter()
            .position(|c| c == COL_RELATED)
            .unwrap();
        assert_eq!(model_values(&model).unwrap()[at], Value::Null);
        let model = model.related(NamedDataset::new("counties", Dataset::new("counties")));
        let Value::Json(Json::Array(related)) = &model_values(&model).unwrap()[at] else {
            panic!("an array");
        };
        assert_eq!(related[0]["name"], "counties");
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let model = Model::new("m", "linear_regression", Dataset::new("houses"));
        assert_eq!(model_columns().len(), model_values(&model).unwrap().len());
        let declared: Vec<String> = model_fields().iter().map(|f| f.base.name.clone()).collect();
        assert_eq!(model_columns(), declared);
    }

    #[test]
    fn the_table_column_is_written_from_the_dataset_and_not_beside_it() {
        let model = Model::new(
            "house prices",
            "linear_regression",
            Dataset::new("houses").column("price", "price"),
        );
        let values = model_values(&model).unwrap();
        assert_eq!(values[3], Value::Text("houses".to_owned()));
    }
}
