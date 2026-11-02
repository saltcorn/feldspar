//! A review grep that runs as a test (code-quality review §4.1, TODO Q1.4): no code outside
//! [`sc_types::Role`] compares role numbers itself.
//!
//! Roles are stored and sent as integers where lower means more privileged, and that inverted
//! order is what made `role <= min_role` a footgun. `Role` takes the comparison away — it has no
//! `Ord`, and `meets` / `outranks` / `is_admin` are the only tests — but nothing stops a new field
//! from being declared `role: u8` again, or a role from being unwrapped with `.get()` and compared
//! by hand. This test scans every crate's Rust source, with comments and string literals removed
//! (formulas such as `"user.role <= 60"` are the formula language's business), and fails on:
//!
//! - a role-named binding, field or parameter typed `u8` / `Option<u8>`;
//! - a role-named expression compared with a numeric literal (`role == 1`, `min_role <= 40`);
//! - a role number taken out of a `Role` and ordered by hand (`role.get() < x`,
//!   `n > i64::from(caller.role)`);
//! - a `ROLE_ADMIN` / `ROLE_PUBLIC` constant: `Role::ADMIN` / `Role::PUBLIC` are the only ones.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regex_lite::Regex;

/// The only file allowed to look inside a role.
const ALLOWED: &[&str] = &["crates/sc-types/src/role.rs"];

struct Rule {
    why: &'static str,
    pattern: Regex,
}

fn rules() -> Vec<Rule> {
    let rule = |why, pattern: &str| Rule {
        why,
        pattern: Regex::new(pattern).unwrap(),
    };
    vec![
        rule(
            "a role typed as a bare number: use `sc_types::Role`",
            r"(?i)\b\w*role\w*\s*:\s*(Option\s*<\s*)?u8\b",
        ),
        rule(
            "a role compared with a number literal: use `Role::meets`, `outranks`, \
             `is_admin` or `is_public`",
            r"(?i)\b\w*role\w*(\(\))?\s*(<=|>=|==|!=|<|>)\s*\d",
        ),
        rule(
            "a number literal compared with a role: use `Role::meets`, `outranks`, \
             `is_admin` or `is_public`",
            // Not a tuple field (`u.2 == user.role`) or the tail of a name.
            r"(?i)(^|[^\w.])\d+\s*(<=|>=|==|!=|<|>)\s*[\w.]*role\w*\b",
        ),
        rule(
            "a role number ordered by hand: use `Role::meets` or `outranks`",
            r"(?i)\brole\w*(\(\))?\.get\(\)\s*(<=|>=|<|>)",
        ),
        rule(
            "a role number ordered by hand: use `Role::meets` or `outranks`",
            r"(?i)(<=|>=|<|>)\s*(i64|u64|i32|u32|u8)::from\([\w.]*role\w*\)",
        ),
        rule(
            "a copy of a role constant: use `Role::ADMIN` / `Role::PUBLIC`",
            r"\bconst\s+ROLE_(ADMIN|PUBLIC)\b",
        ),
    ]
}

/// `source` with comments and string / char literals blanked out (newlines kept, so line numbers
/// still match).
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let blank = |out: &mut String, text: &str| {
        out.extend(text.chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
    };
    let mut i = 0;
    while i < bytes.len() {
        let rest = &source[i..];
        if rest.starts_with("//") {
            let end = rest.find('\n').unwrap_or(rest.len());
            blank(&mut out, &rest[..end]);
            i += end;
        } else if let Some(after) = rest.strip_prefix("/*") {
            let end = after.find("*/").map_or(rest.len(), |e| e + 4);
            blank(&mut out, &rest[..end]);
            i += end;
        } else if let Some(len) = raw_string_len(rest) {
            blank(&mut out, &rest[..len]);
            i += len;
        } else if rest.starts_with('"') || rest.starts_with("b\"") {
            let start = usize::from(rest.starts_with('b')) + 1;
            let mut j = start;
            let rb = rest.as_bytes();
            while j < rb.len() && rb[j] != b'"' {
                j += if rb[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(rest.len());
            blank(&mut out, &rest[..end]);
            i += end;
        } else if let Some(len) = char_literal_len(rest) {
            blank(&mut out, &rest[..len]);
            i += len;
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// The length of a raw string literal (`r"…"`, `r#"…"#`, `br"…"`) starting `rest`.
fn raw_string_len(rest: &str) -> Option<usize> {
    let after_b = rest.strip_prefix('b').unwrap_or(rest);
    let body = after_b.strip_prefix('r')?;
    let hashes = body.len() - body.trim_start_matches('#').len();
    let body = &body[hashes..];
    if !body.starts_with('"') {
        return None;
    }
    let close = format!("\"{}", "#".repeat(hashes));
    let prefix = rest.len() - body.len();
    let end = body[1..].find(&close)? + 1 + close.len();
    Some(prefix + end)
}

/// The length of a char literal (`'a'`, `'\n'`, `'\''`) starting `rest`, not a lifetime.
fn char_literal_len(rest: &str) -> Option<usize> {
    let b = rest.as_bytes();
    if b.first() != Some(&b'\'') {
        return None;
    }
    if b.get(1) == Some(&b'\\') {
        let close = rest[2..].find('\'')? + 2;
        return Some(close + 1);
    }
    let c = rest[1..].chars().next()?;
    let after = 1 + c.len_utf8();
    (rest.as_bytes().get(after) == Some(&b'\'')).then_some(after + 1)
}

/// Every offence in `source` as `(line, why, text)`.
fn offences(source: &str) -> Vec<(usize, &'static str, String)> {
    let rules = rules();
    let mut found = Vec::new();
    for (n, line) in code_only(source).lines().enumerate() {
        // Every rule needs the word: skip the regexes on the lines without it.
        if !line.to_ascii_lowercase().contains("role") {
            continue;
        }
        for rule in &rules {
            if let Some(m) = rule.pattern.find(line) {
                found.push((n + 1, rule.why, m.as_str().to_owned()));
            }
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target"
                && name != "node_modules"
                && !name.to_string_lossy().starts_with('.')
            {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_code_outside_role_compares_role_numbers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 100, "found only {} source files", files.len());
    let mut report = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.contains(&relative.as_str()) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (line, why, text) in offences(&source) {
            report.push(format!("{relative}:{line}: `{text}` — {why}"));
        }
    }
    assert!(
        report.is_empty(),
        "role numbers compared outside `sc_types::Role`:\n{}",
        report.join("\n")
    );
}

#[test]
fn the_guard_catches_what_it_is_for_and_ignores_comments_and_strings() {
    for bad in [
        "pub role: u8,",
        "fn f(min_role: Option<u8>) {}",
        "if user.role == 1 {",
        "if role <= 40 {",
        "if 100 > caller.role {",
        "if role.get() < floor.get() {",
        "Some(n) if n > i64::from(caller.role) => {}",
        "const ROLE_ADMIN: Role = Role::ADMIN;",
    ] {
        assert!(!offences(bad).is_empty(), "missed: {bad}");
    }
    for fine in [
        "pub role: Role,",
        "if user.role.meets(table.min_role) {",
        "if caller.role.is_admin() {",
        "roles.sort_by_key(|r| r.role.get());",
        "// a role <= 40 in a comment",
        r#"where_sql("user.role <= 60", &env);"#,
        r##"let js = r#"if (user.role == 1) {}"#;"##,
        "let bytes: u8 = 3; let c = '\"'; let roles = 2 > 1;",
        "fn f<'a>(x: &'a str) {}",
        "out.iter().any(|u| u.2 == user.role)",
    ] {
        assert!(
            offences(fine).is_empty(),
            "flagged: {fine} -> {:?}",
            offences(fine)
        );
    }
}
