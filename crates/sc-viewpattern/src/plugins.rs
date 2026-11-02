//! What an installed plugin brings to a rendered document besides its view
//! patterns: the **headers** it declares and the **`public/`** directory they
//! point into (TODO "Saltcorn UI" §6, 11.2, 11.3).
//!
//! A v1 plugin says, as data:
//!
//! ```js
//! headers: [
//!   { script: "/plugins/public/kanban@0.5.5/dragula.min.js", onlyViews: ["Kanban"] },
//!   { css: "/plugins/public/kanban@0.5.5/dragula.min.css", onlyViews: ["Kanban"] },
//! ]
//! ```
//!
//! and ships the files in its package's `public/`. So the document builder puts
//! a plugin's headers into the `<head>` of a page that rendered one of the
//! patterns it names — or of every page, for a header that names none — and the
//! application serves `/plugins/public/<name>@<version>/*` out of that
//! directory.
//!
//! The set is installed **whole** on every module change, beside the pattern
//! registry, by whoever loads the modules; this layer knows nothing about npm.

// A value for a double-quoted attribute: the one escape (Q4.5).
use sc_expr::html::escape;
use std::path::PathBuf;
use std::sync::RwLock;

use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};

/// One header a plugin declares: a script or a stylesheet, for the patterns it
/// names or for every page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHeader {
    /// A `<script src>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// A `<link rel="stylesheet" href>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub css: Option<String>,
    /// v1's `onlyViews`: the **pattern** names whose rendering wants it. `None`
    /// is every document; an empty list is none, as in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_views: Option<Vec<String>>,
}

/// One installed plugin's headers and public directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginAssets {
    /// The module, by package name — for a sentence.
    pub module: String,
    /// The names its public URLs may use: v1 builds them from the plugin's own
    /// name (`kanban`, not `@saltcorn/kanban`).
    pub names: Vec<String>,
    /// The installed version.
    pub version: String,
    /// Its package's `public/`, when it has one.
    pub public_dir: Option<PathBuf>,
    /// Its headers, in the order it declared them.
    pub headers: Vec<PluginHeader>,
}

static INSTALLED: RwLock<Vec<PluginAssets>> = RwLock::new(Vec::new());

/// Install the plugins' assets, replacing whatever was installed.
pub fn install_plugin_assets(assets: Vec<PluginAssets>) -> Result<()> {
    let mut guard = INSTALLED
        .write()
        .map_err(|_| Error::msg("the plugin asset registry lock is poisoned"))?;
    *guard = assets;
    Ok(())
}

/// The installed plugins' assets.
pub fn installed_plugin_assets() -> Vec<PluginAssets> {
    match INSTALLED.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The `<head>` tags for a document that rendered `patterns`: every header
/// whose `onlyViews` names one of them, or that names none — de-duplicated, in
/// the order the plugins and their manifests declare them (11.3).
pub fn plugin_header_tags(assets: &[PluginAssets], patterns: &[String]) -> String {
    let mut tags: Vec<String> = Vec::new();
    for header in assets.iter().flat_map(|a| &a.headers) {
        if let Some(only) = &header.only_views
            && !only.iter().any(|name| patterns.contains(name))
        {
            continue;
        }
        let tag = match (&header.script, &header.css) {
            (Some(src), _) => format!("<script src=\"{}\"></script>", escape(src)),
            (None, Some(href)) => format!("<link rel=\"stylesheet\" href=\"{}\">", escape(href)),
            (None, None) => continue,
        };
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags.iter().map(|tag| format!("{tag}\n")).collect()
}

/// The file `/plugins/public/<plugin>/<parts…>` names, if a plugin by that name
/// has one there — and whether the version in the URL is the installed one,
/// which is what makes a long cache safe.
///
/// `<plugin>` is `name@version` or `name`. Confined the way a local file store
/// confines a path: no empty, `.` or `..` part, and the resolved file — symlinks
/// followed — still inside the plugin's `public/`.
pub fn plugin_public_file(
    assets: &[PluginAssets],
    plugin: &str,
    parts: &[String],
) -> Option<(PathBuf, bool)> {
    let (name, version) = match plugin.rsplit_once('@') {
        Some((name, version)) if !name.is_empty() => (name, Some(version)),
        _ => (plugin, None),
    };
    let asset = assets.iter().find(|a| a.names.iter().any(|n| n == name))?;
    let dir = asset.public_dir.as_ref()?;
    if parts.is_empty() || !crate::framework::confined(parts) {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let file = parts
        .iter()
        .fold(root.clone(), |dir, part| dir.join(part))
        .canonicalize()
        .ok()?;
    (file.starts_with(&root) && file.is_file())
        .then(|| (file, version == Some(asset.version.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kanban(public_dir: Option<PathBuf>) -> PluginAssets {
        PluginAssets {
            module: "@saltcorn/kanban".into(),
            names: vec!["kanban".into()],
            version: "0.5.5".into(),
            public_dir,
            headers: vec![
                PluginHeader {
                    script: Some("/plugins/public/kanban@0.5.5/dragula.min.js".into()),
                    only_views: Some(vec!["Kanban".into(), "KanbanAllocator".into()]),
                    ..PluginHeader::default()
                },
                PluginHeader {
                    css: Some("/plugins/public/kanban@0.5.5/dragula.min.css".into()),
                    only_views: Some(vec!["Kanban".into()]),
                    ..PluginHeader::default()
                },
            ],
        }
    }

    #[test]
    fn a_header_is_injected_for_the_patterns_it_names_or_for_every_page() {
        let mut everywhere = kanban(None);
        everywhere.module = "@saltcorn/other".into();
        everywhere.headers = vec![
            PluginHeader {
                script: Some("/plugins/public/other@1.0.0/a.js?x=1&y=\"2\"".into()),
                ..PluginHeader::default()
            },
            // The same tag again, from another header: once.
            PluginHeader {
                script: Some("/plugins/public/other@1.0.0/a.js?x=1&y=\"2\"".into()),
                only_views: Some(vec!["List".into()]),
                ..PluginHeader::default()
            },
            // Names nothing: nowhere, as in v1.
            PluginHeader {
                css: Some("/never.css".into()),
                only_views: Some(Vec::new()),
                ..PluginHeader::default()
            },
        ];
        let assets = [kanban(None), everywhere];

        let list = plugin_header_tags(&assets, &["List".to_owned()]);
        assert_eq!(
            list,
            "<script src=\"/plugins/public/other@1.0.0/a.js?x=1&amp;y=&quot;2&quot;\"></script>\n"
        );
        let board = plugin_header_tags(&assets, &["Filter".to_owned(), "Kanban".to_owned()]);
        let lines: Vec<&str> = board.lines().collect();
        assert_eq!(
            lines,
            [
                "<script src=\"/plugins/public/kanban@0.5.5/dragula.min.js\"></script>",
                "<link rel=\"stylesheet\" href=\"/plugins/public/kanban@0.5.5/dragula.min.css\">",
                "<script src=\"/plugins/public/other@1.0.0/a.js?x=1&amp;y=&quot;2&quot;\"></script>",
            ]
        );
        let allocator = plugin_header_tags(&assets, &["KanbanAllocator".to_owned()]);
        assert!(allocator.contains("dragula.min.js") && !allocator.contains("dragula.min.css"));
    }

    #[test]
    fn a_public_file_is_found_by_the_plugins_name_and_stays_inside_public() {
        let root = std::env::temp_dir().join(format!("sc-plugin-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let public = root.join("kanban/public");
        std::fs::create_dir_all(public.join("css")).unwrap();
        std::fs::write(public.join("dragula.min.js"), "/* js */").unwrap();
        std::fs::write(public.join("css/all.css"), "/* css */").unwrap();
        std::fs::write(root.join("kanban/package.json"), "{}").unwrap();
        let assets = [kanban(Some(public.clone()))];
        let parts = |p: &str| p.split('/').map(str::to_owned).collect::<Vec<_>>();

        let (file, current) =
            plugin_public_file(&assets, "kanban@0.5.5", &parts("dragula.min.js")).unwrap();
        assert!(file.ends_with("dragula.min.js") && current);
        // Another version tag still finds the installed file, uncached.
        let (_, current) =
            plugin_public_file(&assets, "kanban@0.4.0", &parts("css/all.css")).unwrap();
        assert!(!current);
        assert!(plugin_public_file(&assets, "kanban", &parts("css/all.css")).is_some());

        for bad in [
            "../package.json",
            "css/../../package.json",
            "",
            "missing.js",
            "css",
        ] {
            assert!(
                plugin_public_file(&assets, "kanban@0.5.5", &parts(bad)).is_none(),
                "{bad:?}"
            );
        }
        assert!(plugin_public_file(&assets, "mind-map@0.3.4", &parts("dragula.min.js")).is_none());
        assert!(plugin_public_file(&[kanban(None)], "kanban", &parts("dragula.min.js")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
