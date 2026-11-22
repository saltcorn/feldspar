//! A construction's clarifying questions against a real Postgres
//! (TODO-app-constructor AC8): asking again replaces the open questions and
//! keeps the answered ones, asking discards the mockups, an answer is refused for another construction's
//! question, and deleting the construction takes them.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_construct::{
    QuestionAnswer, answer_questions, bootstrap_constructions, choose_mockup, create_construction,
    delete_construction, mockups_of, parse_mockups, questions_of, save_mockups, save_questions,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use serde_json::json;
use uuid::Uuid;

async fn setup() -> Result<(Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    bootstrap_constructions(&catalog).await?;
    Ok((catalog, db))
}

fn texts(questions: &[sc_construct::Question]) -> Vec<&str> {
    questions.iter().map(|q| q.text.as_str()).collect()
}

fn answer(question: Uuid, text: &str) -> QuestionAnswer {
    QuestionAnswer {
        question,
        text: text.to_owned(),
    }
}

#[tokio::test]
async fn asking_again_keeps_the_answered_questions_and_replaces_the_open_ones() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;

    let first = save_questions(&catalog, app, &["Who?".into(), "How many?".into()])
        .await?
        .questions;
    assert_eq!(texts(&first), ["Who?", "How many?"]);

    answer_questions(&catalog, app, &[answer(first[1].id, " A dozen ")]).await?;
    let again = save_questions(&catalog, app, &["Offline?".into(), "Where?".into()])
        .await?
        .questions;
    // The answered one first, as it was; the open one gone.
    assert_eq!(texts(&again), ["How many?", "Offline?", "Where?"]);
    assert_eq!(again[0].id, first[1].id);
    assert_eq!(again[0].answer.as_deref(), Some("A dozen"));
    assert_eq!(
        texts(&questions_of(&catalog, construction.id).await?),
        texts(&again)
    );

    // Blank text opens it again; another construction's question is refused.
    answer_questions(&catalog, app, &[answer(again[0].id, "  ")]).await?;
    assert_eq!(
        questions_of(&catalog, construction.id).await?[0].answer,
        None
    );
    let other = Uuid::new_v4();
    create_construction(&catalog, other, None).await?;
    let theirs = save_questions(&catalog, other, &["Theirs?".into()])
        .await?
        .questions;
    assert!(
        answer_questions(&catalog, app, &[answer(theirs[0].id, "x")])
            .await
            .is_err()
    );

    delete_construction(&catalog, app).await?;
    assert!(questions_of(&catalog, construction.id).await?.is_empty());
    assert_eq!(
        questions_of(&catalog, theirs[0].construction).await?.len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn asking_discards_the_mockups_drawn_before_the_answers() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;
    let round = |title: &str| {
        parse_mockups(&json!([{
            "title": title,
            "screens": [{ "name": "List", "html": "<main>x</main>" }],
        }]))
        .unwrap()
    };
    let saved = save_mockups(&catalog, app, &round("Table")).await?;
    save_mockups(&catalog, app, &round("Cards")).await?;
    choose_mockup(&catalog, app, saved[0].id).await?;

    let asked = save_questions(&catalog, app, &["Who?".into()]).await?;
    assert_eq!(asked.discarded_mockups, 2);
    assert!(mockups_of(&catalog, construction.id).await?.is_empty());
    // Nothing left to discard the next time; a new round starts over at 1.
    assert_eq!(
        save_questions(&catalog, app, &["Where?".into()])
            .await?
            .discarded_mockups,
        0
    );
    assert_eq!(
        save_mockups(&catalog, app, &round("New")).await?[0].round,
        1
    );
    Ok(())
}
