//! The password of a protected backup, for `feldspar backup` and
//! `feldspar restore`.
//!
//! Three places it can come from, in this order:
//!
//! - `--password VALUE` (or `--password=VALUE`) on the command line — the
//!   simplest, and visible in the shell's history and in `ps`, so for a
//!   throwaway machine;
//! - `--password` with no value: asked for at the terminal without echo —
//!   twice for a backup, once for a restore — or read as one line of stdin
//!   when stdin is not a terminal, as `add-user` reads one;
//! - [`PASSWORD_ENV`], for a cron job or a container, where neither of the
//!   others fits.
//!
//! A restore of a protected backup given none of them asks at the terminal
//! anyway, since the file cannot be read without it; with no terminal to ask
//! at it is refused, naming the flag and the variable. A backup given none is
//! written unprotected, as it always was.

use sc_error::{Error, Result};
use std::io::IsTerminal;

use crate::users::PasswordArg;

/// The environment variable a backup's password is read from when no
/// `--password` is given.
pub const PASSWORD_ENV: &str = "FELDSPAR_BACKUP_PASSWORD";

/// Take `--password` out of `args`, returning it and the arguments left. The
/// value is optional, so the next argument is it only when it is not itself a
/// flag; a password that starts with `--` is written `--password=--…`.
pub fn extract(args: Vec<String>) -> Result<(Option<PasswordArg>, Vec<String>)> {
    let mut password = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        let found = if let Some(value) = arg.strip_prefix("--password=") {
            PasswordArg::Value(value.to_owned())
        } else if arg == "--password" {
            match it.next_if(|next| !next.starts_with("--")) {
                Some(value) => PasswordArg::Value(value),
                None => PasswordArg::Prompt,
            }
        } else {
            rest.push(arg);
            continue;
        };
        if password.replace(found).is_some() {
            return Err(Error::config("--password is given twice"));
        }
    }
    Ok((password, rest))
}

/// The password to write a backup with: the flag's, else the environment's,
/// else none. `env` is the value of [`PASSWORD_ENV`]; an empty one is unset.
pub fn for_backup(flag: Option<PasswordArg>, env: Option<String>) -> Result<Option<String>> {
    for_backup_with(flag, env, crate::users::prompt_password)
}

fn for_backup_with(
    flag: Option<PasswordArg>,
    env: Option<String>,
    prompt: impl FnOnce() -> Result<String>,
) -> Result<Option<String>> {
    let password = match flag {
        Some(PasswordArg::Value(value)) => value,
        Some(PasswordArg::Prompt) => prompt()?,
        None => match env.filter(|v| !v.is_empty()) {
            Some(value) => value,
            None => return Ok(None),
        },
    };
    if password.is_empty() {
        return Err(Error::invalid("the backup password must not be blank"));
    }
    Ok(Some(password))
}

/// The password to read a backup with. None for a backup that is not
/// `encrypted`, whatever was given — a password is not needed to read it.
/// For one that is: the flag's, else the environment's, else asked for at the
/// terminal.
pub fn for_restore(
    flag: Option<PasswordArg>,
    env: Option<String>,
    encrypted: bool,
) -> Result<Option<String>> {
    for_restore_with(
        flag,
        env,
        encrypted,
        std::io::stdin().is_terminal(),
        ask_once,
    )
}

fn for_restore_with(
    flag: Option<PasswordArg>,
    env: Option<String>,
    encrypted: bool,
    terminal: bool,
    prompt: impl FnOnce() -> Result<String>,
) -> Result<Option<String>> {
    if !encrypted {
        return Ok(None);
    }
    let password = match (flag, env.filter(|v| !v.is_empty())) {
        (Some(PasswordArg::Value(value)), _) => value,
        (Some(PasswordArg::Prompt), _) => prompt()?,
        (None, Some(value)) => value,
        (None, None) if terminal => prompt()?,
        (None, None) => {
            return Err(Error::invalid(format!(
                "this backup is protected by a password: pass --password, or set \
                 {PASSWORD_ENV}"
            )));
        }
    };
    if password.is_empty() {
        return Err(Error::invalid("the backup password must not be blank"));
    }
    Ok(Some(password))
}

/// A restore's password: once, since a mistyped one is refused by the file
/// rather than written into anything. One line of stdin when stdin is not a
/// terminal.
fn ask_once() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .map_err(|e| Error::msg(format!("reading the password from stdin: {e}")))?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_owned());
    }
    crate::users::read_hidden("Backup password: ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn never() -> Result<String> {
        panic!("asked for a password it should not have")
    }

    #[test]
    fn the_flag_is_taken_out_of_the_arguments() {
        let (pw, rest) = extract(args(&["--db", "x", "--password", "s3cret"])).unwrap();
        assert_eq!(pw, Some(PasswordArg::Value("s3cret".into())));
        assert_eq!(rest, args(&["--db", "x"]));

        let (pw, rest) = extract(args(&["--password", "--db", "x"])).unwrap();
        assert_eq!(pw, Some(PasswordArg::Prompt));
        assert_eq!(rest, args(&["--db", "x"]));

        let (pw, _) = extract(args(&["--password=--odd"])).unwrap();
        assert_eq!(pw, Some(PasswordArg::Value("--odd".into())));

        let (pw, rest) = extract(args(&["--db", "x"])).unwrap();
        assert_eq!(pw, None);
        assert_eq!(rest.len(), 2);

        assert!(extract(args(&["--password=a", "--password=b"])).is_err());
    }

    #[test]
    fn a_backup_takes_the_flag_then_the_environment_then_none() {
        let flag = Some(PasswordArg::Value("flag".into()));
        assert_eq!(
            for_backup_with(flag, Some("env".into()), never).unwrap(),
            Some("flag".into())
        );
        assert_eq!(
            for_backup_with(None, Some("env".into()), never).unwrap(),
            Some("env".into())
        );
        assert_eq!(
            for_backup_with(None, Some(String::new()), never).unwrap(),
            None
        );
        assert_eq!(for_backup_with(None, None, never).unwrap(), None);
        assert_eq!(
            for_backup_with(Some(PasswordArg::Prompt), None, || Ok("typed".into())).unwrap(),
            Some("typed".into())
        );
        assert!(for_backup_with(Some(PasswordArg::Value(String::new())), None, never).is_err());
    }

    #[test]
    fn a_restore_asks_only_for_a_protected_backup_and_only_when_it_can() {
        // Not encrypted: nothing is asked and nothing is used.
        let flag = || Some(PasswordArg::Value("flag".into()));
        assert_eq!(
            for_restore_with(flag(), None, false, true, never).unwrap(),
            None
        );
        assert_eq!(
            for_restore_with(None, None, false, true, never).unwrap(),
            None
        );
        // Encrypted: flag, then environment, then the terminal.
        assert_eq!(
            for_restore_with(flag(), Some("env".into()), true, true, never).unwrap(),
            Some("flag".into())
        );
        assert_eq!(
            for_restore_with(None, Some("env".into()), true, true, never).unwrap(),
            Some("env".into())
        );
        assert_eq!(
            for_restore_with(None, None, true, true, || Ok("typed".into())).unwrap(),
            Some("typed".into())
        );
        // No terminal to ask at: refused, naming both ways to give it.
        let err = for_restore_with(None, None, true, false, never)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--password") && err.contains(PASSWORD_ENV),
            "{err}"
        );
    }
}
