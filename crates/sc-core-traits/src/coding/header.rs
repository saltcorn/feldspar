//! What opens every `coding` session (TODO 8.2, §9): the project's own notes,
//! a map of the code focused on the request, and what was done lately.
//!
//! Built **once per session** and stored with the run, so it caches behind the
//! stable prefix. In order:
//!
//! 1. **`AGENTS.md`**: the scope root's, then the nearest one above each file
//!    the brief names, when that is a different one (R§4's nearest-file-wins,
//!    with the root kept because it holds the project-wide facts). Each is capped
//!    at [`MAX_AGENTS_CHARS`].
//! 2. **The repo map** at `repo_map_tokens`, focused on what the brief mentions
//!    ([`super::repo_map::header`]).
//! 3. **The application's static directories**, when the `application` setting
//!    names one: a line per mount, with the store and how many files are under
//!    it ([`super::assets`]). A model that does not know `list_assets` exists
//!    will not call it, and finding out costs a turn and the admin's money.
//! 4. **The recent git log**, when the scope is inside a git work tree that
//!    belongs to the store: the last [`GIT_LOG_ENTRIES`] commits touching the
//!    scope. For a new chat this is the history a plan does not carry (§8).
//!
//! **The brief itself is not repeated.** It is the run's first user message,
//! which follows the header directly: for a chat that is the request, and for a
//! feature's child run it is the briefing `implement_feature` writes, with the
//! feature and the last progress entries (§8, step 2). The header uses it only
//! to decide what to focus on.
//!
//! Nothing here fails the run. A tree that cannot be walked, an unreadable
//! `AGENTS.md` or a git that is not installed each leave their section out, and
//! are logged.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use sc_agent::SessionContext;
use sc_files::check_access;
use sc_types::Attrs;

use super::assets;
use super::repo_map::{self, focus_of, source_files, terms_of};
use crate::files::FileScope;

/// The file of notes for agents.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// The most characters of one `AGENTS.md` the header carries.
pub const MAX_AGENTS_CHARS: usize = 8_000;

/// How many commits the header lists.
pub const GIT_LOG_ENTRIES: usize = 10;

/// How long `git log` may take before the header goes without it.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The header for one session, or `None` when there is nothing to say.
pub async fn header(
    scope: &FileScope,
    config: &Attrs,
    cx: &mut SessionContext<'_>,
) -> Option<String> {
    let role = cx.caller.role;
    let (files, truncated) = match source_files(scope, cx.catalog, role).await {
        Ok(found) => found,
        Err(e) => {
            sc_log::log_warn!("session header for {}: {e}", scope.label());
            (Vec::new(), false)
        }
    };

    let mut parts: Vec<String> = Vec::new();
    for path in agents_files(&files, cx.brief) {
        if let Some(text) = read_notes(scope, cx, &path).await {
            parts.push(text);
        }
    }
    if !files.is_empty()
        && let Some(map) = repo_map::header(scope, config, &files, truncated, cx.brief)
    {
        parts.push(map);
    }
    if let Some(mounts) = assets::header(config, cx.catalog, role).await {
        parts.push(mounts);
    }
    if let Some(log) = git_log(scope, cx).await {
        parts.push(log);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Which `AGENTS.md` files to include, relative to the scope: the root's, then
/// for each file the brief names, the nearest one in a directory above it, each
/// once. The root's is included even when the walk found nothing, so a tree
/// that could not be walked still gets its notes.
pub fn agents_files(files: &[sc_repomap::SourceFile], brief: &str) -> Vec<String> {
    let known: BTreeSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut out = vec![AGENTS_FILE.to_owned()];
    let focus = focus_of(files, terms_of(brief));
    for file in &focus.files {
        let mut dir = Path::new(file.as_str()).parent();
        while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            let candidate = format!("{}/{AGENTS_FILE}", d.to_string_lossy());
            if known.contains(candidate.as_str()) {
                if !out.contains(&candidate) {
                    out.push(candidate);
                }
                break;
            }
            dir = d.parent();
        }
    }
    out
}

/// One `AGENTS.md`, as the caller may read it, framed with its path.
async fn read_notes(scope: &FileScope, cx: &SessionContext<'_>, rel: &str) -> Option<String> {
    let (store, floor) = scope.connect(cx.catalog).await.ok()?;
    let path = scope.resolve(rel).ok()?;
    if !store.stat(&path).await.ok()?.is_some_and(|s| !s.is_dir) {
        return None;
    }
    if let Err(e) = check_access(store.as_ref(), floor, &path, cx.caller.role).await {
        sc_log::log_verbose!("session header: `{path}` is not readable here: {e}");
        return None;
    }
    let bytes = match store.read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            sc_log::log_warn!("session header: reading `{path}`: {e}");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(format!(
        "<project path=\"{rel}\">\n{}\n</project>",
        clip(text, MAX_AGENTS_CHARS)
    ))
}

/// `text` cut to at most `max` characters, at a line end where there is one,
/// saying that it was cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    let cut = match cut.rfind('\n') {
        Some(end) if end > max / 2 => &cut[..end],
        _ => cut.as_str(),
    };
    format!("{cut}\n[… cut at {max} characters; read the file for the rest]")
}

/// The recent commits touching the scope, when it is in a git work tree that
/// lies within the store's own directory. A local store that merely sits inside
/// some other repository (a checkout of the server, say) gets none: that
/// history is not the application's.
async fn git_log(scope: &FileScope, cx: &SessionContext<'_>) -> Option<String> {
    let (store, _) = scope.connect(cx.catalog).await.ok()?;
    let store_dir = store.local_path("").ok()??;
    let dir = store.local_path(&scope.resolve("").ok()?).ok()??;
    if !dir.is_dir() {
        return None;
    }
    let top = git(&dir, &["rev-parse", "--show-toplevel"]).await?;
    let top = Path::new(top.trim()).canonicalize().ok()?;
    if !top.starts_with(store_dir.canonicalize().ok()?) {
        return None;
    }
    let count = format!("-{GIT_LOG_ENTRIES}");
    let log = git(
        &dir,
        &[
            "log",
            &count,
            "--no-merges",
            "--date=short",
            "--pretty=format:%h %ad %s",
            "--",
            ".",
        ],
    )
    .await?;
    let log = log.trim();
    (!log.is_empty()).then(|| format!("<git_log>\n{log}\n</git_log>"))
}

/// A git command's stdout in `dir`, when it succeeds in time.
async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let run = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(GIT_TIMEOUT, run).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_repomap::SourceFile;

    fn files(paths: &[&str]) -> Vec<SourceFile> {
        paths
            .iter()
            .map(|p| SourceFile {
                path: (*p).to_owned(),
                tags: Default::default(),
            })
            .collect()
    }

    #[test]
    fn the_nearest_agents_file_above_each_named_file_joins_the_roots() {
        let tree = files(&[
            "AGENTS.md",
            "src/App.tsx",
            "server/AGENTS.md",
            "server/api/routes.ts",
            "server/api/db.ts",
        ]);
        assert_eq!(
            agents_files(
                &tree,
                "Fix server/api/routes.ts and server/api/db.ts, then src/App.tsx"
            ),
            ["AGENTS.md", "server/AGENTS.md"]
        );
        // Nothing named, or nothing nested: the root's alone.
        assert_eq!(agents_files(&tree, "Add dark mode"), ["AGENTS.md"]);
        assert_eq!(agents_files(&[], "server/api/db.ts"), ["AGENTS.md"]);
    }

    #[test]
    fn a_long_file_is_cut_at_a_line_and_says_so() {
        let text = "line\n".repeat(100);
        let cut = clip(&text, 52);
        assert!(cut.starts_with("line\nline\n"), "{cut}");
        assert!(cut.ends_with("read the file for the rest]"), "{cut}");
        assert_eq!(clip("short", 52), "short");
    }
}
