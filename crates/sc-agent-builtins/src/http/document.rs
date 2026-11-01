//! A response, turned into lines a model can be shown a window of.
//!
//! Everything here is about the one number that matters to an agent: how many
//! tokens a page costs. A documentation page is mostly navigation, scripts and
//! markup; the Markdown of its main content is often a tenth of its bytes. So:
//!
//! - **HTML becomes Markdown** (Turndown's rules, via `htmd`), from the page's
//!   `<main>` — or `role="main"`, or `<article>` — when it has one, falling back
//!   to the whole body when that turns out to hold almost nothing. Scripts,
//!   styles, navigation, footers, forms and inline SVG are dropped, and so is an
//!   image whose `src` is a `data:` URI, which would otherwise be inlined as
//!   base64. Relative links are made absolute, so a link the model reads is a
//!   URL it can fetch next.
//! - **JSON is pretty-printed**, so it has lines to page through.
//! - **Text and Markdown pass through.** A site that answers `Accept:
//!   text/markdown` has already done the conversion.
//! - **Anything else is described, not shown**: a PDF or an image as bytes in a
//!   tool result is noise to a text model.
//!
//! The result is **lines**, trimmed, with runs of blank lines collapsed and any
//! line longer than [`MAX_LINE_CHARS`] wrapped, so that a minified file is still
//! a document a window can be taken out of.

use std::rc::Rc;

use htmd::HtmlToMarkdown;
use htmd::options::{BulletListMarker, HrStyle, LinkStyle, Options};
use markup5ever_rcdom::{Node, NodeData};
use reqwest::Url;

use super::client::Response;

/// The longest line kept whole. Longer ones are wrapped, so one line of a
/// minified bundle cannot be a whole window by itself.
pub const MAX_LINE_CHARS: usize = 1_000;

/// Below this many characters, a `<main>` is taken to be a shell whose content
/// is elsewhere on the page (or is drawn by a script), and the body is used.
const THIN_MAIN_CHARS: usize = 200;

/// Elements whose content is never what a reader came for.
const SKIP_TAGS: [&str; 16] = [
    "head", "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object",
    "embed", "nav", "footer", "form", "button", "select", "dialog",
];

/// What a document was made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// HTML, converted to Markdown.
    Html,
    /// Served as Markdown.
    Markdown,
    /// JSON, pretty-printed.
    Json,
    /// Any other text, as it came.
    Text,
    /// Not text: described, with no lines.
    Binary,
}

/// One fetched document, ready to be windowed.
#[derive(Debug, Clone)]
pub struct Document {
    /// The URL that answered.
    pub url: String,
    /// The URL that was asked for, when a redirect led elsewhere.
    pub requested: Option<String>,
    pub status: u16,
    /// The media type, without parameters (`text/html`), or `unknown`.
    pub media_type: String,
    pub kind: Kind,
    /// The HTML `<title>`, where there was one.
    pub title: Option<String>,
    /// How many bytes were read.
    pub source_bytes: usize,
    /// Whether the body was cut off at the download limit.
    pub download_truncated: bool,
    /// Whether the page looks like it is drawn by a script: HTML with scripts
    /// and next to no text. The model is told, because "the page is empty" and
    /// "the page needs a browser" call for different next steps.
    pub scripted: bool,
    pub lines: Vec<String>,
    /// Characters over all lines, newlines included.
    pub chars: usize,
}

impl Document {
    /// Turn `response` into a document. `raw` keeps HTML as HTML, for when the
    /// conversion lost something the model needs to see.
    ///
    /// Synchronous and CPU-bound (parsing a large page is milliseconds to tens
    /// of them), so the caller runs it off the async runtime.
    pub fn from_response(response: Response, raw: bool) -> Document {
        let media_type = response
            .content_type
            .as_deref()
            .and_then(|ct| ct.split(';').next())
            .map(|m| m.trim().to_ascii_lowercase())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        let requested = response.redirects.first().map(Url::to_string);
        let kind = classify(&media_type, &response.body);
        let mut title = None;
        let mut scripted = false;
        let text = match kind {
            Kind::Binary => String::new(),
            _ => String::from_utf8_lossy(&response.body).into_owned(),
        };
        let content = match kind {
            Kind::Html if !raw => {
                let converted = html_to_markdown(&text, &response.url);
                title = converted.title;
                scripted = converted.markdown.trim().chars().count() < THIN_MAIN_CHARS
                    && text.contains("<script");
                converted.markdown
            }
            Kind::Json if !response.truncated => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|json| serde_json::to_string_pretty(&json).ok())
                .unwrap_or(text),
            _ => text,
        };
        let lines = to_lines(&content);
        let chars = lines.iter().map(|l| l.chars().count() + 1).sum();
        Document {
            url: response.url.to_string(),
            requested,
            status: response.status,
            media_type,
            kind,
            title,
            source_bytes: response.body.len(),
            download_truncated: response.truncated,
            scripted,
            lines,
            chars,
        }
    }

    /// Roughly what the document holds in memory, for the cache's budget.
    pub fn weight(&self) -> usize {
        self.lines.iter().map(|l| l.len() + 24).sum::<usize>() + self.url.len() + 256
    }
}

/// What a body is, from its media type — or from the bytes, where the server
/// did not say or said something too vague to go on.
fn classify(media_type: &str, body: &[u8]) -> Kind {
    match media_type {
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "text/markdown" | "text/x-markdown" => Kind::Markdown,
        "application/json" => Kind::Json,
        m if m.ends_with("+json") => Kind::Json,
        m if m.starts_with("text/") || m.ends_with("+xml") => Kind::Text,
        m if m.starts_with("image/")
            || m.starts_with("audio/")
            || m.starts_with("video/")
            || m.starts_with("font/")
            || matches!(
                m,
                "application/pdf" | "application/zip" | "application/gzip" | "application/wasm"
            ) =>
        {
            Kind::Binary
        }
        // `application/javascript`, `application/octet-stream` for a source
        // file, no content type at all: look at the bytes.
        _ => sniff(body),
    }
}

fn sniff(body: &[u8]) -> Kind {
    let head = &body[..body.len().min(8 * 1024)];
    if head.contains(&0) {
        return Kind::Binary;
    }
    // A cut at the download limit or at the sniff window can split a
    // character; only an error before the last few bytes means "not UTF-8".
    let text = match std::str::from_utf8(head) {
        Ok(text) => text,
        Err(e) if head.len() - e.valid_up_to() < 4 => {
            std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return Kind::Binary,
    };
    let start = text.trim_start().get(..512).unwrap_or(text.trim_start());
    let lower = start.to_ascii_lowercase();
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        Kind::Html
    } else {
        Kind::Text
    }
}

/// Text as lines: trailing whitespace trimmed, runs of blank lines collapsed
/// to one, blank lines at either end dropped, long lines wrapped.
fn to_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() && lines.last().is_none_or(String::is_empty) {
            continue;
        }
        if line.chars().count() <= MAX_LINE_CHARS {
            lines.push(line.to_owned());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(MAX_LINE_CHARS) {
            lines.push(chunk.iter().collect());
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// What converting a page produced.
struct Converted {
    title: Option<String>,
    markdown: String,
}

/// Convert an HTML page to Markdown, from its main content where it marks one.
fn html_to_markdown(html: &str, base: &Url) -> Converted {
    let converter = HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            hr_style: HrStyle::Dashes,
            link_style: LinkStyle::Inlined,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Options::default()
        })
        .skip_tags(SKIP_TAGS.to_vec())
        .build();
    let Ok(dom) = converter.html_to_tree(html) else {
        // html5ever recovers from any input; an error here is an I/O error of
        // an in-memory read, and the page as text is still better than nothing.
        return Converted {
            title: None,
            markdown: html.to_owned(),
        };
    };
    let title = find(&dom, &|n| is_element(n, "title"))
        .map(|n| collapse_whitespace(&text_of(&n)))
        .filter(|t| !t.is_empty());
    prepare(&dom, base);
    let main = find(&dom, &|n| is_element(n, "main"))
        .or_else(|| find(&dom, &|n| attr(n, "role").as_deref() == Some("main")))
        .or_else(|| find(&dom, &|n| is_element(n, "article")));
    let markdown = main
        .map(|main| converter.tree_to_markdown(&main))
        .filter(|md| md.trim().chars().count() >= THIN_MAIN_CHARS)
        .unwrap_or_else(|| converter.tree_to_markdown(&dom));
    Converted { title, markdown }
}

/// Rewrite the tree before it is converted: links and image sources made
/// absolute against the page's own URL, and images whose source is a `data:`
/// URI removed.
///
/// A fragment-only link (`#section`) is left as it is: on a documentation page
/// every heading has one, and an absolute URL for each would cost more than
/// the heading.
fn prepare(node: &Rc<Node>, base: &Url) {
    if let NodeData::Element { name, attrs, .. } = &node.data {
        let key = match &*name.local {
            "a" | "link" => Some("href"),
            "img" | "source" => Some("src"),
            _ => None,
        };
        if let Some(key) = key {
            for a in attrs.borrow_mut().iter_mut() {
                if &*a.name.local != key {
                    continue;
                }
                let value = a.value.trim();
                if value.starts_with('#') || value.starts_with("data:") {
                    continue;
                }
                if let Ok(absolute) = base.join(value) {
                    a.value = absolute.to_string().into();
                }
            }
        }
    }
    node.children
        .borrow_mut()
        .retain(|child| !inline_image(child) && !permalink(child));
    for child in node.children.borrow().iter() {
        prepare(child, base);
    }
}

/// An image whose source is a `data:` URI: its base64 would be the whole
/// window.
fn inline_image(node: &Node) -> bool {
    is_element(node, "img")
        && attr(node, "src").is_some_and(|src| src.trim_start().starts_with("data:"))
}

/// A heading's permalink: a link to a fragment of this page whose text is
/// nothing, or one symbol (`¶`, `§`, `#`). Documentation generators put one on
/// every heading — a hundred on a long reference page — and to a reader of the
/// Markdown each is `[¶](#section "Link to this heading")` of noise.
fn permalink(node: &Rc<Node>) -> bool {
    is_element(node, "a")
        && attr(node, "href").is_some_and(|href| href.trim_start().starts_with('#'))
        && text_of(node).trim().chars().count() <= 1
        && find(node, &|n| is_element(n, "img")).is_none()
}

fn is_element(node: &Node, tag: &str) -> bool {
    matches!(&node.data, NodeData::Element { name, .. } if &*name.local == tag)
}

fn attr(node: &Node, key: &str) -> Option<String> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|a| &*a.name.local == key)
            .map(|a| a.value.to_string()),
        _ => None,
    }
}

/// The first node, depth first, that `wanted` accepts.
fn find(node: &Rc<Node>, wanted: &dyn Fn(&Node) -> bool) -> Option<Rc<Node>> {
    if wanted(node) {
        return Some(Rc::clone(node));
    }
    node.children
        .borrow()
        .iter()
        .find_map(|child| find(child, wanted))
}

fn text_of(node: &Node) -> String {
    let mut out = String::new();
    if let NodeData::Text { contents } = &node.data {
        out.push_str(&contents.borrow());
    }
    for child in node.children.borrow().iter() {
        out.push_str(&text_of(child));
    }
    out
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(content_type: &str, body: &str) -> Response {
        Response {
            url: Url::parse("https://docs.example.com/guide/intro.html").unwrap(),
            redirects: Vec::new(),
            status: 200,
            content_type: Some(content_type.to_owned()),
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            truncated: false,
        }
    }

    const PAGE: &str = r##"<!doctype html><html><head><title> Intro —  Guide </title>
        <style>body { color: red }</style><script>var tracking = 1;</script></head>
        <body><nav><a href="/">Home</a> <a href="/api">API</a></nav>
        <main><h1 id="intro">Introduction</h1>
        <p>Install it with <code>npm i thing</code>, then read <a href="../api/index.html">the API</a>
        or jump to <a href="#setup">setup</a>.</p>
        <img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk">
        <img src="diagram.png" alt="Diagram">
        <h2 id="setup">Setup<a href="#setup" title="Link to this heading">¶</a><a href="#setup"></a></h2>
        <ul><li>one</li><li>two</li></ul>
        <pre><code>let x = 1;</code></pre>
        <p>Padding so the main element is plainly the content and not a shell around it.</p>
        </main><footer>© Example Corp</footer></body></html>"##;

    #[test]
    fn a_page_becomes_the_markdown_of_its_main_content() {
        let doc = Document::from_response(response("text/html; charset=utf-8", PAGE), false);
        let text = doc.lines.join("\n");
        assert_eq!(doc.kind, Kind::Html);
        assert_eq!(doc.media_type, "text/html");
        assert_eq!(doc.title.as_deref(), Some("Intro — Guide"));
        assert!(text.starts_with("# Introduction"), "{text}");
        // The heading's permalinks are gone; the link *to* it is kept.
        assert!(text.contains("\n## Setup\n"), "{text}");
        assert!(text.contains("- one"), "{text}");
        assert!(text.contains("```\nlet x = 1;\n```"), "{text}");
        // Relative links are made absolute; fragment links are left short.
        assert!(
            text.contains("[the API](https://docs.example.com/api/index.html)"),
            "{text}"
        );
        assert!(text.contains("[setup](#setup)"), "{text}");
        assert!(
            text.contains("![Diagram](https://docs.example.com/guide/diagram.png)"),
            "{text}"
        );
        // What is not content is gone: navigation, footer, scripts, styles,
        // and the inline image's base64.
        for gone in ["Home", "Example Corp", "tracking", "color: red", "base64"] {
            assert!(!text.contains(gone), "`{gone}` survived: {text}");
        }
        assert!(!doc.scripted);
    }

    #[test]
    fn a_page_without_main_content_falls_back_to_the_body_and_says_it_is_scripted() {
        let shell = r#"<html><head><title>App</title></head><body><div id="root"></div>
            <script src="/bundle.js"></script></body></html>"#;
        let doc = Document::from_response(response("text/html", shell), false);
        assert!(doc.scripted);
        assert_eq!(doc.title.as_deref(), Some("App"));
    }

    #[test]
    fn raw_keeps_the_html() {
        let doc = Document::from_response(response("text/html", PAGE), true);
        assert!(doc.lines.join("\n").contains("<nav>"));
    }

    #[test]
    fn json_is_pretty_printed_and_binary_is_not_shown() {
        let doc = Document::from_response(
            response("application/json", r#"{"a":[1,2],"b":"x"}"#),
            false,
        );
        assert_eq!(doc.kind, Kind::Json);
        assert_eq!(doc.lines.len(), 7, "{:?}", doc.lines);

        let pdf = Document::from_response(response("application/pdf", "%PDF-1.7"), false);
        assert_eq!(pdf.kind, Kind::Binary);
        assert!(pdf.lines.is_empty());
    }

    #[test]
    fn an_unlabelled_body_is_sniffed() {
        assert_eq!(classify("unknown", b"<!DOCTYPE html><html>"), Kind::Html);
        assert_eq!(
            classify("application/octet-stream", b"fn main() {}\n"),
            Kind::Text
        );
        assert_eq!(
            classify("application/octet-stream", b"\x89PNG\r\n\x1a\n\0\0"),
            Kind::Binary
        );
    }

    #[test]
    fn lines_are_trimmed_collapsed_and_wrapped() {
        let long = "x".repeat(MAX_LINE_CHARS * 2 + 5);
        let lines = to_lines(&format!("\n\na  \n\n\n\nb\n{long}\n\n"));
        assert_eq!(lines[..3], ["a", "", "b"]);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[5].len(), 5);
    }
}
