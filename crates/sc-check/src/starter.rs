//! Starter checklists (docs/CHECKLISTS.md §2, TODO C3.6): what a framework
//! offers a new application as its first checklist.
//!
//! **Nothing is created automatically.** [`starters`] says what an
//! application's framework would offer, the admin UI shows it, and one click
//! creates it ([`create_starter`]) — through the same
//! [`create_checklist`](crate::edit::create_checklist) as any other, so it is
//! validated the same way and is the person's from then on. An offer whose name
//! a checklist already has is not made again.
//!
//! What each framework offers:
//!
//! - `react`: the app builds, type-checks against its current client, and `/`
//!   renders with no error boundary showing;
//! - Saltcorn UI: every view renders, once for an anonymous visitor and once
//!   for each check user — one user per checklist, so one checklist per role;
//! - a server-rendered site: `/` renders on the server, and is clean in a
//!   browser;
//! - any other framework with a UI: it builds (when it has a build) and `/`
//!   opens clean.

use sc_app::Application;
use sc_auth::{COL_EMAIL, USERS_TABLE, User};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_query::{Expr, OrderBy, Projection, Select, Source, Statement};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::edit::{self, ChecklistInput, Editor};
use crate::model::{Checklist, StepDraft};
use crate::registry::{FrameworkScope, StepRegistry, serves_ui};
use crate::store;

/// A checklist a framework offers an application.
#[derive(Debug, Clone, PartialEq)]
pub struct Starter {
    /// What the checklist would be.
    pub input: ChecklistInput,
    /// Its steps.
    pub steps: Vec<StepDraft>,
}

impl Starter {
    fn new(app: &Application, name: String, description: &str, steps: Vec<StepDraft>) -> Starter {
        let mut input = ChecklistInput::new(name);
        input.description = description.to_owned();
        input.application = Some(app.id.0);
        Starter { input, steps }
    }

    fn run_as(mut self, user: Option<Uuid>) -> Starter {
        self.input.run_as = user;
        self
    }

    /// As the admin UI lists it.
    pub fn to_json(&self) -> Json {
        json!({
            "name": self.input.name,
            "description": self.input.description,
            "run_as": self.input.run_as.map(|u| u.to_string()),
            "steps": self.steps.iter().map(|s| json!({
                "type": s.type_,
                "label": s.label,
                "params": s.params,
            })).collect::<Vec<_>>(),
        })
    }
}

/// The starter checklists `application`'s framework offers that it does not
/// have yet — none for a framework without a UI.
pub async fn starters(catalog: &Catalog, application: &Application) -> Result<Vec<Starter>> {
    let check_users = check_users(catalog).await?;
    let roles = sc_auth::list_roles(catalog).await?;
    let role_name = |role: u8| {
        roles
            .iter()
            .find(|r| r.role == role)
            .map_or_else(|| format!("role {role}"), |r| r.name.clone())
    };
    let mut offered = offer(application, &check_users, role_name);
    let mut kept = Vec::new();
    for starter in offered.drain(..) {
        if store::load_checklist_by_name(catalog, &starter.input.name)
            .await?
            .is_none()
        {
            kept.push(starter);
        }
    }
    Ok(kept)
}

/// Create the starter called `name` that `application` is offered.
pub async fn create_starter(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    application: &Application,
    name: &str,
) -> Result<Checklist> {
    let starter = starters(catalog, application)
        .await?
        .into_iter()
        .find(|s| s.input.name == name)
        .ok_or_else(|| {
            Error::not_found(format!(
                "`{}` is offered no starter checklist called `{name}` (it may exist already)",
                application.subdomain
            ))
        })?;
    edit::create_checklist(catalog, registry, editor, starter.input, starter.steps).await
}

/// What the framework offers, before the checklists that exist are left out.
fn offer(
    app: &Application,
    check_users: &[(Uuid, String, u8)],
    role_name: impl Fn(u8) -> String,
) -> Vec<Starter> {
    let name = &app.name;
    let open_root = || {
        StepDraft::new("browser.open")
            .param("path", "/")
            .label("The home page opens with no errors")
    };
    match app.framework.name.as_str() {
        sc_app::NONE_FRAMEWORK => Vec::new(),
        sc_app::REACT_FRAMEWORK => vec![Starter::new(
            app,
            format!("{name}: builds and renders"),
            "Offered by the React framework: the app builds, type-checks against its current \
             generated client, and its home route renders with no error boundary showing.",
            vec![
                StepDraft::new("app.builds").label("The app builds"),
                StepDraft::new("react.typecheck")
                    .label("The source type-checks against the current client"),
                StepDraft::new("react.open_route")
                    .param("path", "/")
                    .label("The home route renders"),
                StepDraft::new("react.no_error_boundary").label("No error boundary is showing"),
            ],
        )],
        sc_viewpattern::SALTCORN_UI_FRAMEWORK => {
            let views_render =
                || StepDraft::new("saltcorn_ui.views_render").label("Every view renders");
            let mut out = vec![Starter::new(
                app,
                format!("{name}: views render for visitors"),
                "Offered by Saltcorn UI: every view an anonymous visitor may see renders \
                 without error.",
                vec![views_render()],
            )];
            for (id, email, role) in check_users {
                out.push(
                    Starter::new(
                        app,
                        format!("{name}: views render for {}", role_name(*role)),
                        &format!(
                            "Offered by Saltcorn UI: every view {} may see renders without \
                             error, as the check user {email}.",
                            role_name(*role)
                        ),
                        vec![views_render()],
                    )
                    .run_as(Some(*id)),
                );
            }
            out
        }
        _ if FrameworkScope::ServerRendered.matches(app) => vec![Starter::new(
            app,
            format!("{name}: renders"),
            "Offered for a server-rendered site: the home page renders on the server and opens \
             clean in a browser.",
            vec![
                StepDraft::new("server_render.page_renders")
                    .param("path", "/")
                    .label("The home page renders on the server"),
                open_root(),
            ],
        )],
        _ if serves_ui(app) => {
            let mut steps = Vec::new();
            if sc_app::app_source_from_config(&app.framework).is_ok() {
                steps.push(StepDraft::new("app.builds").label("The app builds"));
            }
            steps.push(open_root());
            vec![Starter::new(
                app,
                format!("{name}: renders"),
                "Offered for every application with a UI: it builds, and its home page opens \
                 with no console errors or failed requests.",
                steps,
            )]
        }
        _ => Vec::new(),
    }
}

/// Every check user, as `(id, email, role)`, most privileged first — one
/// per role, the first of each, since a starter is per role.
async fn check_users(catalog: &Catalog) -> Result<Vec<(Uuid, String, u8)>> {
    let mut select = Select::from(Source::table(USERS_TABLE)).columns(vec![Projection::all()]);
    select.order = vec![OrderBy::asc(Expr::col(COL_EMAIL))];
    let rows = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    let mut out: Vec<(Uuid, String, u8)> = Vec::new();
    for row in &rows {
        let user = User::from_row(row)?;
        if !user.is_check_user() || user.is_disabled() || out.iter().any(|u| u.2 == user.role) {
            continue;
        }
        let email = user
            .get(COL_EMAIL)
            .and_then(sc_query::Value::as_text)
            .unwrap_or_default()
            .to_owned();
        out.push((user.id, email, user.role));
    }
    out.sort_by_key(|u| u.2);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(starter: &Starter) -> Vec<&str> {
        starter.steps.iter().map(|s| s.type_.as_str()).collect()
    }

    #[test]
    fn each_framework_offers_its_own() {
        let react = Application::new(
            "Shop",
            "shop",
            sc_app::FrameworkRef::new("react")
                .with("store", "apps")
                .with("project", "shop"),
        );
        let offered = offer(&react, &[], |r| r.to_string());
        assert_eq!(offered.len(), 1);
        assert_eq!(
            types(&offered[0]),
            [
                "app.builds",
                "react.typecheck",
                "react.open_route",
                "react.no_error_boundary"
            ]
        );
        assert_eq!(offered[0].input.application, Some(react.id.0));
        assert_eq!(offered[0].input.run_as, None);

        let ui = Application::new("Books", "books", sc_app::FrameworkRef::new("saltcorn-ui"));
        let staff = Uuid::new_v4();
        let offered = offer(&ui, &[(staff, "staff@x".to_owned(), 40)], |r| {
            if r == 40 {
                "Staff".to_owned()
            } else {
                r.to_string()
            }
        });
        assert_eq!(offered.len(), 2, "anonymous, and one per check user");
        assert_eq!(offered[0].input.run_as, None);
        assert_eq!(offered[1].input.run_as, Some(staff));
        assert_eq!(offered[1].input.name, "Books: views render for Staff");
        assert_eq!(types(&offered[1]), ["saltcorn_ui.views_render"]);

        let none = Application::new("Api", "api", sc_app::FrameworkRef::new("none"));
        assert!(offer(&none, &[], |r| r.to_string()).is_empty());
    }

    #[test]
    fn every_offered_step_is_valid_for_its_application() {
        let registry = StepRegistry::builtin();
        for app in [
            Application::new(
                "Shop",
                "shop",
                sc_app::FrameworkRef::new("react")
                    .with("store", "apps")
                    .with("project", "shop"),
            ),
            Application::new("Books", "books", sc_app::FrameworkRef::new("saltcorn-ui")),
            Application::new(
                "Site",
                "site",
                sc_app::FrameworkRef::new("code")
                    .with("store", "apps")
                    .with("output", "dist")
                    .with("command", "sh build.sh"),
            ),
        ] {
            for starter in offer(&app, &[(Uuid::new_v4(), "c@x".to_owned(), 80)], |r| {
                r.to_string()
            }) {
                let checklist = Checklist {
                    id: Uuid::new_v4(),
                    name: starter.input.name.clone(),
                    description: String::new(),
                    application: Some(app.id.0),
                    run_as: None,
                    enabled: true,
                    mutates: false,
                    attributes: Default::default(),
                };
                for draft in &starter.steps {
                    edit::validate_draft(&registry, &checklist, Some(&app), draft)
                        .unwrap_or_else(|e| panic!("{}: {e}", starter.input.name));
                }
            }
        }
    }
}
