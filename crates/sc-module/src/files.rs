//! **File writes from JavaScript modules** (TODO-imap M6): what `ctx.files` in a
//! service, and `files` in an action's argument object, reach.
//!
//! A mail module stores attachments. It declares a file field for them (M4.5),
//! `file_store: "mail_files"` with `file_folder: "attachments"`, and that
//! declaration is the whole of its file authority: [`ModuleFiles`] answers
//! operations inside the **areas** its declared file fields name — a store, and
//! the folder within it — and refuses every other store and every path outside
//! the folder, by name. No permission key is needed for the same reason none is
//! needed for `ctx.db`: the module reaches only what it declared.
//!
//! ## The module's authority
//!
//! Every operation runs with **admin authority** within the area, whether a
//! service or an action asked. An action's caller decides which *rows* it may
//! touch (its `Table` reads run as the caller), and the module decides which of
//! its own files those rows need; a user who could not write the store directly
//! still gets the attachment their action stored. Who may then *download* a file
//! is the File field's business, as for any other file.
//!
//! ## What crosses
//!
//! The operations are [`FileStoreHost`]'s own — `write`, `append`, `read`,
//! `stat`, `delete`, `rename` — in its JSON, plus two options this layer applies
//! itself after a `write` or a `rename`: `contentType` (kept as the file's
//! [`CONTENT_TYPE_ATTRIBUTE`], which the file routes serve it as) and `filename`
//! (its [`FILENAME_ATTRIBUTE`]). The bytes travel base64 and one operation
//! carries at most `--code-max-file-bytes`, so the host script writes a large
//! file in pieces: the first with `write`, the rest with `append`, to a
//! temporary name renamed into place at the end. A 20 MB attachment is twenty
//! asks of one megabyte, never one 27 MB string.

use sc_api::code_host::FileStoreHost;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::FileHost;
use sc_files::{CONTENT_TYPE_ATTRIBUTE, FILENAME_ATTRIBUTE};
use serde_json::{Map, Value as Json};

use crate::tables::TableDecl;

/// One place a module may keep files: a store, and the folder within it (`""`
/// for the whole store).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileArea {
    /// The store's name.
    pub store: String,
    /// The folder, without leading or trailing slashes.
    pub folder: String,
}

impl FileArea {
    /// Whether the store-relative, normalised `path` lies inside this area.
    fn holds(&self, path: &str) -> bool {
        self.folder.is_empty()
            || path
                .strip_prefix(self.folder.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    fn describe(&self) -> String {
        if self.folder.is_empty() {
            format!("the file store `{}`", self.store)
        } else {
            format!(
                "the folder `{}/` of the file store `{}`",
                self.folder, self.store
            )
        }
    }
}

/// The areas a module's declared file fields name, without repeats.
pub fn file_areas(decls: &[TableDecl]) -> Vec<FileArea> {
    let mut areas: Vec<FileArea> = decls
        .iter()
        .flat_map(|t| t.fields.iter())
        .filter_map(|f| {
            let store = f.file_store.as_deref()?.trim();
            (!store.is_empty()).then(|| FileArea {
                store: store.to_owned(),
                folder: f
                    .file_folder
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('/')
                    .to_owned(),
            })
        })
        .collect();
    areas.sort();
    areas.dedup();
    areas
}

/// The operations a module's `files` may ask for.
const OPS: &[&str] = &["write", "append", "read", "stat", "delete", "rename"];

/// The fields of a request this layer reads; anything else is refused by name
/// before the request reaches the store host, which would refuse it too but in
/// words about code bodies.
const FIELDS: &[&str] = &[
    "op",
    "store",
    "path",
    "encoding",
    "text",
    "base64",
    "overwrite",
    "toPath",
    "offset",
    "length",
    "contentType",
    "filename",
];

/// A module's file surface: [`FileStoreHost`] at admin authority, confined to
/// the module's [`FileArea`]s.
pub struct ModuleFiles<'a> {
    inner: FileStoreHost<'a>,
    catalog: &'a Catalog,
    module: &'a str,
    areas: &'a [FileArea],
}

impl<'a> ModuleFiles<'a> {
    /// The file surface of `module`, over `catalog`'s stores.
    pub fn new(catalog: &'a Catalog, module: &'a str, areas: &'a [FileArea]) -> ModuleFiles<'a> {
        ModuleFiles {
            inner: FileStoreHost::new(catalog),
            catalog,
            module,
            areas,
        }
    }

    /// `path` normalised and inside one of the module's areas of `store`, or a
    /// refusal naming the areas there are.
    fn admit(&self, store: &str, path: &str) -> Result<String> {
        let path = normalise(path)?;
        if self
            .areas
            .iter()
            .any(|a| a.store == store && a.holds(&path))
        {
            return Ok(path);
        }
        let areas: Vec<String> = self.areas.iter().map(FileArea::describe).collect();
        Err(Error::invalid(format!(
            "the module `{}` may keep files only where its declared file fields do ({}), and \
             {path:?} in the file store `{store}` is not there",
            self.module,
            if areas.is_empty() {
                "it declared none".to_owned()
            } else {
                areas.join(", ")
            }
        )))
    }

    /// Set the attributes a `write` or `rename` asked for on the file it left.
    async fn label(
        &self,
        store: &str,
        path: &str,
        owned: bool,
        content_type: Option<String>,
        filename: Option<String>,
    ) -> Result<()> {
        if !owned && content_type.is_none() && filename.is_none() {
            return Ok(());
        }
        let store = self.catalog.require_file_store(store)?;
        let mut meta = store.get_meta(path).await?;
        // The module's own mark, which is no user's id: `check_write` lets only
        // a file's owner (or the admin) attach it to a row, so a user cannot
        // point a row they write at a file the module stored for somebody else
        // and download it through that row (security review #39).
        if owned && meta.owner.is_none() {
            meta.owner = Some(module_owner(self.module));
        }
        if let Some(content_type) = content_type {
            meta.attributes
                .insert(CONTENT_TYPE_ATTRIBUTE.to_owned(), content_type);
        }
        if let Some(filename) = filename {
            meta.attributes
                .insert(FILENAME_ATTRIBUTE.to_owned(), filename);
        }
        store.set_meta(path, &meta).await
    }
}

#[async_trait::async_trait]
impl FileHost for ModuleFiles<'_> {
    async fn files(&self, request: Json) -> Result<Json> {
        let Json::Object(mut req) = request else {
            return Err(Error::invalid("a file request is an object"));
        };
        if let Some(unknown) = req.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            return Err(Error::invalid(format!(
                "`{unknown}` is not something a module's file request may carry"
            )));
        }
        let op = text(&req, "op")?;
        if !OPS.contains(&op.as_str()) {
            return Err(Error::invalid(format!(
                "`{op}` is not a file operation a module may ask for; they are: {}",
                OPS.join(", ")
            )));
        }
        let store = text(&req, "store")?;
        let path = self.admit(&store, &text(&req, "path")?)?;
        let landed = match req.get("toPath") {
            Some(Json::String(to)) if op == "rename" => {
                let to = self.admit(&store, to)?;
                req.insert("toPath".into(), Json::String(to.clone()));
                to
            }
            Some(_) => {
                return Err(Error::invalid(
                    "`toPath` belongs to a `rename`, and names a path as text",
                ));
            }
            None => path.clone(),
        };
        let content_type = optional_text(&mut req, "contentType")?;
        let filename = optional_text(&mut req, "filename")?;
        if (content_type.is_some() || filename.is_some())
            && !matches!(op.as_str(), "write" | "rename")
        {
            return Err(Error::invalid(
                "`contentType` and `filename` are given with a `write` or a `rename`",
            ));
        }
        req.insert("path".into(), Json::String(path));
        req.insert("authority".into(), Json::String("admin".into()));
        let answer = self.inner.files(Json::Object(req)).await?;
        let owned = matches!(op.as_str(), "write" | "append" | "rename");
        self.label(&store, &landed, owned, content_type, filename)
            .await?;
        Ok(answer)
    }

    fn store_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.areas.iter().map(|a| a.store.clone()).collect();
        names.dedup();
        names
    }
}

/// The [`FileMeta::owner`](sc_files::FileMeta::owner) a module's file is
/// recorded with: `module:<name>`, which no user id is.
pub fn module_owner(module: &str) -> String {
    format!("module:{module}")
}

/// A request's text field, or a refusal naming it.
fn text(req: &Map<String, Json>, key: &str) -> Result<String> {
    match req.get(key) {
        Some(Json::String(s)) => Ok(s.clone()),
        _ => Err(Error::invalid(format!(
            "a module's file request names its `{key}` as text"
        ))),
    }
}

/// An optional text field, taken out of the request.
fn optional_text(req: &mut Map<String, Json>, key: &str) -> Result<Option<String>> {
    match req.remove(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s)),
        Some(_) => Err(Error::invalid(format!("`{key}` is text"))),
    }
}

/// A store-relative path, normalised: no `.` or empty segments, and refused
/// when it is absolute, holds a NUL or climbs with `..`. The store host checks
/// again; this copy is what the area check is made against.
fn normalise(path: &str) -> Result<String> {
    if path.contains('\0') {
        return Err(Error::invalid("a file path may not contain a null byte"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(Error::invalid(format!(
            "{path:?} is an absolute path; a file store's paths are relative to its root"
        )));
    }
    let mut parts = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(Error::invalid(format!(
                    "{path:?} leaves the file store: `..` is not a path segment here"
                )));
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(Error::invalid(
            "a module's file request names a file, not the store's root",
        ));
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(store: &str, folder: &str) -> FileArea {
        FileArea {
            store: store.into(),
            folder: folder.into(),
        }
    }

    #[test]
    fn an_area_holds_only_what_is_under_its_folder() {
        let a = area("mail_files", "attachments");
        assert!(a.holds("attachments/1/a.pdf"));
        assert!(!a.holds("attachments"), "the folder itself is not a file");
        assert!(
            !a.holds("attachments-x/a.pdf"),
            "a sibling with the same prefix"
        );
        assert!(!a.holds("other/a.pdf"));
        assert!(area("mail_files", "").holds("anything/at/all"));
    }

    #[test]
    fn paths_are_normalised_and_confined() {
        assert_eq!(normalise("a//b/./c").unwrap(), "a/b/c");
        for bad in ["../x", "a/../../x", "/etc/passwd", "a\0b", "", "./"] {
            assert!(normalise(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn areas_come_from_the_declared_file_fields() {
        let decls = crate::tables::parse_declarations(&[
            serde_json::json!(
                { "name": "a", "fields": [
                    { "name": "f", "file_store": "s", "file_folder": "/x/" },
                    { "name": "g", "file_store": "s", "file_folder": "x" },
                    { "name": "t", "type": "text" },
                ] }
            ),
            serde_json::json!(
                { "name": "b", "fields": [{ "name": "f", "file_store": "t" }] }
            ),
        ])
        .0;
        assert_eq!(file_areas(&decls), vec![area("s", "x"), area("t", "")]);
    }
}
