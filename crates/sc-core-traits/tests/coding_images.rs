//! `view_image`: a model that can see is shown an image from the code, or one
//! the application serves out of a static directory.
//!
//! The two places an agent building a page meets an image are pinned together
//! here, over one fixture: a logo under the project's `public/`, named by its
//! path the way `read_file` names a file, and a hero image in a **second** store
//! behind a static directory, named by the URL `list_assets` gives — which is
//! the only name the agent has for it. A URL the router would not serve is not
//! shown either, and neither is a file the caller may not read.

use crate::common;

use std::io::Cursor;

use common::{Env, config};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use sc_agent::{RunCaller, RunId, RunMode, ToolsContext, TraitContext};
use sc_app::{Application, FrameworkRef, StaticDir, save_application};
use sc_catalog::FileStoreId;
use sc_core_traits::{
    CFG_APPLICATION, CFG_MAY_VIEW_APP, CFG_ROOT, CFG_STORE, FileScope, tool_names,
};
use sc_error::Result;
use sc_llm::{ImagePart, ModelCapabilities};
use serde_json::{Value as Json, json};

/// An opaque PNG of `width` × `height`.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(
        width,
        height,
        Rgba([20, 90, 200, 255]),
    ))
    .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
    .unwrap();
    out
}

fn write(dir: &std::path::Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The code in `apps/web`, the images in `media/images` served at `/img`.
fn blog() -> Application {
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
    app.static_dirs = vec![StaticDir::new(
        "/img",
        FileStoreId("media".to_owned()),
        "images",
    )];
    app
}

fn coding() -> sc_types::Attrs {
    config(&[
        (CFG_STORE, json!("apps")),
        (CFG_ROOT, json!("web")),
        (CFG_APPLICATION, json!("blog")),
    ])
}

/// One call of `view_image_apps_web` as a caller of `role`: its text and the
/// images it attached.
async fn look(env: &Env, args: Json, role: u8) -> (Result<String>, Vec<ImagePart>) {
    let coding_trait = env.registry.require("coding").unwrap().clone();
    let caller = RunCaller {
        role,
        ..RunCaller::system()
    };
    let mut state = Json::Null;
    let mut ctx = TraitContext {
        catalog: &env.catalog,
        caller: &caller,
        agent: "builder",
        run: RunId::new(),
        mode: RunMode::Plan,
        trait_state: &mut state,
        evaluator: None,
        triggers: None,
        delegate: None,
        previews: None,
        browser: None,
        requests: None,
        modules: None,
        signals: Vec::new(),
        images: Vec::new(),
        extras: Default::default(),
    };
    let result = coding_trait
        .call(&coding(), "view_image_apps_web", &args, &mut ctx)
        .await
        .map(|j| j.as_str().unwrap_or_default().to_owned());
    (result, ctx.images)
}

async fn fixture(media_floor: Option<u8>) -> Result<(Env, Vec<u8>)> {
    let env = Env::new().await?;
    let apps = env.with_file_store("apps", None).await?;
    let logo = png(30, 10);
    write(&apps, "web/public/logo.png", &logo);
    env.put(&apps, "web/src/App.tsx", "export function App() {}\n")?;
    let media = env.with_file_store("media", media_floor).await?;
    write(&media, "images/hero.png", &png(2000, 1000));
    write(&media, "secret.png", &png(4, 4));
    save_application(&env.catalog, &blog()).await?;
    Ok((env, logo))
}

#[tokio::test]
async fn an_image_in_the_code_and_one_the_application_serves_are_both_shown() -> Result<()> {
    let (env, logo) = fixture(None).await?;

    // The code's own, by path, sent as it is.
    let (text, images) = look(&env, json!({"path": "public/logo.png"}), 1).await;
    let text = text?;
    assert!(text.starts_with("view_image public/logo.png\n"), "{text}");
    assert!(text.contains("30×10 PNG"), "{text}");
    assert!(text.ends_with(": attached"), "{text}");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].media_type, "image/png");
    assert_eq!(images[0].data, logo);

    // The application's, by the URL `list_assets` gives, scaled to what a model
    // takes and said to be.
    let (text, images) = look(&env, json!({"url": "/img/hero.png"}), 1).await;
    let text = text?;
    assert!(
        text.starts_with("view_image /img/hero.png (`images/hero.png` in store `media`)"),
        "{text}"
    );
    assert!(
        text.contains("2000×1000 PNG") && text.contains("scaled to 1568×784"),
        "{text}"
    );
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].media_type, "image/jpeg");

    // A URL as the page has it — absolute, with a cache-buster — is the same
    // image.
    let (text, _) = look(
        &env,
        json!({"url": "https://blog.example.com/img/hero.png?v=3"}),
        1,
    )
    .await;
    assert!(text?.contains("in store `media`"));

    // Where the router answers 404, so does this: out of the directory, and
    // under no mount at all — naming the tool that lists what is served.
    for url in [
        "/img/../secret.png",
        "/img/%2e%2e/secret.png",
        "/secret.png",
    ] {
        let (err, images) = look(&env, json!({"url": url}), 1).await;
        let err = err.unwrap_err().to_string();
        assert!(err.contains("list_assets_apps_web"), "{url}: {err}");
        assert!(images.is_empty());
    }

    // Exactly one of the two names.
    let (err, _) = look(
        &env,
        json!({"path": "public/logo.png", "url": "/img/hero.png"}),
        1,
    )
    .await;
    assert!(err.unwrap_err().to_string().contains("exactly one"));

    // Text is not an image; and `read_file` on the image points here.
    let (err, _) = look(&env, json!({"path": "src/App.tsx"}), 1).await;
    let err = err.unwrap_err().to_string();
    assert!(err.contains("not a PNG, JPEG, GIF or WebP"), "{err}");
    let caller = RunCaller::system();
    let err = env
        .call_tool(
            "coding",
            &coding(),
            "read_file_apps_web",
            json!({"path": "public/logo.png"}),
            &caller,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("view_image_apps_web"), "{err}");
    Ok(())
}

/// A mount is not a grant: an image in a store the caller may not read is not
/// shown to them, as the router does not serve it to them.
#[tokio::test]
async fn an_image_closed_to_the_caller_is_not_shown() -> Result<()> {
    let (env, _) = fixture(Some(1)).await?;
    let (err, images) = look(&env, json!({"url": "/img/hero.png"}), 40).await;
    assert!(err.is_err());
    assert!(images.is_empty());
    let (ok, images) = look(&env, json!({"url": "/img/hero.png"}), 1).await;
    ok?;
    assert_eq!(images.len(), 1);
    Ok(())
}

/// Offered to a model that can see, in every mode — it reads — and to no other.
/// And `view_app` in `plan`, where it only looks.
#[tokio::test]
async fn view_image_is_offered_only_to_a_model_with_vision() -> Result<()> {
    let (env, _) = fixture(None).await?;
    let coding_trait = env.registry.require("coding")?.clone();
    let mut config = coding();
    config.insert(CFG_MAY_VIEW_APP.to_owned(), json!(true));
    let scope = FileScope {
        store: "apps".to_owned(),
        root: "web".to_owned(),
    };
    let seeing = ModelCapabilities::built_in(sc_llm::ANTHROPIC_BACKEND, "claude-sonnet-4-5");
    let blind = ModelCapabilities::built_in("", "");
    assert!(seeing.vision && !blind.vision);

    for mode in [RunMode::Plan, RunMode::Act, RunMode::Explore] {
        let names = |caps: &ModelCapabilities| -> Vec<String> {
            coding_trait
                .tools(&ToolsContext::new(&env.catalog, mode, caps), &config)
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        assert!(
            names(&seeing).contains(&tool_names::view_image(&scope)),
            "{mode}: {:?}",
            names(&seeing)
        );
        assert!(
            !names(&blind).contains(&tool_names::view_image(&scope)),
            "{mode}"
        );
    }

    // `plan` looks at the page with `view_app`, and cannot click on it.
    let plan = coding_trait.tools(
        &ToolsContext::new(&env.catalog, RunMode::Plan, &seeing),
        &config,
    );
    let view_app = plan
        .iter()
        .find(|t| t.name == tool_names::view_app(&scope))
        .expect("view_app in plan");
    let actions = view_app.parameters["properties"]["action"]["enum"].to_string();
    assert!(
        actions.contains("screenshot") && actions.contains("goto"),
        "{actions}"
    );
    assert!(
        !actions.contains("click") && !actions.contains("fill"),
        "{actions}"
    );
    let prompt = coding_trait
        .prompt(
            &ToolsContext::new(&env.catalog, RunMode::Plan, &seeing),
            &config,
        )
        .unwrap_or_default();
    assert!(prompt.contains("view_app_apps_web"), "{prompt}");
    Ok(())
}
