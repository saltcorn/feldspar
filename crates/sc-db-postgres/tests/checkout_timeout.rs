//! A checkout from a pool whose connections are all held fails after the
//! driver's checkout timeout, with the pool's state in the message, rather than
//! waiting for ever — the admin sign-in that hung with its button disabled was a
//! request waiting on a connection that never came free.

use std::time::{Duration, Instant};

use sc_db_postgres::PgDriver;
use sc_test_harness::TestDb;

#[tokio::test]
async fn an_exhausted_pool_times_out_instead_of_hanging() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let pool = db.pool().clone();
    let driver =
        PgDriver::from_pool(pool.clone()).with_checkout_timeout(Duration::from_millis(300));

    // Every connection the pool can hand out, held.
    let mut held = Vec::new();
    for _ in 0..pool.status().max_size {
        held.push(
            pool.get()
                .await
                .map_err(|e| sc_error::Error::database(e.to_string()))?,
        );
    }

    let started = Instant::now();
    let err = match driver.introspect().await {
        Ok(_) => panic!("a query on an exhausted pool should fail"),
        Err(e) => e.to_string(),
    };
    let waited = started.elapsed();
    assert!(err.contains("no database connection came free"), "{err}");
    assert!(err.contains("connections are in use"), "{err}");
    assert!(
        waited >= Duration::from_millis(300),
        "gave up early: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "waited far past the timeout: {waited:?}"
    );

    // A connection coming free is all it takes for the next query to work.
    held.pop();
    driver.introspect().await?;
    Ok(())
}
