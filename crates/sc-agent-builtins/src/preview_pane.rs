//! `preview_pane` — a page beside the conversation (TODO "The preview pane").
//!
//! The one trait that contributes **no tool and no prompt**. What it declares is
//! a fact about the *chat*, not about the model: that this agent's work can be
//! looked at, and where. The admin's chat screen reads it off the agent and
//! offers a button that splits the screen — the conversation in a column on the
//! left, the configured URL in an `<iframe>` filling the rest — with the usual
//! three widths (full, tablet, phone) for looking at a layout, and a reload when
//! the agent finishes a turn, because that is when what the pane is showing has
//! just changed.
//!
//! An application's builder agent is created with it, pointed at the
//! application's own subdomain (`sc_app::preview_pane_url`), which is the case
//! that asked for it: someone building a React app wants to watch the app, not
//! read about it. It is a trait rather than a property of that one agent because
//! nothing in it is about building applications — an agent that edits a
//! documentation site, or drives a dashboard, wants the same screen.
//!
//! ## Why the URL may carry `{base}` and `{host}`
//!
//! An agent is stored once and read from wherever the admin happens to be open:
//! `localhost:3000` in development, the deployment's own domain in production.
//! The base domain is a server setting the agent record does not carry, so the
//! stored value may be a template, resolved in the browser:
//!
//! - `{base}` is the base domain applications are served under (with the
//!   admin's port), exactly as the applications list derives an app's link —
//!   so `//todo.{base}` is the `todo` application and `//{base}` the one on
//!   `@`, wherever the admin UI itself is served;
//! - `{host}` is the host the admin is open on, for a page beside the admin.
//!
//! Either follows the deployment instead of pinning it.
//!
//! ## What is refused
//!
//! The value ends up in an `iframe src`, so the schemes that can execute in the
//! admin's own origin are refused on save: `http:`, `https:`, a
//! protocol-relative `//host/path` and an absolute path are the whole of what is
//! accepted, and `javascript:` or `data:` is a rejected configuration rather
//! than something the admin UI has to remember to filter.

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::table::config_str;

/// The URL the pane opens on. May contain `{base}` (the base domain) and
/// `{host}` (the admin's own host), which the admin's browser resolves.
pub const CFG_URL: &str = "url";

/// Whether the pane reloads when the agent finishes a turn.
pub const CFG_RELOAD_ON_TURN: &str = "reload_on_turn";

/// [`CFG_RELOAD_ON_TURN`] when the admin sets nothing.
pub const DEFAULT_RELOAD_ON_TURN: bool = true;

/// Show a page beside the conversation.
pub struct PreviewPane;

#[async_trait::async_trait]
impl AgentTrait for PreviewPane {
    fn name(&self) -> &str {
        "preview_pane"
    }

    fn description(&self) -> &str {
        "Offer the chat a pane beside the conversation, showing one URL — the \
         application this agent builds, reloaded when a turn ends"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_URL, BasicType::Text)
                .label(
                    "URL shown beside the chat (`{base}` becomes the base domain, so \
                     `//shop.{base}` follows the deployment; `{host}` is the admin's own host)",
                )
                .required(),
            FormField::new(CFG_RELOAD_ON_TURN, BasicType::Bool)
                .label("Reload the pane when the agent finishes a turn")
                .default_value(DEFAULT_RELOAD_ON_TURN),
        ]
    }

    /// The URL is one a browser may put in an `iframe src`.
    ///
    /// Checked on save and again on load, like every other trait's: a stored
    /// agent whose pane could run script in the admin's origin should leave the
    /// live set with a reason, not be filtered out in the one screen that reads
    /// it.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        configured_url(check.config)?;
        Ok(())
    }

    /// None. The pane is the admin's screen, not the model's context: a model
    /// told "there is an iframe" would have nothing to do about it, and the
    /// system prompt is the one place where saying nothing is free.
    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        _args: &Json,
        _ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        Err(Error::invalid(format!(
            "`preview_pane` offers no tools, so there is no `{tool}` to call"
        )))
    }
}

/// The configured URL, checked.
///
/// The `{base}` and `{host}` placeholders are left in: resolving them is the
/// browser's, because the host is the one the admin is being read from and not
/// one the server knows.
pub fn configured_url(config: &Attrs) -> Result<String> {
    let url = config_str(config, CFG_URL);
    let url = url.trim();
    if url.is_empty() {
        return Err(Error::invalid(format!("`{CFG_URL}` is required")));
    }
    if !is_framable(url) {
        return Err(Error::invalid(format!(
            "`{CFG_URL}` is `{url}`, and a preview pane can only open an `http://` or \
             `https://` URL, a protocol-relative `//host/path`, or a path on the admin's \
             own origin (`/path`)"
        )));
    }
    Ok(url.to_owned())
}

/// Whether `url` is one the admin may put in an `iframe src`.
///
/// Deliberately a small allow-list rather than a scheme deny-list: a new
/// executable scheme is then something that has to be *added* here.
pub fn is_framable(url: &str) -> bool {
    let url = url.trim();
    if url.starts_with("//") {
        // Protocol-relative. `//` alone is not a host.
        return url.len() > 2;
    }
    if url.starts_with('/') {
        return true;
    }
    url.starts_with("http://") || url.starts_with("https://")
}

/// Whether this configuration reloads the pane when a turn ends.
pub fn reloads_on_turn(config: &Attrs) -> bool {
    config
        .get(CFG_RELOAD_ON_TURN)
        .and_then(Json::as_bool)
        .unwrap_or(DEFAULT_RELOAD_ON_TURN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(url: &str) -> Attrs {
        let mut attrs = Attrs::new();
        attrs.insert(CFG_URL.to_owned(), json!(url));
        attrs
    }

    #[test]
    fn a_pane_opens_on_a_url_a_browser_may_frame() {
        for url in [
            "https://shop.example.com",
            "http://localhost:5173/",
            "//todo.{host}",
            "//todo.{base}",
            "//{base}",
            "/apps/todo/",
        ] {
            assert_eq!(configured_url(&config(url)).unwrap(), url, "{url}");
        }
    }

    #[test]
    fn a_scheme_that_would_run_in_the_admins_origin_is_refused_on_save() {
        for url in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "",
        ] {
            let err = configured_url(&config(url)).unwrap_err();
            assert!(err.to_string().contains(CFG_URL), "{url}: {err}");
        }
    }

    #[test]
    fn the_pane_reloads_at_the_end_of_a_turn_unless_that_is_turned_off() {
        assert!(reloads_on_turn(&config("//a.{host}")));
        let mut off = config("//a.{host}");
        off.insert(CFG_RELOAD_ON_TURN.to_owned(), json!(false));
        assert!(!reloads_on_turn(&off));
    }
}
