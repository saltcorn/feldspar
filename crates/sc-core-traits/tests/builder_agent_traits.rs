//! The builder agent a framework declares (`sc_app::framework_builder_agent`)
//! names traits and settings that live **here**, one layer up — so this is the
//! only place both halves are visible at once, and therefore the only place the
//! two can be held to each other.
//!
//! `sc-app` is layer 8 and the traits are layer 9: a framework can name `coding`
//! and `may_edit` as strings and nothing more, so nothing in `sc-app` fails to
//! compile if a trait is renamed or a setting changes shape. What would happen
//! instead is that every application created afterwards would report an
//! `agent_error` the admin cannot act on. These tests are what turns that into a
//! build failure.
//!
//! No database: this is the structural half — the names resolve, and the
//! configuration validates against the specs the traits declare. Whether the
//! store exists and the agent saves is asserted end-to-end over HTTP in
//! `sc-server`'s `app_builder_agent` test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_app::{
    Application, BuilderAgentSpec, CFG_COMMAND, CFG_OUTPUT, CFG_PROJECT, CFG_SOURCE, CFG_STORE,
    CODE_FRAMEWORK, EDIT_FORMAT_AUTO, FrameworkRef, REACT_FRAMEWORK, TRAIT_CFG_APPLICATION,
    TRAIT_CFG_CHECKS, TRAIT_CFG_EDIT_FORMAT, TRAIT_CFG_MAY_CHECK, TRAIT_CFG_MAY_EDIT,
    TRAIT_CFG_MAY_RUN_SCRIPTS, TRAIT_CFG_MAY_USE_SHELL, TRAIT_CFG_MAY_VIEW_APP,
    TRAIT_CFG_PREVIEW_RELOAD, TRAIT_CFG_PREVIEW_URL, TRAIT_CFG_ROOT, TRAIT_CFG_STORE,
    TRAIT_CFG_WORKFLOW, TRAIT_CODING, TRAIT_PREVIEW_PANE, WORKFLOW_PLANNED,
    framework_builder_agent,
};
use sc_types::validate_attrs;
use serde_json::json;

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

fn spec_for(app: &Application) -> BuilderAgentSpec {
    framework_builder_agent(&app.framework, app).expect("a code framework declares a builder agent")
}

#[test]
fn every_trait_a_framework_declares_is_a_registered_one_configured_as_it_declares() {
    let registry = sc_core_traits::builtin_traits().expect("the built-in traits assemble");

    for app in [react_app(), code_app()] {
        let spec = spec_for(&app);
        assert!(
            !spec.traits.is_empty(),
            "a builder agent with no traits could not build anything"
        );
        for enabled in &spec.traits {
            // The name resolves — the check that a renamed trait breaks here
            // rather than on the next application an admin creates.
            let trait_ = registry.require(&enabled.trait_).unwrap_or_else(|e| {
                panic!(
                    "framework `{}` declares `{}`: {e}",
                    app.framework.name, enabled.trait_
                )
            });
            // ...and every setting it declares is one that trait has, of the
            // type it declares it with. This is `validate_agent`'s own check,
            // run without a database.
            validate_attrs(&trait_.config_spec(), &enabled.config).unwrap_or_else(|e| {
                panic!(
                    "framework `{}`'s `{}` configuration: {e}",
                    app.framework.name, enabled.trait_
                )
            });
        }
    }
}

#[test]
fn the_grants_and_the_build_target_are_the_settings_the_coding_trait_means() {
    // The names are strings on the `sc-app` side, so assert they are the *same*
    // strings the trait uses — a settings rename that only touched the trait
    // would otherwise leave a builder agent that reads its source and cannot
    // change it, which is a silent loss of the capability, not an error.
    let registry = sc_core_traits::builtin_traits().expect("the built-in traits assemble");
    assert!(registry.get(TRAIT_CODING).is_some());
    for (declared, real) in [
        (TRAIT_CFG_STORE, sc_core_traits::CFG_STORE),
        (TRAIT_CFG_ROOT, sc_core_traits::CFG_ROOT),
        (TRAIT_CFG_MAY_EDIT, sc_core_traits::CFG_MAY_EDIT),
        (
            TRAIT_CFG_MAY_RUN_SCRIPTS,
            sc_core_traits::CFG_MAY_RUN_SCRIPTS,
        ),
        (TRAIT_CFG_MAY_CHECK, sc_core_traits::CFG_MAY_CHECK),
        (TRAIT_CFG_MAY_VIEW_APP, sc_core_traits::CFG_MAY_VIEW_APP),
        (TRAIT_CFG_MAY_USE_SHELL, sc_core_traits::CFG_MAY_USE_SHELL),
        (TRAIT_CFG_APPLICATION, sc_core_traits::CFG_APPLICATION),
        (TRAIT_CFG_CHECKS, sc_core_traits::CFG_CHECKS),
        (TRAIT_CFG_WORKFLOW, sc_core_traits::CFG_WORKFLOW),
        (TRAIT_CFG_EDIT_FORMAT, sc_core_traits::CFG_EDIT_FORMAT),
        (WORKFLOW_PLANNED, sc_core_traits::WORKFLOW_PLANNED),
        (EDIT_FORMAT_AUTO, sc_core_traits::EDIT_FORMAT_AUTO),
    ] {
        assert_eq!(declared, real);
    }

    // And the values a created application carries (§12): `coding`, which may
    // edit, check and look at the application it was created to build, starts
    // by planning, and may not run scripts or a shell.
    let app = react_app();
    let spec = spec_for(&app);
    assert_eq!(spec.traits.len(), 4, "{:?}", spec.traits);
    let coding = &spec.traits[0];
    assert_eq!(coding.trait_, TRAIT_CODING);
    for (key, value) in [
        (TRAIT_CFG_MAY_EDIT, json!(true)),
        (TRAIT_CFG_MAY_CHECK, json!(true)),
        (TRAIT_CFG_MAY_VIEW_APP, json!(true)),
        (TRAIT_CFG_MAY_RUN_SCRIPTS, json!(false)),
        (TRAIT_CFG_MAY_USE_SHELL, json!(false)),
        (TRAIT_CFG_APPLICATION, json!(app.subdomain)),
        (TRAIT_CFG_CHECKS, json!(["typecheck"])),
        (TRAIT_CFG_WORKFLOW, json!("planned")),
        (TRAIT_CFG_EDIT_FORMAT, json!("auto")),
    ] {
        assert_eq!(coding.config[key], value, "`{key}`");
    }
}

#[test]
fn the_preview_pane_a_builder_carries_opens_on_the_application_it_builds() {
    // The other half of the same rename risk: `preview_pane` and its `url` are
    // strings on the `sc-app` side, and an agent naming a trait that is not
    // registered does not save at all — so the application would be created
    // with no builder rather than with one that cannot show its work.
    for (declared, real) in [
        (TRAIT_CFG_PREVIEW_URL, sc_core_traits::CFG_URL),
        (TRAIT_CFG_PREVIEW_RELOAD, sc_core_traits::CFG_RELOAD_ON_TURN),
    ] {
        assert_eq!(declared, real);
    }

    let app = code_app();
    let spec = spec_for(&app);
    let pane = spec
        .traits
        .iter()
        .find(|t| t.trait_ == TRAIT_PREVIEW_PANE)
        .expect("a builder agent carries a preview pane");
    // The app's own subdomain, on whatever host the admin is open on: the
    // stored agent must not pin the deployment's domain.
    assert_eq!(pane.config[TRAIT_CFG_PREVIEW_URL], json!("//blog.{base}"));
    assert_eq!(pane.config[TRAIT_CFG_PREVIEW_RELOAD], json!(true));
    // And it is a URL the trait itself accepts, which is what `save_agent`
    // will run over it.
    assert_eq!(
        sc_core_traits::configured_url(&pane.config).expect("the pane's URL validates"),
        "//blog.{base}"
    );
}

#[test]
fn the_scope_a_framework_declares_is_one_the_coding_trait_resolves() {
    // The store and the sub-directory are what every one of that trait's tool
    // names is derived from (`edit_file_apps_todo`), so a scope it cannot
    // resolve is an agent that cannot be saved.
    let app = react_app();
    let spec = spec_for(&app);
    let coding = spec
        .traits
        .iter()
        .find(|t| t.trait_ == TRAIT_CODING)
        .expect("the coding trait");
    let scope = sc_core_traits::configured_scope(&coding.config).expect("the scope resolves");
    assert_eq!(scope.store, "apps");
    assert_eq!(scope.root, "todo");

    // Every derived name is one a provider will accept — the length rule
    // `save_agent` applies, checked here where the derivation is declared.
    for name in sc_core_traits::tool_names::coding(&scope) {
        assert!(!name.is_empty());
        assert!(name.len() <= 64, "tool `{name}` is {} chars", name.len());
    }
}

#[test]
fn the_web_a_builder_reads_is_the_http_trait_read_only_over_public_hosts() {
    use sc_app::{
        HTTP_NAME_WEB, TRAIT_CFG_HTTP_MAY_SEND, TRAIT_CFG_HTTP_NAME,
        TRAIT_CFG_HTTP_PRIVATE_NETWORK, TRAIT_HTTP,
    };
    // The same rename risk as the pane's: strings on the `sc-app` side, held to
    // the trait's own here.
    for (declared, real) in [
        (TRAIT_CFG_HTTP_NAME, sc_core_traits::http::CFG_NAME),
        (TRAIT_CFG_HTTP_MAY_SEND, sc_core_traits::http::CFG_MAY_SEND),
        (
            TRAIT_CFG_HTTP_PRIVATE_NETWORK,
            sc_core_traits::http::CFG_PRIVATE_NETWORK,
        ),
        (HTTP_NAME_WEB, sc_core_traits::http::DEFAULT_NAME),
    ] {
        assert_eq!(declared, real);
    }
    let registry = sc_core_traits::builtin_traits().expect("the built-in traits assemble");
    assert!(registry.get(TRAIT_HTTP).is_some());

    for app in [react_app(), code_app()] {
        let spec = spec_for(&app);
        let http = spec
            .traits
            .iter()
            .find(|t| t.trait_ == TRAIT_HTTP)
            .expect("a builder agent reads the web");
        // Reading public pages is the whole grant: no sending, no private
        // network, and no headers — so no key comes with an application.
        assert_eq!(http.config[TRAIT_CFG_HTTP_MAY_SEND], json!(false));
        assert_eq!(http.config[TRAIT_CFG_HTTP_PRIVATE_NETWORK], json!(false));
        assert!(!http.config.contains_key(sc_core_traits::http::CFG_HEADERS));
        // The tool it offers is the one the tutorial and the prompt call it,
        // and it does not collide with any of `coding`'s.
        let name = http.config[TRAIT_CFG_HTTP_NAME].as_str().unwrap();
        let tool = sc_core_traits::tool_names::http(name);
        assert_eq!(tool, "fetch_web");
        let coding = spec
            .traits
            .iter()
            .find(|t| t.trait_ == TRAIT_CODING)
            .unwrap();
        let scope = sc_core_traits::configured_scope(&coding.config).unwrap();
        assert!(!sc_core_traits::tool_names::coding(&scope).contains(&tool));
    }
}

#[test]
fn a_builder_reads_this_servers_documentation_through_the_docs_trait() {
    use sc_app::TRAIT_DOCS;
    let registry = sc_core_traits::builtin_traits().expect("the built-in traits assemble");
    assert!(registry.get(TRAIT_DOCS).is_some());

    for app in [react_app(), code_app()] {
        let spec = spec_for(&app);
        let docs = spec
            .traits
            .iter()
            .find(|t| t.trait_ == TRAIT_DOCS)
            .expect("a builder agent reads the server's documentation");
        assert!(docs.config.is_empty());
        // Its two tools collide with none of `coding`'s, nor with `fetch_web`.
        let coding = spec
            .traits
            .iter()
            .find(|t| t.trait_ == TRAIT_CODING)
            .unwrap();
        let scope = sc_core_traits::configured_scope(&coding.config).unwrap();
        let coding_tools = sc_core_traits::tool_names::coding(&scope);
        for tool in sc_core_traits::tool_names::docs {
            assert!(!coding_tools.iter().any(|t| t == tool), "{tool}");
            assert_ne!(tool, sc_core_traits::tool_names::http("web"));
        }
    }
}
