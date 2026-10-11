//! Single `Error` enum and `Result` alias for the Saltcorn v2 workspace (layer 0).
//!
//! This crate sits below everything else in the dependency graph (technical
//! design §2/§16) and encodes principle 5 — **no silent failures**. Fallible
//! code returns [`Result<T>`]; errors either carry context and propagate, or the
//! program crashes with a clear message. Library code must not `unwrap()` or
//! `expect()` on fallible paths — the workspace clippy config (`unwrap_used` /
//! `expect_used`) enforces this mechanically.
//!
//! # Adding context
//!
//! Use the [`Context`] extension trait to attach a human-readable message while
//! preserving the underlying error as a [`std::error::Error`] source chain:
//!
//! ```
//! use sc_error::{Context, Result};
//!
//! fn read_port(raw: &str) -> Result<u16> {
//!     let port = raw
//!         .parse::<u16>()
//!         .with_context(|| format!("invalid port {raw:?}"))?;
//!     Ok(port)
//! }
//!
//! assert!(read_port("nope").is_err());
//! ```
//!
//! The [`bail!`] and [`ensure!`] macros cover the common early-return cases.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fmt;
use std::panic::Location;

/// Workspace-wide result alias. Defaults to [`Error`] but keeps the error
/// parameter open so call sites can use a more specific error where useful.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The single error type shared across every Saltcorn crate.
///
/// An error is its classified [`Repr`] plus the source [`Location`] where it was
/// created — the `?`/constructor call site. That location is captured at compile
/// time via `#[track_caller]` (a `&'static Location` threaded through the call
/// chain), so it costs nothing at runtime and needs no `RUST_BACKTRACE`: it is
/// always available. It is surfaced by [`format_chain`]/[`Error::chain`] (so a
/// log line points at the failing code), but deliberately kept out of
/// [`Display`](fmt::Display) so a message shown to a client is unchanged.
///
/// It also carries a [`Backtrace`], the opt-in companion to the always-on
/// location: [`Backtrace::capture`] self-gates on `RUST_BACKTRACE`, so with the
/// variable unset it is `Disabled` (no stack walk, no output) and with it set it
/// records the full stack, which [`format_chain`] then appends. The location
/// answers "which line?" for free; the backtrace answers "how did we get here?"
/// when you ask for it.
#[derive(Debug)]
pub struct Error {
    /// What went wrong, and its message/source — see [`Repr`].
    repr: Repr,
    /// Where this error was constructed (compile-time; zero runtime cost).
    location: &'static Location<'static>,
    /// Stack at construction — only populated when `RUST_BACKTRACE` is set.
    backtrace: Backtrace,
}

/// The answer an [`Error::http`] carries: what a code body's `httpError` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    /// The status, as the body gave it. A transport checks the range (400–599)
    /// itself, because a body is not trusted to have.
    pub status: u16,
    /// The message, which is also the body's `{ "error": … }` when no body was
    /// given.
    pub message: String,
    /// The response body as JSON text, when the body gave one. Text rather than
    /// a value because this crate has no dependencies.
    pub body: Option<String>,
}

/// The classified representation of an [`Error`]: *where* something went wrong
/// (query, database, auth, …) rather than an enumeration of every failure.
///
/// Variants are intentionally coarse. Detail lives in the message string, and
/// the underlying cause — when there is one — is kept in [`Repr::Context`] so the
/// full chain is available via [`std::error::Error::source`]. Match on it via
/// [`Error::repr`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Repr {
    /// A requested resource (table, row, user, file, …) does not exist.
    NotFound(String),
    /// Input failed validation or a precondition was not met.
    Invalid(String),
    /// Misconfiguration (missing/invalid settings, bad connection args, …).
    Config(String),
    /// A database driver or connection error.
    Database(String),
    /// Building or rendering a query failed.
    Query(String),
    /// Authentication or authorization failure.
    Auth(String),
    /// A file store / filesystem error.
    File(String),
    /// Serialization or deserialization failure.
    Serde(String),
    /// A catch-all internal error that does not fit the categories above.
    Internal(String),
    /// A deliberate HTTP answer a code body chose — `throw httpError(403, …)`
    /// (TODO F1.4). Not a fault: the status and body are the body's, and a
    /// transport serves them as they are rather than mapping a kind to a code.
    Http(Box<HttpReply>),
    /// A lower-level error wrapped with a human-readable context message.
    ///
    /// Prefer constructing this via the [`Context`] trait rather than by hand.
    Context {
        /// The context message describing what was being attempted.
        context: String,
        /// The underlying error, preserved for the source chain.
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl Error {
    /// Wrap a [`Repr`] with the caller's source location. `#[track_caller]` means
    /// [`Location::caller`] resolves to the outermost non-tracked call site — the
    /// `Error::database(…)` / `bail!(…)` line — not this helper.
    #[track_caller]
    fn new(repr: Repr) -> Self {
        Self::at(repr, Location::caller())
    }

    /// Wrap a [`Repr`] with an explicitly captured location. Used where the
    /// construction happens inside a closure (e.g. `map_err`), which is not
    /// `#[track_caller]`-transparent, so the location must be grabbed in the
    /// enclosing tracked function and passed in.
    ///
    /// The single point where an [`Error`] is built, so it is also where the
    /// [`Backtrace`] is captured — a no-op unless `RUST_BACKTRACE` is set.
    fn at(repr: Repr, location: &'static Location<'static>) -> Self {
        Self {
            repr,
            location,
            backtrace: Backtrace::capture(),
        }
    }

    /// The source location where this error was created (the failing line).
    /// Always present — captured at compile time, no `RUST_BACKTRACE` needed.
    pub fn location(&self) -> &'static Location<'static> {
        self.location
    }

    /// The stack captured when this error was created. Only populated when
    /// `RUST_BACKTRACE` (or `RUST_LIB_BACKTRACE`) is set; otherwise its
    /// [`status`](Backtrace::status) is [`Disabled`](BacktraceStatus::Disabled).
    pub fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    /// The classified [`Repr`] of this error, for callers that need to branch on
    /// the specific variant (e.g. mapping to an HTTP status).
    pub fn repr(&self) -> &Repr {
        &self.repr
    }

    /// A [`Repr::Http`]: the deliberate answer `status` with `message`, and the
    /// JSON text of its body when there is one.
    #[track_caller]
    pub fn http(status: u16, message: impl Into<String>, body: Option<String>) -> Self {
        Self::new(Repr::Http(Box::new(HttpReply {
            status,
            message: message.into(),
            body,
        })))
    }

    /// The deliberate HTTP answer this error is, if it is one — looking through
    /// any context wrapped around it.
    pub fn http_reply(&self) -> Option<&HttpReply> {
        match &self.repr {
            Repr::Http(reply) => Some(reply),
            Repr::Context { source, .. } => source.downcast_ref::<Error>()?.http_reply(),
            _ => None,
        }
    }

    /// A [`Repr::NotFound`] from any displayable message.
    #[track_caller]
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(Repr::NotFound(msg.into()))
    }

    /// A [`Repr::Invalid`] from any displayable message.
    #[track_caller]
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(Repr::Invalid(msg.into()))
    }

    /// A [`Repr::Config`] from any displayable message.
    #[track_caller]
    pub fn config(msg: impl Into<String>) -> Self {
        Self::new(Repr::Config(msg.into()))
    }

    /// A [`Repr::Database`] from any displayable message.
    #[track_caller]
    pub fn database(msg: impl Into<String>) -> Self {
        Self::new(Repr::Database(msg.into()))
    }

    /// A [`Repr::Query`] from any displayable message.
    #[track_caller]
    pub fn query(msg: impl Into<String>) -> Self {
        Self::new(Repr::Query(msg.into()))
    }

    /// A [`Repr::Auth`] from any displayable message.
    #[track_caller]
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::new(Repr::Auth(msg.into()))
    }

    /// A [`Repr::File`] from any displayable message.
    #[track_caller]
    pub fn file(msg: impl Into<String>) -> Self {
        Self::new(Repr::File(msg.into()))
    }

    /// A [`Repr::Serde`] from any displayable message.
    #[track_caller]
    pub fn serde(msg: impl Into<String>) -> Self {
        Self::new(Repr::Serde(msg.into()))
    }

    /// A free-form [`Repr::Internal`]. Use for genuinely unexpected states.
    #[track_caller]
    pub fn msg(msg: impl Into<String>) -> Self {
        Self::new(Repr::Internal(msg.into()))
    }

    /// Classify this error into one of the two audiences of §16: an
    /// [`Application`](ErrorKind::Application) error the app builder must fix in
    /// their configuration, or a [`System`](ErrorKind::System) error that is
    /// likely a bug in Saltcorn (or its infrastructure).
    ///
    /// The split follows the design's variant families: `Invalid`/`Config`/`Query`
    /// (plus `NotFound`/`Auth`, which are request-level, never "report this bug")
    /// are Application; `Database`/`File`/`Serde`/`Internal` are System. A
    /// [`Context`](Error::Context) inherits the kind of the [`Error`] it wraps, and
    /// is System otherwise (an unclassified foreign error is treated as a fault to
    /// investigate, not something an admin can fix).
    pub fn kind(&self) -> ErrorKind {
        match &self.repr {
            Repr::NotFound(_)
            | Repr::Invalid(_)
            | Repr::Config(_)
            | Repr::Query(_)
            | Repr::Auth(_)
            | Repr::Http(_) => ErrorKind::Application,
            Repr::Database(_) | Repr::File(_) | Repr::Serde(_) | Repr::Internal(_) => {
                ErrorKind::System
            }
            Repr::Context { source, .. } => source
                .downcast_ref::<Error>()
                .map_or(ErrorKind::System, Error::kind),
        }
    }
}

/// The audience an [`Error`] is for (design §16): the two classes split "the app
/// builder must fix their configuration" from "this is likely a bug to report".
///
/// The MVP does not yet log errors to `_fd_errors`, but the classification lands
/// with `sc-error` from the start (as §16 requires) so it is never retrofitted —
/// and `sc-server` already uses it to map a failed build to a client-fixable
/// `422` rather than a `500`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The fault is in configuration authored by an app builder — a bad framework
    /// config, an invalid input, a query over a missing field. Nothing is wrong
    /// with Saltcorn; the admin fixes their configuration.
    Application,
    /// Something crashed and there is likely a bug in Saltcorn or the
    /// infrastructure it depends on — a driver failure, an invariant violation.
    System,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The location is intentionally not shown here — `Display` is what a
        // client may see; the failing line belongs in the log (`format_chain`).
        match &self.repr {
            Repr::NotFound(m) => write!(f, "not found: {m}"),
            Repr::Invalid(m) => write!(f, "invalid: {m}"),
            Repr::Config(m) => write!(f, "configuration error: {m}"),
            Repr::Database(m) => write!(f, "database error: {m}"),
            Repr::Query(m) => write!(f, "query error: {m}"),
            Repr::Auth(m) => write!(f, "auth error: {m}"),
            Repr::File(m) => write!(f, "file error: {m}"),
            Repr::Serde(m) => write!(f, "serialization error: {m}"),
            Repr::Internal(m) => write!(f, "internal error: {m}"),
            Repr::Http(reply) => write!(f, "{}", reply.message),
            // Only the top-level context is shown here; the wrapped cause is
            // reachable via `source()` so callers can print the whole chain.
            Repr::Context { context, .. } => write!(f, "{context}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.repr {
            Repr::Context { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Render an error together with its full [`source`](std::error::Error::source)
/// chain into a single human-readable string.
///
/// The top-level [`Display`](fmt::Display) of an [`Error`] shows only its own
/// message — a wrapped cause (a driver error behind a
/// [`Context`](Error::Context), the real SQL error behind `tokio_postgres`'s
/// terse `"db error"`) lives in `source()`. This walks that chain so callers
/// that log at a boundary emit everything they know, not just the outermost
/// label. Encodes principle 5 — no silent failures.
///
/// ```
/// use sc_error::{format_chain, Context, Result};
///
/// let inner: std::result::Result<(), std::num::ParseIntError> = "x".parse::<i32>().map(|_| ());
/// let err = inner.context("parsing the port").unwrap_err();
/// let chain = format_chain(&err);
/// assert!(chain.starts_with("parsing the port"));
/// assert!(chain.contains("caused by:"));
/// assert!(chain.contains("invalid digit"));
/// ```
/// The whole causal chain as one line, for a **person outside the process**:
/// each error's message joined with `: `, and no code locations.
///
/// The counterpart to [`format_chain`], and the distinction is who is reading.
/// `format_chain` is for the log — multi-line, with the failing line of source,
/// because whoever reads it can also read the code. This is for an admin looking
/// at a screen, where a file path is useful and `src/backend.rs:161` is noise.
///
/// It exists because [`Display`](std::fmt::Display) on a context error shows only
/// the *outermost* context: an error built as "connecting file store `docs`"
/// wrapping "no such file or directory" renders as just the former, which tells
/// an admin that something failed but not what to fix. Anything that surfaces an
/// error into the UI wants this rather than `to_string`.
///
/// ```
/// use sc_error::{format_causes, Context, Result};
///
/// let inner: std::result::Result<(), std::num::ParseIntError> = "x".parse::<i32>().map(|_| ());
/// let err = inner.context("parsing the port").unwrap_err();
/// let text = format_causes(&err);
/// assert!(text.starts_with("parsing the port: "));
/// assert!(text.contains("invalid digit"));
/// // One line, and no file:line noise.
/// assert!(!text.contains('\n'));
/// ```
pub fn format_causes(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

pub fn format_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    append_location(&mut out, err);
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str("\n  caused by: ");
        out.push_str(&cause.to_string());
        append_location(&mut out, cause);
        source = cause.source();
    }
    // The outermost error's backtrace, appended once — present only when
    // `RUST_BACKTRACE` is set, so by default this changes nothing.
    if let Some(e) = err.downcast_ref::<Error>()
        && e.backtrace.status() == BacktraceStatus::Captured
    {
        out.push_str(&format!(
            "\nbacktrace:\n{}",
            head_frames(&e.backtrace.to_string(), MAX_BACKTRACE_FRAMES)
        ));
    }
    out
}

/// How many stack frames of a captured backtrace reach the log.
///
/// A full backtrace here is ~40 frames, most of them the async runtime's own
/// machinery below the code anyone is looking for, and it arrives *per logged
/// error*. The frames that identify the failure are the innermost ones, so the
/// tail is pure volume — and volume is not free: it is what makes a real error
/// scroll off an admin's terminal. Ten is enough to cross a couple of `?`
/// boundaries and still fit on a screen.
const MAX_BACKTRACE_FRAMES: usize = 10;

/// The first `max` frames of a rendered backtrace, with a line saying how many
/// were dropped.
///
/// Works on the rendered text rather than the frames themselves because
/// [`Backtrace`] does not expose them on stable Rust. A frame starts at a line
/// like `   3: some::function`, and everything up to the next such line (the
/// `at file:line` continuation) belongs to it. Text that does not look like that
/// at all is returned unchanged: an unrecognised format should lose nothing.
fn head_frames(rendered: &str, max: usize) -> String {
    let is_frame_start = |line: &str| {
        let t = line.trim_start();
        t.split_once(':')
            .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };

    let mut kept = String::new();
    let mut frames = 0usize;
    let mut dropped = 0usize;
    for line in rendered.lines() {
        if is_frame_start(line) {
            frames += 1;
            if frames > max {
                dropped += 1;
                continue;
            }
        } else if frames > max {
            // A continuation line of a dropped frame.
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    if dropped == 0 {
        return rendered.to_owned();
    }
    kept.push_str(&format!(
        "  … {dropped} further frame{} not shown (of {}); \
         the innermost {max} are the ones that identify this error\n",
        if dropped == 1 { "" } else { "s" },
        frames
    ));
    kept
}

/// If `err` is a workspace [`Error`], append its creation location (`file:line`)
/// to `out`. A foreign error in the chain has no location and is left as-is.
fn append_location(out: &mut String, err: &(dyn std::error::Error + 'static)) {
    if let Some(e) = err.downcast_ref::<Error>() {
        out.push_str(&format!(" (at {})", e.location));
    }
}

impl Error {
    /// This error rendered with its full source chain — see [`format_chain`].
    pub fn chain(&self) -> String {
        format_chain(self)
    }

    /// The causal chain on one line, with no code locations — see
    /// [`format_causes`] for when to prefer this over
    /// [`to_string`](std::string::ToString::to_string).
    pub fn causes(&self) -> String {
        format_causes(self)
    }
}

impl From<std::io::Error> for Error {
    #[track_caller]
    fn from(e: std::io::Error) -> Self {
        Error::new(Repr::Context {
            context: "I/O error".to_string(),
            source: Box::new(e),
        })
    }
}

/// Attach a context message to a [`Result`] or [`Option`], converting it into a
/// workspace [`Result`].
///
/// For `Result`, the original error is preserved as the source of a
/// [`Repr::Context`]. For `Option`, a `None` becomes a [`Repr::Internal`]
/// carrying the message. Both methods are `#[track_caller]`, so the resulting
/// error's [`location`](Error::location) is the `.context(…)` call site.
pub trait Context<T> {
    /// Attach an eagerly-evaluated context message.
    fn context<C>(self, context: C) -> Result<T>
    where
        C: fmt::Display;

    /// Attach a context message computed only on the error path.
    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: fmt::Display,
        F: FnOnce() -> C;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[track_caller]
    fn context<C>(self, context: C) -> Result<T>
    where
        C: fmt::Display,
    {
        // `map_err`'s closure is not `#[track_caller]`-transparent, so capture
        // the call site here (where the attribute is in effect) and carry it in.
        let location = Location::caller();
        self.map_err(|e| {
            Error::at(
                Repr::Context {
                    context: context.to_string(),
                    source: Box::new(e),
                },
                location,
            )
        })
    }

    #[track_caller]
    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: fmt::Display,
        F: FnOnce() -> C,
    {
        let location = Location::caller();
        self.map_err(|e| {
            Error::at(
                Repr::Context {
                    context: f().to_string(),
                    source: Box::new(e),
                },
                location,
            )
        })
    }
}

impl<T> Context<T> for Option<T> {
    #[track_caller]
    fn context<C>(self, context: C) -> Result<T>
    where
        C: fmt::Display,
    {
        let location = Location::caller();
        self.ok_or_else(|| Error::at(Repr::Internal(context.to_string()), location))
    }

    #[track_caller]
    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: fmt::Display,
        F: FnOnce() -> C,
    {
        let location = Location::caller();
        self.ok_or_else(|| Error::at(Repr::Internal(f().to_string()), location))
    }
}

/// Return early with a [`Repr::Internal`] error built from a format string. The
/// error's [`location`](Error::location) is this `bail!` call site.
///
/// ```
/// use sc_error::{bail, Result};
///
/// fn check(ok: bool) -> Result<()> {
///     if !ok {
///         bail!("something went wrong: {}", 42);
///     }
///     Ok(())
/// }
/// assert!(check(false).is_err());
/// ```
#[macro_export]
macro_rules! bail {
    // Route through the `#[track_caller]` constructor rather than building the
    // variant here, so `location` points at the `bail!` invocation.
    ($($arg:tt)*) => {
        return ::core::result::Result::Err($crate::Error::msg(::std::format!($($arg)*)))
    };
}

/// Return early with a [`Repr::Internal`] error unless a condition holds.
///
/// ```
/// use sc_error::{ensure, Result};
///
/// fn check(n: i32) -> Result<()> {
///     ensure!(n > 0, "n must be positive, got {n}");
///     Ok(())
/// }
/// assert!(check(0).is_err());
/// assert!(check(1).is_ok());
/// ```
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !($cond) {
            $crate::bail!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn constructors_map_to_variants() {
        assert!(matches!(Error::not_found("t").repr(), Repr::NotFound(_)));
        assert!(matches!(Error::invalid("t").repr(), Repr::Invalid(_)));
        assert!(matches!(Error::config("t").repr(), Repr::Config(_)));
        assert!(matches!(Error::database("t").repr(), Repr::Database(_)));
        assert!(matches!(Error::query("t").repr(), Repr::Query(_)));
        assert!(matches!(Error::auth("t").repr(), Repr::Auth(_)));
        assert!(matches!(Error::file("t").repr(), Repr::File(_)));
        assert!(matches!(Error::serde("t").repr(), Repr::Serde(_)));
        assert!(matches!(Error::msg("t").repr(), Repr::Internal(_)));
    }

    #[test]
    fn an_http_reply_survives_a_context_and_is_an_application_error() {
        let err = Error::http(401, "sign in", None);
        assert_eq!(err.to_string(), "sign in");
        assert_eq!(err.kind(), ErrorKind::Application);
        let wrapped: Result<()> = Err(err).context("running `login`");
        let wrapped = wrapped.unwrap_err();
        let reply = wrapped.http_reply().expect("still an http reply");
        assert_eq!((reply.status, reply.message.as_str()), (401, "sign in"));
        assert!(Error::invalid("x").http_reply().is_none());
    }

    #[test]
    fn display_includes_category_and_message() {
        assert_eq!(Error::not_found("users").to_string(), "not found: users");
        assert_eq!(Error::query("bad sql").to_string(), "query error: bad sql");
    }

    #[test]
    fn context_preserves_source_chain() {
        let inner: std::result::Result<(), std::num::ParseIntError> =
            "x".parse::<i32>().map(|_| ());
        let err = inner.context("parsing the answer").unwrap_err();
        // Top-level Display is just the context message.
        assert_eq!(err.to_string(), "parsing the answer");
        // The original error is reachable as the source.
        let src = err.source().expect("should have a source");
        assert!(src.to_string().contains("invalid digit"));
    }

    #[test]
    fn with_context_is_only_evaluated_on_error() {
        let ok: std::result::Result<i32, std::num::ParseIntError> = "5".parse();
        let mut called = false;
        let out = ok.with_context(|| {
            called = true;
            "unused"
        });
        assert_eq!(out.unwrap(), 5);
        assert!(!called, "context closure must not run on the Ok path");
    }

    #[test]
    fn option_context_produces_internal_error() {
        let none: Option<i32> = None;
        let err = none.context("missing config value").unwrap_err();
        assert!(matches!(err.repr(), Repr::Internal(_)));
        assert_eq!(err.to_string(), "internal error: missing config value");
    }

    #[test]
    fn io_error_conversion_keeps_source() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no file");
        let err: Error = io.into();
        assert!(matches!(err.repr(), Repr::Context { .. }));
        assert!(err.source().is_some());
    }

    #[test]
    fn bail_returns_internal_error() {
        fn f() -> Result<()> {
            bail!("boom {}", 1);
        }
        let err = f().unwrap_err();
        assert_eq!(err.to_string(), "internal error: boom 1");
    }

    #[test]
    fn ensure_checks_condition() {
        fn f(n: i32) -> Result<()> {
            ensure!(n > 0, "must be positive: {n}");
            Ok(())
        }
        assert!(f(1).is_ok());
        assert_eq!(
            f(-1).unwrap_err().to_string(),
            "internal error: must be positive: -1"
        );
    }

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn format_chain_walks_every_source() {
        // Build a two-level chain: a context wrapping a parse error.
        let inner: std::result::Result<(), std::num::ParseIntError> =
            "x".parse::<i32>().map(|_| ());
        let err = inner.context("parsing the port").unwrap_err();

        let chain = format_chain(&err);
        // The top line is the outermost context...
        assert!(chain.starts_with("parsing the port"), "got: {chain}");
        // ...followed by the underlying cause on its own indented line.
        assert!(chain.contains("\n  caused by: "), "got: {chain}");
        assert!(chain.contains("invalid digit"), "got: {chain}");
        // `chain()` on the concrete error is the same rendering.
        assert_eq!(err.chain(), chain);
    }

    #[test]
    fn format_chain_of_a_leaf_error_is_just_its_message() {
        // No source → single line, no "caused by". The creation location is
        // appended (only in the chain, never in `Display`).
        let err = Error::database("connection refused");
        let chain = err.chain();
        assert!(
            chain.starts_with("database error: connection refused (at "),
            "got: {chain}"
        );
        assert!(!chain.contains("caused by"));
        // The message a client would see carries no location.
        assert_eq!(err.to_string(), "database error: connection refused");
    }

    #[test]
    fn error_records_its_creation_location() {
        let expected_line = line!() + 1;
        let err = Error::database("boom");
        // The location points at the constructor call site above, not into
        // sc-error's own source.
        assert_eq!(err.location().line(), expected_line);
        assert!(err.location().file().ends_with("lib.rs"));
        // It surfaces in the chain but not in Display.
        assert!(err.chain().contains(&format!(":{expected_line}")));
        assert!(!err.to_string().contains("lib.rs"));
    }

    #[test]
    fn context_location_is_the_call_site_not_the_library() {
        let inner: std::result::Result<(), std::io::Error> = Err(std::io::Error::other("disk"));
        let expected_line = line!() + 1;
        let err = inner.context("while loading").unwrap_err();
        // `#[track_caller]` threads the caller through `map_err`, so the location
        // is this `.context(…)` line — not somewhere inside sc-error.
        assert_eq!(err.location().line(), expected_line);
        assert!(err.location().file().ends_with("lib.rs"));
    }

    #[test]
    fn bail_location_is_the_macro_call_site() {
        fn f() -> Result<()> {
            bail!("boom");
        }
        let expected_line = line!() - 2; // the `bail!("boom");` line above
        let err = f().unwrap_err();
        // The location is captured at the `bail!` invocation, in this test file
        // — not inside the macro definition.
        assert!(err.location().file().ends_with("lib.rs"));
        assert_eq!(err.location().line(), expected_line);
    }

    #[test]
    fn format_chain_includes_nested_causes() {
        // A three-deep chain: two contexts over an io error. Every level shows.
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no file");
        let mid: Result<()> = Err::<(), _>(io).context("opening config");
        let err = mid.context("starting up").unwrap_err();

        let chain = format_chain(&err);
        assert!(chain.starts_with("starting up"), "got: {chain}");
        assert!(chain.contains("caused by: opening config"), "got: {chain}");
        assert!(chain.contains("no file"), "got: {chain}");
    }

    #[test]
    fn backtrace_rendering_matches_capture_status() {
        // Robust to the ambient `RUST_BACKTRACE`: the chain shows a backtrace
        // exactly when one was captured, and never otherwise.
        let err = Error::database("boom");
        let rendered = err.chain().contains("backtrace:");
        let captured = err.backtrace().status() == BacktraceStatus::Captured;
        assert_eq!(rendered, captured);
    }

    #[test]
    fn backtrace_captured_when_rust_backtrace_is_set() {
        // `Backtrace::capture()` reads `RUST_BACKTRACE` once and caches it for
        // the process, so the enabled path can't be exercised in-process
        // alongside the other tests. Re-exec this one test in a child with the
        // variable set; the child branch does the real assertions.
        const CHILD: &str = "SC_ERROR_BACKTRACE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let err = Error::database("boom");
            assert_eq!(err.backtrace().status(), BacktraceStatus::Captured);
            let chain = err.chain();
            assert!(chain.contains("backtrace:"), "no backtrace in: {chain}");
            // Capped: a real backtrace here is dozens of frames of runtime
            // machinery, arriving once per logged error. Whatever the ambient
            // depth, at most `MAX_BACKTRACE_FRAMES` reach the log.
            let frames = chain
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    t.split_once(':').is_some_and(|(n, _)| {
                        !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                    })
                })
                .count();
            assert!(
                frames <= MAX_BACKTRACE_FRAMES,
                "{frames} frames survived the cap:\n{chain}"
            );
            return;
        }

        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args(["backtrace_captured_when_rust_backtrace_is_set", "--exact"])
            .env("RUST_BACKTRACE", "1")
            .env(CHILD, "1")
            .output()
            .expect("re-run test binary");
        assert!(
            output.status.success(),
            "child failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A stand-in for what `Backtrace`'s `Display` produces: numbered frames,
    /// each with an indented `at file:line` continuation.
    fn rendered_backtrace(frames: usize) -> String {
        (0..frames)
            .map(|i| format!("{i:>4}: some::function::h{i}\n             at ./src/lib.rs:{i}:9\n"))
            .collect()
    }

    #[test]
    fn a_long_backtrace_is_cut_to_its_innermost_frames() {
        let cut = head_frames(&rendered_backtrace(40), 10);
        // The innermost frames survive whole, `at` lines and all...
        assert!(cut.contains("   0: some::function::h0"));
        assert!(cut.contains("at ./src/lib.rs:0:9"));
        assert!(cut.contains("   9: some::function::h9"));
        // ...and everything past the cap goes, continuation lines included.
        assert!(!cut.contains("some::function::h10"), "{cut}");
        assert!(!cut.contains("at ./src/lib.rs:10:9"), "{cut}");
        // The reader is told what was dropped rather than left to wonder whether
        // the trace simply ended there.
        assert!(cut.contains("30 further frames not shown (of 40)"), "{cut}");
    }

    #[test]
    fn a_short_backtrace_is_left_exactly_as_it_was() {
        // Under the cap nothing is rewritten — not even the trailing newline —
        // so the common case is byte-identical to what `Backtrace` rendered.
        let short = rendered_backtrace(3);
        assert_eq!(head_frames(&short, 10), short);
        // Exactly at the cap is still untouched: the boundary drops nothing.
        let exact = rendered_backtrace(10);
        assert_eq!(head_frames(&exact, 10), exact);
    }

    #[test]
    fn text_that_is_not_a_backtrace_survives_unchanged() {
        // `Backtrace`'s rendering is not a stable contract, and the ones that
        // say only "disabled" or "unsupported" have no frames at all. Anything
        // unrecognised must pass through rather than be truncated to nothing.
        for text in [
            "disabled backtrace",
            "unsupported backtrace",
            "note: some other rendering entirely\n",
        ] {
            assert_eq!(head_frames(text, 10), text);
        }
    }

    #[test]
    fn kind_splits_application_from_system() {
        // Config authored by an app builder — a bad framework config, invalid
        // input — is the admin's to fix.
        assert_eq!(Error::config("bad build").kind(), ErrorKind::Application);
        assert_eq!(Error::invalid("nope").kind(), ErrorKind::Application);
        assert_eq!(Error::query("bad sql").kind(), ErrorKind::Application);
        assert_eq!(Error::not_found("x").kind(), ErrorKind::Application);
        assert_eq!(Error::auth("x").kind(), ErrorKind::Application);

        // A crash or infrastructure failure is likely a bug to report.
        assert_eq!(Error::database("down").kind(), ErrorKind::System);
        assert_eq!(Error::file("io").kind(), ErrorKind::System);
        assert_eq!(Error::serde("bad").kind(), ErrorKind::System);
        assert_eq!(Error::msg("invariant").kind(), ErrorKind::System);
    }

    #[test]
    fn context_inherits_the_wrapped_errors_kind() {
        // A context wrapping an Application error stays Application...
        let wrapped: Result<()> = Err(Error::config("bad build"));
        let err = wrapped.context("while building the app").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Application);

        // ...while a wrapped foreign error defaults to System.
        let io = std::io::Error::other("disk");
        assert_eq!(Error::from(io).kind(), ErrorKind::System);
    }
}
