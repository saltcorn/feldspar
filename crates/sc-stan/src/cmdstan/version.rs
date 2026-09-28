//! A CmdStan version, and where one is read from.

use std::cmp::Ordering;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use sc_error::{Error, Result};

/// The oldest CmdStan this system drives.
///
/// 2.33 is the release that removed the old array syntax, so it is the first
/// whose programs are all written in the syntax our declaration parser speaks
/// (TODO §5); it also has Pathfinder (§13).
pub const MIN_VERSION: Version = Version {
    major: 2,
    minor: 33,
    patch: 0,
    pre: None,
};

/// `major.minor.patch`, with an optional pre-release suffix (`2.36.0-rc1`).
///
/// A pre-release orders **before** the release it precedes, so the newest of
/// `2.36.0-rc1` and `2.36.0` is the release.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub pre: Option<String>,
}

impl Version {
    /// The version of the CmdStan in `dir`, from its top-level `makefile`
    /// (`CMDSTAN_VERSION := 2.36.0`) — the line cmdstanpy reads too.
    ///
    /// `None` when `dir` has no makefile or the makefile names no version:
    /// that is "not a CmdStan directory", which the caller words for its own
    /// context.
    pub fn of_dir(dir: &Path) -> Option<Version> {
        let makefile = std::fs::read_to_string(dir.join("makefile")).ok()?;
        makefile.lines().find_map(|line| {
            let rest = line.trim().strip_prefix("CMDSTAN_VERSION")?;
            let value = rest.trim_start().strip_prefix(":=")?;
            value.trim().parse().ok()
        })
    }

    /// Whether this version is one [`MIN_VERSION`] allows.
    pub fn supported(&self) -> bool {
        *self >= MIN_VERSION
    }
}

impl FromStr for Version {
    type Err = Error;

    /// `2.36.0`, `v2.36.0` (a git tag) or `2.36.0-rc1`. A missing patch is
    /// zero, so `--version 2.36` means 2.36.0.
    fn from_str(s: &str) -> Result<Self> {
        let bad = || {
            Error::invalid(format!(
                "`{s}` is not a CmdStan version (expected e.g. 2.36.0)"
            ))
        };
        let text = s.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (numbers, pre) = match text.split_once('-') {
            Some((numbers, pre)) if !pre.is_empty() => (numbers, Some(pre.to_owned())),
            Some(_) => return Err(bad()),
            None => (text, None),
        };
        let mut parts = numbers.split('.');
        let mut next = |required: bool| -> Result<u32> {
            match parts.next() {
                Some(part) => part.parse().map_err(|_| bad()),
                None if required => Err(bad()),
                None => Ok(0),
            }
        };
        let (major, minor, patch) = (next(true)?, next(true)?, next(false)?);
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(Version {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parses_releases_tags_and_release_candidates() {
        assert_eq!(
            v("2.36.0"),
            Version {
                major: 2,
                minor: 36,
                patch: 0,
                pre: None
            }
        );
        assert_eq!(v("v2.40.1"), v("2.40.1"));
        assert_eq!(v("2.36"), v("2.36.0"));
        assert_eq!(v("2.36.0-rc1").pre.as_deref(), Some("rc1"));
        assert_eq!(v("2.36.0-rc1").to_string(), "2.36.0-rc1");
        for bad in ["", "2", "two.36", "2.36.0.1", "2.36.0-", "2..0"] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn orders_numerically_with_a_release_candidate_before_its_release() {
        assert!(v("2.9.0") < v("2.10.0"));
        assert!(v("2.36.0-rc1") < v("2.36.0"));
        assert!(v("2.36.0") < v("2.36.1-rc1"));
        assert!(v("2.32.2") < MIN_VERSION);
        assert!(!v("2.32.2").supported());
        assert!(v("2.33.0").supported());
        assert!(!v("2.33.0-rc1").supported());
    }

    #[test]
    fn reads_the_version_line_from_the_makefile() {
        let dir = std::env::temp_dir().join(format!("sc-stan-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Version::of_dir(&dir), None);
        std::fs::write(
            dir.join("makefile"),
            "## comment\nCMDSTAN_VERSION := 2.37.0\nSTAN ?= stan/\n",
        )
        .unwrap();
        assert_eq!(Version::of_dir(&dir), Some(v("2.37.0")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
