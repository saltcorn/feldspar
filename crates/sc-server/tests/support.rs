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
