//! A table's primary key against a real Postgres (security review 2026-11 D3,
//! TODO-type-hardening H6.2): the name `Ident::derived("pkey")` computes is
//! the name Postgres gives the key, for a short name, a 59–63 byte name and a
//! name with a multi-byte character at the cut; and setting or renaming the key
//! of a table whose key does not have that name — a long name an earlier
//! version renamed, a name Postgres had to number because it was taken, a
//! name with quotes and a dollar tag in it — works.

use sc_db::{ColumnDef, SchemaChange};
use sc_db_postgres::PgDriver;
use sc_query::Ident;
use sc_test_harness::TestDb;

/// The name of `table`'s primary-key constraint, as Postgres has it.
async fn pkey_name(db: &TestDb, table: &str) -> Option<String> {
    let client = db.pool().get().await.expect("connection");
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    let row = client
        .query_opt(
            "SELECT conname::text FROM pg_constraint \
             WHERE conrelid = to_regclass($1) AND contype = 'p'",
            &[&quoted],
        )
        .await
        .expect("query");
    row.map(|r| r.get::<_, String>(0))
}

async fn create(driver: &PgDriver, name: &str, key: &str) -> sc_error::Result<()> {
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: name.into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null(),
                ColumnDef::new("code", "text").not_null(),
            ],
            primary_key: vec![key.into()],
            unlogged: false,
        })
        .await
}

async fn key_of(driver: &PgDriver, table: &str) -> sc_error::Result<Vec<String>> {
    Ok(driver
        .introspect()
        .await?
        .into_iter()
        .find(|t| t.name == table)
        .map(|t| t.primary_key)
        .unwrap_or_default())
}

#[tokio::test]
async fn the_derived_key_name_is_the_one_postgres_gives() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());
    for name in [
        "short".to_owned(),
        "k".repeat(58),
        "n".repeat(59),
        "m".repeat(63),
        // 56 bytes, then `é` (2 bytes) straddling the 58-byte cut, then more.
        format!("{}é{}", "w".repeat(57), "z".repeat(4)),
    ] {
        create(&driver, &name, "id").await?;
        let derived = Ident::existing(&name)?.derived("pkey");
        assert_eq!(
            pkey_name(&db, &name).await.as_deref(),
            Some(derived.as_str()),
            "{name}"
        );
    }
    Ok(())
}

/// For a 59–63 byte name, `<table>_pkey` (what was dropped before) is not the
/// key's name: `SetPrimaryKey` left the key there and its `ADD` failed with
/// "multiple primary keys".
#[tokio::test]
async fn the_key_of_a_long_named_table_can_be_replaced() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());
    let name = "l".repeat(61);
    create(&driver, &name, "id").await?;
    driver
        .apply_schema(&SchemaChange::SetPrimaryKey {
            table: name.clone(),
            columns: vec!["code".into()],
        })
        .await?;
    assert_eq!(key_of(&driver, &name).await?, vec!["code"]);
    assert_eq!(
        pkey_name(&db, &name).await.as_deref(),
        Some(format!("{}_pkey", "l".repeat(58)).as_str())
    );
    // And no key at all.
    driver
        .apply_schema(&SchemaChange::SetPrimaryKey {
            table: name.clone(),
            columns: Vec::new(),
        })
        .await?;
    assert!(key_of(&driver, &name).await?.is_empty());
    Ok(())
}

/// A rename takes the key's name along, to the name Postgres would give it
/// after the new table; a long name and one renamed by an earlier version
/// (whose key kept the first 63 bytes of `<from>_pkey`) included.
#[tokio::test]
async fn a_rename_takes_the_key_name_along_whatever_its_length() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());

    let from = "r".repeat(60);
    let to = "s".repeat(62);
    create(&driver, &from, "id").await?;
    driver
        .apply_schema(&SchemaChange::RenameTable {
            from: from.clone(),
            to: to.clone(),
        })
        .await?;
    assert_eq!(
        pkey_name(&db, &to).await.as_deref(),
        Some(Ident::existing(&to)?.derived("pkey").as_str())
    );
    // Still replaceable afterwards.
    driver
        .apply_schema(&SchemaChange::SetPrimaryKey {
            table: to.clone(),
            columns: vec!["code".into()],
        })
        .await?;
    assert_eq!(key_of(&driver, &to).await?, vec!["code"]);

    // A key an earlier version renamed to the first 63 bytes of
    // `<table>_pkey`: renamed again, to the derived name.
    let legacy_table = "u".repeat(60);
    create(&driver, &legacy_table, "id").await?;
    let legacy_key = format!("{legacy_table}_pk");
    {
        let client = db.pool().get().await.expect("connection");
        client
            .batch_execute(&format!(
                "ALTER TABLE \"{legacy_table}\" RENAME CONSTRAINT \"{}\" TO \"{legacy_key}\"",
                Ident::existing(&legacy_table)?.derived("pkey").as_str()
            ))
            .await
            .expect("rename the key as an earlier version left it");
    }
    let renamed = "v".repeat(10);
    driver
        .apply_schema(&SchemaChange::RenameTable {
            from: legacy_table,
            to: renamed.clone(),
        })
        .await?;
    assert_eq!(
        pkey_name(&db, &renamed).await.as_deref(),
        Some("vvvvvvvvvv_pkey")
    );
    Ok(())
}

/// Where `<table>_pkey` is taken, Postgres numbers the key (`<table>_pkey1`):
/// it is still found, replaced and renamed, and a rename onto a taken name
/// leaves the key's name alone rather than failing.
#[tokio::test]
async fn a_numbered_key_and_a_taken_name_are_handled() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());
    // A table called `clash_pkey` takes the name the key of `clash` would get.
    create(&driver, "clash_pkey", "id").await?;
    create(&driver, "clash", "id").await?;
    assert_eq!(
        pkey_name(&db, "clash").await.as_deref(),
        Some("clash_pkey1")
    );
    driver
        .apply_schema(&SchemaChange::SetPrimaryKey {
            table: "clash".into(),
            columns: vec!["code".into()],
        })
        .await?;
    assert_eq!(key_of(&driver, "clash").await?, vec!["code"]);

    // `other`'s key would be `other_pkey`, which a table already has: the
    // rename goes through and the key keeps a name Postgres chose.
    create(&driver, "other_pkey", "id").await?;
    create(&driver, "mine", "id").await?;
    driver
        .apply_schema(&SchemaChange::RenameTable {
            from: "mine".into(),
            to: "other".into(),
        })
        .await?;
    assert_eq!(pkey_name(&db, "other").await.as_deref(), Some("mine_pkey"));
    Ok(())
}

/// A name a database somebody else built may have: quotes, a space and a
/// `$sc$` — quoted, and unable to end the `DO` block the key is found in.
#[tokio::test]
async fn a_name_with_quotes_and_a_dollar_tag_is_quoted() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());
    let name = "odd \"name\" $sc$ here";
    create(&driver, name, "id").await?;
    driver
        .apply_schema(&SchemaChange::SetPrimaryKey {
            table: name.into(),
            columns: vec!["code".into()],
        })
        .await?;
    assert_eq!(key_of(&driver, name).await?, vec!["code"]);
    let to = "odd $sc1$ 'too'";
    driver
        .apply_schema(&SchemaChange::RenameTable {
            from: name.into(),
            to: to.into(),
        })
        .await?;
    assert_eq!(
        pkey_name(&db, to).await.as_deref(),
        Some(Ident::existing(to)?.derived("pkey").as_str())
    );
    Ok(())
}
