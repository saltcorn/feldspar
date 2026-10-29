//! Saltcorn UI's **view runtime** on the module worker (TODO "Saltcorn UI"
//! Phase 3): the built-in `@feldspar/saltcorn-ui`, the library a plugin's
//! `require` reaches, and the view snapshot crossing once per generation.
//!
//! Every test here needs the built bundle (`ui/saltcorn-ui/dist/view-runtime.js`)
//! and skips, saying so, without it. The two that install a fixture also need
//! npm. None needs `node`.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{closed, have_npm, installed, temp_root};
use sc_app::AppId;
use sc_error::ErrorKind;
use sc_module::{BUILTIN_VIEW_RUNTIME, CallHosts, ModuleHost, ModuleViewRuntime};
use sc_viewpattern::{
    BUILTIN_PATTERNS, Page, View, ViewContext, ViewRequest, ViewRuntime, ViewSnapshot,
};
use serde_json::json;

/// The built view runtime, if this checkout has one.
pub(crate) fn bundle() -> Option<PathBuf> {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(sc_viewpattern::BUNDLE_DIR_IN_CHECKOUT)
        .join(sc_viewpattern::VIEW_RUNTIME_FILE);
    file.is_file().then_some(file)
}

pub(crate) const NO_BUNDLE: &str =
    "the Saltcorn UI bundle is not built (run `npm ci && npm run build` in ui/saltcorn-ui)";

/// A snapshot of `json` for `application` at `generation`.
fn snapshot(application: AppId, generation: u64, json: &serde_json::Value) -> ViewSnapshot {
    ViewSnapshot::from_json(application, generation, json.to_string())
}

#[tokio::test]
async fn the_built_in_view_runtime_loads_with_no_modules_root_at_all() {
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let root = temp_root("view-runtime-no-root");
    assert!(!root.exists());
    let host = Arc::new(ModuleHost::new(&root).with_view_runtime(bundle()));
    let runtime = ModuleViewRuntime::new(&host);

    // The manifest: v1's six patterns, described as data.
    let patterns = runtime.patterns().await.unwrap();
    let mut names: Vec<&str> = patterns.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    let mut builtin = BUILTIN_PATTERNS.to_vec();
    builtin.sort_unstable();
    assert_eq!(names, builtin);
    let list = patterns.iter().find(|p| p.name == "List").unwrap();
    assert_eq!(list.label, "List");
    assert!(list.table_required);
    assert_eq!(list.view_quantity.as_deref(), Some("Many"));
    assert!(list.routes.iter().any(|r| r == "run_action"), "{list:?}");
    // The step *names*, which need a `req` and nothing else.
    assert_eq!(
        list.steps.first().map(String::as_str),
        Some("Columns"),
        "{list:?}"
    );
    // Nothing was installed to get here.
    assert!(!root.join("node_modules").exists());

    // One configuration step, as a call: List's first is a builder, which answers
    // the options v1's builder is opened with (TODO "The builder" 5.1). Those are
    // computed from the table, and a bare context has no schema and no caller to
    // read it with, so the call fails naming the pattern and the step. The
    // options themselves are compared against Saltcorn 1's in `sc-server`'s
    // `the_builder_options_are_what_saltcorn_1_passes`.
    let app = AppId::new();
    let views = snapshot(
        app,
        1,
        &json!({ "application": { "name": "Books" }, "views": [], "pages": [] }),
    );
    let request = ViewRequest::default();
    let message = runtime
        .config_step(
            "List",
            Some("books"),
            None,
            0,
            &json!({}),
            ViewContext::bare(&views, &request),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("List pattern") && message.contains("Columns step"),
        "{message}"
    );

    // A form step (Phase 10): ListShowList's *Views*, as this server's form
    // fields, opening with what the context says.
    let with_views = snapshot(
        app,
        3,
        &json!({
            "application": { "name": "Books" },
            "views": [
                { "name": "Books LSL", "viewtemplate": "ListShowList", "min_role": 100,
                  "table_id": "books", "configuration": { "list_view": "List Books" } },
                { "name": "List Books", "viewtemplate": "List", "min_role": 100,
                  "table_id": "books", "configuration": { "columns": [] } },
            ],
            "pages": [
                { "name": "Home", "layout": { "type": "view", "view": "List Books", "state": "shared" } },
            ],
        }),
    );
    // Over an application with no other views: listing the views over a table
    // asks each for its state fields, which reads the table, and a bare
    // context has no schema to read it from.
    let no_views = snapshot(
        app,
        4,
        &json!({ "application": { "name": "Books" }, "views": [], "pages": [] }),
    );
    let step = runtime
        .config_step(
            "ListShowList",
            Some("books"),
            Some("Books LSL"),
            0,
            &json!({ "list_width": 4 }),
            ViewContext::bare(&no_views, &request),
        )
        .await
        .unwrap();
    assert_eq!((step.name.as_str(), step.count), ("Views", 2), "{step:?}");
    assert!(
        !step.builder && !step.skip && step.context_field.is_none(),
        "{step:?}"
    );
    let names: Vec<&str> = step.fields.iter().map(|f| f.name()).collect();
    assert_eq!(names, ["list_view", "show_view", "list_width"], "{step:?}");
    let width = &step.fields[2];
    assert_eq!(
        width.base.type_,
        sc_types::TypeRef::Basic(sc_types::BasicType::Int)
    );
    assert_eq!(width.default, Some(json!(6)));
    // The form opens with what the context says.
    assert_eq!(step.values.get("list_width"), Some(&json!(4)), "{step:?}");
    assert!(step.issues.is_empty(), "{:?}", step.issues);

    // The second step keeps its values under `subtables`.
    let step = runtime
        .config_step(
            "ListShowList",
            Some("books"),
            Some("Books LSL"),
            1,
            &json!({}),
            ViewContext::bare(&no_views, &request),
        )
        .await;
    // It lists the table's relations, which needs the schema a bare context
    // does not carry: the failure names the pattern and the step.
    let message = step.unwrap_err().to_string();
    assert!(
        message.contains("ListShowList pattern") && message.contains("Subtables step"),
        "{message}"
    );

    // An initial configuration (10.2): Filter starts with an empty layout.
    let initial = runtime
        .initial_config(
            "Filter",
            Some("books"),
            Some("Books filter"),
            ViewContext::bare(&with_views, &request),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::Value::Object(initial),
        json!({ "layout": {}, "columns": [] })
    );

    // What refers to a view (10.4), by the patterns' own `connectedObjects`.
    let references = runtime
        .references("List Books", ViewContext::bare(&with_views, &request))
        .await
        .unwrap();
    assert_eq!(references.embedded_in, ["Books LSL"], "{references:?}");
    assert!(references.linked_from.is_empty(), "{references:?}");
    assert_eq!(references.pages, ["Home"], "{references:?}");

    // And a page, through `@saltcorn/markup`'s own `renderLayout`.
    let views = snapshot(
        app,
        2,
        &json!({
            "application": { "name": "Books" },
            "views": [],
            "pages": [{ "name": "Hello", "layout": { "type": "blank", "contents": "Hello, books" } }],
        }),
    );
    let page = Page::new(app, "Hello");
    let out = runtime
        .render_page(&page, ViewContext::bare(&views, &request))
        .await
        .unwrap();
    assert!(
        out.body
            .as_str()
            .is_some_and(|html| html.contains("Hello, books")),
        "{:?}",
        out.body
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// The builder 3.3: a `page` segment renders the page it names inside the page
/// that embeds it; pages that embed each other stop at the depth cap views have,
/// naming the cycle; and a `page` segment naming no page says so.
#[tokio::test]
async fn a_page_embeds_a_page_and_a_cycle_of_pages_is_named() {
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let root = temp_root("view-runtime-page-in-page");
    let host = Arc::new(ModuleHost::new(&root).with_view_runtime(bundle()));
    let runtime = ModuleViewRuntime::new(&host);
    let app = AppId::new();
    let views = snapshot(
        app,
        1,
        &json!({
            "application": { "name": "Books" },
            "views": [],
            "pages": [
                { "name": "Outer", "min_role": 100, "layout": { "above": [
                    { "type": "blank", "contents": "Outer text" },
                    { "type": "page", "page": "Inner" },
                ]}},
                { "name": "Inner", "min_role": 100,
                  "layout": { "type": "blank", "contents": "Inner text" } },
                { "name": "Ping", "min_role": 100, "layout": { "type": "page", "page": "Pong" } },
                { "name": "Pong", "min_role": 100, "layout": { "above": [
                    { "type": "blank", "contents": "pong" },
                    { "type": "page", "page": "Ping" },
                ]}},
                { "name": "Lost", "min_role": 100, "layout": { "type": "page", "page": "Nowhere" } },
            ],
        }),
    );
    let request = ViewRequest::default();

    let out = runtime
        .render_page(
            &Page::new(app, "Outer"),
            ViewContext::bare(&views, &request),
        )
        .await
        .unwrap();
    let html = out.body.as_str().unwrap();
    let outer = html.find("Outer text").expect("the outer page's text");
    let inner = html.find("Inner text").expect("the embedded page's text");
    assert!(outer < inner, "{html}");

    let err = runtime
        .render_page(&Page::new(app, "Ping"), ViewContext::bare(&views, &request))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Application);
    let msg = err.to_string();
    assert!(msg.contains("the page `Ping`"), "{msg}");
    assert!(
        msg.contains("this cycle embeds itself: the page Pong → the page Ping → the page Pong"),
        "{msg}"
    );

    let err = runtime
        .render_page(&Page::new(app, "Lost"), ViewContext::bare(&views, &request))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("the page \"Nowhere\", which does not exist"),
        "{err}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Security review #36: a `{{= }}` token's value is data — here the query
/// string, which anyone sends — so the tokens inside it only name values, are
/// looked up rather than evaluated, and are escaped even when they say `{{! }}`.
/// A value that renders itself stops after five passes. The admin's own
/// template is still JavaScript, and escapes quotes too.
#[tokio::test]
async fn a_reinterpolated_value_only_names_fields_and_is_escaped() {
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let root = temp_root("view-runtime-reinterpolate");
    let host = Arc::new(ModuleHost::new(&root).with_view_runtime(bundle()));
    let runtime = ModuleViewRuntime::new(&host);
    let app = AppId::new();
    let views = snapshot(
        app,
        1,
        &json!({
            "application": { "name": "Books" },
            "views": [],
            "pages": [{ "name": "Bio", "min_role": 100, "layout": {
                "type": "blank", "isHTML": true,
                "contents": "<p>[{{= bio}}]</p><p>[{{ name.toUpperCase() + `'\"` }}]</p>",
            }}],
        }),
    );
    let render = |bio: &str| {
        let mut request = ViewRequest::default();
        request.query.insert("bio".into(), bio.into());
        request.query.insert("name".into(), "Ann".into());
        request.query.insert("me".into(), "{{= me}}".into());
        let runtime = &runtime;
        let views = &views;
        async move {
            runtime
                .render_page(&Page::new(app, "Bio"), ViewContext::bare(views, &request))
                .await
                .map(|out| out.body.as_str().unwrap().to_owned())
                .map_err(|e| e.to_string())
        }
    };

    // Fields, and their static members, are still reached.
    let html = render("Hi {{ name }}, {{ name.length }}").await.unwrap();
    assert!(html.contains("[Hi Ann, 3]"), "{html}");
    // The admin's expression is JavaScript, and its quotes come out escaped.
    assert!(html.contains("[ANN&#39;&quot;]"), "{html}");
    // `{{! }}` in data is escaped; so is the value's own markup.
    let html = render("<b>{{! name }}</b>").await.unwrap();
    assert!(html.contains("[&lt;b&gt;Ann&lt;/b&gt;]"), "{html}");
    let mut request = ViewRequest::default();
    request.query.insert("bio".into(), "{{! raw }}".into());
    request.query.insert("raw".into(), "<script>x()</script>".into());
    request.query.insert("name".into(), "Ann".into());
    let html = runtime
        .render_page(&Page::new(app, "Bio"), ViewContext::bare(&views, &request))
        .await
        .unwrap();
    let html = html.body.as_str().unwrap();
    assert!(!html.contains("<script>x()"), "{html}");
    assert!(html.contains("[&lt;script&gt;x()&lt;/script&gt;]"), "{html}");

    // Code is refused, not run.
    for code in [
        "{{ ''.constructor.constructor('return 1')() }}",
        "{{ 1+1 }}",
        "{{ name.toUpperCase() }}",
        "{{ name['length'] }}",
        "{{ name.constructor }}",
        "{{ row.__proto__ }}",
    ] {
        let err = render(code).await.unwrap_err();
        assert!(
            err.contains("may only name fields, as in {{ firstName }}"),
            "{code}: {err}"
        );
    }
    // A name the context lacks is not looked for in the evaluator's globals.
    let err = render("{{ JSON }}").await.unwrap_err();
    assert!(err.contains("JSON is not defined"), "{err}");
    // A value that renders itself stops.
    let err = render("{{= me}}").await.unwrap_err();
    assert!(err.contains("still interpolating after 5 passes"), "{err}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// §4: the JSON crosses when the worker does not hold the generation, and not
/// otherwise. Told by lying: the second call carries different JSON at the same
/// generation, and the worker must still answer from the first.
#[tokio::test]
async fn a_view_snapshot_crosses_once_per_generation_and_a_throw_names_the_view() {
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let root = temp_root("view-runtime-snapshot");
    let host = Arc::new(ModuleHost::new(&root).with_view_runtime(bundle()));
    let runtime = ModuleViewRuntime::new(&host);
    let app = AppId::new();
    let view = View::new(app, "List Books", "List", "books");
    let request = ViewRequest::default();
    let with_view = json!({
        "application": { "name": "BooksDB" },
        "views": [{
            "name": "List Books", "viewtemplate": "List", "min_role": 100,
            "table_id": "books", "table_name": "books", "configuration": {},
        }],
        "pages": [],
    });
    let without = json!({ "application": { "name": "BooksDB" }, "views": [], "pages": [] });

    // Generation 5, with the view. The pattern runs and fails — it was given no
    // database — and the failure is the application's, naming the view, with no
    // stack in it.
    let first = snapshot(app, 5, &with_view);
    let err = runtime
        .render(&view, &json!({}), ViewContext::bare(&first, &request))
        .await
        .unwrap_err();
    let message = err.to_string();
    assert_eq!(err.kind(), ErrorKind::Application, "{message}");
    assert!(
        message.contains("the view `List Books` (List)"),
        "{message}"
    );
    assert!(!message.contains("has no view named"), "{message}");
    assert!(
        !message.contains("    at "),
        "a stack reached the message: {message}"
    );

    // Generation 5 again, carrying JSON without the view. Not sent, so the worker
    // still has the view.
    let same = snapshot(app, 5, &without);
    let message = runtime
        .render(&view, &json!({}), ViewContext::bare(&same, &request))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        !message.contains("has no view named"),
        "the worker was sent a generation it already held: {message}"
    );

    // Generation 6: sent, and the view is gone.
    let moved = snapshot(app, 6, &without);
    let message = runtime
        .render(&view, &json!({}), ViewContext::bare(&moved, &request))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("has no view named List Books"),
        "{message}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// §3.2: the runtime's name is reserved, and the reservation costs an installed
/// package that claims it nothing but the claim.
#[tokio::test]
async fn an_installed_module_claiming_the_runtimes_name_keeps_its_actions_and_takes_nothing() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (installer, _, names) = installed("view-runtime-reserved", &["reserved-name-module"]).await;
    let name = &names[0];
    assert_eq!(name, BUILTIN_VIEW_RUNTIME);
    let host = Arc::new(ModuleHost::new(installer.root()).with_view_runtime(bundle()));

    let manifest = host
        .load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    assert!(
        manifest
            .issues
            .iter()
            .any(|issue| issue.contains("built-in Saltcorn UI view runtime")),
        "{:?}",
        manifest.issues
    );
    let actions: Vec<&str> = manifest.actions.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(actions, ["still_here"]);

    // The built-in is still the runtime…
    let runtime = ModuleViewRuntime::new(&host);
    assert_eq!(
        runtime.patterns().await.unwrap().len(),
        BUILTIN_PATTERNS.len()
    );
    // …and the package is still a module, before and after the runtime started.
    let value = host
        .run(name, "still_here", json!({}), CallHosts::default())
        .await
        .unwrap();
    assert_eq!(value["still"], json!("here"));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 11: a plugin's `viewtemplates` are view patterns. The worker describes
/// them at load and keeps them out of the registry until the server says which
/// module holds each name; then they render, configure and answer routes like
/// v1's six, and are gone again when the server installs a list without them.
#[tokio::test]
async fn a_plugins_view_patterns_render_once_the_server_installs_them() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (installer, _, names) = installed("view-runtime-plugin", &["view-pattern-module"]).await;
    let name = &names[0];
    let host = Arc::new(ModuleHost::new(installer.root()).with_view_runtime(bundle()));
    let runtime = ModuleViewRuntime::new(&host);

    // 11.1: described as data. The worker does not decide clashes — `List` is
    // here, and the set takes it away (see `modules.rs`).
    let manifest = host
        .load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    let described: Vec<&str> = manifest
        .view_patterns
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(described, ["Greeting", "List"]);
    let greeting = &manifest.view_patterns[0];
    assert_eq!(greeting.steps, ["Greeting"]);
    assert_eq!(greeting.routes, ["rename"]);
    assert!(greeting.table_required);
    assert!(
        manifest.unsupported.is_empty(),
        "{:?}",
        manifest.unsupported
    );
    // 11.2: the headers, as data; one this version does not inject says so.
    assert_eq!(manifest.headers.len(), 2, "{:?}", manifest.headers);
    assert_eq!(
        manifest.headers[0].only_views.as_deref(),
        Some(&["Greeting".to_owned()][..])
    );
    assert!(manifest.headers[1].css.is_some() && manifest.headers[1].only_views.is_none());
    let issues = manifest.issues.join("\n");
    assert!(issues.contains("headerTag"), "{issues}");
    // 11.4: its virtual triggers are read and reported, not dropped.
    assert!(
        issues.contains("\"Greeting\" declares virtual triggers"),
        "{issues}"
    );

    // Not in the registry until the server installs it.
    assert_eq!(
        runtime.patterns().await.unwrap().len(),
        BUILTIN_PATTERNS.len()
    );
    host.install_view_patterns(&[(name.clone(), "Greeting".to_owned())]);
    let patterns = runtime.patterns().await.unwrap();
    assert!(
        patterns
            .iter()
            .any(|p| p.name == "Greeting" && p.steps == ["Greeting"]),
        "{patterns:?}"
    );
    // A built-in keeps its name even if a list names a module for it.
    assert_eq!(patterns.iter().filter(|p| p.name == "List").count(), 1);

    // It renders, through the real library, with v1's version tag (11.5), and
    // the call says which patterns ran (11.3).
    let app = AppId::new();
    let views = snapshot(
        app,
        1,
        &json!({
            "application": { "name": "Greetings" },
            "views": [{ "name": "Hi", "viewtemplate": "Greeting", "min_role": 100,
                        "configuration": { "salutation": "Good morning" } }],
            "pages": [],
        }),
    );
    let request = ViewRequest::default();
    let view = View::new(app, "Hi", "Greeting", "books");
    let out = runtime
        .render(&view, &json!({}), ViewContext::bare(&views, &request))
        .await
        .unwrap();
    let html = out.body.as_str().unwrap_or_default();
    assert!(html.contains("Good morning, nobody"), "{html}");
    assert!(
        html.contains(&format!(
            "data-version=\"{}\"",
            sc_viewpattern::ASSET_VERSION_TAG
        )),
        "{html}"
    );
    assert_eq!(out.patterns, ["Greeting"]);

    // Its configuration is a call per step, like a built-in's.
    let step = runtime
        .config_step(
            "Greeting",
            None,
            Some("Hi"),
            0,
            &json!({}),
            ViewContext::bare(&views, &request),
        )
        .await
        .unwrap();
    let fields: Vec<&str> = step.fields.iter().map(|f| f.name()).collect();
    assert_eq!(fields, ["salutation", "field", "live"], "{step:?}");

    // Installed whole: a list without it takes it out of the registry.
    host.install_view_patterns(&[]);
    assert_eq!(
        runtime.patterns().await.unwrap().len(),
        BUILTIN_PATTERNS.len()
    );
    let message = runtime
        .render(&view, &json!({}), ViewContext::bare(&views, &request))
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("Greeting"), "{message}");

    // 11.1: a module granted something cannot join the view runtime's worker,
    // so its patterns are not available — said, and the module still loads.
    let mut granted = closed();
    granted.env.push("HOME".to_owned());
    let manifest = host
        .load(name, &installer.package_dir(name), &json!({}), &granted)
        .await
        .unwrap();
    assert!(
        manifest.view_patterns.is_empty(),
        "{:?}",
        manifest.view_patterns
    );
    assert!(
        manifest.issues.iter().any(|i| i.contains("withdraw them")),
        "{:?}",
        manifest.issues
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// §3.3a and §5: one `require` table. A plugin's `@saltcorn/markup/tags` is v1's
/// own, and the names a plugin feature-detects are absent rather than truthy
/// stubs.
#[tokio::test]
async fn a_plugin_requires_the_real_library_and_feature_detects_what_is_absent() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (installer, without_runtime, names) =
        installed("view-runtime-library", &["library-module"]).await;
    let name = &names[0];

    let host = Arc::new(ModuleHost::new(installer.root()).with_view_runtime(bundle()));
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    let value = host
        .run(name, "reach_the_library", json!({}), CallHosts::default())
        .await
        .unwrap();
    assert_eq!(value["html"], json!("<div class=\"card\">hi</div>"));
    // `features?.public_user_role || 10`: absent, so the default.
    assert_eq!(value["public_user_role"], json!(10));
    // Present-and-undefined, not missing.
    assert_eq!(value["features_present"], json!(true));
    // `runCollabEvents ? … : []`: absent, so the fallback.
    assert_eq!(value["collab"], json!("absent"));
    // `getState` is the host's own now (Phase 4), and a name that is neither
    // implemented nor absent is still a stub.
    assert_eq!(value["get_state"], json!("function"));
    assert_eq!(value["add_tenant"], json!("function"));
    host.shutdown().await;

    // The same plugin on a host with no view runtime: stubs, which throw naming
    // what was called.
    without_runtime
        .load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    let err = without_runtime
        .run(name, "reach_the_library", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("markup/tags.div"), "{err}");
    without_runtime.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}
