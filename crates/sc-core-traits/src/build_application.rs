//! `build_application` — build the app whose source the agent is editing (§11.3).
//!
//! The trait that closes the loop. An agent that can read, search and edit a
//! project can produce a change that does not compile, and a model that is not
//! told cannot fix it. So the build's **diagnostics are the tool result**: a
//! failed build is not an exception here, it is the most useful answer this tool
//! ever returns.
//!
//! Three decisions:
//!
//! - **A failed build comes back as a result, not an error.** `built: false` with
//!   the tools' own output and the file/line/message triples parsed out of it.
//!   The loop would have turned an `Err` into a tool result anyway (§11.2), but
//!   flattened to a sentence; the model can act on a list of diagnostics, and the
//!   admin watching the transcript can read one.
//! - **It builds the *stored* application**, resolved by subdomain from
//!   `_fd_applications` and built through the same `sc_app::build_application`
//!   the admin's Build button runs — the client is regenerated, a React app's
//!   runtime is regenerated, the same bundler runs over the same tree. A second
//!   build path would be a second set of results to explain.
//! - **It does not mount what it built.** Mounting is the server's (§13.2 — "the
//!   mount registry is live"), and an agent's build is a *check*: the question is
//!   "does this compile?", not "serve this to the world". An admin publishes what
//!   the agent produced by pressing Build, which is also where they get to look
//!   at the diff first.

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_app::{
    AppSource, Application, BuildReport, app_source_from_config, build_diagnostics,
    load_application_by_subdomain,
};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::coding::is_admin;
use crate::files::slugify;
use crate::table::{arguments, config_str};

/// The application this trait builds, by subdomain — its unique key (§13.2).
pub const CFG_APPLICATION: &str = "application";

/// Build one configured application.
pub struct BuildApplication;

/// The tool one `build_application` instance offers, derived from its app.
pub fn tool_name(application: &str) -> String {
    format!("build_{}", slugify(application))
}

#[async_trait::async_trait]
impl AgentTrait for BuildApplication {
    fn name(&self) -> &str {
        "build_application"
    }

    fn description(&self) -> &str {
        "Build one application from its source, returning the build's diagnostics"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_APPLICATION, BasicType::Text)
                .label("Application (subdomain)")
                .required(),
        ]
    }

    /// The application must exist, and it must be one that builds.
    ///
    /// Checked on save and again on load, so an agent pointed at a deleted app —
    /// or at one whose framework has no build step, which is the same mistake
    /// found later — leaves the live set with a reason rather than failing when
    /// the model calls the tool.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let subdomain = configured_application(check.config)?;
        resolve_application(check.catalog, &subdomain).await?;
        Ok(())
    }

    /// Withheld from a run whose caller is not an admin ([`is_admin`]): a build
    /// runs the project's own scripts and bundler configuration on the server,
    /// and whoever can edit the source decides what those are. Listed for no
    /// run (validation, the admin UI), it is there.
    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        if cx.caller.is_some_and(|caller| !is_admin(caller)) {
            return Vec::new();
        }
        let configured = config_str(config, CFG_APPLICATION);
        vec![ToolSpec::new(
            tool_name(&configured),
            format!(
                "Build the `{configured}` application from its source. Returns \
                 whether the build succeeded, the build tools' output, and — when \
                 it failed — the diagnostics with their file, line and message. \
                 Run this after changing the source to find out whether the change \
                 compiles; a failed build's diagnostics are what to fix."
            ),
            json!({
                "type": "object",
                "properties": {},
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
        arguments(args, &[])?;
        if !is_admin(ctx.caller) {
            return Err(Error::invalid(format!(
                "agent `{}` may not build the application for this user: a build runs the \
                 project's own code on the server, so it is only for runs whose caller is an \
                 administrator",
                ctx.agent
            )));
        }
        let subdomain = configured_application(config)?;
        let (app, source) = resolve_application(ctx.catalog, &subdomain).await?;

        // Everything from here is *news about the build*, including its failure,
        // so it is reported rather than raised.
        match sc_app::build_application(ctx.catalog, &app, &source, ctx.triggers).await {
            Ok(report) => Ok(json!({
                "application": subdomain,
                "built": true,
                "output": success_log(&report),
                "diagnostics": build_diagnostics(&format!("{}\n{}", report.stdout, report.stderr)),
            })),
            Err(e) => {
                let output = e.to_string();
                Ok(json!({
                    "application": subdomain,
                    "built": false,
                    "output": output,
                    "diagnostics": build_diagnostics(&output),
                }))
            }
        }
    }
}

/// The application served at `subdomain`, and the source it builds from.
///
/// Shared with `coding`'s `application` setting (TODO 6.1), which is validated
/// the same way: resolving the source is what says "this framework builds from a
/// file store", and a static app has nothing to build — the admin should hear
/// that on save, not from a tool call.
pub(crate) async fn resolve_application(
    catalog: &sc_catalog::Catalog,
    subdomain: &str,
) -> Result<(Application, AppSource)> {
    let app = load_application_by_subdomain(catalog, subdomain)
        .await?
        .ok_or_else(|| Error::invalid(format!("no application is served at `{subdomain}`")))?;
    let source = app_source_from_config(&app.framework)?;
    Ok((app, source))
}

/// The configured application's subdomain.
fn configured_application(config: &Attrs) -> Result<String> {
    let name = config_str(config, CFG_APPLICATION);
    if name.is_empty() {
        return Err(Error::invalid(format!("`{CFG_APPLICATION}` is required")));
    }
    Ok(name)
}

/// What a successful build said, in the order it said it.
///
/// Kept even on success: bundlers report warnings here, and a model told only
/// "built" would never see them.
pub(crate) fn success_log(report: &BuildReport) -> String {
    let mut parts = Vec::new();
    if let Some(install) = &report.install_log {
        parts.push(install.trim().to_owned());
    }
    parts.push(report.stdout.trim().to_owned());
    parts.push(report.stderr.trim().to_owned());
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_application() {
        assert_eq!(tool_name("todo-app"), "build_todo_app");
    }

    #[test]
    fn a_failed_build_is_reported_with_the_diagnostics_the_shared_parser_finds() {
        // The parser itself is `sc_app::build_diagnostics`' to test; what this
        // asserts is that this trait still reaches it, so the tool result an
        // agent reads and the one an MCP client reads name the same lines.
        let found = build_diagnostics("src/App.tsx(12,5): error TS2322: Type 'x' is bad.");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["file"], "src/App.tsx");
    }
}
