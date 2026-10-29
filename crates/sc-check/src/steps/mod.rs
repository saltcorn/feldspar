//! The step types compiled into this server (docs/CHECKLISTS.md §2): the
//! core's — `core.*`, `data.*`, `http.*`, `browser.*` and `app.*` — and the
//! built-in frameworks' — `react.*`, `saltcorn_ui.*` and `server_render.*`
//! (TODO C3.2–C3.4), each offered only for its framework's applications.

use std::sync::Arc;

use crate::registry::StepType;

pub mod app;
pub mod browser;
pub mod core;
pub mod data;
pub mod http;
pub mod react;
pub mod saltcorn_ui;
pub mod server_render;

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
        Arc::new(data::ExpectSent),
        Arc::new(http::Request),
        Arc::new(http::ExpectStatus),
        Arc::new(http::ExpectJson),
        Arc::new(browser::Open),
        Arc::new(browser::Click),
        Arc::new(browser::Fill),
        Arc::new(browser::Select),
        Arc::new(browser::Check),
        Arc::new(browser::Press),
        Arc::new(browser::Upload),
        Arc::new(browser::ExpectText),
        Arc::new(browser::ExpectNoText),
        Arc::new(browser::ExpectElement),
        Arc::new(browser::ExpectUrl),
        Arc::new(browser::ExpectClean),
        Arc::new(browser::Screenshot),
        Arc::new(app::Builds),
    ]
}

/// The built-in frameworks' step types.
pub fn framework_steps() -> Vec<Arc<dyn StepType>> {
    vec![
        Arc::new(react::Typecheck),
        Arc::new(react::OpenRoute),
        Arc::new(react::NoErrorBoundary),
        Arc::new(saltcorn_ui::OpenView),
        Arc::new(saltcorn_ui::FillField),
        Arc::new(saltcorn_ui::ViewsRender),
        Arc::new(server_render::PageRenders),
    ]
}
