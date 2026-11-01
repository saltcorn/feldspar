//! The match cascade: where in a file the text a model quoted is (TODO §6, R§3.1).
//!
//! A model editing a file quotes the text it wants replaced, and a cheap model
//! quotes it slightly wrong: a trailing space lost, a CRLF file quoted with LFs,
//! a block quoted at the wrong indentation, one character mistyped. An exact
//! match refuses all four, and each refusal costs a turn and usually a re-read.
//! So the search tries four steps, from strict to loose, and stops at the first
//! step that finds anything:
//!
//! 1. **exact**: the quoted text, character for character;
//! 2. **whitespace**: whole lines equal once trailing whitespace and `\r` are
//!    ignored;
//! 3. **indentation**: whole lines equal once all leading and trailing
//!    whitespace is ignored, with the replacement **re-indented** to where the
//!    match is;
//! 4. **fuzzy**: the region of the same number of lines whose similarity to the
//!    quote is highest, if it is at least [`FUZZY_THRESHOLD`], also re-indented.
//!
//! **Every step must find exactly one place.** Two places at one step is an
//! ambiguity, and it is reported as one rather than resolved by falling through
//! to a looser step or by picking the first: an edit applied to the wrong one of
//! two identical blocks is a corrupted file nobody notices. `replace_all` is the
//! explicit way to mean every place, and it is not offered for the fuzzy step,
//! where "every place that looks roughly like this" is never what was meant.
//!
//! The steps after the first compare **whole lines**. A quote that is a fragment
//! of a line is found by the exact step or not at all.
//!
//! Nothing here reads or writes a file. `edit_file` and `apply_patch` do the I/O
//! and use this for the judgement.

use std::ops::Range;

use similar::TextDiff;

/// How similar a region must be to the quoted text for the fuzzy step to use it.
///
/// High on purpose. The fuzzy step exists for a mistyped character or a changed
/// comment in a block that is otherwise right, not for a block that is only
/// roughly right.
pub const FUZZY_THRESHOLD: f64 = 0.9;

/// The most (file line × quoted line) comparisons the fuzzy step makes before it
/// gives up. A 4000-line file and a 50-line quote is 200 000.
const FUZZY_MAX_COMPARISONS: usize = 400_000;

/// Which step of the cascade found the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Character for character.
    Exact,
    /// Ignoring trailing whitespace and `\r`.
    Whitespace,
    /// Ignoring indentation, then re-indenting the replacement.
    Indentation,
    /// The unique best region above [`FUZZY_THRESHOLD`].
    Fuzzy,
}

impl Level {
    /// How it reads in a tool result.
    pub fn describe(self) -> &'static str {
        match self {
            Level::Exact => "exact match",
            Level::Whitespace => "matched ignoring trailing whitespace and line endings",
            Level::Indentation => "matched ignoring indentation; the new text was re-indented",
            Level::Fuzzy => "fuzzy match; the new text was re-indented",
        }
    }

    /// A short name, for a patch's summary line.
    pub fn short(self) -> &'static str {
        match self {
            Level::Exact => "exact",
            Level::Whitespace => "whitespace",
            Level::Indentation => "indentation",
            Level::Fuzzy => "fuzzy",
        }
    }
}

/// One place the quote was found, as a byte range of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Where the matched text starts.
    pub start: usize,
    /// Where it ends (exclusive).
    pub end: usize,
    /// The step that found it.
    pub level: Level,
    /// The similarity, which is 1 for every step but the fuzzy one.
    pub score: f64,
    /// How to re-indent the replacement, for the steps that ignore indentation.
    reindent: Option<Reindent>,
}

/// The indentation the replacement is moved from and to.
#[derive(Debug, Clone, PartialEq)]
struct Reindent {
    /// The quote's common indentation, in characters.
    quoted: usize,
    /// The matched region's common indentation, as it is written.
    actual: String,
}

/// A region of the file, by line numbers counting from 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    /// The first line.
    pub first: usize,
    /// The last line (inclusive).
    pub last: usize,
    /// How similar it is to the quote, from 0 to 1.
    pub score: f64,
}

/// What a search found.
#[derive(Debug, Clone, PartialEq)]
pub enum Search {
    /// One place, or every place when all were asked for. Never empty.
    Found(Vec<Found>),
    /// More than one place at the first step that found any.
    Ambiguous {
        /// That step.
        level: Level,
        /// The first line of each place.
        lines: Vec<usize>,
    },
    /// Nowhere. `closest` is the most similar region, where there is one worth
    /// showing.
    Missing {
        /// The most similar region.
        closest: Option<Region>,
    },
}

/// Find `quote` in `text`: one place, or every place at the first step that
/// finds any when `all` is set.
pub fn find(text: &str, quote: &str, all: bool) -> Search {
    search(text, quote, all, false)
}

/// The search a patch hunk makes: only `text[from..]`, and only whole lines,
/// so the exact step cannot match a quoted `}` in the middle of `  }`. Offsets
/// and line numbers in the answer are still the whole text's. `from` must be
/// the start of a line.
pub fn find_lines_from(text: &str, from: usize, quote: &str) -> Search {
    let rest = &text[from..];
    let line_offset = text[..from].matches('\n').count();
    let mut search = search(rest, quote, false, true);
    match &mut search {
        Search::Found(found) => {
            for f in found {
                f.start += from;
                f.end += from;
            }
        }
        Search::Ambiguous { lines, .. } => {
            for line in lines {
                *line += line_offset;
            }
        }
        Search::Missing { closest } => {
            if let Some(region) = closest {
                region.first += line_offset;
                region.last += line_offset;
            }
        }
    }
    search
}

/// Replace every found place with `new`, adapted to each: re-indented where the
/// step ignored indentation, and given `\r\n` endings where the place had them
/// and `new` does not.
///
/// `found` must be in file order and must not overlap, as [`find`] returns it.
/// Returns the new text and, for each place, the byte range its replacement
/// occupies in it.
pub fn replace(text: &str, found: &[Found], new: &str) -> (String, Vec<Range<usize>>) {
    let mut out = String::with_capacity(text.len() + new.len());
    let mut ranges = Vec::with_capacity(found.len());
    let mut at = 0;
    for place in found {
        out.push_str(&text[at..place.start]);
        let start = out.len();
        out.push_str(&adapt(&text[place.start..place.end], place, new));
        ranges.push(start..out.len());
        at = place.end;
    }
    out.push_str(&text[at..]);
    (out, ranges)
}

/// The replacement for one place.
fn adapt(matched: &str, place: &Found, new: &str) -> String {
    let mut new = match &place.reindent {
        Some(reindent) => reindent.apply(new),
        None => new.to_owned(),
    };
    if matched.contains("\r\n") && !new.contains('\r') {
        new = new.replace('\n', "\r\n");
    }
    new
}

impl Reindent {
    /// Move every non-blank line of `new` from the quote's indentation to the
    /// match's, keeping whatever it has beyond that.
    fn apply(&self, new: &str) -> String {
        new.split('\n')
            .map(|line| {
                if line.trim().is_empty() {
                    return line.to_owned();
                }
                let leading = leading_whitespace(line);
                let drop = leading.min(self.quoted);
                let rest: String = line.chars().skip(drop).collect();
                format!("{}{rest}", self.actual)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One line of the file.
#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    /// Its first byte.
    start: usize,
    /// The end of its content, before any `\r\n` or `\n`.
    content_end: usize,
    /// The end of the line, after its `\n` if it has one.
    end: usize,
    /// Its content.
    text: &'a str,
}

/// The file's lines, with their offsets.
fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let (content_end, end) = match text[start..].find('\n') {
            Some(i) => (start + i, start + i + 1),
            None => (text.len(), text.len()),
        };
        let content_end = match text[start..content_end].ends_with('\r') {
            true => content_end - 1,
            false => content_end,
        };
        out.push(Line {
            start,
            content_end,
            end,
            text: &text[start..content_end],
        });
        start = end;
    }
    out
}

/// The quote as lines: each without its `\r`, and whether it ended with a line
/// break (which is then not an extra, empty line).
fn quote_lines(quote: &str) -> (Vec<&str>, bool) {
    let trailing = quote.ends_with('\n');
    let body = match trailing {
        true => &quote[..quote.len() - 1],
        false => quote,
    };
    let lines = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    (lines, trailing)
}

fn leading_whitespace(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// Whether two lines are the same at one step of the cascade.
type LineEq = fn(&str, &str) -> bool;

/// The cascade over `text`, with offsets relative to it.
fn search(text: &str, quote: &str, all: bool, whole_lines: bool) -> Search {
    if quote.is_empty() {
        return Search::Missing { closest: None };
    }

    // 1. Exact.
    let exact: Vec<usize> = text
        .match_indices(quote)
        .map(|(i, _)| i)
        .filter(|&i| !whole_lines || i == 0 || text.as_bytes()[i - 1] == b'\n')
        .collect();
    if !exact.is_empty() {
        let found: Vec<Found> = exact
            .iter()
            .map(|&start| Found {
                start,
                end: start + quote.len(),
                level: Level::Exact,
                score: 1.0,
                reindent: None,
            })
            .collect();
        return decide(text, found, all, Level::Exact);
    }

    let file = lines(text);
    let (quoted, trailing) = quote_lines(quote);
    // A quote of nothing but blank lines matches every run of blank lines,
    // which is not a place.
    if quoted.iter().all(|l| l.trim().is_empty()) || quoted.len() > file.len() {
        return Search::Missing { closest: None };
    }

    // 2 and 3. Whole lines, compared more loosely each time.
    let steps: [(Level, LineEq); 2] = [
        (Level::Whitespace, |a, b| a.trim_end() == b.trim_end()),
        (Level::Indentation, |a, b| a.trim() == b.trim()),
    ];
    for (level, same) in steps {
        let mut found = Vec::new();
        let mut i = 0;
        while i + quoted.len() <= file.len() {
            let window = &file[i..i + quoted.len()];
            if window.iter().zip(&quoted).all(|(l, q)| same(l.text, q)) {
                found.push(place(window, &quoted, trailing, level, 1.0));
                i += quoted.len();
            } else {
                i += 1;
            }
        }
        if !found.is_empty() {
            return decide(text, found, all, level);
        }
    }

    // 4. Fuzzy.
    fuzzy(text, &file, &quoted, trailing)
}

/// One place, or every place, or an ambiguity.
fn decide(text: &str, found: Vec<Found>, all: bool, level: Level) -> Search {
    if found.len() == 1 || all {
        return Search::Found(found);
    }
    Search::Ambiguous {
        level,
        lines: found
            .iter()
            .map(|f| text[..f.start].matches('\n').count() + 1)
            .collect(),
    }
}

/// The byte range a window of whole lines covers, and how to re-indent into it.
fn place(window: &[Line<'_>], quoted: &[&str], trailing: bool, level: Level, score: f64) -> Found {
    let first = window[0];
    let last = window[window.len() - 1];
    let reindent = matches!(level, Level::Indentation | Level::Fuzzy).then(|| {
        let quoted_indent = quoted
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| leading_whitespace(l))
            .min()
            .unwrap_or(0);
        let actual = window
            .iter()
            .filter(|l| !l.text.trim().is_empty())
            .min_by_key(|l| leading_whitespace(l.text))
            .map(|l| {
                l.text
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect::<String>()
            })
            .unwrap_or_default();
        Reindent {
            quoted: quoted_indent,
            actual,
        }
    });
    Found {
        start: first.start,
        end: match trailing {
            true => last.end,
            false => last.content_end,
        },
        level,
        score,
        reindent,
    }
}

/// How similar two lines are, ignoring their indentation: 0 to 1.
fn line_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    f64::from(TextDiff::from_chars(a, b).ratio())
}

/// The fuzzy step, and the closest region when it finds nothing.
fn fuzzy(text: &str, file: &[Line<'_>], quoted: &[&str], trailing: bool) -> Search {
    let n = quoted.len();
    let windows = file.len() + 1 - n;
    if windows.saturating_mul(n) > FUZZY_MAX_COMPARISONS {
        return Search::Missing {
            closest: closest_by_first_line(file, quoted),
        };
    }
    // Longer lines count for more: a mistyped brace on its own line should not
    // weigh as much as a mistyped signature.
    let weights: Vec<f64> = quoted
        .iter()
        .map(|q| q.trim().chars().count().max(1) as f64)
        .collect();
    let total: f64 = weights.iter().sum();
    let scores: Vec<f64> = (0..windows)
        .map(|i| {
            file[i..i + n]
                .iter()
                .zip(quoted)
                .zip(&weights)
                .map(|((l, q), w)| line_similarity(l.text, q) * w)
                .sum::<f64>()
                / total
        })
        .collect();

    // The candidates above the threshold, best first, dropping any that overlap
    // a better one: a region shifted by a line is the same place.
    let mut ranked: Vec<usize> = (0..windows)
        .filter(|&i| scores[i] >= FUZZY_THRESHOLD)
        .collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    let mut chosen: Vec<usize> = Vec::new();
    for i in ranked {
        if chosen.iter().all(|&c| i + n <= c || c + n <= i) {
            chosen.push(i);
        }
    }
    match chosen.as_slice() {
        [one] => Search::Found(vec![place(
            &file[*one..*one + n],
            quoted,
            trailing,
            Level::Fuzzy,
            scores[*one],
        )]),
        [] => {
            let best =
                (0..windows).max_by(|&a, &b| scores[a].total_cmp(&scores[b]).then(b.cmp(&a)));
            Search::Missing {
                closest: best.filter(|&i| scores[i] > 0.0).map(|i| Region {
                    first: i + 1,
                    last: i + n,
                    score: scores[i],
                }),
            }
        }
        many => {
            let mut lines: Vec<usize> = many
                .iter()
                .map(|&i| text[..file[i].start].matches('\n').count() + 1)
                .collect();
            lines.sort_unstable();
            Search::Ambiguous {
                level: Level::Fuzzy,
                lines,
            }
        }
    }
}

/// For a file too big to score every region of: the region starting at the line
/// most like the quote's first non-blank line.
fn closest_by_first_line(file: &[Line<'_>], quoted: &[&str]) -> Option<Region> {
    let (offset, first) = quoted
        .iter()
        .enumerate()
        .find(|(_, l)| !l.trim().is_empty())?;
    let (index, score) = file
        .iter()
        .enumerate()
        .map(|(i, l)| (i, line_similarity(l.text, first)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let start = index.saturating_sub(offset);
    Some(Region {
        first: start + 1,
        last: (start + quoted.len()).min(file.len()),
        score,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply `old` → `new` and return the text and the level, or panic with
    /// what the search said instead.
    fn edit(text: &str, old: &str, new: &str) -> (String, Level) {
        match find(text, old, false) {
            Search::Found(found) => {
                assert_eq!(found.len(), 1);
                (replace(text, &found, new).0, found[0].level)
            }
            other => panic!("expected one match, got {other:?}"),
        }
    }

    const FILE: &str = "function a() {\n    const x = 1;\n    return x;\n}\n";

    // --- each level finds what it is for -------------------------------------

    #[test]
    fn the_exact_step_matches_character_for_character() {
        let (text, level) = edit(FILE, "const x = 1;", "const x = 2;");
        assert_eq!(level, Level::Exact);
        assert_eq!(text, FILE.replace("x = 1", "x = 2"));
    }

    #[test]
    fn the_whitespace_step_ignores_trailing_spaces_and_crlf() {
        // The file has a trailing space the model did not quote.
        let file = "a();\nb();   \nc();\n";
        let (text, level) = edit(file, "a();\nb();\n", "a();\nB();\n");
        assert_eq!(level, Level::Whitespace);
        assert_eq!(text, "a();\nB();\nc();\n");

        // A CRLF file quoted with LFs keeps its CRLFs.
        let file = "a();\r\nb();\r\nc();\r\n";
        let (text, level) = edit(file, "a();\nb();\n", "a();\nB();\n");
        assert_eq!(level, Level::Whitespace);
        assert_eq!(text, "a();\r\nB();\r\nc();\r\n");
    }

    #[test]
    fn the_indentation_step_reindents_the_replacement() {
        // Quoted at no indentation; the file has it at four spaces.
        let (text, level) = edit(
            FILE,
            "const x = 1;\nreturn x;",
            "const x = 1;\nif (x) {\n  return x;\n}",
        );
        assert_eq!(level, Level::Indentation);
        assert_eq!(
            text,
            "function a() {\n    const x = 1;\n    if (x) {\n      return x;\n    }\n}\n"
        );

        // Quoted too deep: the extra indentation is removed, and tabs in the
        // file are kept.
        let file = "if (a) {\n\tb();\n\tc();\n}\n";
        let (text, _) = edit(file, "        b();\n        c();\n", "        b();\n");
        assert_eq!(text, "if (a) {\n\tb();\n}\n");
    }

    #[test]
    fn the_fuzzy_step_takes_the_unique_best_region_above_the_threshold() {
        let file = "export function total(items: Item[]): number {\n  \
                    return items.reduce((sum, item) => sum + item.price, 0);\n}\n\
                    export const other = 1;\n";
        // One character wrong in a long line.
        let (text, level) = edit(
            file,
            "export function total(items: Item[]): number {\n  \
             return items.reduce((sum, item) => sum + item.prise, 0);\n}\n",
            "export function total(items: Item[]): number {\n  return 0;\n}\n",
        );
        assert_eq!(level, Level::Fuzzy);
        assert_eq!(
            text,
            "export function total(items: Item[]): number {\n  return 0;\n}\n\
             export const other = 1;\n"
        );
    }

    #[test]
    fn a_step_is_used_only_when_the_stricter_ones_found_nothing() {
        // `b();` is there exactly, so the exact step answers even though a looser
        // step would also have matched.
        let file = "  b();\nb();  \n";
        match find(file, "b();", false) {
            Search::Ambiguous { level, lines } => {
                assert_eq!(level, Level::Exact);
                assert_eq!(lines, vec![1, 2]);
            }
            other => panic!("{other:?}"),
        }
    }

    // --- ambiguity at each level ---------------------------------------------

    #[test]
    fn two_exact_places_are_ambiguous_and_named_by_line() {
        let file = "x = 1;\ny = 2;\nx = 1;\n";
        assert_eq!(
            find(file, "x = 1;", false),
            Search::Ambiguous {
                level: Level::Exact,
                lines: vec![1, 3]
            }
        );
    }

    #[test]
    fn two_places_equal_but_for_trailing_whitespace_are_ambiguous() {
        let file = "a();  \nb();\na(); \t\nb();\n";
        assert_eq!(
            find(file, "a();\nb();\n", false),
            Search::Ambiguous {
                level: Level::Whitespace,
                lines: vec![1, 3]
            }
        );
    }

    #[test]
    fn two_places_equal_but_for_indentation_are_ambiguous() {
        let file = "  a();\n  b();\n\n      a();\n      b();\n";
        assert_eq!(
            find(file, "a();\nb();", false),
            Search::Ambiguous {
                level: Level::Indentation,
                lines: vec![1, 4]
            }
        );
    }

    #[test]
    fn two_fuzzy_places_are_ambiguous() {
        let block = "const total = items.reduce((sum, item) => sum + item.price, 0);\n";
        let file = format!("{block}console.log(1);\n{block}");
        let quote = "const total = items.reduce((sum, item) => sum + item.prise, 0);\n";
        assert_eq!(
            find(&file, quote, false),
            Search::Ambiguous {
                level: Level::Fuzzy,
                lines: vec![1, 3]
            }
        );
    }

    // --- all, missing, and the offsets ---------------------------------------

    #[test]
    fn all_replaces_every_place_at_the_step_that_found_them() {
        let file = "  a();\n    a();\n";
        match find(file, "a();\n", true) {
            Search::Found(found) => {
                assert_eq!(found.len(), 2);
                assert_eq!(found[0].level, Level::Exact);
                assert_eq!(replace(file, &found, "b();\n").0, "  b();\n    b();\n");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_quote_found_nowhere_reports_the_closest_region() {
        let file = "one();\ntwo();\nfunction three() {\n  return 3;\n}\n";
        match find(file, "function tree(x) {\n  return x * 4;\n}", false) {
            Search::Missing {
                closest: Some(region),
            } => {
                assert_eq!((region.first, region.last), (3, 5));
                assert!(region.score < FUZZY_THRESHOLD, "{region:?}");
            }
            other => panic!("{other:?}"),
        }
        // A quote of blank lines is not a place.
        assert_eq!(find(file, "\n\n", false), Search::Missing { closest: None });
    }

    #[test]
    fn a_hunk_search_reports_whole_file_positions_and_matches_whole_lines() {
        let file = "a();\nb();\na();\nb();\n";
        let from = "a();\nb();\n".len();
        match find_lines_from(file, from, "a();\n") {
            Search::Found(found) => assert_eq!(found[0].start, from),
            other => panic!("{other:?}"),
        }
        match find_lines_from(file, from, "zzz_unlike_anything\nb();") {
            Search::Missing { closest } => assert_eq!(closest.map(|r| r.first), Some(3)),
            other => panic!("{other:?}"),
        }
        // `}` quoted as a line is the line `}`, not the brace ending `  }`.
        let file = "if (a) {\n  b();\n  }\n}\n";
        match find_lines_from(file, 0, "}\n") {
            Search::Found(found) => assert_eq!(found[0].start, file.rfind('}').unwrap()),
            other => panic!("{other:?}"),
        }
    }
}
