//! Rendering the ranked map, and fitting it to a token budget (TODO 7.4).
//!
//! ```text
//! src/TaskList.tsx
//!    9│ export function TaskList({ filter }: Props) {
//! src/api.ts
//!    3│ export async function listTasks(filter: Filter) {
//!   18│ export type Filter = 'all' | 'done';
//! README.md
//! ```
//!
//! Files appear in the order of their best-ranked entry, so the map reads from
//! what matters most; within a file, definitions are in line order, so it reads
//! like the file. A file with nothing ranked in it is its path alone.
//!
//! **Fitting** is a binary search for the longest prefix of the ranked list
//! whose rendering the counter puts within the budget, so what is left out is
//! always what ranked lowest.

use std::collections::BTreeMap;

use crate::rank::Ranked;

/// The budget a map gets when nobody says otherwise (`repo_map_tokens`).
pub const DEFAULT_TOKENS: usize = 1024;

/// Characters per token, the same rule `sc-llm`'s estimate uses for text.
const CHARS_PER_TOKEN: f64 = 3.5;

/// A text's size in tokens, estimated as `sc-llm` estimates a request's text.
#[allow(
    clippy::cast_possible_truncation,
    reason = "an estimate: a non-negative float, saturating at usize::MAX"
)]
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
}

/// Render `entries` as the map.
pub fn render_entries(entries: &[Ranked]) -> String {
    // Files by first appearance; each with its definitions' lines.
    let mut order: Vec<&str> = Vec::new();
    let mut rows: BTreeMap<&str, Vec<(u32, &str)>> = BTreeMap::new();
    for entry in entries {
        let path = entry.path();
        if !rows.contains_key(path) {
            order.push(path);
            rows.insert(path, Vec::new());
        }
        if let Ranked::Definition { tag, .. } = entry
            && let Some(lines) = rows.get_mut(path)
        {
            lines.push((tag.line, tag.signature.as_str()));
        }
    }
    let mut out = String::new();
    for path in order {
        out.push_str(path);
        out.push('\n');
        let Some(lines) = rows.get_mut(path) else {
            continue;
        };
        lines.sort_unstable();
        lines.dedup();
        for (line, signature) in lines.iter() {
            out.push_str(&format!("{line:>5}│ {signature}\n"));
        }
    }
    out
}

/// The most of `ranked` whose rendering `count` puts within `tokens`.
pub fn fit(ranked: &[Ranked], tokens: usize, count: impl Fn(&str) -> usize) -> String {
    let fits = |n: usize| {
        let text = render_entries(&ranked[..n]);
        (count(&text) <= tokens).then_some(text)
    };
    if let Some(all) = fits(ranked.len()) {
        return all;
    }
    // The longest prefix that fits: `low` always fits, `high` never does.
    let (mut low, mut high) = (0, ranked.len());
    let mut best = String::new();
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match fits(middle) {
            Some(text) => {
                low = middle;
                best = text;
            }
            None => high = middle,
        }
    }
    best
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tags::{Tag, TagKind};

    fn definition(path: &str, name: &str, line: u32, rank: f64) -> Ranked {
        Ranked::Definition {
            path: path.to_owned(),
            tag: Tag {
                name: name.to_owned(),
                kind: TagKind::Definition,
                line,
                syntax: "function".to_owned(),
                signature: format!("export function {name}() {{"),
            },
            rank,
        }
    }

    #[test]
    fn files_in_rank_order_and_lines_in_file_order() {
        let entries = vec![
            definition("src/api.ts", "listTasks", 18, 0.5),
            definition("src/App.tsx", "App", 3, 0.3),
            definition("src/api.ts", "saveTask", 4, 0.2),
            Ranked::File {
                path: "README.md".to_owned(),
            },
        ];
        assert_eq!(
            render_entries(&entries),
            "src/api.ts\n\
             \x20   4│ export function saveTask() {\n\
             \x20  18│ export function listTasks() {\n\
             src/App.tsx\n\
             \x20   3│ export function App() {\n\
             README.md\n"
        );
    }

    #[test]
    fn the_fit_is_the_longest_prefix_under_the_budget() {
        let entries: Vec<Ranked> = (0..200)
            .map(|i| definition(&format!("src/f{i}.ts"), &format!("f{i}"), 1, 1.0))
            .collect();
        for budget in [0, 10, 50, 333, 1024] {
            let map = fit(&entries, budget, estimate_tokens);
            assert!(estimate_tokens(&map) <= budget, "{budget}");
            let shown = map.lines().filter(|l| l.starts_with("src/")).count();
            // One more entry would not have fitted.
            if shown < entries.len() {
                let more = render_entries(&entries[..shown + 1]);
                assert!(estimate_tokens(&more) > budget, "{budget}: {shown}");
            }
            // And what is shown is the top of the ranking.
            if shown > 0 {
                assert!(map.starts_with("src/f0.ts\n"));
            }
        }
        assert_eq!(fit(&entries, 100_000, estimate_tokens).lines().count(), 400);
    }
}
