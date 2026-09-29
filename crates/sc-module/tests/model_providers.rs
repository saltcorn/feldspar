//! A module's **model providers**, end to end on a worker (TODO "Predictive
//! models" phase 7, §14).
//!
//! The fixture supplies five and three of them are broken on purpose, which is
//! half of what this file is about: a module with one mis-declared provider must
//! still supply the others, with a sentence on its card naming the one that is
//! missing. The other half is the seam itself — a columnar frame in, a state and
//! its parameters out, a list of predictions out — driven through
//! [`ModelProvider`], which is the trait `sc-model` fits and predicts with and
//! therefore the only one worth asserting against.
//!
//! No network: the fixture is a local directory with no dependencies, so `npm
//! install <dir>` reaches nothing. It still needs npm, and skips without it.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::sync::Arc;

use common::{fixture, have_npm, installed};
use sc_model::{Column, Frame, ModelProviderHost, ModelRegistry, ParameterBlock, Prediction};
use sc_module::{
    LoadedModule, Module, ModuleModelProviders, ModuleSet, ModuleSource, model_providers,
};
use sc_types::Attrs;
use serde_json::json;

/// The fixture installed, loaded on a worker, and its manifest folded into a
/// one-module set — which is what `ModuleModelProviders` reads.
///
/// A set rather than a catalog on purpose: what this file is about is below the
/// store, and a Postgres for it would be a dependency in the way of reading the
/// test.
async fn loaded(tag: &str) -> (Arc<sc_module::ModuleHost>, ModuleSet, Vec<String>) {
    let (installer, host, names) = installed(tag, &["model-module"]).await;
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
        fixture("model-module").display().to_string(),
    );
    let set = ModuleSet::empty().merged(vec![LoadedModule {
        module,
        manifest: Some(manifest),
        config_spec: Vec::new(),
        issues: issues.clone(),
    }]);
    (host, set, issues)
}

/// A frame of five rows: one feature and one label.
fn frame() -> Frame {
    Frame::new(
        vec![
            (
                "area".to_owned(),
                Column::Float(vec![
                    Some(-2.0),
                    Some(-1.0),
                    Some(1.0),
                    Some(2.0),
                    Some(4.0),
                ]),
            ),
            (
                "price".to_owned(),
                Column::Float(vec![
                    Some(10.0),
                    Some(20.0),
                    Some(30.0),
                    Some(40.0),
                    Some(50.0),
                ]),
            ),
        ],
        Vec::new(),
    )
    .expect("a rectangular frame")
}

fn attrs(value: serde_json::Value) -> Attrs {
    value.as_object().expect("an object").clone()
}

#[tokio::test]
async fn a_module_supplies_model_providers_and_reports_the_ones_it_cannot() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (_host, set, issues) = loaded("model-providers-declared").await;

    let providers =
        ModuleModelProviders::new(&Arc::new(sc_module::ModuleHost::new("unused")), &set);
    let mut names: Vec<&str> = providers
        .providers()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    names.sort_unstable();
    // The two that work, and only those: a provider with no `fit`, one whose
    // outcome kind is not a kind, and one whose supervised outcome names no
    // label are each dropped.
    assert_eq!(names, ["echo_mean", "echo_sign"]);

    // And each is dropped **with a sentence**, on the module's own card, naming
    // the provider and what is wrong with it — which is the difference between
    // "why is my estimator missing" being answerable and not.
    for (provider, reason) in [
        ("echo_unfittable", "no fit function"),
        ("echo_nonsense", "haruspicy"),
        ("echo_unlabelled", "regression"),
    ] {
        assert!(
            issues
                .iter()
                .any(|i| i.contains(provider) && i.contains(reason)),
            "no issue names {provider} ({reason}): {issues:?}"
        );
    }

    // The declaration crossed whole: the module it came from (so a duplicate
    // name can be refused naming both sources), the label picker as a *query*
    // rather than an empty option list, the hyperparameter, and the
    // standardisation request.
    let mean = providers
        .providers()
        .iter()
        .find(|p| p.name == "echo_mean")
        .expect("echo_mean");
    assert_eq!(mean.module.as_deref(), Some("@saltcorn-test/model"));
    assert_eq!(mean.description, "Predict the mean of the label");
    assert!(mean.source().contains("@saltcorn-test/model"));
    assert!(!mean.standardise);
    assert_eq!(mean.hyperparameters.len(), 1);
    assert_eq!(mean.hyperparameters[0].name(), "shift");
    assert_eq!(mean.config_spec.len(), 1);
    assert_eq!(
        mean.config_spec[0].query(),
        Some(sc_model::NUMERIC_COLUMNS_QUERY)
    );

    // The other one declares its settings as a v1 `configuration_workflow` — the
    // shape a plugin that already has one reaches for — and asks to be
    // standardised.
    let sign = providers
        .providers()
        .iter()
        .find(|p| p.name == "echo_sign")
        .expect("echo_sign");
    assert!(sign.standardise);
    assert_eq!(sign.config_spec.len(), 1);
    assert_eq!(sign.config_spec[0].name(), "on");
}

#[tokio::test]
async fn a_module_provider_fits_and_predicts_across_the_seam() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("model-providers-fit").await;
    let providers: Arc<dyn ModelProviderHost> = Arc::new(ModuleModelProviders::new(&host, &set));

    // Through the registry, because that is how a fit reaches one: the supplied
    // declaration is wrapped in a `HostProvider` and the map holds one kind of
    // thing.
    let mut registry = ModelRegistry::new();
    registry
        .register_host(Arc::clone(&providers))
        .expect("two providers register");
    let provider = registry.require("echo_mean").expect("registered");

    let fitted = provider
        .fit(
            &frame(),
            &attrs(json!({ "label": "price" })),
            &attrs(json!({ "shift": 5 })),
        )
        .await
        .expect("the fit crosses");

    // The state is the provider's own and opaque to everything here — except
    // that it proves two things: the hyperparameter arrived as a number (30 + 5)
    // and the module's own configuration was in scope when its `modelproviders`
    // function was called.
    assert_eq!(fitted.state["mean"].as_f64(), Some(35.0));
    assert_eq!(fitted.state["from"], json!("https://configured.example"));

    // The parameters came back structured, in the three variants the instance
    // screen renders — so a provider it has never heard of needs no new
    // rendering.
    assert_eq!(fitted.parameters.len(), 2);
    assert_eq!(fitted.parameters[0], ParameterBlock::scalar("Mean", 30.0));
    match &fitted.parameters[1] {
        ParameterBlock::Table {
            name,
            columns,
            rows,
        } => {
            assert_eq!(name, "Rows seen");
            assert_eq!(columns, &["Column".to_owned(), "Rows".to_owned()]);
            assert_eq!(rows[0].cells, vec![json!("price"), json!(5)]);
        }
        other => panic!("expected a table, got {other:?}"),
    }

    // Prediction takes a **frame**, not a row, and answers one per row. The
    // fixture answers bare numbers, which is what a provider written over an
    // array library naturally produces.
    let predicted = provider
        .predict(&fitted.state, &frame())
        .await
        .expect("the prediction crosses");
    assert_eq!(predicted, vec![Prediction::number(35.0); 5]);

    // And the other direction: a cluster number is not a number a regression
    // predicts, so it crosses written out in full.
    let sign = registry.require("echo_sign").expect("registered");
    let fitted = sign
        .fit(&frame(), &attrs(json!({ "on": "area" })), &Attrs::new())
        .await
        .expect("the fit crosses");
    assert_eq!(
        fitted.parameters[0],
        ParameterBlock::text("Rule", "negative is 0, otherwise 1")
    );
    let predicted = sign
        .predict(&fitted.state, &frame())
        .await
        .expect("the prediction crosses");
    assert_eq!(
        predicted,
        vec![
            Prediction::Cluster { cluster: 0 },
            Prediction::Cluster { cluster: 0 },
            Prediction::Cluster { cluster: 1 },
            Prediction::Cluster { cluster: 1 },
            Prediction::Cluster { cluster: 1 },
        ]
    );
}

#[tokio::test]
async fn a_provider_that_has_gone_away_is_refused_by_name() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (host, set, _) = loaded("model-providers-gone").await;
    let providers = ModuleModelProviders::new(&host, &set);

    // The set is re-checked on this side because a module can be uninstalled
    // between a model being saved and its Fit button being pressed, and the
    // honest answer then is a sentence naming what went away rather than a
    // worker's "not loaded in this host".
    let err = providers
        .fit(
            "@saltcorn-test/model",
            "echo_unfittable",
            &frame(),
            &Attrs::new(),
            &Attrs::new(),
        )
        .await
        .expect_err("it was never registered");
    assert!(
        err.to_string().contains("echo_unfittable") && err.to_string().contains("uninstalled"),
        "{err}"
    );
}

#[test]
fn a_provider_that_answers_the_wrong_number_of_predictions_is_named() {
    // Not a worker's business, but the reader that sits on top of one: a
    // provider answering 4 predictions for 5 rows would otherwise put every row
    // after the missing one against its neighbour's answer.
    let err = model_providers::read_predictions(
        "@saltcorn-test/model",
        "echo_mean",
        json!([1.0, 2.0, 3.0, 4.0]),
        5,
    )
    .expect_err("one short");
    assert!(
        err.to_string().contains("4 predictions for 5 rows"),
        "{err}"
    );
}

#[test]
fn a_fit_answer_carries_its_warnings_and_one_without_any_is_clean() {
    let warned = model_providers::read_fit(
        "@saltcorn-test/model",
        "echo_mean",
        json!({ "state": 1, "parameters": [], "warnings": ["only five rows: add more"] }),
    )
    .expect("readable");
    assert_eq!(warned.warnings, vec!["only five rows: add more".to_owned()]);
    let clean = model_providers::read_fit(
        "@saltcorn-test/model",
        "echo_mean",
        json!({ "state": 1, "parameters": [] }),
    )
    .expect("readable");
    assert!(clean.warnings.is_empty());
}
