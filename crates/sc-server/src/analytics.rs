//! The Analytics UI's handlers (analytics TODO A1.13, A2.6, A2.8, A2.14, A4.2, A5.5–A5.13):
//! datasets, plots, hypothesis tests, panels, map layers, the Map workspace's attribute table,
//! selection and toolbox, and workspaces, over `sc-dataset` and `sc-analytics`. The endpoints
//! are declared in `sc-api`'s `analytics.rs`, which says what each one is for.
//!
//! **Who asks** (A9.1, A9.4). Every handler starts from an [`Access`]: the
//! caller — whose reads are guarded to the rows their table permissions and
//! ownership formulas allow — and, on an Analytics application's host, what
//! that application allows. On the admin host the caller is the admin and
//! nothing is narrowed. In an application, a dataset or workspace it does not
//! show is answered as one that does not exist, and a change it does not allow
//! is refused with a sentence.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_analytics::app::AppScope;
use sc_analytics::layer;
use sc_analytics::map;
use sc_analytics::panel::Panel;
use sc_analytics::plot::{self, PlotSpec};
use sc_analytics::selection;
use sc_analytics::stats;
use sc_analytics::tools;
use sc_analytics::{Workspace, WorkspaceId, WorkspaceKind};
use sc_catalog::Catalog;
use sc_dataset::{
    Base, Caller, Compilation, DatasetDef, DatasetId, Library, OpStatus, Operation, Options, Page,
    Schema, Sharing, compile,
};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::handler::{HandlerCtx, HandlerRegistry, HandlerResponse};

/// The most distinct values `datasetColumnValues` answers.
const MAX_VALUES: u64 = 200;
/// The rows a stage read answers when the caller does not say.
const DEFAULT_PAGE: u64 = 100;

/// The fit a model's outputs show when none is named: the active one, else
/// the newest that fitted.
async fn shown_fit(
    catalog: &Catalog,
    model: sc_model::ModelId,
) -> Result<Option<sc_model::ModelInstance>> {
    let fits = sc_model::list_model_instances(catalog, model).await?;
    if let Some(active) = fits.iter().find(|i| i.active) {
        return Ok(Some(active.clone()));
    }
    Ok(fits
        .into_iter()
        .find(|i| i.status == sc_model::FitStatus::Fitted))
}

/// Who is asking, and through which Analytics application (A9.1, A9.4).
struct Access {
    /// The caller every read is guarded as: in a self-serve application, only
    /// its tables.
    caller: Caller,
    /// The application, on its host; `None` on the admin host.
    app: Option<Arc<AppScope>>,
}

impl Access {
    fn of(ctx: &HandlerCtx) -> Access {
        let caller = Caller::of_user(ctx.user.as_ref());
        match &ctx.analytics_app {
            Some(app) => Access {
                caller: app.narrow(caller),
                app: Some(app.clone()),
            },
            None => Access { caller, app: None },
        }
    }

    /// Whether the caller sees `def`, shared as `sharing`.
    fn sees(&self, def: &DatasetDef, sharing: &Sharing, library: &Library) -> bool {
        match &self.app {
            Some(app) => app.sees_dataset(&self.caller, def, sharing, library),
            None => sharing.admits(&self.caller),
        }
    }

    /// The stored dataset `id`, if the caller sees it; one they do not is
    /// answered as one that is not there.
    async fn dataset(&self, catalog: &Catalog, id: DatasetId) -> Result<(DatasetDef, Sharing)> {
        let missing = || Error::not_found(format!("there is no dataset with id {id}"));
        let def = sc_dataset::load_dataset(catalog, id)
            .await?
            .ok_or_else(missing)?;
        let sharing = sc_dataset::dataset_sharing(catalog, id)
            .await?
            .unwrap_or_default();
        if self.app.is_some() || !self.caller.is_admin() {
            let library = sc_dataset::load_library(catalog).await?;
            if !self.sees(&def, &sharing, &library) {
                return Err(missing());
            }
        }
        Ok((def, sharing))
    }

    /// Refuse unless the caller sees every one of `ids`.
    async fn check_datasets(
        &self,
        catalog: &Catalog,
        ids: impl IntoIterator<Item = DatasetId>,
    ) -> Result<()> {
        if self.app.is_none() && self.caller.is_admin() {
            return Ok(());
        }
        for id in ids {
            self.dataset(catalog, id).await?;
        }
        Ok(())
    }

    /// Refuse unless datasets may be created and changed here.
    fn check_editor(&self) -> Result<()> {
        match &self.app {
            Some(app) => app.check_dataset_editor(),
            None => Ok(()),
        }
    }

    /// Refuse unless the caller may change a dataset shared as `sharing`.
    fn check_change_dataset(&self, def: &DatasetDef, sharing: &Sharing) -> Result<()> {
        self.check_editor()?;
        if sharing.may_change(&self.caller) {
            Ok(())
        } else {
            Err(Error::auth(format!(
                "`{}` is not yours to change; clone it to make one of your own",
                def.name
            )))
        }
    }

    /// Refuse a new dataset's base unless it may be built on here: one of the
    /// application's tables, or a dataset the caller sees.
    async fn check_base(&self, catalog: &Catalog, base: &Base) -> Result<()> {
        match (base, &self.app) {
            (_, None) => Ok(()),
            (Base::Table { table }, Some(app)) => {
                if app.offers_table(table) {
                    Ok(())
                } else {
                    Err(Error::auth(format!(
                        "`{table}` is not one of the tables this application reads"
                    )))
                }
            }
            (Base::Dataset { dataset }, Some(_)) => self.check_datasets(catalog, [*dataset]).await,
        }
    }

    /// The definition a read endpoint's body names, as this caller may read
    /// it. In an application: a stored dataset must be one they see, and
    /// where they may not edit datasets it is read as stored, whatever
    /// operations were sent; one not stored needs the Dataset editor and a base
    /// they may build on.
    async fn body_def(&self, catalog: &Catalog, def: DatasetDef) -> Result<DatasetDef> {
        let Some(app) = &self.app else {
            return Ok(def);
        };
        if sc_dataset::load_dataset(catalog, def.id).await?.is_some() {
            let (stored, _) = self.dataset(catalog, def.id).await?;
            return Ok(if app.check_dataset_editor().is_ok() {
                def
            } else {
                stored
            });
        }
        self.check_editor()?;
        self.check_base(catalog, &def.base).await?;
        Ok(def)
    }

    /// Whether `table` is one a formula may name here, for the editor's
    /// completions: every table on the admin host, the application's own in a
    /// self-serve application, none in a fixed one.
    fn names_table(&self, table: &str) -> bool {
        match &self.app {
            None => true,
            Some(app) => app.offers_table(table),
        }
    }

    /// Whether the caller sees the workspace `ws`: in an application, as it
    /// says; on the admin host, the unrestricted UI's own.
    fn sees_workspace(&self, ws: &Workspace) -> bool {
        match &self.app {
            Some(app) => app.sees_workspace(&self.caller, ws),
            None => ws.application.is_none(),
        }
    }

    /// The workspace `id`, if the caller sees it.
    async fn workspace(&self, catalog: &Catalog, id: WorkspaceId) -> Result<Workspace> {
        let ws = sc_analytics::require_workspace(catalog, id).await?;
        // The admin opens any workspace by its id, an application's included.
        if self.app.is_none() || self.sees_workspace(&ws) {
            Ok(ws)
        } else {
            Err(Error::not_found(format!(
                "there is no workspace with id {id}"
            )))
        }
    }

    /// Refuse unless the caller may change `ws`.
    fn check_change_workspace(&self, ws: &Workspace) -> Result<()> {
        match &self.app {
            Some(app) => app.check_change_workspace(&self.caller, ws),
            None => Ok(()),
        }
    }

    /// A workspace as the endpoints answer it, with whether the caller may
    /// change it.
    fn workspace_json(&self, ws: &Workspace) -> Json {
        let mut out = workspace_json(ws);
        if let Some(fields) = out.as_object_mut() {
            fields.insert(
                "may_change".into(),
                Json::Bool(self.check_change_workspace(ws).is_ok()),
            );
        }
        out
    }
}

/// Register the Analytics UI's handlers on `reg`.
pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>) {
    // --- datasets ------------------------------------------------------------

    reg.register("listDatasets", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let schema = Schema::of_catalog(&catalog)?;
                let library = sc_dataset::load_library(&catalog).await?;
                let sharing = sc_dataset::list_dataset_sharing(&catalog).await?;
                let out: Vec<Json> = library
                    .defs()
                    .filter_map(|def| {
                        let shared = sharing.get(&def.id).copied().unwrap_or_default();
                        access.sees(def, &shared, &library).then_some((def, shared))
                    })
                    .map(|(def, shared)| {
                        let resolved = sc_model::Dataset::of_def(&schema, &library, def);
                        json!({
                            "id": def.id,
                            "name": def.name,
                            "description": def.description,
                            "base": def.base,
                            "table": resolved.table,
                            "operations": def.operations.len(),
                            "columns": resolved
                                .columns
                                .iter()
                                .map(|c| json!({ "name": c.name, "type": c.ty, "key": c.key }))
                                .collect::<Vec<_>>(),
                            "error": resolved.error,
                            "grain": resolved.grain,
                            "share_role": shared.share_role,
                            "may_change": access.check_editor().is_ok()
                                && shared.may_change(&access.caller),
                        })
                    })
                    .collect();
                let mut out = out;
                out.sort_by(|a, b| {
                    a["name"]
                        .as_str()
                        .unwrap_or_default()
                        .cmp(b["name"].as_str().unwrap_or_default())
                });
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register("getDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let (def, _) = access.dataset(&catalog, dataset_id(&ctx)?).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &access, &def).await?))
            }
        }
    });

    reg.register("createDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                access.check_editor()?;
                let def = def_from_input(&ctx.body, DatasetId::new())?;
                access.check_base(&catalog, &def.base).await?;
                sc_dataset::save_dataset(&catalog, &def).await?;
                owned_by_caller(&catalog, &access, def.id).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &access, &def).await?).with_status(201))
            }
        }
    });

    reg.register("updateDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let id = dataset_id(&ctx)?;
                let (stored, sharing) = access.dataset(&catalog, id).await?;
                access.check_change_dataset(&stored, &sharing)?;
                let def = def_from_input(&ctx.body, id)?;
                sc_dataset::save_dataset(&catalog, &def).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &access, &def).await?))
            }
        }
    });

    reg.register("deleteDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let id = dataset_id(&ctx)?;
                let (stored, sharing) = access.dataset(&catalog, id).await?;
                access.check_change_dataset(&stored, &sharing)?;
                if !sc_dataset::delete_dataset(&catalog, id).await? {
                    return Err(Error::not_found(format!(
                        "there is no dataset with id {id}"
                    )));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.register("shareDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let (def, sharing) = access.dataset(&catalog, dataset_id(&ctx)?).await?;
                access.check_change_dataset(&def, &sharing)?;
                let share_role = share_role(&ctx.body)?;
                sc_dataset::set_dataset_sharing(
                    &catalog,
                    def.id,
                    Sharing {
                        share_role,
                        ..sharing
                    },
                )
                .await?;
                Ok(HandlerResponse::ok(detail(&catalog, &access, &def).await?))
            }
        }
    });

    reg.register("cloneDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                access.check_editor()?;
                let (source, _) = access.dataset(&catalog, dataset_id(&ctx)?).await?;
                let name = ctx.body.get("name").and_then(Json::as_str);
                let copy = sc_dataset::clone_dataset(&catalog, source.id, name).await?;
                owned_by_caller(&catalog, &access, copy.id).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &access, &copy).await?).with_status(201))
            }
        }
    });

    reg.register("datasetUsage", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let id = dataset_id(&ctx)?;
                access.dataset(&catalog, id).await?;
                let library = sc_dataset::load_library(&catalog).await?;
                let sharing = sc_dataset::list_dataset_sharing(&catalog).await?;
                let datasets: Vec<Json> = sc_dataset::datasets_using(&catalog, id)
                    .await?
                    .iter()
                    .filter(|d| {
                        access.sees(
                            d,
                            &sharing.get(&d.id).copied().unwrap_or_default(),
                            &library,
                        )
                    })
                    .map(|d| json!({ "id": d.id, "name": d.name }))
                    .collect();
                // Models are the unrestricted UI's: an application lists none.
                let models: Vec<Json> = match access.app {
                    Some(_) => Vec::new(),
                    None => sc_model::list_models(&catalog)
                        .await?
                        .iter()
                        .filter(|m| {
                            m.dataset.id == id || m.related.iter().any(|r| r.dataset.id == id)
                        })
                        .map(|m| json!({ "id": m.id.0, "name": m.name }))
                        .collect(),
                };
                let workspaces = sc_analytics::list_workspaces(&catalog).await?;
                let visible: Vec<Workspace> = workspaces
                    .into_iter()
                    .filter(|w| access.sees_workspace(w))
                    .collect();
                let index = sc_analytics::panel::UsageIndex::of_workspaces(&visible);
                Ok(HandlerResponse::ok(json!({
                    "datasets": datasets,
                    "models": models,
                    "workspaces": index.dataset(id),
                })))
            }
        }
    });

    reg.register("datasetShapes", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let def = access.body_def(&catalog, def_from_body(&ctx.body)?).await?;
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                Ok(HandlerResponse::ok(report(&schema, &access, &compiled)))
            }
        }
    });

    reg.register("validateDatasetOperation", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                access.check_editor()?;
                let mut def = access.body_def(&catalog, def_from_body(&ctx.body)?).await?;
                let position = ctx
                    .body
                    .get("position")
                    .and_then(Json::as_u64)
                    .ok_or_else(|| Error::invalid("`position` is required"))?
                    as usize;
                let replace = ctx
                    .body
                    .get("replace")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                let operation: Operation = serde_json::from_value(
                    ctx.body
                        .get("operation")
                        .cloned()
                        .ok_or_else(|| Error::invalid("`operation` is required"))?,
                )
                .map_err(|e| Error::invalid(format!("`operation` is not an operation: {e}")))?;
                let len = def.operations.len();
                if position > len || (replace && position >= len) {
                    return Err(Error::invalid(format!(
                        "the dataset has {len} operations, so there is no position {position}"
                    )));
                }
                if replace {
                    def.operations[position] = operation;
                } else {
                    def.operations.insert(position, operation);
                }
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                let answer = match compiled.first_error() {
                    Some((i, report)) if i < position => json!({
                        "error": format!(
                            "operation {} ({}) before it has an error: {}",
                            i + 1,
                            report.kind,
                            report.error.as_deref().unwrap_or_default()
                        ),
                        "shape": Json::Null,
                    }),
                    _ => match compiled.base.error.as_ref() {
                        Some(e) => json!({ "error": e, "shape": Json::Null }),
                        None => {
                            let report = &compiled.operations[position];
                            json!({ "error": report.error, "shape": report.shape })
                        }
                    },
                };
                Ok(HandlerResponse::ok(answer))
            }
        }
    });

    reg.register("readDatasetStage", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let def = access.body_def(&catalog, def_from_body(&ctx.body)?).await?;
                let upto = optional_usize(&ctx.body, "upto");
                let offset = optional_usize(&ctx.body, "offset").unwrap_or(0) as u64;
                let limit = optional_usize(&ctx.body, "limit").map_or(DEFAULT_PAGE, |l| l as u64);
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                let stage = compiled
                    .stage(upto.unwrap_or(def.operations.len()))
                    .map_err(Error::invalid)?;
                let page =
                    sc_dataset::read_page(&catalog, &access.caller, stage, Page { offset, limit })
                        .await?;
                Ok(HandlerResponse::ok(json!({
                    "columns": page.columns,
                    "grain": page.grain,
                    "rows": page
                        .rows
                        .iter()
                        .map(|r| Json::Array(r.iter().map(sc_dataset::value_json).collect()))
                        .collect::<Vec<_>>(),
                    "total": page.total,
                })))
            }
        }
    });

    reg.register("datasetColumnValues", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let def = access.body_def(&catalog, def_from_body(&ctx.body)?).await?;
                let upto = optional_usize(&ctx.body, "upto");
                let column = ctx
                    .body
                    .get("column")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::invalid("`column` is required"))?
                    .to_owned();
                let limit = optional_usize(&ctx.body, "limit")
                    .map_or(MAX_VALUES, |l| (l as u64).min(MAX_VALUES));
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                let stage = compiled
                    .stage(upto.unwrap_or(def.operations.len()))
                    .map_err(Error::invalid)?;
                let values =
                    sc_dataset::column_values(&catalog, &access.caller, stage, &column, limit)
                        .await?;
                Ok(HandlerResponse::ok(Json::Array(
                    values.iter().map(sc_dataset::value_json).collect(),
                )))
            }
        }
    });

    reg.register("listDatasetTables", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let schema = Schema::of_catalog(&catalog)?;
                Ok(HandlerResponse::ok(Json::Array(
                    schema
                        .tables
                        .values()
                        // The base picker offers what may be built on here.
                        .filter(|t| access.names_table(&t.name))
                        .map(|t| {
                            json!({
                                "name": t.name,
                                "columns": t.columns,
                                "primary_key": t.primary_key,
                            })
                        })
                        .collect(),
                )))
            }
        }
    });

    // --- plots ---------------------------------------------------------------

    reg.register("renderPlot", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let spec: PlotSpec = serde_json::from_value(
                    ctx.body
                        .get("spec")
                        .cloned()
                        .ok_or_else(|| Error::invalid("`spec` is required"))?,
                )
                .map_err(|e| Error::invalid(format!("`spec` is not a plot spec: {e}")))?;
                let access = Access::of(&ctx);
                access
                    .check_datasets(&catalog, data_datasets(&spec.data))
                    .await?;
                let rendered = plot::render_plot(&catalog, &access.caller, &spec).await?;
                Ok(HandlerResponse::ok(
                    serde_json::to_value(rendered).map_err(|e| {
                        Error::serde(format!("a plot's data does not serialise: {e}"))
                    })?,
                ))
            }
        }
    });

    reg.register("renderTable", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let spec: plot::TableSpec = serde_json::from_value(
                    ctx.body
                        .get("spec")
                        .cloned()
                        .ok_or_else(|| Error::invalid("`spec` is required"))?,
                )
                .map_err(|e| Error::invalid(format!("`spec` is not a summary table: {e}")))?;
                let access = Access::of(&ctx);
                access
                    .check_datasets(&catalog, data_datasets(&spec.data))
                    .await?;
                let rendered = plot::render_table(&catalog, &access.caller, &spec).await?;
                Ok(HandlerResponse::ok(
                    serde_json::to_value(rendered).map_err(|e| {
                        Error::serde(format!("a table's data does not serialise: {e}"))
                    })?,
                ))
            }
        }
    });

    reg.register("runTests", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let spec: stats::TestSpec = serde_json::from_value(
                    ctx.body
                        .get("spec")
                        .cloned()
                        .ok_or_else(|| Error::invalid("`spec` is required"))?,
                )
                .map_err(|e| Error::invalid(format!("`spec` is not a set of test roles: {e}")))?;
                let access = Access::of(&ctx);
                access
                    .check_datasets(&catalog, data_datasets(&spec.data))
                    .await?;
                let answer = stats::run_tests(&catalog, &access.caller, &spec).await?;
                Ok(HandlerResponse::ok(serde_json::to_value(answer).map_err(
                    |e| Error::serde(format!("a test's results do not serialise: {e}")),
                )?))
            }
        }
    });

    reg.register("plotGallery", move |_ctx| async move {
        Ok(HandlerResponse::ok(json!(plot::gallery())))
    });

    reg.register("suggestPlot", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id: DatasetId = ctx
                    .body
                    .get("dataset")
                    .and_then(Json::as_str)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::invalid("`dataset` is required, as a dataset's id"))?;
                let assignment: plot::Assignment = match ctx.body.get("assignment") {
                    None | Some(Json::Null) => plot::Assignment::default(),
                    Some(a) => serde_json::from_value(a.clone()).map_err(|e| {
                        Error::invalid(format!("`assignment` is not a set of drop zones: {e}"))
                    })?,
                };
                let named = |key: &str| ctx.body.get(key).filter(|v| !v.is_null()).cloned();
                let preset: Option<plot::Preset> = named("preset")
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| Error::invalid(format!("`preset` is not a gallery item: {e}")))?;
                let mark: Option<plot::Mark> = named("mark")
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| Error::invalid(format!("`mark` is not a mark: {e}")))?;
                let (def, _) = Access::of(&ctx).dataset(&catalog, id).await?;
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                let shape = match compiled.last() {
                    Ok(stage) => stage.shape(),
                    Err(e) => {
                        return Ok(HandlerResponse::ok(json!({
                            "error": format!("the dataset `{}` does not read: {e}", def.name),
                        })));
                    }
                };
                let data = plot::DataRef::Dataset { dataset: id };
                let answer = match preset {
                    Some(p) => plot::preset(p, data, &shape, &assignment).map(
                        |(spec, assignment)| json!({ "spec": spec, "assignment": assignment }),
                    ),
                    None => plot::show_me(data, &shape, &assignment, mark)
                        .map(|spec| json!({ "spec": spec, "assignment": assignment })),
                };
                Ok(HandlerResponse::ok(
                    answer.unwrap_or_else(|error| json!({ "error": error })),
                ))
            }
        }
    });

    // --- panels ---------------------------------------------------------------

    // --- map layers (A5.5) -----------------------------------------------------

    reg.register("layerData", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let raw = ctx
                    .body
                    .get("layer")
                    .cloned()
                    .ok_or_else(|| Error::invalid("`layer` is required"))?;
                let layer = parse_layer(raw)?;
                let access = Access::of(&ctx);
                access.check_datasets(&catalog, [layer.dataset]).await?;
                let data =
                    layer::layer_data(&catalog, &access.caller, &layer, layer::Limits::default())
                        .await?;
                let mut out = serde_json::to_value(&data)
                    .map_err(|e| Error::serde(format!("a layer's data does not serialise: {e}")))?;
                if matches!(data, layer::LayerData::Tiles { .. })
                    && let Some(fields) = out.as_object_mut()
                {
                    fields.insert("tiles".into(), Json::String(tile_template(&layer)?));
                }
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.register("layerTile", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let coordinate = |name: &str| -> Result<u32> {
                    let raw = ctx.path_param(name)?;
                    raw.parse::<u32>().map_err(|_| {
                        Error::invalid(format!("the tile's `{name}` is {raw}, not a tile number"))
                    })
                };
                let (z, x, y) = (coordinate("z")?, coordinate("x")?, coordinate("y")?);
                let raw = ctx
                    .query_get("layer")
                    .ok_or_else(|| Error::invalid("`layer` is required"))?;
                let layer = parse_layer(
                    serde_json::from_str(raw)
                        .map_err(|e| Error::invalid(format!("`layer` is not JSON: {e}")))?,
                )?;
                let access = Access::of(&ctx);
                access.check_datasets(&catalog, [layer.dataset]).await?;
                let bytes = layer::layer_tile(&catalog, &access.caller, &layer, z, x, y).await?;
                Ok(HandlerResponse::download(crate::handler::Download {
                    body: crate::handler::DownloadBody::Bytes(bytes.into()),
                    content_type: "application/vnd.mapbox-vector-tile".to_owned(),
                    // Fetched by the map, not saved by a person.
                    filename: String::new(),
                }))
            }
        }
    });

    // --- the Map workspace's table and selection (A5.10) ------------------------

    reg.register("layerRows", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let layer = parse_layer(required(&ctx.body, "layer")?)?;
                let access = Access::of(&ctx);
                access.check_datasets(&catalog, [layer.dataset]).await?;
                let sort: Option<sc_dataset::SortKey> = ctx
                    .body
                    .get("sort")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| Error::invalid(format!("`sort` is not an order: {e}")))?;
                let limit =
                    optional_usize(&ctx.body, "limit").map_or(selection::TABLE_ROWS, |l| l as u64);
                let answer = match selection::layer_rows(
                    &catalog,
                    &access.caller,
                    &layer,
                    sort.as_ref(),
                    limit,
                )
                .await?
                {
                    Ok(rows) => serde_json::to_value(rows).map_err(|e| {
                        Error::serde(format!("a layer's rows do not serialise: {e}"))
                    })?,
                    Err(error) => json!({ "error": error }),
                };
                Ok(HandlerResponse::ok(answer))
            }
        }
    });

    reg.register("selectFeatures", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let layer = parse_layer(required(&ctx.body, "layer")?)?;
                let access = Access::of(&ctx);
                access.check_datasets(&catalog, [layer.dataset]).await?;
                let by: selection::SelectBy = serde_json::from_value(required(&ctx.body, "by")?)
                    .map_err(|e| {
                        Error::invalid(format!("`by` is not a way to select features: {e}"))
                    })?;
                let answer = match selection::select_features(&catalog, &access.caller, &layer, &by)
                    .await?
                {
                    Ok(found) => serde_json::to_value(found).map_err(|e| {
                        Error::serde(format!("a selection does not serialise: {e}"))
                    })?,
                    Err(error) => json!({ "error": error }),
                };
                Ok(HandlerResponse::ok(answer))
            }
        }
    });

    reg.register("saveSelection", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let layer = parse_layer(required(&ctx.body, "layer")?)?;
                let access = Access::of(&ctx);
                access.check_editor()?;
                access.check_datasets(&catalog, [layer.dataset]).await?;
                let name = text(&ctx.body, "name")?;
                let ids: Vec<Json> = match ctx.body.get("ids") {
                    Some(Json::Array(ids)) => ids.clone(),
                    _ => Vec::new(),
                };
                let condition = ctx.body.get("condition").and_then(Json::as_str);
                let def = selection::save_selection(
                    &catalog,
                    &access.caller,
                    &layer,
                    &name,
                    &ids,
                    condition,
                )
                .await?;
                owned_by_caller(&catalog, &access, def.id).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &access, &def).await?).with_status(201))
            }
        }
    });

    // --- maps (A5.6, A5.7, A5.11, A5.12) ------------------------------------------

    reg.register("mapSettings", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let settings = sc_config::map_settings(&catalog).await?;
                Ok(HandlerResponse::ok(map_settings_json(&settings)))
            }
        }
    });

    reg.register("allowMapHost", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let url = text(&ctx.body, "url")?;
                let settings = sc_config::allow_map_host(&catalog, &url).await?;
                Ok(HandlerResponse::ok(map_settings_json(&settings)))
            }
        }
    });

    reg.register("listMapTools", move |_ctx| async move {
        Ok(HandlerResponse::ok(json!(tools::tool_descriptors())))
    });

    reg.register("runMapTool", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let tool = text(&ctx.body, "tool")?;
                let params = match ctx.body.get("params") {
                    Some(Json::Object(params)) => params.clone(),
                    None | Some(Json::Null) => Map::new(),
                    Some(other) => {
                        return Err(Error::invalid(format!(
                            "`params` is the tool's answers, an object; got {other}"
                        )));
                    }
                };
                let name = ctx.body.get("name").and_then(Json::as_str);
                let access = Access::of(&ctx);
                if access.app.is_some() {
                    // A tool makes a dataset from the datasets its answers name.
                    access.check_editor()?;
                    let library = sc_dataset::load_library(&catalog).await?;
                    let named: Vec<DatasetId> = params
                        .values()
                        .filter_map(Json::as_str)
                        .filter_map(|v| v.parse().ok())
                        .filter(|id| library.get(*id).is_some())
                        .collect();
                    access.check_datasets(&catalog, named).await?;
                }
                let run = tools::run_tool(&catalog, &tool, &tools::ToolArgs(params), name).await?;
                owned_by_caller(&catalog, &access, run.dataset.id).await?;
                let mut answer = detail(&catalog, &access, &run.dataset).await?;
                if let Some(fields) = answer.as_object_mut() {
                    fields.insert(
                        "layer".into(),
                        serde_json::to_value(&run.layer).map_err(|e| {
                            Error::serde(format!("a layer does not serialise: {e}"))
                        })?,
                    );
                }
                Ok(HandlerResponse::ok(answer).with_status(201))
            }
        }
    });

    reg.register("suggestMap", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id: DatasetId = ctx
                    .body
                    .get("dataset")
                    .and_then(Json::as_str)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::invalid("`dataset` is required, as a dataset's id"))?;
                let assignment: plot::Assignment = match ctx.body.get("assignment") {
                    None | Some(Json::Null) => plot::Assignment::default(),
                    Some(a) => serde_json::from_value(a.clone()).map_err(|e| {
                        Error::invalid(format!("`assignment` is not a set of drop zones: {e}"))
                    })?,
                };
                let geometry: Option<layer::GeometrySource> = ctx
                    .body
                    .get("geometry")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| {
                        Error::invalid(format!("`geometry` is not a geometry source: {e}"))
                    })?;
                Access::of(&ctx).check_datasets(&catalog, [id]).await?;
                let answer = map::suggest_map(&catalog, id, &assignment, geometry.as_ref()).await?;
                Ok(HandlerResponse::ok(serde_json::to_value(answer).map_err(
                    |e| Error::serde(format!("a suggested map does not serialise: {e}")),
                )?))
            }
        }
    });

    reg.register("renderMap", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let spec: map::MapSpec = serde_json::from_value(
                    ctx.body
                        .get("spec")
                        .cloned()
                        .ok_or_else(|| Error::invalid("`spec` is required"))?,
                )
                .map_err(|e| Error::invalid(format!("`spec` is not a map: {e}")))?;
                if spec.layers.is_empty() {
                    return Err(Error::invalid("a map needs at least one layer"));
                }
                let access = Access::of(&ctx);
                access
                    .check_datasets(&catalog, spec.datasets().into_keys())
                    .await?;
                let rendered = map::render_map(&catalog, &access.caller, &spec).await?;
                Ok(HandlerResponse::ok(rendered_map_json(&rendered)?))
            }
        }
    });

    reg.register("renderPanel", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let panel = Panel::from_json(
                    ctx.body
                        .get("panel")
                        .ok_or_else(|| Error::invalid("`panel` is required"))?,
                )?;
                let filters = match ctx.body.get("filters") {
                    None | Some(Json::Null) => Vec::new(),
                    Some(raw) => sc_analytics::crossfilter::Condition::read_list(raw)?,
                };
                let access = Access::of(&ctx);
                let read: Vec<DatasetId> = panel
                    .datasets()
                    .into_iter()
                    .chain(filters.iter().map(|c| c.dataset))
                    .collect();
                access.check_datasets(&catalog, read).await?;
                let rendered = sc_analytics::panel::render_panel_in(
                    &catalog,
                    &access.caller,
                    &panel,
                    &filters,
                )
                .await?;
                let mut answer = serde_json::to_value(&rendered)
                    .map_err(|e| Error::serde(format!("a panel's data does not serialise: {e}")))?;
                // A map panel's tiled layers are fetched by URL, as
                // `renderMap`'s are.
                if let (Some(map), Some(fields)) = (&rendered.map, answer.as_object_mut()) {
                    fields.insert("map".into(), rendered_map_json(map)?);
                }
                Ok(HandlerResponse::ok(answer))
            }
        }
    });

    // --- the model editor ----------------------------------------------------

    reg.register("getModelOutputs", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let raw = ctx.path_param("id")?;
                let id = sc_model::ModelId(
                    raw.parse()
                        .map_err(|_| Error::invalid(format!("`{raw}` is not a model id")))?,
                );
                let model = sc_model::load_model(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no model with id {id}")))?;
                let instance = match ctx.query_get("fit").filter(|f| !f.is_empty()) {
                    Some(raw) => {
                        let fit =
                            sc_model::InstanceId(raw.parse().map_err(|_| {
                                Error::invalid(format!("`{raw}` is not a fit's id"))
                            })?);
                        let instance = sc_model::require_model_instance(&catalog, fit).await?;
                        if instance.model != id {
                            return Err(Error::invalid(format!(
                                "fit {fit} is not a fit of the model `{}`",
                                model.name
                            )));
                        }
                        Some(instance)
                    }
                    None => shown_fit(&catalog, id).await?,
                };
                let Some(instance) = instance else {
                    return Ok(HandlerResponse::ok(json!({
                        "model": id.0, "fit": null, "outputs": [],
                    })));
                };
                let include: std::collections::BTreeSet<String> = ctx
                    .query_get("include")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                let outputs =
                    sc_analytics::model_outputs::render_outputs(&catalog, &instance, &include)
                        .await?;
                Ok(HandlerResponse::ok(json!({
                    "model": id.0,
                    "fit": {
                        "id": instance.id.0,
                        "name": instance.name,
                        "status": instance.status.as_str(),
                        "created": instance.created,
                        "active": instance.active,
                        "error": instance.error(),
                        "dataset_changed":
                            sc_model::dataset_changed(&instance, &model.dataset, &model.related),
                    },
                    "outputs": outputs,
                })))
            }
        }
    });

    // --- the shell -----------------------------------------------------------

    reg.register("analyticsShell", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let roles: Vec<Json> = sc_auth::list_roles(&catalog)
                    .await?
                    .into_iter()
                    .map(|r| json!({ "role": r.role, "name": r.name }))
                    .collect();
                Ok(HandlerResponse::ok(json!({
                    "application": access.app.as_ref().map(|app| app.shell_json()),
                    "roles": roles,
                })))
            }
        }
    });

    // --- workspaces ----------------------------------------------------------

    reg.register("listWorkspaceKinds", move |ctx| async move {
        let access = Access::of(&ctx);
        Ok(HandlerResponse::ok(Json::Array(
            WorkspaceKind::ALL
                .iter()
                // An application lists the kinds it opens, as available.
                .filter(|k| access.app.as_ref().is_none_or(|app| app.allows_kind(**k)))
                .map(|k| {
                    json!({
                        "kind": k.as_str(),
                        "label": k.label(),
                        "available": k.is_available(),
                        "arrives_in": k.arrives_in(),
                    })
                })
                .collect(),
        )))
    });

    reg.register("listWorkspaces", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let all = sc_analytics::list_workspaces(&catalog).await?;
                Ok(HandlerResponse::ok(Json::Array(
                    all.iter()
                        .filter(|w| access.sees_workspace(w))
                        .map(|w| access.workspace_json(w))
                        .collect(),
                )))
            }
        }
    });

    reg.register("getWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let ws = access.workspace(&catalog, workspace_id(&ctx)?).await?;
                Ok(HandlerResponse::ok(access.workspace_json(&ws)))
            }
        }
    });

    reg.register("createWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = text(&ctx.body, "name")?;
                let kind = WorkspaceKind::parse(&text(&ctx.body, "kind")?)?;
                kind.check_available()?;
                let access = Access::of(&ctx);
                let mut ws = Workspace::new(name, kind, ctx.user.as_ref().map(|u| u.id));
                if let Some(app) = &access.app {
                    app.check_create_workspace(kind)?;
                    ws = ws.in_application(app.app);
                }
                sc_analytics::create_workspace(&catalog, &ws).await?;
                let ws = sc_analytics::require_workspace(&catalog, ws.id).await?;
                Ok(HandlerResponse::ok(access.workspace_json(&ws)).with_status(201))
            }
        }
    });

    reg.register("updateWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = text(&ctx.body, "name")?;
                let access = Access::of(&ctx);
                let ws = access.workspace(&catalog, workspace_id(&ctx)?).await?;
                access.check_change_workspace(&ws)?;
                let ws = sc_analytics::rename_workspace(&catalog, ws.id, &name).await?;
                Ok(HandlerResponse::ok(access.workspace_json(&ws)))
            }
        }
    });

    reg.register("shareWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let ws = access.workspace(&catalog, workspace_id(&ctx)?).await?;
                access.check_change_workspace(&ws)?;
                if access.app.is_none() && ws.application.is_none() {
                    return Err(Error::invalid(
                        "the Analytics UI's own workspaces are the admins'; share a workspace \
                         of an application, or show this one in a fixed application",
                    ));
                }
                let ws =
                    sc_analytics::share_workspace(&catalog, ws.id, share_role(&ctx.body)?).await?;
                Ok(HandlerResponse::ok(access.workspace_json(&ws)))
            }
        }
    });

    reg.register("saveWorkspaceState", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let state = ctx
                    .body
                    .get("state")
                    .cloned()
                    .ok_or_else(|| Error::invalid("`state` is required"))?;
                let access = Access::of(&ctx);
                let ws = access.workspace(&catalog, workspace_id(&ctx)?).await?;
                access.check_change_workspace(&ws)?;
                let ws = sc_analytics::save_workspace_state(&catalog, ws.id, state).await?;
                Ok(HandlerResponse::ok(access.workspace_json(&ws)))
            }
        }
    });

    reg.register("deleteWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let access = Access::of(&ctx);
                let id = workspace_id(&ctx)?;
                let ws = access.workspace(&catalog, id).await?;
                access.check_change_workspace(&ws)?;
                if !sc_analytics::delete_workspace(&catalog, id).await? {
                    return Err(Error::not_found(format!(
                        "there is no workspace with id {id}"
                    )));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });
}

fn uuid_param(ctx: &HandlerCtx, what: &str) -> Result<Uuid> {
    let raw = ctx.path_param("id")?;
    Uuid::parse_str(raw).map_err(|_| Error::invalid(format!("`{raw}` is not a {what} id")))
}

fn dataset_id(ctx: &HandlerCtx) -> Result<DatasetId> {
    uuid_param(ctx, "dataset").map(DatasetId)
}

fn workspace_id(ctx: &HandlerCtx) -> Result<WorkspaceId> {
    uuid_param(ctx, "workspace").map(WorkspaceId)
}

fn text(body: &Json, key: &str) -> Result<String> {
    body.get(key)
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

fn optional_usize(body: &Json, key: &str) -> Option<usize> {
    body.get(key).and_then(Json::as_u64).map(|n| n as usize)
}

/// A definition from `createDataset`'s or `updateDataset`'s body, under `id`.
pub(crate) fn def_from_input(body: &Json, id: DatasetId) -> Result<DatasetDef> {
    let base: Base = serde_json::from_value(
        body.get("base")
            .cloned()
            .ok_or_else(|| Error::invalid("`base` is required"))?,
    )
    .map_err(|e| Error::invalid(format!("`base` is not a table or a dataset: {e}")))?;
    let operations: Vec<Operation> = match body.get("operations") {
        None | Some(Json::Null) => Vec::new(),
        Some(ops) => serde_json::from_value(ops.clone()).map_err(|e| {
            Error::invalid(format!("`operations` is not a list of operations: {e}"))
        })?,
    };
    Ok(DatasetDef {
        id,
        name: text(body, "name")?.trim().to_owned(),
        description: body
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        base,
        operations,
    })
}

/// The `dataset` of a read endpoint's body: a whole definition, stored or not.
fn def_from_body(body: &Json) -> Result<DatasetDef> {
    let value = body
        .get("dataset")
        .cloned()
        .ok_or_else(|| Error::invalid("`dataset` is required"))?;
    // An unsaved definition may come without an id or a name yet.
    let mut value = value;
    if let Json::Object(map) = &mut value {
        map.entry("id").or_insert_with(|| json!(DatasetId::new()));
        map.entry("name").or_insert_with(|| json!("(unsaved)"));
    }
    serde_json::from_value(value)
        .map_err(|e| Error::invalid(format!("`dataset` is not a dataset: {e}")))
}

/// The schema and every stored dataset, with `def` in place of its stored
/// self — what a definition being edited compiles against.
async fn world(catalog: &Catalog, def: &DatasetDef) -> Result<(Schema, Library)> {
    let schema = Schema::of_catalog(catalog)?;
    let mut library = sc_dataset::load_library(catalog).await?;
    library.insert(def.clone());
    Ok((schema, library))
}

/// A definition and its report.
async fn detail(catalog: &Catalog, access: &Access, def: &DatasetDef) -> Result<Json> {
    let (schema, library) = world(catalog, def).await?;
    let compiled = compile(&schema, &library, def, Options::default());
    Ok(json!({ "dataset": def, "report": report(&schema, access, &compiled) }))
}

/// A compiled definition as JSON, with what its formulas may name — in an
/// application, only the tables it reads.
fn report(schema: &Schema, access: &Access, compiled: &Compilation) -> Json {
    let tables: Map<String, Json> = schema
        .tables
        .values()
        .filter(|t| access.names_table(&t.name))
        .map(|t| (t.name.clone(), json!(t.columns)))
        .collect();
    let mut children: BTreeMap<String, Vec<Json>> = BTreeMap::new();
    for name in schema.tables.keys().filter(|t| access.names_table(t)) {
        let incoming: Vec<Json> = schema
            .shape
            .incoming(name)
            .into_iter()
            .filter(|(child, _)| schema.tables.contains_key(*child) && access.names_table(child))
            .map(|(child, key)| json!({ "table": child, "key": key }))
            .collect();
        if !incoming.is_empty() {
            children.insert(name.clone(), incoming);
        }
    }
    json!({
        "base": compiled.base,
        "operations": compiled
            .operations
            .iter()
            .map(|r| json!({
                "id": r.id,
                "kind": r.kind,
                "status": r.status,
                "shape": r.shape,
                "error": r.error,
                "enabled": r.status != OpStatus::Disabled,
            }))
            .collect::<Vec<_>>(),
        "tables": tables,
        "children": children,
    })
}

pub(crate) fn workspace_json(ws: &Workspace) -> Json {
    json!({
        "id": ws.id.0,
        "name": ws.name,
        "kind": ws.kind.as_str(),
        "state": ws.state,
        "created_by": ws.created_by,
        "updated_at": ws.updated_at,
        "share_role": ws.share_role,
        "application": ws.application,
    })
}

/// Make the caller the owner of the dataset `id` they just made (A9.1).
async fn owned_by_caller(catalog: &Catalog, access: &Access, id: DatasetId) -> Result<()> {
    sc_dataset::set_dataset_sharing(catalog, id, Sharing::owned_by(access.caller.user_id())).await
}

/// A sharing body's `share_role`: a role, or null for nobody but the owner.
fn share_role(body: &Json) -> Result<Option<u8>> {
    match body.get("share_role") {
        None | Some(Json::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|r| u8::try_from(r).ok())
            .map(Some)
            .ok_or_else(|| {
                Error::invalid(format!("`share_role` is a role from 1 to 100, not {value}"))
            }),
    }
}

/// The dataset a plot's, a table's or a test's data reads, if it reads one.
fn data_datasets(data: &plot::DataRef) -> Option<DatasetId> {
    match data {
        plot::DataRef::Dataset { dataset } => Some(*dataset),
        plot::DataRef::FitOutput { .. } => None,
    }
}

/// A body's `key`, which must be there.
fn required(body: &Json, key: &str) -> Result<Json> {
    body.get(key)
        .filter(|v| !v.is_null())
        .cloned()
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// `mapSettings`' answer.
fn map_settings_json(settings: &sc_config::MapSettings) -> Json {
    json!({
        "style": settings.style,
        "style_dark": settings.dark_style(),
        "hosts": settings.hosts(),
    })
}

/// A drawn map as `renderMap` answers it: each layer's request, data,
/// domains and classes, a tiled layer with the URL template of its tiles.
fn rendered_map_json(rendered: &map::RenderedMap) -> Result<Json> {
    let mut layers = Vec::with_capacity(rendered.layers.len());
    for one in &rendered.layers {
        let mut data = serde_json::to_value(&one.data)
            .map_err(|e| Error::serde(format!("a layer's data does not serialise: {e}")))?;
        if matches!(one.data, layer::LayerData::Tiles { .. })
            && let Some(fields) = data.as_object_mut()
        {
            fields.insert("tiles".into(), Json::String(tile_template(&one.layer)?));
        }
        let mut entry = json!({
            "layer": one.layer,
            "data": data,
            "domains": one.domains,
        });
        if let (Some(classes), Some(fields)) = (&one.classes, entry.as_object_mut()) {
            fields.insert("classes".into(), json!(classes));
        }
        layers.push(entry);
    }
    Ok(json!({ "layers": layers }))
}

/// A layer as `layerData` and `layerTile` are given it.
fn parse_layer(raw: Json) -> Result<layer::LayerRequest> {
    serde_json::from_value(raw).map_err(|e| Error::invalid(format!("`layer` is not a layer: {e}")))
}

/// The URL template of a layer's vector tiles: `layerTile`'s path with
/// MapLibre's `{z}`, `{x}` and `{y}` in it, and the layer in the query string.
fn tile_template(layer: &layer::LayerRequest) -> Result<String> {
    let json = serde_json::to_string(layer)
        .map_err(|e| Error::serde(format!("a layer does not serialise: {e}")))?;
    Ok(format!(
        "/{}/layers/tiles/{{z}}/{{x}}/{{y}}?layer={}",
        sc_api::ADMIN_API_PREFIX,
        crate::builder::encode_component(&json)
    ))
}
