//! Offline sync's API (§14.11, `TODO-offline-sync.md`): the endpoints an
//! application's REST API gets when one of its tables is synced.
//!
//! - [`device`]: `sync/register`, and the check that a device is the caller's.
//! - [`pull`]: `sync/pull`, what changed on the server since the device's
//!   watermark.
//! - [`push`]: `sync/push`, the changes the device made offline.

mod device;
mod pull;
mod push;

pub(crate) use device::{register, register_input_schema, register_output_schema};
pub(crate) use pull::{pull, pull_input_schema, pull_output_schema};
pub(crate) use push::{push, push_input_schema, push_output_schema};
