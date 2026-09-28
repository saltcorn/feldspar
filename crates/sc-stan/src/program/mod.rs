//! A Stan program: the file in a store, the files it `#include`s, and what it
//! declares (TODO §§5–6).
//!
//! The program lives in a file store and nowhere else (§6), so reading one is
//! reading a store: [`Program::load`] reads the main file, finds its
//! `#include`s with the [lexer](lex) (so an `#include` inside a comment is not
//! one), resolves each **relative to the including file and inside the same
//! store**, and reads those in turn. What comes back is every file's text and
//! SHA-256 — the snapshot a fit stores on its instance, so that editing the
//! file afterwards changes the model and never an existing instance.
//!
//! [`Program::interface`] then parses the declarations (see [`parse`]) into the
//! host's [`Interface`].
//!
//! ## Includes
//!
//! `#include` is textual, so the included file's tokens are spliced in where
//! the directive was. A path is resolved against the including file's
//! directory; `..` may climb within the store but never out of it, and an
//! absolute path is refused — both by name, because a program that can include
//! `/etc/passwd` is a program that can read it into an error message. A cycle
//! is refused naming the whole chain.
//!
//! `stanc` itself looks an `#include` up in its include paths rather than
//! beside the including file, so [`Program::layout`] writes each file with its
//! directives **rewritten** to the resolved store path and hands `stanc` the
//! layout's root as its one include path. The rewrite keeps each directive on
//! its line, so `stanc`'s line numbers are the store file's.

mod lex;
mod parse;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use sc_error::{Context, Error, Result};
use sc_files::FileStore;
use sc_model::Interface;
use sha2::{Digest, Sha256};

use lex::{Tok, Token, lex};

/// How many files one program may consist of. A real program has a handful;
/// this is a bound on a mistake, not on a model.
pub const MAX_FILES: usize = 64;
/// How large one file of a program may be. The largest programs in the Stan
/// case studies are tens of kilobytes.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// One file of a program, as read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProgramFile {
    /// Its path in the store — the main file's as configured, an included
    /// file's as resolved.
    pub path: String,
    /// The source.
    pub text: String,
    /// The SHA-256 of the source, lowercase hex.
    pub sha256: String,
}

impl ProgramFile {
    /// A file of `path` with this source.
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> ProgramFile {
        let text = text.into();
        let sha256 = hex(&Sha256::digest(text.as_bytes()));
        ProgramFile {
            path: path.into(),
            text,
            sha256,
        }
    }
}

/// A program and every file it includes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Program {
    /// The file store it was read from.
    pub store: String,
    /// Every file, the main one first, then the included ones in the order
    /// they were first reached.
    pub files: Vec<ProgramFile>,
}

impl Program {
    /// A program from files already in hand — the main one first. Includes
    /// are resolved against these files and nothing else.
    pub fn from_files(store: impl Into<String>, files: Vec<ProgramFile>) -> Result<Program> {
        if files.is_empty() {
            return Err(Error::invalid("a program needs at least its main file"));
        }
        Ok(Program {
            store: store.into(),
            files,
        })
    }

    /// Read the program at `path` in `store` (called `store_name` in
    /// sentences), and every file it includes.
    pub async fn load(store: &dyn FileStore, store_name: &str, path: &str) -> Result<Program> {
        let main = normalise(path.trim())
            .map_err(|why| Error::invalid(format!("the program path `{path}` {why}")))?;
        let mut files: Vec<ProgramFile> = Vec::new();
        let mut queue = VecDeque::from([(main, None::<(String, u32, String)>)]);
        while let Some((path, from)) = queue.pop_front() {
            if files.iter().any(|f| f.path == path) {
                continue;
            }
            if files.len() >= MAX_FILES {
                return Err(Error::invalid(format!(
                    "the program includes more than {MAX_FILES} files"
                )));
            }
            let missing = || match &from {
                None => Error::invalid(format!(
                    "the program `{path}` is not in the file store `{store_name}`"
                )),
                Some((by, line, raw)) => Error::invalid(format!(
                    "{by}:{line}: `#include {raw}` names `{path}`, which is not in the file \
                     store `{store_name}`"
                )),
            };
            let stat = store
                .stat(&path)
                .await
                .with_context(|| format!("reading `{path}` from the file store `{store_name}`"))?;
            let Some(stat) = stat else {
                return Err(missing());
            };
            if stat.is_dir {
                return Err(Error::invalid(format!(
                    "`{path}` in the file store `{store_name}` is a directory, not a Stan file"
                )));
            }
            if stat.size > MAX_FILE_BYTES {
                return Err(Error::invalid(format!(
                    "`{path}` is larger than {} KiB, which is not a Stan program",
                    MAX_FILE_BYTES / 1024
                )));
            }
            let bytes = store
                .read(&path)
                .await
                .with_context(|| format!("reading `{path}` from the file store `{store_name}`"))?;
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| Error::invalid(format!("`{path}` is not UTF-8 text")))?;
            for token in lex(&text, files.len(), &path)? {
                if let Tok::Include(raw) = &token.kind {
                    let target = resolve_include(&path, raw).map_err(|why| {
                        Error::invalid(format!("{path}:{}: `#include {raw}` {why}", token.line))
                    })?;
                    queue.push_back((target, Some((path.clone(), token.line, raw.clone()))));
                }
            }
            files.push(ProgramFile::new(path, text));
        }
        Ok(Program {
            store: store_name.to_owned(),
            files,
        })
    }

    /// The main file's store path.
    pub fn main_path(&self) -> &str {
        &self.files[0].path
    }

    /// What the program declares (§5), refusing what the binder cannot bind.
    pub fn interface(&self) -> Result<Interface> {
        let tokens = self.tokens()?;
        parse::interface(&tokens, &self.files)
    }

    /// The whole program as one token list, every `#include` replaced by the
    /// tokens of the file it names.
    fn tokens(&self) -> Result<Vec<Token>> {
        let index: BTreeMap<&str, usize> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.as_str(), i))
            .collect();
        let mut out = Vec::new();
        let mut chain = Vec::new();
        self.splice(0, &index, &mut chain, &mut out)?;
        Ok(out)
    }

    fn splice(
        &self,
        file: usize,
        index: &BTreeMap<&str, usize>,
        chain: &mut Vec<usize>,
        out: &mut Vec<Token>,
    ) -> Result<()> {
        chain.push(file);
        let this = &self.files[file];
        for token in lex(&this.text, file, &this.path)? {
            let Tok::Include(raw) = &token.kind else {
                out.push(token);
                continue;
            };
            let target = resolve_include(&this.path, raw).map_err(|why| {
                Error::invalid(format!(
                    "{}:{}: `#include {raw}` {why}",
                    this.path, token.line
                ))
            })?;
            let Some(&next) = index.get(target.as_str()) else {
                return Err(Error::invalid(format!(
                    "{}:{}: `#include {raw}` names `{target}`, which is not part of this program",
                    this.path, token.line
                )));
            };
            if let Some(at) = chain.iter().position(|f| *f == next) {
                let cycle: Vec<&str> = chain[at..]
                    .iter()
                    .chain([&next])
                    .map(|f| self.files[*f].path.as_str())
                    .collect();
                return Err(Error::invalid(format!(
                    "the program's `#include`s go round in a circle: {}",
                    cycle.join(" → ")
                )));
            }
            self.splice(next, index, chain, out)?;
        }
        chain.pop();
        Ok(())
    }

    /// Write the program under `dir` for `stanc` and `make`: each file at its
    /// store path, its `#include`s rewritten to the resolved store path (see
    /// the module docs). Returns the main file's path relative to `dir`, which
    /// is its store path.
    pub fn layout(&self, dir: &Path) -> Result<String> {
        for file in &self.files {
            let target = dir.join(&file.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&target, self.rewritten(file)?)
                .with_context(|| format!("writing {}", target.display()))?;
        }
        Ok(self.main_path().to_owned())
    }

    /// `file`'s source with each `#include` naming the resolved store path.
    fn rewritten(&self, file: &ProgramFile) -> Result<String> {
        let mut text = file.text.clone();
        let includes: Vec<Token> = lex(&file.text, 0, &file.path)?
            .into_iter()
            .filter(|t| matches!(t.kind, Tok::Include(_)))
            .collect();
        // Back to front, so the earlier spans stay where they were.
        for token in includes.iter().rev() {
            let Tok::Include(raw) = &token.kind else {
                continue;
            };
            let target = resolve_include(&file.path, raw).map_err(|why| {
                Error::invalid(format!(
                    "{}:{}: `#include {raw}` {why}",
                    file.path, token.line
                ))
            })?;
            text.replace_range(token.start..token.end, &format!("#include \"{target}\""));
        }
        Ok(text)
    }

    /// Every file's path and SHA-256 — what an instance records so the screen
    /// can say "the program has changed since this fit".
    pub fn hashes(&self) -> BTreeMap<String, String> {
        self.files
            .iter()
            .map(|f| (f.path.clone(), f.sha256.clone()))
            .collect()
    }
}

/// `raw`, an `#include` in the file at `from`, as a store path.
fn resolve_include(from: &str, raw: &str) -> std::result::Result<String, String> {
    if raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':') {
        return Err(
            "names an absolute path; an include must be a path inside the same file \
                    store, relative to the including file"
                .to_owned(),
        );
    }
    let dir = match from.rfind('/') {
        Some(at) => &from[..at],
        None => "",
    };
    let joined = if dir.is_empty() {
        raw.to_owned()
    } else {
        format!("{dir}/{raw}")
    };
    normalise(&joined)
}

/// `path` with `.` and `..` segments folded away; refused when `..` climbs out
/// of the store or the path is absolute or empty.
fn normalise(path: &str) -> std::result::Result<String, String> {
    if path.starts_with('/') {
        return Err("must be relative to the file store, not absolute".to_owned());
    }
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err("climbs out of the file store with `..`".to_owned());
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err("names no file".to_owned());
    }
    Ok(parts.join("/"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_include_resolves_beside_the_including_file() {
        assert_eq!(resolve_include("radon.stan", "f.stan").unwrap(), "f.stan");
        assert_eq!(
            resolve_include("models/radon.stan", "lib/f.stan").unwrap(),
            "models/lib/f.stan"
        );
        assert_eq!(
            resolve_include("models/lib/f.stan", "../g.stan").unwrap(),
            "models/g.stan"
        );
        assert_eq!(
            resolve_include("models/radon.stan", "./f.stan").unwrap(),
            "models/f.stan"
        );
    }

    #[test]
    fn an_include_cannot_leave_the_store() {
        assert!(
            resolve_include("radon.stan", "../secrets.stan")
                .unwrap_err()
                .contains("climbs out of the file store")
        );
        assert!(
            resolve_include("models/radon.stan", "../../x.stan")
                .unwrap_err()
                .contains("climbs out")
        );
        assert!(
            resolve_include("radon.stan", "/etc/passwd")
                .unwrap_err()
                .contains("absolute path")
        );
    }

    #[test]
    fn a_file_is_hashed_as_it_was_read() {
        let f = ProgramFile::new("a.stan", "");
        assert_eq!(
            f.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_cycle_is_refused_naming_the_chain() {
        let program = Program::from_files(
            "s",
            vec![
                ProgramFile::new("m.stan", "functions {\n#include a.stan\n}"),
                ProgramFile::new("a.stan", "#include b.stan"),
                ProgramFile::new("b.stan", "#include a.stan"),
            ],
        )
        .unwrap();
        let err = program.interface().unwrap_err().to_string();
        assert!(
            err.contains("go round in a circle: a.stan → b.stan → a.stan"),
            "{err}"
        );
    }

    #[test]
    fn the_layout_rewrites_includes_to_store_paths_on_the_same_line() {
        let program = Program::from_files(
            "s",
            vec![
                ProgramFile::new("models/m.stan", "functions {\n#include lib/f.stan\n}\n"),
                ProgramFile::new("models/lib/f.stan", "#include <../g.stan> // g\n"),
                ProgramFile::new("models/g.stan", "real g(real x) { return x; }\n"),
            ],
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("sc-stan-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(program.layout(&dir).unwrap(), "models/m.stan");
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
        assert_eq!(
            read("models/m.stan"),
            "functions {\n#include \"models/lib/f.stan\"\n}\n"
        );
        assert_eq!(read("models/lib/f.stan"), "#include \"models/g.stan\"\n");
        assert_eq!(read("models/g.stan"), "real g(real x) { return x; }\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
