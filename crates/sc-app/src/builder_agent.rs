//! What agent builds an application — declared by its **framework** (§13.3,
//! §11.3).
//!
//! Creating an application creates the agent that will build it. Which agent that
//! is cannot be a property of "applications in general": a code framework's app is
//! a source tree in a file store, so the agent that builds it is a coding agent
//! pointed at that tree, while a framework that renders from the catalog would
//! want something else entirely and one that is served from elsewhere wants
//! nothing at all. So the declaration sits beside the framework's other
//! declarations — its settings ([`framework_config_spec`](crate::framework_config_spec)),
//! its default CSP ([`framework_default_csp`](crate::framework_default_csp)) —
//! and is resolved the same way: **by name, from the registry**, because an
//! application is created before there is any built instance to ask.
//!
//! ## Why the traits are named as strings
//!
//! The trait itself (`coding`) lives in `sc-core-traits`,
//! which is layer 9 and sits *above* this crate — as it must, since a trait's
//! writes go through the row layer. A framework here can therefore only *name* the
//! trait it wants and the settings to give it, exactly as an application names its
//! API providers rather than holding them. That the names and the configuration
//! keys still match the real traits is asserted from above, in `sc-core-traits`'
//! own tests, where both sides are visible at once.
//!
//! Assembling a spec into an [`Agent`](sc_agent::Agent) and storing it is the
//! server's, for the same layering reason: this crate does not know agents exist.
//!
//! ## Three traits, planned
//!
//! The agent is `coding`, plus `http` so it can read the documentation of what
//! it is building with, plus `preview_pane` pointed at the application's own
//! subdomain (TODO §12, "The preview pane"). Building the application is one of
//! `coding`'s checks — its `application` setting — rather than a second trait
//! with a tool of its own, so the build runs where the ratchet, the baseline and
//! the preview mount are, and a model is never offered two ways to ask "does
//! this compile?". It starts in `plan` mode (`workflow: planned`), may check its
//! work and look at the result, and may not run arbitrary scripts or a shell:
//! those execute code nobody reviewed, and are grants the admin gives
//! deliberately rather than ones that arrive with an application.
//!
//! `http` is the trait's default shape: `fetch_web`, read-only, public hosts
//! only. No host allow-list, because which documentation a project needs is the
//! project's — a React app reads `react.dev`, a declared framework's app reads
//! whatever that framework is — and a list that is wrong is a builder that cannot
//! read the one page it needed. It cannot send (`may_send` off) and cannot reach
//! the private network, so what it adds is reading public pages; an admin who
//! wants it narrower lists hosts on the agent.
//!
//! `preview_pane` contributes no tool and no prompt: it is what lets the person
//! chatting put the running application beside the conversation and watch it
//! change. It is declared here, with the builder, because the URL it opens on is
//! the application's — nobody else knows it.
//!
//! The prompt is **role and platform** only. How to work — locate, change,
//! check, summarise — is `coding`'s own contribution, which depends on the mode
//! and the edit tool the run is offered and so cannot be written here.

use std::collections::BTreeMap;

use sc_types::Attrs;
use serde_json::Value as Json;

use crate::application::{Application, FrameworkRef};
use crate::build::app_source_in;
use crate::declared::{FrameworkDecl, FrameworkSet, installed_frameworks};
use crate::framework::CODE_FRAMEWORK;
use crate::none::{NONE_FRAMEWORK, none_source_dir};
use crate::react::REACT_FRAMEWORK;

/// The trait that reads, searches, edits and checks a file store's contents.
pub const TRAIT_CODING: &str = "coding";

/// `coding`'s file store setting.
pub const TRAIT_CFG_STORE: &str = "store";
/// `coding`'s sub-directory setting.
pub const TRAIT_CFG_ROOT: &str = "root";
/// `coding`'s "may create and change files" grant.
pub const TRAIT_CFG_MAY_EDIT: &str = "may_edit";
/// `coding`'s "may run the project's scripts" grant.
pub const TRAIT_CFG_MAY_RUN_SCRIPTS: &str = "may_run_scripts";
/// `coding`'s "may run the configured checks" grant.
pub const TRAIT_CFG_MAY_CHECK: &str = "may_check";
/// `coding`'s "may look at the application's preview" grant.
pub const TRAIT_CFG_MAY_VIEW_APP: &str = "may_view_app";
/// `coding`'s "may run shell commands" grant.
pub const TRAIT_CFG_MAY_USE_SHELL: &str = "may_use_shell";
/// `coding`'s application setting: the subdomain its `check` builds and its
/// `view_app` looks at (§13.2). Also what identifies an agent as that
/// application's builder.
pub const TRAIT_CFG_APPLICATION: &str = "application";
/// `coding`'s checks: `package.json` script names, in order.
pub const TRAIT_CFG_CHECKS: &str = "checks";
/// `coding`'s workflow: `direct` or `planned`.
pub const TRAIT_CFG_WORKFLOW: &str = "workflow";
/// `coding`'s edit format.
pub const TRAIT_CFG_EDIT_FORMAT: &str = "edit_format";
/// The [`TRAIT_CFG_WORKFLOW`] a builder agent is created with.
pub const WORKFLOW_PLANNED: &str = "planned";
/// The [`TRAIT_CFG_EDIT_FORMAT`] a builder agent is created with.
pub const EDIT_FORMAT_AUTO: &str = "auto";

/// The trait that fetches web pages, for the documentation a builder reads.
pub const TRAIT_HTTP: &str = "http";
/// `http`'s name setting: the tool is `fetch_<name>`.
pub const TRAIT_CFG_HTTP_NAME: &str = "name";
/// `http`'s "may send POST, PUT, PATCH and DELETE" grant.
pub const TRAIT_CFG_HTTP_MAY_SEND: &str = "may_send";
/// `http`'s "may reach loopback and private-network addresses" grant.
pub const TRAIT_CFG_HTTP_PRIVATE_NETWORK: &str = "private_network";
/// The [`TRAIT_CFG_HTTP_NAME`] a builder agent is created with: `fetch_web`.
pub const HTTP_NAME_WEB: &str = "web";

/// The trait that puts a page beside the conversation (TODO "The preview pane").
pub const TRAIT_PREVIEW_PANE: &str = "preview_pane";
/// `preview_pane`'s URL setting: what the pane opens on.
pub const TRAIT_CFG_PREVIEW_URL: &str = "url";
/// `preview_pane`'s "reload when the agent has finished a turn" setting.
pub const TRAIT_CFG_PREVIEW_RELOAD: &str = "reload_on_turn";

/// The `preview_pane` URL an application's builder is created with: the
/// application's own subdomain on whatever host the admin is being read from.
///
/// `{host}` rather than a base domain, because the base domain is a *server*
/// setting this crate cannot see and the admin already derives an application's
/// URL from its own location (`Applications.tsx`). The pane resolves it in the
/// browser, so one stored agent follows the deployment from `localhost:3000` to
/// the production domain without being rewritten.
pub fn preview_pane_url(app: &Application) -> String {
    format!("//{}.{{host}}", app.subdomain.trim())
}

/// One trait an application's builder agent is created with: which trait, and how
/// it is configured.
///
/// The shape of `sc-agent`'s `EnabledTrait`, restated here because that type is a
/// layer above; the server maps one to the other where it can see both.
#[derive(Debug, Clone, PartialEq)]
pub struct BuilderTrait {
    /// The registered name of the trait.
    pub trait_: String,
    /// Its configuration, keyed by the trait's own settings.
    pub config: Attrs,
}

impl BuilderTrait {
    /// The named trait, with no configuration set.
    pub fn new(trait_: impl Into<String>) -> BuilderTrait {
        BuilderTrait {
            trait_: trait_.into(),
            config: Attrs::new(),
        }
    }

    /// Set one configuration value, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> BuilderTrait {
        self.config.insert(key.into(), value.into());
        self
    }
}

/// The agent an application is created with: its name, what it is for, what it is
/// told it is, and the traits it is granted.
///
/// Deliberately *not* a provider or a model: which LLM a deployment has connected
/// is not something a framework can know, and picking one is the job of whoever
/// creates the record.
#[derive(Debug, Clone, PartialEq)]
pub struct BuilderAgentSpec {
    /// The agent's name — unique, and derived from the application's subdomain so
    /// it is stable and recognisably that application's.
    pub name: String,
    /// One line: what this agent is for.
    pub description: String,
    /// What the agent is told about the application before the conversation
    /// starts.
    pub system_prompt: String,
    /// The traits it is created with, in the order they are offered to the model.
    pub traits: Vec<BuilderTrait>,
}

/// The name the builder agent of `app` is created under.
///
/// Derived from the **subdomain** rather than the display name: the subdomain is
/// unique (§13.2) and an agent's name is unique too, so the derivation cannot
/// collide for two applications; and it is stable under a rename of the app's
/// human-facing name, which agents referenced from triggers and chats should be.
pub fn builder_agent_name(app: &Application) -> String {
    format!("build-{}", app.subdomain.trim())
}

/// The builder agent the framework `fw` declares for `app`, or `None` for a
/// framework that has no application-building agent to declare.
///
/// The registry lookup, mirroring [`framework_config_spec`](crate::framework_config_spec):
/// resolved from the framework's *name*, because an application is created long
/// before it is built and there is no [`Framework`](crate::Framework) instance to
/// ask.
///
/// Both registered frameworks build from a file store, so both declare a coding
/// agent over that store's source directory — but they declare it separately and
/// say different things in the prompt, because what the agent is working on
/// differs: a `react` app is a project Saltcorn scaffolded and whose generated
/// client must not be hand-edited, while a `code` app is whatever the admin
/// brought.
pub fn framework_builder_agent(fw: &FrameworkRef, app: &Application) -> Option<BuilderAgentSpec> {
    builder_agent_in(&installed_frameworks(), fw, app)
}

/// [`framework_builder_agent`] against an explicit framework set.
///
/// A declared framework's prompt is a [`Template`](sc_expr::Template) over its
/// own settings plus the application's name, subdomain, store and project root —
/// the same four a built-in's prompt interpolates, which is why they are named
/// rather than positional. A framework that declares no prompt declares no agent,
/// exactly as a framework with no source tree does: "this application has no
/// builder" is an answer, and the one a framework serving something it does not
/// own should give.
pub fn builder_agent_in(
    set: &FrameworkSet,
    fw: &FrameworkRef,
    app: &Application,
) -> Option<BuilderAgentSpec> {
    let built = || {
        let source = app_source_in(set, fw).ok()?;
        Some((source.store.0, source.build.source_dir))
    };
    match fw.name.as_str() {
        // The type check, then the build that `application` adds.
        REACT_FRAMEWORK => coding_agent(built()?, app, vec!["typecheck".to_owned()], react_prompt),
        // A `code` project is the admin's own: which of its scripts are checks is
        // theirs to say, and until they do `check` tells the model so.
        CODE_FRAMEWORK => coding_agent(built()?, app, Vec::new(), code_prompt),
        // Nothing to build, but a directory to write the files its static
        // directories serve: `check` has no build to run and says so, and its
        // preview is the application as it is served.
        NONE_FRAMEWORK => coding_agent(none_source_dir(fw)?, app, Vec::new(), none_prompt),
        other => {
            let decl = set.find(other)?.clone();
            let checks = decl.checks.clone();
            coding_agent(built()?, app, checks, move |app, store, root| {
                declared_prompt(&decl, &fw_config(app, store, root), app, store, root)
            })
        }
    }
}

/// The names a declared framework's prompt has in scope beyond its own settings.
fn fw_config(app: &Application, store: &str, root: &str) -> BTreeMap<String, String> {
    [
        ("app", app.name.as_str()),
        ("subdomain", app.subdomain.trim()),
        ("store", store),
        ("root", root),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// A declared framework's prompt, rendered — or a one-sentence stand-in when its
/// template will not render, which is a declaration problem the module's card
/// already reports and not a reason for the application to have no builder.
fn declared_prompt(
    decl: &FrameworkDecl,
    extra: &BTreeMap<String, String>,
    app: &Application,
    store: &str,
    root: &str,
) -> String {
    decl.prompt(&app.framework.config, extra)
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            format!(
                "You build the `{}` application, served at the `{}` subdomain, from the \
                 `{store}` file store under `{root}`.",
                app.name,
                app.subdomain.trim()
            )
        })
}

/// A coding agent over `(store, root)` — the framework's source tree — that
/// checks its work by building this one application (TODO §12).
///
/// The caller answers `None` instead when the framework's settings do not
/// resolve to a source tree — which on a saved application means the config was
/// rejected on save, so there is nothing to point an agent at and nothing to
/// report either.
fn coding_agent(
    (store, root): (String, String),
    app: &Application,
    checks: Vec<String>,
    prompt: impl FnOnce(&Application, &str, &str) -> String,
) -> Option<BuilderAgentSpec> {
    Some(BuilderAgentSpec {
        name: builder_agent_name(app),
        description: format!("Builds the `{}` application", app.name),
        system_prompt: prompt(app, &store, &root),
        traits: vec![
            BuilderTrait::new(TRAIT_CODING)
                .with(TRAIT_CFG_STORE, store)
                .with(TRAIT_CFG_ROOT, root)
                // The point of this agent: it exists to change the application's
                // source, check the change, and look at the result.
                .with(TRAIT_CFG_MAY_EDIT, true)
                .with(TRAIT_CFG_MAY_CHECK, true)
                .with(TRAIT_CFG_MAY_VIEW_APP, true)
                // Off, like the trait's own defaults: both execute code the model
                // chose, which is a grant the admin gives deliberately rather than
                // one that arrives with an application.
                .with(TRAIT_CFG_MAY_RUN_SCRIPTS, false)
                .with(TRAIT_CFG_MAY_USE_SHELL, false)
                .with(TRAIT_CFG_APPLICATION, app.subdomain.trim())
                .with(TRAIT_CFG_CHECKS, checks)
                .with(TRAIT_CFG_WORKFLOW, WORKFLOW_PLANNED)
                .with(TRAIT_CFG_EDIT_FORMAT, EDIT_FORMAT_AUTO),
            // The documentation of what it builds with: read-only, public hosts,
            // stated explicitly rather than left to the trait's defaults so the
            // stored agent says what it may do.
            BuilderTrait::new(TRAIT_HTTP)
                .with(TRAIT_CFG_HTTP_NAME, HTTP_NAME_WEB)
                .with(TRAIT_CFG_HTTP_MAY_SEND, false)
                .with(TRAIT_CFG_HTTP_PRIVATE_NETWORK, false),
            // ...and the application itself, beside the conversation. An agent
            // that builds a thing a person looks at should be able to put the
            // thing it built next to what it said about it, and the pane is
            // reloaded when a turn ends because that is exactly when what it is
            // showing has just changed.
            BuilderTrait::new(TRAIT_PREVIEW_PANE)
                .with(TRAIT_CFG_PREVIEW_URL, preview_pane_url(app))
                .with(TRAIT_CFG_PREVIEW_RELOAD, true),
        ],
    })
}

/// What a builder agent is told to do about a sign-up form the application's
/// API does not back: say so, rather than post to a route that 404s only when
/// somebody fills the form in.
const SIGNUP_SWITCH: &str = "When sign-up is off, do not build a sign-up form \
     against a request you wrote yourself: tell the user that an administrator has \
     to turn on \"Allow sign-up\" in the application's REST API settings.";

/// The prompt for a scaffolded React project: the role, and the one platform
/// convention a model would otherwise break on its first edit.
fn react_prompt(app: &Application, store: &str, root: &str) -> String {
    format!(
        "You build the `{name}` application, a React + Vite project served at the \
         `{subdomain}` subdomain, from the `{store}` file store under `{root}`.\n\n\
         `src/feldspar/` is the client generated from the application's API, rewritten \
         on every build: read it to learn what data there is, never edit it, and reach \
         data only through it.\n\n\
         Signing in is `api.login` / `api.logout` / `api.whoami`, wrapped by \
         `src/auth.tsx`. Signing up is `api.signup`, which exists only when the \
         application's REST API settings allow sign-up; `src/feldspar/README.md` says \
         whether they do. {SIGNUP_SWITCH}\n\n\
         Emailed invitation and password-reset links open `/set-password#token=…` \
         (`src/SetPassword.tsx`, calling `api.setPassword`): keep a public page at \
         that path. `api.forgotPassword` sends a reset link; `api.invite` — when \
         the settings allow invitations — makes an account for somebody less \
         powerful than the caller and emails them a link. The README has the details.\n\n\
         {BUILDING}",
        name = app.name,
        subdomain = app.subdomain.trim(),
    )
}

/// What a builder is told about being handed a whole application, which is how
/// the `admin_copilot` agent uses it (§13.6): the tables already exist and are
/// connected, and the brief is the specification.
const BUILDING: &str = "When you are handed an application to build, the tables it needs \
have been created and connected for you, so they are already in `src/feldspar/`. You cannot \
change the schema: if data the brief needs is not in the client, build what you can and say \
exactly which table or field is missing in your report. Replace the scaffold's placeholder \
pages with the real ones, implement every page the brief describes, and run `check` until it \
passes before you report.";

/// The prompt for a `code` application: the same role, without conventions this
/// framework has not got.
fn code_prompt(app: &Application, store: &str, root: &str) -> String {
    format!(
        "You build the `{name}` application, served at the `{subdomain}` subdomain, \
         from the `{store}` file store under `{root}`. The project is the admin's \
         own: read it before changing it rather than assuming a layout.\n\n\
         The application's REST API signs people in with `POST /api/login` (and \
         `/api/logout`, `/api/whoami`); `POST /api/signup` — email and password, \
         answering the new user and signing them in — exists only when its REST \
         API settings allow sign-up. {SIGNUP_SWITCH}\n\n\
         Password links: `POST /api/forgot-password` (`{{ email }}`) emails a reset \
         link, `POST /api/invite` (when the settings allow it) makes an account for \
         somebody less powerful than the caller and emails them one, and both open \
         `/set-password#token=…` in the application, whose page posts \
         `{{ token, password }}` to `/api/set-password`. Keep a page at that path.",
        name = app.name,
        subdomain = app.subdomain.trim(),
    )
}

/// The prompt for a `none` application: there is no project, only files a
/// static directory serves, beside the application's APIs and streams.
///
/// What the record says *now* is written in, because it is what tells the model
/// which of the files it writes anybody will see; the admin changes the record
/// on the application's Settings, and the model cannot.
fn none_prompt(app: &Application, store: &str, root: &str) -> String {
    let root = match root.trim_matches('/') {
        "" => "its root".to_owned(),
        r => format!("`{r}`"),
    };
    let dirs = if app.static_dirs.is_empty() {
        "It serves no static directory yet, so nothing you write is served: tell the \
         user an administrator adds one on the application's Settings, pointing at the \
         directory you work in."
            .to_owned()
    } else {
        let list = app
            .static_dirs
            .iter()
            .map(|d| {
                let what = match d.path.trim_matches('/') {
                    "" => format!("the root of the `{}` file store", d.store.0),
                    p => format!("`{p}` in the `{}` file store", d.store.0),
                };
                format!("- `{}` serves {what}", d.mount)
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Its static directories serve files exactly as they are in the store, and a \
             request for a directory serves its `index.html`:\n{list}"
        )
    };
    let apis = if app.apis.is_empty() {
        "It enables no API.".to_owned()
    } else {
        format!(
            "Its APIs: {}.",
            app.apis
                .iter()
                .map(|a| format!("`{}` at `{}`", a.provider, a.mount))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let streams = if app.streams.is_empty() {
        String::new()
    } else {
        format!(
            " Its streams ({}) are observed over one WebSocket at `{}`: send \
             `{{\"type\":\"subscribe\",\"sub\":\"s1\",\"stream\":\"<name>\"}}` and \
             read `ready`, then `element` frames whose `envelope.value` is the element.",
            app.streams
                .iter()
                .map(|s| format!("`{}`", s.0))
                .collect::<Vec<_>>()
                .join(", "),
            crate::live_socket_path(app),
        )
    };
    format!(
        "You work on the `{name}` application, served at the `{subdomain}` subdomain, \
         in the `{store}` file store under {root}. It has no UI framework and no build \
         step: what you write is served as it is, so write plain HTML, CSS and \
         JavaScript that a browser runs without a bundler.\n\n\
         {dirs}\n\n{apis}{streams}",
        name = app.name,
        subdomain = app.subdomain.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::{CFG_COMMAND, CFG_OUTPUT, CFG_SOURCE, CFG_STORE};
    use crate::react::CFG_PROJECT;

    fn react_app() -> Application {
        Application::new(
            "Todo",
            "todo",
            FrameworkRef::new(REACT_FRAMEWORK)
                .with(CFG_STORE, "apps")
                .with(CFG_PROJECT, "todo"),
        )
    }

    fn code_app() -> Application {
        Application::new(
            "Blog",
            "blog",
            FrameworkRef::new(CODE_FRAMEWORK)
                .with(CFG_STORE, "apps")
                .with(CFG_SOURCE, "web")
                .with(CFG_OUTPUT, "web/dist")
                .with(CFG_COMMAND, "npm run build"),
        )
    }

    #[test]
    fn a_react_app_gets_a_coding_agent_over_its_project_directory() {
        let app = react_app();
        let spec = framework_builder_agent(&app.framework, &app).expect("react declares one");

        assert_eq!(spec.name, "build-todo");
        assert!(spec.description.contains("Todo"), "{}", spec.description);

        // `coding` builds it, `http` reads the documentation, and
        // `preview_pane` shows it: building is one of `coding`'s checks, not a
        // trait of its own.
        let names: Vec<&str> = spec.traits.iter().map(|t| t.trait_.as_str()).collect();
        assert_eq!(names, [TRAIT_CODING, TRAIT_HTTP, TRAIT_PREVIEW_PANE]);
        assert_eq!(
            spec.traits[2].config[TRAIT_CFG_PREVIEW_URL],
            Json::from("//todo.{host}")
        );
        // Reading public pages only: it may not send, nor reach this network.
        let http = &spec.traits[1];
        assert_eq!(http.config[TRAIT_CFG_HTTP_NAME], Json::from("web"));
        assert_eq!(http.config[TRAIT_CFG_HTTP_MAY_SEND], Json::from(false));
        assert_eq!(
            http.config[TRAIT_CFG_HTTP_PRIVATE_NETWORK],
            Json::from(false)
        );
        // The coding trait is scoped to the *derived* project directory, not the
        // store root: an agent that could edit every project in the store would
        // be one grant for every application that shares it.
        let coding = &spec.traits[0];
        assert_eq!(coding.trait_, TRAIT_CODING);
        assert_eq!(coding.config[TRAIT_CFG_STORE], Json::from("apps"));
        assert_eq!(coding.config[TRAIT_CFG_ROOT], Json::from("todo"));
        // It exists to change the source, so editing is on; running the project's
        // other scripts is not part of building and stays the admin's to grant.
        assert_eq!(coding.config[TRAIT_CFG_MAY_EDIT], Json::from(true));
        assert_eq!(
            coding.config[TRAIT_CFG_MAY_RUN_SCRIPTS],
            Json::from(false),
            "running arbitrary scripts is not something an application creation grants"
        );

        // It checks its work — the type check, then the build of the one
        // application it was created for, by subdomain — and looks at the result.
        assert_eq!(coding.config[TRAIT_CFG_MAY_CHECK], Json::from(true));
        assert_eq!(coding.config[TRAIT_CFG_MAY_VIEW_APP], Json::from(true));
        assert_eq!(coding.config[TRAIT_CFG_MAY_USE_SHELL], Json::from(false));
        assert_eq!(coding.config[TRAIT_CFG_APPLICATION], Json::from("todo"));
        assert_eq!(
            coding.config[TRAIT_CFG_CHECKS],
            serde_json::json!(["typecheck"])
        );
        // It plans, and edits in whichever format the model is best at.
        assert_eq!(coding.config[TRAIT_CFG_WORKFLOW], Json::from("planned"));
        assert_eq!(coding.config[TRAIT_CFG_EDIT_FORMAT], Json::from("auto"));

        // The prompt says which application, where its source is, and the
        // convention a model would otherwise break on its first edit.
        let prompt = &spec.system_prompt;
        assert!(prompt.contains("Todo"), "{prompt}");
        assert!(prompt.contains("apps"), "{prompt}");
        assert!(prompt.contains("src/feldspar/"), "{prompt}");
        // Role and platform only: how to work is `coding`'s own prompt.
        assert!(!prompt.contains("Work in small steps"), "{prompt}");
    }

    #[test]
    fn a_code_app_gets_one_over_its_stated_source_directory() {
        let app = code_app();
        let spec = framework_builder_agent(&app.framework, &app).expect("code declares one");

        assert_eq!(spec.name, "build-blog");
        // Every framework's builder reads the web, not only React's.
        assert!(spec.traits.iter().any(|t| t.trait_ == TRAIT_HTTP));
        let coding = &spec.traits[0];
        assert_eq!(coding.config[TRAIT_CFG_STORE], Json::from("apps"));
        // The `code` framework states its source directory rather than deriving
        // it, and that is the directory the agent gets.
        assert_eq!(coding.config[TRAIT_CFG_ROOT], Json::from("web"));
        // No checks are assumed for the admin's own project: `check` says so
        // until they list some. The build still runs, as the application's.
        assert_eq!(coding.config[TRAIT_CFG_CHECKS], serde_json::json!([]));
        assert_eq!(coding.config[TRAIT_CFG_APPLICATION], Json::from("blog"));

        // No React conventions are claimed for a project this framework knows
        // nothing about.
        assert!(!spec.system_prompt.contains("src/feldspar/"));
        assert!(spec.system_prompt.contains("Blog"));
    }

    /// Both builders are told that sign-up is an API setting, and what to say
    /// when it is off — a sign-up form against a route the server does not
    /// serve fails only when somebody fills it in.
    #[test]
    fn builders_are_told_where_signup_is_switched_on() {
        for app in [react_app(), code_app()] {
            let spec = framework_builder_agent(&app.framework, &app).expect("declares one");
            let prompt = &spec.system_prompt;
            assert!(prompt.contains("signup"), "{prompt}");
            assert!(prompt.contains("Allow sign-up"), "{prompt}");
        }
    }

    #[test]
    fn the_name_is_the_subdomain_so_two_applications_cannot_collide() {
        // The subdomain is unique (§13.2) and an agent's name is unique, so the
        // derivation has to be from the one to the other.
        assert_eq!(builder_agent_name(&react_app()), "build-todo");
        assert_eq!(builder_agent_name(&code_app()), "build-blog");
        // And it is stable under a rename of the display name, which is what an
        // agent referenced from a chat or a trigger needs.
        let mut renamed = react_app();
        renamed.name = "To-do list".to_owned();
        assert_eq!(builder_agent_name(&renamed), "build-todo");
    }

    #[test]
    fn a_none_app_gets_a_coding_agent_over_its_directory_with_no_build() {
        use crate::application::{ApiConfig, StaticDir};
        use sc_catalog::FileStoreId;

        let app = Application::new(
            "Landing",
            "landing",
            FrameworkRef::new(NONE_FRAMEWORK)
                .with(CFG_STORE, "site")
                .with(CFG_SOURCE, "public"),
        )
        .with_api(ApiConfig::new("rest", "/api"))
        .with_static_dir(StaticDir::new(
            "/",
            FileStoreId("site".to_owned()),
            "public",
        ));
        let spec = framework_builder_agent(&app.framework, &app).expect("none declares one");

        assert_eq!(spec.name, "build-landing");
        let names: Vec<&str> = spec.traits.iter().map(|t| t.trait_.as_str()).collect();
        assert_eq!(names, [TRAIT_CODING, TRAIT_HTTP, TRAIT_PREVIEW_PANE]);
        let coding = &spec.traits[0];
        assert_eq!(coding.config[TRAIT_CFG_STORE], Json::from("site"));
        assert_eq!(coding.config[TRAIT_CFG_ROOT], Json::from("public"));
        assert_eq!(coding.config[TRAIT_CFG_MAY_EDIT], Json::from(true));
        // Still names the application — it is how the admin finds its builder,
        // and what `view_app` looks at — but there are no checks to run.
        assert_eq!(coding.config[TRAIT_CFG_APPLICATION], Json::from("landing"));
        assert_eq!(coding.config[TRAIT_CFG_CHECKS], serde_json::json!([]));

        // The prompt says there is no build, and where what it writes is served.
        let prompt = &spec.system_prompt;
        assert!(prompt.contains("no build"), "{prompt}");
        assert!(prompt.contains("`/` serves `public`"), "{prompt}");
        assert!(prompt.contains("`rest` at `/api`"), "{prompt}");
        assert!(!prompt.contains("src/feldspar/"), "{prompt}");

        // With no static directory, it is told nothing it writes is served yet.
        let bare = Application::new(
            "Bare",
            "bare",
            FrameworkRef::new(NONE_FRAMEWORK).with(CFG_STORE, "site"),
        );
        let spec = framework_builder_agent(&bare.framework, &bare).expect("declares one");
        assert!(
            spec.system_prompt.contains("serves no static directory"),
            "{}",
            spec.system_prompt
        );
        assert_eq!(spec.traits[0].config[TRAIT_CFG_ROOT], Json::from(""));

        // And with no store there is nowhere to work.
        let unset = Application::new("Api", "api", FrameworkRef::new(NONE_FRAMEWORK));
        assert_eq!(framework_builder_agent(&unset.framework, &unset), None);
    }

    #[test]
    fn a_framework_with_no_source_tree_declares_no_agent() {
        // An unregistered framework — one from a guest language, say — is not
        // assumed to want a coding agent, and nothing here fails over it.
        let app = Application::new("Legacy", "legacy", FrameworkRef::new("saltcorn-v1"));
        assert_eq!(framework_builder_agent(&app.framework, &app), None);

        // Nor does a registered one whose settings do not resolve to a source
        // tree: there is nothing to point the agent at.
        let unset = Application::new("Broken", "broken", FrameworkRef::new(CODE_FRAMEWORK));
        assert_eq!(framework_builder_agent(&unset.framework, &unset), None);
    }
}
