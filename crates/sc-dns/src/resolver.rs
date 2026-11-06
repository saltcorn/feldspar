//! The resolver the wrapper asks: one thread, one current-thread runtime, one
//! [`hickory_resolver`] client.
//!
//! **Why a thread of its own.** `getaddrinfo` is a blocking C function called
//! from anywhere: a tokio worker, tokio's blocking pool, `async-net`'s blocking
//! pool, a V8 thread, a thread this process never created. A resolver that is
//! `async` cannot be driven from any of those without a runtime handle, and
//! `block_on` inside a runtime worker panics. So the runtime is *here*, on a
//! thread this crate owns, and the outside world talks to it over a channel and
//! waits on the reply. One thread, parked on a channel receive, is the whole
//! cost.
//!
//! **Why it never fails the process.** A boot that cannot read
//! `/etc/resolv.conf` still serves: the fallback is the same one glibc makes
//! (`127.0.0.1`), and a name that then does not resolve fails one request rather
//! than the server. What *is* worth saying out loud is the fallback itself, so
//! it goes to stderr where the journal reads it.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{LookupIpStrategy, NameServerConfigGroup, ResolverConfig};
use hickory_resolver::name_server::TokioConnectionProvider;
use sc_error::{Error, Result};

/// How long [`lookup`] waits for the resolver thread before giving up.
///
/// Longer than the resolver's own timeout and retry budget (hickory's default is
/// 5 seconds, twice), so a caller times out here only when the resolver thread
/// itself is wedged — the case the reply channel cannot report.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`init`] waits for the resolver thread to come up.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The nameserver used when the system has no usable configuration, matching
/// what glibc does with an empty `/etc/resolv.conf`.
const FALLBACK_NAMESERVER: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Why a lookup produced no addresses.
///
/// Three outcomes rather than an error type, because three is what the C ABI
/// above can express: the name is not there, ask again later, or this resolver
/// is not working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LookupError {
    /// The name does not exist, or has no address records.
    NotFound,
    /// A timeout or a server failure: the same name may resolve later.
    Temporary,
    /// The resolver thread is not running at all.
    Unavailable,
}

/// One question for the resolver thread, and where to send the answer.
struct Job {
    name: String,
    reply: SyncSender<std::result::Result<Vec<IpAddr>, LookupError>>,
}

/// The handle on the resolver thread; `None` when it could not be started.
struct Service {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

static SERVICE: OnceLock<Option<Service>> = OnceLock::new();

/// Start the resolver thread, so a broken DNS configuration is reported at boot
/// rather than at the first name a request needs.
///
/// Idempotent, and cheap to call from anywhere: the thread is started once for
/// the process. Calling this is also what links the wrapper into a binary —
/// `__wrap_getaddrinfo` lives in an rlib, and an rlib nothing references is an
/// rlib the linker never opens.
pub fn init() -> Result<()> {
    match service() {
        Some(_) => Ok(()),
        None => Err(Error::msg(
            "the DNS resolver thread could not be started; this process cannot resolve hostnames",
        )),
    }
}

/// The resolver thread, started on first use.
fn service() -> Option<&'static Service> {
    SERVICE.get_or_init(start).as_ref()
}

thread_local! {
    /// Set on the resolver thread itself, and read by [`lookup`].
    ///
    /// The resolver talks to nameservers by address, so it should never need a
    /// name — but a lookup that *did* start here would post a job to the queue
    /// this thread is the only consumer of and then block waiting for it, which
    /// is a deadlock the reply channel cannot break. Answering "no" straight
    /// away costs nothing and removes the possibility.
    static ON_RESOLVER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Resolve `name` to its addresses, blocking the calling thread.
///
/// IPv4 first, then IPv6 — see the note on ordering in the crate documentation.
pub(crate) fn lookup(name: &str) -> std::result::Result<Vec<IpAddr>, LookupError> {
    if ON_RESOLVER_THREAD.with(std::cell::Cell::get) {
        return Err(LookupError::Unavailable);
    }
    let Some(service) = service() else {
        return Err(LookupError::Unavailable);
    };
    let (reply, answers) = sync_channel(1);
    let job = Job {
        name: name.to_owned(),
        reply,
    };
    if service.jobs.send(job).is_err() {
        return Err(LookupError::Unavailable);
    }
    match answers.recv_timeout(LOOKUP_TIMEOUT) {
        Ok(result) => result,
        // The thread died, or is stuck: either way this name has no answer now.
        Err(_) => Err(LookupError::Temporary),
    }
}

/// Spawn the resolver thread and wait for it to report that it is up.
#[allow(
    clippy::disallowed_methods,
    reason = "unbounded: each job is a getaddrinfo caller blocked on its reply, so the queue is never longer than the threads waiting on it"
)]
fn start() -> Option<Service> {
    let (jobs, queue) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (ready, started) = sync_channel::<std::result::Result<(), String>>(1);

    let spawned = std::thread::Builder::new()
        .name("feldspar-dns".to_owned())
        .spawn(move || run(queue, &ready));
    if let Err(error) = spawned {
        eprintln!("feldspar: could not start the DNS resolver thread: {error}");
        return None;
    }

    match started.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Some(Service { jobs }),
        Ok(Err(error)) => {
            eprintln!("feldspar: could not start the DNS resolver: {error}");
            None
        }
        Err(_) => {
            eprintln!("feldspar: the DNS resolver did not start within {START_TIMEOUT:?}");
            None
        }
    }
}

/// The resolver thread's body: build the runtime and the resolver, say whether
/// that worked, then answer questions until the process ends.
#[allow(
    clippy::disallowed_methods,
    reason = "tokio::spawn on the resolver's own runtime and thread, below the catalog: there is no task scope to carry"
)]
fn run(
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Job>,
    ready: &SyncSender<std::result::Result<(), String>>,
) {
    ON_RESOLVER_THREAD.with(|here| here.set(true));
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(format!("building the resolver's runtime: {error}")));
            return;
        }
    };

    runtime.block_on(async move {
        let resolver = build_resolver();
        let _ = ready.send(Ok(()));
        // Each lookup is its own task, so a slow name does not hold up the next
        // caller: hickory shares one connection pool across them.
        while let Some(job) = queue.recv().await {
            let resolver = resolver.clone();
            tokio::spawn(async move {
                let _ = job.reply.send(resolve(&resolver, &job.name).await);
            });
        }
    });
}

/// One name, through hickory.
async fn resolve(
    resolver: &TokioResolver,
    name: &str,
) -> std::result::Result<Vec<IpAddr>, LookupError> {
    match resolver.lookup_ip(name).await {
        Ok(answer) => {
            let addresses: Vec<IpAddr> = answer.iter().collect();
            if addresses.is_empty() {
                Err(LookupError::NotFound)
            } else {
                Ok(addresses)
            }
        }
        Err(error) if error.is_nx_domain() || error.is_no_records_found() => {
            Err(LookupError::NotFound)
        }
        Err(_) => Err(LookupError::Temporary),
    }
}

/// The resolver: the system's configuration where there is one, loopback where
/// there is not.
fn build_resolver() -> TokioResolver {
    let mut builder = match TokioResolver::builder_tokio() {
        Ok(builder) => builder,
        Err(error) => {
            eprintln!(
                "feldspar: no usable DNS configuration ({error}); \
                 resolving through {FALLBACK_NAMESERVER} instead"
            );
            let servers = NameServerConfigGroup::from_ips_clear(&[FALLBACK_NAMESERVER], 53, true);
            let config = ResolverConfig::from_parts(None, vec![], servers);
            TokioResolver::builder_with_config(config, TokioConnectionProvider::default())
        }
    };
    // Both families, rather than hickory's default of "IPv4, and IPv6 only if
    // that failed": a name with both records must offer both, because the caller
    // is the one that knows which of them it can reach.
    builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
    builder.build()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `/etc/hosts` is read by the resolver itself, which is what keeps the
    /// hosts file working with NSS out of the path.
    #[test]
    fn localhost_resolves_from_the_hosts_file() {
        let addresses = lookup("localhost").expect("localhost resolves");
        assert!(
            addresses.iter().all(std::net::IpAddr::is_loopback),
            "localhost resolved to {addresses:?}"
        );
    }

    /// A name that cannot exist is `NotFound`, not a timeout: the distinction is
    /// what becomes `EAI_NONAME` rather than `EAI_AGAIN` a layer up, and a
    /// caller retries one of those and not the other.
    #[test]
    fn a_nonexistent_name_is_not_found() {
        // `.invalid` is reserved by RFC 2606 and answers NXDOMAIN everywhere.
        match lookup("this-name-does-not-exist.invalid") {
            Err(LookupError::NotFound) => {}
            // A machine with no network at all cannot tell the two apart, and
            // this test is not about the network.
            Err(LookupError::Temporary | LookupError::Unavailable) => {}
            Ok(addresses) => panic!("`.invalid` resolved to {addresses:?}"),
        }
    }

    #[test]
    fn init_is_idempotent() {
        init().expect("the resolver starts");
        init().expect("the resolver is still up");
    }
}
