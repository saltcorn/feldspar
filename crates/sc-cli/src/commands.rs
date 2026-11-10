//! The subcommands, one module per command or family of commands. `main`
//! dispatches to them.

pub(crate) mod agent;
pub(crate) mod api;
pub(crate) mod app;
pub(crate) mod auth;
pub(crate) mod backup;
pub(crate) mod check;
pub(crate) mod cmdstan;
pub(crate) mod config;
pub(crate) mod demo;
pub(crate) mod i18n;
pub(crate) mod mcp_token;
pub(crate) mod migrate;
pub(crate) mod module;
pub(crate) mod python;
pub(crate) mod registry;
pub(crate) mod serve;
pub(crate) mod trigger;
pub(crate) mod users;
