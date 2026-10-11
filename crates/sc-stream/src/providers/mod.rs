//! The stream providers that are compiled in (TODO §11, tasks 1.4 and 4.2;
//! TODO.md "Live updates" §2).
//!
//! Three of them:
//!
//! - **MQTT** ([`mqtt`]), behind a default-on `mqtt` feature so a build can
//!   drop it — `smartcore`'s arrangement, for `smartcore`'s reason;
//! - **internal** ([`internal`]), always: its elements are Saltcorn's own, it
//!   carries no network client, and it is what an application's toasts and
//!   progress bars are made of;
//! - **table_changes** ([`table_changes`]), always: one element per committed
//!   write on a table, handed to it by the live hub.
//!
//! Everything else a stream can be observed from comes from a module (§12). A
//! build with `--no-default-features` and no module installed therefore offers
//! only `internal` and `table_changes`, and the Streams screen says why MQTT is missing in a
//! sentence rather than leaving the admin to wonder — see
//! [`BUILTINS_COMPILED_OUT`].

use std::sync::Arc;

use sc_error::Result;

use crate::provider::StreamProvider;
use crate::registry::StreamRegistry;

pub mod internal;
#[cfg(feature = "mqtt")]
pub mod mqtt;
pub mod table_changes;

/// Whether this build carries the MQTT provider.
///
/// A `const` rather than a `cfg!` at each call site so that the one place the
/// question is answered is here, beside the notice that explains the answer —
/// `sc-model`'s `SMARTCORE`.
pub const MQTT_COMPILED_IN: bool = cfg!(feature = "mqtt");

/// What the admin screen says when this build has no MQTT provider (task 6.1).
///
/// A picker without the provider the docs name reads like a bug; "this build
/// was made without it" reads like a decision, which is what it is. The sentence names the flag,
/// because the person reading it is usually the person who can rebuild, and it
/// names the other way to get a provider, because that one needs no rebuild.
pub const BUILTINS_COMPILED_OUT: &str = "this server was built with `--no-default-features`, so the built-in MQTT stream provider was \
     compiled out; internal streams still work, and a module can still supply stream providers";

/// Every compiled-in stream provider.
///
/// `internal` and `table_changes` always, and MQTT with the `mqtt` feature on.
pub fn builtin_providers() -> Vec<Arc<dyn StreamProvider>> {
    let mut providers: Vec<Arc<dyn StreamProvider>> = vec![
        Arc::new(internal::Internal),
        Arc::new(table_changes::TableChanges),
    ];
    #[cfg(feature = "mqtt")]
    providers.push(Arc::new(mqtt::Mqtt));
    providers
}

/// A registry holding the built-in providers.
///
/// Fallible because [`StreamRegistry::register`] is: two built-ins under one
/// name is a programming error, and it is better found at boot with both names
/// in the message than by whichever one happened to win.
pub fn builtin_registry() -> Result<StreamRegistry> {
    let mut registry = StreamRegistry::new();
    for provider in builtin_providers() {
        registry.register(provider)?;
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_set_assembles_into_a_registry() {
        let registry = builtin_registry().unwrap();
        assert_eq!(registry.len(), builtin_providers().len());
    }

    #[cfg(feature = "mqtt")]
    #[test]
    fn mqtt_is_in_the_registry_under_the_name_a_stored_row_references() {
        let registry = builtin_registry().unwrap();
        let provider = registry.require(mqtt::MQTT).unwrap();
        assert_eq!(provider.name(), "mqtt");
        const { assert!(MQTT_COMPILED_IN) };
        // The picker's row: a label, a sentence, and the settings the form
        // renders without knowing what MQTT is.
        let kind = provider.kind();
        assert_eq!(kind.label, "MQTT");
        assert!(kind.module.is_none(), "a built-in has no module");
        assert!(!kind.config_spec.is_empty());
    }

    #[test]
    fn internal_is_built_in_whatever_the_features() {
        let registry = builtin_registry().unwrap();
        let provider = registry.require(internal::INTERNAL).unwrap();
        assert!(provider.accepts_publish());
        assert!(provider.kind().module.is_none());
    }

    #[cfg(not(feature = "mqtt"))]
    #[test]
    fn a_build_without_the_feature_has_a_sentence_rather_than_an_empty_picker() {
        assert_eq!(builtin_providers().len(), 1, "only `internal`");
        const { assert!(!MQTT_COMPILED_IN) };
        assert!(BUILTINS_COMPILED_OUT.contains("--no-default-features"));
    }
}
