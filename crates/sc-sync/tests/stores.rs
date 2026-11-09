//! File stores in the tree (S1.7, S1.8; APP_SYNC_PROPOSAL §3.5): a local
//! store's code and asset directories are exported byte for byte and its user
//! content is not; a git store is exported as its URL, branch and pinned
//! commit, with none of its files and none of this instance's settings.

use serde_json::json;

use sc_api::schema_edit::{Operation, TableSettings};
use sc_error::Result;
use sc_files::{CFG_BRANCH, CFG_DIR, CFG_KEY_PATH, CFG_PUBLIC_KEY, CFG_PUBLIC_URL, SyncContents};
use sc_sync::StoreWarning;

use crate::fixture::{Installation, code_app, entity, file_field, git, put};

#[tokio::test]
async fn only_the_code_and_asset_stores_files_are_exported() -> Result<()> {
    let inst = Installation::new().await?;

    // A code store: an application's source, with its build output, its
    // dependencies and an ignored file beside it.
    let (_, code) = inst.local_store("code", |d| d).await?;
    put(&code, ".gitignore", b"*.log\n");
    put(&code, "web/index.ts", b"export {}\n");
    put(&code, "web/debug.log", b"noise\n");
    put(&code, "web/dist/bundle.js", b"built\n");
    put(&code, "web/node_modules/dep/index.js", b"dep\n");
    put(&code, "README.md", b"outside the source directory\n");
    // An asset store: a static directory with an upload folder inside it.
    let (_, assets) = inst.local_store("assets", |d| d).await?;
    put(
        &assets,
        "public/logo.png",
        &[0x89, b'P', b'N', b'G', 0, 1, 2],
    );
    put(&assets, "public/uploads/user-photo.jpg", b"theirs");
    put(&assets, "drafts/unused.txt", b"not referenced");
    // An upload store: only a `File` field writes to it.
    let (_, uploads) = inst.local_store("uploads", |d| d).await?;
    put(&uploads, "attachments/invoice.pdf", b"%PDF user content");

    inst.schema(vec![Operation::CreateTable {
        name: "tickets".into(),
        database: String::new(),
        settings: TableSettings::default(),
        fields: vec![
            crate::fixture::text_field("title"),
            file_field("attachment", "uploads", Some("attachments")),
        ],
    }])
    .await?;
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["file_stores"] = json!(["code", "assets"]);
    body["static_dirs"] = json!([{ "mount": "/static", "store": "assets", "path": "public" }]);
    body["uploads"] = json!([{
        "name": "photos", "store": "assets", "folder": "public/uploads",
        "min_role": 80, "max_bytes": 1_000_000,
    }]);
    inst.application(body).await?;

    let export = inst.export().await?;
    let files: Vec<&str> = export
        .tree
        .paths()
        .filter(|p| p.starts_with("file-stores/") && p.contains("/files/"))
        .collect();
    assert_eq!(
        files,
        [
            "file-stores/assets/files/public/logo.png",
            "file-stores/code/files/web/index.ts",
        ]
    );
    // Byte for byte.
    assert_eq!(
        export
            .tree
            .get("file-stores/assets/files/public/logo.png")
            .unwrap()
            .render(),
        [0x89, b'P', b'N', b'G', 0, 1, 2]
    );
    // Every store's definition is there, the upload store's too.
    for store in ["assets", "code", "uploads"] {
        let def = entity(&export.tree, &format!("file-stores/{store}.json"));
        assert_eq!(def["sync_contents"], json!("auto"), "{def}");
        assert!(def["config"].get("path").is_none(), "{def}");
    }
    // The upload store is used for nothing but content, and Status says so.
    assert_eq!(
        export.warnings,
        [StoreWarning::Unclassified {
            store: "uploads".into()
        }]
    );

    // `include` on the upload store syncs it whole, less the field's folder.
    let def = sc_catalog::load_file_store_by_name(&inst.catalog, "uploads")
        .await?
        .unwrap()
        .with_sync_contents(SyncContents::Include);
    put(&uploads, "templates/invoice.html", b"<p>{{total}}</p>");
    sc_catalog::save_file_store(&inst.catalog, &def).await?;
    let export = inst.export().await?;
    assert!(
        export
            .tree
            .get("file-stores/uploads/files/templates/invoice.html")
            .is_some()
    );
    assert!(
        export
            .tree
            .get("file-stores/uploads/files/attachments/invoice.pdf")
            .is_none()
    );
    assert_eq!(
        entity(&export.tree, "file-stores/uploads.json")["sync_contents"],
        json!("include")
    );
    assert!(export.warnings.is_empty(), "{:?}", export.warnings);
    Ok(())
}

#[tokio::test]
async fn a_git_store_is_its_url_branch_and_pinned_commit() -> Result<()> {
    let inst = Installation::new().await?;
    let checkout = inst.dir.join("web-checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q"]);
    put(&checkout, "index.html", b"<h1>v1</h1>");
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-q", "-m", "v1"]);
    let first = git(&checkout, &["rev-parse", "HEAD"]);

    let def = sc_files::FileStoreDef::git("web", "git@github.com:acme/web.git")
        .with(CFG_DIR, checkout.to_string_lossy())
        .with(CFG_BRANCH, "main")
        .with(CFG_PUBLIC_URL, "https://mirror.example.com/acme/web.git")
        .with(CFG_KEY_PATH, "/var/lib/feldspar/keys/web")
        .with(CFG_PUBLIC_KEY, "ssh-ed25519 AAAA web");
    sc_catalog::save_file_store(&inst.catalog, &def).await?;
    // An application serves it, so it is not merely an unused store.
    let mut body = code_app("site", "web", "", "dist");
    body["file_stores"] = json!(["web"]);
    inst.application(body).await?;

    let export = inst.export().await?;
    let web = entity(&export.tree, "file-stores/web.json");
    assert_eq!(web["backend"], json!("git"));
    assert_eq!(web["config"]["url"], json!("git@github.com:acme/web.git"));
    assert_eq!(
        web["config"]["public_url"],
        json!("https://mirror.example.com/acme/web.git")
    );
    assert_eq!(web["config"]["branch"], json!("main"));
    assert_eq!(web["commit"], json!(first));
    // None of this instance's own settings.
    for key in [CFG_KEY_PATH, CFG_PUBLIC_KEY, CFG_DIR] {
        assert!(web["config"].get(key).is_none(), "{key} in {web}");
    }
    assert!(!crate::fixture::all_text(&export.tree).contains("web-checkout"));
    // And none of its files.
    assert!(
        !export
            .tree
            .paths()
            .any(|p| p.starts_with("file-stores/web/")),
        "{:?}",
        export.tree.paths().collect::<Vec<_>>()
    );
    assert!(export.warnings.is_empty(), "{:?}", export.warnings);

    // The pin follows the checked-out commit.
    put(&checkout, "index.html", b"<h1>v2</h1>");
    git(&checkout, &["commit", "-q", "-am", "v2"]);
    let second = git(&checkout, &["rev-parse", "HEAD"]);
    assert_ne!(first, second);
    let export = inst.export().await?;
    assert_eq!(
        entity(&export.tree, "file-stores/web.json")["commit"],
        json!(second)
    );
    git(&checkout, &["checkout", "-q", &first]);
    let export = inst.export().await?;
    assert_eq!(
        entity(&export.tree, "file-stores/web.json")["commit"],
        json!(first)
    );
    Ok(())
}
