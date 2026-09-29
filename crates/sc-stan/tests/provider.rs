//! `StanProvider` (TODO 2.5): what it declares, the program it reads out of a
//! store, the configuration checks it owns, and what it says when there is no
//! CmdStan.

use std::sync::Arc;

use sc_error::{Error, Result};
use sc_files::{FileStore, LocalFileStore};
use sc_model::{
    BINDINGS_KEY, DatasetShape, FitContext, ModelProvider, OutcomeSpec, PosteriorInput,
};
use sc_stan::cmdstan::{CmdStan, Source, Version};
use sc_stan::{STAN_PROVIDER, StanProvider, StoreLookup, config_keys};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use crate::programs::RADON;

fn store_dir(label: &str) -> std::path::PathBuf {
    let dir = store_dir_path(label);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A lookup knowing one store, `models`, holding `radon.stan`.
fn lookup(label: &str) -> Arc<dyn StoreLookup> {
    let dir = store_dir(label);
    std::fs::write(dir.join("radon.stan"), RADON).unwrap();
    let store: Arc<dyn FileStore> = Arc::new(LocalFileStore::new("models", &dir).unwrap());
    Arc::new(move |name: &str| -> Result<Arc<dyn FileStore>> {
        match name {
            "models" => Ok(Arc::clone(&store)),
            other => Err(Error::not_found(format!(
                "file store `{other}` is not connected"
            ))),
        }
    })
}

fn no_cmdstan() -> Result<CmdStan> {
    Err(Error::not_found(
        "$CMDSTAN is not set and ~/.cmdstan is empty",
    ))
}

fn config(pairs: Json) -> Attrs {
    serde_json::from_value(pairs).unwrap()
}

fn radon_config() -> Attrs {
    config(json!({
        config_keys::PROGRAM_STORE: "models",
        config_keys::PROGRAM: "radon.stan",
    }))
}

fn shape() -> DatasetShape {
    DatasetShape {
        table: "homes".into(),
        columns: vec![],
    }
}

#[test]
fn it_declares_a_posterior_that_binds_data_and_the_whole_form() {
    let provider = StanProvider::new(lookup("kind"), no_cmdstan());
    let kind = provider.kind();
    assert_eq!(kind.name, STAN_PROVIDER);
    assert!(kind.binds_data);
    assert_eq!(kind.outcome, OutcomeSpec::Posterior { prediction: None });
    assert!(kind.hyperparameters.is_empty());
    let fields: Vec<&str> = kind.config_spec.iter().map(|f| f.name()).collect();
    assert_eq!(
        fields,
        [
            "program_store",
            "program",
            "dimensions",
            "bindings",
            "policies",
            "labels",
            "method",
            "chains",
            "parallel_chains",
            "iter_warmup",
            "iter_sampling",
            "thin",
            "adapt_delta",
            "max_treedepth",
            "seed",
            "init",
            "save_warmup",
            "max_runtime_minutes",
            "runs_store",
            "runs_dir",
            "exclude_variables",
            "keep_draws",
        ]
    );
    // The store pickers are the file-store query the server resolves.
    let store = &kind.config_spec[0];
    assert_eq!(store.query(), Some(sc_catalog::QUERY_FILE_STORES));
    // Listed, not hidden, when there is no CmdStan — with the reason.
    assert!(
        kind.description
            .ends_with("— CmdStan was not found: $CMDSTAN is not set and ~/.cmdstan is empty"),
        "{}",
        kind.description
    );
    assert_eq!(
        provider.unavailable(),
        Some("$CMDSTAN is not set and ~/.cmdstan is empty")
    );
}

/// The instance screen's "the program has changed since this fit" (§§6, 18):
/// the snapshot a fit keeps in its state against the file in the store now.
#[tokio::test]
async fn a_snapshot_is_compared_with_the_program_in_its_store_now() {
    let provider = StanProvider::new(lookup("changed"), no_cmdstan());
    let program = provider.program(&radon_config()).await.unwrap();
    let state = json!({
        "program": { "store": program.store, "main": program.main_path() },
        "hashes": program.hashes(),
    });
    assert_eq!(
        provider.program_changed(&radon_config(), &state).await,
        Some(false)
    );

    // An edit in the IDE.
    std::fs::write(
        store_dir_path("changed").join("radon.stan"),
        format!("{RADON}\n// edited\n"),
    )
    .unwrap();
    assert_eq!(
        provider.program_changed(&radon_config(), &state).await,
        Some(true)
    );

    // The model pointed at another store is another program; one that cannot
    // be read says nothing; a state with no snapshot (another method's, or a
    // failed fit's) says nothing either.
    let elsewhere = config(json!({"program_store": "other", "program": "radon.stan"}));
    assert_eq!(
        provider.program_changed(&elsewhere, &state).await,
        Some(true)
    );
    let gone = config(json!({"program_store": "models", "program": "nope.stan"}));
    assert_eq!(provider.program_changed(&gone, &state).await, None);
    assert_eq!(
        provider.program_changed(&radon_config(), &json!({})).await,
        None
    );
}

/// [`store_dir`]'s path, without clearing it.
fn store_dir_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("sc-stan-provider-{label}-{}", std::process::id()))
}

#[tokio::test]
async fn the_interface_is_the_program_in_its_store_now() {
    let provider = StanProvider::new(lookup("interface"), no_cmdstan());
    let interface = provider.interface(&radon_config()).await.unwrap().unwrap();
    assert_eq!(
        interface
            .data
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["N", "J", "county", "x", "u", "y"]
    );

    let err = |cfg: Json| {
        let provider = &provider;
        async move {
            provider
                .interface(&config(cfg))
                .await
                .unwrap_err()
                .to_string()
        }
    };
    assert!(
        err(json!({"program": "radon.stan"}))
            .await
            .contains("`program_store`: no file store is chosen")
    );
    assert!(
        err(json!({"program_store": "models"}))
            .await
            .contains("`program`: no program is chosen in `models`")
    );
    assert!(
        err(json!({"program_store": "gone", "program": "radon.stan"}))
            .await
            .contains("file store `gone` is not connected")
    );
    assert!(
        err(json!({"program_store": "models", "program": "nope.stan"}))
            .await
            .contains("the program `nope.stan` is not in the file store `models`")
    );
}

#[test]
fn the_configuration_is_checked_for_what_only_the_provider_knows() {
    let provider = StanProvider::new(lookup("validate"), no_cmdstan());
    let check = |extra: Json| {
        let mut cfg = radon_config();
        cfg.extend(config(extra));
        provider.validate(&shape(), &cfg).map_err(|e| e.to_string())
    };
    check(json!({})).unwrap();
    check(
        json!({"chains": 4, "parallel_chains": 2, "adapt_delta": 0.95, "seed": 4711,
                 "exclude_variables": ["log_lik"], "runs_store": "models", "runs_dir": "runs",
                 BINDINGS_KEY: {"N": {"kind": "count"}}}),
    )
    .unwrap();

    let refused = [
        (
            json!({"chains": 0}),
            "`chains` must be a whole number from 1 to 64",
        ),
        (
            json!({"chains": 2, "parallel_chains": 4}),
            "`parallel_chains` is 4, but there are only 2 chains",
        ),
        (
            json!({"adapt_delta": 1.0}),
            "`adapt_delta` must be between 0 and 1",
        ),
        (
            json!({"iter_sampling": 0}),
            "`iter_sampling` must be a whole number from 1",
        ),
        (
            json!({"seed": -1}),
            "`seed` must be a whole number from 0 to 4294967295",
        ),
        (
            json!({"init": -2}),
            "`init` must be zero or a positive number",
        ),
        (json!({"bindings": ["N"]}), "`bindings` must be an object"),
        (json!({"policies": "drop"}), "`policies` must be an object"),
        (
            json!({"exclude_variables": "log_lik"}),
            "`exclude_variables` must be a list",
        ),
        (
            json!({"runs_dir": "runs"}),
            "`runs_dir` is set but `runs_store` is not",
        ),
        (
            json!({"program": "../radon.stan"}),
            "must be a path inside the file store",
        ),
    ];
    for (extra, expected) in refused {
        let err = check(extra.clone()).unwrap_err();
        assert!(err.contains(expected), "{extra}: {err}");
    }
}

#[tokio::test]
async fn without_cmdstan_a_program_is_checked_on_our_parse_with_a_notice() {
    let provider = StanProvider::new(lookup("notice"), no_cmdstan());
    let checked = provider.check_program(&radon_config()).await.unwrap();
    assert_eq!(checked.interface.data.len(), 6);
    assert!(
        checked
            .notice
            .unwrap()
            .contains("has not been checked by stanc, because CmdStan was not found")
    );
    // And a fit says the same thing, as a configuration problem.
    let err = provider
        .fit_posterior(
            &PosteriorInput {
                model: "radon".into(),
                instance: None,
                datasets: vec![],
                interface: None,
                data: json!({}),
                coordinates: Default::default(),
                unread: Default::default(),
            },
            &radon_config(),
            &FitContext::detached(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), sc_error::Repr::Config(_)), "{err:?}");
    assert!(err.to_string().contains("CmdStan was not found"));
}

#[cfg(unix)]
#[tokio::test]
async fn with_cmdstan_a_program_is_checked_by_its_stanc() {
    use std::os::unix::fs::PermissionsExt;

    // A CmdStan directory whose `bin/stanc` refuses everything, so the check
    // visibly went through it.
    let dir = store_dir("cmdstan").join("cmdstan-2.40.0");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let stanc = dir.join("bin").join("stanc");
    std::fs::write(
        &stanc,
        "#!/bin/sh\necho \"Semantic error in '$3', line 1: nope\" >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&stanc, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cmdstan = CmdStan {
        dir,
        version: Version {
            major: 2,
            minor: 40,
            patch: 0,
            pre: None,
        },
        source: Source::Env,
    };
    let provider = StanProvider::new(lookup("with-cmdstan"), Ok(cmdstan))
        .with_scratch(store_dir("with-cmdstan-scratch"));
    assert!(provider.cmdstan().is_some());
    assert!(!provider.description().contains("not found"));
    let err = provider
        .check_program(&radon_config())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "stanc refused the program `radon.stan`:\nSemantic error in 'radon.stan', line 1: nope"
        ),
        "{err}"
    );
}
