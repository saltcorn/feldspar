//! Evaluator parity over **presented values** (TODO-type-hardening H7.5,
//! TECHNICAL_DESIGN §7.3a): `column === _request.key`, decided three ways —
//! the runtime SQL with the value bound, the RLS SQL with the value in its
//! `sc.request.<key>` GUC, and V8 with `_request` bound — on a text column and
//! a UUID one. An absent value equals nothing, not even a null column.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use sc_db_postgres::{PgDialect, PgParam};
use sc_expr::{
    Ambient, AmbientValues, DenoEvaluator, Env, FieldKind, Formula, FormulaCall, JsEvaluator,
    Operation, RequestValues, SchemaShape, TableShape, UserEnv, check_request_uses, request_guc,
    translate,
};
use sc_query::{Expr as QExpr, Select, Source, SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use tokio_postgres::types::ToSql;
use uuid::Uuid;

const CODE: &str = "6f1c1d0e-8a8e-4b51-9d7e-2b8f3f0a1c55";

fn shape() -> SchemaShape {
    SchemaShape::new()
        .table(
            "shares",
            TableShape::new()
                .typed_field("token", FieldKind::Text)
                .with_field("token", |f| {
                    f.unique = true;
                    f.random_default = true;
                })
                .typed_field("code", FieldKind::Other)
                .with_field("code", |f| {
                    f.unique = true;
                    f.random_default = true;
                }),
        )
        .with_request()
}

async fn sql_verdict(client: &tokio_postgres::Client, pred: QExpr, src: &str) -> bool {
    let stmt: Statement = Select::from(Source::table("shares")).filter(pred).into();
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

#[tokio::test]
async fn a_presented_value_means_the_same_on_every_path() {
    let db = TestDb::new().await.expect("test db");
    let client = db.client().await.expect("client");
    client
        .batch_execute("CREATE TABLE shares (token text UNIQUE, code uuid UNIQUE)")
        .await
        .unwrap();
    let ev = DenoEvaluator::new();

    for (src, token, code, presented, expect) in [
        (
            "token === _request.token",
            Some("t1"),
            None,
            vec![("token", "t1")],
            true,
        ),
        (
            "token === _request.token",
            Some("t1"),
            None,
            vec![("token", "t2")],
            false,
        ),
        ("token === _request.token", Some("t1"), None, vec![], false),
        // Absent equals nothing, not even a null column; empty is absent.
        ("token === _request.token", None, None, vec![], false),
        (
            "token === _request.token",
            None,
            None,
            vec![("token", "")],
            false,
        ),
        // `!` of an equality that cannot hold is true, as JavaScript's is.
        ("!(token === _request.token)", None, None, vec![], true),
        // A UUID column compares as its text, so a value that is not a UUID is
        // a mismatch rather than an error.
        (
            "code === _request.code",
            None,
            Some(CODE),
            vec![("code", CODE)],
            true,
        ),
        (
            "code === _request.code",
            None,
            Some(CODE),
            vec![("code", "nope")],
            false,
        ),
        (
            "token === _request.token || code === _request.code",
            Some("t1"),
            Some(CODE),
            vec![("code", CODE)],
            true,
        ),
    ] {
        let formula = Formula::parse(src).expect(src);
        formula.validate(&shape(), "shares").expect(src);
        check_request_uses(&formula, &shape(), "shares").expect(src);
        let code_uuid = code.map(|c| Uuid::parse_str(c).unwrap());
        client.execute("DELETE FROM shares", &[]).await.unwrap();
        client
            .execute(
                "INSERT INTO shares (token, code) VALUES ($1, $2)",
                &[&token, &code_uuid],
            )
            .await
            .unwrap();
        let values = RequestValues::new(presented.iter().copied()).unwrap();
        let ambient: AmbientValues =
            BTreeMap::from([(Ambient::Request, Some(values.to_ambient()))]);

        let inline = UserEnv::Inline(None);
        let pred = translate(
            &formula,
            Operation::Read,
            &Env::new(&inline).with_ambient(&ambient),
            &shape(),
            "shares",
        )
        .unwrap_or_else(|e| panic!("{src}: {e}"));
        let runtime = sql_verdict(&client, pred, src).await;

        for key in ["token", "code"] {
            client
                .execute(
                    "SELECT set_config($1, $2, false)",
                    &[&request_guc(key), &values.get(key).unwrap_or("")],
                )
                .await
                .unwrap();
        }
        let policy = UserEnv::Guc {
            field_types: BTreeMap::new(),
        };
        let pred = translate(
            &formula,
            Operation::Read,
            &Env::new(&policy),
            &shape(),
            "shares",
        )
        .unwrap_or_else(|e| panic!("{src}: {e}"));
        let rls = sql_verdict(&client, pred, src).await;

        let row = BTreeMap::from([
            (
                "token".to_owned(),
                token.map_or(Value::Null, |t| Value::Text(t.into())),
            ),
            (
                "code".to_owned(),
                code_uuid.map_or(Value::Null, Value::Uuid),
            ),
        ]);
        let reified = ev
            .eval(FormulaCall {
                formula,
                op: Operation::Read,
                row,
                user: None,
                ambient,
                now: chrono::Utc::now(),
            })
            .await
            .unwrap_or_else(|e| panic!("{src}: {e}"));

        assert_eq!(
            runtime, reified,
            "{src} {presented:?}: runtime SQL vs evaluator"
        );
        assert_eq!(rls, reified, "{src} {presented:?}: RLS SQL vs evaluator");
        assert_eq!(reified, expect, "{src} {presented:?}");
    }
}
