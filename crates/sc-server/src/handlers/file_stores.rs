//! File stores (their configuration) and the backend operations a store's settings form runs.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    // The **union** of defined and connected stores, which neither alone gets
    // right. Listing only the connected ones (what the MVP did) hides a store
    // whose directory has been unmounted — exactly the store the admin needs to
    // find and repoint. Listing only the definitions hides a store connected by
    // `--file-store`, which has no row but is real and browsable, so a developer
    // running with the flag would see an empty list. Stored stores come first,
    // then any connected store with no definition, which reports a null id to
    // say "there is nothing here to edit".
    reg.handle(sc_api::handler_ids::LIST_FILE_STORES, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let defs = list_file_stores(&catalog).await?;
                let mut out: Vec<Json> = defs
                    .iter()
                    .map(|def| file_store_json(&catalog, def))
                    .collect::<Result<_>>()?;

                for name in catalog.file_store_names()? {
                    if !defs.iter().any(|def| def.name == name) {
                        out.push(ephemeral_file_store_json(&catalog, &name)?);
                    }
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(
        sc_api::handler_ids::CREATE_FILE_STORE,
        typed(state, create_file_store),
    );

    reg.handle(sc_api::handler_ids::UPDATE_FILE_STORE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_file_store_id(ctx.path_param("id")?)?;
                let existing = load_file_store(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no file store with id {id:?}")))?;
                // The id is the path's, not the body's — the row's identity is
                // not something a payload gets to reassign.
                let mut def = file_store_from_body(id, &ctx.body)?;
                // Attributes are server-managed (§9) — a git store's recorded
                // clone directory lives there — and the edit form neither shows
                // nor sends them, so they are carried across rather than reset.
                // Dropping them would orphan a working tree on the next save.
                def.attributes = existing.attributes.clone();
                // A secret the form sent back untouched is the sentinel it was
                // shown, not the key: restore what is stored (§11.1). A
                // create-only setting is restored whatever was sent — the store
                // exists, so that setting is no longer the admin's to change.
                def.config = unredacted_config(&def.backend, &existing.config, &def.config);
                save_file_store(&catalog, &def).await?;

                // A rename leaves the old handle connected under the old name,
                // still serving, because saving deliberately does not touch the
                // registry (§1.1). Disconnect it here — this is the one place
                // that knows both the before and the after.
                if existing.name != def.name {
                    catalog.disconnect_file_store(&existing.name)?;
                }
                // Reconnect under the current name so an edited path takes
                // effect immediately, with no restart (§1.3).
                recreate_backend_resources(&catalog, &mut def).await?;
                Ok(HandlerResponse::ok(file_store_json(&catalog, &def)?))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_FILE_STORE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_file_store_id(ctx.path_param("id")?)?;
                let existing = load_file_store(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no file store with id {id:?}")))?;

                // Collect the references `sc-catalog` cannot see. Applications
                // live a crate above it, so this is the one place that can put
                // both halves of the check together — without it a store still
                // serving an app's source could be deleted out from under it.
                let app_refs = applications_using_file_store(&catalog, &existing.name).await?;
                let deleted = delete_file_store(&catalog, id, &app_refs).await?;

                // Removing the row is the definition's end; disconnecting is what
                // stops it serving. Both are needed — the file manager would
                // otherwise go on browsing a store the admin had just deleted.
                if deleted {
                    catalog.disconnect_file_store(&existing.name)?;
                }
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_FILE_STORE_BACKENDS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let mut out = Vec::new();
                for name in registered_backends() {
                    let spec = resolve_options(&catalog, backend_config_spec(&name)?).await?;
                    out.push(json!({
                        "name": name,
                        "config_spec": spec_json(&spec, &ctx.locale),
                        // What the backend can *do*, declared the same way as
                        // what it can be told — so the form renders a button per
                        // operation without knowing what any of them mean.
                        "operations": backend_operations(&name)?
                            .iter()
                            .map(|op| operation_json(op, &ctx.locale))
                            .collect::<Vec<_>>(),
                    }));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    // Both handlers are backend-agnostic: they look up what the backend declared
    // and run it. Nothing here knows what a repository, a clone or a deploy key
    // is — which is what lets a backend added later, in Rust or through
    // `sc-code`, have buttons in the admin UI without either of these changing.

    // Configure scope: against configuration the admin has not saved yet, so the
    // definition is built from the body and thrown away, and what comes back is
    // its config for the form to adopt.
    reg.handle(
        sc_api::handler_ids::RUN_BACKEND_OPERATION,
        typed(state, backend_operation),
    );

    // Instance scope: against a saved store. Whatever the operation changed in
    // the definition is persisted, and the store is reconnected — an operation
    // may be exactly what makes it connectable (a clone), and an admin who has
    // just repaired a store should not have to save again to use it.
    reg.handle(sc_api::handler_ids::RUN_FILE_STORE_OPERATION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_file_store_id(ctx.path_param("id")?)?;
                let operation = ctx.path_param("operation")?.to_owned();
                let mut def = load_file_store(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no file store with id {id:?}")))?;
                require_scope(&def.backend, &operation, OperationScope::Instance)?;
                let input = match &ctx.body {
                    Json::Object(obj) => object_field(obj, "input")?,
                    _ => sc_types::Attrs::new(),
                };

                let before = def.clone();
                let outcome = run_backend_operation(&mut def, &operation, &input).await?;
                // Only when it actually changed something: an operation that
                // merely reports (a status) must not rewrite the row on every
                // screen open.
                if def != before {
                    save_file_store(&catalog, &def).await?;
                }
                // Failure stays a non-error, exactly as it is on save (§1.2):
                // pulling a store whose disk has gone should report the pull's
                // outcome, not swallow it behind a connection error.
                let _ = connect_file_store_def(&catalog, &def);
                Ok(HandlerResponse::ok(json!({
                    "config": Json::Object(def.config),
                    "output": outcome.output,
                    // Optional and backend-shaped (§14.1): the git backend fills
                    // it with the working copy's state so the IDE's source
                    // control view can draw itself; every other backend leaves it
                    // null and every other client ignores it.
                    "data": outcome.data,
                    "connected": catalog.has_file_store(&def.name)?,
                })))
            }
        }
    });
}

/// The reverse, on the way in: a submitted config whose sentinels have been
/// replaced by what is stored, so an admin editing a store's *name* does not
/// save the mask over its key — and whose
/// [`create_only`](FormField::create_only) settings are put back to what is
/// stored, because the store already exists.
///
/// Both are the same move and belong together: the spec says how a value
/// submitted for a field is to be read against the one already there. The
/// create-only half is the load-bearing one for a git store's working-copy
/// directory — the form renders it read-only, but a request need not come from
/// the form, and a save that repointed it would abandon the working tree rather
/// than move it.
///
/// **Only reached on an edit.** A create has nothing stored, so there is nothing
/// to preserve and a create-only setting is settable exactly once, which is what
/// the flag means.
fn unredacted_config(
    backend: &str,
    stored: &sc_types::Attrs,
    submitted: &sc_types::Attrs,
) -> sc_types::Attrs {
    match backend_config_spec(backend) {
        Ok(spec) => sc_types::preserve_create_only(
            &spec,
            stored,
            &sc_types::merge_secrets(&spec, stored, submitted),
        ),
        Err(_) => submitted.clone(),
    }
}

/// The same operations, on an **edit** of a store that already exists — where a
/// failure is reported rather than fatal.
///
/// The asymmetry with [`create_backend_resources`] is deliberate and is §1.2's
/// rule: an existing store must stay saved and editable even when it cannot be
/// brought up, because editing it is the repair. Refusing the save would trap an
/// admin whose remote is briefly unreachable with a definition they can no
/// longer correct.
///
/// It runs on every save rather than only when something changed, because it is
/// idempotent (a clone that has already happened is a no-op) and because this is
/// how repointing a store at a reachable URL brings it back up.
async fn recreate_backend_resources(catalog: &Catalog, def: &mut FileStoreDef) -> Result<()> {
    for op in backend_operations(&def.backend)? {
        if !op.on_create {
            continue;
        }
        match run_backend_operation(def, &op.name, &sc_types::Attrs::new()).await {
            // The operation may have recorded something in the definition — a
            // git clone records where it cloned to — which has to reach the row,
            // or a later rename would abandon the working copy.
            Ok(_) => save_file_store(catalog, def).await?,
            // Record this failure rather than letting the connect that follows
            // overwrite it with "this has not been cloned yet" — which is true
            // but says nothing about *why*, and the why is the admin's next move
            // (a wrong URL, a deploy key not yet installed at the remote).
            Err(e) => return report_unusable(catalog, def, &e),
        }
    }
    if let Err(e) = connect_file_store_def(catalog, def) {
        return report_unusable(catalog, def, &e);
    }
    Ok(())
}

/// An edited store that cannot be brought up: stop it serving, and record why.
///
/// The **disconnect** is the part that is easy to miss. A handle in the registry
/// was built from the *previous* definition, so leaving it there after a failed
/// save means the store goes on serving the old URL, the old directory — while
/// the admin is looking at a form that says something else. The store is
/// reported as connected and works, which is the most confusing possible answer
/// to "did my edit take effect?".
///
/// Disconnecting clears any recorded error, so the reason is recorded after it.
fn report_unusable(catalog: &Catalog, def: &FileStoreDef, error: &Error) -> Result<()> {
    catalog.disconnect_file_store(&def.name)?;
    catalog.record_file_store_error(&def.name, sc_error::format_causes(error))
}

/// Check that `operation` is one the backend declares **and** that it is being
/// run at the right scope.
///
/// The scope check is not pedantry: a `Configure`-scope operation runs against
/// an unsaved definition built from a request body, so allowing one through the
/// instance endpoint (or the reverse) would run it against a definition it was
/// never written for. Refusing by name is also the clearer error — "the `local`
/// backend has no operation `pull`" beats a failure from inside git.
fn require_scope(backend: &str, operation: &str, scope: OperationScope) -> Result<()> {
    let declared = backend_operations(backend)?;
    let found = declared
        .iter()
        .find(|op| op.name == operation)
        .ok_or_else(|| {
            Error::invalid(format!(
                "the `{backend}` backend has no operation `{operation}`"
            ))
        })?;
    if found.scope != scope {
        return Err(Error::invalid(format!(
            "operation `{operation}` of the `{backend}` backend cannot be run here"
        )));
    }
    Ok(())
}

/// A connected store that has **no** stored definition — one supplied by
/// `--file-store` (§1.3), which is deliberately ephemeral and unpersisted.
///
/// Reported with a null id, which is what tells the admin UI there is nothing to
/// edit or delete: the store is real and browsable, but its existence ends with
/// the process unless the flag is passed again. It is listed rather than hidden
/// because a developer running with the flag would otherwise see an empty
/// file-stores screen and have no way to reach the store they just connected.
fn ephemeral_file_store_json(catalog: &Catalog, name: &str) -> Result<Json> {
    let git = catalog
        .file_store_is_git_repo(name)?
        .ok_or_else(|| Error::not_found(format!("file store `{name}` is not connected")))?;
    Ok(json!({
        "id": Json::Null,
        "name": name,
        "description": "",
        // Named rather than left blank: the flag only ever makes local stores,
        // and the UI shows this next to the stored ones.
        "backend": sc_files::LOCAL_BACKEND,
        "config": json!({}),
        "min_role": Json::Null,
        // It is in the registry, so by construction it is connected.
        "connected": true,
        "error": Json::Null,
        "is_git_repo": Some(git),
    }))
}

fn parse_file_store_id(raw: &str) -> Result<FileStoreDefId> {
    uuid::Uuid::parse_str(raw)
        .map(FileStoreDefId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid file store id")))
}

/// An [`Operation`] as the API returns it (matching `operation_schema`): what a
/// backend can be asked to *do*, in the same declared-as-data shape its settings
/// use.
fn operation_json(op: &Operation, locale: &Locale) -> Json {
    json!({
        "name": op.name,
        "label": say(&op.label, locale),
        "description": say(&op.description, locale),
        "scope": match op.scope {
            OperationScope::Configure => "configure",
            OperationScope::Instance => "instance",
        },
        "input_spec": spec_json(&op.input_spec, locale),
        "on_create": op.on_create,
        "automatic": op.automatic,
    })
}

/// The `createFileStore` endpoint.
pub(crate) async fn create_file_store(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let catalog = state.catalog;
    // A create mints a fresh id; the body carries everything else.
    let mut def = file_store_from_body(FileStoreDefId::new(), &body)?;

    // **Creating a store is transactional.** The cheap checks run
    // first, then whatever the backend has to build, and only then
    // is anything written — so a git store whose clone fails leaves
    // no row at all. Saving first and reporting the failure (what
    // an *edit* does, §1.2) is wrong here for a concrete reason:
    // the admin corrects the URL, presses Create again, and is told
    // the name is already taken — by the row their failed attempt
    // left behind. There is nothing to preserve on a create, since
    // the form still holds everything they typed.
    check_file_store_saveable(&catalog, &def).await?;
    create_backend_resources(&mut def).await?;

    save_file_store(&catalog, &def).await?;
    // Connect it straight away so the admin finds out now whether
    // the store is actually reachable, rather than on next boot. A
    // failure here is deliberately *not* an error — the store was
    // created, and the reason is reported in the response so the UI
    // can show "saved, but not connected: <why>".
    let _ = connect_file_store_def(&catalog, &def);
    Ok(HandlerResponse::ok(file_store_json(&catalog, &def)?).with_status(201))
}

/// The `runBackendOperation` endpoint.
pub(crate) async fn backend_operation(
    _state: HandlerState,
    ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let backend = ctx.path_param("backend")?.to_owned();
    let operation = ctx.path_param("operation")?.to_owned();
    let obj = require_object(&body)?;
    let name = obj.get("name").and_then(Json::as_str).unwrap_or("");
    let mut def = FileStoreDef {
        config: object_field(obj, "config")?,
        ..FileStoreDef::new(name, &backend)
    };
    require_scope(&backend, &operation, OperationScope::Configure)?;

    let outcome = run_backend_operation(&mut def, &operation, &object_field(obj, "input")?).await?;
    Ok(HandlerResponse::ok(json!({
        "config": Json::Object(def.config),
        "output": outcome.output,
        "data": outcome.data,
    })))
}
