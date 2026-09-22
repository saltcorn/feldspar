//! `list_assets` — the URLs of the files the application serves statically
//! (TODO "Static directories" §6).
//!
//! An application's images do not live in the code. The logo is in a file store,
//! the screenshots are in another directory of it, and the page that shows them
//! is in the source tree the agent is editing — so `find_files` cannot answer
//! "what images may I use?", because it walks the **coding scope** and the
//! images are not in it.
//!
//! And it could not answer it even if they were, for a second reason that
//! matters more: a path is not a URL. A model handed `media/hero.png` will
//! invent `/media/hero.png`, or `/files/serve/Assets/media/hero.png`, or
//! whatever it saw last, and the page will 404. So this tool walks the
//! **application's** static directories and returns, for each file, the URL the
//! router actually answers — built by [`sc_app::StaticDir::url_for`], the
//! inverse of the `resolve` the router serves through, so the two cannot drift.
//!
//! The URL is **app-root-relative** (`/img/hero.png`): it is what belongs in the
//! JSX, because an absolute URL baked into a component follows the application
//! from `localhost:3000` to production as a broken link.
//!
//! **A mount is not a grant.** Listing goes through the same
//! [`check_access`](sc_files::check_access) as every other reader, as the run's
//! caller, so a file a guest may not read is not named to a run a guest started
//! — the same rule the router applies when it refuses to serve it.
//!
//! **Reading only.** Uploading an image through the agent is a grant an admin
//! gives on purpose, and this is not it.

use sc_agent::TraitContext;
use sc_app::{Application, StaticDir, asset_content_type, load_application_by_subdomain};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_files::{DEFAULT_EXCLUDED_DIRS, DEFAULT_MAX_RESULTS, glob_matches, walk_store};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::check::configured_application;
use super::search::CFG_MAX_RESULTS;
use crate::files::{FileScope, config_count, optional_string_arg};
use crate::table::arguments;

/// The glob, matched as `find_files` matches one.
const ARG_PATTERN: &str = "pattern";
/// A directory within each mount to look under.
const ARG_DIR: &str = "dir";

/// The tool one configured scope offers.
pub fn tool_name(scope: &FileScope) -> String {
    format!("list_assets_{}", scope.slug())
}

/// Whether this configuration offers the tool at all: it names an application,
/// and an application is where the static directories are.
pub fn offered(config: &Attrs) -> bool {
    configured_application(config).is_some()
}

/// The tool this scope's asset lister contributes.
pub fn spec(scope: &FileScope, config: &Attrs) -> ToolSpec {
    let application = configured_application(config).unwrap_or_default();
    let ceiling =
        config_count(config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64).unwrap_or(u64::MAX);
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Images `{application}` serves, newest first, at most {ceiling}. Put each `url` \
             in the page as given."
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATTERN: {"type": "string", "description": "`*.png`, or `icons/**`"},
                ARG_DIR: {"type": "string", "description": "Only under this directory"},
            },
            "additionalProperties": false,
        }),
    )
}

/// One static directory as a scope over the store it lives in, so the walk, the
/// floor and the access check are the ones every other file tool uses.
fn dir_scope(dir: &StaticDir) -> FileScope {
    FileScope {
        store: dir.store.0.clone(),
        root: dir
            .path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect::<Vec<_>>()
            .join("/"),
    }
}

/// The application the `coding` trait names, by subdomain.
async fn application(catalog: &Catalog, config: &Attrs) -> Result<Application> {
    let subdomain = configured_application(config).ok_or_else(|| {
        Error::invalid(format!(
            "list_assets needs the `coding` trait's `{}` setting",
            crate::CFG_APPLICATION
        ))
    })?;
    load_application_by_subdomain(catalog, &subdomain)
        .await?
        .ok_or_else(|| Error::invalid(format!("no application is served at `{subdomain}`")))
}

/// One asset: what it is, and the URL that serves it.
struct Asset {
    url: String,
    modified: Option<String>,
    json: Json,
}

/// The assets an application serves, as the run's caller may read them.
pub async fn call(config: &Attrs, args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let ceiling = config_count(config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64)? as usize;
    let args = arguments(args, &[ARG_PATTERN, ARG_DIR])?;
    let pattern = optional_string_arg(&args, ARG_PATTERN)?;
    let rel_dir = optional_string_arg(&args, ARG_DIR)?;
    let app = application(ctx.catalog, config).await?;

    let mut found: Vec<Asset> = Vec::new();
    let mut walk_stopped = false;
    for dir in &app.static_dirs {
        // The declared subset is the whole truth about which stores an
        // application touches (§13.2), and the router refuses one outside it —
        // so a tool that named its files would be describing a 404.
        if !app.can_access_file_store(&dir.store) {
            continue;
        }
        let scope = dir_scope(dir);
        // A store that is not connected, a directory that is not there and a
        // directory this caller may not open are all "nothing here": one mount
        // must not fail the listing of the others.
        let Ok((store, floor)) = scope.connect(ctx.catalog).await else {
            continue;
        };
        let Ok(under) = scope.resolve(&rel_dir) else {
            continue;
        };
        if sc_files::check_access(store.as_ref(), floor, &under, ctx.caller.role)
            .await
            .is_err()
        {
            continue;
        }
        let Ok(walked) = walk_store(
            store.as_ref(),
            floor,
            ctx.caller.role,
            &under,
            &DEFAULT_EXCLUDED_DIRS,
        )
        .await
        else {
            continue;
        };
        walk_stopped |= walked.truncated;

        let prefix = match under.is_empty() {
            true => String::new(),
            false => format!("{under}/"),
        };
        for entry in walked.entries {
            if entry.is_dir {
                continue;
            }
            let within = entry.path.strip_prefix(&prefix).unwrap_or(&entry.path);
            if !glob_matches(&pattern, within, &entry.name) {
                continue;
            }
            let Some(url) = dir.url_for(&entry.path) else {
                continue;
            };
            let mut row = serde_json::Map::new();
            row.insert("url".to_owned(), json!(url));
            row.insert("path".to_owned(), json!(entry.path));
            row.insert("store".to_owned(), json!(dir.store.0));
            row.insert(
                "content_type".to_owned(),
                json!(asset_content_type(&entry.path)),
            );
            if let Some(size) = entry.size {
                row.insert("size".to_owned(), json!(size));
            }
            if let Some(modified) = &entry.modified {
                row.insert("modified".to_owned(), json!(modified));
            }
            found.push(Asset {
                url,
                modified: entry.modified,
                json: Json::Object(row),
            });
        }
    }

    // Newest first; an asset whose backend records no time last; then by URL, so
    // the order is stable.
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.url.cmp(&b.url)));
    let total = found.len();
    let assets: Vec<Json> = found.into_iter().take(ceiling).map(|a| a.json).collect();
    let truncated = total > ceiling || walk_stopped;

    let mut out = serde_json::Map::new();
    out.insert("assets".to_owned(), Json::Array(assets));
    out.insert("truncated".to_owned(), json!(truncated));
    if total > ceiling {
        out.insert(
            "note".to_owned(),
            json!(format!(
                "{ceiling} of {total} assets shown. Narrow the list with `{ARG_PATTERN}` or \
                 `{ARG_DIR}`."
            )),
        );
    } else if walk_stopped {
        out.insert(
            "note".to_owned(),
            json!(format!(
                "The walk stopped after {} entries; the list is incomplete.",
                sc_files::MAX_FILES_SCANNED
            )),
        );
    } else if total == 0 && app.static_dirs.is_empty() {
        out.insert(
            "note".to_owned(),
            json!(format!(
                "`{}` has no static directories; an administrator adds one to the \
                 application to serve images from a file store.",
                app.subdomain
            )),
        );
    }
    Ok(Json::Object(out))
}

/// One line per static directory for the session header (§6), or `None` when
/// the application has none, names none this caller can read, or cannot be
/// read at all.
///
/// A model that does not know the tool exists will not call it, and finding out
/// costs a turn and the admin's money — so the mounts are named up front,
/// beside `AGENTS.md` and the repo map. **Nothing here fails a run**: every step
/// that can go wrong leaves the section out and is logged, as the rest of the
/// header does.
pub async fn header(config: &Attrs, catalog: &Catalog, role: u8) -> Option<String> {
    let app = match application(catalog, config).await {
        Ok(app) => app,
        Err(e) => {
            sc_log::log_verbose!("session header: the application's assets: {e}");
            return None;
        }
    };
    let mut lines = Vec::new();
    for dir in &app.static_dirs {
        if !app.can_access_file_store(&dir.store) {
            continue;
        }
        let scope = dir_scope(dir);
        let Ok((store, floor)) = scope.connect(catalog).await else {
            continue;
        };
        let Ok(root) = scope.resolve("") else {
            continue;
        };
        if sc_files::check_access(store.as_ref(), floor, &root, role)
            .await
            .is_err()
        {
            continue;
        }
        let Ok(walked) =
            walk_store(store.as_ref(), floor, role, &root, &DEFAULT_EXCLUDED_DIRS).await
        else {
            continue;
        };
        let count = walked.entries.iter().filter(|e| !e.is_dir).count();
        let within = match dir.path.trim_matches('/') {
            "" => String::new(),
            path => format!(" ({path})"),
        };
        let files = match count {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        lines.push(format!(
            "{} \u{2192} store \"{}\"{within}, {files}",
            dir.mount, dir.store.0
        ));
    }
    (!lines.is_empty()).then(|| {
        format!(
            "<static_dirs>\nThese URLs are served by `{}`; use them as they are.\n{}\n\
             </static_dirs>",
            app.subdomain,
            lines.join("\n")
        )
    })
}
