#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `build_application` against a real store, a real application row and a real
//! (stand-in) bundler (§11.3, TODO Phase 5).
//!
//! The bundler here is a shell script rather than `vite`, for the reason
//! `sc-app`'s own build tests use one: what is under test is the path from *an
//! agent asked to build* to *the diagnostics it is handed*, and a script that
//! prints a `tsc` error and exits 2 exercises every step of that path in a tenth
//! of a second. The parsing of those diagnostics is pinned in the unit tests; the
//! wiring is pinned here.
//!
//! Two properties are the point:
//!
//! - **A failed build is a result, not an error.** `built: false`, the tools'
//!   own output, and the file/line/message triples parsed out of it — because a
//!   model that is told only "the build failed" cannot fix anything.
//! - **The application is resolved from its stored row**, so an agent pointed at
//!   one that has been deleted leaves the live set with a reason instead of
//!   failing when the model calls the tool.

use crate::common;

use common::{Env, config};
use sc_agent::RunCaller;
use sc_agent_builtins::{CFG_APPLICATION, tool_names};
use sc_app::{Application, FrameworkRef, save_application};
use sc_error::Result;
use serde_json::json;

/// The framework config an admin would have filled in: source in `web`, output
/// in `web/dist`, built by a script in the source directory.
fn framework(store: &str) -> FrameworkRef {
    FrameworkRef::new("code")
        .with("store", store)
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
}

/// Write `body` as the app's stand-in bundler, executable.
fn bundler(path: &std::path::Path, body: &str) -> Result<()> {
    std::fs::write(path, body).map_err(|e| sc_error::Error::config(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| sc_error::Error::config(e.to_string()))?;
    }
    Ok(())
}

#[tokio::test]
async fn a_broken_build_comes_back_as_diagnostics_with_file_and_line() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/src/App.tsx", "export const App = 1;\n")?;
    // A stand-in `tsc`: it reports one type error the way tsc does, and fails.
    bundler(
        &dir.join("web/build.sh"),
        "#!/bin/sh\n\
         echo \"src/App.tsx(12,5): error TS2322: Type 'number' is not assignable to type 'string'.\"\n\
         exit 2\n",
    )?;

    let app = Application::new("Todo", "todo", framework("apps"));
    save_application(&env.catalog, &app).await?;

    let cfg = config(&[(CFG_APPLICATION, json!("todo"))]);
    env.check("build_application", &cfg).await?;
    assert_eq!(
        env.tools("build_application", &cfg)[0].name,
        tool_names::build_application("todo")
    );

    let report = env
        .call("build_application", &cfg, json!({}), &RunCaller::system())
        .await?;
    assert_eq!(report["built"], json!(false), "{report}");
    let diagnostics = report["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{report}");
    assert_eq!(diagnostics[0]["file"], json!("src/App.tsx"));
    assert_eq!(diagnostics[0]["line"], json!(12));
    assert_eq!(diagnostics[0]["column"], json!(5));
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("not assignable"),
        "{report}"
    );
    // The whole output travels beside the parsed list: what this module did not
    // recognise is not lost.
    assert!(
        report["output"].as_str().unwrap().contains("TS2322"),
        "{report}"
    );
    Ok(())
}

#[tokio::test]
async fn a_build_that_succeeds_says_so_and_carries_what_the_tools_said() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/src/App.tsx", "export const App = 1;\n")?;
    bundler(
        &dir.join("web/build.sh"),
        "#!/bin/sh\n\
         set -e\n\
         echo 'built 3 modules'\n\
         mkdir -p dist\n\
         printf '<!doctype html><div id=root></div>' > dist/index.html\n",
    )?;

    let app = Application::new("Todo", "todo", framework("apps"));
    save_application(&env.catalog, &app).await?;

    let report = env
        .call(
            "build_application",
            &config(&[(CFG_APPLICATION, json!("todo"))]),
            json!({}),
            &RunCaller::system(),
        )
        .await?;
    assert_eq!(report["built"], json!(true), "{report}");
    assert!(
        report["output"]
            .as_str()
            .unwrap()
            .contains("built 3 modules"),
        "{report}"
    );
    assert!(report["diagnostics"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn an_application_that_is_not_there_leaves_the_agent_invalid_with_a_reason() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;

    // Never existed: refused on save, and on load, naming the subdomain.
    let err = env
        .check(
            "build_application",
            &config(&[(CFG_APPLICATION, json!("gone"))]),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("gone"), "{err}");

    // An app that exists is accepted, and the check resolves its build step
    // rather than merely finding the row — an app whose framework could not
    // build would be refused here, where the admin can change it.
    let app = Application::new("Todo", "todo", framework("apps"));
    save_application(&env.catalog, &app).await?;
    env.check(
        "build_application",
        &config(&[(CFG_APPLICATION, json!("todo"))]),
    )
    .await?;

    // And a blank setting is refused as a blank setting.
    assert!(env.check("build_application", &config(&[])).await.is_err());
    Ok(())
}
