//! Who an automated backup tells when it fails, and what it says.
//!
//! Each schedule names its recipient ([`FailureEmail`]): nobody, every
//! administrator, or one administrator by email address. The choice is offered
//! only when the installation sends mail (Settings → Email), and a recipient is
//! always someone whose base role is admin when the message is sent — the
//! address is looked up again then, so an administrator who has since been
//! demoted, disabled or deleted is not written to, and "all" means the
//! administrators there are on the day it fails, not on the day it was saved.
//!
//! **Once per run of failures.** The scheduler retries a failed backup every
//! hour ([`is_due`](crate::schedule::is_due)); an email for each retry would be
//! a full inbox by morning. The message goes out on the first failure after a
//! success (or the first run), and says that no more will follow until a
//! backup succeeds again.
//!
//! Sending is best effort: a failure email that cannot be sent is logged, and
//! changes nothing about the schedule's status, which records the backup's
//! failure, not the mail's.

use chrono::{DateTime, Utc};
use sc_catalog::Catalog;
use sc_email::{Email, Mailer};
use sc_error::{Error, Result};
use sc_query::Value;
use sc_types::Role;
use serde_json::Value as Json;

/// The stored and wire value of [`FailureEmail::All`].
pub const ALL: &str = "all";

/// Who is emailed when a schedule's backup fails.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FailureEmail {
    /// Nobody: what a schedule saved before the choice existed has.
    #[default]
    None,
    /// Every administrator.
    All,
    /// The administrator with this email address.
    To(String),
}

impl FailureEmail {
    /// Read a body's or a stored record's `failure_email`: absent, null or
    /// empty is nobody, `"all"` every administrator, and anything else an
    /// email address.
    pub fn from_json(value: Option<&Json>) -> Result<FailureEmail> {
        match value {
            None | Some(Json::Null) => Ok(FailureEmail::None),
            Some(Json::String(s)) => match s.trim() {
                "" => Ok(FailureEmail::None),
                ALL => Ok(FailureEmail::All),
                address => {
                    let mailbox = sc_config::parse_mailbox(address).map_err(|e| {
                        Error::invalid(format!("`failure_email` is not an email address: {e}"))
                    })?;
                    Ok(FailureEmail::To(mailbox.address))
                }
            },
            Some(_) => Err(Error::invalid(
                "`failure_email` must be null, \"all\" or an administrator's email address",
            )),
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            FailureEmail::None => Json::Null,
            FailureEmail::All => Json::String(ALL.to_owned()),
            FailureEmail::To(address) => Json::String(address.clone()),
        }
    }

    /// Refuse an address that is not one of `admins`'. Run when a schedule is
    /// saved: the dropdown offers only administrators, and the API holds a
    /// script to the same rule.
    pub fn check(&self, admins: &[String]) -> Result<()> {
        match self {
            FailureEmail::To(address) if !is_among(address, admins) => {
                Err(Error::invalid(format!(
                    "`failure_email`: {address} is not an administrator; choose an \
                     administrator, \"all\" or nobody"
                )))
            }
            _ => Ok(()),
        }
    }

    /// The addresses a failure is sent to, out of the administrators there are
    /// now.
    pub fn recipients(&self, admins: &[String]) -> Vec<String> {
        match self {
            FailureEmail::None => Vec::new(),
            FailureEmail::All => admins.to_vec(),
            FailureEmail::To(address) => admins
                .iter()
                .filter(|a| a.eq_ignore_ascii_case(address))
                .cloned()
                .collect(),
        }
    }
}

fn is_among(address: &str, admins: &[String]) -> bool {
    admins.iter().any(|a| a.eq_ignore_ascii_case(address))
}

/// The email addresses of the administrators — base role admin, not disabled,
/// and not a check user (an account a checklist signs in as) — by address.
pub async fn admin_addresses(catalog: &Catalog) -> Result<Vec<String>> {
    let users = sc_auth::users_with_base_role(catalog, Role::ADMIN).await?;
    Ok(users
        .iter()
        .filter(|u| !u.is_disabled() && !u.is_check_user())
        .filter_map(|u| match u.get(sc_auth::COL_EMAIL) {
            Some(Value::Text(email)) if !email.trim().is_empty() => Some(email.trim().to_owned()),
            _ => None,
        })
        .collect())
}

/// The message: which backup failed, when, and why.
pub fn failure_message(
    from: sc_config::Mailbox,
    to: &[String],
    location: &str,
    error: &str,
    at: DateTime<Utc>,
) -> Email {
    let mut email = Email::new(from);
    email.to = to
        .iter()
        .map(|address| sc_config::Mailbox {
            name: None,
            address: address.clone(),
        })
        .collect();
    email.subject = format!("Automated backup to {location} failed");
    email.text = Some(format!(
        "The automated backup to {location} failed at {} UTC:\n\n\
         {error}\n\n\
         It will be tried again every hour. You will not be emailed about this \
         backup again until it has succeeded.\n\n\
         The schedule is in Settings → Backup → Automated backups.\n",
        at.format("%Y-%m-%d %H:%M"),
    ));
    email
}

/// Email the schedule's recipients that its backup failed. Logs, never fails.
pub async fn send_failure_email(
    catalog: &Catalog,
    mailer: &dyn Mailer,
    recipient: &FailureEmail,
    location: &str,
    error: &str,
    at: DateTime<Utc>,
) {
    if *recipient == FailureEmail::None {
        return;
    }
    let result = async {
        let to = recipient.recipients(&admin_addresses(catalog).await?);
        if to.is_empty() {
            return Err(Error::invalid(format!(
                "no administrator has the address {}",
                match recipient {
                    FailureEmail::To(address) => address.as_str(),
                    _ => "(any)",
                }
            )));
        }
        let from = mailer.sender().await?;
        mailer
            .send(&failure_message(from, &to, location, error, at))
            .await
    }
    .await;
    if let Err(e) = result {
        sc_log::log_warn!("could not email that the automated backup to `{location}` failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_body_names_nobody_everybody_or_an_address() {
        assert_eq!(FailureEmail::from_json(None).unwrap(), FailureEmail::None);
        assert_eq!(
            FailureEmail::from_json(Some(&Json::Null)).unwrap(),
            FailureEmail::None
        );
        assert_eq!(
            FailureEmail::from_json(Some(&json!(""))).unwrap(),
            FailureEmail::None
        );
        assert_eq!(
            FailureEmail::from_json(Some(&json!("all"))).unwrap(),
            FailureEmail::All
        );
        let to = FailureEmail::from_json(Some(&json!(" ada@example.com "))).unwrap();
        assert_eq!(to, FailureEmail::To("ada@example.com".into()));
        for value in [FailureEmail::None, FailureEmail::All, to] {
            assert_eq!(
                FailureEmail::from_json(Some(&value.to_json())).unwrap(),
                value
            );
        }
        assert!(FailureEmail::from_json(Some(&json!("not an address"))).is_err());
        assert!(FailureEmail::from_json(Some(&json!(3))).is_err());
    }

    #[test]
    fn only_an_administrator_is_written_to() {
        let admins = vec!["ada@example.com".to_owned(), "bob@example.com".to_owned()];
        let ada = FailureEmail::To("Ada@Example.com".into());
        assert!(ada.check(&admins).is_ok());
        assert_eq!(ada.recipients(&admins), vec!["ada@example.com"]);
        assert_eq!(FailureEmail::All.recipients(&admins), admins);
        assert!(FailureEmail::None.recipients(&admins).is_empty());
        let eve = FailureEmail::To("eve@example.com".into());
        assert!(eve.check(&admins).is_err());
        // Demoted since: nobody to write to.
        assert!(eve.recipients(&admins).is_empty());
        assert!(FailureEmail::All.check(&[]).is_ok());
    }
}
