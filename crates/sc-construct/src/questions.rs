//! A construction's clarifying questions (TODO-app-constructor AC8): what the
//! construction agent asks about the specification before the mockups are
//! drawn and the phases planned, and the admin's answers.
//!
//! Asking again replaces the open questions and keeps the answered ones, so
//! nothing the admin wrote is lost and the agent is not asked twice for the
//! same thing. Questions come before the mockups, which are drawn from the
//! answers: asking discards the construction's mockups, every round, as they
//! were drawn without what is asked now. The answered questions are what the mockup run, the phase plan
//! and every later run are told ([`answers_text`]), beside the specification.
//!
//! Optional: a construction with no questions, or none answered, is built
//! from its specification alone.

use std::collections::HashSet;

use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, Delete, Expr, OrderBy, Projection, Select, Source, Statement, UnOp, Update, Value,
};
use sc_types::BasicType;
use uuid::Uuid;

use crate::mockups::MOCKUPS_TABLE;
use crate::model::{Question, QuestionAnswer};
use crate::store::{
    CONSTRUCTIONS_TABLE, insert_statement, opt_text, opt_text_value, req_float, req_text, req_uuid,
    require_construction, rows, run, tx_rows, tx_run,
};

/// A construction's questions.
pub const QUESTIONS_TABLE: &str = "_fd_construction_questions";

/// The most questions one asking may add: fewer is better.
pub const MAX_QUESTIONS: usize = 10;
/// The longest a question may be, in characters.
pub const MAX_QUESTION: usize = 500;

pub(crate) fn question_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        crate::store::key("construction", crate::store::CONSTRUCTIONS_TABLE).required(),
        DataField::plain("position", BasicType::Float).required(),
        DataField::plain("question", BasicType::Text).required(),
        DataField::plain("answer", BasicType::Text),
    ]
}

/// Parse and check the questions the agent asks, naming what is wrong: one to
/// [`MAX_QUESTIONS`] distinct, non-blank texts of at most [`MAX_QUESTION`]
/// characters.
pub fn parse_questions(value: &serde_json::Value) -> Result<Vec<String>> {
    let list = value
        .as_array()
        .ok_or_else(|| Error::invalid("`questions` must be a list of texts"))?;
    if list.is_empty() || list.len() > MAX_QUESTIONS {
        return Err(Error::invalid(format!(
            "ask between 1 and {MAX_QUESTIONS} questions, not {}",
            list.len()
        )));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        let n = i + 1;
        let text = item
            .as_str()
            .ok_or_else(|| Error::invalid(format!("question {n} must be a text")))?
            .trim();
        if text.is_empty() {
            return Err(Error::invalid(format!("question {n} is blank")));
        }
        if text.chars().count() > MAX_QUESTION {
            return Err(Error::invalid(format!(
                "question {n} is longer than {MAX_QUESTION} characters; ask one thing at a time"
            )));
        }
        if !seen.insert(text.to_lowercase()) {
            return Err(Error::invalid(format!("question {n} is asked twice")));
        }
        out.push(text.to_owned());
    }
    Ok(out)
}

/// What [`save_questions`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedQuestions {
    /// The construction's questions as they now stand: the answered ones
    /// first, as they were, then the new ones.
    pub questions: Vec<Question>,
    /// How many mockups were discarded.
    pub discarded_mockups: usize,
}

/// Save `questions` for `application`'s construction. The open questions
/// there were are replaced, and the mockups discarded, chosen one included.
///
/// All in one transaction, the construction locked first, so an answer saved
/// meanwhile is either kept or its question replaced, never half of each, and
/// a mockup saved meanwhile is either discarded or saved after.
pub async fn save_questions(
    catalog: &Catalog,
    application: Uuid,
    questions: &[String],
) -> Result<SavedQuestions> {
    let construction = require_construction(catalog, application).await?;
    let questions = questions.to_vec();
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
                let delete_unanswered = Delete::from(QUESTIONS_TABLE).filter(
                    Expr::col("construction")
                        .eq(Expr::lit(construction.id))
                        .and(Expr::unary(UnOp::IsNull, Expr::col("answer"))),
                );
                tx_run(tx, &Statement::from(delete_unanswered)).await?;
                let mut delete_mockups = Delete::from(MOCKUPS_TABLE)
                    .filter(Expr::col("construction").eq(Expr::lit(construction.id)));
                delete_mockups.returning = vec![Projection::expr(Expr::col("id"))];
                let discarded_mockups = tx
                    .query(&Statement::from(delete_mockups))
                    .await?
                    .try_collect()
                    .await?
                    .len();
                let mut all = tx_rows(tx, questions_select(construction.id))
                    .await?
                    .iter()
                    .map(question_from_row)
                    .collect::<Result<Vec<_>>>()?;
                let last = all.iter().map(|q| q.position).fold(0.0, f64::max);
                for (i, text) in questions.into_iter().enumerate() {
                    #[allow(clippy::cast_precision_loss)]
                    let question = Question {
                        id: Uuid::new_v4(),
                        construction: construction.id,
                        position: last + (i + 1) as f64,
                        text,
                        answer: None,
                    };
                    let insert = insert_statement(
                        QUESTIONS_TABLE,
                        &["id", "construction", "position", "question", "answer"],
                        vec![
                            Value::Uuid(question.id),
                            Value::Uuid(question.construction),
                            Value::Float(question.position),
                            Value::Text(question.text.clone()),
                            Value::Null,
                        ],
                    );
                    tx_run(tx, &Statement::from(insert)).await?;
                    all.push(question);
                }
                Ok(SavedQuestions {
                    questions: all,
                    discarded_mockups,
                })
            })
        },
    )
    .await
}

/// A construction's questions, in order.
pub async fn questions_of(catalog: &Catalog, construction: Uuid) -> Result<Vec<Question>> {
    // Should not happen: the server bootstraps the table at startup.
    if !matches!(catalog.get(QUESTIONS_TABLE), Ok(Some(_))) {
        return Ok(Vec::new());
    }
    rows(catalog, questions_select(construction))
        .await?
        .iter()
        .map(question_from_row)
        .collect()
}

fn questions_select(construction: Uuid) -> Select {
    let mut select = Select::from(Source::table(QUESTIONS_TABLE))
        .filter(Expr::col("construction").eq(Expr::lit(construction)));
    select.order = vec![OrderBy::asc(Expr::col("position"))];
    select
}

/// Record the admin's answers to questions of `application`'s construction,
/// blank text leaving a question open. A question of another construction is
/// refused, before anything is saved.
pub async fn answer_questions(
    catalog: &Catalog,
    application: Uuid,
    answers: &[QuestionAnswer],
) -> Result<()> {
    let construction = require_construction(catalog, application).await?;
    let ids: HashSet<Uuid> = questions_of(catalog, construction.id)
        .await?
        .iter()
        .map(|q| q.id)
        .collect();
    if let Some(unknown) = answers.iter().find(|a| !ids.contains(&a.question)) {
        return Err(Error::not_found(format!(
            "no question {} in this construction",
            unknown.question
        )));
    }
    for QuestionAnswer { question, text } in answers {
        let text = text.trim();
        let update = Update::new(
            QUESTIONS_TABLE,
            vec![Assignment::new(
                "answer",
                Expr::Lit(opt_text_value((!text.is_empty()).then_some(text))),
            )],
        )
        .filter(Expr::col("id").eq(Expr::lit(*question)));
        run(catalog, Statement::from(update)).await?;
    }
    Ok(())
}

/// The answered questions as the agent is told them, beside the
/// specification; `None` when none is answered.
pub fn answers_text(questions: &[Question]) -> Option<String> {
    let pairs: Vec<String> = questions
        .iter()
        .filter_map(|q| {
            q.answer
                .as_deref()
                .map(|a| format!("Question: {}\nAnswer: {}", q.text, a.trim()))
        })
        .collect();
    (!pairs.is_empty()).then(|| {
        format!(
            "## The admin's answers to the clarifying questions\n\n{}",
            pairs.join("\n\n")
        )
    })
}

fn question_from_row(row: &Row) -> Result<Question> {
    let t = QUESTIONS_TABLE;
    Ok(Question {
        id: req_uuid(row, t, "id")?,
        construction: req_uuid(row, t, "construction")?,
        position: req_float(row, t, "position")?,
        text: req_text(row, t, "question")?,
        answer: opt_text(row, t, "answer")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_to_ten_distinct_questions_each_a_text() {
        assert!(parse_questions(&json!([])).is_err());
        assert_eq!(
            parse_questions(&json!([" Who uses it? ", "How many?"])).unwrap(),
            vec!["Who uses it?", "How many?"]
        );
        let eleven: Vec<String> = (0..=MAX_QUESTIONS).map(|i| format!("Q{i}?")).collect();
        assert!(parse_questions(&json!(eleven)).is_err());
        let error = |v| parse_questions(&v).unwrap_err().to_string();
        assert!(error(json!(["A?", "a?"])).contains("asked twice"));
        assert!(error(json!(["A?", " "])).contains("blank"));
        assert!(error(json!([1])).contains("must be a text"));
        assert!(error(json!(["x".repeat(MAX_QUESTION + 1)])).contains("longer than"));
    }

    #[test]
    fn only_answered_questions_are_told() {
        let question = |text: &str, answer: Option<&str>| Question {
            id: Uuid::nil(),
            construction: Uuid::nil(),
            position: 1.0,
            text: text.into(),
            answer: answer.map(Into::into),
        };
        assert_eq!(answers_text(&[question("Who?", None)]), None);
        let text = answers_text(&[
            question("Who uses it?", Some("The sales team ")),
            question("Offline?", None),
        ])
        .unwrap();
        assert!(text.contains("Question: Who uses it?\nAnswer: The sales team"));
        assert!(!text.contains("Offline?"));
    }
}
