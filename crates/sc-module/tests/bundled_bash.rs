//! `plugins/bash` — the bundled **unsafe** module — installed from the
//! directory it ships in, loaded on a worker, and its `run_bash_script` run.
//!
//! The worker cannot start a process; the action asks the server to, over the
//! `shell` ask surface, and the server answers only when the call carries a
//! [`ModuleShell`] — which `ModuleAction` gives only to a module that declared
//! itself unsafe and loaded under `FELDSPAR_ALLOW_UNSAFE_MODULES=1`. The switch
//! is answered by the test here ([`ModuleShell::with_switch`]): setting a
//! process's environment while other tests run is not safe.
//!
//! Installing it runs `npm install` of the directory, which downloads nothing:
//! the module has no dependencies.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use common::{have_npm, temp_root};
use sc_module::{
    AskSurface, BundledModules, CallHosts, Installer, ModuleAgentSkill, ModuleHost,
    ModulePermissions, ModuleShell, ModuleSource, SkillCaller,
};
use serde_json::json;

#[tokio::test]
async fn the_bundled_bash_module_runs_a_script_with_the_row_as_variables() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let catalog = BundledModules::discover(None);
    let entry = catalog.get("bash").expect("the bash module ships");
    let root = temp_root("bundled-bash");
    let installer = Installer::new(&root);
    let package = installer
        .install(ModuleSource::Local, &entry.directory.display().to_string())
        .await
        .unwrap_or_else(|e| panic!("installing plugins/bash: {e}"));
    assert_eq!(package.name, "@feldspar/bash");
    let dir = installer.package_dir(&package.name);
    // What the install and the load check: the package declares itself unsafe.
    assert!(sc_module::declared_unsafe(&dir).is_some());

    // No worker permission at all: the script runs on the server, not here.
    let host = ModuleHost::new(&root);
    let manifest = host
        .load(
            &package.name,
            &dir,
            &json!({}),
            &ModulePermissions::closed(),
        )
        .await
        .unwrap();
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);
    let names: Vec<&str> = manifest.actions.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["run_bash_script"]);

    let args = json!({
        "row": { "id": 4, "name": "Ada" },
        "user": { "id": 7, "role_id": 40 },
        "configuration": {
            "code": "echo \"$ROW_NAME $ROW_ID $SC_USER_ID $SC_USER_ROLE\"\npwd\necho warn >&2\nexit 2",
            "cwd": "/tmp",
        },
    });
    let shell = ModuleShell::with_switch(&package.name, || true);
    let answer = host
        .run(
            &package.name,
            "run_bash_script",
            args.clone(),
            CallHosts::default().with_ask(AskSurface::Shell, &shell),
        )
        .await
        .unwrap();
    assert_eq!(answer["stdout"], json!("Ada 4 7 40\n/tmp\n"), "{answer}");
    assert_eq!(answer["stderr"], json!("warn\n"));
    assert_eq!(answer["exitcode"], json!(2));

    // The script can come from a field of the row instead (v1's "Field").
    let from_field = json!({
        "row": { "id": 4, "cmd": "echo \"from $ROW_ID\"" },
        "configuration": { "script_source": "Field", "code_field": "cmd" },
    });
    let answer = host
        .run(
            &package.name,
            "run_bash_script",
            from_field,
            CallHosts::default().with_ask(AskSurface::Shell, &shell),
        )
        .await
        .unwrap();
    assert_eq!(answer["stdout"], json!("from 4\n"), "{answer}");
    assert_eq!(answer["exitcode"], json!(0));

    // With the switch off, the server refuses and says how to turn it on.
    let off = ModuleShell::with_switch(&package.name, || false);
    let err = host
        .run(
            &package.name,
            "run_bash_script",
            args.clone(),
            CallHosts::default().with_ask(AskSurface::Shell, &off),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("FELDSPAR_ALLOW_UNSAFE_MODULES"),
        "{err}"
    );

    // And a call that does not carry the surface at all — what every action
    // of a module that did not declare itself unsafe is — is refused too.
    let err = host
        .run(&package.name, "run_bash_script", args, CallHosts::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unsafe"), "{err}");

    host.unload(&package.name).await;
    let _ = std::fs::remove_dir_all(&root);
}

/// The module's agent skill — v1's "Run Bash command" — read at load, described
/// for a configuration, and its `run_bash` tool called: refused without an
/// administrator, run with one.
#[tokio::test]
async fn the_bundled_bash_module_supplies_the_run_bash_agent_skill() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let catalog = BundledModules::discover(None);
    let entry = catalog.get("bash").expect("the bash module ships");
    let root = temp_root("bundled-bash-skill");
    let installer = Installer::new(&root);
    let package = installer
        .install(ModuleSource::Local, &entry.directory.display().to_string())
        .await
        .unwrap_or_else(|e| panic!("installing plugins/bash: {e}"));
    let dir = installer.package_dir(&package.name);
    let host = std::sync::Arc::new(ModuleHost::new(&root));
    let manifest = host
        .load(
            &package.name,
            &dir,
            &json!({}),
            &ModulePermissions::closed(),
        )
        .await
        .unwrap();
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);
    // `exchange` is read for its skills, so it is not reported as unsupported.
    assert!(
        !manifest
            .unsupported
            .iter()
            .any(|u| u.key.starts_with("exchange")),
        "{:?}",
        manifest.unsupported
    );
    let [skill] = manifest.agent_skills.as_slice() else {
        panic!("one skill: {:?}", manifest.agent_skills);
    };
    assert_eq!(skill.name, "run_bash");
    assert_eq!(skill.label, "Bash");
    assert_eq!(skill.tools[0]["name"], json!("run_bash"));
    assert!(skill.prompt.as_deref().unwrap().contains("via SSH"));

    let skill = ModuleAgentSkill::new(&package.name, skill.clone(), true, host.clone())
        .with_switch(|| true);
    assert_eq!(skill.config_spec()[0].name(), "timeout");
    let mut config = sc_types::Attrs::new();
    config.insert("timeout".into(), json!(5));
    let described = skill.describe(&config).await.unwrap();
    assert_eq!(described.tools[0].name, "run_bash");
    assert_eq!(skill.described(&config), described);

    let args = json!({ "command": "echo \"hi $((6 * 7))\"; echo oops >&2; exit 3" });
    // Nobody at the console: refused before the module is called.
    let err = skill
        .call(&config, "run_bash", &args, &SkillCaller::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("admin console"), "{err}");
    // An administrator: run, stdout then stderr, and the exit code.
    let admin = SkillCaller {
        user: Some(json!({ "id": "u-1", "role": 1 })),
        admin: Some(sc_proc::AdminProof::vouched(Some("u-1".into()), "a test")),
    };
    let answer = skill
        .call(&config, "run_bash", &args, &admin)
        .await
        .unwrap();
    assert_eq!(answer, json!("hi 42\noops\n[exit code 3]"));
    // Over SSH, a host name that could be an ssh option never reaches ssh.
    let err = skill
        .call(
            &config,
            "run_bash",
            &json!({ "command": "true", "ssh_host": "-oProxyCommand=touch /tmp/x" }),
            &admin,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("host"), "{err}");
    // With the switch off, nothing runs even for an administrator.
    let off = ModuleAgentSkill::new(
        &package.name,
        manifest.agent_skills[0].clone(),
        true,
        host.clone(),
    )
    .with_switch(|| false);
    let err = off
        .call(&config, "run_bash", &args, &admin)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("FELDSPAR_ALLOW_UNSAFE_MODULES"),
        "{err}"
    );

    host.unload(&package.name).await;
    let _ = std::fs::remove_dir_all(&root);
}
