//! `feldspar build-agent`: run this machine as a build host
//! (`TODO-build-hosts.md`, design §13.3 "Build hosts").
//!
//! The agent asks the server for work and the server never calls it, so the
//! machine (a Mac behind NAT, say) needs only outbound HTTPS. It needs no
//! database either: **a build host never generates code, it only builds.**
//!
//! - **The heartbeat**: every interval the agent tells the server who it is and
//!   whether this machine has what each of its targets requires. The server
//!   answers with the targets and their requirements, which the agent checks
//!   here and reports at the next heartbeat. The first two go one after the
//!   other, so the admin sees the readiness at once. A heartbeat also keeps
//!   the jobs the agent is building held for it.
//! - **Jobs**: meanwhile the agent asks for a job, long-polling. It unpacks the
//!   project into a workspace it keeps per application, builds the target with
//!   the code a local build runs ([`sc_app::run_target`]), sends the log while
//!   the build runs, and ends with the artifact or the reason it failed.
//!
//! **The token never goes on the command line**, where `ps` and the shell's
//! history would show it: it is read from `--token-file` or from
//! [`TOKEN_ENV`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use sc_app::{AgentReport, AnsweredTarget, HEARTBEAT_SECONDS, TargetSpec, answered_targets};
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Where the token is read from when there is no `--token-file`.
pub const TOKEN_ENV: &str = sc_app::BUILD_TOKEN_ENV;

/// What `build-agent` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentArgs {
    /// `--server`: the server's address, `https://apps.example.com`.
    pub server: String,
    /// `--token-file`: a file holding the build host's token.
    pub token_file: Option<PathBuf>,
    /// `--once`: one heartbeat and its report, then exit — to check a setup.
    pub once: bool,
    /// `--workspace`: where the projects are unpacked and built, one directory
    /// per application. By default `build-host` in Feldspar's data directory.
    pub workspace: Option<PathBuf>,
    /// `--insecure-http`: allow an `http://` server that is not this machine.
    /// The token, the build secrets and the signing keys then cross the
    /// network unencrypted, so it is for testing on a trusted network only.
    pub insecure_http: bool,
}

/// Read `build-agent`'s arguments.
pub fn parse_args(args: &[String]) -> Result<AgentArgs> {
    let mut server = None;
    let mut token_file = None;
    let mut once = false;
    let mut workspace = None;
    let mut insecure_http = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| Error::config(format!("{name} needs a value")))
        };
        match arg.as_str() {
            "--server" => server = Some(value("--server")?),
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--once" => once = true,
            "--workspace" => workspace = Some(PathBuf::from(value("--workspace")?)),
            "--insecure-http" => insecure_http = true,
            "--token" => {
                return Err(Error::config(format!(
                    "the token is not taken on the command line, where `ps` and the shell's \
                     history would show it: put it in a file and pass --token-file, or set \
                     {TOKEN_ENV}"
                )));
            }
            other => {
                return Err(Error::config(format!(
                    "unknown build-agent argument `{other}`; there are --server, \
                     --token-file, --workspace, --insecure-http and --once"
                )));
            }
        }
    }
    let server = server
        .ok_or_else(|| Error::config("build-agent needs --server, the server's address"))?
        .trim()
        .trim_end_matches('/')
        .to_owned();
    if !(server.starts_with("https://") || server.starts_with("http://")) {
        return Err(Error::config(format!(
            "--server `{server}` is not an address: it starts with https://"
        )));
    }
    // The token and the build secrets would cross the network unencrypted.
    if server.starts_with("http://") && !insecure_http && !is_this_machine(&server) {
        return Err(Error::config(format!(
            "--server `{server}` is plain http: the token, the build secrets and the signing \
             keys would cross the network unencrypted. Use https://, or pass --insecure-http \
             to test on a network you trust"
        )));
    }
    Ok(AgentArgs {
        server,
        token_file,
        once,
        workspace,
        insecure_http,
    })
}

/// Whether `server` is this machine (`localhost`, `127.0.0.1`, `[::1]`), where
/// plain http never leaves it.
fn is_this_machine(server: &str) -> bool {
    let Some(host) = reqwest::Url::parse(server)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The build host's token: from `--token-file`, else from [`TOKEN_ENV`]
/// (`env` is its value, passed in so this is testable).
pub fn read_token(args: &AgentArgs, env: Option<String>) -> Result<String> {
    let token = match &args.token_file {
        Some(path) => std::fs::read_to_string(path).map_err(|e| {
            Error::config(format!("reading the token from {}: {e}", path.display()))
        })?,
        None => env.unwrap_or_default(),
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(Error::config(format!(
            "build-agent needs the build host's token, shown when the host was added under \
             Settings → Development → Build hosts: in a file named by --token-file, or in {TOKEN_ENV}"
        )));
    }
    Ok(token)
}

/// How a heartbeat went wrong: worth retrying (the server is unreachable or
/// failed), or not (the token or the agent's version was refused).
#[derive(Debug)]
pub enum HeartbeatError {
    /// Try again at the next interval.
    Retry(Error),
    /// Stop: retrying cannot help until someone changes something.
    Fatal(Error),
}

/// What the server answered a heartbeat with.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The host's name on the server.
    pub name: String,
    /// When to send the next heartbeat.
    pub interval: Duration,
    /// The targets the host builds, with what each requires.
    pub targets: Vec<AnsweredTarget>,
}

/// One build agent's conversation with its server.
pub struct Agent {
    client: reqwest::Client,
    url: String,
    token: String,
    targets: Vec<AnsweredTarget>,
}

impl Agent {
    /// An agent for `server` with `token`, which has not heard its targets yet.
    pub fn new(server: &str, token: String) -> Result<Agent> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::msg(format!("making the HTTP client: {e}")))?;
        Ok(Agent {
            client,
            url: format!("{server}{}", sc_server::BUILD_AGENT_HEARTBEAT_ROUTE),
            token,
            targets: Vec::new(),
        })
    }

    /// Send one heartbeat: this machine's report against the targets heard so
    /// far. The answer's targets are kept for the next one.
    pub async fn heartbeat(&mut self) -> std::result::Result<Answer, HeartbeatError> {
        let report = AgentReport::of_this_machine(&self.targets);
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&report.to_json())
            .send()
            .await
            .map_err(|e| HeartbeatError::Retry(Error::msg(format!("{}: {e}", self.url))))?;
        let status = response.status();
        let body: Json = response.json().await.unwrap_or(Json::Null);
        let said = || {
            body.get("error")
                .and_then(Json::as_str)
                .map_or_else(|| format!("the server answered {status}"), str::to_owned)
        };
        match status {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::CONFLICT | StatusCode::NOT_FOUND => {
                return Err(HeartbeatError::Fatal(Error::config(said())));
            }
            _ => return Err(HeartbeatError::Retry(Error::msg(said()))),
        }
        let targets = answered_targets(&body).map_err(HeartbeatError::Fatal)?;
        self.targets = targets.clone();
        let seconds = body
            .get("interval_seconds")
            .and_then(Json::as_i64)
            .unwrap_or(HEARTBEAT_SECONDS)
            .max(1);
        Ok(Answer {
            name: body
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
            interval: Duration::from_secs(seconds.unsigned_abs()),
            targets,
        })
    }

    /// What this machine reports for its targets now, one line per target and
    /// one per thing missing: what the operator reads in the terminal.
    pub fn readiness_lines(&self) -> Vec<String> {
        let report = AgentReport::of_this_machine(&self.targets);
        let mut lines = Vec::new();
        for (target, (_, missing)) in self.targets.iter().zip(&report.readiness) {
            if missing.is_empty() {
                lines.push(format!("  {} ({}): ready", target.name, target.label));
            } else {
                lines.push(format!("  {} ({}): not ready", target.name, target.label));
                lines.extend(missing.iter().map(|m| format!("    - {m}")));
            }
        }
        lines
    }
}

/// `feldspar build-agent`: heartbeats and jobs until stopped, or one heartbeat
/// with `--once`.
///
/// A refused token or version stops the agent with the server's reason. An
/// unreachable server is retried at every interval, and said once, not at
/// every attempt.
pub async fn run(args: &AgentArgs) -> Result<()> {
    let token = read_token(args, std::env::var(TOKEN_ENV).ok())?;
    if args.server.starts_with("http://") && !is_this_machine(&args.server) {
        eprintln!(
            "feldspar build-agent: warning: {} is plain http, so the token, the build secrets \
             and the signing keys cross the network unencrypted",
            args.server
        );
    }
    let mut agent = Agent::new(&args.server, token.clone())?;
    // The first heartbeat hears the targets; the second reports on them.
    let fatal = |e: HeartbeatError| match e {
        HeartbeatError::Retry(e) | HeartbeatError::Fatal(e) => e,
    };
    agent.heartbeat().await.map_err(fatal)?;
    let answer = agent.heartbeat().await.map_err(fatal)?;
    eprintln!(
        "feldspar build-agent: connected to {} as `{}`",
        args.server, answer.name
    );
    let shown = agent.readiness_lines();
    for line in &shown {
        eprintln!("{line}");
    }
    if args.once {
        return Ok(());
    }
    let workspace = match &args.workspace {
        Some(dir) => dir.clone(),
        None => sc_files::data_dir()?.join("build-host"),
    };
    let worker = Worker::new(&args.server, token, workspace)?;
    let mut heartbeats = tokio::spawn(heartbeats(agent, answer.interval, shown));
    tokio::select! {
        ended = &mut heartbeats => match ended {
            Ok(result) => result,
            Err(e) => Err(Error::msg(format!("the heartbeat stopped: {e}"))),
        },
        ended = work(&worker, answer.interval) => {
            heartbeats.abort();
            ended
        }
    }
}

/// Send a heartbeat every interval, saying when the readiness changes and when
/// the server stops or starts answering. Ends only on a refusal.
async fn heartbeats(
    mut agent: Agent,
    mut interval: Duration,
    mut shown: Vec<String>,
) -> Result<()> {
    let mut unreachable = false;
    loop {
        tokio::time::sleep(interval).await;
        match agent.heartbeat().await {
            Ok(answer) => {
                if unreachable {
                    eprintln!("feldspar build-agent: the server answers again");
                    unreachable = false;
                }
                interval = answer.interval;
                let lines = agent.readiness_lines();
                if lines != shown {
                    for line in &lines {
                        eprintln!("{line}");
                    }
                    shown = lines;
                }
            }
            Err(HeartbeatError::Retry(e)) => {
                if !unreachable {
                    eprintln!(
                        "feldspar build-agent: {e}; retrying every {}s",
                        interval.as_secs()
                    );
                    unreachable = true;
                }
            }
            Err(HeartbeatError::Fatal(e)) => return Err(e),
        }
    }
}

/// Ask for jobs and build them, one at a time, until the server refuses this
/// agent. While the server is unreachable it asks again every `retry`.
async fn work(worker: &Worker, retry: Duration) -> Result<()> {
    loop {
        match worker.claim(CLAIM_WAIT_SECONDS).await {
            Ok(Some(job)) => {
                eprintln!(
                    "feldspar build-agent: building the {} of `{}`",
                    job.spec.label, job.application
                );
                match worker.build(&job).await {
                    Ok(Built::Artifact(size)) => eprintln!(
                        "feldspar build-agent: the {} is built ({size} bytes) and sent",
                        job.spec.label
                    ),
                    Ok(Built::Failed(why)) => eprintln!(
                        "feldspar build-agent: the {} failed: {}",
                        job.spec.label,
                        why.lines().next().unwrap_or_default()
                    ),
                    Err(HeartbeatError::Fatal(e)) => return Err(e),
                    Err(HeartbeatError::Retry(e)) => {
                        eprintln!("feldspar build-agent: the job was lost: {e}");
                    }
                }
            }
            Ok(None) => {}
            Err(HeartbeatError::Fatal(e)) => return Err(e),
            // The heartbeat says the server is unreachable; this waits.
            Err(HeartbeatError::Retry(_)) => tokio::time::sleep(retry).await,
        }
    }
}

/// How long one claim waits on the server for a job.
const CLAIM_WAIT_SECONDS: u64 = 20;

/// How often the log is sent while a build runs.
const LOG_EVERY: Duration = Duration::from_secs(2);

/// The most log one request carries: well under the server's default request
/// limit (2 MiB), however much an install step printed at once.
const LOG_CHUNK_BYTES: usize = 1024 * 1024;

/// A job the server gave this agent.
#[derive(Debug, Clone)]
pub struct Job {
    /// The job's id on the server.
    pub id: String,
    /// The application's id, which names its workspace, and its name.
    pub application_id: String,
    pub application: String,
    /// The target, rendered for the application.
    pub spec: TargetSpec,
    /// Where the log goes, relative to the workspace (and the server's store).
    pub log_path: String,
}

/// How a build that was reported ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Built {
    /// The artifact was sent: its size.
    Artifact(u64),
    /// The build failed, and the server was told why.
    Failed(String),
}

/// The agent's side of jobs: claiming them, building them, reporting them.
pub struct Worker {
    client: reqwest::Client,
    server: String,
    token: String,
    workspace: PathBuf,
}

impl Worker {
    /// A worker for `server` with `token`, building under `workspace`.
    pub fn new(server: &str, token: String, workspace: PathBuf) -> Result<Worker> {
        // No overall timeout: a claim waits, and an artifact takes as long as
        // it takes. Each request sets its own.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::msg(format!("making the HTTP client: {e}")))?;
        Ok(Worker {
            client,
            server: server.to_owned(),
            token,
            workspace,
        })
    }

    /// Ask for a job, waiting up to `wait` seconds for one.
    pub async fn claim(&self, wait: u64) -> std::result::Result<Option<Job>, HeartbeatError> {
        let url = format!(
            "{}{}?wait={wait}",
            self.server,
            sc_server::BUILD_AGENT_CLAIM_ROUTE
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(wait + 30))
            .send()
            .await
            .map_err(|e| HeartbeatError::Retry(Error::msg(format!("{url}: {e}"))))?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let body = answered(response).await?;
        let text = |key: &str| body.get(key).and_then(Json::as_str).map(str::to_owned);
        let bad = || HeartbeatError::Fatal(Error::invalid("the server's job is malformed"));
        let application = body.get("application").ok_or_else(bad)?;
        Ok(Some(Job {
            id: text("job").ok_or_else(bad)?,
            application_id: application
                .get("id")
                .and_then(Json::as_str)
                .ok_or_else(bad)?
                .to_owned(),
            application: application
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
            spec: sc_app::target_spec_from_json(body.get("target").ok_or_else(bad)?)
                .map_err(HeartbeatError::Fatal)?,
            log_path: text("log_path").ok_or_else(bad)?,
        }))
    }

    /// Build `job`: unpack its project, run the target with its log sent as it
    /// grows, and send the artifact or the reason it failed. An error is a
    /// conversation with the server that broke, not a build that failed.
    ///
    /// Signing keys are deleted from the workspace afterwards, however it went:
    /// the build host keeps the project for the next build, not the keys.
    pub async fn build(&self, job: &Job) -> std::result::Result<Built, HeartbeatError> {
        let root = self.workspace.join(&job.application_id);
        let built = self.build_in(job, &root).await;
        let project = root.join(&job.spec.source_dir);
        match tokio::task::spawn_blocking(move || sc_server::remove_signing_keys(&project)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("feldspar build-agent: {e}"),
            Err(e) => eprintln!("feldspar build-agent: deleting the signing keys stopped: {e}"),
        }
        built
    }

    async fn build_in(&self, job: &Job, root: &Path) -> std::result::Result<Built, HeartbeatError> {
        let outcome = match self.unpack(job, root).await? {
            Ok(()) => self.run(job, root).await?,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(artifact) => {
                let size = self.send_artifact(job, &root.join(&artifact)).await?;
                Ok(Built::Artifact(size))
            }
            Err(e) => {
                let why = sc_error::format_chain(&e);
                self.send(
                    job,
                    "failed",
                    reqwest::Body::from(json!({ "error": why }).to_string()),
                )
                .await?;
                Ok(Built::Failed(why))
            }
        }
    }

    /// Fetch the project and unpack it into `root`. The outer error is the
    /// server; the inner one, a project that cannot be unpacked here.
    async fn unpack(
        &self,
        job: &Job,
        root: &Path,
    ) -> std::result::Result<Result<()>, HeartbeatError> {
        let response = self
            .client
            .get(self.job_url(job, "project"))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| HeartbeatError::Retry(Error::msg(e.to_string())))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        let archive = response
            .bytes()
            .await
            .map_err(|e| HeartbeatError::Retry(Error::msg(e.to_string())))?;
        let root = root.to_owned();
        Ok(tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&root)
                .map_err(|e| Error::msg(format!("creating {}: {e}", root.display())))?;
            sc_server::unpack_project(&archive, &root)
        })
        .await
        .unwrap_or_else(|e| Err(Error::msg(format!("unpacking stopped: {e}")))))
    }

    /// Run the target in `root`, sending the log every [`LOG_EVERY`] and the
    /// rest at the end. Answers the artifact's path, relative to `root`.
    async fn run(
        &self,
        job: &Job,
        root: &Path,
    ) -> std::result::Result<Result<String>, HeartbeatError> {
        let log_file = root.join(&job.log_path);
        let mut sent = 0u64;
        let build = sc_app::run_target(&job.spec, root, &job.log_path);
        tokio::pin!(build);
        let outcome = loop {
            tokio::select! {
                outcome = &mut build => break outcome,
                () = tokio::time::sleep(LOG_EVERY) => {
                    self.send_log(job, &log_file, &mut sent).await?;
                }
            }
        };
        self.send_log(job, &log_file, &mut sent).await?;
        Ok(outcome.map(|report| report.artifact))
    }

    /// Send what the log has gained since `sent` bytes.
    async fn send_log(
        &self,
        job: &Job,
        log_file: &Path,
        sent: &mut u64,
    ) -> std::result::Result<(), HeartbeatError> {
        let Ok(mut file) = tokio::fs::File::open(log_file).await else {
            return Ok(());
        };
        if file.seek(std::io::SeekFrom::Start(*sent)).await.is_err() {
            return Ok(());
        }
        let mut more = Vec::new();
        if file.read_to_end(&mut more).await.is_err() || more.is_empty() {
            return Ok(());
        }
        for chunk in more.chunks(LOG_CHUNK_BYTES) {
            self.send(job, "log", reqwest::Body::from(chunk.to_vec()))
                .await?;
            *sent += chunk.len() as u64;
        }
        Ok(())
    }

    /// Send the artifact, streamed from its file. Answers its size.
    async fn send_artifact(
        &self,
        job: &Job,
        path: &Path,
    ) -> std::result::Result<u64, HeartbeatError> {
        let file = tokio::fs::File::open(path).await.map_err(|e| {
            HeartbeatError::Retry(Error::msg(format!("opening {}: {e}", path.display())))
        })?;
        let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        self.send(job, "artifact", reqwest::Body::from(file))
            .await?;
        Ok(size)
    }

    async fn send(
        &self,
        job: &Job,
        what: &str,
        body: reqwest::Body,
    ) -> std::result::Result<(), HeartbeatError> {
        let response = self
            .client
            .post(self.job_url(job, what))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(if what == "artifact" {
                3600
            } else {
                60
            }))
            .body(body)
            .send()
            .await
            .map_err(|e| HeartbeatError::Retry(Error::msg(e.to_string())))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(refused(response).await)
        }
    }

    fn job_url(&self, job: &Job, what: &str) -> String {
        format!(
            "{}{}/{}/{what}",
            self.server,
            sc_server::BUILD_AGENT_JOBS_ROUTE,
            job.id
        )
    }
}

/// The body of a successful answer, or what the server said instead.
async fn answered(response: reqwest::Response) -> std::result::Result<Json, HeartbeatError> {
    if !response.status().is_success() {
        return Err(refused(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| HeartbeatError::Retry(Error::msg(format!("reading the answer: {e}"))))
}

/// An answer that is not a success, as the error it means: a refused token
/// stops the agent; anything else (a job taken away, a server failing) is the
/// end of that conversation only.
async fn refused(response: reqwest::Response) -> HeartbeatError {
    let status = response.status();
    let body: Json = response.json().await.unwrap_or(Json::Null);
    let said = body
        .get("error")
        .and_then(Json::as_str)
        .map_or_else(|| format!("the server answered {status}"), str::to_owned);
    match status {
        StatusCode::UNAUTHORIZED => HeartbeatError::Fatal(Error::config(said)),
        _ => HeartbeatError::Retry(Error::msg(said)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_server_is_required_and_the_token_is_never_an_argument() {
        let parsed =
            parse_args(&args(&["--server", "https://apps.example.com/", "--once"])).unwrap();
        assert_eq!(parsed.server, "https://apps.example.com");
        assert!(parsed.once);
        assert!(parse_args(&args(&["--once"])).is_err());
        assert!(parse_args(&args(&["--server", "apps.example.com"])).is_err());
        let why = parse_args(&args(&["--server", "https://a", "--token", "fsbh_x"]))
            .unwrap_err()
            .to_string();
        assert!(
            why.contains("--token-file") && why.contains(TOKEN_ENV),
            "{why}"
        );
    }

    #[test]
    fn plain_http_is_refused_unless_it_is_this_machine_or_asked_for() {
        for local in [
            "http://localhost:3000",
            "http://127.0.0.1:3000/",
            "http://[::1]:3000",
        ] {
            assert!(parse_args(&args(&["--server", local])).is_ok(), "{local}");
        }
        let why = parse_args(&args(&["--server", "http://192.168.1.20:3000"]))
            .unwrap_err()
            .to_string();
        assert!(why.contains("--insecure-http"), "{why}");
        let parsed = parse_args(&args(&[
            "--server",
            "http://192.168.1.20:3000",
            "--insecure-http",
        ]))
        .unwrap();
        assert!(parsed.insecure_http);
        assert!(parse_args(&args(&["--server", "https://apps.example.com"])).is_ok());
    }

    #[test]
    fn the_token_comes_from_its_file_or_else_the_environment() {
        let dir = std::env::temp_dir().join(format!("build-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "fsbh_from_file\n").unwrap();
        let mut parsed = parse_args(&args(&["--server", "https://a"])).unwrap();
        assert_eq!(
            read_token(&parsed, Some("fsbh_env".into())).unwrap(),
            "fsbh_env"
        );
        assert!(read_token(&parsed, None).is_err());
        parsed.token_file = Some(file);
        assert_eq!(
            read_token(&parsed, Some("fsbh_env".into())).unwrap(),
            "fsbh_from_file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
