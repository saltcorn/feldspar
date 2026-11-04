//! Validating a path a `File` field references (design §3.5).
//!
//! A `File` field is a *reference* — a store-relative path — with optional
//! constraints: a folder the path must live under, and a MIME allow-list the
//! file's type must be in. This module is the pure, store-independent half of
//! checking such a path: the caller resolves the field's store (that it is
//! connected at all), and this checks the path *shape* against the field's
//! folder and MIME rules. Keeping it here, beside the [`FileStore`](crate::FileStore)
//! trait, is what lets both the row-write path (`sc-api`) and, later, an
//! application's file endpoints (§4) share one definition of "is this path valid
//! for this field".
//!
//! MIME is derived from the path's extension, not by reading the file: the check
//! runs on a *write*, before any bytes need exist, and the extension is what an
//! allow-list like `["image/png"]` is really about.

use sc_error::{Error, Result};

/// The MIME type inferred from a path's extension (its essence, e.g.
/// `image/png`), or `None` when the extension maps to nothing known.
pub fn mime_for_path(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    mime_guess::from_path(name)
        .first()
        .map(|mime| mime.essence_str().to_owned())
}

/// Whether `mime` is in `allow`, a `File` field's or upload slot's allow-list.
///
/// An entry is an exact type (`image/png`), a whole top-level type
/// (`image/*`, as in an HTML `accept` attribute) or `*/*` (anything with a
/// known type). Matching ignores case and any `;` parameters on an entry. An
/// empty list allows everything; this only answers for a type that is known.
pub fn mime_allowed(mime: &str, allow: &[String]) -> bool {
    let mime = mime.trim().to_ascii_lowercase();
    allow.iter().any(|entry| {
        let entry = entry
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match entry.strip_suffix("/*") {
            Some("*") => true,
            Some(top) => mime
                .split_once('/')
                .is_some_and(|(t, sub)| t == top && !sub.is_empty()),
            None => !entry.is_empty() && entry == mime,
        }
    })
}

/// Check a store-relative `path` against a `File` field's `folder` and
/// `mime_allow` constraints (design §3.5).
///
/// Three ways to fail, each an [`Error::invalid`]:
///
/// - **Unsafe path** — empty, absolute, containing a `..` segment, or holding a
///   `\`, `:` or NUL (Windows separators and drives). A `File`
///   reference is confined to its store, so a path that could escape it is
///   refused here rather than relying on the store driver to sanitise it later.
/// - **Outside the folder** — when the field restricts to a folder, the path must
///   live under it.
/// - **Disallowed MIME** — when the field lists allowed MIME types, the path's
///   inferred type must match one of them ([`mime_allowed`]: `image/*` takes
///   every `image/…` type).
///
/// The store's *resolvability* (that it is connected) is the caller's to check,
/// because only the caller holds the catalog's store registry; this is the part
/// that needs nothing but the field's own rules.
pub fn validate_file_path(path: &str, folder: Option<&str>, mime_allow: &[String]) -> Result<()> {
    if path.is_empty() {
        return Err(Error::invalid("file path is empty"));
    }
    if path.starts_with('/') {
        return Err(Error::invalid(format!(
            "file path `{path}` must be relative to the store, not absolute"
        )));
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err(Error::invalid(format!(
            "file path `{path}` must not contain a `..` segment"
        )));
    }
    // `/` is the only separator a store path has. A `\` is one more on Windows
    // (`uploads/..\..\x`), a `:` starts a drive (`C:\x`) or an NTFS stream, and
    // a segment of dots and spaces is trimmed by Windows to `..` — each a way
    // out of the folder this check is about to compare against. The one
    // validator ([`StorePath`](crate::StorePath)) decides; this keeps its own
    // wording for a field value.
    if crate::StorePath::parse(path).is_err() {
        return Err(Error::invalid(format!(
            "file path `{path}` may only use `/` between plain names"
        )));
    }

    if let Some(folder) = folder
        .map(|f| f.trim_matches('/'))
        .filter(|f| !f.is_empty())
    {
        let prefix = format!("{folder}/");
        if !path.starts_with(&prefix) {
            return Err(Error::invalid(format!(
                "file path `{path}` must be under folder `{folder}`"
            )));
        }
    }

    if !mime_allow.is_empty() {
        let mime = mime_for_path(path);
        let allowed = mime.as_deref().is_some_and(|m| mime_allowed(m, mime_allow));
        if !allowed {
            return Err(Error::invalid(format!(
                "file `{path}` has MIME type `{}`, which is not allowed (allowed: {})",
                mime.as_deref().unwrap_or("unknown"),
                mime_allow.join(", ")
            )));
        }
    }
    Ok(())
}

/// How many suffixed names [`free_path`] tries before giving up.
const MAX_SUFFIX: u32 = 10_000;

/// `path` with `_n` before its extension: `docs/report.pdf` → `docs/report_2.pdf`,
/// `notes` → `notes_2`. A leading dot (`.env`) is a name, not an extension, and
/// only the last extension moves (`a.tar.gz` → `a.tar_2.gz`).
pub(crate) fn suffixed_path(path: &str, n: u32) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(at) => path.split_at(at + 1),
        None => ("", path),
    };
    match name.rfind('.').filter(|&at| at > 0) {
        Some(at) => format!("{dir}{}_{n}{}", &name[..at], &name[at..]),
        None => format!("{dir}{name}_{n}"),
    }
}

/// The first of `path`, `name_1.ext`, `name_2.ext`, … that nothing in `store`
/// occupies — where an upload lands when its field keeps what is there rather
/// than replacing it. Two uploads racing for the same name may still pick the
/// same one; the window is the gap between this check and the write.
pub async fn free_path(
    store: &dyn crate::FileStore,
    path: impl crate::AsStorePath,
) -> Result<String> {
    let path = path.store_path()?;
    if store.stat(&path).await?.is_none() {
        return Ok(path.to_string());
    }
    let path = path.as_str();
    for n in 1..=MAX_SUFFIX {
        let candidate = crate::StorePath::parse(&suffixed_path(path, n))?;
        if store.stat(&candidate).await?.is_none() {
            return Ok(candidate.to_string());
        }
    }
    Err(Error::invalid(format!(
        "`{path}` and its first {MAX_SUFFIX} suffixed names are all taken"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suffix_goes_before_the_last_extension() {
        assert_eq!(suffixed_path("docs/report.pdf", 1), "docs/report_1.pdf");
        assert_eq!(suffixed_path("a.tar.gz", 2), "a.tar_2.gz");
        assert_eq!(suffixed_path("notes", 3), "notes_3");
        assert_eq!(suffixed_path("dir.d/.env", 1), "dir.d/.env_1");
    }

    #[test]
    fn mime_is_inferred_from_the_extension() {
        assert_eq!(mime_for_path("a/b/cover.png").as_deref(), Some("image/png"));
        assert_eq!(mime_for_path("photo.JPG").as_deref(), Some("image/jpeg"));
        assert_eq!(mime_for_path("doc.pdf").as_deref(), Some("application/pdf"));
        assert_eq!(mime_for_path("noext"), None);
    }

    #[test]
    fn an_unsafe_path_is_refused() {
        assert!(validate_file_path("", None, &[]).is_err());
        assert!(validate_file_path("/etc/passwd", None, &[]).is_err());
        assert!(validate_file_path("a/../../etc", None, &[]).is_err());
        // Windows separators, drives, streams and trimmed dots (review #13):
        // each would leave the folder on a Windows server.
        for bad in [
            "covers/..\\..\\Windows\\win.ini",
            "covers/C:\\x",
            "covers/a.png:stream",
            "covers/.. /x",
            "covers/.../x",
            "covers/a\0b",
        ] {
            assert!(
                validate_file_path(bad, Some("covers"), &[]).is_err(),
                "{bad:?}"
            );
        }
        // A plain relative path is fine.
        assert!(validate_file_path("covers/a.png", None, &[]).is_ok());
    }

    #[test]
    fn a_folder_confines_the_path() {
        let folder = Some("covers");
        assert!(validate_file_path("covers/a.png", folder, &[]).is_ok());
        assert!(validate_file_path("covers/sub/a.png", folder, &[]).is_ok());
        let err = validate_file_path("other/a.png", folder, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("covers"), "{err}");
        // A trailing slash on the folder is tolerated.
        assert!(validate_file_path("covers/a.png", Some("covers/"), &[]).is_ok());
    }

    #[test]
    fn a_mime_allow_list_restricts_the_type() {
        let allow = vec!["image/png".to_owned(), "image/jpeg".to_owned()];
        assert!(validate_file_path("a.png", None, &allow).is_ok());
        assert!(validate_file_path("a.jpg", None, &allow).is_ok());
        let err = validate_file_path("a.gif", None, &allow)
            .unwrap_err()
            .to_string();
        assert!(err.contains("image/gif"), "{err}");
        assert!(err.contains("image/png"), "names what is allowed: {err}");
        // An unknown extension against a non-empty allow-list is refused.
        assert!(validate_file_path("a.unknownext", None, &allow).is_err());
    }

    /// `image/*` takes every image type — png, jpeg, webp, gif, svg — and
    /// nothing else (bug report: it refused png).
    #[test]
    fn a_wildcard_allows_a_whole_top_level_type() {
        let allow = vec!["image/*".to_owned()];
        for ok in ["x/dot.png", "a.jpg", "a.webp", "a.gif", "a.svg"] {
            assert!(validate_file_path(ok, None, &allow).is_ok(), "{ok}");
        }
        let err = validate_file_path("x/doc.pdf", None, &allow)
            .unwrap_err()
            .to_string();
        assert!(err.contains("application/pdf"), "{err}");
        assert!(validate_file_path("a.unknownext", None, &allow).is_err());

        assert!(mime_allowed("image/png", &["IMAGE/*".to_owned()]));
        assert!(mime_allowed("video/mp4", &["*/*".to_owned()]));
        assert!(mime_allowed("image/png", &["image/png; q=1".to_owned()]));
        assert!(!mime_allowed("imagex/png", &["image/*".to_owned()]));
        assert!(!mime_allowed("image/png", &["".to_owned()]));
        // An exact list is still exact.
        let exact = vec!["image/png".to_owned(), "image/webp".to_owned()];
        assert!(mime_allowed("image/webp", &exact));
        assert!(!mime_allowed("image/gif", &exact));
    }
}
