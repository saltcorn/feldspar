//! The running server's services, as a backup restore writes into them
//! ([`sc_backup::RestoreHost`]) and a deploy or registry save does
//! ([`sc_sync::DeployHost`]): the mount registry reaches each of them.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_error::Result;

use crate::apps::AppMounts;
use crate::handlers::{agents_of, dataset_shape, models_of, modules_of, streams_of, triggers_of};

#[async_trait::async_trait]
impl sc_backup::RestoreHost for AppMounts {
    fn agent_registry(&self) -> Result<Arc<sc_agent::AgentRegistry>> {
        Ok(Arc::clone(agents_of(self)?.registry()))
    }

    fn trigger_dispatcher(&self) -> Result<Arc<sc_action::TriggerDispatcher>> {
        triggers_of(self)
    }

    fn modules_installed(&self) -> Result<()> {
        modules_of(self).map(|_| ())
    }

    async fn install_module_package(
        &self,
        language: sc_module::ModuleLanguage,
        source: sc_module::ModuleSource,
        location: &str,
    ) -> Result<sc_module::InstalledPackage> {
        modules_of(self)?
            .install_package(language, source, location)
            .await
    }

    async fn reload_modules(&self) -> Result<()> {
        modules_of(self)?.reload().await
    }

    fn stream_registry(&self) -> Result<Arc<sc_stream::StreamRegistry>> {
        Ok(streams_of(self)?.registry())
    }

    async fn reload_streams(&self, catalog: &Catalog) -> Result<bool> {
        streams_of(self)?.reload(catalog).await
    }

    fn model_registry(&self) -> Result<Arc<sc_model::ModelRegistry>> {
        Ok(models_of(self)?.registry())
    }

    async fn dataset_shape(&self, dataset: &sc_model::Dataset) -> Option<sc_model::DatasetShape> {
        let models = models_of(self).ok()?;
        dataset_shape(&models, dataset).await
    }

    async fn build_and_mount(&self, app: sc_app::Application) -> Result<sc_app::BuildReport> {
        crate::apps::build_and_mount(self, app).await
    }
}

#[async_trait::async_trait]
impl sc_sync::DeployHost for AppMounts {
    /// Everything loaded again after a deploy or a registry save: the
    /// catalog, the triggers, the streams, and every mounted application
    /// re-projected. What was saved is saved, so each problem is a sentence
    /// on the receipt rather than an error.
    async fn reload_all(&self, catalog: &Catalog) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(e) = catalog.reload().await {
            problems.push(format!("the catalog could not be reloaded: {}", e.causes()));
        }
        // A server that runs no triggers (or no streams) has none to reload.
        if let Ok(dispatcher) = triggers_of(self)
            && let Err(e) = dispatcher.reload(catalog).await
        {
            problems.push(format!(
                "the triggers could not be reloaded: {}",
                e.causes()
            ));
        }
        if let Ok(streams) = streams_of(self)
            && let Err(e) = streams.reload(catalog).await
        {
            problems.push(format!(
                "the streams could not be restarted: {}",
                e.causes()
            ));
        }
        if let Err(e) = self.refresh_triggers() {
            problems.push(format!(
                "an application could not be re-projected and keeps its previous mount: {}",
                e.causes()
            ));
        }
        problems
    }

    /// The registries an export redacts secrets by: this server's, and the
    /// built-in ones for a service it does not run (an admin-only server has
    /// no trigger dispatcher), so a registry read works on every server.
    fn registries(&self) -> Result<sc_sync::Registries> {
        let builtin = sc_sync::Registries::builtin()?;
        Ok(sc_sync::Registries {
            actions: triggers_of(self).map_or(builtin.actions, |d| d.registry()),
            streams: streams_of(self).map_or(builtin.streams, |s| s.registry()),
            agent_traits: agents_of(self)
                .map_or(builtin.agent_traits, |a| Arc::clone(a.registry())),
            modules: std::collections::BTreeMap::new(),
        })
    }
}
