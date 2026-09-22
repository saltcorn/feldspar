//! `list_assets` and the mounts in the session header (TODO "Static
//! directories" §6, 3.3).
//!
//! The half of the milestone that lives in this crate: a `coding` trait whose
//! `application` setting names an application knows about that application's
//! static directories, and says so before the model asks. That the URLs it
//! builds are the URLs the **router** answers is pinned where both halves can
//! be run against one fixture — `sc-server`'s `app_static_dirs`.
//!
//! What is asserted here is the part a server is not needed for:
//!
//! - the mounts are in the session header, so a model that has never heard of
//!   the tool still knows the images exist and where they are;
//! - an application with no static directories contributes no header line and
//!   no noise, and the tool says what is missing rather than returning a bare
//!   empty list;
//! - the walk is the caller's: a store closed below the caller's role puts
//!   neither a line in the header nor a row in the result.

use crate::common;

use std::sync::Arc;

use common::{Env, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, EnabledTrait, RunCaller, Runner, save_agent};
use sc_app::{Application, FrameworkRef, StaticDir, save_application};
use sc_catalog::FileStoreId;
use sc_core_traits::{CFG_APPLICATION, CFG_ROOT, CFG_STORE};
use sc_error::Result;
use serde_json::{Value as Json, json};

/// An application whose source is in `apps/web` and whose images are in a
/// **second** store — which is the case `find_files` cannot answer for.
fn blog(static_dirs: Vec<StaticDir>) -> Application {
    let mut app = Application::new(
        "Blog",
        "blog",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_file_store(FileStoreId("media".to_owned()));
    app.static_dirs = static_dirs;
    app
}

/// The `coding` configuration an application's builder agent is given.
fn coding(application: &str) -> sc_types::Attrs {
    config(&[
        (CFG_STORE, json!("apps")),
        (CFG_ROOT, json!("web")),
        (CFG_APPLICATION, json!(application)),
    ])
}

/// The tool's result for one call, as `caller`.
async fn list(env: &Env, config: &sc_types::Attrs, args: Json, role: u8) -> Result<Json> {
    let caller = RunCaller {
        role,
        ..RunCaller::system()
    };
    env.call_tool("coding", config, "list_assets_apps_web", args, &caller)
        .await
}

/// The header of one session, built by running the agent for a turn.
async fn session_header(env: &Env, agent: &Agent, brief: &str) -> Result<String> {
    save_agent(&env.catalog, &env.registry, agent).await?;
    let provider = Arc::new(FakeProvider::new([Reply::says("ok")]));
    Runner::new(
        &env.catalog,
        &env.registry,
        agent,
        sc_llm::ConnectedModel::unconfigured(provider.clone()),
        RunCaller::system(),
    )
    .start(brief)
    .await?;
    Ok(provider.session_header(0).unwrap_or_default())
}

#[tokio::test]
async fn the_session_header_names_the_mounts_and_the_tool_lists_them() -> Result<()> {
    let env = Env::new().await?;
    let apps = env.with_file_store("apps", None).await?;
    env.put(&apps, "web/src/App.tsx", "export function App() {}\n")?;
    let media = env.with_file_store("media", None).await?;
    env.put(&media, "images/hero.png", "\u{89}PNG hero")?;
    env.put(&media, "images/icons/save.svg", "<svg/>")?;
    env.put(&media, "handbook/intro.md", "# Intro\n")?;

    save_application(
        &env.catalog,
        &blog(vec![
            StaticDir::new("/img", FileStoreId("media".to_owned()), "images"),
            StaticDir::new("/docs", FileStoreId("media".to_owned()), "handbook"),
        ]),
    )
    .await?;

    // --- the tool -------------------------------------------------------------
    let listed = list(&env, &coding("blog"), json!({}), 1).await?;
    let urls: Vec<&str> = listed["assets"]
        .as_array()
        .expect("assets")
        .iter()
        .map(|a| a["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        ["/docs/intro.md", "/img/hero.png", "/img/icons/save.svg"],
        "{listed}"
    );
    assert_eq!(listed["truncated"], json!(false), "{listed}");
    let icon = listed["assets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["url"] == json!("/img/icons/save.svg"))
        .expect("the icon");
    // The path is the store's, the URL is the application's, and the two are
    // different strings — which is the whole reason this tool exists.
    assert_eq!(icon["path"], json!("images/icons/save.svg"));
    assert_eq!(icon["store"], json!("media"));
    assert_eq!(icon["content_type"], json!("image/svg+xml"));

    // A glob over names, as `find_files` matches one.
    let svgs = list(&env, &coding("blog"), json!({"pattern": "*.svg"}), 1).await?;
    assert_eq!(svgs["assets"].as_array().unwrap().len(), 1, "{svgs}");

    // --- the header -----------------------------------------------------------
    let agent = Agent::new("builder", "main").with_trait(
        EnabledTrait::new("coding")
            .config(CFG_STORE, "apps")
            .config(CFG_ROOT, "web")
            .config(CFG_APPLICATION, "blog"),
    );
    let header = session_header(&env, &agent, "Put the hero image on the landing page").await?;
    assert!(
        header.contains("/img \u{2192} store \"media\" (images), 2 files"),
        "{header}"
    );
    assert!(
        header.contains("/docs \u{2192} store \"media\" (handbook), 1 file"),
        "{header}"
    );
    assert!(header.contains("served by `blog`"), "{header}");
    Ok(())
}

#[tokio::test]
async fn an_application_with_no_static_directories_says_so_and_adds_no_header() -> Result<()> {
    let env = Env::new().await?;
    let apps = env.with_file_store("apps", None).await?;
    env.put(&apps, "web/src/App.tsx", "export function App() {}\n")?;
    save_application(&env.catalog, &blog(Vec::new())).await?;

    let listed = list(&env, &coding("blog"), json!({}), 1).await?;
    assert_eq!(listed["assets"], json!([]), "{listed}");
    // Not a bare empty list: a model told only `[]` will guess a URL anyway.
    assert!(
        listed["note"]
            .as_str()
            .unwrap_or_default()
            .contains("no static directories"),
        "{listed}"
    );

    let agent = Agent::new("builder", "main").with_trait(
        EnabledTrait::new("coding")
            .config(CFG_STORE, "apps")
            .config(CFG_ROOT, "web")
            .config(CFG_APPLICATION, "blog"),
    );
    let header = session_header(&env, &agent, "Add a footer").await?;
    assert!(!header.contains("<static_dirs>"), "{header}");
    Ok(())
}

/// A mount is not a grant on the way in either: a store the caller may not read
/// is not listed to them, and does not appear in their header.
#[tokio::test]
async fn a_store_closed_to_the_caller_is_neither_listed_nor_announced() -> Result<()> {
    let env = Env::new().await?;
    let apps = env.with_file_store("apps", None).await?;
    env.put(&apps, "web/src/App.tsx", "export function App() {}\n")?;
    // The images store admits nobody below role 1.
    let media = env.with_file_store("media", Some(1)).await?;
    env.put(&media, "images/hero.png", "\u{89}PNG hero")?;

    save_application(
        &env.catalog,
        &blog(vec![StaticDir::new(
            "/img",
            FileStoreId("media".to_owned()),
            "images",
        )]),
    )
    .await?;

    let closed = list(&env, &coding("blog"), json!({}), 40).await?;
    assert_eq!(closed["assets"], json!([]), "{closed}");
    let open = list(&env, &coding("blog"), json!({}), 1).await?;
    assert_eq!(open["assets"].as_array().unwrap().len(), 1, "{open}");
    Ok(())
}

/// The trait that names no application offers no `list_assets`, because there
/// is no set of static directories for it to be about.
#[tokio::test]
async fn without_an_application_the_tool_is_not_in_the_spec_list() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    save_application(&env.catalog, &blog(Vec::new())).await?;

    let names = |config: &sc_types::Attrs| -> Vec<String> {
        env.tools("coding", config)
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    assert!(names(&coding("blog")).contains(&"list_assets_apps_web".to_owned()));
    let bare = config(&[(CFG_STORE, json!("apps")), (CFG_ROOT, json!("web"))]);
    assert!(
        !names(&bare).contains(&"list_assets_apps_web".to_owned()),
        "{:?}",
        names(&bare)
    );
    // Called anyway — a resumed transcript can carry the call — it names the
    // setting an administrator would have to fill in.
    let err = list(&env, &bare, json!({}), 1)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("application"), "{err}");
    Ok(())
}
