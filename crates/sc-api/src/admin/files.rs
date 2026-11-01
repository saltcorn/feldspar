//! The admin API's endpoints: the file manager: browsing, searching, reading and writing files in a
//! store.
//!
//! `sc-server`'s `handlers::files` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Browse one directory of a store. A POST (not GET) so the directory — which
    // may contain `/` and would not fit a single path segment — rides in the body.
    set.register(
        Endpoint::new(
            "browseFiles",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("browse"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "dir",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::array(file_listing_entry_schema()))
        .auth(AuthRequirement::admin()),
    );

    // Find entries anywhere under a directory by **name** — the file manager's
    // search box. Not `searchFiles`: that one reads every text file to find a
    // matching *line*, which is the wrong instrument, and much the wrong cost,
    // for "where did I put invoice-2024.pdf". Nothing is read here; the walk
    // needs names, and names are in the listing.
    //
    // The hits are listing entries rather than paths, so a result can be shown
    // in the same table as a directory, with the same columns, without a request
    // per row.
    set.register(
        Endpoint::new(
            "findFiles",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("find"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("query", TypeSchema::text()),
            StructField::new("dir", TypeSchema::optional(TypeSchema::text())),
            StructField::new("max_results", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("entries", TypeSchema::array(file_listing_entry_schema())),
            StructField::new("truncated", TypeSchema::bool()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The files of a kind in a store, by path — what a setting that names a file
    // (`store_files:png,jpg`, an app icon or a keystore) offers as its choices.
    // Dependency and generated directories are skipped, so a project's
    // `node_modules` does not bury the one icon the admin is looking for.
    set.register(
        Endpoint::new(
            "listStoreFiles",
            Method::Get,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("files-by-type"),
        )
        // Comma-separated, without dots: `png,jpg,jpeg`.
        .query([QueryParam::new("extensions", ValueType::Text)])
        .output(TypeSchema::struct_of([
            StructField::new("paths", TypeSchema::array(TypeSchema::text())),
            StructField::new("truncated", TypeSchema::bool()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Search a store's text files, server-side. This is the endpoint the IDE's
    // find-in-files runs on (§12.1): walking the tree through the filesystem
    // provider is one request per directory, and the same walk done where the
    // bytes are is one request in total. `search_files` (§11.3) runs the same
    // search, so what a person finds in the editor is what a model finds.
    set.register(
        Endpoint::new(
            "searchFiles",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("search"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("pattern", TypeSchema::text()),
            StructField::new("regex", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("case_sensitive", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("whole_word", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("glob", TypeSchema::optional(TypeSchema::text())),
            StructField::new("dir", TypeSchema::optional(TypeSchema::text())),
            StructField::new("max_results", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(file_search_schema())
        .auth(AuthRequirement::admin()),
    );

    // Read one file's bytes (download) — base64 always, plus a UTF-8 `text`
    // shortcut when the contents decode cleanly (for the text editor).
    set.register(
        Endpoint::new(
            "readFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("read"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(file_content_schema())
        .auth(AuthRequirement::admin()),
    );

    // Write one file (upload / save an edited text file). The body carries the
    // contents as either base64 (`base64`) or UTF-8 (`text`); exactly one.
    set.register(
        Endpoint::new(
            "writeFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("write"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("path", TypeSchema::text()),
            StructField::new("base64", TypeSchema::optional(TypeSchema::text())),
            StructField::new("text", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(file_entry_schema())
        .auth(AuthRequirement::admin()),
    );

    // Create a directory (and any missing parents). Idempotent — asking for a
    // directory that exists is a success, since the caller wanted one there.
    set.register(
        Endpoint::new(
            "makeDirectory",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("mkdir"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(file_entry_schema())
        .auth(AuthRequirement::admin()),
    );

    // Delete a file, or a directory and everything in it. Reports whether
    // anything was there, so the caller need not race an existence check.
    set.register(
        Endpoint::new(
            "deleteFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("delete"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Move or rename within the store. Never overwrites an existing destination.
    set.register(
        Endpoint::new(
            "renameFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("rename"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("from", TypeSchema::text()),
            StructField::new("to", TypeSchema::text()),
        ]))
        .output(file_entry_schema())
        .auth(AuthRequirement::admin()),
    );

    // Per-file metadata (design §9): the access rule and the free-form
    // attributes kept beside the bytes rather than in a database row. `min_role`
    // is what the path-cumulative rule is built from, so this is how an admin
    // restricts a folder.
    set.register(
        Endpoint::new(
            "getFileMeta",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("meta"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(file_meta_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "setFileMeta",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("set-meta"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("path", TypeSchema::text()),
            StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
            StructField::new("attributes", TypeSchema::json()),
        ]))
        .output(file_meta_schema())
        .auth(AuthRequirement::admin()),
    );
}

/// One file's metadata (design §9): the access rule and the free-form attributes
/// kept beside the bytes rather than in a database row.
///
/// `effective_min_role` is the *computed* answer — the most restrictive rule on
/// the whole path, including the store's own floor and every parent directory —
/// while `min_role` is only what is set on this entry. The UI needs both: the
/// second is what an admin edits, the first is what actually applies, and
/// showing only the second would let an admin believe a file is public when a
/// parent directory has locked it.
fn file_meta_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("path", TypeSchema::text()),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new(
            "effective_min_role",
            TypeSchema::optional(TypeSchema::int()),
        ),
        StructField::new("attributes", TypeSchema::json()),
    ])
}

/// One entry (file or sub-directory) inside a browsed directory.
fn file_entry_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
        StructField::new("is_dir", TypeSchema::bool()),
        StructField::new("size", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// One entry as a **listing** reports it: the entry, plus the three facts a file
/// manager draws a column for and would otherwise fetch one request per row.
///
/// It is a wider shape than [`file_entry_schema`] deliberately. `writeFile` and
/// `makeDirectory` answer with the entry they just made, where "who owns it" and
/// "what rule reaches it" are the caller's own answers echoed back; a listing is
/// the one place those are news. `owner` is a **label** — the user's email where
/// the account is still there, and the stored id when it is not — because the
/// column is read by a person, and a UUID in it says nothing.
fn file_listing_entry_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
        StructField::new("is_dir", TypeSchema::bool()),
        StructField::new("size", TypeSchema::optional(TypeSchema::int())),
        StructField::new("modified", TypeSchema::optional(TypeSchema::text())),
        StructField::new("owner", TypeSchema::optional(TypeSchema::text())),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new(
            "effective_min_role",
            TypeSchema::optional(TypeSchema::int()),
        ),
    ])
}

/// What a search found: the matching lines, and whether a ceiling cut it short.
///
/// `truncated` is not decoration. A caller that renders results without it tells
/// the reader there is nothing else, which is false exactly when it matters — and
/// it is the difference between "no other uses of this symbol" and "the first
/// hundred uses of this symbol".
fn file_search_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new(
            "matches",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("path", TypeSchema::text()),
                StructField::new("line", TypeSchema::int()),
                StructField::new("column", TypeSchema::int()),
                StructField::new("length", TypeSchema::int()),
                StructField::new("text", TypeSchema::text()),
            ])),
        ),
        StructField::new("files_searched", TypeSchema::int()),
        StructField::new("truncated", TypeSchema::bool()),
    ])
}

/// A file's contents: base64 always, plus decoded `text` when it is valid UTF-8.
fn file_content_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("path", TypeSchema::text()),
        StructField::new("size", TypeSchema::int()),
        StructField::new("base64", TypeSchema::text()),
        StructField::new("text", TypeSchema::optional(TypeSchema::text())),
    ])
}
