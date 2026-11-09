//! Secrets in the structure tree (APP_SYNC_PROPOSAL §3.3): every value a
//! definition declares secret is written as [`SECRET_SENTINEL`].
//!
//! A repository is shared, mirrored and kept forever, so a secret that reached
//! one commit is a secret that has leaked. The rule is therefore the admin
//! API's own — a value is redacted where its spec declares it
//! [`secret`](sc_types::FormField::secret), through [`redact_attrs`] — with one
//! change made for a repository: **where no spec is known, every value is
//! treated as secret.** The
//! admin API passes such a value through, because a screen showing a stored
//! setting of an uninstalled plugin to the admin who stored it discloses
//! nothing; a commit pushed to a remote is not that. So a stream whose provider
//! is not installed, a trigger whose action nothing implements, a module whose
//! settings this build cannot read and an agent trait that is not registered
//! all have their configuration's text values replaced by the sentinel. On
//! deploy `merge_secrets` keeps the receiving instance's own values, exactly as
//! it does for a declared secret.
//!
//! What declares a secret lives in registries the server assembles — its
//! actions, stream providers, agent traits and loaded modules — and a crate
//! below the server cannot reach them. [`Registries`] is how they are handed
//! in; [`Registries::builtin`] is the built-in set, for a caller with no server.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;

use sc_action::ActionRegistry;
use sc_agent::AgentRegistry;
use sc_catalog::Catalog;
use sc_error::Result;
use sc_stream::StreamRegistry;
use sc_types::{Attrs, FormField, SECRET_SENTINEL, redact_attrs};

/// The registries a definition's secret settings are recognised by.
#[derive(Clone)]
pub struct Registries {
    /// The actions a trigger, or a workflow's action step, runs.
    pub actions: Arc<ActionRegistry>,
    /// The stream providers.
    pub streams: Arc<StreamRegistry>,
    /// The agent traits.
    pub agent_traits: Arc<AgentRegistry>,
    /// Each loaded module's settings, by module name.
    pub modules: BTreeMap<String, Vec<FormField>>,
}

impl Registries {
    /// The built-in actions, stream providers and agent traits, and no modules.
    ///
    /// What a caller with no running server has. A module's contribution — an
    /// action, a provider, a trait, its own settings — is then unknown, and is
    /// redacted whole (see the module docs).
    pub fn builtin() -> Result<Registries> {
        Ok(Registries {
            actions: Arc::new(sc_action_builtins::builtin_actions()?),
            streams: Arc::new(sc_stream::builtin_registry()?),
            agent_traits: Arc::new(sc_agent_builtins::builtin_traits()?),
            modules: BTreeMap::new(),
        })
    }

    /// An action's configuration, redacted by its spec, or whole when the
    /// action is unknown.
    pub fn redact_action(
        &self,
        catalog: &Catalog,
        action: &str,
        channel: Option<&str>,
        configuration: &Attrs,
    ) -> Attrs {
        match self.actions.get(action) {
            Some(action) => redact_attrs(&action.config_spec_for(catalog, channel), configuration),
            None => redact_all(configuration),
        }
    }

    /// A stream's configuration, redacted by its provider's spec, or whole when
    /// the provider is unknown.
    pub fn redact_stream(&self, provider: &str, configuration: &Attrs) -> Attrs {
        match self.streams.get(provider.trim()) {
            Some(provider) => redact_attrs(&provider.config_spec(), configuration),
            None => redact_all(configuration),
        }
    }

    /// An agent trait's settings, redacted by its spec, or whole when the trait
    /// is unknown.
    pub fn redact_trait(&self, name: &str, config: &Attrs) -> Attrs {
        match self.agent_traits.get(name) {
            Some(agent_trait) => redact_attrs(&agent_trait.config_spec(), config),
            None => redact_all(config),
        }
    }

    /// A module's settings, redacted by its spec, or whole when the module's
    /// settings are not known.
    pub fn redact_module(&self, module: &str, configuration: &Attrs) -> Attrs {
        match self.modules.get(module) {
            Some(spec) => redact_attrs(spec, configuration),
            None => redact_all(configuration),
        }
    }
}

/// Settings redacted by `spec` when there is one, whole when there is not.
pub fn redact_by(spec: Option<&[FormField]>, attrs: &Attrs) -> Attrs {
    match spec {
        Some(spec) => redact_attrs(spec, attrs),
        None => redact_all(attrs),
    }
}

/// Every text value of `attrs`, at any depth, replaced by the sentinel: the
/// rule for settings whose spec is not known. Numbers, booleans and nulls are
/// kept — none of them is a password — and so is an empty string, which is
/// "not set" rather than a value.
pub fn redact_all(attrs: &Attrs) -> Attrs {
    attrs
        .iter()
        .map(|(key, value)| (key.clone(), redact_value(value)))
        .collect()
}

fn redact_value(value: &Json) -> Json {
    match value {
        Json::String(text) if !text.is_empty() => Json::String(SECRET_SENTINEL.to_owned()),
        Json::Array(items) => Json::Array(items.iter().map(redact_value).collect()),
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unknown_spec_redacts_every_text_value() {
        let attrs: Attrs = json!({
            "host": "broker.example.com",
            "port": 1883,
            "tls": true,
            "nothing": "",
            "nested": { "token": "abc", "list": ["x", 2] },
        })
        .as_object()
        .unwrap()
        .clone();
        let out = redact_all(&attrs);
        assert_eq!(
            Json::Object(out),
            json!({
                "host": SECRET_SENTINEL,
                "port": 1883,
                "tls": true,
                "nothing": "",
                "nested": { "token": SECRET_SENTINEL, "list": [SECRET_SENTINEL, 2] },
            })
        );
    }

    #[test]
    fn a_known_spec_redacts_only_what_it_declares_secret() {
        let spec = vec![
            FormField::new("host", sc_types::BasicType::Text),
            FormField::new("password", sc_types::BasicType::Text).secret(),
        ];
        let attrs: Attrs = json!({ "host": "h", "password": "p" })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            Json::Object(redact_by(Some(&spec), &attrs)),
            json!({ "host": "h", "password": SECRET_SENTINEL })
        );
        assert_eq!(
            Json::Object(redact_by(None, &attrs)),
            json!({ "host": SECRET_SENTINEL, "password": SECRET_SENTINEL })
        );
    }
}
