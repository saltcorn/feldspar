//! Tokens of the R subset a formula is written in (R Language Definition
//! §10.3, "Tokens"): names (bare or in backquotes), numbers, strings, the
//! formula operators, and statement breaks.
//!
//! A newline ends a statement only where R's would: outside every bracket, and
//! not after an operator or a comma that still wants its right-hand side. So a
//! `bf(…)` pasted over five lines is one statement, and so is a long formula
//! broken after a `+`.

use super::{ParseError, Span};

/// A token's kind.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// A name: `log_radon`, `x.1`, `` `my col` ``.
    Name(String),
    /// A number literal (`1L`'s `L` dropped).
    Num(f64),
    /// A string literal, unescaped.
    Str(String),
    /// An operator or punctuation: `~ + - * / : ^ ( ) , | || = %in%`.
    Op(&'static str),
    /// The end of a statement.
    Break,
}

/// A token and where it is in the text (byte offsets).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// The tokens of `text`, statements separated by [`Tok::Break`]. Comments
/// (`#` to the end of the line) are skipped.
#[allow(clippy::too_many_lines)] // one arm per kind of token, in one match
pub(crate) fn lex(text: &str) -> Result<Vec<Token>, ParseError> {
    let bytes = text.as_bytes();
    let mut out: Vec<Token> = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        match c {
            b'\n' => {
                i += 1;
                let continues = out.last().is_none_or(|t| match &t.tok {
                    Tok::Op(op) => *op != ")",
                    Tok::Break => true,
                    _ => false,
                });
                if depth == 0 && !continues {
                    out.push(Token {
                        tok: Tok::Break,
                        span: Span::new(start, start),
                    });
                }
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b';' => {
                i += 1;
                if depth == 0 && out.last().is_some_and(|t| t.tok != Tok::Break) {
                    out.push(Token {
                        tok: Tok::Break,
                        span: Span::new(start, i),
                    });
                }
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'`' => {
                i += 1;
                let mut name = String::new();
                loop {
                    let Some(ch) = text[i..].chars().next() else {
                        return Err(ParseError::new(
                            "this backquoted name is never closed",
                            Span::new(start, text.len()),
                        ));
                    };
                    i += ch.len_utf8();
                    match ch {
                        '`' => break,
                        '\\' if text[i..].starts_with('`') => {
                            name.push('`');
                            i += 1;
                        }
                        _ => name.push(ch),
                    }
                }
                if name.is_empty() {
                    return Err(ParseError::new("an empty name", Span::new(start, i)));
                }
                out.push(Token {
                    tok: Tok::Name(name),
                    span: Span::new(start, i),
                });
            }
            b'"' | b'\'' => {
                i += 1;
                let mut value = String::new();
                loop {
                    let Some(ch) = text[i..].chars().next() else {
                        return Err(ParseError::new(
                            "this string is never closed",
                            Span::new(start, text.len()),
                        ));
                    };
                    i += ch.len_utf8();
                    if ch == char::from(c) {
                        break;
                    }
                    if ch == '\\' {
                        let Some(next) = text[i..].chars().next() else {
                            continue;
                        };
                        i += next.len_utf8();
                        value.push(match next {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    } else {
                        value.push(ch);
                    }
                }
                out.push(Token {
                    tok: Tok::Str(value),
                    span: Span::new(start, i),
                });
            }
            b'0'..=b'9' => {
                i = number_end(bytes, i);
                out.push(number(text, start, i)?);
            }
            b'.' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i = number_end(bytes, i);
                out.push(number(text, start, i)?);
            }
            c if c.is_ascii_alphabetic()
                || c == b'.'
                || c == b'_'
                || text[i..].chars().next().is_some_and(char::is_alphabetic) =>
            {
                // R's names: letters (in any script), digits, `.` and `_`.
                while let Some(ch) = text[i..].chars().next() {
                    if ch.is_alphanumeric() || ch == '.' || ch == '_' {
                        i += ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if c == b'_' {
                    return Err(ParseError::new(
                        "a name cannot start with `_`; put it in backquotes",
                        Span::new(start, i),
                    ));
                }
                out.push(Token {
                    tok: Tok::Name(text[start..i].to_owned()),
                    span: Span::new(start, i),
                });
            }
            b'%' => {
                let end = text[i + 1..].find('%').map(|e| i + 1 + e + 1);
                match end {
                    Some(end) if &text[i..end] == "%in%" => {
                        i = end;
                        out.push(op("%in%", start, i));
                    }
                    Some(end) => {
                        return Err(ParseError::new(
                            format!("the operator `{}` is not part of a formula", &text[i..end]),
                            Span::new(start, end),
                        ));
                    }
                    None => {
                        return Err(ParseError::new(
                            "a `%` that starts no operator",
                            Span::new(start, start + 1),
                        ));
                    }
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    i += 2;
                    out.push(op("||", start, i));
                } else {
                    i += 1;
                    out.push(op("|", start, i));
                }
            }
            b'=' if bytes.get(i + 1) == Some(&b'=') => {
                return Err(ParseError::new(
                    "a comparison (`==`) is not part of a formula; compute it as a column of the \
                     dataset",
                    Span::new(start, start + 2),
                ));
            }
            b'<' if bytes.get(i + 1) == Some(&b'-') => {
                return Err(ParseError::new(
                    "an assignment (`<-`) is not part of a formula",
                    Span::new(start, start + 2),
                ));
            }
            _ => {
                let symbol = match c {
                    b'~' => "~",
                    b'+' => "+",
                    b'-' => "-",
                    b'*' => "*",
                    b'/' => "/",
                    b':' => ":",
                    b'^' => "^",
                    b'(' => "(",
                    b')' => ")",
                    b',' => ",",
                    b'=' => "=",
                    _ => {
                        let ch = text[i..].chars().next().unwrap_or('?');
                        return Err(ParseError::new(
                            format!("`{ch}` is not part of a formula"),
                            Span::new(start, start + ch.len_utf8()),
                        ));
                    }
                };
                i += 1;
                if symbol == "(" {
                    depth += 1;
                } else if symbol == ")" {
                    depth -= 1;
                    if depth < 0 {
                        return Err(ParseError::new(
                            "this `)` closes nothing",
                            Span::new(start, i),
                        ));
                    }
                }
                out.push(op(symbol, start, i));
            }
        }
    }
    if depth > 0 {
        let open = out
            .iter()
            .rev()
            .find(|t| t.tok == Tok::Op("("))
            .map_or(Span::new(0, 0), |t| t.span);
        return Err(ParseError::new("this `(` is never closed", open));
    }
    Ok(out)
}

fn op(symbol: &'static str, start: usize, end: usize) -> Token {
    Token {
        tok: Tok::Op(symbol),
        span: Span::new(start, end),
    }
}

fn number_end(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i < bytes.len() && bytes[i] == b'L' {
        i += 1;
    }
    i
}

fn number(text: &str, start: usize, end: usize) -> Result<Token, ParseError> {
    let digits = text[start..end].trim_end_matches('L');
    let value: f64 = digits.parse().map_err(|_| {
        ParseError::new(
            format!("`{}` is not a number", &text[start..end]),
            Span::new(start, end),
        )
    })?;
    Ok(Token {
        tok: Tok::Num(value),
        span: Span::new(start, end),
    })
}
