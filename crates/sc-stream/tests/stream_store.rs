//! `_fd_streams` against a **real Postgres** (principle 4; TODO task 2.5): the
//! row *is* the definition, so what a save writes and a load reads back is the
//! whole of whether a configured dataflow — and the broker password in it —
//! survives a restart.
//!
//! What is here rather than in a unit test, and why:
//!
//! - **The round trip**, because a `Stream` that equals itself in memory proves
//!   nothing about a JSON column, a nullable `min_role` and a uuid primary key.
//! - **The strict read**, damaged *with SQL*, because the failure this guards
//!   against is a row the database holds, not a value a test constructed. A
//!   stream read with half a configuration would connect anyway — to the wrong
//!   broker, or the wrong topic.
//! - **Uniqueness**, twice: the validator's sentence, which is what an admin
//!   sees, and the `UNIQUE` constraint underneath it, which is what stops two
//!   admins saving the same name at once. Only the second is a fact about the
//!   database, and it is the one a unit test cannot reach.
//! - **The delete refusal**, because the referents come from a caller a layer
//!   up and the row must still be there afterwards.
//! - **The secret round trip**, because "the password survived" is a question
//!   about what is *stored*, and the sentinel reaching the row is the failure
//!   the whole arrangement exists to prevent.

use sc_types::Role;
use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, Insert, Statement, Update, Value};
use sc_stream::store::{COL_CONFIGURATION, COL_MIN_ROLE};
use sc_stream::{
    ElementType, STREAMS_TABLE, Stream, StreamId, StreamProvider, StreamRegistry, StreamSink,
    Subscription, bootstrap_streams, delete_stream, list_streams, load_stream, load_stream_by_name,
    redacted_stream, require_stream, save_stream, trigger_referent,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, SECRET_SENTINEL, TypeRef};
use serde_json::{Value as Json, json};

/// A catalog over a per-test database with `_fd_streams` bootstrapped.
async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_streams(&cat).await?;
    Ok(cat)
}

/// A provider shaped like the MQTT one: a broker, a topic, and a **password**
/// it declares `secret`.
///
/// Its `validate` checks the password's *prefix*, which is not decoration — it
/// is the assertion that the redaction sentinel never reaches a provider's own
/// validation. A real one does this (an LLM provider checks `sk-`), and handed
/// `••••••••` it would refuse a configuration that is in fact fine.
struct Broker;

/// The prefix [`Broker`] insists its passwords carry.
const TOKEN_PREFIX: &str = "tok-";

#[async_trait]
impl StreamProvider for Broker {
    fn name(&self) -> &str {
        "broker"
    }
    fn description(&self) -> &str {
        "a broker with a password, for tests"
    }
    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("topic", TypeRef::Basic(BasicType::Text)).required(),
            FormField::new("username", TypeRef::Basic(BasicType::Text)),
            FormField::new("password", TypeRef::Basic(BasicType::Text)).secret(),
        ]
    }
    fn element_type(&self, _config: &Attrs) -> Result<ElementType> {
        Ok(ElementType::text())
    }
    fn validate(&self, config: &Attrs) -> Result<()> {
        if let Some(password) = config.get("password").and_then(Json::as_str)
            && !password.starts_with(TOKEN_PREFIX)
        {
            return Err(Error::invalid(format!(
                "this broker's passwords all start with `{TOKEN_PREFIX}`"
            )));
        }
        Ok(())
    }
    async fn subscribe(
        &self,
        _stream: &str,
        _config: &Attrs,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<Subscription> {
        Ok(Subscription::spawn(|mut stop| async move {
            stop.stopped().await;
        }))
    }
}

fn registry() -> Result<StreamRegistry> {
    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(Broker))?;
    Ok(registry)
}

/// A stream with every column filled in with something distinguishable.
fn boiler() -> Stream {
    Stream::new("boiler", "broker")
        .description("the boiler's temperature")
        .config("topic", "house/boiler/#")
        .config("username", "sensor")
        .config("password", "tok-hunter2")
        .min_role(Role::of(40))
        .attribute("note", "from the tutorial")
}

#[tokio::test]
async fn a_stream_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let stream = boiler();
    save_stream(&cat, &reg, &stream).await?;

    let loaded = load_stream(&cat, stream.id)
        .await?
        .expect("the row that was just written");
    assert_eq!(loaded, stream);

    // By name, which is how a trigger's channel and an app's `StreamRef`
    // resolve it.
    assert_eq!(
        load_stream_by_name(&cat, "boiler").await?.as_ref(),
        Some(&stream)
    );
    assert_eq!(require_stream(&cat, "boiler").await?, stream);

    // The columns that are easy to lose in a serialisation, one at a time: the
    // JSON configuration whole, the nullable role, the sparse attributes.
    assert_eq!(loaded.configuration["topic"], json!("house/boiler/#"));
    assert_eq!(loaded.configuration["password"], json!("tok-hunter2"));
    assert_eq!(loaded.min_role, Some(Role::of(40)));
    assert_eq!(loaded.attributes["note"], json!("from the tutorial"));
    assert!(loaded.is_enabled());

    // A name nothing answers to is `None` rather than an error, and
    // `require_stream` is the half that names it.
    assert!(load_stream_by_name(&cat, "furnace").await?.is_none());
    let err = require_stream(&cat, "furnace")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("furnace"), "{err}");
    Ok(())
}

#[tokio::test]
async fn saving_an_existing_stream_updates_its_row_rather_than_adding_one() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let mut stream = boiler();
    save_stream(&cat, &reg, &stream).await?;

    stream.description = "the boiler, renamed".to_owned();
    stream.set_enabled(false);
    stream.min_role = None;
    save_stream(&cat, &reg, &stream).await?;

    let all = list_streams(&cat).await?;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].description, "the boiler, renamed");
    assert!(!all[0].is_enabled());
    // Switching a stream off and taking away its role floor are both edits an
    // admin makes; neither may leave the old value behind.
    assert_eq!(all[0].min_role, None);

    // Disabled and admin-only, and still listed: which streams to subscribe to
    // is the supervisor's business, not the store's.
    save_stream(
        &cat,
        &reg,
        &Stream::new("furnace", "broker").config("topic", "house/#"),
    )
    .await?;
    let names: Vec<String> = list_streams(&cat)
        .await?
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["boiler", "furnace"], "ordered by name");
    Ok(())
}

#[tokio::test]
async fn a_damaged_row_is_refused_naming_the_stream_and_the_column() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let stream = boiler();
    save_stream(&cat, &reg, &stream).await?;

    // A configuration that is not an object. The stream would otherwise be
    // read with no settings at all and subscribed to anyway.
    damage(
        &cat,
        stream.id,
        COL_CONFIGURATION,
        Value::Json(json!("house/boiler/#")),
    )
    .await?;
    let err = list_streams(&cat).await.unwrap_err().to_string();
    assert!(
        err.contains("boiler") && err.contains(COL_CONFIGURATION) && err.contains("a string"),
        "{err}"
    );
    // The single-row reads are as strict as the list: whichever path the admin
    // arrived by, the answer is the same sentence.
    assert!(load_stream(&cat, stream.id).await.is_err());
    assert!(load_stream_by_name(&cat, "boiler").await.is_err());

    // And a role off the 1–100 scale is not clamped to the nearest one: that
    // would quietly change who may observe the flow.
    damage(
        &cat,
        stream.id,
        COL_CONFIGURATION,
        Value::Json(json!({"topic": "house/boiler/#"})),
    )
    .await?;
    damage(&cat, stream.id, COL_MIN_ROLE, Value::Int(140)).await?;
    let err = list_streams(&cat).await.unwrap_err().to_string();
    assert!(
        err.contains("boiler") && err.contains("between 1 and 100") && err.contains("140"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn two_streams_may_not_answer_to_one_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let stream = boiler();
    save_stream(&cat, &reg, &stream).await?;

    // The half an admin meets: a sentence saying the name is taken and why
    // that matters.
    let clash = Stream::new("boiler", "broker").config("topic", "house/#");
    let err = save_stream(&cat, &reg, &clash)
        .await
        .expect_err("a second stream called `boiler` must be refused")
        .to_string();
    assert!(
        err.contains("boiler") && err.contains("already exists"),
        "{err}"
    );
    assert_eq!(list_streams(&cat).await?.len(), 1);

    // Editing a stream without renaming it is not a clash with itself: the
    // check is against *another* row's name, not against the name.
    let mut same = stream.clone();
    same.description = "edited".to_owned();
    save_stream(&cat, &reg, &same).await?;

    // And the half the validator cannot promise, because two admins saving at
    // once cannot see each other's transaction: the column itself is UNIQUE.
    let insert = Insert::row(
        STREAMS_TABLE,
        [
            "id",
            "name",
            "description",
            "provider",
            "configuration",
            "attributes",
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::Lit(Value::Uuid(StreamId::new().0)),
            Expr::lit("boiler"),
            Expr::lit(""),
            Expr::lit("broker"),
            Expr::Lit(Value::Json(json!({}))),
            Expr::Lit(Value::Json(json!({}))),
        ],
    );
    assert!(
        cat.primary().query(&Statement::from(insert)).await.is_err(),
        "the database is the authority on the name being unique"
    );
    Ok(())
}

#[tokio::test]
async fn a_stream_a_trigger_still_names_is_not_deleted() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let stream = boiler();
    save_stream(&cat, &reg, &stream).await?;

    // A trigger holds its stream as a `channel`, which is a name rather than a
    // foreign key — so nothing in the database stops this, and the referents
    // come from a caller that can see triggers.
    let referents = vec![trigger_referent("store_temp"), trigger_referent("alert")];
    let err = delete_stream(&cat, stream.id, &referents)
        .await
        .expect_err("a stream a trigger fires on must not be deleted out from under it")
        .to_string();
    assert!(
        err.contains("boiler") && err.contains("store_temp") && err.contains("alert"),
        "{err}"
    );
    assert!(
        load_stream(&cat, stream.id).await?.is_some(),
        "the refusal must leave the row alone"
    );

    // With the references gone, so is the stream — and deleting it twice says
    // "there was nothing there" rather than failing.
    assert!(delete_stream(&cat, stream.id, &[]).await?);
    assert!(!delete_stream(&cat, stream.id, &[]).await?);
    assert!(load_stream(&cat, stream.id).await?.is_none());

    // A stream that is not there cannot be being referenced, so "already
    // gone" beats a reference to a row nobody can see.
    assert!(!delete_stream(&cat, stream.id, &referents).await?);
    Ok(())
}

#[tokio::test]
async fn a_broker_password_survives_an_edit_that_never_saw_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let stream = boiler();
    save_stream(&cat, &reg, &stream).await?;

    // The admin opens the form. What the API serialises is redacted, so this is
    // everything the browser could possibly know.
    let stored = load_stream(&cat, stream.id).await?.expect("the row");
    let shown = redacted_stream(&reg, &stored);
    assert_eq!(shown.configuration["password"], json!(SECRET_SENTINEL));
    assert!(
        !Json::Object(shown.configuration.clone())
            .to_string()
            .contains("hunter2"),
        "no part of the password may leave, not even its tail"
    );

    // They widen the topic filter and save, sending the mask back untouched.
    let submitted = shown.config("topic", "house/#");
    save_stream(&cat, &reg, &submitted).await?;

    let after = load_stream(&cat, stream.id).await?.expect("the row");
    assert_eq!(
        after.configuration["password"],
        json!("tok-hunter2"),
        "the stored password, not the mask"
    );
    assert_eq!(after.configuration["topic"], json!("house/#"));

    // And the provider's own validation saw the real password rather than the
    // mask — `••••••••` does not start with `tok-`, so a save that merged after
    // validating would have been refused here.
    let retyped = redacted_stream(&reg, &after).config("password", "not-a-token");
    let err = save_stream(&cat, &reg, &retyped)
        .await
        .expect_err("a retyped password is validated")
        .to_string();
    assert!(err.contains(TOKEN_PREFIX), "{err}");

    // A password retyped to a good one replaces the stored one.
    let retyped = redacted_stream(&reg, &after).config("password", "tok-correct-horse");
    save_stream(&cat, &reg, &retyped).await?;
    let after = load_stream(&cat, stream.id).await?.expect("the row");
    assert_eq!(after.configuration["password"], json!("tok-correct-horse"));
    Ok(())
}

#[tokio::test]
async fn creating_a_stream_with_the_mask_as_its_password_stores_no_password() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    // There is nothing behind the mask on a create, so the key is dropped
    // rather than stored — a stream that connected with `••••••••` as its
    // password would retry against a live broker forever.
    let created = Stream::new("boiler", "broker")
        .config("topic", "house/#")
        .config("password", SECRET_SENTINEL);
    save_stream(&cat, &reg, &created).await?;

    let stored = load_stream(&cat, created.id).await?.expect("the row");
    assert!(!stored.configuration.contains_key("password"));
    Ok(())
}

/// Overwrite one column of one stream's row, with SQL — the only way to
/// produce the rows the strict read exists for.
async fn damage(catalog: &Catalog, id: StreamId, column: &str, value: Value) -> Result<()> {
    let update = Update::new(
        STREAMS_TABLE,
        vec![Assignment::new(column.to_owned(), Expr::Lit(value))],
    )
    .filter(Expr::col("id").eq(Expr::Lit(Value::Uuid(id.0))));
    catalog.primary().query(&Statement::from(update)).await?;
    Ok(())
}
