//! The admin API, from below it (§13.6): the seam the administrative tools
//! reach the server's own handlers through.
//!
//! Creating an application is not one write. The admin's Create button makes the
//! file store it asked for, saves the record, scaffolds the project with its
//! generated client, creates the agent that will build it and mounts what can be
//! mounted — and the agent half needs `sc-agent`'s trait registry, the mount half
//! the server's `AppMounts`. The tools that let an agent create an application
//! (`sc_app::mcp`) sit far below both, and so does the `admin_copilot` trait that
//! offers them. A second implementation of that sequence for the tools would be
//! the drift `sc_api::mcp`'s module comment was written to prevent.
//!
//! So the tools **call the handler the button calls**, through this trait —
//! declared here because a [`Catalog`] is what every caller already holds, and
//! installed by `sc-server` when it builds its router: the arrangement
//! [`ModelHost`](crate::ModelHost) already has, for the same reason.
//!
//! **One method per operation** (TODO-code-quality Q5.3). Each is implemented
//! by calling the handler's own function, not by looking an endpoint up by
//! name, so a missing or renamed operation is a compile error. The arguments
//! are typed; an application or a stream is passed as the admin API's JSON
//! document, because their types live in crates above this one and the
//! handler parses that document anyway. What comes back is the handler's
//! answer, which the tools pass on to the agent.
//!
//! **Every operation takes a [`ConsoleAdmin`]** (security review 2026-11 #16,
//! TODO-type-hardening H3.3): the proof that an administrator at the console
//! (or the server, with a named reason) is asking, which only the console's
//! own checks make. It used to take a bare `Option<Uuid>`, so a new tool that
//! forgot its admin check got every handler here. The implementation checks
//! again that the named user is a live administrator before it runs the
//! handler as one.
//!
//! [`Catalog`]: crate::Catalog

use async_trait::async_trait;
use sc_error::Result;
use serde_json::{Map, Value as Json};
use uuid::Uuid;

use crate::ConsoleAdmin;

/// A file store backend's operation (`runBackendOperation`), run on the
/// settings a store would be created with.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendOperation {
    /// The backend (`local`, `git`).
    pub backend: String,
    /// The operation (`suggest_dir`, `generate_key`).
    pub operation: String,
    /// The store's name.
    pub name: String,
    /// The store's settings so far.
    pub config: Map<String, Json>,
}

/// A file store to create (`createFileStore`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewFileStore {
    /// Its name.
    pub name: String,
    /// Its backend.
    pub backend: String,
    /// A description; empty for none.
    pub description: String,
    /// The backend's settings.
    pub config: Map<String, Json>,
}

/// Whose translations: one application's, or the instance's shared catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationsOf {
    /// The application with this id.
    Application(Uuid),
    /// The instance's shared catalogue.
    Instance,
}

/// The server's admin operations the administrative tools reach.
#[async_trait]
pub trait AdminHost: Send + Sync {
    /// Run a file store backend's operation; the answer carries the settings
    /// it produced under `config`.
    async fn backend_operation(&self, admin: &ConsoleAdmin, op: BackendOperation) -> Result<Json>;

    /// Create a file store; the answer is the store as the admin API shows it.
    async fn create_file_store(&self, admin: &ConsoleAdmin, store: NewFileStore) -> Result<Json>;

    /// Create an application from the admin API's application document.
    async fn create_application(&self, admin: &ConsoleAdmin, application: Json) -> Result<Json>;

    /// Every application, as the admin API lists them.
    async fn list_applications(&self, admin: &ConsoleAdmin) -> Result<Vec<Json>>;

    /// Update the application `id` from the admin API's application document.
    async fn update_application(
        &self,
        admin: &ConsoleAdmin,
        id: Uuid,
        application: Json,
    ) -> Result<Json>;

    /// Build the application `id`.
    async fn build_application(&self, admin: &ConsoleAdmin, id: Uuid) -> Result<Json>;

    /// The translations screen of an application or of the instance.
    async fn translations(&self, admin: &ConsoleAdmin, of: TranslationsOf) -> Result<Json>;

    /// Save one locale's messages for an application or the instance.
    async fn save_translations(
        &self,
        admin: &ConsoleAdmin,
        of: TranslationsOf,
        locale: String,
        messages: Map<String, Json>,
    ) -> Result<Json>;

    /// Save a stream from the admin API's stream document.
    async fn save_stream(&self, admin: &ConsoleAdmin, stream: Json) -> Result<Json>;

    /// Delete the stream `id`.
    async fn delete_stream(&self, admin: &ConsoleAdmin, id: Uuid) -> Result<Json>;
}
