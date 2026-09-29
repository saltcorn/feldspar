//! `send_email` — build a message out of the event and send it (design §18.2).

use sc_action::{
    Action, ActionContext, ConfigCheck, config_flag, optional_template, render_event_template,
    required_template, template_scope,
};
use sc_catalog::{Catalog, DataField, DataFieldKind, Table};
use sc_email::{Attachment, Email, Mailbox, parse_mailbox, parse_recipients, render_mjml};
use sc_error::{Error, Result};
use sc_expr::{RenderMode, Template};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

/// The primary recipients — a rendered list, commas between addresses.
const CFG_TO: &str = "to";
/// Carbon copies.
const CFG_CC: &str = "cc";
/// Blind carbon copies.
const CFG_BCC: &str = "bcc";
/// An optional from-address, overriding the transport's configured one.
const CFG_FROM: &str = "from";
/// The subject line.
const CFG_SUBJECT: &str = "subject";
/// The `text/html` body.
const CFG_HTML: &str = "html";
/// Whether the HTML body is written as MJML and has to be compiled.
const CFG_MJML: &str = "mjml";
/// The `text/plain` body.
const CFG_TEXT: &str = "text";
/// The prefix of the per-File-field attachment settings: `attach_invoice` means
/// "attach the file the row's `invoice` field points at".
///
/// A prefix rather than a list setting because the *form* is the point: one
/// checkbox per File field of the table, offered only where there is a file to
/// attach, is a question an admin can answer without knowing which of their
/// columns are files. A free-text list would move that knowledge back onto them
/// and the checking to save time.
const CFG_ATTACH_PREFIX: &str = "attach_";

/// The largest file this action will attach.
///
/// Mail providers cap a *message* at around 25 MB, and base64 inflates a file by
/// about a third on the way into one — so 20 MB of PDF is already at the line and
/// anything past it is a message that will be refused after it has been built.
/// Refusing here means the admin is told which field and how big, rather than
/// reading "552 message size exceeds fixed limit" from a mail server.
///
/// The file is read before it is measured: the store's contract is
/// [`read`](sc_files::FileStore::read), which yields the whole thing. So this
/// bounds what is *sent*, not what is momentarily held — a real cap on the read
/// needs a `stat` the store trait does not have, and inventing one for this is
/// out of proportion.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Send one email, built from the event by interpolation.
///
/// **Every setting is a [`Template`]** — the recipients, the subject and both
/// bodies — so the configuration is written the way the message reads
/// (`Receipt for order {{ id }}`) rather than as a formula returning a string.
/// They are validated in the event's scope on save *and* on load, like every
/// other action's configuration, so a subject naming a field that was dropped
/// takes its trigger out of the live set with a reason instead of failing at 3am
/// with the message half sent.
///
/// The recipients, the subject and the text body render as **text** and the HTML
/// body renders as **HTML** (decision 3): a subject put through the HTML rule
/// turns `Tea & Coffee` into `Tea &amp; Coffee`, and a plain-text body becomes a
/// page of entities.
///
/// **A File field of the table can travel with the message.** Every File field
/// the trigger's table has becomes a checkbox
/// ([`config_spec_for`](Action::config_spec_for)), and a ticked one attaches the
/// file the row's own path points at — read from the store that field is
/// declared against, named after the file, and typed by its extension. A null
/// path attaches nothing and is not an error; anything else that goes wrong is,
/// named with the field.
///
/// **The transport is handed in**, through [`ActionContext::mailer`] exactly as
/// the evaluator is. That is what lets this action's tests assert *what would
/// have been sent* against a recording mailer, and it is why the from-address is
/// asked of the transport rather than read from the settings here: an
/// installation that has no transport at all is one error, at one place, naming
/// the screen that fixes it.
pub struct SendEmail;

#[async_trait::async_trait]
impl Action for SendEmail {
    fn name(&self) -> &str {
        "send_email"
    }

    fn description(&self) -> &str {
        "Send an email, with the recipients, subject and body written as templates"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TO, BasicType::Text).label("To"),
            FormField::new(CFG_CC, BasicType::Text).label("Cc"),
            FormField::new(CFG_BCC, BasicType::Text).label("Bcc"),
            FormField::new(CFG_FROM, BasicType::Text).label("From"),
            // The one required setting. A message with no recipient at all is
            // refused by `validate_config` rather than here, because a `bcc`-only
            // message is a message — but a transactional email with no subject is
            // a mistake in every case anyone has, and the spec is where the form
            // can say so before it is submitted.
            FormField::new(CFG_SUBJECT, BasicType::Text)
                .label("Subject")
                .required(),
            FormField::new(CFG_HTML, BasicType::Text)
                .label("HTML body")
                .multiline(),
            FormField::new(CFG_MJML, BasicType::Bool).label("HTML body is MJML"),
            FormField::new(CFG_TEXT, BasicType::Text)
                .label("Text body")
                .multiline(),
        ]
    }

    /// The static settings, plus **one checkbox per File field** of the trigger's
    /// table (§6.2's "settings as data", now answering a question about the
    /// table).
    ///
    /// Offered only where there is a table: a `login` trigger has no row, so it
    /// has no file to attach, and a checkbox for one would be a control that
    /// cannot mean anything. An unresolvable channel yields the static spec —
    /// "no such table" is [`validate_trigger`](sc_action::validate_trigger)'s
    /// error to give, and giving it twice in two voices helps nobody.
    fn config_spec_for(&self, catalog: &Catalog, channel: Option<&str>) -> Vec<FormField> {
        let mut spec = self.config_spec();
        let Some(table) = channel.and_then(|name| catalog.get(name).ok().flatten()) else {
            return spec;
        };
        for field in file_fields(&table) {
            spec.push(
                FormField::new(attach_key(&field.base.name), BasicType::Bool)
                    .label(format!("Attach {}", field.base.label)),
            );
        }
        spec
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let scope = template_scope(check.channel);
        let templates = Templates::parse(check.config)?;

        // Every template, in the scope the event will give it: an identifier that
        // does not resolve is named here, in front of the admin, together with
        // the token it is in.
        for (what, template) in templates.each() {
            check
                .template(scope, template, &format!("`{what}`"))
                .await?;
        }

        // What a message must have, said while it can still be fixed. Both of
        // these would otherwise surface as `Email::check`'s error at send time,
        // which is a worse moment and a message with no setting name in it.
        if templates.recipients().next().is_none() {
            return Err(Error::invalid(format!(
                "a message needs a recipient: set `{CFG_TO}`, `{CFG_CC}` or `{CFG_BCC}`"
            )));
        }
        if templates.html.is_none() && templates.text.is_none() {
            return Err(Error::invalid(format!(
                "a message needs a body: set `{CFG_HTML}`, `{CFG_TEXT}`, or both"
            )));
        }
        if templates.mjml && templates.html.is_none() {
            return Err(Error::invalid(format!(
                "`{CFG_MJML}` says the HTML body is MJML, but there is no `{CFG_HTML}` body"
            )));
        }

        // A **static** address is parsed now. A template that interpolates a
        // column cannot be checked until a row is in hand — that failure names
        // the address it got (`parse_recipients`) — but `ada@example` typed into
        // the form is a mistake the form should catch.
        for (what, template) in templates.recipients() {
            if template.is_literal() {
                parse_recipients(template.source())
                    .map_err(|e| Error::invalid(format!("`{what}`: {e}")))?;
            }
        }
        if let Some(from) = &templates.from
            && from.is_literal()
        {
            parse_mailbox(from.source())
                .map_err(|e| Error::invalid(format!("`{CFG_FROM}`: {e}")))?;
        }
        // And a static MJML body is compiled now, for the same reason: a missing
        // `</mj-section>` is a save-time error, not a Tuesday-night one.
        if templates.mjml
            && let Some(html) = &templates.html
            && html.is_literal()
        {
            render_mjml(html.source()).map_err(|e| Error::invalid(format!("`{CFG_HTML}`: {e}")))?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let templates = Templates::parse(ctx.config)?;
        // Asked for first, and before anything is rendered: an installation with
        // no transport has nothing to do with a rendered body, and this is the
        // error that names Settings → Email.
        let mailer = ctx.mailer()?;
        let from = match &templates.from {
            Some(template) => {
                let rendered = render_event_template(
                    ctx,
                    template,
                    &format!("`{CFG_FROM}`"),
                    RenderMode::Text,
                )
                .await?;
                parse_mailbox(&rendered)
                    .map_err(|e| named(ctx.trigger, CFG_FROM, &e.to_string()))?
            }
            // The transport's own answer, with the trigger named: "this
            // installation sends no mail" is about the installation, but *which*
            // trigger found out is what the admin needs to see it from.
            None => mailer
                .sender()
                .await
                .map_err(|e| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger)))?,
        };

        let mut email = Email::new(from);
        email.subject = render_event_template(
            ctx,
            &templates.subject,
            &format!("`{CFG_SUBJECT}`"),
            RenderMode::Text,
        )
        .await?;
        for (what, template) in templates.recipients() {
            // A recipient list is text: an address is not HTML, and escaping one
            // would put `&amp;` inside a display name.
            let rendered =
                render_event_template(ctx, template, &format!("`{what}`"), RenderMode::Text)
                    .await?;
            let parsed = parse_recipients(&rendered)
                .map_err(|e| named(ctx.trigger, what, &e.to_string()))?;
            match what {
                CFG_CC => email.cc = parsed,
                CFG_BCC => email.bcc = parsed,
                _ => email.to = parsed,
            }
        }
        if let Some(template) = &templates.html {
            let rendered =
                render_event_template(ctx, template, &format!("`{CFG_HTML}`"), RenderMode::Html)
                    .await?;
            // Compiled **after** interpolation, so `{{ }}` tokens are written
            // into the MJML source an admin wrote and are escaped by the ordinary
            // HTML rule on the way in. Interpolating into the generated tables
            // instead would put a rendered value somewhere the author never saw.
            email.html = Some(if templates.mjml {
                render_mjml(&rendered).map_err(|e| named(ctx.trigger, CFG_HTML, &e.to_string()))?
            } else {
                rendered
            });
        }
        if let Some(template) = &templates.text {
            email.text = Some(
                render_event_template(ctx, template, &format!("`{CFG_TEXT}`"), RenderMode::Text)
                    .await?,
            );
        }

        email.attachments = attachments(ctx).await?;

        // Checked before the transport is opened, so "no recipients" is this
        // action's message rather than an SMTP rejection: a `to` template that
        // rendered to nothing is a null column, not a network problem.
        email
            .check()
            .map_err(|e| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger)))?;
        mailer
            .send(&email)
            .await
            .map_err(|e| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger)))?;

        // What was sent, so a button that ran this trigger can say so — and so a
        // workflow step downstream has the addresses without re-rendering them.
        Ok(json!({
            "to": addresses(&email.to),
            "cc": addresses(&email.cc),
            "bcc": addresses(&email.bcc),
            "subject": email.subject,
            "attachments": email
                .attachments
                .iter()
                .map(|a| a.filename.clone())
                .collect::<Vec<_>>(),
        }))
    }
}

/// Every configured template, parsed once.
///
/// One type because `validate_config` and `run` must read the *same* settings
/// the same way: a configuration that validated and then behaved differently at
/// send time is the class of bug this whole arrangement exists to prevent.
#[derive(Debug)]
struct Templates {
    to: Option<Template>,
    cc: Option<Template>,
    bcc: Option<Template>,
    from: Option<Template>,
    subject: Template,
    html: Option<Template>,
    text: Option<Template>,
    mjml: bool,
}

impl Templates {
    fn parse(config: &Attrs) -> Result<Templates> {
        Ok(Templates {
            to: optional_template(config, CFG_TO)?,
            cc: optional_template(config, CFG_CC)?,
            bcc: optional_template(config, CFG_BCC)?,
            from: optional_template(config, CFG_FROM)?,
            subject: required_template(config, CFG_SUBJECT)?,
            html: optional_template(config, CFG_HTML)?,
            text: optional_template(config, CFG_TEXT)?,
            mjml: config_flag(config, CFG_MJML)?,
        })
    }

    /// The three recipient settings that were configured, with their names.
    fn recipients(&self) -> impl Iterator<Item = (&'static str, &Template)> {
        [(CFG_TO, &self.to), (CFG_CC, &self.cc), (CFG_BCC, &self.bcc)]
            .into_iter()
            .filter_map(|(what, template)| template.as_ref().map(|t| (what, t)))
    }

    /// Every configured template, with the setting it came from — what the
    /// save-time check walks, so a setting added to this struct cannot be left
    /// unvalidated.
    fn each(&self) -> impl Iterator<Item = (&'static str, &Template)> {
        self.recipients().chain(
            [
                (CFG_FROM, &self.from),
                (CFG_HTML, &self.html),
                (CFG_TEXT, &self.text),
            ]
            .into_iter()
            .filter_map(|(what, template)| template.as_ref().map(|t| (what, t)))
            .chain(std::iter::once((CFG_SUBJECT, &self.subject))),
        )
    }
}

/// The File fields of a table, in the order the table declares them — which is
/// the order the checkboxes appear in and the order the attachments travel in.
fn file_fields(table: &Table) -> impl Iterator<Item = &DataField> {
    table
        .fields
        .iter()
        .filter(|field| matches!(field.kind, DataFieldKind::File { .. }))
}

/// The setting name that attaches `field`.
fn attach_key(field: &str) -> String {
    format!("{CFG_ATTACH_PREFIX}{field}")
}

/// The files this run attaches: for each ticked File field, the bytes the row's
/// path points at.
///
/// **A null or empty path attaches nothing**, and is not an error. That is the
/// same reading a null column gets everywhere else here (decision 4): an order
/// with no invoice yet is data, and refusing to send the receipt over it would
/// make the trigger fail on the rows that are merely incomplete.
///
/// Everything else *is* an error, named with the field: a path that points into
/// a store that is not connected, a file that is not there, one past
/// [`MAX_ATTACHMENT_BYTES`]. A message that silently arrives without the invoice
/// it was supposed to carry is the failure nobody notices until the customer
/// does.
///
/// The read runs under the **action's** authority, not the caller's, like every
/// other thing a trigger does (§10.1): the trigger is the admin's configuration,
/// and the gate on who may cause it to run is the trigger's own `min_role` plus —
/// for a row-scoped run — the caller's ability to read the row at all.
async fn attachments(ctx: &ActionContext<'_>) -> Result<Vec<Attachment>> {
    // The ticked boxes, by field name. A box that is present and false is a
    // checkbox the admin left alone — the form posts every one it rendered — so
    // it is not a request for anything, and a stored value that is not a boolean
    // at all is named by the loop below rather than silently read as false.
    let ticked: Vec<&str> = ctx
        .config
        .iter()
        .filter(|(_, value)| value.as_bool() == Some(true))
        .filter_map(|(key, _)| key.strip_prefix(CFG_ATTACH_PREFIX))
        .collect();
    if ticked.is_empty() {
        return Ok(Vec::new());
    }
    // An event with no table has no row, and a file field is a column of one.
    // Reachable only from a stored configuration that outlived its channel,
    // because the spec offers no checkbox where there is no table.
    let Some(channel) = ctx.event.channel.as_deref() else {
        return Err(Error::invalid(format!(
            "trigger `{}`: `{}{}` names a file field, but this event has no table",
            ctx.trigger,
            CFG_ATTACH_PREFIX,
            ticked.first().copied().unwrap_or_default()
        )));
    };
    let table = ctx.catalog.require(channel)?;
    let row = ctx.event.row_object();

    let mut out = Vec::new();
    // Walked in the table's field order rather than the configuration's, so the
    // order the files arrive in is the order the admin saw the checkboxes in.
    for field in file_fields(&table) {
        let name = field.base.name.as_str();
        if !config_flag(ctx.config, &attach_key(name))? {
            continue;
        }
        let path = match row.get(name) {
            Some(Json::String(path)) if !path.trim().is_empty() => path.trim(),
            // Absent, null or empty: no file, which is not a mistake.
            _ => continue,
        };
        let DataFieldKind::File { store, .. } = &field.kind else {
            continue;
        };
        let failed = |what: String| Error::invalid(format!("trigger `{}`: {what}", ctx.trigger));
        let store = ctx.catalog.file_store(&store.0)?.ok_or_else(|| {
            failed(format!(
                "`{name}`: file store `{}` is not connected, so `{path}` cannot be attached",
                store.0
            ))
        })?;
        let bytes = store
            .read(path)
            .await
            .map_err(|e| failed(format!("`{name}`: could not read `{path}`: {e}")))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(failed(format!(
                "`{name}`: `{path}` is {} bytes, past the {MAX_ATTACHMENT_BYTES}-byte limit on \
                 one attachment",
                bytes.len()
            )));
        }
        out.push(Attachment {
            filename: file_name(path),
            // The extension is what a mail client will go by anyway; a file whose
            // name says nothing travels as bytes, which is what
            // `application/octet-stream` means and what a client shows as "a
            // file you can save".
            content_type: sc_files::mime_for_path(path)
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
            bytes: bytes.to_vec(),
        });
    }
    Ok(out)
}

/// The last component of a store path — the name the recipient sees and saves
/// it under. A store path is `/`-separated by the store contract, so this needs
/// no platform knowledge.
fn file_name(path: &str) -> String {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// A failure that is about one setting's *rendered value*, named with both the
/// trigger and the setting — the shape every other action's run-time error has.
fn named(trigger: &str, what: &str, message: &str) -> Error {
    Error::invalid(format!("trigger `{trigger}`: `{what}`: {message}"))
}

/// The addresses of a header, as the action's result reports them.
fn addresses(mailboxes: &[Mailbox]) -> Vec<String> {
    mailboxes.iter().map(|m| m.address.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(entries: &[(&str, Json)]) -> Attrs {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn the_spec_requires_only_a_subject_and_names_every_part_of_a_message() {
        let spec = SendEmail.config_spec();
        let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            vec!["to", "cc", "bcc", "from", "subject", "html", "mjml", "text"]
        );
        // `to` is *not* required: a `bcc`-only message is a message, and which
        // headers are needed is `validate_config`'s question.
        let required: Vec<&str> = spec
            .iter()
            .filter(|f| f.required)
            .map(|f| f.name())
            .collect();
        assert_eq!(required, vec!["subject"]);
    }

    #[test]
    fn the_settings_are_parsed_as_templates_and_the_flag_as_a_flag() {
        let cfg = config(&[
            ("to", json!("{{ customerⱵemail }}")),
            ("subject", json!("Receipt for order {{ id }}")),
            ("html", json!("<p>{{ total }}</p>")),
            ("mjml", json!(true)),
        ]);
        let templates = Templates::parse(&cfg).unwrap();
        assert!(templates.mjml);
        assert!(templates.cc.is_none() && templates.text.is_none());
        // Every configured setting is walked by the save-time check.
        let walked: Vec<&str> = templates.each().map(|(what, _)| what).collect();
        assert_eq!(walked, vec!["to", "html", "subject"]);
        assert!(!templates.subject.is_literal());

        // A subject is required, and an unclosed token is refused by name.
        assert!(Templates::parse(&config(&[("to", json!("a@b.c"))])).is_err());
        let msg = Templates::parse(&config(&[("subject", json!("{{ id"))]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("subject"), "{msg}");

        // A flag that is not one names the setting rather than reading false.
        let msg = Templates::parse(&config(&[
            ("subject", json!("hi")),
            ("mjml", json!("true")),
        ]))
        .unwrap_err()
        .to_string();
        assert!(msg.contains("mjml"), "{msg}");
    }

    #[test]
    fn the_result_reports_the_addresses_a_message_went_to() {
        let email = Email {
            to: vec![parse_mailbox("Ada <ada@example.com>").unwrap()],
            ..Email::new(parse_mailbox("saltcorn@example.com").unwrap())
        };
        // The bare addresses, not the headers: a caller showing "sent to …" wants
        // the address, and a display name is the transport's business.
        assert_eq!(addresses(&email.to), vec!["ada@example.com".to_owned()]);
        assert!(addresses(&email.cc).is_empty());
    }
}
