//! The routes a build host's agent talks to (`TODO-build-hosts.md`, design
//! §13.3 "Build hosts"): `feldspar build-agent` on a Mac asks this server for
//! work, and the server never calls it.
//!
//! They authenticate by the build host's bearer token and by **nothing else**:
//! a session cookie is ignored, and an API token is refused, as a build host
//! token is everywhere else. A bearer request is exempt from the CSRF check
//! (`security::is_bearer_authenticated`), which is safe for the reason it is
//! on `/mcp`.
//!
//! - **The heartbeat**: the agent says who it is and whether its machine has
//!   what its targets require; the server keeps that, so the admin sees the
//!   host online, and answers with the targets and their requirements. It also
//!   renews the lease of every job the host is running.
//! - **Claiming a job**: a long poll that answers the oldest queued job for one
//!   of the host's targets, with the target rendered for the application.
//! - **A claimed job**: the agent fetches the project, sends the log as the
//!   build runs, and ends with the artifact or the reason it failed. These
//!   answer for the host's own running jobs only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use sc_app::{
    AgentReport, AppId, BuildHost, BuildJob, JobOutcome, authenticate_build_host, check_protocol,
    claim_build_job, finish_build_job, heartbeat_answer, held_build_job, load_application,
    record_heartbeat, renew_leases, target_spec_json,
};
use sc_catalog::Catalog;
use serde_json::{Value as Json, json};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::router::AppState;

/// The heartbeat: `POST` with an [`AgentReport`], answered with
/// [`heartbeat_answer`].
pub const BUILD_AGENT_HEARTBEAT_ROUTE: &str = "/build-agent/heartbeat";

/// Claiming a job: `POST`, answered with a job or `204 No Content` when none
/// came within `?wait=` seconds.
pub const BUILD_AGENT_CLAIM_ROUTE: &str = "/build-agent/claim";

/// A claimed job's routes, under `/build-agent/jobs/{id}/`: `project` (`GET`, the
/// zip), `log` (`POST`, a piece of the log), `artifact` (`POST`, the file) and
/// `failed` (`POST`, `{ error }`).
pub const BUILD_AGENT_JOBS_ROUTE: &str = "/build-agent/jobs";

/// Ceiling on a heartbeat's body: a report is a few hundred bytes.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Ceiling on an artifact: an IPA is a few hundred megabytes at most.
const MAX_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The longest a claim waits for a job, and how often it looks meanwhile.
const MAX_CLAIM_WAIT_SECONDS: u64 = 25;
const CLAIM_POLL: Duration = Duration::from_secs(1);

/// How much of the log a finished job keeps, as a local build's report does.
const LOG_TAIL_BYTES: u64 = 32 * 1024;

pub(crate) async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (catalog, host) = match authenticated(&state, &headers).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    if body.len() > MAX_BODY_BYTES {
        return refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "a heartbeat is at most 64 KiB",
        );
    }
    let report = match serde_json::from_slice::<Json>(&body)
        .map_err(|_| sc_error::Error::invalid("a heartbeat's body is JSON"))
        .and_then(|value| AgentReport::from_json(&value))
    {
        Ok(report) => report,
        Err(e) => return refusal(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    // An agent on another protocol is turned away, and the reason is kept in
    // its report, so the admin sees why the host is idle.
    let (status, stored, answer) = match check_protocol(&report) {
        Ok(()) => (StatusCode::OK, report.to_json(), heartbeat_answer(&host)),
        Err(e) => {
            let why = e.to_string();
            let mut stored = report.to_json();
            stored["refused"] = Json::String(why.clone());
            (StatusCode::CONFLICT, stored, json!({ "error": why }))
        }
    };
    let kept = record_heartbeat(&catalog, host.id, &stored).await;
    let renewed = match status {
        StatusCode::OK => renew_leases(&catalog, host.id).await,
        _ => Ok(()),
    };
    if let Err(e) = kept.and(renewed) {
        sc_log::log_error!(
            "build agent `{}`: keeping its heartbeat failed: {e}",
            host.name
        );
        return refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the heartbeat could not be kept",
        );
    }
    (status, axum::Json(answer)).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct ClaimQuery {
    /// How long to wait for a job, in seconds; at most [`MAX_CLAIM_WAIT_SECONDS`].
    wait: Option<u64>,
}

pub(crate) async fn claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ClaimQuery>,
) -> Response {
    let (catalog, host) = match authenticated(&state, &headers).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(MAX_CLAIM_WAIT_SECONDS));
    let until = tokio::time::Instant::now() + wait;
    loop {
        match claim_ready(&catalog, &host).await {
            Ok(Some(answer)) => return (StatusCode::OK, axum::Json(answer)).into_response(),
            Ok(None) => {}
            Err(e) => {
                sc_log::log_error!("build agent `{}`: claiming a job failed: {e}", host.name);
                return refusal(StatusCode::INTERNAL_SERVER_ERROR, "no job could be claimed");
            }
        }
        if tokio::time::Instant::now() + CLAIM_POLL > until {
            return StatusCode::NO_CONTENT.into_response();
        }
        tokio::time::sleep(CLAIM_POLL).await;
    }
}

/// Claim a job for `host` and answer it as the agent hears it. A job whose
/// application cannot be built any more (deleted, its target gone) fails here
/// with the reason, and the next one is tried.
async fn claim_ready(catalog: &Arc<Catalog>, host: &BuildHost) -> sc_error::Result<Option<Json>> {
    while let Some(job) = claim_build_job(catalog, host).await? {
        match job_answer(catalog, &job).await {
            Ok(answer) => return Ok(Some(answer)),
            Err(e) => {
                finish_build_job(
                    catalog,
                    job.id,
                    JobOutcome::Failed(sc_error::format_chain(&e)),
                )
                .await?;
            }
        }
    }
    Ok(None)
}

/// A claimed job as the agent hears it: the target rendered for the
/// application, secrets included, and where the log goes. The log file is
/// started here, so the admin can open it as soon as the build has a host.
async fn job_answer(catalog: &Catalog, job: &BuildJob) -> sc_error::Result<Json> {
    let app = load_application(catalog, AppId(job.application))
        .await?
        .ok_or_else(|| sc_error::Error::not_found("the application was deleted"))?;
    let spec = sc_app::app_target_spec(&app, &job.target)?;
    let log = store_file(catalog, &app, &job.log_path)?;
    if let Some(dir) = log.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let host = job.host_name.as_deref().unwrap_or_default();
    tokio::fs::write(&log, format!("Building on the build host `{host}`.\n")).await?;
    Ok(json!({
        "job": job.id,
        "application": { "id": job.application, "name": app.name, "subdomain": app.subdomain },
        "target": target_spec_json(&spec),
        "log_path": job.log_path,
    }))
}

pub(crate) async fn project(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let (catalog, job) = match held_job(&state, &headers, &id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let packed = async {
        let app = job_application(&catalog, &job).await?;
        let spec = sc_app::app_target_spec(&app, &job.target)?;
        let root = sc_app::app_store_root(&catalog, &app)?;
        let target = job.target.clone();
        tokio::task::spawn_blocking(move || {
            crate::build_archive::pack_project(&root, &spec.source_dir, &target)
        })
        .await
        .map_err(|e| sc_error::Error::msg(format!("packing the project stopped: {e}")))?
    };
    match packed.await {
        Ok(zip) => ([(header::CONTENT_TYPE, "application/zip")], zip).into_response(),
        Err(e) => {
            let why = sc_error::format_chain(&e);
            let _ = finish_build_job(&catalog, job.id, JobOutcome::Failed(why.clone())).await;
            refusal(StatusCode::CONFLICT, &why)
        }
    }
}

pub(crate) async fn log(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let (catalog, job) = match held_job(&state, &headers, &id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let appended = async {
        let app = job_application(&catalog, &job).await?;
        append(&store_file(&catalog, &app, &job.log_path)?, &body).await?;
        if let Some(host) = job.host {
            renew_leases(&catalog, host).await?;
        }
        Ok::<(), sc_error::Error>(())
    };
    match appended.await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refusal(StatusCode::CONFLICT, &sc_error::format_chain(&e)),
    }
}

pub(crate) async fn artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> Response {
    let (catalog, job) = match held_job(&state, &headers, &id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let stored = async {
        let app = job_application(&catalog, &job).await?;
        let spec = sc_app::app_target_spec(&app, &job.target)?;
        let path = store_file(&catalog, &app, &spec.artifact)?;
        let (partial, size) = receive(body, &path).await?;
        // A long upload can outlast the lease. A job that ended meanwhile
        // keeps the end it has, and the store keeps the artifact it had.
        if held_job(&state, &headers, &id).await.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(sc_error::Error::invalid(
                "the job ended while its artifact arrived (its lease ran out), so the \
                 artifact was not stored",
            ));
        }
        if let Err(e) = tokio::fs::rename(&partial, &path).await {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(e.into());
        }
        let log = store_file(&catalog, &app, &job.log_path)?;
        let outcome = JobOutcome::Succeeded {
            artifact: spec.artifact.clone(),
            size: i64::try_from(size).unwrap_or(i64::MAX),
            log: file_tail(&log, LOG_TAIL_BYTES).await,
        };
        finish_build_job(&catalog, job.id, outcome).await
    };
    match stored.await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        // The build is done and only the upload failed, so the job ends here
        // rather than waiting out its lease.
        Err(e) => {
            let why = format!(
                "the artifact could not be stored: {}",
                sc_error::format_chain(&e)
            );
            let _ = finish_build_job(&catalog, job.id, JobOutcome::Failed(why.clone())).await;
            refusal(StatusCode::CONFLICT, &why)
        }
    }
}

pub(crate) async fn failed(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let (catalog, job) = match held_job(&state, &headers, &id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let error = serde_json::from_slice::<Json>(&body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "the build host gave no reason".to_owned());
    // The reason goes at the end of the log too, where the admin reading it looks.
    if let Ok(app) = job_application(&catalog, &job).await
        && let Ok(log) = store_file(&catalog, &app, &job.log_path)
    {
        let _ = append(&log, format!("\n{error}\n").as_bytes()).await;
    }
    match finish_build_job(&catalog, job.id, JobOutcome::Failed(error)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refusal(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The build host the request's bearer token belongs to, or the refusal.
async fn authenticated(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Arc<Catalog>, BuildHost), Response> {
    let Some(catalog) = state.apps.catalog().cloned() else {
        return Err(refusal(
            StatusCode::NOT_FOUND,
            "this server has no build hosts",
        ));
    };
    let Some(presented) = crate::security::bearer_token(headers) else {
        return Err(refusal(
            StatusCode::UNAUTHORIZED,
            "a build agent authenticates by `Authorization: Bearer <token>`: the token \
             shown when the host was added under Settings → Build hosts",
        ));
    };
    match authenticate_build_host(&catalog, &presented).await {
        Ok(host) => Ok((catalog, host)),
        Err(e) => Err(refusal(StatusCode::UNAUTHORIZED, &e.to_string())),
    }
}

/// The running job `id` of the host the request comes from. Another host's job,
/// a finished one or none at all is a 404: nothing tells one from another.
async fn held_job(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<(Arc<Catalog>, BuildJob), Response> {
    let (catalog, host) = authenticated(state, headers).await?;
    let not_held = || {
        refusal(
            StatusCode::NOT_FOUND,
            "this build host is not running that job: it finished, or its lease ran out",
        )
    };
    let Ok(id) = Uuid::parse_str(id) else {
        return Err(not_held());
    };
    match held_build_job(&catalog, id, &host).await {
        Ok(Some(job)) => Ok((catalog, job)),
        Ok(None) => Err(not_held()),
        Err(e) => Err(refusal(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    }
}

async fn job_application(
    catalog: &Catalog,
    job: &BuildJob,
) -> sc_error::Result<sc_app::Application> {
    load_application(catalog, AppId(job.application))
        .await?
        .ok_or_else(|| sc_error::Error::not_found("the application was deleted"))
}

/// `path`, relative to `app`'s store, as a file on disk; never outside the store.
fn store_file(
    catalog: &Catalog,
    app: &sc_app::Application,
    path: &str,
) -> sc_error::Result<PathBuf> {
    let root = sc_app::app_store_root(catalog, app)?;
    let clean = sc_app::clean_path(path);
    if clean.is_empty() || clean.split('/').any(|part| part == "..") {
        return Err(sc_error::Error::invalid(format!(
            "`{path}` is not a path inside the file store"
        )));
    }
    Ok(root.join(clean))
}

async fn append(path: &Path, bytes: &[u8]) -> sc_error::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    Ok(())
}

/// Write an uploaded body into a file beside `path` (`app.apk.part`), which
/// the caller moves into its place once it knows the job still wants it.
/// Answers that file and the size. A failed upload leaves nothing behind.
async fn receive(body: Body, path: &Path) -> sc_error::Result<(PathBuf, u64)> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".part");
    let partial = path.with_file_name(name);
    match write_body(body, &partial).await {
        Ok(size) => Ok((partial, size)),
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(e)
        }
    }
}

async fn write_body(body: Body, path: &Path) -> sc_error::Result<u64> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| sc_error::Error::msg(format!("receiving the artifact: {e}")))?;
        size += chunk.len() as u64;
        if size > MAX_ARTIFACT_BYTES {
            return Err(sc_error::Error::invalid("an artifact is at most 4 GiB"));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(size)
}

async fn file_tail(path: &Path, bytes: u64) -> String {
    let Ok(data) = tokio::fs::read(path).await else {
        return String::new();
    };
    let start = data
        .len()
        .saturating_sub(usize::try_from(bytes).unwrap_or(usize::MAX));
    String::from_utf8_lossy(&data[start..]).into_owned()
}

fn refusal(status: StatusCode, why: &str) -> Response {
    (status, axum::Json(json!({ "error": why }))).into_response()
}
