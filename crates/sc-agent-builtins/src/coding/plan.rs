//! The plan: what a planner run is doing, kept in its own state (TODO §8).
//!
//! **A plan is one planner run's state**, not a file. It lives in `coding`'s
//! per-run trait state ([`CodingState::plan`]), inside the planner run's row, so
//! it is saved after every step, restored on resume, and untouched by
//! compaction, which rewrites only the history. There are no `.agent/` files and
//! nothing lands in the application's repository.
//!
//! - **`features`** are the units of work, each done in one fresh `act` session
//!   by `implement_feature` ([`super::feature`]). The planner writes the list with
//!   `save_plan`; the harness owns each feature's `status`, `attempts` and
//!   `runs`, which a new `save_plan` keeps for the ids it keeps.
//! - **`progress`** is the handoff record: one entry per session, appended by the
//!   harness, with the session's closing summary, its check result and its
//!   diffstat. A feature's session is briefed with the last few.
//!
//! Every plan tool's result ends with [`checklist`], so the freshest copy of the
//! plan is always the last thing in the planner's context: the session header
//! does not repeat it.

use sc_agent::TraitContext;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

use super::state::CodingState;
use crate::files::FileScope;

/// The most features one plan may hold.
pub const MAX_FEATURES: usize = 30;

/// How many progress entries a feature's briefing carries.
pub const BRIEF_PROGRESS: usize = 3;

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("save_plan_{}", scope.slug())
}

/// What kind of work a feature is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// New behaviour.
    #[default]
    Feature,
    /// Wrong behaviour, reproduced before it is fixed (TODO 9.7).
    Bug,
}

/// Where a feature has got to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Not started, or back to be tried again after a failed session.
    #[default]
    Todo,
    /// A session is running, or was when the server stopped: its run is the
    /// last of [`Feature::runs`], and is driven on rather than replaced.
    InProgress,
    /// A session passed the independent check.
    Done,
    /// Two sessions in a row failed.
    Failed,
    /// The planner set it aside.
    Blocked,
}

impl Status {
    /// The checklist's mark.
    fn mark(self) -> &'static str {
        match self {
            Status::Todo => "[ ]",
            Status::InProgress => "[~]",
            Status::Done => "[x]",
            Status::Failed => "[!]",
            Status::Blocked => "[-]",
        }
    }
}

/// One unit of work: one session of the executor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    /// Chosen by the planner; stable across `save_plan` calls.
    pub id: String,
    /// One line.
    pub title: String,
    /// What to do, for the session that does it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Feature or bug.
    #[serde(default)]
    pub kind: Kind,
    /// What must be true when it is done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    /// The files it likely touches, relative to the scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Routes of the application to look at once it passes (TODO §7b).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<String>,
    /// The checks that matter most to it, for the session's brief.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// The planner's notes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// Where it has got to. The harness's.
    #[serde(default)]
    pub status: Status,
    /// Failed sessions in a row. The harness's.
    #[serde(default)]
    pub attempts: u32,
    /// Its sessions' run ids, latest last. The harness's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<String>,
}

/// One session's handoff entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// The feature the session was for.
    pub feature: String,
    /// The session's run id.
    pub run: String,
    /// The feature's status after the session.
    pub status: Status,
    /// The session's closing summary.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// The independent check's first line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub check: String,
    /// What changed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub diffstat: String,
}

/// The plan state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The features, in order.
    #[serde(default)]
    pub features: Vec<Feature>,
    /// One entry per session, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress: Vec<Progress>,
}

impl Plan {
    /// The feature with `id`, or an error listing the ids there are.
    pub fn feature_mut(&mut self, id: &str) -> Result<&mut Feature> {
        let ids = self.ids();
        self.features
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "the plan has no feature `{id}`; its features are {ids}"
                ))
            })
    }

    fn ids(&self) -> String {
        match self.features.is_empty() {
            true => "none".to_owned(),
            false => self
                .features
                .iter()
                .map(|f| format!("`{}`", f.id))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// Replace the feature list with `features`, keeping what the harness owns
    /// for every id that stays. A feature whose session is running may not be
    /// dropped: its run is still recorded against it.
    pub fn replace(&mut self, features: Vec<Feature>) -> Result<()> {
        for old in &self.features {
            if old.status == Status::InProgress && !features.iter().any(|f| f.id == old.id) {
                return Err(Error::invalid(format!(
                    "feature `{}` has a session in progress and cannot be dropped from the plan",
                    old.id
                )));
            }
        }
        let mut next = Vec::with_capacity(features.len());
        for mut feature in features {
            if let Some(old) = self.features.iter().find(|f| f.id == feature.id) {
                // The planner may set a feature aside, or take it back up;
                // anything else about its progress is the harness's to say.
                feature.status = match (old.status, feature.status) {
                    (Status::Todo | Status::Blocked, wanted @ (Status::Todo | Status::Blocked)) => {
                        wanted
                    }
                    (kept, _) => kept,
                };
                feature.attempts = old.attempts;
                feature.runs = old.runs.clone();
            }
            next.push(feature);
        }
        self.features = next;
        Ok(())
    }
}

/// The `save_plan` tool.
pub fn spec(scope: &FileScope) -> ToolSpec {
    let strings = json!({"type": "array", "items": {"type": "string"}});
    ToolSpec::new(
        tool_name(scope),
        "Save the plan, replacing any earlier one: the features, in the order they should be \
         built, each one session of work, with acceptance criteria. A feature whose `id` stays \
         the same keeps its progress, so re-planning does not lose finished work."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {
                "features": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "title": {"type": "string"},
                            "description": {"type": "string"},
                            "kind": {"type": "string", "enum": ["feature", "bug"]},
                            "acceptance": strings,
                            "files": strings,
                            "pages": {"type": "array", "items": {"type": "string"}, "description": "routes to look at, e.g. /tasks"},
                            "checks": strings,
                            "notes": {"type": "string"},
                            "status": {"type": "string", "enum": ["todo", "blocked"]},
                        },
                        "required": ["id", "title"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["features"],
            "additionalProperties": false,
        }),
    )
}

/// The features `save_plan`'s arguments describe, validated.
pub fn parse_features(args: &Json) -> Result<Vec<Feature>> {
    let items = args
        .get("features")
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid("`features` must be a list of features"))?;
    if items.is_empty() {
        return Err(Error::invalid(
            "a plan needs at least one feature; a one-line fix is a one-feature plan",
        ));
    }
    if items.len() > MAX_FEATURES {
        return Err(Error::invalid(format!(
            "a plan may hold at most {MAX_FEATURES} features, got {}; group them",
            items.len()
        )));
    }
    let mut features: Vec<Feature> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let at = |msg: String| Error::invalid(format!("feature {}: {msg}", i + 1));
        let map: &Map<String, Json> = item
            .as_object()
            .ok_or_else(|| at("must be an object".to_owned()))?;
        let mut feature: Feature =
            serde_json::from_value(item.clone()).map_err(|e| at(e.to_string()))?;
        // What the harness owns is never taken from the model.
        feature.attempts = 0;
        feature.runs = Vec::new();
        if map.contains_key("attempts") || map.contains_key("runs") {
            return Err(at(
                "`attempts` and `runs` are kept by the harness".to_owned()
            ));
        }
        if !matches!(feature.status, Status::Todo | Status::Blocked) {
            return Err(at(
                "`status` may only be `todo` or `blocked`; the harness sets the rest".to_owned(),
            ));
        }
        feature.id = feature.id.trim().to_owned();
        feature.title = feature.title.trim().to_owned();
        if feature.id.is_empty() || feature.id.chars().any(char::is_whitespace) {
            return Err(at(format!(
                "`id` must be a non-empty word, got `{}`",
                feature.id
            )));
        }
        if feature.title.is_empty() {
            return Err(at(format!("`{}` needs a title", feature.id)));
        }
        if features.iter().any(|f| f.id == feature.id) {
            return Err(at(format!("the id `{}` is used twice", feature.id)));
        }
        if let Some(page) = feature.pages.iter().find(|p| !p.starts_with('/')) {
            return Err(at(format!(
                "`pages` are routes starting with `/`, got `{page}`"
            )));
        }
        features.push(feature);
    }
    Ok(features)
}

/// `save_plan`: replace the feature list and show the plan.
pub async fn call(args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let features = parse_features(args)?;
    let mut state = CodingState::load(ctx.trait_state);
    let plan = state.plan.get_or_insert_with(Plan::default);
    plan.replace(features)?;
    let out = format!("plan saved.\n{}", checklist(plan));
    state.store(ctx.trait_state);
    Ok(Json::String(out))
}

/// The plan in a few lines: a count, then one line per feature.
pub fn checklist(plan: &Plan) -> String {
    let done = plan
        .features
        .iter()
        .filter(|f| f.status == Status::Done)
        .count();
    let mut out = format!("<plan> {done} of {} done", plan.features.len());
    for f in &plan.features {
        let extra = match f.status {
            Status::InProgress => " (in progress)".to_owned(),
            Status::Failed => format!(" (failed, {} sessions)", f.runs.len()),
            Status::Blocked => " (blocked)".to_owned(),
            Status::Todo if f.attempts > 0 => format!(" ({} failed)", f.attempts),
            _ => String::new(),
        };
        out.push_str(&format!(
            "\n{} {}: {}{extra}",
            f.status.mark(),
            f.id,
            f.title
        ));
    }
    out.push_str("\n</plan>");
    out
}

/// The briefing a feature's session starts with: the feature, the last few
/// progress entries, and for a bug, reproducing it first.
pub fn briefing(feature: &Feature, progress: &[Progress], may_check: bool) -> String {
    let mut out = format!("Implement feature `{}`: {}", feature.id, feature.title);
    if !feature.description.is_empty() {
        out.push_str(&format!("\n\n{}", feature.description));
    }
    let list = |heading: &str, items: &[String]| match items.is_empty() {
        true => String::new(),
        false => format!(
            "\n\n{heading}:\n{}",
            items
                .iter()
                .map(|i| format!("- {i}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    out.push_str(&list("Acceptance", &feature.acceptance));
    out.push_str(&list("Likely files", &feature.files));
    out.push_str(&list("Checks that matter", &feature.checks));
    out.push_str(&list("Pages it shows on", &feature.pages));
    if !feature.notes.is_empty() {
        out.push_str(&format!("\n\nNotes: {}", feature.notes));
    }
    if feature.kind == Kind::Bug {
        out.push_str(match may_check {
            true => {
                "\n\nThis is a bug. Reproduce it first: write a test that fails because of it, \
                 or show it failing in a check, and only then fix it."
            }
            false => "\n\nThis is a bug. Find and show its cause before changing anything.",
        });
    }
    let recent = &progress[progress.len().saturating_sub(BRIEF_PROGRESS)..];
    if !recent.is_empty() {
        out.push_str("\n\nEarlier sessions:");
        for p in recent {
            out.push_str(&format!(
                "\n- `{}` ({}): {}",
                p.feature,
                serde_json::to_value(p.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                first_line(&p.summary)
            ));
            if !p.diffstat.is_empty() {
                out.push_str(&format!(" [{}]", last_line(&p.diffstat)));
            }
        }
    }
    out.push_str(
        "\n\nDo only this feature. End with a 3–5 line summary of what changed and how you \
         verified it.",
    );
    out
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

fn last_line(text: &str) -> &str {
    text.lines().last().unwrap_or("")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn features(value: Json) -> Result<Vec<Feature>> {
        parse_features(&json!({ "features": value }))
    }

    #[test]
    fn a_plan_is_validated() {
        let ok = features(json!([
            {"id": "tasks", "title": "Tasks page", "pages": ["/tasks"], "kind": "bug"},
            {"id": "filter", "title": "Filter"},
        ]))
        .unwrap();
        assert_eq!(ok[0].kind, Kind::Bug);
        assert_eq!(ok[1].status, Status::Todo);
        for (bad, says) in [
            (json!([]), "at least one"),
            (json!([{"id": "a b", "title": "x"}]), "non-empty word"),
            (json!([{"id": "a", "title": " "}]), "needs a title"),
            (
                json!([{"id": "a", "title": "x"}, {"id": "a", "title": "y"}]),
                "used twice",
            ),
            (
                json!([{"id": "a", "title": "x", "pages": ["tasks"]}]),
                "starting with `/`",
            ),
            (
                json!([{"id": "a", "title": "x", "kind": "chore"}]),
                "feature 1",
            ),
        ] {
            let err = features(bad).unwrap_err().to_string();
            assert!(err.contains(says), "{err}");
        }
    }

    #[test]
    fn replacing_keeps_what_the_harness_owns_for_ids_that_stay() {
        let mut plan = Plan::default();
        plan.replace(
            features(json!([{"id": "a", "title": "A"}, {"id": "b", "title": "B"}])).unwrap(),
        )
        .unwrap();
        {
            let a = plan.feature_mut("a").unwrap();
            a.status = Status::Done;
            a.runs = vec!["r1".to_owned()];
            a.attempts = 1;
        }
        plan.feature_mut("b").unwrap().status = Status::InProgress;

        // Dropping a feature whose session runs is refused.
        let err = plan
            .replace(features(json!([{"id": "a", "title": "A"}])).unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("in progress"), "{err}");

        // A reworded `a` keeps its progress; the model cannot un-finish it.
        plan.replace(
            features(json!([
                {"id": "a", "title": "A, reworded", "status": "todo"},
                {"id": "b", "title": "B"},
                {"id": "c", "title": "C", "status": "blocked"},
            ]))
            .unwrap(),
        )
        .unwrap();
        let a = &plan.features[0];
        assert_eq!(
            (a.title.as_str(), a.status, a.attempts, a.runs.len()),
            ("A, reworded", Status::Done, 1, 1)
        );
        assert_eq!(plan.features[1].status, Status::InProgress);
        assert_eq!(
            checklist(&plan),
            "<plan> 1 of 3 done\n[x] a: A, reworded\n[~] b: B (in progress)\n[-] c: C (blocked)\n</plan>"
        );
    }

    #[test]
    fn a_bugs_briefing_asks_for_a_reproduction_first() {
        let bug = Feature {
            id: "fix".to_owned(),
            title: "Totals are off by one".to_owned(),
            kind: Kind::Bug,
            acceptance: vec!["the total counts every row".to_owned()],
            ..Feature::default()
        };
        let progress = vec![Progress {
            feature: "tasks".to_owned(),
            run: "r1".to_owned(),
            status: Status::Done,
            summary: "Added the tasks page.\nChecked.".to_owned(),
            check: "check: green".to_owned(),
            diffstat: "A src/Tasks.tsx | +40 -0\n1 file changed".to_owned(),
        }];
        let brief = briefing(&bug, &progress, true);
        assert!(brief.starts_with("Implement feature `fix`"), "{brief}");
        assert!(brief.contains("- the total counts every row"), "{brief}");
        assert!(brief.contains("Reproduce it first"), "{brief}");
        assert!(
            brief.contains("- `tasks` (done): Added the tasks page. [1 file changed]"),
            "{brief}"
        );
    }
}
