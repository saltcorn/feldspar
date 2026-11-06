//! The message format, stated once (proposal §2, decision D2).
//!
//! A message may contain `{identifier}` and nothing else. No expressions, no
//! member access, no function calls, no filters. The rules, in full:
//!
//! - `{identifier}` is a **placeholder**, where an identifier is an ASCII letter
//!   or `_` followed by letters, digits and `_`.
//! - `{{` is a literal `{`.
//! - Anything else between braces is a **literal run**, copied out as written —
//!   `{not an identifier}` renders as `{not an identifier}`.
//! - **A closing brace is never ambiguous and therefore never escaped.** `}` is
//!   always the character `}`, and `}}` is two of them. Only the opening brace
//!   can start something, so only the opening brace has an escape — which is one
//!   fewer rule for a translator to get wrong than the symmetric spelling would
//!   be, and one fewer place the two implementations can disagree.
//! - A placeholder with **no argument renders as written**: `{name}` with no
//!   `name` in the arguments comes out as the five characters `{name}`. A
//!   visible `{name}` is a bug report; an empty string is a mystery.
//! - A message is **never HTML**. It is escaped by whatever renders it, exactly
//!   as any other string is.
//!
//! Single braces rather than the `{{ }}` of [`sc_expr::Template`] on purpose:
//! in a Saltcorn UI layout a text element's content is simultaneously a
//! translatable message and a `{{ }}` template, and a translator's
//! `{{ user.email }}` must not be mistakable for an interpolation this server
//! evaluates (D2).
//!
//! This is implemented twice — here, and in the generated `messages.ts` the
//! applications get — and the two are held to each other by
//! `crates/sc-i18n/fixtures/format.json`, a corpus of (message, args, expected)
//! triples that a Rust test and a vitest test both run. Two implementations of
//! one thing disagree by the third bug fixed in one of them; a shared fixture is
//! what makes this instance affordable.
//!
//! [`sc_expr::Template`]: https://docs.rs/sc-expr

use std::collections::BTreeSet;

/// One argument's value, as a message renders it.
///
/// Three shapes rather than `impl Display`, because one of the three has a
/// second job: plural selection reads the *number* behind `count`
/// ([`Arg::plural_count`]), and a `&dyn Display` cannot be asked what number it
/// was.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// Text, rendered as itself.
    Text(String),
    /// A whole number — the shape a `count` normally has.
    Int(i64),
    /// A fractional number.
    Float(f64),
}

impl Arg {
    /// The value as the message renders it.
    pub fn render(&self) -> String {
        match self {
            Arg::Text(s) => s.clone(),
            Arg::Int(i) => i.to_string(),
            Arg::Float(f) => f.to_string(),
        }
    }

    /// The number this argument is, for selecting a plural category — `None` for
    /// text.
    ///
    /// A fractional value is **truncated towards zero** for selection while still
    /// rendering in full. CLDR does distinguish `1` from `1.0` (they are `one`
    /// and `other` in Czech), and this does not; the cost of that is a wrong
    /// plural form for a fractional count, and the messages this facility
    /// formats — "{count} rows", "{count} files selected" — do not have one. The
    /// alternative is a decimal type in a layer-0 crate, which is a real
    /// dependency bought for a case that does not arise.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the plural category of a float is chosen by its integral part; the cast saturates"
    )]
    pub fn plural_count(&self) -> Option<i64> {
        match self {
            Arg::Text(_) => None,
            Arg::Int(i) => Some(*i),
            Arg::Float(f) => Some(*f as i64),
        }
    }
}

impl From<&str> for Arg {
    fn from(value: &str) -> Arg {
        Arg::Text(value.to_owned())
    }
}

impl From<String> for Arg {
    fn from(value: String) -> Arg {
        Arg::Text(value)
    }
}

impl From<&String> for Arg {
    fn from(value: &String) -> Arg {
        Arg::Text(value.clone())
    }
}

macro_rules! arg_from_int {
    ($($ty:ty),+) => {
        $(impl From<$ty> for Arg {
            fn from(value: $ty) -> Arg {
                Arg::Int(i64::from(value))
            }
        })+
    };
}
arg_from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<usize> for Arg {
    fn from(value: usize) -> Arg {
        // Saturating rather than wrapping: a count past `i64::MAX` is not a
        // number anybody is about to read, and a negative one would be a lie.
        Arg::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<u64> for Arg {
    fn from(value: u64) -> Arg {
        Arg::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<f64> for Arg {
    fn from(value: f64) -> Arg {
        Arg::Float(value)
    }
}

impl From<bool> for Arg {
    fn from(value: bool) -> Arg {
        Arg::Text(value.to_string())
    }
}

/// The arguments a message is formatted with: name → value, in whatever order
/// the call site wrote them.
///
/// A slice rather than a map because the call sites have one, two or three
/// arguments and a linear scan over three pairs beats hashing one of them.
pub type Args<'a> = [(&'a str, Arg)];

/// One piece of a scanned message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part<'a> {
    /// Text to copy out as-is.
    Literal(&'a str),
    /// A `{name}` placeholder, carrying the name and the source text it was
    /// written as (which is what renders when there is no such argument).
    Placeholder { name: &'a str, source: &'a str },
}

/// Scan a message into literals and placeholders.
///
/// One scanner, used by both [`format`] and [`placeholders`], so "what the
/// renderer substitutes" and "what the validator checks" cannot drift apart —
/// which is the entire point of D9's placeholder check.
struct Parts<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Parts<'a> {
    type Item = Part<'a>;

    fn next(&mut self) -> Option<Part<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        let bytes = self.rest.as_bytes();
        match bytes.first() {
            // `{{` — a literal brace. Emitted as a one-character literal so the
            // escape disappears exactly once.
            Some(b'{') if bytes.get(1) == Some(&b'{') => {
                self.rest = &self.rest[2..];
                Some(Part::Literal("{"))
            }
            Some(b'{') => match placeholder_at(self.rest) {
                Some(end) => {
                    let source = &self.rest[..end];
                    let name = &self.rest[1..end - 1];
                    self.rest = &self.rest[end..];
                    Some(Part::Placeholder { name, source })
                }
                // Not a placeholder: the brace is literal, and whatever follows
                // is scanned normally — which is what makes
                // `{not an identifier}` come out as written.
                None => {
                    self.rest = &self.rest[1..];
                    Some(Part::Literal("{"))
                }
            },
            _ => {
                // Up to the next brace, or the whole of what is left.
                let end = self.rest.find('{').unwrap_or(self.rest.len());
                let (literal, rest) = self.rest.split_at(end);
                self.rest = rest;
                Some(Part::Literal(literal))
            }
        }
    }
}

/// If `s` opens with `{identifier}`, the index just past the closing brace.
fn placeholder_at(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 1;
    match bytes.get(i) {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => i += 1,
        _ => return None,
    }
    while let Some(c) = bytes.get(i) {
        if c.is_ascii_alphanumeric() || *c == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    match bytes.get(i) {
        Some(b'}') => Some(i + 1),
        _ => None,
    }
}

fn parts(message: &str) -> Parts<'_> {
    Parts { rest: message }
}

/// Render `message` with `args`, per the rules at the top of this module.
///
/// Never fails: a message with a placeholder nobody supplied renders that
/// placeholder as written, which is a bug report an admin can read rather than
/// an error a request dies of.
pub fn format(message: &str, args: &Args<'_>) -> String {
    // The common case by a wide margin — a message with no braces in it at all —
    // costs one `memchr` and one allocation, and is why this is checked here
    // rather than discovered by the scanner one literal at a time.
    if !message.contains('{') {
        return message.to_owned();
    }
    let mut out = String::with_capacity(message.len());
    for part in parts(message) {
        match part {
            Part::Literal(text) => out.push_str(text),
            Part::Placeholder { name, source } => match lookup(args, name) {
                Some(value) => out.push_str(&value.render()),
                None => out.push_str(source),
            },
        }
    }
    out
}

/// The set of placeholder names `message` uses.
///
/// A set rather than a list: `"{name} moved to {name}"` uses one placeholder
/// twice, and a translation that uses it once has not lost anything. What D9
/// rejects is a translation that mentions a *different* set of names.
pub fn placeholders(message: &str) -> BTreeSet<&str> {
    if !message.contains('{') {
        return BTreeSet::new();
    }
    parts(message)
        .filter_map(|part| match part {
            Part::Placeholder { name, .. } => Some(name),
            Part::Literal(_) => None,
        })
        .collect()
}

/// The value given for `name`, if any.
fn lookup<'a>(args: &'a Args<'_>, name: &str) -> Option<&'a Arg> {
    args.iter().find(|(key, _)| *key == name).map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_named_placeholders() {
        assert_eq!(
            format("Delete {name}?", &[("name", Arg::from("Tasks"))]),
            "Delete Tasks?"
        );
    }

    #[test]
    fn a_placeholder_with_no_argument_renders_as_written() {
        // The whole of D2's failure mode: visible, and therefore reportable.
        assert_eq!(format("Delete {name}?", &[]), "Delete {name}?");
    }

    #[test]
    fn double_brace_is_a_literal_brace() {
        assert_eq!(format("{{name}", &[("name", Arg::from("x"))]), "{name}");
    }

    #[test]
    fn a_run_that_is_not_an_identifier_is_literal() {
        assert_eq!(format("{not an identifier}", &[]), "{not an identifier}");
        assert_eq!(format("a { b } c", &[]), "a { b } c");
        assert_eq!(format("{}", &[]), "{}");
    }

    #[test]
    fn numbers_render_as_numbers() {
        assert_eq!(
            format("{count} rows", &[("count", Arg::from(3u32))]),
            "3 rows"
        );
        assert_eq!(
            format("{ratio} of them", &[("ratio", Arg::from(0.5f64))]),
            "0.5 of them"
        );
    }

    #[test]
    fn placeholders_are_a_set() {
        let found = placeholders("{name} moved to {target}, {name}");
        assert_eq!(found.into_iter().collect::<Vec<_>>(), ["name", "target"]);
        assert!(placeholders("no braces here").is_empty());
        // `{{` is an escape, not a placeholder.
        assert!(placeholders("{{count}").is_empty());
    }

    #[test]
    fn a_fractional_count_selects_on_its_integer_part() {
        assert_eq!(Arg::from(2.7f64).plural_count(), Some(2));
        assert_eq!(Arg::from("three").plural_count(), None);
    }
}
