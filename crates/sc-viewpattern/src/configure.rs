//! Configuring a view without the builder (TODO "Saltcorn UI" Phase 10).
//!
//! A view's configuration is v1's, and so is what edits it: the pattern's
//! `configuration_workflow`, a wizard whose every step is a form built over the
//! table and the configuration gathered before it. [`Configurer`] makes those
//! calls for the admin screen, one [`ConfigStep`] at a time, and **replays** them
//! over a configuration being saved ([`Configurer::check`]), so what the wizard's
//! forms would not accept is refused on save with the step and the field named —
//! including a configuration that never went through the wizard.
//!
//! The calls run as the **admin**: a step lists the rows a key may point at
//! (List's *Default state*), and it is the admin configuring the view who is
//! asking. The snapshot is the application's own, built by the function the
//! framework builds it with, because the worker holds one snapshot per
//! generation and it must be the same one whichever of the two sent it first.
//!
//! What is not replayed is a **builder** step: its layout is the builder's, and
//! is checked by the save it arrives through (TODO "The builder" §6). The
//! builder's own calls — its options, and the previews and lookups its canvas
//! makes (§10) — are made here too, as the admin, over the same snapshot.

use std::sync::Arc;

use sc_action::TriggerDispatcher;
use sc_api::code_host::{FileStoreHost, TableHost, schema_snapshot};
use sc_app::Application;
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Repr, Result};
use sc_expr::CodeHosts;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::framework::{application_snapshot, view_sets, view_user};
use crate::runtime::{ConfigStep, ViewContext, ViewReferences, ViewRequest, ViewRuntime};
use crate::snapshot::ViewSnapshot;
use crate::view::{Page, View};

/// The most steps a configuration is replayed through. v1's longest built-in
/// workflow (List) has five; a pattern answering more than this is refused
/// rather than walked.
const MAX_STEPS: usize = 64;

/// The view runtime's configuration calls for one application, as the admin.
pub struct Configurer<'a> {
    runtime: Arc<dyn ViewRuntime>,
    catalog: &'a Catalog,
    snapshot: ViewSnapshot,
    request: ViewRequest,
    table: TableHost<'a>,
    /// The file stores, as the admin: what a builder step's image list is read
    /// from (TODO "The builder" 5.4). The worker lists only the application's
    /// own stores, which the snapshot names.
    files: FileStoreHost<'a>,
}

impl<'a> Configurer<'a> {
    /// The configuration calls for `app`, made by `user` (the signed-in admin)
    /// through `runtime`. `triggers` is the server's dispatcher, which the
    /// snapshot reads what each of the application's triggers runs from.
    pub async fn new(
        runtime: Arc<dyn ViewRuntime>,
        catalog: &'a Catalog,
        app: &Application,
        user: Option<&User>,
        triggers: Option<&TriggerDispatcher>,
    ) -> Result<Configurer<'a>> {
        let set = view_sets().get(catalog, app.id).await?;
        // No origin: the admin is not on the application's subdomain. A view's
        // links are relative to it, and a request's own base URL reaches
        // `req.get_base_url()` directly (see the framework's `snapshot`).
        let snapshot = application_snapshot(catalog, app, &set, "", triggers).await?;
        let caller = sc_api::caller_context_at(sc_auth::ROLE_ADMIN, user);
        Ok(Configurer {
            runtime,
            catalog,
            snapshot,
            request: ViewRequest {
                method: "GET".to_owned(),
                path: "/".to_owned(),
                user: user.map(view_user),
                ..ViewRequest::default()
            },
            table: TableHost::new(catalog).caused_by(caller.role, caller.user.clone()),
            files: FileStoreHost::new(catalog).caused_by(caller.role, caller.user.as_ref()),
        })
    }

    /// The surfaces a configuration call reaches: the tables and the file
    /// stores, as the admin.
    fn hosts(&self) -> CodeHosts<'_> {
        CodeHosts {
            host: Some(&self.table),
            files: Some(&self.files),
            ..CodeHosts::default()
        }
    }

    /// One step of `pattern`'s configuration over `table`, for the view named
    /// `view` (none for a view not yet saved), with `context` gathered so far.
    pub async fn step(
        &self,
        pattern: &str,
        table: Option<&str>,
        view: Option<&str>,
        step: usize,
        context: &Json,
    ) -> Result<ConfigStep> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime
            .config_step(pattern, table, view, step, context, ctx)
            .await
    }

    /// The configuration a new view of `pattern` over `table` starts with
    /// (TODO "Saltcorn UI" 10.2).
    pub async fn initial_config(
        &self,
        pattern: &str,
        table: Option<&str>,
        view: Option<&str>,
    ) -> Result<Attrs> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime.initial_config(pattern, table, view, ctx).await
    }

    /// What in the application refers to the view named `view` (10.4).
    pub async fn references(&self, view: &str) -> Result<ViewReferences> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime.references(view, ctx).await
    }

    /// The strings a view's own configuration puts in front of a person — v1's
    /// `getStringsForI18n`, asked of the pattern (§16.1, task 4.5).
    ///
    /// This is a Saltcorn UI application's half of "what does this application
    /// say?": a code application's strings are `t()` call sites a parser finds,
    /// and these are values in a configuration only the pattern can read.
    pub async fn strings_for_i18n(&self, view: &View) -> Result<Vec<String>> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime.strings_for_i18n(view, ctx).await
    }

    /// The options v1's builder is opened with for `page` (TODO "The builder"
    /// 5.5), computed in the worker as the admin, over the same snapshot and
    /// surfaces a builder step's options are.
    pub async fn page_builder_options(&self, page: &Page) -> Result<Json> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime.page_builder_options(page, ctx).await
    }

    /// A fieldview rendered for the builder's canvas (TODO "The builder" §10).
    pub async fn field_preview(
        &self,
        table: &str,
        field: &str,
        fieldview: &str,
        body: &Json,
    ) -> Result<String> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime
            .builder_field_preview(table, field, fieldview, body, ctx)
            .await
    }

    /// A fieldview's configuration form, as v1's form JSON, for the builder.
    pub async fn fieldview_config(&self, table: &str, body: &Json) -> Result<Json> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime
            .builder_fieldview_config(table, body, ctx)
            .await
    }

    /// An embedded view rendered for the builder's canvas with `state`.
    pub async fn view_preview(&self, view: &str, state: &Json) -> Result<String> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime.builder_view_preview(view, state, ctx).await
    }

    /// An embedded page rendered for the builder's canvas.
    pub async fn page_preview(&self, page: &str) -> Result<String> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime.builder_page_preview(page, ctx).await
    }

    /// A field's distinct values, as v1's `{ success }`, for the builder's *Tabs*.
    pub async fn distinct_values(&self, table: &str, field: &str) -> Result<Json> {
        let schema = schema_snapshot(self.catalog)?;
        let ctx = ViewContext {
            snapshot: &self.snapshot,
            request: &self.request,
            hosts: self.hosts(),
            schema: Some(&schema),
        };
        self.runtime
            .builder_distinct_values(table, field, ctx)
            .await
    }

    /// Replay `view`'s configuration through its pattern's steps, refusing the
    /// first value a step's form would not accept, naming the step and the
    /// field (10.1).
    ///
    /// A pattern the runtime does not describe is not replayed: saving such a
    /// view is refused by `save_view`, by the pattern's name, which is the more
    /// useful sentence.
    pub async fn check(&self, view: &View) -> Result<()> {
        let count = match self
            .runtime
            .patterns()
            .await?
            .into_iter()
            .find(|p| p.name == view.viewpattern)
        {
            Some(pattern) => pattern.steps.len(),
            None => return Ok(()),
        };
        if count > MAX_STEPS {
            return Err(Error::invalid(format!(
                "the {} pattern declares {count} configuration steps, more than the {MAX_STEPS} \
                 a configuration is checked through",
                view.viewpattern
            )));
        }
        let context = Json::Object(view.configuration.clone());
        for index in 0..count {
            let step = self
                .step(
                    &view.viewpattern,
                    view.table_name.as_deref(),
                    Some(&view.name),
                    index,
                    &context,
                )
                .await?;
            check_step_values(&view.name, &step, &view.configuration)?;
        }
        Ok(())
    }
}

/// Check what `configuration` holds for one step against that step's form: each
/// field's value is required when the form requires it, of the field's type,
/// and one of its options when it has them. The refusal names the view, the
/// step and the field.
///
/// What is checked is what v1's wizard form would have posted, read the way v1
/// reads it, so that a configuration v1's own wizard produces is not refused:
///
/// - an empty string is no value;
/// - an unticked checkbox is no value, and is not "missing" even when required;
/// - a choice may be stored as the other scalar spelling of its option (`"3"`
///   for `3`);
/// - a field its `showIf` hides — as the browser hides it, in form order, an
///   unset condition hiding it too — posts nothing and is not checked (List's
///   *Row click action* is only asked for an `Action` row click);
/// - a shown required select left unset posts its first option, as a browser
///   does (List's *Row click event* is `Nothing` until somebody picks another);
/// - a setting the form declares more than once under different conditions
///   (List's `create_view_location`, one list of places for a link and another
///   for an embedded view) is accepted by any of its shown declarations.
///
/// A builder step and one v1 skips for this configuration are not checked, and
/// neither is anything the step's form does not ask for — another step's
/// settings are that step's to check.
pub fn check_step_values(view: &str, step: &ConfigStep, configuration: &Attrs) -> Result<()> {
    if step.skip || step.builder {
        return Ok(());
    }
    let refused = |reason: String| {
        Error::invalid(format!(
            "view `{view}` cannot be saved: its {} step says {reason}",
            step.name
        ))
    };
    let top = Attrs::new();
    let values = match &step.context_field {
        None => configuration,
        Some(key) => match configuration.get(key) {
            None | Some(Json::Null) => &top,
            Some(Json::Object(values)) => values,
            Some(other) => {
                return Err(refused(format!(
                    "its settings are kept under `{key}`, which holds {other} rather than an object"
                )));
            }
        },
    };
    let conditions = show_ifs(step);
    // What each field holds as stored, before anything is posted.
    let mut stored = Attrs::new();
    for field in &step.fields {
        if !stored.contains_key(field.name())
            && let Some(value) = read_value(field, values.get(field.name()))
        {
            stored.insert(field.name().to_owned(), value);
        }
    }
    // The form, in order: which declarations are shown, and what is posted.
    let mut posted = Attrs::new();
    let mut shown = vec![false; step.fields.len()];
    for (index, field) in step.fields.iter().enumerate() {
        let current = |key: &str| posted.get(key).or_else(|| stored.get(key)).cloned();
        if conditions[index].is_some_and(|c| hides(c, &current)) {
            continue;
        }
        shown[index] = true;
        if posted.contains_key(field.name()) {
            continue;
        }
        let options = field.static_options();
        let value = stored
            .get(field.name())
            .cloned()
            .or_else(|| (field.required && !options.is_empty()).then(|| options[0].clone()));
        if let Some(value) = value {
            posted.insert(field.name().to_owned(), value);
        }
    }
    let mut checked: Vec<&str> = Vec::new();
    for (index, field) in step.fields.iter().enumerate() {
        let name = field.name();
        if !shown[index] || checked.contains(&name) {
            continue;
        }
        checked.push(name);
        let declarations: Vec<&FormField> = step
            .fields
            .iter()
            .zip(&shown)
            .filter(|(f, shown)| **shown && f.name() == name)
            .map(|(f, _)| f)
            .collect();
        let Some(value) = posted.get(name) else {
            if declarations
                .iter()
                .any(|f| f.required && !matches!(f.base.type_.as_basic(), Some(BasicType::Bool)))
            {
                return Err(refused(format!("setting `{name}` is required")));
            }
            continue;
        };
        let mut one = Attrs::new();
        one.insert(name.to_owned(), value.clone());
        let mut first_error = None;
        for declaration in &declarations {
            match declaration.validate(&one) {
                Ok(()) => {
                    first_error = None;
                    break;
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_error {
            return Err(refused(match e.repr() {
                Repr::Invalid(message) => message.clone(),
                _ => e.to_string(),
            }));
        }
    }
    Ok(())
}

/// Each form field's `showIf`, read off the step's v1 form — this server's form
/// fields carry no conditions. A setting declared twice is paired by order: the
/// second field of a name with the second v1 field of that name.
fn show_ifs(step: &ConfigStep) -> Vec<Option<&serde_json::Map<String, Json>>> {
    let raw: Vec<&Json> = step
        .form
        .get("fields")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .collect();
    step.fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let nth = step.fields[..index]
                .iter()
                .filter(|f| f.name() == field.name())
                .count();
            raw.iter()
                .filter(|f| f.get("name").and_then(Json::as_str) == Some(field.name()))
                .nth(nth)
                .and_then(|f| f.get("showIf"))
                .and_then(Json::as_object)
        })
        .collect()
}

/// Whether a `showIf` hides its field, as the browser applies it: every named
/// field must hold a value the condition accepts — one of a list, the value
/// given, set for `true`, unset for `false` — compared with v1's loose `==`. An
/// unset field matches no value, so it hides what depends on it.
fn hides(
    condition: &serde_json::Map<String, Json>,
    current: &dyn Fn(&str) -> Option<Json>,
) -> bool {
    condition.iter().any(|(key, criteria)| {
        let value = current(key).filter(truthy);
        match (criteria, value) {
            (Json::Bool(true), value) => value.is_none(),
            (Json::Bool(false), value) => value.is_some(),
            (_, None) => true,
            (Json::Array(targets), Some(value)) => !targets.iter().any(|t| loose_eq(&value, t)),
            (criteria, Some(value)) => !loose_eq(&value, criteria),
        }
    })
}

/// JavaScript's truthiness, for a posted value.
fn truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Json::String(s) => !s.is_empty(),
        Json::Array(_) | Json::Object(_) => true,
    }
}

/// JavaScript's `==` over the scalars a form posts: equal values, or a number
/// and a boolean or numeric string that convert to the same number.
fn loose_eq(a: &Json, b: &Json) -> bool {
    fn number(value: &Json) -> Option<f64> {
        match value {
            Json::Number(n) => n.as_f64(),
            Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Json::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
    if a == b {
        return true;
    }
    if a.is_string() && b.is_string() {
        return false;
    }
    matches!((number(a), number(b)), (Some(x), Some(y)) if x == y)
}

/// A stored value as v1's form would have read it back, or `None` for no value.
fn read_value(field: &FormField, value: Option<&Json>) -> Option<Json> {
    let value = match value? {
        Json::Null => return None,
        Json::String(s) if s.is_empty() => return None,
        value => value,
    };
    let options = field.static_options();
    if !options.is_empty()
        && !options.contains(value)
        && let Some(same) = options
            .iter()
            .find(|o| scalar_text(o).is_some() && scalar_text(o) == scalar_text(value))
    {
        return Some(same.clone());
    }
    // A text control posts text, whatever it looks like; v1 kept a number typed
    // into one as the number.
    if matches!(field.base.type_.as_basic(), Some(BasicType::Text))
        && let Some(text) = scalar_text(value).filter(|_| !value.is_string())
    {
        return Some(Json::String(text));
    }
    Some(value.clone())
}

/// A scalar's text, as a form posts it; `None` for an array or an object.
fn scalar_text(value: &Json) -> Option<String> {
    match value {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(context_field: Option<&str>, fields: Vec<FormField>) -> ConfigStep {
        ConfigStep {
            name: "Views".to_owned(),
            count: 2,
            builder: false,
            builder_options: None,
            skip: false,
            context_field: context_field.map(str::to_owned),
            blurb: None,
            fields,
            values: Attrs::new(),
            issues: Vec::new(),
            form: Json::Null,
        }
    }

    fn attrs(value: Json) -> Attrs {
        value.as_object().cloned().unwrap_or_default()
    }

    /// ListShowList's first step, as `config_fields_to_form_fields` renders it.
    fn views_step() -> ConfigStep {
        step(
            None,
            vec![
                FormField::new("list_view", BasicType::Text)
                    .options(vec![json!("List Books"), json!("List Authors")]),
                FormField::new("list_width", BasicType::Int).default_value(json!(6)),
                FormField::new("descending", BasicType::Bool).required(),
            ],
        )
    }

    #[test]
    fn a_configuration_the_steps_form_accepts_is_accepted() {
        let configuration = attrs(json!({
            "list_view": "List Books",
            "list_width": 4,
            // Another step's settings are not this one's to refuse.
            "subtables": { "ChildList:Show Books.books.author": true },
        }));
        check_step_values("Books", &views_step(), &configuration).unwrap();
        // Nothing at all: the optional select is unset, the width defaults, and
        // an unticked checkbox is `false` rather than missing.
        check_step_values("Books", &views_step(), &Attrs::new()).unwrap();
    }

    #[test]
    fn a_refusal_names_the_view_the_step_and_the_field() {
        let err = check_step_values(
            "Books",
            &views_step(),
            &attrs(json!({ "list_width": "wide" })),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("view `Books`"), "{err}");
        assert!(err.contains("Views step"), "{err}");
        assert!(err.contains("`list_width`"), "{err}");

        let err = check_step_values(
            "Books",
            &views_step(),
            &attrs(json!({ "list_view": "Show Books" })),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`list_view`") && err.contains("List Authors"),
            "{err}"
        );
    }

    #[test]
    fn what_v1_would_have_read_off_a_form_is_read_the_same_way() {
        let step = step(
            None,
            vec![
                FormField::new("author", BasicType::Int).options(vec![json!(1), json!(2)]),
                FormField::new("title", BasicType::Text).required(),
            ],
        );
        // A key's choice stored as text, and a number typed into a text field.
        check_step_values(
            "Books",
            &step,
            &attrs(json!({ "author": "2", "title": 1984 })),
        )
        .unwrap();
        // An empty string is no value, so a required field left empty is refused.
        let err = check_step_values("Books", &step, &attrs(json!({ "title": "" })))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`title` is required"), "{err}");
    }

    #[test]
    fn a_steps_context_field_is_where_its_values_are_read() {
        let options = step(
            Some("default_state"),
            vec![FormField::new("_descending", BasicType::Bool)],
        );
        check_step_values(
            "Books",
            &options,
            &attrs(json!({ "default_state": { "_descending": true }, "_descending": "x" })),
        )
        .unwrap();
        let err = check_step_values(
            "Books",
            &options,
            &attrs(json!({ "default_state": { "_descending": "yes" } })),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`_descending`"), "{err}");
        let err = check_step_values("Books", &options, &attrs(json!({ "default_state": [1] })))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`default_state`"), "{err}");
    }

    /// List's *Options*, as v1 saves it without a row click: the required
    /// select posts its first option, and the action it would ask for is not
    /// shown, so not required.
    #[test]
    fn a_required_select_posts_its_first_option_and_a_field_it_hides_is_not_checked() {
        let mut options = step(
            Some("default_state"),
            vec![
                FormField::new("_row_click_type", BasicType::Text)
                    .required()
                    .options(vec![json!("Nothing"), json!("Action")]),
                FormField::new("_row_click_action", BasicType::Text).required(),
            ],
        );
        options.form = json!({ "fields": [
            { "name": "_row_click_type" },
            { "name": "_row_click_action", "showIf": { "_row_click_type": "Action" } },
        ]});
        check_step_values("Books", &options, &Attrs::new()).unwrap();
        let err = check_step_values(
            "Books",
            &options,
            &attrs(json!({ "default_state": { "_row_click_type": "Action" } })),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`_row_click_action` is required"), "{err}");
        check_step_values(
            "Books",
            &options,
            &attrs(json!({ "default_state": { "_row_click_type": "Action", "_row_click_action": "TrimPages" } })),
        )
        .unwrap();
        // v1's `==`: a posted `"1"` shows a field conditioned on `1`.
        assert!(loose_eq(&json!("1"), &json!(1)) && !loose_eq(&json!("a"), &json!("b")));
    }

    /// List's *Create new row*: one setting declared twice, for a link and for an
    /// embedded view, each under its own condition — and none of it asked for
    /// until a view to create is chosen.
    #[test]
    fn a_setting_declared_twice_is_checked_against_the_declaration_that_is_shown() {
        let places = |options: &[&str]| {
            FormField::new("create_view_location", BasicType::Text)
                .required()
                .options(options.iter().map(|o| json!(o)).collect::<Vec<Json>>())
        };
        let mut create = step(
            None,
            vec![
                FormField::new("view_to_create", BasicType::Text)
                    .options(vec![json!("Edit Books")]),
                FormField::new("create_view_display", BasicType::Text)
                    .required()
                    .options(vec![json!("Link"), json!("Embedded"), json!("Popup")]),
                places(&["Bottom left", "Top left"]),
                places(&["Bottom", "Top"]),
            ],
        );
        let chosen = json!(["Edit Books"]);
        create.form = json!({ "fields": [
            { "name": "view_to_create" },
            { "name": "create_view_display", "showIf": { "view_to_create": chosen } },
            { "name": "create_view_location",
              "showIf": { "create_view_display": ["Link", "Popup"], "view_to_create": chosen } },
            { "name": "create_view_location",
              "showIf": { "create_view_display": ["Embedded"], "view_to_create": chosen } },
        ]});
        // Nothing chosen: nothing below is asked, whatever it holds.
        check_step_values(
            "Books",
            &create,
            &attrs(json!({ "create_view_location": "Bottom" })),
        )
        .unwrap();
        // Embedded, at the bottom.
        let embedded = json!({
            "view_to_create": "Edit Books",
            "create_view_display": "Embedded",
            "create_view_location": "Bottom",
        });
        check_step_values("Books", &create, &attrs(embedded)).unwrap();
        // A link cannot be "Bottom".
        let err = check_step_values(
            "Books",
            &create,
            &attrs(json!({
                "view_to_create": "Edit Books",
                "create_view_display": "Link",
                "create_view_location": "Bottom",
            })),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`create_view_location`") && err.contains("Bottom left"),
            "{err}"
        );
        // A view chosen and nothing else: the display is a Link, placed bottom left.
        check_step_values(
            "Books",
            &create,
            &attrs(json!({ "view_to_create": "Edit Books" })),
        )
        .unwrap();
    }

    #[test]
    fn a_builder_step_and_a_skipped_one_are_not_checked() {
        let mut skipped = views_step();
        skipped.skip = true;
        let wrong = attrs(json!({ "list_width": "wide" }));
        check_step_values("Books", &skipped, &wrong).unwrap();
        let mut builder = views_step();
        builder.builder = true;
        check_step_values("Books", &builder, &wrong).unwrap();
    }
}
