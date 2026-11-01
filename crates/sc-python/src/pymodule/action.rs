//! A Python module's action, as an ordinary [`Action`].
//!
//! The whole of what makes it work here: the manifest says what it is called and
//! what it is configured with, and `run` turns an [`ActionContext`] into the
//! parameters §2 of the API lists — of which the plugin's function receives only
//! the ones it asked for.
//!
//! # It asks for what it wants
//!
//! ```python
//! @sc.action(config=[sc.Field.string("model", required=True)])
//! def score_lead(row, config, user): ...
//! ```
//!
//! `row`, `old`, `table`, `user`, `payload`, `config`, `configuration`,
//! `trigger`, `mode` — declared, and passed. This is the one place the Python
//! plugin API is *better* than the JavaScript one rather than merely different,
//! and it is free: Python has `inspect.signature` and JavaScript does not. The
//! selection happens in the Python half, where the signature is; what this file
//! does is build the object it selects from, once, in one place, so the two
//! languages' actions cannot come to disagree about what `user` means.
//!
//! # And it reaches the same five surfaces a body does
//!
//! Built by [`CodeSurfaces`] — `sc_action_builtins`', the very ones `run_js_code`
//! and `run_python_code` build — so a module's write carries the event's caller
//! and this trigger's chain, its `fetch` is counted on the run's budget, and its
//! `trigger(…)` goes through *the* dispatcher. §2: a Python plugin has no v1 to
//! be compatible with, so it is handed the plans directly rather than a stubbed
//! `Table`.

use std::sync::Arc;

use sc_action::{Action, ActionContext, Event};
use sc_action_builtins::CodeSurfaces;
use sc_error::Result;
use sc_types::{Attrs, FormField};
use serde_json::{Map, Value as Json};

use super::host::PyModuleHost;

/// One action supplied by one Python module.
pub struct PyModuleAction {
    /// The distribution the action came from — what the Modules tab attributes
    /// it to, and what the host dispatches on.
    module: String,
    /// The action's own name, unqualified, as the decorator registered it.
    name: String,
    /// The description the plugin gave, or one naming the module.
    description: String,
    /// Its settings, as the trigger form asks them.
    config_spec: Vec<FormField>,
    host: Arc<PyModuleHost>,
    /// The HTTP client behind `fetch`, held once for the reason
    /// `run_python_code` holds one: a client is a connection pool and a TLS
    /// configuration, and one per firing would pay for a handshake every time a
    /// trigger runs.
    surfaces: Arc<CodeSurfaces>,
}

impl PyModuleAction {
    /// An action of `module`, as its manifest describes it.
    pub fn new(
        module: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        config_spec: Vec<FormField>,
        host: Arc<PyModuleHost>,
        surfaces: Arc<CodeSurfaces>,
    ) -> PyModuleAction {
        let module = module.into();
        let name = name.into();
        let described = description.into();
        let description = if described.trim().is_empty() {
            format!("{name}, from the module {module}")
        } else {
            described
        };
        PyModuleAction {
            module,
            name,
            description,
            config_spec,
            host,
            surfaces,
        }
    }

    /// The distribution this action came from.
    #[must_use]
    pub fn module(&self) -> &str {
        &self.module
    }
}

#[async_trait::async_trait]
impl Action for PyModuleAction {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn config_spec(&self) -> Vec<FormField> {
        self.config_spec.clone()
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let args = arguments(ctx.event, ctx.config, ctx.trigger);
        // Held for the length of the call: the five surfaces borrow from it, and
        // the borrow lives exactly as long as the run does.
        let hosts = self.surfaces.build(ctx);
        self.host
            .run(&self.module, &self.name, args, hosts.surfaces())
            .await
    }
}

/// Everything an action *may* be passed, in this system's own names.
///
/// Every key is always present, and absent values are `null`. That is the
/// opposite of the "presence is scope" rule a code **body** follows, and
/// deliberately: a body names a binding and gets a `NameError` where there is
/// none, which is a mistake caught at the moment it is made. A plugin's
/// parameter list is fixed when the package is written, so a `row` that
/// disappeared on a `login` trigger would be a `TypeError` inside somebody
/// else's function — the wrong place to learn that this trigger has no rows.
fn arguments(event: &Event, config: &Attrs, trigger: &str) -> Json {
    let mut args = Map::new();
    args.insert("row".into(), event.row.clone().unwrap_or(Json::Null));
    args.insert("old".into(), event.old_row.clone().unwrap_or(Json::Null));
    // The table's **name**, not an object standing in for a model that does not
    // exist here: what a plugin can do with a table it did not open is name it,
    // and `db.<name>` is how it reads one.
    args.insert(
        "table".into(),
        match &event.channel {
            Some(name) => Json::String(name.clone()),
            None => Json::Null,
        },
    );
    args.insert("user".into(), event.user.clone().unwrap_or(Json::Null));
    args.insert("payload".into(), event.payload.clone());
    // This action's own configured settings. The **module's** are the host's to
    // supply and arrive as `configuration`, because they are what an admin typed
    // on the Modules tab and a trigger cannot change them.
    args.insert("config".into(), Json::Object(config.clone()));
    args.insert("trigger".into(), Json::String(trigger.to_owned()));
    args.insert("mode".into(), Json::String(event.kind.as_str().to_owned()));
    Json::Object(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_action::EventKind;
    use serde_json::json;

    #[test]
    fn the_argument_object_carries_this_systems_names() {
        let mut event = Event::new(EventKind::Update);
        event.channel = Some("leads".into());
        event.row = Some(json!({ "id": 1, "email": "a@b.c" }));
        event.old_row = Some(json!({ "id": 1, "email": "was@b.c" }));
        event.user = Some(json!({ "id": 7 }));
        let mut config = Attrs::new();
        config.insert("model".into(), json!("fast"));

        let args = arguments(&event, &config, "score new leads");
        assert_eq!(args["row"]["email"], json!("a@b.c"));
        assert_eq!(args["old"]["email"], json!("was@b.c"));
        // A name, not a `{ name }` object: v1's spelling is v1's, and this API
        // has no v1 to be compatible with.
        assert_eq!(args["table"], json!("leads"));
        assert_eq!(args["user"]["id"], json!(7));
        assert_eq!(args["config"]["model"], json!("fast"));
        assert_eq!(args["trigger"], json!("score new leads"));
        assert_eq!(args["mode"], json!("update"));
        // `configuration` is not here: the module's own settings are the host's
        // and are added on the far side, where nothing a trigger holds can
        // reach them.
        assert!(args.get("configuration").is_none());
    }

    #[test]
    fn an_event_with_no_row_and_no_caller_still_has_every_key() {
        let args = arguments(&Event::new(EventKind::None), &Attrs::new(), "run me");
        for key in [
            "row", "old", "table", "user", "payload", "config", "trigger", "mode",
        ] {
            assert!(args.get(key).is_some(), "missing {key}");
        }
        assert_eq!(args["row"], Json::Null);
        assert_eq!(args["table"], Json::Null);
    }
}
