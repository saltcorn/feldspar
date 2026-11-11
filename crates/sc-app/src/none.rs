//! The `none` framework: an application with no UI framework and no build step.
//!
//! Some applications are only what the rest of the record already says — their
//! API providers, their static directories and their streams. A webhook
//! receiver, a data feed, a site of hand-written (or agent-written) HTML and CSS
//! served from a file store: none of them has a bundle to build or a project for
//! Saltcorn to scaffold, and asking the admin for a build command to get one would
//! be asking them to invent a project they do not have.
//!
//! So `none` serves **nothing of its own**. Every request the router has not
//! already answered from an API, a static directory or a stream socket is a 404
//! ([`NoneFramework`]), and because there are no framework paths to protect it
//! declares [`serves_ui`](crate::Framework::serves_ui) `false`, which is what lets
//! an API be mounted at `/`.
//!
//! It is constructed rather than built, so it is registered as a
//! [`FrameworkFactory`] — the seam every "nothing to build" path in the server
//! already asks (saving is the deployment, there is no Build button, there is no
//! generated client to rewrite). Unlike the factories installed from above, it
//! is compiled into this crate and always present
//! ([`framework_factories`](crate::framework_factories)).
//!
//! It still has a **file store and a directory**, for one reason: the coding
//! agent an application is created with (§13.3) needs somewhere to work. What it
//! writes there — HTML, CSS, images — is served by whichever static directory
//! the admin points at it.

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::Catalog;
use sc_error::Result;
use sc_types::{BasicType, FormField};

use crate::application::{Application, CspPolicy, FrameworkRef};
use crate::factory::{FrameworkFactory, MountContext};
use crate::framework::{
    AppRequest, AppResponse, BuildSpec, CFG_SOURCE, CFG_STORE, Framework, FrameworkInfo,
};

/// The registered name of the framework with no UI and no build.
pub const NONE_FRAMEWORK: &str = "none";

/// The settings a `none` application has: the file store its coding agent works
/// in, and the directory within it.
///
/// Named like `code`'s two settings of the same meaning, so a store picker (and
/// "create a new local store") works on this form exactly as it does on that
/// one.
pub fn none_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_STORE, BasicType::Text)
            .label("File store")
            .required()
            .server_query(sc_catalog::QUERY_FILE_STORES),
        FormField::new(CFG_SOURCE, BasicType::Text)
            .label("Directory")
            .default_value(""),
    ]
}

/// The file store and directory a `none` application's coding agent works in,
/// or `None` when `fw` is not `none` or does not name a store.
///
/// The counterpart of [`app_source_from_config`](crate::app_source_from_config)
/// for a framework with a directory but nothing to build from it.
pub fn none_source_dir(fw: &FrameworkRef) -> Option<(String, String)> {
    if fw.name != NONE_FRAMEWORK {
        return None;
    }
    let setting = |name: &str| {
        fw.config
            .get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().trim_matches('/').to_owned())
    };
    let store = setting(CFG_STORE).filter(|s| !s.is_empty())?;
    Some((store, setting(CFG_SOURCE).unwrap_or_default()))
}

/// `none` in the framework registry.
pub(crate) struct NoneFactory;

#[async_trait]
impl FrameworkFactory for NoneFactory {
    fn info(&self) -> FrameworkInfo {
        FrameworkInfo {
            name: NONE_FRAMEWORK.to_owned(),
            label: "None".to_owned(),
            description: "No UI framework and no build step: the application is its APIs, \
                          its static directories and its streams. A coding agent works in \
                          the file store you pick — to write the HTML and CSS a static \
                          directory serves, say."
                .to_owned(),
            serves_ui: false,
            settings_tabs: None,
        }
    }

    fn config_spec(&self) -> Vec<FormField> {
        none_config_spec()
    }

    fn default_csp(&self) -> CspPolicy {
        CspPolicy::strict()
    }

    async fn mount(
        &self,
        _app: &Application,
        _ctx: MountContext<'_>,
    ) -> Result<Arc<dyn Framework>> {
        Ok(Arc::new(NoneFramework))
    }
}

/// A mounted `none` application's framework: it answers nothing.
pub struct NoneFramework;

#[async_trait]
impl Framework for NoneFramework {
    fn name(&self) -> &str {
        NONE_FRAMEWORK
    }

    fn config_spec(&self) -> Vec<FormField> {
        none_config_spec()
    }

    async fn handle(&self, _req: AppRequest, _cat: &Catalog) -> Result<AppResponse> {
        Ok(AppResponse::not_found())
    }

    fn build(&self) -> Option<BuildSpec> {
        None
    }

    fn serves_ui(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_directory_is_the_store_and_directory_settings() {
        let fw = FrameworkRef::new(NONE_FRAMEWORK)
            .with(CFG_STORE, "site")
            .with(CFG_SOURCE, "/public/");
        assert_eq!(
            none_source_dir(&fw),
            Some(("site".to_owned(), "public".to_owned()))
        );
        // No directory is the store root; no store is no source at all.
        let root = FrameworkRef::new(NONE_FRAMEWORK).with(CFG_STORE, "site");
        assert_eq!(
            none_source_dir(&root),
            Some(("site".to_owned(), String::new()))
        );
        assert_eq!(none_source_dir(&FrameworkRef::new(NONE_FRAMEWORK)), None);
        // Another framework's settings are not read as this one's.
        assert_eq!(
            none_source_dir(&FrameworkRef::new("code").with(CFG_STORE, "site")),
            None
        );
    }

    #[test]
    fn it_is_headless_and_has_no_build() {
        let fw = NoneFramework;
        assert!(!fw.serves_ui());
        assert!(fw.build().is_none());
        assert!(!NoneFactory.info().serves_ui);
    }
}
