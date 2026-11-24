//! **Unsafe modules**: a module that declares it can do what no permission set
//! bounds — run any program on the server — and the one switch that lets one be
//! installed.
//!
//! A module's worker reaches nothing it was not granted ([`crate::permissions`]),
//! and subprocesses are not on any allow-list. Some modules exist to do exactly
//! that: the bundled `bash` module's `run_bash_script` runs whatever script an
//! admin typed into a trigger, as the server's own user. Asking an admin to
//! widen a permission for that would be a lie about what the click grants, so
//! such a module **declares itself unsafe** in its `package.json`:
//!
//! ```json
//! { "name": "@feldspar/bash",
//!   "feldspar": { "unsafe": "Runs any bash script on the server, as the server's user." } }
//! ```
//!
//! (`"unsafe": true` is accepted too; a sentence is better, because the card and
//! the refusal print it.) A bundled module says the same in its
//! `feldspar-module.json`, so the catalog can say so before anything is
//! downloaded.
//!
//! # The switch
//!
//! An unsafe module is installed, loaded and given its powers only when the
//! **operator** started the server with [`ALLOW_UNSAFE_MODULES`]`=1`. Not an
//! admin setting: an admin's account is a password away from the web, and the
//! operator is the person who decided what runs on the machine. Checked at
//! three points, so taking the variable away later takes the powers away too:
//!
//! - **install** ([`refuse_unless_allowed`]): without it, the install is
//!   refused — and on a first install the package npm just put on the disk is
//!   taken off again;
//! - **load** (`modules::load_one`): a module that is installed but declares
//!   itself unsafe on a server without the variable does not load, and its card
//!   says why;
//! - **each call** ([`ModuleShell`]): the variable is read again before every
//!   script, so the powers end the moment the operator restarts without it.
//!
//! # What an unsafe module gets
//!
//! The `shell` ask surface ([`AskSurface::Shell`](crate::host::AskSurface)),
//! carried only by the actions of a module that declared itself unsafe and was
//! loaded under the switch — and by its agent skills' tools
//! ([`crate::agent_skills`]) in a run of an administrator at the admin
//! console, on that administrator's proof. The worker itself still cannot spawn anything: the
//! script runs on the server, through [`sc_proc::HostCommand`], with a cleared
//! environment (the base set plus what the module passes), a timeout, its own
//! process group, and a log line naming [`UNSAFE_MODULE_SHELL`].

use std::path::Path;

use sc_error::{Error, Result};
use sc_proc::{AdminProof, EnvName, HostCommand, Operand, Program, Timeout};
use serde_json::{Map, Value as Json, json};

use crate::host::Ask;

/// The variable that, set to `1`, lets this server install and run unsafe
/// modules.
pub const ALLOW_UNSAFE_MODULES: &str = "FELDSPAR_ALLOW_UNSAFE_MODULES";

/// On whose authority an unsafe module's script runs: the operator's switch
/// ([`ALLOW_UNSAFE_MODULES`]) and the administrator who installed the module and
/// configured the trigger. Listed in the system-authority inventory.
pub(crate) const UNSAFE_MODULE_SHELL: &str =
    "an unsafe module's script (FELDSPAR_ALLOW_UNSAFE_MODULES, an admin's install and trigger)";

/// How long a script may run when its call does not say.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// The longest a call may ask for: an hour. A longer job is a service, not an
/// action.
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// How much of each of stdout and stderr is answered; the rest is cut, and the
/// answer says so.
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Whether the operator allowed unsafe modules on this server.
pub fn unsafe_modules_allowed() -> bool {
    std::env::var(ALLOW_UNSAFE_MODULES).is_ok_and(|v| v.trim() == "1")
}

/// What a package's `package.json` says about itself under `feldspar.unsafe`:
/// the sentence, or a stock one for `true`; `None` for a package that does not
/// declare itself unsafe (or has no readable `package.json`).
pub fn declared_unsafe(package_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(package_dir.join("package.json")).ok()?;
    let json: Json = serde_json::from_str(&text).ok()?;
    unsafe_reason(json.get("feldspar")?.get("unsafe")?)
}

/// The reason an `unsafe` entry gives: a non-empty string as it stands, `true`
/// as a stock sentence, anything else as not unsafe.
pub fn unsafe_reason(value: &Json) -> Option<String> {
    match value {
        Json::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Json::Bool(true) => Some("it can run any program on the server".to_owned()),
        _ => None,
    }
}

/// The sentence an unsafe module is refused with on a server without the
/// switch.
pub fn refusal(name: &str, reason: &str) -> String {
    format!(
        "the module `{name}` declares itself unsafe ({reason}), and this server does not \
         allow unsafe modules; the operator allows them by starting the server with \
         {ALLOW_UNSAFE_MODULES}=1"
    )
}

/// Refuse an unsafe module unless the operator allowed them.
pub fn refuse_unless_allowed(name: &str, reason: Option<&str>) -> Result<()> {
    match reason {
        Some(reason) if !unsafe_modules_allowed() => Err(Error::config(refusal(name, reason))),
        _ => Ok(()),
    }
}

/// The `shell` ask of an unsafe module's action:
///
/// ```json
/// { "op": "bash", "script": "echo $ROW_NAME", "cwd": "/srv/data",
///   "env": { "ROW_NAME": "Ada" }, "timeout_seconds": 60 }
/// ```
///
/// answered `{ "exitcode", "stdout", "stderr", "timed_out" }`. A script whose
/// first line is a shebang (`#!/usr/bin/env python3`) runs under that
/// interpreter instead of bash. A non-zero exit is an answer, not an error:
/// what to do about it is the module's decision.
///
/// And on another machine, over SSH:
///
/// ```json
/// { "op": "ssh", "script": "uptime", "host": "db1", "user": "deploy",
///   "port": 2222, "timeout_seconds": 60 }
/// ```
///
/// which runs `ssh -o BatchMode=yes -o ConnectTimeout=<timeout> [-p port]
/// [user@]host "bash -s"` with the script as its stdin, as Saltcorn v1's
/// bash agent skill does — so only a host the server's user can reach without
/// a password (a key, an agent through `SSH_AUTH_SOCK`) is reached at all. The
/// answer has the same shape; an exit of 255 is usually ssh's own failure,
/// and its stderr says which.
pub struct ModuleShell<'a> {
    module: &'a str,
    /// Whether the switch is on, asked on every call: [`unsafe_modules_allowed`],
    /// or a test's answer.
    allowed: fn() -> bool,
    /// Whose authority a script runs on, when somebody's other than the
    /// switch's and the install's: an administrator at the console, for a
    /// module's agent skill.
    proof: Option<AdminProof>,
}

impl<'a> ModuleShell<'a> {
    /// The shell `module` may use.
    pub fn new(module: &'a str) -> ModuleShell<'a> {
        ModuleShell {
            module,
            allowed: unsafe_modules_allowed,
            proof: None,
        }
    }

    /// The shell `module` may use, with the switch answered by `allowed`
    /// instead of the environment — for a test, which cannot set a process's
    /// environment safely while other tests run.
    #[doc(hidden)]
    pub fn with_switch(module: &'a str, allowed: fn() -> bool) -> ModuleShell<'a> {
        ModuleShell {
            module,
            allowed,
            proof: None,
        }
    }

    /// The same shell, its scripts run on `proof` — the administrator whose
    /// agent run asked for them — so the log line names who.
    #[must_use]
    pub fn on_behalf_of(mut self, proof: AdminProof) -> ModuleShell<'a> {
        self.proof = Some(proof);
        self
    }

    fn authority(&self) -> AdminProof {
        self.proof
            .clone()
            .unwrap_or_else(|| AdminProof::vouched(None, UNSAFE_MODULE_SHELL))
    }

    async fn bash(&self, plan: &Map<String, Json>) -> Result<Json> {
        let script = script_of(plan, "bash")?;
        let timeout = timeout_of(plan, "bash")?;
        let mut env = Vec::new();
        match plan.get("env") {
            None | Some(Json::Null) => {}
            Some(Json::Object(vars)) => {
                for (name, value) in vars {
                    let value = match value {
                        Json::Null => continue,
                        Json::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    env.push((EnvName::new(name.clone())?, value));
                }
            }
            Some(_) => {
                return Err(Error::invalid(
                    "shell.bash's `env` is an object of variable names to values",
                ));
            }
        }

        // A file rather than `bash -c`, so a shebang can choose the
        // interpreter and the script may read stdin without reading itself.
        let file = script_file();
        write_private(&file, &script).await?;
        let interpreter = script
            .strip_prefix("#!")
            .and_then(|rest| rest.lines().next())
            .map(|line| {
                line.split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|words| !words.is_empty());
        let mut command = match &interpreter {
            Some(words) => {
                let mut command = HostCommand::new(
                    Program::configured(words[0].clone()),
                    &self.authority(),
                    Timeout::secs(timeout),
                );
                command.configured_args(&words[1..]);
                command
            }
            None => HostCommand::new(
                Program::named("bash"),
                &self.authority(),
                Timeout::secs(timeout),
            ),
        };
        command.positional(Operand::path(&file));
        for (name, value) in &env {
            command.env(name.clone(), value);
        }
        match plan.get("cwd").and_then(Json::as_str).map(str::trim) {
            Some(dir) if !dir.is_empty() => {
                command.cwd(dir);
            }
            _ => {
                if let Some(home) = std::env::var_os("HOME") {
                    command.cwd(home);
                }
            }
        }
        sc_log::log_info!(
            "feldspar: the unsafe module `{}` is running a script ({} bytes)",
            self.module,
            script.len()
        );
        let output = command.output_bounded().await;
        let _ = tokio::fs::remove_file(&file).await;
        answer(output.map_err(Error::from)?, timeout)
    }

    async fn ssh(&self, plan: &Map<String, Json>) -> Result<Json> {
        let script = script_of(plan, "ssh")?;
        let timeout = timeout_of(plan, "ssh")?;
        let host = ssh_word(plan, "host", true)?.ok_or_else(|| {
            Error::invalid("shell.ssh needs `host`, the machine to run the script on")
        })?;
        let user = ssh_word(plan, "user", false)?;
        let port = match plan.get("port") {
            None | Some(Json::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
                    .filter(|n| (1..=65535).contains(n))
                    .ok_or_else(|| {
                        Error::invalid("shell.ssh's `port` is a port number from 1 to 65535")
                    })?,
            ),
        };
        let target = match &user {
            Some(user) => format!("{user}@{host}"),
            None => host.clone(),
        };

        // The script is ssh's stdin, which the remote `bash -s` reads: a file
        // rather than a pipe so nothing here has to write it while the output
        // is read.
        let file = script_file();
        write_private(&file, &script).await?;
        let stdin = std::fs::File::open(&file)
            .map_err(|e| Error::config(format!("reading the script back: {e}")))?;
        let mut command = HostCommand::new(
            Program::named("ssh"),
            &self.authority(),
            Timeout::secs(timeout),
        );
        command
            .flag_value("-o", "BatchMode=yes")
            .flag_value("-o", format!("ConnectTimeout={timeout}"));
        if let Some(port) = port {
            command.flag_value("-p", port.to_string());
        }
        command
            .operand(Operand::new(target.clone())?)
            .operand(Operand::new("bash -s")?)
            .stdin_file(stdin);
        // The agent holding the server user's keys, when it has one.
        command.inherit(EnvName::new("SSH_AUTH_SOCK")?);
        if let Some(home) = std::env::var_os("HOME") {
            command.cwd(home);
        }
        sc_log::log_info!(
            "feldspar: the unsafe module `{}` is running a script on {target} over SSH ({} bytes)",
            self.module,
            script.len()
        );
        let output = command.output_bounded().await;
        let _ = tokio::fs::remove_file(&file).await;
        answer(output.map_err(Error::from)?, timeout)
    }
}

/// The plan's `script`, with Windows line ends made Unix ones.
fn script_of(plan: &Map<String, Json>, op: &str) -> Result<String> {
    Ok(plan
        .get("script")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid(format!("shell.{op} needs `script`, the script to run")))?
        .replace("\r\n", "\n"))
}

/// The plan's `timeout_seconds`, or [`DEFAULT_TIMEOUT_SECS`].
fn timeout_of(plan: &Map<String, Json>, op: &str) -> Result<u64> {
    match plan.get("timeout_seconds") {
        None | Some(Json::Null) => Ok(DEFAULT_TIMEOUT_SECS),
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=MAX_TIMEOUT_SECS).contains(n))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "shell.{op}'s `timeout_seconds` is a whole number of seconds from 1 to \
                     {MAX_TIMEOUT_SECS}"
                ))
            }),
    }
}

/// A host or user name for ssh: letters, digits and `.`, `_`, `-` (and `:`
/// in a host, for an IPv6 address), not starting with `-` — so a name the
/// model made up can never become one of ssh's options (`-oProxyCommand=…`
/// runs a program of its own). `None` when the plan does not give one.
fn ssh_word(plan: &Map<String, Json>, key: &str, host: bool) -> Result<Option<String>> {
    let word = match plan.get(key) {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Json::String(s)) => s.trim(),
        Some(_) => return Err(Error::invalid(format!("shell.ssh's `{key}` is a string"))),
    };
    let allowed = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c) || (host && c == ':');
    if word.starts_with('-') || !word.chars().all(allowed) {
        return Err(Error::invalid(format!(
            "shell.ssh's `{key}` {word:?} is not a {}: letters, digits, `.`, `_` and `-`, not \
             starting with `-`",
            if host { "host name" } else { "user name" }
        )));
    }
    Ok(Some(word.to_owned()))
}

/// A new, unused path for a script.
fn script_file() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("feldspar-script-{}.sh", uuid::Uuid::new_v4()))
}

/// What a script's run answers: its exit code and output, or that it was
/// stopped.
fn answer(output: Option<std::process::Output>, timeout: u64) -> Result<Json> {
    Ok(match output {
        Some(output) => json!({
            "exitcode": output.status.code().unwrap_or(-1),
            "stdout": capped(&output.stdout),
            "stderr": capped(&output.stderr),
            "timed_out": false,
        }),
        None => json!({
            "exitcode": -1,
            "stdout": "",
            "stderr": format!("the script ran for longer than {timeout} seconds and was stopped"),
            "timed_out": true,
        }),
    })
}

/// Write the script so that only the server's user can read it.
async fn write_private(path: &Path, script: &str) -> Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|e| Error::config(format!("writing the script to {}: {e}", path.display())))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, script.as_bytes())
        .await
        .map_err(|e| Error::config(format!("writing the script to {}: {e}", path.display())))
}

/// Output as text, cut at [`MAX_OUTPUT_BYTES`].
fn capped(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_OUTPUT_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut text = String::from_utf8_lossy(&bytes[..MAX_OUTPUT_BYTES]).into_owned();
    text.push_str(&format!(
        "\n[… {} more bytes not shown]",
        bytes.len() - MAX_OUTPUT_BYTES
    ));
    text
}

#[async_trait::async_trait]
impl Ask for ModuleShell<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        // Again on every call: the switch is the operator's, and a server
        // restarted without it must stop running scripts even if something
        // still holds the action.
        if !(self.allowed)() {
            return Err(Error::config(refusal(
                self.module,
                "it asked to run a script",
            )));
        }
        let Json::Object(plan) = plan else {
            return Err(Error::invalid("a shell ask is an object with an `op`"));
        };
        match plan.get("op").and_then(Json::as_str).unwrap_or_default() {
            "bash" => self.bash(&plan).await,
            "ssh" => self.ssh(&plan).await,
            other => Err(Error::invalid(format!(
                "`{other}` is not a shell operation; there are `bash` and `ssh`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_declares_itself_unsafe_with_a_sentence_or_true() {
        assert_eq!(
            unsafe_reason(&json!("runs bash")).as_deref(),
            Some("runs bash")
        );
        assert!(unsafe_reason(&json!(true)).is_some());
        for not in [json!(false), json!(""), json!(null), json!(1)] {
            assert_eq!(unsafe_reason(&not), None, "{not}");
        }
    }

    #[test]
    fn the_declaration_is_read_from_package_json() {
        let dir = std::env::temp_dir().join(format!("sc-unsafe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{ "name": "safe" }"#).unwrap();
        assert_eq!(declared_unsafe(&dir), None);
        std::fs::write(
            dir.join("package.json"),
            r#"{ "name": "x", "feldspar": { "unsafe": "runs bash" } }"#,
        )
        .unwrap();
        assert_eq!(declared_unsafe(&dir).as_deref(), Some("runs bash"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(declared_unsafe(&dir), None);
    }

    #[test]
    fn the_refusal_names_the_variable() {
        let text = refusal("@feldspar/bash", "runs bash");
        assert!(text.contains(ALLOW_UNSAFE_MODULES), "{text}");
        assert!(text.contains("@feldspar/bash"), "{text}");
        // A module that is not unsafe is never refused, switch or not.
        assert!(refuse_unless_allowed("safe", None).is_ok());
    }

    fn shell(allowed: fn() -> bool) -> ModuleShell<'static> {
        ModuleShell::with_switch("@feldspar/bash", allowed)
    }

    #[tokio::test]
    async fn a_script_runs_with_its_environment_and_answers_its_output() {
        let answer = shell(|| true)
            .ask(json!({
                "op": "bash",
                "script": "echo \"hello $ROW_NAME\"\necho oops >&2\nexit 3\n",
                "env": { "ROW_NAME": "Ada", "ROW_SKIPPED": null },
                "cwd": "/",
            }))
            .await
            .unwrap();
        assert_eq!(answer["stdout"], json!("hello Ada\n"));
        assert_eq!(answer["stderr"], json!("oops\n"));
        assert_eq!(answer["exitcode"], json!(3));
        assert_eq!(answer["timed_out"], json!(false));
    }

    #[tokio::test]
    async fn the_server_environment_does_not_reach_the_script() {
        // DATABASE_URL-like secrets of the server's are not passed on: only
        // the base set and what the module named.
        let answer = shell(|| true)
            .ask(json!({ "op": "bash", "script": "echo \"[${CARGO_PKG_NAME:-}]\"; pwd" , "cwd": "/tmp" }))
            .await
            .unwrap();
        assert_eq!(answer["stdout"], json!("[]\n/tmp\n"));
    }

    #[tokio::test]
    async fn a_shebang_chooses_the_interpreter() {
        let answer = shell(|| true)
            .ask(json!({ "op": "bash", "script": "#!/bin/sh\necho $0 | grep -c feldspar-script" }))
            .await
            .unwrap();
        assert_eq!(answer["stdout"], json!("1\n"), "{answer}");
    }

    #[tokio::test]
    async fn a_script_past_its_timeout_is_stopped() {
        let answer = shell(|| true)
            .ask(json!({ "op": "bash", "script": "sleep 30", "timeout_seconds": 1 }))
            .await
            .unwrap();
        assert_eq!(answer["timed_out"], json!(true));
    }

    #[tokio::test]
    async fn without_the_switch_nothing_runs() {
        let err = shell(|| false)
            .ask(json!({ "op": "bash", "script": "echo hi" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains(ALLOW_UNSAFE_MODULES), "{err}");
    }

    #[tokio::test]
    async fn a_variable_that_changes_how_programs_load_is_refused() {
        let err = shell(|| true)
            .ask(json!({ "op": "bash", "script": "true", "env": { "LD_PRELOAD": "/x.so" } }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("LD_PRELOAD"), "{err}");
    }

    #[tokio::test]
    async fn an_ssh_target_that_could_be_an_option_is_refused_before_anything_runs() {
        for (key, value) in [
            ("host", "-oProxyCommand=touch /tmp/x"),
            ("host", "db1 -p 1"),
            ("user", "-l"),
            ("user", "root@evil"),
        ] {
            let mut plan = json!({ "op": "ssh", "script": "true", "host": "db1" });
            plan[key] = json!(value);
            let err = shell(|| true).ask(plan).await.unwrap_err();
            assert!(err.to_string().contains(key), "{key}={value}: {err}");
        }
        let err = shell(|| true)
            .ask(json!({ "op": "ssh", "script": "true", "host": "db1", "port": 70000 }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
        let err = shell(|| true)
            .ask(json!({ "op": "ssh", "script": "true" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("host"), "{err}");
    }

    #[test]
    fn ssh_words_allow_host_names_and_addresses() {
        let plan = |v: &str| json!({ "host": v, "user": v }).as_object().unwrap().clone();
        for ok in ["db1", "db1.example.com", "10.0.0.7", "my_host-2"] {
            assert_eq!(
                ssh_word(&plan(ok), "host", true).unwrap().as_deref(),
                Some(ok)
            );
        }
        assert!(ssh_word(&plan("::1"), "host", true).is_ok());
        assert!(ssh_word(&plan("::1"), "user", false).is_err());
        assert_eq!(ssh_word(&plan(" "), "host", true).unwrap(), None);
    }

    #[tokio::test]
    async fn an_unreachable_ssh_host_is_an_answer_not_an_error() {
        // `ssh` may not be installed where the tests run; then there is
        // nothing to show.
        if which("ssh").is_none() {
            return;
        }
        let answer = shell(|| true)
            .ask(json!({
                "op": "ssh", "script": "echo hi", "host": "feldspar-no-such-host.invalid",
                "timeout_seconds": 5,
            }))
            .await
            .unwrap();
        assert_eq!(answer["exitcode"], json!(255), "{answer}");
        assert_eq!(answer["stdout"], json!(""), "{answer}");
    }

    fn which(program: &str) -> Option<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(program))
            .find(|path| path.is_file())
    }

    #[test]
    fn long_output_is_cut_and_says_so() {
        let text = capped(&vec![b'a'; MAX_OUTPUT_BYTES + 10]);
        assert!(
            text.ends_with("[… 10 more bytes not shown]"),
            "{}",
            &text[text.len() - 40..]
        );
        assert_eq!(capped(b"hi"), "hi");
    }
}
