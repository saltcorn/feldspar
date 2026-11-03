//! A caller may always read their own `users` row (§7.3): the self-read rule
//! `_read && id === user.id` is ORed with whatever the admin's ownership
//! formula grants, on every read path, and grants no write.
//!
//! Over a private in-memory SQLite database, like `scoped.rs`. The table's
//! floors and formula are set on the in-memory `Table` handed to each call.

use std::sync::Arc;

use sc_access::Scoped;
use sc_access::ownership::{self, RowImage};
use sc_access::rows::RowQuery;
use sc_auth::User;
use sc_catalog::{Authority, Catalog, DataField, Principal, Table};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::{Repr, Result};
use sc_query::{Expr, Value};
use sc_types::{BasicType, Role, TypeRef};
use serde_json::{Value as Json, json};
use uuid::Uuid;

const ALICE: &str = "00000000-0000-4000-8000-00000000000a";
const BOB: &str = "00000000-0000-4000-8000-00000000000b";
const CAROL: &str = "00000000-0000-4000-8000-00000000000c";

/// A catalog with a `users` table of three staff accounts, its floors at their
/// default (admin), and the guard that tears the catalog down.
async fn users() -> Result<(Arc<Catalog>, sc_test_harness::Teardown, Table)> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    let teardown = sc_test_harness::Teardown::new();
    teardown.add(catalog.teardown_hook());
    catalog
        .create_table(
            "users",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
                    .required()
                    .primary_key(),
                DataField::plain("email", TypeRef::Basic(BasicType::Text)),
                DataField::plain("role", TypeRef::Basic(BasicType::Int)).required(),
                DataField::plain("department", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    let table = catalog.require("users")?;
    assert_eq!(table.access.min_role_read, Role::ADMIN);
    assert!(table.ownership.is_none());
    let system = Scoped::system(&catalog, "test: seed");
    for (id, email, department) in [
        (ALICE, "alice@x", "ops"),
        (BOB, "bob@x", "ops"),
        (CAROL, "carol@x", "sales"),
    ] {
        system
            .insert(
                &table,
                &json!({ "id": id, "email": email, "role": 80, "department": department }),
            )
            .await?;
    }
    Ok((catalog, teardown, table))
}

/// A signed-in staff caller (role 80) as the row layer carries them.
fn staff(id: &str, department: &str) -> Principal {
    Principal::new(
        Role::of(80),
        Some(json!({ "id": id, "role": 80, "department": department })),
    )
}

fn emails(rows: &Json) -> Vec<&str> {
    let mut out: Vec<&str> = rows
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["email"].as_str()).collect())
        .unwrap_or_default();
    out.sort_unstable();
    out
}

fn by_id(id: &str) -> RowQuery {
    RowQuery::new().where_(Some(
        Expr::col("id").eq(Expr::lit(Value::Uuid(id.parse().unwrap()))),
    ))
}

#[tokio::test]
async fn a_caller_below_the_floor_reads_their_own_row_and_nobody_elses() -> Result<()> {
    let (catalog, _teardown, table) = users().await?;
    let alice = staff(ALICE, "ops");
    let scoped = Scoped::new(&catalog, Authority::Caller(&alice));

    // The "me" query: by key, and as a list of everything.
    assert_eq!(
        emails(&scoped.read(&table, &by_id(ALICE)).await?),
        ["alice@x"]
    );
    assert_eq!(
        emails(&scoped.read(&table, &RowQuery::new()).await?),
        ["alice@x"]
    );
    // Another account is absent, not forbidden: the same empty answer.
    assert_eq!(
        emails(&scoped.read(&table, &by_id(BOB)).await?),
        Vec::<&str>::new()
    );

    // Nobody is nobody's own row.
    let public = Principal::anonymous(Role::PUBLIC);
    let scoped = Scoped::new(&catalog, Authority::Caller(&public));
    assert_eq!(
        emails(&scoped.read(&table, &RowQuery::new()).await?),
        Vec::<&str>::new()
    );

    // Other tables are as they were: below the floor with no formula, refused.
    let rule = table
        .read_rule()
        .expect("the users table always has a read rule");
    assert_eq!(rule.source(), sc_catalog::SELF_READ_FORMULA);
    let mut notes = table.clone();
    notes.name = "notes".into();
    assert!(notes.read_rule().is_none());
    Ok(())
}

#[tokio::test]
async fn the_self_read_grants_no_write() -> Result<()> {
    let (catalog, _teardown, table) = users().await?;
    let alice = staff(ALICE, "ops");
    let scoped = Scoped::new(&catalog, Authority::Caller(&alice));

    // Below the write floor with no formula: the floor's refusal, as before.
    let update = scoped
        .update(&table, ALICE, &json!({ "department": "sales" }))
        .await
        .unwrap_err();
    assert!(matches!(update.repr(), Repr::Auth(_)), "{update}");
    let delete = scoped.delete(&table, ALICE).await.unwrap_err();
    assert!(matches!(delete.repr(), Repr::Auth(_)), "{delete}");

    Ok(())
}

#[tokio::test]
async fn an_admin_formula_widens_the_self_read_and_never_narrows_it() -> Result<()> {
    let (catalog, _teardown, mut table) = users().await?;
    // The department sees its own staff; nothing grants anyone else.
    table.ownership = Some(sc_expr::Formula::parse(
        "_read && department === user.department",
    )?);

    let alice = staff(ALICE, "ops");
    let scoped = Scoped::new(&catalog, Authority::Caller(&alice));
    assert_eq!(
        emails(&scoped.read(&table, &RowQuery::new()).await?),
        ["alice@x", "bob@x"]
    );

    // A caller the formula grants nothing (their session says another
    // department than their row does) still reads their own row.
    let moved = staff(CAROL, "marketing");
    let scoped = Scoped::new(&catalog, Authority::Caller(&moved));
    assert_eq!(
        emails(&scoped.read(&table, &RowQuery::new()).await?),
        ["carol@x"]
    );

    // An aggregate counts what a read would return.
    let count = vec![sc_query::Projection::expr_as(
        Expr::Agg {
            func: "count".to_owned(),
            distinct: false,
            args: Vec::new(),
        },
        "n",
    )];
    let count = scoped
        .aggregate_grouped(&table, count, &RowQuery::new())
        .await?
        .remove(0);
    assert_eq!(count.get("n"), Some(&Value::Int(1)));
    Ok(())
}

#[tokio::test]
async fn an_embed_of_users_stays_refused_below_the_floor() -> Result<()> {
    let (_catalog, _teardown, table) = users().await?;
    // `created_by(email)` reads somebody else's row through a key: the
    // self-read is decided row by row, so it cannot ride in a join subquery.
    let Err(refused) = ownership::join_guard(&table, Role::of(80)) else {
        panic!("a staff embed of users was allowed");
    };
    assert!(
        refused.to_string().contains("ownership formula"),
        "{refused}"
    );
    assert!(ownership::join_guard(&table, Role::ADMIN).is_ok());
    Ok(())
}

#[tokio::test]
async fn a_changed_row_reaches_its_own_user_without_an_evaluator() -> Result<()> {
    let (catalog, _teardown, table) = users().await?;
    let alice = User::new(Uuid::parse_str(ALICE).unwrap(), Role::of(80));
    let row = json!({ "id": ALICE, "email": "alice@x", "role": 80, "department": "ops" });

    let mut image = RowImage::new(&table, &row);
    assert!(!image.needs_evaluator(Role::of(80)));
    assert!(
        image
            .readable(&catalog, Role::of(80), Some(&alice), None)
            .await?
    );
    let bob = User::new(Uuid::parse_str(BOB).unwrap(), Role::of(80));
    assert!(
        !image
            .readable(&catalog, Role::of(80), Some(&bob), None)
            .await?
    );
    assert!(!image.readable(&catalog, Role::PUBLIC, None, None).await?);
    Ok(())
}
