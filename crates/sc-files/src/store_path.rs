//! [`StorePath`]: a store-relative path that has passed the one validator
//! (code-quality review §4.6, TODO-code-quality Q4.1).
//!
//! Every [`FileStore`](crate::FileStore) method takes one, so a backend — the
//! local directory, a git checkout, an object store, a test double — is never
//! handed a path nobody checked, and the rules below are written once instead
//! of once per backend and once per route (security review #12, #13, #31).
//!
//! A path is `/`-separated plain names. The validator:
//!
//! - **drops** empty segments and `.` (a leading, trailing or doubled slash, a
//!   `./`), so `"/a//b/"` is `a/b` and `""`, `"/"` and `"."` are the root. A
//!   stored `File` value or a route's `dir + "/" + name` keeps working, and the
//!   value has no empty segment once parsed;
//! - **refuses** a `..` segment, a `\` (one more separator on Windows), a `:`
//!   (a drive, or an NTFS stream), a NUL, and a segment of nothing but dots and
//!   spaces (Windows trims `.. ` to `..` and `...` to nothing).
//!
//! It also decides, once, whether the path is inside a `.git` directory at any
//! depth — `.git` in any case, with trailing dots and spaces ignored, or its
//! NTFS 8.3 short name `git~1` ([`StorePath::in_git_dir`]). That is a *fact
//! about* the path rather than a refusal, because who may reach `.git` depends
//! on the operation: nothing that writes through a store may land there (the
//! local backend refuses it unless it is restoring a backup's own checkout),
//! and nobody below admin may read or list it (`check_access`), while an
//! administrator's file manager still shows and reads it.
//!
//! What the spelling cannot decide — a symbolic link on disk that takes a path
//! out of the root or into `.git` — stays with the backend that has a disk
//! ([`LocalFileStore`](crate::LocalFileStore)'s `beneath_root`).

use std::borrow::Cow;
use std::fmt;

use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};

/// A validated, normalised store-relative path (see the module docs).
///
/// Serialises as the string it holds, so a value that crosses the wire or is
/// stored is the same text it always was.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StorePath {
    /// The normalised spelling: segments joined by `/`, no leading or trailing
    /// slash, `""` for the root.
    path: String,
    /// Whether a segment is `.git` in one of its spellings.
    git: bool,
}

impl StorePath {
    /// The store's root.
    pub fn root() -> StorePath {
        StorePath {
            path: String::new(),
            git: false,
        }
    }

    /// Parse `raw`, refusing what may not name a path inside a store.
    pub fn parse(raw: &str) -> Result<StorePath> {
        let mut path = String::with_capacity(raw.len());
        let mut git = false;
        for segment in raw.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            check_segment(segment, raw)?;
            git |= is_git_segment(segment);
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(segment);
        }
        Ok(StorePath { path, git })
    }

    /// A path the program spells itself (`"package.json"`, `".feldspar/x"`).
    ///
    /// # Panics
    ///
    /// When `raw` is not a valid store path: that is a bug in the literal, not
    /// something a caller sent.
    pub fn from_static(raw: &'static str) -> StorePath {
        match StorePath::parse(raw) {
            Ok(path) => path,
            Err(e) => panic!("{raw:?} is not a store path: {e}"),
        }
    }

    /// The normalised text.
    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// Whether this is the store's root.
    pub fn is_root(&self) -> bool {
        self.path.is_empty()
    }

    /// Whether the path is, or is inside, a `.git` directory (see the module
    /// docs for the spellings).
    pub fn in_git_dir(&self) -> bool {
        self.git
    }

    /// The segments, from the root inwards.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.path.split('/').filter(|s| !s.is_empty())
    }

    /// The last segment; `""` for the root.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }

    /// The directory holding this entry; `None` for the root.
    pub fn parent(&self) -> Option<StorePath> {
        if self.is_root() {
            return None;
        }
        let parent = match self.path.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };
        Some(StorePath {
            path: parent.to_owned(),
            git: parent.split('/').any(is_git_segment),
        })
    }

    /// `rel` beneath this path, validated as any path is (so `..` in `rel` is
    /// refused rather than climbing out of this one).
    pub fn join(&self, rel: &str) -> Result<StorePath> {
        let child = StorePath::parse(rel)?;
        if self.is_root() {
            return Ok(child);
        }
        if child.is_root() {
            return Ok(self.clone());
        }
        Ok(StorePath {
            path: format!("{}/{}", self.path, child.path),
            git: self.git || child.git,
        })
    }

    /// Every prefix of the path, from the root inwards, including the path
    /// itself: `a/b/c.txt` yields `a`, `a/b`, `a/b/c.txt`. The root yields
    /// nothing.
    pub fn ancestors(&self) -> Vec<StorePath> {
        let mut out = Vec::new();
        let mut current = StorePath::root();
        for segment in self.segments() {
            if !current.path.is_empty() {
                current.path.push('/');
            }
            current.path.push_str(segment);
            current.git |= is_git_segment(segment);
            out.push(current.clone());
        }
        out
    }

    /// Whether `self` is `dir` or lies beneath it.
    pub fn starts_with(&self, dir: &StorePath) -> bool {
        dir.is_root()
            || self.path == dir.path
            || (self.path.starts_with(&dir.path)
                && self.path.as_bytes().get(dir.path.len()) == Some(&b'/'))
    }
}

/// One segment, which is neither empty nor `.`: refused when it could leave
/// the store, here or on Windows.
fn check_segment(segment: &str, raw: &str) -> Result<()> {
    if segment == ".."
        || segment.contains(['\\', ':', '\0'])
        || segment.trim_end_matches(['.', ' ']).is_empty()
    {
        return Err(Error::invalid(format!(
            "path {raw:?} escapes the file store root"
        )));
    }
    Ok(())
}

/// `.git` in any case, with trailing dots and spaces ignored (Windows drops
/// them), or `git~<digits>`, the 8.3 short name NTFS gives `.git` and which
/// git's own `core.protectNTFS` refuses for the same reason (security review
/// #31).
fn is_git_segment(segment: &str) -> bool {
    let segment = segment.trim_end_matches(['.', ' ']);
    if segment.eq_ignore_ascii_case(".git") {
        return true;
    }
    match (segment.get(..4), segment.get(4..)) {
        (Some(head), Some(tail)) => {
            head.eq_ignore_ascii_case("git~")
                && !tail.is_empty()
                && tail.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    }
}

/// Whether the store-relative spelling `path` is inside (or is) a `.git`
/// directory, at any depth — [`StorePath::in_git_dir`] for text that has not
/// been parsed, split on `\` as well as `/` so a spelling the validator would
/// refuse is still recognised.
pub fn in_git_dir(path: &str) -> bool {
    path.split(['/', '\\']).any(is_git_segment)
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path)
    }
}

impl fmt::Debug for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.path, f)
    }
}

impl AsRef<str> for StorePath {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

impl TryFrom<String> for StorePath {
    type Error = Error;
    fn try_from(raw: String) -> Result<StorePath> {
        StorePath::parse(&raw)
    }
}

impl TryFrom<&str> for StorePath {
    type Error = Error;
    fn try_from(raw: &str) -> Result<StorePath> {
        StorePath::parse(raw)
    }
}

impl From<StorePath> for String {
    fn from(path: StorePath) -> String {
        path.path
    }
}

impl std::str::FromStr for StorePath {
    type Err = Error;
    fn from_str(raw: &str) -> Result<StorePath> {
        StorePath::parse(raw)
    }
}

/// Something a route holds that names a store path: a parsed [`StorePath`]
/// (used as it is), or text that is parsed here — the boundary where a
/// caller's string meets the store. What the access helpers and
/// [`CheckedStore`](crate::CheckedStore) take, so a route need not spell the
/// parse; the [`FileStore`](crate::FileStore) trait itself takes only a
/// `&StorePath`.
pub trait AsStorePath {
    /// The parsed path, borrowed when it already is one.
    fn store_path(&self) -> Result<Cow<'_, StorePath>>;
}

impl AsStorePath for StorePath {
    fn store_path(&self) -> Result<Cow<'_, StorePath>> {
        Ok(Cow::Borrowed(self))
    }
}

impl AsStorePath for str {
    fn store_path(&self) -> Result<Cow<'_, StorePath>> {
        StorePath::parse(self).map(Cow::Owned)
    }
}

impl AsStorePath for String {
    fn store_path(&self) -> Result<Cow<'_, StorePath>> {
        StorePath::parse(self).map(Cow::Owned)
    }
}

impl<T: AsStorePath + ?Sized> AsStorePath for &T {
    fn store_path(&self) -> Result<Cow<'_, StorePath>> {
        (**self).store_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> String {
        StorePath::parse(raw).unwrap().to_string()
    }

    #[test]
    fn empty_segments_and_dots_are_normalised_away() {
        assert_eq!(parse("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(parse("/a//b/./c/"), "a/b/c");
        for root in ["", "/", ".", "./", "//"] {
            assert!(StorePath::parse(root).unwrap().is_root(), "{root:?}");
        }
        // A dot inside a name is a name.
        assert_eq!(parse("docs/.gitignore"), "docs/.gitignore");
        assert_eq!(parse("a.b/..c"), "a.b/..c");
    }

    /// Security review #13 (and the traversal the local backend always
    /// refused): every spelling that leaves the root, here or on Windows.
    #[test]
    fn traversal_and_windows_spellings_are_refused() {
        for bad in [
            "..",
            "../x",
            "a/../../etc",
            "a/..",
            "covers/..\\..\\Windows\\win.ini",
            "..\\x",
            "covers/C:\\x",
            "C:/x",
            "covers/a.png:stream",
            "covers/.. /x",
            "covers/.../x",
            "covers/. /x",
            "covers/a\0b",
        ] {
            let err = StorePath::parse(bad).unwrap_err().to_string();
            assert!(
                err.contains("escapes the file store root"),
                "{bad:?}: {err}"
            );
        }
    }

    /// Security review #12 / #31: `.git` in every spelling a file system
    /// opens as the same directory, at any depth.
    #[test]
    fn a_git_dir_is_recognised_in_every_spelling() {
        for git in [
            ".git",
            ".git/config",
            ".GIT/config",
            ".Git/hooks/pre-commit",
            ".git./config",
            ".git /config",
            "sub/.git/hooks/pre-commit",
            "git~1/config",
            "GIT~12/config",
        ] {
            assert!(StorePath::parse(git).unwrap().in_git_dir(), "{git:?}");
        }
        for plain in [
            "docs/.gitignore",
            "dot.git/x",
            ".github/workflows/ci.yml",
            "git~/x",
            "git~a/x",
            "git",
        ] {
            assert!(!StorePath::parse(plain).unwrap().in_git_dir(), "{plain:?}");
        }
        // The text form agrees, and still sees a backslash spelling.
        assert!(in_git_dir("a\\.git\\config"));
        assert!(!in_git_dir("dot.git/x"));
    }

    #[test]
    fn parents_joins_and_ancestors_stay_valid() {
        let p = StorePath::parse("a/b/c.txt").unwrap();
        assert_eq!(p.name(), "c.txt");
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        assert!(StorePath::root().parent().is_none());
        assert_eq!(
            StorePath::parse("x").unwrap().parent().unwrap(),
            StorePath::root()
        );
        let anc: Vec<String> = p.ancestors().iter().map(|a| a.to_string()).collect();
        assert_eq!(anc, ["a", "a/b", "a/b/c.txt"]);
        assert!(StorePath::root().ancestors().is_empty());

        let dir = StorePath::parse("uploads").unwrap();
        assert_eq!(dir.join("x/y.png").unwrap().as_str(), "uploads/x/y.png");
        assert!(dir.join("../etc").is_err());
        assert!(dir.join(".git/config").unwrap().in_git_dir());
        assert!(!dir.join("x").unwrap().in_git_dir());
        assert!(
            StorePath::parse(".git/x")
                .unwrap()
                .parent()
                .unwrap()
                .in_git_dir()
        );

        assert!(p.starts_with(&StorePath::parse("a").unwrap()));
        assert!(p.starts_with(&StorePath::root()));
        assert!(
            !StorePath::parse("ab/c")
                .unwrap()
                .starts_with(&StorePath::parse("a").unwrap())
        );
    }

    #[test]
    fn it_serialises_as_its_text_and_parses_on_the_way_in() {
        let p = StorePath::parse("/a//b").unwrap();
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"a/b\"");
        let back: StorePath = serde_json::from_str("\"a/b\"").unwrap();
        assert_eq!(back, p);
        assert!(serde_json::from_str::<StorePath>("\"../x\"").is_err());
    }

    #[test]
    #[should_panic(expected = "is not a store path")]
    fn a_bad_literal_is_a_bug() {
        let _ = StorePath::from_static("../x");
    }
}
