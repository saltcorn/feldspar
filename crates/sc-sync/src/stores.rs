//! Which of a local file store's files travel with the structure
//! (APP_SYNC_PROPOSAL §3.5, D6).
//!
//! The repository holds **code and configuration, not content**. Which of a
//! store's directories are which is read off the structure itself:
//!
//! | Reference | Role |
//! |---|---|
//! | an application's framework source (store + source directory) | code |
//! | an application's static directory (store + path) | assets |
//! | an application's upload slot (store + folder) | content |
//! | a `File` field (store + optional folder; none claims the whole store) | content |
//!
//! [`resolve`] turns a store's [`SyncContents`] and those references into its
//! synced directories, the directories left out of them, and the warnings
//! Status shows; [`walk_synced`] reads the files. Both are separate from the
//! export so that the rules can be tested on their own, case by case, without
//! a database.
//!
//! A git store syncs no files here at all: its files are already in a
//! repository, and the structure pins its commit instead.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sc_catalog::{Catalog, DataFieldKind};
use sc_error::{Error, Result};
use sc_files::{FileStoreDef, GIT_BACKEND, SyncContents};

/// A file larger than this is a Status warning: a video committed by accident
/// is in the repository's history for good.
pub const LARGE_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A directory always left out of a store's synced files, wherever it is: a
/// package manager's install tree, rebuilt from the lock file that is synced.
pub const NODE_MODULES: &str = "node_modules";

/// Something that writes user content into a store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContentClaimer {
    /// An application's upload slot.
    Upload {
        /// The application's subdomain.
        application: String,
        /// The slot's name.
        slot: String,
    },
    /// A table's `File` field.
    FileField {
        /// The table.
        table: String,
        /// The field.
        field: String,
    },
}

impl ContentClaimer {
    fn describe(&self) -> String {
        match self {
            ContentClaimer::Upload { application, slot } => {
                format!("the upload slot `{slot}` of the application `{application}`")
            }
            ContentClaimer::FileField { table, field } => {
                format!("the File field `{table}.{field}`")
            }
        }
    }
}

/// A folder of a store that user content is written to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContentClaim {
    /// The folder, relative to the store; empty for the whole store.
    pub folder: String,
    /// What writes there.
    pub by: ContentClaimer,
}

/// How the structure uses one store: every reference that gives one of its
/// directories a role.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreUses {
    /// Code directories: applications' framework sources.
    pub code: BTreeSet<String>,
    /// Asset directories: applications' static directories.
    pub assets: BTreeSet<String>,
    /// Build output directories, always left out: they are rebuilt from the
    /// source.
    pub outputs: BTreeSet<String>,
    /// Where user content is written.
    pub content: BTreeSet<ContentClaim>,
}

/// Something about a store's synced files that Status tells the admin.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum StoreWarning {
    /// An `auto` local store that the structure gives no code or asset
    /// directory: nothing of it is synced, so a file it holds that the
    /// installation needs will be missing on the other instance.
    Unclassified {
        /// The store.
        store: String,
    },
    /// A code or asset directory that user content is also written to: it is
    /// left out, because the rules cannot tell the code from the uploads.
    ContentCoversSynced {
        /// The store.
        store: String,
        /// The code or asset directory left out.
        directory: String,
        /// What writes content over it.
        claim: ContentClaim,
    },
    /// User content written to the whole of an `include` store: it will be
    /// committed with the store's code.
    FolderlessContent {
        /// The store.
        store: String,
        /// What writes there.
        claimer: ContentClaimer,
    },
    /// A synced file larger than [`LARGE_FILE_BYTES`].
    LargeFile {
        /// The store.
        store: String,
        /// The file, relative to the store.
        path: String,
        /// Its size in bytes.
        size: u64,
    },
    /// A file that could not be read, or whose name is not UTF-8, and so is
    /// not in the tree.
    Unreadable {
        /// The store.
        store: String,
        /// What could not be read, and why.
        reason: String,
    },
}

impl StoreWarning {
    /// The store the warning is about.
    pub fn store(&self) -> &str {
        match self {
            StoreWarning::Unclassified { store }
            | StoreWarning::ContentCoversSynced { store, .. }
            | StoreWarning::FolderlessContent { store, .. }
            | StoreWarning::LargeFile { store, .. }
            | StoreWarning::Unreadable { store, .. } => store,
        }
    }

    /// The sentence Status shows.
    pub fn message(&self) -> String {
        match self {
            StoreWarning::Unclassified { store } => format!(
                "File store `{store}` is not synced: no application uses it for code or static \
                 files, so its contents are treated as user data. If the installation needs \
                 its files, set its synced contents to `include`."
            ),
            StoreWarning::ContentCoversSynced {
                store,
                directory,
                claim,
            } => format!(
                "File store `{store}`: {} is left out of the synced files, because {} writes \
                 user content {}. Set the store's synced contents to `include` or `exclude`, or \
                 give the uploads a folder of their own.",
                dir_phrase(directory),
                claim.by.describe(),
                if claim.folder.is_empty() {
                    "anywhere in the store".to_owned()
                } else {
                    format!("into `{}`", claim.folder)
                },
            ),
            StoreWarning::FolderlessContent { store, claimer } => format!(
                "File store `{store}` is synced whole, and {} writes user content into it with \
                 no folder of its own, so those files will be committed too. Give it a folder, \
                 or set the store's synced contents to `auto`.",
                claimer.describe()
            ),
            StoreWarning::LargeFile { store, path, size } => format!(
                "File store `{store}`: `{path}` is {:.1} MB and will be committed. Leave it out \
                 with a .gitignore entry if it is not part of the application.",
                megabytes(*size)
            ),
            StoreWarning::Unreadable { store, reason } => {
                format!("File store `{store}`: {reason}, so it is not synced.")
            }
        }
    }
}

fn dir_phrase(directory: &str) -> String {
    if directory.is_empty() {
        "the whole store".to_owned()
    } else {
        format!("the directory `{directory}`")
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal place of a megabyte"
)]
fn megabytes(size: u64) -> f64 {
    size as f64 / (1024.0 * 1024.0)
}

/// What [`resolve`] decided for one store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncedDirs {
    /// The store's name.
    pub store: String,
    /// Its setting.
    pub contents: SyncContents,
    /// The directories whose files are synced, relative to the store (empty
    /// for the whole store). None of them lies inside another.
    pub synced: Vec<String>,
    /// Directories inside them that are left out: content folders and build
    /// output. `node_modules`, `.git` and what `.gitignore` ignores are left out
    /// too, by [`walk_synced`].
    pub excluded: Vec<String>,
    /// What Status says about it.
    pub warnings: Vec<StoreWarning>,
}

/// A directory reference as the store-relative path it names: `/`-separated,
/// with no leading `./` or `/` and no trailing `/`; empty for the store's root.
pub fn normalise_dir(dir: &str) -> String {
    let dir = dir.trim().replace('\\', "/");
    dir.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `outer` is `inner` or one of its ancestors.
pub fn covers(outer: &str, inner: &str) -> bool {
    outer.is_empty()
        || inner == outer
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Decide a store's synced directories from its setting and how the structure
/// uses it.
pub fn resolve(def: &FileStoreDef, uses: &StoreUses) -> Result<SyncedDirs> {
    let contents = def.sync_contents()?;
    let store = def.name.clone();
    let mut out = SyncedDirs {
        store: store.clone(),
        contents,
        ..SyncedDirs::default()
    };
    if def.backend == GIT_BACKEND || contents == SyncContents::Exclude {
        return Ok(out);
    }
    let outputs: Vec<String> = uses.outputs.iter().map(|d| normalise_dir(d)).collect();
    let claims: Vec<ContentClaim> = uses
        .content
        .iter()
        .map(|c| ContentClaim {
            folder: normalise_dir(&c.folder),
            by: c.by.clone(),
        })
        .collect();

    let synced: Vec<String> = match contents {
        SyncContents::Include => {
            for claim in claims.iter().filter(|c| c.folder.is_empty()) {
                out.warnings.push(StoreWarning::FolderlessContent {
                    store: store.clone(),
                    claimer: claim.by.clone(),
                });
            }
            vec![String::new()]
        }
        SyncContents::Auto => {
            let candidates: BTreeSet<String> = uses
                .code
                .iter()
                .chain(&uses.assets)
                .map(|d| normalise_dir(d))
                .collect();
            if candidates.is_empty() {
                out.warnings.push(StoreWarning::Unclassified {
                    store: store.clone(),
                });
            }
            let mut kept = Vec::new();
            for dir in candidates {
                // A claim over the directory (or the whole store) means the
                // rules cannot tell this directory's code from its uploads.
                let over = claims.iter().find(|c| covers(&c.folder, &dir));
                match over {
                    Some(claim) => out.warnings.push(StoreWarning::ContentCoversSynced {
                        store: store.clone(),
                        directory: dir,
                        claim: claim.clone(),
                    }),
                    None => kept.push(dir),
                }
            }
            kept
        }
        SyncContents::Exclude => Vec::new(),
    };
    // Outermost only: a source directory inside a static one is synced by it.
    out.synced = synced
        .iter()
        .filter(|d| !synced.iter().any(|o| o != *d && covers(o, d)))
        .cloned()
        .collect();
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for dir in claims
        .iter()
        .map(|c| &c.folder)
        .filter(|f| !f.is_empty())
        .chain(outputs.iter().filter(|o| !o.is_empty()))
    {
        if out.synced.iter().any(|s| covers(s, dir)) {
            excluded.insert(dir.clone());
        }
    }
    out.excluded = excluded.into_iter().collect();
    out.warnings.sort();
    Ok(out)
}

/// How every store is used, read off the structure: applications' sources,
/// static directories, build outputs and upload slots, and tables' `File`
/// fields.
pub async fn store_uses(catalog: &Catalog) -> Result<BTreeMap<String, StoreUses>> {
    let mut uses: BTreeMap<String, StoreUses> = BTreeMap::new();
    if catalog.get(sc_app::APPLICATIONS_TABLE)?.is_some() {
        for app in sc_app::list_applications(catalog).await? {
            match sc_app::app_source_from_config(&app.framework) {
                Ok(source) => {
                    let entry = uses.entry(source.store.0.clone()).or_default();
                    entry.code.insert(normalise_dir(&source.build.source_dir));
                    entry
                        .outputs
                        .insert(normalise_dir(&source.build.output_dir));
                }
                Err(_) => {
                    if let Some((store, dir)) = sc_app::none_source_dir(&app.framework) {
                        uses.entry(store)
                            .or_default()
                            .code
                            .insert(normalise_dir(&dir));
                    }
                }
            }
            for dir in &app.static_dirs {
                uses.entry(dir.store.0.clone())
                    .or_default()
                    .assets
                    .insert(normalise_dir(&dir.path));
            }
            for slot in &app.uploads {
                uses.entry(slot.store.clone())
                    .or_default()
                    .content
                    .insert(ContentClaim {
                        folder: normalise_dir(&slot.folder),
                        by: ContentClaimer::Upload {
                            application: app.subdomain.clone(),
                            slot: slot.name.clone(),
                        },
                    });
            }
        }
    }
    for table in catalog.tables()? {
        for field in &table.fields {
            if let DataFieldKind::File { store, folder, .. } = &field.kind {
                uses.entry(store.0.clone())
                    .or_default()
                    .content
                    .insert(ContentClaim {
                        folder: normalise_dir(folder.as_deref().unwrap_or("")),
                        by: ContentClaimer::FileField {
                            table: table.name.clone(),
                            field: field.base.name.clone(),
                        },
                    });
            }
        }
    }
    Ok(uses)
}

/// One synced file: its path relative to the store, and its bytes.
pub type SyncedFile = (String, Vec<u8>);

/// Read the synced files of a local store rooted at `root`.
///
/// Walks the store from its root, so a `.gitignore` anywhere in it applies the
/// way git applies it, and descends only into the synced directories (and the
/// directories above them). Left out wherever they are: `.git`,
/// [`NODE_MODULES`], [`SyncedDirs::excluded`], and what a `.gitignore` ignores.
/// Symbolic links are not followed and not synced: one could name a file
/// outside the store. Files come back in path order; the warnings are the
/// large and unreadable files.
pub fn walk_synced(root: &Path, dirs: &SyncedDirs) -> (Vec<SyncedFile>, Vec<StoreWarning>) {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut warnings = Vec::new();
    if dirs.synced.is_empty() {
        return (Vec::new(), warnings);
    }
    if !root.is_dir() {
        warnings.push(StoreWarning::Unreadable {
            store: dirs.store.clone(),
            reason: format!("its directory {} is not there", root.display()),
        });
        return (Vec::new(), warnings);
    }
    let walk_root = root.to_path_buf();
    let synced = dirs.synced.clone();
    let excluded = dirs.excluded.clone();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .git_ignore(true)
        .require_git(false)
        .parents(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let Some(rel) = relative(&walk_root, entry.path()) else {
                return true;
            };
            if rel.is_empty() {
                return true;
            }
            let name = entry.file_name();
            if name == ".git" || name == NODE_MODULES {
                return false;
            }
            if excluded.iter().any(|e| covers(e, &rel)) {
                return false;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            // Inside a synced directory, or a directory on the way to one.
            synced.iter().any(|s| covers(s, &rel))
                || (is_dir && synced.iter().any(|s| covers(&rel, s)))
        })
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warnings.push(StoreWarning::Unreadable {
                    store: dirs.store.clone(),
                    reason: format!("a file could not be listed ({e})"),
                });
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(rel) = relative(root, entry.path()) else {
            warnings.push(StoreWarning::Unreadable {
                store: dirs.store.clone(),
                reason: format!("{} has a name that is not UTF-8", entry.path().display()),
            });
            continue;
        };
        if !dirs.synced.iter().any(|s| covers(s, &rel)) {
            continue;
        }
        match std::fs::read(entry.path()) {
            Ok(bytes) => {
                let size = bytes.len() as u64;
                if size > LARGE_FILE_BYTES {
                    warnings.push(StoreWarning::LargeFile {
                        store: dirs.store.clone(),
                        path: rel.clone(),
                        size,
                    });
                }
                files.insert(rel, bytes);
            }
            Err(e) => warnings.push(StoreWarning::Unreadable {
                store: dirs.store.clone(),
                reason: format!("`{rel}` could not be read ({e})"),
            }),
        }
    }
    (files.into_iter().collect(), warnings)
}

/// `path` relative to `root`, `/`-separated; `None` when it is not under the
/// root or its name is not UTF-8.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for part in rel.components() {
        parts.push(part.as_os_str().to_str()?.to_owned());
    }
    Some(parts.join("/"))
}

/// The error for a store that the export cannot read the directory of.
pub(crate) fn no_directory(def: &FileStoreDef) -> Error {
    Error::invalid(format!(
        "local file store `{}` has no directory setting",
        def.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_files::FileStoreDef;

    fn upload(folder: &str) -> ContentClaim {
        ContentClaim {
            folder: folder.to_owned(),
            by: ContentClaimer::Upload {
                application: "helpdesk".into(),
                slot: "attachments".into(),
            },
        }
    }

    fn file_field(folder: &str) -> ContentClaim {
        ContentClaim {
            folder: folder.to_owned(),
            by: ContentClaimer::FileField {
                table: "tickets".into(),
                field: "attachment".into(),
            },
        }
    }

    fn uses(code: &[&str], assets: &[&str], content: &[ContentClaim]) -> StoreUses {
        StoreUses {
            code: code.iter().map(|s| (*s).to_owned()).collect(),
            assets: assets.iter().map(|s| (*s).to_owned()).collect(),
            outputs: BTreeSet::new(),
            content: content.iter().cloned().collect(),
        }
    }

    fn store(contents: Option<SyncContents>) -> FileStoreDef {
        let def = FileStoreDef::local("s", "/srv/s");
        match contents {
            Some(c) => def.with_sync_contents(c),
            None => def,
        }
    }

    #[test]
    fn auto_syncs_the_code_and_asset_directories() {
        let mut u = uses(&["./web/"], &["public"], &[]);
        u.outputs.insert("web/dist".into());
        let dirs = resolve(&store(None), &u).unwrap();
        assert_eq!(dirs.contents, SyncContents::Auto);
        assert_eq!(dirs.synced, ["public", "web"]);
        // The build output is rebuilt from the source, never synced.
        assert_eq!(dirs.excluded, ["web/dist"]);
        assert!(dirs.warnings.is_empty());
    }

    #[test]
    fn a_code_directory_at_the_root_syncs_the_store_and_nests_the_rest() {
        let dirs = resolve(&store(None), &uses(&[""], &["public"], &[])).unwrap();
        assert_eq!(dirs.synced, [""]);
    }

    #[test]
    fn an_upload_folder_nested_in_a_static_directory_is_left_out() {
        let dirs = resolve(
            &store(None),
            &uses(&[], &["public"], &[upload("public/uploads/")]),
        )
        .unwrap();
        assert_eq!(dirs.synced, ["public"]);
        assert_eq!(dirs.excluded, ["public/uploads"]);
        assert!(dirs.warnings.is_empty());
    }

    #[test]
    fn a_content_claim_over_a_synced_directory_leaves_it_out_and_warns() {
        // A File field with no folder claims the whole store, so the code in it
        // cannot be told from the uploads.
        let dirs = resolve(
            &store(None),
            &uses(&["web"], &["public"], &[file_field("")]),
        )
        .unwrap();
        assert!(dirs.synced.is_empty());
        assert_eq!(dirs.warnings.len(), 2);
        assert!(matches!(
            &dirs.warnings[0],
            StoreWarning::ContentCoversSynced { directory, claim, .. }
                if directory == "public" && claim.folder.is_empty()
        ));
        let message = dirs.warnings[1].message();
        assert!(
            message.contains("`web`") && message.contains("tickets.attachment"),
            "{message}"
        );
        assert!(message.contains("anywhere in the store"), "{message}");

        // A claim over exactly the directory does the same; one beside it does
        // not.
        let dirs = resolve(&store(None), &uses(&[], &["media"], &[upload("media")])).unwrap();
        assert!(dirs.synced.is_empty());
        assert_eq!(dirs.warnings.len(), 1);
        let dirs = resolve(
            &store(None),
            &uses(&[], &["media"], &[upload("mediafiles")]),
        )
        .unwrap();
        assert_eq!(dirs.synced, ["media"]);
        assert!(dirs.warnings.is_empty());
    }

    #[test]
    fn an_unused_auto_store_is_unclassified_and_syncs_nothing() {
        let dirs = resolve(&store(None), &uses(&[], &[], &[upload("in")])).unwrap();
        assert!(dirs.synced.is_empty());
        assert_eq!(
            dirs.warnings,
            [StoreWarning::Unclassified { store: "s".into() }]
        );
        assert!(dirs.warnings[0].message().contains("not synced"));
    }

    #[test]
    fn include_syncs_the_whole_store_less_the_content_folders() {
        let dirs = resolve(
            &store(Some(SyncContents::Include)),
            &uses(
                &[],
                &[],
                &[upload("uploads"), file_field("docs/attachments")],
            ),
        )
        .unwrap();
        assert_eq!(dirs.synced, [""]);
        assert_eq!(dirs.excluded, ["docs/attachments", "uploads"]);
        assert!(dirs.warnings.is_empty());
    }

    #[test]
    fn a_folderless_file_field_on_an_included_store_warns() {
        let dirs = resolve(
            &store(Some(SyncContents::Include)),
            &uses(&[], &[], &[file_field("")]),
        )
        .unwrap();
        assert_eq!(dirs.synced, [""]);
        assert_eq!(dirs.warnings.len(), 1);
        let message = dirs.warnings[0].message();
        assert!(
            message.contains("File field `tickets.attachment`"),
            "{message}"
        );
        assert!(message.contains("committed"), "{message}");
    }

    #[test]
    fn exclude_and_git_stores_sync_nothing() {
        let all = uses(&["web"], &["public"], &[]);
        let dirs = resolve(&store(Some(SyncContents::Exclude)), &all).unwrap();
        assert!(dirs.synced.is_empty() && dirs.warnings.is_empty());
        let git = FileStoreDef::git("web", "git@github.com:o/r.git");
        let dirs = resolve(&git, &all).unwrap();
        assert!(dirs.synced.is_empty() && dirs.warnings.is_empty());
    }

    #[test]
    fn directories_are_normalised_and_covering_is_by_segment() {
        assert_eq!(normalise_dir(" ./a//b/ "), "a/b");
        assert_eq!(normalise_dir("/"), "");
        assert_eq!(normalise_dir("a\\b"), "a/b");
        assert!(covers("", "a"));
        assert!(covers("a", "a/b"));
        assert!(covers("a", "a"));
        assert!(!covers("a", "ab"));
        assert!(!covers("a/b", "a"));
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-sync-walk-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn the_walk_leaves_out_what_git_and_the_rules_leave_out() {
        let root = temp_dir("rules");
        put(&root, ".gitignore", b"*.log\n/web/.env\n");
        put(&root, "web/index.ts", b"export {}\n");
        put(&root, "web/.env", b"SECRET=1\n");
        put(&root, "web/debug.log", b"noise\n");
        put(&root, "web/node_modules/x/index.js", b"dep\n");
        put(&root, "web/dist/bundle.js", b"built\n");
        put(&root, "public/logo.png", &[0x89, b'P']);
        put(&root, "public/uploads/user.jpg", b"theirs");
        put(&root, "notes/private.txt", b"not synced");
        put(&root, ".git/config", b"[core]\n");

        let dirs = SyncedDirs {
            store: "s".into(),
            contents: SyncContents::Auto,
            synced: vec!["public".into(), "web".into()],
            excluded: vec!["public/uploads".into(), "web/dist".into()],
            warnings: Vec::new(),
        };
        let (files, warnings) = walk_synced(&root, &dirs);
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["public/logo.png", "web/index.ts"]);
        assert!(warnings.is_empty(), "{warnings:?}");

        // The whole store: the root's `.gitignore` is a file like any other.
        let whole = SyncedDirs {
            synced: vec![String::new()],
            excluded: Vec::new(),
            ..dirs
        };
        let (files, _) = walk_synced(&root, &whole);
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                ".gitignore",
                "notes/private.txt",
                "public/logo.png",
                "public/uploads/user.jpg",
                "web/dist/bundle.js",
                "web/index.ts",
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_large_file_is_synced_with_a_warning() {
        let root = temp_dir("large");
        let big = vec![0u8; usize::try_from(LARGE_FILE_BYTES).unwrap() + 1];
        put(&root, "public/video.mp4", &big);
        let dirs = SyncedDirs {
            store: "s".into(),
            synced: vec!["public".into()],
            ..SyncedDirs::default()
        };
        let (files, warnings) = walk_synced(&root, &dirs);
        assert_eq!(files.len(), 1);
        assert_eq!(warnings.len(), 1);
        let message = warnings[0].message();
        assert!(
            message.contains("public/video.mp4") && message.contains("10.0 MB"),
            "{message}"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
