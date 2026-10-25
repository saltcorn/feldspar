//! The `fetch` action against a real socket (TODO Phase 3).
//!
//! Pinned against a **one-shot local HTTP listener** rather than a mocked client,
//! because the things that can be wrong here are all in the bytes: which method
//! and path were used, whether the body was sent at all, what content type it
//! claimed, and what the action makes of the reply. A mock would have to assert
//! the same facts about a request that never crossed a socket.
//!
//! What is asserted: the default request is a POST of the **event** as JSON, with
//! the configured headers, and the parsed response body becomes the action's
//! result; a body formula replaces that body (including sending the event's row
//! as-is); a `GET` sends no body at all; a non-2xx is an application error naming
//! the status *and* quoting the endpoint's own explanation; a hung endpoint fails
//! at the configured timeout rather than holding the caller; and the ways the
//! configuration can be wrong are refused on save.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use sc_action::{ActionContext, Event, EventKind, Trigger, validate_trigger};
use sc_catalog::Catalog;
use sc_core_actions::builtin_actions;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use serde_json::{Value as Json, json};

/// One table, so the event's `row` has a shape to be validated against.
const SCHEMA: &str = "CREATE TABLE books (id bigint primary key, title text, pages bigint);";

async fn setup(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(SCHEMA)
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// The event every test fires for: an insert of a book by a known admin.
fn book_insert() -> Event {
    Event::new(EventKind::Insert)
        .on("books")
        .row(json!({ "id": 7, "title": "A Book", "pages": 100 }))
        .caller(1, Some(json!({ "email": "admin@example.com" })))
}

fn config(entries: &[(&str, Json)]) -> Attrs {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// Run `fetch` through the registry — the path a firing trigger takes.
async fn run(catalog: &Catalog, event: &Event, config: &Attrs) -> Result<Json> {
    let engine: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let registry = builtin_actions()?;
    let action = registry.require("fetch")?.clone();
    let mut ctx = ActionContext::new(catalog, event, config, "notify").with_evaluator(&engine);
    action.run(&mut ctx).await
}

/// What the listener saw.
struct Received {
    /// `POST /hook HTTP/1.1`
    start_line: String,
    /// Header names lowercased, values as sent.
    headers: Vec<(String, String)>,
    body: String,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A listener that accepts exactly one connection, answers with `status` and
/// `body`, and hands the request it read back through the channel.
///
/// Bound to port 0 so tests never collide and never need a fixed port.
async fn one_shot(
    status: u16,
    content_type: &str,
    body: &str,
) -> (String, oneshot::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let received = read_request(&mut socket).await;
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        let _ = tx.send(received);
    });
    (url, rx)
}

/// A listener that accepts and then says nothing at all — what a hung endpoint
/// looks like from the client's side.
async fn hangs() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        // Hold the connection open, unanswered, past any test's timeout.
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });
    url
}

/// Read one HTTP request: headers, then exactly `content-length` bytes of body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Received {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let head_end = find(&buf, b"\r\n\r\n");
        if let Some(head_end) = head_end {
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let wanted = content_length(&head);
            if buf.len() >= head_end + 4 + wanted {
                let mut lines = head.split("\r\n");
                let start_line = lines.next().unwrap_or_default().to_owned();
                let headers = lines
                    .filter_map(|line| line.split_once(": "))
                    .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
                    .collect();
                let body = String::from_utf8_lossy(&buf[head_end + 4..]).to_string();
                return Received {
                    start_line,
                    headers,
                    body,
                };
            }
        }
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        assert!(n > 0, "the client closed before sending a whole request");
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn content_length(head: &str) -> usize {
    head.split("\r\n")
        .filter_map(|line| line.split_once(": "))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn a_fetch_posts_the_event_and_returns_the_parsed_response() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (url, received) = one_shot(200, "application/json", r#"{"ok":true,"id":42}"#).await;

    let cfg = config(&[
        ("url", json!(url)),
        (
            "headers",
            // The second one is deliberate: a configured `content-type` must win
            // over the one the JSON body sets, which is what the header ordering
            // in `run` is for.
            json!({ "x-token": "s3cret", "content-type": "application/vnd.acme+json" }),
        ),
    ]);
    let result = run(&catalog, &book_insert(), &cfg).await?;

    // The parsed response body *is* the result — what a directly-run trigger
    // returns to its caller and a workflow step puts in the context.
    assert_eq!(result, json!({ "ok": true, "id": 42 }));

    let request = received.await.unwrap();
    // POST by default, at the configured path.
    assert!(
        request.start_line.starts_with("POST /hook "),
        "{}",
        request.start_line
    );
    assert_eq!(
        request.header("content-type"),
        Some("application/vnd.acme+json"),
        "the configured content type overrides the JSON body's"
    );
    assert_eq!(request.header("x-token"), Some("s3cret"));
    assert!(request.header("host").is_some());

    // The default body is the event: what happened, to what, and who caused it.
    let body: Json = serde_json::from_str(&request.body).expect("a JSON body");
    assert_eq!(body["event"], json!("insert"));
    assert_eq!(body["channel"], json!("books"));
    assert_eq!(body["row"]["title"], json!("A Book"));
    assert_eq!(body["user"]["email"], json!("admin@example.com"));
    assert_eq!(body["role"], json!(1));
    assert_eq!(body["old_row"], Json::Null);
    Ok(())
}

#[tokio::test]
async fn a_body_formula_replaces_the_event_envelope() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    // `row` alone sends the triggering row and nothing else — the shape a service
    // that only cares about the data wants.
    let (url, received) = one_shot(200, "application/json", "{}").await;
    let cfg = config(&[("url", json!(url)), ("body", json!("row"))]);
    run(&catalog, &book_insert(), &cfg).await?;
    let body: Json = serde_json::from_str(&received.await.unwrap().body).unwrap();
    assert_eq!(body, json!({ "id": 7, "title": "A Book", "pages": 100 }));

    // And a computed body is a formula like any other: the event and the caller
    // are in scope, and arithmetic works.
    let (url, received) = one_shot(200, "application/json", "{}").await;
    let cfg = config(&[
        ("url", json!(url)),
        ("method", json!("PUT")),
        ("body", json!("[row.title, user.email, row.pages * 2]")),
    ]);
    run(&catalog, &book_insert(), &cfg).await?;
    let request = received.await.unwrap();
    assert!(
        request.start_line.starts_with("PUT /hook "),
        "{}",
        request.start_line
    );
    assert_eq!(
        serde_json::from_str::<Json>(&request.body).unwrap(),
        json!(["A Book", "admin@example.com", 200])
    );
    Ok(())
}

/// The URL is a template, which is what makes "one interpolation facility" a
/// fact rather than a claim: the same `{{ }}` the email body uses addresses the
/// request, validated at save and rendered against the row at send.
#[tokio::test]
async fn a_url_template_is_rendered_against_the_row() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (url, received) = one_shot(200, "application/json", "{}").await;

    // The listener's URL ends in `/hook`; the trigger addresses the row instead.
    let base = url.trim_end_matches("/hook").to_owned();
    let cfg = config(&[
        (
            "url",
            json!(format!(
                "{base}/books/{{{{ row.id }}}}?title={{{{ row.title }}}}"
            )),
        ),
        ("method", json!("GET")),
    ]);
    run(&catalog, &book_insert(), &cfg).await?;

    let request = received.await.unwrap();
    assert!(
        request
            .start_line
            .starts_with("GET /books/7?title=A%20Book "),
        "{}",
        request.start_line
    );

    // A rendered URL is still parsed and still refused when it is not one: what a
    // row interpolates is data, and data must not choose the scheme.
    let cfg = config(&[("url", json!("{{ row.title }}"))]);
    let msg = run(&catalog, &book_insert(), &cfg)
        .await
        .unwrap_err()
        .to_string();
    assert!(msg.contains("not a valid URL"), "{msg}");
    assert!(msg.contains("A Book"), "the rendered URL is quoted: {msg}");
    Ok(())
}

/// A URL whose host a token fills in reaches the public internet only: a row
/// or a payload must not point the server at its own network and be handed
/// the answer (security review #27). The admin's own host is the admin's.
#[tokio::test]
async fn a_templated_host_reaches_only_public_addresses() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (url, _received) = one_shot(200, "application/json", "{\"secret\": 1}").await;
    let event = Event::new(EventKind::Insert)
        .on("books")
        .row(json!({ "id": 7, "title": url, "pages": 100 }))
        .caller(1, Some(json!({ "email": "admin@example.com" })));

    // The whole URL from the row: a loopback literal is refused before
    // anything is sent.
    let cfg = config(&[("url", json!("{{ row.title }}")), ("method", json!("GET"))]);
    let msg = run(&catalog, &event, &cfg).await.unwrap_err().to_string();
    assert!(msg.contains("private or reserved"), "{msg}");
    for target in [
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]:9/",
        "http://10.1.2.3/",
    ] {
        let event = Event::new(EventKind::Insert)
            .on("books")
            .row(json!({ "id": 7, "title": target, "pages": 100 }))
            .caller(1, None);
        let msg = run(&catalog, &event, &cfg).await.unwrap_err().to_string();
        assert!(msg.contains("private or reserved"), "{target}: {msg}");
    }
    // A name that resolves to loopback is refused by the resolver.
    let port = url
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches("/hook");
    let cfg = config(&[
        (
            "url",
            json!(format!("http://{{{{ row.title }}}}:{port}/hook")),
        ),
        ("method", json!("GET")),
    ]);
    let event = Event::new(EventKind::Insert)
        .on("books")
        .row(json!({ "id": 7, "title": "localhost", "pages": 100 }))
        .caller(1, None);
    let msg = run(&catalog, &event, &cfg).await.unwrap_err().to_string();
    assert!(
        msg.contains("private or reserved") || msg.contains("localhost"),
        "{msg}"
    );

    // The admin's own host, with a token in the path, still reaches it.
    let cfg = config(&[
        (
            "url",
            json!(format!(
                "{}/{{{{ row.id }}}}",
                url.trim_end_matches("/hook")
            )),
        ),
        ("method", json!("GET")),
    ]);
    let answer = run(&catalog, &book_insert(), &cfg).await?;
    assert_eq!(answer, json!({ "secret": 1 }));
    Ok(())
}

#[tokio::test]
async fn a_get_sends_no_body_and_a_text_response_comes_back_as_text() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (url, received) = one_shot(200, "text/plain", "OK").await;

    let cfg = config(&[("url", json!(url)), ("method", json!("GET"))]);
    let result = run(&catalog, &book_insert(), &cfg).await?;
    // Not JSON, not an error: an endpoint that answers `OK` did what was asked.
    assert_eq!(result, json!("OK"));

    let request = received.await.unwrap();
    assert!(
        request.start_line.starts_with("GET /hook "),
        "{}",
        request.start_line
    );
    assert_eq!(request.body, "", "a GET carries no body");
    assert_eq!(request.header("content-type"), None);
    Ok(())
}

#[tokio::test]
async fn a_non_2xx_response_names_the_status_and_quotes_the_endpoint() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (url, _received) = one_shot(500, "text/plain", "upstream exploded").await;

    let cfg = config(&[("url", json!(url))]);
    let err = run(&catalog, &book_insert(), &cfg).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("500"), "{msg}");
    // The endpoint's own explanation is the useful part, so it travels.
    assert!(msg.contains("upstream exploded"), "{msg}");
    assert!(msg.contains("notify"), "the trigger is named: {msg}");
    // The admin configured this endpoint; it answering 500 is not a server bug.
    assert_eq!(err.kind(), sc_error::ErrorKind::Application);
    Ok(())
}

#[tokio::test]
async fn a_hung_endpoint_fails_at_the_configured_timeout() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let url = hangs().await;

    let cfg = config(&[("url", json!(url)), ("timeout_ms", json!(250))]);
    let started = Instant::now();
    let err = run(&catalog, &book_insert(), &cfg).await.unwrap_err();
    let elapsed = started.elapsed();

    let msg = err.to_string();
    assert!(msg.contains("timed out"), "{msg}");
    assert!(msg.contains("notify"), "{msg}");
    // The bound is real: the caller is released, not held for the default 10s.
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    Ok(())
}

#[tokio::test]
async fn a_broken_fetch_configuration_is_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let registry = builtin_actions()?;

    /// One refusal: the configuration, and the words the message must contain.
    type Case<'a> = (&'a [(&'a str, Json)], &'a [&'a str]);

    let cases: &[Case<'_>] = &[
        // Not a URL at all, and not one this action will fetch.
        (&[("url", json!("/hook"))], &["url", "not a valid URL"]),
        (
            &[("url", json!("file:///etc/passwd"))],
            &["url", "http or https"],
        ),
        // A method the picker does not offer.
        (
            &[
                ("url", json!("https://x.test/h")),
                ("method", json!("TRACE")),
            ],
            &["method", "TRACE"],
        ),
        // A header an HTTP request cannot carry.
        (
            &[
                ("url", json!("https://x.test/h")),
                ("headers", json!({ "bad header": "x" })),
            ],
            &["headers", "bad header"],
        ),
        // A URL template naming a field the table does not have: nothing can be
        // parsed until a row is in hand, but the *token* is checked here.
        (
            &[("url", json!("https://x.test/{{ row.titel }}"))],
            &["url", "titel"],
        ),
        // An unbounded timeout is not on offer.
        (
            &[
                ("url", json!("https://x.test/h")),
                ("timeout_ms", json!(600_000)),
            ],
            &["timeout_ms", "60000"],
        ),
        // A body formula on a method that sends no body would be silently
        // dropped, so it is refused instead.
        (
            &[
                ("url", json!("https://x.test/h")),
                ("method", json!("GET")),
                ("body", json!("row")),
            ],
            &["GET", "body"],
        ),
        // And the body formula is read in the event's scope like every other.
        (
            &[("url", json!("https://x.test/h")), ("body", json!("title"))],
            &["body", "unknown identifier"],
        ),
        (
            &[
                ("url", json!("https://x.test/h")),
                ("body", json!("row.titel")),
            ],
            &["body", "titel"],
        ),
    ];

    for (entries, expected) in cases {
        let trigger = Trigger::new("notify", EventKind::Insert, "fetch")
            .on("books")
            .with_configuration(config(entries));
        let err = validate_trigger(&catalog, &registry, &trigger)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("notify") && msg.contains("fetch"), "{msg}");
        for fragment in *expected {
            assert!(msg.contains(fragment), "expected `{fragment}` in: {msg}");
        }
        assert_eq!(err.kind(), sc_error::ErrorKind::Application, "{msg}");
    }

    // The valid configuration the refusals are measured against.
    let ok = Trigger::new("notify", EventKind::Insert, "fetch")
        .on("books")
        .with_configuration(config(&[
            ("url", json!("https://x.test/hook")),
            ("method", json!("POST")),
            ("headers", json!({ "x-token": "abc" })),
            ("body", json!("row")),
            ("timeout_ms", json!(2_000)),
        ]));
    validate_trigger(&catalog, &registry, &ok).await?;
    Ok(())
}
