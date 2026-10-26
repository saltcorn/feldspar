#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The shell (coding agent milestone, Phase 6a) against a real local file store
//! and real processes:
//!
//! - the settings are validated on save, and the tools are offered only under
//!   the grant, in `act` mode, to an admin caller, and refused by name otherwise;
//! - a command's exit code, timeout, truncated output and environment, and the
//!   refusal of a trailing `&`;
//! - managed processes: start, logs, stop, and cleanup when a run ends and when
//!   its drive is dropped (an abort);
//! - the ledger sees what the shell changed, on a plain directory and in git;
//! - the container sandbox, when a runtime and image are installed;
//! - the fingerprint and the prompt note.

use crate::common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Env, as_user, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, EnabledTrait, RunCaller, RunId, RunMode, Runner, ToolsContext, TraitContext, save_agent,
};
use sc_core_traits::{
    CFG_MAY_EDIT, CFG_MAY_USE_SHELL, CFG_ROOT, CFG_SHELL_IMAGE, CFG_SHELL_NETWORK,
    CFG_SHELL_RUNTIME, CFG_SHELL_SANDBOX, CFG_SHELL_TIMEOUT, CFG_SHELL_TIMEOUT_MAX, CFG_STORE,
    configured_scope, run_diff, running_count, tool_names,
};
use sc_error::Result;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// A `coding` configuration over `apps/web` with the shell granted.
fn shell_config() -> Attrs {
    config(&[
        (CFG_STORE, json!("apps")),
        (CFG_ROOT, json!("web")),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_USE_SHELL, json!(true)),
    ])
}

/// One run's calls to `coding`, under one run id and one trait state.
struct Session<'e> {
    env: &'e Env,
    config: Attrs,
    run: RunId,
    state: Json,
    caller: RunCaller,
}

impl<'e> Session<'e> {
    fn new(env: &'e Env, config: Attrs) -> Session<'e> {
        Session {
            env,
            config,
            run: RunId::new(),
            state: Json::Null,
            caller: RunCaller::system(),
        }
    }

    fn tool(&self, kind: &str) -> String {
        let scope = configured_scope(&self.config).unwrap();
        match kind {
            "shell" => tool_names::shell(&scope),
            "process" => tool_names::process(&scope),
            "read_file" => tool_names::read_file(&scope),
            "edit_file" => tool_names::edit_file(&scope),
            other => panic!("{other}"),
        }
    }

    async fn call(&mut self, kind: &str, args: Json) -> Result<String> {
        let tool = self.tool(kind);
        let coding = self.env.registry.require("coding")?.clone();
        let mut ctx = TraitContext {
            catalog: &self.env.catalog,
            caller: &self.caller,
            agent: "coder",
            run: self.run,
            mode: RunMode::Act,
            trait_state: &mut self.state,
            evaluator: None,
            triggers: None,
            delegate: None,
            previews: None,
            browser: None,
            requests: None,
            modules: None,
            signals: Vec::new(),
            images: Vec::new(),
            extras: Default::default(),
        };
        coding
            .call(&self.config, &tool, &args, &mut ctx)
            .await
            .map(|j| j.as_str().unwrap_or_default().to_owned())
    }

    async fn shell(&mut self, command: &str) -> String {
        match self.call("shell", json!({"command": command})).await {
            Ok(text) => text,
            Err(e) => panic!("`{command}` failed: {e}"),
        }
    }

    fn end(&self) {
        self.env
            .registry
            .require("coding")
            .unwrap()
            .run_ended(&self.config, self.run);
    }
}

/// Whether process `pid` is gone (or a zombie awaiting its reaper).
fn dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
    }
}

/// Wait up to five seconds for `pid` to die.
async fn dies(pid: u32) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if dead(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The pid a command wrote into `rel` under `dir`, once it has.
async fn pid_in(dir: &Path, rel: &str) -> u32 {
    let started = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join(rel))
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no pid in {rel}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_shell_settings_are_validated_on_save() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    env.check("coding", &shell_config()).await?;

    let with = |key: &str, value: Json| {
        let mut config = shell_config();
        config.insert(key.to_owned(), value);
        config
    };
    let err = env
        .check("coding", &with(CFG_SHELL_SANDBOX, json!("chroot")))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_SHELL_SANDBOX), "{err}");
    let err = env
        .check("coding", &with(CFG_SHELL_RUNTIME, json!("lxc")))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_SHELL_RUNTIME), "{err}");
    let mut short = with(CFG_SHELL_TIMEOUT, json!(300));
    short.insert(CFG_SHELL_TIMEOUT_MAX.to_owned(), json!(60));
    let err = env.check("coding", &short).await.unwrap_err().to_string();
    assert!(err.contains(CFG_SHELL_TIMEOUT_MAX), "{err}");
    let err = env
        .check("coding", &with(CFG_MAY_USE_SHELL, json!("yes")))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_MAY_USE_SHELL), "{err}");

    // A container sandbox needs an image, which must exist — or, with no
    // runtime installed, the runtime is what is missing. Either is refused.
    let mut container = with(CFG_SHELL_SANDBOX, json!("container"));
    let err = env
        .check("coding", &container)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_SHELL_IMAGE), "{err}");
    container.insert(
        CFG_SHELL_IMAGE.to_owned(),
        json!("feldspar-no-such-image:never"),
    );
    let err = env
        .check("coding", &container)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("feldspar-no-such-image") || err.contains("PATH"),
        "{err}"
    );
    // …but with the grant off, the sandbox is not looked for.
    container.insert(CFG_MAY_USE_SHELL.to_owned(), json!(false));
    env.check("coding", &container).await?;
    Ok(())
}

#[tokio::test]
async fn the_shell_is_offered_only_to_an_admin_in_act_mode_and_refused_by_name_otherwise()
-> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    let coding = env.registry.require("coding")?.clone();
    let capabilities = sc_llm::ModelCapabilities::built_in("", "");
    let names = |mode: RunMode, caller: Option<&RunCaller>, config: &Attrs| -> Vec<String> {
        let mut cx = sc_agent::ToolsContext::new(&env.catalog, mode, &capabilities);
        cx.caller = caller;
        coding
            .tools(&cx, config)
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    let admin = RunCaller::system();
    let user = as_user("ada@example.com");
    let offered = names(RunMode::Act, Some(&admin), &shell_config());
    assert!(
        offered.contains(&"shell_apps_web".to_owned()),
        "{offered:?}"
    );
    assert!(
        offered.contains(&"process_apps_web".to_owned()),
        "{offered:?}"
    );
    // Not to a non-admin, not with no caller, not while planning, not without
    // the grant.
    for withheld in [
        names(RunMode::Act, Some(&user), &shell_config()),
        names(RunMode::Act, None, &shell_config()),
        names(RunMode::Plan, Some(&admin), &shell_config()),
        names(
            RunMode::Act,
            Some(&admin),
            &config(&[(CFG_STORE, json!("apps")), (CFG_ROOT, json!("web"))]),
        ),
    ] {
        assert!(
            !withheld.iter().any(|n| n.starts_with("shell_")),
            "{withheld:?}"
        );
    }

    // A stale transcript's call is refused by name.
    let mut session = Session::new(&env, shell_config());
    session.caller = user;
    let err = session
        .call("shell", json!({"command": "id"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("administrator"), "{err}");
    let err = session
        .call("process", json!({"action": "list"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("administrator"), "{err}");
    let mut session = Session::new(
        &env,
        config(&[(CFG_STORE, json!("apps")), (CFG_ROOT, json!("web"))]),
    );
    let err = session
        .call("shell", json!({"command": "id"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_MAY_USE_SHELL), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_command_reports_its_exit_code_output_and_environment() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/package.json", "{}")?;
    let mut session = Session::new(&env, shell_config());

    // Stateless, in the scope's directory.
    let out = session.shell("pwd; ls").await;
    assert!(out.starts_with("exit code 0"), "{out}");
    assert!(
        out.contains(&dir.join("web").to_string_lossy().into_owned()),
        "{out}"
    );
    assert!(out.contains("package.json"), "{out}");

    // A failing command is a result, with stderr in it.
    let out = session.shell("echo to-stderr >&2; exit 3").await;
    assert!(out.starts_with("exit code 3"), "{out}");
    assert!(out.contains("to-stderr"), "{out}");

    // Non-interactive: the variables, and a closed stdin that `cat` reads at once.
    let out = session
        .shell("echo \"$CI $PAGER $GIT_PAGER $GIT_TERMINAL_PROMPT\"; cat; echo read-stdin")
        .await;
    assert!(out.contains("1 cat cat 0"), "{out}");
    assert!(out.contains("read-stdin"), "{out}");

    // Head and tail, with the elided bytes counted.
    let out = session
        .shell("echo START; head -c 100000 /dev/zero | tr '\\0' x; echo; echo END")
        .await;
    assert!(out.contains("START"), "{out}");
    assert!(out.contains("END"), "{out}");
    assert!(out.contains("bytes of output elided"), "{}", &out[..200]);
    assert!(out.len() < 25_000, "{}", out.len());

    // A trailing `&` is refused, naming the process tool.
    let err = session
        .call("shell", json!({"command": "sleep 100 &"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("process_apps_web"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_command_past_its_timeout_is_killed_with_what_it_started() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/.keep", "")?;
    let mut session = Session::new(&env, shell_config());

    let started = Instant::now();
    let out = session
        .call(
            "shell",
            json!({"command": "sleep 60 & echo $! > child.pid; wait", "timeout": 1}),
        )
        .await?;
    assert!(out.starts_with("timed out after 1s"), "{out}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // The background child went with the shell.
    let pid = pid_in(&dir, "web/child.pid").await;
    assert!(
        dies(pid).await,
        "the background sleep {pid} survived the timeout"
    );

    // A timeout above the ceiling is capped, and says so.
    let mut capped = shell_config();
    capped.insert(CFG_SHELL_TIMEOUT_MAX.to_owned(), json!(120));
    session.config = capped;
    let out = session
        .call("shell", json!({"command": "true", "timeout": 5000}))
        .await?;
    assert!(out.contains("more than the 120s allowed"), "{out}");
    Ok(())
}

#[tokio::test]
async fn a_managed_process_starts_logs_stops_and_goes_with_its_run() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/.keep", "")?;
    let mut session = Session::new(&env, shell_config());

    let out = session
        .call(
            "process",
            json!({"action": "start", "name": "sleeper", "command": "echo $$ > sleeper.pid; echo hello; exec sleep 60"}),
        )
        .await?;
    assert!(out.contains("started `sleeper`"), "{out}");
    let pid = pid_in(&dir, "web/sleeper.pid").await;
    assert_eq!(running_count(session.run, "apps_web"), 1);

    let logs = session
        .call("process", json!({"action": "logs", "name": "sleeper"}))
        .await?;
    assert!(logs.contains("hello"), "{logs}");
    assert!(logs.contains("running"), "{logs}");
    let list = session.call("process", json!({"action": "list"})).await?;
    assert!(list.contains("`sleeper`"), "{list}");

    // A second process of the same name is refused while the first runs.
    let err = session
        .call(
            "process",
            json!({"action": "start", "name": "sleeper", "command": "sleep 1"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("already running"), "{err}");

    let out = session
        .call("process", json!({"action": "stop", "name": "sleeper"}))
        .await?;
    assert!(out.contains("stopped `sleeper`"), "{out}");
    assert!(dies(pid).await);
    assert_eq!(running_count(session.run, "apps_web"), 0);

    // One that fails straight away says so.
    let out = session
        .call(
            "process",
            json!({"action": "start", "name": "broken", "command": "echo nope; exit 4"}),
        )
        .await?;
    assert!(out.contains("exited with code 4"), "{out}");

    // The run ending takes a running process with it.
    let _ = std::fs::remove_file(dir.join("web/sleeper.pid"));
    session
        .call(
            "process",
            json!({"action": "start", "name": "again", "command": "echo $$ > sleeper.pid; exec sleep 60"}),
        )
        .await?;
    let pid = pid_in(&dir, "web/sleeper.pid").await;
    session.end();
    assert!(dies(pid).await, "the run ended and {pid} is still running");
    assert_eq!(running_count(session.run, "apps_web"), 0);
    Ok(())
}

/// A coding agent over `apps/web` with the shell, driven by `replies`.
async fn shell_agent(
    env: &Env,
    name: &str,
    replies: Vec<Reply>,
) -> Result<(Agent, Arc<FakeProvider>)> {
    let agent = Agent::new(name, "main").with_trait(
        EnabledTrait::new("coding")
            .config(CFG_STORE, "apps")
            .config(CFG_ROOT, "web")
            .config(CFG_MAY_USE_SHELL, true),
    );
    save_agent(&env.catalog, &env.registry, &agent).await?;
    Ok((agent, Arc::new(FakeProvider::new(replies))))
}

#[tokio::test]
async fn a_runs_processes_are_killed_when_it_answers_and_when_it_is_aborted() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/.keep", "")?;

    // Answered: the process started in the run is gone when the drive returns.
    let (agent, provider) = shell_agent(
        &env,
        "answers",
        vec![
            Reply::calls(
                "process_apps_web",
                json!({"action": "start", "name": "dev", "command": "echo $$ > answered.pid; exec sleep 60"}),
            ),
            Reply::says("Started the dev server."),
        ],
    )
    .await?;
    let runner = Runner::new(
        &env.catalog,
        &env.registry,
        &agent,
        sc_llm::ConnectedModel::unconfigured(provider),
        RunCaller::system(),
    );
    let (run, _) = runner.start("start the dev server").await?;
    let pid = pid_in(&dir, "web/answered.pid").await;
    assert!(dies(pid).await, "{pid} outlived its run");
    assert_eq!(running_count(run.id, "apps_web"), 0);

    // Aborted: the drive is dropped while a shell command is running, as the
    // chat socket drops it, and the process it had started dies with it.
    let (agent, provider) = shell_agent(
        &env,
        "aborts",
        vec![
            Reply::calls(
                "process_apps_web",
                json!({"action": "start", "name": "dev", "command": "echo $$ > aborted.pid; exec sleep 60"}),
            ),
            Reply::calls("shell_apps_web", json!({"command": "sleep 60"})),
            Reply::says("unreachable"),
        ],
    )
    .await?;
    let runner = Runner::new(
        &env.catalog,
        &env.registry,
        &agent,
        sc_llm::ConnectedModel::unconfigured(provider),
        RunCaller::system(),
    );
    let pid_file = dir.join("web/aborted.pid");
    tokio::select! {
        _ = runner.start("start the dev server, then wait") => panic!("the run was meant to hang"),
        pid = async {
            let pid = pid_in(&dir, "web/aborted.pid").await;
            // Into the hanging shell call.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            pid
        } => {
            assert!(pid_file.exists());
            assert!(dies(pid).await, "{pid} outlived its aborted run");
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_ledger_sees_what_the_shell_changed() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/src/a.ts", "const x = 1;\n")?;
    env.put(&dir, "web/node_modules/dep/index.js", "ignored\n")?;
    let scope = configured_scope(&shell_config())?;
    let mut session = Session::new(&env, shell_config());

    session
        .call("read_file", json!({"path": "src/a.ts"}))
        .await?;
    let out = session
        .shell(
            "sed -i 's/1/2/' src/a.ts && echo new > src/b.ts && echo x > node_modules/dep/index.js",
        )
        .await;
    assert!(out.contains("M src/a.ts"), "{out}");
    assert!(out.contains("A src/b.ts"), "{out}");
    assert!(!out.contains("node_modules"), "{out}");

    let diff = run_diff(&scope, &env.catalog, &session.state).await?;
    assert!(diff.unified.contains("-const x = 1;"), "{}", diff.unified);
    assert!(diff.unified.contains("+const x = 2;"), "{}", diff.unified);
    assert!(diff.stat().contains("A src/b.ts"), "{}", diff.stat());

    // The model's read of the file is stale now: an edit asks for a re-read.
    let err = session
        .call(
            "edit_file",
            json!({"path": "src/a.ts", "old_text": "const x = 2;", "new_text": "const x = 3;"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("read_file_apps_web"), "{err}");

    // A second change to the same file keeps the first pre-image.
    session.shell("sed -i 's/2/5/' src/a.ts").await;
    let diff = run_diff(&scope, &env.catalog, &session.state).await?;
    assert!(diff.unified.contains("-const x = 1;"), "{}", diff.unified);
    assert!(diff.unified.contains("+const x = 5;"), "{}", diff.unified);
    Ok(())
}

#[tokio::test]
async fn in_git_a_clean_files_pre_image_comes_from_head() -> Result<()> {
    let have_git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_git {
        eprintln!("skipped: git is not installed");
        return Ok(());
    }
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/src/a.ts", "committed\n")?;
    env.put(&dir, "web/src/dirty.ts", "committed\n")?;
    let mut session = Session::new(&env, shell_config());
    session
        .shell(
            "git init -q . && git add -A && git -c user.email=t@example.com -c user.name=t \
             commit -qm init && echo edited-before > src/dirty.ts",
        )
        .await;
    // Start the ledger afresh: the set-up is not the change under test.
    session.state = Json::Null;

    let out = session
        .shell(
            "echo changed > src/a.ts && echo changed-again > src/dirty.ts && \
             git -c user.email=t@example.com -c user.name=t commit -qam moved-head",
        )
        .await;
    assert!(out.contains("M src/a.ts"), "{out}");
    let scope = configured_scope(&shell_config())?;
    let diff = run_diff(&scope, &env.catalog, &session.state).await?;
    // From the commit HEAD was at before the command, although it committed.
    assert!(diff.unified.contains("-committed"), "{}", diff.unified);
    assert!(diff.unified.contains("+changed"), "{}", diff.unified);
    // A file dirty before the command has its working-tree pre-image.
    assert!(diff.unified.contains("-edited-before"), "{}", diff.unified);
    assert!(!diff.unified.contains(".git/"), "{}", diff.unified);
    Ok(())
}

#[tokio::test]
async fn the_fingerprint_normalises_the_command_and_the_prompt_notes_the_shell() -> Result<()> {
    let env = Env::new().await?;
    let coding = env.registry.require("coding")?.clone();
    let config = shell_config();
    assert_eq!(
        coding.fingerprint(
            &config,
            "shell_apps_web",
            &json!({"command": "npm   test\n", "timeout": 30})
        ),
        coding.fingerprint(&config, "shell_apps_web", &json!({"command": "npm test"})),
    );
    // Other tools keep their arguments.
    assert_eq!(
        coding.fingerprint(&config, "read_file_apps_web", &json!({"path": "a"})),
        json!({"path": "a"})
    );

    let note = |caller: &RunCaller, mode: RunMode, config: &Attrs| {
        let capabilities = sc_llm::ModelCapabilities::built_in("", "");
        let cx = ToolsContext::new(&env.catalog, mode, &capabilities).for_caller(caller);
        let text = coding.prompt(&cx, config).unwrap_or_default();
        // Only the shell's paragraph: the rest of the prompt is the workflow.
        async move {
            match text.contains("Shell: ") {
                true => text,
                false => String::new(),
            }
        }
    };
    let admin = RunCaller::system();
    let text = note(&admin, RunMode::Act, &config).await;
    assert!(text.contains("shell_apps_web"), "{text}");
    assert!(text.contains("process_apps_web"), "{text}");
    assert_eq!(note(&admin, RunMode::Plan, &config).await, "");
    assert_eq!(
        note(&as_user("ada@example.com"), RunMode::Act, &config).await,
        ""
    );
    let mut off = config.clone();
    off.insert(CFG_MAY_USE_SHELL.to_owned(), json!(false));
    assert_eq!(note(&admin, RunMode::Act, &off).await, "");
    Ok(())
}

/// The runtime and image the container tests use, when both are here.
fn container_runtime() -> Option<(&'static str, &'static str)> {
    const IMAGE: &str = "debian:12-slim";
    ["podman", "docker"].into_iter().find_map(|runtime| {
        std::process::Command::new(runtime)
            .args(["image", "inspect", IMAGE])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
            .then_some((runtime, IMAGE))
    })
}

#[tokio::test]
async fn the_container_sandbox_mounts_only_the_scope_without_network() -> Result<()> {
    let Some((runtime, image)) = container_runtime() else {
        eprintln!(
            "skipped: the container sandbox tests need docker or podman with `debian:12-slim` \
             pulled"
        );
        return Ok(());
    };
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/inside.txt", "mounted\n")?;
    env.put(&dir, "outside.txt", "not mounted\n")?;
    let mut sandboxed = shell_config();
    sandboxed.insert(CFG_SHELL_SANDBOX.to_owned(), json!("container"));
    sandboxed.insert(CFG_SHELL_IMAGE.to_owned(), json!(image));
    sandboxed.insert(CFG_SHELL_RUNTIME.to_owned(), json!(runtime));
    env.check("coding", &sandboxed).await?;
    let mut session = Session::new(&env, sandboxed.clone());

    let out = session
        .shell(
            "cat /etc/debian_version >/dev/null && echo in-debian; cat inside.txt; \
             ls ../outside.txt 2>&1; ls /sys/class/net; echo made > made.txt",
        )
        .await;
    assert!(out.starts_with("exit code 0"), "{out}");
    assert!(out.contains("in-debian"), "{out}");
    assert!(out.contains("mounted"), "{out}");
    assert!(!out.contains("not mounted"), "{out}");
    assert!(!out.contains("eth0"), "{out}");
    assert!(out.contains("A made.txt"), "{out}");
    // Written as the server's own user.
    assert_eq!(env.slurp(&dir, "web/made.txt")?, "made\n");
    std::fs::remove_file(dir.join("web/made.txt")).expect("the file is ours to delete");

    // Managed processes share one container, which goes with the run.
    let out = session
        .call(
            "process",
            json!({"action": "start", "name": "dev", "command": "echo up; sleep 60"}),
        )
        .await?;
    assert!(out.contains("started `dev`"), "{out}");
    let logs = session
        .call("process", json!({"action": "logs", "name": "dev"}))
        .await?;
    assert!(logs.contains("up"), "{logs}");
    let out = session
        .call("process", json!({"action": "stop", "name": "dev"}))
        .await?;
    assert!(!out.contains("running for"), "{out}");
    session
        .call(
            "process",
            json!({"action": "start", "name": "dev2", "command": "sleep 60"}),
        )
        .await?;
    session.end();
    let name = format!("feldspar-run-{}-apps_web", session.run.0.simple());
    let started = Instant::now();
    loop {
        let listed = std::process::Command::new(runtime)
            .args(["ps", "-aq", "--filter", &format!("name={name}")])
            .output()
            .expect("the runtime lists containers");
        if listed.stdout.is_empty() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "container {name} outlived its run"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // With the network on, there is an interface besides loopback.
    sandboxed.insert(CFG_SHELL_NETWORK.to_owned(), json!(true));
    let mut session = Session::new(&env, sandboxed);
    let out = session.shell("ls /sys/class/net").await;
    assert!(
        out.lines()
            .skip(1)
            .any(|l| l.trim() != "lo" && !l.trim().is_empty()),
        "{out}"
    );
    Ok(())
}
