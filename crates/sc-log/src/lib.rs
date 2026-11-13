//! How much this process says, and whether it prints the SQL it issues
//! (layer 0).
//!
//! Two switches, both of them **process-wide atomics** rather than values
//! carried down through every call: the verbosity, and whether every statement
//! sent to the database is echoed. They are set from the stored Development
//! settings (`sc_config::development`) at boot and again whenever an admin
//! saves them, so turning SQL logging on is a checkbox rather than a restart.
//!
//! **Why globals.** Logging is the one concern that cuts across the whole
//! dependency graph: the thing that wants to print a statement is the Postgres
//! driver (layer 2), the thing that knows what the admin ticked is the config
//! store (layer 5), and the layers in between have no business carrying a
//! logging handle from one to the other. An `AtomicBool` read on the statement
//! path costs a relaxed load, which is less than the `format!` it guards.
//!
//! **Where it comes out.** SQL goes to **stdout**, because it is the *output* an
//! admin turned the switch on to read — pipe it to a file, grep it. Level
//! messages go to **stderr**, so a redirected stdout is the SQL and nothing
//! else.
//!
//! **A line is the message and nothing else** — no program name, no level tag.
//! Whatever reads these logs (a terminal, `journald`, a container runtime)
//! already knows which process wrote them and which stream it used, and a
//! prefix on every line is width spent saying so twice. The consequence, taken
//! knowingly: an `info` line and a `warning` line look alike, so a message that
//! wants to be recognisable as a warning says so in its own words. It also
//! leaves the SQL echo as *valid SQL* — the binds ride in a `--` comment — so a
//! redirected stdout is a script, not a log that has to be unwrapped first.
//!
//! ```
//! sc_log::set_verbosity(sc_log::Verbosity::Info);
//! assert!(sc_log::enabled(sc_log::Verbosity::Info));
//! assert!(!sc_log::enabled(sc_log::Verbosity::Trace));
//! sc_log::log_info!("{} {} → {}", "GET", "/health", 200);
//! ```

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use sc_error::{Error, Result};

/// How much this process says: the standard Unix ladder, least talkative first.
///
/// The order is the whole meaning — a message is printed when its level is at
/// or below the configured one — so the discriminants are deliberate and
/// [`Ord`] is derived from them rather than written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Verbosity {
    /// Only what failed.
    Error = 0,
    /// What failed, and what is about to.
    Warning = 1,
    /// The above, plus **every server request** — one line per request, with
    /// its method, path, status and duration.
    Info = 2,
    /// The above, plus each request as it *arrives*, so a request that never
    /// finishes is visible while it is hanging.
    Verbose = 3,
    /// Everything: the whole of what an LLM was sent and answered, and every
    /// tool call's arguments and result.
    ///
    /// A server request logs the same at `trace` as at `verbose` — the
    /// transcripts are the reason to be here, and a header dump per request is
    /// what buries them.
    Trace = 4,
}

/// The verbosity a process runs at until something sets one.
///
/// Warning rather than Info: Info is defined to print every request, and a
/// server that logs every request by default is a server whose logs nobody
/// reads. Turning it up is a checkbox in Settings → Development.
pub const DEFAULT_VERBOSITY: Verbosity = Verbosity::Warning;

impl Verbosity {
    /// Every level, least talkative first — the option list the settings
    /// dropdown is built from, so the form and the parser cannot disagree
    /// about what a level is called.
    pub const ALL: [Verbosity; 5] = [
        Verbosity::Error,
        Verbosity::Warning,
        Verbosity::Info,
        Verbosity::Verbose,
        Verbosity::Trace,
    ];

    /// The stored spelling: lower-case, as every other option-valued setting in
    /// `_fd_config` is stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Verbosity::Error => "error",
            Verbosity::Warning => "warning",
            Verbosity::Info => "info",
            Verbosity::Verbose => "verbose",
            Verbosity::Trace => "trace",
        }
    }

    /// Parse a stored `log_verbosity`, case-insensitively.
    ///
    /// Case-insensitive because these five words are the ones an operator
    /// already knows from every other daemon they run, and being refused for
    /// typing `Info` would be a rule with nothing behind it. An unrecognised
    /// value *is* an error naming what was expected — the declaration's
    /// `options` mean the form cannot produce one, so it can only come from a
    /// hand-edited row.
    pub fn parse(raw: &str) -> Result<Verbosity> {
        let lowered = raw.trim().to_ascii_lowercase();
        Verbosity::ALL
            .into_iter()
            .find(|level| level.as_str() == lowered)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "unknown log verbosity `{raw}`; expected one of {}",
                    Verbosity::ALL.map(Verbosity::as_str).join(", ")
                ))
            })
    }

    /// The level a stored `u8` means, falling back to [`DEFAULT_VERBOSITY`].
    fn from_u8(raw: u8) -> Verbosity {
        Verbosity::ALL
            .into_iter()
            .find(|level| *level as u8 == raw)
            .unwrap_or(DEFAULT_VERBOSITY)
    }
}

/// Whether every statement sent to the database is echoed to stdout.
static LOG_SQL: AtomicBool = AtomicBool::new(false);

/// The current [`Verbosity`], as its discriminant.
static VERBOSITY: AtomicU8 = AtomicU8::new(DEFAULT_VERBOSITY as u8);

/// Set how much this process says. Takes effect on the next message.
pub fn set_verbosity(level: Verbosity) {
    VERBOSITY.store(level as u8, Ordering::Relaxed);
}

/// How much this process is currently saying.
pub fn verbosity() -> Verbosity {
    Verbosity::from_u8(VERBOSITY.load(Ordering::Relaxed))
}

/// Whether a message at `level` would be printed.
///
/// Call this before building a message that costs something to build: the
/// macros already do, and the request middleware uses it to avoid cloning a URI
/// nobody will read.
pub fn enabled(level: Verbosity) -> bool {
    level <= verbosity()
}

/// Turn the SQL echo on or off. Takes effect on the next statement.
pub fn set_log_sql(on: bool) {
    LOG_SQL.store(on, Ordering::Relaxed);
}

/// Whether the SQL echo is on.
///
/// Independent of the verbosity on purpose: SQL is a firehose that an admin
/// turns on to debug one thing, not a rung of the same ladder that decides
/// whether requests are logged.
pub fn log_sql_enabled() -> bool {
    LOG_SQL.load(Ordering::Relaxed)
}

/// Print a message at `level`, if the configured verbosity includes it.
///
/// The message is written as it was formatted, with no prefix of any kind (see
/// the module docs).
///
/// The macros ([`log_error!`], [`log_warn!`], [`log_info!`], [`log_verbose!`],
/// [`log_trace!`]) are the way to call this: they defer formatting until the
/// level check has passed.
pub fn log(level: Verbosity, args: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    emit(Stream::Err, args.to_string());
}

/// What a panic's payload says: the message of `panic!("…")` or of a failed
/// `assert!`, or a placeholder for a payload that is neither.
///
/// For the places a panic is caught and reported — the HTTP layer's 500, a
/// supervised loop's restart — so each says what went wrong the same way.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message")
}

/// The line [`install_panic_hook`]'s hook prints for one panic.
fn panic_line(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unnamed");
    let at = info
        .location()
        .map(|l| format!(" at {}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_default();
    format!(
        "feldspar: a bug: thread `{name}` panicked{at}: {}",
        panic_message(info.payload())
    )
}

/// Report every panic through this crate's log, as one line saying where it
/// happened (security review 2026-11 F-3, TODO-type-hardening H5.2).
///
/// Installed by the binary at start. A panic in a request handler is answered
/// with a 500 and the server goes on (sc-server's panic boundary); a panic in
/// a background loop is restarted (`sc_catalog::supervise`). Neither is
/// silent: this is the line that says it happened and where. It is printed at
/// every verbosity — a panic is a bug — and is followed by the backtrace when
/// `RUST_BACKTRACE` asks for one, as the default hook's would be.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        emit(Stream::Err, panic_line(info));
        let backtrace = std::backtrace::Backtrace::capture();
        if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            emit(Stream::Err, backtrace.to_string());
        }
    }));
}

/// Echo one statement, if the SQL log is on.
///
/// `binds` are the values that will be sent with it. They are printed —
/// unlike in an error message, which is *returned to a client* and so carries
/// only their count — because a statement without its parameters does not tell
/// you what ran, and the person who ticked this box is reading their own
/// server's stdout. That includes password hashes and session tokens, which is
/// why the setting says so.
pub fn log_sql<T: fmt::Debug>(sql: &str, binds: &[T]) {
    if !log_sql_enabled() {
        return;
    }
    emit(Stream::Out, sql_line(sql, binds));
}

/// A duration as an operator reads one: milliseconds up to a second, then
/// seconds.
///
/// Here rather than in each caller because "how long did it take" is what every
/// log line about a slow thing ends with — a model call, an agent run, a tool —
/// and three spellings of 2.3 seconds in one log is three things to eyeball.
/// (A finished HTTP request keeps its own sub-millisecond format: at that scale
/// this one would print `0ms` for most of them.)
pub fn human_duration(elapsed: std::time::Duration) -> String {
    let ms = elapsed.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.1}s", ms / 1000.0)
    }
}

/// How long a single bind's rendering may get before it is cut short: enough
/// for a row's worth of text, short of a megabyte of `bytea` scrolling past.
const MAX_BIND_CHARS: usize = 120;

/// One line of SQL log: the statement, and the binds it goes out with in a
/// trailing `--` comment.
///
/// The line is the statement itself — nothing is prepended — so the echo reads
/// as the script it is. Separate from [`log_sql`] so the rendering can be tested
/// without capturing stdout, and so a caller that has already decided to log (a
/// batch of DDL, a transaction verb) can build the line itself.
pub fn sql_line<T: fmt::Debug>(sql: &str, binds: &[T]) -> String {
    let sql = collapse_whitespace(sql);
    if binds.is_empty() {
        return sql;
    }
    let rendered: Vec<String> = binds
        .iter()
        .map(|bind| truncate(&format!("{bind:?}"), MAX_BIND_CHARS))
        .collect();
    format!("{sql} -- binds: [{}]", rendered.join(", "))
}

/// One statement on one line: a multi-line rendering (DDL, a policy batch) is
/// still one thing that ran, and a log that can be `grep`ed line-by-line is
/// worth more than the original layout.
fn collapse_whitespace(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `text` to `max` characters, marking that it was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

/// Which stream a line goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// Standard output: the SQL echo, which is what somebody redirects.
    Out,
    /// Standard error: level messages, beside every other `feldspar:` line.
    Err,
}

/// Write one finished line — to the [`capture`] buffer if this thread has one,
/// else to the stream it belongs on.
fn emit(stream: Stream, line: String) {
    #[cfg(any(test, feature = "capture"))]
    if capture::intercept(stream, &line) {
        return;
    }
    match stream {
        Stream::Out => println!("{line}"),
        Stream::Err => eprintln!("{line}"),
    }
}

/// Print at [`Verbosity::Error`].
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log($crate::Verbosity::Error, ::std::format_args!($($arg)*))
    };
}

/// Print at [`Verbosity::Warning`].
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log($crate::Verbosity::Warning, ::std::format_args!($($arg)*))
    };
}

/// Print at [`Verbosity::Info`] — where every server request is logged.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log($crate::Verbosity::Info, ::std::format_args!($($arg)*))
    };
}

/// Print at [`Verbosity::Verbose`].
#[macro_export]
macro_rules! log_verbose {
    ($($arg:tt)*) => {
        $crate::log($crate::Verbosity::Verbose, ::std::format_args!($($arg)*))
    };
}

/// Print at [`Verbosity::Trace`].
#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        $crate::log($crate::Verbosity::Trace, ::std::format_args!($($arg)*))
    };
}

/// Collecting log lines instead of printing them, so a **test can assert what
/// was logged** rather than what a formatter would have produced.
///
/// Behind the `capture` feature, which nothing but a `[dev-dependencies]` entry
/// should turn on: without it this module does not exist and [`emit`] is a
/// `println!`. A crate that wants to test its own logging adds
/// `sc-log = { workspace = true, features = ["capture"] }` to its dev
/// dependencies and calls [`start`] at the top of the test.
///
/// **Per thread, and opt-in.** The buffer is a thread-local installed by
/// [`start`], so a test captures its own lines and not those of the tests
/// running beside it, and a thread that never started one prints as usual. An
/// async test on tokio's current-thread runtime (`#[tokio::test]`) runs its
/// futures on the thread that started the capture, which is why this works for
/// a streamed model call.
///
/// **The switches are not per-thread, though**, so two tests in one binary that
/// each set a verbosity will read each other's. [`guard`] is the answer and the
/// thing to use: it takes a process-wide lock for the duration, sets the level,
/// starts the capture, and puts everything back when it drops.
#[cfg(any(test, feature = "capture"))]
pub mod capture {
    use std::cell::RefCell;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::{Stream, Verbosity};

    /// Taken for the whole of a test that moves the logging switches, because
    /// the switches are the process's and the tests are not.
    fn lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            // A test that panicked while holding it poisoned nothing that
            // matters: the guard restores the switches on the way out either
            // way, and refusing to run every later test because one failed
            // would turn one red test into a binary's worth.
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Exclusive use of the logging switches, with capture on, for as long as
    /// this is held.
    ///
    /// Dropping it restores the verbosity and the SQL echo to what they were and
    /// stops capturing — including when the test panics, which is what keeps one
    /// failure from leaving every later test in the binary at `trace`.
    pub struct Guard {
        /// Held for the lifetime of the guard; never read.
        _lock: MutexGuard<'static, ()>,
        /// The verbosity to put back.
        verbosity: Verbosity,
        /// The SQL echo to put back.
        log_sql: bool,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            super::set_verbosity(self.verbosity);
            super::set_log_sql(self.log_sql);
            stop();
        }
    }

    /// Take the switches, set the verbosity to `level`, and start capturing.
    ///
    /// ```ignore
    /// let _log = sc_log::capture::guard(sc_log::Verbosity::Trace);
    /// do_the_thing();
    /// assert!(sc_log::capture::take().iter().any(|l| l.contains("…")));
    /// ```
    pub fn guard(level: Verbosity) -> Guard {
        let guard = Guard {
            _lock: lock(),
            verbosity: super::verbosity(),
            log_sql: super::log_sql_enabled(),
        };
        super::set_verbosity(level);
        super::set_log_sql(false);
        start();
        guard
    }

    thread_local! {
        /// The lines this thread is collecting, or `None` when it is printing.
        static LINES: RefCell<Option<Vec<(bool, String)>>> = const { RefCell::new(None) };
    }

    /// Start (or restart) capturing on this thread, discarding anything held.
    pub fn start() {
        LINES.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
    }

    /// Stop capturing on this thread; later lines print again.
    pub fn stop() {
        LINES.with(|lines| *lines.borrow_mut() = None);
    }

    /// Take every line captured on this thread so far, in order.
    pub fn take() -> Vec<String> {
        taken().into_iter().map(|(_, line)| line).collect()
    }

    /// Take only the lines that went to **stdout** — the SQL echo.
    pub fn take_stdout() -> Vec<String> {
        taken()
            .into_iter()
            .filter_map(|(is_stdout, line)| is_stdout.then_some(line))
            .collect()
    }

    /// Every captured line with the stream it was written to (`true` = stdout).
    pub fn taken() -> Vec<(bool, String)> {
        LINES.with(|lines| match &mut *lines.borrow_mut() {
            Some(held) => std::mem::take(held),
            None => Vec::new(),
        })
    }

    /// Record `line` if this thread is capturing, answering whether it was
    /// taken instead of printed.
    pub(super) fn intercept(stream: Stream, line: &str) -> bool {
        LINES.with(|lines| match &mut *lines.borrow_mut() {
            Some(held) => {
                held.push((stream == Stream::Out, line.to_owned()));
                true
            }
            None => false,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_panic_is_logged_as_one_line_saying_where() {
        let _log = capture::guard(Verbosity::Error);
        install_panic_hook();
        let caught = std::panic::catch_unwind(|| {
            std::panic::panic_any(String::from("the sky fell"));
        });
        // Back to the default hook, so a failing test elsewhere in this binary
        // still prints as usual.
        drop(std::panic::take_hook());
        let payload = caught.unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "the sky fell");
        let lines = capture::take();
        let line = lines
            .iter()
            .find(|l| l.contains("the sky fell"))
            .unwrap_or_else(|| panic!("no panic line in {lines:?}"));
        assert!(line.starts_with("feldspar: a bug: thread `"), "{line}");
        assert!(line.contains("panicked at "), "{line}");
        assert!(line.contains("lib.rs:"), "the location is named: {line}");
        assert_eq!(panic_message(&42_u8), "a panic with no message");
    }

    /// The switches are process-wide, so the tests that move them take turns —
    /// through the same guard every other crate's tests use.
    fn switches() -> capture::Guard {
        capture::guard(DEFAULT_VERBOSITY)
    }

    /// The levels a test reads back, as plain strings.
    fn lines() -> Vec<String> {
        capture::take()
    }

    #[test]
    fn the_ladder_runs_from_error_to_trace() {
        assert!(Verbosity::Error < Verbosity::Warning);
        assert!(Verbosity::Warning < Verbosity::Info);
        assert!(Verbosity::Info < Verbosity::Verbose);
        assert!(Verbosity::Verbose < Verbosity::Trace);
        assert_eq!(
            Verbosity::ALL.map(Verbosity::as_str),
            ["error", "warning", "info", "verbose", "trace"]
        );
    }

    #[test]
    fn a_level_is_parsed_however_it_is_capitalised_and_a_typo_is_not() {
        assert_eq!(Verbosity::parse("info").unwrap(), Verbosity::Info);
        assert_eq!(Verbosity::parse(" Trace ").unwrap(), Verbosity::Trace);
        let err = Verbosity::parse("chatty").unwrap_err().to_string();
        assert!(err.contains("chatty"), "{err}");
        assert!(err.contains("verbose"), "{err}");
    }

    /// The point of the whole ladder: a message is printed when its level is at
    /// or below the configured one, and silent otherwise.
    #[test]
    fn a_message_above_the_configured_level_is_not_printed() {
        let _guard = switches();
        set_verbosity(Verbosity::Info);
        assert!(enabled(Verbosity::Error) && enabled(Verbosity::Info));
        assert!(!enabled(Verbosity::Verbose) && !enabled(Verbosity::Trace));

        log_error!("boom");
        log_info!("GET /health → {}", 200);
        log_verbose!("not this one");
        log_trace!("nor this");
        assert_eq!(lines(), ["boom", "GET /health → 200"]);
    }

    /// The default is quiet enough that a server does not log every request
    /// until somebody asks it to.
    #[test]
    fn requests_are_not_logged_until_the_level_reaches_info() {
        let _guard = switches();
        assert_eq!(verbosity(), Verbosity::Warning);
        assert!(!enabled(Verbosity::Info));
        set_verbosity(Verbosity::Info);
        assert!(enabled(Verbosity::Info));
    }

    #[test]
    fn sql_is_printed_to_stdout_only_while_the_switch_is_on() {
        let _guard = switches();
        log_sql("SELECT 1", &[] as &[String]);
        assert!(capture::take().is_empty(), "the switch was off");

        set_log_sql(true);
        log_sql("SELECT * FROM \"books\" WHERE \"id\" = $1", &["Int(3)"]);
        // Stdout, because it is the output somebody redirects to a file.
        let logged = capture::take_stdout();
        assert_eq!(logged.len(), 1);
        assert!(logged[0].contains("SELECT * FROM \"books\""));
        assert!(logged[0].contains("Int(3)"), "{}", logged[0]);
    }

    #[test]
    fn a_duration_reads_as_milliseconds_then_seconds() {
        assert_eq!(human_duration(Duration::from_micros(12_340)), "12ms");
        assert_eq!(human_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(human_duration(Duration::from_millis(2_340)), "2.3s");
    }

    #[test]
    fn a_logged_statement_is_one_line_with_its_binds() {
        assert_eq!(sql_line("SELECT 1", &[] as &[i32]), "SELECT 1");
        assert_eq!(
            sql_line("INSERT INTO t\n  VALUES ($1, $2)", &["a", "b"]),
            "INSERT INTO t VALUES ($1, $2) -- binds: [\"a\", \"b\"]"
        );
    }

    /// A `bytea` bind is a megabyte of digits printed one row at a time unless
    /// something cuts it off.
    #[test]
    fn an_enormous_bind_is_cut_short() {
        let big = "x".repeat(5_000);
        let line = sql_line("SELECT $1", &[big]);
        assert!(line.chars().count() < MAX_BIND_CHARS + 60, "{}", line.len());
        assert!(line.ends_with("…]"), "{line}");
    }
}
