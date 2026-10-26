//! Model-written JavaScript (TODO-agent-skills AS5.4), on the real engine and a
//! real database: only what is granted is in scope, `db` reads and writes as
//! the caller, a read-only grant refuses a write, a timeout ends only the one
//! evaluation, and every evaluation is kept with its source.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use crate::common;

use common::{Env, as_user};
use sc_core_traits::sandbox::{Grants, SandboxLimits, TableAccess, run_model_code};
use sc_error::Result;
use serde_json::{Value as Json, json};

async fn env() -> Result<Env> {
    let env = Env::new().await?.with_engine();
    // Each reader sees their own books, and nobody else's.
    env.own("books", "owner === user.email").await?;
    Ok(env)
}

fn tables(access: TableAccess) -> Grants {
    Grants {
        tables: access,
        ..Grants::default()
    }
}

#[tokio::test]
async fn a_grant_not_given_is_a_reference_error_the_model_can_read() -> Result<()> {
    let env = env().await?;
    let caller = as_user("ada@example.com");
    let mut state = Json::Null;
    let mut ctx = env.context(&caller, &mut state);
    for (surface, source) in [
        ("db", "return await db.books.count()"),
        (
            "fetch",
            "return (await fetch('https://example.com')).status",
        ),
        ("fs", "return await fs('x').open('a').text()"),
        ("trigger", "return await trigger('x').run()"),
    ] {
        let err = run_model_code(
            &mut ctx,
            "run_code",
            source,
            BTreeMap::new(),
            &Grants::default(),
            &SandboxLimits::default(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("ReferenceError") && err.contains(surface),
            "{surface}: {err}"
        );
    }
    // Arguments are in scope, and a plain computation needs no grant.
    let args = BTreeMap::from([("n".to_owned(), json!(20))]);
    let value = run_model_code(
        &mut ctx,
        "run_code",
        "return n * 2 + 2",
        args,
        &Grants::default(),
        &SandboxLimits::default(),
    )
    .await?;
    assert_eq!(value, json!(42));
    // Every evaluation is kept with its source, failed or not.
    let code = &ctx.extras.code;
    assert_eq!(code.len(), 5);
    assert!(!code[0].ok && code[0].error.is_some());
    assert_eq!(code[4].source, "return n * 2 + 2");
    assert!(code[4].ok && code[4].output_bytes > 0);
    Ok(())
}

#[tokio::test]
async fn tables_are_read_as_the_caller_and_a_read_only_grant_refuses_a_write() -> Result<()> {
    let env = env().await?;
    let caller = as_user("ada@example.com");
    let mut state = Json::Null;
    let mut ctx = env.context(&caller, &mut state);
    // Asking for the admin's authority changes nothing: the caller is the
    // ceiling.
    let titles = run_model_code(
        &mut ctx,
        "run_code",
        "const rows = await db.books.orderBy('id').rows(); return rows.map(r => r.title)",
        BTreeMap::new(),
        &tables(TableAccess::ReadOnly),
        &SandboxLimits::default(),
    )
    .await?;
    assert_eq!(titles, json!(["Dune", "Ilium"]));

    let err = run_model_code(
        &mut ctx,
        "run_code",
        "await db.books.insert({ id: 9, title: 'Mine', owner: 'ada@example.com' }); return 1",
        BTreeMap::new(),
        &tables(TableAccess::ReadOnly),
        &SandboxLimits::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("read-only"), "{err}");
    let err = run_model_code(
        &mut ctx,
        "run_code",
        "return await db.sql('select 1', [])",
        BTreeMap::new(),
        &tables(TableAccess::ReadWrite),
        &SandboxLimits::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not available"), "{err}");

    // A read-write grant writes, as the caller.
    run_model_code(
        &mut ctx,
        "run_code",
        "await db.books.where({ id: 1 }).update({ pages: 500 }); return true",
        BTreeMap::new(),
        &tables(TableAccess::ReadWrite),
        &SandboxLimits::default(),
    )
    .await?;
    let rows = env.rows("books").await?;
    let dune = rows.iter().find(|r| r["id"] == json!(1)).unwrap();
    assert_eq!(dune["pages"], json!(500));
    // An allow-list keeps the other tables out.
    let grants = Grants {
        tables: TableAccess::ReadOnly,
        only_tables: vec!["authors".into()],
        ..Grants::default()
    };
    let err = run_model_code(
        &mut ctx,
        "run_code",
        "return await db.books.count()",
        BTreeMap::new(),
        &grants,
        &SandboxLimits::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not one this code may use"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_timeout_or_a_large_result_ends_only_that_evaluation() -> Result<()> {
    let env = env().await?;
    let caller = as_user("ada@example.com");
    let mut state = Json::Null;
    let mut ctx = env.context(&caller, &mut state);
    let quick = SandboxLimits {
        timeout: Duration::from_millis(500),
        ..SandboxLimits::default()
    };
    let started = std::time::Instant::now();
    let err = run_model_code(
        &mut ctx,
        "run_code",
        "for (;;) {}",
        BTreeMap::new(),
        &Grants::default(),
        &quick,
    )
    .await;
    assert!(err.is_err());
    assert!(started.elapsed() < Duration::from_secs(10));

    let err = run_model_code(
        &mut ctx,
        "run_code",
        "return 'x'.repeat(100000)",
        BTreeMap::new(),
        &Grants::default(),
        &SandboxLimits::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("may return at most"), "{err}");

    // The engine is still there for the next one.
    let value = run_model_code(
        &mut ctx,
        "run_code",
        "return 'still here'",
        BTreeMap::new(),
        &Grants::default(),
        &SandboxLimits::default(),
    )
    .await?;
    assert_eq!(value, json!("still here"));
    Ok(())
}
