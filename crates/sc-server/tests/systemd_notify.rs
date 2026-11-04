//! `feldspar serve` under a `Type=notify` service manager, end to end.
//!
//! The unit tests in `sc-server`'s `systemd` module assert the *protocol* — what
//! each notification looks like on the wire. What they cannot assert is the one
//! thing `Type=notify` is bought for: that `READY=1` means **the port is
//! open**. A readiness notification sent a moment too early is worse than none,
//! because every unit ordered `After=feldspar.service` is then released into a
//! connection refused.
//!
//! So this test stands the real [`sc_server::serve`] up with a real
//! `NOTIFY_SOCKET` behind it, and checks the three moments an operator's unit
//! file depends on:
//!
//! 1. `READY=1` arrives, and a TCP connection to the bind address succeeds *at
//!    that moment* — the listener was bound before the notification, not after.
//! 2. `WATCHDOG=1` keeps arriving on its own, at the interval `WATCHDOG_USEC`
//!    implies, which is what makes `WatchdogSec` in the unit safe to set.
//! 3. `STOPPING=1` arrives when the process is sent `SIGTERM`, so the unit shows
//!    `deactivating` for the graceful drain rather than looking hung.
//!
//! Unix only: on a platform with no Unix datagram sockets there is no service
//! manager to talk to, and the implementation is compiled out.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test code: it drives its own servers, tasks and channels; the replacements are for production paths"
)]

use std::net::SocketAddr;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sc_api::EndpointSet;
use sc_auth::SessionStore;
use sc_server::{AppMounts, HandlerRegistry, ServerConfig};

/// Receive one notification, or fail with what was being waited for.
fn recv(socket: &UnixDatagram, what: &str) -> String {
    let mut buf = [0_u8; 1024];
    let n = socket
        .recv(&mut buf)
        .unwrap_or_else(|e| panic!("waiting for {what}: {e}"));
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// A free port, found by binding one and letting it go. The window between the
/// two is the same one every ephemeral-port test lives with.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// The whole notify protocol against a running server.
///
/// One test rather than three, deliberately: readiness, the watchdog and the
/// stop are three points on **one** process's life, and the second and third
/// cannot be reached without the first.
#[tokio::test]
#[allow(
    unsafe_code,
    reason = "systemd's variables reach the server through the environment"
)]
async fn a_served_process_reports_ready_watchdog_and_stopping() {
    // The service manager's end of the socket. In a directory of this process's
    // own, since the path is the address.
    let dir = std::env::temp_dir().join(format!("sc-serve-notify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("notify");
    let _ = std::fs::remove_file(&socket_path);
    let manager_end = UnixDatagram::bind(&socket_path).unwrap();
    manager_end
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    // What systemd puts in the service's environment. Setting it is `unsafe`
    // because another thread may be reading the environment concurrently; this
    // is the only test in this binary, and this runs before anything is spawned,
    // which is the condition that makes it sound.
    //
    // `WATCHDOG_USEC=2000000` is `WatchdogSec=2s`, so a ping every second.
    //
    // SAFETY: the only test in this binary, before anything is spawned.
    unsafe {
        std::env::set_var("NOTIFY_SOCKET", &socket_path);
        std::env::set_var("WATCHDOG_USEC", "2000000");
        std::env::set_var("WATCHDOG_PID", std::process::id().to_string());
    }

    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let config = ServerConfig {
        addr,
        ..ServerConfig::default()
    };
    let server = tokio::spawn(async move {
        sc_server::serve(
            config,
            EndpointSet::new(),
            HandlerRegistry::new(),
            Arc::new(SessionStore::memory()),
            Arc::new(AppMounts::none()),
        )
        .await
    });

    // 1. Ready — and the address answers *now*, not eventually.
    let manager_end = tokio::task::spawn_blocking(move || {
        let ready = recv(&manager_end, "READY=1");
        assert_eq!(
            ready,
            format!("READY=1\nSTATUS=serving on http://{addr}"),
            "readiness carries the address the operator sees in `systemctl status`"
        );
        std::net::TcpStream::connect(addr)
            .expect("the listener must be bound before READY=1 is sent, not after");
        manager_end
    })
    .await
    .unwrap();

    // The server is serving, which also means the graceful-shutdown future has
    // been polled and the `SIGTERM` handler below is installed.
    let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert!(health.status().is_success());

    // 2. The watchdog pings on its own. Two of them, so the assertion is about a
    //    repeating keep-alive rather than a single message, and the gap between
    //    them is the interval `WATCHDOG_USEC` implies rather than a busy loop.
    let manager_end = tokio::task::spawn_blocking(move || {
        assert_eq!(recv(&manager_end, "the first WATCHDOG=1"), "WATCHDOG=1");
        let first = Instant::now();
        assert_eq!(recv(&manager_end, "the second WATCHDOG=1"), "WATCHDOG=1");
        let gap = first.elapsed();
        assert!(
            gap >= Duration::from_millis(500) && gap < Duration::from_secs(3),
            "pings should arrive about every second for WatchdogSec=2s, not {gap:?}"
        );
        manager_end
    })
    .await
    .unwrap();

    // 3. Stopping. `SIGTERM` to this very process is caught rather than fatal,
    //    because the running server has installed a handler for it — which is the
    //    whole reason `systemctl stop` is a graceful shutdown. Sent with `kill(1)`
    //    so this test needs no libc dependency of its own.
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .expect("kill(1)");
    assert!(killed.success());

    let manager_end = tokio::task::spawn_blocking(move || {
        assert_eq!(
            recv(&manager_end, "STOPPING=1"),
            "STOPPING=1\nSTATUS=draining in-flight requests"
        );
        manager_end
    })
    .await
    .unwrap();

    // And the server actually stops, rather than only saying so.
    let stopped = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server should shut down on SIGTERM")
        .unwrap();
    assert!(stopped.is_ok(), "graceful shutdown is not an error");

    drop(manager_end);
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_dir(&dir);
}
