//! The Notebook workspace's state (analytics TODO A10.1; the goals document's
//! "Workspaces").
//!
//! A notebook is a list of **cells**, each text (Markdown) or code, and a
//! **language** chosen when it is created and never changed: `javascript`,
//! `python` or `llm`, in which a code cell is a prompt. A code cell keeps
//! what its last run made, so a notebook reopens with its outputs without
//! running anything:
//!
//! ```json
//! { "language": "javascript",
//!   "cells": [
//!     { "id": "c1", "kind": "text", "source": "# Houses" },
//!     { "id": "c2", "kind": "code", "source": "nb.table(d)",
//!       "outputs": [ { "kind": "panel", "panel": { "id": "…", "kind": "summary_table", "content": { … } } } ],
//!       "run": { "count": 3, "at": "2026-11-20T10:00:00Z", "ms": 41 } } ] }
//! ```
//!
//! An output is one of:
//!
//! | kind | fields |
//! |---|---|
//! | `panel` | `panel` — a panel (A4.2), dragged like any other |
//! | `text` | `text` — what the cell printed |
//! | `value` | `value` — the cell's last value, when it is not a panel, as JSON |
//! | `dataset` | `data`, `columns`, `rows`, `total` — a dataset the cell showed (A10.4): what it reads and its first rows |
//! | `error` | `message`, `line`? — why the cell stopped, and the line it stopped on |
//! | `calls` | `calls` — an LLM prompt cell's recorded calls, each `{ function, arguments }` |
//!
//! The panels in outputs are what the usage index reads
//! ([`panel_slots`]). Running cells — the session that keeps one cell's
//! globals for the next, and the `nb` functions — is A10.3–A10.5's, not this
//! module's: here is only what is stored, and what a stored state may be.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::panel::{MAX_TEXT, Panel, PanelBody};
use crate::plot::{Cell as TableCell, DataRef, TableSpec};

/// The most cells a notebook holds.
pub const MAX_CELLS: usize = 500;
/// The longest a cell's source may be, in bytes.
pub const MAX_SOURCE: usize = 100_000;
/// The most outputs one cell keeps.
pub const MAX_OUTPUTS: usize = 50;
/// The most calls an LLM prompt cell records.
pub const MAX_CALLS: usize = 100;

/// The most rows a dataset output keeps.
pub const MAX_DATASET_ROWS: usize = 100;

/// What a notebook's code cells are written in, chosen when it is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotebookLanguage {
    /// Run on the `run_js_code` isolate.
    Javascript,
    /// Run on `sc-python`'s interpreter.
    Python,
    /// A prompt, answered by an agent whose tools are the `nb` functions.
    Llm,
}

impl NotebookLanguage {
    /// Every language, in the order the create dialog lists them.
    pub const ALL: [NotebookLanguage; 3] = [
        NotebookLanguage::Javascript,
        NotebookLanguage::Python,
        NotebookLanguage::Llm,
    ];

    /// Its name as it is stored and sent.
    pub fn as_str(self) -> &'static str {
        match self {
            NotebookLanguage::Javascript => "javascript",
            NotebookLanguage::Python => "python",
            NotebookLanguage::Llm => "llm",
        }
    }

    /// Its name for a person.
    pub fn label(self) -> &'static str {
        match self {
            NotebookLanguage::Javascript => "JavaScript",
            NotebookLanguage::Python => "Python",
            NotebookLanguage::Llm => "LLM prompt",
        }
    }

    /// The language called `name`.
    pub fn parse(name: &str) -> Result<NotebookLanguage> {
        NotebookLanguage::ALL
            .into_iter()
            .find(|l| l.as_str() == name)
            .ok_or_else(|| Error::invalid(not_a_language(name)))
    }
}

fn not_a_language(name: &str) -> String {
    format!(
        "`{name}` is not a notebook's language; it is one of {}",
        NotebookLanguage::ALL
            .iter()
            .map(|l| format!("`{}`", l.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A notebook: its language and its cells, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookState {
    /// What its code cells are written in.
    pub language: NotebookLanguage,
    /// Its cells, top first.
    #[serde(default)]
    pub cells: Vec<Cell>,
}

/// Whether a cell is Markdown or code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    /// Markdown, rendered in place; never run.
    Text,
    /// Code in the notebook's language — a prompt in an LLM notebook.
    Code,
}

/// One cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// Its identity within the notebook: what a run and the browser name it
    /// by, since its position changes when cells are moved.
    pub id: String,
    /// Text or code.
    pub kind: CellKind,
    /// The Markdown or the code.
    #[serde(default)]
    pub source: String,
    /// What its last run made, in order; none for a text cell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<Output>,
    /// Its last run, if it has been run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<CellRun>,
}

/// When a cell was last run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellRun {
    /// Its place in the order the session ran cells in: Jupyter's `[3]`.
    pub count: u64,
    /// When it finished.
    pub at: DateTime<Utc>,
    /// How long it took, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

/// One thing a cell's run made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Output {
    /// A panel: `nb.plot(…)`, `nb.table(…)`, … or the last value when it is
    /// one.
    Panel {
        /// The panel.
        panel: Box<Panel>,
    },
    /// What the cell printed.
    Text {
        /// The text.
        text: String,
    },
    /// The cell's last value, when it is not a panel.
    Value {
        /// The value, as JSON.
        value: Json,
    },
    /// Why the cell stopped.
    Error {
        /// The sentence.
        message: String,
        /// The line of the cell it stopped on, from 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
    },
    /// The calls an LLM prompt cell's agent made, in order: replayed when the
    /// cell is run again, until it is regenerated.
    Calls {
        /// The calls.
        calls: Vec<RecordedCall>,
        /// The prompt they were made for: a cell whose prompt has changed
        /// since asks the LLM again rather than replaying them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// A dataset the cell showed: what it reads — a non-persisted dataset
    /// (A10.2) or a stored one — and its first rows as they were when it ran.
    Dataset {
        /// What it reads.
        data: DataRef,
        /// Its columns' names.
        columns: Vec<String>,
        /// Its first rows, each in column order.
        #[serde(default)]
        rows: Vec<Vec<Json>>,
        /// How many rows it had.
        total: u64,
    },
}

/// One `nb` function an LLM prompt cell's agent called.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedCall {
    /// The function's name: `plot`, `dataset`, ….
    pub function: String,
    /// Its arguments, as the agent sent them.
    #[serde(default)]
    pub arguments: Json,
}

impl NotebookState {
    /// An empty notebook in `language`: what a new one's state is.
    pub fn new(language: NotebookLanguage) -> NotebookState {
        NotebookState {
            language,
            cells: Vec::new(),
        }
    }

    /// The notebook a workspace's state is, or the sentence saying why it is
    /// not one.
    pub fn from_json(state: &Json) -> Result<NotebookState> {
        match state.get("language") {
            None | Some(Json::Null) => {
                return Err(Error::invalid(
                    "a notebook's language — `javascript`, `python` or `llm` — is chosen when it \
                     is created",
                ));
            }
            Some(Json::String(name)) => {
                NotebookLanguage::parse(name)?;
            }
            Some(other) => return Err(Error::invalid(not_a_language(&other.to_string()))),
        }
        let notebook: NotebookState = serde_json::from_value(state.clone())
            .map_err(|e| Error::invalid(format!("this is not a notebook: {e}")))?;
        notebook.check()?;
        Ok(notebook)
    }

    /// The state as a workspace stores it.
    pub fn to_json(&self) -> Json {
        serde_json::to_value(self).unwrap_or_else(|_| Json::Object(serde_json::Map::new()))
    }

    /// What is wrong with it, naming the cell.
    pub fn check(&self) -> Result<()> {
        if self.cells.len() > MAX_CELLS {
            return Err(Error::invalid(format!(
                "a notebook holds at most {MAX_CELLS} cells"
            )));
        }
        let mut ids = BTreeSet::new();
        for (i, cell) in self.cells.iter().enumerate() {
            let refuse =
                |why: String| Error::invalid(format!("cell {} of the notebook {why}", i + 1));
            if cell.id.trim().is_empty() {
                return Err(refuse("has no id".into()));
            }
            if !ids.insert(cell.id.as_str()) {
                return Err(refuse(format!(
                    "has the id `{}` of a cell above it",
                    cell.id
                )));
            }
            if cell.source.len() > MAX_SOURCE {
                return Err(refuse(format!("is longer than {MAX_SOURCE} bytes")));
            }
            if cell.kind == CellKind::Text && (!cell.outputs.is_empty() || cell.run.is_some()) {
                return Err(refuse(
                    "is text, which is never run and has no outputs".into(),
                ));
            }
            if cell.outputs.len() > MAX_OUTPUTS {
                return Err(refuse(format!("keeps more than {MAX_OUTPUTS} outputs")));
            }
            for (j, output) in cell.outputs.iter().enumerate() {
                self.check_output(output)
                    .map_err(|why| refuse(format!("has an output {} that {why}", j + 1)))?;
            }
        }
        Ok(())
    }

    /// What is wrong with one output, as the end of a sentence.
    fn check_output(&self, output: &Output) -> std::result::Result<(), String> {
        match output {
            Output::Panel { panel } => panel.check().map_err(|e| e.message().to_owned()),
            Output::Text { text } if text.len() > MAX_TEXT => {
                Err(format!("is more than {MAX_TEXT} bytes of text"))
            }
            Output::Value { value } if value.to_string().len() > MAX_TEXT => {
                Err(format!("is a value of more than {MAX_TEXT} bytes"))
            }
            Output::Error { message, .. } if message.len() > MAX_TEXT => {
                Err(format!("is an error of more than {MAX_TEXT} bytes"))
            }
            Output::Error { line: Some(0), .. } => {
                Err("names line 0: lines are counted from 1".into())
            }
            Output::Calls { .. } if self.language != NotebookLanguage::Llm => Err(format!(
                "is an LLM prompt's calls, in a {} notebook",
                self.language.label()
            )),
            Output::Calls { calls, .. } if calls.len() > MAX_CALLS => {
                Err(format!("records more than {MAX_CALLS} calls"))
            }
            Output::Calls { calls, .. } if calls.iter().any(|c| c.function.trim().is_empty()) => {
                Err("records a call without the function it called".into())
            }
            Output::Dataset { rows, .. } if rows.len() > MAX_DATASET_ROWS => Err(format!(
                "is a dataset keeping more than {MAX_DATASET_ROWS} rows"
            )),
            Output::Dataset { rows, columns, .. }
                if rows.iter().any(|r| r.len() != columns.len()) =>
            {
                Err("is a dataset with a row that is not one value per column".into())
            }
            _ => Ok(()),
        }
    }
}

/// The JSON values in a notebook state where its panels are: the `panel` of
/// each output whose `kind` is `panel`, cell by cell. Read loosely, as the
/// other kinds' places are, so a state that is not a notebook has none here
/// and [`NotebookState::from_json`] says why.
pub(crate) fn panel_slots(state: &Json) -> Vec<&Json> {
    state
        .get("cells")
        .and_then(Json::as_array)
        .map(|cells| {
            cells
                .iter()
                .filter_map(|c| c.get("outputs").and_then(Json::as_array))
                .flatten()
                .filter(|o| o.get("kind").and_then(Json::as_str) == Some("panel"))
                .filter_map(|o| o.get("panel"))
                .collect()
        })
        .unwrap_or_default()
}

/// Refuse a saved state that changes a notebook's language: the code in its
/// cells is written in it. A stored state without one (none can be made, but
/// a restore brings what it brings) may be given one.
/// The panels standing for what a notebook's dataset outputs read: a count of
/// each one's rows, so the usage index, an application's reach and **Save as
/// dataset** find what those outputs read as they find a panel's.
pub(crate) fn dataset_output_panels(state: &Json) -> Vec<Panel> {
    state
        .get("cells")
        .and_then(Json::as_array)
        .map(|cells| {
            cells
                .iter()
                .filter_map(|c| c.get("outputs").and_then(Json::as_array))
                .flatten()
                .filter(|o| o.get("kind").and_then(Json::as_str) == Some("dataset"))
                .filter_map(|o| serde_json::from_value::<DataRef>(o.get("data")?.clone()).ok())
                .map(|data| {
                    Panel::new(PanelBody::SummaryTable {
                        spec: TableSpec {
                            data,
                            fold: None,
                            rows: Vec::new(),
                            columns: Vec::new(),
                            cells: vec![TableCell::count()],
                            totals: false,
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn check_language_kept(old: &Json, new: &Json) -> Result<()> {
    let language = |s: &Json| s.get("language").and_then(Json::as_str).map(str::to_owned);
    match (language(old), language(new)) {
        (Some(was), Some(now)) if was != now => Err(Error::invalid(format!(
            "a notebook's language is chosen when it is created: this one is `{was}`, not \
             `{now}`; create a new notebook instead"
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn text_panel() -> Json {
        json!({ "id": Uuid::new_v4(), "kind": "text", "content": { "markdown": "hi" } })
    }

    fn notebook(language: &str, cells: Json) -> Json {
        json!({ "language": language, "cells": cells })
    }

    #[test]
    fn a_notebook_round_trips_with_every_kind_of_output() {
        let state = notebook(
            "llm",
            json!([
                { "id": "a", "kind": "text", "source": "# Houses" },
                { "id": "b", "kind": "code", "source": "a box plot",
                  "outputs": [
                      { "kind": "panel", "panel": text_panel() },
                      { "kind": "text", "text": "printed" },
                      { "kind": "value", "value": { "n": 3 } },
                      { "kind": "error", "message": "no column `pirce`", "line": 2 },
                      { "kind": "calls", "calls": [
                          { "function": "plot", "arguments": { "mark": "boxplot" } } ] }
                  ],
                  "run": { "count": 1, "at": "2026-11-20T10:00:00Z", "ms": 41 } }
            ]),
        );
        let nb = NotebookState::from_json(&state).expect("a notebook");
        assert_eq!(nb.language, NotebookLanguage::Llm);
        assert_eq!(nb.cells.len(), 2);
        assert_eq!(nb.cells[1].outputs.len(), 5);
        assert_eq!(nb.to_json(), state);
        assert_eq!(panel_slots(&state).len(), 1);
    }

    #[test]
    fn a_notebook_without_a_language_or_with_a_wrong_one_is_refused() {
        let err = NotebookState::from_json(&json!({ "cells": [] })).unwrap_err();
        assert!(
            err.to_string().contains("chosen when it is created"),
            "{err}"
        );
        let err = NotebookState::from_json(&notebook("cobol", json!([]))).unwrap_err();
        assert!(
            err.to_string()
                .contains("`cobol` is not a notebook's language"),
            "{err}"
        );
        for language in NotebookLanguage::ALL {
            assert_eq!(
                NotebookLanguage::parse(language.as_str()).unwrap(),
                language
            );
            NotebookState::from_json(&NotebookState::new(language).to_json()).unwrap();
        }
    }

    #[test]
    fn cells_are_checked_and_named_by_position() {
        let refused = |cells: Json, language: &str, says: &str| {
            let err = NotebookState::from_json(&notebook(language, cells)).unwrap_err();
            assert!(err.to_string().contains(says), "{err} — wanted {says}");
        };
        refused(
            json!([{ "id": "a", "kind": "code" }, { "id": "a", "kind": "code" }]),
            "python",
            "cell 2 of the notebook has the id `a`",
        );
        refused(
            json!([{ "id": " ", "kind": "code" }]),
            "python",
            "has no id",
        );
        refused(
            json!([{ "id": "a", "kind": "text", "outputs": [{ "kind": "text", "text": "x" }] }]),
            "python",
            "is text, which is never run",
        );
        refused(
            json!([{ "id": "a", "kind": "code",
                     "outputs": [{ "kind": "calls", "calls": [] }] }]),
            "javascript",
            "an LLM prompt's calls, in a JavaScript notebook",
        );
        refused(
            json!([{ "id": "a", "kind": "code",
                     "outputs": [{ "kind": "error", "message": "x", "line": 0 }] }]),
            "javascript",
            "lines are counted from 1",
        );
        refused(
            json!([{ "id": "a", "kind": "code", "source": "x".repeat(MAX_SOURCE + 1) }]),
            "javascript",
            "longer than",
        );
        refused(
            json!([{ "id": "a", "kind": "markdown" }]),
            "javascript",
            "not a notebook",
        );
    }

    #[test]
    fn the_language_is_kept() {
        let js = NotebookState::new(NotebookLanguage::Javascript).to_json();
        let py = NotebookState::new(NotebookLanguage::Python).to_json();
        check_language_kept(&js, &js).unwrap();
        check_language_kept(&json!({}), &py).unwrap();
        let err = check_language_kept(&js, &py).unwrap_err();
        assert!(
            err.to_string().contains("this one is `javascript`"),
            "{err}"
        );
    }
}
