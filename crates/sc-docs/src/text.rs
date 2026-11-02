//! Markdown as this crate reads it: headings into sections, and text into the
//! terms the index is built over.
//!
//! Neither is a markdown parser. A section is "a heading line outside a code
//! fence, up to the next one", which is all a document needs to be split into
//! the pieces a model reads one at a time, and a term is a run of letters,
//! digits and `_`, which is what keeps `run_js_code` and `allow_triggers` whole —
//! the names an agent searches by are code identifiers more often than they are
//! English.

/// One heading of a document and the text under it.
///
/// Offsets are into the document's text. A section **owns** the text from its
/// heading to the next heading of any level, and **spans** the text to the next
/// heading of its own level or above — its subsections included.
#[derive(Debug, Clone)]
pub struct Section {
    /// The heading's anchor: GitHub's slug of it, `-1`, `-2`… on a repeat.
    pub id: String,
    /// The heading as written, `#`s removed.
    pub heading: String,
    /// 1 for `#`, 2 for `##`, …
    pub level: u8,
    /// The headings this one sits under, outermost first, ending with its own.
    pub path: Vec<String>,
    /// Where its heading line starts.
    pub start: usize,
    /// Where its heading line ends: the start of the text it owns.
    pub body: usize,
    /// Where the next heading of any level starts.
    pub own_end: usize,
    /// Where the next heading of this level or above starts.
    pub end: usize,
}

/// The sections of `text`, in order. `text` is expected to start with a
/// heading; text before the first one belongs to no section.
pub fn sections(text: &str) -> Vec<Section> {
    let mut headings: Vec<(usize, usize, u8, String)> = Vec::new();
    let mut fence: Option<char> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        // Up to three spaces of indent, as CommonMark allows; more is code.
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        if let Some(c) = ['`', '~']
            .into_iter()
            .find(|c| trimmed.starts_with([*c; 3]))
        {
            fence = match fence {
                None => Some(c),
                Some(open) if open == c => None,
                still => still,
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some((level, heading)) = heading(trimmed) {
            headings.push((start, offset, level, heading));
        }
    }

    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Section> = Vec::with_capacity(headings.len());
    let mut stack: Vec<(u8, String)> = Vec::new();
    for (i, (start, body, level, heading)) in headings.iter().enumerate() {
        let own_end = headings.get(i + 1).map_or(text.len(), |h| h.0);
        let end = headings[i + 1..]
            .iter()
            .find(|h| h.2 <= *level)
            .map_or(text.len(), |h| h.0);
        while stack.last().is_some_and(|(l, _)| l >= level) {
            stack.pop();
        }
        stack.push((*level, heading.clone()));
        let base = slug(heading);
        let repeats = seen.iter().filter(|s| **s == base).count();
        seen.push(base.clone());
        out.push(Section {
            id: match repeats {
                0 => base,
                n => format!("{base}-{n}"),
            },
            heading: heading.clone(),
            level: *level,
            path: stack.iter().map(|(_, h)| h.clone()).collect(),
            start: *start,
            body: *body,
            own_end,
            end,
        });
    }
    out
}

/// `line` as a heading: its level and its text, or `None`.
fn heading(line: &str) -> Option<(u8, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !(rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r'])) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((hashes as u8, text.to_owned()))
}

/// GitHub's anchor for a heading: lower case, punctuation dropped, spaces to `-`.
pub fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// Words that carry no meaning in a query about this software.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from",
    "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "not", "of", "on", "or", "so",
    "that", "the", "then", "there", "this", "to", "was", "what", "when", "which", "with", "you",
];

/// The terms of `text`, each with the byte offset of the word it came from.
///
/// Lower-cased and lightly stemmed (a plural `s` dropped), stopwords removed. A
/// word with `_` in it is a term whole **and** each of its parts, so
/// `allow_triggers` is found by itself and by "allow triggers".
pub fn terms(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut push = |at: usize, word: &str| {
        let lower = word.to_lowercase();
        if lower.chars().count() < 2 && !lower.chars().all(|c| c.is_ascii_digit()) {
            return;
        }
        if STOPWORDS.contains(&lower.as_str()) {
            return;
        }
        out.push((at, stem(lower)));
    };
    let mut start: Option<usize> = None;
    let flush = |start: usize, end: usize, push: &mut dyn FnMut(usize, &str)| {
        let word = text[start..end].trim_matches('_');
        if word.is_empty() {
            return;
        }
        push(start, word);
        if word.contains('_') {
            let mut at = start;
            for part in text[start..end].split('_') {
                if !part.is_empty() {
                    push(at, part);
                }
                at += part.len() + 1;
            }
        }
    };
    for (i, c) in text.char_indices() {
        let word_char = c.is_alphanumeric() || c == '_';
        match (word_char, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                flush(s, i, &mut push);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        flush(s, text.len(), &mut push);
    }
    out
}

/// A plural taken off: `triggers` and `trigger` are one term, `queries` and
/// `query`. Nothing cleverer, because the vocabulary is mostly names.
fn stem(word: String) -> String {
    if word.len() > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..word.len() - 3]);
    }
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") && !word.ends_with("us") {
        return word[..word.len() - 1].to_owned();
    }
    word
}

/// The largest char boundary of `text` at or before `at`.
pub fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Runs of whitespace as one space.
pub fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_in_a_code_fence_is_not_a_section() {
        let text =
            "# Title\n\nintro\n\n## One\n\n```bash\n# a comment\n```\n\n### One a\n\nx\n\n## Two\n";
        let s = sections(text);
        let ids: Vec<&str> = s.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["title", "one", "one-a", "two"]);
        // `One` spans its subsection and owns only what is above it.
        assert!(text[s[1].start..s[1].end].contains("x"));
        assert!(!text[s[1].start..s[1].own_end].contains("One a"));
        assert_eq!(s[2].path, ["Title", "One", "One a"]);
        assert_eq!(s[0].end, text.len());
    }

    #[test]
    fn a_repeated_heading_gets_githubs_suffix() {
        let s = sections("# T\n## Step 1 — Turn it on\n## Example\n## Example\n");
        let ids: Vec<&str> = s.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["t", "step-1--turn-it-on", "example", "example-1"]);
    }

    #[test]
    fn identifiers_are_terms_whole_and_in_parts() {
        let words: Vec<String> = terms("Set `allow_triggers` on the Triggers")
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert_eq!(
            words,
            ["set", "allow_trigger", "allow", "trigger", "trigger"]
        );
        assert_eq!(terms("queries")[0].1, "query");
    }
}
