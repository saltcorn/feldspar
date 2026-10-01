//! SQL functions Postgres has and this build of SQLite does not, registered on
//! every connection so one rendered statement means the same on both.
//!
//! The bundled amalgamation is compiled without `SQLITE_ENABLE_MATH_FUNCTIONS`,
//! so it has no `sqrt` or `floor`, and SQLite has never had the sample standard
//! deviation. A dataset's Aggregate offers "standard deviation" as a summary
//! (analytics TODO A1.4) and compiles it to `stddev_samp(x)`, and a plot's bins
//! and percentiles (A2.3) are `floor((x - origin) / width)` and
//! `floor((n - 1) * p)`; rather than a second spelling for SQLite, the
//! functions are supplied here.
//!
//! Each follows Postgres exactly where the two could differ: `NULL` inputs are
//! skipped, fewer than two values give `NULL`, and a negative argument to
//! `sqrt` is an error rather than `NaN`.

use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::{Connection, Error as SqlError, types::ValueRef};

/// Register this module's functions on `connection`.
pub(crate) fn register(connection: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    connection.create_scalar_function("sqrt", 1, flags, |ctx| {
        let Some(x) = number(ctx, 0)? else {
            return Ok(None);
        };
        if x < 0.0 {
            return Err(SqlError::UserFunctionError(
                "cannot take square root of a negative number".into(),
            ));
        }
        Ok(Some(x.sqrt()))
    })?;
    // `CAST(x AS integer)` is no substitute: SQLite truncates towards zero
    // where Postgres rounds, and neither is a floor below zero.
    connection
        .create_scalar_function("floor", 1, flags, |ctx| Ok(number(ctx, 0)?.map(f64::floor)))?;
    connection.create_aggregate_function("stddev_samp", 1, flags, StddevSamp)?;
    Ok(())
}

/// Argument `i` as a number: `None` for `NULL`, an error for text or bytes.
fn number(ctx: &Context<'_>, i: usize) -> rusqlite::Result<Option<f64>> {
    match ctx.get_raw(i) {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(n) => Ok(Some(n as f64)),
        ValueRef::Real(x) => Ok(Some(x)),
        ValueRef::Text(_) | ValueRef::Blob(_) => Err(SqlError::UserFunctionError(
            "a standard deviation, square root or floor needs a number".into(),
        )),
    }
}

/// Welford's running mean and sum of squared deviations: stable where the
/// textbook `Σx² − (Σx)²/n` loses every digit to cancellation.
#[derive(Default)]
struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

/// `stddev_samp(x)`: the sample standard deviation of the non-null values.
struct StddevSamp;

impl Aggregate<Welford, Option<f64>> for StddevSamp {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<Welford> {
        Ok(Welford::default())
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Welford) -> rusqlite::Result<()> {
        if let Some(x) = number(ctx, 0)? {
            acc.n += 1;
            let delta = x - acc.mean;
            acc.mean += delta / acc.n as f64;
            acc.m2 += delta * (x - acc.mean);
        }
        Ok(())
    }

    fn finalize(&self, _: &mut Context<'_>, acc: Option<Welford>) -> rusqlite::Result<Option<f64>> {
        Ok(acc
            .filter(|w| w.n >= 2)
            .map(|w| (w.m2 / (w.n - 1) as f64).sqrt()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(conn: &Connection, sql: &str) -> Option<f64> {
        conn.query_row(sql, [], |row| row.get(0)).expect(sql)
    }

    #[test]
    fn stddev_samp_and_sqrt_agree_with_postgres() {
        let conn = Connection::open_in_memory().expect("open");
        register(&conn).expect("register");
        conn.execute_batch("CREATE TABLE t (x REAL); INSERT INTO t VALUES (2), (4), (4), (4), (5), (5), (7), (9), (NULL);")
            .expect("rows");
        // Postgres: select stddev_samp(x) from (values (2),(4),(4),(4),(5),(5),(7),(9)) v(x)
        // = 2.1380899352993950
        let sd = one(&conn, "SELECT stddev_samp(x) FROM t").expect("a value");
        assert!((sd - 2.138_089_935_299_395).abs() < 1e-12, "{sd}");
        assert_eq!(one(&conn, "SELECT stddev_samp(x) FROM t WHERE x = 2"), None);
        assert_eq!(
            one(&conn, "SELECT stddev_samp(x) FROM t WHERE x > 100"),
            None
        );
        assert_eq!(one(&conn, "SELECT sqrt(16)"), Some(4.0));
        assert_eq!(one(&conn, "SELECT sqrt(NULL)"), None);
        assert!(
            conn.query_row("SELECT sqrt(-1)", [], |r| r.get::<_, f64>(0))
                .is_err()
        );
    }

    #[test]
    fn floor_rounds_down_on_both_sides_of_zero() {
        let conn = Connection::open_in_memory().expect("open");
        register(&conn).expect("register");
        assert_eq!(one(&conn, "SELECT floor(2.7)"), Some(2.0));
        assert_eq!(one(&conn, "SELECT floor(-2.2)"), Some(-3.0));
        assert_eq!(one(&conn, "SELECT floor(5)"), Some(5.0));
        assert_eq!(one(&conn, "SELECT floor(NULL)"), None);
        // An integer compares equal to the floor it was computed from, which
        // is how a percentile picks its row: `rn = floor((n - 1) * p) + 1`.
        assert_eq!(one(&conn, "SELECT 3 = floor(3.5)"), Some(1.0));
    }
}
