//! Helpers the test files of this binary share.
//!
//! Each file used to carry its own copy of these. A helper that one file needs
//! differently stays in that file, under its own name.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use sc_app::{ApiConfig, Application, FrameworkRef};
use sc_catalog::FileStoreId;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

/// A client's live socket to the server under test.
pub type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Send one JSON frame.
pub async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::text(frame.to_string()))
        .await
        .expect("the socket takes a frame");
}

/// The next JSON frame, skipping pings and pongs; panics after 20 s.
pub async fn next_frame(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("a frame arrives within the timeout")
            .expect("the socket is still open")
            .expect("the frame is readable");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The `code` framework building `web/` of the `apps` store with `sh build.sh`.
pub fn code_framework() -> FrameworkRef {
    FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts")
}

/// The blog application: `posts`, its files in `apps`, a REST API at `/api`.
pub fn blog_app() -> Application {
    Application::new("Blog", "blog", code_framework())
        .with_table("posts".to_owned())
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
}

/// This binary's data directory, set once for all of its tests: where a
/// restore places a store, and where the sync mirror and its key go. Never the
/// developer's own.
///
/// The environment is process-wide and the tests run in parallel, so there is
/// one, shared: two files each setting their own would move each other's
/// directory mid-test. Each test works in a subdirectory of its own.
#[allow(
    unsafe_code,
    clippy::disallowed_methods,
    reason = "the binary's data directory is set once, through the environment the server reads"
)]
pub fn data_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Never removed (it outlives every test), so a later run clears it.
        sc_test_harness::sweep_stale_scratch("sc-server-it-data");
        let dir = std::env::temp_dir().join(format!("sc-server-it-data-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only: written once per binary, under the `OnceLock`,
        // and never changed after. A concurrent `getenv` in another test is
        // the test-code race security review 2026-11 (F §1) records.
        unsafe { std::env::set_var(sc_files::DATA_DIR_ENV, &dir) };
        dir
    })
}
