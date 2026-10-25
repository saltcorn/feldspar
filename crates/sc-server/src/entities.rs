//! `listEntities`: every entity an admin configures, in one list — the admin
//! UI's home page (Saltcorn 1's "All entities").
//!
//! Five kinds: tables, file stores, triggers, agents and applications. A view
//! or a page belongs to one application's framework rather than to the
//! installation, so it is not one of them. Each row carries what a mixed list
//! shows — its name, a line of details, who may use it — and when it was last
//! saved (`sc_catalog::edited`), which is what the "recently edited" section
//! sorts by. The ordering and the searching are the screen's: the list is short
//! enough to send whole.
//!
//! A **deep** listing also carries each entity's definition as JSON text, the
//! same JSON its own list endpoint answers (so secrets are masked exactly as
//! they are there), for a search that looks inside definitions rather than at
//! their names.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sc_action::TriggerBody;
use sc_auth::{ROLE_ADMIN, list_roles};
use sc_catalog::{Catalog, DbId};
use sc_error::Result;
use serde_json::{Value as Json, json};

use crate::apps::AppMounts;
use crate::handler::{HandlerRegistry, HandlerResponse};
use crate::handlers::{
    agent_json, agents_of, application_api_json, field_json, file_store_json, table_json,
    trigger_json_with, triggers_of,
};

/// Register `listEntities` on `reg`.
pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>, apps: Arc<AppMounts>) {
    reg.register("listEntities", {
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let deep = matches!(ctx.query_get("deep"), Some("true" | "1"));
                let out = list_entities(&catalog, &apps, deep, &ctx.locale).await?;
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });
}

/// Role numbers as the names an admin gave them.
struct RoleNames(HashMap<u8, String>);

impl RoleNames {
    fn name(&self, role: u8) -> String {
        self.0
            .get(&role)
            .cloned()
            .unwrap_or_else(|| role.to_string())
    }

    /// A floor that may be absent, with what absent means for this kind.
    fn floor(&self, role: Option<u8>, absent: &str) -> String {
        role.map_or_else(|| absent.to_owned(), |r| self.name(r))
    }
}

/// One row, before the definition is attached.
struct Row {
    kind: &'static str,
    id: String,
    name: String,
    description: String,
    details: Vec<String>,
    access: String,
    updated_at: Option<DateTime<Utc>>,
    runnable: bool,
    subdomain: Option<String>,
    definition: Option<Json>,
}

impl Row {
    fn into_json(self, deep: bool) -> Json {
        json!({
            "kind": self.kind,
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "details": self.details,
            "access": self.access,
            "updated_at": self.updated_at.map(|t| t.to_rfc3339()),
            "runnable": self.runnable,
            "subdomain": self.subdomain,
            "definition": if deep {
                self.definition.map(|d| Json::String(d.to_string()))
            } else {
                None
            },
        })
    }
}

async fn list_entities(
    catalog: &Catalog,
    apps: &AppMounts,
    deep: bool,
    locale: &sc_i18n::Locale,
) -> Result<Vec<Json>> {
    let roles = RoleNames(
        list_roles(catalog)
            .await?
            .into_iter()
            .map(|r| (r.role, r.name))
            .collect(),
    );
    let admin = roles.name(ROLE_ADMIN);
    let mut rows: Vec<Row> = Vec::new();

    // --- tables
    let edited = sc_catalog::updated_at_by(catalog, sc_catalog::TABLE_EDITS_TABLE, "name").await?;
    let rls = catalog.primary().capabilities().row_level_security;
    for table in catalog.tables()?.iter().filter(|t| !t.is_hidden()) {
        let mut details = Vec::new();
        if let Some((_, provider)) = table.provider() {
            details.push(provider.to_owned());
        } else if table.database != DbId::primary() {
            details.push(table.database.0.clone());
        }
        let fields = table.fields.len();
        details.push(format!(
            "{fields} field{}",
            if fields == 1 { "" } else { "s" }
        ));
        rows.push(Row {
            kind: "table",
            id: table.name.clone(),
            name: table.name.clone(),
            description: table.description.clone(),
            details,
            access: format!(
                "{}/{}",
                roles.name(table.access.min_role_read),
                roles.name(table.access.min_role_write)
            ),
            updated_at: edited.get(&table.name).copied(),
            runnable: true,
            subdomain: None,
            // The table's settings and its fields, so a deep search finds a
            // table by a column it has.
            definition: deep.then(|| {
                let mut out = table_json(catalog, table, rls, locale);
                out["fields"] =
                    Json::Array(table.fields.iter().map(|f| field_json(f, "")).collect());
                out
            }),
        });
    }

    // --- file stores: the stored definitions; a store a module made for itself
    // has no definition to configure, so it is not an entity here.
    let edited = sc_catalog::updated_at_by(catalog, sc_catalog::FILE_STORES_TABLE, "id").await?;
    let stores = if catalog.get(sc_catalog::FILE_STORES_TABLE)?.is_some() {
        sc_catalog::list_file_stores(catalog).await?
    } else {
        Vec::new()
    };
    for def in stores {
        let mut details = vec![def.backend.clone()];
        if catalog.file_store(&def.name)?.is_none() {
            details.push("not connected".to_owned());
        }
        let definition = if deep {
            Some(file_store_json(catalog, &def)?)
        } else {
            None
        };
        rows.push(Row {
            kind: "file_store",
            id: def.id.0.to_string(),
            updated_at: edited.get(&def.id.0.to_string()).copied(),
            access: roles.floor(def.min_role, "public"),
            name: def.name,
            description: def.description,
            details,
            runnable: true,
            subdomain: None,
            definition,
        });
    }

    // --- triggers: the stored rows, with the live set's issues, as the
    // triggers list shows them. A server with no dispatcher still lists them.
    let edited = sc_catalog::updated_at_by(catalog, sc_action::TRIGGERS_TABLE, "id").await?;
    let dispatcher = triggers_of(apps).ok();
    let live = dispatcher.as_ref().map(|d| d.triggers()).transpose()?;
    let stored = if catalog.get(sc_action::TRIGGERS_TABLE)?.is_some() {
        sc_action::list_triggers(catalog).await?
    } else {
        Vec::new()
    };
    for trigger in stored {
        let problem = live.as_ref().and_then(|live| {
            live.issues()
                .iter()
                .find(|i| i.trigger == trigger.name)
                .map(|i| i.problem.clone())
        });
        let mut details = vec![trigger.when.as_str().to_owned()];
        if let Some(channel) = &trigger.channel {
            details.push(channel.clone());
        }
        details.push(match trigger.body {
            TriggerBody::Workflow => "workflow".to_owned(),
            _ => trigger.action().unwrap_or_default().to_owned(),
        });
        if problem.is_some() {
            details.push("not usable".to_owned());
        } else if !trigger.is_enabled() {
            details.push("off".to_owned());
        }
        // Masked (§11.1) as the triggers list masks it; with no dispatcher
        // there is no registry to say what is secret, so no definition either.
        let definition = match (&dispatcher, deep) {
            (Some(dispatcher), true) => {
                let shown = dispatcher.registry().redact_trigger(catalog, &trigger);
                Some(trigger_json_with(&shown, problem.clone(), None))
            }
            _ => None,
        };
        rows.push(Row {
            kind: "trigger",
            id: trigger.id.0.to_string(),
            updated_at: edited.get(&trigger.id.0.to_string()).copied(),
            access: roles.floor(trigger.min_role, &admin),
            runnable: dispatcher.is_some() && problem.is_none() && trigger.is_enabled(),
            name: trigger.name,
            description: trigger.description,
            details,
            subdomain: None,
            definition,
        });
    }

    // --- agents
    let edited = sc_catalog::updated_at_by(catalog, sc_agent::AGENTS_TABLE, "id").await?;
    // An installation without the agent services still lists its stored
    // agents; it just cannot say which are broken.
    let issues = match agents_of(apps) {
        Ok(services) => sc_agent::validate::Agents::load(catalog, services.registry())
            .await?
            .issues()
            .to_vec(),
        Err(_) => Vec::new(),
    };
    let agents = if catalog.get(sc_agent::AGENTS_TABLE)?.is_some() {
        sc_agent::list_agents(catalog).await?
    } else {
        Vec::new()
    };
    for agent in agents {
        let problem = issues
            .iter()
            .find(|i| i.agent == agent.name)
            .map(|i| i.problem.clone());
        let mut details = vec![agent.provider.clone()];
        if let Some(model) = agent.model.as_deref().filter(|m| !m.trim().is_empty()) {
            details.push(model.to_owned());
        }
        if problem.is_some() {
            details.push("not usable".to_owned());
        }
        rows.push(Row {
            kind: "agent",
            id: agent.id.0.to_string(),
            updated_at: edited.get(&agent.id.0.to_string()).copied(),
            access: roles.floor(agent.min_role, &admin),
            runnable: problem.is_none(),
            definition: deep.then(|| agent_json(&agent, problem.clone())),
            name: agent.name,
            description: agent.description,
            details,
            subdomain: None,
        });
    }

    // --- applications
    let edited = sc_catalog::updated_at_by(catalog, sc_app::APPLICATIONS_TABLE, "id").await?;
    let applications = if catalog.get(sc_app::APPLICATIONS_TABLE)?.is_some() {
        sc_app::list_applications(catalog).await?
    } else {
        Vec::new()
    };
    for app in applications {
        rows.push(Row {
            kind: "application",
            id: app.id.0.to_string(),
            updated_at: edited.get(&app.id.0.to_string()).copied(),
            details: vec![app.framework.name.clone(), app.subdomain.clone()],
            access: roles.name(app.min_role),
            runnable: true,
            subdomain: Some(app.subdomain.clone()),
            definition: deep.then(|| application_api_json(&app)),
            name: app.name,
            description: app.description,
        });
    }

    Ok(rows.into_iter().map(|row| row.into_json(deep)).collect())
}
