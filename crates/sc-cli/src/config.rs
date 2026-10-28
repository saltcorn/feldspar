//! `feldspar get-cfg` / `set-cfg` — the stored configuration values from the
//! command line (§13.5).
//!
//! Saltcorn's settings are rows in `_fd_config`, not a file, which is what makes
//! every node against one database agree about them — and what makes them
//! unreachable from a terminal until there is a command for it. This is that
//! command, and it exists for the reasons `build-app` and `api` do: it is
//! scriptable, it is what a deploy step or a coding agent calls, and it works
//! when there is no browser pointed at the server. Turning on the SQL echo, or
//! reading back the port a machine serves HTTPS on, should not need a session.
//!
//! This module is the **parsing and the rendering**: a key and an optional
//! value out of the arguments, a string turned into the JSON its declaration
//! says it is, and a JSON value turned back into something a shell can use.
//! Connecting and reading or writing the row is the binary's, so this half is
//! testable without a database.
//!
//! **A value arrives as a string** — that is what a terminal has — and its type
//! comes from the declaration ([`sc_config::definition`]), not from the way it
//! was written. `2525` is an integer because `smtp_port` is declared `int`, and
//! `on` is refused for `log_sql` because `bool` means `true` or `false` (plus
//! the handful of spellings below, which a shell script is likely to produce).
//! The alternative — guessing the type from the text — would store `"2525"` for
//! a port and leave the server reading a string where it wants a number.

use sc_error::{Error, Result};
use sc_types::{BasicType, FormField, SECRET_SENTINEL};
use serde_json::Value as Json;

/// The arguments of `get-cfg`: which key, or all of them.
#[derive(Debug, Clone, PartialEq)]
pub struct GetArgs {
    /// The key to print, or `None` to print every declared key.
    pub key: Option<String>,
    /// `--json`: print JSON — the value itself for one key, and an object of
    /// every declared key for a listing (secrets redacted, unset keys `null`).
    pub json: bool,
}

/// What `set-cfg` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum SetCommand {
    /// `set-cfg KEY [VALUE]`: one key.
    One(SetArgs),
    /// `set-cfg --json`: a JSON object of keys on stdin, checked together and
    /// written in one transaction.
    Many,
    /// `set-cfg --unset KEY`: delete the stored value, so the key returns to
    /// its declared default.
    Unset(String),
}

/// The arguments of `set-cfg`: which key, and the value if it was given on the
/// command line rather than waiting on stdin.
#[derive(Debug, Clone, PartialEq)]
pub struct SetArgs {
    /// The key to write.
    pub key: String,
    /// The value, when it came from the command line. `None` means "read stdin",
    /// which is how a certificate or a private key is set without quoting a
    /// multi-line string into a shell.
    pub value: Option<String>,
}

/// Parse `get-cfg [KEY]` — whatever the database flags left behind.
///
/// A second positional is refused rather than ignored: `get-cfg a b` is a typo
/// (or a shell that split a value), and printing `a` while dropping `b` would
/// answer a question nobody asked.
pub fn parse_get(args: &[String]) -> Result<GetArgs> {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    let positional = positional("get-cfg", &args)?;
    let mut it = positional.into_iter();
    let key = it.next();
    if let Some(extra) = it.next() {
        return Err(Error::config(format!(
            "get-cfg takes one configuration key at most, and got `{extra}` as well; \
             run it with no key to print every value"
        )));
    }
    Ok(GetArgs { key, json })
}

/// Parse `set-cfg`'s three forms: `--json` alone, `--unset KEY`, or
/// `KEY [VALUE]` ([`parse_set`]).
pub fn parse_set_command(args: &[String]) -> Result<SetCommand> {
    match args {
        [flag] if flag == "--json" => Ok(SetCommand::Many),
        [flag, ..] if flag == "--json" => Err(Error::config(
            "set-cfg --json takes no other arguments: the keys and values are a JSON object \
             on stdin",
        )),
        [flag, key] if flag == "--unset" => Ok(SetCommand::Unset(key.clone())),
        [flag, ..] if flag == "--unset" => Err(Error::config(
            "set-cfg --unset takes exactly one configuration key",
        )),
        _ => parse_set(args).map(SetCommand::One),
    }
}

/// The values of a `set-cfg --json` document, each one in the type its key
/// is declared to hold.
///
/// A value already of the right JSON type is kept; a **string** for a key that
/// is not text is read as a terminal argument would be ([`value_for`]), so
/// `{"smtp_port": "587"}` — the shape a shell script tends to produce — is the
/// number 587. `null` unsets the key. An undeclared key, or the redaction
/// sentinel as a value, is refused naming the key, before anything is written.
pub fn values_from_json(document: &Json) -> Result<sc_types::Attrs> {
    let Json::Object(object) = document else {
        return Err(Error::invalid(
            "set-cfg --json reads a JSON object of configuration keys and values",
        ));
    };
    let mut out = sc_types::Attrs::new();
    for (key, value) in object {
        let field = sc_config::definition(key).ok_or_else(|| {
            Error::invalid(format!(
                "no configuration key `{key}`; known keys are {}",
                sc_config::known_keys().join(", ")
            ))
        })?;
        let value = match value {
            Json::String(raw)
                if field
                    .base
                    .type_
                    .as_basic()
                    .is_some_and(|b| !matches!(b, BasicType::Text)) =>
            {
                value_for(&field, raw)?
            }
            other => other.clone(),
        };
        refuse_sentinel(key, &value)?;
        out.insert(key.clone(), value);
    }
    Ok(out)
}

/// Parse `set-cfg KEY [VALUE]` — whatever the database flags left behind.
///
/// With a value it is written; without one the value is read from **stdin**,
/// whole. That is not a convenience: `ssl_certificate` and `ssl_private_key` are
/// multi-line PEM, and a command that could only take an argument would mean
/// quoting a certificate into a shell.
pub fn parse_set(args: &[String]) -> Result<SetArgs> {
    let positional = positional("set-cfg", args)?;
    let mut it = positional.into_iter();
    let key = it.next().ok_or_else(|| {
        Error::config(
            "set-cfg needs the configuration key to write: \
             feldspar set-cfg KEY [VALUE]  (without VALUE the value is read from stdin)",
        )
    })?;
    let value = it.next();
    if let Some(extra) = it.next() {
        return Err(Error::config(format!(
            "set-cfg takes one key and one value, and got `{extra}` as well; \
             quote the value if it contains spaces, or pipe it in on stdin"
        )));
    }
    Ok(SetArgs { key, value })
}

/// The positional arguments in `args`, refusing anything that looks like a flag.
///
/// The database flags have already been taken out by [`DbConfig::extract`](crate::DbConfig::extract),
/// so what is left starting with `-` is a flag this command does not have —
/// which fails loudly here rather than being silently written as a value.
fn positional(command: &str, args: &[String]) -> Result<Vec<String>> {
    for arg in args {
        // A lone `-` is a value (some tools spell stdin that way), and a
        // negative number is a value too; neither is a flag.
        if arg.starts_with('-') && arg.len() > 1 && !arg[1..].starts_with(|c: char| c.is_numeric())
        {
            return Err(Error::config(format!("unknown {command} argument `{arg}`")));
        }
    }
    Ok(args.to_vec())
}

/// Turn the string a terminal supplied into the JSON `field` is declared to
/// hold.
///
/// The declared type decides, so the result is checked once more by
/// [`sc_config::set_config`] against the very same declaration — this only has
/// to produce the right *shape*; options and required-ness stay that function's
/// business, in the one place every writer goes through.
///
/// - `bool`: `true`/`false`, and the spellings a shell script produces —
///   `yes`/`no`, `on`/`off`, `1`/`0` — case-insensitively.
/// - `int`, `float`: parsed, so a port that is not a number is refused here
///   rather than stored as text.
/// - `json`: parsed as JSON, so `set-cfg backup_include '{"tables":true}'` is a
///   document and not a string that looks like one.
/// - everything else — `text` above all, and the string-encoded families like
///   `date` and `uuid` — is the string itself, which is what
///   [`BasicType::accepts_json`] checks it against.
pub fn value_for(field: &FormField, raw: &str) -> Result<Json> {
    let key = field.name();
    let Some(basic) = field.base.type_.as_basic() else {
        return Ok(Json::String(raw.to_owned()));
    };
    match basic {
        BasicType::Bool => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Json::Bool(true)),
            "false" | "no" | "off" | "0" => Ok(Json::Bool(false)),
            other => Err(Error::invalid(format!(
                "`{key}` is a yes/no setting, so it takes true or false, not `{other}`"
            ))),
        },
        BasicType::Int => raw
            .trim()
            .parse::<i64>()
            .map(Json::from)
            .map_err(|_| Error::invalid(format!("`{key}` is a number, and `{raw}` is not one"))),
        BasicType::Float | BasicType::Decimal => raw
            .trim()
            .parse::<f64>()
            .map(Json::from)
            .map_err(|_| Error::invalid(format!("`{key}` is a number, and `{raw}` is not one"))),
        BasicType::Json => serde_json::from_str(raw).map_err(|e| {
            Error::invalid(format!(
                "`{key}` holds a JSON document, and this is not one: {e}"
            ))
        }),
        _ => Ok(Json::String(raw.to_owned())),
    }
}

/// Refuse the redaction sentinel as a value.
///
/// A listing shows a secret as [`SECRET_SENTINEL`], so the one way to end up
/// typing it is to have copied it out of one. Storing it would produce a setting
/// that *looks* configured and authenticates with `••••••••` — the same failure
/// [`sc_types::merge_secrets`] avoids on a form save, in the one other place a
/// redacted value can come back.
pub fn refuse_sentinel(key: &str, value: &Json) -> Result<()> {
    if value.as_str() == Some(SECRET_SENTINEL) {
        return Err(Error::invalid(format!(
            "`{key}` was given the redaction `{SECRET_SENTINEL}` that a listing prints \
             in place of a secret; pass the real value, or leave the setting alone"
        )));
    }
    Ok(())
}

/// Strip the one trailing newline a pipe or a heredoc adds, and nothing else.
///
/// `echo 2525 | feldspar set-cfg smtp_port` should not store a newline, and a
/// certificate read from a file should keep every byte of its interior. One
/// newline is the artefact of the *transport*; a second would be part of the
/// value.
pub fn strip_final_newline(raw: &str) -> &str {
    raw.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(raw)
}

/// A value as a **shell** wants it: a string is its own text, everything else is
/// its JSON.
///
/// So `get-cfg ssl_mode` prints `letsencrypt` and not `"letsencrypt"`, and
/// `get-cfg ssl_certificate > cert.pem` writes a certificate rather than a
/// quoted one-line escape of it. That is the whole reason to print a single key
/// on its own: it is the value, ready to be captured.
pub fn render(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A value as a **listing** wants it: one line, unambiguous.
///
/// A plain one-line string is printed bare, for the same readability
/// [`render`] is after; anything that would break the line — or hide leading
/// space — is printed as JSON instead, so `key=value` stays one setting per
/// line however multi-line the certificate is.
pub fn render_inline(value: &Json) -> String {
    match value {
        Json::String(s)
            if !s.contains(['\n', '\r'])
                && s.trim() == s
                && !s.starts_with('"')
                && !s.is_empty() =>
        {
            s.clone()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_types::BasicType;
    use serde_json::json;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn get_takes_one_key_or_none() {
        assert_eq!(parse_get(&args(&[])).unwrap().key, None);
        let json = parse_get(&args(&["--json", "smtp_port"])).unwrap();
        assert_eq!((json.key.as_deref(), json.json), (Some("smtp_port"), true));
        assert_eq!(
            parse_get(&args(&["ssl_mode"])).unwrap().key,
            Some("ssl_mode".to_owned())
        );
        let msg = parse_get(&args(&["ssl_mode", "letsencrypt"]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("letsencrypt"), "{msg}");
    }

    #[test]
    fn set_takes_a_key_and_an_optional_value() {
        let parsed = parse_set(&args(&["smtp_port", "587"])).unwrap();
        assert_eq!(parsed.key, "smtp_port");
        assert_eq!(parsed.value.as_deref(), Some("587"));
        // No value is not an error: it means the value is on stdin.
        assert_eq!(parse_set(&args(&["smtp_port"])).unwrap().value, None);
        // …but no key at all is, and the message says what the command wanted.
        let msg = parse_set(&args(&[])).unwrap_err().to_string();
        assert!(msg.contains("KEY"), "{msg}");
    }

    /// The database flags are gone by the time this parses, so a leftover flag
    /// is one the command does not have — not a value to store under it.
    #[test]
    fn a_flag_this_command_does_not_have_is_refused() {
        let msg = parse_set(&args(&["log_sql", "--force"]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("--force"), "{msg}");
        assert!(parse_get(&args(&["--all"])).is_err());
        // A negative number is a value, not a flag.
        assert_eq!(
            parse_set(&args(&["log_verbosity", "-1"])).unwrap().value,
            Some("-1".to_owned())
        );
    }

    #[test]
    fn a_value_takes_the_type_its_declaration_gives_it() {
        let port = FormField::new("smtp_port", BasicType::Int);
        assert_eq!(value_for(&port, "8443").unwrap(), json!(8443));
        assert_eq!(value_for(&port, " 8443\t").unwrap(), json!(8443));
        let msg = value_for(&port, "yes").unwrap_err().to_string();
        assert!(msg.contains("smtp_port"), "{msg}");

        let echo = FormField::new("log_sql", BasicType::Bool);
        assert_eq!(value_for(&echo, "true").unwrap(), json!(true));
        assert_eq!(value_for(&echo, "YES").unwrap(), json!(true));
        assert_eq!(value_for(&echo, "off").unwrap(), json!(false));
        assert_eq!(value_for(&echo, "0").unwrap(), json!(false));
        assert!(value_for(&echo, "sortof").is_err());

        // Text keeps every byte, including the newlines a PEM block is made of.
        let cert = FormField::new("ssl_certificate", BasicType::Text);
        assert_eq!(
            value_for(&cert, "-----BEGIN-----\nabc\n").unwrap(),
            json!("-----BEGIN-----\nabc\n")
        );
        // …and a number is text when the declaration says text, so a port typed
        // into a text setting is not silently turned into an integer.
        assert_eq!(value_for(&cert, "8443").unwrap(), json!("8443"));

        let bag = FormField::new("backup_include", BasicType::Json);
        assert_eq!(
            value_for(&bag, r#"{"tables":true}"#).unwrap(),
            json!({"tables": true})
        );
        assert!(value_for(&bag, "{tables}").is_err());
    }

    #[test]
    fn the_transports_trailing_newline_is_not_part_of_the_value() {
        assert_eq!(strip_final_newline("8443\n"), "8443");
        assert_eq!(strip_final_newline("8443\r\n"), "8443");
        // Only one, and only at the end.
        assert_eq!(strip_final_newline("a\n\n"), "a\n");
        assert_eq!(strip_final_newline("a\nb"), "a\nb");
        assert_eq!(strip_final_newline(""), "");
    }

    /// Printing a single key prints the *value*, so it can be captured; the
    /// listing prints one line per setting, so a certificate does not turn one
    /// row into thirty.
    #[test]
    fn a_string_prints_bare_and_a_multi_line_one_prints_as_json() {
        assert_eq!(render(&json!("letsencrypt")), "letsencrypt");
        assert_eq!(render(&json!(8443)), "8443");
        assert_eq!(render(&json!(true)), "true");
        assert_eq!(render(&json!("a\nb")), "a\nb");

        assert_eq!(render_inline(&json!("letsencrypt")), "letsencrypt");
        assert_eq!(render_inline(&json!(8443)), "8443");
        assert_eq!(render_inline(&json!("a\nb")), r#""a\nb""#);
        assert_eq!(render_inline(&json!(" padded ")), r#"" padded ""#);
        assert_eq!(render_inline(&json!("")), r#""""#);
    }

    #[test]
    fn set_has_a_json_form_and_an_unset_form() {
        assert_eq!(
            parse_set_command(&args(&["--json"])).unwrap(),
            SetCommand::Many
        );
        assert_eq!(
            parse_set_command(&args(&["--unset", "smtp_host"])).unwrap(),
            SetCommand::Unset("smtp_host".into())
        );
        assert!(matches!(
            parse_set_command(&args(&["smtp_port", "587"])).unwrap(),
            SetCommand::One(_)
        ));
        for bad in [&["--json", "x"][..], &["--unset"], &["--unset", "a", "b"]] {
            assert!(parse_set_command(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_json_document_is_typed_by_the_declarations() {
        let values = values_from_json(&json!({
            "smtp_host": "mail.example",
            "smtp_port": "587",
            "log_sql": "yes",
            "smtp_username": null,
        }))
        .unwrap();
        assert_eq!(values["smtp_host"], json!("mail.example"));
        assert_eq!(values["smtp_port"], json!(587));
        assert_eq!(values["log_sql"], json!(true));
        assert_eq!(values["smtp_username"], Json::Null);
        // An undeclared key is named, and nothing is returned to write.
        let msg = values_from_json(&json!({"smtp_host": "x", "smtp_hots": "y"}))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("smtp_hots"), "{msg}");
        assert!(values_from_json(&json!({"smtp_port": "many"})).is_err());
        assert!(values_from_json(&json!({"smtp_password": SECRET_SENTINEL})).is_err());
        assert!(values_from_json(&json!(["smtp_host"])).is_err());
    }

    #[test]
    fn the_redaction_is_not_a_value() {
        assert!(refuse_sentinel("smtp_password", &json!("hunter2")).is_ok());
        let msg = refuse_sentinel("smtp_password", &json!(SECRET_SENTINEL))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("smtp_password"), "{msg}");
    }
}
