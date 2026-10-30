//! Settings → System: which feldspar this is, and what it is doing to the
//! machine it runs on.
//!
//! It exists because "which version is installed?" had no answer short of
//! asking whoever deployed it. The build identity is compiled in by
//! `scripts/build-static.sh`, which sets two environment variables for the
//! `cargo build` it runs:
//!
//! - `SC_BUILD_TIME` — when the script ran, in UTC (RFC 3339);
//! - `SC_BUILD_COMMIT` — the full git commit it built, with `-dirty` appended
//!   when the checkout had edits in it.
//!
//! A binary built any other way (`cargo build`, `cargo test`) has neither, and
//! the screen says it is not a release build rather than inventing a date. The
//! version is the workspace's (`[workspace.package] version` in the root
//! `Cargo.toml`), which every crate inherits and the release tarball is already
//! named after.
//!
//! The readings — memory, CPU, disk, uptime — are read from `/proc` and
//! `statvfs` on each request. Nothing is sampled in the background: the tab is
//! opened rarely, and a reading taken when it is opened is the one wanted. On a
//! platform without `/proc` the readings are absent and the screen says so.
//!
//! **Restart** exits the process with [`RESTART_EXIT_CODE`] after the answer
//! has gone out. The process does not restart itself, and does not try to find
//! out whether anything will: there are too many service managers to recognise
//! them all, and seeing one does not say whether it restarts. The systemd unit
//! `feldspar setup` writes says `Restart=on-failure`, which a non-zero exit
//! triggers, and the Docker compose file says `restart: unless-stopped`. With
//! nothing managing the process, the server simply stops.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use sc_catalog::Catalog;
use serde_json::{Value as Json, json};

use crate::module_routes::parse_meminfo;

/// The workspace version this binary was built from.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// When `scripts/build-static.sh` built this binary (UTC, RFC 3339), or `None`
/// for a binary built any other way.
pub fn build_time() -> Option<&'static str> {
    stamped(option_env!("SC_BUILD_TIME"))
}

/// The git commit `scripts/build-static.sh` built, `-dirty` when the checkout
/// had edits, or `None` for a binary built any other way.
pub fn build_commit() -> Option<&'static str> {
    stamped(option_env!("SC_BUILD_COMMIT"))
}

/// A build variable, where an empty one — the Dockerfile passes its build
/// arguments through whether or not they were given — is no value.
fn stamped(value: Option<&'static str>) -> Option<&'static str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// The exit code a requested restart leaves with: `EX_TEMPFAIL`, "try again".
/// Non-zero, because `Restart=on-failure` does not restart a process that
/// exited 0 — a clean exit is the operator's `systemctl stop`.
pub const RESTART_EXIT_CODE: i32 = 75;

/// How long a restart waits before exiting, so the answer saying it will
/// reaches the browser first.
const RESTART_DELAY: Duration = Duration::from_millis(500);

/// How long the CPU reading watches the process for.
const CPU_SAMPLE: Duration = Duration::from_millis(250);

static STARTED: OnceLock<SystemTime> = OnceLock::new();

/// When this process started.
///
/// On Linux, the kernel's own record (`/proc/self/stat`), so the boot before
/// the listener opens — which can be minutes when applications are built — is
/// counted. Elsewhere, the first time this is asked, which [`serve`](crate::serve)
/// does as it starts.
pub fn started_at() -> SystemTime {
    *STARTED.get_or_init(|| process_start().unwrap_or_else(SystemTime::now))
}

/// The process's start from `/proc`: boot time (`btime` in `/proc/stat`) plus
/// the clock ticks after boot at which the process started (field 22 of
/// `/proc/self/stat`).
fn process_start() -> Option<SystemTime> {
    let boot: u64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok())?;
    let ticks = parse_start_ticks(&std::fs::read_to_string("/proc/self/stat").ok()?)?;
    // SAFETY: `sysconf` reads a constant and has no preconditions.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let per_second = u64::try_from(per_second).ok().filter(|&t| t > 0)?;
    let after_boot = Duration::from_millis(ticks * 1000 / per_second);
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(boot) + after_boot)
}

/// Everything Settings → System shows, as the `getSystemInfo` endpoint
/// returns it.
pub async fn system_info(catalog: &Catalog) -> Json {
    let started_at = started_at();
    let uptime = SystemTime::now()
        .duration_since(started_at)
        .unwrap_or_default();
    let database = match catalog.primary().server_version().await {
        Ok(found) => json!({ "product": found.product, "version": found.version, "error": null }),
        Err(e) => json!({ "product": null, "version": null, "error": e.to_string() }),
    };
    let data_dir = sc_files::data_dir().ok();
    json!({
        "version": VERSION,
        "build_time": build_time(),
        "commit": build_commit(),
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "pid": std::process::id(),
        "started_at": chrono::DateTime::<chrono::Utc>::from(started_at).to_rfc3339(),
        "uptime_seconds": uptime.as_secs(),
        "database": database,
        "memory": memory(),
        "cpu": cpu().await,
        "disk": data_dir.as_deref().and_then(disk),
    })
}

/// Exit the process shortly, for whatever manages it to start it again.
///
/// The coding agents' managed processes are stopped first, as on a shutdown,
/// so none outlives the server it belonged to.
pub fn schedule_restart() {
    tokio::spawn(async {
        tokio::time::sleep(RESTART_DELAY).await;
        sc_log::log_info!("restarting at an administrator's request");
        sc_core_traits::kill_all_processes();
        std::process::exit(RESTART_EXIT_CODE);
    });
}

/// The process's resident memory and the machine's, in bytes. Both files are
/// `Name:   1234 kB` lines, which `parse_meminfo` reads.
fn memory() -> Option<Json> {
    let status = parse_meminfo(&std::fs::read_to_string("/proc/self/status").ok()?);
    let meminfo = parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?);
    Some(json!({
        "process_bytes": status.get("VmRSS"),
        "total_bytes": meminfo.get("MemTotal"),
        "available_bytes": meminfo.get("MemAvailable"),
    }))
}

/// CPU use over a short sample, as a percentage of the whole machine (every
/// core), for this process and for everything; plus the load average.
async fn cpu() -> Option<Json> {
    let before = cpu_ticks()?;
    tokio::time::sleep(CPU_SAMPLE).await;
    let after = cpu_ticks()?;
    let elapsed = after.total.saturating_sub(before.total);
    let percent = |ticks: u64| {
        if elapsed == 0 {
            0.0
        } else {
            (ticks as f64 * 1000.0 / elapsed as f64).round() / 10.0
        }
    };
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|text| parse_loadavg(&text));
    Some(json!({
        "process_percent": percent(after.process.saturating_sub(before.process)),
        "system_percent": percent(elapsed.saturating_sub(after.idle.saturating_sub(before.idle))),
        "cores": cores,
        "load_average": load.unwrap_or_default(),
    }))
}

/// The filesystem holding the data directory: its size and what is free.
fn disk(dir: &Path) -> Option<Json> {
    // A data directory not made yet is on the filesystem its nearest existing
    // ancestor is on.
    let existing = dir.ancestors().find(|p| p.exists())?;
    let size = filesystem_size(existing).ok()?;
    Some(json!({
        "path": dir.display().to_string(),
        "total_bytes": size.total,
        "available_bytes": size.available,
    }))
}

/// The size of a filesystem, in bytes, as `statvfs` reports it.
pub(crate) struct FsSize {
    pub total: u64,
    pub free: u64,
    /// What an unprivileged user can still write: `free` less the blocks
    /// reserved for root.
    pub available: u64,
}

/// `statvfs` on the filesystem `dir` is on.
#[cfg(unix)]
pub(crate) fn filesystem_size(dir: &Path) -> std::io::Result<FsSize> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "the path has a NUL in it")
    })?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and
    // `stat` is written by `statvfs` before it is read, which only happens when
    // the call succeeded.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `statvfs` succeeded, so it filled `stat`.
    let stat = unsafe { stat.assume_init() };
    // The field types differ between platforms, hence the casts.
    #[allow(clippy::unnecessary_cast)]
    let block = (stat.f_frsize as u64).max(1);
    #[allow(clippy::unnecessary_cast)]
    Ok(FsSize {
        total: stat.f_blocks as u64 * block,
        free: stat.f_bfree as u64 * block,
        available: stat.f_bavail as u64 * block,
    })
}

#[cfg(not(unix))]
pub(crate) fn filesystem_size(_dir: &Path) -> std::io::Result<FsSize> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "filesystem sizes are read on Unix only",
    ))
}

/// Clock ticks: this process's (user + system), the machine's in total, and
/// the machine's idle ones.
struct CpuTicks {
    process: u64,
    total: u64,
    idle: u64,
}

fn cpu_ticks() -> Option<CpuTicks> {
    let process = parse_process_ticks(&std::fs::read_to_string("/proc/self/stat").ok()?)?;
    let (total, idle) = parse_system_ticks(&std::fs::read_to_string("/proc/stat").ok()?)?;
    Some(CpuTicks {
        process,
        total,
        idle,
    })
}

/// `utime + stime`, fields 14 and 15 of `/proc/<pid>/stat`.
fn parse_process_ticks(stat: &str) -> Option<u64> {
    Some(stat_field(stat, 14)? + stat_field(stat, 15)?)
}

/// `starttime`, the 22nd field of `/proc/<pid>/stat`: clock ticks after boot.
fn parse_start_ticks(stat: &str) -> Option<u64> {
    stat_field(stat, 22)
}

/// The numeric field `n` (1-based, as proc(5) numbers them) of
/// `/proc/<pid>/stat`, counted from the end of the command name.
fn stat_field(stat: &str, n: usize) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // Field 3 is the first after the `)`.
    rest.split_whitespace().nth(n.checked_sub(3)?)?.parse().ok()
}

/// The aggregate `cpu` line of `/proc/stat`: the total of user, nice, system,
/// idle, iowait, irq, softirq and steal (guest time is already inside user),
/// and the idle part of it (idle + iowait).
fn parse_system_ticks(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|v| v.parse().ok())
        .collect::<Option<_>>()?;
    let idle = values.get(3)? + values.get(4).copied().unwrap_or(0);
    Some((values.iter().sum(), idle))
}

/// The 1, 5 and 15 minute load averages.
fn parse_loadavg(text: &str) -> Vec<f64> {
    text.split_whitespace()
        .take(3)
        .filter_map(|v| v.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_are_counted_from_the_last_parenthesis() {
        // A command name with a space and a `)` in it would shift every field
        // for a parser that split from the start.
        let stat = "4242 (my (odd) proc) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    700 300 0 0 20 0 8 0 12345 1000 200";
        assert_eq!(parse_process_ticks(stat), Some(1000));
        assert_eq!(parse_start_ticks(stat), Some(12345));
    }

    #[test]
    fn system_ticks_leave_out_guest_time() {
        let stat = "cpu  100 5 50 800 20 3 2 1 40 0\ncpu0 50 2 25 400 10 1 1 0 20 0\n";
        // 100+5+50+800+20+3+2+1, guest (40) is already inside user.
        assert_eq!(parse_system_ticks(stat), Some((981, 820)));
    }

    #[test]
    fn an_empty_build_variable_is_no_value() {
        assert_eq!(stamped(None), None);
        assert_eq!(stamped(Some("")), None);
        assert_eq!(stamped(Some("abc123")), Some("abc123"));
    }

    #[test]
    fn load_average_is_the_first_three_numbers() {
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 2/1234 98765\n"),
            vec![0.52, 0.58, 0.59]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_machine_can_be_read() {
        assert!(memory().is_some_and(|m| m["process_bytes"].as_u64().is_some_and(|b| b > 0)));
        let ticks = cpu_ticks().expect("readable /proc");
        assert!(ticks.total >= ticks.idle);
        let started = started_at();
        assert!(started <= SystemTime::now());
        assert!(
            SystemTime::now().duration_since(started).unwrap() < Duration::from_secs(24 * 3600)
        );
        let disk = disk(&std::env::temp_dir().join("not-made-yet")).expect("statvfs");
        assert!(disk["total_bytes"].as_u64().is_some_and(|t| t > 0));
    }
}
