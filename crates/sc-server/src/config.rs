//! Server configuration (technical design §16).
//!
//! [`ServerConfig`] is the small, plain settings value the CLI builds and hands
//! to [`serve`](crate::serve): where to bind, where the built `ui/admin` bundle
//! lives, session lifetime, and whether cookies carry the `Secure` attribute.
//! [`ServerConfig::from_args`] parses the handful of flags the `feldspar serve`
//! command accepts, so the binary needs no argument-parsing dependency.

use std::net::SocketAddr;
use std::path::PathBuf;

use sc_error::{Error, Result};

use crate::tls::TlsSettings;

/// What `--python` was set to: whether this process starts the interpreter it
/// has (§7's second switch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PythonMode {
    /// The default: start the interpreter when something first needs it, and not
    /// before. A server that fires no Python body and loads no Python module
    /// never starts one, exactly as the code isolate pool is never built.
    #[default]
    Auto,
    /// Do not start one at all. For an operator who has a Python-capable binary
    /// and wants this deployment not to run Python — the Python trigger then
    /// fails naming the flag.
    Off,
}

/// Default address the server binds when `--bind` is not given.
pub const DEFAULT_BIND: &str = "127.0.0.1:3032";

/// The base domain of a server that was given none and listens on this machine
/// only: an app `blog` is then served at `blog.localhost:<port>`.
pub const LOOPBACK_BASE_DOMAIN: &str = "localhost";

/// The base domain to use when none is configured: `localhost` for a server
/// that listens on this machine only, so its apps are served at
/// `<subdomain>.localhost` (browsers and the iOS simulator resolve these).
/// None for a server reachable from elsewhere or using TLS: it must be told its
/// domain, and `localhost` gets no certificate.
pub fn implied_base_domain(addr: &SocketAddr, tls: bool) -> Option<&'static str> {
    (addr.ip().is_loopback() && !tls).then_some(LOOPBACK_BASE_DOMAIN)
}

/// Runtime configuration for the HTTP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The socket address to bind.
    pub addr: SocketAddr,
    /// Directory holding the built `ui/admin` SPA bundle, if any. When unset the
    /// server still serves the minimal bootstrap document so the SPA can boot
    /// once built.
    pub static_dir: Option<PathBuf>,
    /// Directory holding the built `ui/ide` bundle — the file-store IDE, served
    /// under `/ide/` (design §12.1).
    ///
    /// **Not a command-line setting.** The IDE is part of the admin UI as far as
    /// an operator is concerned: it is built with it and served with it, so there
    /// is nothing to decide and no flag to forget. The binary fills this in from
    /// the bundle it was built with, or from the checkout it was built in (see
    /// `sc-cli`); a test points it at a directory of its own.
    pub ide_dir: Option<PathBuf>,
    /// Directory holding the built `ui/analytics` bundle — the Analytics UI,
    /// served admin-only under `/analytics/` (analytics TODO A1.14).
    ///
    /// **Not a command-line setting**, for the reason `ide_dir` is not. `None`
    /// is a server whose `/analytics/` says the bundle is not built.
    pub analytics_dir: Option<PathBuf>,
    /// Directory holding the built `ui/saltcorn-ui` bundle — Saltcorn UI's view
    /// runtime (`view-runtime.js`) and the browser assets its pages load (TODO
    /// "Saltcorn UI" §9).
    ///
    /// **Not a command-line setting**, for the reason `ide_dir` is not. The
    /// binary fills it in from the directory its build recorded, and a build with
    /// `SC_BUILD_ADMIN=0` records none: `None` is a server with no Saltcorn UI,
    /// where an application whose framework is `saltcorn-ui` fails to mount with
    /// a sentence naming the missing bundle.
    pub saltcorn_ui_dir: Option<PathBuf>,
    /// Directory holding the built `ui/builder` bundle: v1's layout builder,
    /// served under `/builder/` (TODO "The builder" §2).
    ///
    /// **Not a command-line setting**, for the reason `ide_dir` is not. A build
    /// with `SC_BUILD_ADMIN=0` records none, and `None` is a server whose builder
    /// routes answer a page saying the builder is not built.
    pub builder_dir: Option<PathBuf>,
    /// Directory holding the **bundled modules** — `plugins/`, the modules this
    /// server ships with and can install from itself (`sc_module::bundled`).
    ///
    /// **Not a command-line setting**, for the same reason `ide_dir` is not: it
    /// is part of the artifact rather than a choice about this host. The binary
    /// fills it in from the tree it was packaged into, or from the checkout it
    /// was built in (see `sc-cli`); a test points it at a directory of its own.
    /// `None` means the checkout's `plugins/`, and a server that finds nothing
    /// there simply offers an empty catalog.
    pub plugins_dir: Option<PathBuf>,
    /// Session lifetime in hours.
    pub session_ttl_hours: i64,
    /// Whether the session/CSRF cookies carry the `Secure` attribute (set behind
    /// TLS; off for plain-HTTP local development).
    pub secure_cookies: bool,
    /// The domain applications are served under: an app with subdomain `blog` is
    /// served at `blog.<base_domain>` (design §13.2).
    ///
    /// `None` (the default) disables subdomain app routing entirely, so every
    /// request reaches the admin. App routing is opt-in because without a base
    /// domain to anchor it, a request's own `Host` header would choose its app.
    /// `serve` fills it in with [`implied_base_domain`] for a loopback-only
    /// server that was given none.
    pub base_domain: Option<String>,
    /// Further domains the same applications answer under
    /// (`--extra-base-domain`, repeatable): `blog.<extra>` reaches the app `blog`
    /// as `blog.<base_domain>` does.
    ///
    /// For reaching a development server from somewhere `localhost` does not
    /// mean this machine — `10.0.2.2.nip.io` from an Android emulator,
    /// `192.168.1.50.nip.io` from a phone on the LAN. Only routing reads these:
    /// an application's URL, its cookies' scope and the admin's framing policy
    /// all stay with the base domain. Ignored without a base domain, which is
    /// what turns application routing on at all.
    pub extra_base_domains: Vec<String>,
    /// How many V8 isolates the **code** pool runs (`--code-workers`), and how
    /// many runs each of them keeps resident at once (`--code-max-inflight`).
    ///
    /// A `run_js_code` body's run costs a pending promise rather than a thread,
    /// so the worker count buys CPU parallelism and the admission bound buys
    /// occupancy: the server serves `code_workers × code_max_inflight` bodies at
    /// once (512 by default) and queues the rest, with the queue time still
    /// inside each run's own deadline. Past that the ceiling is the database
    /// connection pool, which is where it belongs (design §10.1).
    ///
    /// Flags rather than stored settings because both are properties of *this
    /// process's* machine — its cores and its memory — not of the application,
    /// and a node with more of either should be able to say so without every
    /// other node against the same database hearing it.
    pub code_workers: usize,
    /// Runs each code isolate admits at once — see [`code_workers`].
    ///
    /// [`code_workers`]: ServerConfig::code_workers
    pub code_max_inflight: usize,
    /// How many Deno workers the **module** pool runs (`--module-workers`).
    ///
    /// One by default, because that is what the `node` sidecar it replaces
    /// already is: one runtime holding every module, not one per module. A
    /// module is pinned to a worker for its lifetime — its `require` cache and
    /// its module-level state (an MQTT client with a reconnect timer, a
    /// configured geocoder) live there — so the reason to run a second worker is
    /// **blast radius** and not throughput: the JS-slice watchdog stops an
    /// isolate and everything resident on it, so a module that must not share a
    /// runaway's fate wants a worker to itself.
    ///
    /// A flag rather than a stored setting, for the reason `--code-workers` is
    /// one: it is a property of *this process's* machine and not of the
    /// installation every node shares.
    ///
    pub module_workers: usize,
    /// Where installed **modules** live: the npm project the server installs
    /// packages into and runs the module host in (TODO "Modules", §1).
    ///
    /// `None` means the platform's data directory
    /// (`sc_module::default_modules_root`). It is a flag rather than a stored
    /// setting for the reason `--code-workers` is one: it is a property of
    /// *this machine* — which disk has room, which directory the service
    /// account may write — and not of the installation every node shares.
    pub modules_dir: Option<PathBuf>,
    /// Whether this process may **start** the Python interpreter it was built
    /// with (`--python off|auto`, default `auto`).
    ///
    /// Not whether it *has* one: that is a Cargo feature and a rebuild (§7). A
    /// binary built without `python` has no interpreter and no flag adds one; a
    /// binary built with it registers `run_python_code` either way, so what
    /// `--python off` changes is the sentence a Python trigger fails with — the
    /// flag, rather than a missing action.
    pub python: PythonMode,
    /// How many Python runs may be resident at once (`--python-max-inflight`),
    /// and how many stuck threads are tolerated (`--python-max-stuck`).
    ///
    /// One admission bound over one interpreter, so this is the Python
    /// counterpart of `--code-max-inflight` and not of `--code-workers`: there
    /// is no second interpreter to run. A resident run costs a thread (about
    /// 40 KB), which is why the default is small and raising it is cheap.
    pub python_max_inflight: usize,
    /// Stuck run threads tolerated before new runs are refused — see
    /// [`python_max_inflight`].
    ///
    /// [`python_max_inflight`]: ServerConfig::python_max_inflight
    pub python_max_stuck: usize,
    /// The virtual environment Python modules are installed into
    /// (`--python-dir`), and the external interpreter `pip` runs under
    /// (`--python-bin`) — §9's two halves, which are a machine's properties for
    /// the reason `--modules-dir` is.
    ///
    /// `None` in either is the default: the platform data directory beside the
    /// modules root, and `python3` on the path.
    pub python_env: sc_python::PythonEnv,
    /// The ceiling on the rows one **dataset** may materialise for a model fit
    /// (`--model-max-rows`, default [`sc_model::DEFAULT_MAX_ROWS`]).
    ///
    /// A dataset is a `SELECT` an admin wrote and the fit has to hold the answer
    /// in memory columnar, so there has to be a bound; the count is asked for
    /// before the rows, so exceeding it costs one `COUNT(*)` and is refused by
    /// name rather than by the OOM killer.
    ///
    /// A flag rather than a stored setting for the reason `--code-workers` is
    /// one: what a materialisation costs is a property of *this process's*
    /// memory, and a node with more of it should be able to say so without every
    /// other node against the same database hearing it.
    pub model_max_rows: u64,
    /// Where this node's CmdStan is, where compiled programs are kept, how many
    /// chain processes may run at once, and the ceilings on a posterior fit's
    /// data, draws and responses — `--cmdstan` and the `--stan-*` flags (TODO
    /// "Bayesian models with Stan" §20).
    ///
    /// Flags rather than stored settings, for `--model-max-rows`' reason: a
    /// CmdStan is a directory on *this* machine, a chain is one of its cores,
    /// and a gigabyte of draws is its memory.
    pub stan: crate::models::StanSettings,
    /// How the stream supervisor behaves: the per-stream broadcast buffer, the
    /// element-rate cap, the replay ring and the reconnection backoff (TODO
    /// "Streams" §7).
    ///
    /// Flags rather than stored settings, for `--model-max-rows`' reason: what
    /// a buffer of a thousand envelopes costs is a property of *this process's*
    /// memory, and a node with more of it should be able to say so without
    /// every other node against the same database hearing it. Only the two §7
    /// calls configuration are exposed — `--stream-buffer` and
    /// `--stream-max-rate`; the ring and the backoff are the defaults, because
    /// nobody has ever wanted a different answer to "how far back does the
    /// Observe screen go".
    pub streams: sc_stream::StreamConfig,
    /// How this server obtains the certificate it serves HTTPS with (§13.5).
    ///
    /// **Not a command-line setting**, deliberately: certificates are edited in
    /// the admin UI and stored in `_fd_config`, so every node against one
    /// database serves the same thing and a renewal is not a deploy. The boot
    /// path reads the settings and fills this in
    /// ([`TlsSettings::from_ssl`](crate::tls::TlsSettings::from_ssl)); the
    /// default is [`Off`](TlsSettings::Off), which is plain HTTP.
    pub tls: TlsSettings,
    /// The port HTTPS is served on when [`tls`](Self::tls) is on
    /// (`--https-port`, or `https_port` in `feldspar.toml`; default 443).
    ///
    /// A flag rather than a stored setting, although the certificates are
    /// stored: the port is a property of this host — what its firewall and its
    /// proxy expect — and a stored one would travel with a backup into a
    /// deployment that does not listen there.
    pub https_port: u16,
    /// The headless Chromium `view_app` drives (`--browser`, TODO §7b). `None`
    /// searches `PATH`; see [`detect_browser`](crate::browser::detect_browser).
    pub browser: Option<PathBuf>,
    /// Whether that browser keeps its sandbox (`--no-browser-sandbox` turns it
    /// off).
    pub browser_sandbox: bool,
    /// How many runs may hold a browser context at once
    /// (`--browser-contexts`). A call beyond it waits, within its timeout.
    pub browser_contexts: usize,
    /// How long a run's preview mount may go unused before the sweep removes it
    /// (`--preview-idle-minutes`, default an hour).
    pub preview_idle: std::time::Duration,
}

/// The port HTTPS is served on when `--https-port` is not given.
pub const DEFAULT_HTTPS_PORT: u16 = 443;

/// How many runs may hold a browser context at once, by default.
pub const DEFAULT_BROWSER_CONTEXTS: usize = 4;

/// How long a preview may go unused before it is swept, by default.
pub const DEFAULT_PREVIEW_IDLE_MINUTES: u64 = 60;

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            // `DEFAULT_BIND` is a valid literal, so parsing it cannot fail.
            addr: DEFAULT_BIND
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 3032))),
            static_dir: None,
            ide_dir: None,
            saltcorn_ui_dir: None,
            builder_dir: None,
            analytics_dir: None,
            plugins_dir: None,
            session_ttl_hours: sc_auth::DEFAULT_TTL_HOURS,
            secure_cookies: false,
            base_domain: None,
            extra_base_domains: Vec::new(),
            code_workers: sc_expr::DEFAULT_CODE_WORKERS,
            code_max_inflight: sc_expr::DEFAULT_MAX_INFLIGHT,
            module_workers: sc_module::DEFAULT_MODULE_WORKERS,
            modules_dir: None,
            python: PythonMode::Auto,
            python_max_inflight: sc_python::DEFAULT_MAX_INFLIGHT,
            python_max_stuck: sc_python::DEFAULT_MAX_STUCK,
            python_env: sc_python::PythonEnv::default(),
            model_max_rows: sc_model::DEFAULT_MAX_ROWS,
            stan: crate::models::StanSettings::default(),
            streams: sc_stream::StreamConfig::default(),
            tls: TlsSettings::Off,
            https_port: DEFAULT_HTTPS_PORT,
            browser: None,
            browser_sandbox: true,
            browser_contexts: DEFAULT_BROWSER_CONTEXTS,
            preview_idle: std::time::Duration::from_secs(DEFAULT_PREVIEW_IDLE_MINUTES * 60),
        }
    }
}

impl ServerConfig {
    /// Parse configuration from CLI arguments (everything after the subcommand).
    ///
    /// Recognised flags: `--bind <addr>`, `--static-dir <path>`,
    /// `--session-ttl-hours <n>`, `--secure-cookies`, `--https-port <n>`,
    /// `--base-domain <domain>`,
    /// `--extra-base-domain <domain>` (repeatable), `--code-workers <n>`,
    /// `--code-max-inflight <n>`, `--module-workers <n>`,
    /// `--modules-dir <path>`, `--python <auto|off>`,
    /// `--python-max-inflight <n>`, `--python-max-stuck <n>`,
    /// `--python-dir <path>`, `--python-bin <path>`, `--model-max-rows <n>`,
    /// `--cmdstan <dir>`, `--stan-cache-dir <dir>`, `--stan-max-processes <n>`,
    /// `--stan-max-data-values <n>`, `--stan-max-draws-bytes <n>`,
    /// `--stan-max-draws-response <n>`, `--stan-summary-max-elements <n>`,
    /// `--browser <path>`, `--no-browser-sandbox`, `--browser-contexts <n>` and
    /// `--preview-idle-minutes <n>`. Unknown flags are an
    /// [`Error::Config`], so a typo fails loudly rather than being ignored.
    pub fn from_args<I, S>(args: I) -> Result<ServerConfig>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut cfg = ServerConfig::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_ref() {
                "--bind" => {
                    let raw = next_value(&mut it, "--bind")?;
                    cfg.addr = raw
                        .parse()
                        .map_err(|e| Error::config(format!("invalid --bind `{raw}`: {e}")))?;
                }
                "--static-dir" => {
                    cfg.static_dir = Some(PathBuf::from(next_value(&mut it, "--static-dir")?));
                }
                "--session-ttl-hours" => {
                    let raw = next_value(&mut it, "--session-ttl-hours")?;
                    cfg.session_ttl_hours = raw.parse().map_err(|e| {
                        Error::config(format!("invalid --session-ttl-hours `{raw}`: {e}"))
                    })?;
                }
                "--code-workers" => {
                    cfg.code_workers =
                        positive(&next_value(&mut it, "--code-workers")?, "--code-workers")?;
                }
                "--code-max-inflight" => {
                    cfg.code_max_inflight = positive(
                        &next_value(&mut it, "--code-max-inflight")?,
                        "--code-max-inflight",
                    )?;
                }
                "--module-workers" => {
                    cfg.module_workers = positive(
                        &next_value(&mut it, "--module-workers")?,
                        "--module-workers",
                    )?;
                }
                "--modules-dir" => {
                    cfg.modules_dir = Some(PathBuf::from(next_value(&mut it, "--modules-dir")?));
                }
                "--python" => {
                    let raw = next_value(&mut it, "--python")?;
                    cfg.python = match raw.as_str() {
                        "auto" => PythonMode::Auto,
                        "off" => PythonMode::Off,
                        other => {
                            return Err(Error::config(format!(
                                "invalid --python `{other}`: expected `auto` or `off`. \
                                 Whether this server *has* Python is a build-time feature, \
                                 not a flag."
                            )));
                        }
                    };
                }
                "--python-max-inflight" => {
                    cfg.python_max_inflight = positive(
                        &next_value(&mut it, "--python-max-inflight")?,
                        "--python-max-inflight",
                    )?;
                }
                "--python-max-stuck" => {
                    cfg.python_max_stuck = positive(
                        &next_value(&mut it, "--python-max-stuck")?,
                        "--python-max-stuck",
                    )?;
                }
                "--python-dir" => {
                    cfg.python_env.dir = Some(PathBuf::from(next_value(&mut it, "--python-dir")?));
                }
                "--python-bin" => {
                    cfg.python_env.bin = Some(PathBuf::from(next_value(&mut it, "--python-bin")?));
                }
                "--model-max-rows" => {
                    let raw = next_value(&mut it, "--model-max-rows")?;
                    cfg.model_max_rows = match raw.parse::<u64>() {
                        Ok(n) if n > 0 => n,
                        // Zero would make every dataset refuse, which reads as a
                        // broken server rather than as a bound.
                        Ok(_) => {
                            return Err(Error::config("--model-max-rows must be at least 1"));
                        }
                        Err(e) => {
                            return Err(Error::config(format!(
                                "invalid --model-max-rows `{raw}`: {e}"
                            )));
                        }
                    };
                }
                "--cmdstan" => {
                    cfg.stan.cmdstan = Some(PathBuf::from(next_value(&mut it, "--cmdstan")?));
                }
                "--stan-cache-dir" => {
                    cfg.stan.cache_dir =
                        Some(PathBuf::from(next_value(&mut it, "--stan-cache-dir")?));
                }
                "--stan-max-processes" => {
                    cfg.stan.max_processes = Some(positive(
                        &next_value(&mut it, "--stan-max-processes")?,
                        "--stan-max-processes",
                    )?);
                }
                // The four ceilings are refused at zero for `--model-max-rows`'
                // reason: a bound of nothing makes every fit (or every read)
                // fail, which reads as a broken server rather than as a bound.
                "--stan-max-data-values" => {
                    cfg.stan.limits.max_data_values = positive_u64(
                        &next_value(&mut it, "--stan-max-data-values")?,
                        "--stan-max-data-values",
                    )?;
                }
                "--stan-max-draws-bytes" => {
                    cfg.stan.limits.max_draws_bytes = positive_u64(
                        &next_value(&mut it, "--stan-max-draws-bytes")?,
                        "--stan-max-draws-bytes",
                    )?;
                }
                "--stan-max-draws-response" => {
                    cfg.stan.max_draws_response = positive_u64(
                        &next_value(&mut it, "--stan-max-draws-response")?,
                        "--stan-max-draws-response",
                    )?;
                }
                "--stan-summary-max-elements" => {
                    // Zero is allowed: it means "summarise every generated
                    // quantity on demand", which is a coherent choice for a
                    // node that fits programs with large `y_rep`s.
                    let raw = next_value(&mut it, "--stan-summary-max-elements")?;
                    cfg.stan.limits.summary_max_elements = raw.parse().map_err(|e| {
                        Error::config(format!("invalid --stan-summary-max-elements `{raw}`: {e}"))
                    })?;
                }
                "--stream-buffer" => {
                    cfg.streams.channel_capacity =
                        positive(&next_value(&mut it, "--stream-buffer")?, "--stream-buffer")?;
                }
                "--stream-max-rate" => {
                    // Zero is **allowed** here, unlike `--model-max-rows`, and
                    // it means "no cap": a rate limit of nothing is a coherent
                    // thing to ask for on a stream nobody else is paying for,
                    // where a dataset cap of zero would only ever mean a
                    // broken server.
                    let raw = next_value(&mut it, "--stream-max-rate")?;
                    cfg.streams.max_elements_per_second = raw.parse::<u64>().map_err(|e| {
                        Error::config(format!("invalid --stream-max-rate `{raw}`: {e}"))
                    })?;
                }
                "--secure-cookies" => cfg.secure_cookies = true,
                "--https-port" => {
                    let raw = next_value(&mut it, "--https-port")?;
                    // A port that is not a port is refused rather than quietly
                    // swapped for 443: binding somewhere the operator did not
                    // ask for is worse than not starting.
                    cfg.https_port = match raw.parse::<u16>() {
                        Ok(port) if port > 0 => port,
                        _ => {
                            return Err(Error::config(format!(
                                "invalid --https-port `{raw}`: should be a TCP port (1–65535)"
                            )));
                        }
                    };
                }
                "--browser" => {
                    cfg.browser = Some(PathBuf::from(next_value(&mut it, "--browser")?));
                }
                "--no-browser-sandbox" => cfg.browser_sandbox = false,
                "--browser-contexts" => {
                    cfg.browser_contexts = positive(
                        &next_value(&mut it, "--browser-contexts")?,
                        "--browser-contexts",
                    )?;
                }
                "--preview-idle-minutes" => {
                    let minutes = positive(
                        &next_value(&mut it, "--preview-idle-minutes")?,
                        "--preview-idle-minutes",
                    )?;
                    cfg.preview_idle = std::time::Duration::from_secs(minutes as u64 * 60);
                }
                "--base-domain" => {
                    cfg.base_domain = Some(next_value(&mut it, "--base-domain")?);
                }
                "--extra-base-domain" => {
                    let domain = next_value(&mut it, "--extra-base-domain")?;
                    let domain = domain.trim().trim_end_matches('.').to_owned();
                    if !domain.is_empty() && !cfg.extra_base_domains.contains(&domain) {
                        cfg.extra_base_domains.push(domain);
                    }
                }
                other => {
                    return Err(Error::config(format!("unknown server argument `{other}`")));
                }
            }
        }
        Ok(cfg)
    }
}

/// [`positive`], for a ceiling counted in values or bytes rather than in
/// workers — which can exceed a 32-bit `usize`.
fn positive_u64(raw: &str, flag: &str) -> Result<u64> {
    match raw.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        Ok(_) => Err(Error::config(format!("{flag} must be at least 1"))),
        Err(e) => Err(Error::config(format!("invalid {flag} `{raw}`: {e}"))),
    }
}

/// Take the value following a flag, erroring if it is missing.
fn next_value<I, S>(it: &mut I, flag: &str) -> Result<String>
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    it.next()
        .map(|s| s.as_ref().to_owned())
        .ok_or_else(|| Error::config(format!("{flag} requires a value")))
}

/// Parse a count that must be at least one. Zero isolates or zero resident runs
/// would serve nothing at all, and the pool silently clamps both — so a `0` on
/// the command line is refused here rather than quietly meaning `1`.
fn positive(raw: &str, flag: &str) -> Result<usize> {
    match raw.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        Ok(_) => Err(Error::config(format!("{flag} must be at least 1"))),
        Err(e) => Err(Error::config(format!("invalid {flag} `{raw}`: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a server nobody else can reach gets `localhost` without asking, and
    /// only without TLS.
    #[test]
    fn a_loopback_server_without_a_base_domain_serves_its_apps_under_localhost() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            implied_base_domain(&addr(DEFAULT_BIND), false),
            Some("localhost")
        );
        assert_eq!(
            implied_base_domain(&addr("[::1]:3032"), false),
            Some("localhost")
        );
        assert_eq!(implied_base_domain(&addr("0.0.0.0:3032"), false), None);
        assert_eq!(implied_base_domain(&addr("192.168.1.50:80"), false), None);
        assert_eq!(implied_base_domain(&addr(DEFAULT_BIND), true), None);
    }

    #[test]
    fn defaults_bind_to_localhost_3032() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.addr.to_string(), "127.0.0.1:3032");
        assert!(cfg.static_dir.is_none());
        assert!(cfg.ide_dir.is_none());
        assert!(cfg.plugins_dir.is_none());
        assert!(!cfg.secure_cookies);
        assert_eq!(cfg.https_port, DEFAULT_HTTPS_PORT);
        // App subdomain routing is opt-in.
        assert!(cfg.base_domain.is_none());
        // The code pool's defaults are the engine's own (design §10.1).
        assert_eq!(cfg.code_workers, sc_expr::DEFAULT_CODE_WORKERS);
        assert_eq!(cfg.code_max_inflight, sc_expr::DEFAULT_MAX_INFLIGHT);
        // One module worker: the sidecar this replaced was one process holding
        // every module.
        assert_eq!(cfg.module_workers, sc_module::DEFAULT_MODULE_WORKERS);
        // Modules land in the platform's data directory unless this machine
        // says otherwise.
        assert!(cfg.modules_dir.is_none());
        // Python: started when something needs it, with the runtime's own
        // bounds and its own environment defaults.
        assert_eq!(cfg.python, PythonMode::Auto);
        assert_eq!(cfg.python_max_inflight, sc_python::DEFAULT_MAX_INFLIGHT);
        assert_eq!(cfg.python_max_stuck, sc_python::DEFAULT_MAX_STUCK);
        assert_eq!(cfg.python_env, sc_python::PythonEnv::default());
        // A dataset is bounded by the engine's own default (TODO §9).
        assert_eq!(cfg.model_max_rows, sc_model::DEFAULT_MAX_ROWS);
    }

    #[test]
    fn parses_all_flags() {
        let cfg = ServerConfig::from_args([
            "--bind",
            "0.0.0.0:8080",
            "--static-dir",
            "/srv/admin",
            "--session-ttl-hours",
            "12",
            "--secure-cookies",
            "--https-port",
            "8443",
            "--base-domain",
            "example.com",
            "--extra-base-domain",
            "10.0.2.2.nip.io",
            "--extra-base-domain",
            "192.168.1.50.nip.io.",
            "--extra-base-domain",
            "10.0.2.2.nip.io",
            "--code-workers",
            "4",
            "--code-max-inflight",
            "64",
            "--module-workers",
            "3",
            "--modules-dir",
            "/srv/modules",
            "--python",
            "off",
            "--python-max-inflight",
            "8",
            "--python-max-stuck",
            "2",
            "--python-dir",
            "/srv/python",
            "--python-bin",
            "/usr/bin/python3.12",
            "--model-max-rows",
            "5000",
        ])
        .expect("parse");
        assert_eq!(cfg.addr.to_string(), "0.0.0.0:8080");
        assert_eq!(
            cfg.static_dir.as_deref(),
            Some(std::path::Path::new("/srv/admin"))
        );
        assert_eq!(cfg.session_ttl_hours, 12);
        assert!(cfg.secure_cookies);
        assert_eq!(cfg.https_port, 8443);
        assert_eq!(cfg.base_domain.as_deref(), Some("example.com"));
        // Repeatable, in order, each once, and a trailing dot is the same name.
        assert_eq!(
            cfg.extra_base_domains,
            ["10.0.2.2.nip.io", "192.168.1.50.nip.io"]
        );
        assert_eq!(cfg.code_workers, 4);
        assert_eq!(cfg.code_max_inflight, 64);
        assert_eq!(cfg.module_workers, 3);
        assert_eq!(
            cfg.modules_dir.as_deref(),
            Some(std::path::Path::new("/srv/modules"))
        );
        assert_eq!(cfg.python, PythonMode::Off);
        assert_eq!(cfg.python_max_inflight, 8);
        assert_eq!(cfg.python_max_stuck, 2);
        assert_eq!(cfg.model_max_rows, 5000);
        assert_eq!(
            cfg.python_env.dir.as_deref(),
            Some(std::path::Path::new("/srv/python"))
        );
        assert_eq!(
            cfg.python_env.bin.as_deref(),
            Some(std::path::Path::new("/usr/bin/python3.12"))
        );
    }

    /// `--python` takes one of two words, and neither of them is the *build*
    /// decision: an operator who writes `--python on` against a binary with no
    /// interpreter in it must be told that here rather than at the first
    /// trigger.
    #[test]
    fn the_python_switch_is_auto_or_off_and_says_so() {
        assert_eq!(
            ServerConfig::from_args(["--python", "auto"])
                .expect("parse")
                .python,
            PythonMode::Auto
        );
        let said = ServerConfig::from_args(["--python", "on"])
            .expect_err("`on` is not one of the two")
            .to_string();
        assert!(said.contains("auto"), "{said}");
        assert!(said.contains("build-time feature"), "{said}");
        assert!(ServerConfig::from_args(["--python"]).is_err());
        // Zero resident runs would serve nothing, exactly as zero isolates
        // would; the runtime clamps it, so a `0` here is refused.
        assert!(ServerConfig::from_args(["--python-max-inflight", "0"]).is_err());
        assert!(ServerConfig::from_args(["--python-max-stuck", "lots"]).is_err());
    }

    /// A dataset ceiling of zero would make every fit refuse, which reads as a
    /// broken server rather than as a bound — so it is refused on the command
    /// line, where an operator can still see what they typed.
    #[test]
    fn rejects_a_zero_or_unparseable_dataset_ceiling() {
        assert!(ServerConfig::from_args(["--model-max-rows", "0"]).is_err());
        assert!(ServerConfig::from_args(["--model-max-rows", "many"]).is_err());
        assert!(ServerConfig::from_args(["--model-max-rows"]).is_err());
    }

    /// Both code-pool counts are clamped to at least one by the pool itself, so
    /// a `0` here is a mistake that would be silently rewritten. Refuse it.
    #[test]
    fn rejects_a_zero_or_unparseable_code_pool() {
        assert!(ServerConfig::from_args(["--code-workers", "0"]).is_err());
        assert!(ServerConfig::from_args(["--code-max-inflight", "0"]).is_err());
        assert!(ServerConfig::from_args(["--code-workers", "lots"]).is_err());
        assert!(ServerConfig::from_args(["--code-max-inflight"]).is_err());
        assert!(ServerConfig::from_args(["--module-workers", "0"]).is_err());
    }

    /// The IDE is not configurable, and asking for it is a typo like any other.
    #[test]
    fn the_ide_bundle_is_not_a_flag() {
        assert!(ServerConfig::from_args(["--ide-dir", "/srv/ide"]).is_err());
    }

    /// Nor is Saltcorn UI's: it is a property of the build, not of the host.
    #[test]
    fn the_saltcorn_ui_bundle_is_not_a_flag() {
        assert!(ServerConfig::from_args(["--saltcorn-ui-dir", "/srv/sui"]).is_err());
        assert!(ServerConfig::default().saltcorn_ui_dir.is_none());
    }

    /// The browser flags (TODO 6b.2): a path, the sandbox switch, and two
    /// counts that must be at least one.
    #[test]
    fn parses_the_browser_and_preview_flags() {
        let cfg = ServerConfig::default();
        assert!(cfg.browser.is_none());
        assert!(cfg.browser_sandbox);
        assert_eq!(cfg.browser_contexts, DEFAULT_BROWSER_CONTEXTS);
        assert_eq!(cfg.preview_idle.as_secs(), 3600);

        let cfg = ServerConfig::from_args([
            "--browser",
            "/usr/bin/chromium",
            "--no-browser-sandbox",
            "--browser-contexts",
            "2",
            "--preview-idle-minutes",
            "5",
        ])
        .expect("parse");
        assert_eq!(
            cfg.browser.as_deref(),
            Some(std::path::Path::new("/usr/bin/chromium"))
        );
        assert!(!cfg.browser_sandbox);
        assert_eq!(cfg.browser_contexts, 2);
        assert_eq!(cfg.preview_idle.as_secs(), 300);
        assert!(ServerConfig::from_args(["--browser-contexts", "0"]).is_err());
        assert!(ServerConfig::from_args(["--preview-idle-minutes", "soon"]).is_err());
    }

    /// The Stan flags of TODO §20: unset, each is the engine's own default and
    /// the budget is the machine's; set, each lands where the models services
    /// read it.
    #[test]
    fn parses_the_stan_flags() {
        let cfg = ServerConfig::default();
        assert!(cfg.stan.cmdstan.is_none());
        assert!(cfg.stan.cache_dir.is_none());
        assert!(
            cfg.stan.max_processes.is_none(),
            "half the CPUs, decided at boot"
        );
        assert_eq!(cfg.stan.limits, sc_model::PosteriorLimits::default());
        assert_eq!(
            cfg.stan.max_draws_response,
            sc_model::DEFAULT_MAX_DRAWS_RESPONSE
        );

        let cfg = ServerConfig::from_args([
            "--cmdstan",
            "/opt/cmdstan-2.40.0",
            "--stan-cache-dir",
            "/var/cache/feldspar-stan",
            "--stan-max-processes",
            "6",
            "--stan-max-data-values",
            "1000",
            "--stan-max-draws-bytes",
            "5000000000",
            "--stan-max-draws-response",
            "250000",
            "--stan-summary-max-elements",
            "0",
        ])
        .expect("parse");
        assert_eq!(
            cfg.stan.cmdstan.as_deref(),
            Some(std::path::Path::new("/opt/cmdstan-2.40.0"))
        );
        assert_eq!(
            cfg.stan.cache_dir.as_deref(),
            Some(std::path::Path::new("/var/cache/feldspar-stan"))
        );
        assert_eq!(cfg.stan.max_processes, Some(6));
        assert_eq!(cfg.stan.limits.max_data_values, 1000);
        // Past 4 GB: a byte ceiling is a u64 on every platform.
        assert_eq!(cfg.stan.limits.max_draws_bytes, 5_000_000_000);
        assert_eq!(cfg.stan.max_draws_response, 250_000);
        assert_eq!(cfg.stan.limits.summary_max_elements, 0);

        for flag in [
            "--stan-max-processes",
            "--stan-max-data-values",
            "--stan-max-draws-bytes",
            "--stan-max-draws-response",
        ] {
            let said = ServerConfig::from_args([flag, "0"])
                .expect_err("a ceiling of nothing is refused")
                .to_string();
            assert!(said.contains(flag), "{said}");
            assert!(ServerConfig::from_args([flag, "lots"]).is_err());
            assert!(ServerConfig::from_args([flag]).is_err());
        }
        assert!(ServerConfig::from_args(["--stan-summary-max-elements", "-1"]).is_err());
        assert!(ServerConfig::from_args(["--cmdstan"]).is_err());
    }

    #[test]
    fn rejects_bad_bind_and_unknown_flags() {
        assert!(ServerConfig::from_args(["--bind", "not-an-addr"]).is_err());
        assert!(ServerConfig::from_args(["--https-port", "0"]).is_err());
        assert!(ServerConfig::from_args(["--https-port", "70000"]).is_err());
        assert!(ServerConfig::from_args(["--bind"]).is_err()); // missing value
        assert!(ServerConfig::from_args(["--nope"]).is_err()); // unknown flag
    }
}
