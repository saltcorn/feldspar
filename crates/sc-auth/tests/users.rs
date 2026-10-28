//! Phase 5 integration test: bootstrap the `users` table against a real Postgres
//! and verify its shape via the catalog and driver introspection.

use std::sync::Arc;

use sc_auth::{
    COL_ATTRIBUTES, COL_DISABLED, COL_EMAIL, COL_EMAIL_VERIFIED, COL_ID, COL_LANGUAGE,
    COL_PASSWORD_HASH, COL_ROLE, USERS_TABLE, bootstrap,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

#[tokio::test]
async fn bootstrap_creates_users_table() -> sc_error::Result<()> {
    let db = TestDb::new().await?;

    // The local test template is a multi-tenant v1 database that carries a
    // `users` table in several schemas; the catalog keys tables by name alone,
    // so drop every `users` table across schemas to reach the clean-primary-DB
    // state the MVP assumes. In clean CI (empty template) this is a no-op.
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver.clone() as Arc<dyn DatabaseDriver>).await?;

    // No users table before bootstrap.
    assert!(catalog.get(USERS_TABLE)?.is_none());

    let users = bootstrap(&catalog).await?;
    assert_eq!(users.name, USERS_TABLE);

    // UUID primary key, not deletable — modelled here as the single PK column.
    assert_eq!(users.primary_key, vec![COL_ID.to_string()]);
    let id = users.field(COL_ID).expect("id field");
    assert!(id.primary_key && id.required);
    assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));

    // role (1–100) stored as an integer.
    let role = users.field(COL_ROLE).expect("role field");
    assert!(role.required);
    assert_eq!(role.base.type_, TypeRef::Basic(BasicType::Int));

    // Initial email identifier: required + unique text.
    let email = users.field(COL_EMAIL).expect("email field");
    assert!(email.required);
    assert_eq!(email.base.type_, TypeRef::Basic(BasicType::Text));

    // argon2id password hash: nullable text.
    let pw = users.field(COL_PASSWORD_HASH).expect("password_hash field");
    assert!(!pw.required);
    assert_eq!(pw.base.type_, TypeRef::Basic(BasicType::Text));

    // The disabled flag: nullable boolean, since `NULL` is an ordinary enabled
    // account and that is what every account starts as.
    let disabled = users.field(COL_DISABLED).expect("disabled field");
    assert!(!disabled.required);
    assert_eq!(disabled.base.type_, TypeRef::Basic(BasicType::Bool));

    // The chosen language (§16.1): nullable text, since `NULL` is "whatever the
    // request negotiates" and most accounts never state one.
    let language = users.field(COL_LANGUAGE).expect("language field");
    assert!(!language.required);
    assert_eq!(language.base.type_, TypeRef::Basic(BasicType::Text));

    // The system's sparse per-user settings (the check-user flag): nullable JSON.
    let attributes = users.field(COL_ATTRIBUTES).expect("attributes field");
    assert!(!attributes.required);
    assert_eq!(attributes.base.type_, TypeRef::Basic(BasicType::Json));

    // The table is reflected in a fresh driver introspection.
    let introspected = driver.introspect().await?;
    let physical = introspected
        .iter()
        .find(|t| t.name == USERS_TABLE)
        .expect("users present in introspection");
    let cols: Vec<&str> = physical.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        cols,
        [
            COL_ID,
            COL_ROLE,
            COL_EMAIL,
            COL_PASSWORD_HASH,
            COL_DISABLED,
            COL_LANGUAGE,
            COL_EMAIL_VERIFIED,
            COL_ATTRIBUTES
        ]
    );
    assert_eq!(physical.primary_key, vec![COL_ID.to_string()]);

    // Idempotent: a second bootstrap returns the existing table, no error.
    let again = bootstrap(&catalog).await?;
    assert_eq!(again.name, USERS_TABLE);
    assert_eq!(again.fields.len(), users.fields.len());

    Ok(())
}
