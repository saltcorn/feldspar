//! The **in-process** module host: a v1 plugin loaded and run on a Deno worker
//! in this process, with no `node` anywhere near it (TODO "Modules in-process",
//! phases 1 and 2).
//!
//! `npm` is still needed to *install* a module and these tests skip without it.
//! `node` is not: every assertion below runs on a `deno_runtime` worker inside
//! the test binary, which is the milestone's whole claim.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::time::Duration;

use common::{closed, have_npm, installed_on_deno};
use sc_module::{CallHosts, PoolBounds};
use serde_json::json;

/// The default bounds, which is what a server runs.
fn default_bounds() -> PoolBounds {
    PoolBounds::default()
}

#[tokio::test]
async fn a_module_loads_and_runs_in_this_process() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-load", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];

    let manifest = host
        .load(
            name,
            &installer.package_dir(name),
            &json!({ "endpoint": "e" }),
            &closed(),
        )
        .await
        .unwrap();

    // The same manifest the sidecar answers — phase 0 diffed these byte for
    // byte against `node`'s, and this is the assertion that keeps them equal.
    assert_eq!(manifest.name, *name);
    assert_eq!(manifest.api_version, Some(1));
    assert_eq!(manifest.plugin_name.as_deref(), Some("echo"));
    let actions: Vec<&str> = manifest.actions.iter().map(|a| a.name.as_str()).collect();
    assert!(actions.contains(&"echo_row"), "{actions:?}");
    assert_eq!(manifest.config_fields.len(), 2);
    let census: Vec<&str> = manifest
        .unsupported
        .iter()
        .map(|e| e.key.as_str())
        .collect();
    assert!(census.contains(&"types"), "{census:?}");
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);

    // And the action runs, with v1's argument object and the module's own
    // configuration closed over by `actions(cfg)`.
    let value = host
        .run(
            name,
            "echo_row",
            json!({ "row": { "id": 7 }, "configuration": { "greeting": "hi" } }),
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["greeting"], json!("hi"));
    assert_eq!(value["row"]["id"], json!(7));
    assert_eq!(value["module_config"]["endpoint"], json!("e"));

    // `require` reached the modules root's own `node_modules` through byonm,
    // and a v1 API this server does not implement throws, naming itself, rather
    // than quietly answering `undefined` — `File.findOne` from the refusal list
    // since the Saltcorn UI models (Phase 4), a namespace stub before.
    let err = host
        .run(name, "echo_missing_api", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("File.findOne"), "{err}");
    assert!(err.to_string().contains("not available"), "{err}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// v1's `onLoad(configuration)`, which is called and which the milestone's
/// definition of done depends on.
///
/// `@saltcorn/mqtt` is the reason: its `mqtt_publish` publishes through a
/// module-level `client` that only `onLoad` assigns, so "a trigger wired to
/// `mqtt_publish` publishes" is false in any host that does not call it. The
/// fixture stands in for it here — the real module has a test of its own,
/// against a real broker, and it needs one to be running.
#[tokio::test]
async fn a_modules_on_load_hook_runs_with_its_configuration() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-onload", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];

    let manifest = host
        .load(
            name,
            &installer.package_dir(name),
            &json!({ "endpoint": "https://echo.example" }),
            &closed(),
        )
        .await
        .unwrap();
    // A hook is not an entity type, so it is not in the census of what this
    // version does not load — it is loaded.
    let census: Vec<&str> = manifest
        .unsupported
        .iter()
        .map(|e| e.key.as_str())
        .collect();
    assert!(!census.contains(&"onLoad"), "{census:?}");
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);

    // It ran, before any action did, and it was handed the module's own stored
    // configuration rather than an empty object.
    let value = host
        .run(name, "echo_loaded", json!({}), CallHosts::default())
        .await
        .unwrap();
    assert_eq!(
        value["loaded_with"]["endpoint"],
        json!("https://echo.example")
    );

    // And a reload runs it again with what changed, because that is what a
    // module holding a connection has to be told.
    host.load(
        name,
        &installer.package_dir(name),
        &json!({ "endpoint": "https://elsewhere.example" }),
        &closed(),
    )
    .await
    .unwrap();
    let value = host
        .run(name, "echo_loaded", json!({}), CallHosts::default())
        .await
        .unwrap();
    assert_eq!(
        value["loaded_with"]["endpoint"],
        json!("https://elsewhere.example")
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 2a: v1's `functions`, in all three shapes v1 allows.
///
/// One `load` reports them with their declared signatures, and one `call` runs
/// each on the isolate its module was loaded on — which is what makes the
/// module's own configuration and its own state reachable from a formula and a
/// code body at all.
#[tokio::test]
async fn a_modules_functions_are_reported_and_called() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-functions", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];

    let manifest = host
        .load(
            name,
            &installer.package_dir(name),
            &json!({ "endpoint": "https://echo.example" }),
            &closed(),
        )
        .await
        .unwrap();

    // Reported with v1's own vocabulary: `isAsync`, the description, and the
    // declared `arguments` — which is what a code editor's signature reads.
    let by_name = |wanted: &str| {
        manifest
            .functions
            .iter()
            .find(|f| f.name == wanted)
            .unwrap_or_else(|| panic!("no function {wanted} in {:?}", manifest.functions))
            .clone()
    };
    let upper = by_name("echo_upper");
    // A bare function is judged by what it is rather than by what nobody said.
    assert!(!upper.is_async);
    assert!(upper.arguments.is_empty());
    let join = by_name("echo_join");
    assert_eq!(join.description, "Join what it was given");
    let argument_names: Vec<&str> = join.arguments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(argument_names, ["a", "b"]);
    assert_eq!(join.arguments[0].type_name.as_deref(), Some("String"));
    assert!(by_name("echo_endpoint").is_async);
    // And `functions` is no longer among the entity types this version reports
    // as unloaded, because it is loaded now.
    let census: Vec<&str> = manifest
        .unsupported
        .iter()
        .map(|e| e.key.as_str())
        .collect();
    assert!(!census.contains(&"functions"), "{census:?}");
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);

    // A synchronous v1 function, awaited across the seam (§4a's behaviour
    // difference), with v1's positional arguments.
    let value = host
        .call(name, "echo_upper", vec![json!("hi")], CallHosts::default())
        .await
        .unwrap();
    assert_eq!(value, json!("HI"));
    let value = host
        .call(
            name,
            "echo_join",
            vec![json!("a"), json!("b")],
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value, json!("a-b"));

    // The one that matters: a function closing over the module's own
    // configuration sees the configured value, because it ran where the module
    // was loaded.
    let value = host
        .call(
            name,
            "echo_endpoint",
            vec![json!("/v1")],
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value, json!("https://echo.example/v1"));

    // A result JSON will not encode is a failure naming why, never a mangled
    // value (§4a).
    let err = host
        .call(name, "echo_unserialisable", vec![], CallHosts::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not JSON"), "{err}");

    // A name the module does not have says so rather than answering null.
    let err = host
        .call(name, "echo_nonesuch", vec![], CallHosts::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("echo_nonesuch"), "{err}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// A module function doing the **module's own network**, which is the shape
/// `@saltcorn/nominatim-geocode`'s `geocode_lat` has and the reason a module
/// runs on a node-capable isolate at all.
///
/// Against a stub HTTP server in this process rather than a real geocoder, for
/// the reason `install.rs` gives about the registry: somebody else's network is
/// not a thing a test suite should depend on.
///
/// Phase 3: the module is **granted that one host**, because a module that was
/// granted nothing reaches nothing — which the next test is about.
#[tokio::test]
async fn a_module_function_reaches_a_stub_http_server() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (port, server) = stub_http_server();

    let (installer, host, names) =
        installed_on_deno("deno-fn-http", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(
        name,
        &installer.package_dir(name),
        &json!({}),
        &allowing_net(&[&format!("127.0.0.1:{port}")]),
    )
    .await
    .unwrap();

    let value = host
        .call(
            name,
            "echo_fetch",
            vec![json!(format!("http://127.0.0.1:{port}/search"))],
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["lat"], json!(55.6761));

    let _ = server.join();
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 3, specification §2: **a module granted one host reaches that host and
/// no other.**
///
/// The one the sidecar could not do at all — `node` has no permission model to
/// ask for, so a module that published to a broker could also have reached the
/// database, the metadata service and the admin's home directory.
#[tokio::test]
async fn a_module_granted_one_host_cannot_reach_a_second() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (allowed, first) = stub_http_server();
    let (denied, second) = stub_http_server();

    let (installer, host, names) =
        installed_on_deno("deno-perm-net", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(
        name,
        &installer.package_dir(name),
        &json!({}),
        &allowing_net(&[&format!("127.0.0.1:{allowed}")]),
    )
    .await
    .unwrap();

    let value = host
        .call(
            name,
            "echo_fetch",
            vec![json!(format!("http://127.0.0.1:{allowed}/"))],
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["lat"], json!(55.6761));

    // The same host on a different port is a different permission, and this is
    // the assertion that says the allow-list is an allow-list rather than a
    // switch that "network" turns on.
    let err = host
        .call(
            name,
            "echo_fetch",
            vec![json!(format!("http://127.0.0.1:{denied}/"))],
            CallHosts::default(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&denied.to_string()), "{err}");
    assert!(
        err.contains(name),
        "the denial should name the module: {err}"
    );
    assert!(
        err.contains("Settings → Modules"),
        "the denial should name where to allow it: {err}"
    );
    assert!(
        !err.contains("--allow-net"),
        "an admin has no command line to run that on: {err}"
    );
    // And the worker is untouched by a denial: it is the module's error, not the
    // host's.
    assert_eq!(host.ping().await.unwrap()["pong"], json!(true));

    let _ = first.join();
    drop(second);
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 3: **denied the filesystem, and still able to be its own code.**
///
/// The two halves of the fence, in one test. `require` walked the modules root
/// to load the module at all — so the module is running — and `node:fs` reading
/// that same root is denied, because the code a module is made of is not a
/// capability and the filesystem it sits on is.
#[tokio::test]
async fn a_module_denied_the_filesystem_cannot_read_the_modules_root() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-perm-fs", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    // Loading proves `require` read the root: a closed module is not a module
    // that cannot start.
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    assert_eq!(
        host.call(name, "echo_upper", vec![json!("hi")], CallHosts::default())
            .await
            .unwrap(),
        json!("HI")
    );

    let script = installer
        .root()
        .join(sc_module::host::HOST_SCRIPT_NAME)
        .display()
        .to_string();
    let err = host
        .call(
            name,
            "echo_read",
            vec![json!(script.clone())],
            CallHosts::default(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(name), "{err}");
    assert!(err.contains("Settings → Modules"), "{err}");

    // Granted, it reads — the same call, the same path, one row edited.
    host.load(
        name,
        &installer.package_dir(name),
        &json!({}),
        &sc_module::ModulePermissions {
            read: vec![installer.root().display().to_string()],
            ..sc_module::ModulePermissions::closed()
        },
    )
    .await
    .unwrap();
    let text = host
        .call(name, "echo_read", vec![json!(script)], CallHosts::default())
        .await
        .unwrap();
    // The first forty characters of the written script, which since the v1
    // `Table` milestone are the shared `v1_api.js`'s own first line: the host
    // script is that source with this host's half concatenated after it.
    assert!(
        text.as_str().unwrap_or_default().contains("Saltcorn 1"),
        "{text:?}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 3: an environment variable nobody granted is **not there**, rather than
/// an exception.
///
/// The one deliberate softness in the fence, and it is node compatibility rather
/// than generosity: half of npm reads `process.env.NODE_ENV` at load time, and a
/// throw there would be a rule that most modules may not be installed. Net and
/// filesystem denials are errors, because there is something for an admin to do
/// about each of them.
#[tokio::test]
async fn an_ungranted_environment_variable_is_invisible_rather_than_fatal() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-perm-env", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    // `PATH` is set in every process this could run in, and the module cannot
    // see it.
    assert_eq!(
        host.call(name, "echo_env", vec![json!("PATH")], CallHosts::default())
            .await
            .unwrap(),
        json!(null)
    );

    host.load(
        name,
        &installer.package_dir(name),
        &json!({}),
        &sc_module::ModulePermissions {
            env: vec!["PATH".into()],
            ..sc_module::ModulePermissions::closed()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        host.call(name, "echo_env", vec![json!("PATH")], CallHosts::default())
            .await
            .unwrap(),
        json!(true)
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 3: two permission sets are two isolates, even in a pool of one.
///
/// A `PermissionsContainer` is handed to a worker when its isolate is built, and
/// there is no per-module fence inside one — so a module granted a host must not
/// be resident beside a module that was granted nothing.
#[tokio::test]
async fn modules_with_different_permissions_do_not_share_a_worker() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) = installed_on_deno(
        "deno-perm-pin",
        &["echo-module", "clash-module"],
        1,
        default_bounds(),
    )
    .await;
    let echo = names
        .iter()
        .find(|n| n.contains("echo"))
        .expect("the echo fixture")
        .clone();
    let other = names
        .iter()
        .find(|n| **n != echo)
        .expect("the other fixture")
        .clone();

    host.load(&echo, &installer.package_dir(&echo), &json!({}), &closed())
        .await
        .unwrap();
    host.load(
        &other,
        &installer.package_dir(&other),
        &json!({}),
        &allowing_net(&["broker.example:1883"]),
    )
    .await
    .unwrap();
    assert_ne!(host.worker_of(&echo).await, host.worker_of(&other).await);
    assert_eq!(host.workers().await, 2, "one worker per permission set");

    // Granting the closed one the *same* host brings them together: the pool
    // pins by set, not by module.
    host.load(
        &echo,
        &installer.package_dir(&echo),
        &json!({}),
        &allowing_net(&["broker.example:1883"]),
    )
    .await
    .unwrap();
    assert_eq!(host.worker_of(&echo).await, host.worker_of(&other).await);
    // And the worker the module left is stopped rather than kept running with an
    // isolate nobody is on.
    assert_eq!(host.workers().await, 1);
    // The moved module still answers, on its new isolate.
    assert_eq!(
        host.call(
            &echo,
            "echo_upper",
            vec![json!("moved")],
            CallHosts::default()
        )
        .await
        .unwrap(),
        json!("MOVED")
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// `os.hostname()` answers with the machine's name, granted to every module
/// even with nothing else granted; the rest of `sys` stays denied.
///
/// imapflow's logger, pino, calls `os.hostname()` when it is required, so
/// without this the IMAP module could not load at all (TODO-imap.md M1.4).
#[tokio::test]
async fn a_module_reads_the_hostname_and_nothing_else_of_sys() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-sys", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();

    let value = host
        .run(name, "echo_os", json!({}), CallHosts::default())
        .await
        .unwrap();
    let expected = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .ok();
    let hostname = value["hostname"].as_str().unwrap();
    assert!(!hostname.is_empty(), "{value}");
    if let Some(expected) = expected {
        assert_eq!(hostname, expected, "the real name, not a stand-in");
    }
    let interfaces = value["interfaces"].as_str().unwrap();
    assert!(interfaces.contains("sys access"), "{value}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 2: a module's `console.*` is the server's log, not a forwarded stderr.
///
/// What is asserted here is the **seam**, which is what a test can see from
/// outside the worker thread: `console.log` and its siblings resolve to the
/// native `__scLog` the host installed, and calling four of them formats and
/// logs without throwing. Were the global missing, or its signature wrong, the
/// action would fail with a `ReferenceError` instead of answering — which is
/// exactly what this asserts it does not do. (The lines themselves go to
/// `sc_log` from the worker's own thread, and `sc_log`'s capture is
/// thread-local, so the *text* is not reachable from here.)
#[tokio::test]
async fn a_modules_console_log_goes_to_the_log_and_not_to_a_pipe() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-log", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();

    let value = host
        .run(
            name,
            "echo_log",
            json!({ "configuration": { "greeting": "hello" } }),
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value, json!("logged"));

    // And the worker is still serving afterwards: a log line is not a call, and
    // nothing about it can be mistaken for one now that there is no stream for
    // it to corrupt.
    assert_eq!(host.ping().await.unwrap()["pong"], json!(true));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// §4a's third limit, one milestone early because the seam is where it lands: a
/// result that will not serialise is a failure naming why, not a mangled value.
#[tokio::test]
async fn a_result_that_is_not_json_fails_with_a_sentence() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-cycle", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();

    let err = host
        .run(name, "echo_cycle", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not JSON"),
        "the failure should name what went wrong: {err}"
    );
    // The module's fault, and the worker is untouched by it.
    assert_eq!(err.kind(), sc_error::ErrorKind::Application);
    assert_eq!(
        host.run(
            name,
            "echo_row",
            json!({ "configuration": {} }),
            CallHosts::default()
        )
        .await
        .unwrap()["row"],
        json!(null)
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// The pool is what runs, and nothing else is: no `node` child exists while a
/// module is loaded and answering.
#[tokio::test]
async fn no_node_process_is_involved() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-nonode", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();

    // The host answers, so a module is resident…
    let pong = host.ping().await.unwrap();
    assert_eq!(pong["pong"], json!(true));
    // …and nothing is running the sidecar's script. Not "no children at all":
    // the other tests in this binary are running `npm install` beside this one,
    // and npm is a node process nobody claimed to have removed — this milestone
    // takes `node` out of *running* a module, not out of installing one.
    let sidecars: Vec<String> = children_of_this_process()
        .into_iter()
        .filter(|child| child.contains(sc_module::host::HOST_SCRIPT_NAME))
        .collect();
    assert!(
        sidecars.is_empty(),
        "a module sidecar is running: {sidecars:?}"
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// §4: `process.exit()` costs the module's worker and nothing else.
#[tokio::test]
async fn a_module_that_exits_loses_its_call_and_leaves_the_server_up() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-exit", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(
        name,
        &installer.package_dir(name),
        &json!({ "endpoint": "e" }),
        &closed(),
    )
    .await
    .unwrap();

    // The exiting call is lost, and it is lost **by name** rather than by
    // waiting out the 120-second wall clock.
    let err = host
        .run(name, "echo_exit", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("process.exit"),
        "the failure should name what happened: {err}"
    );

    // The next call gets a fresh worker with the load replayed into it — the
    // module is back without anybody having reinstalled or reconfigured it, and
    // it still has the configuration it was loaded with.
    let value = host
        .run(
            name,
            "echo_row",
            json!({ "row": {}, "configuration": { "greeting": "again" } }),
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["greeting"], json!("again"));
    assert_eq!(value["module_config"]["endpoint"], json!("e"));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// The JS slice, with the bound turned down so a test can wait for it: a module
/// that never yields is stopped, named, and costs its own worker.
#[tokio::test]
async fn a_runaway_module_is_stopped_by_the_js_slice() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let bounds = PoolBounds {
        // Long enough that `require`ing the fixture is not mistaken for a
        // runaway, short enough that a test may wait for it.
        js_slice: Duration::from_millis(1500),
        // And a wall clock well under the default two minutes, so that a
        // regression here is a failing test rather than a two-minute one: the
        // slice is what must stop this call, not the caller giving up.
        timeout: Duration::from_secs(20),
        ..PoolBounds::default()
    };
    let (installer, host, names) =
        installed_on_deno("deno-spin", &["echo-module"], 1, bounds).await;
    let name = &names[0];
    let manifest = host
        .load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();
    let actions: Vec<&str> = manifest.actions.iter().map(|a| a.name.as_str()).collect();
    assert!(actions.contains(&"echo_spin"), "{actions:?}");

    let err = host
        .run(name, "echo_spin", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("without yielding"),
        "the failure should name the slice: {err}"
    );
    // The worker is replaced and the module replayed: a runaway costs its own
    // worker and no more than that.
    let pong = host.ping().await.unwrap();
    assert_eq!(pong["pong"], json!(true));
    let value = host
        .run(
            name,
            "echo_row",
            json!({ "row": {}, "configuration": { "greeting": "still here" } }),
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["greeting"], json!("still here"));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Phase 4: `echo_exit` with a **co-resident**, which is the case the sidecar
/// used to handle and the one most easily lost by moving in-process.
///
/// Two modules on one worker, because they were granted the same thing: one of
/// them calls `process.exit()`. What has to happen is four things at once — the
/// exiting call is failed **by name** rather than waiting out the wall clock,
/// the co-resident module still answers, the module that exited answers again
/// on a worker whose loads have been replayed, and the process running all of
/// this is still here to be asked.
///
/// What the co-resident does *not* keep is its module-level state: it was
/// resident on the isolate that died, and a replay is a fresh `require`. That
/// is the price of sharing a worker, it is the sidecar's price too — where the
/// unit was the whole process rather than one worker — and it is why a module
/// worth isolating is isolated by giving it a permission set of its own.
#[tokio::test]
async fn a_co_resident_module_survives_its_neighbours_exit() {
    skip_without!(have_npm(), "npm is not on the PATH");
    // One worker per permission set and both sets closed, so the two fixtures
    // are co-resident by construction rather than by luck.
    let (installer, host, names) = installed_on_deno(
        "deno-exit-shared",
        &["echo-module", "clash-module"],
        1,
        default_bounds(),
    )
    .await;
    let echo = names
        .iter()
        .find(|n| n.contains("echo"))
        .expect("the echo fixture")
        .clone();
    let other = names
        .iter()
        .find(|n| **n != echo)
        .expect("the other fixture")
        .clone();
    for name in [&echo, &other] {
        host.load(
            name,
            &installer.package_dir(name),
            &json!({ "endpoint": "shared" }),
            &closed(),
        )
        .await
        .unwrap();
    }
    assert_eq!(host.worker_of(&echo).await, host.worker_of(&other).await);
    assert_eq!(host.workers().await, 1);

    // The exiting call is lost, by name.
    let err = host
        .run(&echo, "echo_exit", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("process.exit"),
        "the failure should name what happened: {err}"
    );

    // The co-resident answers — on a worker that was rebuilt underneath it, with
    // its load replayed, which is what makes this an interruption rather than an
    // uninstall.
    assert_eq!(
        host.run(&other, "clash_ok", json!({}), CallHosts::default())
            .await
            .unwrap(),
        json!("fine")
    );
    // And so does the module that exited, still carrying the configuration it
    // was loaded with.
    let value = host
        .run(
            &echo,
            "echo_row",
            json!({ "row": {}, "configuration": { "greeting": "back" } }),
            CallHosts::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["greeting"], json!("back"));
    assert_eq!(value["module_config"]["endpoint"], json!("shared"));

    // One worker still, not one per restart, and the pool answers.
    assert_eq!(host.workers().await, 1);
    assert_eq!(host.ping().await.unwrap()["pong"], json!(true));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// Two modules, two workers: a module is pinned to one worker for its lifetime,
/// and an exit on one worker does not reach a module on the other.
#[tokio::test]
async fn a_module_on_another_worker_does_not_notice() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) = installed_on_deno(
        "deno-pinned",
        &["echo-module", "clash-module"],
        2,
        default_bounds(),
    )
    .await;
    for name in &names {
        host.load(name, &installer.package_dir(name), &json!({}), &closed())
            .await
            .unwrap();
    }
    let echo = names
        .iter()
        .find(|n| n.contains("echo"))
        .expect("the echo fixture");
    let other = names
        .iter()
        .find(|n| *n != echo)
        .expect("the other fixture");
    assert_ne!(
        host.worker_of(echo).await,
        host.worker_of(other).await,
        "two modules and two workers should not share one"
    );

    let _ = host
        .run(echo, "echo_exit", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    // The other worker never saw it: its module answers straight away, with no
    // restart and no replay in between.
    assert_eq!(
        host.run(other, "clash_ok", json!({}), CallHosts::default())
            .await
            .unwrap(),
        json!("fine")
    );
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// An unloaded module is forgotten by the replay table too: it must not come
/// back when the worker it was on is restarted under it.
#[tokio::test]
async fn an_unloaded_module_is_not_replayed() {
    skip_without!(have_npm(), "npm is not on the PATH");
    // One worker on purpose: the point is that the *shared* worker's restart
    // does not resurrect what was uninstalled.
    let (installer, host, names) = installed_on_deno(
        "deno-unload",
        &["echo-module", "clash-module"],
        1,
        default_bounds(),
    )
    .await;
    for name in &names {
        host.load(name, &installer.package_dir(name), &json!({}), &closed())
            .await
            .unwrap();
    }
    let echo = names
        .iter()
        .find(|n| n.contains("echo"))
        .expect("the echo fixture");
    let gone = names
        .iter()
        .find(|n| *n != echo)
        .expect("the other fixture");

    host.unload(gone).await;
    assert_eq!(host.worker_of(gone).await, None);

    // Take the worker down and let the next call rebuild it. What comes back is
    // the module that is still installed, and only that one.
    let _ = host
        .run(echo, "echo_exit", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert_eq!(
        host.run(
            echo,
            "echo_row",
            json!({ "row": {}, "configuration": {} }),
            CallHosts::default()
        )
        .await
        .unwrap()["row"],
        json!({})
    );
    let err = host
        .run(gone, "clash_ok", json!({}), CallHosts::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not loaded"), "{err}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// A set granting exactly these hosts and nothing else.
fn allowing_net(hosts: &[&str]) -> sc_module::ModulePermissions {
    sc_module::ModulePermissions {
        net: hosts.iter().map(|host| (*host).to_owned()).collect(),
        ..sc_module::ModulePermissions::closed()
    }
}

/// A one-shot HTTP server on a free port: its port, and the thread serving it.
///
/// One request and one canned geocoder answer, which is all any caller here
/// wants. The thread ends on its own when nobody connects, so a test that
/// asserts a *denial* simply never joins it.
fn stub_http_server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        let body = r#"{"lat":55.6761,"lon":12.5683}"#;
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    });
    (port, server)
}

#[tokio::test]
async fn a_table_provider_declares_its_settings_its_columns_and_its_rows() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-provider", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];

    let manifest = host
        .load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();

    // The manifest names the provider and the form its own
    // `configuration_workflow` declares — flattened exactly as the module's own
    // settings are.
    let provider_names: Vec<&str> = manifest
        .table_providers
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        provider_names,
        ["echo_rows", "echo_writable", "echo_calls"],
        "{provider_names:?}"
    );
    let provider = &manifest.table_providers[0];
    assert_eq!(provider.name, "echo_rows");
    let settings: Vec<&str> = provider
        .config_fields
        .iter()
        .map(|f| f["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(settings, ["prefix", "count"], "{settings:?}");

    // The columns are a *function* of the configuration, which is v1's second
    // shape: with `count` set the provider presents a third column, and without
    // it, two.
    let bare = host
        .provider_fields(name, "echo_rows", &json!({}))
        .await
        .unwrap();
    let names_of = |fields: &[serde_json::Value]| -> Vec<String> {
        fields
            .iter()
            .map(|f| f["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(names_of(&bare), ["id", "name"]);
    let configured = host
        .provider_fields(name, "echo_rows", &json!({ "prefix": "row-", "count": 2 }))
        .await
        .unwrap();
    assert_eq!(names_of(&configured), ["id", "name", "n"]);

    // And the rows, with v1's `where`/`options` pair and the table's own name
    // reaching `get_table`'s second argument.
    let rows = host
        .provider_rows(
            name,
            "echo_rows",
            &json!({ "prefix": "row-", "count": 2 }),
            "headlines",
            &json!({ "id": 1 }),
            &json!({ "limit": 5 }),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], json!("row-1"));
    assert_eq!(rows[0]["table_was"], json!("headlines"));
    assert_eq!(rows[0]["asked"]["where"], json!({ "id": 1 }));
    assert_eq!(rows[0]["asked"]["opts"], json!({ "limit": 5 }));

    // A provider nobody supplies is a sentence naming it, not a silence.
    let err = host
        .provider_rows(
            name,
            "no_such_provider",
            &json!({}),
            "t",
            &json!({}),
            &json!({}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no_such_provider"), "{err}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// The four write ops, against the fixture's writable provider.
///
/// What is asserted here is the **module half**: that `get_table(cfg)` is asked
/// afresh for each one, that what reaches the plugin is v1's own vocabulary (a
/// record, an id, a `where` object), and that a configuration which withholds
/// the three methods is refused by name. The narrowing from a `Statement` into
/// those three is `sc-catalog`'s and is tested there.
#[tokio::test]
async fn a_writable_table_provider_answers_v1s_three_write_methods() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (installer, host, names) =
        installed_on_deno("deno-provider-write", &["echo-module"], 1, default_bounds()).await;
    let name = &names[0];
    host.load(name, &installer.package_dir(name), &json!({}), &closed())
        .await
        .unwrap();

    let writable = json!({});
    let read_only = json!({ "read_only": true });

    // Writability is a property of the *configuration*: the same provider
    // answers all three for one and none for the other.
    let writes = host
        .provider_writes(name, "echo_writable", &writable, "t")
        .await
        .unwrap();
    assert_eq!(
        writes,
        json!({ "insert": true, "update": true, "delete": true })
    );
    let writes = host
        .provider_writes(name, "echo_writable", &read_only, "t")
        .await
        .unwrap();
    assert_eq!(
        writes,
        json!({ "insert": false, "update": false, "delete": false })
    );

    // `insertRow` answers the new key.
    let answer = host
        .provider_insert(
            name,
            "echo_writable",
            &writable,
            "t",
            &json!({ "name": "two" }),
        )
        .await
        .unwrap();
    assert_eq!(answer["key"], json!(2));

    host.provider_update(
        name,
        "echo_writable",
        &writable,
        "t",
        &json!(1),
        &json!({ "name": "edited" }),
    )
    .await
    .unwrap();

    host.provider_delete(
        name,
        "echo_writable",
        &writable,
        "t",
        &json!({ "id": { "in": [2] } }),
    )
    .await
    .unwrap();

    // The rows the module now holds: row 1 renamed, row 2 gone. Module-scope
    // state, so this is also the proof that all four calls reached the one
    // worker the module is loaded on.
    let rows = host
        .provider_rows(
            name,
            "echo_writable",
            &writable,
            "t",
            &json!({}),
            &json!({}),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], json!(1));
    assert_eq!(rows[0]["name"], json!("edited"));

    // And what the plugin was handed is v1's vocabulary, unchanged.
    let calls = host
        .provider_rows(name, "echo_calls", &json!({}), "t", &json!({}), &json!({}))
        .await
        .unwrap();
    let calls: Vec<String> = calls
        .iter()
        .map(|c| c["op"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        calls
            .iter()
            .any(|c| c.contains(r#""op":"insertRow""#) && c.contains(r#""rec":{"name":"two"}"#)),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains(r#""op":"updateRow""#) && c.contains(r#""id":1"#)),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains(r#""op":"deleteRows""#) && c.contains(r#""in":[2]"#)),
        "{calls:?}"
    );

    // A write the configuration withholds names the method a module author
    // would have to add.
    let err = host
        .provider_insert(name, "echo_writable", &read_only, "t", &json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("insertRow"), "{err}");
    assert!(err.contains("echo_writable"), "{err}");

    // A delete naming no rows is refused rather than sent: a provider handed an
    // empty `where` deletes everything.
    let err = host
        .provider_delete(name, "echo_writable", &writable, "t", &json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("deletes everything"), "{err}");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// This process's children, as their full command lines, read out of `/proc`.
#[cfg(target_os = "linux")]
fn children_of_this_process() -> Vec<String> {
    let pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(status) = std::fs::read_to_string(path.join("status")) else {
            continue;
        };
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<u32>().ok());
        if parent != Some(pid) {
            continue;
        }
        // The command line rather than the name: `comm` is truncated to fifteen
        // characters, and what identifies a sidecar is its script's path.
        let command = std::fs::read_to_string(path.join("cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        children.push(command);
    }
    children
}

#[cfg(not(target_os = "linux"))]
fn children_of_this_process() -> Vec<String> {
    // No portable process table; the Linux assertion is the one that matters,
    // and it is where CI runs.
    Vec::new()
}
