//! `plugins/astro` — the bundled Astro (website) module — installed from the
//! directory it ships in, loaded on a worker, and asked for a project.
//!
//! What only this side can check: that the declaration a module worker reads
//! translates into a framework whose build has a **server entry** (the one
//! thing this framework adds to the seam), and that the two files the module
//! ships beside `index.js` — the build and the server runtime — reach the
//! project through the sandbox, which lets a module `require` its own files
//! but not read them.
//!
//! Compiling and rendering the project is `plugins/astro/test/` (Node, with the
//! real `@astrojs/compiler`); running a server entry through the router is
//! `sc-server`'s `app_server_render`.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::sync::Arc;

use common::{have_npm, temp_root};
use sc_app::{FilePhase, FrameworkHost};
use sc_module::{
    BundledModules, Installer, LoadedModule, Module, ModuleFrameworks, ModuleHost,
    ModulePermissions, ModuleSet, ModuleSource,
};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// The bundled module, installed into a throwaway root and loaded on a worker.
async fn frameworks(tag: &str) -> (ModuleFrameworks, std::path::PathBuf) {
    let catalog = BundledModules::discover(None);
    let entry = catalog.get("astro").expect("the Astro module ships");
    let root = temp_root(tag);
    let installer = Installer::new(&root);
    let package = installer
        .install(ModuleSource::Local, &entry.directory.display().to_string())
        .await
        .unwrap_or_else(|e| panic!("installing plugins/astro: {e}"));
    assert_eq!(package.name, entry.name);

    let host = Arc::new(ModuleHost::new(&root));
    let manifest = host
        .load(
            &package.name,
            &installer.package_dir(&package.name),
            &json!({}),
            &ModulePermissions::closed(),
        )
        .await
        .expect("the bundled Astro module loads");
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);

    let module = Module::new(
        &package.name,
        ModuleSource::Bundled,
        entry.directory.display().to_string(),
    );
    let set = ModuleSet::empty().merged(vec![LoadedModule {
        module,
        manifest: Some(manifest),
        config_spec: Vec::new(),
        issues: Vec::new(),
    }]);
    let frameworks = ModuleFrameworks::new(&host, &set);
    assert!(frameworks.issues().is_empty(), "{:?}", frameworks.issues());
    (frameworks, entry.directory.clone())
}

fn context(site: &str) -> Json {
    json!({
        "project": "www",
        "name": "www",
        "runtime": "src/feldspar",
        "client": "src/feldspar/client.ts",
        "auth": false,
        "app": { "name": "Acme", "subdomain": "www", "description": "Rockets.",
                 "url": "http://www.<the server's base domain>" },
        "settings": { "store": "apps", "project": "www", "site": site },
        "tables": [{
            "name": "posts", "pascal": "Posts", "title": "Posts", "client": "posts",
            "pk": "id", "pk_ts_type": "number",
            "ops": { "list": true, "get": true, "create": false, "update": false, "delete": false },
            "fields": [
                { "name": "id", "label": "Id", "primary_key": true },
                { "name": "title", "label": "Title", "primary_key": false }
            ],
            "form": { "inputs": [], "minted": [] }
        }],
        "graphql": null,
        "schema_sql": "",
        "roles": [],
        "skill": ""
    })
}

#[tokio::test]
async fn the_astro_framework_declares_a_server_entry() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (frameworks, _) = frameworks("bundled-astro-decl").await;
    let declared = frameworks.frameworks();
    let astro = declared.iter().find(|f| f.name == "astro").expect("astro");
    assert!(astro.scaffolds);
    assert_eq!(
        astro
            .config_spec
            .iter()
            .map(|f| f.name())
            .collect::<Vec<_>>(),
        ["store", "project", "site"]
    );

    let config: Attrs = [
        ("store".to_owned(), json!("apps")),
        ("project".to_owned(), json!("www")),
    ]
    .into_iter()
    .collect();
    let spec = astro.build_spec(&config).unwrap();
    assert_eq!(spec.output_dir, "www/dist");
    assert_eq!(spec.server_entry.as_deref(), Some("_server/entry.js"));

    // Scripts are files, so the policy needs no script widening.
    let csp = astro.default_csp().header_value();
    assert!(!csp.contains("script-src"), "{csp}");
    assert!(csp.contains("form-action 'self'"), "{csp}");
}

#[tokio::test]
async fn the_scaffold_ships_the_build_and_runtime_and_a_page_per_table() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (frameworks, dir) = frameworks("bundled-astro-scaffold").await;
    let files = frameworks
        .framework_files(
            "astro",
            FilePhase::Scaffold,
            context("https://www.acme.example/"),
        )
        .await
        .expect("the scaffold runs");
    let file = |path: &str| {
        files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("missing {path}"))
            .contents
            .clone()
    };

    // The module's own files reach the project byte for byte.
    for name in ["build.mjs", "server.mjs"] {
        let shipped = std::fs::read_to_string(dir.join("files").join(name)).unwrap();
        assert_eq!(
            file(&format!("src/feldspar/astro/{name}")),
            shipped,
            "{name}"
        );
    }
    let config: Json = serde_json::from_str(&file("src/feldspar/astro/config.json")).unwrap();
    assert_eq!(
        config["site"], "https://www.acme.example",
        "trailing slash dropped"
    );

    // The page per table reads it as the visitor, never at admin authority.
    assert!(
        file("src/pages/posts/index.astro").contains("await db.posts.asUser().limit(50).rows()")
    );
    assert!(file("src/pages/index.astro").contains("export const prerender = true"));
    assert!(file("src/pages/contact.astro").contains("<Csrf />"));
    assert!(file("package.json").contains("\"@astrojs/compiler\": \"4.0.0\""));

    // A rebuild rewrites the generated directory and nothing of the author's.
    let runtime = frameworks
        .framework_files("astro", FilePhase::Runtime, context(""))
        .await
        .expect("the runtime generator runs");
    assert!(runtime.iter().all(|f| f.path.starts_with("src/feldspar/")));
    let config = runtime
        .iter()
        .find(|f| f.path == "src/feldspar/astro/config.json")
        .unwrap();
    assert!(
        config.contents.contains("\"site\": \"\""),
        "{}",
        config.contents
    );
}
