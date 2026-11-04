//! "Get datasets": the public dataset catalogue's handlers, and its installs
//! run as **background jobs** (`sc_api::public_datasets`).
//!
//! Getting the largest dataset is a 33 MB download and a third of a million
//! rows written through the row layer: a minute or two. Holding a request open
//! for that is at the mercy of every proxy between the admin and the server, so
//! starting an install answers at once with the job and the Analytics UI asks
//! how it is going until it is done — the shape [`crate::target_builds`] gives
//! a target build, for the same reason. One job per dataset, started at most
//! once at a time; the last one is kept after it finishes so a page that polls
//! a moment late still reads the outcome. The registry lives in this process: a
//! restart forgets the jobs, and one cut short leaves no tables behind, because
//! its rows were never committed.

use sc_types::Role;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sc_api::public_datasets::{self, Fetch, Installed, Progress};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::handler::{HandlerRegistry, HandlerResponse};

/// How long one file may take to download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// The largest file this downloads; the catalogue's largest is about 33 MB.
const MAX_DOWNLOAD: usize = 256 * 1024 * 1024;

/// Downloads over HTTPS with the server's one `reqwest`.
pub struct HttpFetch {
    client: reqwest::Client,
}

impl HttpFetch {
    /// A client with a timeout, saying who it is.
    pub fn new() -> HttpFetch {
        let client = sc_net::client_builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .user_agent(concat!(
                "Feldspar/",
                env!("CARGO_PKG_VERSION"),
                " (public datasets)"
            ))
            .build()
            // Only a TLS backend that will not start fails here, and then the
            // default client fails the same way, on first use.
            .unwrap_or_default();
        HttpFetch { client }
    }
}

impl Default for HttpFetch {
    fn default() -> HttpFetch {
        HttpFetch::new()
    }
}

#[async_trait]
impl Fetch for HttpFetch {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| Error::msg(e.to_string()))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::msg(e.to_string()))?
        {
            if bytes.len() + chunk.len() > MAX_DOWNLOAD {
                return Err(Error::msg(format!(
                    "it is larger than the {} MB a public dataset may be",
                    MAX_DOWNLOAD / (1024 * 1024)
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// Where an install has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Still running; the latest progress, once there is some.
    Running(Option<Progress>),
    /// Finished, and made (or found) this.
    Succeeded(Installed),
    /// Finished without it, and why.
    Failed(String),
}

/// One install of one public dataset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallJob {
    /// The catalogue entry.
    pub key: String,
    /// Where it has got to.
    pub status: JobStatus,
    /// When it was started.
    pub started_at: DateTime<Utc>,
    /// When it finished, once it has.
    pub finished_at: Option<DateTime<Utc>>,
}

impl InstallJob {
    /// The wire shape (`public_dataset_job_schema`).
    pub fn to_json(&self) -> Json {
        let (status, progress, installed, error) = match &self.status {
            JobStatus::Running(progress) => ("running", progress.as_ref(), None, None),
            JobStatus::Succeeded(installed) => ("succeeded", None, Some(installed), None),
            JobStatus::Failed(error) => ("failed", None, None, Some(error)),
        };
        json!({
            "key": self.key,
            "status": status,
            "stage": progress.map(|p| p.stage),
            "subject": progress.map(|p| p.subject.clone()),
            "done": progress.map_or(0, |p| p.done),
            "total": progress.map_or(0, |p| p.total),
            "started_at": self.started_at.to_rfc3339(),
            "finished_at": self.finished_at.map(|t| t.to_rfc3339()),
            "dataset_id": installed.map(|i| i.dataset_id.to_string()),
            "dataset_name": installed.map(|i| i.dataset_name.clone()),
            "rows": installed.map(|i| i.rows),
            "error": error,
        })
    }
}

/// The installs, by catalogue key.
#[derive(Default, Clone)]
pub struct InstallJobs {
    jobs: Arc<Mutex<HashMap<String, InstallJob>>>,
}

impl InstallJobs {
    /// The latest install of `key`, running or finished.
    pub fn get(&self, key: &str) -> Option<InstallJob> {
        self.jobs.lock().ok()?.get(key).cloned()
    }

    /// Start installing `key` on its own task, unless an install of it is
    /// already running — in which case that one is answered.
    #[allow(
        clippy::disallowed_methods,
        reason = "tokio::spawn: detached on purpose; the work outlives the request that started it and must not run in that request's changeset"
    )]
    pub fn start(
        &self,
        catalog: Arc<Catalog>,
        key: &str,
        fetch: Arc<dyn Fetch>,
        context: sc_catalog::Principal,
    ) -> InstallJob {
        let job = {
            let Ok(mut jobs) = self.jobs.lock() else {
                return failed_now(key, "the install registry is unavailable");
            };
            if let Some(running) = jobs
                .get(key)
                .filter(|j| matches!(j.status, JobStatus::Running(_)))
            {
                return running.clone();
            }
            let job = InstallJob {
                key: key.to_owned(),
                status: JobStatus::Running(None),
                started_at: Utc::now(),
                finished_at: None,
            };
            jobs.insert(key.to_owned(), job.clone());
            job
        };

        let owned_key = key.to_owned();
        let progress_jobs = Arc::clone(&self.jobs);
        let progress_key = owned_key.clone();
        let install = sc_catalog::spawn_scoped(async move {
            let progress = move |p: Progress| {
                if let Ok(mut jobs) = progress_jobs.lock()
                    && let Some(job) = jobs.get_mut(&progress_key)
                    && matches!(job.status, JobStatus::Running(_))
                {
                    job.status = JobStatus::Running(Some(p));
                }
            };
            public_datasets::install(
                &catalog,
                &owned_key,
                &*fetch,
                &progress,
                sc_access::admin_access(&context)?,
            )
            .await
        });
        // A supervisor awaits the install, so one that panics is recorded as
        // failed rather than leaving the job running for good.
        let jobs = Arc::clone(&self.jobs);
        let key = key.to_owned();
        tokio::spawn(async move {
            let outcome = match install.await {
                Ok(outcome) => outcome.map_err(|e| e.message_and_causes()),
                Err(joined) => Err(if joined.is_panic() {
                    "the install stopped unexpectedly (a panic in the server); see the server log"
                        .to_owned()
                } else {
                    "the install was cancelled".to_owned()
                }),
            };
            let Ok(mut jobs) = jobs.lock() else { return };
            let Some(job) = jobs.get_mut(&key) else {
                return;
            };
            job.finished_at = Some(Utc::now());
            job.status = match outcome {
                Ok(installed) => JobStatus::Succeeded(installed),
                Err(error) => JobStatus::Failed(error),
            };
        });
        job
    }
}

fn failed_now(key: &str, error: &str) -> InstallJob {
    let now = Utc::now();
    InstallJob {
        key: key.to_owned(),
        status: JobStatus::Failed(error.to_owned()),
        started_at: now,
        finished_at: Some(now),
    }
}

/// Register the public dataset handlers on `reg`, downloading with `fetch`.
pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>, fetch: Arc<dyn Fetch>) {
    let jobs = InstallJobs::default();

    reg.register_admin("listPublicDatasets", {
        let catalog = catalog.clone();
        let jobs = jobs.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            let jobs = jobs.clone();
            async move {
                let listing = public_datasets::list(&catalog).await?;
                let out: Vec<Json> = listing
                    .iter()
                    .map(|entry| {
                        let mut value = serde_json::to_value(entry).unwrap_or(Json::Null);
                        value["job"] = jobs.get(entry.key).map_or(Json::Null, |j| j.to_json());
                        value
                    })
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("installPublicDataset", {
        let catalog = catalog.clone();
        let jobs = jobs.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let jobs = jobs.clone();
            let fetch = Arc::clone(&fetch);
            async move {
                let key = ctx.path_param("key")?;
                // An unknown key is a 404 now, not a failed job later.
                public_datasets::find(key)?;
                let caller = sc_access::principal_at(Role::ADMIN, ctx.user());
                let job = jobs.start(catalog, key, fetch, caller);
                Ok(HandlerResponse::ok(job.to_json()).with_status(202))
            }
        }
    });

    reg.register_admin("getPublicDatasetInstall", {
        let jobs = jobs.clone();
        move |ctx| {
            let jobs = jobs.clone();
            async move {
                let key = ctx.path_param("key")?;
                let job = jobs.get(key).ok_or_else(|| {
                    Error::not_found(format!(
                        "`{key}` has not been installed since the server started"
                    ))
                })?;
                Ok(HandlerResponse::ok(job.to_json()))
            }
        }
    });
}
