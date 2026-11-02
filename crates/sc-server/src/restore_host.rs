//! The running server's services, as a backup restore writes into them
//! ([`sc_backup::RestoreHost`]): the mount registry reaches each of them.

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
