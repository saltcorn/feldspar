//! The change log's cleanup, on a timer: entries offline sync and row history
//! are both done with, then the files of rows whose delete entries expired,
//! then the changesets left empty (§14.11 "Cleanup", §14.12 "Schema edits and
//! retention"). The work is [`sc_catalog::retention::clean_change_log`].

use std::sync::Arc;

use crate::router::log_failure;

/// How often the cleanup runs. Retention is counted in days, so an hour late
/// is nothing.
const INTERVAL_SECS: u64 = 3600;

/// Clean the change log every hour, for the life of the process.
pub(crate) fn spawn_change_log_cleanup(apps: Arc<crate::apps::AppMounts>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(INTERVAL_SECS));
        loop {
            tick.tick().await;
            let Some(catalog) = apps.catalog() else {
                continue;
            };
            match sc_catalog::retention::clean_change_log(catalog, chrono::Utc::now()).await {
                Ok(done) if done == Default::default() => {}
                Ok(done) => sc_log::log_info!(
                    "change log cleanup: removed {} entries, {} files, {} changesets, {} sync keys",
                    done.entries,
                    done.files,
                    done.changesets,
                    done.sync_keys
                ),
                Err(e) => log_failure("cleaning the change log", &e),
            }
        }
    });
}
