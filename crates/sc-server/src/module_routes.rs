//! v1 plugin **routes** over HTTP: `GET`/`POST`
//! `/_feldspar/routes/<module>/<path>`, on the admin host only
//! ([`sc_module::routes`] says why there).
//!
//! The request is served as the person making it, so before the module is
//! called this decides who that is:
//!
//! - an `Authorization: Bearer` API token, when there is one — what a monitor
//!   presents; a token that does not authenticate is a `401`, not the public;
//! - else the admin host's session cookie;
//! - else nobody — and nobody is also what a cookie-less `POST` to a `noCsrf`
//!   route is, since the CSRF layer passes it on only as the anonymous caller.
//!
//! A route that declares `apiToken` refuses nobody with a `401`, as v1's
//! middleware does. The CSRF check itself is the server's own, before any of
//! this: a `POST` carries the token unless the route says `noCsrf`
//! ([`no_csrf_route`]).
//!
//! It is bounded the way a webhook is — a body cap, a per-address rate limit,
//! and the module call's own wall clock — and a route that fails is a `500`
//! and the `error` event.
//!
//! Also here: [`SystemFacts`], what a route reads as `systeminformation`,
//! `EventLog` and `MetaData`, and the startup record those startups come from.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use chrono::{DateTime, Utc};
use sc_auth::ROLE_PUBLIC;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_module::{RouteCaller, RouteRequest, WEBHOOK_MAX_BODY, WebhookRequest};
use serde_json::{Value as Json, json};

use crate::backup::schedule::{Frequency, load_schedules, load_statuses};
use crate::modules::RouteLookup;
use crate::router::{
    AppState, Resolved, json_error, parse_query, request_client_ip, resolve_app, session_user,
};
use crate::security::AnonymousCaller;

/// The route.
pub const ROUTE: &str = "/_feldspar/routes/{module}/{*path}";

/// The headers a module is not handed: the session's cookie and the
/// credential are the server's to read, and it has read them.
const WITHHELD: &[&str] = &["cookie", "authorization", "x-csrf-token", "csrf-token"];

/// `GET` or `POST` `/_feldspar/routes/{module}/{*path}`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn route(
    State(state): State<AppState>,
    AxumPath((module, path)): AxumPath<(String, String)>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    anonymous: Option<Extension<AnonymousCaller>>,
    jar: CookieJar,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The admin host's, and no application's: an application's host is its
    // own, and a module installed for every application claims nothing there.
    if !matches!(resolve_app(&state, &headers, &jar), Resolved::Admin) {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    let ip = request_client_ip(&state, peer.map(|p| p.0.0), &headers);
    if let Some(ip) = &ip
        && let Err(limited) =
            sc_ratelimit::global().hit(&format!("module-route:{ip}"), crate::webhooks::WEBHOOK_RATE)
    {
        let secs = limited.retry_after_secs();
        let mut response = json_error(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many requests from this address; retry in {secs} s"),
        );
        if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    if body.len() > WEBHOOK_MAX_BODY {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("a module route's body is at most {WEBHOOK_MAX_BODY} bytes"),
        );
    }
    let Some(modules) = state.apps.modules() else {
        return json_error(StatusCode::NOT_FOUND, "this server runs no modules");
    };
    let route_path = format!("/{path}");
    let declared = match modules.route_lookup(&module, method.as_str(), &route_path) {
        Ok(RouteLookup::Found(declared)) => declared,
        Ok(RouteLookup::NotFound) => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("no module here serves `{module}{route_path}`"),
            );
        }
        Ok(RouteLookup::MethodNotAllowed(allowed)) => {
            let allow = allowed.join(", ");
            let mut response = json_error(
                StatusCode::METHOD_NOT_ALLOWED,
                format!("`{module}{route_path}` answers {allow}"),
            );
            if let Ok(value) = HeaderValue::from_str(&allow) {
                response.headers_mut().insert(header::ALLOW, value);
            }
            return response;
        }
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };

    let caller = match route_caller(&state, &jar, &headers, anonymous.is_some()).await {
        Ok(caller) => caller,
        Err(refused) => return *refused,
    };
    if declared.api_token && caller.user.is_none() {
        return json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }

    let request = RouteRequest {
        request: WebhookRequest {
            method: method.as_str().to_owned(),
            headers: headers
                .iter()
                .filter(|(k, _)| !WITHHELD.contains(&k.as_str()))
                .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
                .collect(),
            query: parse_query(&uri),
            rest: path,
            body: body.to_vec(),
        },
        ip: ip.map(|ip| ip.to_string()),
        protocol: if state.secure_cookies {
            "https"
        } else {
            "http"
        }
        .to_owned(),
    };
    match modules.route(&module, request, &caller).await {
        Ok(Some(answer)) => {
            let mut response = Response::new(Body::from(answer.body));
            *response.status_mut() =
                StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            for (k, v) in answer.headers {
                if let (Ok(k), Ok(v)) = (HeaderName::try_from(k), HeaderValue::from_str(&v)) {
                    response.headers_mut().insert(k, v);
                }
            }
            response
        }
        Ok(None) => json_error(
            StatusCode::NOT_FOUND,
            format!("no module here serves `{module}{route_path}`"),
        ),
        Err(err) => {
            sc_log::log_error!(
                "feldspar: the route `{module}{route_path}` failed: {}",
                err.chain()
            );
            if let (Some(triggers), Some(catalog)) = (state.apps.triggers(), state.apps.catalog()) {
                let event = sc_action::Event::error(
                    err.kind(),
                    err.to_string(),
                    method.as_str(),
                    uri.path(),
                );
                triggers.fire(catalog, &event).await;
            }
            json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        }
    }
    .into_response()
}

/// Who is calling: the API token's user, else the session's, else nobody.
async fn route_caller(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    anonymous: bool,
) -> std::result::Result<RouteCaller, Box<Response>> {
    let nobody = RouteCaller {
        role: ROLE_PUBLIC,
        user: None,
    };
    if anonymous {
        return Ok(nobody);
    }
    if let Some(token) = crate::security::bearer_token(headers) {
        let Some(catalog) = state.apps.catalog() else {
            return Err(Box::new(json_error(
                StatusCode::UNAUTHORIZED,
                "this server has no API tokens",
            )));
        };
        return match sc_auth::authenticate_route_token(catalog, &token).await {
            Ok(caller) => Ok(caller_of(&caller.user)),
            Err(e) => Err(Box::new(json_error(
                StatusCode::UNAUTHORIZED,
                e.to_string(),
            ))),
        };
    }
    Ok(session_user(state, jar)
        .await?
        .map_or(nobody, |user| caller_of(&user)))
}

/// A signed-in user as a route's caller: the role, and the row a write's
/// caller carries — its columns, with `id` and `role`.
fn caller_of(user: &sc_auth::User) -> RouteCaller {
    let mut fields: serde_json::Map<String, Json> = user
        .extra
        .iter()
        .map(|(k, v)| (k.clone(), sc_types::value_to_json(v)))
        .collect();
    fields.insert("id".to_owned(), json!(user.id));
    fields.insert("role".to_owned(), json!(user.role));
    RouteCaller {
        role: user.role,
        user: Some(Json::Object(fields)),
    }
}

/// Whether `request` is a `POST` to a module route that declared `noCsrf` —
/// which the CSRF layer then lets through as the anonymous caller, as it does
/// a webhook.
pub(crate) fn no_csrf_route(state: &AppState, request: &axum::extract::Request) -> bool {
    let Some(rest) = request
        .uri()
        .path()
        .strip_prefix(sc_module::ROUTES_PREFIX)
        .and_then(|p| p.strip_prefix('/'))
    else {
        return false;
    };
    let Some((module, path)) = rest.split_once('/') else {
        return false;
    };
    let Some(modules) = state.apps.modules() else {
        return false;
    };
    matches!(
        modules.route_lookup(module, request.method().as_str(), &format!("/{path}")),
        Ok(RouteLookup::Found(route)) if route.no_csrf
    )
}

/// The installation-wide settings a route's `getState().getConfig` answers,
/// by v1's names: `auto_backup_frequency`, from the most frequent automated
/// backup (`Never` when there is none).
pub(crate) async fn installation(catalog: &Catalog) -> Json {
    let schedules = load_schedules(catalog).await.unwrap_or_default();
    let most_frequent = schedules
        .iter()
        .map(|s| match s.frequency {
            Frequency::Daily => (0, "Daily"),
            Frequency::Weekly => (1, "Weekly"),
            Frequency::Monthly => (2, "Monthly"),
        })
        .min()
        .map_or("Never", |(_, name)| name);
    json!({ "config": { "auto_backup_frequency": most_frequent } })
}

/// Record that the server has started, keeping the last
/// [`STARTUP_HISTORY_DAYS`](sc_config::STARTUP_HISTORY_DAYS) days.
pub(crate) async fn record_startup(catalog: &Catalog, now: DateTime<Utc>) -> Result<()> {
    let mut startups = startups(catalog).await?;
    let horizon = now - chrono::Duration::days(sc_config::STARTUP_HISTORY_DAYS);
    startups.retain(|t| *t >= horizon);
    startups.push(now);
    let list: Vec<Json> = startups.iter().map(|t| json!(t.to_rfc3339())).collect();
    sc_config::set_config(catalog, sc_config::STARTUP_HISTORY, Json::Array(list)).await
}

/// When the server started, oldest first.
async fn startups(catalog: &Catalog) -> Result<Vec<DateTime<Utc>>> {
    let stored = sc_config::stored_config(catalog, sc_config::STARTUP_HISTORY).await?;
    Ok(stored
        .as_ref()
        .and_then(Json::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Json::as_str)
                .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc))
                .collect()
        })
        .unwrap_or_default())
}

/// The `system` ask surface of a route's call.
pub(crate) struct SystemFacts<'a> {
    pub catalog: &'a Catalog,
    /// Whose filesystem `fsSize` reports: the server's data directory.
    pub data_dir: &'a Path,
}

#[async_trait::async_trait]
impl sc_module::SystemAsks for SystemFacts<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        match plan.get("op").and_then(Json::as_str).unwrap_or_default() {
            "fs_size" => fs_size(self.data_dir),
            "mem" => mem(),
            "current_load" => current_load().await,
            "events" => Ok(Json::Array(
                startups(self.catalog)
                    .await?
                    .into_iter()
                    .map(|t| {
                        json!({
                            "event_type": "Startup",
                            "channel": null,
                            "occur_at": t.timestamp_millis(),
                        })
                    })
                    .collect(),
            )),
            "metadata" => {
                let statuses = load_statuses(self.catalog).await?;
                Ok(Json::Array(
                    statuses
                        .values()
                        .filter_map(|status| {
                            Some(json!({
                                "type": "Backup",
                                "name": "Success",
                                "written_at": status.last_success_at?.to_rfc3339(),
                                "body": { "file": status.last_file },
                                "user_id": null,
                            }))
                        })
                        .collect(),
                ))
            }
            other => Err(Error::config(format!(
                "`{other}` is not something a route can read of this server; it reads \
                 `fs_size`, `mem` and `current_load` (`systeminformation`), `events` \
                 (`EventLog`) and `metadata` (`MetaData`)"
            ))),
        }
    }
}

/// `systeminformation`'s `fsSize()`: one entry, the filesystem `dir` is on.
#[cfg(unix)]
fn fs_size(dir: &Path) -> Result<Json> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| Error::invalid("the data directory's path has a NUL in it"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and
    // `stat` is written by `statvfs` before it is read, which only happens when
    // the call succeeded.
    let failed = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0;
    if failed {
        return Err(Error::config(format!(
            "the size of the filesystem holding {} could not be read: {}",
            dir.display(),
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `statvfs` succeeded, so it filled `stat`.
    let stat = unsafe { stat.assume_init() };
    // The field types differ between platforms, hence the casts.
    #[allow(clippy::unnecessary_cast)]
    let block = (stat.f_frsize as u64).max(1);
    #[allow(clippy::unnecessary_cast)]
    let (blocks, free, available) = (
        stat.f_blocks as u64 * block,
        stat.f_bfree as u64 * block,
        stat.f_bavail as u64 * block,
    );
    let used = blocks.saturating_sub(free);
    // `df`'s `Use%`: used over what a user could have, which excludes the
    // blocks reserved for root.
    let usable = used + available;
    let percent = if usable == 0 {
        0.0
    } else {
        (used as f64 / usable as f64 * 10000.0).round() / 100.0
    };
    Ok(json!([{
        "fs": dir.display().to_string(),
        "type": "",
        "size": blocks,
        "used": used,
        "available": available,
        "use": percent,
        "mount": dir.display().to_string(),
        "rw": true,
    }]))
}

#[cfg(not(unix))]
fn fs_size(_dir: &Path) -> Result<Json> {
    Err(Error::config("filesystem sizes are read on Unix only"))
}

/// `/proc/meminfo`'s fields, in bytes.
fn meminfo() -> Result<std::collections::HashMap<String, u64>> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|e| Error::config(format!("the memory in use could not be read: {e}")))?;
    Ok(parse_meminfo(&text))
}

fn parse_meminfo(text: &str) -> std::collections::HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((key.trim().to_owned(), kb * 1024))
        })
        .collect()
}

/// `systeminformation`'s `mem()`, from `/proc/meminfo` as it computes it:
/// `active` is what is in use, `total - available`.
fn mem() -> Result<Json> {
    Ok(mem_of(&meminfo()?))
}

fn mem_of(info: &std::collections::HashMap<String, u64>) -> Json {
    let get = |key: &str| info.get(key).copied().unwrap_or(0);
    let total = get("MemTotal");
    let free = get("MemFree");
    let available = info.get("MemAvailable").copied().unwrap_or(free);
    let buffers = get("Buffers");
    let cached = get("Cached");
    let slab = get("Slab");
    let swaptotal = get("SwapTotal");
    let swapfree = get("SwapFree");
    json!({
        "total": total,
        "free": free,
        "used": total.saturating_sub(free),
        "active": total.saturating_sub(available),
        "available": available,
        "buffers": buffers,
        "cached": cached,
        "slab": slab,
        "buffcache": buffers + cached + slab,
        "swaptotal": swaptotal,
        "swapused": swaptotal.saturating_sub(swapfree),
        "swapfree": swapfree,
    })
}

/// The CPU's cumulative time, from `/proc/stat`'s first line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    irq: u64,
    total: u64,
}

fn parse_cpu(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let n: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let at = |i: usize| n.get(i).copied().unwrap_or(0);
    // user nice system idle iowait irq softirq steal; guest time is already
    // inside user, so it is not counted twice.
    Some(CpuTimes {
        user: at(0),
        nice: at(1),
        system: at(2),
        idle: at(3) + at(4),
        irq: at(5) + at(6),
        total: (0..8).map(at).sum(),
    })
}

fn cpu_times() -> Result<CpuTimes> {
    let text = std::fs::read_to_string("/proc/stat")
        .map_err(|e| Error::config(format!("the CPU's load could not be read: {e}")))?;
    parse_cpu(&text).ok_or_else(|| Error::config("/proc/stat has no `cpu` line"))
}

/// The last sample `current_load` took, so the next measures from it — as
/// `systeminformation` measures from its previous call.
static LAST_CPU: Mutex<Option<(Instant, CpuTimes)>> = Mutex::new(None);

/// How long a fresh measurement waits between its two samples, when there is
/// no earlier sample old enough to measure from.
const CPU_SAMPLE: Duration = Duration::from_millis(250);

/// `systeminformation`'s `currentLoad()`: the share of CPU time spent busy
/// since the last call (or over a quarter of a second, for the first).
async fn current_load() -> Result<Json> {
    let previous = LAST_CPU
        .lock()
        .ok()
        .and_then(|last| *last)
        .filter(|(at, _)| at.elapsed() >= CPU_SAMPLE)
        .map(|(_, times)| times);
    let before = match previous {
        Some(times) => times,
        None => {
            let first = cpu_times()?;
            tokio::time::sleep(CPU_SAMPLE).await;
            first
        }
    };
    let now = cpu_times()?;
    if let Ok(mut last) = LAST_CPU.lock() {
        *last = Some((Instant::now(), now));
    }
    let load_avg = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    Ok(load_between(before, now, load_avg))
}

fn load_between(before: CpuTimes, now: CpuTimes, load_avg: f64) -> Json {
    let total = now.total.saturating_sub(before.total);
    let share = |a: u64, b: u64| {
        if total == 0 {
            0.0
        } else {
            (a.saturating_sub(b) as f64 / total as f64 * 10000.0).round() / 100.0
        }
    };
    let idle = share(now.idle, before.idle);
    json!({
        "avgLoad": load_avg,
        "currentLoad": if total == 0 { 0.0 } else { ((100.0 - idle) * 100.0).round() / 100.0 },
        "currentLoadUser": share(now.user, before.user),
        "currentLoadSystem": share(now.system, before.system),
        "currentLoadNice": share(now.nice, before.nice),
        "currentLoadIdle": idle,
        "currentLoadIrq": share(now.irq, before.irq),
        "cpus": [],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_in_use_is_total_less_available() {
        let info = parse_meminfo(
            "MemTotal:       1000 kB\nMemFree:         100 kB\nMemAvailable:    400 kB\n\
             Buffers:          10 kB\nCached:          200 kB\nSwapTotal:        50 kB\n\
             SwapFree:         20 kB\n",
        );
        let mem = mem_of(&info);
        assert_eq!(mem["total"], json!(1_024_000));
        assert_eq!(mem["active"], json!(600 * 1024));
        assert_eq!(mem["swapused"], json!(30 * 1024));
    }

    #[test]
    fn cpu_load_is_the_busy_share_between_two_samples() {
        let before = parse_cpu("cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 2 3\n").unwrap();
        let after = parse_cpu("cpu  160 0 140 900 0 0 0 0 0 0\n").unwrap();
        let load = load_between(before, after, 0.5);
        assert_eq!(load["currentLoad"], json!(50.0));
        assert_eq!(load["currentLoadUser"], json!(30.0));
        assert_eq!(load["currentLoadIdle"], json!(50.0));
        assert_eq!(load["avgLoad"], json!(0.5));
    }

    #[test]
    fn the_data_directorys_filesystem_has_a_size() {
        let sizes = fs_size(Path::new("/")).unwrap();
        let first = &sizes[0];
        assert!(first["size"].as_u64().unwrap() > 0);
        assert!(first["used"].as_u64().unwrap() <= first["size"].as_u64().unwrap());
    }
}
