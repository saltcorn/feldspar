//! `process` — long-running commands the run owns: a dev server, a watcher, a
//! test runner in watch mode (TODO §7a, 6a.4).
//!
//! The shell's calls are stateless and bounded, so a command that is meant to
//! keep running cannot be one. Instead of `&`, which would leave a process
//! nothing owns, the model starts it here **by name**, reads its logs, and stops
//! it. Every process:
//!
//! - belongs to one run and one `coding` scope, in this server's memory
//! - runs in a process group of its own, so stopping it stops what it started
//! - keeps its output in a capped buffer (the first and the last bytes)
//! - is killed when the run's drive ends — answered, failed or aborted — through
//!   the trait's `run_ended` hook, and when the server stops
//!   ([`kill_all_processes`]). A server that is killed outright takes its
//!   processes with it through the parent-death signal.
//!
//! Under the container sandbox the processes of one run share **one long-lived
//! container**, so a test can reach the dev server another process started on
//! `localhost`, and the container is removed with the run.

use sc_types::ArgsExt;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use sc_agent::{RunId, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::shell::{
    Capture, Runtime, SHELL_ENV, Sandbox, Spawned, container_run_args, kill_group, lock, sandbox,
    scope_dir,
};
use crate::files::FileScope;
use sc_api::mcp::arguments;

/// What to do.
const ARG_ACTION: &str = "action";
/// The process's name.
const ARG_NAME: &str = "name";
/// The command to start.
const ARG_COMMAND: &str = "command";

/// The most processes one run may have running in one scope.
pub const MAX_PROCESSES: usize = 8;
/// The bytes kept from the start of a process's output.
pub const LOG_HEAD_BYTES: usize = 2_000;
/// The bytes kept from the end of it.
pub const LOG_TAIL_BYTES: usize = 16_000;

/// How long `start` watches a new process, so one that fails at once says so.
const START_WATCH: Duration = Duration::from_secs(1);
/// How long `stop` waits for a killed process to be reaped.
const STOP_WAIT: Duration = Duration::from_secs(3);

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("process_{}", scope.slug())
}

/// The `process` tool.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Manage long-running processes (dev servers, watchers) in {}. `start` runs \
             `command` with bash under `name`; `logs` shows a process's output so far; `stop` \
             kills it; `list` shows them all. Processes are stopped when this run ends.",
            scope.label()
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_ACTION: {"type": "string", "enum": ["start", "stop", "logs", "list"]},
                ARG_NAME: {"type": "string", "description": "A short name, e.g. `dev`. Not needed for `list`."},
                ARG_COMMAND: {"type": "string", "description": "For `start`: the bash command."},
            },
            "required": [ARG_ACTION],
            "additionalProperties": false,
        }),
    )
}

/// One managed process.
struct Managed {
    command: String,
    started: Instant,
    pgid: Option<i32>,
    output: Arc<Mutex<Capture>>,
    exit: Arc<Mutex<Option<Option<i32>>>>,
    /// Under the container sandbox: where its pid is written inside.
    pid_file: Option<String>,
}

impl Managed {
    fn running(&self) -> bool {
        lock(&self.exit).is_none()
    }

    fn status(&self) -> String {
        match *lock(&self.exit) {
            None => format!("running for {}s", self.started.elapsed().as_secs()),
            Some(Some(code)) => format!("exited with code {code}"),
            Some(None) => "killed by a signal".to_owned(),
        }
    }
}

/// One run's processes in one scope.
#[derive(Default)]
struct RunProcesses {
    processes: BTreeMap<String, Managed>,
    /// The long-lived container, under the container sandbox.
    container: Option<(Runtime, String)>,
}

/// Every run's processes, by run and scope slug.
static REGISTRY: LazyLock<Mutex<HashMap<(RunId, String), RunProcesses>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Kill every process `run` started in the scope `slug`, and remove its
/// container. Synchronous and quick: it signals, and leaves reaping to the
/// tasks that wait on the processes.
pub fn run_ended(run: RunId, slug: &str) {
    let ended = lock(&REGISTRY).remove(&(run, slug.to_owned()));
    if let Some(ended) = ended {
        release(ended);
    }
}

/// Kill every managed process of every run: the server is stopping.
pub fn kill_all_processes() {
    let all: Vec<RunProcesses> = lock(&REGISTRY).drain().map(|(_, v)| v).collect();
    all.into_iter().for_each(release);
}

/// The number of processes `run` has running in `slug`.
pub fn running_count(run: RunId, slug: &str) -> usize {
    lock(&REGISTRY)
        .get(&(run, slug.to_owned()))
        .map_or(0, |r| r.processes.values().filter(|p| p.running()).count())
}

fn release(ended: RunProcesses) {
    for process in ended.processes.values() {
        if let Some(pgid) = process.pgid {
            kill_group(pgid);
        }
    }
    if let Some((runtime, name)) = ended.container {
        // Detached: this may run in a destructor, with nothing to await on.
        if let Ok(mut child) = std::process::Command::new(runtime.program())
            .args(["rm", "-f", &name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Run one action.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let args = arguments(args, &[ARG_ACTION, ARG_NAME, ARG_COMMAND])?;
    let action = args.string_arg(ARG_ACTION)?;
    let key = (ctx.run, scope.slug());
    if action == "list" {
        return Ok(Json::String(list(&key)));
    }
    let name = args.string_arg(ARG_NAME)?;
    if name.is_empty()
        || name.len() > 40
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::invalid(format!(
            "`{ARG_NAME}` should be a short name of letters, digits, `-` and `_`, got `{name}`"
        )));
    }
    match action.as_str() {
        "start" => {
            let command = args.string_or_empty(ARG_COMMAND)?;
            if command.trim().is_empty() {
                return Err(Error::invalid(format!(
                    "`start` needs `{ARG_COMMAND}`: the command to run"
                )));
            }
            start(scope, config, ctx, key, &name, &command).await
        }
        "logs" => {
            let registry = lock(&REGISTRY);
            let process = registry
                .get(&key)
                .and_then(|r| r.processes.get(&name))
                .ok_or_else(|| unknown(&name, &key))?;
            Ok(Json::String(format!(
                "`{name}` ({}): {}\n{}",
                process.command,
                process.status(),
                output_of(process)
            )))
        }
        "stop" => stop(key, &name).await,
        other => Err(Error::invalid(format!(
            "`{ARG_ACTION}` must be start, stop, logs or list, got `{other}`"
        ))),
    }
}

fn unknown(name: &str, key: &(RunId, String)) -> Error {
    Error::invalid(format!(
        "no process named `{name}` in this run. {}",
        list(key)
    ))
}

fn list(key: &(RunId, String)) -> String {
    let registry = lock(&REGISTRY);
    let Some(run) = registry.get(key).filter(|r| !r.processes.is_empty()) else {
        return "No processes have been started in this run.".to_owned();
    };
    let mut out = String::from("Processes:");
    for (name, process) in &run.processes {
        out.push_str(&format!(
            "\n- `{name}`: {} — {}",
            process.status(),
            process.command
        ));
    }
    out
}

fn output_of(process: &Managed) -> String {
    let text = lock(&process.output).render();
    match text.trim_end() {
        "" => "(no output yet)".to_owned(),
        text => text.to_owned(),
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "tokio::spawn: an I/O pump for a child process or connection; it writes no rows, so there is no task scope to carry"
)]
async fn start(
    scope: &FileScope,
    config: &Attrs,
    ctx: &TraitContext<'_>,
    key: (RunId, String),
    name: &str,
    command: &str,
) -> Result<Json> {
    {
        let registry = lock(&REGISTRY);
        if let Some(run) = registry.get(&key) {
            if run.processes.get(name).is_some_and(Managed::running) {
                return Err(Error::invalid(format!(
                    "a process named `{name}` is already running; stop it first, or use \
                     another name"
                )));
            }
            if run.processes.values().filter(|p| p.running()).count() >= MAX_PROCESSES {
                return Err(Error::invalid(format!(
                    "this run already has {MAX_PROCESSES} processes running; stop one first"
                )));
            }
        }
    }
    let dir = scope_dir(scope, ctx.catalog).await?;
    let sandbox = sandbox(config)?;
    let capture = Capture::new(LOG_HEAD_BYTES, LOG_TAIL_BYTES);
    let (spawned, pid_file) = match &sandbox {
        Sandbox::None => (
            Spawned::spawn(
                "bash",
                &["-c".to_owned(), command.to_owned()],
                &dir,
                capture,
            )?,
            None,
        ),
        Sandbox::Container {
            runtime,
            image,
            network,
        } => {
            let container = ensure_container(&key, *runtime, image, *network, &dir).await?;
            let pid_file = format!("/tmp/feldspar-process-{name}.pid");
            let mut args = vec!["exec".to_owned()];
            for (k, v) in SHELL_ENV {
                args.push("-e".to_owned());
                args.push(format!("{k}={v}"));
            }
            // `setsid` gives the command a process group inside the container,
            // whose id is written where `stop` reads it.
            args.extend([
                container,
                "setsid".to_owned(),
                "-w".to_owned(),
                "bash".to_owned(),
                "-c".to_owned(),
                format!("echo $$ > {pid_file}; exec bash -c \"$1\""),
                "feldspar-process".to_owned(),
                command.to_owned(),
            ]);
            (
                Spawned::spawn(runtime.program(), &args, &dir, capture)?,
                Some(pid_file),
            )
        }
    };

    let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
    let Spawned {
        mut child,
        pgid,
        output,
        ..
    } = spawned;
    {
        let exit = Arc::clone(&exit);
        tokio::spawn(async move {
            let status = child.wait().await.ok().and_then(|s| s.code());
            *lock(&exit) = Some(status);
            // Whatever it left behind in its group goes too.
            if let Some(pgid) = pgid {
                kill_group(pgid);
            }
        });
    }
    let process = Managed {
        command: command.to_owned(),
        started: Instant::now(),
        pgid,
        output,
        exit,
        pid_file,
    };
    let exit = Arc::clone(&process.exit);
    lock(&REGISTRY)
        .entry(key.clone())
        .or_default()
        .processes
        .insert(name.to_owned(), process);

    // A process that fails at once should say so now, not at the next `logs`.
    let watch = Instant::now();
    while watch.elapsed() < START_WATCH && lock(&exit).is_none() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let registry = lock(&REGISTRY);
    let process = registry
        .get(&key)
        .and_then(|r| r.processes.get(name))
        .ok_or_else(|| unknown(name, &key))?;
    let head = match process.running() {
        true => format!(
            "started `{name}`. Read its output with action `logs`; it is stopped when this run \
             ends."
        ),
        false => format!("`{name}` {} straight away.", process.status()),
    };
    Ok(Json::String(format!("{head}\n{}", output_of(process))))
}

/// The run's long-lived container, started on first use.
async fn ensure_container(
    key: &(RunId, String),
    runtime: Runtime,
    image: &str,
    network: bool,
    dir: &Path,
) -> Result<String> {
    if let Some((_, name)) = lock(&REGISTRY).get(key).and_then(|r| r.container.clone()) {
        return Ok(name);
    }
    let name = format!("feldspar-run-{}-{}", key.0.0.simple(), key.1);
    let mut args = container_run_args(runtime, dir, &name, network);
    args.insert(1, "-d".to_owned());
    args.extend([image.to_owned(), "sleep".to_owned(), "infinity".to_owned()]);
    let started = tokio::process::Command::new(runtime.program())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(60), started)
        .await
        .map_err(|_| Error::config("the process container did not start within 60 seconds"))?
        .map_err(|e| Error::config(format!("could not run `{}`: {e}", runtime.program())))?;
    if !output.status.success() {
        return Err(Error::config(format!(
            "could not start the process container: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    lock(&REGISTRY).entry(key.clone()).or_default().container = Some((runtime, name.clone()));
    Ok(name)
}

async fn stop(key: (RunId, String), name: &str) -> Result<Json> {
    let (pgid, pid_file, container, exit) = {
        let registry = lock(&REGISTRY);
        let run = registry.get(&key).ok_or_else(|| unknown(name, &key))?;
        let process = run.processes.get(name).ok_or_else(|| unknown(name, &key))?;
        (
            process.pgid,
            process.pid_file.clone(),
            run.container.clone(),
            Arc::clone(&process.exit),
        )
    };
    if lock(&exit).is_none() {
        if let (Some(pid_file), Some((runtime, container))) = (pid_file, container) {
            let inner = tokio::process::Command::new(runtime.program())
                .args([
                    "exec",
                    &container,
                    "sh",
                    "-c",
                    &format!("kill -KILL -- -$(cat {pid_file})"),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = tokio::time::timeout(Duration::from_secs(10), inner).await;
        }
        if let Some(pgid) = pgid {
            kill_group(pgid);
        }
        let waited = Instant::now();
        while waited.elapsed() < STOP_WAIT && lock(&exit).is_none() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let registry = lock(&REGISTRY);
    let process = registry
        .get(&key)
        .and_then(|r| r.processes.get(name))
        .ok_or_else(|| unknown(name, &key))?;
    Ok(Json::String(format!(
        "stopped `{name}`: {}\n{}",
        process.status(),
        output_of(process)
    )))
}
