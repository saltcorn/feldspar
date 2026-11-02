//! `app.builds` (TODO C1.10): the application builds. Cheap and catches a lot.

use std::time::Duration;

use sc_types::FormField;
use serde_json::Value as Json;

use crate::registry::{Needs, StepContext, StepOutcome, StepType};

/// The most of a failed build's log a step's message carries.
const LOG_TAIL: usize = 2000;

/// `app.builds`.
pub struct Builds;

#[async_trait::async_trait]
impl StepType for Builds {
    fn name(&self) -> &str {
        "app.builds"
    }

    fn description(&self) -> &str {
        "The checklist's application builds from its current source (generated client, \
         install, bundler). What is served is not changed. An application with nothing to \
         build (Saltcorn UI) passes."
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn needs(&self) -> Needs {
        Needs::BUILD
    }

    fn mutates(&self) -> bool {
        false
    }

    fn min_timeout(&self) -> Duration {
        // A first build installs the dependencies.
        Duration::from_secs(600)
    }

    async fn run(&self, cx: &mut StepContext<'_>, _params: &Json) -> StepOutcome {
        let app = match cx.require_application() {
            Ok(app) => app,
            Err(e) => return e.into(),
        };
        if sc_app::framework_factory(&app.framework.name).is_some() {
            return StepOutcome::pass(format!(
                "`{}` is a {} application, which has nothing to build",
                app.subdomain, app.framework.name
            ));
        }
        let source = match sc_app::app_source_from_config(&app.framework) {
            Ok(source) => source,
            Err(e) => return e.into(),
        };
        match sc_app::build_application(cx.catalog, app, &source, cx.caps.triggers).await {
            Ok(report) => StepOutcome::pass(format!(
                "`{}` built: {} file{}",
                app.subdomain,
                report.bundle.len(),
                if report.bundle.len() == 1 { "" } else { "s" }
            )),
            Err(e) => {
                let log = sc_error::format_chain(&e);
                let tail: String = {
                    let chars: Vec<char> = log.chars().collect();
                    let start = chars.len().saturating_sub(LOG_TAIL);
                    chars[start..].iter().collect()
                };
                StepOutcome::fail(format!("`{}` does not build: {tail}", app.subdomain))
            }
        }
    }
}
