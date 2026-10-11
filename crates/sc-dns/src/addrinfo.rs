//! The C ABI: `getaddrinfo` and `freeaddrinfo`, answered from Rust.
//!
//! The linker turns every reference to `getaddrinfo` in the binary into a
//! reference to [`__wrap_getaddrinfo`] and leaves glibc's own reachable as
//! `__real_getaddrinfo` (see `build.rs`). What is implemented here is the part
//! of the contract this process actually uses:
//!
//! - a **hostname** is resolved by [`crate::resolver`] — hickory, `/etc/hosts`,
//!   `/etc/resolv.conf`, no `dlopen`;
//! - an **address literal** is parsed here, including a `%zone` scope on an IPv6
//!   one, and never leaves the process;
//! - a **null `node`** and a **named service** (`"https"`) are handed to glibc,
//!   which reads `/etc/services` and does not touch the `hosts:` line;
//! - the answer is marshalled into a `malloc`ed `addrinfo` list, which
//!   [`__wrap_freeaddrinfo`] frees again.
//!
//! `AI_ADDRCONFIG` is accepted and ignored — the caller is told about both
//! families and picks. `AI_V4MAPPED` is likewise ignored: an `AF_INET6` request
//! gets the IPv6 addresses there are, not IPv4 ones dressed as IPv6.

use std::ffi::{CStr, CString, c_char, c_int};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::resolver::{LookupError, lookup};

#[cfg(target_os = "linux")]
unsafe extern "C" {
    /// glibc's `getaddrinfo`, under the name `--wrap` leaves it at.
    fn __real_getaddrinfo(
        node: *const c_char,
        service: *const c_char,
        hints: *const libc::addrinfo,
        res: *mut *mut libc::addrinfo,
    ) -> c_int;
    /// glibc's `freeaddrinfo`, likewise.
    fn __real_freeaddrinfo(res: *mut libc::addrinfo);
}

// Elsewhere (macOS) the linker has no `--wrap`, so nothing is intercepted and
// the system resolver is called directly; these stand in for the `__real_`
// names so the wrapper still links.
#[cfg(not(target_os = "linux"))]
unsafe fn __real_getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const libc::addrinfo,
    res: *mut *mut libc::addrinfo,
) -> c_int {
    // SAFETY: the caller's contract is `getaddrinfo`'s.
    unsafe { libc::getaddrinfo(node, service, hints, res) }
}

#[cfg(not(target_os = "linux"))]
unsafe fn __real_freeaddrinfo(res: *mut libc::addrinfo) {
    // SAFETY: the caller's contract is `freeaddrinfo`'s.
    unsafe { libc::freeaddrinfo(res) }
}

/// How many calls have come through the wrapper.
///
/// The one thing a *test* cannot otherwise establish: that the link flag is
/// actually in effect and `std`'s resolution went through this code rather than
/// glibc's. A relaxed counter, read by [`intercepted`].
static INTERCEPTED: AtomicUsize = AtomicUsize::new(0);

/// The list heads this module allocated, so [`__wrap_freeaddrinfo`] can tell its
/// own memory from glibc's — the two are freed differently, and a caller frees
/// whatever it was given without knowing which it holds.
///
/// A `Vec` rather than a set: the entries here are the lists that are alive
/// right now, which is a handful, and a linear scan of a handful beats hashing.
static OURS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// How many `getaddrinfo` calls this process has answered here.
///
/// Zero in a binary that was linked without `--wrap=getaddrinfo`, which is the
/// interesting thing to assert.
#[must_use]
pub fn intercepted() -> usize {
    INTERCEPTED.load(Ordering::Relaxed)
}

/// One resolved address, with the scope an IPv6 link-local literal carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Address {
    ip: IpAddr,
    scope: u32,
}

impl From<IpAddr> for Address {
    fn from(ip: IpAddr) -> Address {
        Address { ip, scope: 0 }
    }
}

/// `getaddrinfo(3)`, answered without NSS.
///
/// # Safety
///
/// The C contract: `node` and `service` are null or nul-terminated strings,
/// `hints` is null or a valid `addrinfo`, and `res` is a valid place to write a
/// list pointer. The list written there must be freed with `freeaddrinfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const libc::addrinfo,
    res: *mut *mut libc::addrinfo,
) -> c_int {
    INTERCEPTED.fetch_add(1, Ordering::Relaxed);
    unsafe { getaddrinfo(node, service, hints, res) }
}

/// `freeaddrinfo(3)`: this module's lists, and glibc's for anything it did not
/// allocate.
///
/// # Safety
///
/// `res` is null, or a list head returned by `getaddrinfo` and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_freeaddrinfo(res: *mut libc::addrinfo) {
    if res.is_null() {
        return;
    }
    if !claim(res) {
        // Not ours: the service-name lookup and the null-`node` calls come back
        // from glibc, and glibc frees them.
        unsafe { __real_freeaddrinfo(res) };
        return;
    }
    let mut cursor = res;
    while !cursor.is_null() {
        // Read the node out before any of it is freed.
        let (next, addr, canonname) = unsafe {
            let node = &*cursor;
            (node.ai_next, node.ai_addr, node.ai_canonname)
        };
        if !addr.is_null() {
            unsafe { libc::free(addr.cast()) };
        }
        if !canonname.is_null() {
            unsafe { libc::free(canonname.cast()) };
        }
        unsafe { libc::free(cursor.cast()) };
        cursor = next;
    }
}

/// The body of [`__wrap_getaddrinfo`].
unsafe fn getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const libc::addrinfo,
    res: *mut *mut libc::addrinfo,
) -> c_int {
    if res.is_null() {
        return libc::EAI_SYSTEM;
    }
    if node.is_null() {
        // A service-only lookup: no host, so no `hosts:` line and no NSS module.
        return unsafe { __real_getaddrinfo(node, service, hints, res) };
    }

    let hints = unsafe { hints.as_ref() };
    let flags = hints.map_or(0, |hints| hints.ai_flags);
    let family = hints.map_or(libc::AF_UNSPEC, |hints| hints.ai_family);
    if !matches!(family, libc::AF_UNSPEC | libc::AF_INET | libc::AF_INET6) {
        return libc::EAI_FAMILY;
    }

    let Ok(name) = (unsafe { CStr::from_ptr(node) }).to_str() else {
        return libc::EAI_NONAME;
    };
    let port = match unsafe { port_of(service, flags, hints) } {
        Ok(port) => port,
        Err(code) => return code,
    };
    let addresses = match addresses_of(name, flags) {
        Ok(addresses) => addresses,
        Err(code) => return code,
    };
    let addresses: Vec<Address> = addresses
        .into_iter()
        .filter(|address| family_matches(family, address.ip))
        .collect();
    if addresses.is_empty() {
        // The name resolved, but not in the family that was asked for.
        return libc::EAI_NONAME;
    }

    match unsafe { build_list(&addresses, port, hints, name, flags) } {
        Some(head) => {
            ours().push(head as usize);
            unsafe { *res = head };
            0
        }
        None => libc::EAI_MEMORY,
    }
}

/// The addresses for `name`: parsed if it is a literal, resolved if it is not.
fn addresses_of(name: &str, flags: c_int) -> std::result::Result<Vec<Address>, c_int> {
    if let Some(address) = numeric_host(name) {
        return Ok(vec![address]);
    }
    if flags & libc::AI_NUMERICHOST != 0 || name.is_empty() {
        return Err(libc::EAI_NONAME);
    }
    match lookup(name) {
        Ok(addresses) => Ok(addresses.into_iter().map(Address::from).collect()),
        Err(LookupError::NotFound) => loopback(name).ok_or(libc::EAI_NONAME),
        Err(LookupError::Temporary) => Err(libc::EAI_AGAIN),
        Err(LookupError::Unavailable) => Err(libc::EAI_FAIL),
    }
}

/// `name` as an address literal, with the `%zone` an IPv6 link-local may carry.
fn numeric_host(name: &str) -> Option<Address> {
    match name.split_once('%') {
        None => name.parse::<IpAddr>().ok().map(Address::from),
        Some((address, zone)) => address.parse::<IpAddr>().ok().map(|ip| Address {
            ip,
            scope: scope_id(zone),
        }),
    }
}

/// The interface index a `%zone` suffix names, by number or by name.
fn scope_id(zone: &str) -> u32 {
    if let Ok(index) = zone.parse::<u32>() {
        return index;
    }
    match CString::new(zone) {
        // `if_nametoindex` returns 0 for a name no interface has, which is the
        // same "unscoped" the field means when nobody set it.
        Ok(zone) => unsafe { libc::if_nametoindex(zone.as_ptr()) },
        Err(_) => 0,
    }
}

/// The loopback answer for `localhost`, for the machine whose `/etc/hosts` does
/// not have one.
///
/// glibc has `myhostname` for this, and `myhostname` is exactly what must not be
/// loaded here — so the one name that must never fail to resolve gets the answer
/// that file would have given.
fn loopback(name: &str) -> Option<Vec<Address>> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    (name == "localhost" || name.ends_with(".localhost")).then(|| {
        vec![
            Address::from(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Address::from(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ]
    })
}

/// Whether an address belongs in the answer to a request for `family`.
fn family_matches(family: c_int, ip: IpAddr) -> bool {
    match family {
        libc::AF_INET => ip.is_ipv4(),
        libc::AF_INET6 => ip.is_ipv6(),
        _ => true,
    }
}

/// The port a `service` argument means.
///
/// A number is parsed here; a name is `/etc/services`, which glibc reads for us
/// through a lookup with no host in it.
unsafe fn port_of(
    service: *const c_char,
    flags: c_int,
    hints: Option<&libc::addrinfo>,
) -> std::result::Result<u16, c_int> {
    if service.is_null() {
        return Ok(0);
    }
    let Ok(service_name) = (unsafe { CStr::from_ptr(service) }).to_str() else {
        return Err(libc::EAI_SERVICE);
    };
    if let Ok(port) = service_name.parse::<u16>() {
        return Ok(port);
    }
    if flags & libc::AI_NUMERICSERV != 0 {
        return Err(libc::EAI_SERVICE);
    }

    // The caller's socket type and family are kept — a service can have a
    // different port for TCP and UDP — but its **flags** are not: `AI_CANONNAME`
    // with no `node` is `EAI_BADFLAGS` from glibc, and the caller asked for a
    // canonical name for its *host*, not for `/etc/services`.
    let mut service_hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    if let Some(hints) = hints {
        service_hints.ai_family = hints.ai_family;
        service_hints.ai_socktype = hints.ai_socktype;
        service_hints.ai_protocol = hints.ai_protocol;
    }
    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    if unsafe {
        __real_getaddrinfo(
            std::ptr::null(),
            service,
            &raw const service_hints,
            &raw mut list,
        )
    } != 0
    {
        return Err(libc::EAI_SERVICE);
    }
    let port = unsafe { port_in(list) };
    unsafe { __real_freeaddrinfo(list) };
    port.ok_or(libc::EAI_SERVICE)
}

/// The port in the first address of a list glibc returned.
unsafe fn port_in(list: *const libc::addrinfo) -> Option<u16> {
    let mut cursor = list;
    while !cursor.is_null() {
        let node = unsafe { &*cursor };
        if !node.ai_addr.is_null() {
            let port = match c_int::from(unsafe { (*node.ai_addr).sa_family }) {
                libc::AF_INET => {
                    let addr = node.ai_addr.cast::<libc::sockaddr_in>();
                    Some(u16::from_be(unsafe { (*addr).sin_port }))
                }
                libc::AF_INET6 => {
                    let addr = node.ai_addr.cast::<libc::sockaddr_in6>();
                    Some(u16::from_be(unsafe { (*addr).sin6_port }))
                }
                _ => None,
            };
            if port.is_some() {
                return port;
            }
        }
        cursor = node.ai_next;
    }
    None
}

/// The `(socktype, protocol)` pairs an answer should carry.
///
/// A caller that named neither gets both of the two that matter, which is what
/// glibc does: one entry per socket type it could open.
fn kinds(socktype: c_int, protocol: c_int) -> Vec<(c_int, c_int)> {
    match (socktype, protocol) {
        (0, libc::IPPROTO_TCP) => vec![(libc::SOCK_STREAM, libc::IPPROTO_TCP)],
        (0, libc::IPPROTO_UDP) => vec![(libc::SOCK_DGRAM, libc::IPPROTO_UDP)],
        (0, 0) => vec![
            (libc::SOCK_STREAM, libc::IPPROTO_TCP),
            (libc::SOCK_DGRAM, libc::IPPROTO_UDP),
        ],
        (0, protocol) => vec![(0, protocol)],
        (socktype, 0) => vec![(socktype, default_protocol(socktype))],
        (socktype, protocol) => vec![(socktype, protocol)],
    }
}

/// The protocol a socket type implies when the caller did not say.
fn default_protocol(socktype: c_int) -> c_int {
    match socktype {
        libc::SOCK_STREAM => libc::IPPROTO_TCP,
        libc::SOCK_DGRAM => libc::IPPROTO_UDP,
        _ => 0,
    }
}

/// Marshal the answer into the `addrinfo` list the caller will read and free.
///
/// Every allocation is `libc::malloc`, because `freeaddrinfo` is documented to
/// free what `getaddrinfo` returned and a caller may reach ours by that name.
/// A failed allocation frees what was built so far and reports `EAI_MEMORY`
/// rather than leaking a half-list.
unsafe fn build_list(
    addresses: &[Address],
    port: u16,
    hints: Option<&libc::addrinfo>,
    name: &str,
    flags: c_int,
) -> Option<*mut libc::addrinfo> {
    let socktype = hints.map_or(0, |hints| hints.ai_socktype);
    let protocol = hints.map_or(0, |hints| hints.ai_protocol);
    let kinds = kinds(socktype, protocol);

    let mut nodes: Vec<*mut libc::addrinfo> = Vec::new();
    for address in addresses {
        for &(socktype, protocol) in &kinds {
            match unsafe { build_node(address, port, socktype, protocol) } {
                Some(node) => nodes.push(node),
                None => {
                    unsafe { free_nodes(&nodes) };
                    return None;
                }
            }
        }
    }

    let head = *nodes.first()?;
    if flags & libc::AI_CANONNAME != 0 {
        match unsafe { dup_cstring(name) } {
            // glibc puts the canonical name on the first entry only.
            Some(canonname) => unsafe { (*head).ai_canonname = canonname },
            None => {
                unsafe { free_nodes(&nodes) };
                return None;
            }
        }
    }
    for pair in nodes.windows(2) {
        if let [current, next] = pair {
            unsafe { (**current).ai_next = *next };
        }
    }
    Some(head)
}

/// One `addrinfo` node, with its `sockaddr` beside it.
unsafe fn build_node(
    address: &Address,
    port: u16,
    socktype: c_int,
    protocol: c_int,
) -> Option<*mut libc::addrinfo> {
    let (sockaddr, len) = unsafe { build_sockaddr(address, port) }?;

    let node = unsafe { libc::malloc(size_of::<libc::addrinfo>()) }.cast::<libc::addrinfo>();
    if node.is_null() {
        unsafe { libc::free(sockaddr.cast()) };
        return None;
    }
    let mut value: libc::addrinfo = unsafe { std::mem::zeroed() };
    value.ai_family = if address.ip.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    value.ai_socktype = socktype;
    value.ai_protocol = protocol;
    value.ai_addrlen = len;
    value.ai_addr = sockaddr;
    unsafe { std::ptr::write(node, value) };
    Some(node)
}

/// The `sockaddr_in` or `sockaddr_in6` for one address.
unsafe fn build_sockaddr(
    address: &Address,
    port: u16,
) -> Option<(*mut libc::sockaddr, libc::socklen_t)> {
    match address.ip {
        IpAddr::V4(ip) => {
            let raw = unsafe { libc::malloc(size_of::<libc::sockaddr_in>()) };
            if raw.is_null() {
                return None;
            }
            let mut value: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            value.sin_family = libc::AF_INET as libc::sa_family_t;
            value.sin_port = port.to_be();
            // The octets are already in network order, which is what `s_addr`
            // holds — so the bytes go across as they are.
            value.sin_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(ip.octets()),
            };
            let raw = raw.cast::<libc::sockaddr_in>();
            unsafe { std::ptr::write(raw, value) };
            Some((
                raw.cast(),
                libc::socklen_t::try_from(size_of::<libc::sockaddr_in>()).unwrap_or_default(),
            ))
        }
        IpAddr::V6(ip) => {
            let raw = unsafe { libc::malloc(size_of::<libc::sockaddr_in6>()) };
            if raw.is_null() {
                return None;
            }
            let mut value: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            value.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            value.sin6_port = port.to_be();
            value.sin6_addr = libc::in6_addr {
                s6_addr: ip.octets(),
            };
            value.sin6_scope_id = address.scope;
            let raw = raw.cast::<libc::sockaddr_in6>();
            unsafe { std::ptr::write(raw, value) };
            Some((
                raw.cast(),
                libc::socklen_t::try_from(size_of::<libc::sockaddr_in6>()).unwrap_or_default(),
            ))
        }
    }
}

/// A `malloc`ed copy of `value` as a C string.
unsafe fn dup_cstring(value: &str) -> Option<*mut c_char> {
    let Ok(text) = CString::new(value) else {
        return None;
    };
    let bytes = text.as_bytes_with_nul();
    let raw = unsafe { libc::malloc(bytes.len()) };
    if raw.is_null() {
        return None;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), raw.cast::<u8>(), bytes.len()) };
    Some(raw.cast())
}

/// Free nodes that were built before an allocation failed. They are not linked
/// yet, so each one is freed on its own.
unsafe fn free_nodes(nodes: &[*mut libc::addrinfo]) {
    for &node in nodes {
        let addr = unsafe { (*node).ai_addr };
        if !addr.is_null() {
            unsafe { libc::free(addr.cast()) };
        }
        unsafe { libc::free(node.cast()) };
    }
}

/// The registry of lists this module allocated.
///
/// A poisoned lock is taken anyway: the value behind it is a list of pointers,
/// a panic cannot have left it in a state that means anything different, and
/// refusing to free memory because another thread panicked helps nobody.
fn ours() -> std::sync::MutexGuard<'static, Vec<usize>> {
    match OURS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Take `list` out of the registry, saying whether it was ours to free.
fn claim(list: *mut libc::addrinfo) -> bool {
    let mut ours = ours();
    match ours.iter().position(|&head| head == list as usize) {
        Some(index) => {
            ours.swap_remove(index);
            true
        }
        None => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Build a list the way `getaddrinfo` would, read it back, and free it —
    /// the marshalling on its own, without a name to resolve.
    #[test]
    fn a_list_carries_every_address_and_frees_clean() {
        let addresses = [
            Address::from(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
            Address::from(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ];
        let head = unsafe { build_list(&addresses, 443, None, "example.com", 0) }
            .expect("the list is built");
        ours().push(head as usize);

        // No hints: one stream entry and one datagram entry per address.
        let mut seen = Vec::new();
        let mut cursor: *const libc::addrinfo = head;
        while !cursor.is_null() {
            let node = unsafe { &*cursor };
            seen.push((node.ai_family, node.ai_socktype, node.ai_protocol));
            assert_eq!(unsafe { port_in(cursor) }, Some(443));
            cursor = node.ai_next;
        }
        assert_eq!(
            seen,
            vec![
                (libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_TCP),
                (libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP),
                (libc::AF_INET6, libc::SOCK_STREAM, libc::IPPROTO_TCP),
                (libc::AF_INET6, libc::SOCK_DGRAM, libc::IPPROTO_UDP),
            ]
        );

        unsafe { __wrap_freeaddrinfo(head) };
        assert!(!claim(head), "the list was taken out of the registry");
    }

    /// The socket type asked for is the socket type answered, and a canonical
    /// name goes on the first entry.
    #[test]
    fn hints_choose_the_socket_type_and_the_canonical_name() {
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_socktype = libc::SOCK_STREAM;
        hints.ai_flags = libc::AI_CANONNAME;
        let addresses = [Address::from(IpAddr::V4(Ipv4Addr::LOCALHOST))];

        let head = unsafe {
            build_list(
                &addresses,
                80,
                Some(&hints),
                "host.example",
                libc::AI_CANONNAME,
            )
        }
        .expect("the list is built");
        ours().push(head as usize);

        let node = unsafe { &*head };
        assert_eq!(node.ai_socktype, libc::SOCK_STREAM);
        assert_eq!(node.ai_protocol, libc::IPPROTO_TCP);
        assert!(node.ai_next.is_null(), "one address, one socket type");
        let canonname = unsafe { CStr::from_ptr(node.ai_canonname) };
        assert_eq!(canonname.to_str(), Ok("host.example"));

        unsafe { __wrap_freeaddrinfo(head) };
    }

    #[test]
    fn an_address_literal_never_reaches_the_resolver() {
        assert_eq!(
            numeric_host("192.0.2.7"),
            Some(Address::from(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))))
        );
        assert_eq!(
            numeric_host("::1"),
            Some(Address::from(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
        // A scope suffix is kept: `fe80::1%2` is interface 2, not a hostname.
        assert_eq!(numeric_host("fe80::1%2").map(|a| a.scope), Some(2));
        assert_eq!(numeric_host("example.com"), None);
    }

    #[test]
    fn the_family_hint_filters_the_answer() {
        assert!(family_matches(
            libc::AF_UNSPEC,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        ));
        assert!(family_matches(
            libc::AF_INET,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        ));
        assert!(!family_matches(
            libc::AF_INET,
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        ));
        assert!(family_matches(
            libc::AF_INET6,
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        ));
        assert!(!family_matches(
            libc::AF_INET6,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        ));
    }

    /// `localhost` resolves even where the hosts file does not say so, because
    /// the module glibc would have used for that is the one that must not load.
    #[test]
    fn localhost_always_has_an_answer() {
        let addresses = loopback("localhost").expect("localhost has a fallback");
        assert!(addresses.iter().all(|address| address.ip.is_loopback()));
        assert!(loopback("api.example.com").is_none());
    }
}
