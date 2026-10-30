//! Reloading a running server from its database and its disk, on `SIGHUP`
//! (design §13.2, "the mount registry is live; a full restart should never be
//! required").
//!
//! What a mounted application serves is a **snapshot**: [`CodeFramework`](sc_app::CodeFramework) holds
//! the bytes [`AssetBundle::from_dir`](sc_app::AssetBundle) read out of the
//! build's output directory, and every later request is answered from that map.
//! So a developer — or a coding agent working inside the project directory — who
//! runs `npm run build` changes the disk and changes nothing a browser can see,
//! because only the Build button and a restart ever replace those bytes.
//!
//! `SIGHUP` is the third way, and the cheap one:
//!
//! ```text
//! npm run build && pkill -HUP feldspar
//! ```
//!
//! It **vacates the serving cache** — every mounted app's bundle is re-read from
//! its output directory — and reloads the definitions around it: the catalog
//! (re-introspected, overlays and all), every stored application row, which is
//! what rebuilds the API providers, and the **stream set**, so a table added, a
//! column added, a custom SQL query saved or a stream defined by another
//! process is being served when the signal returns.
//!
//! The streams are a **diff and not a restart** (`sc_stream`'s §6): a stream
//! whose row is untouched keeps its broker session, and only one whose provider
//! or configuration changed is stopped and started. That is what makes it safe
//! to reload everything on every signal — an agent running `npm run build &&
//! pkill -HUP feldspar` in a loop must not be hanging up on a broker each
//! time.
//!
//! **It runs no bundler and no installer.** That is the whole point: the caller
//! has just built, and re-running `npm install && npm run build` would be both
//! slow and a duplicate of the work that prompted the signal. An application
//! that has never been built has nothing to load and says so.
//!
//! The **code libraries** are reloaded too, so a library changed by another
//! process (a restore, a script writing `_fd_code_libraries`) is what the next
//! `lib()` imports. Only a save or delete through this server reloads them
//! otherwise.
//!
//! A **Saltcorn UI** application has no bundle: its views and pages are rows,
//! and a reload re-reads them under a new generation (TODO "Saltcorn UI" 5.7).
//!
//! The **modules** are reloaded too, but only when `_fd_modules` changed since
//! they were loaded — a module installed, configured, granted something or
//! removed by `feldspar module …` in another process. So
//! `feldspar module install --local DIR && pkill -HUP feldspar` is the way to
//! change a running server's modules from a shell, and a signal after a mere
//! build restarts no module's services.
//!
//! What it does *not* reload, and still wants a restart: the trigger set, the
//! agents, the LLM providers, the file-store *connections* (a store's contents
//! are read live, but a store definition added since boot is not connected
//! here), and the **database connections** (a connected database is
//! re-introspected with the catalog, but one defined since boot is not dialled
//! here). Those are assembled once at boot and shared into the scheduler and the
//! catalog's write path, so swapping them is a larger change than a reload — and
//! each already has an admin API that updates the live set in place.
//!
//! The **streams** were on that list and are not any more (TODO "Streams" 5.4).
//! They could come off it because a supervisor already knows how to diff: it is
//! the one of these sets whose "update the live set in place" is a method rather
//! than a restart, so reloading it here costs an untouched stream nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sc_app::{Application, list_applications};

use crate::apps::AppMounts;

/// What one [`reload_all`] did, and how long each half of it took.
///
/// A report rather than a `Result`, because a reload is a **best-effort refresh
/// of a server that is already serving**: a catalog that will not re-introspect
/// and an application whose bundle has gone missing are both things the operator
/// must be told about and neither is a reason to stop answering requests. Every
/// failure is named here and every success is counted, and [`log`](Self::log)
/// writes the lot to the console the operator is already watching.
#[derive(Debug, Default)]
pub struct ReloadReport {
    /// How long re-introspecting the catalog took.
    pub catalog: Duration,
    /// How long reloading the applications took — their rows, their providers
    /// and their bundles.
    pub applications: Duration,
    /// How long the whole reload took, which is the number worth watching: it is
    /// what stands between an agent's `npm run build` and its screenshot.
    pub total: Duration,
    /// How many tables the reloaded catalog holds.
    pub tables: usize,
    /// The subdomains whose apps were reloaded and re-mounted.
    pub reloaded: Vec<String>,
    /// How many assets were re-read from disk across all of them.
    pub assets: usize,
    /// The subdomains unmounted because their row is no longer in the database.
    pub unmounted: Vec<String>,
    /// The apps that could not be reloaded, and why. Each is **still serving its
    /// previous version** — the remount happens only after the new bundle has
    /// loaded, so a failure here disturbs nothing.
    pub failed: Vec<(String, String)>,
    /// Why the catalog did not reload, if it did not.
    pub catalog_error: Option<String>,
    /// Why the application rows could not be listed, if they could not.
    pub applications_error: Option<String>,
    /// How long reloading the stream set took.
    pub streams: Duration,
    /// Whether the reload started, stopped or replaced any stream. `false` is
    /// the ordinary case and the one worth having: it is how the log says "your
    /// broker sessions were not touched".
    pub streams_changed: bool,
    /// Why the stream set could not be reloaded, if it could not.
    pub streams_error: Option<String>,
    /// How long checking — and, when they changed, reloading — the modules
    /// took.
    pub modules: Duration,
    /// Whether `_fd_modules` had changed, so the module set was reloaded.
    /// `false` is the ordinary case: a reload that touched no module leaves
    /// every module's services running.
    pub modules_reloaded: bool,
    /// Why the modules could not be reloaded, if they could not.
    pub modules_error: Option<String>,
    /// Why the code libraries could not be reloaded, if they could not. The
    /// set loaded before is still the one imported.
    pub code_libraries_error: Option<String>,
}

impl ReloadReport {
    /// Whether anything went wrong.
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty()
            && self.catalog_error.is_none()
            && self.applications_error.is_none()
            && self.streams_error.is_none()
            && self.modules_error.is_none()
            && self.code_libraries_error.is_none()
    }

    /// Write the reload to the operator's console — one line per thing that
    /// happened and a summary carrying the timings.
    ///
    /// The timings are the point of logging this at all. A reload is the step
    /// between "I built" and "I can look at it", so the question it has to
    /// answer is *how long do I wait*, and the only honest answer is what it
    /// just took.
    pub fn log(&self) {
        if let Some(e) = &self.modules_error {
            eprintln!("feldspar: the modules could not be reloaded: {e}");
        } else if self.modules_reloaded {
            eprintln!(
                "feldspar: reloaded the modules in {} — the installed set changed",
                ms(self.modules)
            );
        }
        if let Some(e) = &self.catalog_error {
            eprintln!("feldspar: the catalog could not be reloaded: {e}");
        } else {
            eprintln!(
                "feldspar: reloaded the catalog — {} table{} in {}",
                self.tables,
                plural(self.tables),
                ms(self.catalog)
            );
        }
        if let Some(e) = &self.code_libraries_error {
            eprintln!("feldspar: the code libraries could not be reloaded: {e}");
        }
        if let Some(e) = &self.applications_error {
            eprintln!("feldspar: the applications could not be listed: {e}");
        }
        for subdomain in &self.reloaded {
            eprintln!("feldspar: reloaded application `{subdomain}`");
        }
        for subdomain in &self.unmounted {
            eprintln!("feldspar: unmounted application `{subdomain}` — its row is gone");
        }
        if let Some(e) = &self.streams_error {
            eprintln!("feldspar: the streams could not be reloaded: {e}");
        } else if self.streams_changed {
            eprintln!("feldspar: reloaded the streams — the running set changed");
        }
        for (subdomain, error) in &self.failed {
            eprintln!(
                "feldspar: application `{subdomain}` could not be reloaded and is still \
                 serving its previous version: {error}"
            );
        }
        eprintln!(
            "feldspar: reload complete in {} — catalog {}, {} application{} ({} asset{}) {}, \
             streams {}",
            ms(self.total),
            ms(self.catalog),
            self.reloaded.len(),
            plural(self.reloaded.len()),
            self.assets,
            plural(self.assets),
            ms(self.applications),
            ms(self.streams),
        );
    }
}

/// A duration as milliseconds, with enough precision to see a fast reload.
fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

/// `""` or `"s"`.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Reload the catalog and every application, **building nothing**.
///
/// In order, because the order matters: the catalog first, since an application's
/// providers are projected from the tables it holds, then the application rows,
/// whose bundles come off disk exactly as the last build left them.
///
/// An admin-only server (no catalog) has nothing to reload and returns an empty
/// report.
pub async fn reload_all(apps: &AppMounts) -> ReloadReport {
    let started = Instant::now();
    let mut report = ReloadReport::default();

    let Some(catalog) = apps.catalog().cloned() else {
        report.total = started.elapsed();
        return report;
    };

    // The modules first (when another process changed `_fd_modules` —
    // `feldspar module install`, say): a module can supply tables, frameworks
    // and stream providers, so what the catalog, the applications and the
    // streams reload against below is the set it leaves.
    let phase = Instant::now();
    if let Some(modules) = apps.modules() {
        match modules.reload_if_changed().await {
            Ok(reloaded) => report.modules_reloaded = reloaded,
            Err(e) => report.modules_error = Some(sc_error::format_chain(&e)),
        }
    }
    report.modules = phase.elapsed();

    let phase = Instant::now();
    match catalog.reload().await {
        Ok(()) => report.tables = catalog.tables().map(|t| t.len()).unwrap_or_default(),
        Err(e) => report.catalog_error = Some(e.to_string()),
    }
    report.catalog = phase.elapsed();

    // The code libraries: held in memory, and otherwise reloaded only by a
    // save or delete through this server. A failure keeps the set already
    // loaded; a process that never loaded them (generation 0) has no table to
    // read them from.
    if catalog.code_libraries_generation() > 0
        && let Err(e) = sc_catalog::reload_code_libraries(&catalog).await
    {
        report.code_libraries_error = Some(e.to_string());
    }

    let phase = Instant::now();
    match list_applications(&catalog).await {
        Ok(stored) => reload_applications(apps, stored, &mut report).await,
        // The catalog is reloaded either way — half a reload is better than none,
        // and the operator is told which half.
        Err(e) => report.applications_error = Some(e.to_string()),
    }
    report.applications = phase.elapsed();

    // The stream set (TODO "Streams" 5.4): a row added, disabled or reconfigured
    // by another process takes effect here. A **diff**, so a stream nobody
    // touched keeps its connection — see the module docs.
    let phase = Instant::now();
    if let Some(streams) = apps.streams() {
        match streams.reload(&catalog).await {
            Ok(changed) => report.streams_changed = changed,
            Err(e) => report.streams_error = Some(e.to_string()),
        }
    }
    report.streams = phase.elapsed();

    report.total = started.elapsed();
    report
}

/// Re-mount each stored application from disk, and unmount the ones whose rows
/// have gone.
///
/// A **failure is per-app**, the same rule [`mount_all`](crate::mount_all)
/// follows: one application whose output directory is missing must not take the
/// others down, and it keeps serving what it was serving.
async fn reload_applications(
    apps: &AppMounts,
    stored: Vec<Application>,
    report: &mut ReloadReport,
) {
    let mounted_before = apps.subdomains();
    let mut present: Vec<String> = Vec::with_capacity(stored.len());

    for app in stored {
        let subdomain = app.subdomain.clone();
        present.push(subdomain.clone());
        match crate::apps::mount_from_disk(apps, app).await {
            Ok(assets) => {
                report.assets += assets;
                report.reloaded.push(subdomain);
            }
            Err(e) => report.failed.push((subdomain, e.to_string())),
        }
    }

    // An application deleted from another process is still mounted here. Only
    // what *was* mounted is considered, so an app that merely failed to reload
    // above is untouched.
    for gone in mounted_before {
        if !present.contains(&gone) && apps.unmount(&gone) {
            report.unmounted.push(gone);
        }
    }
}

/// Listen for `SIGHUP` for the lifetime of the process, reloading on each one.
///
/// A **signal** rather than an endpoint, because of who sends it: the caller is a
/// shell — a developer's, or a coding agent's — in the application's project
/// directory, right after `npm run build`. It has no session, no CSRF token and
/// no reason to acquire either, but it is on the machine and it can run `pkill
/// -HUP feldspar`. (The admin API's Build button is still there and still builds;
/// this is the half that skips the bundler.)
///
/// Reloads are **serialised by the listener**: a signal that arrives while one is
/// running is coalesced by the kernel into the next `recv`, so a burst of them
/// cannot start overlapping reloads.
#[cfg(unix)]
pub fn spawn_sighup_reload(apps: Arc<AppMounts>) {
    use tokio::signal::unix::{SignalKind, signal};

    // Registered here and **not** inside the spawned task, which is not a
    // stylistic choice: `SIGHUP`'s default disposition is to *terminate*, so a
    // signal arriving before the task first ran would kill the process instead
    // of reloading it. Registering synchronously closes that window — this
    // function returns with the handler installed. It also means a registration
    // failure is reported at boot rather than swallowed by a detached task.
    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(hangup) => hangup,
        // Not fatal: the server serves, it just cannot be reloaded without a
        // restart, and the operator should know which of those they are in.
        Err(e) => {
            eprintln!("feldspar: SIGHUP reloading is unavailable: {e}");
            return;
        }
    };
    // Said out loud once at boot, because a capability nobody knows about is not
    // one: the operator (or the agent reading this terminal) now knows there is
    // something between "edit" and "restart".
    eprintln!(
        "feldspar: SIGHUP reloads the catalog, the applications and changed modules in \
         place — `pkill -HUP feldspar` after a build, no restart needed"
    );
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            eprintln!("feldspar: SIGHUP — reloading the catalog and the applications");
            reload_all(&apps).await.log();
        }
    });
}

/// No `SIGHUP` off Unix; the admin UI's Build button is the whole story there.
#[cfg(not(unix))]
pub fn spawn_sighup_reload(_apps: Arc<AppMounts>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_admin_only_server_has_nothing_to_reload() {
        // No catalog means no tables and no apps — and, crucially, not an error:
        // the signal handler is installed on every server, including this one.
        let report = reload_all(&AppMounts::none()).await;
        assert!(report.is_ok());
        assert_eq!(report.tables, 0);
        assert!(report.reloaded.is_empty());
    }

    #[test]
    fn timings_are_reported_in_milliseconds() {
        assert_eq!(ms(Duration::from_micros(1500)), "1.5ms");
    }
}
