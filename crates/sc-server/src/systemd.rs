//! Talking to the service manager: `Type=notify` readiness and the watchdog.
//!
//! A unit that says `Type=simple` is "started" the moment the process is forked,
//! which is a lie an operator pays for twice: `systemctl start` returns before
//! the port is bound, so a dependent unit ordered `After=feldspar.service` races
//! the listener, and a boot that dies while connecting to the database still
//! looks like a successful start. `Type=notify` replaces the guess with a fact —
//! the process itself says when it is serving.
//!
//! The protocol is one line-oriented datagram to the socket named by
//! `NOTIFY_SOCKET` ([sd_notify(3)]), and it is small enough to speak directly:
//!
//! - `READY=1` — sent by [`serve`](crate::serve) **after** every listener is
//!   bound, so the address is accepting connections before the unit is active.
//! - `STATUS=…` — the line `systemctl status` shows. The boot sends a few, since
//!   the interesting part of a Saltcorn start is what it is doing for the
//!   seconds before the port opens.
//! - `EXTEND_TIMEOUT_USEC=…` — pushes `TimeoutStartSec` out while a boot step
//!   that can legitimately take minutes runs (building an application runs `npm
//!   install`), rather than making the unit's start timeout large enough for the
//!   worst case and useless for every other failure.
//! - `WATCHDOG=1` — the keep-alive `WatchdogSec` expects, sent at half the
//!   configured interval by the task [`ServiceManager::spawn_watchdog`] starts.
//! - `STOPPING=1` — sent when the shutdown signal arrives, so the unit is
//!   `deactivating` for the length of the graceful drain instead of looking
//!   hung.
//!
//! **There is no `libsystemd` here, and no feature flag.** The whole protocol is
//! a `sendto` on an `AF_UNIX` datagram socket, so this compiles with no system
//! dependency on every target the server builds for: on Linux with or without
//! `libsystemd-dev` installed, on the other Unixes (where `NOTIFY_SOCKET` is
//! simply never set, and every method below is a no-op), and on Windows (where
//! the platform has no Unix sockets at all and the implementation is compiled
//! out entirely).
//!
//! **Nothing here ever fails the server.** A socket that has gone away, a
//! malformed `WATCHDOG_USEC`, a `NOTIFY_SOCKET` naming a path that cannot be
//! reached: each is a service manager problem, and none of them is a reason to
//! stop answering requests. They are reported on stderr — where the journal is
//! already reading — and otherwise ignored.
//!
//! [sd_notify(3)]: https://www.freedesktop.org/software/systemd/man/sd_notify.html

use std::time::Duration;

/// Environment variable naming the service manager's notification socket.
const NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";
/// Environment variable carrying the watchdog interval, in microseconds.
const WATCHDOG_USEC: &str = "WATCHDOG_USEC";
/// Environment variable naming the process the watchdog is meant for.
const WATCHDOG_PID: &str = "WATCHDOG_PID";

/// The shortest watchdog ping interval this will use, whatever `WatchdogSec`
/// says. A unit configured with `WatchdogSec=1s` gets pinged every 500 ms rather
/// than every 500 µs if someone writes `WatchdogSec=1ms`.
const MIN_WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

/// Where a notification is sent.
///
/// Parsed once, at construction, so a `NOTIFY_SOCKET` that means nothing is
/// diagnosed at boot rather than at each send — and so the watchdog task can
/// carry a copy without re-reading the environment.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// A socket in the file system: the usual `/run/systemd/notify`.
    Path(std::path::PathBuf),
    /// A Linux abstract-namespace socket, spelled `@name` in the variable. The
    /// name excludes the leading `@`, which stands for the address's leading nul.
    #[cfg(target_os = "linux")]
    Abstract(Vec<u8>),
}

/// The process's link to the service manager that started it.
///
/// Cheap to construct and cheap to clone; when there is no service manager —
/// which is every development run, every container without a notify socket, and
/// every non-Unix platform — it holds nothing and every method returns without
/// doing any work.
#[derive(Clone, Debug, Default)]
pub struct ServiceManager {
    /// The notification socket, if this process was started with one.
    #[cfg(unix)]
    target: Option<Target>,
    /// How often the service manager expects a `WATCHDOG=1`, if it does. This is
    /// the *ping* interval — half of `WatchdogSec`, as sd_notify(3) recommends,
    /// so one missed ping is not a restart.
    watchdog: Option<Duration>,
}

impl ServiceManager {
    /// Read the service manager's environment: `NOTIFY_SOCKET`, `WATCHDOG_USEC`
    /// and `WATCHDOG_PID`.
    ///
    /// Every one of them absent — the common case — yields a manager that does
    /// nothing at all.
    #[must_use]
    pub fn from_env() -> ServiceManager {
        ServiceManager::from_env_values(
            std::env::var(NOTIFY_SOCKET).ok().as_deref(),
            std::env::var(WATCHDOG_USEC).ok().as_deref(),
            std::env::var(WATCHDOG_PID).ok().as_deref(),
        )
    }

    /// [`from_env`](Self::from_env) over values supplied directly.
    ///
    /// Public because it is what makes this testable without mutating the
    /// process environment, which is both racy across threads and unsafe in this
    /// edition — and because a caller that already knows the socket (a test
    /// harness standing a server up) should not have to go through the
    /// environment to say so.
    #[must_use]
    pub fn from_env_values(
        notify_socket: Option<&str>,
        watchdog_usec: Option<&str>,
        watchdog_pid: Option<&str>,
    ) -> ServiceManager {
        // The watchdog is addressed at a specific process. Without this check a
        // child that inherited the environment would answer for its parent, and
        // a hung parent would never be restarted.
        let for_this_process = match watchdog_pid {
            None => true,
            Some(pid) => pid.trim().parse::<u32>() == Ok(std::process::id()),
        };
        let watchdog = watchdog_usec
            .filter(|_| for_this_process)
            .and_then(ping_interval);

        #[cfg(unix)]
        {
            ServiceManager {
                target: notify_socket.and_then(parse_target),
                watchdog,
            }
        }
        #[cfg(not(unix))]
        {
            // No Unix sockets: there is nothing to send to, and `NOTIFY_SOCKET`
            // on such a platform is somebody's stray environment variable.
            let _ = notify_socket;
            let _ = watchdog;
            ServiceManager { watchdog: None }
        }
    }

    /// Whether this process has a service manager to notify.
    #[must_use]
    pub fn enabled(&self) -> bool {
        #[cfg(unix)]
        {
            self.target.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// How often [`spawn_watchdog`](Self::spawn_watchdog) will ping, if a
    /// watchdog is configured for this process.
    #[must_use]
    pub fn watchdog_interval(&self) -> Option<Duration> {
        self.watchdog.filter(|_| self.enabled())
    }

    /// `READY=1`, with the status line the operator sees beside it.
    ///
    /// Send this **after** the listeners are bound: it is the moment
    /// `systemctl start` returns and every unit ordered after this one is
    /// released.
    pub fn notify_ready(&self, status: &str) {
        self.send(&format!("READY=1\nSTATUS={}", one_line(status)));
    }

    /// `STATUS=…`: the line `systemctl status` shows under the unit.
    pub fn notify_status(&self, status: &str) {
        self.send(&format!("STATUS={}", one_line(status)));
    }

    /// `STOPPING=1`: the process has accepted a shutdown signal and is draining.
    pub fn notify_stopping(&self, status: &str) {
        self.send(&format!("STOPPING=1\nSTATUS={}", one_line(status)));
    }

    /// `EXTEND_TIMEOUT_USEC=…`: give the current start (or stop) step this much
    /// longer before the service manager calls it hung.
    ///
    /// It is a *reprieve from now*, not a new total, and it has to be repeated to
    /// keep extending — which is why the boot sends one per long step rather
    /// than one at the top.
    pub fn extend_timeout(&self, by: Duration) {
        let usec = u64::try_from(by.as_micros()).unwrap_or(u64::MAX);
        self.send(&format!("EXTEND_TIMEOUT_USEC={usec}"));
    }

    /// Start the watchdog keep-alive, if this process has one.
    ///
    /// The returned task pings every half `WatchdogSec` until it is dropped. It
    /// is a **liveness** check in the strict sense: it proves the tokio runtime
    /// is still scheduling tasks. That is precisely the failure the watchdog
    /// exists for — a blocking call that has eaten every worker thread, or a
    /// deadlock — and it is the one a request-level health check cannot report,
    /// because a runtime that cannot schedule this task cannot answer a request
    /// either. A server that is scheduling but wrong is a job for monitoring,
    /// not for `WatchdogSec`.
    #[must_use]
    #[allow(
        clippy::disallowed_methods,
        reason = "tokio::spawn: a background loop for the life of the process, at no caller's behalf, so it must not inherit the starting task's scope"
    )]
    pub fn spawn_watchdog(&self) -> Option<tokio::task::JoinHandle<()>> {
        let interval = self.watchdog_interval()?;
        let manager = self.clone();
        eprintln!(
            "feldspar: systemd watchdog enabled, pinging every {:.1}s",
            interval.as_secs_f64()
        );
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // A ping that was late is not worth catching up on: the next one on
            // the normal schedule is what the service manager wants.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick completes immediately; the keep-alive proper starts
            // one interval later, since `READY=1` has just been sent.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                manager.send("WATCHDOG=1");
            }
        }))
    }

    /// Send one already-formatted notification message, reporting a failure to
    /// stderr and continuing.
    fn send(&self, message: &str) {
        #[cfg(unix)]
        {
            let Some(target) = &self.target else {
                return;
            };
            if let Err(error) = send_datagram(target, message.as_bytes()) {
                let first = message.lines().next().unwrap_or_default();
                eprintln!("feldspar: could not notify the service manager ({first}): {error}");
            }
        }
        #[cfg(not(unix))]
        {
            let _ = message;
        }
    }
}

/// Collapse a status line: the protocol is newline-separated `NAME=value`
/// pairs, so an embedded newline in a status would forge a second directive.
fn one_line(status: &str) -> String {
    status.replace(['\n', '\r'], " ")
}

/// The ping interval for a `WATCHDOG_USEC` value: half of it, floored at
/// [`MIN_WATCHDOG_INTERVAL`]. `0` (or anything unparseable) means no watchdog.
fn ping_interval(watchdog_usec: &str) -> Option<Duration> {
    let usec: u64 = watchdog_usec.trim().parse().ok()?;
    if usec == 0 {
        return None;
    }
    Some(Duration::from_micros(usec / 2).max(MIN_WATCHDOG_INTERVAL))
}

/// Parse `NOTIFY_SOCKET` into the address to send to.
///
/// systemd spells an abstract socket with a leading `@` and a file-system one
/// as an absolute path; anything else is not an address this can use.
#[cfg(unix)]
fn parse_target(notify_socket: &str) -> Option<Target> {
    match notify_socket.as_bytes() {
        [] => None,
        #[cfg(target_os = "linux")]
        [b'@', name @ ..] if !name.is_empty() => Some(Target::Abstract(name.to_vec())),
        [b'/', ..] => Some(Target::Path(std::path::PathBuf::from(notify_socket))),
        _ => {
            eprintln!(
                "feldspar: ignoring {NOTIFY_SOCKET}={notify_socket}: not an absolute path or an abstract socket"
            );
            None
        }
    }
}

/// One datagram to the notification socket.
///
/// The socket is opened per message rather than held: notifications are a
/// handful at boot plus one every few seconds, an unbound datagram socket has no
/// connection to keep warm, and holding one would mean a socket that failed once
/// stays failed for the life of the process.
#[cfg(unix)]
fn send_datagram(target: &Target, message: &[u8]) -> std::io::Result<()> {
    use std::os::unix::net::UnixDatagram;

    let socket = UnixDatagram::unbound()?;
    match target {
        Target::Path(path) => socket.send_to(message, path)?,
        #[cfg(target_os = "linux")]
        Target::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            socket.send_to_addr(message, &addr)?
        }
    };
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// No service manager: the common case, and every method a no-op.
    #[test]
    fn without_a_notify_socket_everything_is_inert() {
        let manager = ServiceManager::from_env_values(None, Some("30000000"), None);
        assert!(!manager.enabled());
        assert_eq!(manager.watchdog_interval(), None);
        // Sending against nothing must not panic.
        manager.notify_ready("serving");
        manager.notify_stopping("draining");
    }

    #[test]
    fn the_ping_interval_is_half_the_watchdog_and_never_tiny() {
        assert_eq!(ping_interval("30000000"), Some(Duration::from_secs(15)));
        assert_eq!(ping_interval(" 2000000 "), Some(Duration::from_secs(1)));
        // Floored, not honoured literally.
        assert_eq!(ping_interval("1000"), Some(MIN_WATCHDOG_INTERVAL));
        // Off, or nonsense: no watchdog rather than a busy loop.
        assert_eq!(ping_interval("0"), None);
        assert_eq!(ping_interval("later"), None);
    }

    /// `WATCHDOG_PID` names the process the watchdog is for. A child that
    /// inherited the environment must not answer for its parent.
    ///
    /// Unix-only because it goes through a manager that has a socket to send to,
    /// and off Unix there is no such thing.
    #[cfg(unix)]
    #[test]
    fn the_watchdog_is_ignored_when_it_names_another_process() {
        let socket = "/run/systemd/notify";
        let mine = std::process::id().to_string();
        assert!(
            ServiceManager::from_env_values(Some(socket), Some("30000000"), Some(&mine))
                .watchdog_interval()
                .is_some()
        );
        assert_eq!(
            ServiceManager::from_env_values(Some(socket), Some("30000000"), Some("1"))
                .watchdog_interval(),
            None
        );
    }

    /// A status line cannot smuggle a second directive in through a newline.
    #[test]
    fn a_status_line_stays_one_line() {
        assert_eq!(one_line("bound\nREADY=1"), "bound READY=1");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::net::UnixDatagram;

        /// Receive one datagram, or fail the test.
        fn recv(socket: &UnixDatagram) -> String {
            let mut buf = [0_u8; 512];
            let n = socket.recv(&mut buf).expect("a notification");
            String::from_utf8_lossy(&buf[..n]).into_owned()
        }

        /// A socket in the file system: the `/run/systemd/notify` shape.
        #[test]
        fn a_path_socket_receives_the_protocol() {
            let dir = std::env::temp_dir().join(format!("sc-notify-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("notify");
            let _ = std::fs::remove_file(&path);
            let listener = UnixDatagram::bind(&path).unwrap();
            listener
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let manager = ServiceManager::from_env_values(
                Some(&path.to_string_lossy()),
                Some("30000000"),
                None,
            );
            assert!(manager.enabled());

            manager.notify_status("connecting to the database");
            assert_eq!(recv(&listener), "STATUS=connecting to the database");

            manager.extend_timeout(Duration::from_secs(300));
            assert_eq!(recv(&listener), "EXTEND_TIMEOUT_USEC=300000000");

            manager.notify_ready("listening on http://127.0.0.1:3000");
            assert_eq!(
                recv(&listener),
                "READY=1\nSTATUS=listening on http://127.0.0.1:3000"
            );

            manager.notify_stopping("draining in-flight requests");
            assert_eq!(
                recv(&listener),
                "STOPPING=1\nSTATUS=draining in-flight requests"
            );

            std::fs::remove_file(&path).unwrap();
            // The socket is gone; a send must report and not panic.
            manager.notify_status("gone");
            let _ = std::fs::remove_dir(&dir);
        }

        /// The watchdog task pings on its own, at the interval derived from
        /// `WATCHDOG_USEC`.
        #[tokio::test]
        async fn the_watchdog_task_pings() {
            let dir = std::env::temp_dir().join(format!("sc-watchdog-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("notify");
            let _ = std::fs::remove_file(&path);
            let listener = UnixDatagram::bind(&path).unwrap();
            listener
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            // `WatchdogSec=2s` → a ping every second.
            let manager = ServiceManager::from_env_values(
                Some(&path.to_string_lossy()),
                Some("2000000"),
                None,
            );
            assert_eq!(manager.watchdog_interval(), Some(Duration::from_secs(1)));

            let task = manager.spawn_watchdog().expect("a watchdog task");
            let listener = tokio::task::spawn_blocking(move || {
                assert_eq!(recv(&listener), "WATCHDOG=1");
                assert_eq!(recv(&listener), "WATCHDOG=1");
                listener
            })
            .await
            .unwrap();

            task.abort();
            drop(listener);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&dir);
        }

        /// Linux's abstract namespace: `NOTIFY_SOCKET=@name`, which is what a
        /// socket-activated or user-scoped unit is usually given.
        #[cfg(target_os = "linux")]
        #[test]
        fn an_abstract_socket_receives_the_protocol() {
            use std::os::linux::net::SocketAddrExt;

            let name = format!("sc-notify-abstract-{}", std::process::id());
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
            let listener = UnixDatagram::bind_addr(&addr).unwrap();
            listener
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let manager = ServiceManager::from_env_values(Some(&format!("@{name}")), None, None);
            assert!(manager.enabled());
            assert_eq!(manager.watchdog_interval(), None);

            manager.notify_ready("serving");
            assert_eq!(recv(&listener), "READY=1\nSTATUS=serving");
        }

        /// A `NOTIFY_SOCKET` that is neither an absolute path nor an abstract
        /// name is ignored rather than fatal.
        #[test]
        fn a_meaningless_notify_socket_is_ignored() {
            assert!(!ServiceManager::from_env_values(Some("notify"), None, None).enabled());
            assert!(!ServiceManager::from_env_values(Some(""), None, None).enabled());
        }
    }
}
