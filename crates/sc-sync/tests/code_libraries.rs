//! Code libraries in the tree: an instance-wide library is
//! `code-libraries/<name>.json`, an application's own is
//! `applications/<app>/code-libraries/<name>.json`, since a name is unique only
//! within its scope — and a tree an older build wrote, with an application's
//! library at the top, is still the same library.

use std::collections::BTreeMap;

use sc_catalog::CodeLibrary;
use sc_error::Result;
use sc_sync::{Kind, Outcome, Step, StructureTree};

use crate::fixture::{Installation, code_app};

/// Two applications, `desk` and `shop`, each with its own library `fmt`, and
/// an instance-wide `fmt` beside them.
async fn three_fmts(inst: &Installation) -> Result<()> {
    inst.local_store("code", |d| d).await?;
    for (subdomain, code) in [
        ("desk", "exports.who = 'desk';"),
        ("shop", "exports.who = 'shop';"),
    ] {
        let app = inst
            .application(code_app(subdomain, "code", "web", "web/dist"))
            .await?;
        let library = CodeLibrary::new("fmt", code).in_application(app.id.0);
        sc_app::save_code_library_checked(&inst.catalog, &library).await?;
    }
    let library = CodeLibrary::new("fmt", "exports.who = 'instance';");
    sc_app::save_code_library_checked(&inst.catalog, &library).await?;
    Ok(())
}

/// Each library's code by the subdomain of its application (`-` for the
/// instance's).
async fn by_scope(inst: &Installation) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for library in sc_catalog::list_code_libraries(&inst.catalog).await? {
        let scope = match library.application_id {
            Some(app) => {
                sc_app::load_application(&inst.catalog, sc_app::AppId(app))
                    .await?
                    .unwrap()
                    .subdomain
            }
            None => "-".to_owned(),
        };
        out.insert(scope, library.code);
    }
    Ok(out)
}

#[tokio::test]
async fn same_named_libraries_of_different_scopes_export_to_their_own_files() -> Result<()> {
    let inst = Installation::new().await?;
    three_fmts(&inst).await?;
    let tree = inst.tree().await?;
    let code = |path: &str| {
        tree.json(path)
            .and_then(|v| v.get("code"))
            .and_then(|c| c.as_str())
            .map(str::to_owned)
    };
    assert_eq!(
        code("code-libraries/fmt.json").as_deref(),
        Some("exports.who = 'instance';")
    );
    assert_eq!(
        code("applications/desk/code-libraries/fmt.json").as_deref(),
        Some("exports.who = 'desk';")
    );
    assert_eq!(
        code("applications/shop/code-libraries/fmt.json").as_deref(),
        Some("exports.who = 'shop';")
    );
    Ok(())
}

#[tokio::test]
async fn an_application_library_deploys_after_its_application() -> Result<()> {
    let a = Installation::new().await?;
    three_fmts(&a).await?;
    let tree = a.tree().await?;

    let b = Installation::new().await?;
    let plan = b.plan_deploy(&StructureTree::new(), &tree).await?;
    let steps: BTreeMap<&str, Step> = plan
        .operations
        .iter()
        .filter(|op| op.kind == Kind::CodeLibrary)
        .map(|op| (op.entity.as_str(), op.step))
        .collect();
    assert_eq!(
        steps,
        BTreeMap::from([
            ("code_library:fmt", Step::Stores),
            ("code_library:desk/fmt", Step::Applications),
            ("code_library:shop/fmt", Step::Applications),
        ])
    );

    let receipt = b.deploy(&StructureTree::new(), &tree, |c| c).await?;
    assert!(receipt.applied.failed.is_none(), "{receipt:#?}");
    assert_eq!(receipt.result, Outcome::Converged, "{:#?}", receipt.drift);
    assert_eq!(by_scope(&b).await?, by_scope(&a).await?);
    Ok(())
}

#[tokio::test]
async fn an_application_library_where_an_older_build_wrote_it_is_the_same_library() -> Result<()> {
    let inst = Installation::new().await?;
    inst.local_store("code", |d| d).await?;
    let app = inst
        .application(code_app("desk", "code", "web", "web/dist"))
        .await?;
    let library = CodeLibrary::new("fmt", "exports.x = 1;").in_application(app.id.0);
    sc_app::save_code_library_checked(&inst.catalog, &library).await?;

    // The tree as 0.1.3 wrote it: the library at the top, with its
    // application's id.
    let mut older = inst.tree().await?;
    let file = older
        .remove("applications/desk/code-libraries/fmt.json")
        .unwrap();
    older.insert_json("code-libraries/fmt.json", file.as_json().unwrap().clone());

    let plan = inst.plan_deploy(&older, &older).await?;
    assert!(
        plan.operations
            .iter()
            .all(|op| op.kind != Kind::CodeLibrary),
        "{:#?}",
        plan.operations
    );
    Ok(())
}

#[tokio::test]
async fn a_library_moved_into_an_application_is_kept() -> Result<()> {
    // The same row, instance-wide here and the application's in the target:
    // a create in the application and a delete at the top, which must not
    // delete the row the create just moved.
    let inst = Installation::new().await?;
    inst.local_store("code", |d| d).await?;
    inst.application(code_app("desk", "code", "web", "web/dist"))
        .await?;
    let library = CodeLibrary::new("fmt", "exports.x = 1;");
    sc_app::save_code_library_checked(&inst.catalog, &library).await?;
    let before = inst.tree().await?;

    let mut target = before.clone();
    let file = target.remove("code-libraries/fmt.json").unwrap();
    target.insert_json(
        "applications/desk/code-libraries/fmt.json",
        file.as_json().unwrap().clone(),
    );
    let receipt = inst.deploy(&before, &target, |c| c).await?;
    assert!(receipt.applied.failed.is_none(), "{receipt:#?}");
    assert_eq!(
        by_scope(&inst).await?,
        BTreeMap::from([("desk".to_owned(), "exports.x = 1;".to_owned())])
    );
    Ok(())
}
