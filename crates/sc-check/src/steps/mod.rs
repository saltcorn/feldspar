//! The core's step types: `core.*`, `data.*`, `http.*` and `app.*`
//! (docs/CHECKLISTS.md §2).

use std::sync::Arc;

use crate::registry::StepType;

pub mod app;
pub mod core;
pub mod data;
pub mod http;

/// Every built-in step type.
pub fn builtin() -> Vec<Arc<dyn StepType>> {
    vec![
        Arc::new(core::Wait),
        Arc::new(core::Manual),
        Arc::new(data::Insert),
        Arc::new(data::Update),
        Arc::new(data::Delete),
        Arc::new(data::ExpectRows),
        Arc::new(data::ExpectDenied),
        Arc::new(data::RunTrigger),
        Arc::new(data::ExpectCalculated),
        Arc::new(http::Request),
        Arc::new(http::ExpectStatus),
        Arc::new(http::ExpectJson),
        Arc::new(app::Builds),
    ]
}
