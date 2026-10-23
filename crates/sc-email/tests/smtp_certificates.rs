//! The certificate settings, against a **real TLS handshake**.
//!
//! Two different problems, each with its own setting:
//!
//! - a server dialled by **IP address** whose certificate is issued for a DNS
//!   name. The certificate may be perfectly valid; it is checked against the IP,
//!   which it does not carry — `certificate not valid for name "188.65.204.58"`.
//!   `smtp_certificate_name` checks it against the name it is issued for, and
//!   the check stays strict.
//! - a **self-signed** certificate, which no trusted CA issued, whatever name it
//!   is checked against. Only `smtp_certificate_check = accept_any` lets it
//!   through.
//!
//! The server is [`TestSmtp`] behind a TLS listener on `127.0.0.1`. The trusted
//! roots are a CA generated here, installed as `SSL_CERT_FILE` — which is where
//! the platform verifier reads the roots on Linux — before the first handshake
//! in this binary, so "issued by a trusted CA" is a real check against a real
//! chain.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, OnceLock};

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sc_config::{EmailSettings, SMTP_CERTIFICATE_NAME, SmtpSecurity, parse_mailbox};
use sc_email::{Email, Mailer, SmtpMailer};
use sc_test_harness::TestSmtp;
use tokio::net::{TcpListener, TcpStream};

/// A server certificate and its key, as the TLS listener wants them.
struct Served {
    chain: Vec<CertificateDer<'static>>,
    key: Vec<u8>,
}

struct Certificates {
    /// Issued for `mail.test` by the CA installed as the trusted root.
    issued: Served,
    /// Self-signed for `mail.test`: no trusted CA vouches for it.
    self_signed: Served,
}

/// The certificates, generated once, with the CA installed as the trusted root
/// before anything in this binary can build a verifier.
fn certificates() -> &'static Certificates {
    static CERTIFICATES: OnceLock<Certificates> = OnceLock::new();
    CERTIFICATES.get_or_init(|| {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let roots = std::env::temp_dir().join(format!(
            "sc-email-smtp-certificates-{}.pem",
            std::process::id()
        ));
        std::fs::write(&roots, ca.pem()).unwrap();
        // SAFETY: set once, before any test in this binary builds a verifier
        // (every test calls this first), and never changed after.
        unsafe { std::env::set_var("SSL_CERT_FILE", &roots) };

        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["mail.test".to_owned()])
            .unwrap()
            .signed_by(&leaf_key, &ca, &ca_key)
            .unwrap();
        let own = rcgen::generate_simple_self_signed(vec!["mail.test".to_owned()]).unwrap();
        Certificates {
            issued: Served {
                chain: vec![leaf.der().clone()],
                key: leaf_key.serialize_der(),
            },
            self_signed: Served {
                chain: vec![own.cert.der().clone()],
                key: own.key_pair.serialize_der(),
            },
        }
    })
}

/// Implicit TLS on a loopback port, presenting `served`, in front of a
/// [`TestSmtp`]. Returns the TLS port and the server that receives the message.
async fn tls_smtp(served: &Served) -> (u16, TestSmtp) {
    let smtp = TestSmtp::start().await.unwrap();
    let plain = smtp.port();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        served.chain.clone(),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(served.key.clone())),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        // A client that refuses the certificate ends the handshake here.
        let Ok(mut tls) = acceptor.accept(stream).await else {
            return;
        };
        let Ok(mut inner) = TcpStream::connect(("127.0.0.1", plain)).await else {
            return;
        };
        let _ = tokio::io::copy_bidirectional(&mut tls, &mut inner).await;
    });
    (port, smtp)
}

/// The installation's settings for a server dialled at `127.0.0.1` over
/// implicit TLS — the IP-address host of the report.
fn at_ip(port: u16) -> EmailSettings {
    EmailSettings {
        host: "127.0.0.1".to_owned(),
        port,
        security: SmtpSecurity::Tls,
        username: String::new(),
        password: String::new(),
        from: parse_mailbox("Saltcorn <saltcorn@example.com>").unwrap(),
        certificate_name: None,
        accept_any_certificate: false,
    }
}

fn email() -> Email {
    let mut email = Email::new(parse_mailbox("saltcorn@example.com").unwrap());
    email.to = vec![parse_mailbox("ada@example.com").unwrap()];
    email.subject = "Certificate test".to_owned();
    email.text = Some("Hello.".to_owned());
    email
}

/// Send through `settings`, and what the server received — or the error.
async fn send(settings: &EmailSettings, smtp: TestSmtp) -> Result<String, String> {
    let mailer = SmtpMailer::new(settings).map_err(|e| e.to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(20), mailer.send(&email())).await {
        Ok(Ok(())) => Ok(smtp.message().await.unwrap().transcript()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".to_owned()),
    }
}

/// The report: a host given as an IP, a certificate issued (by a trusted CA)
/// for a name. Checked against the IP it fails, and the error says which
/// setting fixes it.
#[tokio::test]
async fn an_ip_host_is_refused_a_certificate_issued_for_a_name() {
    let (port, smtp) = tls_smtp(&certificates().issued).await;
    let err = send(&at_ip(port), smtp).await.unwrap_err();
    assert!(err.contains("invalid peer certificate"), "{err}");
    assert!(err.contains("127.0.0.1"), "{err}");
    assert!(err.contains(SMTP_CERTIFICATE_NAME), "{err}");
}

/// The certificate name: the connection goes to the IP, the certificate is
/// checked — strictly — against the name it is issued for.
#[tokio::test]
async fn the_certificate_name_checks_an_ip_host_against_that_name() {
    let (port, smtp) = tls_smtp(&certificates().issued).await;
    let settings = EmailSettings {
        certificate_name: Some("mail.test".to_owned()),
        ..at_ip(port)
    };
    let transcript = send(&settings, smtp).await.unwrap();
    assert!(
        transcript.contains("RCPT TO:<ada@example.com>"),
        "{transcript}"
    );

    // And it is a check: a name the certificate is not issued for is refused.
    let (port, smtp) = tls_smtp(&certificates().issued).await;
    let settings = EmailSettings {
        certificate_name: Some("other.test".to_owned()),
        ..at_ip(port)
    };
    let err = send(&settings, smtp).await.unwrap_err();
    assert!(err.contains("invalid peer certificate"), "{err}");
}

/// A self-signed certificate is a different problem: the right name does not
/// help, because no trusted CA issued it. `accept_any` lets it through — and
/// with it the IP host, with no certificate name.
#[tokio::test]
async fn a_self_signed_certificate_needs_accept_any() {
    let (port, smtp) = tls_smtp(&certificates().self_signed).await;
    let settings_named = EmailSettings {
        certificate_name: Some("mail.test".to_owned()),
        ..at_ip(port)
    };
    let err = send(&settings_named, smtp).await.unwrap_err();
    assert!(err.contains("invalid peer certificate"), "{err}");

    let (port, smtp) = tls_smtp(&certificates().self_signed).await;
    let settings = EmailSettings {
        accept_any_certificate: true,
        ..at_ip(port)
    };
    let transcript = send(&settings, smtp).await.unwrap();
    assert!(
        transcript.contains("RCPT TO:<ada@example.com>"),
        "{transcript}"
    );
}
