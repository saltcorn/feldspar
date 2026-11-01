//! `read_file`: read one text file out of the [`Coding`](super::Coding) trait's
//! scope, as numbered lines (TODO §6, R§3).
//!
//! - **Numbered lines.** An edit quotes text and a failure points at lines, so
//!   the model needs line numbers to connect the two, and it gets them without
//!   counting.
//! - **Paged in lines.** `offset` and `limit` are line numbers, the default page
//!   is [`DEFAULT_MAX_LINES`], and a page that stops short of the end says which
//!   call reads on. Each line is capped at [`MAX_LINE_CHARS`] characters and the
//!   page at [`MAX_READ_CHARS`], so a minified bundle is one page, not the
//!   conversation.
//! - **Text only.** A file with a NUL byte or invalid UTF-8 is refused with its
//!   size. Base64 of a PNG costs context and says nothing; a model that can see
//!   one is pointed at `view_image`, which shows it the picture.
//! - **Plain text, not JSON.** Escaped newlines and quotes cost tokens and are
//!   harder for the model to read than the file itself.
//! - **The read is recorded.** The file's content hash goes into the run's
//!   state, and an edit or overwrite later refuses a file whose hash has changed
//!   since (TODO §6).

use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use sc_agent::TraitContext;

use super::state::CodingState;
use crate::files::{ARG_PATH, FileScope, config_count, open_at, string_arg};
use sc_api::mcp::arguments;

/// The most lines one call returns, when the admin sets nothing else.
pub const CFG_MAX_LINES: &str = "max_lines";

/// A page, in lines.
pub const DEFAULT_MAX_LINES: u64 = 2000;

/// The longest line returned whole.
pub const MAX_LINE_CHARS: usize = 2000;

/// The most characters one page returns, whatever its line count.
pub const MAX_READ_CHARS: usize = 100_000;

/// How far into a file binary detection looks for a NUL byte.
const BINARY_SNIFF_BYTES: usize = 8000;

/// The first line to return, counting from 1.
const ARG_OFFSET: &str = "offset";
/// How many lines.
const ARG_LIMIT: &str = "limit";

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("read_file_{}", scope.slug())
}

/// The tool this scope's read contributes.
pub fn spec(scope: &FileScope, config: &Attrs) -> ToolSpec {
    let ceiling = config_count(config, CFG_MAX_LINES, DEFAULT_MAX_LINES).unwrap_or(u64::MAX);
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Read a text file, returned as numbered lines, at most {ceiling} per call; page \
             through a longer one with `{ARG_OFFSET}` and `{ARG_LIMIT}`. A file must be read \
             before it is edited or overwritten. Binary files are refused (look at an image \
             with view_image, where offered)."
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATH: {"type": "string", "description": "Relative to the project root, e.g. `src/App.tsx`"},
                ARG_OFFSET: {"type": "integer", "minimum": 1, "description": "First line (default 1)"},
                ARG_LIMIT: {"type": "integer", "minimum": 1, "description": "How many lines to read"},
            },
            "required": [ARG_PATH],
            "additionalProperties": false,
        }),
    )
}

/// Read one file, as the run's caller.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let ceiling = config_count(config, CFG_MAX_LINES, DEFAULT_MAX_LINES)?;
    let args = arguments(args, &[ARG_PATH, ARG_OFFSET, ARG_LIMIT])?;
    let rel = string_arg(&args, ARG_PATH)?;
    let offset = line_arg(&args, ARG_OFFSET)?.unwrap_or(1);
    let limit = line_arg(&args, ARG_LIMIT)?.map_or(ceiling, |l| l.min(ceiling)) as usize;

    let (store, path) = open_at(scope, ctx, &rel).await?;
    if store.stat(&path).await?.is_some_and(|s| s.is_dir) {
        return Err(Error::invalid(format!(
            "`{rel}` is a directory; use `{}` to see what is in it",
            super::find::tool_name(scope)
        )));
    }
    let bytes = store.read(&path).await?;
    let text = as_text(&bytes).ok_or_else(|| {
        Error::invalid(format!(
            "`{rel}` is a binary file ({} bytes) and cannot be read as text{}",
            bytes.len(),
            match image::guess_format(&bytes) {
                // Where the model can see, the tool that shows it one is there.
                Ok(_) => format!(
                    "; an image is looked at with `{}`, where it is offered",
                    super::view_image::tool_name(scope)
                ),
                Err(_) => String::new(),
            }
        ))
    })?;

    let page = render(&rel, text, offset as usize, limit, &tool_name(scope))?;
    let mut state = CodingState::load(ctx.state());
    state.saw(&path, &bytes);
    state.store(ctx.state());
    Ok(Json::String(page))
}

/// A positive line number argument.
fn line_arg(args: &serde_json::Map<String, Json>, name: &str) -> Result<Option<u64>> {
    match args.get(name) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => match n.as_u64() {
            Some(n) if n >= 1 => Ok(Some(n)),
            _ => Err(Error::invalid(format!(
                "`{name}` should be a line number of at least 1, got {n}"
            ))),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{name}` should be a number, got {other}"
        ))),
    }
}

/// The file's text, or `None` when it is binary.
pub fn as_text(bytes: &[u8]) -> Option<&str> {
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// One page of `text` as numbered lines, with a header and, when the page stops
/// before the end, how to read on.
fn render(rel: &str, text: &str, offset: usize, limit: usize, tool: &str) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(format!("{rel} (empty file)"));
    }
    if offset > total {
        return Err(Error::invalid(format!(
            "`{rel}` has {total} lines, so there is no line {offset}"
        )));
    }

    let last_wanted = (offset - 1 + limit).min(total);
    let width = last_wanted.to_string().len();
    let mut body = String::new();
    let mut last = offset - 1;
    for (index, line) in lines[offset - 1..last_wanted].iter().enumerate() {
        let number = offset + index;
        let line = cap_line(line);
        // Always at least one line, so a page can never be empty.
        if number > offset && body.len() + line.len() > MAX_READ_CHARS {
            break;
        }
        body.push_str(&format!("{number:>width$}\t{line}\n"));
        last = number;
    }

    let header = match (offset, last) {
        (1, l) if l == total => format!("{rel} ({total} lines)"),
        (o, l) => format!("{rel} (lines {o}-{l} of {total})"),
    };
    let mut out = format!("{header}\n{body}");
    if last < total {
        out.push_str(&format!(
            "[{} more lines. To continue, call {tool} with offset {}.]",
            total - last,
            last + 1
        ));
    } else {
        out.pop();
    }
    Ok(out)
}

/// A line, capped at [`MAX_LINE_CHARS`] characters.
fn cap_line(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        None => line.to_owned(),
        Some((end, _)) => format!(
            "{}… [line truncated: {} more characters]",
            &line[..end],
            line[end..].chars().count()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_scope() {
        let scope = FileScope {
            store: "app-src".to_owned(),
            root: "web".to_owned(),
        };
        assert_eq!(tool_name(&scope), "read_file_app_src_web");
    }

    #[test]
    fn a_whole_file_is_numbered_lines_under_a_header() {
        let page = render("a.ts", "one\ntwo\n", 1, 2000, "read").unwrap();
        assert_eq!(page, "a.ts (2 lines)\n1\tone\n2\ttwo");
        assert_eq!(
            render("e.ts", "", 1, 10, "read").unwrap(),
            "e.ts (empty file)"
        );
    }

    #[test]
    fn a_page_says_how_to_read_on() {
        let text: String = (1..=12).map(|n| format!("line {n}\n")).collect();
        let page = render("a.ts", &text, 9, 2, "read_file_src").unwrap();
        assert_eq!(
            page,
            "a.ts (lines 9-10 of 12)\n 9\tline 9\n10\tline 10\n\
             [2 more lines. To continue, call read_file_src with offset 11.]"
        );
        let err = render("a.ts", &text, 13, 2, "read")
            .unwrap_err()
            .to_string();
        assert!(err.contains("12 lines"), "{err}");
    }

    #[test]
    fn a_long_line_is_capped_and_so_is_the_page() {
        let long = "x".repeat(MAX_LINE_CHARS + 5);
        let page = render("min.js", &long, 1, 10, "read").unwrap();
        assert!(
            page.ends_with("… [line truncated: 5 more characters]"),
            "{page}"
        );

        // Many lines, each under the cap, together over the page budget.
        let text: String = (0..200)
            .map(|_| format!("{}\n", "y".repeat(1000)))
            .collect();
        let page = render("big.txt", &text, 1, 2000, "read").unwrap();
        assert!(page.len() <= MAX_READ_CHARS + 200, "{}", page.len());
        assert!(
            page.contains("To continue, call read with offset"),
            "{page}"
        );
    }

    #[test]
    fn binary_is_a_nul_byte_or_invalid_utf8() {
        assert_eq!(as_text(b"plain"), Some("plain"));
        assert_eq!(as_text(b"PNG\0\x01"), None);
        assert_eq!(as_text(&[0xff, 0xfe, b'a']), None);
    }
}
