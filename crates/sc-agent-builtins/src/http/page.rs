//! What one call shows of a document: a window of it, or the lines matching a
//! search, under a header that says exactly what was left out.
//!
//! This is where the context budget is kept, and it is kept by **paging, not
//! by summarising**. The alternative other agents use — hand the page to a
//! second, cheaper model with the question, and return its answer — is lossy
//! in a way the caller cannot see: the small model decides what mattered, and
//! "the page does not mention X" reads the same as "X was not in what I was
//! given". A window is exact, and it is honest about its edges:
//!
//! - **The header says what the document is and how big**, before any of it:
//!   lines, characters, what it was converted from, whether the download was
//!   cut off.
//! - **A document that does not fit says so, with the way on**: the lines
//!   shown, the lines left, and the `start_line` that continues. Truncation
//!   that is not marked is the failure this is designed against — a model that
//!   cannot tell it was given part of a page reasons as if it had all of it.
//! - **The first window of a long document carries its outline**: the headings
//!   with their line numbers, so the next call can jump to the section that
//!   answers the question instead of reading its way down.
//! - **`find` searches the whole document** and returns the matching lines with
//!   a little context and their line numbers — the cheap half of "write code to
//!   filter the page", with no code.

use super::document::{Document, Kind};

/// The most headings an outline lists.
pub const MAX_OUTLINE_ENTRIES: usize = 40;

/// Lines of context either side of a `find` match.
const FIND_CONTEXT: usize = 2;

/// The longest a heading is shown in an outline.
const OUTLINE_HEADING_CHARS: usize = 100;

/// What to show.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    /// The first line to show (or to search from), counting from 1.
    pub start_line: usize,
    /// Show the lines containing this instead of a window.
    pub find: Option<&'a str>,
    /// The most characters of the document to show.
    pub max_chars: usize,
}

/// The tool result for `doc` under `view`.
///
/// `cached` says the document came from this conversation's cache rather than
/// a new request, which the model should know when it is waiting for a page
/// to change.
pub fn render(doc: &Document, view: &View<'_>, cached: bool) -> String {
    let mut out = header(doc, cached);
    if doc.kind == Kind::Binary {
        out.push_str(&format!(
            "\nThis is not text ({}, {} bytes), so none of it is shown.",
            doc.media_type,
            thousands(doc.source_bytes)
        ));
        return out;
    }
    if doc.lines.is_empty() {
        out.push_str("\nThe response has no text content.");
        return out;
    }
    let total = doc.lines.len();
    let start = view.start_line.max(1);
    if start > total {
        out.push_str(&format!(
            "\n`start_line` {start} is past the end: the document has {} lines.",
            thousands(total)
        ));
        return out;
    }
    match view.find.map(str::trim).filter(|f| !f.is_empty()) {
        Some(needle) => render_find(&mut out, doc, needle, start, view.max_chars),
        None => render_window(&mut out, doc, start, view.max_chars),
    }
    out
}

fn header(doc: &Document, cached: bool) -> String {
    let mut out = format!("{} {} {}", doc.status, doc.media_type, doc.url);
    if let Some(requested) = &doc.requested {
        out.push_str(&format!(" (redirected from {requested})"));
    }
    if cached {
        out.push_str(" (from this conversation's cache)");
    }
    if let Some(title) = &doc.title {
        out.push_str(&format!("\ntitle: {title}"));
    }
    if doc.kind != Kind::Binary && !doc.lines.is_empty() {
        let from = match doc.kind {
            Kind::Html => format!(
                "Markdown converted from {} bytes of HTML",
                thousands(doc.source_bytes)
            ),
            Kind::Json => "JSON, pretty-printed".to_owned(),
            _ => "text".to_owned(),
        };
        out.push_str(&format!(
            "\ndocument: {from}; {} lines, {} characters.",
            thousands(doc.lines.len()),
            thousands(doc.chars)
        ));
    }
    if doc.download_truncated {
        out.push_str(&format!(
            "\nnote: the response was longer than {} bytes and was cut off there; \
             the end of the document is missing.",
            thousands(super::client::MAX_DOWNLOAD_BYTES)
        ));
    }
    if doc.scripted {
        out.push_str(
            "\nnote: this page has almost no text without JavaScript, which this tool \
             does not run. Look for a static version: the project's README, a raw \
             source file, an API that returns JSON, or `/llms.txt` on the same site.",
        );
    }
    out
}

/// The lines from `start` that fit in `max_chars`, and how to go on.
fn render_window(out: &mut String, doc: &Document, start: usize, max_chars: usize) {
    let total = doc.lines.len();
    let end = window_end(&doc.lines, start, max_chars);
    let whole = start == 1 && end == total;
    if !whole {
        out.push_str(&format!(
            "\nshowing lines {start}–{end} of {}.",
            thousands(total)
        ));
        if start == 1 {
            push_outline(out, doc);
        }
    }
    out.push_str("\n---\n");
    out.push_str(&doc.lines[start - 1..end].join("\n"));
    if end < total {
        out.push_str(&format!(
            "\n---\n[{} more lines not shown. Continue with start_line={}, or pass \
             `find` to search the whole document.]",
            thousands(total - end),
            end + 1
        ));
    }
}

/// The last line (1-based, inclusive) of the window starting at `start`: as
/// many lines as fit in `max_chars`, and always at least one.
fn window_end(lines: &[String], start: usize, max_chars: usize) -> usize {
    let mut used = 0;
    let mut end = start - 1;
    for line in &lines[start - 1..] {
        let cost = line.chars().count() + 1;
        if end >= start && used + cost > max_chars {
            break;
        }
        used += cost;
        end += 1;
    }
    end
}

/// The headings of a Markdown document, with the line each is on.
///
/// Levels 1 to 3, outside fenced code (where a `#` is a comment). When there
/// are more than fit, the third level goes first, then the list is cut and
/// says how many it left out.
fn push_outline(out: &mut String, doc: &Document) {
    if !matches!(doc.kind, Kind::Html | Kind::Markdown) {
        return;
    }
    let mut headings: Vec<(usize, usize, &str)> = Vec::new();
    let mut fenced = false;
    for (i, line) in doc.lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let level = line.chars().take_while(|&c| c == '#').count();
        if (1..=3).contains(&level) && line[level..].starts_with(' ') {
            headings.push((i + 1, level, line.as_str()));
        }
    }
    if headings.len() < 2 {
        return;
    }
    if headings.len() > MAX_OUTLINE_ENTRIES {
        headings.retain(|&(_, level, _)| level <= 2);
    }
    let left_out = headings.len().saturating_sub(MAX_OUTLINE_ENTRIES);
    out.push_str("\noutline (line: heading):");
    for (line, _, heading) in headings.iter().take(MAX_OUTLINE_ENTRIES) {
        let mut shown: String = heading.chars().take(OUTLINE_HEADING_CHARS).collect();
        if heading.chars().count() > OUTLINE_HEADING_CHARS {
            shown.push('…');
        }
        out.push_str(&format!("\n{line}: {shown}"));
    }
    if left_out > 0 {
        out.push_str(&format!("\n… and {left_out} more headings; use `find`."));
    }
}

/// The lines from `start` on that contain `needle` (ignoring case), each with
/// [`FIND_CONTEXT`] lines either side, in as much as fits in `max_chars`.
///
/// grep's format, so it reads as what it is: `12:` is a match, `13-` is
/// context, `--` separates groups.
fn render_find(out: &mut String, doc: &Document, needle: &str, start: usize, max_chars: usize) {
    let needle = needle.to_lowercase();
    let matches: Vec<usize> = (start - 1..doc.lines.len())
        .filter(|&i| doc.lines[i].to_lowercase().contains(&needle))
        .collect();
    let from = match start {
        1 => String::new(),
        n => format!(" from line {n} on"),
    };
    if matches.is_empty() {
        out.push_str(&format!(
            "\nNo line{from} contains `{needle}` (case is ignored). The search is over \
             the whole document; try a shorter or different term."
        ));
        return;
    }
    out.push_str(&format!(
        "\n{} line{} contain{} `{needle}`{from}:\n---",
        matches.len(),
        if matches.len() == 1 { "" } else { "s" },
        if matches.len() == 1 { "s" } else { "" },
    ));

    // Groups of (first, last) line indexes, overlapping context merged.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &m in &matches {
        let lo = m.saturating_sub(FIND_CONTEXT).max(start - 1);
        let hi = (m + FIND_CONTEXT).min(doc.lines.len() - 1);
        match groups.last_mut() {
            Some((_, last)) if lo <= *last + 1 => *last = hi,
            _ => groups.push((lo, hi)),
        }
    }
    // The budget is spent a line at a time, not a group at a time: dense
    // matches merge into one group as long as the document, and "always show
    // the first group" would then be no budget at all.
    let mut used = 0;
    let mut last_shown = None;
    'groups: for (g, &(lo, hi)) in groups.iter().enumerate() {
        for i in lo..=hi {
            let mark = if matches.binary_search(&i).is_ok() {
                ':'
            } else {
                '-'
            };
            let separator = if g > 0 && i == lo { "\n--" } else { "" };
            let line = format!("{separator}\n{}{mark} {}", i + 1, doc.lines[i]);
            let cost = line.chars().count();
            if last_shown.is_some() && used + cost > max_chars {
                break 'groups;
            }
            used += cost;
            out.push_str(&line);
            last_shown = Some(i);
        }
    }
    let last_shown = last_shown.unwrap_or(0);
    let unshown = matches.iter().filter(|&&m| m > last_shown).count();
    if unshown > 0 {
        out.push_str(&format!(
            "\n---\n[{unshown} more matching lines not shown. Continue with the same \
             `find` and start_line={}.]",
            last_shown + 2
        ));
    }
}

/// `12345` as `12,345`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(kind: Kind, lines: Vec<String>) -> Document {
        let chars = lines.iter().map(|l| l.chars().count() + 1).sum();
        Document {
            url: "https://docs.example.com/big".into(),
            requested: None,
            status: 200,
            media_type: "text/html".into(),
            kind,
            title: Some("Big".into()),
            source_bytes: 400_000,
            download_truncated: false,
            scripted: false,
            lines,
            chars,
        }
    }

    /// Two hundred sections of ten lines each: 2,000 lines, far over a window.
    fn big() -> Document {
        let mut lines = Vec::new();
        for s in 1..=200 {
            lines.push(format!("## Section {s}"));
            for l in 1..=9 {
                lines.push(format!(
                    "Body line {l} of section {s}, padded to be a line of prose."
                ));
            }
        }
        doc(Kind::Html, lines)
    }

    fn view(start_line: usize, find: Option<&str>) -> View<'_> {
        View {
            start_line,
            find,
            max_chars: 2_000,
        }
    }

    #[test]
    fn a_long_document_is_windowed_and_says_where_to_go_on() {
        let d = big();
        let out = render(&d, &view(1, None), false);
        assert!(
            out.len() < 2_000 + 2_500,
            "the window plus header ran to {}",
            out.len()
        );
        assert!(
            out.contains("document: Markdown converted from 400,000 bytes of HTML; 2,000 lines")
        );
        let shown = out
            .lines()
            .find_map(|l| l.strip_prefix("showing lines 1–"))
            .expect("the range is stated");
        let end: usize = shown.split(' ').next().unwrap().parse().unwrap();
        assert!(
            out.contains(&format!("Continue with start_line={}", end + 1)),
            "{out}"
        );

        // The next window starts exactly there, and has no outline.
        let next = render(&d, &view(end + 1, None), false);
        assert!(
            next.contains(&format!("showing lines {}–", end + 1)),
            "{next}"
        );
        assert!(!next.contains("outline"), "{next}");
    }

    #[test]
    fn the_first_window_carries_an_outline_that_is_bounded() {
        let out = render(&big(), &view(1, None), false);
        assert!(
            out.contains("outline (line: heading):\n1: ## Section 1\n11: ## Section 2"),
            "{out}"
        );
        assert!(out.contains(&format!(
            "… and {} more headings",
            200 - MAX_OUTLINE_ENTRIES
        )));
    }

    #[test]
    fn a_short_document_is_shown_whole_with_no_paging_text() {
        let d = doc(
            Kind::Markdown,
            vec!["# Title".into(), "".into(), "Body.".into()],
        );
        let out = render(&d, &view(1, None), true);
        assert!(out.ends_with("---\n# Title\n\nBody."), "{out}");
        assert!(out.contains("(from this conversation's cache)"));
        assert!(
            !out.contains("showing lines") && !out.contains("more lines"),
            "{out}"
        );
    }

    #[test]
    fn find_returns_numbered_matches_with_context_and_pages_through_them() {
        let d = big();
        let out = render(&d, &view(1, Some("SECTION 150,")), false);
        assert!(out.contains("9 lines contain `section 150,`"), "{out}");
        assert!(out.contains("\n1492: Body line 1 of section 150"), "{out}");
        // Context is marked `-`, and the heading above the first match is it.
        assert!(out.contains("\n1491- ## Section 150"), "{out}");

        // Too many matches for one window: the rest are counted, with the way on.
        // Every body line matches, so context merges them all into one group
        // as long as the document: the budget must still hold.
        let every = render(&d, &view(1, Some("padded")), false);
        assert!(
            every.len() < 2_000 + 500,
            "a dense find ran to {}",
            every.len()
        );
        let next: usize = every
            .split("start_line=")
            .nth(1)
            .and_then(|s| s.trim_end_matches(".]").parse().ok())
            .expect("a continuation");
        let after = render(&d, &view(next, Some("padded")), false);
        assert!(after.contains(&format!("from line {next} on")), "{after}");
        assert!(after.contains(&format!("\n{next}: ")), "{after}");
    }

    #[test]
    fn find_with_no_match_and_a_start_past_the_end_say_so() {
        let d = big();
        assert!(render(&d, &view(1, Some("zebra")), false).contains("No line contains `zebra`"));
        assert!(
            render(&d, &view(5_000, None), false)
                .contains("past the end: the document has 2,000 lines")
        );
    }

    #[test]
    fn a_single_line_longer_than_the_window_is_still_shown() {
        let d = doc(Kind::Text, vec!["x".repeat(3_000), "tail".into()]);
        let out = render(&d, &view(1, None), false);
        assert!(out.contains("showing lines 1–1 of 2"), "{out}");
    }

    #[test]
    fn numbers_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(5_242_880), "5,242,880");
    }
}
