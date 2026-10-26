//! [`BaseField`] and [`FormField`]: the shape of a field, and a field in a form
//! (design §6.2).
//!
//! GOALS calls out that v1 conflated database fields and form fields, so v2
//! separates them: [`BaseField`] is what they share (name, label, type,
//! type-specific attributes), `DataField` (in `sc-catalog`, because its `Key`
//! kind references catalog identifiers) adds the column constraints, and
//! [`FormField`] adds what it takes to render and validate an input control.
//!
//! **A [`FormField`] is also how every configurable extension point declares its
//! settings.** A `Framework` (§13.3), `Action` (§10.1), `Agent` (§11.1),
//! `ModelProvider` (§14.2), `FieldView` (§6.3) and a `RichType`'s attributes
//! (§6.1) all answer the same question — *what should the admin be asked?* — and
//! all answer it with `Vec<FormField>`. That is the whole point of declaring
//! settings as data: the admin UI renders a form for whichever extension the
//! admin picked without a per-extension special case, and an extension supplied
//! by a guest language through `sc-code` is configured the same way as a built-in
//! one. §6.2 already says a form field "may derive from a `DataField` **or be
//! standalone**" — a setting is exactly the standalone case, so it needs no type
//! of its own.
//!
//! The values a `FormField` describes live in an [`Attrs`] bag, are stored in a
//! JSON column, and reach the admin UI as JSON, so [`default`](FormField::default)
//! and [`options`](FormField::options) are [`serde_json::Value`]s — the same
//! thing that ends up in the bag, not [`Value`](sc_query::Value)'s tagged
//! encoding.

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::{Attrs, TypeRef};

/// Properties shared by every field: its identifier name, human label, type, and
/// type-specific attributes (design §6.2).
#[derive(Debug, Clone, PartialEq)]
pub struct BaseField {
    /// A valid identifier in SQL and every guest language.
    pub name: String,
    /// Human-facing string; defaults to the name.
    pub label: String,
    /// The field's type — basic in the MVP (rich types post-MVP).
    pub type_: TypeRef,
    /// Type-specific attributes (JSON object) — e.g. a rich type's min/max. What
    /// may go in here is itself described by that type's `attributes()`
    /// (§6.1), as a `Vec<FormField>`.
    pub attributes: Attrs,
}

impl BaseField {
    /// A base field with the given name and type; the label defaults to the name
    /// and there are no attributes.
    pub fn new(name: impl Into<String>, type_: TypeRef) -> BaseField {
        let name = name.into();
        BaseField {
            label: name.clone(),
            name,
            type_,
            attributes: Attrs::new(),
        }
    }
}

/// Where a [`FormField`]'s permitted values come from (design §6.2).
///
/// The design sketches this as *static | server query | client code*. Two of
/// those are here; the third is not, for the reason the rest of this type's
/// design follows from.
///
/// **Why a query is a name and not an expression.** The whole point of declaring
/// settings as data is that the data is inert: the admin UI renders a spec it
/// does not understand, and a spec may come from a guest language through
/// `sc-code`. An embedded query language would make a settings spec executable,
/// which is both a much larger design and a thing you would not want to evaluate
/// on behalf of a plugin. So a [`ServerQuery`](OptionsSource::ServerQuery) names
/// a source the *server* already knows how to answer, and the set of names is
/// the server's to define (see `sc_catalog::resolve_options`).
///
/// **Where a query is resolved.** The server replaces it with a
/// [`Static`](OptionsSource::Static) list before the spec reaches anyone else —
/// when it hands the spec to the admin UI, and when it validates a config
/// against it. That is what keeps the client from needing an evaluator at all,
/// and it is why `ui/form-runtime` (§12) is not a prerequisite for this.
///
/// **`ClientCode` is deliberately absent.** It is for options that depend on
/// *other values in the form* — a dependent dropdown — which by definition
/// cannot be pre-resolved server-side, and needs the form runtime to evaluate
/// per keystroke. Nothing needs it yet, and this module's convention is to leave
/// out what has no consumer rather than invent it (the same reason `fieldview`
/// and `visibility` are still missing).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum OptionsSource {
    /// Unrestricted: any value of the field's type is allowed.
    #[default]
    None,
    /// A fixed list, known when the spec is written.
    Static(Vec<Json>),
    /// A named server-side source, resolved to [`Static`](OptionsSource::Static)
    /// before the spec is used.
    ServerQuery(String),
}

/// One condition of a [`show_if`](FormField::show_if): setting `setting` must
/// hold one of the `allowed` values — `build_type` must be `"release"`.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowIfCondition {
    /// The setting the condition looks at — `build_type`.
    pub setting: String,
    /// The values it may hold for the condition to be met — `["release"]`.
    pub allowed: Vec<Json>,
}

impl ShowIfCondition {
    /// The condition that `setting` holds one of `allowed`.
    pub fn new(setting: impl Into<String>, allowed: Vec<Json>) -> ShowIfCondition {
        ShowIfCondition {
            setting: setting.into(),
            allowed,
        }
    }
}

/// A field in a form: enough to render an input control for it, and to check
/// what comes back (design §6.2).
///
/// Either derived from a `DataField` (editing a row) or **standalone** — which is
/// what a configurable extension's settings are. See the module docs for why
/// settings do not get a type of their own.
///
/// ```
/// use sc_types::{BasicType, FormField};
///
/// let spec = FormField::new("source", BasicType::Text)
///     .label("Source directory")
///     .required()
///     .default_value("web");
/// assert_eq!(spec.default, Some(serde_json::json!("web")));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct FormField {
    /// The shared field properties.
    pub base: BaseField,
    /// Whether a value must be present.
    pub required: bool,
    /// The value used when none is given. `None` means there is no default,
    /// which for a [`required`](FormField::required) field means the admin must
    /// supply one.
    pub default: Option<Json>,
    /// Where the values this field is restricted to come from (§6.2).
    ///
    /// [`OptionsSource::None`] leaves the field unrestricted; anything else makes
    /// it a choice, which the admin UI renders as a select.
    pub options_source: OptionsSource,
    /// Whether the value is many lines rather than one, so the admin UI gives it
    /// a text area instead of an input.
    ///
    /// A rendering hint rather than a type, because it changes nothing about
    /// what the value *is*: an SSH public key, a certificate, a block of notes
    /// are all `Text` and validate as `Text`. It is here because the alternative
    /// is a UI that special-cases particular settings by name — which is the
    /// coupling this whole vocabulary exists to remove.
    pub multiline: bool,
    /// Whether the value is a **secret** — an API key, a password, a token
    /// (design §11.1).
    ///
    /// A property of the *declaration*, so it reaches every consumer at once:
    /// the admin UI renders a password input, [`redact_attrs`] replaces the
    /// value with [`SECRET_SENTINEL`] wherever the record carrying it is
    /// serialised, and [`merge_secrets`] restores the stored value when a save
    /// submits that sentinel back unchanged. Doing it on the declaration is what
    /// stops a second reader — a listing endpoint added later, an export — from
    /// leaking a key its author never thought about.
    pub secret: bool,
    /// Whether the value is fixed **once the thing it configures exists**: the
    /// admin sets it when creating, and from then on it is shown but not
    /// editable.
    ///
    /// A property of the declaration for the same reason
    /// [`secret`](FormField::secret) is: it has to reach both consumers at once.
    /// The admin UI renders the control read-only on an edit, and
    /// [`preserve_create_only`] puts the stored value back over whatever a save
    /// submitted, so a client that does not honour the flag — or a hand-written
    /// request — cannot change it either.
    ///
    /// It exists because some settings *place* something rather than describe
    /// it: a git store's checkout directory names where its working tree was put,
    /// and editing that afterwards would not move the tree, it would abandon it.
    pub create_only: bool,
    /// The value is **source code**, in the language named here (`"javascript"`),
    /// so the admin UI gives it a code editor rather than a text area.
    ///
    /// The same kind of declaration [`multiline`](FormField::multiline) is, one
    /// step further: a `run_js_code` body and a block of notes are both `Text`
    /// and validate identically, but only one of them is worth syntax
    /// highlighting, bracket matching and completions. It carries the *language*
    /// rather than a bare flag because that is what an editor has to be told, and
    /// because the next code setting will not be JavaScript.
    ///
    /// It is on the declaration for the reason every other hint here is: the
    /// alternative is an admin screen that knows `run_js_code`'s `code` setting
    /// by name, which is the coupling this vocabulary exists to remove.
    pub code_language: Option<String>,
    /// When the field applies at all: every named setting resolves to one of
    /// the values listed for it — v1's `showIf`, `{ build_type: ["release"] }`.
    /// Empty means always.
    ///
    /// A field that does not apply is hidden by the admin UI and is **not
    /// required** ([`validate`](FormField::validate)): an own keystore's alias
    /// is required only when signing with an own keystore is switched on. Its
    /// value is kept rather than cleared, so switching back restores it.
    pub show_if: Vec<ShowIfCondition>,
    /// The heading of a **group of settings that starts with this one**, or
    /// `None` for a field that continues the group before it — v1's
    /// `section_header`, which a v1 form writes as a field of its own.
    ///
    /// Presentation only, like [`multiline`](FormField::multiline): it changes
    /// nothing about the value. It is on the field that opens the group, rather
    /// than a heading entry in the spec, so that a spec stays a list of
    /// settings — something every consumer can validate, store and look up by
    /// name without stepping over entries that are not settings.
    pub section: Option<String>,
    /// A sentence or two shown **under the control**, explaining the setting
    /// beyond its label: what to enter, where to find it. v1's `sublabel`.
    ///
    /// Presentation only, like [`section`](FormField::section).
    pub sublabel: Option<String>,
    /// A regular expression a non-empty text value must match in full — v1's
    /// `attributes.regexp`. Checked by [`validate_patterns`] on save, and by the
    /// admin UI as the admin types.
    pub pattern: Option<String>,
    /// What to tell the admin when [`pattern`](FormField::pattern) does not
    /// match — v1's `attributes.re_invalid_error`.
    pub pattern_error: Option<String>,
    /// The environment variable a **blank** value leaves the job to —
    /// `ANDROID_HOME` for the React Native module's SDK directory. The admin UI
    /// shows the variable's value on the server under the empty control, so a
    /// cleared setting that still takes effect from the environment says so.
    ///
    /// Presentation only, like [`sublabel`](FormField::sublabel): the module
    /// itself decides what a blank value means.
    pub env_fallback: Option<String>,
    // Post-MVP (§6.2, §6.3, §12): `fieldview: FieldViewRef` and
    // `visibility: Option<Formula>`. Both name types that do not exist yet —
    // fieldviews and formulas are out of MVP scope — so they are left out rather
    // than invented here with no consumer.
}

impl FormField {
    /// A form field of the given name and type: optional, with no default and no
    /// restricted options, and a label defaulting to the name.
    pub fn new(name: impl Into<String>, type_: impl Into<TypeRef>) -> FormField {
        FormField {
            base: BaseField::new(name, type_.into()),
            required: false,
            default: None,
            options_source: OptionsSource::None,
            multiline: false,
            secret: false,
            create_only: false,
            code_language: None,
            show_if: Vec::new(),
            section: None,
            sublabel: None,
            pattern: None,
            pattern_error: None,
            env_fallback: None,
        }
    }

    /// Require a non-empty text value to match `pattern` in full, saying
    /// `error` when it does not (see [`pattern`](FormField::pattern)).
    pub fn pattern(mut self, pattern: impl Into<String>, error: Option<String>) -> FormField {
        self.pattern = Some(pattern.into());
        self.pattern_error = error;
        self
    }

    /// Explain this field under its control (see
    /// [`sublabel`](FormField::sublabel)).
    pub fn sublabel(mut self, text: impl Into<String>) -> FormField {
        self.sublabel = Some(text.into());
        self
    }

    /// Say that a blank value leaves the job to environment variable `name`
    /// (see [`env_fallback`](FormField::env_fallback)).
    pub fn env_fallback(mut self, name: impl Into<String>) -> FormField {
        self.env_fallback = Some(name.into());
        self
    }

    /// Open a group of settings headed `heading` with this field (see
    /// [`section`](FormField::section)).
    pub fn section(mut self, heading: impl Into<String>) -> FormField {
        self.section = Some(heading.into());
        self
    }

    /// Apply this field only while setting `name` resolves to one of `values`
    /// (see [`show_if`](FormField::show_if)). Called again for another setting,
    /// every condition must hold.
    pub fn show_if(mut self, name: impl Into<String>, values: Vec<Json>) -> FormField {
        self.show_if.push(ShowIfCondition::new(name, values));
        self
    }

    /// Whether the field applies to `attrs` under `spec` (see
    /// [`show_if`](FormField::show_if)). A condition naming a setting `spec`
    /// does not have compares against nothing, and so does not hold.
    pub fn applies(&self, spec: &[FormField], attrs: &Attrs) -> bool {
        self.show_if.iter().all(|condition| {
            let current = spec
                .iter()
                .find(|f| f.base.name == condition.setting)
                .and_then(|f| f.resolve(attrs))
                .or_else(|| attrs.get(&condition.setting));
            current.is_some_and(|v| condition.allowed.iter().any(|a| same_setting(a, v)))
        })
    }

    /// The field's name — the key it occupies in an [`Attrs`] bag.
    pub fn name(&self) -> &str {
        &self.base.name
    }

    /// Render this setting as a text area: its value is many lines, not one.
    pub fn multiline(mut self) -> FormField {
        self.multiline = true;
        self
    }

    /// Render this setting as a **code editor** for the named language: its value
    /// is source, not prose.
    ///
    /// Implies [`multiline`](FormField::multiline), so a consumer that has no
    /// editor — a plain form, a terminal — still gives the value the room it
    /// needs instead of a one-line input.
    pub fn code(mut self, language: impl Into<String>) -> FormField {
        self.code_language = Some(language.into());
        self.multiline = true;
        self
    }

    /// Mark this setting a [`secret`](FormField::secret): its value is redacted
    /// where the record holding it is serialised, and a save that submits the
    /// sentinel keeps what is stored.
    pub fn secret(mut self) -> FormField {
        self.secret = true;
        self
    }

    /// Mark this setting [`create_only`](FormField::create_only): it is set when
    /// the thing is created and fixed thereafter.
    pub fn create_only(mut self) -> FormField {
        self.create_only = true;
        self
    }

    /// Set the human-facing label.
    pub fn label(mut self, label: impl Into<String>) -> FormField {
        self.base.label = label.into();
        self
    }

    /// Mark the field required.
    pub fn required(mut self) -> FormField {
        self.required = true;
        self
    }

    /// Set the default value.
    ///
    /// Named `default_value` rather than `default` so it is not confused with
    /// [`Default::default`]; the field it sets is [`default`](FormField::default).
    pub fn default_value(mut self, value: impl Into<Json>) -> FormField {
        self.default = Some(value.into());
        self
    }

    /// Restrict the field to a fixed set of values, rendered as a select.
    pub fn options(mut self, options: impl IntoIterator<Item = impl Into<Json>>) -> FormField {
        self.options_source = OptionsSource::Static(options.into_iter().map(Into::into).collect());
        self
    }

    /// Restrict the field to the values a **named server-side query** yields
    /// (§6.2) — a list that is not knowable when the spec is written, such as
    /// "the file stores that exist".
    ///
    /// The name is a key, not an expression: the server owns what each one means
    /// (see `sc_catalog::resolve_options`). Declaring settings as data only works
    /// if the data stays inert, and an embedded query language in a settings spec
    /// would be neither inert nor safe to hand to a guest-language extension.
    pub fn server_query(mut self, query: impl Into<String>) -> FormField {
        self.options_source = OptionsSource::ServerQuery(query.into());
        self
    }

    /// Replace the source with a resolved static list — what the server does to a
    /// [`ServerQuery`](OptionsSource::ServerQuery) before handing the spec to the
    /// admin UI or validating a config against it.
    pub fn with_resolved_options(
        mut self,
        options: impl IntoIterator<Item = impl Into<Json>>,
    ) -> FormField {
        self.options_source = OptionsSource::Static(options.into_iter().map(Into::into).collect());
        self
    }

    /// The values this field is restricted to **right now**: the static list, or
    /// empty for an unrestricted field or an unresolved server query.
    ///
    /// An unresolved query yields nothing on purpose. It means "the answer is not
    /// here", and the only safe reading of that is to not restrict — validating
    /// against a list you do not have would reject every value.
    pub fn static_options(&self) -> &[Json] {
        match &self.options_source {
            OptionsSource::Static(values) => values,
            _ => &[],
        }
    }

    /// The server-side query this field's options come from, if any.
    pub fn query(&self) -> Option<&str> {
        match &self.options_source {
            OptionsSource::ServerQuery(name) => Some(name),
            _ => None,
        }
    }

    /// The value to use for this field given the `attrs` actually supplied: the
    /// stored value if present, otherwise the default.
    ///
    /// Absent *and* no default yields `None` — which for a required field is what
    /// makes the config invalid.
    pub fn resolve<'a>(&'a self, attrs: &'a Attrs) -> Option<&'a Json> {
        attrs.get(&self.base.name).or(self.default.as_ref())
    }

    /// Check the value this field resolves to out of `attrs`.
    ///
    /// Three ways to fail, each an [`Error::invalid`] **naming the field**, so the
    /// message lands the admin on the control they have to fix:
    ///
    /// - required and absent (with no default to stand in),
    /// - present but of the wrong type ([`BasicType::accepts_json`]),
    /// - present but not one of the [`options`](FormField::options).
    ///
    /// A `null` counts as absent, so an optional field explicitly set to null is
    /// fine and a required one is not.
    pub fn validate(&self, attrs: &Attrs) -> Result<()> {
        let label = &self.base.name;
        let value = self.resolve(attrs).filter(|v| !v.is_null());

        let Some(value) = value else {
            if self.required {
                return Err(Error::invalid(format!("setting `{label}` is required")));
            }
            return Ok(());
        };

        let Some(basic) = self.base.type_.as_basic() else {
            // A settings field is always a basic type; a rich type reaching here
            // would validate through its own `validate` (with attributes), which
            // this per-key JSON check does not have. Not an error — just not this
            // function's job.
            return Ok(());
        };
        if !basic.accepts_json(value) {
            return Err(Error::invalid(format!(
                "setting `{label}` should be {}, got {}",
                basic.name(),
                json_kind(value)
            )));
        }

        let options = self.static_options();
        if !options.is_empty() && !options.contains(value) {
            let allowed = options
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::invalid(format!(
                "setting `{label}` should be one of {allowed}, got {value}"
            )));
        }
        Ok(())
    }
}

/// Check a whole `Attrs` bag against the spec that describes it — the check a
/// configurable extension's config goes through **on save** (§13.3), so a
/// misconfiguration is rejected where the admin can fix it rather than at build
/// or serve time.
///
/// Every field is validated, and an `attrs` key the spec does not describe is
/// itself an error: the admin UI renders exactly the spec, so an unknown setting
/// is a typo or a stale config, and silently ignoring it is how a setting the
/// admin believes they set does nothing at all.
///
/// The first failure wins. Reporting every problem at once would be friendlier,
/// but it needs an error type that can carry a list; `Error` cannot yet, and
/// inventing one here is out of proportion to a settings form.
pub fn validate_attrs(spec: &[FormField], attrs: &Attrs) -> Result<()> {
    for field in spec {
        if field.applies(spec, attrs) {
            field.validate(attrs)?;
        } else {
            // Not applying, it is not required — but what it holds is still
            // checked, since it comes back the moment the field applies again.
            let optional = FormField {
                required: false,
                ..field.clone()
            };
            optional.validate(attrs)?;
        }
    }
    validate_patterns(spec, attrs)?;
    for key in attrs.keys() {
        if !spec.iter().any(|f| &f.base.name == key) {
            let known = spec
                .iter()
                .map(|f| f.base.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::invalid(if known.is_empty() {
                format!("unknown setting `{key}`; this takes no settings")
            } else {
                format!("unknown setting `{key}`; known settings are {known}")
            }));
        }
    }
    Ok(())
}

/// Check every applying field's [`pattern`](FormField::pattern) against its
/// value in `attrs`. An empty value, a non-text value and a secret's
/// [`SECRET_SENTINEL`] are not checked, nor is a field `show_if` hides, so a
/// switched-off group of settings never blocks a save.
///
/// Separate from [`validate_attrs`] (which calls it) so that a v1 module's
/// settings, which are not held to the full spec, can still be held to it.
pub fn validate_patterns(spec: &[FormField], attrs: &Attrs) -> Result<()> {
    for field in spec {
        let Some(pattern) = field.pattern.as_deref().filter(|p| !p.is_empty()) else {
            continue;
        };
        let Some(value) = field.resolve(attrs).and_then(Json::as_str) else {
            continue;
        };
        if value.is_empty() || value == SECRET_SENTINEL || !field.applies(spec, attrs) {
            continue;
        }
        let re = regex_lite::Regex::new(&format!("^(?:{pattern})$")).map_err(|e| {
            Error::invalid(format!(
                "setting `{}`: `{pattern}` is not a valid regular expression: {e}",
                field.base.name
            ))
        })?;
        if !re.is_match(value) {
            return Err(Error::invalid(match &field.pattern_error {
                Some(message) => format!("{}: {message}", field.base.label),
                None => format!(
                    "setting `{}` must match the pattern `{pattern}`",
                    field.base.name
                ),
            }));
        }
    }
    Ok(())
}

/// What a [`secret`](FormField::secret) setting's value is replaced by when the
/// record holding it is serialised (design §11.1).
///
/// **A fixed sentinel, not a truncation.** Showing the first four characters of
/// an API key is a leak, not a courtesy: it narrows a brute force and, for keys
/// whose prefix identifies the account, identifies the account. It is also a
/// fixed string rather than one derived from the value, so two providers sharing
/// a key cannot be told apart by reading a listing.
///
/// It doubles as the *input* contract: a save that submits this value unchanged
/// means "I did not touch it", and [`merge_secrets`] restores what is stored.
pub const SECRET_SENTINEL: &str = "••••••••";

/// Replace every [`secret`](FormField::secret) value in `attrs` with
/// [`SECRET_SENTINEL`] — what a record carrying a spec-declared config does on
/// the way out.
///
/// Only keys that are *present* are redacted: an unset key stays unset, so the
/// admin UI can still tell "no key configured yet" from "a key it may not see".
/// A key the spec does not describe is left alone; `validate_attrs` is what
/// rejects those, and silently dropping one here would hide the mistake.
pub fn redact_attrs(spec: &[FormField], attrs: &Attrs) -> Attrs {
    let mut out = attrs.clone();
    for field in spec.iter().filter(|f| f.secret) {
        if let Some(value) = out.get_mut(field.name())
            && !value.is_null()
        {
            *value = Json::String(SECRET_SENTINEL.to_owned());
        }
    }
    out
}

/// Undo [`redact_attrs`] for the values the submitter did not change: wherever
/// `submitted` carries [`SECRET_SENTINEL`] for a secret field, take `stored`'s
/// value instead.
///
/// This is the other half of the contract and the reason redaction can be done
/// at all. Without it, an admin who opened a provider's form to fix a typo in
/// its name would save the mask over the key and break the provider — the
/// classic failure of masking a field the round trip writes back.
///
/// A secret submitted as the sentinel with *nothing* stored is dropped rather
/// than saved: storing the mask itself would produce a provider that
/// authenticates with `••••••••`, which is worse than an unset key because it
/// looks configured.
pub fn merge_secrets(spec: &[FormField], stored: &Attrs, submitted: &Attrs) -> Attrs {
    let mut out = submitted.clone();
    for field in spec.iter().filter(|f| f.secret) {
        if out.get(field.name()).and_then(Json::as_str) != Some(SECRET_SENTINEL) {
            continue;
        }
        match stored.get(field.name()) {
            Some(value) => {
                out.insert(field.name().to_owned(), value.clone());
            }
            None => {
                out.remove(field.name());
            }
        }
    }
    out
}

/// Put every [`create_only`](FormField::create_only) value back to what is
/// stored — what an **edit** of an existing record does to the config it was
/// handed.
///
/// The server-side half of the flag, and the half that is load-bearing: the
/// admin UI renders such a control read-only, but a form is a suggestion and a
/// request is not obliged to come from one. Doing it here means every consumer
/// of a spec-declared config gets the guarantee by declaring the field, in the
/// same place and the same shape [`merge_secrets`] gets its own.
///
/// A key absent from `stored` is *removed* from the result rather than taken
/// from `submitted`: "it was never set" is the stored state, and a save that
/// could set it would be an edit of a create-only value by another name. Keys
/// the spec does not describe are left alone — [`validate_attrs`] is what
/// rejects those.
pub fn preserve_create_only(spec: &[FormField], stored: &Attrs, submitted: &Attrs) -> Attrs {
    let mut out = submitted.clone();
    for field in spec.iter().filter(|f| f.create_only) {
        match stored.get(field.name()) {
            Some(value) => {
                out.insert(field.name().to_owned(), value.clone());
            }
            None => {
                out.remove(field.name());
            }
        }
    }
    out
}

/// A JSON value's shape, for error messages.
/// Whether a `show_if` value matches a setting's value. Compared as JSON, and
/// also as text, because a form posts a checkbox as `"true"` where a
/// declaration says `true`.
fn same_setting(allowed: &Json, value: &Json) -> bool {
    let text = |v: &Json| match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    allowed == value || text(allowed) == text(value)
}

fn json_kind(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "a list",
        Json::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BasicType;
    use serde_json::json;

    #[test]
    fn base_field_label_defaults_to_name() {
        let f = BaseField::new("title", TypeRef::Basic(BasicType::Text));
        assert_eq!(f.name, "title");
        assert_eq!(f.label, "title");
        assert!(f.attributes.is_empty());
    }

    #[test]
    fn a_new_form_field_is_optional_unrestricted_and_labelled_by_name() {
        let f = FormField::new("source", BasicType::Text);
        assert_eq!(f.name(), "source");
        assert_eq!(f.base.label, "source");
        assert_eq!(f.base.type_, TypeRef::Basic(BasicType::Text));
        assert!(!f.required);
        assert_eq!(f.default, None);
        assert!(f.static_options().is_empty());
    }

    #[test]
    fn the_setters_chain() {
        let f = FormField::new("workers", BasicType::Int)
            .label("Worker processes")
            .required()
            .default_value(4);
        assert_eq!(f.base.label, "Worker processes");
        assert!(f.required);
        assert_eq!(f.default, Some(json!(4)));
    }

    #[test]
    fn a_form_field_describes_json_not_a_tagged_value() {
        // The values it describes live in an `Attrs`, are stored as a JSON column
        // and reach the admin UI as JSON, so a default is the plain JSON that
        // ends up in the bag — not `Value`'s tagged encoding.
        assert_eq!(
            FormField::new("minify", BasicType::Bool)
                .default_value(true)
                .default,
            Some(json!(true))
        );
        assert_eq!(
            FormField::new("source", BasicType::Text)
                .default_value("web")
                .default,
            Some(json!("web"))
        );
    }

    #[test]
    fn options_make_a_field_a_choice() {
        let f = FormField::new("bundler", BasicType::Text).options(["vite", "webpack"]);
        assert_eq!(f.static_options(), [json!("vite"), json!("webpack")]);
    }

    #[test]
    fn resolve_prefers_the_supplied_value_then_the_default() {
        let f = FormField::new("source", BasicType::Text).default_value("web");
        let mut attrs = Attrs::new();

        // Absent: the default stands in.
        assert_eq!(f.resolve(&attrs), Some(&json!("web")));

        // Supplied: the admin's value wins.
        attrs.insert("source".to_owned(), json!("frontend"));
        assert_eq!(f.resolve(&attrs), Some(&json!("frontend")));

        // Absent with no default is what makes a required field invalid.
        let no_default = FormField::new("output", BasicType::Text).required();
        assert_eq!(no_default.resolve(&Attrs::new()), None);
    }

    #[test]
    fn validate_names_the_setting_it_rejects() {
        // Every message must name the field: it is what tells the admin which
        // control to go and fix.
        let required = FormField::new("store", BasicType::Text).required();
        let err = required.validate(&Attrs::new()).unwrap_err().to_string();
        assert!(err.contains("store"), "{err}");
        assert!(err.contains("required"), "{err}");

        let mut wrong_type = Attrs::new();
        wrong_type.insert("store".to_owned(), json!(42));
        let err = required.validate(&wrong_type).unwrap_err().to_string();
        assert!(err.contains("store"), "{err}");
        assert!(err.contains("text"), "{err}");
        assert!(err.contains("a number"), "{err}");
    }

    #[test]
    fn validate_accepts_what_the_spec_allows() {
        let field = FormField::new("source", BasicType::Text).required();
        let mut attrs = Attrs::new();
        attrs.insert("source".to_owned(), json!("web"));
        assert!(field.validate(&attrs).is_ok());

        // A default satisfies a required field: it is a value, just not a typed
        // one.
        let defaulted = FormField::new("source", BasicType::Text)
            .required()
            .default_value("web");
        assert!(defaulted.validate(&Attrs::new()).is_ok());

        // Optional and absent is fine; optional and explicitly null is too.
        let optional = FormField::new("client", BasicType::Text);
        assert!(optional.validate(&Attrs::new()).is_ok());
        let mut nulled = Attrs::new();
        nulled.insert("client".to_owned(), json!(null));
        assert!(optional.validate(&nulled).is_ok());

        // But null does not satisfy a required field — null is absence.
        let mut nulled_required = Attrs::new();
        nulled_required.insert("store".to_owned(), json!(null));
        assert!(
            FormField::new("store", BasicType::Text)
                .required()
                .validate(&nulled_required)
                .is_err()
        );
    }

    #[test]
    fn validate_enforces_options() {
        let field = FormField::new("bundler", BasicType::Text).options(["vite", "webpack"]);
        let mut ok = Attrs::new();
        ok.insert("bundler".to_owned(), json!("vite"));
        assert!(field.validate(&ok).is_ok());

        let mut bad = Attrs::new();
        bad.insert("bundler".to_owned(), json!("rollup"));
        let err = field.validate(&bad).unwrap_err().to_string();
        assert!(err.contains("bundler"), "{err}");
        assert!(err.contains("vite"), "should list what is allowed: {err}");
    }

    #[test]
    fn a_pattern_is_matched_in_full_where_the_field_applies() {
        let spec = [
            FormField::new("use_api", BasicType::Bool),
            FormField::new("key_id", BasicType::Text)
                .pattern("[A-Z0-9]{10}", Some("ten capitals and digits".to_owned()))
                .show_if("use_api", vec![json!(true)]),
            FormField::new("secret", BasicType::Text)
                .secret()
                .pattern("[a-z]+", None)
                .show_if("use_api", vec![json!(true)]),
        ];
        let bag = |use_api: bool, key_id: &str, secret: &str| {
            let mut attrs = Attrs::new();
            attrs.insert("use_api".to_owned(), json!(use_api));
            attrs.insert("key_id".to_owned(), json!(key_id));
            attrs.insert("secret".to_owned(), json!(secret));
            attrs
        };
        assert!(validate_attrs(&spec, &bag(true, "2X9R4HXF34", "abc")).is_ok());

        // The whole value, and the field's own sentence, under its label.
        let err = validate_patterns(&spec, &bag(true, "2X9R4HXF34x", "abc"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("key_id: ten capitals and digits"), "{err}");
        // No sentence declared: the pattern itself.
        let err = validate_attrs(&spec, &bag(true, "2X9R4HXF34", "ABC"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`secret` must match the pattern `[a-z]+`"),
            "{err}"
        );

        // Not judged: an empty value, a stored secret handed back, a hidden field.
        assert!(validate_patterns(&spec, &bag(true, "", SECRET_SENTINEL)).is_ok());
        assert!(validate_patterns(&spec, &bag(false, "nonsense", "NOPE")).is_ok());

        // A pattern that does not compile is the declaration's mistake, named.
        let broken = [FormField::new("x", BasicType::Text).pattern("(", None)];
        let mut attrs = Attrs::new();
        attrs.insert("x".to_owned(), json!("y"));
        let err = validate_patterns(&broken, &attrs).unwrap_err().to_string();
        assert!(err.contains("not a valid regular expression"), "{err}");
    }

    #[test]
    fn validate_attrs_rejects_an_unknown_setting() {
        let spec = [FormField::new("store", BasicType::Text).required()];
        let mut attrs = Attrs::new();
        attrs.insert("store".to_owned(), json!("apps"));
        assert!(validate_attrs(&spec, &attrs).is_ok());

        // A setting the spec does not describe is a typo or a stale config.
        // Ignoring it silently is how a setting the admin believes they set does
        // nothing at all.
        attrs.insert("storee".to_owned(), json!("apps"));
        let err = validate_attrs(&spec, &attrs).unwrap_err().to_string();
        assert!(err.contains("storee"), "{err}");
        assert!(
            err.contains("store"),
            "should list the known settings: {err}"
        );

        // An extension that takes no settings says so.
        let mut any = Attrs::new();
        any.insert("x".to_owned(), json!(1));
        let err = validate_attrs(&[], &any).unwrap_err().to_string();
        assert!(err.contains("takes no settings"), "{err}");
    }

    #[test]
    fn a_settings_spec_is_just_form_fields() {
        // What a `Framework::config_spec()` returns (§13.3): no `AttrSpec`, no
        // per-extension type — the same `FormField` a row editor renders.
        let spec: Vec<FormField> = vec![
            FormField::new("store", BasicType::Text).required(),
            FormField::new("source", BasicType::Text).default_value("web"),
            FormField::new("minify", BasicType::Bool).default_value(false),
        ];
        let mut config = Attrs::new();
        config.insert("store".to_owned(), json!("apps"));

        let resolved: Vec<Option<&Json>> = spec.iter().map(|f| f.resolve(&config)).collect();
        assert_eq!(
            resolved,
            [
                Some(&json!("apps")),
                Some(&json!("web")),
                Some(&json!(false))
            ]
        );
    }

    /// The spec a secret round-trip is tested against: one secret, one not.
    fn secret_spec() -> Vec<FormField> {
        vec![
            FormField::new("base_url", BasicType::Text),
            FormField::new("api_key", BasicType::Text)
                .secret()
                .required(),
        ]
    }

    #[test]
    fn secret_is_a_property_of_the_declaration() {
        let spec = secret_spec();
        assert!(!spec[0].secret);
        assert!(spec[1].secret);
        // It changes nothing about what the value *is*, so it still validates as
        // the text it is.
        let mut attrs = Attrs::new();
        attrs.insert("api_key".to_owned(), json!("sk-live-1234"));
        assert!(validate_attrs(&spec, &attrs).is_ok());
    }

    #[test]
    fn redaction_replaces_the_whole_value_and_leaves_the_rest() {
        let spec = secret_spec();
        let mut attrs = Attrs::new();
        attrs.insert("base_url".to_owned(), json!("https://api.example.com"));
        attrs.insert("api_key".to_owned(), json!("sk-live-1234"));

        let out = redact_attrs(&spec, &attrs);
        assert_eq!(out.get("api_key"), Some(&json!(SECRET_SENTINEL)));
        // Not a truncation: no part of the key survives, not even its prefix.
        let rendered = Json::Object(out.clone()).to_string();
        assert!(!rendered.contains("sk-"), "{rendered}");
        // A non-secret setting is untouched.
        assert_eq!(out.get("base_url"), Some(&json!("https://api.example.com")));
    }

    #[test]
    fn an_unset_secret_stays_unset_rather_than_becoming_the_sentinel() {
        // "No key configured" and "a key you may not see" are different states
        // and the form has to show them differently.
        let out = redact_attrs(&secret_spec(), &Attrs::new());
        assert!(out.get("api_key").is_none());
    }

    #[test]
    fn the_sentinel_round_trips_without_destroying_the_stored_key() {
        let spec = secret_spec();
        let mut stored = Attrs::new();
        stored.insert("base_url".to_owned(), json!("https://api.example.com"));
        stored.insert("api_key".to_owned(), json!("sk-live-1234"));

        // Read → edit an unrelated field → save.
        let mut submitted = redact_attrs(&spec, &stored);
        submitted.insert("base_url".to_owned(), json!("https://gateway.internal"));

        let merged = merge_secrets(&spec, &stored, &submitted);
        assert_eq!(merged.get("api_key"), Some(&json!("sk-live-1234")));
        assert_eq!(
            merged.get("base_url"),
            Some(&json!("https://gateway.internal"))
        );
    }

    #[test]
    fn a_submitted_new_secret_replaces_the_stored_one() {
        let spec = secret_spec();
        let mut stored = Attrs::new();
        stored.insert("api_key".to_owned(), json!("sk-old"));
        let mut submitted = Attrs::new();
        submitted.insert("api_key".to_owned(), json!("sk-new"));

        let merged = merge_secrets(&spec, &stored, &submitted);
        assert_eq!(merged.get("api_key"), Some(&json!("sk-new")));
    }

    #[test]
    fn the_sentinel_with_nothing_stored_is_dropped_not_saved() {
        // Otherwise the provider would authenticate with the mask, which looks
        // configured and is not.
        let spec = secret_spec();
        let mut submitted = Attrs::new();
        submitted.insert("api_key".to_owned(), json!(SECRET_SENTINEL));

        let merged = merge_secrets(&spec, &Attrs::new(), &submitted);
        assert!(merged.get("api_key").is_none());
        // And it is then caught as the missing required setting it is.
        assert!(validate_attrs(&spec, &merged).is_err());
    }

    /// A spec with one ordinary setting and one that is fixed at creation.
    fn create_only_spec() -> Vec<FormField> {
        vec![
            FormField::new("url", BasicType::Text),
            FormField::new("directory", BasicType::Text).create_only(),
        ]
    }

    #[test]
    fn create_only_is_a_property_of_the_declaration() {
        let spec = create_only_spec();
        assert!(!spec[0].create_only);
        assert!(spec[1].create_only);
        // Like `secret`, it changes nothing about what the value is, so it still
        // validates as the text it is.
        let mut attrs = Attrs::new();
        attrs.insert("directory".to_owned(), json!("/srv/checkout"));
        assert!(validate_attrs(&spec, &attrs).is_ok());
    }

    #[test]
    fn an_edit_of_a_create_only_setting_keeps_what_is_stored() {
        let spec = create_only_spec();
        let mut stored = Attrs::new();
        stored.insert("url".to_owned(), json!("git@example.com:me/app.git"));
        stored.insert("directory".to_owned(), json!("/srv/checkout"));

        // The submitted config changes both — only the editable one takes.
        let mut submitted = Attrs::new();
        submitted.insert("url".to_owned(), json!("git@example.com:me/other.git"));
        submitted.insert("directory".to_owned(), json!("/tmp/elsewhere"));

        let merged = preserve_create_only(&spec, &stored, &submitted);
        assert_eq!(merged.get("directory"), Some(&json!("/srv/checkout")));
        assert_eq!(
            merged.get("url"),
            Some(&json!("git@example.com:me/other.git"))
        );
    }

    #[test]
    fn a_create_only_setting_that_was_never_set_cannot_be_set_by_an_edit() {
        // Unset is the stored state; letting a save fill it in would be editing
        // a create-only value under another name.
        let spec = create_only_spec();
        let mut submitted = Attrs::new();
        submitted.insert("directory".to_owned(), json!("/tmp/elsewhere"));

        let merged = preserve_create_only(&spec, &Attrs::new(), &submitted);
        assert!(merged.get("directory").is_none());
    }

    #[test]
    fn preserving_create_only_settings_leaves_a_create_alone() {
        // On a create there is nothing stored, so nothing is preserved — which
        // is what makes the setting settable exactly once.
        let spec = create_only_spec();
        let mut submitted = Attrs::new();
        submitted.insert("url".to_owned(), json!("git@example.com:me/app.git"));
        assert_eq!(
            preserve_create_only(&spec, &Attrs::new(), &submitted),
            submitted
        );
    }

    #[test]
    fn a_field_that_does_not_apply_is_not_required() {
        let spec = vec![
            FormField::new("build_type", BasicType::Text).default_value("release"),
            FormField::new("own_key", BasicType::Bool)
                .default_value(false)
                .show_if("build_type", vec![json!("release")]),
            FormField::new("alias", BasicType::Text)
                .required()
                .show_if("build_type", vec![json!("release")])
                .show_if("own_key", vec![json!(true)]),
        ];
        let attrs = |pairs: &[(&str, Json)]| -> Attrs {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        // Switched off (by default): the alias is not asked for.
        assert!(!spec[2].applies(&spec, &attrs(&[])));
        validate_attrs(&spec, &attrs(&[])).unwrap();
        // Switched on, for a release build: it is.
        let on = attrs(&[("own_key", json!(true))]);
        assert!(spec[2].applies(&spec, &on));
        let msg = validate_attrs(&spec, &on).unwrap_err().to_string();
        assert!(msg.contains("alias"), "{msg}");
        // A form posts the checkbox as text; it means the same.
        assert!(spec[2].applies(&spec, &attrs(&[("own_key", json!("true"))])));
        // A debug build hides it again, whatever the checkbox says.
        let debug = attrs(&[("own_key", json!(true)), ("build_type", json!("debug"))]);
        assert!(!spec[2].applies(&spec, &debug));
        validate_attrs(&spec, &debug).unwrap();
        // A hidden value is still checked for what it holds, required or not.
        let wrong = attrs(&[("build_type", json!("debug")), ("alias", json!(5))]);
        assert!(validate_attrs(&spec, &wrong).is_err());
        let wrong = attrs(&[("build_type", json!("debug")), ("own_key", json!("maybe"))]);
        assert!(validate_attrs(&spec, &wrong).is_err());
    }
}
