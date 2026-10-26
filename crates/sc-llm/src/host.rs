//! The LLM provider a **host** supplies, and the installation's default
//! provider.
//!
//! An operator may give an instance an LLM provider in `feldspar.toml`
//! (`[environments.NAME.llm_provider]`): a key they pay for, with the models
//! they allow. It is the TLS keys' arrangement applied to a provider — the file
//! wins, and the admin of the instance, who on managed hosting is not the
//! operator, can use it but not change it:
//!
//! - **It is never a row.** [`set_host_llm_provider`] holds it on the
//!   [`Catalog`], and `storage`'s readers merge it in — so an agent names it like
//!   any other provider, it is listed beside the admin's, and its models are
//!   found by name — while its key is in no backup, Clear all does not remove it
//!   and a restore cannot overwrite it.
//! - **Every write to it is refused**, with a sentence naming the file: saving
//!   or deleting the provider, and adding, changing or removing its models. A
//!   provider the admin adds may not take its name either.
//! - **Its ids are derived from its name** (UUID v5), so the admin UI's links
//!   and a run's record of which model it called survive a restart.
//!
//! The **default provider** is what a caller uses when nothing names one — a
//! new application's builder agent, a translation, an eval. The admin picks it
//! on the LLM providers screen ([`set_default_llm_provider`], stored as
//! `default_llm_provider` in `_fd_config`); unset, it is the host's provider,
//! else the first by name. So an operator who supplies a provider has it used
//! out of the box, and the admin can still add their own and make it the
//! default.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::def::{LlmProviderDef, LlmProviderDefId, validate_provider_config};
use crate::model::{
    LlmModelDef, LlmModelDefId, ModelKind, normalise_model_config, validate_model_config,
};

/// The namespace the host provider's ids are derived in.
const ID_NAMESPACE: Uuid = Uuid::from_u128(0x0b5f_31a7_8c2e_4d61_9a0e_6f4c_2d8b_e173);

/// The LLM provider this host supplies, with its models — checked, and with
/// the ids derived from the names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLlmProvider {
    def: LlmProviderDef,
    models: Vec<LlmModelDef>,
}

impl HostLlmProvider {
    /// Check `def` and `models` and give them their derived ids.
    ///
    /// `models` are the provider's models with their settings; `default_model`
    /// is the one marked default, and is added with the built-in settings if
    /// the list leaves it out. Refused: a provider whose settings do not fit its
    /// backend, a model whose settings do not, two models of one name.
    pub fn new(
        def: LlmProviderDef,
        models: Vec<LlmModelDef>,
        default_model: &str,
    ) -> Result<HostLlmProvider> {
        let mut def = def;
        def.name = def.name.trim().to_owned();
        if def.name.is_empty() {
            return Err(Error::config("an LLM provider needs a name"));
        }
        def.id = host_provider_id(&def.name);
        validate_provider_config(&def)?;

        let default_model = default_model.trim();
        let mut models = models;
        for model in &mut models {
            model.name = model.name.trim().to_owned();
        }
        if !models.iter().any(|m| m.name == default_model) {
            models.push(LlmModelDef::new(def.id, default_model));
        }
        let mut checked: Vec<LlmModelDef> = Vec::new();
        for mut model in models {
            if model.name.is_empty() {
                return Err(Error::config(format!(
                    "a model of the `{}` LLM provider has no name",
                    def.name
                )));
            }
            if checked.iter().any(|m| m.name == model.name) {
                return Err(Error::config(format!(
                    "the `{}` LLM provider lists the model `{}` twice",
                    def.name, model.name
                )));
            }
            model.id = host_model_id(&def.name, &model.name);
            model.provider_id = def.id;
            model.is_default = model.name == default_model;
            if model.is_default && model.kind != ModelKind::Chat {
                return Err(Error::config(format!(
                    "the `{}` LLM provider's default model `{}` is {} model; the default \
                     model is the chat model agents call",
                    def.name, model.name, model.kind
                )));
            }
            model.config = normalise_model_config(&model.config);
            validate_model_config(&def.backend, &model)?;
            checked.push(model);
        }
        checked.sort_by(|a, b| a.name.cmp(&b.name));
        // Each other kind's default is its first model by name.
        for kind in ModelKind::ALL {
            if kind != ModelKind::Chat
                && let Some(first) = checked.iter_mut().find(|m| m.kind == kind)
            {
                first.is_default = true;
            }
        }
        Ok(HostLlmProvider {
            def,
            models: checked,
        })
    }

    /// The provider's definition.
    pub fn def(&self) -> &LlmProviderDef {
        &self.def
    }

    /// Its models, ordered by name, exactly one of them the default.
    pub fn models(&self) -> &[LlmModelDef] {
        &self.models
    }
}

/// The id the host provider called `name` has, on every boot.
pub fn host_provider_id(name: &str) -> LlmProviderDefId {
    LlmProviderDefId(Uuid::new_v5(
        &ID_NAMESPACE,
        format!("provider\0{name}").as_bytes(),
    ))
}

/// The id the model `model` of the host provider `provider` has, on every boot.
fn host_model_id(provider: &str, model: &str) -> LlmModelDefId {
    LlmModelDefId(Uuid::new_v5(
        &ID_NAMESPACE,
        format!("model\0{provider}\0{model}").as_bytes(),
    ))
}

/// Make `provider` the LLM provider this host supplies — or, with `None`, stop
/// supplying one.
///
/// Refused when a provider the admin added already has its name: an agent
/// naming it could not say which it meant, and quietly shadowing the admin's
/// would swap the key it is billed to. Which one to rename is the operator's
/// call, and the file is the easier to change.
pub async fn set_host_llm_provider(
    catalog: &Catalog,
    provider: Option<HostLlmProvider>,
) -> Result<()> {
    if let Some(provider) = &provider
        && let Some(stored) =
            crate::storage::load_stored_provider_by_name(catalog, &provider.def.name).await?
    {
        return Err(Error::config(format!(
            "the configuration file supplies an LLM provider named `{}`, and the admin \
             has already added one by that name (id {}); rename the one in the file",
            stored.name, stored.id.0
        )));
    }
    catalog.set_host_llm_provider(provider.map(|p| Arc::new(p) as _));
    Ok(())
}

/// The LLM provider this host supplies, if it supplies one.
pub fn host_llm_provider(catalog: &Catalog) -> Option<Arc<HostLlmProvider>> {
    catalog
        .host_llm_provider()?
        .downcast::<HostLlmProvider>()
        .ok()
}

/// Whether `id` is the provider this host supplies — the one no write may touch.
pub fn is_host_llm_provider(catalog: &Catalog, id: LlmProviderDefId) -> bool {
    host_llm_provider(catalog).is_some_and(|host| host.def.id == id)
}

/// The refusal for a write to the host's provider or one of its models.
pub(crate) fn read_only(what: &str) -> Error {
    Error::invalid(format!(
        "{what} is set by the server's configuration file (feldspar.toml) and cannot \
         be changed here"
    ))
}

/// The installation's default LLM provider: the one the admin chose, else the
/// one this host supplies, else the first by name — `None` only when there is
/// no provider at all.
///
/// A chosen provider that has since been deleted is not an error: the choice
/// falls back as if it had not been made, which is what deleting the default
/// should do.
pub async fn default_llm_provider(catalog: &Catalog) -> Result<Option<LlmProviderDef>> {
    if let Some(id) = chosen_default(catalog).await?
        && let Some(def) = crate::storage::load_llm_provider(catalog, id).await?
    {
        return Ok(Some(def));
    }
    if let Some(host) = host_llm_provider(catalog) {
        return Ok(Some(host.def.clone()));
    }
    Ok(crate::storage::list_llm_providers(catalog)
        .await?
        .into_iter()
        .next())
}

/// Make `id` the default LLM provider, or with `None` go back to the fallback.
pub async fn set_default_llm_provider(
    catalog: &Catalog,
    id: Option<LlmProviderDefId>,
) -> Result<()> {
    let value = match id {
        Some(id) => {
            crate::storage::load_llm_provider(catalog, id)
                .await?
                .ok_or_else(|| Error::not_found(format!("no LLM provider with id {}", id.0)))?;
            Json::from(id.0.to_string())
        }
        None => Json::Null,
    };
    sc_config::set_config(catalog, sc_config::DEFAULT_LLM_PROVIDER, value).await
}

/// The id stored as the default, if one is — and the configuration table is
/// there to hold it.
async fn chosen_default(catalog: &Catalog) -> Result<Option<LlmProviderDefId>> {
    if catalog.get(sc_config::CONFIG_TABLE)?.is_none() {
        return Ok(None);
    }
    let stored = sc_config::stored_config(catalog, sc_config::DEFAULT_LLM_PROVIDER).await?;
    Ok(stored
        .as_ref()
        .and_then(Json::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .map(LlmProviderDefId))
}
