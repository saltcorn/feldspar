//! The SQLite driver against a real database: rows in and out, the schema
//! changes SQLite has no `ALTER TABLE` for, transactions, and introspection.
//!
//! No harness and no server: a SQLite database is a file (or, here, a private
//! in-memory database), which is the whole reason this backend exists.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use rust_decimal::Decimal;
use sc_db::{
    ColumnDef, ColumnGenerator, CommentTarget, DatabaseDriver, IndexOn, PhysicalConstraintKind,
    SchemaChange,
};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{
    Assignment, Delete, Expr, Insert, Projection, Select, Source, Statement, Update, Value,
};
use uuid::Uuid;

/// A driver on a fresh in-memory database.
fn driver() -> SqliteDriver {
    SqliteDriver::open_in_memory().expect("open an in-memory database")
}

/// A book table with a key that numbers itself.
async fn books(driver: &SqliteDriver) -> Result<()> {
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "book".into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null().identity(),
                ColumnDef::new("title", "text").not_null(),
                ColumnDef::new("pages", "int8"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        })
        .await
}

async fn rows(driver: &SqliteDriver, stmt: Statement) -> Result<Vec<sc_db::Row>> {
    driver.query(&stmt).await?.try_collect().await
}

#[tokio::test]
async fn a_table_is_created_written_read_and_changed() -> Result<()> {
    let driver = driver();
    books(&driver).await?;

    // The key numbers itself, so an insert that names only the title comes back
    // with the id the database chose — which is what `RETURNING` is for.
    let inserted = rows(
        &driver,
        Insert::row(
            "book",
            vec!["title".into(), "pages".into()],
            vec![Expr::lit("Orlando"), Expr::lit(288_i64)],
        )
        .returning(vec![Projection::expr(Expr::col("id"))])
        .into(),
    )
    .await?;
    assert_eq!(inserted.len(), 1);
    let id = match inserted[0].get("id") {
        Some(Value::Int(id)) => *id,
        other => panic!("expected an int id, got {other:?}"),
    };
    assert_eq!(id, 1, "the first row of a self-numbering key is 1");

    rows(
        &driver,
        Insert::row(
            "book",
            vec!["title".into(), "pages".into()],
            vec![Expr::lit("The Waves"), Expr::lit(228_i64)],
        )
        .into(),
    )
    .await?;

    // A filtered, ordered read.
    let found = rows(
        &driver,
        Select::from(Source::table("book"))
            .columns(vec![
                Projection::expr(Expr::col("title")),
                Projection::expr(Expr::col("pages")),
            ])
            .filter(Expr::Binary {
                op: sc_query::BinOp::Gt,
                l: Box::new(Expr::col("pages")),
                r: Box::new(Expr::lit(200_i64)),
            })
            .into(),
    )
    .await?;
    assert_eq!(found.len(), 2);

    // An update and a delete, each reporting what they touched.
    let updated = rows(
        &driver,
        Update {
            returning: vec![Projection::expr(Expr::col("pages"))],
            ..Update::new("book", vec![Assignment::new("pages", Expr::lit(300_i64))])
                .filter(Expr::col("id").eq(Expr::lit(id)))
        }
        .into(),
    )
    .await?;
    assert_eq!(updated[0].get("pages"), Some(&Value::Int(300)));

    let deleted = rows(
        &driver,
        Delete {
            returning: vec![Projection::expr(Expr::col("title"))],
            ..Delete::from("book").filter(Expr::col("id").eq(Expr::lit(id)))
        }
        .into(),
    )
    .await?;
    assert_eq!(
        deleted[0].get("title"),
        Some(&Value::Text("Orlando".into()))
    );
    Ok(())
}

/// Eleven `Value` kinds into five storage classes and back again — the whole
/// point of keeping the declared type name.
#[tokio::test]
async fn every_value_kind_survives_a_round_trip() -> Result<()> {
    let driver = driver();
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "kinds".into(),
            columns: vec![
                ColumnDef::new("b", "bool"),
                ColumnDef::new("i", "int8"),
                ColumnDef::new("f", "float8"),
                ColumnDef::new("d", "numeric"),
                ColumnDef::new("t", "text"),
                ColumnDef::new("bytes", "bytea"),
                ColumnDef::new("j", "jsonb"),
                ColumnDef::new("u", "uuid"),
                ColumnDef::new("day", "date"),
                ColumnDef::new("clock", "time"),
                ColumnDef::new("when_", "timestamptz"),
                ColumnDef::new("nothing", "text"),
            ],
            primary_key: Vec::new(),
            unlogged: false,
        })
        .await?;

    let uuid = Uuid::new_v4();
    let values = vec![
        Value::Bool(true),
        Value::Int(-42),
        Value::Float(2.5),
        Value::Decimal(Decimal::new(12345, 2)),
        Value::Text("Woolf".into()),
        Value::Bytes(vec![0, 1, 2, 255]),
        Value::Json(serde_json::json!({"a": [1, 2]})),
        Value::Uuid(uuid),
        Value::Date(NaiveDate::from_ymd_opt(2024, 5, 6).unwrap()),
        Value::Time(NaiveTime::from_hms_opt(12, 30, 0).unwrap()),
        Value::Timestamp(Utc.with_ymd_and_hms(2024, 5, 6, 12, 0, 0).unwrap()),
        Value::Null,
    ];
    let columns: Vec<String> = [
        "b", "i", "f", "d", "t", "bytes", "j", "u", "day", "clock", "when_", "nothing",
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect();
    rows(
        &driver,
        Insert::row(
            "kinds",
            columns.clone(),
            values.iter().cloned().map(Expr::Lit).collect(),
        )
        .into(),
    )
    .await?;

    let read = rows(&driver, Select::from(Source::table("kinds")).into()).await?;
    assert_eq!(read.len(), 1);
    for (column, expected) in columns.iter().zip(&values) {
        assert_eq!(read[0].get(column), Some(expected), "column {column}");
    }
    Ok(())
}

/// A timestamp column is written so that text order is time order — otherwise
/// every `ORDER BY` on a date would be wrong in a way nobody would spot.
#[tokio::test]
async fn timestamps_sort_chronologically() -> Result<()> {
    let driver = driver();
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "events".into(),
            columns: vec![
                ColumnDef::new("name", "text"),
                ColumnDef::new("at", "timestamptz"),
            ],
            primary_key: Vec::new(),
            unlogged: false,
        })
        .await?;
    for (name, hour) in [("late", 23), ("early", 1), ("noon", 12)] {
        rows(
            &driver,
            Insert::row(
                "events",
                vec!["name".into(), "at".into()],
                vec![
                    Expr::lit(name),
                    Expr::Lit(Value::Timestamp(
                        Utc.with_ymd_and_hms(2024, 5, 6, hour, 0, 0).unwrap(),
                    )),
                ],
            )
            .into(),
        )
        .await?;
    }
    let ordered = rows(
        &driver,
        Select {
            order: vec![sc_query::OrderBy::asc(Expr::col("at"))],
            ..Select::from(Source::table("events"))
        }
        .into(),
    )
    .await?;
    let names: Vec<&Value> = ordered.iter().filter_map(|r| r.get("name")).collect();
    assert_eq!(
        names,
        vec![
            &Value::Text("early".into()),
            &Value::Text("noon".into()),
            &Value::Text("late".into())
        ]
    );
    Ok(())
}

#[tokio::test]
async fn introspection_reports_the_live_shape() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "review".into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null().identity(),
                ColumnDef::new("book", "int8")
                    .not_null()
                    .references("book", "id"),
                ColumnDef::new("stars", "int8"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        })
        .await?;
    driver
        .apply_schema(&SchemaChange::AddUniqueConstraint {
            table: "review".into(),
            name: "review_book_key".into(),
            columns: vec!["book".into()],
        })
        .await?;
    driver
        .apply_schema(&SchemaChange::CreateIndex {
            table: "review".into(),
            name: "review_stars_idx".into(),
            on: IndexOn::Columns(vec!["stars".into()]),
            method: None,
        })
        .await?;

    let tables = driver.introspect().await?;
    let book = tables.iter().find(|t| t.name == "book").expect("book");
    assert_eq!(book.primary_key, vec!["id"]);
    // The declared type is kept, so the type layer sees what it would see on
    // Postgres — and the key numbers itself.
    assert_eq!(book.columns[0].sql_type.to_lowercase(), "integer");
    assert_eq!(book.columns[0].generated, Some(ColumnGenerator::Identity));
    assert!(book.columns[1].sql_type.eq_ignore_ascii_case("text"));
    assert!(!book.columns[1].nullable);
    assert!(book.columns[2].nullable);

    let review = tables.iter().find(|t| t.name == "review").expect("review");
    assert_eq!(review.foreign_keys.len(), 1);
    assert_eq!(review.foreign_keys[0].columns, vec!["book"]);
    assert_eq!(review.foreign_keys[0].referenced_table, "book");
    assert_eq!(review.foreign_keys[0].referenced_columns, vec!["id"]);

    let unique = review
        .constraints
        .iter()
        .find(|c| c.name == "review_book_key")
        .expect("the unique constraint is reported");
    assert_eq!(
        unique.kind,
        PhysicalConstraintKind::Unique {
            columns: vec!["book".into()]
        }
    );
    let index = review
        .constraints
        .iter()
        .find(|c| c.name == "review_stars_idx")
        .expect("the index is reported");
    assert!(matches!(
        &index.kind,
        PhysicalConstraintKind::Index { columns, .. } if columns == &vec!["stars".to_string()]
    ));
    // The primary key is reported as the key, not a second time as an index.
    assert!(
        !review
            .constraints
            .iter()
            .any(|c| c.name.contains("autoindex"))
    );
    Ok(())
}

/// A foreign key is enforced — SQLite leaves them off by default, so this is a
/// property of the driver rather than of the database.
#[tokio::test]
async fn a_foreign_key_is_enforced() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "review".into(),
            columns: vec![ColumnDef::new("book", "int8").references("book", "id")],
            primary_key: Vec::new(),
            unlogged: false,
        })
        .await?;
    let error = rows(
        &driver,
        Insert::row("review", vec!["book".into()], vec![Expr::lit(99_i64)]).into(),
    )
    .await
    .expect_err("there is no book 99");
    let text = format!("{error}");
    assert!(text.contains("23503"), "{text}");
    Ok(())
}

/// The comment that carries a constraint's error message, in a database with no
/// `COMMENT ON`.
#[tokio::test]
async fn a_constraints_comment_is_stored_and_read_back() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    driver
        .apply_schema(&SchemaChange::AddUniqueConstraint {
            table: "book".into(),
            name: "book_title_key".into(),
            columns: vec!["title".into()],
        })
        .await?;
    driver
        .apply_schema(&SchemaChange::SetComment {
            target: CommentTarget::Constraint {
                table: "book".into(),
                name: "book_title_key".into(),
            },
            comment: Some("that title is already here".into()),
        })
        .await?;

    let tables = driver.introspect().await?;
    let book = tables.iter().find(|t| t.name == "book").expect("book");
    let constraint = book
        .constraints
        .iter()
        .find(|c| c.name == "book_title_key")
        .expect("the constraint");
    assert_eq!(
        constraint.comment.as_deref(),
        Some("that title is already here")
    );

    // …and removing it removes the row rather than leaving an empty comment.
    driver
        .apply_schema(&SchemaChange::SetComment {
            target: CommentTarget::Constraint {
                table: "book".into(),
                name: "book_title_key".into(),
            },
            comment: None,
        })
        .await?;
    let tables = driver.introspect().await?;
    let book = tables.iter().find(|t| t.name == "book").expect("book");
    assert!(
        book.constraints
            .iter()
            .find(|c| c.name == "book_title_key")
            .expect("the constraint")
            .comment
            .is_none()
    );

    // The driver's own comment table is not offered as a table to use.
    assert!(
        driver
            .introspect()
            .await?
            .iter()
            .any(|t| t.name == "_fd_object_comments"),
        "it is an ordinary table in the file; the catalog hides `_fd_` tables"
    );
    Ok(())
}

/// A unique violation has to arrive as the SQLSTATE the rest of Saltcorn reads,
/// or an admin's own error message for the rule is never shown.
#[tokio::test]
async fn a_unique_violation_is_reported_as_one() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    driver
        .apply_schema(&SchemaChange::AddUniqueConstraint {
            table: "book".into(),
            name: "book_title_key".into(),
            columns: vec!["title".into()],
        })
        .await?;
    let insert = |title: &str| {
        Statement::from(Insert::row(
            "book",
            vec!["title".into()],
            vec![Expr::lit(title)],
        ))
    };
    rows(&driver, insert("Orlando")).await?;
    let error = rows(&driver, insert("Orlando"))
        .await
        .expect_err("the second Orlando is a unique violation");
    let text = format!("{error}");
    assert!(text.contains("23505"), "{text}");
    Ok(())
}

/// The changes SQLite has no `ALTER TABLE` for: a key added to a table that had
/// none, and a column taught to number itself — both of which rebuild the table
/// and must keep the rows that are in it.
#[tokio::test]
async fn a_key_can_be_added_to_a_table_that_had_none() -> Result<()> {
    let driver = driver();
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "note".into(),
            columns: vec![ColumnDef::new("id", "int8"), ColumnDef::new("body", "text")],
            primary_key: Vec::new(),
            unlogged: false,
        })
        .await?;
    driver
        .apply_schema(&SchemaChange::CreateIndex {
            table: "note".into(),
            name: "note_body_idx".into(),
            on: IndexOn::Columns(vec!["body".into()]),
            method: None,
        })
        .await?;
    for (id, body) in [(1_i64, "first"), (2, "second")] {
        rows(
            &driver,
            Insert::row(
                "note",
                vec!["id".into(), "body".into()],
                vec![Expr::lit(id), Expr::lit(body)],
            )
            .into(),
        )
        .await?;
    }

    // The two halves of "make this field the key", exactly as the schema editor
    // issues them.
    driver
        .apply_schema(&SchemaChange::SetPrimaryKey {
            table: "note".into(),
            columns: vec!["id".into()],
        })
        .await?;
    driver
        .apply_schema(&SchemaChange::SetColumnGenerator {
            table: "note".into(),
            column: "id".into(),
            generator: Some(ColumnGenerator::Identity),
        })
        .await?;

    let tables = driver.introspect().await?;
    let note = tables.iter().find(|t| t.name == "note").expect("note");
    assert_eq!(note.primary_key, vec!["id"]);
    assert_eq!(note.columns[0].generated, Some(ColumnGenerator::Identity));
    assert!(!note.columns[0].nullable, "a key column is NOT NULL");
    // The rows are still there…
    let kept = rows(&driver, Select::from(Source::table("note")).into()).await?;
    assert_eq!(kept.len(), 2);
    // …the index the table had is still there…
    assert!(note.constraints.iter().any(|c| c.name == "note_body_idx"));
    // …and the key now numbers itself, past the rows that were already there.
    let inserted = rows(
        &driver,
        Insert::row("note", vec!["body".into()], vec![Expr::lit("third")])
            .returning(vec![Projection::expr(Expr::col("id"))])
            .into(),
    )
    .await?;
    assert_eq!(inserted[0].get("id"), Some(&Value::Int(3)));
    Ok(())
}

/// `NOT NULL` toggled on an existing column: another rebuild, and one that has
/// to fail — leaving the table as it was — while a row still holds a null.
#[tokio::test]
async fn a_column_can_be_made_required_and_optional_again() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    let insert = |title: &str, pages: Option<i64>| -> Statement {
        Insert::row(
            "book",
            vec!["title".into(), "pages".into()],
            vec![
                Expr::lit(title),
                Expr::lit(pages.map_or(Value::Null, Value::Int)),
            ],
        )
        .into()
    };
    let nullable = |tables: &[sc_db::PhysicalTable], column: &str| {
        let book = tables.iter().find(|t| t.name == "book").expect("book");
        book.columns
            .iter()
            .find(|c| c.name == column)
            .expect("column")
            .nullable
    };
    rows(&driver, insert("Dune", None)).await?;

    let require_pages = SchemaChange::SetColumnNullable {
        table: "book".into(),
        column: "pages".into(),
        nullable: false,
    };
    let err = driver
        .apply_schema(&require_pages)
        .await
        .expect_err("a null is in the way");
    assert!(format!("{err}").contains("NOT NULL"), "{err}");
    assert!(nullable(&driver.introspect().await?, "pages"));
    assert_eq!(
        rows(&driver, Select::from(Source::table("book")).into())
            .await?
            .len(),
        1,
        "the failed rebuild kept the row"
    );

    rows(
        &driver,
        Update::new("book", vec![Assignment::new("pages", Expr::lit(412_i64))]).into(),
    )
    .await?;
    driver.apply_schema(&require_pages).await?;
    assert!(!nullable(&driver.introspect().await?, "pages"));
    assert!(rows(&driver, insert("Emma", None)).await.is_err());

    // And back: the title was declared NOT NULL when the table was created.
    driver
        .apply_schema(&SchemaChange::SetColumnNullable {
            table: "book".into(),
            column: "title".into(),
            nullable: true,
        })
        .await?;
    assert!(nullable(&driver.introspect().await?, "title"));

    // A key column never accepts nulls, and says so rather than being quietly
    // put back by the rebuild.
    let err = driver
        .apply_schema(&SchemaChange::SetColumnNullable {
            table: "book".into(),
            column: "id".into(),
            nullable: true,
        })
        .await
        .expect_err("a key column is NOT NULL");
    assert!(format!("{err}").contains("primary key"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_column_reference_is_repointed_and_removed() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    for (name, column) in [
        ("author", ColumnDef::new("id", "int8").not_null().identity()),
        (
            "review",
            ColumnDef::new("about", "int8").references("book", "id"),
        ),
    ] {
        driver
            .apply_schema(&SchemaChange::CreateTable {
                name: name.into(),
                columns: vec![column],
                primary_key: if name == "author" {
                    vec!["id".into()]
                } else {
                    vec![]
                },
                unlogged: false,
            })
            .await?;
    }
    let target = |tables: &[sc_db::PhysicalTable]| {
        let review = tables.iter().find(|t| t.name == "review").expect("review");
        assert!(review.foreign_keys.len() <= 1, "{:?}", review.foreign_keys);
        review
            .foreign_keys
            .first()
            .map(|k| (k.referenced_table.clone(), k.columns.clone()))
    };
    let repoint = |to: Option<&str>| SchemaChange::SetColumnReference {
        table: "review".into(),
        column: "about".into(),
        references: to.map(|t| sc_db::ColumnRef {
            table: t.into(),
            column: "id".into(),
        }),
    };

    driver.apply_schema(&repoint(Some("author"))).await?;
    assert_eq!(
        target(&driver.introspect().await?),
        Some(("author".into(), vec!["about".into()]))
    );

    // A value the new target does not have makes the rebuild fail, and the key
    // stays where it was.
    rows(
        &driver,
        Insert::row("book", vec!["title".into()], vec![Expr::lit("Dune")]).into(),
    )
    .await?;
    driver.apply_schema(&repoint(None)).await?;
    assert_eq!(target(&driver.introspect().await?), None);
    rows(
        &driver,
        Insert::row("review", vec!["about".into()], vec![Expr::lit(1_i64)]).into(),
    )
    .await?;
    driver
        .apply_schema(&repoint(Some("author")))
        .await
        .expect_err("no author 1");
    assert_eq!(target(&driver.introspect().await?), None);
    driver.apply_schema(&repoint(Some("book"))).await?;
    assert_eq!(
        target(&driver.introspect().await?),
        Some(("book".into(), vec!["about".into()]))
    );
    Ok(())
}

#[tokio::test]
async fn columns_are_added_and_dropped() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    driver
        .apply_schema(&SchemaChange::AddColumn {
            table: "book".into(),
            column: ColumnDef::new("isbn", "text").unique(),
        })
        .await?;
    // The unique column arrived as a unique index, which is the only way SQLite
    // will accept one — and it is enforced.
    rows(
        &driver,
        Insert::row(
            "book",
            vec!["title".into(), "isbn".into()],
            vec![Expr::lit("Orlando"), Expr::lit("978")],
        )
        .into(),
    )
    .await?;
    let error = rows(
        &driver,
        Insert::row(
            "book",
            vec!["title".into(), "isbn".into()],
            vec![Expr::lit("The Waves"), Expr::lit("978")],
        )
        .into(),
    )
    .await
    .expect_err("the ISBN is taken");
    assert!(format!("{error}").contains("23505"));

    driver
        .apply_schema(&SchemaChange::DropColumn {
            table: "book".into(),
            column: "pages".into(),
            if_exists: false,
        })
        .await?;
    let tables = driver.introspect().await?;
    let book = tables.iter().find(|t| t.name == "book").expect("book");
    assert!(!book.columns.iter().any(|c| c.name == "pages"));

    // `IF EXISTS` on a column that is already gone is a no-op, not an error —
    // SQLite has no such clause, so the driver answers the question itself.
    driver
        .apply_schema(&SchemaChange::DropColumn {
            table: "book".into(),
            column: "pages".into(),
            if_exists: true,
        })
        .await?;
    let error = driver
        .apply_schema(&SchemaChange::DropColumn {
            table: "book".into(),
            column: "pages".into(),
            if_exists: false,
        })
        .await
        .expect_err("without IF EXISTS it is an error");
    assert!(format!("{error}").contains("pages"));
    Ok(())
}

#[tokio::test]
async fn a_transaction_commits_or_rolls_back() -> Result<()> {
    let driver = driver();
    books(&driver).await?;

    let mut tx = driver.begin().await?;
    tx.query(&Insert::row("book", vec!["title".into()], vec![Expr::lit("Orlando")]).into())
        .await?
        .try_collect()
        .await?;
    tx.commit().await?;
    assert_eq!(
        rows(&driver, Select::from(Source::table("book")).into())
            .await?
            .len(),
        1
    );

    let mut tx = driver.begin().await?;
    tx.query(&Insert::row("book", vec!["title".into()], vec![Expr::lit("The Waves")]).into())
        .await?
        .try_collect()
        .await?;
    tx.rollback().await?;
    assert_eq!(
        rows(&driver, Select::from(Source::table("book")).into())
            .await?
            .len(),
        1,
        "the rolled-back insert left nothing behind"
    );

    // A transaction abandoned without either verb rolls back, and its
    // connection goes back to the pool usable.
    {
        let mut tx = driver.begin().await?;
        tx.query(&Insert::row("book", vec!["title".into()], vec![Expr::lit("Flush")]).into())
            .await?
            .try_collect()
            .await?;
    }
    assert_eq!(
        rows(&driver, Select::from(Source::table("book")).into())
            .await?
            .len(),
        1
    );

    // A schema change inside a transaction is part of it.
    let mut tx = driver.begin().await?;
    tx.apply_schema(&SchemaChange::AddColumn {
        table: "book".into(),
        column: ColumnDef::new("subtitle", "text"),
    })
    .await?;
    tx.rollback().await?;
    let tables = driver.introspect().await?;
    let book = tables.iter().find(|t| t.name == "book").expect("book");
    assert!(!book.columns.iter().any(|c| c.name == "subtitle"));
    Ok(())
}

/// What the generated `schema.sql` in an application's project is written from.
#[tokio::test]
async fn ddl_is_rendered_without_being_run() -> Result<()> {
    let driver = driver();
    let sql = driver.render_ddl(&SchemaChange::CreateTable {
        name: "book".into(),
        columns: vec![
            ColumnDef::new("id", "int8").not_null().identity(),
            ColumnDef::new("title", "text").not_null(),
        ],
        primary_key: vec!["id".into()],
        unlogged: false,
    })?;
    assert_eq!(
        sql,
        "CREATE TABLE \"book\" (\"id\" INTEGER PRIMARY KEY, \"title\" text NOT NULL)"
    );
    // Rendering is not running.
    assert!(driver.introspect().await?.is_empty());
    Ok(())
}

/// A custom SQL query is typed by the database (§13.4), and one that will not
/// prepare is refused while its author is still looking at it.
#[tokio::test]
async fn a_statement_is_described_and_a_broken_one_refused() -> Result<()> {
    let driver = driver();
    books(&driver).await?;

    let described = driver
        .describe(
            "SELECT title, pages FROM book WHERE pages > ?1",
            &["int8".into()],
        )
        .await?;
    assert_eq!(described.len(), 2);
    assert_eq!(described[0].name, "title");
    assert!(described[0].sql_type.eq_ignore_ascii_case("text"));
    assert!(
        described[1].sql_type.eq_ignore_ascii_case("int8"),
        "a type name SQLite does not know is kept as it was written: {}",
        described[1].sql_type
    );

    // An expression has no declared type, which is the honest answer: SQLite
    // decides one per value.
    let counted = driver
        .describe("SELECT count(*) AS n FROM book", &[])
        .await?;
    assert_eq!(counted[0].name, "n");
    assert_eq!(counted[0].sql_type, "");

    let error = driver
        .describe("SELECT titel FROM book", &[])
        .await
        .expect_err("there is no `titel`");
    assert!(format!("{error}").contains("titel"), "{error}");
    Ok(())
}

/// Admin-authored SQL with named parameters — a custom SQL query (§13.4),
/// which is the one hole in the AST and the one place the placeholders are
/// written by the query layer rather than by the renderer.
#[tokio::test]
async fn a_raw_statement_runs_with_its_named_parameters() -> Result<()> {
    let driver = driver();
    books(&driver).await?;
    for (title, pages) in [("Orlando", 288_i64), ("The Waves", 228)] {
        rows(
            &driver,
            Insert::row(
                "book",
                vec!["title".into(), "pages".into()],
                vec![Expr::lit(title), Expr::lit(pages)],
            )
            .into(),
        )
        .await?;
    }

    let named = sc_query::rewrite_named_params(
        driver.dialect(),
        "SELECT title FROM book WHERE pages > :least ORDER BY title",
    )?;
    assert_eq!(named.params, vec!["least"]);
    let found = rows(
        &driver,
        Statement::raw(named.sql.clone(), vec![Value::Int(250)]),
    )
    .await?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].get("title"), Some(&Value::Text("Orlando".into())));

    // The same statement is what `describe` types the query from.
    let described = driver.describe(&named.sql, &["int8".into()]).await?;
    assert_eq!(described.len(), 1);
    assert_eq!(described[0].name, "title");
    Ok(())
}

/// A file on disk, opened twice: the driver is a database, not a process-local
/// cache, and a secondary connection must refuse a file that is not there rather
/// than create an empty one.
#[tokio::test]
async fn a_file_is_created_reopened_and_never_invented() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("sc-sqlite-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("app.sqlite");

    let missing = SqliteDriver::open_existing(&path)
        .expect_err("a connection to a file that is not there is a mistake, not a new database");
    assert!(format!("{missing}").contains("no SQLite database"));

    {
        let driver = SqliteDriver::open(&path)?;
        books(&driver).await?;
        rows(
            &driver,
            Insert::row("book", vec!["title".into()], vec![Expr::lit("Orlando")]).into(),
        )
        .await?;
    }
    assert!(path.is_file(), "the primary database creates its file");

    let reopened = SqliteDriver::open_existing(&path)?;
    let read = rows(&reopened, Select::from(Source::table("book")).into()).await?;
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].get("title"), Some(&Value::Text("Orlando".into())));

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// The driver is held as a trait object by the catalog, and its capabilities are
/// what the layers above branch on.
#[tokio::test]
async fn the_driver_is_a_database_driver_that_says_what_it_cannot_do() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(driver());
    let caps = driver.capabilities();
    assert!(caps.composite_pk);
    assert!(caps.returning);
    assert!(!caps.row_level_security, "SQLite has no policies");
    assert!(!caps.listen_notify);
    assert!(!caps.unlogged_tables);

    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "member".into(),
            columns: vec![
                ColumnDef::new("org", "int8").not_null(),
                ColumnDef::new("person", "int8").not_null(),
            ],
            primary_key: vec!["org".into(), "person".into()],
            unlogged: false,
        })
        .await?;
    let tables = driver.introspect().await?;
    assert_eq!(tables[0].primary_key, vec!["org", "person"]);

    // A transaction-local setting is kept in the settings table, which only
    // the change log creates (no policy reads it here). Without the table
    // there is nothing to keep it in, and nothing reads it.
    let mut tx = driver.begin().await?;
    tx.set_local("sc.role", "1").await?;
    tx.commit().await?;
    let tables = driver.introspect().await?;
    assert!(!tables.iter().any(|t| t.name == sc_db::TX_SETTINGS_TABLE));
    Ok(())
}

/// With the settings table there, a setting is a row while its transaction
/// is open and gone once it commits, so no other writer ever reads it; a
/// read-only transaction keeps none.
#[tokio::test]
async fn a_transaction_local_setting_lives_as_long_as_its_transaction() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(driver());
    let settings = sc_db::TX_SETTINGS_TABLE;
    let mut tx = driver.begin().await?;
    tx.batch(&format!(
        "CREATE TABLE {settings} (name TEXT PRIMARY KEY, value TEXT)"
    ))
    .await?;
    tx.commit().await?;

    let read = |sql: String| Statement::raw(sql, Vec::new());
    let mut tx = driver.begin().await?;
    tx.set_local("sc.changeset", "{\"id\":\"a\"}").await?;
    tx.set_local("sc.changeset", "{\"id\":\"b\"}").await?;
    let inside = tx
        .query(&read(format!("SELECT value FROM {settings}")))
        .await?
        .try_collect()
        .await?;
    assert_eq!(inside.len(), 1);
    assert_eq!(
        inside[0].get("value"),
        Some(&Value::Text("{\"id\":\"b\"}".into()))
    );
    tx.commit().await?;
    let after = driver
        .query(&read(format!("SELECT value FROM {settings}")))
        .await?
        .try_collect()
        .await?;
    assert!(after.is_empty());

    let mut tx = driver.begin().await?;
    tx.set_read_only().await?;
    tx.set_local("sc.role", "1").await?;
    tx.commit().await?;
    // And the connection it used still writes.
    let mut tx = driver.begin().await?;
    tx.set_local("sc.role", "1").await?;
    tx.rollback().await?;
    Ok(())
}

/// Several queries at once, on one database — the reason there is a pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queries_do_not_queue_behind_one_connection() -> Result<()> {
    let driver = Arc::new(driver());
    books(&driver).await?;
    for i in 0..10_i64 {
        rows(
            &driver,
            Insert::row(
                "book",
                vec!["title".into(), "pages".into()],
                vec![Expr::lit(format!("book {i}")), Expr::lit(i)],
            )
            .into(),
        )
        .await?;
    }

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let driver = driver.clone();
        tasks.push(tokio::spawn(async move {
            let stmt: Statement = Select::from(Source::table("book")).into();
            driver.query(&stmt).await?.try_collect().await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("task")?.len(), 10);
    }
    Ok(())
}
