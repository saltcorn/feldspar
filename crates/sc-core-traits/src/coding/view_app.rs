//! `view_app` — look at the application the run built, in a headless browser
//! (TODO §7b, 6b.9).
//!
//! A green `check` mounts the build as the run's **preview** (see
//! [`super::check`]). This tool opens that preview in the server's headless
//! Chromium, in a browser context of the run's own, **as the run's caller**: the
//! person chatting, or for a run nobody is present for, the account the
//! [`CFG_VIEW_APP_USER`] setting names.
//!
//! **A run that has built nothing yet looks at the live build.** Its first call
//! mounts the preview from what the application's last build left on disk —
//! what the live mount is serving — so an agent can see the page it was asked
//! about *before* changing it, which is where most visual work starts. The
//! result says so, and a later green `check` re-mounts the same preview with
//! the run's own build.
//!
//! One action per call, over one page:
//!
//! - `goto(path)`, `click(ref)`, `fill(ref, text)`, `press(key)`,
//!   `wait_for(text | ref, timeout)` and `snapshot()`, each answered with a
//!   compact **accessibility snapshot** whose interactive elements carry refs
//!   (`@e12`) for the next call. It is text, a few hundred tokens a page, and
//!   works for any model.
//! - `screenshot(full_page?)`, a JPEG attached to the result, offered only when
//!   the model has `vision`.
//!
//! In a `plan` run only the looking actions are offered — `goto`, `wait_for`,
//! `snapshot` and `screenshot` — because a plan is written from what is there,
//! and `click`, `fill` and `press` change it.
//!
//! Any call may pass `signed_out`, and looks as a visitor who is not signed
//! in: the browser context drops its session for that call (and a later call
//! without it signs back in). It needs no user, so a triggered run with no
//! [`CFG_VIEW_APP_USER`] can still look at the public pages.
//!
//! Every result also carries the URL, the last document's HTTP status, and the
//! console errors and failed requests since the previous call.
//!
//! **Its data is the live data.** The preview talks to the application's real
//! tables as the caller, so `click` and `fill` on a form write real rows. The
//! description says so.

use std::time::Duration;

use sc_agent::{
    BrowserAction, BrowserRequest, Elidable, PreviewInfo, RunMode, TraitCheck, TraitContext,
};
use sc_auth::User;
use sc_error::{Error, Result};
use sc_llm::{ImagePart, ToolSpec};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use super::check::{checked_application, configured_application};
use super::{CFG_MAY_VIEW_APP, may};
use crate::files::{FileScope, config_count};
use crate::table::config_str;

/// The user a run with no caller (one a trigger started) looks at the
/// application as, by email.
pub const CFG_VIEW_APP_USER: &str = "view_app_user";

/// How long one `view_app` call may take, in seconds.
pub const CFG_VIEW_APP_TIMEOUT: &str = "view_app_timeout";

/// [`CFG_VIEW_APP_TIMEOUT`] when the admin sets none.
pub const DEFAULT_VIEW_APP_TIMEOUT: u64 = 30;

/// The tool one configured scope offers.
pub fn tool_name(scope: &FileScope) -> String {
    format!("view_app_{}", scope.slug())
}

/// The settings, after `check`'s.
pub fn config_fields() -> Vec<FormField> {
    vec![
        FormField::new(CFG_MAY_VIEW_APP, BasicType::Bool)
            .label(
                "May look at the application's preview in a headless browser, as the person \
                 chatting (needs the application setting and a browser on the server)",
            )
            .default_value(false),
        FormField::new(CFG_VIEW_APP_USER, BasicType::Text).label(
            "User a triggered run looks at the application as (email; a low-privilege account)",
        ),
        FormField::new(CFG_VIEW_APP_TIMEOUT, BasicType::Int)
            .label("view_app timeout per action (seconds)")
            .default_value(DEFAULT_VIEW_APP_TIMEOUT as i64),
    ]
}

/// The grant needs an application and a browser; the user must exist.
pub async fn validate(check: &TraitCheck<'_>) -> Result<()> {
    config_count(check.config, CFG_VIEW_APP_TIMEOUT, DEFAULT_VIEW_APP_TIMEOUT)?;
    let email = config_str(check.config, CFG_VIEW_APP_USER);
    if !email.is_empty()
        && sc_auth::load_user_by_email(check.catalog, &email)
            .await?
            .is_none()
    {
        return Err(Error::invalid(format!(
            "`{CFG_VIEW_APP_USER}` names `{email}`, and no user has that email"
        )));
    }
    if !may(check.config, CFG_MAY_VIEW_APP) {
        return Ok(());
    }
    if configured_application(check.config).is_none() {
        return Err(Error::invalid(format!(
            "`{CFG_MAY_VIEW_APP}` needs the `{}` setting: view_app looks at that \
             application's preview",
            crate::CFG_APPLICATION
        )));
    }
    if let Err(reason) = &check.host.browser {
        return Err(Error::invalid(format!(
            "`{CFG_MAY_VIEW_APP}` needs a headless browser on the server, and there is none: \
             {reason}"
        )));
    }
    Ok(())
}

/// The actions that change nothing: all a `plan` run is offered.
const LOOKING: [&str; 3] = ["goto", "wait_for", "snapshot"];

/// The tool, with `screenshot` only for a model that takes images, and only
/// the looking actions outside `act`.
pub fn spec(scope: &FileScope, config: &Attrs, vision: bool, mode: RunMode) -> ToolSpec {
    let application = configured_application(config).unwrap_or_default();
    let acts = mode == RunMode::Act;
    let mut actions = LOOKING.to_vec();
    if acts {
        actions.splice(1..1, ["click", "fill", "press"]);
    }
    if vision {
        actions.push("screenshot");
    }
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Open `{application}` in a headless browser, signed in as the user you are working \
             for, and do one action per call. It shows this run's last green check, or the \
             live build before there is one. Each result has the URL, the HTTP status, an \
             accessibility snapshot of the page whose interactive elements carry refs (`@e12`) \
             for the next action, and any console errors and failed requests since the last \
             call. Pass signed_out: true to look as a visitor who is not signed in; every \
             call that should stay signed out needs it, and a call without it signs back in \
             and reloads the page.{}{}",
            if acts {
                " The data is live: click and fill on a form write real rows."
            } else {
                " Only looking actions are available here."
            },
            if vision {
                " screenshot returns an image of the page."
            } else {
                ""
            }
        ),
        parameters(actions, vision, acts),
    )
}

/// The tool's arguments: `full_page` only beside `screenshot`, `key` only
/// beside `press`.
fn parameters(actions: Vec<&str>, vision: bool, acts: bool) -> Json {
    let mut properties = json!({
        "action": {"type": "string", "enum": actions},
        "path": {"type": "string", "description": "For goto: the path to open, e.g. /tasks"},
        "ref": {"type": "string", "description": "For click, fill, wait_for: an @e ref from the last snapshot"},
        "text": {"type": "string", "description": "For fill: the text to type; for wait_for: text to wait for"},
        "timeout": {"type": "integer", "description": "For wait_for: seconds to wait"},
        "signed_out": {"type": "boolean", "description": "Look as a visitor who is not signed in, for this call"},
    });
    if let Some(map) = properties.as_object_mut() {
        if acts {
            map.insert(
                "key".to_owned(),
                json!({"type": "string", "description": "For press: a key, e.g. Enter, Tab or Escape"}),
            );
        }
        if vision {
            map.insert(
                "full_page".to_owned(),
                json!({"type": "boolean", "description": "For screenshot: the whole page, not just the viewport"}),
            );
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": ["action"],
        "additionalProperties": false,
    })
}

/// The action the arguments ask for.
fn action(args: &Json, limit: Duration) -> Result<BrowserAction> {
    let text = |key: &str| -> Result<String> {
        args.get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{}` needs `{key}`",
                    args.get("action").and_then(Json::as_str).unwrap_or("?")
                ))
            })
    };
    let optional = |key: &str| args.get(key).and_then(Json::as_str).map(str::to_owned);
    Ok(
        match args
            .get("action")
            .and_then(Json::as_str)
            .unwrap_or("snapshot")
        {
            "goto" => BrowserAction::Goto {
                path: text("path")?,
            },
            "click" => BrowserAction::Click {
                reference: text("ref")?,
            },
            "fill" => BrowserAction::Fill {
                reference: text("ref")?,
                text: args
                    .get("text")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            },
            "press" => BrowserAction::Press { key: text("key")? },
            "wait_for" => {
                let (text, reference) = (optional("text"), optional("ref"));
                if text.is_none() && reference.is_none() {
                    return Err(Error::invalid("`wait_for` needs `text` or `ref`"));
                }
                let seconds = args.get("timeout").and_then(Json::as_u64).unwrap_or(10);
                BrowserAction::WaitFor {
                    text,
                    reference,
                    timeout: Duration::from_secs(seconds).min(limit),
                }
            }
            "snapshot" => BrowserAction::Snapshot,
            "screenshot" => BrowserAction::Screenshot {
                full_page: args
                    .get("full_page")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            },
            other => {
                return Err(Error::invalid(format!(
                    "`{other}` is not a view_app action; use goto, click, fill, press, wait_for \
                     or snapshot"
                )));
            }
        },
    )
}

/// The run's preview of `application`, and whether it was mounted just now
/// from the live build because the run had none (see the module notes).
async fn preview(application: &str, ctx: &TraitContext<'_>) -> Result<(PreviewInfo, bool)> {
    let previews = ctx.require_previews()?;
    if let Some(preview) = previews.preview(ctx.run, application) {
        return Ok((preview, false));
    }
    let never_built = |reason: &dyn std::fmt::Display| {
        Error::invalid(format!(
            "no preview of `{application}` is mounted for this run, and its live build cannot \
             be previewed ({reason}): run the check tool, and a green build is mounted as the \
             preview"
        ))
    };
    let (_, source) = checked_application(ctx.catalog, application).await?;
    // An application with nothing to build is previewed as it is served, and
    // the directory is not read.
    let output_dir = match source {
        None => std::path::PathBuf::new(),
        // What is on disk was not necessarily built by the server: below admin
        // it may only be looked at once an admin's build has mounted it as the
        // run's preview, since mounting a server-rendered bundle runs its
        // server entry with admin authority (security review #16).
        Some(_) if !super::is_admin(ctx.caller) => {
            return Err(Error::invalid(format!(
                "no preview of `{application}` is mounted for this run, and its build output \
                 on disk is only previewed for a run whose caller is an administrator: \
                 report that an administrator can build the application"
            )));
        }
        Some(source) => {
            let dir = sc_app::app_output_dir(ctx.catalog, &source).map_err(|e| never_built(&e))?;
            if !dir.is_dir() {
                return Err(never_built(&"it has never been built"));
            }
            dir
        }
    };
    let preview = previews
        .mount_preview(ctx.run, application, &output_dir)
        .await
        .map_err(|e| never_built(&e))?;
    Ok((preview, true))
}

/// Whom the run looks at the application as.
pub(super) async fn viewer(config: &Attrs, ctx: &TraitContext<'_>) -> Result<User> {
    if let Some(user) = &ctx.caller.user {
        return Ok(user.clone());
    }
    // A visitor who is not signed in is somebody: the configured account is
    // for runs nobody is chatting in, and lending it to a visitor would hand
    // them its access (security review #20).
    if ctx.caller.is_anonymous() {
        return Err(Error::invalid(format!(
            "agent `{}` may not use view_app for a visitor who is not signed in: it signs in \
             to the application as a user, and this visitor is none",
            ctx.agent
        )));
    }
    let email = config_str(config, CFG_VIEW_APP_USER);
    if email.is_empty() {
        return Err(Error::invalid(format!(
            "agent `{}` may not use view_app in this run: nobody is chatting, so there is no \
             user to look at the application as, and its `coding` trait has no \
             `{CFG_VIEW_APP_USER}`. Report that an administrator can set it to a \
             low-privilege account.",
            ctx.agent
        )));
    }
    sc_auth::load_user_by_email(ctx.catalog, &email)
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_VIEW_APP_USER}` names `{email}`, and no user has that email"
            ))
        })
}

/// Perform one action and report the page.
pub async fn call(config: &Attrs, args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let application = configured_application(config).ok_or_else(|| {
        Error::invalid(format!(
            "view_app needs the `coding` trait's `{}` setting",
            crate::CFG_APPLICATION
        ))
    })?;
    let limit = Duration::from_secs(config_count(
        config,
        CFG_VIEW_APP_TIMEOUT,
        DEFAULT_VIEW_APP_TIMEOUT,
    )?);
    let action = action(args, limit)?;
    let looks = matches!(
        action,
        BrowserAction::Goto { .. }
            | BrowserAction::WaitFor { .. }
            | BrowserAction::Snapshot
            | BrowserAction::Screenshot { .. }
    );
    if ctx.mode != RunMode::Act && !looks {
        return Err(Error::invalid(format!(
            "`{}` changes the page, and a `{}` run only looks: use goto, wait_for, snapshot or \
             screenshot",
            action.name(),
            ctx.mode
        )));
    }
    let signed_out = args
        .get("signed_out")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let (preview, live) = preview(&application, ctx).await?;
    let user = if signed_out {
        None
    } else {
        Some(viewer(config, ctx).await?)
    };
    let name = action.name();
    let target = match &action {
        BrowserAction::Goto { path } => format!(" {path}"),
        BrowserAction::Click { reference } | BrowserAction::Fill { reference, .. } => {
            format!(" {reference}")
        }
        BrowserAction::Press { key } => format!(" {key}"),
        _ => String::new(),
    };
    let report = ctx
        .require_browser()?
        .act(BrowserRequest {
            run: ctx.run,
            preview: &preview,
            user: user.as_ref(),
            action,
            timeout: limit,
        })
        .await?;

    let mut out = format!(
        "view_app {name}{target}{}\nurl: {}",
        if signed_out { " (signed out)" } else { "" },
        report.url
    );
    if let Some(status) = report.status {
        out.push_str(&format!(" (status {status})"));
    }
    if live {
        out.push_str(&format!(
            "\nnote: this run has no green check yet, so this is the live build of \
             `{application}`; after one, it is the run's own build."
        ));
    }
    if let Some(note) = &report.note {
        out.push_str(&format!("\nnote: {note}"));
    }
    for (heading, lines) in [
        ("console errors", &report.console_errors),
        ("failed requests", &report.failed_requests),
    ] {
        if !lines.is_empty() {
            out.push_str(&format!("\n{heading}:"));
            for line in lines {
                out.push_str(&format!("\n- {line}"));
            }
        }
    }
    if let Some(shot) = report.screenshot {
        out.push_str(&format!(
            "\nscreenshot: attached ({} KB JPEG)",
            shot.len().div_ceil(1024)
        ));
        ctx.attach_image(ImagePart::new("image/jpeg", shot));
    }
    if let Some(snapshot) = &report.snapshot {
        out.push_str(&format!("\nsnapshot:\n{snapshot}"));
    }
    Ok(Json::String(out))
}

/// The action and its target: a repeated look at the same thing is a repeat.
pub fn fingerprint(args: &Json) -> Json {
    let mut out = serde_json::Map::new();
    for key in ["action", "path", "ref", "key", "text", "signed_out"] {
        if let Some(value) = args.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    Json::Object(out)
}

/// An old result, as one line: `[elided snapshot of /tasks: 41 lines]`, and a
/// screenshot as a stub.
pub fn elide(old: &Elidable<'_>) -> String {
    let path = old
        .content
        .lines()
        .find_map(|line| line.strip_prefix("url: "))
        .map(|url| {
            let url = url.split(" (status").next().unwrap_or(url);
            match url.split_once("://") {
                Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]).to_owned(),
                None => url.to_owned(),
            }
        })
        .unwrap_or_else(|| "the page".to_owned());
    if old.images > 0 {
        return format!("[elided screenshot of {path}]");
    }
    match old.content.split_once("\nsnapshot:\n") {
        Some((_, snapshot)) => format!(
            "[elided snapshot of {path}: {} lines]",
            snapshot.lines().count()
        ),
        None => old.default_stub(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_make_one_action() {
        let limit = Duration::from_secs(30);
        assert_eq!(
            action(&json!({"action": "goto", "path": "/tasks"}), limit).unwrap(),
            BrowserAction::Goto {
                path: "/tasks".to_owned()
            }
        );
        assert_eq!(
            action(
                &json!({"action": "wait_for", "text": "Saved", "timeout": 99}),
                limit
            )
            .unwrap(),
            BrowserAction::WaitFor {
                text: Some("Saved".to_owned()),
                reference: None,
                timeout: limit,
            }
        );
        assert!(
            action(&json!({"action": "click"}), limit)
                .unwrap_err()
                .to_string()
                .contains("`click` needs `ref`")
        );
        assert!(action(&json!({"action": "wait_for"}), limit).is_err());
        assert!(action(&json!({"action": "scroll"}), limit).is_err());
    }

    #[test]
    fn the_fingerprint_is_the_action_and_its_target() {
        assert_eq!(
            fingerprint(&json!({"action": "click", "ref": "@e3", "timeout": 5})),
            json!({"action": "click", "ref": "@e3"})
        );
    }

    #[test]
    fn a_screenshot_is_offered_only_with_vision() {
        let scope = FileScope {
            store: "code".to_owned(),
            root: "web".to_owned(),
        };
        let config: Attrs = [("application".to_owned(), json!("todo"))]
            .into_iter()
            .collect();
        let without = spec(&scope, &config, false, RunMode::Act);
        let with = spec(&scope, &config, true, RunMode::Act);
        assert_eq!(without.name, "view_app_code_web");
        assert!(!without.parameters.to_string().contains("screenshot"));
        assert!(with.parameters.to_string().contains("screenshot"));
        assert!(without.description.contains("write real rows"));
    }

    #[test]
    fn an_old_result_is_one_line() {
        let call = sc_llm::ToolCall {
            id: "1".to_owned(),
            name: "view_app_code_web".to_owned(),
            arguments: json!({"action": "snapshot"}),
        };
        let content = "view_app snapshot\nurl: http://x--todo.example.com/tasks (status 200)\n\
                       snapshot:\npage \"Tasks\"\n  button \"Add\" @e1";
        let old = Elidable {
            index: 3,
            call: &call,
            content,
            images: 0,
            transcript: &[],
            state: None,
        };
        assert_eq!(elide(&old), "[elided snapshot of /tasks: 2 lines]");
        let shot = Elidable { images: 1, ..old };
        assert_eq!(elide(&shot), "[elided screenshot of /tasks]");
    }
}
