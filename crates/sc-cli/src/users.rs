//! `feldspar add-user`, `modify-user`, `delete-user` and `role`: accounts and
//! roles, from a terminal.
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
//! terminal, so a script can pipe it in. `--random-password` has one generated
//! and printed once on stdout.
//!
//! stdin is only read when it was asked for. `add-user` with neither flag asks
//! at a terminal, but with no terminal it is refused rather than reading stdin:
//! in a `while read …; do feldspar add-user …; done < users.txt` loop, stdin is
//! the loop's own file, and the next line of it would silently become the new
//! account's password. And a line read from stdin is read byte by byte, so
//! nothing after it is taken from whoever reads stdin next.
//!
//! May use `unsafe` (the workspace denies it) for the hidden password prompt:
//! an unbuffered `libc::read` of stdin, and `tcgetattr`/`tcsetattr` to turn
//! echo off and back on.
#![allow(unsafe_code)]

use sc_types::Role;
use std::io::{BufRead, IsTerminal, Read, Write};

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
    /// `None` asks at a terminal and is refused without one (see
    /// [`password_from`]): stdin is read only when `--password` asks for it.
    pub password: Option<PasswordArg>,
    /// `--random-password`: generate one and print it on stdout.
    pub random_password: bool,
}

/// What `modify-user` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifyUserArgs {
    pub email: String,
    pub role: Option<String>,
    pub password: Option<PasswordArg>,
    /// `--random-password`: generate one and print it on stdout.
    pub random_password: bool,
    /// `--disabled` (`Some(true)`) or `--enabled` (`Some(false)`) (TODO F1.8).
    pub disabled: Option<bool>,
    /// `--reset-second-factor`: turn off the account's two-factor
    /// authentication — how a sole admin who lost their phone and their
    /// recovery codes gets back in.
    pub reset_second_factor: bool,
}

/// The flags both commands share, and the one positional argument.
#[derive(Default)]
struct Parsed {
    email: Option<String>,
    role: Option<String>,
    password: Option<PasswordArg>,
    random_password: bool,
    disabled: Option<bool>,
    reset_second_factor: bool,
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
        } else if arg == "--random-password" {
            parsed.random_password = true;
        } else if arg == "--role" {
            parsed.role = Some(
                it.next()
                    .ok_or_else(|| Error::config("--role needs a value: a role name or number"))?
                    .clone(),
            );
        } else if (arg == "--disabled" || arg == "--enabled") && command == "modify-user" {
            let disabled = arg == "--disabled";
            if parsed.disabled.is_some_and(|d| d != disabled) {
                return Err(Error::config(
                    "modify-user takes --disabled or --enabled, not both",
                ));
            }
            parsed.disabled = Some(disabled);
        } else if arg == "--reset-second-factor" && command == "modify-user" {
            parsed.reset_second_factor = true;
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
    if parsed.random_password && parsed.password.is_some() {
        return Err(Error::config(format!(
            "{command} takes --password or --random-password, not both"
        )));
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
        random_password: parsed.random_password,
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
    if parsed.role.is_none()
        && parsed.password.is_none()
        && !parsed.random_password
        && parsed.disabled.is_none()
        && !parsed.reset_second_factor
    {
        return Err(Error::config(
            "modify-user needs something to change: --role ROLE, --password [VALUE], \
             --random-password, --disabled, --enabled or --reset-second-factor",
        ));
    }
    Ok(ModifyUserArgs {
        email,
        role: parsed.role,
        password: parsed.password,
        random_password: parsed.random_password,
        disabled: parsed.disabled,
        reset_second_factor: parsed.reset_second_factor,
    })
}

/// Parse `delete-user EMAIL` (TODO F1.8): the one email, nothing else.
pub fn parse_delete_user(args: &[String]) -> Result<String> {
    match args {
        [] => Err(Error::config(
            "delete-user needs the email address of the user to delete",
        )),
        [first, ..] if first.starts_with("--") => Err(Error::config(format!(
            "unknown delete-user argument `{first}`"
        ))),
        [email] => Ok(email.clone()),
        [first, second, ..] => Err(Error::config(format!(
            "delete-user takes one email address, not `{first}` and `{second}`"
        ))),
    }
}

/// What `feldspar role` was asked for (TODO F1.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleCommand {
    /// `role list [--json]`.
    List { json: bool },
    /// `role add NAME --level N [--description TEXT]`.
    Add {
        name: String,
        level: Role,
        description: String,
    },
    /// `role delete NAME` — a name or a number, as `--role` takes one.
    Delete { role: String },
}

/// Parse `role list|add|delete …`.
pub fn parse_role(args: &[String]) -> Result<RoleCommand> {
    let usage = "role needs a subcommand: list [--json], add NAME --level N \
                 [--description TEXT], or delete NAME";
    let (sub, rest) = args.split_first().ok_or_else(|| Error::config(usage))?;
    match sub.as_str() {
        "list" => match rest {
            [] => Ok(RoleCommand::List { json: false }),
            [flag] if flag == "--json" => Ok(RoleCommand::List { json: true }),
            [other, ..] => Err(Error::config(format!(
                "unknown role list argument `{other}`"
            ))),
        },
        "add" => {
            let mut name = None;
            let mut level = None;
            let mut description = String::new();
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                let mut value = |flag: &str| {
                    it.next()
                        .cloned()
                        .ok_or_else(|| Error::config(format!("{flag} needs a value")))
                };
                if arg == "--level" {
                    let raw = value("--level")?;
                    level = Some(raw.parse::<u8>().ok().and_then(Role::new).ok_or_else(|| {
                        Error::config(format!(
                            "--level is a role number from 1 to 100, not `{raw}`"
                        ))
                    })?);
                } else if arg == "--description" {
                    description = value("--description")?;
                } else if arg.starts_with("--") {
                    return Err(Error::config(format!("unknown role add argument `{arg}`")));
                } else if name.is_none() {
                    name = Some(arg.clone());
                } else {
                    return Err(Error::config(format!(
                        "role add takes one name; quote a name with spaces (`{arg}` is extra)"
                    )));
                }
            }
            Ok(RoleCommand::Add {
                name: name.ok_or_else(|| Error::config("role add needs the new role's name"))?,
                level: level.ok_or_else(|| {
                    Error::config("role add needs --level N: 1 is admin, 100 is public")
                })?,
                description,
            })
        }
        "delete" => match rest {
            [role] if !role.starts_with("--") => Ok(RoleCommand::Delete { role: role.clone() }),
            [] => Err(Error::config("role delete needs the role's name or number")),
            [other, ..] => Err(Error::config(format!(
                "unknown role delete argument `{other}`"
            ))),
        },
        other => Err(Error::config(format!(
            "unknown role subcommand `{other}`; {usage}"
        ))),
    }
}

/// The password `arg` names, asking for it if it names none.
///
/// No `--password` at all asks only at a terminal. Without one it is refused
/// rather than reading stdin, which a caller did not offer as a password: in a
/// shell loop over a file it is that file, and its next line would become the
/// password (and the loop would lose it).
pub fn password_from(arg: Option<&PasswordArg>) -> Result<String> {
    check_password_source(arg, std::io::stdin().is_terminal())?;
    let password = match arg {
        Some(PasswordArg::Value(value)) => value.clone(),
        Some(PasswordArg::Prompt) | None => prompt_password()?,
    };
    if password.is_empty() {
        return Err(Error::invalid("the password must not be blank"));
    }
    Ok(password)
}

/// Refuse no `--password` with no terminal to ask at; see [`password_from`].
fn check_password_source(arg: Option<&PasswordArg>, terminal: bool) -> Result<()> {
    if arg.is_none() && !terminal {
        return Err(Error::config(
            "no password given, and stdin is not a terminal to ask at: pass \
             --random-password to have one generated and printed, --password to read \
             one line of stdin, or --password=VALUE",
        ));
    }
    Ok(())
}

/// Ask for a password: twice and without echo at a terminal, or one line of
/// stdin when it is not one.
pub(crate) fn prompt_password() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return read_stdin_line();
    }
    let first = read_hidden("Password: ")?;
    let second = read_hidden("Confirm password: ")?;
    if first != second {
        return Err(Error::invalid("the two passwords did not match"));
    }
    Ok(first)
}

/// One line of stdin, which is not a terminal, without its line ending.
///
/// Read straight from the file descriptor a byte at a time, not through
/// `std::io::stdin()`, whose buffer would take up to 8 KiB past the line — input
/// that the shell, or the next command reading the same stdin, then never sees.
pub(crate) fn read_stdin_line() -> Result<String> {
    read_one_line(&mut RawStdin)
        .map_err(|e| Error::msg(format!("reading the password from stdin: {e}")))
}

/// Read up to and including the first newline, one byte at a time, so the
/// reader is left just after it. The line ending is dropped.
fn read_one_line(reader: &mut impl Read) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "the line is not UTF-8"))
}

/// File descriptor 0, unbuffered.
struct RawStdin;

impl Read for RawStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: `read` writes at most `buf.len()` bytes into `buf`, which is
        // valid for that many.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

/// Read one line from the terminal with echo turned off, restoring it after —
/// including when the read fails.
pub(crate) fn read_hidden(prompt: &str) -> Result<String> {
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
        assert!(!parsed.random_password);

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
    fn random_password_is_a_flag_of_its_own() {
        let parsed = parse_add_user(&args(&[
            "a@example.com",
            "--role",
            "1",
            "--random-password",
        ]))
        .unwrap();
        assert!(parsed.random_password);
        assert_eq!(parsed.password, None);
        // It is something to change, for modify-user.
        let parsed = parse_modify_user(&args(&["a@example.com", "--random-password"])).unwrap();
        assert!(parsed.random_password);
        for both in [
            &[
                "a@example.com",
                "--role",
                "1",
                "--password",
                "--random-password",
            ][..],
            &[
                "a@example.com",
                "--role",
                "1",
                "--random-password",
                "--password=pw",
            ][..],
        ] {
            let err = parse_add_user(&args(both)).unwrap_err().to_string();
            assert!(err.contains("not both"), "{err}");
        }
    }

    #[test]
    fn no_password_flag_and_no_terminal_is_refused_rather_than_reading_stdin() {
        let err = check_password_source(None, false).unwrap_err().to_string();
        assert!(err.contains("--random-password"), "{err}");
        // At a terminal it is asked for; asked for explicitly, stdin is read.
        assert!(check_password_source(None, true).is_ok());
        assert!(check_password_source(Some(&PasswordArg::Prompt), false).is_ok());
        assert!(check_password_source(Some(&PasswordArg::Value("pw".into())), false).is_ok());
    }

    #[test]
    fn one_line_is_read_and_nothing_after_it() {
        let mut input: &[u8] = b"secret\r\nb@example.com staff\nc@example.com user\n";
        assert_eq!(read_one_line(&mut input).unwrap(), "secret");
        // The rest is still there for whoever reads next.
        assert_eq!(input, b"b@example.com staff\nc@example.com user\n");
        assert_eq!(read_one_line(&mut input).unwrap(), "b@example.com staff");
        // A last line with no newline, then the end.
        let mut input: &[u8] = b"no-newline";
        assert_eq!(read_one_line(&mut input).unwrap(), "no-newline");
        assert_eq!(read_one_line(&mut input).unwrap(), "");
        assert!(read_one_line(&mut &b"\xff\n"[..]).is_err());
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

    #[test]
    fn modify_user_disables_or_enables_but_not_both() {
        let parsed = parse_modify_user(&args(&["a@example.com", "--disabled"])).unwrap();
        assert_eq!(parsed.disabled, Some(true));
        let parsed = parse_modify_user(&args(&["a@example.com", "--enabled"])).unwrap();
        assert_eq!(parsed.disabled, Some(false));
        let err = parse_modify_user(&args(&["a@example.com", "--enabled", "--disabled"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
        // add-user has no such flag.
        assert!(parse_add_user(&args(&["a@example.com", "--role", "1", "--disabled"])).is_err());
    }

    #[test]
    fn modify_user_can_reset_the_second_factor_alone() {
        let parsed = parse_modify_user(&args(&["a@example.com", "--reset-second-factor"])).unwrap();
        assert!(parsed.reset_second_factor);
        assert!(
            !parse_modify_user(&args(&["a@example.com", "--enabled"]))
                .unwrap()
                .reset_second_factor
        );
        assert!(
            parse_add_user(&args(&[
                "a@example.com",
                "--role",
                "1",
                "--reset-second-factor"
            ]))
            .is_err()
        );
    }

    #[test]
    fn delete_user_takes_exactly_one_email() {
        assert_eq!(
            parse_delete_user(&args(&["a@example.com"])).unwrap(),
            "a@example.com"
        );
        assert!(parse_delete_user(&args(&[])).is_err());
        assert!(parse_delete_user(&args(&["a@example.com", "b@example.com"])).is_err());
        assert!(parse_delete_user(&args(&["--force"])).is_err());
    }

    #[test]
    fn role_subcommands_parse() {
        assert_eq!(
            parse_role(&args(&["list"])).unwrap(),
            RoleCommand::List { json: false }
        );
        assert_eq!(
            parse_role(&args(&["list", "--json"])).unwrap(),
            RoleCommand::List { json: true }
        );
        assert_eq!(
            parse_role(&args(&[
                "add",
                "Editor",
                "--level",
                "40",
                "--description",
                "Edits"
            ]))
            .unwrap(),
            RoleCommand::Add {
                name: "Editor".into(),
                level: Role::of(40),
                description: "Edits".into()
            }
        );
        let err = parse_role(&args(&["add", "Editor"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--level"), "{err}");
        let err = parse_role(&args(&["add", "Editor", "--level", "101"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("1 to 100"), "{err}");
        assert_eq!(
            parse_role(&args(&["delete", "Editor"])).unwrap(),
            RoleCommand::Delete {
                role: "Editor".into()
            }
        );
        assert!(parse_role(&args(&[])).is_err());
        assert!(parse_role(&args(&["rename"])).is_err());
    }
}
