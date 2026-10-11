//! Link this crate's `getaddrinfo` in front of libc's — for this crate's own
//! test binaries.
//!
//! The interception is a linker feature, not a Rust one: `--wrap=getaddrinfo`
//! rewrites every *undefined reference* to `getaddrinfo` — including the ones
//! inside `std`, which is where `ToSocketAddrs` resolves — into a reference to
//! `__wrap_getaddrinfo`, and leaves the original reachable as
//! `__real_getaddrinfo`. Both symbols are defined in `src/addrinfo.rs`.
//!
//! A build script's link arguments apply to the crate it belongs to, so this
//! file covers `cargo test -p sc-dns` and `crates/sc-cli/build.rs` covers the
//! binary. Nothing else in the workspace links the wrapper: an integration test
//! elsewhere resolves through glibc exactly as it did before, which is the right
//! default — the tests that care about the wrapper are the ones here.
//!
//! `rustc-link-arg` rather than `rustc-link-arg-tests`, which reaches the
//! integration tests in `tests/` but not the lib's own unit-test binary — and
//! that one links `__wrap_freeaddrinfo` too, so it needs `__real_freeaddrinfo`
//! to exist.
fn main() {
    // `--wrap` is a GNU ld option; Apple's linker has no equivalent.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    for symbol in ["getaddrinfo", "freeaddrinfo"] {
        println!("cargo:rustc-link-arg=-Wl,--wrap={symbol}");
    }
}
