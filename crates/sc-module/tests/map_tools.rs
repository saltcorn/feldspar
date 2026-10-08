//! A module's **map tools** (analytics TODO A5.12): declared as data under
//! `maptools`, they cross the worker into the manifest whole, and a
//! declaration that is not a tool is an issue on the module's card.
//!
//! What the server makes of a declaration — the form, the operations it fills
//! in — is `sc_analytics::tools::TemplateTool`'s, tested there.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use common::{fixture, have_npm, installed};
use sc_module::{LoadedModule, Module, ModuleSet, ModuleSource};
use serde_json::json;

#[tokio::test]
async fn a_module_declares_map_tools_as_data() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) = installed("map-tools-declared", &["map-tools-module"]).await;
    let name = names[0].clone();
    let manifest = host
        .load(
            &name,
            &installer.package_dir(&name),
            &json!({ "minutes": 15 }),
            &sc_module::ModulePermissions::default(),
        )
        .await
        .expect("the fixture loads");
    assert_eq!(manifest.map_tools.len(), 1, "{:?}", manifest.map_tools);
    let walk = &manifest.map_tools[0];
    assert_eq!(walk["id"], json!("walk"));
    assert_eq!(walk["base"], json!("layer"));
    // Built from the module's configuration.
    assert_eq!(walk["params"][1]["default"], json!(15));
    assert!(
        manifest
            .issues
            .iter()
            .any(|i| i.contains("\"broken\"") && i.contains("not a tool")),
        "{:?}",
        manifest.issues
    );
    assert!(!manifest.unsupported.iter().any(|u| u.key == "maptools"));

    let set = ModuleSet::empty().merged(vec![LoadedModule {
        module: Module::new(
            &name,
            ModuleSource::Local,
            fixture("map-tools-module").display().to_string(),
        ),
        manifest: Some(manifest),
        config_spec: Vec::new(),
        issues: Vec::new(),
    }]);
    let tools = set.map_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0, "@saltcorn-test/map-tools");
    assert_eq!(tools[0].1["label"], json!("Walking distance"));
}
