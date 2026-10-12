//! An application's **upload slots** and the resumable uploads made through
//! them (TODO F2, §2.3).
//!
//! A slot is a named place a client may put a large file: a store, a folder in
//! it, who may start an upload, how big one may be, which types it takes, and
//! optionally a code query that decides the rest. The server speaks the tus 1.0
//! protocol on `{mount}/_upload/{name}` ([`upload_route`]), streaming each chunk
//! into the store, so a file is never in memory and a dropped connection resumes
//! from where it stopped.
//!
//! What an upload in progress is — its slot, its uploader, its length and where
//! its bytes are going — is a row of [`UPLOADS_TABLE`], so a resume survives a
//! restart. The bytes themselves are the temporary file, and its size is the
//! offset: a row cannot claim bytes that are not on disk.

use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, BinOp, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::application::{AppId, Application};

/// The path segment upload slots are served under, beside the endpoint set.
pub const UPLOAD_SEGMENT: &str = "_upload";

/// The role a slot admits unless it says otherwise: the administrator, as a
/// custom query's default is (§13.4).
pub const DEFAULT_SLOT_MIN_ROLE: u8 = 1;

/// A named upload slot of an application (§2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSlot {
    /// The URL segment and the generated client's method name
    /// (`api.uploads.<name>`), so an identifier.
    pub name: String,
    /// The file store the files land in: one of the application's own.
    pub store: String,
    /// The folder within the store (`""` is its root). A file lands at
    /// `<folder>/<generated name>`, or `<folder>/<the hook's path>`.
    pub folder: String,
    /// The least privileged role that may start an upload (`1` = admin … `100`
    /// = public).
    pub min_role: u8,
    /// The largest file one upload may be. `0` is the server's own ceiling
    /// (`--max-upload-bytes`), which caps a larger value too.
    pub max_bytes: u64,
    /// The MIME types accepted, matched as a `File` field's allow-list is: on
    /// the type the file's name implies. Empty accepts any.
    pub mime_allow: Vec<String>,
    /// A JavaScript or Python custom query of this application, called before
    /// an upload is accepted with `body = { filename, size, mime, slot }`.
    pub hook: Option<String>,
}

impl UploadSlot {
    /// A slot `name` into `folder` of `store`, admin-only, with no limit of its
    /// own, any type and no hook.
    pub fn new(
        name: impl Into<String>,
        store: impl Into<String>,
        folder: impl Into<String>,
    ) -> UploadSlot {
        UploadSlot {
            name: name.into(),
            store: store.into(),
            folder: clean_folder(&folder.into()),
            min_role: DEFAULT_SLOT_MIN_ROLE,
            max_bytes: 0,
            mime_allow: Vec::new(),
            hook: None,
        }
    }

    /// Set who may start an upload.
    pub fn min_role(mut self, min_role: u8) -> UploadSlot {
        self.min_role = min_role;
        self
    }

    /// Set the largest file an upload may be.
    pub fn max_bytes(mut self, max_bytes: u64) -> UploadSlot {
        self.max_bytes = max_bytes;
        self
    }

    /// Accept only these MIME types.
    pub fn mime_allow(mut self, mime: impl IntoIterator<Item = impl Into<String>>) -> UploadSlot {
        self.mime_allow = mime.into_iter().map(Into::into).collect();
        self
    }

    /// Call the custom query `hook` before accepting an upload.
    pub fn hook(mut self, hook: impl Into<String>) -> UploadSlot {
        self.hook = Some(hook.into());
        self
    }

    /// The limit an upload through this slot is held to, given the server's.
    pub fn limit(&self, server_max: u64) -> u64 {
        match self.max_bytes {
            0 => server_max,
            own => own.min(server_max),
        }
    }

    /// The slot as the record, the admin API and a backup spell it.
    pub fn to_json(&self) -> Json {
        json!({
            "name": self.name,
            "store": self.store,
            "folder": self.folder,
            "min_role": self.min_role,
            "max_bytes": self.max_bytes,
            "mime_allow": self.mime_allow,
            "hook": self.hook,
        })
    }

    /// Read a slot back: `name` and `store` are required, everything else has
    /// the default [`UploadSlot::new`] gives it. An unknown key is refused,
    /// because a misspelled `max_byte` that was silently ignored is a slot
    /// with no limit.
    pub fn from_json(value: &Json) -> Result<UploadSlot> {
        let Json::Object(obj) = value else {
            return Err(Error::invalid(format!(
                "an upload slot should be an object, got {value}"
            )));
        };
        const KEYS: [&str; 7] = [
            "name",
            "store",
            "folder",
            "min_role",
            "max_bytes",
            "mime_allow",
            "hook",
        ];
        if let Some(unknown) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(Error::invalid(format!(
                "an upload slot has no setting `{unknown}`; the settings are {}",
                KEYS.join(", ")
            )));
        }
        let text = |key: &str, required: bool| -> Result<String> {
            match obj.get(key) {
                Some(Json::String(s)) => Ok(s.trim().to_owned()),
                None | Some(Json::Null) if !required => Ok(String::new()),
                None | Some(Json::Null) => {
                    Err(Error::invalid(format!("an upload slot needs a `{key}`")))
                }
                Some(other) => Err(Error::invalid(format!(
                    "an upload slot's `{key}` should be text, got {other}"
                ))),
            }
        };
        let min_role = match obj.get("min_role") {
            None | Some(Json::Null) => DEFAULT_SLOT_MIN_ROLE,
            Some(v) => v
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "an upload slot's `min_role` should be a role number, 1 to 100, got {v}"
                    ))
                })?,
        };
        let max_bytes = match obj.get("max_bytes") {
            None | Some(Json::Null) => 0,
            Some(v) => v.as_u64().ok_or_else(|| {
                Error::invalid(format!(
                    "an upload slot's `max_bytes` should be a whole number of bytes, got {v}"
                ))
            })?,
        };
        let mime_allow = match obj.get("mime_allow") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.trim().to_ascii_lowercase())
                        .ok_or_else(|| {
                            Error::invalid(format!(
                                "an upload slot's `mime_allow` should be a list of types, \
                                 got {item}"
                            ))
                        })
                })
                .filter(|m| m.as_ref().map_or(true, |m| !m.is_empty()))
                .collect::<Result<_>>()?,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "an upload slot's `mime_allow` should be a list of types, got {other}"
                )));
            }
        };
        let hook = Some(text("hook", false)?).filter(|h| !h.is_empty());
        Ok(UploadSlot {
            name: text("name", true)?,
            store: text("store", true)?,
            folder: clean_folder(&text("folder", false)?),
            min_role,
            max_bytes,
            mime_allow,
            hook,
        })
    }
}

/// A folder as a slot keeps it: no leading or trailing slash.
fn clean_folder(folder: &str) -> String {
    folder.trim().trim_matches('/').to_owned()
}

/// The slots of a stored or submitted list, in order.
pub fn upload_slots_from_json(value: Option<&Json>) -> Result<Vec<UploadSlot>> {
    match value {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items.iter().map(UploadSlot::from_json).collect(),
        Some(other) => Err(Error::invalid(format!(
            "`uploads` should be a list of upload slots, got {other}"
        ))),
    }
}

/// The slots as a JSON list.
pub fn upload_slots_json(slots: &[UploadSlot]) -> Json {
    Json::Array(slots.iter().map(UploadSlot::to_json).collect())
}

/// The application's mount: its first API's, as the live socket's and the
/// catalogues' are. Empty for an application with no API provider.
fn mount(app: &Application) -> &str {
    app.apis
        .first()
        .map(|api| api.mount.trim_end_matches('/'))
        .unwrap_or("")
}

/// Where the slot `name` is served: `{mount}/_upload/{name}`. The generated
/// client and the router both go through this, so they cannot disagree.
pub fn upload_path(app: &Application, name: &str) -> String {
    format!("{}/{UPLOAD_SEGMENT}/{name}", mount(app))
}

/// The application's upload slots as its generated client names them —
/// `api.uploads.<name>` (TODO F2.11).
pub fn upload_exports(app: &Application) -> Vec<sc_api::UploadExport> {
    app.uploads
        .iter()
        .map(|slot| sc_api::UploadExport {
            name: slot.name.clone(),
            path: upload_path(app, &slot.name),
        })
        .collect()
}

/// A request path that is one of `app`'s upload slots: creating an upload
/// (`id` is `None`) or one upload in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRoute {
    /// The slot, as the application declares it.
    pub slot: UploadSlot,
    /// The upload, for `PATCH`, `HEAD` and `DELETE`.
    pub id: Option<String>,
}

/// The upload slot `path` addresses on `app`, if any.
///
/// A path under `{mount}/_upload/` naming no slot is `None` and falls through
/// to the providers, whose answer is the 404 any unknown path gets.
pub fn upload_route(app: &Application, path: &str) -> Option<UploadRoute> {
    if app.uploads.is_empty() {
        return None;
    }
    let rest = path
        .strip_prefix(mount(app))?
        .strip_prefix('/')?
        .strip_prefix(UPLOAD_SEGMENT)?
        .strip_prefix('/')?;
    let (name, id) = match rest.split_once('/') {
        Some((name, id)) if !id.is_empty() && !id.contains('/') => (name, Some(id.to_owned())),
        Some(_) => return None,
        None => (rest, None),
    };
    let slot = app.uploads.iter().find(|s| s.name == name)?.clone();
    Some(UploadRoute { slot, id })
}

/// Check an application's upload slots on save (§2.3), beside the static
/// directories: a name that is an identifier and unique, a store the
/// application declares, a folder inside the store, a role that is a role, and
/// a hook that is one of the application's own code queries.
pub fn validate_upload_slots(app: &Application) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for slot in &app.uploads {
        let name = &slot.name;
        let identifier = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !identifier {
            return Err(Error::invalid(format!(
                "upload slot `{name}` should be named like an identifier (letters, digits \
                 and `_`, not starting with a digit): it is a URL segment and a method \
                 name in the generated client"
            )));
        }
        if !seen.insert(name.as_str()) {
            return Err(Error::invalid(format!(
                "application `{}` has two upload slots named `{name}`",
                app.name
            )));
        }
        if !app.file_stores.iter().any(|s| s.0 == slot.store) {
            return Err(Error::invalid(format!(
                "upload slot `{name}` puts files in file store `{}`, which application `{}` \
                 does not declare access to; add the store to the application's file stores, \
                 or pick one it already has",
                slot.store, app.name
            )));
        }
        if slot
            .folder
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
            || slot.folder.contains(['\\', '\0'])
        {
            return Err(Error::invalid(format!(
                "upload slot `{name}`'s folder `{}` must be a folder inside the store",
                slot.folder
            )));
        }
        if !(1..=100).contains(&slot.min_role) {
            return Err(Error::invalid(format!(
                "upload slot `{name}`'s minimum role should be 1 (admin) to 100 (public), \
                 got {}",
                slot.min_role
            )));
        }
        if let Some(hook) = &slot.hook {
            let found = app
                .apis
                .iter()
                .filter_map(|api| sc_api::custom_queries(&api.config).ok())
                .flatten()
                .find(|q| &q.name == hook);
            match found {
                Some(q) if q.is_code() => {}
                Some(_) => {
                    return Err(Error::invalid(format!(
                        "upload slot `{name}`'s hook `{hook}` is a SQL query; a hook is a \
                         JavaScript or Python query, since it decides with code"
                    )));
                }
                None => {
                    return Err(Error::invalid(format!(
                        "upload slot `{name}`'s hook `{hook}` is not one of application `{}`'s \
                         custom queries",
                        app.name
                    )));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Uploads in progress: `_fd_uploads`
// ---------------------------------------------------------------------------

/// The table resumable uploads are recorded in.
pub const UPLOADS_TABLE: &str = "_fd_uploads";

const COL_ID: &str = "id";
/// The original filename — an upload row's `name` (§9).
const COL_NAME: &str = "name";
const COL_DESCRIPTION: &str = "description";
const COL_APPLICATION: &str = "application";
const COL_SLOT: &str = "slot";
const COL_STORE: &str = "store";
const COL_UPLOADER: &str = "uploader";
const COL_MIME: &str = "mime";
const COL_LENGTH: &str = "length";
const COL_OFFSET: &str = "upload_offset";
const COL_TEMP_PATH: &str = "temp_path";
const COL_FINAL_PATH: &str = "final_path";
const COL_META: &str = "meta";
const COL_COMPLETED: &str = "completed";
const COL_CREATED: &str = "created";
const COL_ATTRIBUTES: &str = "attributes";

fn uploads_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let int = || TypeRef::Basic(BasicType::Int);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_APPLICATION, uuid()).required(),
        DataField::plain(COL_SLOT, text()).required(),
        DataField::plain(COL_STORE, text()).required(),
        DataField::plain(COL_UPLOADER, text()).required(),
        DataField::plain(COL_MIME, text()),
        DataField::plain(COL_LENGTH, int()).required(),
        DataField::plain(COL_OFFSET, int()).required(),
        DataField::plain(COL_TEMP_PATH, text()).required(),
        DataField::plain(COL_FINAL_PATH, text()).required(),
        DataField::plain(COL_META, json()).required(),
        DataField::plain(COL_COMPLETED, TypeRef::Basic(BasicType::Bool)).required(),
        DataField::plain(COL_CREATED, int()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure `_fd_uploads` exists. Idempotent and additively reconciled, like
/// every other bootstrap; called by [`bootstrap`](crate::bootstrap).
pub async fn bootstrap_uploads(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(UPLOADS_TABLE, &uploads_fields())
        .await?;
    Ok(())
}

/// One resumable upload, begun and perhaps finished.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadRecord {
    /// The id in the upload's URL.
    pub id: Uuid,
    /// The application whose slot it came through.
    pub application: AppId,
    /// The slot's name.
    pub slot: String,
    /// The store the bytes are going to.
    pub store: String,
    /// Who may continue it: `user:<id>` for a signed-in uploader, otherwise
    /// `anon:<hash>` of the browser's cookie — never the cookie itself.
    pub uploader: String,
    /// The name the client gave the file.
    pub filename: String,
    /// The type it was accepted as.
    pub mime: Option<String>,
    /// The declared size.
    pub length: u64,
    /// The bytes received, as last recorded. The temporary file's size is the
    /// truth; this is what a listing shows.
    pub offset: u64,
    /// Where the bytes are being written, in the store.
    pub temp_path: String,
    /// Where the file goes when it is complete, in the store.
    pub final_path: String,
    /// Extra attributes for the finished file (the hook's `meta`), plus
    /// `min_role` when the hook tightened it.
    pub meta: Json,
    /// Whether the file is in place.
    pub completed: bool,
    /// When it was begun, in Unix seconds.
    pub created: i64,
}

/// Record a new upload.
pub async fn insert_upload(cat: &Catalog, upload: &UploadRecord) -> Result<()> {
    let insert = Insert::row(
        UPLOADS_TABLE,
        [
            COL_ID,
            COL_NAME,
            COL_DESCRIPTION,
            COL_APPLICATION,
            COL_SLOT,
            COL_STORE,
            COL_UPLOADER,
            COL_MIME,
            COL_LENGTH,
            COL_OFFSET,
            COL_TEMP_PATH,
            COL_FINAL_PATH,
            COL_META,
            COL_COMPLETED,
            COL_CREATED,
            COL_ATTRIBUTES,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::lit(upload.id),
            Expr::lit(upload.filename.as_str()),
            Expr::lit(""),
            Expr::lit(upload.application.0),
            Expr::lit(upload.slot.as_str()),
            Expr::lit(upload.store.as_str()),
            Expr::lit(upload.uploader.as_str()),
            Expr::Lit(
                upload
                    .mime
                    .as_ref()
                    .map_or(Value::Null, |m| Value::Text(m.clone())),
            ),
            Expr::Lit(Value::Int(as_i64(upload.length))),
            Expr::Lit(Value::Int(as_i64(upload.offset))),
            Expr::lit(upload.temp_path.as_str()),
            Expr::lit(upload.final_path.as_str()),
            Expr::Lit(Value::Json(upload.meta.clone())),
            Expr::Lit(Value::Bool(upload.completed)),
            Expr::Lit(Value::Int(upload.created)),
            Expr::Lit(Value::Json(json!({}))),
        ],
    );
    run(cat, Statement::from(insert)).await
}

/// The upload `id`, if it is recorded.
pub async fn load_upload(cat: &Catalog, id: Uuid) -> Result<Option<UploadRecord>> {
    let select = Select::from(Source::table(UPLOADS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    rows(cat, select)
        .await?
        .first()
        .map(record_from_row)
        .transpose()
}

/// Record how far an upload has got, and whether it is finished.
pub async fn set_upload_progress(
    cat: &Catalog,
    id: Uuid,
    offset: u64,
    completed: bool,
) -> Result<()> {
    let update = Update::new(
        UPLOADS_TABLE,
        vec![
            Assignment::new(COL_OFFSET.to_owned(), Expr::Lit(Value::Int(as_i64(offset)))),
            Assignment::new(COL_COMPLETED.to_owned(), Expr::Lit(Value::Bool(completed))),
        ],
    )
    .filter(Expr::col(COL_ID).eq(Expr::lit(id)));
    run(cat, Statement::from(update)).await
}

/// Forget an upload. Its bytes are the caller's to remove.
pub async fn delete_upload(cat: &Catalog, id: Uuid) -> Result<()> {
    let delete = Delete::from(UPLOADS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id)));
    run(cat, Statement::from(delete)).await
}

/// Every upload begun before `before` (Unix seconds) — what the sweep removes.
pub async fn uploads_begun_before(cat: &Catalog, before: i64) -> Result<Vec<UploadRecord>> {
    if cat.get(UPLOADS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let select = Select::from(Source::table(UPLOADS_TABLE)).filter(Expr::binary(
        BinOp::Lt,
        Expr::col(COL_CREATED),
        Expr::Lit(Value::Int(before)),
    ));
    rows(cat, select)
        .await?
        .iter()
        .map(record_from_row)
        .collect()
}

fn as_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn record_from_row(row: &Row) -> Result<UploadRecord> {
    let bad = |column: &str, wanted: &str, got: Option<&Value>| {
        Error::invalid(format!(
            "{UPLOADS_TABLE}.{column} should be {wanted}, got {}",
            got.map(Value::kind).unwrap_or("nothing")
        ))
    };
    let uuid = |column: &str| match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad(column, "a uuid", other)),
    };
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad(column, "text", other)),
    };
    let int = |column: &str| match row.get(column) {
        Some(Value::Int(n)) => Ok(*n),
        other => Err(bad(column, "an integer", other)),
    };
    Ok(UploadRecord {
        id: uuid(COL_ID)?,
        application: AppId(uuid(COL_APPLICATION)?),
        slot: text(COL_SLOT)?,
        store: text(COL_STORE)?,
        uploader: text(COL_UPLOADER)?,
        filename: text(COL_NAME)?,
        mime: match row.get(COL_MIME) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        },
        length: u64::try_from(int(COL_LENGTH)?).unwrap_or(0),
        offset: u64::try_from(int(COL_OFFSET)?).unwrap_or(0),
        temp_path: text(COL_TEMP_PATH)?,
        final_path: text(COL_FINAL_PATH)?,
        meta: match row.get(COL_META) {
            Some(Value::Json(j)) => j.clone(),
            _ => Json::Object(Map::new()),
        },
        completed: matches!(row.get(COL_COMPLETED), Some(Value::Bool(true))),
        created: int(COL_CREATED)?,
    })
}

async fn run(cat: &Catalog, statement: Statement) -> Result<()> {
    cat.primary().query(&statement).await?.try_collect().await?;
    Ok(())
}

async fn rows(cat: &Catalog, select: Select) -> Result<Vec<Row>> {
    cat.primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiConfig, FrameworkRef};
    use sc_catalog::FileStoreId;

    fn app() -> Application {
        Application::new("Drop", "drop", FrameworkRef::new("code"))
            .with_file_store(FileStoreId("files".to_owned()))
            .with_api(ApiConfig::new("rest", "/api"))
    }

    #[test]
    fn a_slot_round_trips_through_json_with_its_defaults() {
        let slot = UploadSlot::new("drop", "files", "/drops/")
            .min_role(80)
            .max_bytes(1 << 30)
            .mime_allow(["video/mp4"])
            .hook("beforeDrop");
        assert_eq!(slot.folder, "drops");
        assert_eq!(UploadSlot::from_json(&slot.to_json()).unwrap(), slot);

        let bare = UploadSlot::from_json(&json!({ "name": "a", "store": "files" })).unwrap();
        assert_eq!(bare, UploadSlot::new("a", "files", ""));
        assert_eq!(bare.min_role, 1, "admin unless stated");

        let err = UploadSlot::from_json(&json!({ "name": "a", "store": "s", "max_byte": 3 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_byte"), "{err}");
        assert!(UploadSlot::from_json(&json!({ "name": "a" })).is_err());
    }

    #[test]
    fn a_slots_limit_is_capped_by_the_servers() {
        let slot = UploadSlot::new("a", "s", "");
        assert_eq!(slot.limit(100), 100);
        assert_eq!(slot.clone().max_bytes(10).limit(100), 10);
        assert_eq!(slot.max_bytes(1000).limit(100), 100);
    }

    #[test]
    fn the_route_is_beside_the_endpoint_set() {
        let mut app = app();
        app.uploads.push(UploadSlot::new("drop", "files", "drops"));
        assert_eq!(upload_path(&app, "drop"), "/api/_upload/drop");

        let create = upload_route(&app, "/api/_upload/drop").unwrap();
        assert_eq!(create.slot.name, "drop");
        assert_eq!(create.id, None);
        let one = upload_route(&app, "/api/_upload/drop/abc").unwrap();
        assert_eq!(one.id.as_deref(), Some("abc"));

        assert!(upload_route(&app, "/api/_upload/other").is_none());
        assert!(upload_route(&app, "/api/_upload/drop/a/b").is_none());
        assert!(upload_route(&app, "/_upload/drop").is_none());
        assert!(upload_route(&app, "/api/posts").is_none());
    }

    #[test]
    fn a_slot_is_validated_against_the_application() {
        let ok = |slot: UploadSlot| {
            let mut app = app();
            app.uploads.push(slot);
            validate_upload_slots(&app)
        };
        ok(UploadSlot::new("drop", "files", "drops")).unwrap();

        let err = ok(UploadSlot::new("drop", "elsewhere", ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not declare"), "{err}");
        let err = ok(UploadSlot::new("my-drop", "files", ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("identifier"), "{err}");
        assert!(ok(UploadSlot::new("drop", "files", "../up")).is_err());
        assert!(ok(UploadSlot::new("drop", "files", "").min_role(0)).is_err());
        let err = ok(UploadSlot::new("drop", "files", "").hook("nothing"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not one of"), "{err}");

        let mut twice = app();
        twice.uploads.push(UploadSlot::new("drop", "files", ""));
        twice.uploads.push(UploadSlot::new("drop", "files", "b"));
        assert!(validate_upload_slots(&twice).is_err());
    }

    #[test]
    fn a_hook_is_one_of_the_applications_code_queries() {
        let queries = json!([
            { "name": "beforeDrop", "method": "POST", "path": "/before",
              "language": "javascript", "code": "return {};" },
            { "name": "count", "method": "GET", "path": "/count", "code": "select 1" },
        ]);
        let mut app = Application::new("Drop", "drop", FrameworkRef::new("code"))
            .with_file_store(FileStoreId("files".to_owned()))
            .with_api(ApiConfig::new("rest", "/api").with("queries", queries));
        app.uploads
            .push(UploadSlot::new("drop", "files", "").hook("beforeDrop"));
        validate_upload_slots(&app).unwrap();
        app.uploads[0].hook = Some("count".to_owned());
        let err = validate_upload_slots(&app).unwrap_err().to_string();
        assert!(err.contains("SQL"), "{err}");
    }
}
