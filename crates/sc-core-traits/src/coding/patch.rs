//! `apply_patch`: the V4A patch format, parsed and applied here (TODO 5.7).
//!
//! V4A is the patch format OpenAI's models are trained to write (Codex's
//! `apply_patch`). It names files by header and anchors each hunk by context
//! lines rather than line numbers:
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/app.ts
//! *** Move to: src/main.ts
//! @@ function total() {
//!  const items = load();
//! -return 0;
//! +return items.length;
//! *** Add File: src/new.ts
//! +export const x = 1;
//! *** Delete File: src/old.ts
//! *** End Patch
//! ```
//!
//! **The context lines are found by the same cascade `edit_file` uses**, so a
//! hunk quoted with the wrong indentation or a lost trailing space still
//! applies, and says which step it took. A hunk's context plus its removed lines
//! must match exactly one place after the previous hunk, and after its `@@`
//! anchor lines when it has any.
//!
//! **All or nothing.** Every file's new content is computed before anything is
//! written. A hunk that does not apply leaves every file untouched, and a write
//! that fails part-way restores what was already written.
//!
//! **The guards are `edit_file`'s.** A file updated, deleted or moved must have
//! been read in this run and be unchanged since. A file added over an existing
//! one is an overwrite, guarded the same way.
//!
//! **Only the function tool.** Some OpenAI models also have `apply_patch` as a
//! native tool type in the Responses API, but rig 0.41 cannot declare one or
//! parse its calls, so every backend is offered this function tool (TODO 5.8).

use std::collections::BTreeMap;

use bytes::Bytes;
use sc_agent::{Signal, TraitContext};
use sc_error::{Error, Result};
use sc_files::FileStore;
use sc_llm::ToolSpec;
use serde_json::{Value as Json, json};

use super::change::{current, delete_tracked, edited_region, numbered, write_tracked};
use super::matching::{self, Level, Search};
use super::read::as_text;
use super::state::{CodingState, stale_message};
use crate::files::{FileScope, open_to_write, string_arg};
use crate::table::arguments;

/// The patch text.
const ARG_PATCH: &str = "patch";

/// The most edited regions shown per file.
const MAX_REGIONS_SHOWN: usize = 3;

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("apply_patch_{}", scope.slug())
}

/// The tool this scope's patch contributes.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Edit files in {} with a V4A patch, applied all or nothing. Files to update, \
             move or delete must be read first.\n\
             *** Begin Patch\n\
             *** Update File: src/a.ts\n\
             @@ function foo() {{\n \
             context line\n\
             -old line\n\
             +new line\n\
             *** Add File: src/b.ts\n\
             +new file line\n\
             *** Delete File: src/c.ts\n\
             *** End Patch\n\
             Context lines start with a space. `*** Move to: path` after an Update line \
             renames the file.",
            scope.label()
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATCH: {"type": "string", "description": "The whole patch."},
            },
            "required": [ARG_PATCH],
            "additionalProperties": false,
        }),
    )
}

// --- parsing -------------------------------------------------------------------

/// A parsed patch.
#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    /// Its file operations, in order.
    pub ops: Vec<Op>,
}

/// One file operation.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Create a file with this content.
    Add {
        /// The path, as written.
        path: String,
        /// The content.
        content: String,
    },
    /// Delete a file.
    Delete {
        /// The path, as written.
        path: String,
    },
    /// Change a file, and perhaps move it.
    Update {
        /// The path, as written.
        path: String,
        /// Where to move it.
        move_to: Option<String>,
        /// The hunks, in file order.
        hunks: Vec<Hunk>,
    },
}

/// One hunk of an update.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hunk {
    /// `@@` lines to find, in order, before the hunk.
    pub anchors: Vec<String>,
    /// Context and removed lines: what is there now.
    pub old: Vec<String>,
    /// Context and added lines: what is to be there.
    pub new: Vec<String>,
    /// Marked `*** End of File`: an insertion goes at the end.
    pub end_of_file: bool,
}

impl Hunk {
    fn is_empty(&self) -> bool {
        self.old.is_empty() && self.new.is_empty()
    }
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF_MARK: &str = "*** End of File";

/// Parse a V4A patch. The error names the line and what was expected there.
pub fn parse(text: &str) -> std::result::Result<Patch, String> {
    let lines: Vec<&str> = text
        .trim()
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if lines.first().map(|l| l.trim()) != Some(BEGIN) {
        return Err(format!("a patch must start with `{BEGIN}`"));
    }
    let mut ops = Vec::new();
    let mut i = 1;
    let header = |l: &str| {
        l.starts_with(ADD) || l.starts_with(DELETE) || l.starts_with(UPDATE) || l.trim() == END
    };
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == END {
            break;
        }
        if let Some(path) = line.strip_prefix(ADD) {
            i += 1;
            let mut content = String::new();
            while i < lines.len() && !header(lines[i]) {
                match lines[i].strip_prefix('+') {
                    Some(text) => {
                        content.push_str(text);
                        content.push('\n');
                    }
                    None => {
                        return Err(format!(
                            "line {}: every line of an added file must start with `+`",
                            i + 1
                        ));
                    }
                }
                i += 1;
            }
            ops.push(Op::Add {
                path: path.trim().to_owned(),
                content,
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            ops.push(Op::Delete {
                path: path.trim().to_owned(),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let path = path.trim().to_owned();
            i += 1;
            let mut move_to = None;
            if let Some(to) = lines.get(i).and_then(|l| l.strip_prefix(MOVE)) {
                move_to = Some(to.trim().to_owned());
                i += 1;
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            let mut hunk = Hunk::default();
            while i < lines.len() && !header(lines[i]) {
                let l = lines[i];
                if let Some(anchor) = l.strip_prefix("@@") {
                    if !hunk.is_empty() {
                        hunks.push(std::mem::take(&mut hunk));
                    }
                    let anchor = anchor.trim();
                    if !anchor.is_empty() {
                        hunk.anchors.push(anchor.to_owned());
                    }
                } else if l.trim() == EOF_MARK {
                    hunk.end_of_file = true;
                } else if let Some(text) = l.strip_prefix(' ') {
                    hunk.old.push(text.to_owned());
                    hunk.new.push(text.to_owned());
                } else if let Some(text) = l.strip_prefix('-') {
                    hunk.old.push(text.to_owned());
                } else if let Some(text) = l.strip_prefix('+') {
                    hunk.new.push(text.to_owned());
                } else if l.is_empty() {
                    // A blank context line whose leading space was lost.
                    hunk.old.push(String::new());
                    hunk.new.push(String::new());
                } else {
                    return Err(format!(
                        "line {} of the update to `{path}`: `{l}` should start with ` ` \
                         (context), `-`, `+` or `@@`",
                        i + 1
                    ));
                }
                i += 1;
            }
            if !hunk.is_empty() {
                hunks.push(hunk);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("the update to `{path}` has no changes"));
            }
            ops.push(Op::Update {
                path,
                move_to,
                hunks,
            });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(format!(
                "line {}: expected `{ADD}`, `{UPDATE}`, `{DELETE}` or `{END}`, got `{line}`",
                i + 1
            ));
        }
    }
    if ops.is_empty() {
        return Err("the patch changes no files".to_owned());
    }
    Ok(Patch { ops })
}

// --- applying hunks ------------------------------------------------------------

/// The result of applying one file's hunks.
#[derive(Debug, Clone, PartialEq)]
pub struct Updated {
    /// The new content.
    pub text: String,
    /// The loosest cascade step any hunk needed.
    pub level: Option<Level>,
    /// Where each hunk's new lines are in `text`.
    pub ranges: Vec<std::ops::Range<usize>>,
}

/// Apply `hunks` to `text`, in order. The error says which hunk failed and
/// shows the closest lines.
pub fn apply_hunks(text: &str, hunks: &[Hunk], rel: &str) -> std::result::Result<Updated, String> {
    let mut text = text.to_owned();
    let mut cursor = 0;
    let mut level: Option<Level> = None;
    let mut ranges = Vec::new();
    let count = hunks.len();
    for (n, hunk) in hunks.iter().enumerate() {
        let which = match count {
            1 => format!("the hunk for `{rel}`"),
            _ => format!("hunk {} of {count} for `{rel}`", n + 1),
        };
        for anchor in &hunk.anchors {
            cursor = find_anchor(&text, cursor, anchor).ok_or_else(|| {
                format!("{which}: the `@@ {anchor}` line was not found after the previous hunk")
            })?;
        }
        let new = joined(&hunk.new);
        if hunk.old.is_empty() {
            // A pure insertion: after the anchors, or at the end.
            let at = match hunk.anchors.is_empty() || hunk.end_of_file {
                true => {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.len()
                }
                false => cursor,
            };
            text.insert_str(at, &new);
            ranges.push(at..at + new.len());
            cursor = at + new.len();
            continue;
        }
        let quote = joined(&hunk.old);
        match matching::find_lines_from(&text, cursor, &quote) {
            Search::Found(found) => {
                level = level.max(Some(found[0].level));
                let (edited, placed) = matching::replace(&text, &found, &new);
                let range = placed[0].clone();
                // The next hunk starts at a line start: after this one's lines,
                // or after the line it ended in.
                cursor = match edited[..range.end].ends_with('\n') || range.end == 0 {
                    true => range.end,
                    false => edited[range.end..]
                        .find('\n')
                        .map_or(edited.len(), |i| range.end + i + 1),
                };
                text = edited;
                ranges.push(range);
            }
            Search::Ambiguous { level, lines } => {
                return Err(format!(
                    "{which}: its context matches {} places ({}), starting at lines {:?}. Add \
                     more context lines or an `@@` line naming the enclosing function.",
                    lines.len(),
                    level.describe(),
                    lines
                ));
            }
            Search::Missing { closest } => {
                return Err(match closest {
                    Some(r) => format!(
                        "{which}: its context and removed lines were not found. The most \
                         similar lines ({}-{}) are:\n{}\nCopy the context exactly from these \
                         lines.",
                        r.first,
                        r.last,
                        numbered(&text, r.first, r.last)
                    ),
                    None => format!(
                        "{which}: its context and removed lines were not found. Read the file \
                         again and copy the context exactly."
                    ),
                });
            }
        }
    }
    Ok(Updated {
        text,
        level,
        ranges,
    })
}

/// Lines as text, each ending in a line break.
fn joined(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// The offset just after the first line at or after `from` that is `anchor`,
/// ignoring surrounding whitespace.
fn find_anchor(text: &str, from: usize, anchor: &str) -> Option<usize> {
    let anchor = anchor.trim();
    let mut at = from;
    while at < text.len() {
        let end = text[at..].find('\n').map_or(text.len(), |i| at + i + 1);
        if text[at..end].trim() == anchor {
            return Some(end);
        }
        at = end;
    }
    None
}

// --- the tool ------------------------------------------------------------------

/// One path's planned change.
struct Planned {
    /// The path relative to the scope.
    rel: String,
    /// What is there now.
    before: Option<Bytes>,
    /// What is to be there, or `None` to delete.
    after: Option<String>,
}

/// Apply one patch, as the run's caller.
pub async fn call(scope: &FileScope, args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let args = arguments(args, &[ARG_PATCH])?;
    let text = string_arg(&args, ARG_PATCH)?;
    let patch = parse(&text)
        .map_err(|e| Error::invalid(format!("status: failed. The patch is malformed: {e}")))?;

    let (store, _) = scope.connect(ctx.catalog).await?;
    let mut state = CodingState::load(ctx.state());
    let read_tool = super::read::tool_name(scope);

    // Plan every change against a virtual view of the files, so a later
    // operation sees an earlier one's result.
    let mut plan: BTreeMap<String, Planned> = BTreeMap::new();
    let mut lines: Vec<String> = Vec::new();
    let mut regions: Vec<String> = Vec::new();
    let mut moves: Vec<(String, String)> = Vec::new();
    // The loosest cascade step any hunk in the whole patch needed: a patch
    // counts as one edit, at the step that took the most licence (TODO §13).
    let mut loosest: Option<Level> = None;
    let refuse =
        |message: String| Error::invalid(format!("status: failed. No file was changed. {message}"));

    for op in &patch.ops {
        match op {
            Op::Add { path: rel, content } => {
                let (path, now) = plan_view(scope, ctx, store.as_ref(), &plan, rel).await?;
                if let Some(bytes) = &now {
                    guard(&state, &plan, &path, bytes, rel, &read_tool).map_err(refuse)?;
                }
                lines.push(format!("{} {rel}", if now.is_some() { 'M' } else { 'A' }));
                upsert(&mut plan, &path, rel, now, Some(content.clone()));
            }
            Op::Delete { path: rel } => {
                let (path, now) = plan_view(scope, ctx, store.as_ref(), &plan, rel).await?;
                let Some(bytes) = now else {
                    return Err(refuse(format!(
                        "`{rel}` cannot be deleted: it does not exist."
                    )));
                };
                guard(&state, &plan, &path, &bytes, rel, &read_tool).map_err(refuse)?;
                lines.push(format!("D {rel}"));
                upsert(&mut plan, &path, rel, Some(bytes), None);
            }
            Op::Update {
                path: rel,
                move_to,
                hunks,
            } => {
                let (path, now) = plan_view(scope, ctx, store.as_ref(), &plan, rel).await?;
                let Some(bytes) = now else {
                    return Err(refuse(format!(
                        "`{rel}` cannot be updated: it does not exist. Use `{ADD}` to create it."
                    )));
                };
                guard(&state, &plan, &path, &bytes, rel, &read_tool).map_err(refuse)?;
                let text = as_text(&bytes).ok_or_else(|| {
                    refuse(format!("`{rel}` is a binary file and cannot be patched."))
                })?;
                let updated = match apply_hunks(text, hunks, rel) {
                    Ok(updated) => updated,
                    Err(message) => {
                        ctx.signal(Signal::EditFailed);
                        state.edit_failed();
                        state.store(ctx.state());
                        return Err(refuse(message));
                    }
                };
                loosest = loosest.max(updated.level);
                let how = updated
                    .level
                    .map(|l| format!(" ({})", l.short()))
                    .unwrap_or_default();
                let shown_rel = move_to.as_deref().unwrap_or(rel);
                for range in updated.ranges.iter().take(MAX_REGIONS_SHOWN) {
                    regions.push(format!(
                        "{shown_rel}:\n{}",
                        edited_region(&updated.text, range)
                    ));
                }
                match move_to {
                    None => {
                        lines.push(format!("M {rel}{how}"));
                        upsert(&mut plan, &path, rel, Some(bytes), Some(updated.text));
                    }
                    Some(to_rel) => {
                        let (to, existing) =
                            plan_view(scope, ctx, store.as_ref(), &plan, to_rel).await?;
                        if existing.is_some() {
                            return Err(refuse(format!(
                                "`{rel}` cannot be moved to `{to_rel}`: that file already exists."
                            )));
                        }
                        lines.push(format!("R {rel} → {to_rel}{how}"));
                        upsert(&mut plan, &path, rel, Some(bytes), None);
                        upsert(&mut plan, &to, to_rel, None, Some(updated.text));
                        moves.push((path.clone(), to));
                    }
                }
            }
        }
    }

    // Write everything, restoring what was written if a write fails.
    let mut done: Vec<&str> = Vec::new();
    for (path, planned) in &plan {
        if planned.before.is_none() && planned.after.is_none() {
            continue;
        }
        let result = match &planned.after {
            Some(text) => {
                write_tracked(
                    store.as_ref(),
                    &mut state,
                    path,
                    planned.before.as_deref(),
                    text.clone().into_bytes(),
                )
                .await
            }
            None => match &planned.before {
                Some(before) => delete_tracked(store.as_ref(), &mut state, path, before).await,
                None => Ok(()),
            },
        };
        if let Err(e) = result {
            for written in done {
                let planned = &plan[written];
                let _ = match &planned.before {
                    Some(before) => store.write(written, before.clone()).await,
                    None => store.delete(written).await.map(|_| ()),
                };
            }
            return Err(Error::invalid(format!(
                "status: failed. Writing `{}` failed ({e}); the files already written were \
                 restored.",
                planned.rel
            )));
        }
        done.push(path);
    }
    for (from, to) in moves {
        state.ledger.moved(&from, &to);
    }
    state.edit_applied(loosest);
    state.store(ctx.state());

    let mut out = format!("status: applied\n{}", lines.join("\n"));
    if !regions.is_empty() {
        out.push_str("\n\n");
        out.push_str(&regions.join("\n\n"));
    }
    Ok(Json::String(out))
}

/// A path's store path and its content in the plan so far: the planned content
/// if an earlier operation changed it, else what the store holds.
async fn plan_view(
    scope: &FileScope,
    ctx: &TraitContext<'_>,
    store: &dyn FileStore,
    plan: &BTreeMap<String, Planned>,
    rel: &str,
) -> Result<(String, Option<Bytes>)> {
    let (_, path) = open_to_write(scope, ctx, rel).await?;
    if let Some(planned) = plan.get(&path) {
        return Ok((path, planned.after.clone().map(Bytes::from)));
    }
    let now = current(store, &path, rel).await?;
    Ok((path, now))
}

/// Record a planned change, keeping the store's `before` from the first time
/// the plan saw the path.
fn upsert(
    plan: &mut BTreeMap<String, Planned>,
    path: &str,
    rel: &str,
    before: Option<Bytes>,
    after: Option<String>,
) {
    match plan.get_mut(path) {
        Some(planned) => planned.after = after,
        None => {
            plan.insert(
                path.to_owned(),
                Planned {
                    rel: rel.to_owned(),
                    before,
                    after,
                },
            );
        }
    }
}

/// The read-before-change guard, for a path the plan has not changed yet. A
/// path an earlier operation of this patch produced is the patch's own.
fn guard(
    state: &CodingState,
    plan: &BTreeMap<String, Planned>,
    path: &str,
    bytes: &[u8],
    rel: &str,
    read_tool: &str,
) -> std::result::Result<(), String> {
    if plan.contains_key(path) {
        return Ok(());
    }
    state
        .check_current(path, bytes)
        .map_err(|stale| stale_message(stale, rel, read_tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(anchors: &[&str], old: &[&str], new: &[&str]) -> Hunk {
        Hunk {
            anchors: anchors.iter().map(|s| (*s).to_owned()).collect(),
            old: old.iter().map(|s| (*s).to_owned()).collect(),
            new: new.iter().map(|s| (*s).to_owned()).collect(),
            end_of_file: false,
        }
    }

    #[test]
    fn a_tools_name_is_derived_from_its_scope() {
        let scope = FileScope {
            store: "src".to_owned(),
            root: "web".to_owned(),
        };
        assert_eq!(tool_name(&scope), "apply_patch_src_web");
    }

    /// The shape of the examples in Codex's `apply_patch` instructions: an add,
    /// a delete, and an update with an anchor, a move and two hunks.
    #[test]
    fn a_patch_parses_into_its_operations() {
        let patch = parse(
            "*** Begin Patch\n\
             *** Add File: hello.txt\n\
             +Hello world\n\
             *** Update File: src/app.py\n\
             *** Move to: src/main.py\n\
             @@ def greet():\n\
             -print(\"Hi\")\n\
             +print(\"Hello, world!\")\n\
             @@\n \
             x = 1\n\
             -y = 2\n\
             *** Delete File: obsolete.txt\n\
             *** End Patch",
        )
        .unwrap();
        assert_eq!(
            patch.ops,
            vec![
                Op::Add {
                    path: "hello.txt".to_owned(),
                    content: "Hello world\n".to_owned()
                },
                Op::Update {
                    path: "src/app.py".to_owned(),
                    move_to: Some("src/main.py".to_owned()),
                    hunks: vec![
                        hunk(
                            &["def greet():"],
                            &["print(\"Hi\")"],
                            &["print(\"Hello, world!\")"]
                        ),
                        hunk(&[], &["x = 1", "y = 2"], &["x = 1"]),
                    ],
                },
                Op::Delete {
                    path: "obsolete.txt".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_malformed_patch_is_refused_with_the_line() {
        assert!(
            parse("*** Update File: a")
                .unwrap_err()
                .contains("Begin Patch")
        );
        let err = parse("*** Begin Patch\n*** Add File: a\nno plus\n*** End Patch").unwrap_err();
        assert!(err.contains("line 3"), "{err}");
        let err = parse("*** Begin Patch\n*** Update File: a\n?what\n*** End Patch").unwrap_err();
        assert!(err.contains("should start with"), "{err}");
        assert!(parse("*** Begin Patch\n*** End Patch").is_err());
    }

    #[test]
    fn hunks_apply_in_order_after_their_anchors() {
        let file = "def a():\n    return 1\n\ndef b():\n    return 1\n";
        let updated = apply_hunks(
            file,
            &[hunk(&["def b():"], &["    return 1"], &["    return 2"])],
            "m.py",
        )
        .unwrap();
        // Without the anchor, `return 1` would be ambiguous.
        assert_eq!(
            updated.text,
            "def a():\n    return 1\n\ndef b():\n    return 2\n"
        );
        assert_eq!(updated.level, Some(Level::Exact));

        let err = apply_hunks(
            file,
            &[hunk(&[], &["    return 1"], &["    return 2"])],
            "m.py",
        )
        .unwrap_err();
        assert!(err.contains("matches 2 places"), "{err}");
    }

    #[test]
    fn a_hunk_uses_the_cascade_and_says_so() {
        let file = "if (a) {\n    b();\n    c();\n}\n";
        // Quoted without indentation.
        let updated = apply_hunks(
            file,
            &[hunk(&[], &["b();", "c();"], &["b();", "d();"])],
            "a.js",
        )
        .unwrap();
        assert_eq!(updated.text, "if (a) {\n    b();\n    d();\n}\n");
        assert_eq!(updated.level, Some(Level::Indentation));
    }

    #[test]
    fn an_insertion_goes_after_its_anchor_or_at_the_end() {
        let file = "a\nb\nc";
        let updated = apply_hunks(file, &[hunk(&["a"], &[], &["inserted"])], "f").unwrap();
        assert_eq!(updated.text, "a\ninserted\nb\nc");
        let updated = apply_hunks(file, &[hunk(&[], &[], &["last"])], "f").unwrap();
        assert_eq!(updated.text, "a\nb\nc\nlast\n");
    }

    #[test]
    fn a_hunk_that_does_not_apply_shows_the_closest_lines() {
        let file = "one\ntwo\nthree\n";
        let err = apply_hunks(
            file,
            &[
                hunk(&[], &["one"], &["1"]),
                hunk(&[], &["tree", "four"], &["3"]),
            ],
            "n.txt",
        )
        .unwrap_err();
        assert!(err.starts_with("hunk 2 of 2 for `n.txt`"), "{err}");
        assert!(err.contains("not found"), "{err}");
    }
}
