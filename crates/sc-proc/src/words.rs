//! The pieces of a command line: [`Program`], [`Operand`], [`EnvName`] and
//! [`Timeout`].

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sc_error::{Error, Result};

/// The program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program(OsString);

impl Program {
    /// A program the source names, looked up on the child's `PATH` (`"git"`).
    pub fn named(name: &'static str) -> Program {
        Program(OsString::from(name))
    }

    /// An executable at `path`: one this process located (`which`, a
    /// project's `node_modules/.bin`), or the operator configured.
    pub fn path(path: impl Into<PathBuf>) -> Program {
        Program(path.into().into_os_string())
    }

    /// The program an administrator or operator configured by name or path
    /// (a build command's first word, a target's executable).
    pub fn configured(command: impl Into<OsString>) -> Program {
        Program(command.into())
    }

    /// The program as given.
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

impl From<&'static str> for Program {
    fn from(name: &'static str) -> Program {
        Program::named(name)
    }
}

impl From<&Path> for Program {
    fn from(path: &Path) -> Program {
        Program::path(path)
    }
}

impl From<PathBuf> for Program {
    fn from(path: PathBuf) -> Program {
        Program::path(path)
    }
}

impl From<&PathBuf> for Program {
    fn from(path: &PathBuf) -> Program {
        Program::path(path)
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string_lossy())
    }
}

/// An argument a program reads as a name — a file, a package, a branch, a
/// URL — never as an option: it does not start with `-`, and it has no NUL.
///
/// [`HostCommand::operand`](crate::HostCommand::operand) emits `--` before the
/// first one as well, so a program that honours it would not read even a `-`
/// as an option; [`positional`](crate::HostCommand::positional) is for one
/// where `--` means something else (`npm run`, `make`, `git checkout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operand(OsString);

impl Operand {
    /// `text` as an operand, refused when it starts with `-` or holds a NUL.
    pub fn new(text: impl Into<OsString>) -> Result<Operand> {
        let text = text.into();
        let bytes = text.as_encoded_bytes();
        if bytes.first() == Some(&b'-') {
            return Err(Error::invalid(format!(
                "{:?} starts with `-`, so a program would read it as an option",
                text.to_string_lossy()
            )));
        }
        if bytes.contains(&0) {
            return Err(Error::invalid(format!(
                "{:?} contains a NUL byte",
                text.to_string_lossy()
            )));
        }
        if bytes.is_empty() {
            return Err(Error::invalid("an empty argument names nothing"));
        }
        Ok(Operand(text))
    }

    /// A path. A relative one that starts with `-` is given a `./`, so it
    /// still names the same file and cannot be an option.
    pub fn path(path: impl AsRef<Path>) -> Operand {
        let path = path.as_ref();
        let os = path.as_os_str();
        if os.is_empty() {
            return Operand(OsString::from("."));
        }
        if os.as_encoded_bytes().first() == Some(&b'-') {
            return Operand(Path::new(".").join(path).into_os_string());
        }
        Operand(os.to_owned())
    }

    /// A count or an id.
    pub fn number(n: u64) -> Operand {
        Operand(OsString::from(n.to_string()))
    }

    /// The argument.
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

/// The name of an environment variable a child is given, refused when it is
/// one of the variables that change how a program loads or what it runs
/// rather than what it reads:
///
/// - the dynamic loader's (`LD_*`, `DYLD_*`) and glibc's `GCONV_PATH`;
/// - the shells' (`BASH_ENV`, `ENV`, `BASH_FUNC_*`, `SHELLOPTS`, `BASHOPTS`,
///   `PS4`, `PROMPT_COMMAND`, `IFS`);
/// - the interpreters' (`NODE_OPTIONS`, `PYTHON*`, `PERL5OPT`, `PERL5LIB`,
///   `PERLLIB`, `RUBYOPT`, `RUBYLIB`, `JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`,
///   `JDK_JAVA_OPTIONS`);
/// - git's (`GIT_*`), which only [`Git`](crate::Git) sets — except the two
///   that only stop it asking (`GIT_PAGER`, `GIT_TERMINAL_PROMPT`).
///
/// A name is letters, digits and `_`, not starting with a digit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvName(Cow<'static, str>);

const REFUSED_EXACT: [&str; 18] = [
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "IFS",
    "GCONV_PATH",
    "NODE_OPTIONS",
    "PERL5OPT",
    "PERL5LIB",
    "PERLLIB",
    "PERL5DB",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
];

const REFUSED_PREFIXES: [&str; 5] = ["LD_", "DYLD_", "BASH_FUNC_", "PYTHON", "GIT_"];

/// The `GIT_*` variables that only keep git from asking anything.
const GIT_QUIET: [&str; 2] = ["GIT_PAGER", "GIT_TERMINAL_PROMPT"];

const fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn starts_with(a: &[u8], prefix: &[u8]) -> bool {
    if a.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if a[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Why `name` may not be set for a child, or `None` when it may.
const fn refusal(name: &str) -> Option<&'static str> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Some("it is empty");
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let ok = b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit());
        if !ok {
            return Some("a variable's name is letters, digits and `_`");
        }
        i += 1;
    }
    // Compared in upper case: Windows' environment is case-insensitive.
    let mut upper = [0u8; 64];
    if bytes.len() > upper.len() {
        return Some("it is too long");
    }
    let mut i = 0;
    while i < bytes.len() {
        upper[i] = bytes[i].to_ascii_uppercase();
        i += 1;
    }
    let (upper, _) = upper.split_at(bytes.len());
    let mut i = 0;
    while i < GIT_QUIET.len() {
        if eq(upper, GIT_QUIET[i].as_bytes()) {
            return None;
        }
        i += 1;
    }
    let mut i = 0;
    while i < REFUSED_EXACT.len() {
        if eq(upper, REFUSED_EXACT[i].as_bytes()) {
            return Some("it changes what a program runs, not what it reads");
        }
        i += 1;
    }
    let mut i = 0;
    while i < REFUSED_PREFIXES.len() {
        if starts_with(upper, REFUSED_PREFIXES[i].as_bytes()) {
            return Some("it changes what a program runs, not what it reads");
        }
        i += 1;
    }
    None
}

impl EnvName {
    /// `name`, refused as the type docs say.
    pub fn new(name: impl Into<String>) -> Result<EnvName> {
        let name = name.into();
        match refusal(&name) {
            Some(why) => Err(Error::invalid(format!(
                "`{name}` may not be set for a program the server runs: {why}"
            ))),
            None => Ok(EnvName(Cow::Owned(name))),
        }
    }

    /// A name the source spells. Checked at compile time through
    /// [`env_name!`](crate::env_name), or in a `const` item.
    ///
    /// # Panics
    ///
    /// When `name` is refused: that is a bug in the literal, which a `const`
    /// context turns into a compile error.
    pub const fn fixed(name: &'static str) -> EnvName {
        if refusal(name).is_some() {
            panic!("this environment variable may not be set for a child process");
        }
        EnvName(Cow::Borrowed(name))
    }

    /// `GIT_*` for [`Git`](crate::Git), which is what may set them.
    pub(crate) const fn git(name: &'static str) -> EnvName {
        EnvName(Cow::Borrowed(name))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How long a process may run before its process group is killed. No
/// `Default`: every spawn says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(Option<Duration>, &'static str);

impl Timeout {
    /// At most `duration`.
    pub const fn after(duration: Duration) -> Timeout {
        Timeout(Some(duration), "")
    }

    /// At most `n` seconds.
    pub const fn secs(n: u64) -> Timeout {
        Timeout::after(Duration::from_secs(n))
    }

    /// At most `n` minutes.
    pub const fn minutes(n: u64) -> Timeout {
        Timeout::after(Duration::from_secs(n.saturating_mul(60)))
    }

    /// No limit, for `reason`: a process that lives as long as something
    /// else does (a terminal's socket, an agent run) and is killed with it.
    pub const fn unbounded(reason: &'static str) -> Timeout {
        Timeout(None, reason)
    }

    /// The limit, or `None` for an unbounded one.
    pub fn duration(&self) -> Option<Duration> {
        self.0
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => write!(f, "no time limit ({})", self.1),
            Some(d) if d.as_secs() >= 120 && d.as_secs() % 60 == 0 => {
                write!(f, "{} minutes", d.as_secs() / 60)
            }
            Some(d) if d.as_millis() % 1000 == 0 => write!(f, "{} seconds", d.as_secs()),
            Some(d) => write!(f, "{} ms", d.as_millis()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operand_is_never_an_option() {
        assert!(Operand::new("--upload-pack=sh -c id").is_err());
        assert!(Operand::new("-rf").is_err());
        assert!(Operand::new("").is_err());
        assert!(Operand::new("a\0b").is_err());
        assert!(Operand::new("lodash@4").is_ok());
        assert!(Operand::new("https://example.com/r.git").is_ok());
        assert_eq!(Operand::path("-x").as_os_str(), "./-x");
        assert_eq!(Operand::path("/abs/-x").as_os_str(), "/abs/-x");
        assert_eq!(Operand::path("").as_os_str(), ".");
    }

    #[test]
    fn a_variable_that_changes_what_runs_is_refused() {
        for name in [
            "LD_PRELOAD",
            "ld_library_path",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "BASH_ENV",
            "ENV",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "GIT_DIR",
            "GIT_SSH_COMMAND",
            "GIT_CONFIG_COUNT",
            "BASH_FUNC_x%%",
            "JAVA_TOOL_OPTIONS",
            "A=B",
            "",
            "1X",
        ] {
            assert!(EnvName::new(name).is_err(), "{name}");
        }
        for name in [
            "PATH",
            "TERM",
            "ANDROID_HOME",
            "JAVA_HOME",
            "GIT_PAGER",
            "GIT_TERMINAL_PROMPT",
            "npm_lifecycle_event",
            "CI",
        ] {
            assert!(EnvName::new(name).is_ok(), "{name}");
        }
        const TERM: EnvName = EnvName::fixed("TERM");
        assert_eq!(TERM.as_str(), "TERM");
        assert_eq!(crate::env_name!("CI").as_str(), "CI");
    }

    #[test]
    fn a_timeout_says_how_long() {
        assert_eq!(Timeout::secs(30).to_string(), "30 seconds");
        assert_eq!(Timeout::minutes(20).to_string(), "20 minutes");
        assert_eq!(
            Timeout::after(Duration::from_millis(250)).to_string(),
            "250 ms"
        );
        assert!(Timeout::unbounded("x").duration().is_none());
    }
}
