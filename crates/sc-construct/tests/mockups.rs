//! A construction's mockups against a real Postgres (TODO-app-constructor
//! AC7.1): each proposal is a new round, feedback is kept per mockup, one
//! mockup at most is chosen, and deleting the construction takes them.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_construct::{
    MockupFeedback, MockupStatus, bootstrap_constructions, choose_mockup, chosen_mockup,
    create_construction, delete_construction, latest_round, mockups_of, parse_mockups,
    save_mockups, set_mockup_feedback,
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

fn round(titles: &[&str]) -> Vec<sc_construct::MockupInput> {
    let list: Vec<_> = titles
        .iter()
        .map(|t| {
            json!({
                "title": t,
                "rationale": format!("{t}, explained"),
                "screens": [
                    { "name": "Orders list", "html": "<table><th>Customer</th></table>" },
                    { "name": "Order form", "html": "<form><label>Customer</label></form>" },
                ],
            })
        })
        .collect();
    parse_mockups(&json!(list)).unwrap()
}

#[tokio::test]
async fn proposals_come_in_rounds_and_one_mockup_is_chosen() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;

    let first = save_mockups(&catalog, app, &round(&["Table", "Cards"])).await?;
    assert_eq!(first.iter().map(|m| m.round).collect::<Vec<_>>(), [1, 1]);
    assert!(chosen_mockup(&catalog, construction.id).await?.is_none());

    // Feedback is kept on the mockup it is about; blank clears it.
    set_mockup_feedback(
        &catalog,
        app,
        &[
            feedback(first[0].id, " denser rows "),
            feedback(first[1].id, ""),
        ],
    )
    .await?;
    let stored = mockups_of(&catalog, construction.id).await?;
    assert_eq!(stored[0].feedback.as_deref(), Some("denser rows"));
    assert_eq!(stored[1].feedback, None);
    // A mockup of no construction of this application is refused.
    let foreign = set_mockup_feedback(&catalog, app, &[feedback(Uuid::new_v4(), "x")]).await;
    assert!(foreign.is_err());

    // A revision is the next round; the first stays readable.
    let second = save_mockups(&catalog, app, &round(&["Dense table"])).await?;
    assert_eq!(second[0].round, 2);
    let all = mockups_of(&catalog, construction.id).await?;
    assert_eq!(all.len(), 3);
    let latest: Vec<_> = latest_round(&all).iter().map(|m| m.title.clone()).collect();
    assert_eq!(latest, ["Dense table"]);
    assert_eq!(all[2].screens[1].name, "Order form");

    // One chosen at most: choosing another unchooses the first.
    choose_mockup(&catalog, app, first[1].id).await?;
    choose_mockup(&catalog, app, second[0].id).await?;
    let chosen: Vec<_> = mockups_of(&catalog, construction.id)
        .await?
        .into_iter()
        .filter(|m| m.status == MockupStatus::Chosen)
        .collect();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].id, second[0].id);
    assert!(choose_mockup(&catalog, app, Uuid::new_v4()).await.is_err());

    // Deleting the construction takes its mockups with it.
    delete_construction(&catalog, app).await?;
    assert!(mockups_of(&catalog, construction.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn two_rounds_saved_at_once_are_two_rounds() -> Result<()> {
    let (catalog, _db) = setup().await?;
    let app = Uuid::new_v4();
    let construction = create_construction(&catalog, app, None).await?;

    // Each save locks the construction before it counts the rounds, so the
    // second waits for the first and takes the next round.
    let (a, b) = (round(&["Table", "Cards"]), round(&["Split view"]));
    let (a, b) = tokio::join!(
        save_mockups(&catalog, app, &a),
        save_mockups(&catalog, app, &b),
    );
    let mut rounds = vec![a?[0].round, b?[0].round];
    rounds.sort_unstable();
    assert_eq!(rounds, [1, 2]);
    // A round is saved whole.
    let all = mockups_of(&catalog, construction.id).await?;
    assert_eq!(all.len(), 3);
    Ok(())
}

fn feedback(mockup: Uuid, text: &str) -> MockupFeedback {
    MockupFeedback {
        mockup,
        text: text.to_owned(),
    }
}
