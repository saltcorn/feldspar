//! Searching a store's text files, server-side (TODO Phase 5, §11.3).
//!
//! Find-in-files over a store used to be the client's job: the IDE walked the
//! tree through the filesystem provider, one HTTP request per directory, and an
//! agent could not do it at all. This module is the walk done **where the bytes
//! are** — one call in, the matches out — and it is the same code both callers
//! use, so what the IDE's search box finds is what the model's `search_files`
//! tool finds.
//!
//! Three things it is deliberately careful about:
//!
//! - **Access is the same rule as everywhere else.** Every directory listing goes
//!   through [`filter_visible`](crate::filter_visible), so a search cannot report
//!   a line from a file the caller could not have opened — which would leak the
//!   contents of exactly the tree an admin restricted, one match at a time.
//! - **It is bounded, in three directions at once.** Results
//!   ([`SearchQuery::max_results`]), file size ([`MAX_FILE_BYTES`]) and files
//!   visited ([`MAX_FILES_SCANNED`]) each have a ceiling, and the outcome says
//!   when one was hit. A search over a store that happens to hold a
//!   `node_modules` must come back, and it must not come back lying about having
//!   looked everywhere.
//! - **Binary files are skipped, not mangled.** A file whose bytes are not UTF-8
//!   has no lines to report, and "matched at line 3" of a PNG is noise in a
//!   result list a person is reading.

use crate::access::filter_visible;
use crate::store::{Entry, FileStore};
use sc_error::{Error, Result};

/// Directory names skipped unless the caller asks for them by searching inside
/// one.
///
/// Editorial, and stated here rather than at each call site so the IDE and the
/// agent skip the same things. Every entry is either not source (`node_modules`,
/// `target`) or generated from source (`dist`, `build`), and a match in one is
/// almost always a copy of a match the search already reported — with the
/// difference that there are ten thousand of them. A search *rooted* at
/// `dist` still searches it: the exclusion is on descent, not on the root the
/// caller named.
pub const DEFAULT_EXCLUDED_DIRS: [&str; 6] =
    [".git", "node_modules", "dist", "build", "target", ".venv"];

/// The default ceiling on matches returned.
pub const DEFAULT_MAX_RESULTS: usize = 100;

/// The largest file that is read at all. Above this it is treated as not text:
/// a minified bundle or a checked-in archive costs more to scan than any match
/// in it is worth.
pub const MAX_FILE_BYTES: u64 = 2_000_000;

/// How many files one search may open before it stops and says so.
pub const MAX_FILES_SCANNED: usize = 20_000;

/// The longest line reported in a hit. A minified line is one match and 200kB of
/// context nobody can read.
pub const MAX_LINE_CHARS: usize = 500;

/// What to look for, where, and how much of it to bring back.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// The literal text, or the regular expression when
    /// [`regex`](SearchQuery::regex) is set.
    pub pattern: String,
    /// Whether `pattern` is a regular expression rather than literal text.
    pub regex: bool,
    /// Whether case matters. `false` is the editor's default and the useful one.
    pub case_sensitive: bool,
    /// Whether the match must be a whole word (`\b…\b`).
    pub whole_word: bool,
    /// A file glob narrowing which files are searched (`*.ts`, `src/**/*.tsx`).
    /// A pattern with no `/` is matched against the file's **name**; one with a
    /// `/` against its whole store-relative path.
    pub glob: Option<String>,
    /// The sub-tree to search; `""` is the whole store.
    pub dir: String,
    /// At most this many matches.
    pub max_results: usize,
    /// Directory names not descended into. Defaults to [`DEFAULT_EXCLUDED_DIRS`].
    pub exclude_dirs: Vec<String>,
}

impl SearchQuery {
    /// A case-insensitive literal search of the whole store.
    pub fn literal(pattern: impl Into<String>) -> SearchQuery {
        SearchQuery {
            pattern: pattern.into(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            glob: None,
            dir: String::new(),
            max_results: DEFAULT_MAX_RESULTS,
            exclude_dirs: DEFAULT_EXCLUDED_DIRS
                .iter()
                .map(|d| (*d).to_owned())
                .collect(),
        }
    }

    /// The same, as a regular expression.
    pub fn regex(pattern: impl Into<String>) -> SearchQuery {
        SearchQuery {
            regex: true,
            ..SearchQuery::literal(pattern)
        }
    }
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The file's store-relative path.
    pub path: String,
    /// The line the match is on, counting from 1 — what an editor and a person
    /// both mean by a line number.
    pub line: u32,
    /// Where in that line the match starts, in **characters**, counting from 1.
    pub column: u32,
    /// How many characters the match covers.
    pub length: u32,
    /// The whole line, truncated at [`MAX_LINE_CHARS`].
    pub text: String,
}

/// What a search found, and whether it stopped early.
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    /// The matches, in the order the walk found them (directory order, depth
    /// first).
    pub hits: Vec<SearchHit>,
    /// How many files were opened and scanned.
    pub files_scanned: usize,
    /// Whether a ceiling stopped the search before the tree was exhausted. A
    /// caller that reports results without reporting this is telling the reader
    /// there is nothing else, which may be false.
    pub truncated: bool,
}

/// Search `store` under the caller's access rules.
///
/// `store_min_role` is the store definition's own floor and `caller` the caller,
/// exactly as [`check_access`](crate::check_access) takes them: this is the file
/// manager's rule applied to a walk instead of to a single path.
pub async fn search_store(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    caller: &crate::FileCaller,
    query: &SearchQuery,
) -> Result<SearchOutcome> {
    let matcher = Matcher::new(query)?;
    let max_results = query.max_results.max(1);

    let mut outcome = SearchOutcome::default();
    // Depth-first, so results arrive in an order that reads like the tree. The
    // stack holds `(path, floor)` — the floor already accumulated for that
    // directory, so a child's rule is one `get_meta`, not one per ancestor per
    // file.
    let root_floor = crate::effective_min_role(store, store_min_role, &query.dir).await?;
    let mut stack = vec![(query.dir.clone(), root_floor)];

    while let Some((dir, floor)) = stack.pop() {
        let entries = store.list(&dir).await?;
        let visible = filter_visible(store, floor, entries, caller).await?;
        // Reversed onto the stack so the pops come back in listing order.
        let mut directories = Vec::new();
        for entry in visible {
            if entry.is_dir {
                if query.exclude_dirs.iter().any(|d| d == &entry.name) {
                    continue;
                }
                let child_floor = crate::effective_min_role(store, floor, &entry.path).await?;
                directories.push((entry.path, child_floor));
                continue;
            }
            if !file_selected(&entry, query) {
                continue;
            }
            if outcome.files_scanned >= MAX_FILES_SCANNED {
                outcome.truncated = true;
                return Ok(outcome);
            }
            outcome.files_scanned += 1;
            let bytes = store.read(&entry.path).await?;
            // Not text: nothing here has lines, and pretending otherwise fills
            // a result list with mojibake.
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if matcher.scan(&entry.path, text, max_results, &mut outcome.hits) {
                outcome.truncated = true;
                return Ok(outcome);
            }
        }
        directories.reverse();
        stack.extend(directories);
    }
    Ok(outcome)
}

/// Whether this file is in scope: small enough to read, and matching the glob.
fn file_selected(entry: &Entry, query: &SearchQuery) -> bool {
    if entry.size.is_some_and(|size| size > MAX_FILE_BYTES) {
        return false;
    }
    match &query.glob {
        None => true,
        Some(glob) => glob_matches(glob, &entry.path, &entry.name),
    }
}

/// The compiled pattern, and the scan of one file with it.
#[derive(Debug)]
struct Matcher {
    regex: regex_lite::Regex,
}

impl Matcher {
    /// Compile the query's pattern.
    ///
    /// A literal is escaped rather than fed to the engine: a person searching
    /// for `foo(bar)` means those characters, and an engine error about an
    /// unclosed group would be a baffling answer to a search box.
    fn new(query: &SearchQuery) -> Result<Matcher> {
        if query.pattern.is_empty() {
            return Err(Error::invalid("a search needs something to search for"));
        }
        let mut source = match query.regex {
            true => query.pattern.clone(),
            false => regex_lite::escape(&query.pattern),
        };
        if query.whole_word {
            source = format!(r"\b(?:{source})\b");
        }
        let regex = regex_lite::RegexBuilder::new(&source)
            .case_insensitive(!query.case_sensitive)
            .build()
            .map_err(|e| {
                Error::invalid(format!("`{}` is not a valid search: {e}", query.pattern))
            })?;
        Ok(Matcher { regex })
    }

    /// Append every match in `text` to `hits`, returning whether the ceiling was
    /// reached.
    fn scan(&self, path: &str, text: &str, max_results: usize, hits: &mut Vec<SearchHit>) -> bool {
        for (index, line) in text.lines().enumerate() {
            for found in self.regex.find_iter(line) {
                if hits.len() >= max_results {
                    return true;
                }
                // Characters, not bytes: the column is for a person and an
                // editor, both of which count characters, and a byte offset in a
                // line with an accent in it points at the wrong place.
                let column = line[..found.start()].chars().count() + 1;
                let length = found.as_str().chars().count();
                hits.push(SearchHit {
                    path: path.to_owned(),
                    line: index as u32 + 1,
                    column: column as u32,
                    length: length as u32,
                    text: truncate_chars(line, MAX_LINE_CHARS),
                });
            }
        }
        false
    }
}

/// The first `max` characters of `line`.
fn truncate_chars(line: &str, max: usize) -> String {
    match line.char_indices().nth(max) {
        None => line.to_owned(),
        Some((end, _)) => line[..end].to_owned(),
    }
}

// --- globs -------------------------------------------------------------------

/// Whether `glob` selects a file at `path` (named `name`).
///
/// A pattern with no `/` is matched against the **name**, so `*.ts` finds every
/// TypeScript file in the tree — which is what a person typing it into a search
/// box means. One with a `/` is matched against the whole store-relative path,
/// where `**` spans directories and `*` does not.
///
/// `{a,b}` alternatives are expanded first (`*.{ts,tsx}`), because that is how
/// a model writes "either extension".
pub fn glob_matches(glob: &str, path: &str, name: &str) -> bool {
    let glob = glob.trim();
    if glob.is_empty() {
        return true;
    }
    expand_braces(glob)
        .iter()
        .any(|glob| match glob.contains('/') {
            true => glob_match_path(glob, path),
            false => glob_match_segment(glob, name),
        })
}

/// Every alternative a glob's `{a,b}` groups spell, innermost choices first.
/// A `{` with no matching `}` is literal.
fn expand_braces(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_owned()];
    };
    let Some(close) = glob[open..].find('}').map(|i| open + i) else {
        return vec![glob.to_owned()];
    };
    let (head, body, tail) = (&glob[..open], &glob[open + 1..close], &glob[close + 1..]);
    body.split(',')
        .flat_map(|choice| expand_braces(&format!("{head}{choice}{tail}")))
        .collect()
}

/// Match a whole path against a glob whose segments may include `**`.
fn glob_match_path(glob: &str, path: &str) -> bool {
    let pattern: Vec<&str> = glob.split('/').filter(|s| !s.is_empty()).collect();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pattern, &segments)
}

/// `**` spans any number of segments, so this is the same backtracking shape as
/// the character matcher one level up.
fn match_segments(pattern: &[&str], segments: &[&str]) -> bool {
    match pattern.first() {
        None => segments.is_empty(),
        Some(&"**") => {
            // Zero segments, or one and try again — which covers `a/**/b` where
            // `**` matches nothing as well as where it matches three levels.
            (0..=segments.len()).any(|skip| match_segments(&pattern[1..], &segments[skip..]))
        }
        Some(first) => match segments.first() {
            Some(segment) if glob_match_segment(first, segment) => {
                match_segments(&pattern[1..], &segments[1..])
            }
            _ => false,
        },
    }
}

/// Match one path segment against one glob segment: `*` any run of characters
/// (not `/`, which cannot occur in a segment), `?` exactly one.
fn glob_match_segment(pattern: &str, segment: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let segment: Vec<char> = segment.chars().collect();
    match_chars(&pattern, &segment)
}

fn match_chars(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|skip| match_chars(&pattern[1..], &text[skip..])),
        Some('?') => !text.is_empty() && match_chars(&pattern[1..], &text[1..]),
        Some(c) => text.first() == Some(c) && match_chars(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glob_without_a_slash_matches_the_name_anywhere_in_the_tree() {
        assert!(glob_matches("*.ts", "src/deep/app.ts", "app.ts"));
        assert!(!glob_matches("*.ts", "src/deep/app.tsx", "app.tsx"));
        assert!(glob_matches("app.?s", "src/app.js", "app.js"));
    }

    #[test]
    fn a_glob_with_a_slash_matches_the_whole_path() {
        assert!(glob_matches("src/*.ts", "src/app.ts", "app.ts"));
        // `{a,b}` is either.
        assert!(glob_matches("*.{ts,tsx}", "src/app.tsx", "app.tsx"));
        assert!(glob_matches("src/{lib,app}/*.ts", "src/lib/a.ts", "a.ts"));
        assert!(!glob_matches("*.{ts,tsx}", "src/app.js", "app.js"));
        // `*` does not span a directory separator; `**` does.
        assert!(!glob_matches("src/*.ts", "src/deep/app.ts", "app.ts"));
        assert!(glob_matches("src/**/*.ts", "src/deep/app.ts", "app.ts"));
        // `**` also matches no segments at all.
        assert!(glob_matches("src/**/*.ts", "src/app.ts", "app.ts"));
        assert!(!glob_matches("web/**", "src/app.ts", "app.ts"));
    }

    #[test]
    fn an_empty_glob_selects_everything() {
        assert!(glob_matches("", "src/app.ts", "app.ts"));
        assert!(glob_matches("   ", "src/app.ts", "app.ts"));
    }

    #[test]
    fn a_literal_search_is_escaped_and_a_regex_is_not() {
        let mut query = SearchQuery::literal("foo(bar)");
        let matcher = Matcher::new(&query).unwrap();
        let mut hits = Vec::new();
        assert!(!matcher.scan("a.ts", "call foo(bar) here", 10, &mut hits));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, 6);
        assert_eq!(hits[0].length, 8);

        // The same text as a regular expression is `foo` followed by a group, so
        // it matches `foobar` and *not* the parenthesised text a literal found.
        query.regex = true;
        let matcher = Matcher::new(&query).unwrap();
        let mut hits = Vec::new();
        matcher.scan("a.ts", "call foo(bar) here", 10, &mut hits);
        assert!(hits.is_empty());
        matcher.scan("a.ts", "call foobar here", 10, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].length, 6);
    }

    #[test]
    fn case_and_whole_word_are_honoured() {
        let mut query = SearchQuery::literal("todo");
        // Case-insensitive by default.
        let mut hits = Vec::new();
        Matcher::new(&query)
            .unwrap()
            .scan("a.ts", "TODO: the todo list", 10, &mut hits);
        assert_eq!(hits.len(), 2);

        query.case_sensitive = true;
        let mut hits = Vec::new();
        Matcher::new(&query)
            .unwrap()
            .scan("a.ts", "TODO: the todo list", 10, &mut hits);
        assert_eq!(hits.len(), 1);

        query.whole_word = true;
        let mut hits = Vec::new();
        Matcher::new(&query)
            .unwrap()
            .scan("a.ts", "todos and todo", 10, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, 11);
    }

    #[test]
    fn a_column_counts_characters_rather_than_bytes() {
        let query = SearchQuery::literal("beta");
        let mut hits = Vec::new();
        Matcher::new(&query)
            .unwrap()
            .scan("a.ts", "αβγ beta", 10, &mut hits);
        assert_eq!(hits[0].column, 5);
    }

    #[test]
    fn an_invalid_regex_is_refused_with_the_engines_reason() {
        let err = Matcher::new(&SearchQuery::regex("foo(")).unwrap_err();
        assert!(err.to_string().contains("foo("), "{err}");
        // And an empty pattern is not a search at all.
        assert!(Matcher::new(&SearchQuery::literal("")).is_err());
    }

    #[test]
    fn a_long_line_is_truncated_but_still_reported() {
        let line = format!("{}needle", "x".repeat(MAX_LINE_CHARS));
        let mut hits = Vec::new();
        Matcher::new(&SearchQuery::literal("needle"))
            .unwrap()
            .scan("a.ts", &line, 10, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text.chars().count(), MAX_LINE_CHARS);
        // The column still points at the real position in the line.
        assert_eq!(hits[0].column as usize, MAX_LINE_CHARS + 1);
    }

    #[test]
    fn the_result_ceiling_stops_the_scan_and_says_so() {
        let text = "hit\nhit\nhit\nhit\n";
        let mut hits = Vec::new();
        let stopped = Matcher::new(&SearchQuery::literal("hit"))
            .unwrap()
            .scan("a.ts", text, 2, &mut hits);
        assert!(stopped);
        assert_eq!(hits.len(), 2);
    }
}

// --- finding files by name ---------------------------------------------------

/// The default ceiling on entries a name search returns.
pub const DEFAULT_MAX_FOUND: usize = 500;

/// What a name search found, and whether a ceiling cut it short.
///
/// The entries are whole [`Entry`] values rather than paths, because the caller
/// showing them is a file listing: a result the file manager cannot show a size
/// or a modification time for is a row with three empty columns, and one more
/// `stat` per hit to fill them in.
#[derive(Debug, Clone, Default)]
pub struct FoundFiles {
    /// The matching entries, in the order the walk found them (directory order,
    /// depth first). Directories match too — a folder is a thing a person looks
    /// for by name.
    pub entries: Vec<Entry>,
    /// Whether a ceiling stopped the walk before the tree was exhausted.
    pub truncated: bool,
}

/// Find entries under `dir` whose **name** contains `query`, case-insensitively.
///
/// This is the file manager's search box, and it is deliberately not
/// [`search_store`]: that one reads every text file to find a *line*, which is
/// the wrong instrument (and the wrong cost) for "where did I put
/// `invoice-2024.pdf`". Nothing is read here — the walk needs names, and names
/// are in the listing.
///
/// Access is the same rule as a listing's: every directory is filtered through
/// [`filter_visible`], so a search cannot surface the name of a file the caller
/// could not have opened, and a directory they may not enter is not descended
/// into. Unlike the content search, **nothing is excluded by default**: a person
/// looking for a file in `node_modules` means it, and there is no ten-thousand-
/// matches-per-file blow-up here to guard against — only [`MAX_FILES_SCANNED`]
/// entries visited and `max_results` returned.
pub async fn find_files(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    caller: &crate::FileCaller,
    dir: &str,
    query: &str,
    max_results: usize,
) -> Result<FoundFiles> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Err(Error::invalid("a search needs something to search for"));
    }
    let max_results = max_results.max(1);

    let mut found = FoundFiles::default();
    let mut visited = 0usize;
    let root_floor = crate::effective_min_role(store, store_min_role, dir).await?;
    let mut stack = vec![(dir.to_owned(), root_floor)];

    while let Some((current, floor)) = stack.pop() {
        let entries = store.list(&current).await?;
        let visible = filter_visible(store, floor, entries, caller).await?;
        let mut directories = Vec::new();
        for entry in visible {
            visited += 1;
            if visited > MAX_FILES_SCANNED {
                found.truncated = true;
                return Ok(found);
            }
            if entry.name.to_lowercase().contains(&needle) {
                if found.entries.len() >= max_results {
                    found.truncated = true;
                    return Ok(found);
                }
                found.entries.push(entry.clone());
            }
            if entry.is_dir {
                let child_floor = crate::effective_min_role(store, floor, &entry.path).await?;
                directories.push((entry.path, child_floor));
            }
        }
        directories.reverse();
        stack.extend(directories);
    }
    Ok(found)
}

// --- finding files by type --------------------------------------------------

/// Directories [`find_by_extension`] does not descend into, on top of
/// [`DEFAULT_EXCLUDED_DIRS`]: the native projects mobile tooling writes (Expo,
/// Capacitor and Cordova all use these names), which hold dozens of generated
/// launcher icons and a debug keystore that are copies, not sources.
const GENERATED_PROJECT_DIRS: [&str; 2] = ["android", "ios"];

/// The files under `dir` the caller may see whose extension is one of
/// `extensions` (any case, without the dot), in path order — the choices for a
/// setting that names a file of a kind, such as an app icon or a keystore.
///
/// Dependency and generated directories are skipped ([`DEFAULT_EXCLUDED_DIRS`]
/// and [`GENERATED_PROJECT_DIRS`]): a project's `node_modules` holds thousands
/// of images and none of them is the app's.
pub async fn find_by_extension(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    caller: &crate::FileCaller,
    dir: &str,
    extensions: &[String],
) -> Result<FoundFiles> {
    let excluded: Vec<&str> = DEFAULT_EXCLUDED_DIRS
        .iter()
        .chain(GENERATED_PROJECT_DIRS.iter())
        .copied()
        .collect();
    let walked = walk_store(store, store_min_role, caller, dir, &excluded).await?;
    let mut entries: Vec<Entry> = walked
        .entries
        .into_iter()
        .filter(|e| !e.is_dir && has_extension(&e.name, extensions))
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(FoundFiles {
        entries,
        truncated: walked.truncated,
    })
}

/// Whether a file name ends in one of `extensions`, in any case.
fn has_extension(name: &str, extensions: &[String]) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        extensions
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known.trim_start_matches('.')))
    })
}

// --- walking the visible tree ------------------------------------------------

/// Every entry under `dir` the caller may see, files and directories, depth
/// first, without descending into a directory named in `exclude_dirs`.
///
/// The walk `search_store` and [`find_files`] each do, for a caller that wants
/// to choose and order the entries itself (the coding agent's `find_files`,
/// which sorts by modification time). Access is filtered per directory exactly
/// as theirs is. At most [`MAX_FILES_SCANNED`] entries are visited, and the
/// outcome says when that stopped the walk.
pub async fn walk_store(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    caller: &crate::FileCaller,
    dir: &str,
    exclude_dirs: &[&str],
) -> Result<FoundFiles> {
    let mut found = FoundFiles::default();
    let root_floor = crate::effective_min_role(store, store_min_role, dir).await?;
    let mut stack = vec![(dir.to_owned(), root_floor)];
    while let Some((current, floor)) = stack.pop() {
        let entries = store.list(&current).await?;
        let visible = filter_visible(store, floor, entries, caller).await?;
        let mut directories = Vec::new();
        for entry in visible {
            if found.entries.len() >= MAX_FILES_SCANNED {
                found.truncated = true;
                return Ok(found);
            }
            if entry.is_dir {
                if exclude_dirs.contains(&entry.name.as_str()) {
                    continue;
                }
                let child_floor = crate::effective_min_role(store, floor, &entry.path).await?;
                directories.push((entry.path.clone(), child_floor));
            }
            found.entries.push(entry);
        }
        directories.reverse();
        stack.extend(directories);
    }
    Ok(found)
}
