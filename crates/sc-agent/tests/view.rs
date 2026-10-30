//! The preview and browser seam (TODO 6b.4): a trait reaches both through its
//! context, an image it attaches travels with its result, and when the drive
//! stops the run's previews are unmounted and its browser context closed —
//! including when they come from the registry rather than the runner.

use crate::common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::catalog;
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, AgentRegistry, AgentTrait, AppPreviewer, BrowserAction, BrowserDriver, BrowserReport,
    BrowserRequest, EnabledTrait, PreviewInfo, RunCaller, RunId, Runner, ToolsContext,
    TraitContext,
};
use sc_auth::User;
use sc_error::Result;
use sc_llm::{ImagePart, LlmMessage, ToolSpec};
use sc_test_harness::TestDb;
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

/// What the fake server was asked to do, in order.
#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl Log {
    fn push(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct Previews(Arc<Log>);

#[async_trait::async_trait]
impl AppPreviewer for Previews {
    async fn mount_preview(
        &self,
        _run: RunId,
        subdomain: &str,
        _output_dir: &Path,
    ) -> Result<PreviewInfo> {
        self.0.push(format!("mount {subdomain}"));
        Ok(PreviewInfo {
            subdomain: subdomain.to_owned(),
            label: "abc".to_owned(),
            host: format!("abc--{subdomain}.example.com"),
        })
    }
    fn preview(&self, _run: RunId, _subdomain: &str) -> Option<PreviewInfo> {
        None
    }
    fn unmount_previews(&self, _run: RunId) {
        self.0.push("unmount".to_owned());
    }
}

struct Browser(Arc<Log>);

#[async_trait::async_trait]
impl BrowserDriver for Browser {
    async fn act(&self, request: BrowserRequest<'_>) -> Result<BrowserReport> {
        self.0.push(format!("act {}", request.action.name()));
        Ok(BrowserReport {
            url: format!(
                "http://{}/",
                request
                    .target
                    .preview
                    .as_ref()
                    .map_or("", |p| p.host.as_str())
            ),
            screenshot: Some(vec![0xff, 0xd8, 0xff]),
            ..BrowserReport::default()
        })
    }
    fn close(&self, _context: uuid::Uuid) {
        self.0.push("close".to_owned());
    }
}

/// A trait with one tool that mounts a preview, takes a screenshot through the
/// browser and attaches it.
struct Look;

#[async_trait::async_trait]
impl AgentTrait for Look {
    fn name(&self) -> &str {
        "look"
    }
    fn description(&self) -> &str {
        "looks"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "look",
            "look",
            json!({"type": "object", "properties": {}}),
        )]
    }
    async fn call(
        &self,
        _config: &Attrs,
        _tool: &str,
        _args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let preview = ctx
            .require_previews()?
            .mount_preview(ctx.run, "todo", Path::new("/nowhere"))
            .await?;
        let user = User::new(uuid::Uuid::new_v4(), 1)?;
        let report = ctx
            .require_browser()?
            .act(BrowserRequest {
                context: ctx.run.0,
                target: &preview.target(ctx.run),
                user: Some(&user),
                action: BrowserAction::Screenshot { full_page: false },
                timeout: std::time::Duration::from_secs(5),
            })
            .await?;
        ctx.attach_image(ImagePart::new(
            "image/jpeg",
            report.screenshot.unwrap_or_default(),
        ));
        Ok(Json::String(format!("looked at {}", report.url)))
    }
}

fn registry() -> Result<AgentRegistry> {
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(Look))?;
    Ok(registry)
}

fn agent() -> Agent {
    Agent::new("viewer", "main").with_trait(EnabledTrait::new("look"))
}

#[tokio::test]
async fn a_trait_reaches_the_seam_its_image_is_kept_and_the_run_end_releases_both() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry()?;
    let agent = agent();
    let log = Arc::new(Log::default());
    let previews: Arc<dyn AppPreviewer> = Arc::new(Previews(log.clone()));
    let browser: Arc<dyn BrowserDriver> = Arc::new(Browser(log.clone()));
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("look", json!({})),
        Reply::says("seen"),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .with_previews(&previews)
    .with_browser(&browser);

    let (run, conclusion) = runner.start("look at it").await?;
    assert_eq!(conclusion.answer(), Some("seen"));
    assert_eq!(
        log.lines(),
        ["mount todo", "act screenshot", "unmount", "close"],
        "released once, after the drive"
    );

    // The screenshot went to the model with the result, and is in the run.
    let second = &provider.requests()[1];
    let images = second.messages.iter().find_map(|m| match m {
        LlmMessage::ToolResult { images, .. } => Some(images.clone()),
        _ => None,
    });
    assert_eq!(images.map(|i| i.len()), Some(1));
    let stored = run.agent_loop()?;
    assert!(stored.messages().iter().any(|m| matches!(
        m,
        LlmMessage::ToolResult { images, content, .. }
            if images.len() == 1 && content.starts_with("looked at http://abc--todo")
    )));
    Ok(())
}

#[tokio::test]
async fn without_the_seam_a_tool_is_told_by_name_and_the_registry_supplies_it() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let agent = agent();

    // No previewer anywhere: the tool's error names what is missing.
    let registry = registry()?;
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("look", json!({})),
        Reply::says("no"),
    ]));
    Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .start("look")
    .await?;
    let second = &provider.requests()[1];
    assert!(second.messages.iter().any(|m| matches!(
        m,
        LlmMessage::ToolResult { content, .. } if content.contains("application previews")
    )));

    // Installed on the registry once, a runner built without `with_*` uses them.
    let log = Arc::new(Log::default());
    registry
        .view_services()
        .set_previews(Arc::new(Previews(log.clone())));
    registry
        .view_services()
        .set_browser(Arc::new(Browser(log.clone())));
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("look", json!({})),
        Reply::says("yes"),
    ]));
    Runner::new(
        &catalog,
        &registry.clone(),
        &agent,
        common::model(provider),
        RunCaller::system(),
    )
    .start("look")
    .await?;
    assert_eq!(
        log.lines(),
        ["mount todo", "act screenshot", "unmount", "close"]
    );
    Ok(())
}
