//! [`SafeHtml`] and the one HTML escape (code-quality review §4.6,
//! TODO-code-quality Q4.5).
//!
//! The server built markup with five hand-written escapers: the template
//! engine's (`{{ }}` in an HTML body), the Saltcorn UI framework's pages, the
//! builder's document, the OAuth hand-off page and a plugin's asset tags. Three
//! escaped `&`, `<`, `>`, `"` and `'`; two (the OAuth page, a plugin's
//! `src`/`href`) left `'` alone, which is safe only because each put its value
//! in a **double**-quoted attribute. They are one function now, the five-
//! character one: the same entities (`&amp;` `&lt;` `&gt;` `&quot;` `&#39;`),
//! correct for element text and for an attribute value in either kind of
//! quotes, and a superset of what the two shorter ones did — what a browser
//! shows is unchanged.
//!
//! It is **not** an escape for an unquoted attribute, a URL's scheme, a
//! `<script>` or a `<style>`: JSON embedded in a script has its own
//! (`<` → `<`, in the builder), and is not markup.
//!
//! A [`SafeHtml`] is markup that is safe to send: made by [`escape`] from
//! text, or by [`SafeHtml::trusted`] from a literal the program spells, and
//! joined with others. It displays as its markup, so a page written with
//! `format!` interpolates one where it would have interpolated a `String`
//! someone remembered to escape — the type is the evidence that they did.

use std::borrow::Cow;
use std::fmt;

/// Markup that is safe to put in a page: escaped text, a trusted literal, or
/// a join of those.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafeHtml(Cow<'static, str>);

impl SafeHtml {
    /// `text`, escaped ([`escape_html`]).
    pub fn escape(text: &str) -> SafeHtml {
        SafeHtml(Cow::Owned(escape_html(text)))
    }

    /// Markup the program spells itself (`"<br>"`): a literal, never data.
    pub const fn trusted(markup: &'static str) -> SafeHtml {
        SafeHtml(Cow::Borrowed(markup))
    }

    /// The markup.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The markup, owned.
    pub fn into_string(self) -> String {
        self.0.into_owned()
    }

    /// Append `other`.
    pub fn push(&mut self, other: &SafeHtml) {
        self.0.to_mut().push_str(&other.0);
    }

    /// Append `text`, escaped.
    pub fn push_text(&mut self, text: &str) {
        push_escaped(self.0.to_mut(), text);
    }

    /// `value` as JSON that is safe inside a `<script>` element (a
    /// `type="application/json"` block a page boots from): nothing in it can
    /// close the element, open a comment or end a line in a JavaScript string.
    pub fn json_in_script(value: &serde_json::Value) -> SafeHtml {
        let json = serde_json::to_string(value)
            .unwrap_or_else(|_| "null".to_owned())
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        SafeHtml(Cow::Owned(json))
    }

    /// Markup a program in this server draws (an SVG a QR encoder made from
    /// a string it was given), named by `generator` so each such site is a
    /// greppable `SafeHtml::generated(` a reviewer can read. Never data a
    /// person or a request supplied.
    pub fn generated(markup: String, generator: &'static str) -> SafeHtml {
        let _ = generator;
        SafeHtml(Cow::Owned(markup))
    }

    /// The markup, for [`safe_html!`](crate::safe_html): taking `&SafeHtml`
    /// is what makes the macro refuse a `String` argument.
    #[doc(hidden)]
    pub fn markup(&self) -> &str {
        &self.0
    }

    /// What [`safe_html!`](crate::safe_html) formats: a literal the program
    /// spells, with only `SafeHtml` interpolated. Not for direct use.
    #[doc(hidden)]
    pub fn __composed(markup: String) -> SafeHtml {
        SafeHtml(Cow::Owned(markup))
    }
}

impl FromIterator<SafeHtml> for SafeHtml {
    fn from_iter<I: IntoIterator<Item = SafeHtml>>(iter: I) -> SafeHtml {
        let mut out = SafeHtml::default();
        for piece in iter {
            out.push(&piece);
        }
        out
    }
}

/// Markup from a format string the program spells, interpolating only
/// [`SafeHtml`] (TODO-type-hardening H3.7, security review 2026-11 A10):
/// `safe_html!("<p>{}</p>", html::escape(name))` or with named arguments,
/// `safe_html!("<a href=\"{href}\">", href = html::escape(url))`.
///
/// The format string must be a literal, and it is expanded through `concat!`,
/// so `format!`'s implicit capture of a variable named in the braces does not
/// apply — every value interpolated is an argument, and every argument must be
/// a `SafeHtml`:
///
/// ```compile_fail
/// let name = String::from("<script>");
/// let _ = sc_expr::safe_html!("<p>{}</p>", name);
/// ```
///
/// ```compile_fail
/// let name = String::from("<script>");
/// let _ = sc_expr::safe_html!("<p>{name}</p>");
/// ```
#[macro_export]
macro_rules! safe_html {
    ($fmt:literal $(,)?) => {
        $crate::html::SafeHtml::__composed(::std::format!(::std::concat!($fmt)))
    };
    ($fmt:literal, $($name:ident = $arg:expr),+ $(,)?) => {
        $crate::html::SafeHtml::__composed(::std::format!(
            ::std::concat!($fmt),
            $($name = $crate::html::SafeHtml::markup(&$arg)),+
        ))
    };
    ($fmt:literal, $($arg:expr),+ $(,)?) => {
        $crate::html::SafeHtml::__composed(::std::format!(
            ::std::concat!($fmt),
            $($crate::html::SafeHtml::markup(&$arg)),+
        ))
    };
}

impl fmt::Display for SafeHtml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SafeHtml> for String {
    fn from(html: SafeHtml) -> String {
        html.into_string()
    }
}

/// `text` as [`SafeHtml`]: the one escape.
pub fn escape(text: &str) -> SafeHtml {
    SafeHtml::escape(text)
}

/// Escape the five characters that can end an element, an attribute value
/// (in either quotes) or an entity: `&` `<` `>` `"` `'`.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    push_escaped(&mut out, text);
    out
}

fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    /// TODO-type-hardening H3.7: the macro interpolates only `SafeHtml`.
    #[test]
    fn safe_html_interpolates_only_safe_html() {
        let name = "<b>\"ann\"</b>";
        let page = crate::safe_html!("<p>{}</p>", escape(name));
        assert_eq!(page.as_str(), "<p>&lt;b&gt;&quot;ann&quot;&lt;/b&gt;</p>");
        let link = crate::safe_html!(
            "<a href=\"{href}\">{label}</a>",
            href = escape("/a?b=1&c=2"),
            label = SafeHtml::trusted("<i>go</i>"),
        );
        assert_eq!(link.as_str(), "<a href=\"/a?b=1&amp;c=2\"><i>go</i></a>");
        assert_eq!(crate::safe_html!("<br>{{x}}").as_str(), "<br>{x}");
        let joined: SafeHtml = ["a<", "b"].iter().map(|t| escape(t)).collect();
        assert_eq!(joined.as_str(), "a&lt;b");
        let boot = SafeHtml::json_in_script(&serde_json::json!({ "x": "</script><!--" }));
        assert!(!boot.as_str().contains('<') && !boot.as_str().contains('>'));
    }

    use super::*;

    #[test]
    fn escaping_covers_the_five_markup_characters() {
        assert_eq!(
            escape_html(r#"<a href="x">&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
        // Escaped once: an entity in the text is text.
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
        assert_eq!(escape_html("plain — ü"), "plain — ü");
    }

    /// The five escapers this replaced, as they were: three agreed with it
    /// exactly, and two (double-quoted attributes only) left `'` alone, so
    /// the one escape is theirs plus `'` → `&#39;`.
    #[test]
    fn it_is_what_the_five_old_escapers_did() {
        let five = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;")
        };
        let four = |s: &str| {
            s.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        for sample in [
            r#"<script>alert("x")</script>"#,
            "a & b's <c>",
            "&lt; already",
            "https://example.com/?a=1&b='2'",
            "",
        ] {
            assert_eq!(escape_html(sample), five(sample), "{sample}");
            assert_eq!(escape_html(sample), four(sample).replace('\'', "&#39;"));
        }
    }

    #[test]
    fn safe_html_is_escaped_text_trusted_literals_and_their_joins() {
        let mut page = SafeHtml::trusted("<p>");
        page.push_text("Tom & <Jerry>");
        page.push(&SafeHtml::trusted("</p>"));
        assert_eq!(page.as_str(), "<p>Tom &amp; &lt;Jerry&gt;</p>");
        assert_eq!(
            format!("<a title=\"{}\">", escape(r#"say "hi""#)),
            "<a title=\"say &quot;hi&quot;\">"
        );
        assert_eq!(String::from(page.clone()), page.to_string());
    }
}
