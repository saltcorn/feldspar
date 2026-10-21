//! The Analytics UI as an application framework (analytics TODO A9.2–A9.3).
//!
//! An application whose framework is `analytics` serves the Analytics UI's own
//! bundle — the one the admin host serves under `/analytics/` — on its
//! subdomain, to its own users, in a restricted shell. Three pieces:
//!
//! - [`AnalyticsFactory`], the framework in `sc-app`'s registry: its settings
//!   ([`sc_analytics::app::analytics_config_spec`]), their checks — the
//!   structural ones on every path, and that the workspaces a fixed
//!   application shows exist on save — and the policy an application gets
//!   when the admin states none.
//! - [`AnalyticsFramework`], what a mount constructs: nothing to build, the
//!   bundle served at `/` (the document) and `/analytics/…` (its assets, at
//!   the paths the bundle already links them by).
//! - [`APP_ENDPOINTS`], the endpoints the bundle may call on the
//!   application's subdomain. The router dispatches those — and only those —
//!   with the application's role floor in place of the admin's, and hands the
//!   handler the application's [`AppScope`](sc_analytics::app::AppScope); every
//!   other path under `/api/` is a 404 there, as anything an application does
//!   not expose is.
//!
//! The session is the application's own cookie on its own host, made by the
//! same `login` endpoint the admin host signs in with: the shell offers it to a
//! visitor who is not signed in.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sc_analytics::app::{ANALYTICS_FRAMEWORK, AnalyticsConfig, analytics_config_spec};
use sc_app::{
    AppRequest, AppResponse, Application, BuildSpec, CspPolicy, Framework, FrameworkFactory,
    FrameworkInfo, Method, MountContext,
};
use sc_catalog::Catalog;
use sc_error::Result;
use sc_types::{Attrs, FormField};

/// The endpoints an Analytics application's browser may call: signing in and
/// out, the shell, and the Analytics UI's own minus what is the admin's alone —
/// models and their outputs, public datasets, Settings → Maps.
pub const APP_ENDPOINTS: &[&str] = &[
    // Who is looking, and signing in and out.
    "authStatus",
    "login",
    "logout",
    // The shell.
    "analyticsShell",
    // Datasets.
    "listDatasets",
    "getDataset",
    "createDataset",
    "updateDataset",
    "shareDataset",
    "deleteDataset",
    "cloneDataset",
    "datasetUsage",
    "datasetShapes",
    "validateDatasetOperation",
    "readDatasetStage",
    "datasetColumnValues",
    "listDatasetTables",
    // Plots, tests and panels.
    "renderPlot",
    "renderTable",
    "runTests",
    "plotGallery",
    "suggestPlot",
    "renderPanel",
    // Maps.
    "layerData",
    "layerTile",
    "layerRows",
    "selectFeatures",
    "saveSelection",
    "mapSettings",
    "listMapTools",
    "runMapTool",
    "suggestMap",
    "renderMap",
    // Workspaces.
    "listWorkspaceKinds",
    "listWorkspaces",
    "getWorkspace",
    "createWorkspace",
    "updateWorkspace",
    "shareWorkspace",
    "saveWorkspaceState",
    "deleteWorkspace",
];

/// The endpoints of [`APP_ENDPOINTS`] that keep their own requirement — a
/// visitor must be able to see who they are and sign in before the role floor
/// can admit them.
pub const APP_AUTH_ENDPOINTS: &[&str] = &["authStatus", "login", "logout"];

/// Install the Analytics framework into `sc-app`'s registry. Idempotent.
pub fn install_analytics_framework() -> Result<()> {
    sc_app::install_framework_factory(Arc::new(AnalyticsFactory))
}

/// The policy an Analytics application gets unless the admin states one: the
/// admin host's Analytics UI policy, with the default base map's origins — the
/// bundle is the same bundle, and needs what it needs there. A base map from
/// elsewhere is the admin's to add to the application's own policy.
pub fn analytics_app_csp() -> CspPolicy {
    let header = crate::security::analytics_content_security_policy(
        &sc_config::MapSettings::default().hosts(),
    );
    let mut policy = CspPolicy {
        directives: Default::default(),
    };
    for directive in header.split(';') {
        let mut words = directive.split_whitespace();
        if let Some(name) = words.next() {
            policy = policy.directive(name, words.map(str::to_owned).collect::<Vec<_>>());
        }
    }
    policy
}

/// `analytics` in the framework registry.
pub struct AnalyticsFactory;

#[async_trait]
impl FrameworkFactory for AnalyticsFactory {
    fn info(&self) -> FrameworkInfo {
        FrameworkInfo {
            name: ANALYTICS_FRAMEWORK.to_owned(),
            label: "Analytics".to_owned(),
            description: "Part of the Analytics UI for your users: dashboards and reports you \
                          made, or a self-serve explorer over the tables you pick. They read \
                          only the rows their role and the ownership formulas let them."
                .to_owned(),
            serves_ui: true,
        }
    }

    fn config_spec(&self) -> Vec<FormField> {
        analytics_config_spec()
    }

    fn default_csp(&self) -> CspPolicy {
        analytics_app_csp()
    }

    fn check_config(&self, config: &Attrs) -> Result<()> {
        AnalyticsConfig::read(config).map(|_| ())
    }

    async fn check_config_against(&self, catalog: &Catalog, config: &Attrs) -> Result<()> {
        sc_analytics::app::check_config_against(catalog, config).await
    }

    async fn mount(&self, _app: &Application, ctx: MountContext<'_>) -> Result<Arc<dyn Framework>> {
        Ok(Arc::new(AnalyticsFramework {
            bundle: ctx.bundle_dir.map(Path::to_path_buf),
        }))
    }
}

/// A mounted Analytics application: the bundle, served.
pub struct AnalyticsFramework {
    /// The built `ui/analytics`; `None` on a server built without it, where
    /// every page says so.
    bundle: Option<PathBuf>,
}

/// `Cache-Control` for a content-hashed asset: it never changes under its name.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `Cache-Control` for the document: ask again, so a new build is picked up.
const REVALIDATE: &str = "no-cache";

impl AnalyticsFramework {
    /// The bundle's file at `rest` (relative, no `..`), if it has one.
    async fn file(&self, rest: &str) -> Option<(Bytes, &'static str)> {
        let dir = self.bundle.as_ref()?;
        let relative = Path::new(rest.trim_start_matches('/'));
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return None;
        }
        let bytes = tokio::fs::read(dir.join(relative)).await.ok()?;
        Some((Bytes::from(bytes), sc_app::asset_content_type(rest)))
    }
}

#[async_trait]
impl Framework for AnalyticsFramework {
    fn name(&self) -> &str {
        ANALYTICS_FRAMEWORK
    }

    fn config_spec(&self) -> Vec<FormField> {
        analytics_config_spec()
    }

    async fn handle(&self, req: AppRequest, _cat: &Catalog) -> Result<AppResponse> {
        if !matches!(req.method, Method::Get) {
            return Ok(AppResponse::method_not_allowed());
        }
        // The document at `/` (and at `/analytics/`, where the admin host has
        // it, so a link copied from one works on the other); its assets where
        // it links them, `/analytics/assets/…`.
        let rest = match req.path.as_str() {
            "/" | "/analytics" | "/analytics/" | "/index.html" => "index.html",
            path => match path.strip_prefix("/analytics/") {
                Some(rest) if !rest.is_empty() => rest,
                _ => return Ok(AppResponse::not_found()),
            },
        };
        let Some((bytes, content_type)) = self.file(rest).await else {
            return Ok(if self.bundle.is_none() {
                AppResponse::with_status(
                    404,
                    "text/plain; charset=utf-8",
                    "the Analytics UI bundle is not built (run `npm ci && npm run build` in \
                     ui/analytics)",
                )
            } else {
                AppResponse::not_found()
            });
        };
        let cache = if rest == "index.html" {
            REVALIDATE
        } else {
            IMMUTABLE
        };
        Ok(AppResponse::ok(content_type, bytes).header("Cache-Control", cache))
    }

    fn build(&self) -> Option<BuildSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_endpoint_an_application_may_call_is_declared() {
        let set = sc_api::admin_endpoints();
        for name in APP_ENDPOINTS {
            assert!(set.find(name).is_some(), "`{name}` is not an endpoint");
        }
        for name in APP_AUTH_ENDPOINTS {
            assert!(APP_ENDPOINTS.contains(name), "{name}");
        }
        // What is the admin's alone stays the admin's.
        for name in [
            "getModelOutputs",
            "allowMapHost",
            "installPublicDataset",
            "listModels",
        ] {
            assert!(!APP_ENDPOINTS.contains(&name), "{name}");
        }
    }

    #[test]
    fn the_default_policy_is_the_analytics_uis() {
        let csp = analytics_app_csp().header_value();
        assert!(csp.contains("style-src 'self' 'unsafe-inline'"), "{csp}");
        assert!(csp.contains("worker-src 'self'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    }

    #[tokio::test]
    async fn the_bundle_is_served_at_the_root_and_under_analytics() {
        let dir = std::env::temp_dir().join(format!("sc-analytics-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<html>shell</html>").unwrap();
        std::fs::write(dir.join("assets/app-1a2b.js"), "1").unwrap();
        let fw = AnalyticsFramework {
            bundle: Some(dir.clone()),
        };
        let driver = sc_db_sqlite::SqliteDriver::open_in_memory().unwrap();
        let catalog = sc_catalog::Catalog::init(Arc::new(driver)).await.unwrap();
        let doc = fw.handle(AppRequest::get("/"), &catalog).await.unwrap();
        assert_eq!(doc.status, 200);
        assert_eq!(&doc.body[..], b"<html>shell</html>");
        assert!(doc.content_type.starts_with("text/html"));
        let asset = fw
            .handle(AppRequest::get("/analytics/assets/app-1a2b.js"), &catalog)
            .await
            .unwrap();
        assert_eq!(asset.status, 200);
        assert!(asset.headers.iter().any(|(_, v)| v.contains("immutable")));
        for path in [
            "/analytics/../secret",
            "/elsewhere",
            "/analytics/missing.js",
        ] {
            let r = fw.handle(AppRequest::get(path), &catalog).await.unwrap();
            assert_eq!(r.status, 404, "{path}");
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
