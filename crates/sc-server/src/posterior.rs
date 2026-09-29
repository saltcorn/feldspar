//! The server's half of reading a posterior (Stan TODO §§16, 18): what the
//! handlers need that `sc-model` cannot do from below the row layer — reading
//! a model's datasets for a preview, and zipping a run for download.

use std::io::{Cursor, Write as _};

use sc_catalog::Catalog;
use sc_error::{Context as _, Error, Result};
use sc_model::{Frame, Interface, MAIN_DATASET, Model, ModelInstance, ModelProvider};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use zip::write::SimpleFileOptions;

use crate::ModelServices;

/// The provider of `model` and the interface its program declares, refused by
/// name for a provider that binds no data.
pub(crate) async fn binding_interface(
    models: &ModelServices,
    model: &Model,
) -> Result<(Arc<dyn ModelProvider>, Interface)> {
    let provider = Arc::clone(models.registry().require(model.provider.trim())?);
    if !provider.binds_data() {
        return Err(Error::invalid(format!(
            "the model provider `{}` takes one dataset as it is, and has no data to bind",
            provider.name()
        )));
    }
    let interface = provider
        .interface(&model.configuration)
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "the model provider `{}` declares no program interface to bind",
                provider.name()
            ))
        })?;
    Ok((provider, interface))
}

/// Every dataset of `model`, read as a fit reads them: the main one as `main`,
/// each related one with its label, each under the row cap.
pub(crate) async fn posterior_datasets(
    models: &ModelServices,
    model: &Model,
) -> Result<Vec<(String, Frame)>> {
    let source = models.source();
    let mut out = Vec::with_capacity(1 + model.related.len());
    out.push((
        MAIN_DATASET.to_owned(),
        source
            .materialise(&model.dataset, models.max_rows())
            .await
            .context("reading the dataset")?,
    ));
    for related in &model.related {
        let frame = source
            .materialise(&sc_model::binding_dataset(related), models.max_rows())
            .await
            .with_context(|| format!("reading the related dataset `{}`", related.name))?;
        out.push((related.name.clone(), frame));
    }
    Ok(out)
}

/// The run of `instance` (a fit of `model`) as a zip: the raw CmdStan run when
/// the provider kept one, else one CSV of draws per chain from
/// `_fd_model_draws` with `coordinates.json` beside them. A raw run that can no
/// longer be read is replaced by the draws, and `README.txt` says why.
pub(crate) async fn run_zip(
    catalog: &Catalog,
    models: &ModelServices,
    model: &Model,
    instance: &ModelInstance,
) -> Result<Vec<u8>> {
    if instance.status != sc_model::FitStatus::Fitted {
        return Err(Error::invalid(format!(
            "instance {} is `{}`, so it has no run to download",
            instance.id, instance.status
        )));
    }
    let registry = models.registry();
    let provider = registry.get(model.provider.trim());
    let raw = match &provider {
        Some(provider) => provider.run_files(&instance.state).await,
        None => Ok(None),
    };
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    match raw {
        Ok(Some(raw)) => files = raw,
        Ok(None) => {}
        Err(e) => files.push((
            "README.txt".to_owned(),
            format!(
                "{}.\n\nThese are the draws this server stored for the fit instead: one CSV \
                 per chain, in CmdStan's column layout, without CmdStan's comment lines.\n",
                sc_error::format_chain(&e)
            )
            .into_bytes(),
        )),
    }
    if files.iter().all(|(name, _)| name == "README.txt") {
        let draws = sc_model::draws_csv(catalog, instance).await?;
        if draws.is_empty() {
            return Err(Error::invalid(format!(
                "instance {} kept no draws (`keep_draws: false`) and no raw run, so there is \
                 nothing to download; its summary is on the instance",
                instance.id
            )));
        }
        files.extend(draws);
        let coordinates = instance
            .attributes
            .get(sc_model::ATTR_COORDINATES)
            .cloned()
            .unwrap_or_else(|| json!({ "dimensions": [] }));
        files.push(("coordinates.json".to_owned(), pretty(&coordinates)?));
        files.push((
            "variables.json".to_owned(),
            pretty(
                instance
                    .attributes
                    .get(sc_model::ATTR_AXES)
                    .unwrap_or(&Json::Null),
            )?,
        ));
    }
    zip_files(&files)
}

fn pretty(value: &Json) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(|e| Error::msg(format!("writing JSON: {e}")))
}

/// `files` as a zip, deflated.
pub(crate) fn zip_files(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        zip.start_file(name.as_str(), options)
            .map_err(|e| Error::msg(format!("zipping {name}: {e}")))?;
        zip.write_all(bytes)
            .map_err(|e| Error::msg(format!("zipping {name}: {e}")))?;
    }
    Ok(zip
        .finish()
        .map_err(|e| Error::msg(format!("finishing the zip: {e}")))?
        .into_inner())
}

/// The file name a run downloads as: `radon-<instance>.zip`.
pub(crate) fn run_filename(model: &Model, instance: &ModelInstance) -> String {
    let name: String = model
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}-{}.zip", instance.id)
}
