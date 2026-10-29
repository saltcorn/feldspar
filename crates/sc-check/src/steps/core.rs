//! `core.wait` and `core.manual` (TODO C1.7).

use std::time::Duration;

use sc_types::{BasicType, FormField};
use serde_json::Value as Json;

use crate::model::SkipReason;
use crate::registry::{Needs, StepContext, StepOutcome, StepType};

/// The longest `core.wait` may wait: anything longer is a step that should be
/// an assertion that waits.
const MAX_WAIT_MS: u64 = 60_000;

/// A fixed delay. Discouraged — a `browser.expect_*` step waits by itself —
/// but sometimes a background job needs a moment.
pub struct Wait;

#[async_trait::async_trait]
impl StepType for Wait {
    fn name(&self) -> &str {
        "core.wait"
    }

    fn description(&self) -> &str {
        "Wait a fixed number of milliseconds (at most 60000). Prefer an assertion that waits by \
         itself."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("ms", BasicType::Int)
                .label("Milliseconds")
                .required()
                .default_value(1000),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::NONE
    }

    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, _cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let ms = params.get("ms").and_then(Json::as_u64).unwrap_or(1000);
        if ms > MAX_WAIT_MS {
            return StepOutcome::error(format!(
                "`ms` is {ms}; a wait is at most {MAX_WAIT_MS} ms"
            ));
        }
        tokio::time::sleep(Duration::from_millis(ms)).await;
        StepOutcome::pass(format!("waited {ms} ms"))
    }
}

/// An instruction for a person ("The invoice PDF looks right"). Never
/// automated: an automated run lists it as skipped, and it does not make the
/// run incomplete — it is what makes the checklist the application's manifest
/// and not only its test suite.
pub struct Manual;

#[async_trait::async_trait]
impl StepType for Manual {
    fn name(&self) -> &str {
        "core.manual"
    }

    fn description(&self) -> &str {
        "An instruction for a person to check by hand. Listed, and skipped in every automated run."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("instruction", BasicType::Text)
                .label("What to check")
                .required()
                .multiline(),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::NONE
    }

    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, _cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let instruction = params
            .get("instruction")
            .and_then(Json::as_str)
            .unwrap_or("");
        StepOutcome::skipped(
            SkipReason::Manual,
            format!("manual: {instruction}"),
        )
    }
}
