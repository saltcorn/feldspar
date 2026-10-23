//! One test against a **real SMTP conversation**.
//!
//! Everything else in this crate can be asserted against a trait, and a
//! trait-only test proves nothing about the question that actually matters here:
//! whether `lettre` was wired up correctly — whether the port is the one the
//! settings named, whether credentials are attached when a username is
//! configured, whether the message that arrives is the one that was built. So
//! these run against [`TestSmtp`], which binds a listener on `127.0.0.1` and
//! speaks enough SMTP to accept a message, and assert on what the server
//! *received*.
//!
//! `none` security throughout, because the point is the SMTP conversation and
//! not the TLS handshake: `starttls` and `tls` are two of lettre's builders in
//! `SmtpMailer::new`, and a self-signed certificate here would exercise rustls
//! rather than this crate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_config::{EmailSettings, SmtpSecurity, parse_mailbox};
use sc_email::{Email, Mailer, SmtpMailer};
use sc_test_harness::TestSmtp;

/// Settings pointing at a local listener, with or without credentials.
fn settings(port: u16, username: &str) -> EmailSettings {
    EmailSettings {
        host: "127.0.0.1".to_owned(),
        port,
        security: SmtpSecurity::None,
        username: username.to_owned(),
        password: if username.is_empty() {
            String::new()
        } else {
            "hunter2".to_owned()
        },
        from: parse_mailbox("Saltcorn <saltcorn@example.com>").unwrap(),
        certificate_name: None,
        accept_any_certificate: false,
    }
}

/// A message actually crosses a socket, and what arrives is what was built.
#[tokio::test]
async fn a_message_reaches_an_smtp_server() -> sc_error::Result<()> {
    let server = TestSmtp::start().await?;
    let settings = settings(server.port(), "");
    let mailer = SmtpMailer::new(&settings)?;

    let email = Email {
        from: settings.from.clone(),
        to: vec![parse_mailbox("ada@example.com")?],
        cc: Vec::new(),
        bcc: vec![parse_mailbox("audit@example.com")?],
        subject: "Receipt for order 7".to_owned(),
        text: Some("Thank you.".to_owned()),
        html: Some("<p>Thank you.</p>".to_owned()),
        // A file travels too: base64 of arbitrary bytes across a protocol that
        // is line-oriented and 7-bit is exactly the sort of thing a trait-only
        // test cannot tell you went well.
        attachments: vec![sc_email::Attachment {
            filename: "invoice-7.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            bytes: b"%PDF-1.4 \r\n.\r\n binary\x00bytes".to_vec(),
        }],
    };
    mailer.send(&email).await?;

    let received = server.message().await?;
    let transcript = received.transcript();

    // The envelope: the from-address, and *both* recipients — a blind copy is a
    // recipient even though it is not a header, which is the one place those two
    // ideas come apart.
    assert!(
        transcript.contains("MAIL FROM:<saltcorn@example.com>"),
        "{transcript}"
    );
    assert!(
        transcript.contains("RCPT TO:<ada@example.com>"),
        "{transcript}"
    );
    assert!(
        transcript.contains("RCPT TO:<audit@example.com>"),
        "{transcript}"
    );

    // The message itself.
    let body = received.body;
    assert!(body.contains("Subject: Receipt for order 7"), "{body}");
    assert!(body.contains("To: ada@example.com"), "{body}");
    assert!(body.contains("multipart/alternative"), "{body}");
    // The attachment arrived, named and typed, and the bytes that would have
    // ended the DATA section early (a lone `.` on its own line) did not: they are
    // base64 by the time they reach the wire.
    assert!(body.contains("multipart/mixed"), "{body}");
    assert!(body.contains(r#"filename="invoice-7.pdf""#), "{body}");
    assert!(body.contains("application/pdf"), "{body}");
    // The blind copy is a recipient and not a header: it must not appear in
    // anything the other recipients can read.
    assert!(!body.contains("audit@example.com"), "{body}");
    Ok(())
}

/// Credentials are attached when a username is configured, and not otherwise.
///
/// Asserted over the wire rather than over the builder, because "did lettre
/// actually authenticate" is not a question the builder's type answers.
#[tokio::test]
async fn credentials_are_offered_only_when_configured() -> sc_error::Result<()> {
    for (username, expect_auth) in [("", false), ("ada", true)] {
        let server = TestSmtp::start().await?;
        let settings = settings(server.port(), username);
        let mailer = SmtpMailer::new(&settings)?;

        let mut email = Email::new(settings.from.clone());
        email.to = vec![parse_mailbox("ada@example.com")?];
        email.subject = "Hello".to_owned();
        email.text = Some("Hello.".to_owned());
        mailer.send(&email).await?;

        let received = server.message().await?;
        assert_eq!(
            received.authenticated(),
            expect_auth,
            "username `{username}`: {}",
            received.transcript()
        );
    }
    Ok(())
}

/// A server that is not there is the transport's own error, not a hang and not a
/// success — which is what decision 12 means by "a failure is the button's
/// answer".
#[tokio::test]
async fn a_refused_connection_is_the_transports_own_error() -> sc_error::Result<()> {
    // A bare listener bound and dropped, so the port is one nothing is listening
    // on. Deliberately not a dropped `TestSmtp`: that would test whether the
    // harness stops accepting, which is not what this is about.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local listener");
        listener.local_addr().expect("a bound address").port()
    };
    let settings = settings(port, "");
    let mailer = SmtpMailer::new(&settings)?;
    let mut email = Email::new(settings.from.clone());
    email.to = vec![parse_mailbox("ada@example.com")?];
    email.text = Some("Hello.".to_owned());

    let err = mailer
        .send(&email)
        .await
        .expect_err("nothing is listening there")
        .to_string();
    assert!(err.contains("mail server"), "{err}");
    Ok(())
}

/// An SMTP override's transport connects to **the address that was checked**
/// and not to wherever its host name resolves (TODO-imap M3.3): the name here
/// is one that resolves nowhere (`.invalid`, RFC 2606), so the message can
/// only arrive if the transport never looked it up. The EHLO name is the
/// sender's domain, not the machine's.
#[tokio::test]
async fn a_pinned_transport_connects_to_the_checked_address_not_the_name() -> sc_error::Result<()> {
    let server = TestSmtp::start().await?;
    let settings = EmailSettings {
        host: "smtp.example.invalid".to_owned(),
        username: "orders@example.com".to_owned(),
        password: "Grüße-€-pässwort".to_owned(),
        from: parse_mailbox("Orders <orders@example.com>").unwrap(),
        ..settings(server.port(), "")
    };
    let mailer = SmtpMailer::pinned(&settings, "127.0.0.1".parse().unwrap())?;
    let mut email = Email::new(settings.from.clone());
    email.to = vec![parse_mailbox("ada@example.com")?];
    email.subject = "Your order has shipped".to_owned();
    email.text = Some("It is on its way.".to_owned());
    mailer.send(&email).await?;

    let received = server.message().await?;
    let transcript = received.transcript();
    assert!(transcript.contains("EHLO example.com"), "{transcript}");
    assert!(received.authenticated(), "{transcript}");
    assert!(
        transcript.contains("MAIL FROM:<orders@example.com>"),
        "{transcript}"
    );
    Ok(())
}

/// The override's way in, [`Mailer::send_via`], holds the server to the relay
/// rules before anything is connected to: loopback is refused (a local
/// listener *is* there, and is not reached), and the error names the host and
/// never the password.
#[tokio::test]
async fn an_override_through_a_loopback_server_is_refused() -> sc_error::Result<()> {
    let server = TestSmtp::start().await?;
    let relay = EmailSettings {
        host: "127.0.0.1".to_owned(),
        port: 587,
        security: SmtpSecurity::StartTls,
        username: "orders@example.com".to_owned(),
        password: "Grüße-€-pässwort".to_owned(),
        from: parse_mailbox("orders@example.com").unwrap(),
        certificate_name: None,
        accept_any_certificate: false,
    };
    let recorder = sc_email::RecordingMailer::new();
    let mut email = Email::new(relay.from.clone());
    email.to = vec![parse_mailbox("ada@example.com")?];
    email.text = Some("Hello.".to_owned());
    // Through the trait's own default: what `SettingsMailer` and `SmtpMailer`
    // do with an override.
    let mailer = SmtpMailer::new(&settings(server.port(), ""))?;
    let err = mailer
        .send_via(&relay, &email)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("127.0.0.1"), "{err}");
    assert!(err.contains("private or reserved"), "{err}");
    assert!(!err.contains("pässwort"), "{err}");
    // The recorder checks the port and encryption only — DNS is not its to ask.
    recorder.send_via(&relay, &email).await?;
    assert_eq!(recorder.servers().len(), 1);
    Ok(())
}
