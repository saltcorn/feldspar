//! What an action's configuration means: the scope its formulas are read in, and
//! the values the event puts in that scope.
//!
//! Every configuration value of a built-in action that reads the event is a
//! **formula** in the same `sc-expr` language as an ownership rule, a calculated
//! field and a trigger's `only_if` (decision 7). This module is the one answer to
//! "what is in scope, what is bound, and how is a setting parsed" — shared by the
//! actions in this crate *and* by the ones that live above it because they write
//! rows (`sc-api`'s `insert_row`/`update_rows`/`delete_rows`). Two crates, one
//! answer: an action whose configuration validated on save must bind the same
//! things at fire time, wherever it is implemented.
//!
//! ## Scope
//!
//! - `user`, `row` and `old` are **ambient**: the caller and the event's rows, in
//!   scope exactly where the event has them. A `login` trigger's action gets
//!   `user` and no `row`, and naming `row` there is an error, not a null.
//! - `payload` is ambient too, and in scope for **every** kind: it is what a
//!   directly-run trigger was posted and what an error event carries, and it has
//!   no declared fields because nothing declares what a sender put in it — so
//!   `payload.x` resolves and reads null where there is none.
//! - `context` is ambient in a **workflow run** and nowhere else (§10.3): what
//!   the steps before this one left behind. A step's action is told it is one
//!   ([`ActionContext::with_run_context`]), and that is what both puts `context`
//!   in its settings' scope ([`step_shape`]) and binds it
//!   ([`EventBindings::with_context`]).
//! - **Bare identifiers are the row the formula ranges over**, which for a
//!   formula that only reads the event is *nothing*: those are validated against
//!   [`EVENT_SCOPE`], an empty table, so `title` where `row.title` was meant is
//!   refused by name at save time.
//! - The operation flags (`_insert`, …) are refused, for the reason an `only_if`
//!   refuses them: the trigger's own event *is* the operation.

use std::collections::BTreeMap;

use sc_catalog::{Catalog, Table, prefetch_bindings};
use sc_error::{Error, Result};
use sc_expr::{
    Ambient, AmbientValues, Analysis, Formula, FormulaCall, Operation, RenderMode, SchemaShape,
    TableShape, Template, value_from_json,
};
use sc_query::Value;
use sc_types::{Attrs, BasicType, TypeRef, json_to_value};
use serde_json::{Map, Value as Json};

use crate::action::ActionContext;
use crate::event::Event;
use crate::validate::trigger_shape;

/// The name a formula that ranges over **no table** is validated under.
///
/// `sc-expr` validates a formula in some table's scope, so the no-table scope is
/// a table with no fields. The name is unspellable as a real table on purpose
/// (nothing the catalog can hold collides with it) and reads as an explanation
/// where it surfaces: ``formula on `(the event)`: unknown identifier `title` `` is
/// the message an `insert_row` value gets for writing `title` where it meant
/// `row.title`.
pub const EVENT_SCOPE: &str = "(the event)";

/// The shape an action's formulas are validated and evaluated in: the catalog's
/// tables, the event's ambient objects ([`trigger_shape`] — the same function an
/// `only_if` uses, so the two scopes cannot disagree), plus [`EVENT_SCOPE`].
pub fn action_shape(catalog: &Catalog, channel: Option<&str>) -> Result<SchemaShape> {
    Ok(trigger_shape(catalog, channel)?.table(EVENT_SCOPE, TableShape::new()))
}

/// The shape the settings of an action run as a **workflow step** are validated
/// and evaluated in: [`action_shape`] plus the ambient `context` (§10.3,
/// decision 8).
///
/// The one place a step's scope is decided — `sc-workflow`'s `workflow_shape` is
/// this function, and so is [`ActionContext::shape`](crate::ActionContext::shape)
/// — because a step's own formulas (a `Set`, a branch guard, a loop's collection)
/// and the settings of the action it runs are the same language read in the same
/// place, and two functions answering "what is in scope" eventually answer
/// differently.
///
/// `context` is fieldless: nothing declares what a run has accumulated, so
/// `context.total` resolves and reads null before the step that writes it has
/// run. It is `context.x` rather than a bare `x` because bare identifiers already
/// mean "a field of the row this formula ranges over" ([`EVENT_SCOPE`]), and
/// quietly redefining them inside a workflow would make one language mean two
/// things depending on where it was written.
pub fn step_shape(catalog: &Catalog, channel: Option<&str>) -> Result<SchemaShape> {
    Ok(action_shape(catalog, channel)?.ambient_fields(Ambient::Context, None::<[String; 0]>))
}

/// A required string setting, or an error naming it.
///
/// [`validate_attrs`](sc_types::validate_attrs) has already run by the time an
/// action sees a stored configuration, so this is the belt for a row edited around
/// the API — and the message an admin gets from a half-filled form.
pub fn config_str(config: &Attrs, key: &str) -> Result<String> {
    match config.get(key) {
        Some(Json::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        _ => Err(Error::invalid(format!("the `{key}` setting is required"))),
    }
}

/// An optional formula setting, parsed. An absent or blank one is `None`.
pub fn optional_formula(config: &Attrs, key: &str) -> Result<Option<Formula>> {
    let Some(Json::String(source)) = config.get(key) else {
        return Ok(None);
    };
    if source.trim().is_empty() {
        return Ok(None);
    }
    Formula::parse(source)
        .map(Some)
        .map_err(|e| Error::invalid(format!("`{key}`: {e}")))
}

/// A required formula setting, parsed.
pub fn required_formula(config: &Attrs, key: &str) -> Result<Formula> {
    let source = config_str(config, key)?;
    Formula::parse(&source).map_err(|e| Error::invalid(format!("`{key}`: {e}")))
}

/// An optional **template** setting, parsed. An absent or blank one is `None`.
///
/// Not trimmed, unlike [`config_str`]: a template is prose, and the newline an
/// admin left at the end of an HTML body is part of what they wrote.
pub fn optional_template(config: &Attrs, key: &str) -> Result<Option<Template>> {
    let Some(Json::String(source)) = config.get(key) else {
        return Ok(None);
    };
    if source.trim().is_empty() {
        return Ok(None);
    }
    Template::parse(source)
        .map(Some)
        .map_err(|e| Error::invalid(format!("`{key}`: {e}")))
}

/// A required template setting, parsed.
pub fn required_template(config: &Attrs, key: &str) -> Result<Template> {
    optional_template(config, key)?
        .ok_or_else(|| Error::invalid(format!("the `{key}` setting is required")))
}

/// A boolean setting. Absent or null is `false`; anything that is not a boolean
/// is an error naming the setting rather than a silent `false`, for the reason
/// the timeout is refused rather than clamped — a stored `"true"` means somebody
/// wrote the configuration by hand and should be told which key is wrong.
pub fn config_flag(config: &Attrs, key: &str) -> Result<bool> {
    match config.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(flag)) => Ok(*flag),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be true or false, got {other}"
        ))),
    }
}

/// A field → formula map setting (`{"title": "row.title", "at": "user.id"}`),
/// parsed in the order the stored document gives (which is the order the admin
/// entered), with a parse failure named against the field it belongs to.
///
/// An **empty** map is refused: an `insert_row` with no values and an
/// `update_rows` with no assignments have nothing to do, and an action that
/// quietly does nothing is the failure this project refuses to ship
/// (principle 5).
pub fn formula_map(config: &Attrs, key: &str) -> Result<Vec<(String, Formula)>> {
    let Some(Json::Object(map)) = config.get(key) else {
        return Err(Error::invalid(format!(
            "`{key}` must be an object of field name → formula"
        )));
    };
    if map.is_empty() {
        return Err(Error::invalid(format!("`{key}` names no fields")));
    }
    let mut out = Vec::with_capacity(map.len());
    for (field, source) in map {
        let Json::String(source) = source else {
            return Err(Error::invalid(format!(
                "`{key}`.`{field}` must be a formula, given as a string"
            )));
        };
        let formula =
            Formula::parse(source).map_err(|e| Error::invalid(format!("`{field}`: {e}")))?;
        out.push((field.clone(), formula));
    }
    Ok(out)
}

/// The scope an action's **templates** range over: the event's table, or
/// [`EVENT_SCOPE`] where the event has no row.
///
/// Deliberately not the same rule as a configured formula's, which is always
/// [`EVENT_SCOPE`]. A template is written where a person is writing prose —
/// `Receipt for order {{ id }}`, `{{ customerⱵemail }}` — and demanding
/// `{{ row.id }}` there buys nothing: the ambiguity a bare identifier creates in
/// an `insert_row` value (is `title` the source row's or the target's?) does not
/// exist in a subject line, which has exactly one row in view. `row.id` still
/// works, and means the same thing.
pub fn template_scope(channel: Option<&str>) -> &str {
    channel.unwrap_or(EVENT_SCOPE)
}

/// Check one configured formula in the scope it will be evaluated in: every
/// identifier resolves, and none of the operation flags is used. Answers the
/// analysis, for [`ConfigCheck::formula`]'s model check.
///
/// A `predict("…")` in [`EVENT_SCOPE`] is refused here, because it can never
/// work there: it predicts the row the formula ranges over, and that scope
/// ranges over none. Whether a model exists and predicts the scope's table is
/// [`ConfigCheck::formula`]'s to ask, since models are rows.
///
/// [`ConfigCheck::formula`]: crate::ConfigCheck::formula
pub fn check_formula(
    shape: &SchemaShape,
    scope: &str,
    formula: &Formula,
    what: &str,
) -> Result<Analysis> {
    let analysis = formula
        .validate(shape, scope)
        .map_err(|e| Error::invalid(format!("{what}: {e}")))?;
    if !analysis.flags.is_empty() {
        return Err(Error::invalid(format!(
            "{what}: the operation flags (`_insert`, `_update`, …) are not available — \
             the trigger's own event is the operation"
        )));
    }
    refuse_rowless_prediction(scope, &analysis, what)?;
    Ok(analysis)
}

/// Refuse a `predict("…")` in a scope that ranges over no row.
fn refuse_rowless_prediction(scope: &str, analysis: &Analysis, what: &str) -> Result<()> {
    if scope != EVENT_SCOPE {
        return Ok(());
    }
    let Some(call) = analysis.first_model_call() else {
        return Ok(());
    };
    Err(Error::invalid(format!(
        "{what}: `{}` predicts the row a formula ranges over, and this setting ranges over \
         none (the event's row is `row`). Predict in an `update_rows` assignment or an \
         `only if` on the model's table, or in a code body with `models.get(…)`",
        call.key
    )))
}

/// Check one configured **template** in the scope it will be rendered in: every
/// token's every identifier resolves, and none of them uses an operation flag.
///
/// The template twin of [`check_formula`], and it exists for the same reason:
/// an action's configuration is validated on save *and* on load, so a subject
/// line naming a field that was dropped takes its trigger out of the live set
/// with a reason rather than failing at 3am with the send half done.
///
/// `scope` is [`template_scope`]'s answer for the trigger's channel.
pub fn check_template(
    shape: &SchemaShape,
    scope: &str,
    template: &Template,
    what: &str,
) -> Result<Vec<Analysis>> {
    let analyses = template
        .validate(shape, scope)
        .map_err(|e| Error::invalid(format!("{what}: {e}")))?;
    if analyses.iter().any(|a| !a.flags.is_empty()) {
        return Err(Error::invalid(format!(
            "{what}: the operation flags (`_insert`, `_update`, …) are not available — \
             the trigger's own event is the operation"
        )));
    }
    for analysis in &analyses {
        refuse_rowless_prediction(scope, analysis, what)?;
    }
    Ok(analyses)
}

/// The values an event puts in scope: the ambient `row`/`old` and the caller.
///
/// **Presence is scope.** Neither `row` nor `old` is in the map for an event with
/// no row, so a formula naming one fails rather than reading null; on an insert or
/// a delete `old` *is* in the map with no value — in scope and null — which is
/// what makes `old.x` there a null rather than an error. Those rules are the
/// drift-prone part, so they live here once and both crates' actions build their
/// bindings through this type.
#[derive(Debug, Clone, Default)]
pub struct EventBindings {
    /// `row`/`old`, present exactly where the event has them.
    pub ambient: AmbientValues,
    /// The caller's fields, or `None` for an anonymous event (`user === null`).
    pub user: Option<BTreeMap<String, Value>>,
}

impl EventBindings {
    /// The event's values, each read as the [`Value`] its own JSON shape implies.
    ///
    /// Enough for **reified** evaluation, which is all an action that only reads
    /// the event needs: the evaluator renders every binding back through
    /// `value_to_json`, so a typed and an untyped reading of the same JSON reach
    /// JavaScript identically. A caller that also *translates* a formula to SQL
    /// needs real column types and builds its values with [`with_values`] instead.
    ///
    /// [`with_values`]: EventBindings::with_values
    pub fn of(event: &Event) -> EventBindings {
        EventBindings::with_values(event, |_, _, json| value_from_json(json))
    }

    /// Put a **workflow run's** context in scope as `context` (§10.3, decision 8).
    ///
    /// Only the engine calls this, and only where there *is* a run: presence is
    /// scope here as it is for `row`, so an ordinary trigger's formula naming
    /// `context` is the unknown identifier it should be rather than a null that
    /// reads as "nothing has happened yet".
    ///
    /// The values are read as their own JSON shapes, because that is what a run
    /// context is: what the steps before this one returned, under no column's
    /// type.
    pub fn with_context(mut self, context: &Attrs) -> EventBindings {
        self.ambient.insert(
            Ambient::Context,
            Some(
                context
                    .iter()
                    .map(|(name, json)| (name.clone(), value_from_json(json)))
                    .collect(),
            ),
        );
        self
    }

    /// The event's values with a caller-supplied reading of each field, given the
    /// object it belongs to (`row`/`old` are the event's table, `user` the users
    /// table) — how `sc-api` types them against real columns so an inlined
    /// `user.id` can be compared to a `uuid` column in SQL.
    pub fn with_values(
        event: &Event,
        value: impl Fn(Ambient, &str, &Json) -> Value,
    ) -> EventBindings {
        let object = |ambient: Ambient, obj: &Map<String, Json>| -> BTreeMap<String, Value> {
            obj.iter()
                .map(|(name, json)| (name.clone(), value(ambient, name, json)))
                .collect()
        };
        let mut ambient = AmbientValues::new();
        // `row` is bound when the event **has** one, not when its kind is a
        // table event: a `none` trigger run against a row of its table (the row
        // button, §13.4) carries a row and must read it exactly as an `update`
        // trigger does, and a table event that somehow arrived without one must
        // say `row` is unbound rather than bind an empty object that reads as a
        // row where every field is missing.
        if event.row.is_some() {
            ambient.insert(
                Ambient::Row,
                Some(object(Ambient::Row, &event.row_object())),
            );
        }
        // `old` stays a property of the *kind*: only a table event has a
        // "before", and on an insert or a delete it is in scope and null, which
        // is what makes `old.x` there a null rather than an error.
        if event.kind.is_table_event() {
            ambient.insert(
                Ambient::Old,
                event
                    .old_row
                    .is_some()
                    .then(|| object(Ambient::Old, &event.old_row_object())),
            );
        }
        // `payload` is in scope for **every** kind, because every event has the
        // field (null where there is nothing to say) — and because the events
        // that carry one are exactly the ones with nothing else to read: a
        // directly-run trigger's posted body, an error's `{kind, message, …}`.
        //
        // Only an *object* payload binds as one. A body that is an array or a
        // scalar reads as null in a formula, which is the honest answer for a
        // scope whose whole vocabulary is `payload.x`; `run_js_code` gets the
        // payload as it is, and is the tool for one that is not an object.
        ambient.insert(
            Ambient::Payload,
            event
                .payload
                .as_object()
                .map(|obj| object(Ambient::Payload, obj)),
        );
        let user = event
            .user
            .as_ref()
            .and_then(Json::as_object)
            .map(|obj| object(Ambient::User, obj));
        EventBindings { ambient, user }
    }

    /// One evaluation request: a formula, the bare scope (`row`'s own fields for a
    /// formula that ranges over a table, empty for one that only reads the event),
    /// and these bindings.
    ///
    /// Built here so the two evaluator entry points — and the two crates' actions
    /// — cannot bind different things.
    pub fn call(
        &self,
        formula: &Formula,
        op: Operation,
        row: &BTreeMap<String, Value>,
    ) -> FormulaCall {
        FormulaCall {
            formula: formula.clone(),
            op,
            row: row.clone(),
            user: self.user.clone(),
            ambient: self.ambient.clone(),
        }
    }
}

/// One value read as the type of the **column** it belongs to, where the table
/// has one — and as its own JSON shape where it does not (a calculated field, a
/// value the column could not hold, a row from a table since dropped, which is
/// the event's problem to report rather than this conversion's).
///
/// Reified evaluation cannot tell the difference: the evaluator renders every
/// binding back through `value_to_json`. It matters for everything *around* the
/// evaluation that reaches SQL — a `where` predicate translated with `user.id`
/// inlined against a `uuid` column, and the [`prefetch`] a Ⱶ-path needs, which
/// correlates on the row's own key value. A uuid compared as text is a SQL error,
/// not a mismatch, so this is what makes those two paths work at all.
///
/// [`prefetch`]: sc_catalog::prefetch_bindings
pub fn typed_value(table: Option<&Table>, field: &str, json: &Json) -> Value {
    let Some(type_) = table
        .and_then(|t| t.field(field))
        .map(|f| &f.base.type_)
        .filter(|_| !json.is_null())
    else {
        return value_from_json(json);
    };
    json_to_value(&storage_type(type_), json).unwrap_or_else(|_| value_from_json(json))
}

/// The basic (storage) type a JSON value is coerced through: the type itself for
/// a basic field, or the SQL type a rich field sits on (a `String` stores as
/// `text`, an `Integer` as `int8`).
fn storage_type(type_: &TypeRef) -> BasicType {
    match type_.as_basic() {
        Some(basic) => basic.clone(),
        None => BasicType::from_sql_type(type_.sql_type()),
    }
}

/// Evaluate one configured formula against the event, to a JSON value.
///
/// For an action whose formulas only read the event: the bare scope is empty, so
/// the formula sees `row`/`old`/`user` and nothing else. `what` names the setting
/// being computed, so a throwing formula points at the setting it belongs to
/// rather than at the trigger as a whole.
pub async fn event_formula_value(
    ctx: &ActionContext<'_>,
    formula: &Formula,
    what: &str,
) -> Result<Json> {
    let bindings = ctx.bindings();
    let call = bindings.call(formula, Operation::Read, &BTreeMap::new());
    ctx.evaluator()?
        .eval_value(call)
        .await
        .map_err(|e| Error::invalid(format!("trigger `{}`: {what}: {e}", ctx.trigger)))
}

/// Render one configured template against the event, in `mode`.
///
/// The template twin of [`event_formula_value`], and **not** a call site of it:
/// this prefetches. A template ranges over the event's row
/// ([`template_scope`]), so `{{ customerⱵemail }}` is an ordinary Ⱶ-path, and
/// the evaluator does no I/O — the value has to be fetched first, by the same
/// [`prefetch_bindings`](sc_catalog::prefetch_bindings) a trigger's `only_if`
/// and an ownership check use. One prefetch per token over one shared map, so a
/// path two tokens both read is fetched once.
///
/// `what` names the setting being rendered, so a failure points at the subject
/// line rather than at the trigger as a whole.
pub async fn render_event_template(
    ctx: &ActionContext<'_>,
    template: &Template,
    what: &str,
    mode: RenderMode,
) -> Result<String> {
    let named = |e: Error| Error::invalid(format!("trigger `{}`: {what}: {e}", ctx.trigger));
    // A constant costs nothing: no shape, no prefetch, no isolate.
    if template.is_literal() {
        return Ok(template.source().to_owned());
    }
    // The **table** channel (see [`Event::table_channel`]): a stream event's
    // channel is a stream name, and a template on one ranges over
    // [`EVENT_SCOPE`] reading the envelope through `payload`.
    let channel = ctx.event.table_channel();
    let table = match channel {
        Some(name) => Some(ctx.catalog.require(name).map_err(named)?),
        None => None,
    };
    // The scope the *context* asks for when this is a workflow step, so
    // `{{ context.total }}` in a subject line is read the same way the `Set`
    // that wrote it was.
    let shape = ctx.shape()?;
    let analyses = template
        .validate(&shape, template_scope(channel))
        .map_err(named)?;

    // The event's objects, each field typed by its own column — what a prefetch
    // correlates on has to agree with the database (a uuid compared as text is
    // a SQL error, not a mismatch).
    let bindings = ctx.bind_values(|ambient, field, json| match ambient {
        Ambient::User | Ambient::Payload | Ambient::Context => value_from_json(json),
        Ambient::Row | Ambient::Old => typed_value(table.as_ref(), field, json),
    });
    // The bare scope is the event's row, so `{{ id }}` is the row this template
    // is about and `{{ row.id }}` is the same thing spelled the other way.
    let mut values: BTreeMap<String, Value> = bindings
        .ambient
        .get(&Ambient::Row)
        .cloned()
        .flatten()
        .unwrap_or_default();
    if let Some(table) = &table {
        for analysis in &analyses {
            prefetch_bindings(ctx.catalog, table, analysis, &shape, &mut values)
                .await
                .map_err(named)?;
        }
    }
    let evaluator = ctx.evaluator()?;
    template
        .render(mode, evaluator.as_ref(), |formula| {
            bindings.call(formula, Operation::Read, &values)
        })
        .await
        .map_err(named)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use serde_json::json;

    #[test]
    fn a_run_context_is_bound_only_when_a_run_puts_it_there() {
        let event = Event::new(EventKind::Insert)
            .on("orders")
            .row(json!({ "id": 1 }));
        // Outside a run there is no `context` in the map at all, so naming it is
        // an unknown identifier rather than a null that reads as "nothing has
        // happened yet".
        assert!(
            !EventBindings::of(&event)
                .ambient
                .contains_key(&Ambient::Context)
        );

        let context: Attrs = [
            ("total".to_owned(), json!(120)),
            ("approval".to_owned(), json!({ "approved": true })),
        ]
        .into_iter()
        .collect();
        let bindings = EventBindings::of(&event).with_context(&context);
        let bound = bindings.ambient[&Ambient::Context]
            .as_ref()
            .expect("in scope, with values");
        assert_eq!(bound.get("total"), Some(&Value::Int(120)));
        // A step's whole result travels as its own JSON shape: no column types
        // it out, because a run context has no columns behind it.
        assert_eq!(
            bound.get("approval"),
            Some(&Value::Json(json!({ "approved": true })))
        );
        // And what the event already bound is untouched.
        assert!(bindings.ambient.contains_key(&Ambient::Row));
    }

    #[test]
    fn presence_is_scope_for_the_ambient_objects() {
        // A table event: `row` bound, `old` in scope and null on an insert.
        let insert = Event::new(EventKind::Insert)
            .on("books")
            .row(json!({ "id": 1, "title": "A" }));
        let bindings = EventBindings::of(&insert);
        assert_eq!(
            bindings.ambient[&Ambient::Row].as_ref().map(BTreeMap::len),
            Some(2)
        );
        assert!(bindings.ambient.contains_key(&Ambient::Old));
        assert!(bindings.ambient[&Ambient::Old].is_none(), "in scope, null");
        // Anonymous: `user` is null rather than an empty object.
        assert!(bindings.user.is_none());

        // An update carries both rows.
        let update = insert
            .clone()
            .old_row(json!({ "id": 1, "title": "was" }))
            .caller(1, Some(json!({ "email": "a@b.c" })));
        let bindings = EventBindings::of(&update);
        assert_eq!(
            bindings.ambient[&Ambient::Old]
                .as_ref()
                .and_then(|old| old.get("title")),
            Some(&Value::Text("was".into()))
        );
        assert_eq!(
            bindings.user.as_ref().and_then(|u| u.get("email")),
            Some(&Value::Text("a@b.c".into()))
        );

        // An event with no row: neither object is in scope at all, so a formula
        // naming `row` gets `unknown identifier` instead of a silent null.
        let login = Event::new(EventKind::Login).caller(1, Some(json!({ "email": "a@b.c" })));
        let bindings = EventBindings::of(&login);
        assert!(!bindings.ambient.contains_key(&Ambient::Row));
        assert!(!bindings.ambient.contains_key(&Ambient::Old));
        // `payload` is the exception, and is in scope for every kind — see
        // `the_payload_is_in_scope_for_every_kind_and_only_as_an_object`.
        assert_eq!(bindings.ambient.len(), 1);
        assert!(bindings.user.is_some());
    }

    #[test]
    fn the_row_is_bound_when_the_event_has_one_whatever_its_kind() {
        // A `none` trigger run against a row of its table — the row button
        // (§13.4) — carries a row, and its templates read it exactly as an
        // `update` trigger's do.
        let run = Event::new(EventKind::None)
            .on("orders")
            .row(json!({ "id": 42, "total": 250 }));
        let bindings = EventBindings::of(&run);
        assert_eq!(
            bindings.ambient[&Ambient::Row]
                .as_ref()
                .and_then(|r| r.get("total")),
            Some(&Value::Int(250))
        );
        // There is no "before" for a button run, so `old` is not in scope at all
        // — naming it is the unknown-identifier error it deserves.
        assert!(!bindings.ambient.contains_key(&Ambient::Old));

        // And an event with no row binds none, whatever its kind claims.
        let bare = Event::new(EventKind::None).on("orders");
        assert!(!EventBindings::of(&bare).ambient.contains_key(&Ambient::Row));
    }

    #[test]
    fn the_payload_is_in_scope_for_every_kind_and_only_as_an_object() {
        // Every event has a payload field, so `payload` is always bound — null
        // where there is nothing to say, which is what lets an `insert_row` on a
        // `login` trigger name it without knowing which events carry one.
        let login = Event::new(EventKind::Login);
        let bindings = EventBindings::of(&login);
        assert!(bindings.ambient.contains_key(&Ambient::Payload));
        assert!(
            bindings.ambient[&Ambient::Payload].is_none(),
            "in scope, null"
        );

        // An object binds field by field — the shape a posted body and an error
        // envelope both have.
        let called = Event::new(EventKind::None).payload(json!({ "n": 5, "who": "a@b.c" }));
        let bindings = EventBindings::of(&called);
        assert_eq!(
            bindings.ambient[&Ambient::Payload]
                .as_ref()
                .and_then(|p| p.get("n")),
            Some(&Value::Int(5))
        );

        // A payload that is *not* an object has no `payload.x` to offer, so it
        // reads null rather than pretending: `run_js_code` is the tool for one.
        for body in [json!([1, 2]), json!("plain text"), json!(7)] {
            let event = Event::new(EventKind::None).payload(body.clone());
            let bindings = EventBindings::of(&event);
            assert!(
                bindings.ambient[&Ambient::Payload].is_none(),
                "bound as an object: {body}"
            );
        }
    }

    #[test]
    fn a_caller_supplied_reading_is_used_for_every_field() {
        let event = Event::new(EventKind::Insert)
            .on("books")
            .row(json!({ "id": 1 }))
            .caller(1, Some(json!({ "id": 2 })));
        // The hook is told which object each field came from, which is how a
        // caller picks the table to type it against.
        let bindings = EventBindings::with_values(&event, |ambient, name, _| {
            Value::Text(format!("{ambient}.{name}"))
        });
        assert_eq!(
            bindings.ambient[&Ambient::Row]
                .as_ref()
                .and_then(|r| r.get("id")),
            Some(&Value::Text("row.id".into()))
        );
        assert_eq!(
            bindings.user.as_ref().and_then(|u| u.get("id")),
            Some(&Value::Text("user.id".into()))
        );
    }

    #[test]
    fn the_settings_parsers_name_what_is_wrong() {
        let config: Attrs = [
            ("url".to_owned(), json!("https://x.test")),
            ("blank".to_owned(), json!("  ")),
            ("body".to_owned(), json!("row.title")),
            ("broken".to_owned(), json!("row.")),
            ("values".to_owned(), json!({ "b": "1", "a": "2" })),
        ]
        .into_iter()
        .collect();

        assert_eq!(config_str(&config, "url").unwrap(), "https://x.test");
        for key in ["blank", "missing"] {
            let msg = config_str(&config, key).unwrap_err().to_string();
            assert!(msg.contains(key) && msg.contains("required"), "{msg}");
        }
        assert!(optional_formula(&config, "body").unwrap().is_some());
        // Blank and absent are both "not configured", not an error.
        assert!(optional_formula(&config, "blank").unwrap().is_none());
        assert!(optional_formula(&config, "missing").unwrap().is_none());
        assert!(required_formula(&config, "missing").is_err());
        let msg = optional_formula(&config, "broken").unwrap_err().to_string();
        assert!(
            msg.contains("broken") && msg.contains("parse error"),
            "{msg}"
        );

        // The map keeps the document's order and names the field that fails.
        let fields: Vec<String> = formula_map(&config, "values")
            .unwrap()
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert_eq!(fields, vec!["b", "a"]);
        for (key, expected) in [("url", "object of field name"), ("missing", "object")] {
            let msg = formula_map(&config, key).unwrap_err().to_string();
            assert!(msg.contains(expected), "{msg}");
        }
    }

    #[test]
    fn the_operation_flags_are_refused_in_an_actions_formula() {
        let shape = SchemaShape::new()
            .table(EVENT_SCOPE, TableShape::new())
            .ambient_fields(Ambient::Row, Some(["title"]));
        let ok = Formula::parse("row.title").unwrap();
        check_formula(&shape, EVENT_SCOPE, &ok, "`body`").unwrap();

        let flag = Formula::parse("_insert").unwrap();
        let msg = check_formula(&shape, EVENT_SCOPE, &flag, "`body`")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`body`") && msg.contains("_insert"), "{msg}");

        // A bare identifier has nothing to resolve against in the event scope.
        let bare = Formula::parse("title").unwrap();
        let msg = check_formula(&shape, EVENT_SCOPE, &bare, "`body`")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("unknown identifier"), "{msg}");
        assert!(msg.contains(EVENT_SCOPE), "the scope is named: {msg}");
    }
}
