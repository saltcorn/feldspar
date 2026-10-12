//! **Code libraries** (TODO F3, design "The code library"): named, shared
//! JavaScript that a code body imports with `lib(name)`.
//!
//! # What crosses, and when
//!
//! A body's `lib` is synchronous, so whatever it may import has to be on the
//! isolate before the body starts — the same constraint that makes `fs(name)`'s
//! store names travel with the job. The caller resolves which libraries a run
//! may import ([`CodeLibraries`]: the application's own over the instance's),
//! and the worker makes sure each one is **compiled** on the isolate before the
//! run's script, sending a library's source only when the isolate has not got
//! that exact text. Compiled, not run: a library's top level runs only when a
//! body imports it.
//!
//! # Evaluated per run, compiled per isolate
//!
//! A library runs **with the importing run's own handles** — its `db`, `fetch`,
//! `fs`, `trigger`, `modfn`, `live`, `console` and `user` — so it can never do
//! more than the body that imported it. Those handles are bound to the run's
//! token and die with the run, so the *evaluated* exports cannot outlive it:
//! a library is evaluated at most once per run (a second `lib("x")` in the same
//! run answers the same object) and its compiled function is what is cached.
//! That also means module-level state never leaks from one caller to the next.
//!
//! The compiled form is a function of the library's code **and** the handle set
//! of the run, because a handle a run does not have is a parameter the library
//! does not have either — naming `fetch` in a library imported by a body with no
//! network is the same `ReferenceError` it is in the body. So the content key
//! covers both, and an edited library is a new key: nothing has to be told to
//! forget the old one, which the isolate's bounded cache drops in time.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::code::CodeCall;
use crate::eval::JsEvaluator;

/// The parameters every library function takes before the run's handles:
/// `exports` and `module` (CommonJS's shape, so `module.exports = …` works),
/// `lib` (to import another library) and `user`.
pub(crate) const LIBRARY_PARAMS: [&str; 4] = ["exports", "module", "lib", "user"];

/// The name `lib` binds under in a code body.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub(crate) const LIB: &str = "lib";

/// The code libraries one run may import, **already resolved**: one source per
/// name, the application's own having won over the instance's.
///
/// Shared (`Arc`) and cheap to clone: the server resolves a set once per
/// application and generation and hands the same one to every run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeLibraries {
    libraries: BTreeMap<String, Arc<str>>,
}

impl CodeLibraries {
    /// A set from `(name, code)` pairs. A later pair with the same name
    /// replaces an earlier one.
    pub fn new<N: Into<String>, C: Into<Arc<str>>>(
        libraries: impl IntoIterator<Item = (N, C)>,
    ) -> CodeLibraries {
        CodeLibraries {
            libraries: libraries
                .into_iter()
                .map(|(name, code)| (name.into(), code.into()))
                .collect(),
        }
    }

    /// The set a body in an application sees: the instance-wide libraries, with
    /// the application's own replacing any of the same name (design §3.3's
    /// "application first, then instance").
    pub fn resolve<N: Into<String>, C: Into<Arc<str>>>(
        instance: impl IntoIterator<Item = (N, C)>,
        application: impl IntoIterator<Item = (N, C)>,
    ) -> CodeLibraries {
        let mut set = CodeLibraries::new(instance);
        for (name, code) in application {
            set.libraries.insert(name.into(), code.into());
        }
        set
    }

    /// Whether there is nothing to import.
    pub fn is_empty(&self) -> bool {
        self.libraries.is_empty()
    }

    /// The names a body can import, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.libraries.keys().map(String::as_str)
    }

    /// The source of the library called `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.libraries.get(name).map(|code| &**code)
    }

    /// Every library, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.libraries
            .iter()
            .map(|(name, code)| (name.as_str(), &**code))
    }
}

/// The parameters a library takes after [`LIBRARY_PARAMS`]: the run's
/// `handles`, then the shadowed node globals (§1a) nobody passes — so a library
/// reaching for `process` gets `undefined`, exactly as a body does.
pub(crate) fn library_params(mut handles: Vec<&'static str>) -> Vec<&'static str> {
    for name in crate::code::SHADOWED_NODE_GLOBALS {
        if !LIBRARY_PARAMS.contains(&name) && !handles.contains(&name) {
            handles.push(name);
        }
    }
    handles
}

/// A library's compiled form: a plain (not `async`) function, because `lib` is
/// synchronous — a library defines things, and what it defines may be async.
///
/// Everything before the code is on **one line**, so a line number the engine
/// reports is the author's plus one ([`LIBRARY_LINE_OFFSET`]). `require` is the
/// body's refusal and `models` rides on `db` exactly as in a body.
pub(crate) fn library_definition(code: &str, handles: &[&str], wants_db: bool) -> String {
    let params = LIBRARY_PARAMS
        .iter()
        .chain(handles.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let models = if wants_db {
        "const models = __scMakeModels(db); "
    } else {
        ""
    };
    format!(
        "function ({params}) {{ \"use strict\"; const require = __scRequire(false); {models}\n\
         {code}\n}}"
    )
}

/// How many lines the definition script puts before the library's first line.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub(crate) const LIBRARY_LINE_OFFSET: usize = 1;

/// The content key a library definition is known by on an isolate — the same
/// hash a body's definition is keyed with.
pub(crate) fn library_key(definition: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    definition.hash(&mut hasher);
    hasher.finish()
}

/// Turn the engine's report of a library that did not compile into the
/// author's terms: the message, and the line **in the library**.
///
/// The engine names the definition script and a line in it
/// (`…lib:auth.js:3:7`); the script carries one line of wrapper before the
/// code, which is taken off.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub(crate) fn compile_error(name: &str, engine: &str) -> String {
    let script = format!("lib:{name}.js:");
    let first = engine
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(engine)
        .trim_start_matches("Uncaught ")
        .to_owned();
    let line = engine.find(&script).and_then(|at| {
        let rest = &engine[at + script.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<usize>().ok()
    });
    match line {
        Some(line) if line > LIBRARY_LINE_OFFSET => {
            format!("{first} (line {})", line - LIBRARY_LINE_OFFSET)
        }
        _ => first,
    }
}

/// The name a library is compiled under for [`check_code_library`]. Not a
/// library anyone can import: the check's run has nothing else in scope.
const CHECK_NAME: &str = "library";

/// Whether `code` compiles as a library, asked of `evaluator`'s code runtime.
///
/// What a save calls (design §3.3: "a save compile-checks the code"). The code
/// is compiled exactly as a run would compile it and **not run**: a library's
/// top level may read tables or send mail, and a save is not the place for
/// either. Answers `None` when it compiles, or why not — the engine's message
/// with the line **in the library**. `Err` is the engine failing to answer.
pub async fn check_code_library(evaluator: &dyn JsEvaluator, code: &str) -> Result<Option<String>> {
    // The check's run is a pure one — no host of any kind and no bindings — so
    // its handle set is `console` alone, and the key below is the key the
    // runtime will compile the library under for it.
    let definition = library_definition(code, &library_params(vec!["console"]), false);
    let key = library_key(&definition);
    let call = CodeCall {
        code: format!("return __scLibraryError(\"{key:016x}\");"),
        libraries: Some(Arc::new(CodeLibraries::new([(CHECK_NAME, code)]))),
        ..CodeCall::default()
    };
    match evaluator.run_code(call).await? {
        Json::Null => Ok(None),
        Json::String(message) => Ok(Some(message)),
        other => Err(Error::msg(format!(
            "the library check answered something unexpected: {other}"
        ))),
    }
}

/// The libraries `code` imports by a literal name — every `lib("name")`,
/// `lib('name')` or `` lib(`name`) `` in it — for an admin's "used by" list.
///
/// A scan, not a parse: a name built at run time is not found, and a mention in
/// a comment or a string is. That is the right error for a "used by" list,
/// which is a warning before an edit and never an authority.
pub fn imported_libraries(code: &str) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(at) = code[from..].find("lib(") {
        let start = from + at;
        from = start + 4;
        // `mylib(` and `x.lib(` are not this `lib`.
        if start > 0 {
            let before = bytes[start - 1];
            if before.is_ascii_alphanumeric() || before == b'_' || before == b'$' || before == b'.'
            {
                continue;
            }
        }
        let rest = code[from..].trim_start();
        let Some(quote) = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\'' | '`'))
        else {
            continue;
        };
        let inner = &rest[1..];
        let Some(end) = inner.find(quote) else {
            continue;
        };
        let name = &inner[..end];
        if !name.is_empty() && inner[end + 1..].trim_start().starts_with(')') {
            found.insert(name.to_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_imports_are_the_literal_names() {
        let code = r#"const { a } = lib("auth"); const u = lib( 'util' );
            const t = lib(`tpl`); mylib("no"); x.lib("no"); lib(name); lib("half"#;
        let found: Vec<String> = imported_libraries(code).into_iter().collect();
        assert_eq!(found, vec!["auth", "tpl", "util"]);
    }

    #[test]
    fn the_application_wins_over_the_instance() {
        let set = CodeLibraries::resolve(
            [("auth", "exports.who = 'instance';"), ("util", "")],
            [("auth", "exports.who = 'app';")],
        );
        assert_eq!(set.get("auth"), Some("exports.who = 'app';"));
        assert_eq!(set.get("util"), Some(""));
        assert_eq!(set.names().collect::<Vec<_>>(), vec!["auth", "util"]);
    }

    #[test]
    fn the_definition_keeps_the_code_on_its_own_lines() {
        let def = library_definition("exports.a = 1;\nexports.b = 2;", &["db", "console"], true);
        let lines: Vec<&str> = def.lines().collect();
        assert_eq!(lines[LIBRARY_LINE_OFFSET], "exports.a = 1;");
        assert!(lines[0].contains("function (exports, module, lib, user, db, console)"));
        assert!(lines[0].contains("__scMakeModels(db)"));
        // A different handle set is a different key: the cached function must
        // never be called with a scope it was not compiled for.
        let pure = library_definition("exports.a = 1;\nexports.b = 2;", &["console"], false);
        assert_ne!(library_key(&def), library_key(&pure));
    }

    #[test]
    fn a_compile_error_names_the_authors_line() {
        let engine = "Uncaught SyntaxError: Unexpected identifier 'b'\n    at lib:auth.js:3:7";
        assert_eq!(
            compile_error("auth", engine),
            "SyntaxError: Unexpected identifier 'b' (line 2)"
        );
        // No location is still the message.
        assert_eq!(
            compile_error("auth", "SyntaxError: nope"),
            "SyntaxError: nope"
        );
    }
}
