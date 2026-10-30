//! Terminating TLS in-process: pasted certificates and ACME (design §13.5).
//!
//! The server serves HTTPS itself, so a deployment needs no reverse proxy in
//! front of it — and the admin UI and every application get the same treatment,
//! because they are one listener with a `Host` router behind it. Certificates
//! come from one of two places, chosen by the `ssl_mode` setting:
//!
//! - **`custom`** — a chain and a key the admin pasted into the settings screen.
//!   They are parsed into a [`rustls::ServerConfig`] and that is the whole of it:
//!   no ACME traffic, nothing to renew.
//! - **`letsencrypt`** — [`rustls_acme`] obtains and renews certificates from an
//!   ACME CA, answering the **TLS-ALPN-01** challenge inside the handshake it is
//!   already terminating. That choice of challenge is why there is no `/.well-
//!   known/acme-challenge` route anywhere in the router: the validation never
//!   reaches HTTP, so there is nothing to keep in step and nothing an
//!   application's own routes could shadow.
//!
//! Both arrive at the listener as an `axum_server` acceptor, which is why this
//! crate serves through `axum-server` when TLS is on and through `axum::serve`
//! when it is off — one accept seam, two ways of filling it.
//!
//! **ALPN advertises `http/1.1` only, on purpose.** The admin UI's agent chat
//! and the IDE's language server are WebSockets, and a WebSocket over HTTP/2
//! needs RFC 8441's extended CONNECT, which axum's `ws` does not implement.
//! Advertising `h2` would therefore trade two working features for a
//! multiplexing win on an admin console. If HTTP/2 is wanted for an
//! application's API, that is a reverse proxy's job today. In ACME mode
//! `acme-tls/1` is advertised beside it, because the same listener answers the
//! CA's validation handshake: a client that offers only that protocol is the CA,
//! and it is served the challenge certificate the resolver holds.
//!
//! **The certificates are checked where they are pasted.** [`check_certificate`]
//! is what the settings endpoint calls before saving: a key that does not match
//! its chain fails in front of the admin, not at the next restart when nobody is
//! watching and the symptom is a server that will not bind.
//!
//! **The ACME name set is live.** A certificate covers the base domain, every
//! mounted application's subdomain and whatever else the admin listed — and an
//! application is created while the server runs, so that set changes while the
//! server runs. [`AcmeCertificate`] is what makes it a new order rather than a
//! restart: [`AppMounts`](crate::AppMounts) reports every mount and unmount to
//! it (through the [`Certificate`] seam), and a name that is not covered yet
//! starts a fresh order for the union of the old names and the new one. Until
//! that order finishes the **previous certificate keeps serving**, so adding an
//! application never takes the ones already up off the air.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_acme::{AcmeConfig, AcmeState};
use sc_config::{AcmeCache, SslMode, SslSettings};
use sc_error::{Error, Result};

/// The shutdown handle of a running TLS server.
///
/// `axum_server::Handle` is generic over the kind of address its server is
/// bound to; every listener here is a TCP one, so this names that once instead
/// of spelling the parameter at each use.
pub type TlsHandle = axum_server::Handle<SocketAddr>;

/// The one ALPN protocol the server advertises. See the module docs for why
/// `h2` is not in this list.
const ALPN_HTTP11: &[u8] = b"http/1.1";

/// How long in-flight requests have to finish after a shutdown signal.
const GRACEFUL_SECONDS: u64 = 10;

/// How this server obtains the certificate it serves.
///
/// Built from the stored [`SslSettings`] plus the two things only the running
/// server knows: which domains it answers for, and which database the ACME
/// account and certificates are cached in.
#[derive(Clone)]
pub enum TlsSettings {
    /// Plain HTTP: no TLS listener, no redirect.
    Off,
    /// An admin-supplied certificate chain and private key, both PEM.
    Custom {
        /// The PEM chain: server certificate first, then intermediates.
        certificate: String,
        /// The PEM private key (PKCS#8, PKCS#1 or SEC1).
        private_key: String,
        /// The port TLS is served on.
        port: u16,
        /// Whether the plain-HTTP listener redirects here.
        redirect_http: bool,
    },
    /// Certificates obtained and renewed from an ACME CA.
    Acme {
        /// The live certificate: the ACME client, the names it currently covers,
        /// and the resolver the listener hands every handshake to. Shared rather
        /// than owned, because the mount registry holds the same handle and adds
        /// a name to it when an application is mounted.
        certificate: Arc<AcmeCertificate>,
        /// The port TLS is served on. Reaching the ACME CA's TLS-ALPN-01
        /// validation means this must be **443** on a public address.
        port: u16,
        /// Whether the plain-HTTP listener redirects here.
        redirect_http: bool,
    },
}

impl std::fmt::Debug for TlsSettings {
    /// Prints what is configured, never the private key. A `Debug` that dumps
    /// key material is a key in a log file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsSettings::Off => f.write_str("TlsSettings::Off"),
            TlsSettings::Custom { port, .. } => f
                .debug_struct("TlsSettings::Custom")
                .field("port", port)
                .field("certificate", &"<pem>")
                .field("private_key", &"<redacted>")
                .finish(),
            TlsSettings::Acme {
                certificate, port, ..
            } => f
                .debug_struct("TlsSettings::Acme")
                .field("domains", &certificate.domains())
                .field("contact_email", &certificate.contact_email)
                .field("directory_url", &certificate.directory_url)
                .field("port", port)
                .finish(),
        }
    }
}

impl TlsSettings {
    /// Whether a TLS listener is served at all.
    pub fn enabled(&self) -> bool {
        !matches!(self, TlsSettings::Off)
    }

    /// The port TLS is served on, if any.
    pub fn port(&self) -> Option<u16> {
        match self {
            TlsSettings::Off => None,
            TlsSettings::Custom { port, .. } | TlsSettings::Acme { port, .. } => Some(*port),
        }
    }

    /// Whether the plain-HTTP listener answers with a redirect instead of the
    /// application.
    pub fn redirect_http(&self) -> bool {
        match self {
            TlsSettings::Off => false,
            TlsSettings::Custom { redirect_http, .. } | TlsSettings::Acme { redirect_http, .. } => {
                *redirect_http
            }
        }
    }

    /// Build the serving plan from the stored settings.
    ///
    /// `names` is where the ACME order's names come from — the base domain, the
    /// mounted subdomains and the admin's extras (see [`TlsNames`]) — and
    /// `cache` is where its results are kept. Both are ignored outside
    /// `letsencrypt` mode, and both are **required** in it: an order with no
    /// names is not an order, and a client with no cache would order a fresh
    /// certificate on every boot, which is how a deployment meets Let's
    /// Encrypt's rate limits.
    ///
    /// The names are kept rather than flattened, because the set is live: the
    /// [`AcmeCertificate`] this builds is what a later mount adds a subdomain
    /// to, and it needs the base domain to make a name out of one.
    pub fn from_ssl(
        settings: &SslSettings,
        https_port: u16,
        names: TlsNames,
        cache: Option<AcmeCache>,
    ) -> Result<TlsSettings> {
        settings.check()?;
        match settings.mode {
            // The proxy in front holds the certificates; this process speaks
            // plain HTTP to it.
            SslMode::Off | SslMode::Proxy => Ok(TlsSettings::Off),
            SslMode::Custom => {
                check_certificate(&settings.certificate, &settings.private_key)?;
                Ok(TlsSettings::Custom {
                    certificate: settings.certificate.clone(),
                    private_key: settings.private_key.clone(),
                    port: https_port,
                    redirect_http: settings.redirect_http,
                })
            }
            SslMode::LetsEncrypt => {
                if names.domains().is_empty() {
                    return Err(Error::config(
                        "TLS is set to `letsencrypt` but this server has no domain to certify: \
                         set a base domain (`--base-domain`) or list the names in \
                         `ssl_extra_domains`",
                    ));
                }
                let cache = cache.ok_or_else(|| {
                    Error::config("ACME needs a database to cache its account and certificates in")
                })?;
                Ok(TlsSettings::Acme {
                    certificate: AcmeCertificate::new(
                        names,
                        settings.contact_email.clone(),
                        settings.directory_url.clone(),
                        cache,
                    ),
                    port: https_port,
                    redirect_http: settings.redirect_http,
                })
            }
        }
    }
}

/// Where a certificate's names come from, kept apart so the set can be
/// recomputed while the server runs.
///
/// The admin lists only `extra`. The base domain is where the admin UI is and
/// every application is a subdomain of it (§13.2), so those two are derived from
/// what the server is configured with and from what it currently serves — and
/// *what it currently serves* is the part that changes without a restart, which
/// is why this keeps the three sources rather than the one flat list
/// [`tls_domains`] makes of them.
#[derive(Clone, Debug, Default)]
pub struct TlsNames {
    /// The domain applications are subdomains of, if this deployment has one.
    base_domain: Option<String>,
    /// The application subdomains mounted when the plan was built.
    subdomains: Vec<String>,
    /// The names the admin listed in `ssl_extra_domains`.
    extra: Vec<String>,
}

impl TlsNames {
    /// The three sources, as the boot path has them.
    pub fn new(
        base_domain: Option<String>,
        subdomains: Vec<String>,
        extra: Vec<String>,
    ) -> TlsNames {
        TlsNames {
            base_domain,
            subdomains,
            extra,
        }
    }

    /// Every name, flattened — what an ACME order covers.
    pub fn domains(&self) -> Vec<String> {
        tls_domains(self.base_domain.as_deref(), &self.subdomains, &self.extra)
    }

    /// Every name, with `subdomains` in place of the ones this was built with —
    /// what the certificate should cover now that the mount registry holds these.
    pub fn domains_for(&self, subdomains: &[String]) -> Vec<String> {
        tls_domains(self.base_domain.as_deref(), subdomains, &self.extra)
    }
}

/// Every name this server should hold a certificate for.
///
/// The admin does not list the obvious ones: the base domain is where the admin
/// UI is, and every application is a subdomain of it (§13.2), so both are
/// derived from what the server is already configured with. `extra` is for the
/// names that are neither — an apex the deployment also answers to, a vanity
/// domain pointed at one application.
///
/// De-duplicated and lower-cased, because a SAN list is a set.
pub fn tls_domains(
    base_domain: Option<&str>,
    subdomains: &[String],
    extra: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    };
    if let Some(base) = base_domain.map(str::trim).filter(|b| !b.is_empty()) {
        push(base.to_owned());
        for subdomain in subdomains {
            // `@` is the application on the base domain itself, which is
            // already the first name.
            if subdomain != sc_config::ROOT_SUBDOMAIN {
                push(format!("{subdomain}.{base}"));
            }
        }
    }
    for name in extra {
        push(name.clone());
    }
    out
}

/// Install the process-wide rustls crypto provider.
///
/// Two providers reach this binary — `aws-lc-rs` here and `ring` through other
/// dependencies — and rustls refuses to guess between them, failing at the first
/// handshake rather than at startup. Naming one at boot is what turns that into
/// a decision. Idempotent: a second call (another test in the same process) is a
/// no-op.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Parse a PEM chain and key into a rustls server configuration.
///
/// Every failure names what is wrong with the *paste* — no certificate in it, no
/// key in it, a key that does not go with the chain — because that is what the
/// admin has in front of them.
pub fn certificate_config(certificate: &str, private_key: &str) -> Result<ServerConfig> {
    install_crypto_provider();

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut certificate.trim().as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::invalid(format!("the certificate is not valid PEM: {e}")))?;
    if certs.is_empty() {
        return Err(Error::invalid(
            "no certificate found: the chain should start with a \
             `-----BEGIN CERTIFICATE-----` block",
        ));
    }

    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut private_key.trim().as_bytes())
            .map_err(|e| Error::invalid(format!("the private key is not valid PEM: {e}")))?
            .ok_or_else(|| {
                Error::invalid(
                    "no private key found: expected a PKCS#8 (`BEGIN PRIVATE KEY`), \
                     PKCS#1 (`BEGIN RSA PRIVATE KEY`) or SEC1 (`BEGIN EC PRIVATE KEY`) block",
                )
            })?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        // rustls checks here that the key matches the leaf certificate, which is
        // the mistake a paste actually makes: two files, copied one at a time.
        .map_err(|e| Error::invalid(format!("certificate and private key do not match: {e}")))?;
    config.alpn_protocols = vec![ALPN_HTTP11.to_vec()];
    Ok(config)
}

/// Check a pasted certificate and key without keeping the result — what the
/// settings endpoint calls before it saves.
pub fn check_certificate(certificate: &str, private_key: &str) -> Result<()> {
    certificate_config(certificate, private_key).map(|_| ())
}

/// Serve `app` over TLS on an already-bound listener until `handle` is told to
/// shut down.
///
/// The listener is bound by the caller so a port that cannot be bound is
/// reported by whoever chose it, with its address in the message, before
/// anything else is started.
pub async fn serve_https(
    listener: std::net::TcpListener,
    app: Router,
    tls: &TlsSettings,
    handle: TlsHandle,
) -> Result<()> {
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::msg(format!("preparing the TLS listener: {e}")))?;

    match tls {
        TlsSettings::Off => Err(Error::config("serve_https called with TLS off")),
        TlsSettings::Custom {
            certificate,
            private_key,
            ..
        } => {
            let config = certificate_config(certificate, private_key)?;
            let acceptor = RustlsAcceptor::new(RustlsConfig::from_config(Arc::new(config)));
            axum_server::from_tcp(listener)
                .map_err(|e| Error::msg(format!("serving TLS: {e}")))?
                .acceptor(acceptor)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .map_err(|e| Error::msg(format!("TLS server error: {e}")))
        }
        TlsSettings::Acme { certificate, .. } => {
            // The first order. The ACME state is a stream of events that only
            // makes progress while something polls it, so `start` spawns the task
            // that *is* the certificate's provisioning and renewal. Its outcomes
            // are logged rather than fatal — a CA that is briefly unreachable
            // must not take the server with it, and a cached certificate keeps
            // serving meanwhile — and a later mount replaces that task with one
            // ordering a certificate that also covers the new subdomain.
            certificate.start();
            let acceptor = RustlsAcceptor::new(RustlsConfig::from_config(Arc::new(
                certificate.server_config(),
            )));
            axum_server::from_tcp(listener)
                .map_err(|e| Error::msg(format!("serving TLS: {e}")))?
                .acceptor(acceptor)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .map_err(|e| Error::msg(format!("TLS server error: {e}")))
        }
    }
}

/// The seam a live certificate is kept in step through: what the mount registry
/// tells the thing holding the certificate when the set of served subdomains
/// changes.
///
/// A trait rather than the concrete [`AcmeCertificate`] because most servers have
/// no certificate at all — TLS off, or a pasted one, which covers whatever names
/// the admin's certificate covers and has nothing to reorder — and because
/// [`AppMounts`](crate::AppMounts) must be testable without an ACME client.
pub trait Certificate: Send + Sync {
    /// These are the subdomains served now. A name among them that the
    /// certificate does not cover should be ordered.
    ///
    /// **Called from a mount**, so it must not block: the implementation decides
    /// and spawns, it does not wait for a CA.
    fn subdomains_changed(&self, subdomains: &[String]);

    /// Whether a handshake for `host` is answered with a certificate for it
    /// yet — what the admin UI's move to a new subdomain waits on before it
    /// lets the admin follow it (Settings → Development).
    ///
    /// The default is for a certificate that does not track names: it covers
    /// what it covers, and there is nothing to wait for.
    fn status_for(&self, _host: &str) -> CertificateStatus {
        CertificateStatus::ready()
    }
}

/// Where the certificate for one host name has got to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CertificateStatus {
    /// `plain_http` (TLS is off: nothing to obtain), `ready`, `ordering`, or
    /// `not_covered` (no certificate this server holds names the host, and
    /// none is being ordered).
    pub state: &'static str,
    /// What the CA last said, while ordering; why not, when not covered.
    pub message: Option<String>,
}

impl CertificateStatus {
    /// TLS is off: the host is served over plain HTTP and needs nothing.
    pub fn plain_http() -> CertificateStatus {
        CertificateStatus {
            state: "plain_http",
            message: None,
        }
    }

    /// A handshake for the host is answered with a certificate naming it.
    pub fn ready() -> CertificateStatus {
        CertificateStatus {
            state: "ready",
            message: None,
        }
    }

    /// Whether the host can be served now, with or without TLS.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, "ready" | "plain_http")
    }
}

/// An admin-supplied certificate, as the mount registry sees it: it orders
/// nothing, and it covers the names written into it.
///
/// What it adds over having no [`Certificate`] at all is the answer to "does it
/// cover this host?", which is the difference between the admin UI moving to a
/// name a browser accepts and moving to one it refuses.
pub struct PastedCertificate {
    /// The leaf certificate, DER.
    leaf: Option<CertificateDer<'static>>,
}

impl PastedCertificate {
    /// The certificate whose chain is `pem` (the leaf first, as pasted).
    pub fn new(pem: &str) -> PastedCertificate {
        let leaf = rustls_pemfile::certs(&mut pem.trim().as_bytes())
            .next()
            .and_then(|cert| cert.ok());
        PastedCertificate { leaf }
    }
}

impl Certificate for PastedCertificate {
    fn subdomains_changed(&self, _subdomains: &[String]) {}

    fn status_for(&self, host: &str) -> CertificateStatus {
        let covered = self.leaf.as_ref().is_some_and(|leaf| {
            let (Ok(cert), Ok(name)) = (
                webpki::EndEntityCert::try_from(leaf),
                rustls::pki_types::ServerName::try_from(host),
            ) else {
                return false;
            };
            cert.verify_is_valid_for_subject_name(&name).is_ok()
        });
        if covered {
            CertificateStatus::ready()
        } else {
            CertificateStatus {
                state: "not_covered",
                message: Some(format!(
                    "the certificate pasted in Settings → SSL / TLS does not name {host}: paste \
                     one that does (a wildcard for the base domain covers every subdomain)"
                )),
            }
        }
    }
}

/// An ACME certificate whose **name set is live** (design §13.5).
///
/// One of these exists per TLS server in `letsencrypt` mode. It owns the ACME
/// client — and replaces it when a name is added, because `rustls_acme` takes its
/// domain list at construction and the list is part of the cache key — while the
/// listener keeps the one resolver it was built with. That indirection is the
/// whole trick: the [`LiveAcmeResolver`] the handshake asks is stable, and what
/// it delegates to is swapped underneath.
///
/// **A new order never interrupts the old certificate.** The previous client's
/// resolver is kept as the fallback and answers every handshake until the new
/// one has deployed a certificate, which is a minute of ACME traffic away. So an
/// admin who creates an application does not take the running ones off the air.
///
/// **The name set only grows while the process runs.** A deleted application's
/// name stays on the certificate until the next restart, where the set is
/// recomputed from what is actually mounted. Ordering a smaller certificate buys
/// nothing — the name resolves to nothing either way — and each order is charged
/// against the CA's rate limits.
pub struct AcmeCertificate {
    /// Where the names come from, so a new subdomain can be made into one.
    names: TlsNames,
    /// The account contact, without the `mailto:` scheme.
    contact_email: String,
    /// The CA's directory URL.
    directory_url: String,
    /// Where the account key and the issued certificates are kept, so a renewal
    /// survives a restart and a second node does not order its own.
    cache: AcmeCache,
    /// What every handshake resolves against, for the life of the listener.
    resolver: Arc<LiveAcmeResolver>,
    /// The order in force, and the one before it.
    orders: Mutex<Orders>,
}

/// The ACME clients this certificate is running.
///
/// Two, briefly: the one ordering the names that are wanted now, and the one
/// whose certificate is keeping the server up while it does. Both are **driven**
/// — the previous client's task is not aborted when it is replaced, because it is
/// the thing that renews the certificate still being served. A new order that
/// never succeeds (a subdomain whose DNS was never pointed here, say) therefore
/// costs an ACME client, not the expiry of the certificate that works.
#[derive(Default)]
struct Orders {
    /// The order in force.
    current: Option<Order>,
    /// The task driving the client before it, kept alive for its renewal. Only
    /// one: an older generation is no longer consulted by the resolver, so its
    /// renewals would be work nobody reads.
    previous: Option<tokio::task::JoinHandle<()>>,
}

/// One ACME client: what it covers, and the task that is its progress.
struct Order {
    /// The names ordered, in the order they were ordered in (the cache key).
    domains: Vec<String>,
    /// The task polling the ACME state — dropping it would stop the renewal, so
    /// it is held here and aborted only when it is replaced.
    task: tokio::task::JoinHandle<()>,
    /// Whether it has a certificate yet, and what the CA last said.
    progress: Arc<OrderProgress>,
}

/// What one order's driving task has seen, for whoever is waiting on it.
#[derive(Debug, Default)]
struct OrderProgress {
    /// Set on the first deployed certificate. The same flag the resolver's
    /// generation reads.
    ready: Arc<AtomicBool>,
    /// The last error the ACME client reported, cleared by the next success.
    last_error: Mutex<Option<String>>,
}

impl AcmeCertificate {
    /// The certificate for these names, with nothing ordered yet: an ACME client
    /// is built by [`start`](Self::start), which the serving path calls once it
    /// is inside a runtime.
    pub fn new(
        names: TlsNames,
        contact_email: String,
        directory_url: String,
        cache: AcmeCache,
    ) -> Arc<AcmeCertificate> {
        Arc::new(AcmeCertificate {
            names,
            contact_email,
            directory_url,
            cache,
            resolver: Arc::new(LiveAcmeResolver::default()),
            orders: Mutex::new(Orders::default()),
        })
    }

    /// Order the names this was built with — the boot path's first order.
    pub fn start(&self) {
        self.order(self.names.domains());
    }

    /// The names the certificate in force covers, sorted as they were ordered.
    /// Empty before [`start`](Self::start).
    pub fn domains(&self) -> Vec<String> {
        self.lock()
            .current
            .as_ref()
            .map(|order| order.domains.clone())
            .unwrap_or_default()
    }

    /// The server configuration the listener serves with: one resolver for the
    /// life of the process, and `acme-tls/1` beside `http/1.1` so the CA's
    /// validation handshake reaches the same resolver (see the module docs).
    pub fn server_config(&self) -> ServerConfig {
        install_crypto_provider();
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(self.resolver.clone());
        config.alpn_protocols = vec![
            ALPN_HTTP11.to_vec(),
            rustls_acme::acme::ACME_TLS_ALPN_NAME.to_vec(),
        ];
        config
    }

    /// Order `domains` (canonicalised), replacing whatever was in force.
    ///
    /// Silently does nothing outside a tokio runtime: the ACME client *is* the
    /// task that polls it, so with nowhere to spawn there is no client to build.
    /// That is the case in a unit test, not in a server.
    fn order(&self, domains: Vec<String>) {
        let domains = canonical_domains(domains);
        if domains.is_empty() {
            return;
        }
        // Idempotent: the same names again are the certificate already in force,
        // and re-ordering them would spend a rate limit on a certificate the
        // cache is holding.
        if self
            .lock()
            .current
            .as_ref()
            .is_some_and(|o| o.domains == domains)
        {
            return;
        }
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                eprintln!(
                    "feldspar: acme: no runtime to order a certificate for {} in",
                    domains.join(", ")
                );
                return;
            }
        };
        let state = acme_state(
            &domains,
            &self.contact_email,
            &self.directory_url,
            self.cache.clone(),
        );
        // Installed *before* the task is spawned, so no event can land on a
        // generation the resolver has not heard of.
        let progress = Arc::new(OrderProgress {
            ready: self.resolver.install(state.resolver()),
            last_error: Mutex::new(None),
        });
        let task = runtime.spawn(drive_acme(state, progress.clone()));
        eprintln!(
            "feldspar: acme: ordering a certificate for {}",
            domains.join(", ")
        );
        let mut orders = self.lock();
        if let Some(replaced) = orders.current.replace(Order {
            domains,
            task,
            progress,
        }) {
            // Its resolver is still the fallback and its client still renews what
            // that resolver is serving, so its task is kept — see [`Orders`]. The
            // generation *before* it is not consulted any more, so that one's is
            // dropped.
            if let Some(stale) = orders.previous.replace(replaced.task) {
                stale.abort();
            }
        }
    }

    /// The order lock, recovering from a poisoned one: a panic while ordering
    /// must not stop the server serving the certificate it already has.
    fn lock(&self) -> MutexGuard<'_, Orders> {
        self.orders.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Certificate for AcmeCertificate {
    fn subdomains_changed(&self, subdomains: &[String]) {
        let wanted = self.names.domains_for(subdomains);
        let covered = self.domains();
        let missing: Vec<&String> = wanted.iter().filter(|n| !covered.contains(n)).collect();
        if missing.is_empty() {
            return;
        }
        eprintln!(
            "feldspar: acme: {} {} not on the certificate yet",
            missing
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if missing.len() == 1 { "is" } else { "are" }
        );
        // The union, not the wanted set: a name that has stopped being served
        // stays on the certificate until the next restart (see the type docs).
        let mut names = covered;
        for name in wanted {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        self.order(names);
    }

    fn status_for(&self, host: &str) -> CertificateStatus {
        let host = host.to_ascii_lowercase();
        let orders = self.lock();
        let Some(order) = orders
            .current
            .as_ref()
            .filter(|o| o.domains.contains(&host))
        else {
            return CertificateStatus {
                state: "not_covered",
                message: Some(format!("no certificate has been ordered for {host}")),
            };
        };
        if order.progress.ready.load(Ordering::Relaxed) {
            return CertificateStatus::ready();
        }
        CertificateStatus {
            state: "ordering",
            message: order
                .progress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
}

impl std::fmt::Debug for AcmeCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeCertificate")
            .field("domains", &self.domains())
            .field("directory_url", &self.directory_url)
            .finish_non_exhaustive()
    }
}

/// One name set in one order, whatever order the names arrived in.
///
/// The first name is left where it is — it is the base domain, the one a
/// certificate is *about* — and the rest are sorted and de-duplicated. The point
/// is the **cache key**: `rustls_acme` digests the list, so two ways of arriving
/// at the same set must produce the same list or a restart would order a
/// certificate it already has cached.
fn canonical_domains(domains: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(domains.len());
    let mut names = domains.into_iter();
    if let Some(first) = names.next() {
        out.push(first);
    }
    let mut rest: Vec<String> = names.collect();
    rest.sort();
    rest.dedup();
    for name in rest {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The certificate resolver the listener is built with, whose ACME client can be
/// replaced under it.
///
/// It holds two generations. `current` is the newest client — the one whose
/// challenge the CA is validating right now — and `previous` is the one that was
/// serving before it, kept because the new one has no certificate for the first
/// minute of its life.
#[derive(Debug, Default)]
struct LiveAcmeResolver {
    generations: RwLock<Generations>,
}

/// The two ACME clients a handshake may be answered from.
#[derive(Debug, Default)]
struct Generations {
    current: Option<Generation>,
    previous: Option<Generation>,
}

/// One ACME client's resolver, and whether it has deployed a certificate yet.
#[derive(Debug)]
struct Generation {
    resolver: Arc<rustls_acme::ResolvesServerCertAcme>,
    /// Set by the driving task on the first `DeployedCachedCert`/`DeployedNewCert`.
    /// Until then this generation has no certificate and asking it would answer a
    /// handshake with nothing.
    ready: Arc<AtomicBool>,
}

impl LiveAcmeResolver {
    /// Make `resolver` the current generation, keeping the one it replaces as the
    /// fallback. Returns the flag its driving task sets when it has a certificate.
    fn install(&self, resolver: Arc<rustls_acme::ResolvesServerCertAcme>) -> Arc<AtomicBool> {
        let ready = Arc::new(AtomicBool::new(false));
        let mut generations = self.write();
        let previous = generations.current.take();
        generations.current = Some(Generation {
            resolver,
            ready: ready.clone(),
        });
        // Only one fallback is kept: two orders in a row before either deployed
        // would otherwise pile up, and the oldest is the one whose certificate is
        // closest to expiring.
        if previous.is_some() {
            generations.previous = previous;
        }
        ready
    }

    fn read(&self) -> RwLockReadGuard<'_, Generations> {
        self.generations.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Generations> {
        self.generations.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl rustls::server::ResolvesServerCert for LiveAcmeResolver {
    /// **One generation answers**, chosen before the hello is handed over: a
    /// [`ClientHello`](rustls::server::ClientHello) cannot be cloned, so this
    /// cannot ask one resolver and then the other.
    ///
    /// The CA's validation handshake goes to the newest client, because the
    /// challenge it is answering is that client's. Ordinary traffic goes to the
    /// newest client that has a certificate, which during an order is the
    /// previous one.
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let generations = self.read();
        let challenge = rustls_acme::is_tls_alpn_challenge(&client_hello);
        let current = generations.current.as_ref();
        let previous = generations.previous.as_ref();
        let answering = if challenge {
            current.or(previous)
        } else {
            match current {
                Some(generation) if generation.ready.load(Ordering::Relaxed) => Some(generation),
                current => previous.or(current),
            }
        };
        answering?.resolver.resolve(client_hello)
    }
}

/// The ACME client for these domains, cached in the database.
fn acme_state(
    domains: &[String],
    contact_email: &str,
    directory_url: &str,
    cache: AcmeCache,
) -> AcmeState<Error, Error> {
    let mut config = AcmeConfig::new(domains)
        .directory(directory_url)
        .cache(DatabaseCache(cache));
    if !contact_email.trim().is_empty() {
        config = config.contact_push(format!("mailto:{}", contact_email.trim()));
    }
    config.state()
}

/// Poll the ACME state forever, reporting what it does and marking its
/// generation ready the moment it has a certificate to serve.
///
/// Every line here is an operator's answer to "is my certificate coming?", which
/// is otherwise unanswerable from outside: the handshake either works or does
/// not, and the reason lives in the CA's response.
async fn drive_acme(mut state: AcmeState<Error, Error>, progress: Arc<OrderProgress>) {
    use futures::StreamExt;
    while let Some(event) = state.next().await {
        match event {
            Ok(ok) => {
                if matches!(
                    ok,
                    rustls_acme::EventOk::DeployedCachedCert
                        | rustls_acme::EventOk::DeployedNewCert
                ) {
                    // From here this generation answers ordinary handshakes, and
                    // the one it replaced stops being asked.
                    progress.ready.store(true, Ordering::Relaxed);
                }
                *progress
                    .last_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                eprintln!("feldspar: acme: {ok:?}");
            }
            Err(err) => {
                // Kept for the admin UI's move to a new subdomain, which shows
                // it while it waits: "the CA could not reach admin.example.com"
                // is the sentence that says the DNS record is missing.
                *progress
                    .last_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
                eprintln!("feldspar: acme error: {err}");
            }
        }
    }
}

/// The database-backed ACME cache, in the shape `rustls-acme` asks for.
///
/// The storage itself is [`sc_config::AcmeCache`] — a layer down, where the
/// tables are. This is only the adapter, which is why it is here: naming
/// `rustls_acme`'s traits is this crate's business, not the configuration
/// crate's (§2).
struct DatabaseCache(AcmeCache);

#[async_trait::async_trait]
impl rustls_acme::CertCache for DatabaseCache {
    type EC = Error;

    async fn load_cert(
        &self,
        domains: &[String],
        directory_url: &str,
    ) -> std::result::Result<Option<Vec<u8>>, Error> {
        self.0.load_cert(domains, directory_url).await
    }

    async fn store_cert(
        &self,
        domains: &[String],
        directory_url: &str,
        cert: &[u8],
    ) -> std::result::Result<(), Error> {
        self.0.store_cert(domains, directory_url, cert).await
    }
}

#[async_trait::async_trait]
impl rustls_acme::AccountCache for DatabaseCache {
    type EA = Error;

    async fn load_account(
        &self,
        contact: &[String],
        directory_url: &str,
    ) -> std::result::Result<Option<Vec<u8>>, Error> {
        self.0.load_account(contact, directory_url).await
    }

    async fn store_account(
        &self,
        contact: &[String],
        directory_url: &str,
        account: &[u8],
    ) -> std::result::Result<(), Error> {
        self.0.store_account(contact, directory_url, account).await
    }
}

/// The plain-HTTP router served when TLS is on and redirects are wanted: every
/// request is answered with a permanent redirect to the same URL over HTTPS.
///
/// **308, not 301**: a 301 lets a client turn a `POST` into a `GET`, which for
/// an API request means a write silently becoming a read.
pub fn redirect_router(https_port: u16) -> Router {
    Router::new()
        .fallback(move |request: Request| async move { redirect_response(&request, https_port) })
}

/// The redirect for one request, or 400 when there is no host to redirect to.
fn redirect_response(request: &Request, https_port: u16) -> Response {
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "no Host header to redirect").into_response();
    };
    // The `Host` carries the *plain-HTTP* port, which is not the port the
    // redirect goes to; the hostname is the only part worth keeping.
    let hostname = host.split(':').next().unwrap_or(host);
    let target = if https_port == 443 {
        format!("https://{hostname}{}", path_and_query(request.uri()))
    } else {
        format!(
            "https://{hostname}:{https_port}{}",
            path_and_query(request.uri())
        )
    };
    match HeaderValue::from_str(&target) {
        Ok(location) => (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "invalid Host header").into_response(),
    }
}

/// The part of a URI that survives the scheme change.
fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned())
}

/// Ask a running TLS server to stop, giving in-flight requests
/// [`GRACEFUL_SECONDS`] to finish.
pub fn graceful_shutdown(handle: &TlsHandle) {
    handle.graceful_shutdown(Some(std::time::Duration::from_secs(GRACEFUL_SECONDS)));
}

/// The address TLS is served on: the bind address's interface, the settings'
/// port.
///
/// The interface is not a second setting on purpose — an operator who binds
/// `127.0.0.1` for a local trial should not have to remember that TLS has its
/// own idea of where to listen.
pub fn https_addr(http_addr: SocketAddr, port: u16) -> SocketAddr {
    SocketAddr::new(http_addr.ip(), port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_config::{SslMode, SslSettings};

    fn settings(mode: SslMode) -> SslSettings {
        SslSettings {
            mode,
            ..SslSettings::default()
        }
    }

    #[test]
    fn tls_off_is_the_default_and_serves_nothing_extra() {
        let plan =
            TlsSettings::from_ssl(&settings(SslMode::Off), 443, TlsNames::default(), None).unwrap();
        assert!(!plan.enabled());
        assert_eq!(plan.port(), None);
        assert!(!plan.redirect_http());
    }

    /// Behind a proxy there is nothing to certify and no CA to ask, even with
    /// domains known and an ACME cache at hand.
    #[test]
    fn proxy_mode_serves_plain_http_and_starts_no_acme() {
        let names = TlsNames::new(Some("example.com".to_owned()), Vec::new(), Vec::new());
        let plan = TlsSettings::from_ssl(&settings(SslMode::Proxy), 443, names, None).unwrap();
        assert!(matches!(plan, TlsSettings::Off));
        assert!(!plan.enabled());
        assert_eq!(plan.port(), None);
        assert!(!plan.redirect_http());
    }

    #[test]
    fn the_domain_list_is_derived_from_what_the_server_serves() {
        let apps = ["blog".to_owned(), "shop".to_owned()];
        let extra = ["vanity.example.net".to_owned(), "Example.com".to_owned()];
        assert_eq!(
            tls_domains(Some("example.com"), &apps, &extra),
            [
                "example.com",
                "blog.example.com",
                "shop.example.com",
                "vanity.example.net"
            ]
        );
        // No base domain: apps are not addressable, so only what was listed.
        assert_eq!(
            tls_domains(None, &apps, &extra),
            ["vanity.example.net", "example.com"]
        );
        assert!(tls_domains(None, &apps, &[]).is_empty());
    }

    /// ACME with nothing to certify is refused at boot rather than at the CA.
    #[test]
    fn acme_without_a_domain_or_a_cache_is_refused() {
        let mut acme = settings(SslMode::LetsEncrypt);
        acme.contact_email = "admin@example.com".to_owned();
        let err = TlsSettings::from_ssl(&acme, 443, TlsNames::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("domain"), "{err}");

        let err = TlsSettings::from_ssl(
            &acme,
            443,
            TlsNames::new(Some("example.com".to_owned()), vec![], vec![]),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cache"), "{err}");
    }

    #[test]
    fn the_root_application_adds_no_name_of_its_own() {
        let subdomains = ["@".to_owned(), "admin".to_owned()];
        assert_eq!(
            tls_domains(Some("example.com"), &subdomains, &[]),
            ["example.com", "admin.example.com"]
        );
    }

    /// A pasted certificate covers exactly the names written into it, and says
    /// so for a host the admin UI might move to.
    #[test]
    fn a_pasted_certificate_covers_the_names_it_carries() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![
            "example.com".to_owned(),
            "*.example.com".to_owned(),
        ])
        .unwrap()
        .self_signed(&key)
        .unwrap();
        let pasted = PastedCertificate::new(&cert.pem());
        assert!(pasted.status_for("admin.example.com").is_ready());
        assert!(pasted.status_for("example.com").is_ready());
        let other = pasted.status_for("admin.example.net");
        assert_eq!(other.state, "not_covered");
        assert!(other.message.unwrap().contains("admin.example.net"));
        assert_eq!(
            PastedCertificate::new("junk")
                .status_for("example.com")
                .state,
            "not_covered"
        );
    }

    #[test]
    fn a_paste_that_is_not_a_certificate_says_so() {
        let err = check_certificate("hello", "world").unwrap_err().to_string();
        assert!(err.contains("certificate"), "{err}");

        let err = check_certificate(
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("private key"), "{err}");
    }

    #[test]
    fn a_redirect_keeps_the_path_and_the_method() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/tables?limit=2")
            .header(header::HOST, "blog.example.com:8080")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = redirect_response(&request, 443);
        // 308 rather than 301: a redirected POST stays a POST.
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "https://blog.example.com/api/tables?limit=2"
        );

        // A non-default TLS port is spelled out; the HTTP port never is.
        let request = Request::builder()
            .uri("/")
            .header(header::HOST, "example.com:8080")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = redirect_response(&request, 8443);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "https://example.com:8443/"
        );
    }

    #[test]
    fn a_request_with_no_host_is_a_bad_request_not_a_panic() {
        let request = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            redirect_response(&request, 443).status(),
            StatusCode::BAD_REQUEST
        );
    }
}
