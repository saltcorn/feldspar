//! Translating an application (§16.1, §13.6): `describe_translations` and
//! `save_translations`. Which locales an application serves is a section of
//! `update_application`, since it is part of the record.
//!
//! An agent that can edit an application should also be able to translate it:
//! "translate this app into German", or "the translation of 'Save' is wrong, it
//! should be …". The agent asking **is** the translator — it has the model, and
//! it has the conversation the person corrected it in — so there is no tool
//! here that calls the server's own LLM. What the agent cannot do with a file
//! editor is everything around the words: find the strings a project or a
//! Saltcorn UI view actually uses, know which locales the application serves,
//! write a catalogue to the place it lives (a file in the app's store, or an
//! `_fd_translations` row), check the placeholders, and make the running app
//! serve the change. Those are these two tools, and the `locales` section of
//! `update_application`.
//!
//! ## Through the admin's own handlers
//!
//! Exactly as `update_application` does, each tool runs the handler behind the
//! Translations screen — `getTranslations` and `saveTranslations` — so a save
//! is checked by the same placeholder rule, keeps orphans the same way and
//! drops the mount's cached catalogue the same way.
//! Those handlers are the server's (the extraction needs the file stores and the
//! view runtime), so a context with no server refuses rather than half-works.
//!
//! ## A save merges
//!
//! `saveTranslations` replaces a locale's catalogue, because the screen always
//! sends the whole grid. An agent fixing one word sends one word, so
//! `save_translations` reads the catalogue first and lays the agent's messages
//! over it: a key not named is kept, a key named with `null` is removed.
//!
//! ## The shared catalogue
//!
//! `scope: "instance"` points both tools at the instance catalogue (TODO F6):
//! strings every application serves under its own, kept once. Its keys are
//! the ones it holds, so a save may add any key; its locales are the
//! installation's enabled languages. On an application, a key the shared
//! catalogue covers is listed with its `inherited` translation and is not
//! missing.

use std::sync::Arc;

use sc_api::mcp::{
    AdminTool, Area, ToolContext, arguments, optional_bool, optional_string, require_grant,
};
use sc_api::schema_edit::{GRANT_EDIT, Grants};
use sc_catalog::{AdminCall, AdminHost, Catalog};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use super::{ARG_APPLICATION, TOOL_UPDATE_APP, load_app};
use crate::Application;

/// Reads an application's strings, its locales and what each has translated.
pub const TOOL_DESCRIBE_TRANSLATIONS: &str = "describe_translations";
/// Writes translations into one locale's catalogue.
pub const TOOL_SAVE_TRANSLATIONS: &str = "save_translations";

const ARG_LOCALE: &str = "locale";
const ARG_SCOPE: &str = "scope";
const SCOPE_INSTANCE: &str = "instance";
const SCOPE_APPLICATION: &str = "application";
const ARG_MISSING_ONLY: &str = "missing_only";
const ARG_MESSAGES: &str = "messages";

/// How many unwrapped literals one description lists. The count is always
/// given; past this many the list is a work queue the agent should take in
/// passes rather than a payload it has to hold at once.
const MAX_UNWRAPPED: usize = 200;

/// The two tools.
pub fn translate_tools() -> Vec<Arc<dyn AdminTool>> {
    vec![Arc::new(DescribeTranslations), Arc::new(SaveTranslations)]
}

struct DescribeTranslations;

#[async_trait::async_trait]
impl AdminTool for DescribeTranslations {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE_TRANSLATIONS
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        format!(
            "Describe an application's translations: every user-visible string its \
             source wraps in `t()`/`tc()`/`<T>` (or, for a Saltcorn UI application, \
             every string its views show), the locales it serves, each locale's \
             coverage, and the translation each string has. Also lists the call \
             sites whose message is not a string literal (`problems`) and the \
             literals nothing wraps (`unwrapped`) — those can never be translated \
             until the source wraps them.\n\n\
             Give `{ARG_LOCALE}` to see one locale's translations (with \
             `{ARG_MISSING_ONLY}` for just the untranslated ones, which is what to \
             translate next). A message key is the **English source text**; a key \
             with a context is shown with its `context`. Call this before \
             `{TOOL_SAVE_TRANSLATIONS}`, and again after it to confirm coverage.\n\n\
             Strings every application says (\"Sign in\", \"Password\", \"Cancel\") \
             belong in the **shared catalogue**: `{ARG_SCOPE}: \"{SCOPE_INSTANCE}\"` \
             (no `{ARG_APPLICATION}`) describes it. Every application serves it under \
             its own translations, so a key an application lists with an \
             `inherited` translation is already translated there and is not missing; \
             translating it for the application overrides the shared one."
        )
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_SCOPE: scope_parameter(),
                ARG_APPLICATION: {
                    "type": "string",
                    "description": "The application, by subdomain. Required unless `scope` is `instance`.",
                },
                ARG_LOCALE: {
                    "type": "string",
                    "description":
                        "A locale tag (`de`, `pt-BR`): show only this locale's \
                         translation of each message. Omit for every locale.",
                },
                ARG_MISSING_ONLY: {
                    "type": "boolean",
                    "description":
                        "With `locale`: list only the messages it has no translation for.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(
            args,
            &[ARG_SCOPE, ARG_APPLICATION, ARG_LOCALE, ARG_MISSING_ONLY],
        )?;
        let target = target(ctx, &args).await?;
        let locale = locale_arg(&args)?;
        let missing_only = optional_bool(&args, ARG_MISSING_ONLY)?.unwrap_or(false);
        if missing_only && locale.is_none() {
            return Err(Error::invalid(format!(
                "`{ARG_MISSING_ONLY}` needs `{ARG_LOCALE}`: missing is per locale"
            )));
        }
        let screen = get_translations(ctx, &target).await?;
        Ok(describe(&target, screen, locale.as_deref(), missing_only))
    }
}

/// Which catalogue a call is about: one application's, or the instance's.
enum Target {
    Application(Box<Application>),
    Instance,
}

/// The `scope` parameter, as both tools declare it.
fn scope_parameter() -> Json {
    json!({
        "type": "string",
        "enum": [SCOPE_APPLICATION, SCOPE_INSTANCE],
        "description":
            "`application` (the default) for one application's catalogue; \
             `instance` for the shared catalogue every application serves under \
             its own.",
    })
}

/// The `scope` and `application` arguments, resolved.
async fn target(ctx: &ToolContext<'_>, args: &Map<String, Json>) -> Result<Target> {
    match optional_string(args, ARG_SCOPE)?.as_deref().map(str::trim) {
        None | Some("") | Some(SCOPE_APPLICATION) => {
            Ok(Target::Application(Box::new(load_app(ctx, args).await?)))
        }
        Some(SCOPE_INSTANCE) => {
            if args.contains_key(ARG_APPLICATION) {
                return Err(Error::invalid(format!(
                    "`{ARG_SCOPE}: \"{SCOPE_INSTANCE}\"` is the shared catalogue, which \
                     belongs to no application: leave out `{ARG_APPLICATION}`"
                )));
            }
            Ok(Target::Instance)
        }
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_SCOPE}` is `{SCOPE_APPLICATION}` or `{SCOPE_INSTANCE}`, not `{other}`"
        ))),
    }
}

/// The screen's payload, reshaped for a model: one locale's column when it
/// asked for one, the unwrapped list capped, and a next step in words.
fn describe(target: &Target, mut screen: Json, locale: Option<&str>, missing_only: bool) -> Json {
    let enabled: Vec<String> = screen["locales"]
        .as_array()
        .map(|ls| {
            ls.iter()
                .filter_map(|l| l["locale"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut notes = Vec::new();
    match target {
        Target::Application(app) => {
            if enabled.is_empty() {
                notes.push(format!(
                    "`{}` serves no locales yet: enable one with `{TOOL_UPDATE_APP}`'s \
                     `locales` before saving translations into it.",
                    app.subdomain
                ));
            }
            if let Some(tag) = locale
                && !enabled.iter().any(|l| l == tag)
            {
                notes.push(format!(
                    "`{tag}` is not one of this application's locales; `{TOOL_UPDATE_APP}` \
                     with `locales: {{ add: [\"{tag}\"] }}` enables it."
                ));
            }
        }
        Target::Instance => {
            if let Some(tag) = locale
                && !enabled.iter().any(|l| l == tag)
            {
                notes.push(format!(
                    "`{tag}` is not one of this installation's enabled languages (Settings → \
                     Localisation, the `enabled_locales` setting), so the shared catalogue \
                     cannot hold it yet."
                ));
            } else if enabled.is_empty() {
                notes.push(
                    "This installation serves one language (Settings → Localisation, the \
                     `enabled_locales` setting), so the shared catalogue has no locales yet."
                        .to_owned(),
                );
            }
            if screen["messages"].as_array().is_some_and(Vec::is_empty) {
                notes.push(format!(
                    "The shared catalogue is empty. `{TOOL_SAVE_TRANSLATIONS}` with \
                     `{ARG_SCOPE}: \"{SCOPE_INSTANCE}\"` adds any key: the English text \
                     exactly as the applications' `t()` calls say it."
                ));
            }
        }
    }

    let messages = screen["messages"].take();
    let messages: Vec<Json> = messages
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|mut row| {
            let Some(tag) = locale else {
                return Some(row);
            };
            let translation = row["translations"].get(tag).cloned().unwrap_or(Json::Null);
            // What the shared catalogue serves where the app has nothing (or
            // a blank) of its own: covered, so not missing.
            let own_blank = translation.is_null() || translation == json!("");
            let inherited = match own_blank {
                true => row["inherited"].get(tag).cloned().unwrap_or(Json::Null),
                false => Json::Null,
            };
            if missing_only && !(own_blank && inherited.is_null()) {
                return None;
            }
            let object = row.as_object_mut()?;
            object.remove("translations");
            object.remove("inherited");
            object.insert("translation".to_owned(), translation);
            if !inherited.is_null() {
                object.insert("inherited".to_owned(), inherited);
            }
            Some(row)
        })
        .collect();

    let unwrapped = screen["unwrapped"].take();
    let unwrapped = unwrapped.as_array().cloned().unwrap_or_default();
    if !unwrapped.is_empty() {
        notes.push(format!(
            "{} user-visible literal(s) are not wrapped in `t()` and cannot be \
             translated: wrap them in the source (see the project's AGENTS.md), \
             and they appear here as messages.",
            unwrapped.len()
        ));
    }
    let mut out = screen;
    match target {
        Target::Application(app) => out["application"] = json!(app.subdomain),
        Target::Instance => out[ARG_SCOPE] = json!(SCOPE_INSTANCE),
    }
    out["messages"] = json!(messages);
    out["unwrapped_count"] = json!(unwrapped.len());
    out["unwrapped"] = json!(
        unwrapped
            .into_iter()
            .take(MAX_UNWRAPPED)
            .collect::<Vec<_>>()
    );
    out["notes"] = json!(notes);
    out
}

struct SaveTranslations;

#[async_trait::async_trait]
impl AdminTool for SaveTranslations {
    fn name(&self) -> &'static str {
        TOOL_SAVE_TRANSLATIONS
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        let edit = match grants.edit {
            true => "",
            false => " You may **not** call this: it needs `allow_edit`.",
        };
        format!(
            "Write translations into one locale of an application. You are the \
             translator: `{ARG_MESSAGES}` maps a message key — the English source \
             text exactly as `{TOOL_DESCRIBE_TRANSLATIONS}` lists it — to its \
             translation. Only the keys you name change; the rest of the locale's \
             translations are kept, and a key mapped to `null` is removed. To \
             correct one translation, send just that key.\n\n\
             A translation must keep the source's `{{placeholder}}` names exactly \
             (`Delete {{name}}?` → `{{name}} löschen?`, never `{{Name}}`); a message \
             with a plural form is an object keyed by CLDR category (`{{\"one\": \
             \"…\", \"other\": \"…\"}}`) with the categories that locale uses. A \
             translation that breaks either rule is refused naming its key, and \
             nothing is written. Translate large applications in batches of about \
             a hundred keys. The running application serves the change on its next \
             page load; no build is needed. The locale must be enabled first \
             (`{TOOL_UPDATE_APP}`'s `locales`).\n\n\
             With `{ARG_SCOPE}: \"{SCOPE_INSTANCE}\"` (and no `{ARG_APPLICATION}`) \
             this writes the **shared catalogue** every application serves under its \
             own — the place for strings many applications say. Any key may be \
             added there; its locales are the installation's enabled languages. An \
             application's own translation of a key still wins.{edit}"
        )
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_SCOPE: scope_parameter(),
                ARG_APPLICATION: {
                    "type": "string",
                    "description": "The application, by subdomain. Required unless `scope` is `instance`.",
                },
                ARG_LOCALE: {
                    "type": "string",
                    "description": "The locale tag being written, e.g. `de`.",
                },
                ARG_MESSAGES: {
                    "type": "object",
                    "description":
                        "English message key → translation (a string, a plural \
                         object, or null to remove it).",
                    "additionalProperties": {
                        "type": ["string", "object", "null"],
                    },
                },
            },
            "required": [ARG_LOCALE, ARG_MESSAGES],
            "additionalProperties": false,
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(
            args,
            &[ARG_SCOPE, ARG_APPLICATION, ARG_LOCALE, ARG_MESSAGES],
        )?;
        require_grant(grants.edit, "translate an application", GRANT_EDIT)?;
        let target = target(ctx, &args).await?;
        let locale = locale_arg(&args)?
            .ok_or_else(|| Error::invalid(format!("`{ARG_LOCALE}` is required")))?;
        let Some(Json::Object(changes)) = args.get(ARG_MESSAGES) else {
            return Err(Error::invalid(format!(
                "`{ARG_MESSAGES}` should be an object of English key → translation"
            )));
        };

        // What the locale has now, from the same screen the admin sees. Orphans
        // are not in it, and need not be: the save puts those back itself.
        let screen = get_translations(ctx, &target).await?;
        let mut merged = current_translations(&screen, &locale);
        let known: Vec<&str> = screen["messages"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r["key"].as_str()).collect())
            .unwrap_or_default();
        let mut unknown = Vec::new();
        let mut removed = 0;
        for (key, value) in changes {
            if value.is_null() {
                removed += usize::from(merged.remove(key).is_some());
                continue;
            }
            // The shared catalogue has no source: its keys are whatever it
            // holds, so a new one is the normal way to add a string.
            if matches!(target, Target::Application(_)) && !known.contains(&key.as_str()) {
                unknown.push(key.clone());
            }
            merged.insert(key.clone(), value.clone());
        }

        let call = match &target {
            Target::Application(app) => {
                AdminCall::new("saveTranslations", json!({ "messages": merged }))
                    .param("id", app.id.to_string())
            }
            Target::Instance => {
                AdminCall::new("saveInstanceTranslations", json!({ "messages": merged }))
            }
        };
        let mut out = host(ctx)?
            .call_admin(
                call.param("locale", locale.clone())
                    .user(ctx.user.map(|u| u.id)),
            )
            .await?;
        out["written"] = json!(changes.values().filter(|v| !v.is_null()).count());
        out["removed"] = json!(removed);
        if !unknown.is_empty() {
            // Saved, because a key the extractor cannot see may still be used
            // (a file it does not read), but said: the usual cause is a key that
            // is not the source text exactly.
            out["not_in_source"] = json!(unknown);
            out["notes"] = json!([format!(
                "{} key(s) are not strings this application's source uses, so \
                 nothing renders them. A key must be the English text exactly as \
                 `{TOOL_DESCRIBE_TRANSLATIONS}` lists it.",
                unknown.len()
            )]);
        }
        Ok(out)
    }
}

/// One locale's current translations, out of the screen's grid.
fn current_translations(screen: &Json, locale: &str) -> Map<String, Json> {
    screen["messages"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let key = row["key"].as_str()?;
                    let message = row["translations"].get(locale)?;
                    Some((key.to_owned(), message.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn get_translations(ctx: &ToolContext<'_>, target: &Target) -> Result<Json> {
    let call = match target {
        Target::Application(app) => {
            AdminCall::new("getTranslations", Json::Null).param("id", app.id.to_string())
        }
        Target::Instance => AdminCall::new("getInstanceTranslations", Json::Null),
    };
    host(ctx)?
        .call_admin(call.user(ctx.user.map(|u| u.id)))
        .await
}

/// The running server's admin handlers. Finding an application's strings needs
/// its file store and the view runtime, which only a server holds.
fn host(ctx: &ToolContext<'_>) -> Result<Arc<dyn AdminHost>> {
    ctx.catalog.admin_host().ok_or_else(|| {
        Error::config(format!(
            "{} is not connected to a running server, and translations are read \
             and written through it",
            ctx.actor
        ))
    })
}

/// The `locale` argument, checked to be a locale tag.
fn locale_arg(args: &Map<String, Json>) -> Result<Option<String>> {
    match optional_string(args, ARG_LOCALE)?.map(|s| s.trim().to_owned()) {
        None => Ok(None),
        Some(tag) if tag.is_empty() => Ok(None),
        Some(tag) => Ok(Some(sc_i18n::Locale::parse(&tag)?.as_str().to_owned())),
    }
}
