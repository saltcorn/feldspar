//! The Analytics UI's handlers (analytics TODO A1.13, A2.6, A2.8, A2.14, A4.2): datasets,
//! plots, hypothesis tests, panels and workspaces, over `sc-dataset` and `sc-analytics`. The endpoints are
//! declared in `sc-api`'s `analytics.rs`, which says what each one is for.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_analytics::panel::Panel;
use sc_analytics::plot::{self, PlotSpec};
use sc_analytics::stats;
use sc_analytics::{Workspace, WorkspaceId, WorkspaceKind};
use sc_catalog::Catalog;
use sc_dataset::{
    Base, Compilation, DatasetDef, DatasetId, Library, OpStatus, Operation, Options, Page, Schema,
    compile,
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

/// Register the Analytics UI's handlers on `reg`.
pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>) {
    // --- datasets ------------------------------------------------------------

    reg.register("listDatasets", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let schema = Schema::of_catalog(&catalog)?;
                let library = sc_dataset::load_library(&catalog).await?;
                let out: Vec<Json> = library
                    .defs()
                    .map(|def| {
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
                let def = sc_dataset::require_dataset(&catalog, dataset_id(&ctx)?).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &def).await?))
            }
        }
    });

    reg.register("createDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let def = def_from_input(&ctx.body, DatasetId::new())?;
                sc_dataset::save_dataset(&catalog, &def).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &def).await?).with_status(201))
            }
        }
    });

    reg.register("updateDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = dataset_id(&ctx)?;
                sc_dataset::require_dataset(&catalog, id).await?;
                let def = def_from_input(&ctx.body, id)?;
                sc_dataset::save_dataset(&catalog, &def).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &def).await?))
            }
        }
    });

    reg.register("deleteDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = dataset_id(&ctx)?;
                if !sc_dataset::delete_dataset(&catalog, id).await? {
                    return Err(Error::not_found(format!(
                        "there is no dataset with id {id}"
                    )));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.register("cloneDataset", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.body.get("name").and_then(Json::as_str);
                let copy = sc_dataset::clone_dataset(&catalog, dataset_id(&ctx)?, name).await?;
                Ok(HandlerResponse::ok(detail(&catalog, &copy).await?).with_status(201))
            }
        }
    });

    reg.register("datasetUsage", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = dataset_id(&ctx)?;
                let datasets: Vec<Json> = sc_dataset::datasets_using(&catalog, id)
                    .await?
                    .iter()
                    .map(|d| json!({ "id": d.id, "name": d.name }))
                    .collect();
                let models: Vec<Json> = sc_model::list_models(&catalog)
                    .await?
                    .iter()
                    .filter(|m| m.dataset.id == id || m.related.iter().any(|r| r.dataset.id == id))
                    .map(|m| json!({ "id": m.id.0, "name": m.name }))
                    .collect();
                let workspaces = sc_analytics::panel::UsageIndex::build(&catalog).await?;
                Ok(HandlerResponse::ok(json!({
                    "datasets": datasets,
                    "models": models,
                    "workspaces": workspaces.dataset(id),
                })))
            }
        }
    });

    reg.register("datasetShapes", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let def = def_from_body(&ctx.body)?;
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                Ok(HandlerResponse::ok(report(&schema, &compiled)))
            }
        }
    });

    reg.register("validateDatasetOperation", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let mut def = def_from_body(&ctx.body)?;
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
                let def = def_from_body(&ctx.body)?;
                let upto = optional_usize(&ctx.body, "upto");
                let offset = optional_usize(&ctx.body, "offset").unwrap_or(0) as u64;
                let limit = optional_usize(&ctx.body, "limit").map_or(DEFAULT_PAGE, |l| l as u64);
                let (schema, library) = world(&catalog, &def).await?;
                let compiled = compile(&schema, &library, &def, Options::default());
                let stage = compiled
                    .stage(upto.unwrap_or(def.operations.len()))
                    .map_err(Error::invalid)?;
                let page = sc_dataset::read_page(&catalog, stage, Page { offset, limit }).await?;
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
                let def = def_from_body(&ctx.body)?;
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
                let values = sc_dataset::column_values(&catalog, stage, &column, limit).await?;
                Ok(HandlerResponse::ok(Json::Array(
                    values.iter().map(sc_dataset::value_json).collect(),
                )))
            }
        }
    });

    reg.register("listDatasetTables", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let schema = Schema::of_catalog(&catalog)?;
                Ok(HandlerResponse::ok(Json::Array(
                    schema
                        .tables
                        .values()
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
                let rendered = plot::render_plot(&catalog, &spec).await?;
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
                let rendered = plot::render_table(&catalog, &spec).await?;
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
                let answer = stats::run_tests(&catalog, &spec).await?;
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
                let def = sc_dataset::require_dataset(&catalog, id).await?;
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
                let rendered = sc_analytics::panel::render_panel(&catalog, &panel).await?;
                Ok(HandlerResponse::ok(
                    serde_json::to_value(rendered).map_err(|e| {
                        Error::serde(format!("a panel's data does not serialise: {e}"))
                    })?,
                ))
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

    // --- workspaces ----------------------------------------------------------

    reg.register("listWorkspaceKinds", move |_ctx| async move {
        Ok(HandlerResponse::ok(Json::Array(
            WorkspaceKind::ALL
                .iter()
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
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let all = sc_analytics::list_workspaces(&catalog).await?;
                Ok(HandlerResponse::ok(Json::Array(
                    all.iter().map(workspace_json).collect(),
                )))
            }
        }
    });

    reg.register("getWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let ws = sc_analytics::require_workspace(&catalog, workspace_id(&ctx)?).await?;
                Ok(HandlerResponse::ok(workspace_json(&ws)))
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
                let ws = Workspace::new(name, kind, ctx.user.as_ref().map(|u| u.id));
                sc_analytics::create_workspace(&catalog, &ws).await?;
                let ws = sc_analytics::require_workspace(&catalog, ws.id).await?;
                Ok(HandlerResponse::ok(workspace_json(&ws)).with_status(201))
            }
        }
    });

    reg.register("updateWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = text(&ctx.body, "name")?;
                let ws =
                    sc_analytics::rename_workspace(&catalog, workspace_id(&ctx)?, &name).await?;
                Ok(HandlerResponse::ok(workspace_json(&ws)))
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
                let ws = sc_analytics::save_workspace_state(&catalog, workspace_id(&ctx)?, state)
                    .await?;
                Ok(HandlerResponse::ok(workspace_json(&ws)))
            }
        }
    });

    reg.register("deleteWorkspace", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = workspace_id(&ctx)?;
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
async fn detail(catalog: &Catalog, def: &DatasetDef) -> Result<Json> {
    let (schema, library) = world(catalog, def).await?;
    let compiled = compile(&schema, &library, def, Options::default());
    Ok(json!({ "dataset": def, "report": report(&schema, &compiled) }))
}

/// A compiled definition as JSON, with what its formulas may name.
fn report(schema: &Schema, compiled: &Compilation) -> Json {
    let tables: Map<String, Json> = schema
        .tables
        .values()
        .map(|t| (t.name.clone(), json!(t.columns)))
        .collect();
    let mut children: BTreeMap<String, Vec<Json>> = BTreeMap::new();
    for name in schema.tables.keys() {
        let incoming: Vec<Json> = schema
            .shape
            .incoming(name)
            .into_iter()
            .filter(|(child, _)| schema.tables.contains_key(*child))
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
    })
}
