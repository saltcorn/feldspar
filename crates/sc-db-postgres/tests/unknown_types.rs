//! Columns of types the driver has no binary codec for — pgvector's `halfvec`
//! and `sparsevec`, `interval`, `inet`, an array — travel as their text (bug
//! report: `feldspar backup` aborted on an app's `halfvec` column).
//!
//! A read of such a column is rerun with it cast to text, keeping the row order
//! and the column names; a text value written to one is sent in Postgres's text
//! format; and introspection spells the pgvector types with their dimensions.

use sc_db_postgres::PgDriver;
use sc_query::{Expr, Insert, OrderBy, Projection, Select, Source, Statement, TrustedSql, Value};
use sc_test_harness::TestDb;

/// Types every Postgres has: `(column, type, a value as Postgres prints it)`.
const BUILT_IN: &[(&str, &str, &str)] = &[
    ("span", "interval", "1 day 02:00:00"),
    ("addr", "inet", "10.0.0.0/24"),
    ("tags", "int4[]", "{1,2,3}"),
];

/// pgvector's types this system does not know.
const PGVECTOR: &[(&str, &str, &str)] = &[
    ("half", "halfvec(3)", "[1,0.5,-2]"),
    ("sparse", "sparsevec(5)", "{1:1.5,4:2}/5"),
];

fn text(s: &str) -> Expr {
    Expr::lit(Value::Text(s.to_owned()))
}

fn raw(sql: &str) -> Statement {
    Statement::raw(TrustedSql::admin_authored(sql), vec![])
}

async fn rows(driver: &PgDriver, stmt: Statement) -> sc_error::Result<Vec<sc_db::Row>> {
    driver.query(&stmt).await?.try_collect().await
}

fn text_of(row: &sc_db::Row, column: &str) -> String {
    match row.get(column) {
        Some(Value::Text(t)) => t.clone(),
        other => panic!("`{column}` as text, not {other:?}"),
    }
}

/// `odd (id int8 primary key, <columns>)`, three rows written through ordinary
/// inserts with text values — as a restore writes them — then read back.
async fn round_trip(driver: &PgDriver, columns: &[(&str, &str, &str)]) -> sc_error::Result<()> {
    let ddl: Vec<String> = columns.iter().map(|(c, t, _)| format!("{c} {t}")).collect();
    rows(
        driver,
        raw(&format!(
            "CREATE TABLE odd (id int8 PRIMARY KEY, {})",
            ddl.join(", ")
        )),
    )
    .await?;
    let mut names = vec!["id".to_owned()];
    names.extend(columns.iter().map(|(c, _, _)| (*c).to_owned()));
    for id in 1..=3_i64 {
        let mut values = vec![Expr::lit(id)];
        values.extend(columns.iter().map(|(_, _, v)| text(v)));
        rows(
            driver,
            Statement::from(Insert::row("odd", names.clone(), values)),
        )
        .await?;
    }

    // `SELECT *`, ordered and limited the way a backup pages through a table.
    let mut select = Select::from(Source::table("odd"))
        .columns(vec![Projection::all()])
        .limit(2);
    select.order = vec![OrderBy::desc(Expr::col("id"))];
    let got = rows(driver, Statement::from(select)).await?;
    assert_eq!(got.len(), 2);
    let got_names: Vec<String> = got[0].columns().iter().map(ToString::to_string).collect();
    assert_eq!(got_names, names);
    assert_eq!(got[0].get("id"), Some(&Value::Int(3)));
    assert_eq!(got[1].get("id"), Some(&Value::Int(2)));
    for (column, _, value) in columns {
        assert_eq!(text_of(&got[0], column), *value);
    }

    // What a read gives back writes back unchanged.
    let mut values = vec![Expr::lit(9_i64)];
    values.extend(columns.iter().map(|(c, _, _)| text(&text_of(&got[0], c))));
    rows(
        driver,
        Statement::from(Insert::row("odd", names.clone(), values)),
    )
    .await?;
    let same: Vec<String> = columns
        .iter()
        .map(|(c, _, _)| format!("a.{c} = b.{c}"))
        .collect();
    let n = rows(
        driver,
        raw(&format!(
            "SELECT count(*) AS n FROM odd a JOIN odd b ON b.id = 9 WHERE a.id = 3 AND {}",
            same.join(" AND ")
        )),
    )
    .await?;
    assert_eq!(n[0].get("n"), Some(&Value::Int(1)));

    // NULL in such a column is NULL, not the text "NULL".
    rows(driver, raw("INSERT INTO odd (id) VALUES (10)")).await?;
    let empty = rows(driver, raw("SELECT * FROM odd WHERE id = 10")).await?;
    for (column, _, _) in columns {
        assert_eq!(empty[0].get(column), Some(&Value::Null), "{column}");
    }
    Ok(())
}

#[tokio::test]
async fn built_in_types_without_a_codec_are_read_and_written_as_text() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());
    round_trip(&driver, BUILT_IN).await?;

    // A data-modifying statement's `RETURNING` reads them as text too, and a
    // text parameter of such a type goes in as text.
    let returned = rows(
        &driver,
        Statement::raw(
            TrustedSql::admin_authored("UPDATE odd SET span = $1 WHERE id = 1 RETURNING id, span"),
            vec![Value::Text("3 hours".into())],
        ),
    )
    .await?;
    assert_eq!(returned.len(), 1);
    assert_eq!(text_of(&returned[0], "span"), "03:00:00");

    // Two result columns of one name cannot be read from outside: refused with
    // a sentence, never answered with the wrong column.
    let err = rows(&driver, raw("SELECT a.span, b.span FROM odd a, odd b"))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("two columns named `span`"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn pgvector_halfvec_and_sparsevec_are_read_and_written_as_text() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_pgvector().await? else {
        return Ok(());
    };
    let driver = PgDriver::from_pool(db.pool().clone());
    round_trip(&driver, PGVECTOR).await?;

    // Introspection spells them with their dimensions, so a restore can make
    // the same column.
    let tables = driver.introspect().await?;
    let odd = tables.iter().find(|t| t.name == "odd").expect("odd");
    for (column, sql_type, _) in PGVECTOR {
        let found = odd
            .columns
            .iter()
            .find(|c| c.name == *column)
            .expect(column);
        assert_eq!(found.sql_type, *sql_type);
    }
    Ok(())
}
