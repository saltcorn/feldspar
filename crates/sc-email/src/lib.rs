//! Sending mail: a message, a transport seam, and SMTP (layer 6, design §18.2).
//!
//! Three things, and the split between them is the point:
//!
//! - [`Email`] is a message with no opinion about how it travels — addresses, a
//!   subject and one or both bodies. It is what an action produces.
//! - [`Mailer`] is the transport, one `async fn send`. Everything that sends
//!   mail goes through it, so changing provider stays a configuration change and
//!   the OAuth2 and Microsoft Graph transports §18.2 leaves for later are a new
//!   implementation rather than a rewrite.
//! - [`SmtpMailer`] is the one implementation that reaches a network, built from
//!   the [`EmailSettings`](sc_config::EmailSettings) an admin saved, and
//!   [`SettingsMailer`] is the one a server installs: it reads those settings
//!   afresh for every message, so changing them takes effect on the next one.
//!
//! Beside them is [`render_mjml`], which compiles an HTML body written as
//! [MJML](https://mjml.io) — the markup an email designer writes — into the
//! table markup a mail client will lay out. It is here rather than in the action
//! because "what an email body may be written in" is this crate's question, and
//! the system emails will want the same answer.
//!
//! The trait is not speculation about future providers: it is what lets an
//! action's tests assert *what would have been sent* without an SMTP server,
//! through [`RecordingMailer`]. That is why the recorder lives here beside the
//! real transport rather than in some test module — two crates use it.
//!
//! **Addresses are parsed, not concatenated.** [`parse_recipients`] splits a
//! rendered recipient string into mailboxes on the commas that separate them and
//! not on the ones inside a quoted display name, and names the address it
//! rejected. A template renders `to`, so the string this is handed is whatever a
//! row held, and "which address was wrong" is the only useful answer.

mod mjml;
mod relay;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{
        Attachment as LettreAttachment, Mailbox as LettreMailbox, MultiPart, SinglePart,
        header::ContentType as LettreContentType,
    },
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
};
use sc_catalog::Catalog;
use sc_error::{Error, Result};

pub use mjml::render_mjml;
pub use relay::{
    SUBMISSION_STARTTLS_PORT, SUBMISSION_TLS_PORT, check_relay, is_public, relay_mailer,
    resolve_public, security_for_port,
};
// The address vocabulary, re-exported: a crate that builds a message should not
// have to depend on the *settings* crate to name an address. Parsed there, and
// only there, because the from-address is a setting and a setting is checked
// where it is declared — this is that one grammar, borrowed.
pub use sc_config::{Mailbox, parse_mailbox};
// And the settings a server is described by, for a caller that names one of
// its own ([`Mailer::send_via`]).
use sc_config::{CERTIFICATE_ACCEPT_ANY, SMTP_CERTIFICATE_CHECK, SMTP_CERTIFICATE_NAME};
pub use sc_config::{EmailSettings, SmtpSecurity};

/// One message, independent of how it is sent.
///
/// Both bodies are optional and at least one has to be present; that is checked
/// by [`Email::check`] rather than by the type, because the action that builds
/// one wants to report "a message needs a body" against the trigger the admin is
/// editing, not to be unable to construct the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    /// Who the message is from.
    pub from: Mailbox,
    /// The primary recipients.
    pub to: Vec<Mailbox>,
    /// Carbon copies.
    pub cc: Vec<Mailbox>,
    /// Blind carbon copies.
    pub bcc: Vec<Mailbox>,
    /// The subject line, already rendered.
    pub subject: String,
    /// The `text/plain` body.
    pub text: Option<String>,
    /// The `text/html` body.
    pub html: Option<String>,
    /// Files travelling with the message.
    pub attachments: Vec<Attachment>,
}

/// One file attached to a message: what it is called, what it is, and its bytes.
///
/// The bytes are owned and in memory, which is what the transport needs anyway —
/// a MIME part is base64 of the whole file, and there is no streaming SMTP body
/// to hand a reader to. What that costs is bounded by whoever *builds* the
/// attachment (the `send_email` action refuses a file past a size limit), not
/// here: this type is a message part, and a message part has no policy.
#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The name the recipient sees, and saves it under.
    pub filename: String,
    /// The MIME type, e.g. `application/pdf`.
    pub content_type: String,
    /// The file itself.
    pub bytes: Vec<u8>,
}

/// Debug written by hand, because the derived one would print every byte of a
/// two-megabyte PDF into a test failure or a log line.
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .finish()
    }
}

impl Email {
    /// A message with a from-address and nothing else, to be filled in.
    pub fn new(from: Mailbox) -> Email {
        Email {
            from,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            text: None,
            html: None,
            attachments: Vec::new(),
        }
    }

    /// What has to be true before a message can be sent at all.
    ///
    /// Separate from the transport so an action can run it while an admin is
    /// still looking at the form: a message with no recipients and a message
    /// with no body are both mistakes worth naming there rather than at send.
    pub fn check(&self) -> Result<()> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(Error::invalid(
                "an email needs at least one recipient: `to`, `cc` or `bcc`",
            ));
        }
        if self.text.is_none() && self.html.is_none() {
            return Err(Error::invalid(
                "an email needs a body: set the HTML body, the text body, or both",
            ));
        }
        Ok(())
    }

    /// Every recipient, in header order — what a log line or a test assertion
    /// wants when it does not care which header an address was in.
    pub fn recipients(&self) -> Vec<&Mailbox> {
        self.to.iter().chain(&self.cc).chain(&self.bcc).collect()
    }
}

/// The transport: the one way a message leaves this system.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Send one message, or say why it could not be sent.
    async fn send(&self, email: &Email) -> Result<()>;

    /// The address this transport sends **as**, for a sender that does not name
    /// one of its own.
    ///
    /// On the trait rather than read from the settings by every caller, because
    /// the from-address is a property of the transport: it is the configured
    /// `email_from` for SMTP, and it would be the authorised mailbox for an
    /// OAuth2 or Graph transport, which is not a setting at all. It is also
    /// where "this installation sends no mail" is discovered — a `send_email`
    /// action asks for it before it builds a message, so an unconfigured
    /// installation is an error pointing at Settings → Email rather than a
    /// connection attempt to nowhere.
    ///
    /// Async and fallible because the answer is read at send time: an admin who
    /// changes the from-address gets it on the next message, not the next
    /// restart.
    async fn sender(&self) -> Result<Mailbox>;

    /// Send one message through `server` instead of this transport's own —
    /// `send_email`'s SMTP override (TODO-imap M3), where a trigger names the
    /// server an account sends through.
    ///
    /// On the trait, with the real behaviour as its default, so that what an
    /// action does with an override is one call whichever transport it was
    /// handed, and a test can record the server a message *would* have gone
    /// through ([`RecordingMailer::servers`]) without a public SMTP server to
    /// send it to. The default holds `server` to the relay rules
    /// ([`relay_mailer`]): a submission port, public addresses only, and a
    /// connection to the address that was checked.
    async fn send_via(&self, server: &EmailSettings, email: &Email) -> Result<()> {
        relay_mailer(server).await?.send(email).await
    }

    /// Whether this transport already keeps a checklist run's messages back
    /// ([`Withholding`]), so it is not wrapped twice.
    fn withholds(&self) -> bool {
        false
    }
}

/// Split a rendered recipient string into mailboxes.
///
/// Commas separate addresses — except inside a quoted display name, where
/// `"Lovelace, Ada" <ada@example.com>` is one recipient and splitting on every
/// comma would make it two nonsense ones. An empty string is no recipients
/// rather than an error: which headers a message needs is [`Email::check`]'s
/// question, and a `cc` template that rendered to nothing is not a mistake.
pub fn parse_recipients(raw: &str) -> Result<Vec<Mailbox>> {
    let mut out = Vec::new();
    for part in split_on_unquoted_commas(raw) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(parse_mailbox(part).map_err(|e| {
            // The whole list is quoted as well as the offending part, because a
            // recipient string is *rendered* — the admin sees a template, and
            // knowing which of three addresses a row produced badly is the
            // difference between a fixable report and a shrug.
            Error::invalid(format!("recipient `{part}` in `{raw}` is not usable: {e}"))
        })?);
    }
    Ok(out)
}

/// Split on the commas that separate addresses, not the ones inside quotes.
fn split_on_unquoted_commas(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&raw[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&raw[start..]);
    parts
}

/// SMTP, over [`lettre`].
///
/// Built from the stored settings, so what it sends through is what the admin
/// saved rather than what a caller passed in — which is what makes a test
/// message a test of the *configuration* (§18.2).
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    /// The configured `email_from`, kept because it is what
    /// [`sender`](Mailer::sender) answers.
    from: Mailbox,
    /// The server's name, for errors: a trigger that sends through an
    /// account's server has to say *which* server refused it.
    host: String,
}

impl SmtpMailer {
    /// Build a transport for these settings.
    ///
    /// The three security modes: `tls` is implicit TLS from the first byte,
    /// `starttls` connects in the clear and upgrades, and `none` is no TLS at
    /// all — reachable only because [`EmailSettings::check`] has already refused
    /// it in combination with credentials. The certificate is checked as
    /// [`tls_for`] says.
    pub fn new(settings: &EmailSettings) -> Result<SmtpMailer> {
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
            .port(settings.port)
            .tls(tls_for(settings)?);
        if settings.authenticates() {
            builder = builder.credentials(Credentials::new(
                settings.username.clone(),
                settings.password.clone(),
            ));
        }
        Ok(SmtpMailer {
            transport: builder.build(),
            from: settings.from.clone(),
            host: settings.host.clone(),
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn sender(&self) -> Result<Mailbox> {
        Ok(self.from.clone())
    }

    async fn send(&self, email: &Email) -> Result<()> {
        let message = build_message(email)?;
        self.transport
            .send(message)
            .await
            // The transport's own words, verbatim: "connection refused",
            // "relay denied" and "authentication failed" are three different
            // problems with three different fixes, and no summary this crate
            // could write beats naming which one happened.
            .map_err(|e| {
                let mut message =
                    format!("the mail server `{}` rejected the message: {e}", self.host);
                // The two certificate problems have settings that fix them, and
                // "invalid peer certificate" names neither.
                if message.contains("invalid peer certificate") {
                    message.push_str(&format!(
                        ". If the server's certificate is issued for another name — as it is \
                         when the SMTP host is an IP address — set `{SMTP_CERTIFICATE_NAME}` to \
                         that name. If it is self-signed, `{SMTP_CERTIFICATE_CHECK}` \
                         `{CERTIFICATE_ACCEPT_ANY}` accepts it"
                    ));
                }
                Error::msg(message)
            })?;
        Ok(())
    }
}

/// The TLS a connection to `settings` uses: none for `none`, and otherwise the
/// certificate checked against [`certificate_name`](EmailSettings::certificate_name)
/// — or the host when there is none — by the platform's verifier, unless the
/// admin chose to accept any certificate.
///
/// The name is also the SNI the client sends. An IP host with no certificate
/// name is checked against the IP, which a certificate issued for a DNS name
/// does not carry: that is the error `certificate not valid for name
/// "188.65.204.58"`, and the certificate name is its fix.
fn tls_for(settings: &EmailSettings) -> Result<Tls> {
    if settings.security == SmtpSecurity::None {
        return Ok(Tls::None);
    }
    let name = settings.certificate_name.clone().unwrap_or_else(|| {
        settings
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned()
    });
    let parameters = TlsParameters::builder(name.clone())
        // Both: lettre's "invalid certs" still checks the name, whatever its
        // documentation says, and a self-signed certificate on an IP host fails
        // that too. (The name switch alone is ignored under the platform
        // verifier; together they replace it.)
        .dangerous_accept_invalid_certs(settings.accept_any_certificate)
        .dangerous_accept_invalid_hostnames(settings.accept_any_certificate)
        .build()
        .map_err(|e| {
            Error::config(format!(
                "could not set up TLS for SMTP server `{}` (certificate name `{name}`): {e}",
                settings.host
            ))
        })?;
    Ok(match settings.security {
        SmtpSecurity::Tls => Tls::Wrapper(parameters),
        _ => Tls::Required(parameters),
    })
}

/// An [`Email`] as the MIME message that goes on the wire.
///
/// Public because it is the only place the message's *structure* is decided, and
/// a test that asserts the structure should assert this rather than a
/// reimplementation of it.
pub fn build_message(email: &Email) -> Result<lettre::Message> {
    email.check()?;
    let mut builder = lettre::Message::builder()
        .from(to_lettre(&email.from)?)
        .subject(email.subject.clone());
    for address in &email.to {
        builder = builder.to(to_lettre(address)?);
    }
    for address in &email.cc {
        builder = builder.cc(to_lettre(address)?);
    }
    for address in &email.bcc {
        builder = builder.bcc(to_lettre(address)?);
    }
    let body = match (&email.text, &email.html, email.attachments.is_empty()) {
        // Text first inside `multipart/alternative`: the parts are ordered
        // least-preferred first, and every mail client reads the last one it can
        // render. Reversed, a graphical client would show the plain text.
        (Some(text), Some(html), true) => builder.multipart(MultiPart::alternative_plain_html(
            text.clone(),
            html.clone(),
        )),
        (Some(text), None, true) => builder.singlepart(SinglePart::plain(text.clone())),
        (None, Some(html), true) => builder.singlepart(SinglePart::html(html.clone())),
        // With files, the whole body becomes one part of a `multipart/mixed`
        // and the attachments are its siblings. That nesting is the structure
        // every mail client expects — bodies alternative *inside* mixed, never
        // beside the attachments — and getting it the other way round is how a
        // message arrives as three files and no text.
        (text, html, false) => {
            let mut mixed = MultiPart::mixed().build();
            match (text, html) {
                (Some(text), Some(html)) => {
                    mixed = mixed.multipart(MultiPart::alternative_plain_html(
                        text.clone(),
                        html.clone(),
                    ));
                }
                (Some(text), None) => mixed = mixed.singlepart(SinglePart::plain(text.clone())),
                (None, Some(html)) => mixed = mixed.singlepart(SinglePart::html(html.clone())),
                // Refused by `check` above.
                (None, None) => return Err(Error::invalid("an email needs a body")),
            }
            for file in &email.attachments {
                mixed = mixed.singlepart(attachment_part(file)?);
            }
            builder.multipart(mixed)
        }
        // Refused by `check` above; this arm exists because the type says it
        // might happen, not because it can.
        (None, None, true) => return Err(Error::invalid("an email needs a body")),
    };
    body.map_err(|e| Error::invalid(format!("could not build the message: {e}")))
}

/// One attachment as its MIME part.
///
/// The content type is parsed rather than pasted into a header: a value that is
/// not a MIME type would otherwise produce a message whose part no client can
/// read, and the error names the file it came from.
fn attachment_part(file: &Attachment) -> Result<SinglePart> {
    let content_type = LettreContentType::parse(&file.content_type).map_err(|e| {
        Error::invalid(format!(
            "attachment `{}`: `{}` is not a usable content type: {e}",
            file.filename, file.content_type
        ))
    })?;
    Ok(LettreAttachment::new(file.filename.clone()).body(file.bytes.clone(), content_type))
}

/// Our mailbox as lettre's.
///
/// Parsed here rather than carried as lettre's type through the whole system,
/// for the reason §2 gives generally: nothing above this crate should have to
/// name a dependency's type to describe an address.
fn to_lettre(mailbox: &Mailbox) -> Result<LettreMailbox> {
    mailbox.to_header().parse::<LettreMailbox>().map_err(|e| {
        Error::invalid(format!(
            "`{}` is not a usable address: {e}",
            mailbox.to_header()
        ))
    })
}

/// The transport an installation actually has: whatever the **saved settings**
/// say, read afresh for every message.
///
/// This is the mailer a server installs, and the indirection is the point. A
/// transport built once at boot would be the settings as they were at boot: an
/// admin who fixes a password would have to restart the server for the fix to
/// take, and the Email section's own help text ("changes take effect on the next
/// message; nothing here needs a restart") would be false. Reading the settings
/// per message costs one config lookup against a catalog that already caches
/// them, and building an [`SmtpMailer`] opens no connection — the socket is the
/// `send` call's, not the constructor's.
///
/// It is also where "this installation sends no mail" is discovered, once, with
/// a message pointing at the screen that fixes it.
pub struct SettingsMailer {
    catalog: Arc<Catalog>,
}

impl SettingsMailer {
    /// A mailer over the settings stored in `catalog`.
    pub fn new(catalog: Arc<Catalog>) -> SettingsMailer {
        SettingsMailer { catalog }
    }

    /// The saved settings, or the one error that names what is missing and where
    /// to put it.
    ///
    /// `EmailSettings::load` already distinguishes the two failures this has to
    /// keep apart: `None` is "no `smtp_host`", which is an installation that was
    /// never configured, and an `Err` is settings that are configured *wrongly*,
    /// whose own message says which key is wrong. Collapsing them into one
    /// "email is not set up" would throw away the half that is actionable.
    async fn settings(&self) -> Result<EmailSettings> {
        EmailSettings::load(&self.catalog).await?.ok_or_else(|| {
            Error::invalid(
                "this installation has no mail transport: set an SMTP host in \
                 Settings → Email and save",
            )
        })
    }
}

#[async_trait]
impl Mailer for SettingsMailer {
    async fn sender(&self) -> Result<Mailbox> {
        Ok(self.settings().await?.from)
    }

    async fn send(&self, email: &Email) -> Result<()> {
        SmtpMailer::new(&self.settings().await?)?.send(email).await
    }
}

/// The mailer a dispatcher installs around the one it is handed: a message
/// sent on behalf of a checklist run goes to the
/// [run's outbox](sc_catalog::outbox) instead (TODO-checklists C4.1), and
/// every other message to the transport it wraps.
///
/// A wrapper rather than a check in each transport, so a transport added later
/// cannot forget it: every message a trigger, a workflow or an auth route sends
/// reaches the transport through the dispatcher's mailer, which is this.
pub struct Withholding {
    inner: Arc<dyn Mailer>,
}

impl Withholding {
    /// `inner`, withholding what a checklist run sends. Wrapping one that is
    /// already wrapped returns it as it is.
    pub fn wrap(inner: Arc<dyn Mailer>) -> Arc<dyn Mailer> {
        if inner.withholds() {
            return inner;
        }
        Arc::new(Withholding { inner })
    }

    /// Keep `email` on `outbox`, after the checks the real transport makes, so
    /// a check cannot pass on a message SMTP would refuse to build.
    fn withhold(outbox: &sc_catalog::outbox::RunOutbox, email: &Email) -> Result<()> {
        email.check()?;
        build_message(email)?;
        let body = email
            .text
            .clone()
            .or_else(|| email.html.clone())
            .unwrap_or_default();
        outbox.capture(sc_catalog::outbox::Sent::new(
            sc_catalog::outbox::SentKind::Email,
            email
                .recipients()
                .iter()
                .map(|m| m.address.clone())
                .collect(),
            email.subject.clone(),
            serde_json::Value::String(body),
        ));
        Ok(())
    }
}

#[async_trait]
impl Mailer for Withholding {
    async fn sender(&self) -> Result<Mailbox> {
        self.inner.sender().await
    }

    async fn send(&self, email: &Email) -> Result<()> {
        match sc_catalog::outbox::current() {
            Some(outbox) => Withholding::withhold(&outbox, email),
            None => self.inner.send(email).await,
        }
    }

    async fn send_via(&self, server: &EmailSettings, email: &Email) -> Result<()> {
        match sc_catalog::outbox::current() {
            Some(outbox) => {
                check_relay(server)?;
                Withholding::withhold(&outbox, email)
            }
            None => self.inner.send_via(server, email).await,
        }
    }

    fn withholds(&self) -> bool {
        true
    }
}

/// A [`Mailer`] that keeps what it was handed and sends nothing.
///
/// The way an action's behaviour is asserted without an SMTP server: the test
/// runs the trigger and then reads the message, which is a stronger statement
/// than "the call returned `Ok`".
#[derive(Debug)]
pub struct RecordingMailer {
    sent: Mutex<Vec<Email>>,
    /// The server each message in `sent` was handed to: `None` for this
    /// transport's own, the override's settings for [`Mailer::send_via`].
    servers: Mutex<Vec<Option<EmailSettings>>>,
    /// The identity this stand-in claims — see
    /// [`sending_as`](RecordingMailer::sending_as).
    from: Mailbox,
}

impl Default for RecordingMailer {
    fn default() -> RecordingMailer {
        RecordingMailer {
            sent: Mutex::new(Vec::new()),
            servers: Mutex::new(Vec::new()),
            // `.invalid` is the reserved TLD for exactly this (RFC 2606): a test
            // that does not care what the from-address is gets one that cannot
            // be a real address by construction, so a message that escaped into
            // a real transport would bounce rather than arrive from a domain
            // somebody owns.
            from: Mailbox {
                name: Some("Saltcorn".to_owned()),
                address: "recorder@example.invalid".to_owned(),
            },
        }
    }
}

impl RecordingMailer {
    /// A recorder with nothing in it.
    pub fn new() -> RecordingMailer {
        RecordingMailer::default()
    }

    /// A recorder that claims `from` as the transport's configured identity —
    /// for a test asserting that a message with no `from` of its own is sent as
    /// the installation's address.
    pub fn sending_as(from: Mailbox) -> RecordingMailer {
        RecordingMailer {
            from,
            ..RecordingMailer::default()
        }
    }

    /// Every message handed to this mailer, in order.
    pub fn sent(&self) -> Vec<Email> {
        match self.sent.lock() {
            Ok(sent) => sent.clone(),
            // A poisoned lock means a test panicked while holding it; the
            // messages recorded before that are still what the assertion wants.
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The server each message was sent through, in the order of
    /// [`sent`](RecordingMailer::sent): `None` for this transport's own, and
    /// the override's settings for one sent with [`Mailer::send_via`].
    pub fn servers(&self) -> Vec<Option<EmailSettings>> {
        match self.servers.lock() {
            Ok(servers) => servers.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Keep `email`, sent through `server`.
    fn record(&self, email: &Email, server: Option<EmailSettings>) -> Result<()> {
        // The same checks the real transport runs, so a test against the
        // recorder cannot pass on a message SMTP would refuse to build.
        build_message(email)?;
        match self.sent.lock() {
            Ok(mut sent) => sent.push(email.clone()),
            Err(poisoned) => poisoned.into_inner().push(email.clone()),
        }
        match self.servers.lock() {
            Ok(mut servers) => servers.push(server),
            Err(poisoned) => poisoned.into_inner().push(server),
        }
        Ok(())
    }

    /// The one message this mailer was handed, or an error saying how many it
    /// actually got — which is the assertion a test almost always means.
    pub fn only(&self) -> Result<Email> {
        let sent = self.sent();
        match sent.len() {
            1 => Ok(sent.into_iter().next().unwrap_or_else(|| {
                // Unreachable: the length was just checked.
                Email::new(Mailbox {
                    name: None,
                    address: String::new(),
                })
            })),
            n => Err(Error::invalid(format!(
                "expected exactly one message to have been sent, got {n}"
            ))),
        }
    }
}

#[async_trait]
impl Mailer for RecordingMailer {
    async fn sender(&self) -> Result<Mailbox> {
        Ok(self.from.clone())
    }

    async fn send(&self, email: &Email) -> Result<()> {
        self.record(email, None)
    }

    /// Recorded rather than sent, after the checks that need no network: the
    /// port and its encryption ([`check_relay`]). Whether the host is public
    /// is a DNS question, and a recorder answers none.
    async fn send_via(&self, server: &EmailSettings, email: &Email) -> Result<()> {
        check_relay(server)?;
        self.record(email, Some(server.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(raw: &str) -> Mailbox {
        parse_mailbox(raw).unwrap()
    }

    fn message(text: Option<&str>, html: Option<&str>) -> Email {
        Email {
            from: mailbox("Saltcorn <saltcorn@example.com>"),
            to: vec![mailbox("ada@example.com")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Receipt for order 7".to_owned(),
            text: text.map(str::to_owned),
            html: html.map(str::to_owned),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_recipient_list_is_split_on_the_commas_that_separate_addresses() {
        let parsed = parse_recipients("ada@example.com, Grace <grace@example.com>").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].address, "ada@example.com");
        assert_eq!(parsed[1].name.as_deref(), Some("Grace"));

        // A comma inside a quoted display name is part of the name.
        let parsed = parse_recipients("\"Lovelace, Ada\" <ada@example.com>").unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name.as_deref(), Some("Lovelace, Ada"));

        // An empty `cc` is no recipients, not an error.
        assert!(parse_recipients("").unwrap().is_empty());
        assert!(parse_recipients("  , ").unwrap().is_empty());
    }

    #[test]
    fn a_rejected_address_is_named_in_the_error() {
        let err = parse_recipients("ada@example.com, not-an-address, grace@example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not-an-address"), "{err}");
        // …and the whole list, so the admin can see which template produced it.
        assert!(err.contains("grace@example.com"), "{err}");
    }

    #[test]
    fn a_message_needs_a_recipient_and_a_body() {
        let mut email = message(Some("hello"), None);
        email.to.clear();
        assert!(email.check().unwrap_err().to_string().contains("recipient"));

        let email = message(None, None);
        assert!(email.check().unwrap_err().to_string().contains("body"));

        // A `bcc`-only message is a message: that is what a blind copy is.
        let mut email = message(Some("hello"), None);
        email.to.clear();
        email.bcc.push(mailbox("ada@example.com"));
        email.check().unwrap();
    }

    /// The headers and the MIME structure, for each of the three body shapes.
    #[test]
    fn the_built_message_has_the_headers_and_the_structure() {
        let both = String::from_utf8(
            build_message(&Email {
                cc: vec![mailbox("Grace <grace@example.com>")],
                bcc: vec![mailbox("audit@example.com")],
                ..message(Some("plain words"), Some("<p>rich words</p>"))
            })
            .unwrap()
            .formatted(),
        )
        .unwrap();
        assert!(
            both.contains("From: Saltcorn <saltcorn@example.com>"),
            "{both}"
        );
        assert!(both.contains("To: ada@example.com"), "{both}");
        assert!(both.contains("Cc: Grace <grace@example.com>"), "{both}");
        // Bcc is a recipient, not a header: it must not reach the message.
        assert!(!both.contains("audit@example.com"), "{both}");
        assert!(both.contains("Subject: Receipt for order 7"), "{both}");
        assert!(both.contains("multipart/alternative"), "{both}");
        // Text before HTML: `alternative` parts are least-preferred first, and a
        // graphical client renders the last one it understands.
        let text_at = both.find("text/plain").unwrap();
        let html_at = both.find("text/html").unwrap();
        assert!(text_at < html_at, "text should come first:\n{both}");

        let text_only = String::from_utf8(
            build_message(&message(Some("plain"), None))
                .unwrap()
                .formatted(),
        )
        .unwrap();
        assert!(text_only.contains("text/plain"), "{text_only}");
        assert!(!text_only.contains("multipart"), "{text_only}");

        let html_only = String::from_utf8(
            build_message(&message(None, Some("<p>hi</p>")))
                .unwrap()
                .formatted(),
        )
        .unwrap();
        assert!(html_only.contains("text/html"), "{html_only}");
        assert!(!html_only.contains("multipart"), "{html_only}");
    }

    /// With files, the bodies are nested *inside* a `multipart/mixed` rather
    /// than sitting beside the attachments — the difference between a message
    /// that reads as a receipt with an invoice and one that arrives as three
    /// files and no text.
    #[test]
    fn attachments_nest_the_bodies_inside_a_mixed_part() {
        let mut email = message(Some("plain words"), Some("<p>rich words</p>"));
        email.attachments = vec![
            Attachment {
                filename: "invoice-7.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                bytes: b"%PDF-1.4 not really".to_vec(),
            },
            Attachment {
                filename: "logo.png".to_owned(),
                content_type: "image/png".to_owned(),
                bytes: vec![0x89, b'P', b'N', b'G'],
            },
        ];
        let built = String::from_utf8(build_message(&email).unwrap().formatted()).unwrap();

        assert!(built.contains("multipart/mixed"), "{built}");
        // The two bodies are still alternatives of each other…
        let alternative_at = built
            .find("multipart/alternative")
            .expect("alternative part");
        // …and the files come after them, each named and typed.
        let invoice_at = built.find("invoice-7.pdf").expect("the invoice");
        assert!(alternative_at < invoice_at, "{built}");
        assert!(built.contains("application/pdf"), "{built}");
        assert!(
            built.contains(r#"attachment; filename="invoice-7.pdf""#),
            "{built}"
        );
        assert!(built.contains(r#"filename="logo.png""#), "{built}");
        // Base64, so a PNG's bytes cannot break the message.
        assert!(built.contains("base64"), "{built}");

        // One body plus a file is the same shape with one part fewer.
        let mut text_only = message(Some("plain words"), None);
        text_only.attachments = email.attachments.clone();
        let built = String::from_utf8(build_message(&text_only).unwrap().formatted()).unwrap();
        assert!(built.contains("multipart/mixed"), "{built}");
        assert!(!built.contains("multipart/alternative"), "{built}");
        assert!(built.contains("invoice-7.pdf"), "{built}");
    }

    #[test]
    fn an_attachments_content_type_is_parsed_not_pasted() {
        let mut email = message(Some("hello"), None);
        email.attachments = vec![Attachment {
            filename: "notes.txt".to_owned(),
            content_type: "not a mime type".to_owned(),
            bytes: b"hi".to_vec(),
        }];
        let err = build_message(&email).unwrap_err().to_string();
        assert!(err.contains("notes.txt"), "{err}");
        assert!(err.contains("content type"), "{err}");
    }

    /// The bytes stay out of the debug output: a two-megabyte PDF in a test
    /// failure or a log line helps nobody.
    #[test]
    fn an_attachment_debugs_as_a_size() {
        let debug = format!(
            "{:?}",
            Attachment {
                filename: "invoice.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                bytes: vec![0; 1024],
            }
        );
        assert!(debug.contains("1024 bytes"), "{debug}");
        assert!(!debug.contains("0, 0, 0"), "{debug}");
    }

    #[tokio::test]
    async fn the_recorder_keeps_what_it_was_handed() {
        let mailer = RecordingMailer::new();
        assert!(mailer.only().is_err());
        mailer.send(&message(Some("one"), None)).await.unwrap();
        assert_eq!(mailer.only().unwrap().text.as_deref(), Some("one"));
        mailer.send(&message(Some("two"), None)).await.unwrap();
        assert_eq!(mailer.sent().len(), 2);
        assert!(mailer.only().is_err());
    }

    /// The recorder runs the same checks the wire does, so a test that passes
    /// against it is a test of a message SMTP would have accepted.
    #[tokio::test]
    async fn the_recorder_refuses_what_smtp_would_refuse() {
        let mailer = RecordingMailer::new();
        assert!(mailer.send(&message(None, None)).await.is_err());
        assert!(mailer.sent().is_empty());
    }
}
