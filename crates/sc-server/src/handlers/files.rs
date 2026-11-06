//! The file manager: browsing, searching, reading and writing files in a store.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    reg.handle(sc_api::handler_ids::BROWSE_FILES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                // `dir` is optional-ish: an absent/empty value lists the root.
                let dir = obj.get("dir").and_then(Json::as_str).unwrap_or("");
                // Owner-aware (§2.1): a caller who may not read `dir` still sees
                // the entries they own in it, and one who owns nothing there is
                // refused as before. `list_visible` keeps the metadata the filter
                // read, which is what the owner and access columns are drawn
                // from — the alternative is a second `get_meta` per row, or a
                // request per row from the browser.
                let visible = files.list(dir).await?;
                let out = listing_json(&catalog, &visible).await?;
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::FIND_FILES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let dir = obj.get("dir").and_then(Json::as_str).unwrap_or("");
                // The directory a search starts in is reached like any other, so
                // one rooted where the caller may not go is refused rather than
                // quietly answering nothing.
                files.check_read(dir).await?;
                let query = obj.non_empty_str_field("query")?.to_owned();
                let max_results = match obj.get("max_results").and_then(Json::as_i64) {
                    Some(n) if n >= 1 => usize::try_from(n).unwrap_or(usize::MAX),
                    _ => sc_files::DEFAULT_MAX_FOUND,
                };
                // The columns beside each hit's name are the same ones a listing
                // shows, read the same way.
                let (visible, truncated) = files.find_files(dir, &query, max_results).await?;
                let entries = listing_json(&catalog, &visible).await?;
                Ok(HandlerResponse::ok(json!({
                    "entries": entries,
                    "truncated": truncated,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::LIST_STORE_FILES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                files.check_read("").await?;
                let extensions: Vec<String> = ctx
                    .query_get("extensions")
                    .unwrap_or("")
                    .split(',')
                    .map(|e| e.trim().to_owned())
                    .filter(|e| !e.is_empty())
                    .collect();
                if extensions.is_empty() {
                    return Err(Error::invalid(
                        "say which files to list: `extensions`, e.g. png,jpg",
                    ));
                }
                let found = files.find_by_extension("", &extensions).await?;
                Ok(HandlerResponse::ok(json!({
                    "paths": found.entries.into_iter().map(|e| e.path).collect::<Vec<_>>(),
                    "truncated": found.truncated,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SEARCH_FILES, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let dir = obj.get("dir").and_then(Json::as_str).unwrap_or("");
                // The directory the search starts in is reached like any other,
                // so a search rooted somewhere the caller may not go is refused
                // rather than quietly returning nothing.
                files.check_read(dir).await?;

                let glob = obj
                    .get("glob")
                    .and_then(Json::as_str)
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(str::to_owned);
                let flag = |key: &str| obj.get(key).and_then(Json::as_bool).unwrap_or(false);
                let max_results = match obj.get("max_results").and_then(Json::as_i64) {
                    Some(n) if n >= 1 => usize::try_from(n).unwrap_or(usize::MAX),
                    _ => sc_files::DEFAULT_MAX_RESULTS,
                };
                let query = sc_files::SearchQuery {
                    pattern: obj.non_empty_str_field("pattern")?.to_owned(),
                    regex: flag("regex"),
                    case_sensitive: flag("case_sensitive"),
                    whole_word: flag("whole_word"),
                    glob,
                    dir: dir.to_owned(),
                    max_results,
                    ..sc_files::SearchQuery::literal("")
                };
                // The caller's own role, so the walk skips what a listing would
                // have hidden: a search must not report a line out of a file the
                // caller could not open.
                let found = files.search(&query).await?;
                let matches: Vec<Json> = found
                    .hits
                    .iter()
                    .map(|hit| {
                        json!({
                            "path": hit.path,
                            "line": hit.line,
                            "column": hit.column,
                            "length": hit.length,
                            "text": hit.text,
                        })
                    })
                    .collect();
                Ok(HandlerResponse::ok(json!({
                    "matches": matches,
                    "files_searched": found.files_scanned,
                    "truncated": found.truncated,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::READ_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let path = obj.non_empty_str_field("path")?;
                let bytes = files.read(path).await?;
                // Always provide base64 (for binary download); add a decoded
                // `text` when the bytes are valid UTF-8 (for the text editor).
                let text = std::str::from_utf8(&bytes)
                    .ok()
                    .map(|s| Json::String(s.to_owned()))
                    .unwrap_or(Json::Null);
                Ok(HandlerResponse::ok(json!({
                    "path": path,
                    "size": bytes.len(),
                    "base64": BASE64.encode(&bytes),
                    "text": text,
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::WRITE_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let path = obj.non_empty_str_field("path")?.to_owned();
                let file = files.writable(&path).await?;
                let data = file_body_bytes(obj)?;
                let size = data.len();
                file.write(data).await?;
                file.claim().await;
                Ok(HandlerResponse::ok(file_entry_written_json(&path, size)).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::MAKE_DIRECTORY, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let path = obj.non_empty_str_field("path")?.to_owned();
                let dir = files.writable(&path).await?;
                dir.mkdir().await?;
                dir.claim().await;
                Ok(HandlerResponse::ok(directory_entry_json(&path)).with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let path = obj.non_empty_str_field("path")?.to_owned();
                let deleted = files.delete(&path).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::RENAME_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let from = obj.non_empty_str_field("from")?.to_owned();
                let to = obj.non_empty_str_field("to")?.to_owned();
                // Both ends are checked: reading from a restricted place and
                // writing into one are each things the rule governs, and a move
                // does both.
                files.rename(&from, &to).await?;
                Ok(HandlerResponse::ok(entry_json(&renamed_entry(&to))))
            }
        }
    });

    // Served by the `/upload/{store}/{*path}` route rather than a typed endpoint
    // (the endpoint model has no bytes shape), but registered here like any
    // other handler so it reaches the same catalog and the same access rule.
    // Routing around the `EndpointSet` must not mean routing around those.
    reg.handle(sc_api::handler_ids::UPLOAD_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let path = ctx.path_param("path")?.to_owned();
                let file = files.writable(&path).await?;
                let data = ctx.raw_body()?.clone();
                let size = data.len();
                file.write(data).await?;
                file.claim().await;
                Ok(HandlerResponse::ok(file_entry_written_json(&path, size)).with_status(201))
            }
        }
    });

    // The bytes of one file, as a download (the `/download/{store}/{*path}`
    // route): what the file manager's Download fetches, however large.
    reg.handle(sc_api::handler_ids::DOWNLOAD_FILE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let path = ctx.path_param("path")?.to_owned();
                // Checked by the store the caller is bound to; streamed by the
                // shared builder, with Range, and saved under the file's own
                // name (`attributes.filename`, else the basename).
                Ok(HandlerResponse::serve_file(
                    files.serve(&path).await?.download(true),
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_FILE_META, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let path = obj.non_empty_str_field("path")?.to_owned();
                let meta = files.get_meta(&path).await?;
                let effective = files.effective_min_role(&path).await?;
                Ok(HandlerResponse::ok(file_meta_json(&path, &meta, effective)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::SET_FILE_META, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let name = ctx.path_param("store")?.to_owned();
                let files = resolve_store(&catalog, &name, &ctx).await?;
                let obj = require_object(&ctx.body)?;
                let path = obj.non_empty_str_field("path")?.to_owned();
                let file = files.writable(&path).await?;

                let min_role = optional_role(obj, "min_role")?;
                let attributes = match obj.get("attributes") {
                    Some(Json::Object(o)) => o
                        .iter()
                        .map(|(k, v)| {
                            let text = v.as_str().map(str::to_owned).ok_or_else(|| {
                                Error::invalid(format!("attribute `{k}` must be a string"))
                            })?;
                            Ok((k.clone(), text))
                        })
                        .collect::<Result<BTreeMap<String, String>>>()?,
                    None | Some(Json::Null) => BTreeMap::new(),
                    Some(_) => return Err(Error::invalid("field `attributes` must be an object")),
                };

                let meta = FileMeta {
                    min_role,
                    // Not something this dialog edits, and not something it may
                    // silently drop: the owner is who created the entry, and
                    // rewriting the access rule does not change that.
                    owner: file.get_meta().await.unwrap_or_default().owner,
                    attributes,
                };
                file.set_meta(&meta).await?;
                // Recomputed after the write, so the response reflects what now
                // applies rather than what was asked for — they differ whenever a
                // parent directory is more restrictive than the rule just set.
                let effective = files.effective_min_role(&path).await?;
                Ok(HandlerResponse::ok(file_meta_json(&path, &meta, effective)))
            }
        }
    });
}

/// Resolve a store by name to its live handle **and** its store-wide `min_role`
/// floor (§1.1), which is the outermost entry on every path in it.
///
/// The floor comes from the stored definition, so a store connected by
/// `--file-store` — which has no definition — has no floor. That is correct
/// rather than a gap: an ephemeral developer store has no configured policy, and
/// inventing a restrictive default would break the workflow the flag exists for.
///
/// Answered as a [`CheckedStore`] with the request's caller bound
/// every operation a file endpoint makes is checked
/// by the store it holds, rather than by a call it has to remember to make.
async fn resolve_store(
    catalog: &Catalog,
    name: &str,
    ctx: &crate::handler::HandlerCtx<impl crate::handler::Who>,
) -> Result<CheckedStore> {
    // The catalog reads the floor whatever the caller's role — it is reported
    // (a file's effective rule), not only checked — and for a caller below the admin a failure to read it
    // is an error rather than "no floor" (TODO-type-hardening H3.2).
    //
    // The caller is **admin in practice**, because every file endpoint is
    // `AuthRequirement::admin()` and an admin clears every rule. The check is
    // wired in regardless: it is correct by construction, and starts doing
    // real work the moment a non-admin role can reach a store.
    let caller = sc_access::principal_of(ctx.user());
    catalog.checked_file_store(name, &caller).await
}

/// The entry JSON returned after creating a directory.
fn directory_entry_json(path: &str) -> Json {
    json!({
        "name": path.trim_matches('/').rsplit('/').next().unwrap_or(path),
        "path": path.trim_matches('/'),
        "is_dir": true,
        "size": Json::Null,
    })
}

/// The [`Entry`] describing a rename's destination, so the response says where
/// the thing now is.
fn renamed_entry(to: &str) -> Entry {
    let path = to.trim_matches('/').to_owned();
    Entry {
        name: path.rsplit('/').next().unwrap_or(&path).to_owned(),
        path,
        is_dir: false,
        size: None,
        modified: None,
    }
}

/// A [`FileStore`](sc_files::FileStore) directory listing entry as JSON, matching
/// the API's `file_entry_schema` (`name`, `path`, `is_dir`, `size?`).
fn entry_json(entry: &Entry) -> Json {
    json!({
        "name": entry.name,
        "path": entry.path,
        "is_dir": entry.is_dir,
        "size": entry.size,
    })
}

/// A listing's entries as the file manager reads them: the entry, plus the
/// three columns beside the name — when it changed, who created it, and what
/// rule reaches it.
///
/// The owner is turned into a **label** here rather than in the browser. What is
/// stored is the user's id (an account can be renamed without its files
/// changing hands), and a column of UUIDs tells a person nothing; the id is kept
/// as the label only when the account behind it is gone, which is the honest
/// answer to "who owns this" for a deleted user. Lookups are cached across the
/// listing, so a directory of two hundred files one person uploaded is one
/// query, not two hundred.
async fn listing_json(catalog: &Catalog, visible: &[VisibleEntry]) -> Result<Vec<Json>> {
    let mut labels: HashMap<String, Json> = HashMap::new();
    let mut out = Vec::with_capacity(visible.len());
    for item in visible {
        let owner = match &item.meta.owner {
            None => Json::Null,
            Some(id) => match labels.get(id) {
                Some(label) => label.clone(),
                None => {
                    let label = owner_label(catalog, id).await;
                    labels.insert(id.clone(), label.clone());
                    label
                }
            },
        };
        out.push(json!({
            "name": item.entry.name,
            "path": item.entry.path,
            "is_dir": item.entry.is_dir,
            "size": item.entry.size,
            "modified": item.entry.modified,
            "owner": owner,
            "min_role": item.meta.min_role,
            "effective_min_role": item.effective_min_role,
        }));
    }
    Ok(out)
}

/// The display name for a stored owner id: the account's email, or the id itself
/// when it names nothing (a deleted user, or a value written by something other
/// than a signed-in request).
///
/// A lookup that *fails* — the database is unreachable, `users` has no email
/// column — falls back to the id as well. A file listing is not the place to
/// turn a metadata lookup into an error the admin sees instead of their files.
async fn owner_label(catalog: &Catalog, id: &str) -> Json {
    let user = match uuid::Uuid::parse_str(id) {
        Ok(uuid) => sc_auth::load_user(catalog, uuid).await.ok().flatten(),
        Err(_) => None,
    };
    let email = user
        .as_ref()
        .and_then(|u| u.extra().get("email"))
        .and_then(|value| value.as_text().map(str::to_owned));
    Json::String(email.unwrap_or_else(|| id.to_owned()))
}

/// The entry JSON returned after a successful `writeFile`: a file (never a
/// directory) at `path` with the byte count just written.
fn file_entry_written_json(path: &str, size: usize) -> Json {
    let name = path.rsplit('/').next().unwrap_or(path);
    json!({
        "name": name,
        "path": path,
        "is_dir": false,
        "size": size,
    })
}

/// Decode a `writeFile` body's contents: exactly one of `base64` (arbitrary
/// bytes) or `text` (UTF-8) must be present.
fn file_body_bytes(obj: &Map<String, Json>) -> Result<Bytes> {
    let base64 = obj.get("base64").and_then(Json::as_str);
    let text = obj.get("text").and_then(Json::as_str);
    match (base64, text) {
        (Some(_), Some(_)) => Err(Error::invalid(
            "provide either `base64` or `text`, not both",
        )),
        (Some(encoded), None) => BASE64
            .decode(encoded)
            .map(Bytes::from)
            .map_err(|e| Error::invalid(format!("invalid base64 in `base64`: {e}"))),
        (None, Some(text)) => Ok(Bytes::from(text.to_owned().into_bytes())),
        (None, None) => Err(Error::invalid(
            "missing file contents: set `base64` or `text`",
        )),
    }
}
