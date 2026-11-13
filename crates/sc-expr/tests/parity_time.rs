//! Evaluator parity over **time** (TODO-type-hardening H7.2, TECHNICAL_DESIGN
//! §7.3a): `new Date()`, `Date.now()`, `new Date(…)`, `.getTime()`,
//! millisecond arithmetic and comparisons, on a date and a timestamp column.
//!
//! Each case is decided three ways, against one fixed instant, and all three
//! must agree with the expected verdict:
//!
//! - the runtime check: translated with [`UserEnv::Inline`], the instant bound
//!   as a parameter, run by Postgres;
//! - RLS: translated with [`UserEnv::Guc`], run on a connection whose `sc.now`
//!   and `sc.user` GUCs hold the instant and the user;
//! - the evaluator: V8, with the same instant as [`FormulaCall::now`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use sc_db_postgres::{PgDialect, PgParam};
use sc_expr::{
    DenoEvaluator, Env, FieldKind, Formula, FormulaCall, JsEvaluator, NOW_GUC, Operation,
    SchemaShape, TableShape, USER_GUC, UserEnv, translate,
};
use sc_query::{Expr as QExpr, Select, Source, SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use tokio_postgres::types::ToSql;

/// posts(owner text, created_at timestamptz, due date, stamp int8).
fn shape() -> SchemaShape {
    SchemaShape::new()
        .table(
            "posts",
            TableShape::new()
                .typed_field("owner", FieldKind::Text)
                .typed_field("created_at", FieldKind::Timestamp)
                .typed_field("due", FieldKind::Date)
                .typed_field("stamp", FieldKind::Integer),
        )
        .user_fields(["id"])
}

/// The instant every case is decided at: a millisecond that is not a whole
/// second, so truncation errors would show.
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-05T12:00:00.123Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[derive(Clone, Default)]
struct Row {
    owner: Option<&'static str>,
    created_at: Option<DateTime<Utc>>,
    due: Option<NaiveDate>,
    stamp: Option<i64>,
}

impl Row {
    fn bindings(&self) -> BTreeMap<String, Value> {
        BTreeMap::from([
            (
                "owner".into(),
                self.owner.map_or(Value::Null, |o| Value::Text(o.into())),
            ),
            (
                "created_at".into(),
                self.created_at.map_or(Value::Null, Value::Timestamp),
            ),
            ("due".into(), self.due.map_or(Value::Null, Value::Date)),
            ("stamp".into(), self.stamp.map_or(Value::Null, Value::Int)),
        ])
    }
}

async fn set_row(client: &tokio_postgres::Client, row: &Row) {
    client.execute("DELETE FROM posts", &[]).await.unwrap();
    client
        .execute(
            "INSERT INTO posts (owner, created_at, due, stamp) VALUES ($1, $2, $3, $4)",
            &[&row.owner, &row.created_at, &row.due, &row.stamp],
        )
        .await
        .unwrap();
}

async fn sql_verdict(client: &tokio_postgres::Client, pred: QExpr, src: &str) -> bool {
    let stmt: Statement = Select::from(Source::table("posts")).filter(pred).into();
    let (sql, binds) = PgDialect.render(&stmt).expect("render");
    let wrapped = format!("SELECT EXISTS({sql})");
    let params: Vec<PgParam<'_>> = binds.iter().map(PgParam).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    client
        .query_one(&wrapped, &refs)
        .await
        .unwrap_or_else(|e| panic!("{src}: {wrapped} {binds:?}: {e}"))
        .get(0)
}

struct Harness<'a> {
    inline: &'a tokio_postgres::Client,
    guc: &'a tokio_postgres::Client,
    ev: DenoEvaluator,
}

impl Harness<'_> {
    /// Decide `src` for `op` on `row`, as user `u1` (or anonymously), all three
    /// ways, and assert they agree on `expect`.
    async fn check(&self, src: &str, op: Operation, row: Row, signed_in: bool, expect: bool) {
        let formula = Formula::parse(src).expect(src);
        formula.validate(&shape(), "posts").expect(src);
        let user = signed_in.then(|| BTreeMap::from([("id".to_owned(), Value::Text("u1".into()))]));
        set_row(self.inline, &row).await;

        let inline = UserEnv::Inline(user.clone());
        let pred = translate(
            &formula,
            op,
            &Env::new(&inline).with_now(now()),
            &shape(),
            "posts",
        )
        .unwrap_or_else(|e| panic!("{src}: translate inline: {e}"));
        let runtime = sql_verdict(self.inline, pred, src).await;

        let user_json = if signed_in { r#"{"id":"u1"}"# } else { "" };
        self.guc
            .execute("SELECT set_config($1, $2, false)", &[&USER_GUC, &user_json])
            .await
            .unwrap();
        let policy = UserEnv::Guc {
            field_types: BTreeMap::from([("id".to_owned(), "text".to_owned())]),
        };
        let pred = translate(&formula, op, &Env::new(&policy), &shape(), "posts")
            .unwrap_or_else(|e| panic!("{src}: translate guc: {e}"));
        let rls = sql_verdict(self.guc, pred, src).await;

        let reified = self
            .ev
            .eval(FormulaCall {
                formula,
                op,
                row: row.bindings(),
                user,
                ambient: Default::default(),
                now: now(),
            })
            .await
            .unwrap_or_else(|e| panic!("{src}: reified: {e}"));

        assert_eq!(runtime, reified, "{src}: runtime SQL vs evaluator ({op:?})");
        assert_eq!(rls, reified, "{src}: RLS SQL vs evaluator ({op:?})");
        assert_eq!(reified, expect, "{src}: verdict ({op:?})");
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn time_means_the_same_on_the_evaluator_the_runtime_check_and_rls() {
    let db = TestDb::new().await.expect("test db");
    let inline = db.client().await.expect("client");
    inline
        .batch_execute(
            "CREATE TABLE posts (owner text, created_at timestamptz, due date, stamp int8);",
        )
        .await
        .unwrap();
    // A session time zone far from UTC: a calendar date is still its UTC
    // midnight, as JavaScript's `new Date("2026-11-05")` is.
    inline
        .batch_execute("SET TIME ZONE 'Pacific/Auckland'")
        .await
        .unwrap();
    let guc = db.client().await.expect("client");
    guc.batch_execute("SET TIME ZONE 'America/Los_Angeles'")
        .await
        .unwrap();
    guc.execute(
        "SELECT set_config($1, $2, false)",
        &[&NOW_GUC, &now().to_rfc3339()],
    )
    .await
    .unwrap();
    let h = Harness {
        inline: &inline,
        guc: &guc,
        ev: DenoEvaluator::new(),
    };
    let read = Operation::Read;
    let at = |offset: Duration| Row {
        owner: Some("u1"),
        created_at: Some(now() + offset),
        ..Row::default()
    };

    // An embargo: published once its instant has passed.
    let embargo = "created_at <= new Date()";
    h.check(embargo, read, at(Duration::minutes(-1)), false, true)
        .await;
    h.check(embargo, read, at(Duration::minutes(1)), false, false)
        .await;
    h.check(embargo, read, at(Duration::zero()), false, true)
        .await;
    h.check(embargo, read, at(Duration::milliseconds(1)), false, false)
        .await;
    h.check(embargo, read, Row::default(), false, false).await;
    h.check(
        "!(created_at <= new Date())",
        read,
        Row::default(),
        false,
        true,
    )
    .await;
    h.check(
        "new Date() > created_at",
        read,
        at(Duration::seconds(-5)),
        false,
        true,
    )
    .await;

    // A 15-minute edit window, for the owner only.
    let window = "_update && owner === user.id && created_at.getTime() > Date.now() - 15*60e3";
    let update = Operation::Update;
    h.check(window, update, at(Duration::minutes(-5)), true, true)
        .await;
    h.check(window, update, at(Duration::minutes(-20)), true, false)
        .await;
    h.check(window, update, at(Duration::minutes(-5)), false, false)
        .await;
    h.check(window, read, at(Duration::minutes(-5)), true, false)
        .await;
    h.check(
        window,
        update,
        Row {
            owner: Some("u1"),
            ..Row::default()
        },
        true,
        false,
    )
    .await;

    // Milliseconds are whole, as a JavaScript `Date`'s are.
    h.check(
        "created_at.getTime() === Date.now()",
        read,
        at(Duration::microseconds(400)),
        false,
        true,
    )
    .await;
    h.check(
        "created_at.getTime() - Date.now() === 1",
        read,
        at(Duration::milliseconds(1)),
        false,
        true,
    )
    .await;
    h.check(
        "new Date().getTime() > 0",
        read,
        Row::default(),
        false,
        true,
    )
    .await;

    // `new Date(<milliseconds>)`, and `new Date(<instant>)` in both directions.
    let stamped = |offset_ms: i64| Row {
        created_at: Some(now()),
        stamp: Some(now().timestamp_millis() + offset_ms),
        ..Row::default()
    };
    h.check(
        "new Date(stamp) <= new Date()",
        read,
        stamped(0),
        false,
        true,
    )
    .await;
    h.check(
        "new Date(stamp) <= new Date()",
        read,
        stamped(1),
        false,
        false,
    )
    .await;
    h.check(
        "created_at === new Date(stamp)",
        read,
        stamped(0),
        false,
        true,
    )
    .await;
    h.check(
        "created_at !== new Date(stamp)",
        read,
        stamped(0),
        false,
        false,
    )
    .await;
    h.check(
        "created_at === new Date(stamp)",
        read,
        stamped(7),
        false,
        false,
    )
    .await;
    h.check(
        "new Date(created_at.getTime() + 60000) > new Date()",
        read,
        at(Duration::seconds(-30)),
        false,
        true,
    )
    .await;
    h.check(
        "new Date(stamp + 1000) > new Date(created_at)",
        read,
        stamped(-500),
        false,
        true,
    )
    .await;

    // A calendar date: the instant of its UTC midnight.
    let due = |d: NaiveDate| Row {
        due: Some(d),
        ..Row::default()
    };
    let today = now().date_naive();
    h.check("due <= new Date()", read, due(today), false, true)
        .await;
    h.check(
        "due <= new Date()",
        read,
        due(today.succ_opt().unwrap()),
        false,
        false,
    )
    .await;
    h.check("new Date(due) < new Date()", read, due(today), false, true)
        .await;
    h.check(
        "due.getTime() === Date.now() - 12*3600e3 - 123",
        read,
        due(today),
        false,
        true,
    )
    .await;
    h.check("due <= new Date()", read, Row::default(), false, false)
        .await;
}

/// What does not translate still decides on the evaluator, and is refused by
/// the translator by name rather than rendered as something else.
#[test]
fn time_outside_the_vocabulary_does_not_translate() {
    let env = UserEnv::Inline(None);
    for src in [
        "owner <= new Date()",           // a text column is not a date
        "new Date(owner) <= new Date()", // nor is `new Date` of one
        "new Date() - created_at > 0",   // arithmetic on an instant itself
        "new Date(2026, 1) <= new Date()",
    ] {
        let formula = Formula::parse(src).unwrap();
        let err = translate(
            &formula,
            Operation::Read,
            &Env::new(&env),
            &shape(),
            "posts",
        )
        .expect_err(src);
        assert!(
            matches!(err, sc_expr::TranslateError::Untranslatable(_)),
            "{src}: {err}"
        );
    }
}
