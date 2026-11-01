//! A module's **stream providers**, end to end on a worker (TODO "Streams"
//! §12, phase 9).
//!
//! The fixture supplies five and two of them are broken on purpose, which is
//! half of what this file is about: a module with one mis-declared provider
//! must still supply the others, with a sentence on its card naming the one that
//! is missing. The other half is the seam itself — a configuration in, an
//! element type out, elements and an opaque cursor out — driven through
//! [`StreamProvider`], which is the trait the supervisor subscribes with and
//! therefore the only one worth asserting against.
//!
//! The **poll loop is `sc-stream`'s**, not the module's (§12), so what is
//! asserted here is that the two halves meet: the interval, the cursor and the
//! decoding come from [`PollingProvider`], and the answers come from
//! JavaScript.
//!
//! No network: the fixture is a local directory with no dependencies, so `npm
//! install <dir>` reaches nothing. It still needs npm, and skips without it.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::sync::Arc;
use std::time::Duration;

use common::{fixture, have_npm, installed};
use sc_module::{LoadedModule, Module, ModuleSet, ModuleSource, ModuleStreamProviders};
use sc_stream::testing::Collector;
use sc_stream::{StreamProviderHost, StreamRegistry, StreamSink};
use sc_types::Attrs;
use serde_json::json;

/// The fixture installed, loaded on a worker, and its manifest folded into a
/// one-module set — which is what [`ModuleStreamProviders`] reads.
async fn loaded(tag: &str) -> (Arc<sc_module::ModuleHost>, ModuleSet, Vec<String>) {
    let (installer, host, names) = installed(tag, &["stream-module"]).await;
    let name = names[0].clone();
    let manifest = host
        .load(
            &name,
            &installer.package_dir(&name),
            &json!({ "endpoint": "https://configured.example" }),
            &sc_module::ModulePermissions::default(),
        )
        .await
        .expect("the fixture loads");
    let issues = manifest.issues.clone();
    let module = Module::new(
        &name,
        ModuleSource::Local,
        fixture("stream-module").display().to_string(),
    );
    let set = ModuleSet::empty().merged(vec![LoadedModule {
        module,
        manifest: Some(manifest),
        config_spec: Vec::new(),
        issues: issues.clone(),
    }]);
    (host, set, issues)
}

fn attrs(value: serde_json::Value) -> Attrs {
    value.as_object().expect("an object").clone()
}

#[tokio::test]
async fn a_module_supplies_stream_providers_and_reports_the_ones_it_cannot() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (_host, set, issues) = loaded("stream-providers-declared").await;

    let providers =
        ModuleStreamProviders::new(&Arc::new(sc_module::ModuleHost::new("unused")), &set);
    let mut names: Vec<&str> = providers
        .providers()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    names.sort_unstable();
    // The three that could work, and only those: one with no `poll` and one
    // that declares no element type are dropped.
    assert_eq!(names, ["counter", "mixed", "unreachable"]);

    // And each is dropped **with a sentence**, naming the provider and what is
    // wrong with it, on the module's own card.
    for (provider, reason) in [
        ("unpollable", "no poll function"),
        ("shapeless", "no element type"),
    ] {
        assert!(
            issues
                .iter()
                .any(|i| i.contains(provider) && i.contains(reason)),
            "no issue names {provider} ({reason}): {issues:?}"
        );
    }

    // The declaration crossed whole: the module it came from (so a duplicate
    // name can be refused naming both sources), the label, the description and
    // the settings the Streams form renders.
    let counter = providers
        .providers()
        .iter()
        .find(|p| p.name == "counter")
        .expect("counter");
    assert_eq!(counter.module.as_deref(), Some("@saltcorn-test/stream"));
    assert_eq!(counter.label, "Counter");
    assert_eq!(counter.description, "Counts, in batches");
    assert!(counter.source().contains("@saltcorn-test/stream"));
    let fields: Vec<&str> = counter.config_spec.iter().map(|f| f.name()).collect();
    assert_eq!(fields, ["batch", "interval_s"]);
}

#[tokio::test]
async fn the_element_type_is_a_function_of_the_configuration_across_the_seam() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("stream-providers-type").await;
    let mut registry = StreamRegistry::new();
    registry
        .register_host(Arc::new(ModuleStreamProviders::new(&host, &set)))
        .expect("three providers register");
    let provider = registry.require("counter").expect("registered");

    // Two keys without the setting, three with it — which is the whole of what
    // GOALS means by "as a function of these configuration fields", and what a
    // provider per shape would have cost.
    let plain = provider
        .resolve_element_type(&attrs(json!({ "batch": 2 })))
        .await
        .expect("the declaration crosses");
    let keys: Vec<&str> = plain.keys().iter().map(|k| k.name.as_str()).collect();
    assert_eq!(keys, ["n", "from"]);
    assert!(plain.keys()[0].required, "`n` is declared required");

    let labelled = provider
        .resolve_element_type(&attrs(json!({ "batch": 2, "label": "north" })))
        .await
        .expect("the declaration crosses");
    assert_eq!(labelled.keys().len(), 3);

    // The synchronous side answers from what was resolved, per configuration —
    // which is what the Streams list, the form and the client generator read.
    assert_eq!(
        provider
            .element_type(&attrs(json!({ "batch": 2 })))
            .expect("resolved a moment ago"),
        plain
    );
    let err = provider
        .element_type(&attrs(json!({ "batch": 9 })))
        .expect_err("nothing has resolved this one")
        .to_string();
    assert!(err.contains("counter"), "{err}");
}

#[tokio::test]
async fn a_stream_over_a_module_provider_delivers_elements_and_carries_its_cursor() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("stream-providers-poll").await;
    let mut registry = StreamRegistry::new();
    registry
        .register_host(Arc::new(ModuleStreamProviders::new(&host, &set)))
        .expect("three providers register");
    let provider = registry.require("counter").expect("registered");

    let sink = Arc::new(Collector::new());
    let subscription = provider
        .subscribe(
            "counting",
            &attrs(json!({ "batch": 2, "interval_s": 0.15, "label": "north" })),
            Arc::clone(&sink) as Arc<dyn StreamSink>,
        )
        .await
        .expect("it subscribes");

    // Three polls, and what matters is not exactly how many ran but that they
    // carried on from each other: the cursor is the module's own "where I got
    // to", and `sc-stream` hands it back untouched.
    sc_test_harness::wait_until("four elements", Duration::from_secs(10), || sink.len() >= 4).await;
    let values = sink.values();
    assert!(values.len() >= 4, "{values:?}");
    for (n, value) in values.iter().enumerate() {
        assert_eq!(value["n"], json!(n), "the counts run on: {values:?}");
        // The module's *own* configuration was in scope when its
        // `streamproviders` function was called, and the stream's settings
        // reached the poll.
        assert_eq!(value["from"], json!("https://configured.example"));
        assert_eq!(value["label"], json!("north"));
    }

    drop(subscription);
    let delivered = sink.len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sink.len(), delivered, "nothing is polled after a drop");
}

#[tokio::test]
async fn an_element_that_is_not_what_was_declared_is_counted_and_not_delivered() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("stream-providers-malformed").await;
    let mut registry = StreamRegistry::new();
    registry
        .register_host(Arc::new(ModuleStreamProviders::new(&host, &set)))
        .expect("three providers register");

    let sink = Arc::new(Collector::new());
    let subscription = registry
        .require("mixed")
        .expect("registered")
        .subscribe(
            "mixed",
            &attrs(json!({ "interval_s": 10 })),
            Arc::clone(&sink) as Arc<dyn StreamSink>,
        )
        .await
        .expect("it subscribes");
    sc_test_harness::wait_until("the first poll", Duration::from_secs(10), || {
        sink.len() + sink.malformed().len() >= 2
    })
    .await;
    drop(subscription);

    // The good one arrived; the one whose `n` is a string was counted, not
    // delivered — a feed with one misbehaving item must not cost the rest of
    // it.
    assert_eq!(sink.values(), vec![json!({ "n": 1 })]);
    let malformed = sink.malformed();
    assert_eq!(malformed.len(), 1, "{malformed:?}");
    assert!(malformed[0].contains("`n`"), "{malformed:?}");
}

#[tokio::test]
async fn a_poll_that_throws_ends_the_subscription_for_the_supervisor_to_retry() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("stream-providers-throw").await;
    let mut registry = StreamRegistry::new();
    registry
        .register_host(Arc::new(ModuleStreamProviders::new(&host, &set)))
        .expect("three providers register");

    let sink = Arc::new(Collector::new());
    let subscription = registry
        .require("unreachable")
        .expect("registered")
        .subscribe(
            "unreachable",
            &attrs(json!({ "interval_s": 10 })),
            Arc::clone(&sink) as Arc<dyn StreamSink>,
        )
        .await
        .expect("subscribing is not polling");
    sc_test_harness::wait_until("the subscription to end", Duration::from_secs(10), || {
        subscription.has_ended()
    })
    .await;

    // Ended rather than spinning: the module's throw is a failure the
    // supervisor services with its backoff, which is the one place retrying is
    // written.
    assert!(subscription.has_ended());
    assert!(sink.is_empty());
}

#[tokio::test]
async fn a_provider_whose_module_went_away_says_so_rather_than_panicking() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("stream-providers-gone").await;
    let providers = ModuleStreamProviders::new(&host, &set);
    let kind = providers
        .providers()
        .iter()
        .find(|p| p.name == "counter")
        .expect("counter")
        .clone();
    let provider = providers.provider(&kind).expect("code for it");

    // The module set is rebuilt without it — an uninstall — while a provider
    // handed out earlier is still held. What it answers is a sentence naming
    // the module, which is what the supervisor shows as the stream's status.
    let empty = ModuleStreamProviders::empty(&host);
    assert!(empty.provider(&kind).is_err());
    host.shutdown().await;
    let err = provider
        .resolve_element_type(&attrs(json!({ "batch": 1 })))
        .await
        .expect_err("nothing is loaded any more")
        .to_string();
    assert!(!err.is_empty(), "a sentence, not a panic");
}
