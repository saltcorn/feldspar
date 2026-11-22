//! A construction's mockups (TODO-app-constructor AC7): screens of the
//! application as self-contained HTML, proposed by the construction agent
//! before the phases are planned, for the admin to choose from or comment on.
//!
//! The agent proposes a few at a time, a **round**; a revision is the next
//! round, written after the admin's feedback on the last one. Earlier rounds
//! are kept, so what was asked for stays readable. One mockup of a
//! construction at most is chosen, and the chosen one is what the phase plan,
//! the task plans and the feature tasks are told about ([`mockup_text`]).
//!
//! Optional: a construction with no mockups, or none chosen, is planned and
//! built from its specification alone.

use std::collections::HashSet;

use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, CaseArm, Expr, Insert, OrderBy, Projection, Select, Source, Statement, Update,
    Value,
};
use sc_types::BasicType;
use serde::Deserialize;
use uuid::Uuid;

use crate::model::{Mockup, MockupFeedback, MockupScreen, MockupStatus, parse, spelling};
use crate::store::{
    CONSTRUCTIONS_TABLE, from_json, insert_statement, opt_text, opt_text_value, req_float,
    req_text, req_uuid, require_construction, rows, run, to_json, tx_rows, tx_run,
};

/// A construction's mockups, every round.
pub const MOCKUPS_TABLE: &str = "_fd_construction_mockups";

/// The most mockups one round may have.
pub const MAX_MOCKUPS: usize = 3;
/// How many a proposal asks for unless told otherwise.
pub const DEFAULT_MOCKUPS: usize = 2;
/// The most screens one mockup may have: the screens that decide the
/// application's shape, not every page of it.
pub const MAX_SCREENS: usize = 8;
/// The longest a screen's HTML may be, in bytes.
pub const MAX_SCREEN_HTML: usize = 100_000;
/// The longest a mockup's title or a screen's name may be.
pub const MAX_NAME: usize = 120;

pub(crate) fn mockup_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        crate::store::key("construction", crate::store::CONSTRUCTIONS_TABLE).required(),
        DataField::plain("round", BasicType::Int).required(),
        DataField::plain("position", BasicType::Float).required(),
        DataField::plain("title", BasicType::Text).required(),
        DataField::plain("rationale", BasicType::Text).required(),
        DataField::plain("screens", BasicType::Json).required(),
        DataField::plain("status", BasicType::Text).required(),
        DataField::plain("feedback", BasicType::Text),
    ]
}

/// One mockup as the tool takes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockupInput {
    pub title: String,
    #[serde(default)]
    pub rationale: String,
    pub screens: Vec<MockupScreen>,
}

/// Parse and check a round of mockups, naming what is wrong with it: one to
/// [`MAX_MOCKUPS`] mockups, each with a title and one to [`MAX_SCREENS`]
/// screens of distinct names and some HTML no longer than
/// [`MAX_SCREEN_HTML`].
pub fn parse_mockups(value: &serde_json::Value) -> Result<Vec<MockupInput>> {
    let list = value.as_array().ok_or_else(|| {
        Error::invalid("`mockups` must be a list of `{title, rationale, screens: [{name, html}]}`")
    })?;
    if list.is_empty() || list.len() > MAX_MOCKUPS {
        return Err(Error::invalid(format!(
            "propose between 1 and {MAX_MOCKUPS} mockups, not {}",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        let n = i + 1;
        let mut mockup = serde_json::from_value::<MockupInput>(item.clone())
            .map_err(|e| Error::invalid(format!("mockup {n}: {e}")))?;
        mockup.title = mockup.title.trim().to_owned();
        mockup.rationale = mockup.rationale.trim().to_owned();
        if mockup.title.is_empty() {
            return Err(Error::invalid(format!("mockup {n} has no title")));
        }
        if mockup.title.chars().count() > MAX_NAME {
            return Err(Error::invalid(format!(
                "mockup {n}'s title is longer than {MAX_NAME} characters"
            )));
        }
        if mockup.screens.is_empty() || mockup.screens.len() > MAX_SCREENS {
            return Err(Error::invalid(format!(
                "mockup {n} must have between 1 and {MAX_SCREENS} screens, not {}",
                mockup.screens.len()
            )));
        }
        let mut names = HashSet::new();
        for screen in &mut mockup.screens {
            screen.name = screen.name.trim().to_owned();
            if screen.name.is_empty() {
                return Err(Error::invalid(format!(
                    "mockup {n} has a screen with no name"
                )));
            }
            if screen.name.chars().count() > MAX_NAME {
                return Err(Error::invalid(format!(
                    "mockup {n}: the screen name `{}` is longer than {MAX_NAME} characters",
                    screen.name
                )));
            }
            if !names.insert(screen_file(&screen.name)) {
                return Err(Error::invalid(format!(
                    "mockup {n} has two screens named `{}`",
                    screen.name
                )));
            }
            if screen.html.trim().is_empty() {
                return Err(Error::invalid(format!(
                    "mockup {n}: the screen `{}` has no HTML",
                    screen.name
                )));
            }
            if screen.html.len() > MAX_SCREEN_HTML {
                return Err(Error::invalid(format!(
                    "mockup {n}: the screen `{}` is longer than {MAX_SCREEN_HTML} bytes; \
                     keep a mockup to the layout and a few rows of sample data",
                    screen.name
                )));
            }
        }
        out.push(mockup);
    }
    Ok(out)
}

/// Save `mockups` as the next round of `application`'s construction,
/// answering them as saved.
///
/// All in one transaction: a failure saves none of the round. The
/// construction is locked first, so two saves at once take two rounds rather
/// than the same one.
pub async fn save_mockups(
    catalog: &Catalog,
    application: Uuid,
    mockups: &[MockupInput],
) -> Result<Vec<Mockup>> {
    let construction = require_construction(catalog, application).await?;
    let mockups = mockups.to_vec();
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let lock = Select::from(Source::table(CONSTRUCTIONS_TABLE))
                    .filter(Expr::col("id").eq(Expr::lit(construction.id)))
                    .for_update();
                if tx_rows(tx, lock).await?.is_empty() {
                    return Err(Error::not_found(
                        "the construction was deleted meanwhile".to_owned(),
                    ));
                }
                // The rounds only: the screens' HTML is not needed here.
                let rounds_query = Select::from(Source::table(MOCKUPS_TABLE))
                    .columns(vec![Projection::expr(Expr::col("round"))])
                    .filter(Expr::col("construction").eq(Expr::lit(construction.id)));
                let mut last_round = 0;
                for row in tx_rows(tx, rounds_query).await? {
                    last_round = last_round.max(req_round(&row)?);
                }
                let round = last_round + 1;
                let mut saved = Vec::with_capacity(mockups.len());
                for (i, input) in mockups.into_iter().enumerate() {
                    #[allow(clippy::cast_precision_loss)]
                    let mockup = Mockup {
                        id: Uuid::new_v4(),
                        construction: construction.id,
                        round,
                        position: (i + 1) as f64,
                        title: input.title,
                        rationale: input.rationale,
                        screens: input.screens,
                        status: MockupStatus::Proposed,
                        feedback: None,
                    };
                    tx_run(tx, &Statement::from(mockup_insert(&mockup)?)).await?;
                    saved.push(mockup);
                }
                Ok(saved)
            })
        },
    )
    .await
}

/// The `INSERT` of a new `mockup`.
fn mockup_insert(mockup: &Mockup) -> Result<Insert> {
    Ok(insert_statement(
        MOCKUPS_TABLE,
        &[
            "id",
            "construction",
            "round",
            "position",
            "title",
            "rationale",
            "screens",
            "status",
            "feedback",
        ],
        vec![
            Value::Uuid(mockup.id),
            Value::Uuid(mockup.construction),
            Value::Int(mockup.round),
            Value::Float(mockup.position),
            Value::Text(mockup.title.clone()),
            Value::Text(mockup.rationale.clone()),
            Value::Json(to_json(&mockup.screens)?),
            Value::Text(spelling(mockup.status)),
            Value::Null,
        ],
    ))
}

/// A construction's mockups, oldest round first and in order within it.
pub async fn mockups_of(catalog: &Catalog, construction: Uuid) -> Result<Vec<Mockup>> {
    // Should not happen: the server bootstraps the table at startup.
    if !matches!(catalog.get(MOCKUPS_TABLE), Ok(Some(_))) {
        return Ok(Vec::new());
    }
    let mut select = Select::from(Source::table(MOCKUPS_TABLE))
        .filter(Expr::col("construction").eq(Expr::lit(construction)));
    select.order = vec![
        OrderBy::asc(Expr::col("round")),
        OrderBy::asc(Expr::col("position")),
    ];
    rows(catalog, select)
        .await?
        .iter()
        .map(mockup_from_row)
        .collect()
}

/// The last round's mockups, in order: what the admin is choosing from.
pub fn latest_round(mockups: &[Mockup]) -> Vec<&Mockup> {
    let Some(round) = mockups.iter().map(|m| m.round).max() else {
        return Vec::new();
    };
    mockups.iter().filter(|m| m.round == round).collect()
}

/// The chosen mockup of a construction, if one is.
pub async fn chosen_mockup(catalog: &Catalog, construction: Uuid) -> Result<Option<Mockup>> {
    Ok(mockups_of(catalog, construction)
        .await?
        .into_iter()
        .find(|m| m.status == MockupStatus::Chosen))
}

/// Record the admin's feedback on mockups of `application`'s construction,
/// blank text clearing it. A mockup of another construction is refused.
pub async fn set_mockup_feedback(
    catalog: &Catalog,
    application: Uuid,
    feedback: &[MockupFeedback],
) -> Result<()> {
    let construction = require_construction(catalog, application).await?;
    let ids: HashSet<Uuid> = mockups_of(catalog, construction.id)
        .await?
        .iter()
        .map(|m| m.id)
        .collect();
    for MockupFeedback { mockup: id, text } in feedback {
        if !ids.contains(id) {
            return Err(Error::not_found(format!(
                "no mockup {id} in this construction"
            )));
        }
        let text = text.trim();
        let update = Update::new(
            MOCKUPS_TABLE,
            vec![Assignment::new(
                "feedback",
                Expr::Lit(opt_text_value((!text.is_empty()).then_some(text))),
            )],
        )
        .filter(Expr::col("id").eq(Expr::lit(*id)));
        run(catalog, Statement::from(update)).await?;
    }
    Ok(())
}

/// Choose `mockup` for `application`'s construction, answering it. Any other
/// chosen one goes back to proposed: one is chosen at most.
pub async fn choose_mockup(catalog: &Catalog, application: Uuid, mockup: Uuid) -> Result<Mockup> {
    let construction = require_construction(catalog, application).await?;
    let mockups = mockups_of(catalog, construction.id).await?;
    let Some(chosen) = mockups.iter().find(|m| m.id == mockup).cloned() else {
        return Err(Error::not_found(format!(
            "no mockup {mockup} in this construction"
        )));
    };
    // One statement over the construction's mockups: this one chosen, every
    // other proposed. Atomic, so two admins choosing at once never leave two
    // chosen.
    let spelled = |status: MockupStatus| Expr::Lit(Value::Text(spelling(status)));
    let status = Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: Expr::col("id").eq(Expr::lit(mockup)),
            then: spelled(MockupStatus::Chosen),
        }],
        else_result: Some(Box::new(spelled(MockupStatus::Proposed))),
    };
    let update = Update::new(MOCKUPS_TABLE, vec![Assignment::new("status", status)])
        .filter(Expr::col("construction").eq(Expr::lit(construction.id)));
    run(catalog, Statement::from(update)).await?;
    Ok(Mockup {
        status: MockupStatus::Chosen,
        ..chosen
    })
}

/// The file a screen is written to in the application's store, under
/// `mockups/`: its name in lower case, letters and digits kept and the rest
/// one dash ("Orders list" → `orders-list.html`).
pub fn screen_file(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    format!("{}.html", if slug.is_empty() { "screen" } else { slug })
}

/// The chosen mockup as the phase plan and the task plans are told it: its
/// title and rationale, the admin's feedback, and each screen's file and HTML.
/// The screens show the fields the data model needs.
pub fn mockup_text(mockup: &Mockup) -> String {
    let mut out = format!(
        "## The chosen mockup: {}\n\n{}\n",
        mockup.title,
        if mockup.rationale.is_empty() {
            "(no rationale given)"
        } else {
            &mockup.rationale
        }
    );
    if let Some(feedback) = &mockup.feedback {
        out.push_str(&format!("\nThe admin's feedback on it: {feedback}\n"));
    }
    for screen in &mockup.screens {
        out.push_str(&format!(
            "\n### {} (`mockups/{}`)\n\n```html\n{}\n```\n",
            screen.name,
            screen_file(&screen.name),
            screen.html.trim()
        ));
    }
    out
}

/// A row's `round`.
fn req_round(row: &Row) -> Result<i64> {
    match row.get("round") {
        Some(Value::Int(i)) => Ok(*i),
        other => Err(Error::invalid(format!(
            "{MOCKUPS_TABLE}.round should be an integer, got {}",
            other.map_or("nothing", |v| v.kind())
        ))),
    }
}

fn mockup_from_row(row: &Row) -> Result<Mockup> {
    let t = MOCKUPS_TABLE;
    Ok(Mockup {
        id: req_uuid(row, t, "id")?,
        construction: req_uuid(row, t, "construction")?,
        round: req_round(row)?,
        position: req_float(row, t, "position")?,
        title: req_text(row, t, "title")?,
        rationale: req_text(row, t, "rationale")?,
        screens: from_json(row, t, "screens")?,
        status: parse("mockup status", &req_text(row, t, "status")?)?,
        feedback: opt_text(row, t, "feedback")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn screen(name: &str) -> serde_json::Value {
        json!({ "name": name, "html": "<main>hi</main>" })
    }

    #[test]
    fn a_round_needs_one_to_three_mockups_each_with_screens() {
        assert!(parse_mockups(&json!([])).is_err());
        let one = json!({ "title": "Cards", "screens": [screen("List")] });
        assert_eq!(parse_mockups(&json!([one.clone()])).unwrap().len(), 1);
        assert!(parse_mockups(&json!([one.clone(), one.clone(), one.clone(), one])).is_err());
        let none = json!([{ "title": "Cards", "screens": [] }]);
        assert!(
            parse_mockups(&none)
                .unwrap_err()
                .to_string()
                .contains("between 1 and")
        );
    }

    #[test]
    fn screens_need_distinct_names_and_some_html() {
        let twice =
            json!([{ "title": "A", "screens": [screen("Orders list"), screen("orders  LIST")] }]);
        assert!(
            parse_mockups(&twice)
                .unwrap_err()
                .to_string()
                .contains("two screens")
        );
        let empty = json!([{ "title": "A", "screens": [{ "name": "List", "html": " " }] }]);
        assert!(
            parse_mockups(&empty)
                .unwrap_err()
                .to_string()
                .contains("no HTML")
        );
        let big = "x".repeat(MAX_SCREEN_HTML + 1);
        let long = json!([{ "title": "A", "screens": [{ "name": "List", "html": big }] }]);
        assert!(
            parse_mockups(&long)
                .unwrap_err()
                .to_string()
                .contains("longer than")
        );
        let untitled = json!([{ "title": " ", "screens": [screen("List")] }]);
        assert!(
            parse_mockups(&untitled)
                .unwrap_err()
                .to_string()
                .contains("no title")
        );
    }

    #[test]
    fn a_screens_file_is_its_name_as_a_slug() {
        assert_eq!(screen_file("Orders list"), "orders-list.html");
        assert_eq!(screen_file("  Edit / new order! "), "edit-new-order.html");
        assert_eq!(screen_file("???"), "screen.html");
    }

    #[test]
    fn the_prompt_text_names_each_screens_file_and_carries_its_html() {
        let mockup = Mockup {
            id: Uuid::nil(),
            construction: Uuid::nil(),
            round: 1,
            position: 1.0,
            title: "Cards".into(),
            rationale: "One card per order.".into(),
            screens: vec![MockupScreen {
                name: "Orders list".into(),
                html: "<table><th>Customer</th></table>".into(),
            }],
            status: MockupStatus::Chosen,
            feedback: Some("bigger buttons".into()),
        };
        let text = mockup_text(&mockup);
        assert!(text.contains("The chosen mockup: Cards"));
        assert!(text.contains("`mockups/orders-list.html`"));
        assert!(text.contains("<th>Customer</th>"));
        assert!(text.contains("bigger buttons"));
    }
}
