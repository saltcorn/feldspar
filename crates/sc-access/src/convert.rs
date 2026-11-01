//! Converting between JSON (the wire shape of the typed API) and the query
//! layer's [`Value`](sc_query::Value).
//!
//! The pair itself lives in [`sc_types::json`] — it moved down when a trigger's
//! `only_if` needed to type an event's row by its columns, three layers below any
//! API. This module stays as the name the API side has always called them by, so
//! `crate::convert::{json_to_value, value_to_json}` still resolves and the row
//! endpoints read as they did.

pub use sc_types::{json_to_value, value_to_json};
