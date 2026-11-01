//! What a build's output *said*, parsed into things a reader can act on.
//!
//! A build that failed is not an exception, it is news — and the useful part of
//! that news is a list of file/line/message triples, not a wall of tool output.
//! Two callers want exactly that list and would otherwise each parse it:
//!
//! - `sc_agent_builtins::build_application`, the agent trait whose whole argument
//!   is that "the build's diagnostics are the tool result";
//! - the MCP projection of `buildApplication` (§13.6), where a coding agent that
//!   just changed a schema rebuilds the application it changed and has to be
//!   told what did not compile.
//!
//! Two copies of these regexes would be two answers to "what did tsc say?", so
//! the parser lives at the layer that owns the build itself. `coding`'s `check`
//! (TODO 6.2) reads its test runners' and linters' output with it too.
//!
//! Anything unrecognised is **not lost**: every caller sends the whole output
//! beside this list, and this is the index into it rather than a replacement
//! for it.

use serde_json::{Value as Json, json};

/// One diagnostic a tool reported: where, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The file, as the tool wrote it.
    pub file: String,
    /// The line, from 1.
    pub line: u32,
    /// The column, from 1.
    pub column: u32,
    /// The message.
    pub message: String,
}

/// The diagnostics a build log names, as JSON objects with `file`, `line`,
/// `column` and `message`.
///
/// The Rust sibling of the IDE's `buildDiagnostics.ts`, understanding the same
/// shapes for the same reason: the type errors exist only in the tools' output,
/// and file/line/message is what a reader — a person in the Problems panel, a
/// model deciding what to edit — can act on.
pub fn build_diagnostics(log: &str) -> Vec<Json> {
    parse_diagnostics(log)
        .iter()
        .map(|d| {
            json!({
                "file": d.file,
                "line": d.line,
                "column": d.column,
                "message": d.message,
            })
        })
        .collect()
}

/// A test that failed, while its message and location are still being read.
struct PendingTest {
    /// The file, when the runner named it before the location.
    file: Option<String>,
    /// The test's name.
    name: Option<String>,
    /// The first line of its failure.
    message: Option<String>,
}

/// The diagnostics a tool's output names, in the order it named them (TODO 6.2).
///
/// Understands:
///
/// - **tsc**, plain (`src/App.tsx(12,5): error TS2322: …`) and pretty
///   (`src/App.tsx:12:5 - error TS2322: …`);
/// - **esbuild**-style one-liners (`src/main.ts:2:1: ERROR: …`), which are also
///   eslint's `unix` and `compact` formats and the generic `path:line:col: message`;
/// - **rolldown**'s boxed report, whose message is the line above the frame;
/// - **eslint**'s default `stylish` format: a file on a line of its own, then
///   `  line:col  error  message  rule` rows;
/// - **vitest**: `FAIL  file > suite > test`, the error's first line, then
///   `❯ file:line:col`;
/// - **jest**: `FAIL file`, `● suite › test`, the error's first line, then the
///   first stack frame outside `node_modules`.
///
/// A failed test becomes one diagnostic whose message is the test's name and
/// the first line of its failure; one whose location is never printed is placed
/// at line 1 of its file.
pub fn parse_diagnostics(log: &str) -> Vec<Diagnostic> {
    let Some(p) = Patterns::new() else {
        // A pattern that does not compile is this module's bug, not the tool's;
        // every caller still has the whole output.
        return Vec::new();
    };

    let mut found: Vec<Diagnostic> = Vec::new();
    let mut previous = String::new();
    // eslint stylish: the file the rows below belong to.
    let mut eslint_file: Option<String> = None;
    // jest: the test file of the `FAIL` line above.
    let mut test_file: Option<String> = None;
    let mut pending: Option<PendingTest> = None;

    for raw in log.lines() {
        let line = strip_ansi(raw);
        let line = line.trim_end();
        let trimmed = line.trim();

        if let Some(c) = p.vitest_fail.captures(line) {
            flush(&mut found, pending.take(), None);
            let file = c[1].to_owned();
            match c.get(2) {
                Some(name) => {
                    pending = Some(PendingTest {
                        file: Some(file),
                        name: Some(name.as_str().trim().to_owned()),
                        message: None,
                    })
                }
                None if c.get(3).is_some() => {
                    pending = Some(PendingTest {
                        file: Some(file),
                        name: None,
                        message: None,
                    })
                }
                None => test_file = Some(file),
            }
            continue;
        }
        if let Some(c) = p.jest_test.captures(line) {
            flush(&mut found, pending.take(), None);
            if !c[1].starts_with("Console") {
                pending = Some(PendingTest {
                    file: test_file.clone(),
                    name: Some(c[1].trim().to_owned()),
                    message: None,
                });
            }
            continue;
        }
        if let Some(test) = pending.as_mut() {
            let location = p
                .vitest_at
                .captures(line)
                .or_else(|| p.stack_frame.captures(line));
            if let Some(c) = location {
                let file = c[1].to_owned();
                let ours = !file.contains("node_modules")
                    && test
                        .file
                        .as_deref()
                        .is_none_or(|t| file == t || file.ends_with(&format!("/{t}")));
                if ours {
                    let at = (file, c[2].parse().unwrap_or(1), c[3].parse().unwrap_or(1));
                    flush(&mut found, pending.take(), Some(at));
                }
                continue;
            }
            if test.message.is_none() && !trimmed.is_empty() && !trimmed.starts_with('⎯') {
                test.message = Some(trimmed.to_owned());
                continue;
            }
        }

        if let Some(c) = p.frame.captures(line) {
            add(&mut found, &c, previous.trim());
            continue;
        }
        if let Some(c) = p.tsc.captures(line) {
            add(&mut found, &c, &format!("{}: {}", &c[4], &c[5]));
            previous.clear();
            continue;
        }
        if let Some(c) = p.tsc_pretty.captures(line) {
            add(&mut found, &c, &format!("{}: {}", &c[4], &c[5]));
            previous.clear();
            continue;
        }
        if let Some(c) = p.positioned.captures(line) {
            add(&mut found, &c, &c[4]);
            previous.clear();
            continue;
        }
        if trimmed.is_empty() {
            eslint_file = None;
            continue;
        }
        if let Some(file) = &eslint_file
            && let Some(c) = p.eslint_row.captures(line)
        {
            let message = match c.get(5) {
                Some(rule) => format!("{} ({})", c[4].trim(), rule.as_str()),
                None => c[4].trim().to_owned(),
            };
            push(
                &mut found,
                Diagnostic {
                    file: file.clone(),
                    line: c[1].parse().unwrap_or(1),
                    column: c[2].parse().unwrap_or(1),
                    message,
                },
            );
            continue;
        }
        if p.eslint_file.is_match(line) {
            eslint_file = Some(line.to_owned());
            continue;
        }
        previous = trimmed.to_owned();
    }
    flush(&mut found, pending, None);
    found
}

/// The patterns [`parse_diagnostics`] reads with.
struct Patterns {
    tsc: regex_lite::Regex,
    tsc_pretty: regex_lite::Regex,
    positioned: regex_lite::Regex,
    frame: regex_lite::Regex,
    eslint_file: regex_lite::Regex,
    eslint_row: regex_lite::Regex,
    vitest_fail: regex_lite::Regex,
    vitest_at: regex_lite::Regex,
    jest_test: regex_lite::Regex,
    stack_frame: regex_lite::Regex,
}

impl Patterns {
    fn new() -> Option<Patterns> {
        let re = |pattern: &str| regex_lite::Regex::new(pattern).ok();
        Some(Patterns {
            // `src/App.tsx(12,5): error TS2322: Type 'x' is not assignable…`
            tsc: re(r"^(\S[^(]*)\((\d+),(\d+)\):\s*(?:error|warning)\s+([A-Za-z]+\d+):\s*(.+)$")?,
            // `src/App.tsx:12:5 - error TS2322: Type 'x' is not assignable…`
            tsc_pretty: re(
                r"^\s*([^\s:]+\.[A-Za-z0-9]+):(\d+):(\d+)\s+-\s+(?:error|warning)\s+([A-Za-z]+\d+):\s*(.+)$",
            )?,
            // `src/App.tsx:12:5: ERROR: Expected ";"` — esbuild and its imitators.
            positioned: re(
                r"^\s*(?:\[[^\]]*\]\s*)?([^\s:]+\.[A-Za-z0-9]+):(\d+):(\d+):\s*(?:(?:ERROR|WARNING|error|warning):\s*)?(\S.*)$",
            )?,
            // `╭─[ src/main.ts:2:1 ]` — rolldown.
            frame: re(r"[╭┌][─-]*\[\s*([^\s\]]+?):(\d+):(\d+)\s*\]")?,
            // `/srv/app/src/App.tsx` alone on its line — eslint stylish.
            eslint_file: re(r"^[^\s:]*[^\s:]\.(?:[cm]?[jt]sx?|vue|svelte|astro)$")?,
            // `  12:5  error  'x' is never used  no-unused-vars`
            eslint_row: re(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(\S+))?$")?,
            // ` FAIL  src/App.test.tsx > App > renders` (vitest), ` FAIL  src/a.test.ts [ src/a.test.ts ]`
            // (vitest, a whole file) and `FAIL src/App.test.js` (jest).
            vitest_fail: re(r"^\s*FAIL\s+(\S+)(?:\s+>\s+(.+)|\s+(\[.*\]))?\s*$")?,
            // ` ❯ src/App.test.tsx:8:20`
            vitest_at: re(r"^\s*❯\s+(\S+?):(\d+):(\d+)\s*$")?,
            // `  ● App › renders the title`
            jest_test: re(r"^\s*●\s+(.+)$")?,
            // `      at Object.toBe (src/App.test.js:7:17)`
            stack_frame: re(r"^\s*at\s+(?:.*\()?([^\s()]+?):(\d+):(\d+)\)?\s*$")?,
        })
    }
}

/// Finish a failed test: one diagnostic at its location, or at line 1 of its
/// file when none was printed.
fn flush(found: &mut Vec<Diagnostic>, test: Option<PendingTest>, at: Option<(String, u32, u32)>) {
    let Some(test) = test else {
        return;
    };
    let (file, line, column) = match (at, test.file) {
        (Some(at), _) => at,
        (None, Some(file)) => (file, 1, 1),
        (None, None) => return,
    };
    let message = match (test.name, test.message) {
        (Some(name), Some(message)) => format!("{name}: {message}"),
        (Some(name), None) => format!("{name}: failed"),
        (None, Some(message)) => message,
        (None, None) => "failed".to_owned(),
    };
    push(
        found,
        Diagnostic {
            file,
            line,
            column,
            message,
        },
    );
}

/// Record one diagnostic from a match whose groups 1–3 are file, line, column.
fn add(found: &mut Vec<Diagnostic>, caps: &regex_lite::Captures<'_>, message: &str) {
    push(
        found,
        Diagnostic {
            file: caps[1].to_owned(),
            line: caps[2].parse().unwrap_or(1),
            column: caps[3].parse().unwrap_or(1),
            message: message.trim().to_owned(),
        },
    );
}

/// Record one diagnostic, skipping a duplicate and one with nothing to say.
fn push(found: &mut Vec<Diagnostic>, diagnostic: Diagnostic) {
    if diagnostic.message.is_empty() {
        return;
    }
    if !found.contains(&diagnostic) {
        found.push(diagnostic);
    }
}

/// Terminal colour, which a build that thought it had a TTY leaves behind.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tscs_diagnostics_are_parsed_with_their_file_and_line() {
        let log = "\
build command `npm run build` failed in /srv/store/web with exit status: 2
src/App.tsx(12,5): error TS2322: Type 'number' is not assignable to type 'string'.
src/App.tsx(19,1): error TS1005: ';' expected.
";
        let found = build_diagnostics(log);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["file"], "src/App.tsx");
        assert_eq!(found[0]["line"], 12);
        assert_eq!(found[0]["column"], 5);
        assert!(
            found[0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("not assignable"),
            "{found:?}"
        );
    }

    #[test]
    fn esbuilds_one_line_form_is_parsed_too_and_colour_is_ignored() {
        let log = "\u{1b}[31msrc/main.ts:2:1: ERROR: Expected \";\" but found \"}\"\u{1b}[0m";
        let found = build_diagnostics(log);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["file"], "src/main.ts");
        assert_eq!(found[0]["line"], 2);
        assert_eq!(found[0]["message"], "Expected \";\" but found \"}\"");
    }

    #[test]
    fn a_boxed_report_takes_its_message_from_the_line_above_it() {
        let log = "\
[builtin:vite-transform] 'export' modifier cannot be used here.
   ╭─[ src/main.ts:2:1 ]
";
        let found = build_diagnostics(log);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["file"], "src/main.ts");
        assert!(
            found[0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("cannot be used here"),
            "{found:?}"
        );
    }

    /// `(file, line, column, message)` for each diagnostic, for comparing.
    fn found(log: &str) -> Vec<(String, u32, u32, String)> {
        parse_diagnostics(log)
            .into_iter()
            .map(|d| (d.file, d.line, d.column, d.message))
            .collect()
    }

    fn d(file: &str, line: u32, column: u32, message: &str) -> (String, u32, u32, String) {
        (file.to_owned(), line, column, message.to_owned())
    }

    #[test]
    fn tscs_pretty_form_is_parsed() {
        let log = "\
src/App.tsx:12:5 - error TS2322: Type 'number' is not assignable to type 'string'.

12     const title: string = 1;
       ~~~~~

Found 1 error in src/App.tsx:12
";
        assert_eq!(
            found(log),
            [d(
                "src/App.tsx",
                12,
                5,
                "TS2322: Type 'number' is not assignable to type 'string'."
            )]
        );
    }

    #[test]
    fn eslints_stylish_output_is_parsed_with_its_rule() {
        let log = "\
> p@1.0.0 lint
> eslint src

/srv/apps/web/src/App.tsx
   3:10  error    'unused' is assigned a value but never used  @typescript-eslint/no-unused-vars
  12:1   warning  Unexpected console statement                 no-console

/srv/apps/web/src/main.ts
  1:1  error  Parsing error: Unexpected token

✖ 3 problems (2 errors, 1 warning)
";
        assert_eq!(
            found(log),
            [
                d(
                    "/srv/apps/web/src/App.tsx",
                    3,
                    10,
                    "'unused' is assigned a value but never used (@typescript-eslint/no-unused-vars)"
                ),
                d(
                    "/srv/apps/web/src/App.tsx",
                    12,
                    1,
                    "Unexpected console statement (no-console)"
                ),
                d(
                    "/srv/apps/web/src/main.ts",
                    1,
                    1,
                    "Parsing error: Unexpected token"
                ),
            ]
        );
    }

    #[test]
    fn eslints_unix_format_is_the_generic_one() {
        let log = "src/App.tsx:3:10: 'unused' is assigned a value but never used. [Error/no-unused-vars]\n";
        assert_eq!(
            found(log),
            [d(
                "src/App.tsx",
                3,
                10,
                "'unused' is assigned a value but never used. [Error/no-unused-vars]"
            )]
        );
    }

    #[test]
    fn a_vitest_failure_is_the_test_name_its_error_and_its_location() {
        let log = "\
 RUN  v1.6.0 /srv/apps/web

 ❯ src/App.test.tsx (2 tests | 1 failed) 12ms
   ❯ src/App.test.tsx > App > renders the title
     → expected 'Todo' to be 'Todos' // Object.is equality

⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  src/App.test.tsx > App > renders the title
AssertionError: expected 'Todo' to be 'Todos' // Object.is equality
 ❯ src/App.test.tsx:8:27
      6|   it('renders the title', () => {
      7|     render(<App />);
      8|     expect(title()).toBe('Todos');
       |                           ^

⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯

 FAIL  src/broken.test.ts [ src/broken.test.ts ]
Error: Failed to resolve import \"./nope\" from \"src/broken.test.ts\".

 Test Files  2 failed (2)
      Tests  1 failed | 1 passed (2)
";
        assert_eq!(
            found(log),
            [
                d(
                    "src/App.test.tsx",
                    8,
                    27,
                    "App > renders the title: AssertionError: expected 'Todo' to be 'Todos' // Object.is equality"
                ),
                d(
                    "src/broken.test.ts",
                    1,
                    1,
                    "Error: Failed to resolve import \"./nope\" from \"src/broken.test.ts\"."
                ),
            ]
        );
    }

    #[test]
    fn a_jest_failure_takes_its_location_from_the_first_frame_outside_node_modules() {
        let log = "\
FAIL src/App.test.js
  App
    ✕ renders the title (5 ms)
    ✓ counts (1 ms)

  ● App › renders the title

    expect(received).toBe(expected) // Object.is equality

    Expected: \"Todos\"
    Received: \"Todo\"

       6 |   it('renders the title', () => {
    >  7 |     expect(title()).toBe('Todos');
         |                     ^

      at Object.toBe (node_modules/expect/build/index.js:1:1)
      at Object.<anonymous> (/srv/apps/web/src/App.test.js:7:21)

Tests:       1 failed, 1 passed, 2 total
";
        assert_eq!(
            found(log),
            [d(
                "/srv/apps/web/src/App.test.js",
                7,
                21,
                "App › renders the title: expect(received).toBe(expected) // Object.is equality"
            )]
        );
    }

    #[test]
    fn output_with_no_diagnostics_in_it_produces_none() {
        assert!(
            build_diagnostics("vite v5.0.0 building for production...\n✓ 34 modules").is_empty()
        );
    }
}
