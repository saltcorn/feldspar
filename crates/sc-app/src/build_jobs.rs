//! Target builds that run on a build host (`TODO-build-hosts.md` B2, design
//! §13.3 "Build hosts").
//!
//! A local target build is a task of the server and lives in its memory. A job
//! for a build host is a row of **`_fd_build_jobs`** instead, because it waits
//! for an agent to ask for it and then runs on another machine for a quarter of
//! an hour: a restart of the server in between must not lose it.
//!
//! A job is `queued` until an agent that builds its target claims it, then
//! `running` on that host until the agent sends the artifact (`succeeded`) or
//! the reason it failed (`failed`). While it runs the host holds a **lease**,
//! which every heartbeat and every piece of log renews. A job whose lease runs
//! out fails, saying the build host stopped answering, so a Mac that is switched
//! off mid-build does not leave the target building forever.

use chrono::{DateTime, Duration, SubsecRound, Utc};
use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, Insert, Projection, Select, Source, Statement, Update, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::build_hosts::{BuildHost, ONLINE_SECONDS, list_build_hosts};
use crate::declared::{TargetSpec, requires_json, target_requires};
use crate::framework::InstallSpec;

/// The build job table in the primary database.
pub const BUILD_JOBS_TABLE: &str = "_fd_build_jobs";

/// How long a running job is held for its host without hearing from it: as
/// long as the host counts as online.
pub const LEASE_SECONDS: i64 = ONLINE_SECONDS;

const COL_ID: &str = "id";
const COL_APPLICATION: &str = "application_id";
const COL_TARGET: &str = "target";
const COL_LABEL: &str = "label";
const COL_STORE: &str = "store";
const COL_LOG_PATH: &str = "log_path";
const COL_STATE: &str = "state";
const COL_HOST: &str = "build_host_id";
const COL_HOST_NAME: &str = "build_host_name";
const COL_LEASE: &str = "lease_until";
const COL_CREATED_AT: &str = "created_at";
const COL_STARTED_AT: &str = "started_at";
const COL_FINISHED_AT: &str = "finished_at";
const COL_ARTIFACT: &str = "artifact";
const COL_SIZE: &str = "size";
const COL_LOG: &str = "log";
const COL_ERROR: &str = "error";

fn build_job_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_APPLICATION, uuid()).required(),
        DataField::plain(COL_TARGET, text()).required(),
        DataField::plain(COL_LABEL, text()).required(),
        DataField::plain(COL_STORE, text()).required(),
        DataField::plain(COL_LOG_PATH, text()).required(),
        DataField::plain(COL_STATE, text()).required(),
        DataField::plain(COL_HOST, uuid()),
        DataField::plain(COL_HOST_NAME, text()),
        DataField::plain(COL_LEASE, timestamp()),
        DataField::plain(COL_CREATED_AT, timestamp()).required(),
        DataField::plain(COL_STARTED_AT, timestamp()),
        DataField::plain(COL_FINISHED_AT, timestamp()),
        DataField::plain(COL_ARTIFACT, text()),
        DataField::plain(COL_SIZE, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_LOG, text()),
        DataField::plain(COL_ERROR, text()),
    ]
}

/// Ensure the build job table exists. Idempotent, like every bootstrap.
pub async fn bootstrap_build_jobs(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(BUILD_JOBS_TABLE, &build_job_fields())
        .await?;
    Ok(())
}

/// Where a job has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for a build host to ask for it.
    Queued,
    /// Claimed by a build host, which is building it.
    Running,
    /// The artifact is in the store.
    Succeeded,
    /// Ended without one; [`BuildJob::error`] says why.
    Failed,
}

impl JobState {
    /// The wire and column spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<JobState> {
        Some(match text {
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "succeeded" => JobState::Succeeded,
            "failed" => JobState::Failed,
            _ => return None,
        })
    }

    /// Whether the job is still to finish.
    pub fn is_open(self) -> bool {
        matches!(self, JobState::Queued | JobState::Running)
    }
}

/// One target build for a build host.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildJob {
    pub id: Uuid,
    /// The application it builds.
    pub application: Uuid,
    /// The target's key, `ios`, and its label, `iOS app`.
    pub target: String,
    pub label: String,
    /// The file store the log and the artifact go into.
    pub store: String,
    /// The build's log, relative to the store: written as the agent sends it.
    pub log_path: String,
    pub state: JobState,
    /// The build host that claimed it, and its name then.
    pub host: Option<Uuid>,
    pub host_name: Option<String>,
    /// Until when the host holds it without being heard from.
    pub lease_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// What it produced, relative to the store, and its size.
    pub artifact: Option<String>,
    pub size: Option<i64>,
    /// The end of the log, once it succeeded.
    pub log: Option<String>,
    /// Why it failed.
    pub error: Option<String>,
}

/// What a job is about, before it is queued.
#[derive(Debug, Clone, Copy)]
pub struct QueuedJob<'a> {
    pub application: Uuid,
    pub target: &'a str,
    pub label: &'a str,
    pub store: &'a str,
    pub log_path: &'a str,
}

/// Held while a build is queued, from looking for an open job to inserting the
/// new one, so two presses of Build at once queue one job, not two. It guards
/// this server process only.
static QUEUEING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Wait for any other request that is queueing a job, then block the next ones
/// until the returned guard is dropped. Hold it while checking for an open
/// job, preparing the project and calling [`queue_build_job`].
pub async fn lock_build_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUEING.lock().await
}

/// Queue a build of `job.target` for a build host, unless one is already queued
/// or running for the same application and target: then that one is answered,
/// as a second press of a local build's button joins the running build.
pub async fn queue_build_job(catalog: &Catalog, job: QueuedJob<'_>) -> Result<BuildJob> {
    expire_leases(catalog).await?;
    if let Some(open) = latest_build_job(catalog, job.application, job.target)
        .await?
        .filter(|j| j.state.is_open())
    {
        return Ok(open);
    }
    let queued = BuildJob {
        id: Uuid::new_v4(),
        application: job.application,
        target: job.target.to_owned(),
        label: job.label.to_owned(),
        store: job.store.to_owned(),
        log_path: job.log_path.to_owned(),
        state: JobState::Queued,
        host: None,
        host_name: None,
        lease_until: None,
        // To the microsecond, as the database keeps it, so this answer and
        // the next read of the job agree.
        created_at: Utc::now().trunc_subsecs(6),
        started_at: None,
        finished_at: None,
        artifact: None,
        size: None,
        log: None,
        error: None,
    };
    let columns = [
        COL_ID,
        COL_APPLICATION,
        COL_TARGET,
        COL_LABEL,
        COL_STORE,
        COL_LOG_PATH,
        COL_STATE,
        COL_CREATED_AT,
    ];
    let insert = Insert::row(
        BUILD_JOBS_TABLE,
        columns.iter().map(|c| (*c).to_owned()).collect(),
        vec![
            Expr::Lit(Value::Uuid(queued.id)),
            Expr::Lit(Value::Uuid(queued.application)),
            Expr::Lit(Value::Text(queued.target.clone())),
            Expr::Lit(Value::Text(queued.label.clone())),
            Expr::Lit(Value::Text(queued.store.clone())),
            Expr::Lit(Value::Text(queued.log_path.clone())),
            Expr::Lit(Value::Text(JobState::Queued.as_str().to_owned())),
            Expr::Lit(Value::Timestamp(queued.created_at)),
        ],
    );
    rows(catalog, Statement::from(insert)).await?;
    Ok(queued)
}

/// The latest job for `target` of `application`, whatever its state.
pub async fn latest_build_job(
    catalog: &Catalog,
    application: Uuid,
    target: &str,
) -> Result<Option<BuildJob>> {
    expire_leases(catalog).await?;
    let select = Select::from(Source::table(BUILD_JOBS_TABLE)).filter(
        Expr::col(COL_APPLICATION)
            .eq(Expr::lit(application))
            .and(Expr::col(COL_TARGET).eq(Expr::lit(target.to_owned()))),
    );
    Ok(jobs(catalog, select)
        .await?
        .into_iter()
        .max_by_key(|j| j.created_at))
}

/// Hand `host` the oldest queued job for one of its targets, marked `running`
/// with a lease. Returns `None` when no job is waiting.
///
/// Two agents asking at once never get the same job: only a job that is still
/// queued can be taken, so the slower agent moves on to the next one.
///
/// An agent builds one job at a time, so when it asks for another, any job it
/// is still running was lost: it gave up on it, restarted, or never heard the
/// answer that handed it over. That job fails now, rather than being kept alive
/// by the host's heartbeats forever.
pub async fn claim_build_job(catalog: &Catalog, host: &BuildHost) -> Result<Option<BuildJob>> {
    expire_leases(catalog).await?;
    drop_lost_jobs(catalog, host).await?;
    let select = Select::from(Source::table(BUILD_JOBS_TABLE))
        .filter(Expr::col(COL_STATE).eq(Expr::lit(JobState::Queued.as_str().to_owned())));
    let mut queued: Vec<BuildJob> = jobs(catalog, select)
        .await?
        .into_iter()
        .filter(|j| host.targets.contains(&j.target))
        .collect();
    queued.sort_by_key(|j| j.created_at);
    let now = Utc::now();
    for job in queued {
        let update = Update {
            returning: vec![Projection::all()],
            ..Update::new(
                BUILD_JOBS_TABLE,
                vec![
                    set(
                        COL_STATE,
                        Value::Text(JobState::Running.as_str().to_owned()),
                    ),
                    set(COL_HOST, Value::Uuid(host.id)),
                    set(COL_HOST_NAME, Value::Text(host.name.clone())),
                    set(COL_STARTED_AT, Value::Timestamp(now)),
                    set(COL_LEASE, Value::Timestamp(lease_from(now))),
                ],
            )
            .filter(
                Expr::col(COL_ID)
                    .eq(Expr::lit(job.id))
                    .and(Expr::col(COL_STATE).eq(Expr::lit(JobState::Queued.as_str().to_owned()))),
            )
        };
        if let Some(row) = rows(catalog, Statement::from(update)).await?.first() {
            return job_from_row(row).map(Some);
        }
    }
    Ok(None)
}

/// Fail every job `host` is running: it asked for a new one, so it lost them.
async fn drop_lost_jobs(catalog: &Catalog, host: &BuildHost) -> Result<()> {
    let select = Select::from(Source::table(BUILD_JOBS_TABLE)).filter(
        Expr::col(COL_HOST)
            .eq(Expr::lit(host.id))
            .and(Expr::col(COL_STATE).eq(Expr::lit(JobState::Running.as_str().to_owned()))),
    );
    for job in jobs(catalog, select).await? {
        finish_build_job(
            catalog,
            job.id,
            JobOutcome::Failed(format!(
                "the build host `{}` dropped the build of the {}: its agent restarted or lost \
                 touch with this server mid-build. Build again.",
                host.name, job.label
            )),
        )
        .await?;
    }
    Ok(())
}

/// Running job `id`, if `host` is the one building it. Returns `None` for
/// another host's job, a finished job, or an unknown id, so an agent can only
/// send logs and artifacts for its own jobs.
pub async fn held_build_job(
    catalog: &Catalog,
    id: Uuid,
    host: &BuildHost,
) -> Result<Option<BuildJob>> {
    expire_leases(catalog).await?;
    let select = Select::from(Source::table(BUILD_JOBS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    Ok(jobs(catalog, select)
        .await?
        .into_iter()
        .find(|j| j.state == JobState::Running && j.host == Some(host.id)))
}

/// Renew the lease of every job `host` is running: it was heard from just now.
pub async fn renew_leases(catalog: &Catalog, host: Uuid) -> Result<()> {
    let update = Update::new(
        BUILD_JOBS_TABLE,
        vec![set(COL_LEASE, Value::Timestamp(lease_from(Utc::now())))],
    )
    .filter(
        Expr::col(COL_HOST)
            .eq(Expr::lit(host))
            .and(Expr::col(COL_STATE).eq(Expr::lit(JobState::Running.as_str().to_owned()))),
    );
    rows(catalog, Statement::from(update)).await?;
    Ok(())
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    /// The artifact is in the store at `artifact`, `size` bytes, and `log` is
    /// the end of the build's log.
    Succeeded {
        artifact: String,
        size: i64,
        log: String,
    },
    Failed(String),
}

/// Record how running job `id` ended. A job that is no longer running (its
/// lease ran out first) keeps the end it already has.
pub async fn finish_build_job(catalog: &Catalog, id: Uuid, outcome: JobOutcome) -> Result<()> {
    end_job(catalog, id, JobState::Running, outcome).await
}

/// End job `id` with `outcome`, if it is still in state `from`.
async fn end_job(catalog: &Catalog, id: Uuid, from: JobState, outcome: JobOutcome) -> Result<()> {
    let mut assignments = vec![set(COL_FINISHED_AT, Value::Timestamp(Utc::now()))];
    match outcome {
        JobOutcome::Succeeded {
            artifact,
            size,
            log,
        } => {
            assignments.push(set(
                COL_STATE,
                Value::Text(JobState::Succeeded.as_str().to_owned()),
            ));
            assignments.push(set(COL_ARTIFACT, Value::Text(artifact)));
            assignments.push(set(COL_SIZE, Value::Int(size)));
            assignments.push(set(COL_LOG, Value::Text(log)));
        }
        JobOutcome::Failed(error) => {
            assignments.push(set(
                COL_STATE,
                Value::Text(JobState::Failed.as_str().to_owned()),
            ));
            assignments.push(set(COL_ERROR, Value::Text(error)));
        }
    }
    let update = Update::new(BUILD_JOBS_TABLE, assignments).filter(
        Expr::col(COL_ID)
            .eq(Expr::lit(id))
            .and(Expr::col(COL_STATE).eq(Expr::lit(from.as_str().to_owned()))),
    );
    rows(catalog, Statement::from(update)).await?;
    Ok(())
}

/// Fail every job no build host will finish, so none stays open forever:
/// - a running job whose lease has run out: its build host stopped answering;
/// - a queued job no online host builds: the host it was queued for went
///   offline, or was removed, before it took the job.
///
/// Asked whenever jobs are read, so nothing has to watch the clock.
pub async fn expire_leases(catalog: &Catalog) -> Result<()> {
    let select = Select::from(Source::table(BUILD_JOBS_TABLE))
        .filter(Expr::col(COL_STATE).eq(Expr::lit(JobState::Running.as_str().to_owned())));
    let now = Utc::now();
    for job in jobs(catalog, select).await? {
        if job.lease_until.is_none_or(|until| until > now) {
            continue;
        }
        let host = job.host_name.as_deref().unwrap_or("the build host");
        finish_build_job(
            catalog,
            job.id,
            JobOutcome::Failed(format!(
                "the build host `{host}` stopped answering while it built the {}, so the \
                 build was given up. Check that its agent is running, then build again.",
                job.label
            )),
        )
        .await?;
    }
    let select = Select::from(Source::table(BUILD_JOBS_TABLE))
        .filter(Expr::col(COL_STATE).eq(Expr::lit(JobState::Queued.as_str().to_owned())));
    let queued = jobs(catalog, select).await?;
    if queued.is_empty() {
        return Ok(());
    }
    let hosts = list_build_hosts(catalog).await?;
    for job in queued {
        let taken_up = hosts
            .iter()
            .any(|host| host.is_online(now) && host.targets.contains(&job.target));
        if taken_up {
            continue;
        }
        end_job(
            catalog,
            job.id,
            JobState::Queued,
            JobOutcome::Failed(format!(
                "no build host that builds the {} is online any more, so the build was \
                 given up before it started. Start the agent on the build host, then build \
                 again.",
                job.label
            )),
        )
        .await?;
    }
    Ok(())
}

/// The online build host that would build `target` now: it builds the target,
/// and its last heartbeat said its machine has everything the target needs.
pub fn host_for_target<'a>(
    hosts: &'a [BuildHost],
    target: &str,
    now: DateTime<Utc>,
) -> Option<&'a BuildHost> {
    hosts.iter().find(|host| {
        host.is_online(now)
            && host.targets.iter().any(|t| t == target)
            && host
                .report
                .as_ref()
                .and_then(|r| r.get("readiness")?.get(target)?.get("missing")?.as_array())
                .is_some_and(|missing| missing.is_empty())
    })
}

// --- the job as the agent hears it ---------------------------------------------

/// A target as a claim's answer carries it: everything the agent needs to run
/// the build, already rendered for the application, secrets included.
pub fn target_spec_json(spec: &TargetSpec) -> Json {
    json!({
        "name": spec.name,
        "label": spec.label,
        "command": spec.command,
        "args": spec.args,
        "source_dir": spec.source_dir,
        "artifact": spec.artifact,
        "install": spec.install.as_ref().map(|i| json!({
            "command": i.command,
            "args": i.args,
            "marker": i.marker,
        })),
        "env": spec.env,
        "requires": requires_json(&spec.requires),
    })
}

/// [`target_spec_json`] read back, on the agent.
pub fn target_spec_from_json(value: &Json) -> Result<TargetSpec> {
    let bad = |key: &str| Error::invalid(format!("the job's target has no valid `{key}`"));
    let text = |key: &str| -> Result<String> {
        value
            .get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad(key))
    };
    let texts = |v: Option<&Json>, key: &str| -> Result<Vec<String>> {
        v.and_then(Json::as_array)
            .ok_or_else(|| bad(key))?
            .iter()
            .map(|a| a.as_str().map(str::to_owned).ok_or_else(|| bad(key)))
            .collect()
    };
    let name = text("name")?;
    let install = match value.get("install") {
        None | Some(Json::Null) => None,
        Some(install) => Some(InstallSpec {
            command: install
                .get("command")
                .and_then(Json::as_str)
                .ok_or_else(|| bad("install"))?
                .to_owned(),
            args: texts(install.get("args"), "install")?,
            marker: install
                .get("marker")
                .and_then(Json::as_str)
                .ok_or_else(|| bad("install"))?
                .to_owned(),
        }),
    };
    let env = match value.get("env") {
        Some(Json::Object(env)) => env
            .iter()
            .map(|(k, v)| Ok((k.clone(), v.as_str().ok_or_else(|| bad("env"))?.to_owned())))
            .collect::<Result<_>>()?,
        _ => return Err(bad("env")),
    };
    Ok(TargetSpec {
        label: text("label")?,
        command: text("command")?,
        args: texts(value.get("args"), "args")?,
        source_dir: text("source_dir")?,
        artifact: text("artifact")?,
        install,
        env,
        requires: target_requires(&name, value.get("requires"))?,
        name,
    })
}

// --- rows ----------------------------------------------------------------------

fn set(column: &str, value: Value) -> Assignment {
    Assignment::new(column.to_owned(), Expr::Lit(value))
}

fn lease_from(now: DateTime<Utc>) -> DateTime<Utc> {
    now + Duration::seconds(LEASE_SECONDS)
}

async fn jobs(catalog: &Catalog, select: Select) -> Result<Vec<BuildJob>> {
    rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(job_from_row)
        .collect()
}

fn job_from_row(row: &Row) -> Result<BuildJob> {
    let bad = |column: &str| Error::invalid(format!("{BUILD_JOBS_TABLE}.{column} is malformed"));
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(text)) => Ok(Some(text.clone())),
        Some(Value::Null) | None => Ok(None),
        _ => Err(bad(column)),
    };
    let required = |column: &str| text(column)?.ok_or_else(|| bad(column));
    let uuid = |column: &str| match row.get(column) {
        Some(Value::Uuid(id)) => Ok(Some(*id)),
        Some(Value::Null) | None => Ok(None),
        _ => Err(bad(column)),
    };
    let timestamp = |column: &str| match row.get(column) {
        Some(Value::Timestamp(at)) => Ok(Some(*at)),
        Some(Value::Null) | None => Ok(None),
        _ => Err(bad(column)),
    };
    Ok(BuildJob {
        id: uuid(COL_ID)?.ok_or_else(|| bad(COL_ID))?,
        application: uuid(COL_APPLICATION)?.ok_or_else(|| bad(COL_APPLICATION))?,
        target: required(COL_TARGET)?,
        label: required(COL_LABEL)?,
        store: required(COL_STORE)?,
        log_path: required(COL_LOG_PATH)?,
        state: JobState::parse(&required(COL_STATE)?).ok_or_else(|| bad(COL_STATE))?,
        host: uuid(COL_HOST)?,
        host_name: text(COL_HOST_NAME)?,
        lease_until: timestamp(COL_LEASE)?,
        created_at: timestamp(COL_CREATED_AT)?.ok_or_else(|| bad(COL_CREATED_AT))?,
        started_at: timestamp(COL_STARTED_AT)?,
        finished_at: timestamp(COL_FINISHED_AT)?,
        artifact: text(COL_ARTIFACT)?,
        size: match row.get(COL_SIZE) {
            Some(Value::Int(size)) => Some(*size),
            Some(Value::Null) | None => None,
            _ => return Err(bad(COL_SIZE)),
        },
        log: text(COL_LOG)?,
        error: text(COL_ERROR)?,
    })
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declared::{TargetRequirement, TargetRequirementKind};

    fn host(targets: &[&str], report: Json) -> BuildHost {
        BuildHost {
            id: Uuid::new_v4(),
            name: "Mac mini".into(),
            targets: targets.iter().map(|t| (*t).to_owned()).collect(),
            created_at: Utc::now(),
            last_seen_at: Some(Utc::now()),
            report: Some(report),
        }
    }

    #[test]
    fn a_target_goes_to_the_agent_and_back_unchanged() {
        let spec = TargetSpec {
            name: "ios".into(),
            label: "iOS app".into(),
            command: "npx".into(),
            args: vec!["expo".into(), "run:ios".into()],
            source_dir: "todo".into(),
            artifact: "todo/ios-output/app.ipa".into(),
            install: Some(InstallSpec {
                command: "npm".into(),
                args: vec!["install".into()],
                marker: "node_modules".into(),
            }),
            env: [("ASC_KEY_ID".to_owned(), "ABC123".to_owned())].into(),
            requires: vec![TargetRequirement {
                kind: TargetRequirementKind::Os {
                    name: "macos".into(),
                },
                hint: String::new(),
            }],
        };
        assert_eq!(
            target_spec_from_json(&target_spec_json(&spec)).unwrap(),
            spec
        );
    }

    #[test]
    fn a_target_goes_to_an_online_host_that_reported_it_ready() {
        let now = Utc::now();
        let ready = json!({ "readiness": { "ios": { "missing": [] } } });
        let lacking =
            json!({ "readiness": { "ios": { "missing": ["`pod` is not on the PATH."] } } });
        assert!(host_for_target(&[host(&["ios"], ready.clone())], "ios", now).is_some());
        assert!(host_for_target(&[host(&["ios"], lacking)], "ios", now).is_none());
        assert!(host_for_target(&[host(&["ios"], json!({}))], "ios", now).is_none());
        assert!(host_for_target(&[host(&["android"], ready.clone())], "ios", now).is_none());
        let mut offline = host(&["ios"], ready);
        offline.last_seen_at = Some(now - Duration::seconds(ONLINE_SECONDS + 1));
        assert!(host_for_target(&[offline], "ios", now).is_none());
    }
}
