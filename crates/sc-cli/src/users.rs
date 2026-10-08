//! `feldspar add-user` and `feldspar modify-user`: accounts, from a terminal.
//!
//! The way back in for an admin who has forgotten their password on a server
//! with no SMTP configured — where the reset email that would otherwise do this
//! has nowhere to go. Like `auth token`, these commands hold the primary
//! database's credentials, which is already the authority to rewrite any row of
//! the users table; they go through `sc-auth`'s own create and update functions
//! so the password is hashed and the role checked exactly as the admin UI does.
//!
//! A password is never required on the command line, where it would land in the
//! shell's history and in `ps`: `--password` with no value asks for it, without
//! echo, at the terminal — and reads one line from stdin when stdin is not a
//! terminal, so a script can pipe it in.

use std::io::{BufRead, IsTerminal, Write};

use sc_error::{Error, Result};

/// What `--password` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordArg {
    /// `--password VALUE` or `--password=VALUE`.
    Value(String),
    /// `--password` with no value: ask for it.
    Prompt,
}

/// What `add-user` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddUserArgs {
    pub email: String,
    /// A role name or number, resolved against the database.
    pub role: String,
    /// `None` is the same as [`PasswordArg::Prompt`]: a new account needs one.
    pub password: Option<PasswordArg>,
}

/// What `modify-user` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifyUserArgs {
    pub email: String,
    pub role: Option<String>,
    pub password: Option<PasswordArg>,
}

/// The flags both commands share, and the one positional argument.
#[derive(Default)]
struct Parsed {
    email: Option<String>,
    role: Option<String>,
    password: Option<PasswordArg>,
}

fn parse(command: &str, args: &[String]) -> Result<Parsed> {
    let mut parsed = Parsed::default();
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        if let Some(value) = arg.strip_prefix("--password=") {
            parsed.password = Some(PasswordArg::Value(value.to_owned()));
        } else if arg == "--password" {
            // The value is optional, so the next argument is it only when it
            // is not itself a flag. A password that starts with `--` is
            // written `--password=--…`.
            parsed.password = Some(match it.next_if(|next| !next.starts_with("--")) {
                Some(value) => PasswordArg::Value(value.clone()),
                None => PasswordArg::Prompt,
            });
        } else if arg == "--role" {
            parsed.role = Some(
                it.next()
                    .ok_or_else(|| Error::config("--role needs a value: a role name or number"))?
                    .clone(),
            );
        } else if arg.starts_with("--") {
            return Err(Error::config(format!("unknown {command} argument `{arg}`")));
        } else if parsed.email.is_none() {
            parsed.email = Some(arg.clone());
        } else {
            return Err(Error::config(format!(
                "{command} takes one email address, not `{}` and `{arg}`",
                parsed.email.as_deref().unwrap_or_default()
            )));
        }
    }
    Ok(parsed)
}

/// Parse `add-user EMAIL --role ROLE [--password [VALUE]]`.
pub fn parse_add_user(args: &[String]) -> Result<AddUserArgs> {
    let parsed = parse("add-user", args)?;
    let email = parsed
        .email
        .ok_or_else(|| Error::config("add-user needs the new user's email address"))?;
    let role = parsed.role.ok_or_else(|| {
        Error::config("add-user needs --role: a role name (e.g. admin) or number")
    })?;
    Ok(AddUserArgs {
        email,
        role,
        password: parsed.password,
    })
}

/// Parse `modify-user EMAIL [--role ROLE] [--password [VALUE]]`.
///
/// A command that changes nothing is refused: it is a caller who forgot the
/// flag, and saying "done" would have them try to sign in with a password that
/// was never set.
pub fn parse_modify_user(args: &[String]) -> Result<ModifyUserArgs> {
    let parsed = parse("modify-user", args)?;
    let email = parsed.email.ok_or_else(|| {
        Error::config("modify-user needs the email address of the user to change")
    })?;
    if parsed.role.is_none() && parsed.password.is_none() {
        return Err(Error::config(
            "modify-user needs something to change: --role ROLE and/or --password [VALUE]",
        ));
    }
    Ok(ModifyUserArgs {
        email,
        role: parsed.role,
        password: parsed.password,
    })
}

/// The password `arg` names, asking for it if it names none.
pub fn password_from(arg: Option<&PasswordArg>) -> Result<String> {
    let password = match arg {
        Some(PasswordArg::Value(value)) => value.clone(),
        Some(PasswordArg::Prompt) | None => prompt_password()?,
    };
    if password.is_empty() {
        return Err(Error::invalid("the password must not be blank"));
    }
    Ok(password)
}

/// Ask for a password: twice and without echo at a terminal, or one line of
/// stdin when it is not one.
fn prompt_password() -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| Error::msg(format!("reading the password from stdin: {e}")))?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_owned());
    }
    let first = read_hidden("Password: ")?;
    let second = read_hidden("Confirm password: ")?;
    if first != second {
        return Err(Error::invalid("the two passwords did not match"));
    }
    Ok(first)
}

/// Read one line from the terminal with echo turned off, restoring it after —
/// including when the read fails.
fn read_hidden(prompt: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    let _echo = EchoOff::new();
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    read.map_err(|e| Error::msg(format!("reading the password: {e}")))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Terminal echo, off for as long as this lives.
struct EchoOff {
    saved: Option<libc::termios>,
}

impl EchoOff {
    fn new() -> EchoOff {
        // SAFETY: `termios` is plain data, and `tcgetattr`/`tcsetattr` only read
        // and write the struct they are handed. A failure (stdin not a tty after
        // all) leaves echo as it was, which is a visible password rather than a
        // broken terminal.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return EchoOff { saved: None };
            }
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            // …but the newline is, so the next prompt starts on its own line.
            term.c_lflag |= libc::ECHONL;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) != 0 {
                return EchoOff { saved: None };
            }
            EchoOff { saved: Some(saved) }
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            // SAFETY: restores the attributes `tcgetattr` read above.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn add_user_takes_an_email_a_role_and_an_optional_password() {
        let parsed = parse_add_user(&args(&["a@example.com", "--role", "admin"])).unwrap();
        assert_eq!(parsed.email, "a@example.com");
        assert_eq!(parsed.role, "admin");
        assert_eq!(parsed.password, None);

        let parsed =
            parse_add_user(&args(&["--role", "1", "a@example.com", "--password", "pw"])).unwrap();
        assert_eq!(parsed.password, Some(PasswordArg::Value("pw".into())));

        assert!(parse_add_user(&args(&["a@example.com"])).is_err());
        assert!(parse_add_user(&args(&["--role", "admin"])).is_err());
    }

    #[test]
    fn a_password_flag_with_no_value_asks_for_one() {
        let parsed = parse_modify_user(&args(&["a@example.com", "--password"])).unwrap();
        assert_eq!(parsed.password, Some(PasswordArg::Prompt));
        // Followed by another flag, it still has no value.
        let parsed =
            parse_modify_user(&args(&["a@example.com", "--password", "--role", "admin"])).unwrap();
        assert_eq!(parsed.password, Some(PasswordArg::Prompt));
        assert_eq!(parsed.role.as_deref(), Some("admin"));
        // A password that looks like a flag is written with `=`.
        let parsed = parse_modify_user(&args(&["a@example.com", "--password=--x"])).unwrap();
        assert_eq!(parsed.password, Some(PasswordArg::Value("--x".into())));
    }

    #[test]
    fn modify_user_needs_something_to_change() {
        let err = parse_modify_user(&args(&["a@example.com"])).unwrap_err();
        assert!(err.to_string().contains("--role"), "{err}");
        assert!(parse_modify_user(&args(&["a@example.com", "--bogus"])).is_err());
        assert!(
            parse_modify_user(&args(&["a@example.com", "b@example.com", "--password"])).is_err()
        );
    }
}
