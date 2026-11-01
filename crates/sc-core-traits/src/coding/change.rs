//! What every tool that changes a file shares (TODO §6): reading the current
//! content, writing through the change ledger and the run's record of what the
//! model has seen, and showing the model the lines it changed.

use std::ops::Range;

use bytes::Bytes;
use sc_error::{Error, Result};
use sc_files::CheckedStore;

use super::state::CodingState;

/// Context lines shown either side of an edited region.
pub const REGION_CONTEXT: usize = 2;

/// The most lines a shown region has before its middle is left out.
const MAX_REGION_LINES: usize = 40;

/// What is at `path` now: its bytes, or `None` when nothing is there. A
/// directory is an error, since no file tool can change one.
pub async fn current(store: &CheckedStore, path: &str, rel: &str) -> Result<Option<Bytes>> {
    match store.stat(path).await? {
        None => Ok(None),
        Some(stat) if stat.is_dir => Err(Error::invalid(format!(
            "`{rel}` is a directory, not a file"
        ))),
        Some(_) => Ok(Some(store.read(path).await?)),
    }
}

/// Write `content` to `path`, recording `before` in the ledger on the run's
/// first touch, and the new content as what the model has seen.
pub async fn write_tracked(
    store: &CheckedStore,
    state: &mut CodingState,
    path: &str,
    before: Option<&[u8]>,
    content: Vec<u8>,
) -> Result<()> {
    state.ledger.touch(path, before);
    let content = Bytes::from(content);
    store.write(path, content.clone()).await?;
    state.saw(path, &content);
    state.edited(path);
    Ok(())
}

/// Delete `path`, recording `before` in the ledger on the run's first touch.
pub async fn delete_tracked(
    store: &CheckedStore,
    state: &mut CodingState,
    path: &str,
    before: &[u8],
) -> Result<()> {
    state.ledger.touch(path, Some(before));
    store.delete(path).await?;
    state.forget(path);
    Ok(())
}

/// The line (from 1) a byte offset of `text` is on.
pub fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// The lines around the replacement at `range` of the edited `text`, numbered.
pub fn edited_region(text: &str, range: &Range<usize>) -> String {
    let first = line_of(text, range.start);
    // A replacement ending in a line break ends on the line before.
    let end = match text[range.clone()].ends_with('\n') && range.end > range.start {
        true => range.end - 1,
        false => range.end,
    };
    let last = line_of(text, end).max(first);
    numbered(
        text,
        first.saturating_sub(REGION_CONTEXT).max(1),
        last + REGION_CONTEXT,
    )
}

/// Lines `first..=last` of `text` (clamped to the file), numbered as
/// `read_file` numbers them, with the middle of a long region left out.
pub fn numbered(text: &str, first: usize, last: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let last = last.min(lines.len());
    if lines.is_empty() || first > last {
        return "(no lines)".to_owned();
    }
    let width = last.to_string().len();
    let row = |n: usize| format!("{n:>width$}\t{}", lines[n - 1]);
    let count = last - first + 1;
    if count <= MAX_REGION_LINES {
        return (first..=last).map(row).collect::<Vec<_>>().join("\n");
    }
    let half = MAX_REGION_LINES / 2;
    let mut out: Vec<String> = (first..first + half).map(row).collect();
    out.push(format!("…\t[{} lines not shown]", count - 2 * half));
    out.extend((last + 1 - half..=last).map(row));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edited_region_is_shown_with_its_context() {
        let text = "a\nb\nc\nNEW\nd\ne\nf\n";
        let start = text.find("NEW").unwrap();
        assert_eq!(
            edited_region(text, &(start..start + 4)),
            "2\tb\n3\tc\n4\tNEW\n5\td\n6\te"
        );
    }

    #[test]
    fn a_long_region_leaves_out_its_middle() {
        let text: String = (1..=100).map(|n| format!("{n}\n")).collect();
        let shown = numbered(&text, 1, 100);
        assert!(shown.contains("[60 lines not shown]"), "{shown}");
        assert!(shown.starts_with("  1\t1\n"), "{shown}");
        assert!(shown.ends_with("100\t100"), "{shown}");
    }
}
