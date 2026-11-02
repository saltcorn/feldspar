//! `Scoped` and `Unchecked` (TODO-code-quality Q3.3, Q3.5): a caller's rows go
//! through the ownership rule, the system's and an administrator's do not, and
//! the unchecked row layer cannot be reached with a caller's authority alone.
//!
//! Over a private in-memory SQLite database: no server, no engine. The table's
//! floors and formula are set on the in-memory `Table` handed to each call, which
//! is what the row layer decides by.

use std::sync::Arc;

use sc_access::Scoped;
use sc_access::rows::{self, RowQuery};
use sc_catalog::{Authority, Catalog, DataField, Principal, Table, Unchecked};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::{Repr, Result};
use sc_types::{BasicType, Role, TypeRef};
use serde_json::{Value as Json, json};

/// A catalog over an in-memory SQLite database with one `notes` table, and the
/// guard that tears the catalog down.
async fn notes() -> Result<(Arc<Catalog>, sc_test_harness::Teardown)> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    let teardown = sc_test_harness::Teardown::new();
    teardown.add(catalog.teardown_hook());
    catalog
        .create_table(
            "notes",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
                DataField::plain("owner", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    Ok((catalog, teardown))
}

/// `notes` with staff-only floors (role 40) for reading and writing.
fn staff_only(catalog: &Catalog) -> Result<Table> {
    let mut table = catalog.require("notes")?;
    table.access.min_role_read = Role::of(40);
    table.access.min_role_write = Role::of(40);
    Ok(table)
}

/// A signed-in caller at `role`, with the email an ownership formula reads.
fn caller(role: Role, email: &str) -> Principal {
    Principal::new(
        role,
        Some(json!({ "id": uuid::Uuid::new_v4().to_string(), "email": email })),
    )
}

fn titles(rows: &Json) -> Vec<&str> {
    rows.as_array()
        .map(|rows| rows.iter().filter_map(|r| r["title"].as_str()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_caller_below_the_floor_is_refused_and_the_system_is_not() -> Result<()> {
    let (catalog, _teardown) = notes().await?;
    let table = staff_only(&catalog)?;

    // The system writes and reads whatever the floors say.
    let system = Scoped::system(&catalog, "test: seed");
    system
        .insert(&table, &json!({ "title": "minutes", "owner": "a@x" }))
        .await?;
    assert_eq!(
        titles(&system.read(&table, &RowQuery::new()).await?),
        ["minutes"]
    );

    // A public caller is below both floors, and there is no formula to extend
    // them: refused, by the rule, not by luck.
    let public = Principal::anonymous(Role::PUBLIC);
    let scoped = Scoped::new(&catalog, Authority::Caller(&public));
    let read = scoped.read(&table, &RowQuery::new()).await.unwrap_err();
    assert!(matches!(read.repr(), Repr::Auth(_)), "{read}");
    let insert = scoped
        .insert(&table, &json!({ "title": "graffiti" }))
        .await
        .unwrap_err();
    assert!(matches!(insert.repr(), Repr::Auth(_)), "{insert}");

    // Staff meet both.
    let staff = caller(Role::of(40), "s@x");
    let scoped = Scoped::new(&catalog, Authority::Caller(&staff));
    let row = scoped
        .insert(&table, &json!({ "title": "agenda", "owner": "s@x" }))
        .await?;
    let id = row["id"].to_string();
    scoped
        .update(&table, &id, &json!({ "title": "agenda (final)" }))
        .await?;
    let mut seen = titles(&scoped.read(&table, &RowQuery::new()).await?)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    seen.sort();
    assert_eq!(seen, ["agenda (final)", "minutes"]);
    scoped.delete(&table, &id).await?;
    assert_eq!(
        titles(&scoped.read(&table, &RowQuery::new()).await?),
        ["minutes"]
    );
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_narrows_a_callers_read() -> Result<()> {
    let (catalog, _teardown) = notes().await?;
    let mut table = staff_only(&catalog)?;
    table.ownership = Some(sc_expr::Formula::parse("owner === user.email")?);
    let system = Scoped::system(&catalog, "test: seed");
    for (title, owner) in [("mine", "a@x"), ("theirs", "b@x")] {
        system
            .insert(&table, &json!({ "title": title, "owner": owner }))
            .await?;
    }

    // Below the floor, the formula grants the caller their own row only —
    // translated into the read, so no engine is needed.
    let alice = caller(Role::of(80), "a@x");
    let scoped = Scoped::new(&catalog, Authority::Caller(&alice));
    assert_eq!(
        titles(&scoped.read(&table, &RowQuery::new()).await?),
        ["mine"]
    );

    // An administrator, like the system, sees every row.
    let admin = caller(Role::ADMIN, "root@x");
    let scoped = Scoped::new(&catalog, Authority::Caller(&admin));
    assert_eq!(
        scoped
            .read(&table, &RowQuery::new())
            .await?
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    Ok(())
}

#[tokio::test]
async fn only_the_system_and_an_administrator_make_an_unchecked_token() -> Result<()> {
    let (catalog, _teardown) = notes().await?;
    let admin = caller(Role::ADMIN, "root@x");
    let staff = caller(Role::of(40), "s@x");

    assert!(Authority::system("test").unchecked().is_some());
    assert!(Authority::Caller(&admin).unchecked().is_some());
    assert!(Authority::Caller(&staff).unchecked().is_none());
    assert!(Unchecked::admin(&staff).is_none());
    assert!(sc_access::admin_access(&staff).is_err());
    assert!(
        Scoped::new(&catalog, Authority::Caller(&staff))
            .unchecked()
            .is_none()
    );

    // A vouched caller names the check that stands behind it, and still carries
    // the caller (the RLS policies' role and user, the event's cause).
    let vouched = Unchecked::vouched(&staff, "a test's own check");
    assert_eq!(vouched.vouched_by(), Some("a test's own check"));
    assert_eq!(vouched.principal(), Some(&staff));
    assert_eq!(Unchecked::system("test").vouched_by(), None);

    // The unchecked row layer runs with the token it is handed: a write the
    // floor would refuse a public caller is the system's to make.
    let table = staff_only(&catalog)?;
    rows::create_row(
        &catalog,
        &table,
        &json!({ "title": "from a job" }),
        Unchecked::system("test: a system write"),
    )
    .await?;
    let all = rows::list_rows(&catalog, &table, Unchecked::system("test: read back")).await?;
    assert_eq!(titles(&all), ["from a job"]);
    Ok(())
}

/// TODO-code-quality Q3.4: a holder that loaded its caller's user (an agent
/// run, a code body's `forUser`) hands it on, and the formula decides with it;
/// a user that is not the principal's is refused rather than used. A caller's
/// aggregate goes through the same rule as their read.
#[tokio::test]
async fn a_scope_decides_with_the_loaded_user_and_refuses_anyone_elses() -> Result<()> {
    let (catalog, _teardown) = notes().await?;
    let mut table = staff_only(&catalog)?;
    table.ownership = Some(sc_expr::Formula::parse("owner === user.email")?);
    let system = Scoped::system(&catalog, "test: seed");
    for (title, owner) in [("mine", "a@x"), ("also mine", "a@x"), ("theirs", "b@x")] {
        system
            .insert(&table, &json!({ "title": title, "owner": owner }))
            .await?;
    }

    let alice = caller(Role::of(80), "a@x");
    let loaded = sc_auth::User::from_json(alice.role, alice.user.as_ref())?;
    let scoped = Scoped::new(&catalog, Authority::Caller(&alice)).with_user(loaded.as_ref());
    let mut seen = titles(&scoped.read(&table, &RowQuery::new()).await?)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    seen.sort();
    assert_eq!(seen, ["also mine", "mine"]);

    // Two of the three rows are hers, and the count says so.
    let count = vec![sc_query::Projection::expr_as(
        sc_query::Expr::Agg {
            func: "count".to_owned(),
            distinct: false,
            args: Vec::new(),
        },
        "n",
    )];
    let grouped = scoped
        .aggregate_grouped(&table, count.clone(), &RowQuery::new())
        .await?;
    assert_eq!(grouped[0].get("n"), Some(&sc_query::Value::Int(2)));
    // The public role is below the floor and has no user for the formula to
    // grant a row to: it counts none, never the table.
    let public = Principal::anonymous(Role::PUBLIC);
    let none = Scoped::new(&catalog, Authority::Caller(&public))
        .aggregate_grouped(&table, count, &RowQuery::new())
        .await?;
    assert_eq!(none[0].get("n"), Some(&sc_query::Value::Int(0)));

    // Somebody else's user on alice's principal decides nothing.
    let bob = caller(Role::of(80), "b@x");
    let bobs = sc_auth::User::from_json(bob.role, bob.user.as_ref())?;
    let mixed = Scoped::new(&catalog, Authority::Caller(&alice)).with_user(bobs.as_ref());
    assert!(mixed.read(&table, &RowQuery::new()).await.is_err());
    Ok(())
}
