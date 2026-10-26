//! v1's `configFields` translated into this system's [`FormField`] (§6.2).
//!
//! A v1 action declares its settings as a list of objects:
//!
//! ```js
//! configFields: [
//!   { name: "channel", label: "Channel", type: "String", required: true },
//!   { name: "protocol", type: "String", attributes: { options: ["mqtt", "mqtts"] } },
//!   { name: "password", type: "String", fieldview: "password" },
//! ]
//! ```
//!
//! and this system declares them as [`FormField`]s — the one vocabulary every
//! configurable thing here already speaks. Translating at the boundary is what
//! lets the trigger form render a module's action with no code that knows what a
//! module is.
//!
//! **An unrecognised type is text, and is reported.** Dropping the field would
//! leave a control the admin cannot fill in for a setting the module still
//! reads; refusing the whole module for one exotic type would lose four working
//! actions to a fifth. So the field is rendered as text, the module carries an
//! issue saying which field and which type, and the admin can see both.

use sc_types::{BasicType, FormField, ShowIfCondition, TypeRef};
use serde_json::Value as Json;

/// What a translated field set could not express faithfully.
///
/// Collected rather than returned as an error, for the reason in the module
/// docs: a module with one odd field is a module with one odd field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecIssue(pub String);

/// Translate v1's `configFields` into form fields, collecting what could not be
/// expressed.
///
/// `owner` names the thing whose fields these are (`the action mqtt_publish`),
/// so an issue reads as a sentence.
///
/// A `section_header` is not a setting, but its label heads the settings after
/// it: it becomes the [`section`](FormField::section) of the next field that is
/// one. A heading with no setting after it heads nothing and is dropped.
pub fn config_fields_to_form_fields(fields: &[Json], owner: &str) -> (Vec<FormField>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let mut heading: Option<String> = None;
    for field in fields {
        if let Some(label) = section_header(field) {
            heading = Some(label);
            continue;
        }
        match config_field(field, owner) {
            Ok(Some((mut form_field, issue))) => {
                if let Some(issue) = issue {
                    issues.push(issue);
                }
                if let Some(label) = heading.take() {
                    form_field.section = Some(label);
                }
                out.push(form_field);
            }
            Ok(None) => {}
            Err(issue) => issues.push(issue),
        }
    }
    (out, issues)
}

/// v1's `input_type`s that are part of a form's presentation and hold no value:
/// a heading between groups of fields, and a hidden field the workflow itself
/// fills in. Neither is a setting, so neither is translated — or reported.
const NOT_SETTINGS: [&str; 2] = ["section_header", "hidden"];

/// A v1 `section_header`'s heading: its label, else its `sublabel`. One with
/// neither heads nothing, and reads as no heading at all.
fn section_header(field: &Json) -> Option<String> {
    if field.get("input_type").and_then(Json::as_str) != Some("section_header") {
        return None;
    }
    ["label", "sublabel"]
        .iter()
        .filter_map(|key| field.get(*key).and_then(Json::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// One v1 field. `Err` is a field that could not be translated at all (it has no
/// name); `Ok(None)` is one that is not a setting ([`NOT_SETTINGS`]); `Ok`'s
/// second half is one that was translated with a caveat.
fn config_field(field: &Json, owner: &str) -> Result<Option<(FormField, Option<String>)>, String> {
    if field
        .get("input_type")
        .and_then(Json::as_str)
        .is_some_and(|t| NOT_SETTINGS.contains(&t))
    {
        return Ok(None);
    }
    let Some(name) = field.get("name").and_then(Json::as_str) else {
        return Err(format!(
            "{owner} declares a setting with no name, which cannot be shown or stored"
        ));
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("{owner} declares a setting with an empty name"));
    }

    // A form built by a v1 `Form` carries the type *object* of a registered
    // type, where a plugin's `configFields` spells the name.
    let declared = match field.get("type") {
        Some(Json::String(t)) => t.trim().to_owned(),
        Some(Json::Object(t)) => t
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("String")
            .to_owned(),
        _ => "String".to_owned(),
    };
    let repeat = declared == "FieldRepeat" || truthy(field.get("isRepeat"));
    let (basic, issue) = match basic_type(&declared) {
        Some(basic) => (basic, None),
        // A repeated section — `FieldRepeat`, a list of groups of fields — has no
        // control of its own here, and its value is a list: edited as JSON.
        None if repeat => (
            BasicType::Json,
            Some(format!(
                "{owner}'s setting `{name}` is a repeated section, which this version edits as JSON"
            )),
        ),
        // A key's value is its target's primary key, whose type the field names
        // (`reftype`); the choices are the target's rows, in `options`.
        None if declared == "Key" => (
            match field.get("reftype").and_then(Json::as_str) {
                Some("Integer") | None => BasicType::Int,
                Some(other) => basic_type(other).unwrap_or(BasicType::Text),
            },
            None,
        ),
        None => (
            BasicType::Text,
            Some(format!(
                "{owner}'s setting `{name}` is of type `{declared}`, which this version does not \
                 know; it is shown as text"
            )),
        ),
    };

    let mut form_field = FormField::new(name, TypeRef::Basic(basic));
    form_field = form_field.label(
        field
            .get("label")
            .and_then(Json::as_str)
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(name),
    );
    if truthy(field.get("required")) {
        form_field = form_field.required();
    }
    if let Some(default) = field.get("default")
        && !default.is_null()
    {
        form_field = form_field.default_value(default.clone());
    }
    if let Some(options) = options(field) {
        form_field = form_field.options(options);
    }
    // v1 spells "this is a password" three ways depending on how old the plugin
    // is; all three mean the same thing, and this system's word for it is
    // `secret` — which is what makes it redact on the way out and merge back on
    // the way in, everywhere, rather than in whichever screen remembered.
    if is_password(field) {
        form_field = form_field.secret();
    } else if is_textarea(field) {
        form_field = form_field.multiline();
    }
    // A field restricted to the **dataset's columns**, which is what a model
    // provider's label picker is. v1 has no such thing — a `modelproviders`
    // export is this system's key, not v1's — so this is the one place where a
    // module's field declaration says something v1 could not.
    if let Some(query) = field.get("server_query").and_then(Json::as_str) {
        form_field = form_field.server_query(query);
    }
    form_field
        .show_if
        .extend(show_if_conditions(field.get("showIf")));
    // `sublabel`: v1's sentence under the control.
    if let Some(text) = field
        .get("sublabel")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        form_field = form_field.sublabel(text);
    }
    // `env_fallback`: the environment variable a blank value leaves the job to.
    // Not v1's; the admin UI shows the variable's value under the empty field.
    if let Some(name) = field
        .get("env_fallback")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        form_field = form_field.env_fallback(name);
    }
    // v1's `attributes.regexp`: what a text value must look like, and the
    // sentence `re_invalid_error` says when it does not.
    let attributes = field.get("attributes");
    if let Some(pattern) = attributes
        .and_then(|a| a.get("regexp"))
        .and_then(Json::as_str)
        .filter(|p| !p.is_empty())
    {
        let error = attributes
            .and_then(|a| a.get("re_invalid_error"))
            .and_then(Json::as_str)
            .map(str::to_string);
        form_field = form_field.pattern(pattern, error);
    }
    Ok(Some((form_field, issue)))
}

/// Read a v1-style `showIf`: when something (a setting, or an operation's
/// button) is shown.
///
/// `{ build_type: "release", own_keystore: true }` means "only while
/// `build_type` is `release` **and** `own_keystore` is ticked". A list allows
/// several values: `{ build_type: ["release", "staging"] }`. Anything that is
/// not an object means no conditions, so always shown.
pub(crate) fn show_if_conditions(declared: Option<&Json>) -> Vec<ShowIfCondition> {
    let Some(Json::Object(conditions)) = declared else {
        return Vec::new();
    };
    conditions
        .iter()
        .map(|(setting, declared)| {
            let allowed = match declared {
                Json::Array(list) => list.clone(),
                single => vec![single.clone()],
            };
            ShowIfCondition::new(setting.clone(), allowed)
        })
        .collect()
}

/// v1's type names, and what they are here.
///
/// The list is v1's built-in scalar types plus the two spellings of a JSON
/// setting. A **rich** v1 type (one a plugin defines) is not here and lands in
/// the caller's issue list as text, which is exactly what it is on the wire.
fn basic_type(declared: &str) -> Option<BasicType> {
    Some(match declared {
        "String" | "string" => BasicType::Text,
        "Integer" | "integer" | "Int" => BasicType::Int,
        "Float" | "float" | "Number" => BasicType::Float,
        "Bool" | "bool" | "Boolean" => BasicType::Bool,
        "Date" | "date" => BasicType::Date,
        "JSON" | "json" => BasicType::Json,
        // v1 renders a colour picker; the value is a `#rrggbb` string, and text
        // is what it is until this system has a colour type of its own.
        "Color" | "color" => BasicType::Text,
        _ => return None,
    })
}

/// The `attributes.options` list — or, for a v1 `Form`'s select, the field's own
/// `options` — as the stored values.
///
/// v1 writes options three ways — `["a", "b"]`, `[{name: "a", label: "A"}]` and a
/// select's `[{label: "A", value: 1}]` — and all mean the same set of stored
/// values. The label is dropped: there is nowhere for it to go today, and
/// inventing a place for it here would be a second vocabulary. An empty value is the select's "none", which the form
/// offers of its own accord, so it is not an option.
fn options(field: &Json) -> Option<Vec<Json>> {
    let options = field
        .get("attributes")
        .and_then(|a| a.get("options"))
        .filter(|o| !o.is_null())
        .or_else(|| field.get("options").filter(|o| o.is_array()))?;
    let list = match options {
        Json::Array(values) => values
            .iter()
            .filter_map(|value| match value {
                Json::String(s) => Some(Json::String(s.clone())),
                Json::Object(o) => o.get("value").or_else(|| o.get("name")).cloned(),
                other => Some(other.clone()),
            })
            .filter(|value| !value.is_null() && value.as_str() != Some(""))
            .collect::<Vec<_>>(),
        // v1 also accepts a comma-separated string.
        Json::String(s) => s
            .split(',')
            .map(|o| Json::String(o.trim().to_owned()))
            .filter(|o| o.as_str() != Some(""))
            .collect(),
        _ => return None,
    };
    (!list.is_empty()).then_some(list)
}

/// Whether v1 asked for a password input, in any of the three spellings.
fn is_password(field: &Json) -> bool {
    let says = |key: &str, value: &str| field.get(key).and_then(Json::as_str) == Some(value);
    says("fieldview", "password")
        || says("input_type", "password")
        || says("type", "password")
        || truthy(field.get("secret"))
}

/// Whether v1 asked for a text area.
fn is_textarea(field: &Json) -> bool {
    field.get("fieldview").and_then(Json::as_str) == Some("textarea")
        || field.get("input_type").and_then(Json::as_str) == Some("textarea")
}

/// JavaScript truthiness for the flags a v1 field carries — a plugin writes
/// `required: true`, and an older one writes `required: "on"`.
fn truthy(value: Option<&Json>) -> bool {
    match value {
        Some(Json::Bool(b)) => *b,
        Some(Json::String(s)) => !s.is_empty() && s != "false",
        Some(Json::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn translate(fields: Json) -> (Vec<FormField>, Vec<String>) {
        let Json::Array(fields) = fields else {
            panic!("test fields should be an array")
        };
        config_fields_to_form_fields(&fields, "the action mqtt_publish")
    }

    #[test]
    fn a_v1_string_field_becomes_a_required_text_setting() {
        let (fields, issues) = translate(json!([
            { "name": "channel", "label": "Channel", "type": "String", "required": true }
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name(), "channel");
        assert_eq!(fields[0].base.label, "Channel");
        assert_eq!(fields[0].base.type_, TypeRef::Basic(BasicType::Text));
        assert!(fields[0].required);
    }

    #[test]
    fn a_v1_show_if_becomes_the_fields_condition() {
        let (fields, issues) = translate(json!([
            { "name": "alias", "type": "String",
              "showIf": { "own_key": true, "build_type": ["release", "staging"] } }
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            fields[0].show_if,
            [
                ShowIfCondition::new("own_key", vec![json!(true)]),
                ShowIfCondition::new("build_type", vec![json!("release"), json!("staging")]),
            ]
        );
    }

    #[test]
    fn the_scalar_types_map_across() {
        let (fields, issues) = translate(json!([
            { "name": "n", "type": "Integer" },
            { "name": "f", "type": "Float" },
            { "name": "b", "type": "Bool" },
            { "name": "d", "type": "Date" },
            { "name": "j", "type": "JSON" },
            { "name": "c", "type": "Color" }
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        let types: Vec<_> = fields.iter().map(|f| f.base.type_.clone()).collect();
        assert_eq!(
            types,
            vec![
                TypeRef::Basic(BasicType::Int),
                TypeRef::Basic(BasicType::Float),
                TypeRef::Basic(BasicType::Bool),
                TypeRef::Basic(BasicType::Date),
                TypeRef::Basic(BasicType::Json),
                TypeRef::Basic(BasicType::Text),
            ]
        );
    }

    #[test]
    fn options_arrive_as_a_restricted_set_whichever_way_v1_wrote_them() {
        let (fields, _) = translate(json!([
            { "name": "protocol", "type": "String",
              "attributes": { "options": ["mqtt", "mqtts"] } },
            { "name": "mode", "type": "String",
              "attributes": { "options": [{ "name": "a", "label": "A" }] } },
            { "name": "old", "type": "String", "attributes": { "options": "x, y" } }
        ]));
        assert_eq!(fields[0].static_options(), &[json!("mqtt"), json!("mqtts")]);
        assert_eq!(fields[1].static_options(), &[json!("a")]);
        assert_eq!(fields[2].static_options(), &[json!("x"), json!("y")]);
    }

    #[test]
    fn a_password_is_a_secret_and_a_textarea_is_multiline() {
        let (fields, _) = translate(json!([
            { "name": "password", "type": "String", "fieldview": "password" },
            { "name": "key", "type": "String", "input_type": "password" },
            { "name": "ca", "type": "String", "fieldview": "textarea" }
        ]));
        assert!(fields[0].secret && !fields[0].multiline);
        assert!(fields[1].secret);
        assert!(fields[2].multiline && !fields[2].secret);
    }

    #[test]
    fn a_default_survives_and_an_unlabelled_field_is_labelled_by_its_name() {
        let (fields, _) = translate(json!([
            { "name": "port", "type": "Integer", "default": 1883 }
        ]));
        assert_eq!(fields[0].default, Some(json!(1883)));
        assert_eq!(fields[0].base.label, "port");
    }

    #[test]
    fn an_unknown_type_is_text_and_says_so() {
        let (fields, issues) = translate(json!([{ "name": "when", "type": "DateLocale" }]));
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].base.type_, TypeRef::Basic(BasicType::Text));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("DateLocale"), "{}", issues[0]);
        assert!(issues[0].contains("when"), "{}", issues[0]);
    }

    #[test]
    fn a_nameless_field_is_dropped_with_a_reason_rather_than_rendered() {
        let (fields, issues) = translate(json!([{ "label": "Mystery", "type": "String" }]));
        assert!(fields.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("no name"), "{}", issues[0]);
    }

    /// v1's `section_header` groups the settings after it: its label becomes
    /// the next setting's `section`. Two headings in a row are one group (the
    /// later wins), and a heading with nothing after it is dropped.
    #[test]
    fn a_v1_regexp_becomes_the_settings_pattern() {
        let (fields, issues) = translate(json!([
            { "name": "key_id", "type": "String",
              "attributes": { "regexp": "[A-Z0-9]{10}", "re_invalid_error": "Ten characters." } },
            { "name": "plain", "type": "String", "attributes": { "regexp": "" } }
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(fields[0].pattern.as_deref(), Some("[A-Z0-9]{10}"));
        assert_eq!(fields[0].pattern_error.as_deref(), Some("Ten characters."));
        // An empty pattern is no pattern.
        assert_eq!(fields[1].pattern, None);
    }

    #[test]
    fn a_setting_names_the_variable_a_blank_value_leaves_the_job_to() {
        let (fields, issues) = translate(json!([
            { "name": "android_home", "type": "String", "env_fallback": " ANDROID_HOME " },
            { "name": "plain", "type": "String", "env_fallback": "" }
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(fields[0].env_fallback.as_deref(), Some("ANDROID_HOME"));
        assert_eq!(fields[1].env_fallback, None);
    }

    #[test]
    fn a_section_header_heads_the_setting_after_it() {
        let (fields, issues) = translate(json!([
            { "name": "store", "type": "String" },
            { "input_type": "section_header", "label": "Superseded" },
            { "input_type": "section_header", "label": " Native apps " },
            { "name": "mobile_url", "type": "String", "sublabel": " Where a phone reaches the server. " },
            { "name": "app_id", "type": "String" },
            { "input_type": "section_header", "sublabel": "Advanced" },
            { "name": "debug", "type": "Bool" },
            { "input_type": "section_header", "label": "Nothing below" }
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        let sections: Vec<(&str, Option<&str>)> = fields
            .iter()
            .map(|f| (f.name(), f.section.as_deref()))
            .collect();
        assert_eq!(
            sections,
            [
                ("store", None),
                ("mobile_url", Some("Native apps")),
                ("app_id", None),
                ("debug", Some("Advanced"))
            ]
        );
        // A setting's own `sublabel` is its description, trimmed; the
        // heading's is not one.
        assert_eq!(
            fields[1].sublabel.as_deref(),
            Some("Where a phone reaches the server.")
        );
        assert!(fields[3].sublabel.is_none());
    }

    /// What a view pattern's configuration form is made of (TODO "Saltcorn UI"
    /// 10.1): v1's `Form` fields, which spell a few things a plugin's
    /// `configFields` do not.
    #[test]
    fn a_v1_forms_fields_translate_the_way_a_plugins_settings_do() {
        let (fields, issues) = translate(json!([
            { "input_type": "section_header", "label": "These fields were missing" },
            { "name": "stepName", "input_type": "hidden" },
            { "name": "list_width", "type": { "name": "Integer" }, "default": 6 },
            { "name": "author", "type": "Key", "reftype": "Integer", "input_type": "select",
              "options": [{ "label": "", "value": "" }, { "label": "Tolkien", "value": 1 }] },
            { "name": "view_to_create", "type": "String",
              "attributes": { "options": [{ "name": "Edit Books", "label": "Edit Books [Edit]" }] } },
            { "name": "formula_destinations", "type": "FieldRepeat", "isRepeat": true,
              "fields": [{ "name": "expression", "type": "String" }] }
        ]));
        let names: Vec<&str> = fields.iter().map(FormField::name).collect();
        assert_eq!(
            names,
            [
                "list_width",
                "author",
                "view_to_create",
                "formula_destinations"
            ]
        );
        // The heading is not a setting, but it heads the first one after it
        // (the hidden field is not a setting either), and only that one.
        assert_eq!(
            fields[0].section.as_deref(),
            Some("These fields were missing")
        );
        assert!(fields[1..].iter().all(|f| f.section.is_none()));
        assert_eq!(fields[0].base.type_, TypeRef::Basic(BasicType::Int));
        assert_eq!(fields[1].base.type_, TypeRef::Basic(BasicType::Int));
        // The select's blank is the form's own "none", not a choice.
        assert_eq!(fields[1].static_options(), &[json!(1)]);
        assert_eq!(fields[2].static_options(), &[json!("Edit Books")]);
        assert_eq!(fields[3].base.type_, TypeRef::Basic(BasicType::Json));
        // The headings are not settings and not issues; the repeat is edited as
        // JSON and says so.
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("`formula_destinations`") && issues[0].contains("JSON"));
    }
}
