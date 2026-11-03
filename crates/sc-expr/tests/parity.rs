//! Evaluator parity: the symbolic translation executed by real Postgres and
//! the reified evaluation in real V8 must agree, case by case, on the same
//! formula, row, user and operation (TODO Phase 3 — "the gate for everything
//! after this phase").
//!
//! Each case asserts three things at once: the SQL verdict, the JS verdict,
//! and the *expected* verdict — so a case where both evaluators drift wrong
//! together still fails.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use sc_expr::{
    Ambient, AmbientValues, DenoEvaluator, Env, Formula, FormulaCall, JsEvaluator, Operation,
    SchemaShape, TableShape, UserEnv, translate,
};
use sc_query::{Expr as QExpr, Select, Source, SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use tokio_postgres::types::ToSql;

struct Pg;

impl SqlDialect for Pg {
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
    fn placeholder(&self, position: usize) -> String {
        format!("${position}")
    }
}

/// books(owner text, pages int8, title text, publisher text → publishers.id);
/// publishers(id, name, country → countries.code); countries(code, name).
fn shape() -> SchemaShape {
    SchemaShape::new()
        .table(
            "books",
            TableShape::new()
                .field("owner")
                .field("pages")
                .field("title")
                .key_field("publisher", "publishers", "id"),
        )
        .table(
            "publishers",
            TableShape::new()
                .field("id")
                .field("name")
                .key_field("country", "countries", "code"),
        )
        .table("countries", TableShape::new().field("code").field("name"))
        .user_fields(["id", "is_admin"])
}

async fn create_schema(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "CREATE TABLE countries (code text PRIMARY KEY, name text);
             CREATE TABLE publishers (id text PRIMARY KEY, name text,
                 country text REFERENCES countries(code));
             CREATE TABLE books (owner text, pages int8, title text,
                 publisher text REFERENCES publishers(id));
             INSERT INTO countries VALUES ('dk', 'Denmark');
             INSERT INTO publishers VALUES ('p1', 'ACME', 'dk');",
        )
        .await
        .expect("create schema");
}

/// The one row under test, as both the database row and the reified bindings.
#[derive(Clone, Default)]
struct Row {
    owner: Option<&'static str>,
    pages: Option<i64>,
    title: Option<&'static str>,
    publisher: Option<&'static str>,
}

impl Row {
    /// The reified binding map: the fields plus the prefetched Ⱶ-join values a
    /// caller (Phase 5) would fetch — computed here from the same seed data
    /// the database holds, so both evaluators see one world.
    fn bindings(&self) -> BTreeMap<String, Value> {
        let opt = |v: &Option<&str>| v.map_or(Value::Null, |s| Value::Text(s.into()));
        let mut row = BTreeMap::new();
        row.insert("owner".into(), opt(&self.owner));
        row.insert("title".into(), opt(&self.title));
        row.insert("pages".into(), self.pages.map_or(Value::Null, Value::Int));
        row.insert("publisher".into(), opt(&self.publisher));
        // publisher 'p1' → name ACME, country dk → Denmark; null FK → null.
        let (name, country_name) = match self.publisher {
            Some("p1") => (Value::Text("ACME".into()), Value::Text("Denmark".into())),
            _ => (Value::Null, Value::Null),
        };
        row.insert("publisherⱵname".into(), name);
        row.insert("publisherⱵcountryⱵname".into(), country_name);
        row
    }
}

fn user_map(fields: &[(&str, Value)]) -> BTreeMap<String, Value> {
    fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn to_params(binds: &[Value]) -> Vec<Box<dyn ToSql + Sync>> {
    binds
        .iter()
        .map(|v| -> Box<dyn ToSql + Sync> {
            match v {
                Value::Null => Box::new(Option::<String>::None),
                Value::Bool(b) => Box::new(*b),
                Value::Int(i) => Box::new(*i),
                Value::Float(f) => Box::new(*f),
                Value::Text(s) => Box::new(s.clone()),
                other => panic!("unsupported test bind: {other:?}"),
            }
        })
        .collect()
}

/// The SQL verdict: does the one row in `books` satisfy the translated
/// predicate?
async fn sql_verdict(client: &tokio_postgres::Client, pred: QExpr) -> bool {
    let stmt: Statement = Select::from(Source::table("books")).filter(pred).into();
    let (sql, binds) = Pg.render(&stmt).expect("render");
    let wrapped = format!("SELECT EXISTS({sql})");
    let params = to_params(&binds);
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
    let row = client
        .query_one(&wrapped, &refs)
        .await
        .unwrap_or_else(|e| panic!("query {wrapped} {binds:?}: {e}"));
    row.get(0)
}

async fn set_row(client: &tokio_postgres::Client, row: &Row) {
    client
        .execute("DELETE FROM books", &[])
        .await
        .expect("clear");
    client
        .execute(
            "INSERT INTO books (owner, pages, title, publisher) VALUES ($1, $2, $3, $4)",
            &[&row.owner, &row.pages, &row.title, &row.publisher],
        )
        .await
        .expect("insert row");
}

/// Assert the three-way agreement for one case: symbolic via Postgres,
/// reified via V8, and the expected verdict.
async fn check(
    client: &tokio_postgres::Client,
    ev: &DenoEvaluator,
    src: &str,
    op: Operation,
    row: Row,
    user: Option<&[(&str, Value)]>,
    expect: bool,
) {
    let formula = Formula::parse(src).expect(src);
    set_row(client, &row).await;

    let env = UserEnv::Inline(user.map(user_map));
    let pred = translate(&formula, op, &Env::new(&env), &shape(), "books")
        .unwrap_or_else(|e| panic!("{src}: translate: {e}"));
    let symbolic = sql_verdict(client, pred).await;

    let reified = ev
        .eval(FormulaCall {
            formula,
            op,
            row: row.bindings(),
            user: user.map(user_map),
            ambient: Default::default(),
        })
        .await
        .unwrap_or_else(|e| panic!("{src}: reified: {e}"));

    assert_eq!(symbolic, reified, "{src}: evaluators disagree (op {op:?})");
    assert_eq!(symbolic, expect, "{src}: verdict (op {op:?})");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn symbolic_and_reified_agree_on_the_translatable_subset() {
    let db = TestDb::new().await.expect("test db");
    let client = db.client().await.expect("client");
    create_schema(&client).await;
    let ev = DenoEvaluator::new();

    let u1 = [("id", Value::Text("u1".into()))];
    let read = Operation::Read;

    // Ownership equality, including every null corner.
    let owner = |o: Option<&'static str>| Row {
        owner: o,
        ..Row::default()
    };
    let f = "owner === user.id";
    check(&client, &ev, f, read, owner(Some("u1")), Some(&u1), true).await;
    check(&client, &ev, f, read, owner(Some("u2")), Some(&u1), false).await;
    check(&client, &ev, f, read, owner(None), Some(&u1), false).await;
    check(&client, &ev, f, read, owner(Some("u1")), None, false).await;
    // An anonymous caller owns nothing: its null `user.id` does not match a
    // null owner — on both evaluators (the bug report: `created_by == user.id`
    // served every `created_by IS NULL` row to an anonymous GET).
    check(&client, &ev, f, read, owner(None), None, false).await;
    check(
        &client,
        &ev,
        "owner == user.id",
        read,
        owner(None),
        None,
        false,
    )
    .await;
    // `!==` stays the complement, anonymously too.
    check(
        &client,
        &ev,
        "owner !== user.id",
        read,
        owner(None),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "owner !== user.id",
        read,
        owner(Some("u1")),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "owner !== user.id",
        read,
        owner(Some("u1")),
        Some(&u1),
        false,
    )
    .await;
    // …and the explicit guard is now merely redundant.
    let guarded = "user && owner === user.id";
    check(&client, &ev, guarded, read, owner(None), None, false).await;
    check(
        &client,
        &ev,
        guarded,
        read,
        owner(Some("u1")),
        Some(&u1),
        true,
    )
    .await;

    // Negated equality with a null side: JS two-valued, `IS DISTINCT FROM`.
    let f = "!(owner === user.id)";
    check(&client, &ev, f, read, owner(None), Some(&u1), true).await;
    check(&client, &ev, f, read, owner(Some("u1")), Some(&u1), false).await;
    check(
        &client,
        &ev,
        "owner !== null",
        read,
        owner(None),
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "owner !== null",
        read,
        owner(Some("x")),
        None,
        true,
    )
    .await;

    // Ordered comparisons: SQL null semantics, and the `!` two-valued fix.
    let pages = |p: Option<i64>| Row {
        pages: p,
        ..Row::default()
    };
    check(
        &client,
        &ev,
        "pages >= 100",
        read,
        pages(Some(150)),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "pages >= 100",
        read,
        pages(Some(50)),
        None,
        false,
    )
    .await;
    check(&client, &ev, "pages >= 100", read, pages(None), None, false).await;
    // `!(null < 100)` grants on both sides (JS `!null`; SQL IS DISTINCT FROM).
    check(
        &client,
        &ev,
        "!(pages < 100)",
        read,
        pages(None),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "!(pages < 100)",
        read,
        pages(Some(50)),
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "!(pages < 100)",
        read,
        pages(Some(150)),
        None,
        true,
    )
    .await;

    // Arithmetic, null-guarded on both sides.
    check(
        &client,
        &ev,
        "pages % 2 === 0",
        read,
        pages(Some(4)),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "pages % 2 === 0",
        read,
        pages(Some(3)),
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "pages % 2 === 0",
        read,
        pages(None),
        None,
        false,
    )
    .await;

    // Ⱶ-join paths: correlated subselect vs prefetched binding; null FK
    // grants nothing (optional chaining), at depth one and two.
    let published = Row {
        publisher: Some("p1"),
        ..Row::default()
    };
    let unpublished = Row::default();
    let f = "publisherⱵname === 'ACME'";
    check(&client, &ev, f, read, published.clone(), None, true).await;
    check(&client, &ev, f, read, unpublished.clone(), None, false).await;
    let f = "publisherⱵcountryⱵname === 'Denmark'";
    check(&client, &ev, f, read, published.clone(), None, true).await;
    check(&client, &ev, f, read, unpublished.clone(), None, false).await;

    // Operation flags.
    let f = "_read || owner === user.id";
    check(&client, &ev, f, read, owner(Some("zz")), None, true).await;
    check(
        &client,
        &ev,
        f,
        Operation::Update,
        owner(Some("zz")),
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "_write && owner === user.id",
        Operation::Delete,
        owner(Some("u1")),
        Some(&u1),
        true,
    )
    .await;

    // `??`, `?:`, `user === null`, boolean user-field truthiness.
    let titled = |t: Option<&'static str>| Row {
        title: t,
        ..Row::default()
    };
    let f = "(title ?? 'anon') === 'anon'";
    check(&client, &ev, f, read, titled(None), None, true).await;
    check(&client, &ev, f, read, titled(Some("x")), None, false).await;
    let f = "title === 'wiki' ? true : owner === user.id";
    check(&client, &ev, f, read, titled(Some("wiki")), None, true).await;
    // A non-wiki title falls to the ownership branch (owner set so the
    // anonymous null-match corner does not apply here).
    let blog = Row {
        title: Some("blog"),
        owner: Some("zz"),
        ..Row::default()
    };
    check(&client, &ev, f, read, blog.clone(), Some(&u1), false).await;
    let blog_owned = Row {
        owner: Some("u1"),
        ..blog
    };
    check(&client, &ev, f, read, blog_owned, Some(&u1), true).await;
    check(
        &client,
        &ev,
        "user === null",
        read,
        Row::default(),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "user === null",
        read,
        Row::default(),
        Some(&u1),
        false,
    )
    .await;
    let admin = [
        ("is_admin", Value::Bool(true)),
        ("id", Value::Text("u9".into())),
    ];
    let plain = [
        ("is_admin", Value::Bool(false)),
        ("id", Value::Text("u9".into())),
    ];
    let f = "user.is_admin || owner === user.id";
    check(&client, &ev, f, read, owner(Some("u1")), Some(&admin), true).await;
    check(
        &client,
        &ev,
        f,
        read,
        owner(Some("u1")),
        Some(&plain),
        false,
    )
    .await;

    // Loose equality is strict by specification.
    check(
        &client,
        &ev,
        "pages == 100",
        read,
        pages(Some(100)),
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "pages != 100",
        read,
        pages(Some(99)),
        None,
        true,
    )
    .await;
}

/// The GUC environment against a real session setting: same verdicts as the
/// inline environment, and a missing setting fails closed.
#[tokio::test]
async fn guc_mode_matches_reified_against_a_real_setting() {
    let db = TestDb::new().await.expect("test db");
    create_schema(&db.client().await.expect("client")).await;
    let ev = DenoEvaluator::new();

    // Two separate sessions: one with the user GUC set, one without.
    let with_guc = db.client().await.expect("client");
    let without_guc = db.client().await.expect("client");
    with_guc
        .execute(
            "SELECT set_config('sc.user', $1, false)",
            &[&r#"{"id":"u1"}"#],
        )
        .await
        .expect("set guc");

    let env = UserEnv::Guc {
        field_types: BTreeMap::from([("id".to_string(), "text".to_string())]),
    };
    let shape = shape();
    let u1 = [("id", Value::Text("u1".into()))];

    for (src, owner, user, expect) in [
        ("owner === user.id", Some("u1"), Some(&u1[..]), true),
        ("owner === user.id", Some("u2"), Some(&u1[..]), false),
        ("user === null", Some("u1"), Some(&u1[..]), false),
        // No GUC ↔ no user: the anonymous corners, incl. fail-closed guard.
        ("user === null", Some("u1"), None, true),
        ("owner === user.id", Some("u1"), None, false),
        ("user && owner === user.id", None, None, false),
    ] {
        let client = if user.is_some() {
            &with_guc
        } else {
            &without_guc
        };
        let row = Row {
            owner,
            ..Row::default()
        };
        set_row(client, &row).await;
        let formula = Formula::parse(src).expect(src);
        let pred = translate(&formula, Operation::Read, &Env::new(&env), &shape, "books")
            .unwrap_or_else(|e| panic!("{src}: translate: {e}"));
        let symbolic = sql_verdict(client, pred).await;
        let reified = ev
            .eval(FormulaCall {
                formula,
                op: Operation::Read,
                row: row.bindings(),
                user: user.map(user_map),
                ambient: Default::default(),
            })
            .await
            .unwrap_or_else(|e| panic!("{src}: reified: {e}"));
        assert_eq!(symbolic, reified, "{src} (guc, user={:?})", user.is_some());
        assert_eq!(symbolic, expect, "{src} (guc, user={:?})", user.is_some());
    }
}

/// Parity over the **ambient objects** `row` and `old` (decision 7): the
/// triggering event's row, inlined as literals symbolically and bound as an
/// object reified.
///
/// This is the gate the trigger machinery stands on. The cases that matter are
/// the null ones, because they are where JS and SQL diverge if nobody specifies
/// them: an object that is in scope but **null** (`old` on an insert) must read
/// as null rather than throw in V8, and as SQL `NULL` rather than a missing
/// binding — and `old === null` must be *true* on both sides.
#[tokio::test]
async fn symbolic_and_reified_agree_on_the_ambient_objects() {
    let db = TestDb::new().await.expect("test db");
    let client = db.client().await.expect("client");
    create_schema(&client).await;
    let ev = DenoEvaluator::new();

    // `row`/`old` in scope with the same fields the table has — a trigger on
    // `books` whose formula also filters `books`.
    let shape = shape()
        .ambient_fields(Ambient::Row, Some(["owner", "pages", "title"]))
        .ambient_fields(Ambient::Old, Some(["owner", "pages", "title"]));

    // (formula, the row in the table, ambient row, ambient old, expected)
    #[allow(clippy::type_complexity)]
    let cases: &[(
        &str,
        Row,
        Option<&[(&str, Value)]>,
        Option<&[(&str, Value)]>,
        bool,
    )] = &[
        // A value from the event compared against a column of the filtered table.
        (
            "owner === row.owner",
            Row {
                owner: Some("u1"),
                ..Row::default()
            },
            Some(&[("owner", Value::Text("u1".into()))]),
            None,
            true,
        ),
        (
            "owner === row.owner",
            Row {
                owner: Some("u1"),
                ..Row::default()
            },
            Some(&[("owner", Value::Text("u2".into()))]),
            None,
            false,
        ),
        // `old` null (an insert): the member is null, so the comparison is false…
        (
            "title === old.title",
            Row {
                title: Some("t"),
                ..Row::default()
            },
            Some(&[("title", Value::Text("t".into()))]),
            None,
            false,
        ),
        // …and `old === null` is the test for "this is not an update".
        (
            "old === null",
            Row::default(),
            Some(&[("title", Value::Text("t".into()))]),
            None,
            true,
        ),
        // `old` present: the classic "the field just changed" only-if.
        (
            "title !== old.title",
            Row {
                title: Some("new"),
                ..Row::default()
            },
            Some(&[("title", Value::Text("new".into()))]),
            Some(&[("title", Value::Text("old".into()))]),
            true,
        ),
        (
            "title !== old.title",
            Row {
                title: Some("same"),
                ..Row::default()
            },
            Some(&[("title", Value::Text("same".into()))]),
            Some(&[("title", Value::Text("same".into()))]),
            false,
        ),
        // A null *field* of a present object is still null, not absent — so the
        // comparison is false on both sides (and its negation true).
        (
            "title === row.title",
            Row {
                title: Some("t"),
                ..Row::default()
            },
            Some(&[("title", Value::Null)]),
            None,
            false,
        ),
        (
            "title !== row.title",
            Row {
                title: Some("t"),
                ..Row::default()
            },
            Some(&[("title", Value::Null)]),
            None,
            true,
        ),
        // Arithmetic and ordered comparison over ambient values.
        (
            "pages > row.pages",
            Row {
                pages: Some(10),
                ..Row::default()
            },
            Some(&[("pages", Value::Int(5))]),
            None,
            true,
        ),
        // Both objects and the user in one formula.
        (
            "user && row.owner === user.id && old === null",
            Row::default(),
            Some(&[("owner", Value::Text("u1".into()))]),
            None,
            true,
        ),
    ];

    for (src, row, amb_row, amb_old, expect) in cases {
        set_row(&client, row).await;
        let formula = Formula::parse(src).expect(src);
        let user = [("id", Value::Text("u1".into()))];
        let user_env = UserEnv::Inline(Some(user_map(&user)));

        let ambient: AmbientValues = BTreeMap::from([
            (Ambient::Row, amb_row.map(user_map)),
            (Ambient::Old, amb_old.map(user_map)),
        ]);
        let pred = translate(
            &formula,
            Operation::Update,
            &Env::new(&user_env).with_ambient(&ambient),
            &shape,
            "books",
        )
        .unwrap_or_else(|e| panic!("{src}: translate: {e}"));
        let symbolic = sql_verdict(&client, pred).await;

        let reified = ev
            .eval(FormulaCall {
                formula,
                op: Operation::Update,
                row: row.bindings(),
                user: Some(user_map(&user)),
                ambient,
            })
            .await
            .unwrap_or_else(|e| panic!("{src}: reified: {e}"));

        assert_eq!(symbolic, reified, "{src}: evaluators disagree");
        assert_eq!(symbolic, *expect, "{src}: verdict");
    }
}
