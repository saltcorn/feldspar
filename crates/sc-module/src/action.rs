//! A module's action, as an [`Action`].
//!
//! The whole of what makes a module's action work here: the manifest says what
//! it is called and what it is configured with, and `run` marshals an
//! [`ActionContext`] into v1's argument object and sends it to the host.
//!
//! **v1's argument object is a wire contract**, not a convenience: a plugin's
//! `run` destructures `{ row, table, configuration, user }` and has done for
//! years, and every one of those names is what a module already reads. So the
//! translation is written out here, once, and the names do not change.
//!
//! # And it reaches the same surfaces a code body does
//!
//! Built by [`CodeSurfaces`] — `sc_core_actions`', the very ones `run_js_code`
//! and `PyModuleAction` build — so a v1 plugin's `books.insertRow(row)` carries
//! the event's caller and this trigger's chain, and its `run_trigger` goes
//! through *the* dispatcher. This is what the v1 `Table` inside the module runs
//! over (TODO "the v1 `Table` API" §3): the ask channel is served by the future
//! awaiting the call, because that is where the hosts are borrowed.

use std::sync::Arc;

use sc_action::{Action, ActionContext, Event};
use sc_core_actions::CodeSurfaces;
use sc_error::Result;
use sc_types::{Attrs, FormField};
use serde_json::{Map, Value as Json, json};

use crate::files::{FileArea, ModuleFiles};
use crate::host::{CallHosts, ModuleHost};

/// One action supplied by one module.
pub struct ModuleAction {
    /// The package the action came from — what the Modules tab attributes it to,
    /// and what the host dispatches on.
    module: String,
    /// The action's own name, unqualified, exactly as v1 spells it.
    name: String,
    /// The description the module gave, or one naming the module.
    description: String,
    /// The settings, translated from v1's `configFields` at load
    /// ([`crate::spec`]).
    config_spec: Vec<FormField>,
    /// The host to run it in.
    host: Arc<ModuleHost>,
    /// The HTTP client behind `fetch` and the builder of the five surfaces, held
    /// once for the reason `run_js_code` holds one: a client is a connection pool
    /// and a TLS configuration, and one per firing would pay for a handshake
    /// every time a trigger runs.
    surfaces: Arc<CodeSurfaces>,
    /// Where the module may keep files (TODO-imap M6): what its declared file
    /// fields name, and what `files` in the argument object reaches.
    areas: Arc<Vec<FileArea>>,
    /// The most privileged role it may grant users (TODO-payments P3.3).
    roles: Option<u8>,
    /// What a direct run takes and answers, as the module declared them
    /// ([`Action::payload_type`], [`Action::returns_type`]).
    payload: Option<Json>,
    returns: Option<Json>,
}

impl ModuleAction {
    /// An action of `module`, as its manifest describes it.
    pub fn new(
        module: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        config_spec: Vec<FormField>,
        host: Arc<ModuleHost>,
        surfaces: Arc<CodeSurfaces>,
    ) -> ModuleAction {
        let module = module.into();
        let name = name.into();
        let described = description.into();
        let description = if described.trim().is_empty() {
            format!("{name}, from the module {module}")
        } else {
            described
        };
        ModuleAction {
            module,
            name,
            description,
            config_spec,
            host,
            surfaces,
            areas: Arc::new(Vec::new()),
            roles: None,
            payload: None,
            returns: None,
        }
    }

    /// The same action, typed: what its payload carries and what it answers,
    /// in a code query's `returns` type language. Checked by whoever registers
    /// it ([`sc_api::action_types`]), which says what is wrong with one.
    #[must_use]
    pub fn with_types(mut self, payload: Option<Json>, returns: Option<Json>) -> ModuleAction {
        self.payload = payload;
        self.returns = returns;
        self
    }

    /// The same action, able to grant roles up to `ceiling` (TODO-payments
    /// P3.3) — the module's [`ModulePermissions::roles`](crate::ModulePermissions::roles).
    #[must_use]
    pub fn with_role_ceiling(mut self, ceiling: Option<u8>) -> ModuleAction {
        self.roles = ceiling;
        self
    }

    /// The same action, able to keep files in its module's `areas`.
    #[must_use]
    pub fn with_file_areas(mut self, areas: Arc<Vec<FileArea>>) -> ModuleAction {
        self.areas = areas;
        self
    }

    /// The package this action came from.
    pub fn module(&self) -> &str {
        &self.module
    }
}

#[async_trait::async_trait]
impl Action for ModuleAction {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn config_spec(&self) -> Vec<FormField> {
        self.config_spec.clone()
    }

    fn payload_type(&self) -> Option<Json> {
        self.payload.clone()
    }

    fn returns_type(&self) -> Option<Json> {
        self.returns.clone()
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let args = v1_arguments(ctx.event, ctx.config, ctx.trigger);
        // Held for the length of the call: the surfaces borrow from it, and the
        // borrow lives exactly as long as the run — which is also what lets the
        // host serve the module's asks while it waits for the answer.
        let hosts = self.surfaces.build(ctx);
        let files = ModuleFiles::new(ctx.catalog, &self.module, &self.areas);
        let roles = crate::roles::ModuleRoles::new(ctx.catalog, &self.module, self.roles);
        self.host
            .run(
                &self.module,
                &self.name,
                args,
                CallHosts::new(hosts.surfaces(), hosts.schema())
                    .with_module_files(&files)
                    .with_roles(&roles),
            )
            .await
    }
}

/// The object a v1 `run` is called with.
///
/// `table` is an object with a `name` rather than v1's `Table` model, even now
/// that there is a real one: a `Table` here would be one built outside the
/// call's own async context, and what a plugin does with the argument is read
/// its name. `Table.findOne(table.name)` inside the action is the model, on the
/// call's own authority.
fn v1_arguments(event: &Event, config: &Attrs, trigger: &str) -> Json {
    let mut args = Map::new();
    args.insert("row".into(), event.row.clone().unwrap_or(Json::Null));
    args.insert(
        "old_row".into(),
        event.old_row.clone().unwrap_or(Json::Null),
    );
    args.insert(
        "table".into(),
        match &event.channel {
            Some(name) => json!({ "name": name }),
            None => Json::Null,
        },
    );
    args.insert(
        "channel".into(),
        match &event.channel {
            Some(name) => Json::String(name.clone()),
            None => Json::Null,
        },
    );
    args.insert("configuration".into(), Json::Object(config.clone()));
    args.insert("user".into(), event.user.clone().unwrap_or(Json::Null));
    args.insert("payload".into(), event.payload.clone());
    args.insert("mode".into(), Json::String(event.kind.as_str().to_owned()));
    args.insert("trigger".into(), Json::String(trigger.to_owned()));
    // Where the event came from (TODO-payments P1.3, P1.5), as a code body
    // sees it: the application whose request raised it — what a checkout's
    // return URLs are built on — and the caller's guest session.
    args.insert(
        "app".into(),
        event.app.as_ref().map_or(Json::Null, |a| a.to_json()),
    );
    args.insert(
        "session".into(),
        event
            .session
            .as_ref()
            .map_or(Json::Null, |id| json!({ "id": id })),
    );
    // v1 code reads two things off `req`: who is asking, and what they posted.
    // Anything else it reaches for is an Express request, which does not exist
    // here — and an object that answered those too would be pretending.
    args.insert(
        "req".into(),
        json!({
            "user": event.user.clone().unwrap_or(Json::Null),
            "body": event.payload.clone(),
        }),
    );
    Json::Object(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_action::EventKind;

    #[test]
    fn the_v1_argument_object_carries_the_names_a_plugin_destructures() {
        let mut event = Event::new(EventKind::Insert);
        event.channel = Some("books".into());
        event.row = Some(json!({ "id": 1, "title": "Dune" }));
        event.user = Some(json!({ "id": 7, "email": "a@b.c" }));
        let mut config = Attrs::new();
        config.insert("channel".into(), json!("readings"));

        let args = v1_arguments(&event, &config, "publish a reading");
        assert_eq!(args["row"]["title"], json!("Dune"));
        assert_eq!(args["table"]["name"], json!("books"));
        assert_eq!(args["channel"], json!("books"));
        assert_eq!(args["configuration"]["channel"], json!("readings"));
        assert_eq!(args["user"]["email"], json!("a@b.c"));
        assert_eq!(args["mode"], json!("insert"));
        assert_eq!(args["trigger"], json!("publish a reading"));
        assert_eq!(args["req"]["user"]["id"], json!(7));
        // No application's request raised it, and nobody is a guest.
        assert_eq!(args["app"], Json::Null);
        assert_eq!(args["session"], Json::Null);
    }

    #[test]
    fn an_event_with_no_row_and_no_caller_still_has_every_key() {
        let event = Event::new(EventKind::None);
        let args = v1_arguments(&event, &Attrs::new(), "run me");
        for key in [
            "row", "old_row", "table", "channel", "user", "payload", "req",
        ] {
            assert!(args.get(key).is_some(), "missing {key}");
        }
        assert_eq!(args["row"], Json::Null);
        assert_eq!(args["table"], Json::Null);
    }

    #[test]
    fn an_action_with_no_description_is_attributed_to_its_module() {
        let host = Arc::new(ModuleHost::new("/tmp/does-not-need-to-exist"));
        let surfaces = Arc::new(CodeSurfaces::new().expect("an HTTP client"));
        let action = ModuleAction::new(
            "@saltcorn/mqtt",
            "mqtt_publish",
            "",
            Vec::new(),
            host,
            surfaces,
        );
        assert_eq!(action.name(), "mqtt_publish");
        assert!(action.description().contains("@saltcorn/mqtt"));
        assert_eq!(action.module(), "@saltcorn/mqtt");
    }
}
