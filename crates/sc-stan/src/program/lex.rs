//! The lexer: a Stan source file as a list of tokens (TODO §5).
//!
//! Deliberately narrow, like the parser it feeds. It knows what it has to know
//! to find the blocks and the top-level declarations and to skip everything
//! else **without being fooled** — comments, string literals (a `}` inside
//! `print("}")` must not close a block), and `#include` — and nothing more.
//! Operators are recognised only well enough that a statement can be skipped to
//! its `;`; whether the program is valid Stan is `stanc`'s question.
//!
//! Every token carries the file it came from and its line and column, because
//! every sentence the parser produces names a place in the program.

use sc_error::{Error, Result};

/// What a token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    /// An identifier or a keyword — the lexer does not tell them apart.
    Ident(String),
    /// An integer literal, as written.
    Int(String),
    /// A real (or imaginary) literal, as written.
    Real(String),
    /// A string literal's contents.
    Str(String),
    /// An operator or a punctuation mark.
    Punct(&'static str),
    /// `#include <path>`: the path as written, not yet resolved.
    Include(String),
}

/// One token and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: Tok,
    /// The index of the file in the program it came from.
    pub file: usize,
    /// 1-based.
    pub line: u32,
    /// 1-based, in characters.
    pub col: u32,
    /// The byte span in that file's text.
    pub start: usize,
    pub end: usize,
}

impl Token {
    /// Whether this is the punctuation `p`.
    pub fn is(&self, p: &str) -> bool {
        matches!(&self.kind, Tok::Punct(q) if *q == p)
    }

    /// The identifier, if it is one.
    pub fn ident(&self) -> Option<&str> {
        match &self.kind {
            Tok::Ident(s) => Some(s),
            _ => None,
        }
    }

    /// Whether this is the identifier (or keyword) `word`.
    pub fn is_word(&self, word: &str) -> bool {
        self.ident() == Some(word)
    }
}

/// Operators, longest first so that `%/%` is not read as `%`, `/`, `%`.
const PUNCT: &[&str] = &[
    "%/%", ".*=", "./=", ".*", "./", ".^", "<=", ">=", "==", "!=", "&&", "||", "+=", "-=", "*=",
    "/=", "<-", "%", "\\", "^", "+", "-", "*", "/", "<", ">", "=", "!", "?", ":", ";", ",", "(",
    ")", "[", "]", "{", "}", "|", "~", "'", ".",
];

/// Lex `text`, the file at index `file` whose store path is `path` (for
/// sentences).
pub(crate) fn lex(text: &str, file: usize, path: &str) -> Result<Vec<Token>> {
    Lexer {
        text,
        bytes: text.as_bytes(),
        pos: 0,
        line: 1,
        line_start: 0,
        file,
        path,
        out: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    line_start: usize,
    file: usize,
    path: &'a str,
    out: Vec<Token>,
}

impl Lexer<'_> {
    fn run(mut self) -> Result<Vec<Token>> {
        while let Some(c) = self.peek_char() {
            let start = self.pos;
            let (line, col) = (self.line, self.col(start));
            if c == '\n' {
                self.newline();
                continue;
            }
            if c.is_whitespace() {
                self.pos += c.len_utf8();
                continue;
            }
            if self.rest().starts_with("//") {
                self.skip_line();
                continue;
            }
            if self.rest().starts_with("/*") {
                self.block_comment(line)?;
                continue;
            }
            let kind = if c == '#' {
                self.hash(line)?
            } else if c == '"' {
                self.string(line)?
            } else if c.is_ascii_digit()
                || (c == '.' && self.bytes.get(start + 1).is_some_and(u8::is_ascii_digit))
            {
                self.number()
            } else if c.is_alphabetic() || c == '_' {
                self.ident()
            } else if let Some(p) = PUNCT.iter().find(|p| self.rest().starts_with(**p)) {
                self.pos += p.len();
                Tok::Punct(p)
            } else {
                return Err(self.error(line, col, format!("unexpected character `{c}`")));
            };
            self.out.push(Token {
                kind,
                file: self.file,
                line,
                col,
                start,
                end: self.pos,
            });
        }
        Ok(self.out)
    }

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn col(&self, at: usize) -> u32 {
        self.text[self.line_start..at].chars().count() as u32 + 1
    }

    fn newline(&mut self) {
        self.pos += 1;
        self.line += 1;
        self.line_start = self.pos;
    }

    fn skip_line(&mut self) {
        let len = self.rest().find('\n').unwrap_or(self.rest().len());
        self.pos += len;
    }

    fn block_comment(&mut self, line: u32) -> Result<()> {
        self.pos += 2;
        loop {
            if self.rest().starts_with("*/") {
                self.pos += 2;
                return Ok(());
            }
            match self.peek_char() {
                None => {
                    return Err(Error::invalid(format!(
                        "{}:{line}: a `/*` comment is never closed with `*/`",
                        self.path
                    )));
                }
                Some('\n') => self.newline(),
                Some(c) => self.pos += c.len_utf8(),
            }
        }
    }

    /// `#include <path>` — the only thing a `#` may start. Stan 2.33 removed
    /// `#` comments, so anything else is refused with that sentence rather than
    /// lexed as something it is not.
    fn hash(&mut self, line: u32) -> Result<Tok> {
        let col = self.col(self.pos);
        // Through `self.text` rather than `self.rest()`, so the slice borrows
        // the source and not the lexer whose position moves below.
        let text = self.text;
        let Some(after) = text[self.pos..].strip_prefix("#include") else {
            return Err(self.error(
                line,
                col,
                "`#` starts a comment only in old Stan (removed in 2.33); write `//`, or \
                 `#include <file>`"
                    .to_owned(),
            ));
        };
        let line_text = after.split('\n').next().unwrap_or("");
        self.pos += "#include".len() + line_text.len();
        let line_text = line_text.split("//").next().unwrap_or("").trim();
        let path = if let Some(quoted) = line_text.strip_prefix('"') {
            quoted.strip_suffix('"')
        } else if let Some(angled) = line_text.strip_prefix('<') {
            angled.strip_suffix('>')
        } else {
            Some(line_text).filter(|p| !p.contains(char::is_whitespace))
        };
        match path.map(str::trim).filter(|p| !p.is_empty()) {
            Some(path) => Ok(Tok::Include(path.to_owned())),
            None => Err(self.error(
                line,
                col,
                format!("`#include` needs one file name, got `{line_text}`"),
            )),
        }
    }

    fn string(&mut self, line: u32) -> Result<Tok> {
        self.pos += 1;
        let from = self.pos;
        loop {
            match self.peek_char() {
                None | Some('\n') => {
                    return Err(Error::invalid(format!(
                        "{}:{line}: a string literal is never closed",
                        self.path
                    )));
                }
                Some('"') => {
                    let body = self.text[from..self.pos].to_owned();
                    self.pos += 1;
                    return Ok(Tok::Str(body));
                }
                Some('\\') => {
                    self.pos += 1;
                    if let Some(c) = self.peek_char().filter(|c| *c != '\n') {
                        self.pos += c.len_utf8();
                    }
                }
                Some(c) => self.pos += c.len_utf8(),
            }
        }
    }

    fn digits(&mut self) {
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
    }

    fn number(&mut self) -> Tok {
        let start = self.pos;
        let mut real = false;
        self.digits();
        // `1.5`, `.5` and `1.` — but not the `.` of `.*` or `./` after an int.
        if self.bytes.get(self.pos) == Some(&b'.')
            && !matches!(self.bytes.get(self.pos + 1), Some(b'*' | b'/' | b'^'))
        {
            real = true;
            self.pos += 1;
            self.digits();
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            let mut at = self.pos + 1;
            if matches!(self.bytes.get(at), Some(b'+' | b'-')) {
                at += 1;
            }
            if self.bytes.get(at).is_some_and(u8::is_ascii_digit) {
                real = true;
                self.pos = at;
                self.digits();
            }
        }
        // An imaginary literal, `2i` or `1.5i`.
        if self.bytes.get(self.pos) == Some(&b'i')
            && !self
                .bytes
                .get(self.pos + 1)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            real = true;
            self.pos += 1;
        }
        let text = self.text[start..self.pos].to_owned();
        if real {
            Tok::Real(text)
        } else {
            Tok::Int(text)
        }
    }

    fn ident(&mut self) -> Tok {
        let start = self.pos;
        while let Some(c) = self
            .peek_char()
            .filter(|c| c.is_alphanumeric() || *c == '_')
        {
            self.pos += c.len_utf8();
        }
        Tok::Ident(self.text[start..self.pos].to_owned())
    }

    fn error(&self, line: u32, col: u32, msg: String) -> Error {
        Error::invalid(format!("{}:{line}:{col}: {msg}", self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Tok> {
        lex(text, 0, "t.stan")
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    fn punct(p: &'static str) -> Tok {
        Tok::Punct(p)
    }

    #[test]
    fn comments_and_strings_hide_what_is_inside_them() {
        let toks = kinds("a /* } { */ b // }\nprint(\"}\");");
        assert_eq!(
            toks,
            [
                Tok::Ident("a".into()),
                Tok::Ident("b".into()),
                Tok::Ident("print".into()),
                punct("("),
                Tok::Str("}".into()),
                punct(")"),
                punct(";"),
            ]
        );
    }

    #[test]
    fn numbers_are_ints_or_reals_and_operators_are_longest_first() {
        assert_eq!(
            kinds("1 1.5 .5 1e3 2.0e-3 3i N %/% 2 x.*y 2./x"),
            [
                Tok::Int("1".into()),
                Tok::Real("1.5".into()),
                Tok::Real(".5".into()),
                Tok::Real("1e3".into()),
                Tok::Real("2.0e-3".into()),
                Tok::Real("3i".into()),
                Tok::Ident("N".into()),
                punct("%/%"),
                Tok::Int("2".into()),
                Tok::Ident("x".into()),
                punct(".*"),
                Tok::Ident("y".into()),
                Tok::Int("2".into()),
                punct("./"),
                Tok::Ident("x".into()),
            ]
        );
    }

    #[test]
    fn an_include_is_one_token_in_any_of_its_three_spellings() {
        assert_eq!(
            kinds("#include a.stan\n  #include \"b/c.stan\" // why\n#include <d.stan>"),
            [
                Tok::Include("a.stan".into()),
                Tok::Include("b/c.stan".into()),
                Tok::Include("d.stan".into()),
            ]
        );
    }

    #[test]
    fn positions_are_lines_and_columns() {
        let toks = lex("data {\n  int N;\n}", 3, "t.stan").unwrap();
        let n = &toks[3];
        assert_eq!(
            (n.kind.clone(), n.line, n.col, n.file),
            (Tok::Ident("N".into()), 2, 7, 3)
        );
    }

    #[test]
    fn what_cannot_be_lexed_is_refused_with_its_place() {
        let err = |text: &str| lex(text, 0, "m.stan").unwrap_err().to_string();
        assert!(err("a\n/* open").contains("m.stan:2: a `/*` comment is never closed"));
        assert!(err("print(\"x);").contains("m.stan:1: a string literal is never closed"));
        assert!(err("# old comment").contains("removed in 2.33"));
        assert!(err("#include").contains("`#include` needs one file name"));
        assert!(err("a @ b").contains("m.stan:1:3: unexpected character `@`"));
    }
}
