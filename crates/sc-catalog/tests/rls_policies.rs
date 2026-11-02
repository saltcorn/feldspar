//! Phase 6 integration test: RLS policy generation and the caller-context
//! runner, against a real database (§7.3, §6).
//!
//! The server-level test proves the end-to-end behaviour; this proves the
//! storage-layer primitives directly — that `enable_rls` actually emits the
//! four policies with `FORCE`, that `disable_rls` removes them cleanly, and
//! that `run_in_context` sets the GUCs a policy reads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::Arc;

use sc_catalog::{
    Catalog, DataFieldKind, FieldMeta, Principal, TableMeta, bootstrap_field_meta,
    bootstrap_table_meta, disable_rls, enable_rls, run_in_context, save_field_meta,
    save_table_meta,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Projection, Select, Source, Statement};
use sc_test_harness::TestDb;

async fn catalog(db: &TestDb) -> Result<Catalog> {
    // No `users` table here (that lives above this crate, in `sc-auth`): with no
    // user fields declared, a `user.email` reference validates unchecked, which
    // is all this storage-layer test needs.
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_table_meta(&catalog).await?;
    Ok(catalog)
}

/// The policy names Postgres reports for a table, sorted.
async fn policies(db: &TestDb, table: &str) -> Vec<String> {
    let client = db.client().await.unwrap();
    let rows = client
        .query(
            "SELECT policyname FROM pg_policies WHERE tablename = $1 ORDER BY policyname",
            &[&table],
        )
        .await
        .unwrap();
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

/// Whether the table has RLS enabled and forced.
async fn rls_flags(db: &TestDb, table: &str) -> (bool, bool) {
    let client = db.client().await.unwrap();
    let row = client
        .query_one(
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = $1",
            &[&table],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn configure_formula(catalog: &Catalog, table: &str, formula: &str) -> Result<()> {
    let mut meta = TableMeta::new(table);
    meta.set_ownership_formula(Some(formula));
    meta.set_rls_enabled(true);
    save_table_meta(catalog, &meta).await
}

#[tokio::test]
async fn enable_creates_four_forced_policies_and_disable_removes_them() -> Result<()> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute("CREATE TABLE posts (id bigint primary key, owner text)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let catalog = catalog(&db).await?;
    catalog.reload().await?;
    configure_formula(&catalog, "posts", "owner === user.email").await?;

    let table = catalog.require("posts")?;
    enable_rls(&catalog, &table).await?;

    // Four policies, one per command, and both RLS flags set (FORCE is what
    // makes the policies bite for the owning connection at all).
    assert_eq!(
        policies(&db, "posts").await,
        vec![
            "sc_owner_delete",
            "sc_owner_insert",
            "sc_owner_select",
            "sc_owner_update",
        ]
    );
    assert_eq!(rls_flags(&db, "posts").await, (true, true));

    // Enabling again is idempotent — the drop-then-create recreates, never
    // duplicates (a formula edit takes this path).
    let table = catalog.require("posts")?;
    enable_rls(&catalog, &table).await?;
    assert_eq!(policies(&db, "posts").await.len(), 4);

    // Disable leaves no policy debris and clears both flags.
    disable_rls(&catalog, "posts").await?;
    assert!(policies(&db, "posts").await.is_empty());
    assert_eq!(rls_flags(&db, "posts").await, (false, false));
    Ok(())
}

#[tokio::test]
async fn run_in_context_sets_the_caller_gucs_a_policy_reads() -> Result<()> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE posts (id bigint primary key, owner text); \
             INSERT INTO posts VALUES (1, 'alice@example.com'), (2, 'bob@example.com')",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let catalog = catalog(&db).await?;
    catalog.reload().await?;
    configure_formula(&catalog, "posts", "owner === user.email").await?;
    let table = catalog.require("posts")?;
    enable_rls(&catalog, &table).await?;

    let select = Statement::Select(Box::new(
        Select::from(Source::table("posts")).columns(vec![Projection::all()]),
    ));

    // Alice's context: only her row is visible through the SELECT policy.
    let alice = Principal::new(
        Role::of(80),
        Some(serde_json::json!({ "email": "alice@example.com" })),
    );
    let rows = run_in_context(
        &catalog,
        sc_catalog::Unchecked::vouched(&alice, "test"),
        &select,
    )
    .await?;
    assert_eq!(rows.len(), 1);

    // An admin context (role 1) clears the role floor and sees both rows even
    // though the table is FORCE'd.
    let admin = Principal::anonymous(Role::ADMIN);
    let rows = run_in_context(
        &catalog,
        sc_catalog::Unchecked::vouched(&admin, "test"),
        &select,
    )
    .await?;
    assert_eq!(rows.len(), 2);

    // An anonymous context (role 100, no user) sees nothing — the policy fails
    // closed when the user GUC is unset.
    let anon = Principal::anonymous(Role::PUBLIC);
    let rows = run_in_context(
        &catalog,
        sc_catalog::Unchecked::vouched(&anon, "test"),
        &select,
    )
    .await?;
    assert!(rows.is_empty());
    Ok(())
}

/// The `USING`/`WITH CHECK` expression Postgres stores for a policy.
async fn policy_qual(db: &TestDb, table: &str, policy: &str) -> String {
    let client = db.client().await.unwrap();
    let row = client
        .query_one(
            "SELECT qual FROM pg_policies WHERE tablename = $1 AND policyname = $2",
            &[&table, &policy],
        )
        .await
        .unwrap();
    row.get::<_, Option<String>>(0).unwrap_or_default()
}

async fn add_calc(catalog: &Catalog, table: &str, name: &str, expr: &str) -> Result<()> {
    save_field_meta(
        catalog,
        &FieldMeta::new(table, name).kind(DataFieldKind::Calc {
            expression: expr.into(),
        }),
    )
    .await
}

/// An ownership formula that names a non-stored calculated field has that
/// field's *definition* inlined into the generated policy (Phase 8) — there is
/// no `is_public` column, so the policy must reference `secret` directly.
#[tokio::test]
async fn a_calc_field_is_inlined_into_the_generated_policy() -> Result<()> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute("CREATE TABLE docs (id bigint primary key, secret bigint)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let catalog = catalog(&db).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog.reload().await?;
    add_calc(&catalog, "docs", "is_public", "secret === 0").await?;
    configure_formula(&catalog, "docs", "is_public").await?;

    let docs = catalog.require("docs")?;
    enable_rls(&catalog, &docs).await?;

    let qual = policy_qual(&db, "docs", "sc_owner_select").await;
    assert!(qual.contains("secret"), "definition not inlined: {qual}");
    assert!(
        !qual.contains("is_public"),
        "calc column referenced: {qual}"
    );
    Ok(())
}

/// Enabling RLS with a formula that names a calc field whose definition does not
/// translate is refused (Phase 8) — the untranslatable construct surfaces as the
/// reason, rather than emitting a policy that would fail at query time.
#[tokio::test]
async fn enabling_rls_refuses_an_untranslatable_calc_definition() -> Result<()> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute("CREATE TABLE docs (id bigint primary key, tags text)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let catalog = catalog(&db).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog.reload().await?;
    // A method call has no SQL form: the calc field is valid but untranslatable.
    add_calc(&catalog, "docs", "tagged", "tags.includes(\"x\")").await?;
    configure_formula(&catalog, "docs", "tagged").await?;

    let docs = catalog.require("docs")?;
    let err = enable_rls(&catalog, &docs).await.unwrap_err().to_string();
    assert!(
        err.contains("translated") || err.contains("function call"),
        "got: {err}"
    );
    Ok(())
}

/// A Ↄ-aggregation policy that queries a child table whose own policy queries
/// back is refused at enablement, naming the cycle (Phase 7) — Postgres would
/// otherwise raise "infinite recursion detected in policy" at query time.
#[tokio::test]
async fn enabling_rls_refuses_a_policy_reference_cycle() -> Result<()> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE documents (id bigint primary key, owner text); \
             CREATE TABLE shares (id bigint primary key, \
                 document bigint references documents(id), shared_with text)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let catalog = catalog(&db).await?;
    catalog.reload().await?;

    // documents' policy queries shares (an aggregation); enabling it alone is
    // fine because shares has no policy yet.
    configure_formula(
        &catalog,
        "documents",
        "owner === user.email || sharesↃdocument.some(s => s.shared_with === user.email)",
    )
    .await?;
    catalog.reload().await?;
    let documents = catalog.require("documents")?;
    enable_rls(&catalog, &documents).await?;

    // shares' policy queries documents back (a Ⱶ-join). Enabling it would close
    // the cycle shares → documents → shares, so it is refused by name.
    configure_formula(&catalog, "shares", "documentⱵowner === user.email").await?;
    catalog.reload().await?;
    let shares = catalog.require("shares")?;
    let err = enable_rls(&catalog, &shares).await.unwrap_err().to_string();
    assert!(err.contains("cycle"), "got: {err}");
    assert!(
        err.contains("shares") && err.contains("documents"),
        "got: {err}"
    );
    Ok(())
}
