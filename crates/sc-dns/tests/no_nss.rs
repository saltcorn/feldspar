//! The regression test for the crash this crate exists to prevent: resolving a
//! name must not make the process load an NSS module.
//!
//! On a Debian VM, `feldspar` built with `+crt-static` died of `SIGFPE` a second
//! after it started serving TLS. The core showed `libnss_myhostname.so.2`,
//! `ld-linux.so` and a whole second `libc.so.6` mapped into a statically linked
//! binary, with the faulting address inside that second libc: glibc's
//! `getaddrinfo` had `dlopen`ed the module named on the `hosts:` line of
//! `/etc/nsswitch.conf`, and the two glibcs did not survive each other.
//!
//! These tests run in a *dynamically* linked test binary, where `libc.so.6` is
//! mapped by definition — so the thing they watch for is a `libnss_*` module,
//! which appears in `/proc/self/maps` only when glibc's resolver went looking
//! for one. They are Linux-only for the same reason: `/proc/self/maps` is where
//! the evidence is.
//!
//! The test calls the C `getaddrinfo` itself, so it may use `unsafe`.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

use std::ffi::CString;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// The NSS modules mapped into this process, if any.
fn nss_modules() -> Vec<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut modules: Vec<String> = maps
        .lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter(|path| path.contains("libnss_"))
        .map(str::to_owned)
        .collect();
    modules.sort_unstable();
    modules.dedup();
    modules
}

/// `std`'s own resolution goes through this crate, and loads no NSS module.
///
/// The name is resolved through `ToSocketAddrs` — the path every client in the
/// tree ends up on, from `async-net` in the ACME client to Deno's `fetch` — and
/// the assertions hold whether or not this machine has a network: a lookup that
/// fails is a lookup that still must not have gone through NSS.
#[test]
fn resolving_through_std_bypasses_nss() {
    let before = sc_dns::intercepted();
    let resolved = ("example.com", 443).to_socket_addrs();
    assert!(
        sc_dns::intercepted() > before,
        "std's resolution did not reach __wrap_getaddrinfo: the binary was linked \
         without -Wl,--wrap=getaddrinfo"
    );
    if let Ok(addresses) = resolved {
        for address in addresses {
            assert_eq!(address.port(), 443);
        }
    }
    assert_eq!(
        nss_modules(),
        Vec::<String>::new(),
        "an NSS module was loaded into the process"
    );
}

/// An address literal is parsed here and never becomes a query.
#[test]
fn address_literals_resolve_without_a_lookup() {
    for (literal, expected) in [
        ("127.0.0.1:5432", "127.0.0.1:5432"),
        ("[::1]:5432", "[::1]:5432"),
    ] {
        let resolved: Vec<SocketAddr> = literal.to_socket_addrs().unwrap().collect();
        assert_eq!(resolved.len(), 1, "{literal} resolved to {resolved:?}");
        assert_eq!(resolved[0].to_string(), expected);
    }
    assert!(nss_modules().is_empty());
}

/// `localhost` keeps working: it is in the hosts file, which the resolver reads
/// itself, and has a loopback answer even where it is not.
#[test]
fn localhost_resolves_to_loopback() {
    let resolved: Vec<SocketAddr> = ("localhost", 80).to_socket_addrs().unwrap().collect();
    assert!(!resolved.is_empty(), "localhost resolved to nothing");
    for address in &resolved {
        assert!(address.ip().is_loopback(), "localhost gave {address}");
        assert_eq!(address.port(), 80);
    }
    assert!(nss_modules().is_empty());
}

/// A **named** service is `/etc/services`, which is not the `hosts:` line: it is
/// answered by glibc, with no host in the question and so no NSS module.
#[test]
fn a_named_service_becomes_a_port() {
    let node = CString::new("127.0.0.1").unwrap();
    let service = CString::new("https").unwrap();
    // SAFETY: `addrinfo` is plain C data, for which all-zero is a valid value.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = libc::AF_INET;
    hints.ai_socktype = libc::SOCK_STREAM;

    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: NUL-terminated strings (or null), a valid `hints`, and a live place for the list.
    let code = unsafe { libc::getaddrinfo(node.as_ptr(), service.as_ptr(), &hints, &mut list) };
    assert_eq!(code, 0, "getaddrinfo(127.0.0.1, https) failed with {code}");

    // SAFETY: `getaddrinfo` returned 0, so `list` is the non-null head of its answer.
    let node = unsafe { &*list };
    assert_eq!(node.ai_family, libc::AF_INET);
    let addr = node.ai_addr.cast::<libc::sockaddr_in>();
    // SAFETY: the family asked for is `AF_INET`, so `ai_addr` is a `sockaddr_in`.
    assert_eq!(u16::from_be(unsafe { (*addr).sin_port }), 443);
    // SAFETY: `list` came from `getaddrinfo` and is freed once.
    unsafe { libc::freeaddrinfo(list) };

    assert!(nss_modules().is_empty());
}

/// A named service and `AI_CANONNAME` together still work.
///
/// The service is resolved by a second call with no host in it, and glibc
/// refuses `AI_CANONNAME` on a call with no host — so the caller's flags must
/// not be passed along to it.
#[test]
fn a_named_service_survives_ai_canonname() {
    let node = CString::new("localhost").unwrap();
    let service = CString::new("https").unwrap();
    // SAFETY: `addrinfo` is plain C data, for which all-zero is a valid value.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = libc::AF_INET;
    hints.ai_socktype = libc::SOCK_STREAM;
    hints.ai_flags = libc::AI_CANONNAME;

    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: NUL-terminated strings (or null), a valid `hints`, and a live place for the list.
    let code = unsafe { libc::getaddrinfo(node.as_ptr(), service.as_ptr(), &hints, &mut list) };
    assert_eq!(
        code, 0,
        "getaddrinfo(localhost, https, AI_CANONNAME) failed with {code}"
    );

    // SAFETY: `getaddrinfo` returned 0, so `list` is the non-null head of its answer.
    let node = unsafe { &*list };
    let addr = node.ai_addr.cast::<libc::sockaddr_in>();
    // SAFETY: the family asked for is `AF_INET`, so `ai_addr` is a `sockaddr_in`.
    assert_eq!(u16::from_be(unsafe { (*addr).sin_port }), 443);
    assert!(!node.ai_canonname.is_null(), "AI_CANONNAME set no name");
    // SAFETY: `list` came from `getaddrinfo` and is freed once.
    unsafe { libc::freeaddrinfo(list) };

    assert!(nss_modules().is_empty());
}

/// The error codes callers branch on: a name that cannot exist is `EAI_NONAME`,
/// a family nobody has is `EAI_FAMILY`, and `AI_NUMERICHOST` with a hostname
/// does not quietly resolve it.
#[test]
fn the_failure_codes_are_the_documented_ones() {
    let numeric_only = {
        let node = CString::new("example.com").unwrap();
        // SAFETY: `addrinfo` is plain C data, for which all-zero is a valid value.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_flags = libc::AI_NUMERICHOST;
        let mut list: *mut libc::addrinfo = std::ptr::null_mut();
        // SAFETY: NUL-terminated strings (or null), a valid `hints`, and a live place for the list.
        unsafe { libc::getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut list) }
    };
    assert_eq!(numeric_only, libc::EAI_NONAME);

    let bad_family = {
        let node = CString::new("127.0.0.1").unwrap();
        // SAFETY: `addrinfo` is plain C data, for which all-zero is a valid value.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_UNIX;
        let mut list: *mut libc::addrinfo = std::ptr::null_mut();
        // SAFETY: NUL-terminated strings (or null), a valid `hints`, and a live place for the list.
        unsafe { libc::getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut list) }
    };
    assert_eq!(bad_family, libc::EAI_FAMILY);

    assert!(nss_modules().is_empty());
}

/// Every address the resolver returns is usable as one: the marshalling puts the
/// family, the port and the bytes where the C ABI says they go, and `std` reads
/// them back.
#[test]
fn addresses_survive_the_round_trip() {
    let resolved: Vec<SocketAddr> = ("localhost", 65000).to_socket_addrs().unwrap().collect();
    for address in resolved {
        match address.ip() {
            IpAddr::V4(ip) => assert_eq!(ip.octets(), [127, 0, 0, 1]),
            IpAddr::V6(ip) => assert_eq!(ip.segments()[7], 1),
        }
        assert_eq!(address.port(), 65000);
    }
}
