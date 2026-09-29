//! `predict_row` — a fitted model applied to the row an event is about (TODO
//! "Predictive models", §12).
//!
//! The second half of what a model is *for*. An instance is inspected (the
//! coefficients, the test statistic, the explained variance) on its own screen;
//! this is the half that applies it — a predicted price written onto a house a
//! trigger just inserted.
//!
//! ## There is no calculated field that predicts, and that is why this exists
//!
//! A calculated field is an `sc-expr` formula with two evaluators that must
//! agree, and a prediction is translatable to neither SQL nor the reified
//! evaluator. A *stored* one would have to be recomputed on every write to every
//! row the model reads, which for a model with an aggregation in its dataset is
//! every row of two tables. An action, fired by a trigger the admin wrote, puts
//! the recomputation where somebody chose it.
//!
//! ## Four properties, each deliberate
//!
//! - **It names a model, not a fit.** The model's *active* instance is what
//!   answers, which is the entire reason `active` exists (§1): the admin refits,
//!   marks the new instance active, and this action follows without being
//!   edited. Naming an `instance` directly is still allowed, for pinning a
//!   trigger to one fit while another is being tried.
//! - **The row is read through the dataset, not off the event.** The event
//!   carries the row's own columns; a dataset column may be
//!   `neighbourhoodⱵaverage_income` or `viewingsↃhouse.length`, which are the
//!   row layer's answer and not the row's. So the read is the *same* read the
//!   fit made, restricted to this row's primary key — which is also what makes a
//!   prediction reproducible from the instance.
//! - **The target is checked against what the model produces.** On save, against
//!   the provider's declaration (which is sometimes two possibilities wide, and
//!   that is honest), and at fire time against the outcome the instance actually
//!   recorded. Writing a class name into a numeric column is a failure that
//!   would otherwise surface as a coercion error with no mention of a model in
//!   it.
//! - **It writes one place: a field, or the workflow context.** Exactly one, and
//!   naming neither is refused on the form — an action that computed a
//!   prediction and put it nowhere is a trigger that appears to do nothing.

use std::sync::Arc;

use sc_action::{Action, ActionContext, ConfigCheck};
use sc_auth::ROLE_ADMIN;
use sc_catalog::{CallerContext, Catalog, Table};
use sc_error::{Error, Result};
use sc_model::{
    DatasetSource, Model, ModelInstance, ModelRegistry, Outcome, Subject, predict_subject,
};
use sc_query::Expr;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use crate::rows_scope::writable_field;
use sc_api::rows;

/// The model this action predicts with, by name.
pub const CFG_MODEL: &str = "model";
/// A particular fit to pin to, by id — empty for "the model's active instance".
pub const CFG_INSTANCE: &str = "instance";
/// The field on the event's row the prediction is written to.
pub const CFG_FIELD: &str = "field";
/// The workflow-context key the prediction is written to.
pub const CFG_CONTEXT: &str = "context_key";

/// Predict the row an event is about with a fitted model, and write the answer.
///
/// Holds the two things a prediction needs that a trigger's configuration cannot
/// name: which providers exist — the **same** registry the fit ran with, because
/// a fit made by one implementation and applied by another would be a silent
/// wrong answer — and how a dataset becomes rows, which is the seam `sc-model`
/// declares and cannot fill in (§4).
pub struct PredictRow {
    providers: Arc<ModelRegistry>,
    source: Arc<dyn DatasetSource>,
    max_rows: u64,
}

impl PredictRow {
    /// The action, predicting with `providers` over rows read through `source`.
    pub fn new(
        providers: Arc<ModelRegistry>,
        source: Arc<dyn DatasetSource>,
        max_rows: u64,
    ) -> PredictRow {
        PredictRow {
            providers,
            source,
            max_rows,
        }
    }
}

#[async_trait::async_trait]
impl Action for PredictRow {
    fn name(&self) -> &str {
        "predict_row"
    }

    fn description(&self) -> &str {
        "Predict the event's row with a fitted model, and write the answer to a field"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_MODEL, BasicType::Text)
                .label("Model")
                .server_query(sc_model::MODELS_QUERY)
                .required(),
            FormField::new(CFG_INSTANCE, BasicType::Text).label("Fit (defaults to the active one)"),
            FormField::new(CFG_FIELD, BasicType::Text).label("Write to field"),
            FormField::new(CFG_CONTEXT, BasicType::Text).label("Write to context key"),
        ]
    }

    /// The models this table has, offered as the picker's options.
    ///
    /// The declaration carries [`sc_model::MODELS_QUERY`] and this is where it
    /// would be resolved if the answer were synchronous; it is not — the models
    /// are rows — so the resolution happens where the declaration is *served*
    /// (`listActions`, which is async and has the table), exactly as a
    /// file-store option list is resolved on the way out. What this method does
    /// carry is the table, so a caller with one asks the right question.
    fn config_spec_for(&self, catalog: &Catalog, channel: Option<&str>) -> Vec<FormField> {
        let _ = (catalog, channel);
        self.config_spec()
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let name = configured(check.config, CFG_MODEL)?;
        let model = sc_model::load_model_by_name(check.catalog, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no model named `{name}`")))?;
        // A trigger on `houses` predicting with a model over `orders` would read
        // a row that is not the event's — refused here rather than at fire time,
        // where the read would simply select nothing.
        if let Some(channel) = check.channel
            && model.table() != channel
        {
            return Err(Error::invalid(format!(
                "model `{name}` is over `{}`, but this trigger fires on `{channel}`",
                model.table()
            )));
        }
        let target = target(check.config)?;
        if let Some(instance) = configured_instance(check.config)? {
            let instance = sc_model::require_model_instance(check.catalog, instance).await?;
            if instance.model != model.id {
                return Err(Error::invalid(format!(
                    "fit {} is not a fit of model `{name}`",
                    instance.id
                )));
            }
        }
        // The provider's declaration, which needs no dataset read: it says
        // whether a fit produces a number, a class, a cluster, a vector or
        // nothing at all, and only `Supervised` leaves two possibilities open.
        let provider = self.providers.require(model.provider.trim())?;
        let spec = provider.outcome_spec();
        let possible = spec.possible_prediction_types();
        if possible.is_empty() {
            let posterior = matches!(spec, sc_model::OutcomeSpec::Posterior { .. });
            return Err(Error::invalid(format!(
                "model `{name}` is {}",
                sc_model::no_per_row_prediction(posterior)
            )));
        }
        if let Target::Field(field) = &target {
            let table = check.catalog.require(model.table())?;
            writable_field(&table, field)?;
            check_target_type(&table, field, &possible)?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let name = configured(ctx.config, CFG_MODEL).map_err(named)?;
        let model = sc_model::require_model(ctx.catalog, &name)
            .await
            .map_err(named)?;
        let instance = self
            .instance(ctx.catalog, ctx.config, &model)
            .await
            .map_err(named)?;
        let table = ctx.catalog.require(model.table()).map_err(named)?;
        let pk = rows::single_pk(&table).map_err(named)?;
        let (id, key) = event_key(ctx, &table, &pk).map_err(named)?;

        // The same read the fit made, restricted to this row — so a join path
        // and an aggregation are computed by the row layer exactly as they were
        // then, rather than approximated from the columns the event carried.
        let restrict = Expr::qcol(table.name.clone(), pk.clone()).eq(Expr::Lit(key));
        let answer = predict_subject(
            &self.providers,
            self.source.as_ref(),
            &model,
            &instance,
            Subject::Dataset(Some(&restrict)),
            self.max_rows,
        )
        .await
        .map_err(named)?;
        let prediction = match answer.predictions.as_slice() {
            [one] => one.clone(),
            [] => {
                return Err(named(Error::invalid(format!(
                    "the dataset of model `{name}` does not read this row, so there is nothing \
                     to predict: it has been deleted, or the ownership rule on its table hides \
                     it"
                ))));
            }
            many => {
                return Err(named(Error::msg(format!(
                    "the dataset of model `{name}` answered {} rows for one primary key",
                    many.len()
                ))));
            }
        };
        let value = prediction.to_json().map_err(named)?;

        match target(ctx.config).map_err(named)? {
            Target::Context(key) => {
                ctx.context.insert(key.clone(), value.clone());
                Ok(json!({ "prediction": value, "context_key": key }))
            }
            Target::Field(field) => {
                // Checked against what this instance actually produces, which
                // the declaration could only narrow to two.
                check_outcome_type(&table, &field, &instance.outcome().map_err(named)?)
                    .map_err(named)?;
                let mut body = Map::with_capacity(1);
                body.insert(field.clone(), value.clone());
                // The trigger's own authority, carrying the firing chain so the
                // event this write raises knows how deep it already is — and the
                // step's transaction when there is one. Built here rather than
                // through `rows_scope::Scope`, which needs a JavaScript
                // evaluator: this action's configuration holds no formulas, and
                // an action that refused to run in a process with no engine for
                // want of one it never uses would be refusing for nothing.
                let authority = CallerContext::new(ROLE_ADMIN, ctx.event.user.clone())
                    .chained(ctx.chain.clone());
                // Through the ordinary row write path, so the target table's own
                // coercion, rich-type rules and triggers all apply — which is
                // the recursion the depth limit bounds rather than a second
                // write path that would quietly skip them.
                rows::update_row_in(
                    ctx.catalog,
                    &table,
                    &id,
                    &Json::Object(body),
                    Some(&authority),
                    &rows::Executor::of(ctx.transaction()),
                )
                .await
                .map_err(named)?;
                Ok(json!({ "prediction": value, "field": field, "id": id }))
            }
        }
    }
}

impl PredictRow {
    /// The fit that answers: the one the trigger pinned to, or the model's
    /// **active** instance — which is the whole reason `active` exists (§1).
    async fn instance(
        &self,
        catalog: &Catalog,
        config: &Attrs,
        model: &Model,
    ) -> Result<ModelInstance> {
        match configured_instance(config)? {
            Some(id) => {
                let instance = sc_model::require_model_instance(catalog, id).await?;
                if instance.model != model.id {
                    return Err(Error::invalid(format!(
                        "fit {id} is not a fit of model `{}`",
                        model.name
                    )));
                }
                Ok(instance)
            }
            None => sc_model::active_model_instance(catalog, model.id)
                .await?
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "model `{}` has no active fit, so there is nothing to predict with: fit \
                         it and mark the fit active",
                        model.name
                    ))
                }),
        }
    }
}

/// Where the prediction goes.
#[derive(Debug)]
enum Target {
    /// A field on the event's row.
    Field(String),
    /// A key in the workflow context.
    Context(String),
}

/// The configured target, refusing both and neither.
///
/// Neither is a trigger that appears to do nothing; both is two answers to
/// "where did my prediction go", and there is no reading of the configuration
/// that makes one of them the intended one.
fn target(config: &Attrs) -> Result<Target> {
    let field = optional(config, CFG_FIELD);
    let context = optional(config, CFG_CONTEXT);
    match (field, context) {
        (Some(field), None) => Ok(Target::Field(field)),
        (None, Some(context)) => Ok(Target::Context(context)),
        (Some(_), Some(_)) => Err(Error::invalid(format!(
            "give either `{CFG_FIELD}` or `{CFG_CONTEXT}`, not both: a prediction goes one place"
        ))),
        (None, None) => Err(Error::invalid(format!(
            "give a `{CFG_FIELD}` to write the prediction to, or a `{CFG_CONTEXT}` for a \
             workflow step"
        ))),
    }
}

/// A required, non-empty configuration string.
fn configured(config: &Attrs, key: &str) -> Result<String> {
    optional(config, key).ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// An optional configuration string, empty treated as absent.
fn optional(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The configured fit, when the trigger pins to one.
fn configured_instance(config: &Attrs) -> Result<Option<sc_model::InstanceId>> {
    let Some(raw) = optional(config, CFG_INSTANCE) else {
        return Ok(None);
    };
    uuid::Uuid::parse_str(&raw)
        .map(|id| Some(sc_model::InstanceId(id)))
        .map_err(|_| Error::invalid(format!("`{CFG_INSTANCE}`: `{raw}` is not a fit id")))
}

/// The event row's primary key, as the value a `WHERE` and a write both take.
fn event_key(
    ctx: &ActionContext<'_>,
    table: &Table,
    pk: &str,
) -> Result<(String, sc_query::Value)> {
    let row = ctx.event.row.as_ref().ok_or_else(|| {
        Error::invalid(
            "this event carries no row, so there is nothing to predict: `predict_row` belongs on \
             an insert, update or delete trigger",
        )
    })?;
    let json = row
        .get(pk)
        .ok_or_else(|| Error::invalid(format!("the event's row has no `{pk}`")))?;
    let value = rows::column_value(table, pk, json)?;
    Ok((render_key(json), value))
}

/// The primary key as the row layer addresses a row by.
fn render_key(json: &Json) -> String {
    match json {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The target field can hold at least one of the types this provider might
/// produce.
fn check_target_type(table: &Table, field: &str, possible: &[BasicType]) -> Result<()> {
    let Some(target) = field_type(table, field) else {
        return Ok(());
    };
    if possible.iter().any(|p| holds(&target, p)) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "`{field}` is {} and this model produces {}",
        target.name(),
        possible
            .iter()
            .map(|p| p.name().to_owned())
            .collect::<Vec<_>>()
            .join(" or ")
    )))
}

/// The target field can hold what *this fit* produces — the definitive check,
/// made where the outcome is known rather than guessed.
fn check_outcome_type(table: &Table, field: &str, outcome: &Outcome) -> Result<()> {
    let produced = outcome.prediction_type().ok_or_else(|| {
        Error::invalid(format!(
            "this fit is {}",
            sc_model::no_per_row_prediction(outcome.is_posterior())
        ))
    })?;
    let Some(target) = field_type(table, field) else {
        return Ok(());
    };
    if holds(&target, &produced) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "`{field}` is {} and this fit produces {}",
        target.name(),
        produced.name()
    )))
}

/// The field's storage type, or `None` for one this check has nothing to say
/// about.
fn field_type(table: &Table, field: &str) -> Option<BasicType> {
    table
        .field(field)
        .and_then(|f| f.base.type_.as_basic().cloned())
}

/// Whether a column of type `target` can hold a `produced` prediction.
///
/// Deliberately narrow. A number goes into a numeric column, a class **name**
/// goes into a text one, a cluster number goes into any numeric column, and a
/// vector goes into JSON and nowhere else — because rendering a vector as text
/// would make it unreadable by anything that wanted to use it.
fn holds(target: &BasicType, produced: &BasicType) -> bool {
    match produced {
        BasicType::Float => matches!(target, BasicType::Float | BasicType::Decimal),
        BasicType::Int => matches!(
            target,
            BasicType::Int | BasicType::Float | BasicType::Decimal
        ),
        BasicType::Text => matches!(target, BasicType::Text),
        BasicType::Json => matches!(target, BasicType::Json),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::OutcomeSpec;

    fn config(pairs: &[(&str, &str)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Json::from(*v)))
            .collect()
    }

    #[test]
    fn a_prediction_goes_one_place_and_naming_neither_is_the_error() {
        // An action that computed a prediction and put it nowhere is a trigger
        // that appears to do nothing, and two targets are two answers to "where
        // did it go".
        assert!(matches!(
            target(&config(&[(CFG_FIELD, "estimate")])).expect("field"),
            Target::Field(f) if f == "estimate"
        ));
        assert!(matches!(
            target(&config(&[(CFG_CONTEXT, "estimate")])).expect("context"),
            Target::Context(k) if k == "estimate"
        ));
        let err = target(&config(&[(CFG_FIELD, "a"), (CFG_CONTEXT, "b")])).expect_err("both");
        assert!(err.to_string().contains("not both"), "{err}");
        let err = target(&Attrs::new()).expect_err("neither");
        assert!(err.to_string().contains(CFG_FIELD), "{err}");
        // An empty box is not a target: a form clears a field to "" and not to
        // absent, and treating that as a choice would write nowhere silently.
        let err = target(&config(&[(CFG_FIELD, "  ")])).expect_err("blank");
        assert!(err.to_string().contains(CFG_CONTEXT), "{err}");
    }

    #[test]
    fn the_declaration_says_what_a_target_field_would_have_to_hold() {
        // Only `Supervised` leaves two open, and that is the whole reason the
        // save-time check is a *set*: a random forest is a regressor or a
        // classifier by its label's type, which no configuration decides.
        assert_eq!(
            OutcomeSpec::Supervised {
                label: "l".to_owned()
            }
            .possible_prediction_types(),
            vec![BasicType::Float, BasicType::Text]
        );
        assert_eq!(
            OutcomeSpec::Cluster.possible_prediction_types(),
            vec![BasicType::Int]
        );
        // A hypothesis test produces nothing per row, so no target is right —
        // which the action reports in those words rather than as a type clash.
        assert!(OutcomeSpec::Test.possible_prediction_types().is_empty());
        // Nor does a posterior that names no prediction — every Stan model,
        // while prediction from a posterior is carried past (Stan TODO §19).
        assert!(
            OutcomeSpec::Posterior { prediction: None }
                .possible_prediction_types()
                .is_empty()
        );
        assert_eq!(
            OutcomeSpec::Posterior {
                prediction: Some("y_new".to_owned())
            }
            .possible_prediction_types(),
            vec![BasicType::Float]
        );
    }

    #[test]
    fn a_class_name_goes_in_a_text_column_and_a_vector_only_in_json() {
        assert!(holds(&BasicType::Float, &BasicType::Float));
        assert!(holds(&BasicType::Decimal, &BasicType::Float));
        assert!(!holds(&BasicType::Int, &BasicType::Float));
        assert!(holds(&BasicType::Text, &BasicType::Text));
        assert!(!holds(&BasicType::Float, &BasicType::Text));
        // A cluster number is an integer and any numeric column takes it.
        assert!(holds(&BasicType::Int, &BasicType::Int));
        assert!(holds(&BasicType::Float, &BasicType::Int));
        // A vector rendered as text would be unreadable by anything that wanted
        // to use it.
        assert!(holds(&BasicType::Json, &BasicType::Json));
        assert!(!holds(&BasicType::Text, &BasicType::Json));
    }

    #[test]
    fn a_pinned_fit_must_be_a_fit_id_and_absent_means_the_active_one() {
        assert_eq!(configured_instance(&Attrs::new()).expect("absent"), None);
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            configured_instance(&config(&[(CFG_INSTANCE, &id.to_string())])).expect("id"),
            Some(sc_model::InstanceId(id))
        );
        let err =
            configured_instance(&config(&[(CFG_INSTANCE, "the good one")])).expect_err("not an id");
        assert!(err.to_string().contains("not a fit id"), "{err}");
    }

    #[test]
    fn the_declaration_asks_for_this_tables_models_rather_than_a_text_box() {
        let action = PredictRow::new(
            Arc::new(sc_model::ModelRegistry::new()),
            Arc::new(NoSource),
            100,
        );
        let spec = action.config_spec();
        let model = spec.iter().find(|f| f.name() == CFG_MODEL).expect("model");
        assert!(model.required);
        // The list is filled in where the declaration is served, because a
        // `config_spec_for` is synchronous and the models are rows.
        assert_eq!(model.query(), Some(sc_model::MODELS_QUERY));
        assert_eq!(
            spec.iter().map(|f| f.name().to_owned()).collect::<Vec<_>>(),
            vec![CFG_MODEL, CFG_INSTANCE, CFG_FIELD, CFG_CONTEXT]
        );
    }

    /// A seam that answers nothing — the declaration tests never read a row.
    struct NoSource;
    #[async_trait::async_trait]
    impl sc_model::DatasetSource for NoSource {
        async fn read(
            &self,
            _ds: &sc_model::Dataset,
            _how: &sc_model::Read<'_>,
        ) -> Result<sc_model::Frame> {
            unreachable!("the declaration tests read no rows")
        }
    }
}
