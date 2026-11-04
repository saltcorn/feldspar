//! Build targets run as **background jobs** (design §13.3).
//!
//! A web build is a minute of a bundler; an Android APK is up to a quarter of an
//! hour of Gradle. Holding one HTTP request open for that is at the mercy of
//! every proxy, browser and reload between the admin and the server, and a
//! request that is dropped loses the result while the build carries on without
//! an owner. So starting a target build answers at once, the build runs as a
//! task of its own, and the admin UI asks how it is going until it is done.
//!
//! One job per application and target, **started at most once at a time**:
//! pressing the button again while a build is running — from another tab, or
//! after a reload — answers the running job instead of starting a second Gradle
//! in the same project directory. The last job is kept after it finishes, so a
//! page that polls a moment late still reads the outcome.
//!
//! The registry lives in this process and nowhere else. A restart ends the
//! builds it was running and forgets their outcomes; the artifact of a finished
//! one is still in the file store, which is the record that matters. A server
//! stopped with Ctrl-C or `systemctl stop` takes its builds with it — the
//! terminal and systemd signal every process the server started — but one
//! killed on its own, or crashing, leaves a Gradle to finish or fail by itself.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use sc_error::Result;

/// Where a target build has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Still running.
    Running,
    /// Finished and left its artifact.
    Succeeded,
    /// Finished without one; [`TargetJob::error`] says why.
    Failed,
}

impl JobStatus {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
        }
    }
}

/// What a finished build left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetArtifact {
    /// Its path, relative to the store.
    pub path: String,
    /// Its size in bytes.
    pub size: u64,
    /// The end of the build's log; the whole of it is at [`TargetJob::log_path`].
    pub log: String,
}

/// One target build of one application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetJob {
    /// The target's key — `android`.
    pub target: String,
    /// The target's label — `Android APK`.
    pub label: String,
    /// The file store the log and the artifact are in.
    pub store: String,
    /// The build's log, relative to the store — written as the build runs, so
    /// it can be opened in the file manager before it finishes.
    pub log_path: String,
    /// Where it has got to.
    pub status: JobStatus,
    /// When it was started.
    pub started_at: DateTime<Utc>,
    /// When it finished, once it has.
    pub finished_at: Option<DateTime<Utc>>,
    /// What it produced, when it succeeded.
    pub artifact: Option<TargetArtifact>,
    /// Why it failed — the tools' own output — when it did.
    pub error: Option<String>,
}

/// The jobs, keyed by application id and target.
#[derive(Default)]
pub struct TargetBuilds {
    jobs: Arc<Mutex<HashMap<(String, String), TargetJob>>>,
}

impl TargetBuilds {
    /// The latest job for `target` of application `app`, running or finished.
    pub fn get(&self, app: &str, target: &str) -> Option<TargetJob> {
        self.jobs
            .lock()
            .ok()?
            .get(&(app.to_owned(), target.to_owned()))
            .cloned()
    }

    /// Start `build` as the job for `target` of application `app`, unless one
    /// is already running — in which case that one is answered and `build` is
    /// dropped without being run.
    ///
    /// Answers the job as it stands once started, which is `running`. The
    /// build runs on its own task, so the caller's request can end at once.
    #[allow(
        clippy::disallowed_methods,
        reason = "tokio::spawn: detached on purpose; the work outlives the request that started it and must not run in that request's changeset"
    )]
    pub fn start<F>(&self, app: &str, job: NewJob<'_>, build: F) -> TargetJob
    where
        F: Future<Output = Result<TargetArtifact>> + Send + 'static,
    {
        let key = (app.to_owned(), job.target.to_owned());
        let job = {
            let Ok(mut jobs) = self.jobs.lock() else {
                return failed_now(job, "the build registry is unavailable");
            };
            if let Some(running) = jobs.get(&key).filter(|j| j.status == JobStatus::Running) {
                return running.clone();
            }
            let job = TargetJob {
                target: job.target.to_owned(),
                label: job.label.to_owned(),
                store: job.store.to_owned(),
                log_path: job.log_path.to_owned(),
                status: JobStatus::Running,
                started_at: Utc::now(),
                finished_at: None,
                artifact: None,
                error: None,
            };
            jobs.insert(key.clone(), job.clone());
            job
        };

        let jobs = Arc::clone(&self.jobs);
        // The build runs on a task of its own and a supervisor awaits it, so a
        // build that **panics** is recorded as failed rather than leaving the job
        // `running` for good — which would make every later start join a dead
        // job and the admin UI poll it forever.
        let build = tokio::spawn(build);
        tokio::spawn(async move {
            let outcome = match build.await {
                Ok(outcome) => outcome,
                Err(joined) => Err(sc_error::Error::msg(if joined.is_panic() {
                    "the build stopped unexpectedly (a panic in the server); see the server log"
                } else {
                    "the build was cancelled"
                })),
            };
            let Ok(mut jobs) = jobs.lock() else { return };
            let Some(job) = jobs.get_mut(&key) else {
                return;
            };
            job.finished_at = Some(Utc::now());
            match outcome {
                Ok(artifact) => {
                    job.status = JobStatus::Succeeded;
                    job.artifact = Some(artifact);
                }
                Err(e) => {
                    job.status = JobStatus::Failed;
                    job.error = Some(sc_error::format_chain(&e));
                }
            }
        });
        job
    }
}

/// What a job is about, before it has started.
#[derive(Debug, Clone, Copy)]
pub struct NewJob<'a> {
    /// The target's key — `android`.
    pub target: &'a str,
    /// Its label — `Android APK`.
    pub label: &'a str,
    /// The file store the log and the artifact go into.
    pub store: &'a str,
    /// Where the log goes, relative to the store.
    pub log_path: &'a str,
}

/// A job that failed before it could start — the registry's own lock poisoned,
/// which a build cannot fix and the admin should hear about.
fn failed_now(job: NewJob<'_>, error: &str) -> TargetJob {
    let now = Utc::now();
    TargetJob {
        target: job.target.to_owned(),
        label: job.label.to_owned(),
        store: job.store.to_owned(),
        log_path: job.log_path.to_owned(),
        status: JobStatus::Failed,
        started_at: now,
        finished_at: Some(now),
        artifact: None,
        error: Some(error.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    const ANDROID: NewJob<'static> = NewJob {
        target: "android",
        label: "Android APK",
        store: "apps",
        log_path: "todo/build-logs/android-1.log",
    };

    fn artifact() -> TargetArtifact {
        TargetArtifact {
            path: "todo/app.apk".to_owned(),
            size: 7,
            log: "BUILD SUCCESSFUL".to_owned(),
        }
    }

    /// Wait until the spawned task has recorded an outcome.
    async fn settled(builds: &TargetBuilds, app: &str, target: &str) -> TargetJob {
        for _ in 0..200 {
            if let Some(job) = builds
                .get(app, target)
                .filter(|j| j.status != JobStatus::Running)
            {
                return job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the job never finished");
    }

    #[tokio::test]
    async fn a_second_start_while_running_joins_the_running_build() {
        let builds = TargetBuilds::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let (finish, finished) = oneshot::channel::<()>();

        let counted = Arc::clone(&runs);
        let first = builds.start("app-1", ANDROID, async move {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = finished.await;
            Ok(artifact())
        });
        assert_eq!(first.status, JobStatus::Running);
        // The log is named from the start, so it can be opened while it grows.
        assert_eq!(first.log_path, "todo/build-logs/android-1.log");

        // Pressed again while it runs: the same job, and the second build never runs.
        let counted = Arc::clone(&runs);
        let second = builds.start("app-1", ANDROID, async move {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(artifact())
        });
        assert_eq!(second.started_at, first.started_at);

        finish.send(()).unwrap();
        let done = settled(&builds, "app-1", "android").await;
        assert_eq!(done.status, JobStatus::Succeeded);
        assert_eq!(done.artifact, Some(artifact()));
        assert!(done.finished_at.is_some());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failure_is_kept_with_the_tools_message_and_a_new_build_may_start() {
        let builds = TargetBuilds::default();
        builds.start("app-1", ANDROID, async {
            Err(sc_error::Error::config("SDK location not found"))
        });
        let failed = settled(&builds, "app-1", "android").await;
        assert_eq!(failed.status, JobStatus::Failed);
        assert!(failed.error.unwrap().contains("SDK location not found"));

        // Finished jobs do not block the next one.
        let again = builds.start("app-1", ANDROID, async { Ok(artifact()) });
        assert_eq!(again.status, JobStatus::Running);
        assert_eq!(
            settled(&builds, "app-1", "android").await.status,
            JobStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn a_build_that_panics_is_recorded_as_failed_and_does_not_block_the_next() {
        let builds = TargetBuilds::default();
        builds.start("app-1", ANDROID, async {
            panic!("a bug in the build");
        });
        let failed = settled(&builds, "app-1", "android").await;
        assert_eq!(failed.status, JobStatus::Failed);
        assert!(failed.error.unwrap().contains("stopped unexpectedly"));
        // The target is free again.
        let again = builds.start("app-1", ANDROID, async { Ok(artifact()) });
        assert_eq!(again.status, JobStatus::Running);
    }

    #[tokio::test]
    async fn jobs_are_per_application_and_target() {
        let builds = TargetBuilds::default();
        let (_hold, held) = oneshot::channel::<()>();
        builds.start("app-1", ANDROID, async move {
            let _ = held.await;
            Ok(artifact())
        });
        assert!(builds.get("app-2", "android").is_none());
        assert!(builds.get("app-1", "ios").is_none());
        let other = builds.start("app-2", ANDROID, async { Ok(artifact()) });
        assert_eq!(other.status, JobStatus::Running);
    }
}
