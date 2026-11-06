//! Instants and epoch milliseconds, spelled the same way on both backends
//! (TODO-type-hardening H7.2).
//!
//! An ownership formula may compare dates (`published_at <= new Date()`) and
//! do arithmetic on milliseconds (`created_at.getTime() > Date.now() - 15*60e3`),
//! and its translation has to mean on the database exactly what JavaScript
//! means. JavaScript has one kind of time, a UTC instant counted in whole
//! milliseconds; the databases have several, and each backend writes them its
//! own way. These four functions are the translation's only spelling of time:
//! each is an [`Expr::Func`] under a reserved `_fd_t_` name, and the renderer
//! writes it in the dialect's own SQL.
//!
//! | function | Postgres | SQLite (UTC text, fixed width) |
//! |---|---|---|
//! | [`instant`] | `CAST(x AS timestamptz)` | `strftime('%Y-%m-%dT%H:%M:%fZ', x)` |
//! | [`date_instant`] | `(CAST(x AS timestamp) AT TIME ZONE 'UTC')` | the same `strftime` |
//! | [`epoch_ms`] | `floor(EXTRACT(EPOCH FROM x) * 1000)` | `CAST(round((julianday(x) - 2440587.5) * 86400000.0) AS INTEGER)` |
//! | [`from_epoch_ms`] | `to_timestamp(CAST(n AS double precision) / 1000)` | `strftime('%Y-%m-%dT%H:%M:%fZ', n / 1000.0, 'unixepoch')` |
//!
//! A calendar `date` is the instant of its UTC midnight, which is what
//! JavaScript's `new Date("2024-05-06")` is; Postgres would otherwise read it at
//! midnight in the session's time zone. Whole milliseconds are `floor`ed, as a
//! JavaScript `Date` truncates the microseconds Postgres keeps.

use crate::expr::Expr;

/// A timestamp-valued expression (a column, a bound instant, a GUC's text)
/// as an instant.
pub const INSTANT: &str = "_fd_t_instant";
/// A calendar `date` as the instant of its UTC midnight.
pub const DATE_INSTANT: &str = "_fd_t_date_instant";
/// An instant as whole milliseconds since the Unix epoch.
pub const EPOCH_MS: &str = "_fd_t_epoch_ms";
/// Milliseconds since the Unix epoch as an instant.
pub const FROM_EPOCH_MS: &str = "_fd_t_from_epoch_ms";

/// The UTC text format SQLite instants are written in, as `strftime` spells it:
/// the driver's own `YYYY-MM-DDTHH:MM:SS.mmmZ`.
const SQLITE_INSTANT: &str = "'%Y-%m-%dT%H:%M:%fZ'";

/// One of the four, by its reserved name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeFn {
    Instant,
    DateInstant,
    EpochMs,
    FromEpochMs,
}

impl TimeFn {
    /// The function a reserved name stands for, if it is one.
    pub(crate) fn of(name: &str) -> Option<TimeFn> {
        Some(match name {
            INSTANT => TimeFn::Instant,
            DATE_INSTANT => TimeFn::DateInstant,
            EPOCH_MS => TimeFn::EpochMs,
            FROM_EPOCH_MS => TimeFn::FromEpochMs,
            _ => return None,
        })
    }

    /// The text around the one argument: `(before, after)`.
    pub(crate) fn template(self, sqlite: bool) -> (String, &'static str) {
        match (self, sqlite) {
            (TimeFn::Instant, false) => ("CAST(".into(), " AS timestamptz)"),
            (TimeFn::DateInstant, false) => ("(CAST(".into(), " AS timestamp) AT TIME ZONE 'UTC')"),
            (TimeFn::EpochMs, false) => ("floor(EXTRACT(EPOCH FROM ".into(), ") * 1000)"),
            (TimeFn::FromEpochMs, false) => {
                ("to_timestamp(CAST(".into(), " AS double precision) / 1000)")
            }
            (TimeFn::Instant | TimeFn::DateInstant, true) => {
                (format!("strftime({SQLITE_INSTANT}, "), ")")
            }
            (TimeFn::EpochMs, true) => (
                "CAST(round((julianday(".into(),
                ") - 2440587.5) * 86400000.0) AS INTEGER)",
            ),
            (TimeFn::FromEpochMs, true) => (
                format!("strftime({SQLITE_INSTANT}, ("),
                ") / 1000.0, 'unixepoch')",
            ),
        }
    }
}

fn call(name: &str, arg: Expr) -> Expr {
    Expr::Func {
        name: name.into(),
        args: vec![arg],
    }
}

/// `x` as an instant (see the module table).
pub fn instant(x: Expr) -> Expr {
    call(INSTANT, x)
}

/// A calendar date `x` as the instant of its UTC midnight.
pub fn date_instant(x: Expr) -> Expr {
    call(DATE_INSTANT, x)
}

/// The instant `x` as whole milliseconds since the epoch.
pub fn epoch_ms(x: Expr) -> Expr {
    call(EPOCH_MS, x)
}

/// `n` milliseconds since the epoch, as an instant.
pub fn from_epoch_ms(n: Expr) -> Expr {
    call(FROM_EPOCH_MS, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BinOp, Select, Source, SqlDialect, Statement, Value};

    struct Pg;
    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{ident}\"")
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }

    struct Lite;
    impl SqlDialect for Lite {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{ident}\"")
        }
        fn placeholder(&self, position: usize) -> String {
            format!("?{position}")
        }
        fn is_sqlite(&self) -> bool {
            true
        }
    }

    fn render(d: &dyn SqlDialect, e: Expr) -> (String, Vec<Value>) {
        let stmt: Statement = Select::from(Source::table("t")).filter(e).into();
        let (sql, binds) = d.render(&stmt).unwrap();
        (
            sql.strip_prefix("SELECT * FROM \"t\" WHERE ")
                .unwrap()
                .to_owned(),
            binds,
        )
    }

    #[test]
    fn each_backend_spells_the_time_functions_its_own_way() {
        let window = Expr::binary(
            BinOp::Gt,
            epoch_ms(date_instant(Expr::col("d"))),
            Expr::lit(5_i64),
        );
        let (pg, binds) = render(&Pg, window.clone());
        assert_eq!(
            pg,
            "(floor(EXTRACT(EPOCH FROM (CAST(\"d\" AS timestamp) AT TIME ZONE 'UTC')) * 1000) > $1)"
        );
        assert_eq!(binds, vec![Value::Int(5)]);
        let (lite, _) = render(&Lite, window);
        assert_eq!(
            lite,
            "(CAST(round((julianday(strftime('%Y-%m-%dT%H:%M:%fZ', \"d\")) - 2440587.5) \
             * 86400000.0) AS INTEGER) > ?1)"
        );

        let embargo = Expr::binary(
            BinOp::Le,
            instant(Expr::col("at")),
            from_epoch_ms(Expr::lit(0_i64)),
        );
        let (pg, _) = render(&Pg, embargo.clone());
        assert_eq!(
            pg,
            "(CAST(\"at\" AS timestamptz) <= to_timestamp(CAST($1 AS double precision) / 1000))"
        );
        let (lite, _) = render(&Lite, embargo);
        assert_eq!(
            lite,
            "(strftime('%Y-%m-%dT%H:%M:%fZ', \"at\") <= \
             strftime('%Y-%m-%dT%H:%M:%fZ', (?1) / 1000.0, 'unixepoch'))"
        );
    }

    #[test]
    fn a_policy_inlines_the_argument_and_keeps_the_template() {
        let sql =
            crate::render_policy_expr(&Pg, &instant(Expr::lit("2026-01-01T00:00:00Z"))).unwrap();
        assert_eq!(sql, "CAST('2026-01-01T00:00:00Z' AS timestamptz)");
    }
}
