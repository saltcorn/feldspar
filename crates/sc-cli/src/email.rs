//! `feldspar send-email --to ADDR --subject TEXT [--html] < body`: one message
//! through the stored SMTP settings and the transport the server sends with.
//!
//! The point is that it is **the server's own sender**: `sc_email::SmtpMailer`
//! over `EmailSettings::load`, the pair Settings → Email's *Send test message*
//! uses — so `smtp_security`, `smtp_certificate_check` and
//! `smtp_certificate_name` mean exactly what they mean to a trigger's
//! `send_email`, and a script that wants to know whether feldspar can send
//! mail does not need its own SMTP client that copies those rules.
//!
//! It needs the database and outbound SMTP and nothing else — no modules, no
//! server, no session — so it runs from a network sandbox that allows only
//! those two.

use sc_error::{Error, Result};

/// `send-email`'s arguments, the database flags already taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendArgs {
    /// `--to`: one address or a comma-separated list.
    pub to: String,
    /// `--subject`.
    pub subject: String,
    /// `--html`: the body on stdin is HTML rather than plain text.
    pub html: bool,
}

/// Parse `send-email --to ADDR --subject TEXT [--html]`.
pub fn parse_send(args: &[String]) -> Result<SendArgs> {
    let usage = "send-email --to ADDR --subject TEXT [--html] [database flags] < body";
    let mut to = None;
    let mut subject = None;
    let mut html = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--to" => &mut to,
            "--subject" => &mut subject,
            "--html" => {
                html = true;
                continue;
            }
            other => {
                return Err(Error::config(format!(
                    "unknown send-email argument `{other}`; usage: {usage}"
                )));
            }
        };
        let value = it
            .next()
            .ok_or_else(|| Error::config(format!("{arg} needs a value")))?;
        if slot.replace(value.clone()).is_some() {
            return Err(Error::config(format!("{arg} is given more than once")));
        }
    }
    Ok(SendArgs {
        to: to.ok_or_else(|| Error::config(format!("--to is required; usage: {usage}")))?,
        subject: subject
            .ok_or_else(|| Error::config(format!("--subject is required; usage: {usage}")))?,
        html,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_message_needs_a_recipient_and_a_subject() {
        assert_eq!(
            parse_send(&args(&[
                "--to",
                "ops@example.com",
                "--subject",
                "Backup failed"
            ]))
            .unwrap(),
            SendArgs {
                to: "ops@example.com".into(),
                subject: "Backup failed".into(),
                html: false,
            }
        );
        assert!(
            parse_send(&args(&["--html", "--subject", "s", "--to", "a@b.c"]))
                .unwrap()
                .html
        );
        for bad in [
            &["--to", "a@b.c"][..],
            &["--subject", "s"],
            &["--to", "a@b.c", "--subject"],
            &["--to", "a@b.c", "--to", "d@e.f", "--subject", "s"],
            &["--to", "a@b.c", "--subject", "s", "--cc", "x@y.z"],
        ] {
            assert!(parse_send(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
