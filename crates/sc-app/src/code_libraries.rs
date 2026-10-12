//! Code libraries as the admin API, MCP and backups see them (TODO F3): the
//! checked save, who uses each one, and the one JSON shape all three speak.
//!
//! The rows are `sc-catalog`'s ([`sc_catalog::CodeLibrary`]); this module is in
//! `sc-app` because the questions it answers need applications — whether a
//! library's application exists, and which custom queries import it.

use std::collections::BTreeMap;

use sc_action::TriggerBody;
use sc_api::QueryLanguage;
use sc_catalog::{Catalog, CodeLibrary};
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::AppId;

/// The action whose `code` setting is a JavaScript body.
const RUN_JS_CODE: &str = "run_js_code";

/// One piece of code that imports a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryUse {
    /// `query` (a custom query) or `trigger`.
    pub kind: &'static str,
    /// The query's or trigger's name.
    pub name: String,
    /// The application a query belongs to, by subdomain; `None` for a trigger.
    pub application: Option<String>,
}

impl LibraryUse {
    fn json(&self) -> Json {
        json!({ "kind": self.kind, "name": self.name, "application": self.application })
    }
}

/// Which JavaScript queries and triggers import each library, by library id —
/// resolved as `lib()` resolves: a query imports its application's library of
/// that name if there is one, and the instance's otherwise; a trigger imports
/// the instance's only.
///
/// A scan of the source for literal names ([`sc_expr::imported_libraries`]),
/// so it is a list to check before an edit, not an authority.
pub async fn code_library_uses(
    catalog: &Catalog,
    libraries: &[CodeLibrary],
) -> Result<BTreeMap<Uuid, Vec<LibraryUse>>> {
    let find = |name: &str, app: Option<Uuid>| {
        app.and_then(|app| {
            libraries
                .iter()
                .find(|l| l.name == name && l.application_id == Some(app))
        })
        .or_else(|| {
            libraries
                .iter()
                .find(|l| l.name == name && l.application_id.is_none())
        })
        .map(|l| l.id)
    };
    let mut uses: BTreeMap<Uuid, Vec<LibraryUse>> = BTreeMap::new();
    for app in crate::list_applications(catalog).await? {
        for api in &app.apis {
            let Ok(queries) = sc_api::custom_queries(&api.config) else {
                continue;
            };
            for query in queries {
                if query.language != QueryLanguage::Javascript {
                    continue;
                }
                for name in sc_expr::imported_libraries(&query.code) {
                    if let Some(id) = find(&name, Some(app.id.0)) {
                        uses.entry(id).or_default().push(LibraryUse {
                            kind: "query",
                            name: query.name.clone(),
                            application: Some(app.subdomain.clone()),
                        });
                    }
                }
            }
        }
    }
    if catalog.get(sc_action::TRIGGERS_TABLE)?.is_some() {
        for trigger in sc_action::list_triggers(catalog).await? {
            let TriggerBody::Action {
                action,
                configuration,
            } = &trigger.body
            else {
                continue;
            };
            let Some(code) = configuration.get("code").and_then(Json::as_str) else {
                continue;
            };
            if action != RUN_JS_CODE {
                continue;
            }
            for name in sc_expr::imported_libraries(code) {
                if let Some(id) = find(&name, None) {
                    uses.entry(id).or_default().push(LibraryUse {
                        kind: "trigger",
                        name: trigger.name.clone(),
                        application: None,
                    });
                }
            }
        }
    }
    Ok(uses)
}

/// Save a library after every check a save makes: the name and its scope
/// ([`sc_catalog::check_code_library_saveable`]), that its application exists,
/// and that the code **compiles** — asked of the server's JavaScript engine,
/// without running it. A process with no engine (a build tool) skips only the
/// last.
pub async fn save_code_library_checked(catalog: &Catalog, library: &CodeLibrary) -> Result<()> {
    if let Some(app) = library.application_id
        && crate::load_application(catalog, AppId(app))
            .await?
            .is_none()
    {
        return Err(Error::invalid(format!(
            "code library `{}` names application {app}, which does not exist",
            library.name
        )));
    }
    sc_catalog::check_code_library_saveable(catalog, library).await?;
    if let Some(engine) = catalog.formula_evaluator()
        && let Some(why) = sc_expr::check_code_library(&*engine, &library.code).await?
    {
        return Err(Error::invalid(format!(
            "code library `{}` does not compile: {why}",
            library.name
        )));
    }
    sc_catalog::save_code_library(catalog, library).await
}

/// One library as the admin API, MCP and a backup show it. `uses` and the
/// application's subdomain are added where the caller knows them.
pub fn code_library_json(
    library: &CodeLibrary,
    application: Option<&str>,
    uses: Option<&[LibraryUse]>,
) -> Json {
    let mut out = json!({
        "id": library.id,
        "name": library.name,
        "description": library.description,
        "code": library.code,
        "application_id": library.application_id,
        "application": application,
    });
    if let Some(uses) = uses {
        out["used_by"] = Json::Array(uses.iter().map(LibraryUse::json).collect());
    }
    out
}

/// A library from the body of a create or update (and a backup's entry): `name`
/// and `code` are required, `description` and `application_id` optional.
pub fn code_library_from_json(id: Uuid, body: &Json) -> Result<CodeLibrary> {
    let obj = body
        .as_object()
        .ok_or_else(|| Error::invalid("a code library is a JSON object"))?;
    let text = |key: &str| obj.get(key).and_then(Json::as_str);
    let name = text("name")
        .ok_or_else(|| Error::invalid("a code library needs a `name`"))?
        .trim()
        .to_owned();
    let code = text("code")
        .ok_or_else(|| Error::invalid(format!("code library `{name}` needs its `code`")))?
        .to_owned();
    let application_id = match obj.get("application_id") {
        None | Some(Json::Null) => None,
        Some(Json::String(s)) if s.is_empty() => None,
        Some(Json::String(s)) => Some(Uuid::parse_str(s).map_err(|_| {
            Error::invalid(format!("`application_id` should be a UUID, got `{s}`"))
        })?),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`application_id` should be a UUID or null, got {other}"
            )));
        }
    };
    Ok(CodeLibrary {
        id,
        name,
        description: text("description").unwrap_or_default().to_owned(),
        code,
        application_id,
    })
}

/// Every library with its application's subdomain and its uses, as the admin
/// list and `describe_code_libraries` answer.
pub async fn describe_code_libraries(catalog: &Catalog) -> Result<Vec<Json>> {
    let libraries = sc_catalog::list_code_libraries(catalog).await?;
    let uses = code_library_uses(catalog, &libraries).await?;
    let subdomains: BTreeMap<Uuid, String> = crate::list_applications(catalog)
        .await?
        .into_iter()
        .map(|app| (app.id.0, app.subdomain))
        .collect();
    Ok(libraries
        .iter()
        .map(|library| {
            let app = library
                .application_id
                .and_then(|id| subdomains.get(&id))
                .map(String::as_str);
            let used = uses.get(&library.id).map(Vec::as_slice).unwrap_or(&[]);
            code_library_json(library, app, Some(used))
        })
        .collect())
}
