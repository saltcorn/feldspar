//! Saltcorn UI's **compat layer** (TODO "Saltcorn UI" Phase 4): the v1 models a
//! view reaches — `View`, `Page`, `getState()`, the `req`/`res` shims, `Form`,
//! `Trigger`, `File`, `User` — held to v1's shape through a real worker.
//!
//! Probed from the `compat-module` fixture's actions, run with a view snapshot
//! on the call: a module's `require` is the same table a view pattern's is, and
//! a call carrying a snapshot answers from that application exactly as a render
//! does. Pages go through the seam itself.
//!
//! Every test needs the built bundle, and the fixture tests need npm; each skips,
//! saying so, without them.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;
use crate::view_runtime::{NO_BUNDLE, bundle};
use sc_types::Role;

use std::sync::Arc;

use async_trait::async_trait;
use common::{closed, have_npm, installed};
use sc_app::AppId;
use sc_error::{ErrorKind, Result};
use sc_expr::{CodeHosts, ModuleFnHost, ModuleFunction};
use sc_module::{CallHosts, ModuleHost, ModuleViewRuntime};
use sc_viewpattern::{BUILTIN_PATTERNS, Page, ViewContext, ViewRequest, ViewRuntime, ViewSnapshot};
use serde_json::{Value as Json, json};

/// BooksDB, as the worker is handed it: four views, three pages, a menu, two
/// roles, one declared trigger and a few settings.
fn books(app: AppId) -> ViewSnapshot {
    let json = json!({
        "application": { "id": app.0.to_string(), "name": "BooksDB", "base_url": "https://books.example" },
        "config": {
            "site_name": "Books Online",
            "exttables_min_role_read": { "books": 1 },
            "enable_dynamic_updates": true,
            "secret_setting": "leaked",
        },
        "menu": [
            { "label": "All books", "type": "View", "viewname": "List Books" },
            { "label": "Home", "type": "Page", "pagename": "Overview" },
        ],
        "roles": [{ "id": 1, "role": "admin" }, { "id": 100, "role": "public" }],
        "triggers": [{ "name": "notify_author" }],
        "views": [
            {
                "id": "v-list", "name": "List Books", "viewtemplate": "List", "table_id": "books",
                "table_name": "books", "min_role": 100, "attributes": { "page_title": "Books" },
                "configuration": { "columns": [], "default_state": { "published": true, "author": "" } },
            },
            { "id": "v-show", "name": "Show Book", "viewtemplate": "Show", "table_id": "books", "min_role": 1, "configuration": {} },
            { "id": "v-feed", "name": "books Feed", "viewtemplate": "Feed", "table_id": "books", "min_role": 100, "configuration": {} },
            { "id": "v-loop", "name": "Loop", "viewtemplate": "List", "table_id": "loops", "min_role": 100, "configuration": {} },
        ],
        "pages": [
            {
                "id": "p-overview", "name": "Overview", "title": "Books", "min_role": 100,
                "layout": { "above": [
                    { "type": "blank", "isHTML": true, "contents": "<b>{{ name }}</b>" },
                    { "type": "link", "url": "/view/List", "text": "All books", "transfer_state": true },
                    { "type": "page", "page": "Footer" },
                ] },
            },
            { "id": "p-footer", "name": "Footer", "min_role": 100, "layout": { "type": "blank", "contents": "the footer" } },
            { "id": "p-loop", "name": "Loop", "min_role": 100, "layout": { "type": "page", "page": "Loop" } },
        ],
    });
    ViewSnapshot::from_json(app, 1, json.to_string())
}

/// The fixture, installed and loaded on a host that has the view runtime.
async fn compat(tag: &str) -> (Arc<ModuleHost>, String, std::path::PathBuf) {
    let (installer, _, names) = installed(tag, &["compat-module"]).await;
    let name = names[0].clone();
    let host = Arc::new(ModuleHost::new(installer.root()).with_view_runtime(bundle()));
    host.load(&name, &installer.package_dir(&name), &json!({}), &closed())
        .await
        .unwrap();
    (host, name, installer.root().to_path_buf())
}

/// One probe, run inside BooksDB.
async fn probe(host: &ModuleHost, name: &str, action: &str, views: &ViewSnapshot) -> Json {
    host.run(
        name,
        action,
        json!({}),
        CallHosts::default().with_views(views),
    )
    .await
    .unwrap_or_else(|e| panic!("the {action} probe failed: {e}"))
}

/// 4.1, 4.2, 4.6: the models answer from the snapshot, synchronously where v1's
/// do, and within the application's own triggers.
#[tokio::test]
async fn views_pages_and_triggers_answer_from_the_applications_snapshot() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-models").await;
    let views = books(AppId::new());
    let out = probe(&host, &name, "models", &views).await;

    let view = &out["view"];
    assert_eq!(view["name"], json!("List Books"));
    assert_eq!(view["viewtemplate"], json!("List"));
    // The table's name, which is what v1's `Table.findOne({ id })` keys on here.
    assert_eq!(view["table_id"], json!("books"));
    assert_eq!(view["min_role"], json!(100));
    assert_eq!(view["attributes"], json!({ "page_title": "Books" }));
    assert_eq!(view["is_view"], json!(true));
    // The pattern is the registry's, dispatched in the worker.
    assert_eq!(view["pattern_runs"], json!("function"));
    assert_eq!(view["menu_label"], json!("All books"));
    assert_eq!(
        view["select_option"],
        json!({ "name": "List Books", "label": "List Books [List on books]" })
    );
    assert_eq!(out["by_id"], json!("Show Book"));
    assert_eq!(out["by_where"], json!("books Feed"));
    assert_eq!(out["missing"], json!(true));
    // A pattern that writes into its configuration does not write into the next
    // call's snapshot.
    assert_eq!(out["unscribbled"], json!(true));
    // v1's order: by name, case-insensitively.
    assert_eq!(
        out["find"],
        json!(["books Feed", "List Books", "Loop", "Show Book"])
    );
    assert_eq!(
        out["find_where"],
        json!(["books Feed", "List Books", "Show Book"])
    );
    // The default state fills what the query does not say, and an empty default
    // is not a default.
    assert_eq!(out["combined"], json!({ "id": 3, "published": true }));
    // v1's `View.run` renders nothing for a viewer below the view's role.
    assert_eq!(out["role_gate"], json!(""));

    assert_eq!(
        out["page"],
        json!({ "name": "Overview", "title": "Books", "menu_label": "Home" })
    );
    assert_eq!(out["pages"], json!(["Footer", "Loop", "Overview"]));

    assert_eq!(out["trigger"], json!("notify_author"));
    assert_eq!(out["undeclared_trigger"], json!(true));
    assert_eq!(out["triggers"], json!(["notify_author"]));
    assert_eq!(
        out["action_options"],
        json!([
            { "optgroup": true, "label": "View actions", "options": ["Delete"] },
            { "optgroup": true, "label": "Triggers", "options": ["notify_author"] },
            { "optgroup": true, "label": "Other", "options": ["Multi-step action"] },
        ])
    );
    assert_eq!(
        out["roles"],
        json!([{ "id": 1, "role": "admin" }, { "id": 100, "role": "public" }])
    );
    assert_eq!(out["users_table"], json!(true));
    assert_eq!(
        out["file"],
        json!({
            "url": "/files/serve/books/cover.png",
            "download": "/files/download/cover.png",
            "absolute": "https://img.example/a.png",
            "mime": "image/png",
            "unknown": false,
            "relative": "a/b.txt",
        })
    );
    assert_eq!(out["library"], json!([]));
    assert_eq!(out["page_groups"], json!([]));
    assert_eq!(out["crash"], json!(true));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// 4.3: `getState()` is the application, and `getConfig` defaults as v1's does
/// over a declared key set.
#[tokio::test]
async fn get_state_is_the_application_and_get_config_defaults_as_v1s_does() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-state").await;
    let views = books(AppId::new());
    let out = probe(&host, &name, "state", &views).await;

    assert_eq!(out["same_state"], json!(true));
    assert_eq!(out["site_name"], json!("Books Online"));
    assert_eq!(out["base_url"], json!("https://books.example"));
    // Neither `getConfigCopy`'s copy nor `getConfig`'s answer is the snapshot.
    assert_eq!(out["menu_items"].as_array().map(Vec::len), Some(2));
    assert_eq!(out["locale"], json!("en"));
    // An unset key: a truthy default wins over the declared one, as in v1.
    assert_eq!(out["locale_with_default"], json!("nb"));
    assert_eq!(out["login_form"], json!(""));
    assert_eq!(out["set_and_declared"], json!({ "books": 1 }));
    // Off whatever the settings say: nothing here listens for dynamic updates.
    assert_eq!(out["dynamic_updates"], json!(false));
    assert_eq!(out["undeclared_with_default"], json!(20));
    assert_eq!(out["undeclared"], json!(true));
    // An undeclared key answers the caller's default, never the setting.
    assert_eq!(out["undeclared_but_set"], json!("fallback"));
    assert_eq!(
        out["roles"],
        json!([{ "id": 1, "role": "admin" }, { "id": 100, "role": "public" }])
    );
    assert_eq!(
        out["views"],
        json!(["List Books", "Show Book", "books Feed", "Loop"])
    );

    // The registries are the bundle's own.
    assert_eq!(out["types"], json!(true));
    assert_eq!(out["select"], json!("function"));
    assert_eq!(out["fileviews"], json!("object"));
    let mut patterns = BUILTIN_PATTERNS.to_vec();
    patterns.sort_unstable();
    assert_eq!(out["viewtemplates"], json!(patterns));
    // §12: no v1 state actions, and no module functions on this call.
    assert_eq!(out["actions"], json!({}));
    assert_eq!(out["functions"], json!([]));
    assert_eq!(out["evaluated"], json!(12));
    assert_eq!(
        out["layout"],
        json!({ "wrap": "function", "render_body": "function" })
    );
    assert_eq!(out["i18n"], json!("Save"));
    assert_eq!(out["translated"], json!("5 rows"));
    assert_eq!(out["logged"], json!(true));
    let emit = out["emit_room"]["error"].as_str().unwrap_or_default();
    assert!(
        emit.contains("state.emitRoom") && emit.contains("no socket transport"),
        "{out}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// The fifth surface, as `getState().functions` sees it.
struct Geocoder;

#[async_trait]
impl ModuleFnHost for Geocoder {
    async fn call(&self, request: Json) -> Result<Json> {
        Ok(json!({ "called": request }))
    }

    fn functions(&self) -> Vec<ModuleFunction> {
        vec![ModuleFunction {
            module: "@acme/geo".into(),
            name: "geocode".into(),
            description: String::new(),
            is_async: false,
            arguments: Vec::new(),
        }]
    }
}

/// 4.3: `functions` and `eval_context` are the call's module functions, called
/// through the `function` ask — from a pattern, and from a formula.
#[tokio::test]
async fn get_state_functions_are_the_calls_module_functions() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-functions").await;
    let views = books(AppId::new());
    let geocoder = Geocoder;
    let hosts = CodeHosts {
        module_fns: Some(&geocoder),
        ..CodeHosts::default()
    };
    let out = host
        .run(
            &name,
            "functions",
            json!({}),
            CallHosts::new(hosts, None).with_views(&views),
        )
        .await
        .unwrap();
    assert_eq!(out["names"], json!(["geocode"]));
    // Every one awaitable here, because every one is a call to another module.
    assert_eq!(out["awaitable"], json!(true));
    assert_eq!(
        out["called"],
        json!({ "called": { "module": "@acme/geo", "function": "geocode", "args": ["Oslo", 2] } })
    );
    assert_eq!(out["in_context"], json!("function"));
    assert_eq!(
        out["through_a_formula"],
        json!({ "called": { "module": "@acme/geo", "function": "geocode", "args": ["Bergen"] } })
    );
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// Outside a view call there is no application: the registries still answer,
/// and everything that needs an application says so by name.
#[tokio::test]
async fn outside_a_view_call_the_models_name_what_they_lack() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-outside").await;
    let out = host
        .run(&name, "outside", json!({}), CallHosts::default())
        .await
        .unwrap();
    assert_eq!(out["types"], json!("object"));
    for (probe, member) in [
        ("get_config", "getState().getConfig(\"site_name\")"),
        ("find_one", "View.findOne"),
        ("trigger", "Trigger.findOne"),
    ] {
        let error = out[probe]["error"].as_str().unwrap_or_default();
        assert!(
            error.contains(member) && error.contains("renders none"),
            "{probe}: {out}"
        );
    }
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// 4.4: `req` and `res` have Express's shape where v1's patterns read it, and
/// what is done to `res` is what crosses back.
#[tokio::test]
async fn the_request_and_response_shims_have_v1s_shape() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-request").await;
    let views = books(AppId::new());
    let request = serde_json::to_value(ViewRequest {
        method: "POST".into(),
        path: "/view/List Books".into(),
        query: [("author".to_owned(), "2".to_owned())].into(),
        body: json!({ "title": "Emma" }),
        headers: [
            (
                "referer".to_owned(),
                "https://books.example/page/Overview".to_owned(),
            ),
            ("x-requested-with".to_owned(), "XMLHttpRequest".to_owned()),
        ]
        .into(),
        user: Some(sc_viewpattern::ViewUser {
            id: "u1".into(),
            email: "ada@example.com".into(),
            role_id: Role::ADMIN,
        }),
        base_url: "https://books.example".into(),
        csrf_token: "tok".into(),
        wrap: None,
        locale: None,
        messages: Default::default(),
    })
    .unwrap();
    let out = host
        .run(
            &name,
            "request",
            json!({ "request": request }),
            CallHosts::default().with_views(&views),
        )
        .await
        .unwrap();
    let shape = &out["shape"];
    assert_eq!(shape["method"], json!("POST"));
    assert_eq!(shape["path"], json!("/view/List Books"));
    assert_eq!(shape["original_url"], json!("/view/List Books?author=2"));
    assert_eq!(shape["query"], json!({ "author": "2" }));
    assert_eq!(shape["body"], json!({ "title": "Emma" }));
    assert_eq!(shape["params"], json!({}));
    assert_eq!(
        shape["user"],
        json!({ "id": "u1", "email": "ada@example.com", "role_id": 1, "attributes": {} })
    );
    assert_eq!(shape["authenticated"], json!(true));
    assert_eq!(shape["xhr"], json!(true));
    // Express's `req.get("Referrer")` is the `referer` header.
    assert_eq!(
        shape["referrer"],
        json!("https://books.example/page/Overview")
    );
    assert_eq!(shape["csrf"], json!("tok"));
    assert_eq!(shape["locale"], json!("en"));
    assert_eq!(shape["translated"], json!("Hello Ada"));
    assert_eq!(shape["base_url"], json!("https://books.example"));
    assert_eq!(shape["no_files"], json!(true));
    assert_eq!(shape["flashes_read"], json!(["Saved"]));
    assert_eq!(shape["sent_before"], json!(false));
    assert_eq!(shape["sent_after"], json!(true));
    assert_eq!(
        out["response"],
        json!({
            "status": 201,
            "redirect": null,
            "flashes": [{ "kind": "success", "message": "Saved" }],
            "headers": [["Page-Title", "Books"]],
            "json": { "ok": true },
        })
    );

    // Anonymous, with nothing sent: no user, and the defaults.
    let out = host
        .run(&name, "request", json!({}), CallHosts::default())
        .await
        .unwrap();
    assert_eq!(out["shape"]["user"], Json::Null);
    assert_eq!(out["shape"]["authenticated"], json!(false));
    assert_eq!(out["shape"]["xhr"], json!(false));
    assert_eq!(out["shape"]["body"], json!({}));
    assert_eq!(out["shape"]["base_url"], json!("/"));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// 4.7: every v1 model member on the refusal list is reachable, fatal on call,
/// and names itself.
#[tokio::test]
async fn every_refused_model_member_names_itself() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-refusals").await;
    let views = books(AppId::new());
    let out = probe(&host, &name, "refusals", &views).await;
    let refusals = out.as_object().unwrap();
    for path in [
        "View.create",
        "view.delete",
        "Page.update",
        "page.authorize",
        "Trigger.create",
        "trigger.delete",
        "File.findOne",
        "User.hashPassword",
        "Crash.find",
        "state.emitRoom",
        "state.setConfig",
    ] {
        assert!(refusals.contains_key(path), "{path} was not walked: {out}");
    }
    for (path, message) in refusals {
        let message = message.as_str().unwrap_or_default();
        assert!(
            message.contains(path.as_str()) && message.contains("is not available"),
            "{path} does not name itself: {message}"
        );
    }
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// §5's absent tier, one name at a time, each with the idiom that puts it there:
/// `features?.public_user_role || 10` and `runCollabEvents ? … : []`, both
/// @saltcorn/kanban's.
#[tokio::test]
async fn kanbans_feature_detection_of_features_and_run_collab_events_finds_them_absent() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-absent").await;
    let views = books(AppId::new());
    let out = probe(&host, &name, "absent", &views).await;
    assert_eq!(out["public_user_role"], json!(10));
    assert_eq!(out["features_present"], json!(true));
    assert_eq!(out["collab"], json!([]));
    assert_eq!(out["run_collab_events_present"], json!(true));
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// §3's depth cap, through `View.run` itself; a failure named by the innermost
/// view; and `runRoute` answering through `res` as v1's does.
#[tokio::test]
async fn views_embedding_views_stop_at_the_cap_and_name_where_they_failed() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (host, name, root) = compat("view-compat-depth").await;
    let views = books(AppId::new());

    let out = probe(&host, &name, "embeds_itself", &views).await;
    let error = out["error"].as_str().unwrap_or_default();
    assert!(error.contains("more than 16 deep"), "{out}");
    assert!(error.contains("Loop → Loop"), "{out}");

    let out = probe(&host, &name, "nested_failure", &views).await;
    let error = out["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("in the view books Feed (Feed): the inner view broke"),
        "{out}"
    );
    // The outermost is named by whoever asked for it, not twice.
    assert!(!error.contains("in the view Loop"), "{out}");

    let out = probe(&host, &name, "route", &views).await;
    assert_eq!(
        out["json"],
        json!({ "hello": "Ada", "table_id": "loops", "name": "Loop" })
    );
    assert_eq!(out["quiet"], json!({ "success": "ok" }));
    assert!(
        out["missing"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("has no route nope")),
        "{out}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// 4.5: `Form` and `FieldRepeat` are v1's own, a `new Field` is a form field, and
/// a module's settings still cross as declarations.
#[tokio::test]
async fn forms_are_v1s_own_and_a_modules_settings_still_cross() {
    skip_without!(have_npm(), "npm is not on the PATH");
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let (installer, _, names) = installed("view-compat-forms", &["compat-module"]).await;
    let name = &names[0];
    let host = Arc::new(ModuleHost::new(installer.root()).with_view_runtime(bundle()));
    let manifest = host
        .load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);
    let field = |wanted: &str| {
        manifest
            .config_fields
            .iter()
            .find(|f| f["name"] == json!(wanted))
            .cloned()
            .unwrap_or_else(|| panic!("no {wanted} in {:?}", manifest.config_fields))
    };
    // The type's name, not v1's type object.
    assert_eq!(field("endpoint")["type"], json!("String"));
    assert_eq!(field("endpoint")["required"], json!(true));
    assert_eq!(field("retries")["type"], json!("Integer"));
    assert_eq!(field("retries")["default"], json!(3));

    let out = host
        .run(name, "forms", json!({}), CallHosts::default())
        .await
        .unwrap();
    // v1's `Form` (this host's stub has no form style).
    assert_eq!(out["form_style"], json!("horiz"));
    assert_eq!(out["is_field"], json!(true));
    assert_eq!(out["key"]["type"], json!("String"));
    assert_eq!(out["key"]["required"], json!(true));
    assert_eq!(out["key"]["input_type"], json!("fromtype"));
    assert_eq!(out["key"]["form_name"], json!("api_key"));
    assert!(
        out["key"]["label"]
            .as_str()
            .is_some_and(|l| !l.is_empty() && l != "api_key"),
        "{out}"
    );
    assert_eq!(
        out["retries"],
        json!({ "type": "Integer", "attributes": { "min": 0 } })
    );
    assert_eq!(
        out["author"],
        json!({ "type": "Key", "reftable_name": "authors", "input_type": "select" })
    );
    assert_eq!(
        out["repeat"],
        json!({ "is_repeat": true, "inner_is_field": true })
    );
    assert_eq!(out["validated"], json!({ "success": "abc" }));
    assert!(out["refused"]["error"].is_string(), "{out}");
    assert!(
        out["no_type"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("no type")),
        "{out}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// 4.2, through the seam: `Page.run` resolves its HTML, links and embedded
/// pages, and a page that embeds itself stops at the cap naming the cycle.
#[tokio::test]
async fn a_page_renders_its_segments_and_a_page_that_embeds_itself_is_named() {
    skip_without!(bundle().is_some(), NO_BUNDLE);
    let root = common::temp_root("view-compat-page");
    let host = Arc::new(ModuleHost::new(&root).with_view_runtime(bundle()));
    let runtime = ModuleViewRuntime::new(&host);
    let app = AppId::new();
    let views = books(app);
    let request = ViewRequest {
        query: [("name".to_owned(), "Ada".to_owned())].into(),
        ..ViewRequest::default()
    };

    let out = runtime
        .render_page(
            &Page::new(app, "Overview"),
            ViewContext::bare(&views, &request),
        )
        .await
        .unwrap();
    let html = out.body.as_str().unwrap_or_default();
    // `{{ name }}` from the query, through v1's own evaluator.
    assert!(html.contains("<b>Ada</b>"), "{html}");
    // `transfer_state` carries the query onto the link.
    assert!(html.contains("/view/List?name=Ada"), "{html}");
    // The embedded page, rendered in place.
    assert!(html.contains("the footer"), "{html}");

    let err = runtime
        .render_page(&Page::new(app, "Loop"), ViewContext::bare(&views, &request))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Application);
    let message = err.to_string();
    assert!(
        message.contains("the page Loop → the page Loop"),
        "{message}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}
