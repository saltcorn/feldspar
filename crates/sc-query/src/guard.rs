//! What admin-authored SQL may not do (TODO-type-hardening H8.3, H8.5;
//! TECHNICAL_DESIGN §7.3a).
//!
//! A custom SQL query (§13.4) and a code body's `db.sql()` run their text as
//! written, inside a transaction whose caller settings (`sc.role`, `sc.user`, …)
//! are what row-level security policies read. Agents author that text as well as
//! people, and a statement that sets those settings itself — `set_config('sc.role',
//! '1', true)` in a CTE is enough — would make every policy believe it. So the
//! text is read here, at save and again at run, and refused when it:
//!
//! - **calls `set_config`**, names **`pg_settings`** (an `UPDATE` of which is
//!   `set_config`), or calls a function that runs SQL text (`query_to_xml` and
//!   its kin, `dblink`), anywhere in the statement;
//! - **is** a `SET`, `RESET` or `DO`, or creates, alters or drops a function or
//!   a procedure, or alters a role, a user, a database or the system (`ALTER …
//!   SET` is a setting with a longer life);
//! - **reshapes row-level security** (H8.5): `DISABLE` / `NO FORCE ROW LEVEL
//!   SECURITY`, disabling or dropping a trigger (the `old` check of H7.4 is one),
//!   or creating, altering or dropping a policy — a new permissive `USING (true)`
//!   widens as well as a dropped one.
//!
//! The reading is a scanner, not a parser, and it is the safe kind of
//! approximate: it knows every way SQL hides text from a reader — literals
//! (`'…'`, `E'…'` with backslash escapes, `$tag$…$tag$`), quoted identifiers,
//! nested block comments — and skips only what cannot be code. A Unicode-escaped
//! identifier or literal (`U&"…"`) is refused outright, because it can spell a
//! name this reader would not recognise. Words are compared case-insensitively
//! when unquoted and exactly when quoted, as Postgres resolves them.

use sc_error::{Error, Result};

/// The end of every refusal of a change to a table's access rules made by
/// anything but a person (TODO-type-hardening H8.5, §7.3a): it says who may make
/// it and what to do instead, and never suggests turning row-level security off.
pub const PERSON_ONLY: &str = "can only be changed by a person in the admin UI. Tell the user \
                               what change you need and why.";

/// The refusal of a change to `table`'s access rules by anything but a
/// person (TODO-type-hardening H8.5): "`<table>` has row-level security, so its
/// access rules [`PERSON_ONLY`]".
pub fn rls_refusal(table: &str) -> Error {
    Error::invalid(format!(
        "`{table}` has row-level security, so its access rules {PERSON_ONLY}"
    ))
}

/// Refuse `sql` if it could forge the caller context the policies read, or
/// reshape row-level security (see the module documentation). `what` names the
/// statement in the refusal ("custom query `x`", "`db.sql()`").
pub fn check_admin_sql(sql: &str, what: &str) -> Result<()> {
    let statements = scan(sql).map_err(|e| Error::invalid(format!("{what}: {e}")))?;
    for words in &statements {
        match refusal(words) {
            None => {}
            Some(Refusal::Forges(reason)) => {
                return Err(Error::invalid(format!(
                    "{what} is refused because it {reason}. A statement here runs inside the \
                     transaction whose settings tell row-level security who is asking, so it \
                     may not change them"
                )));
            }
            Some(Refusal::Reshapes(Some(table))) => return Err(rls_refusal(&table)),
            Some(Refusal::Reshapes(None)) => {
                return Err(Error::invalid(format!(
                    "{what} would change row-level security, and its access rules {PERSON_ONLY}"
                )));
            }
        }
    }
    Ok(())
}

/// Why a statement is refused.
enum Refusal {
    /// It could forge the caller context: the reason, as a clause.
    Forges(String),
    /// It reshapes row-level security, on the table named when the statement
    /// names one.
    Reshapes(Option<String>),
}

/// One word of SQL: an identifier or keyword, lowercased unless it was quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    text: String,
    quoted: bool,
}

impl Word {
    fn is(&self, keyword: &str) -> bool {
        !self.quoted && self.text == keyword
    }
}

/// Functions that call `set_config` or run SQL text handed to them.
const FORBIDDEN_NAMES: [&str; 8] = [
    "set_config",
    "pg_settings",
    "query_to_xml",
    "query_to_xmlschema",
    "query_to_xml_and_xmlschema",
    "cursor_to_xml",
    "dblink",
    "dblink_exec",
];

/// Why a statement's words are refused, or `None`.
fn refusal(words: &[Word]) -> Option<Refusal> {
    if let Some(name) = words
        .iter()
        .find(|w| FORBIDDEN_NAMES.contains(&w.text.as_str()))
    {
        return Some(Refusal::Forges(match name.text.as_str() {
            "set_config" => "calls `set_config`".to_owned(),
            "pg_settings" => "names `pg_settings`".to_owned(),
            other => format!("calls `{other}`, which runs SQL text"),
        }));
    }
    let first = words.first()?;
    let rest = &words[1..];
    let has = |phrase: &[&str]| {
        rest.windows(phrase.len())
            .any(|w| w.iter().zip(phrase).all(|(word, k)| word.is(k)))
    };
    // The object a CREATE / ALTER / DROP is about: the first word past the
    // modifiers that may stand before it.
    let object = rest.iter().position(|w| {
        ![
            "or",
            "replace",
            "temp",
            "temporary",
            "trusted",
            "procedural",
        ]
        .iter()
        .any(|m| w.is(m))
    });
    // The table a statement names: past `TABLE` (and `IF EXISTS`, `ONLY`), or
    // past `ON` for a policy or a trigger.
    let table_after = |keyword: &str| {
        let at = rest.iter().position(|w| w.is(keyword))?;
        rest[at + 1..]
            .iter()
            .find(|w| !(w.is("if") || w.is("exists") || w.is("only")))
            .map(|w| w.text.clone())
    };
    if first.is("set") || first.is("reset") {
        return Some(Refusal::Forges(format!(
            "is a `{}` statement",
            first.text.to_uppercase()
        )));
    }
    if first.is("do") {
        return Some(Refusal::Forges("is a `DO` block".to_owned()));
    }
    if (first.is("create") || first.is("alter") || first.is("drop"))
        && let Some(object) = object.and_then(|at| rest.get(at))
    {
        let verb = first.text.to_uppercase();
        for kind in ["function", "procedure"] {
            if object.is(kind) {
                return Some(Refusal::Forges(format!(
                    "is a `{verb} {}`",
                    kind.to_uppercase()
                )));
            }
        }
        if object.is("policy") || (first.is("drop") && object.is("trigger")) {
            return Some(Refusal::Reshapes(table_after("on")));
        }
        if first.is("alter") {
            for kind in ["role", "user", "database", "system"] {
                if object.is(kind) {
                    return Some(Refusal::Forges(format!(
                        "is an `ALTER {}`",
                        kind.to_uppercase()
                    )));
                }
            }
        }
    }
    if first.is("alter")
        && (has(&["disable", "row", "level", "security"])
            || has(&["no", "force", "row", "level", "security"])
            || has(&["disable", "trigger"]))
    {
        return Some(Refusal::Reshapes(table_after("table")));
    }
    None
}

/// The words of each `;`-separated statement in `sql`.
fn scan(sql: &str) -> std::result::Result<Vec<Vec<Word>>, String> {
    let b = sql.as_bytes();
    let mut statements: Vec<Vec<Word>> = vec![Vec::new()];
    let mut i = 0;
    let push = |statements: &mut Vec<Vec<Word>>, word: Word| {
        if let Some(last) = statements.last_mut() {
            last.push(word);
        }
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b';' => {
                statements.push(Vec::new());
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = block_comment_end(b, i).ok_or("an unterminated block comment")?;
            }
            b'\'' => {
                i = quoted_end(b, i, b'\'', false).ok_or("an unterminated string literal")?;
            }
            b'"' => {
                let end =
                    quoted_end(b, i, b'"', false).ok_or("an unterminated quoted identifier")?;
                let text = sql
                    .get(i + 1..end - 1)
                    .unwrap_or_default()
                    .replace("\"\"", "\"");
                push(&mut statements, Word { text, quoted: true });
                i = end;
            }
            b'$' => match dollar_tag(b, i) {
                Some(tag) => {
                    let body = i + tag.len();
                    i = find(b, body, tag.as_bytes())
                        .ok_or("an unterminated dollar-quoted body")?
                        + tag.len();
                }
                None => i += 1,
            },
            c if is_ident_start(c) => {
                let start = i;
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
                let word = &sql[start..i];
                // A prefixed literal: `E'…'` takes backslash escapes, and a
                // Unicode escape (`U&'…'`, `U&"…"`) can spell any name.
                match (word, b.get(i)) {
                    ("e" | "E", Some(b'\'')) => {
                        i = quoted_end(b, i, b'\'', true)
                            .ok_or("an unterminated string literal")?;
                        continue;
                    }
                    ("u" | "U", Some(b'&')) if matches!(b.get(i + 1), Some(b'\'' | b'"')) => {
                        return Err(
                            "a Unicode-escaped name or literal (`U&`), which this server does \
                             not accept in admin SQL"
                                .to_owned(),
                        );
                    }
                    _ => {}
                }
                push(
                    &mut statements,
                    Word {
                        text: word.to_ascii_lowercase(),
                        quoted: false,
                    },
                );
            }
            _ => i += 1,
        }
    }
    statements.retain(|words| !words.is_empty());
    Ok(statements)
}

/// An identifier's first character: a letter, `_`, or any non-ASCII byte
/// (Postgres takes letters beyond ASCII in identifiers).
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

/// The index just past a quoted run starting at `start`, honouring the
/// doubled-quote escape and, for an `E'…'` literal, the backslash one.
fn quoted_end(b: &[u8], start: usize, quote: u8, backslash: bool) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// The dollar-quote tag opening at `start` (`$$`, `$fn$`), or `None` for a
/// positional placeholder or an operator.
fn dollar_tag(b: &[u8], start: usize) -> Option<String> {
    let mut i = start + 1;
    if b.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
        i += 1;
    }
    (b.get(i) == Some(&b'$')).then(|| String::from_utf8_lossy(&b[start..=i]).into_owned())
}

/// The index just past a (nesting) block comment starting at `start`.
fn block_comment_end(b: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    while i + 1 < b.len() {
        if b[i] == b'/' && b[i + 1] == b'*' {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b[i + 1] == b'/' {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    None
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || b.len() < needle.len() {
        return None;
    }
    (from..=b.len() - needle.len()).find(|&i| b.get(i..i + needle.len()) == Some(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(sql: &str) -> String {
        check_admin_sql(sql, "this SQL").expect_err(sql).to_string()
    }

    #[test]
    fn a_set_config_in_a_cte_is_refused() {
        let message =
            refused("WITH x AS (SELECT set_config('sc.role','1',true)) SELECT * FROM t, x");
        assert!(message.contains("set_config"), "{message}");
    }

    #[test]
    fn every_spelling_of_set_config_is_refused() {
        for sql in [
            "select SET_CONFIG('sc.role', '1', true)",
            "select pg_catalog.set_config('sc.role', '1', true)",
            r#"select "set_config"('sc.role', '1', true)"#,
            "select /* a comment */ set_config/**/('sc.user', '{}', true)",
            "update pg_settings set setting = '1' where name = 'sc.role'",
            "select query_to_xml('select set_config(''sc.role'', ''1'', true)', true, true, '')",
            // A backslash-escaped quote does not hide what follows it.
            r"select E'\'', set_config('sc.role', '1', true)",
            "select U&\"\\0073et_config\"('sc.role', '1', true)",
        ] {
            refused(sql);
        }
    }

    #[test]
    fn set_reset_do_and_functions_are_refused() {
        for sql in [
            "SET sc.role = '1'",
            "set local sc.role to 1",
            "  -- leading comment\n RESET sc.role",
            "DO $$ BEGIN PERFORM 1; END $$",
            "create function f() returns int language sql as $$ select 1 $$",
            "CREATE OR REPLACE FUNCTION f() RETURNS int AS 'select 1' LANGUAGE sql",
            "create procedure p() language sql as $$ select 1 $$",
            "alter function f() security definer",
            "drop function if exists _fd_owner_old_books() cascade",
            "alter role me set sc.role = '1'",
            "ALTER DATABASE x SET sc.role = '1'",
        ] {
            refused(sql);
        }
    }

    #[test]
    fn reshaping_row_level_security_is_refused_with_the_person_only_reply() {
        let message = refused("ALTER TABLE books DISABLE ROW LEVEL SECURITY");
        assert_eq!(
            message,
            rls_refusal("books").to_string(),
            "the reply names the table and who may change it"
        );
        assert!(refused("drop policy sc_owner_select on if exists books").contains("`books`"));
        for sql in [
            "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
            "alter table if exists only t no force row level security",
            "ALTER TABLE t DISABLE TRIGGER sc_owner_update_old",
            "drop trigger sc_owner_update_old on t",
            "CREATE POLICY open ON t USING (true)",
            "alter policy sc_owner_select on t using (true)",
            "DROP POLICY sc_owner_select ON t",
        ] {
            assert!(refused(sql).contains(PERSON_ONLY), "{sql}");
        }
    }

    #[test]
    fn ordinary_sql_and_text_that_only_mentions_the_words_is_accepted() {
        for sql in [
            "select title from books where pages > $1",
            "update books set title = 'x' where id = 1",
            "with t as (select 1 as n) select n from t",
            "select 'set_config' as word, 'SET sc.role' as note",
            "select $tag$ set_config('sc.role') $tag$ as body",
            "select current_setting('sc.role', true)",
            "-- set_config in a comment\nselect 1",
            "insert into notes (what) values ('do it') on conflict do nothing",
            "select \"Set_Config\" from odd_columns",
            "alter table t add column c text",
            "alter table t enable row level security",
            "create table x (id int)",
            "select x::text from t",
        ] {
            check_admin_sql(sql, "this SQL").unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    }

    #[test]
    fn every_statement_is_read() {
        refused("select 1; set sc.role = '1'");
    }
}
