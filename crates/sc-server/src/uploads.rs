//! An application's upload slots, served (TODO F2, §2.3): the core tus 1.0
//! protocol with its `creation` and `termination` extensions, on
//! `{mount}/_upload/{name}`.
//!
//! - `POST {slot}` with `Upload-Length` and `Upload-Metadata` creates an
//!   upload, once the slot's role, size and type rules and its hook have let it
//!   in: `201` with `Location: {slot}/{id}`.
//! - `PATCH {slot}/{id}` with `Upload-Offset` appends one chunk, **streamed**
//!   into the store's `open_append` — no `--max-request-body`, and the slot's
//!   limit enforced as the bytes arrive rather than after.
//! - `HEAD {slot}/{id}` answers the offset, so a client resumes after a
//!   dropped connection.
//! - When the offset reaches the length, the file is renamed into place and
//!   given its owner (the uploader), its original name and the hook's
//!   attributes, and the `PATCH`'s answer carries `Feldspar-File-Path` and
//!   `Feldspar-File-Url`.
//! - `DELETE {slot}/{id}` abandons an upload. One left for a day is removed by
//!   [`sweep_uploads`].
//!
//! Only the uploader may continue, look at or abandon an upload: the signed-in
//! user who began it, or — for a public slot — the browser whose cookie began
//! it. Anybody else gets the 404 an unknown id gets.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use base64::Engine;
use sc_api::AuthRequirement;
use sc_app::{UploadRecord, UploadRoute, UploadSlot};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_files::{FileMeta, FileStore};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::apps::MountedApp;
use crate::router::{AppState, app_caller, enforce_auth, json_error, log_failure};
use crate::security::{SESSION_COOKIE, csrf_cookie};

/// The protocol version this server speaks, and the only one.
const TUS_VERSION: &str = "1.0.0";
/// The extensions it implements.
const TUS_EXTENSIONS: &str = "creation,termination";
/// The content type a `PATCH` body must declare.
const OFFSET_OCTET_STREAM: &str = "application/offset+octet-stream";
/// The header carrying a finished file's path in its store.
pub const FILE_PATH_HEADER: &str = "feldspar-file-path";
/// The header carrying the URL a finished file is served at, when one is.
pub const FILE_URL_HEADER: &str = "feldspar-file-url";
/// Where an upload's bytes wait in its store until they are complete: a
/// folder only an administrator may read.
pub const TEMP_DIR: &str = ".feldspar-uploads";
/// How long an unfinished upload is kept, in seconds.
pub const ABANDONED_AFTER_SECS: i64 = 24 * 60 * 60;

/// The uploads a `PATCH` is writing right now. A second `PATCH` of the same
/// upload is refused rather than interleaved with the first.
static WRITING: LazyLock<Mutex<HashSet<Uuid>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Holds an upload in [`WRITING`] for as long as it lives.
struct Writing(Uuid);

impl Writing {
    fn claim(id: Uuid) -> Option<Writing> {
        let mut writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        writing.insert(id).then_some(Writing(id))
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        WRITING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Answer one request to an upload slot. The body has not been read.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle(
    state: &AppState,
    app: &Arc<MountedApp>,
    route: UploadRoute,
    method: &Method,
    headers: &HeaderMap,
    jar: &CookieJar,
    anonymous: bool,
    body: Body,
) -> Response {
    tus(answer(state, app, route, method, headers, jar, anonymous, body).await)
}

/// Every answer says which protocol it speaks.
fn tus(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    response
}

#[allow(clippy::too_many_arguments)]
async fn answer(
    state: &AppState,
    app: &Arc<MountedApp>,
    route: UploadRoute,
    method: &Method,
    headers: &HeaderMap,
    jar: &CookieJar,
    anonymous: bool,
    body: Body,
) -> Response {
    let limit = route.slot.limit(state.max_upload_bytes);
    if method == Method::OPTIONS {
        return (
            StatusCode::NO_CONTENT,
            [
                ("tus-version", TUS_VERSION.to_owned()),
                ("tus-extension", TUS_EXTENSIONS.to_owned()),
                ("tus-max-size", limit.to_string()),
            ],
        )
            .into_response();
    }
    if let Some(version) = header_str(headers, "tus-resumable")
        && version != TUS_VERSION
    {
        return (
            StatusCode::PRECONDITION_FAILED,
            [("tus-version", TUS_VERSION)],
            axum::Json(json!({ "error": format!("this server speaks tus {TUS_VERSION}") })),
        )
            .into_response();
    }
    let Some(catalog) = state.apps.catalog() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "no catalog");
    };
    let caller = match app_caller(state, &app.app, jar, anonymous).await {
        Ok(caller) => caller,
        Err(rejection) => return *rejection,
    };
    // An upload is not part of anybody's sign-in (TODO F9).
    if let Some(refused) = caller.refused {
        return refused.response();
    }
    let user = caller.user;
    let uploader = uploader_of(user.as_ref(), jar, state.secure_cookies);
    let missing = || json_error(StatusCode::NOT_FOUND, "no such upload");

    let Some(id) = route.id.as_deref() else {
        return match *method {
            Method::POST => {
                create(
                    state,
                    app,
                    catalog,
                    &route.slot,
                    headers,
                    user.as_ref(),
                    uploader,
                )
                .await
            }
            _ => json_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "an upload is begun with POST",
            ),
        };
    };
    // Somebody else's upload, and one that is not there, are the same answer.
    let Ok(id) = Uuid::parse_str(id) else {
        return missing();
    };
    let record = match sc_app::load_upload(catalog, id).await {
        Ok(Some(record)) => record,
        Ok(None) => return missing(),
        Err(e) => return failed("reading an upload", &e),
    };
    if record.application != app.app.id
        || record.slot != route.slot.name
        || uploader.as_deref() != Some(record.uploader.as_str())
    {
        return missing();
    }
    let store = match catalog.require_file_store(&record.store) {
        Ok(store) => store,
        Err(_) => return missing(),
    };
    match *method {
        Method::HEAD => head(app, &record, store.as_ref()).await,
        Method::PATCH => {
            patch(
                app,
                catalog,
                &route.slot,
                limit,
                &record,
                store,
                headers,
                user.as_ref(),
                body,
            )
            .await
        }
        Method::DELETE => terminate(catalog, &record, store.as_ref()).await,
        _ => json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "an upload is continued with PATCH, looked at with HEAD and abandoned with DELETE",
        ),
    }
}

/// Who may continue an upload: `user:<id>` for a signed-in caller, otherwise
/// a hash of the cookie the browser carries — never the cookie itself, which
/// is a credential and has no business in a table. A request with neither has
/// nobody to continue it. `secure` picks the CSRF cookie's name
/// ([`csrf_cookie`]), so a cookie a sibling host tossed is never the one used.
pub(crate) fn uploader_of(user: Option<&User>, jar: &CookieJar, secure: bool) -> Option<String> {
    if let Some(user) = user {
        return Some(format!("user:{}", user.id));
    }
    let cookie = jar
        .get(SESSION_COOKIE)
        .or_else(|| jar.get(csrf_cookie(secure)))?
        .value()
        .to_owned();
    let digest = Sha256::digest(cookie.as_bytes());
    Some(format!(
        "anon:{}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

/// `POST {slot}`: check the caller, the size, the type and the hook, and
/// record the upload.
#[allow(clippy::too_many_arguments)]
async fn create(
    state: &AppState,
    app: &Arc<MountedApp>,
    catalog: &Arc<Catalog>,
    slot: &UploadSlot,
    headers: &HeaderMap,
    user: Option<&User>,
    uploader: Option<String>,
) -> Response {
    if let Some(refusal) = enforce_auth(&AuthRequirement::MinRole(slot.min_role), user) {
        return refusal;
    }
    let Some(uploader) = uploader else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "an upload needs a session or the CSRF cookie, so it can be resumed by whoever began it",
        );
    };
    if headers.contains_key("upload-defer-length") {
        return json_error(
            StatusCode::BAD_REQUEST,
            "this server needs Upload-Length up front (no creation-defer-length)",
        );
    }
    let Some(length) = header_str(headers, "upload-length").and_then(|v| v.parse::<u64>().ok())
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Upload-Length is required: the file's size in bytes",
        );
    };
    let limit = slot.limit(state.max_upload_bytes);
    if length > limit {
        let mut response = json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "the file is {length} bytes; upload slot `{}` takes at most {limit}",
                slot.name
            ),
        );
        if let Ok(max) = HeaderValue::from_str(&limit.to_string()) {
            response.headers_mut().insert("tus-max-size", max);
        }
        return response;
    }
    let metadata = parse_metadata(header_str(headers, "upload-metadata").unwrap_or(""));
    let filename = safe_filename(
        metadata
            .get("filename")
            .or_else(|| metadata.get("name"))
            .map(String::as_str)
            .unwrap_or(""),
    );
    // On the type the name implies, as a `File` field's allow-list is matched
    // (§3.5); the browser's own claim only when the name implies nothing.
    let mime = sc_files::mime_for_path(&filename).or_else(|| {
        metadata
            .get("filetype")
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
    });
    if !slot.mime_allow.is_empty()
        && !mime
            .as_deref()
            .is_some_and(|m| slot.mime_allow.iter().any(|a| a == m))
    {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!(
                "`{filename}` is {}; upload slot `{}` takes {}",
                mime.as_deref().unwrap_or("of no known type"),
                slot.name,
                slot.mime_allow.join(", ")
            ),
        );
    }
    let Ok(store) = catalog.require_file_store(&slot.store) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "the slot's file store is not connected",
        );
    };

    // The hook decides last: everything it is told has been checked.
    let decided = match &slot.hook {
        Some(hook) => {
            let body = json!({
                "filename": filename,
                "size": length,
                "mime": mime,
                "slot": slot.name,
            });
            match run_hook(app, catalog, hook, body, user).await {
                Ok(decided) => decided,
                Err(response) => return *response,
            }
        }
        None => Decided::default(),
    };

    let id = Uuid::new_v4();
    let name = match &decided.path {
        Some(path) => path.clone(),
        None => format!("{}-{filename}", &id.simple().to_string()[..12]),
    };
    let final_path = match slot.folder.is_empty() {
        true => name,
        false => format!("{}/{name}", slot.folder),
    };
    match store.stat(&final_path).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return json_error(
                StatusCode::CONFLICT,
                format!("there is already a file at `{final_path}`"),
            );
        }
        Err(e) => return failed("checking an upload's destination", &e),
    }
    let temp_path = format!("{TEMP_DIR}/{id}.part");
    if let Err(e) = prepare_temp(store.as_ref(), &temp_path).await {
        return failed("starting an upload", &e);
    }
    let mut meta = Map::new();
    meta.insert("attributes".to_owned(), Json::Object(decided.attributes));
    if let Some(min_role) = decided.min_role {
        meta.insert("min_role".to_owned(), json!(min_role));
    }
    let record = UploadRecord {
        id,
        application: app.app.id,
        slot: slot.name.clone(),
        store: slot.store.clone(),
        uploader,
        filename,
        mime,
        length,
        offset: 0,
        temp_path,
        final_path,
        meta: Json::Object(meta),
        completed: false,
        created: now(),
    };
    if let Err(e) = sc_app::insert_upload(catalog, &record).await {
        let _ = store.delete(&record.temp_path).await;
        return failed("recording an upload", &e);
    }
    let location = format!("{}/{id}", sc_app::upload_path(&app.app, &slot.name));
    let mut response = StatusCode::CREATED.into_response();
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
        .headers_mut()
        .insert("upload-offset", HeaderValue::from_static("0"));
    // An empty file is complete as soon as it exists.
    if length == 0 {
        return match complete(app, catalog, &record, store.as_ref(), user).await {
            Ok(done) => with_file_headers(response, &done),
            Err(response) => *response,
        };
    }
    response
}

/// What a hook decided, beyond "yes".
#[derive(Default)]
struct Decided {
    /// The file's path within the slot's folder.
    path: Option<String>,
    /// Extra attributes for the finished file.
    attributes: Map<String, Json>,
    /// A role floor for the finished file, which can only tighten its folder's.
    min_role: Option<u8>,
}

/// Run the slot's hook, and read its answer: a refusal is the hook's own
/// status and body; a `{ path?, meta?, min_role? }` is what it decided.
async fn run_hook(
    app: &Arc<MountedApp>,
    catalog: &Arc<Catalog>,
    hook: &str,
    body: Json,
    user: Option<&User>,
) -> std::result::Result<Decided, Box<Response>> {
    let mut answer = None;
    for provider in &app.providers {
        if let Some(result) = provider.run_hook(hook, body.clone(), catalog, user).await {
            answer = Some(result);
            break;
        }
    }
    let reply = match answer {
        Some(Ok(reply)) => reply,
        Some(Err(e)) => return Err(Box::new(failed("running an upload hook", &e))),
        None => {
            return Err(Box::new(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the upload hook `{hook}` is not one of this application's code queries"),
            )));
        }
    };
    if !(200..300).contains(&reply.status) {
        let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::FORBIDDEN);
        return Err(Box::new((status, axum::Json(reply.body)).into_response()));
    }
    let bad = |what: String| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the upload hook `{hook}` returned {what}"),
        ))
    };
    let obj = match reply.body {
        Json::Null => return Ok(Decided::default()),
        Json::Object(obj) => obj,
        other => return Err(bad(format!("{other}, not an object or nothing"))),
    };
    let mut decided = Decided::default();
    match obj.get("path") {
        None | Some(Json::Null) => {}
        Some(Json::String(path)) => match clean_relative(path) {
            Some(path) => decided.path = Some(path),
            None => {
                return Err(bad(format!(
                    "the path {path:?}, which is not inside the folder"
                )));
            }
        },
        Some(other) => return Err(bad(format!("a `path` of {other}, not text"))),
    }
    match obj.get("meta") {
        None | Some(Json::Null) => {}
        Some(Json::Object(meta)) => {
            for (key, value) in meta {
                let value = match value {
                    Json::String(s) => Json::String(s.clone()),
                    other => Json::String(other.to_string()),
                };
                decided.attributes.insert(key.clone(), value);
            }
        }
        Some(other) => return Err(bad(format!("a `meta` of {other}, not an object"))),
    }
    match obj.get("min_role").or_else(|| obj.get("minRole")) {
        None | Some(Json::Null) => {}
        Some(value) => match value.as_u64().and_then(|n| u8::try_from(n).ok()) {
            Some(role @ 1..=100) => decided.min_role = Some(role),
            _ => return Err(bad(format!("a `min_role` of {value}, not 1 to 100"))),
        },
    }
    Ok(decided)
}

/// `HEAD {slot}/{id}`: how far it has got.
async fn head(app: &MountedApp, record: &UploadRecord, store: &dyn FileStore) -> Response {
    let offset = match current_offset(record, store).await {
        Ok(Some(offset)) => offset,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such upload"),
        Err(e) => return failed("reading an upload's progress", &e),
    };
    let mut response = (
        StatusCode::OK,
        [
            ("upload-offset", offset.to_string()),
            ("upload-length", record.length.to_string()),
            ("cache-control", "no-store".to_owned()),
        ],
    )
        .into_response();
    if record.completed {
        response = with_file_headers(response, &Done::of(app, record));
    }
    response
}

/// The bytes received: the temporary file's size, which is what is on disk
/// whatever the row last recorded. `None` when the temporary file is gone.
async fn current_offset(
    record: &UploadRecord,
    store: &dyn FileStore,
) -> sc_error::Result<Option<u64>> {
    if record.completed {
        return Ok(Some(record.length));
    }
    Ok(store.stat(&record.temp_path).await?.map(|stat| stat.size))
}

/// `PATCH {slot}/{id}`: append one chunk, streamed, and finish the file when
/// it is all there.
#[allow(clippy::too_many_arguments)]
async fn patch(
    app: &Arc<MountedApp>,
    catalog: &Arc<Catalog>,
    slot: &UploadSlot,
    limit: u64,
    record: &UploadRecord,
    store: Arc<dyn FileStore>,
    headers: &HeaderMap,
    user: Option<&User>,
    body: Body,
) -> Response {
    let content_type = header_str(headers, header::CONTENT_TYPE.as_str()).unwrap_or("");
    if !content_type.starts_with(OFFSET_OCTET_STREAM) {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("a chunk is sent as {OFFSET_OCTET_STREAM}"),
        );
    }
    let Some(claimed) = header_str(headers, "upload-offset").and_then(|v| v.parse::<u64>().ok())
    else {
        return json_error(StatusCode::BAD_REQUEST, "Upload-Offset is required");
    };
    let Some(_writing) = Writing::claim(record.id) else {
        return json_error(
            StatusCode::CONFLICT,
            "another request is writing this upload; ask HEAD for the offset and try again",
        );
    };
    let offset = match current_offset(record, store.as_ref()).await {
        Ok(Some(offset)) => offset,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such upload"),
        Err(e) => return failed("reading an upload's progress", &e),
    };
    if claimed != offset {
        let mut response = json_error(
            StatusCode::CONFLICT,
            format!("the upload is at offset {offset}, not {claimed}"),
        );
        if let Ok(offset) = HeaderValue::from_str(&offset.to_string()) {
            response.headers_mut().insert("upload-offset", offset);
        }
        return response;
    }
    if record.completed {
        // The answer to a finished upload's last chunk, again: what a client
        // whose response was lost asks for.
        return with_file_headers(offset_response(offset), &Done::of(app, record));
    }
    // The slot may have shrunk since the upload began; the smaller bound holds.
    let bound = record.length.min(limit);

    let (written, outcome) = append(store.as_ref(), record, offset, bound, body).await;
    let offset = offset + written;
    if let Err(e) = sc_app::set_upload_progress(catalog, record.id, offset, false).await {
        log_failure("recording an upload's progress", &e);
    }
    match outcome {
        Appended::TooLarge => {
            let mut response = json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                if record.length > limit {
                    format!(
                        "upload slot `{}` now takes at most {limit} bytes; this upload is {}",
                        slot.name, record.length
                    )
                } else {
                    format!(
                        "the chunk runs past the upload's length of {} bytes",
                        record.length
                    )
                },
            );
            if let Ok(value) = HeaderValue::from_str(&offset.to_string()) {
                response.headers_mut().insert("upload-offset", value);
            }
            return response;
        }
        Appended::Failed(e) => return failed("writing an upload", &e),
        // A dropped connection keeps what arrived: that is what makes the
        // upload resumable. Nobody is listening for this answer.
        Appended::Interrupted | Appended::Complete => {}
    }
    if offset < record.length {
        return offset_response(offset);
    }
    match complete(app, catalog, record, store.as_ref(), user).await {
        Ok(done) => with_file_headers(offset_response(offset), &done),
        Err(response) => *response,
    }
}

/// How an append ended.
enum Appended {
    /// The body was read to its end.
    Complete,
    /// The body broke off; what arrived before is kept.
    Interrupted,
    /// The body ran past the bound; nothing past it was written.
    TooLarge,
    /// The store refused a write.
    Failed(sc_error::Error),
}

/// Stream `body` onto the end of the upload's temporary file, never past
/// `bound` bytes in all. Answers how many bytes were written.
async fn append(
    store: &dyn FileStore,
    record: &UploadRecord,
    offset: u64,
    bound: u64,
    body: Body,
) -> (u64, Appended) {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut writer = match store.open_append(&record.temp_path).await {
        Ok(writer) => writer,
        Err(e) => return (0, Appended::Failed(e)),
    };
    let mut stream = body.into_data_stream();
    let mut written = 0u64;
    let mut outcome = Appended::Complete;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                outcome = Appended::Interrupted;
                break;
            }
        };
        if offset + written + chunk.len() as u64 > bound {
            outcome = Appended::TooLarge;
            break;
        }
        if let Err(e) = writer.write_all(&chunk).await {
            outcome = Appended::Failed(e.into());
            break;
        }
        written += chunk.len() as u64;
    }
    if let Err(e) = writer.shutdown().await
        && matches!(outcome, Appended::Complete | Appended::Interrupted)
    {
        outcome = Appended::Failed(e.into());
    }
    (written, outcome)
}

/// A finished upload: where it is, and where it is served.
struct Done {
    path: String,
    url: Option<String>,
}

impl Done {
    fn of(app: &MountedApp, record: &UploadRecord) -> Done {
        Done {
            path: record.final_path.clone(),
            url: file_url(app, &record.store, &record.final_path),
        }
    }
}

/// Move the bytes into place and give the file its owner, its name and the
/// hook's attributes.
async fn complete(
    app: &MountedApp,
    catalog: &Catalog,
    record: &UploadRecord,
    store: &dyn FileStore,
    user: Option<&User>,
) -> std::result::Result<Done, Box<Response>> {
    if let Err(e) = store.rename(&record.temp_path, &record.final_path).await {
        // The destination appeared while the bytes were arriving.
        if store
            .stat(&record.final_path)
            .await
            .ok()
            .flatten()
            .is_some()
        {
            return Err(Box::new(json_error(
                StatusCode::CONFLICT,
                format!("there is already a file at `{}`", record.final_path),
            )));
        }
        return Err(Box::new(failed("putting an upload in place", &e)));
    }
    let mut meta = store.get_meta(&record.final_path).await.unwrap_or_default();
    meta = FileMeta {
        owner: user.map(|u| u.id.to_string()).or(meta.owner),
        min_role: record
            .meta
            .get("min_role")
            .and_then(Json::as_u64)
            .and_then(|n| u8::try_from(n).ok())
            .or(meta.min_role),
        ..meta
    };
    meta.attributes.insert(
        sc_files::FILENAME_ATTRIBUTE.to_owned(),
        record.filename.clone(),
    );
    if let Some(Json::Object(attributes)) = record.meta.get("attributes") {
        for (key, value) in attributes {
            if let Some(value) = value.as_str() {
                meta.attributes.insert(key.clone(), value.to_owned());
            }
        }
    }
    if let Err(e) = store.set_meta(&record.final_path, &meta).await {
        log_failure("recording an upload's owner", &e);
    }
    if let Err(e) = sc_app::set_upload_progress(catalog, record.id, record.length, true).await {
        log_failure("recording a finished upload", &e);
    }
    Ok(Done::of(app, record))
}

/// `DELETE {slot}/{id}`: abandon it. A finished file stays; only the record of
/// its upload goes.
async fn terminate(catalog: &Catalog, record: &UploadRecord, store: &dyn FileStore) -> Response {
    if !record.completed
        && let Err(e) = store.delete(&record.temp_path).await
    {
        log_failure("removing an abandoned upload", &e);
    }
    if let Err(e) = sc_app::delete_upload(catalog, record.id).await {
        return failed("forgetting an upload", &e);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Remove every upload begun before `before` (Unix seconds) — the unfinished
/// ones' bytes with them — and answer how many. The housekeeping tick calls it
/// with a day ago; a finished upload's file stays where it was put.
pub async fn sweep_uploads(catalog: &Catalog, before: i64) -> sc_error::Result<usize> {
    let stale = sc_app::uploads_begun_before(catalog, before).await?;
    for record in &stale {
        if !record.completed
            && let Ok(store) = catalog.require_file_store(&record.store)
        {
            let _ = store.delete(&record.temp_path).await;
        }
        sc_app::delete_upload(catalog, record.id).await?;
    }
    Ok(stale.len())
}

/// Sweep abandoned uploads every ten minutes, for the life of the process.
pub(crate) fn spawn_upload_sweep(apps: Arc<crate::apps::AppMounts>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
        loop {
            tick.tick().await;
            let Some(catalog) = apps.catalog() else {
                continue;
            };
            match sweep_uploads(catalog, now() - ABANDONED_AFTER_SECS).await {
                Ok(0) => {}
                Ok(n) => sc_log::log_info!("removed {n} upload(s) begun over a day ago"),
                Err(e) => log_failure("sweeping abandoned uploads", &e),
            }
        }
    });
}

/// Create the temporary file, empty, in a folder only an administrator reads.
async fn prepare_temp(store: &dyn FileStore, temp_path: &str) -> sc_error::Result<()> {
    store.mkdir(TEMP_DIR).await?;
    let meta = store.get_meta(TEMP_DIR).await.unwrap_or_default();
    if meta.min_role != Some(sc_files::ROLE_ADMIN) {
        store
            .set_meta(
                TEMP_DIR,
                &FileMeta {
                    min_role: Some(sc_files::ROLE_ADMIN),
                    ..meta
                },
            )
            .await?;
    }
    store.write(temp_path, bytes::Bytes::new()).await
}

/// The URL a store file is served at on this application: through a static
/// directory that covers it, or a Saltcorn UI application's `/files/serve`.
/// `None` when no route serves it.
pub fn file_url(app: &MountedApp, store: &str, path: &str) -> Option<String> {
    app.app.file_url(store, path)
}

fn offset_response(offset: u64) -> Response {
    (
        StatusCode::NO_CONTENT,
        [("upload-offset", offset.to_string())],
    )
        .into_response()
}

fn with_file_headers(mut response: Response, done: &Done) -> Response {
    if let Ok(path) = HeaderValue::from_str(&done.path) {
        response.headers_mut().insert(FILE_PATH_HEADER, path);
    }
    if let Some(url) = &done.url
        && let Ok(url) = HeaderValue::from_str(url)
    {
        response.headers_mut().insert(FILE_URL_HEADER, url);
    }
    response
}

/// `Upload-Metadata`: comma-separated `key base64(value)` pairs. A pair that
/// does not decode is skipped rather than refused — a client's optional field
/// should not cost it the upload.
fn parse_metadata(raw: &str) -> std::collections::HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| {
            let mut parts = pair.trim().splitn(2, ' ');
            let key = parts.next()?.trim();
            if key.is_empty() {
                return None;
            }
            let value = match parts.next().map(str::trim) {
                Some(encoded) if !encoded.is_empty() => String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()?,
                )
                .ok()?,
                _ => String::new(),
            };
            Some((key.to_owned(), value))
        })
        .collect()
}

/// The last segment of a client's filename, with nothing in it a path could
/// trip on. Empty is `file`.
fn safe_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    match cleaned.is_empty() {
        true => "file".to_owned(),
        false => cleaned.chars().take(200).collect(),
    }
}

/// A hook's path, relative to the slot's folder: segments with no `.`, `..`,
/// backslash or NUL, and at least one of them.
fn clean_relative(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let valid = !segments.is_empty()
        && segments.iter().all(|s| {
            *s != "." && *s != ".." && !s.contains(['\\', '\0']) && !s.chars().any(char::is_control)
        });
    valid.then(|| segments.join("/"))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A failure the caller cannot fix, logged with its cause and answered
/// without it.
fn failed(context: &str, err: &sc_error::Error) -> Response {
    log_failure(context, err);
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("the upload failed while {context}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_base64_pairs() {
        let meta = parse_metadata("filename bmHDr3ZlIOWQjeWJjS5wbmc=,filetype aW1hZ2UvcG5n, flag");
        assert_eq!(meta["filename"], "naïve 名前.png");
        assert_eq!(meta["filetype"], "image/png");
        assert_eq!(meta["flag"], "");
        assert!(parse_metadata("bad !!!").is_empty());
    }

    #[test]
    fn a_filename_is_one_safe_segment() {
        assert_eq!(safe_filename("C:\\Users\\me\\film.mp4"), "film.mp4");
        assert_eq!(safe_filename("../../etc/passwd"), "passwd");
        assert_eq!(safe_filename(".htaccess"), "htaccess");
        assert_eq!(safe_filename(""), "file");
        assert_eq!(safe_filename("a\nb.txt"), "a_b.txt");
    }

    #[test]
    fn a_hook_path_stays_inside_the_folder() {
        assert_eq!(clean_relative("a/b.png").as_deref(), Some("a/b.png"));
        assert_eq!(clean_relative("/a//b.png").as_deref(), Some("a/b.png"));
        assert_eq!(clean_relative("../b"), None);
        assert_eq!(clean_relative("a/./b"), None);
        assert_eq!(clean_relative(""), None);
    }
}
