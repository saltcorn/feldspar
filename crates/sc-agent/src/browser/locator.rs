//! Stable locators (TODO-checklists C2.2): how a checklist step names an
//! element so that the name survives the next build.
//!
//! The `@eN` refs `view_app`'s snapshot hands out are valid for one snapshot.
//! A step written today has to find the same button next month, so it names the
//! element the way a person would — by its role and accessible name, its label,
//! its visible text — or, where the page offers one, by its `data-testid`. CSS
//! is the last resort: it names how the page is built, not what it shows.
//!
//! One string per locator, so a form field and a model can both write one:
//!
//! | written | finds |
//! |---|---|
//! | `button "Save"` | a button whose accessible name is `Save` |
//! | `link Sign in` | a link whose name contains `sign in` |
//! | `role=checkbox` | any checkbox |
//! | `label=Email` | the form control labelled (containing) `email` |
//! | `text="Order saved"` | the element whose visible text is `Order saved` |
//! | `testid=submit-order` | `[data-testid="submit-order"]` |
//! | `css=form .primary` | the CSS selector |
//!
//! A **quoted** text matches the whole name; an unquoted one matches a part of
//! it. Both ignore case and collapse runs of whitespace. The role form is the
//! line `view_app`'s snapshot prints (`button "Save" @e3`, without the ref), so
//! an agent can copy what it saw.

use std::fmt;
use std::str::FromStr;

use sc_error::{Error, Result};

/// Text a locator matches against an element's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    /// The text, as written.
    pub text: String,
    /// Whether it must be the whole name (it was quoted), not just a part.
    pub exact: bool,
}

impl TextMatch {
    /// A match of the whole name.
    pub fn exact(text: impl Into<String>) -> TextMatch {
        TextMatch {
            text: text.into(),
            exact: true,
        }
    }

    /// A match of a part of the name.
    pub fn contains(text: impl Into<String>) -> TextMatch {
        TextMatch {
            text: text.into(),
            exact: false,
        }
    }

    /// Whether `candidate` matches, ignoring case and runs of whitespace.
    pub fn matches(&self, candidate: &str) -> bool {
        let want = normalise(&self.text);
        let have = normalise(candidate);
        if self.exact {
            have == want
        } else {
            have.contains(&want)
        }
    }

    /// `"text"` or `text`, from what follows a prefix.
    fn parse(raw: &str) -> Result<TextMatch> {
        let raw = raw.trim();
        if let Some(inner) = raw.strip_prefix('"') {
            let inner = inner.strip_suffix('"').ok_or_else(|| {
                Error::invalid(format!("`{raw}` opens a quote and does not close it"))
            })?;
            return Ok(TextMatch::exact(inner.replace("\\\"", "\"")));
        }
        Ok(TextMatch::contains(raw))
    }
}

impl fmt::Display for TextMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exact {
            write!(f, "\"{}\"", self.text.replace('"', "\\\""))
        } else {
            f.write_str(&self.text)
        }
    }
}

/// Lower case, with every run of whitespace one space and none at the ends.
pub fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// How a step names an element (see the module notes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locator {
    /// By ARIA role, and by accessible name when one is given.
    Role {
        role: String,
        name: Option<TextMatch>,
    },
    /// A form control by its label (its accessible name).
    Label(TextMatch),
    /// The element whose visible text this is.
    Text(TextMatch),
    /// `[data-testid="…"]`.
    TestId(String),
    /// A CSS selector.
    Css(String),
}

impl Locator {
    /// The CSS selector the DOM is queried with, for the two forms that are
    /// not resolved against the accessibility tree.
    pub fn selector(&self) -> Option<String> {
        match self {
            Locator::TestId(id) => Some(format!(
                "[data-testid=\"{}\"]",
                id.replace('\\', "\\\\").replace('"', "\\\"")
            )),
            Locator::Css(css) => Some(css.clone()),
            _ => None,
        }
    }
}

impl FromStr for Locator {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Locator> {
        let raw = raw.trim();
        let empty = |what: &str| Error::invalid(format!("the locator `{raw}` names no {what}"));
        if let Some((prefix, rest)) = raw.split_once('=') {
            let rest = rest.trim();
            match prefix.trim() {
                "css" => {
                    return match rest {
                        "" => Err(empty("selector")),
                        css => Ok(Locator::Css(css.to_owned())),
                    };
                }
                "testid" | "data-testid" => {
                    let id = rest.trim_matches('"');
                    return match id {
                        "" => Err(empty("test id")),
                        id => Ok(Locator::TestId(id.to_owned())),
                    };
                }
                "label" => {
                    let text = TextMatch::parse(rest)?;
                    return match text.text.trim() {
                        "" => Err(empty("label")),
                        _ => Ok(Locator::Label(text)),
                    };
                }
                "text" => {
                    let text = TextMatch::parse(rest)?;
                    return match text.text.trim() {
                        "" => Err(empty("text")),
                        _ => Ok(Locator::Text(text)),
                    };
                }
                "role" => return role(rest, raw),
                // Not a prefix: `=` inside a role's name, as in `button "a=b"`.
                _ => {}
            }
        }
        role(raw, raw)
    }
}

/// `button`, `button "Save"` or `link Sign in`.
fn role(text: &str, raw: &str) -> Result<Locator> {
    let (role, name) = match text.split_once(char::is_whitespace) {
        Some((role, name)) => (role, Some(name.trim())),
        None => (text, None),
    };
    let is_role = !role.is_empty()
        && role
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_');
    if !is_role {
        return Err(Error::invalid(format!(
            "`{raw}` is not a locator; write one of: button \"Save\" (a role and its name), \
             label=Email, text=\"Order saved\", testid=submit, css=.selector"
        )));
    }
    let name = match name.filter(|n| !n.is_empty()) {
        None => None,
        Some(name) => Some(TextMatch::parse(name)?),
    };
    Ok(Locator::Role {
        role: role.to_ascii_lowercase(),
        name,
    })
}

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Locator::Role { role, name: None } => f.write_str(role),
            Locator::Role {
                role,
                name: Some(name),
            } => write!(f, "{role} {name}"),
            Locator::Label(text) => write!(f, "label={text}"),
            Locator::Text(text) => write!(f, "text={text}"),
            Locator::TestId(id) => write!(f, "testid={id}"),
            Locator::Css(css) => write!(f, "css={css}"),
        }
    }
}

/// An element an action works on: a ref from `view_app`'s last snapshot, or a
/// stable locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Element {
    /// `@e3`.
    Ref(String),
    /// A locator, resolved when the action runs, waiting for the element.
    Locator(Locator),
}

impl fmt::Display for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Element::Ref(reference) => f.write_str(reference),
            Element::Locator(locator) => write!(f, "{locator}"),
        }
    }
}

/// One element a locator found, as the accessibility tree describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElementInfo {
    /// Its ARIA role; empty for an element the tree leaves out.
    pub role: String,
    /// Its accessible name.
    pub name: String,
    /// Its value — a field's text, a select's chosen option — when it has one.
    pub value: Option<String>,
    /// Whether it can be used (not `disabled`).
    pub enabled: bool,
    /// Whether it is checked, for a checkbox, radio or switch.
    pub checked: Option<bool>,
    /// Whether a person can see it: in the accessibility tree and laid out.
    pub visible: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Locator {
        raw.parse().unwrap()
    }

    #[test]
    fn each_form_parses_and_prints_back() {
        assert_eq!(
            parse(r#"button "Save""#),
            Locator::Role {
                role: "button".into(),
                name: Some(TextMatch::exact("Save"))
            }
        );
        assert_eq!(
            parse("link Sign in"),
            Locator::Role {
                role: "link".into(),
                name: Some(TextMatch::contains("Sign in"))
            }
        );
        assert_eq!(
            parse("role=checkbox"),
            Locator::Role {
                role: "checkbox".into(),
                name: None
            }
        );
        assert_eq!(
            parse("label=Email"),
            Locator::Label(TextMatch::contains("Email"))
        );
        assert_eq!(
            parse(r#"text="Order saved""#),
            Locator::Text(TextMatch::exact("Order saved"))
        );
        assert_eq!(parse("testid=submit"), Locator::TestId("submit".into()));
        assert_eq!(
            parse("css=form .primary"),
            Locator::Css("form .primary".into())
        );
        assert_eq!(
            parse(r#"button "a=b""#),
            Locator::Role {
                role: "button".into(),
                name: Some(TextMatch::exact("a=b"))
            }
        );
        for raw in [
            r#"button "Save""#,
            "link Sign in",
            "checkbox",
            "label=Email",
            r#"text="Order saved""#,
            "testid=submit",
            "css=form .primary",
        ] {
            assert_eq!(parse(raw).to_string(), raw);
        }
    }

    #[test]
    fn a_locator_that_names_nothing_is_refused_with_the_forms() {
        for bad in ["", "label=", "css=", r#"text="open"#, "#save", ".primary"] {
            let err = bad.parse::<Locator>().unwrap_err().to_string();
            assert!(!err.is_empty(), "{bad}");
        }
        let err = ".primary".parse::<Locator>().unwrap_err().to_string();
        assert!(err.contains("testid=submit"), "{err}");
    }

    #[test]
    fn quoted_text_matches_the_whole_name_and_unquoted_a_part() {
        let exact = TextMatch::exact("Save");
        assert!(exact.matches("save"));
        assert!(exact.matches("  Save "));
        assert!(!exact.matches("Save draft"));
        let part = TextMatch::contains("sign  in");
        assert!(part.matches("Sign in to continue"));
        assert!(!part.matches("Sign up"));
    }

    #[test]
    fn a_test_id_is_a_quoted_attribute_selector() {
        assert_eq!(
            Locator::TestId(r#"a"b"#.into()).selector().as_deref(),
            Some(r#"[data-testid="a\"b"]"#)
        );
        assert_eq!(Locator::Label(TextMatch::exact("x")).selector(), None);
    }
}
