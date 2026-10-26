//! `skill_file`: a SKILL.md, read from a file store and added to the agent's
//! context (TODO-agent-skills AS8.6).
//!
//! The counterpart of v1's "External Skill": the instructions of an Agent
//! Skill — a directory with a `SKILL.md` and the files it refers to — kept in
//! a file store rather than pasted into the agent's prompt, so the skill is
//! edited (or checked out of a repository) in one place and every agent that
//! names it reads the current version.
//!
//! - **The SKILL.md goes into the session header**, read once per
//!   conversation: a skill edited while a conversation is going on does not
//!   change it halfway.
//! - **The files it references** are offered through a `read_skill_file_<scope>`
//!   tool when [`CFG_READ_TOOL`] is on, and listed in the header so the model
//!   knows what it may ask for. The tool reads only inside the skill's
//!   directory.
//! - **Every read is the caller's**, under §9's file rule, as `coding`'s are:
//!   a skill in a directory the person chatting could not open is refused for
//!   them, rather than read with the server's authority.

use std::sync::Arc;

use sc_agent::{AgentTrait, RunCaller, SessionContext, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_files::{FileStore, check_access};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use crate::files::{
    ARG_PATH, FileScope, check_scope, check_tool_name, config_count, file_caller, scope_as_written,
    scope_fields, string_arg,
};
use crate::table::config_str;

/// The skill's main file, relative to the configured directory.
pub const CFG_SKILL_FILE: &str = "file";
/// Whether the model may read the files the skill references.
pub const CFG_READ_TOOL: &str = "read_tool";
/// The most characters one read returns.
pub const CFG_SKILL_MAX_CHARS: &str = "max_chars";

/// The main file when the admin names none.
pub const DEFAULT_SKILL_FILE: &str = "SKILL.md";
/// The default bound on what one read returns, the SKILL.md included.
pub const DEFAULT_SKILL_MAX_CHARS: u64 = 40_000;

/// The tool reading one of the skill's files, named by its scope.
pub fn tool_name(scope: &FileScope) -> String {
    format!("read_skill_file_{}", scope.slug())
}

/// A SKILL.md and the files it references.
pub struct SkillFile;

#[async_trait::async_trait]
impl AgentTrait for SkillFile {
    fn name(&self) -> &str {
        "skill_file"
    }

    fn description(&self) -> &str {
        "Follow the instructions of a SKILL.md kept in a file store"
    }

    fn config_spec(&self) -> Vec<FormField> {
        let mut fields = scope_fields();
        fields.extend([
            FormField::new(CFG_SKILL_FILE, BasicType::Text)
                .label("Skill file")
                .default_value(DEFAULT_SKILL_FILE)
                .sublabel("In the sub-directory above"),
            FormField::new(CFG_READ_TOOL, BasicType::Bool)
                .label("Let the model read the files the skill refers to")
                .default_value(true),
            FormField::new(CFG_SKILL_MAX_CHARS, BasicType::Int)
                .label("Most characters one file is read to")
                .default_value(DEFAULT_SKILL_MAX_CHARS as i64),
        ]);
        fields
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let scope = check_scope(check).await?;
        skill_path(check.config)?;
        config_count(check.config, CFG_SKILL_MAX_CHARS, DEFAULT_SKILL_MAX_CHARS)?;
        if read_tool(check.config) {
            check_tool_name(&tool_name(&scope))?;
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        if !read_tool(config) {
            return Vec::new();
        }
        let scope = scope_as_written(config);
        vec![ToolSpec::new(
            tool_name(&scope),
            format!(
                "Read a file of the skill whose instructions you were given (its SKILL.md \
                 lists them), in {}. Paths are relative to the skill's directory.",
                scope.label()
            ),
            json!({
                "type": "object",
                "properties": {
                    ARG_PATH: {
                        "type": "string",
                        "description": "The file, as the skill refers to it."
                    }
                },
                "required": [ARG_PATH],
                "additionalProperties": false,
            }),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let scope = skill_scope(config)?;
        let empty = Map::new();
        let rel = string_arg(args.as_object().unwrap_or(&empty), ARG_PATH)?;
        let rel = relative_to_skill(&skill_path(config)?, &rel);
        let limit = config_count(config, CFG_SKILL_MAX_CHARS, DEFAULT_SKILL_MAX_CHARS)? as usize;
        let text = read_as(ctx.catalog, ctx.caller, &scope, &rel).await?;
        let (contents, truncated) = bounded(&text, limit);
        Ok(json!({
            "path": rel,
            "contents": contents,
            "truncated": truncated,
        }))
    }

    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        let scope = skill_scope(config)?;
        let file = skill_path(config)?;
        let limit = config_count(config, CFG_SKILL_MAX_CHARS, DEFAULT_SKILL_MAX_CHARS)? as usize;
        let text = read_as(cx.catalog, cx.caller, &scope, &file)
            .await
            .map_err(|e| {
                Error::invalid(format!(
                    "agent `{}`: the skill file `{file}` in {} could not be read: {e}",
                    cx.agent,
                    scope.label()
                ))
            })?;
        let (contents, truncated) = bounded(&text, limit);
        let mut out = format!(
            "Follow the instructions of this skill (`{file}` in {}):\n\n{}",
            scope.label(),
            contents.trim()
        );
        if truncated {
            out.push_str("\n\n[The skill file is longer; the rest was left out.]");
        }
        let references = referenced_files(&text);
        if read_tool(config) && !references.is_empty() {
            out.push_str(&format!(
                "\n\nThe skill refers to these files, which `{}` reads: {}.",
                tool_name(&scope),
                references
                    .iter()
                    .map(|r| format!("`{r}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(Some(out))
    }
}

fn read_tool(config: &Attrs) -> bool {
    config
        .get(CFG_READ_TOOL)
        .and_then(Json::as_bool)
        .unwrap_or(true)
}

fn skill_scope(config: &Attrs) -> Result<FileScope> {
    crate::files::configured_scope(config)
}

/// The main file's path relative to the scope, checked as one.
fn skill_path(config: &Attrs) -> Result<String> {
    let file = match config_str(config, CFG_SKILL_FILE) {
        f if f.is_empty() => DEFAULT_SKILL_FILE.to_owned(),
        f => f,
    };
    // `resolve` against an unrooted scope is exactly the path check.
    FileScope {
        store: String::new(),
        root: String::new(),
    }
    .resolve(&file)
    .map_err(|_| {
        Error::invalid(format!(
            "`{CFG_SKILL_FILE}` must be a path inside the sub-directory, without `..`"
        ))
    })
}

/// A path the skill wrote, relative to the skill file's own directory, as a
/// path relative to the scope.
fn relative_to_skill(skill: &str, rel: &str) -> String {
    let rel = rel.trim().trim_start_matches("./");
    match skill.rsplit_once('/') {
        Some((dir, _)) if !rel.starts_with(&format!("{dir}/")) => format!("{dir}/{rel}"),
        _ => rel.to_owned(),
    }
}

/// Read `rel` (relative to the scope) as `caller`, under §9's rule.
async fn read_as(
    catalog: &Catalog,
    caller: &RunCaller,
    scope: &FileScope,
    rel: &str,
) -> Result<String> {
    let (store, floor): (Arc<dyn FileStore>, Option<u8>) = scope.connect(catalog).await?;
    let path = scope.resolve(rel)?;
    check_access(store.as_ref(), floor, &path, &file_caller(caller)).await?;
    let bytes = store.read(&path).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// At most `limit` characters of `text`, and whether anything was cut.
fn bounded(text: &str, limit: usize) -> (String, bool) {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => (text[..cut].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// The local files a Markdown document links to: every `](target)` whose
/// target is a relative path, in order, once each.
fn referenced_files(markdown: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = markdown;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else { break };
        let target = after[..end]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c| c == '<' || c == '>');
        let target = target.split('#').next().unwrap_or_default();
        let external = target.contains("://")
            || target.starts_with("mailto:")
            || target.starts_with('/')
            || target.is_empty();
        if !external && !target.split('/').any(|s| s == "..") {
            let target = target.trim_start_matches("./").to_owned();
            if !out.contains(&target) {
                out.push(target);
            }
        }
        rest = &after[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_a_skill_links_to_are_its_relative_links() {
        let md = "See [the form](forms/FORM.md) and [ref](./reference.md#top), \
                  [web](https://example.com/x.md), [up](../secret.md), \
                  [again](forms/FORM.md) and ![img](img/a.png \"A\").";
        assert_eq!(
            referenced_files(md),
            ["forms/FORM.md", "reference.md", "img/a.png"]
        );
    }

    #[test]
    fn a_path_is_relative_to_the_skill_files_directory() {
        assert_eq!(relative_to_skill("SKILL.md", "./a.md"), "a.md");
        assert_eq!(
            relative_to_skill("pdf/SKILL.md", "forms.md"),
            "pdf/forms.md"
        );
        assert_eq!(
            relative_to_skill("pdf/SKILL.md", "pdf/forms.md"),
            "pdf/forms.md"
        );
    }

    #[test]
    fn a_long_file_is_cut_at_a_character_boundary() {
        assert_eq!(bounded("héllo", 2), ("hé".to_owned(), true));
        assert_eq!(bounded("hi", 5), ("hi".to_owned(), false));
    }
}
