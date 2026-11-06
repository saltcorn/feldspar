//! Every integration test in this crate, in one binary.
//!
//! Each file below is still an ordinary test file — it is pulled in as a module
//! rather than compiled as its own target. The workspace statically links V8 into
//! every test binary, so a target per file cost ~400 MB of disk and a link each;
//! CI ran out of disk on the link (`ld terminated with signal 7`) before it ran
//! out of patience. Files stay where they are, so paths relative to a test file
//! (fixtures, `include_str!`, `#[path]`) are unaffected.
//!
//! Add a new test file and it is picked up here — the list is the whole wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity.rs"]
mod parity;
#[path = "parity_agg.rs"]
mod parity_agg;
#[path = "parity_calc.rs"]
mod parity_calc;
#[path = "parity_request.rs"]
mod parity_request;
#[path = "parity_time.rs"]
mod parity_time;
