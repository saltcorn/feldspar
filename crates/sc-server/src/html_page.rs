//! The one way this server answers with an HTML page it built
//! (TODO-type-hardening H3.7, security review 2026-11 A10).
//!
//! `axum::response::Html` takes any string, so a page assembled with
//! `format!` from a value somebody forgot to escape was one `Html(...)` away
//! from the browser. It is a disallowed type (clippy.toml); a page goes out
//! through [`html`], which takes an [`SafeHtml`] — escaped text, a trusted
//! literal, generated markup, or a [`safe_html!`](sc_expr::safe_html) of
//! those. A page an application serves is the application's (`AppResponse`
//! bytes), not built here.

use sc_expr::html::SafeHtml;

/// `page` as an HTML response body.
#[allow(
    clippy::disallowed_types,
    reason = "the one sink for a page this server builds: it takes SafeHtml"
)]
pub(crate) fn html(page: SafeHtml) -> axum::response::Html<String> {
    axum::response::Html(page.into_string())
}
