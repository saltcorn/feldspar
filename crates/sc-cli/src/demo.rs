//! `feldspar demo analytics [--replace]` (analytics TODO A1.18): the argument
//! parsing. The tables and their rows are `sc_analytics::demo`'s, so that a
//! server test can make the same ones.

use sc_error::{Error, Result};

/// What `feldspar demo` was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoArgs {
    /// Which demo — `analytics` is the only one.
    pub which: String,
    /// Drop and remake tables that are already there.
    pub replace: bool,
}

impl DemoArgs {
    /// Parse the arguments after `feldspar demo`, after the database flags
    /// were taken out.
    pub fn parse(args: &[String]) -> Result<DemoArgs> {
        let mut which = None;
        let mut replace = false;
        for arg in args {
            match arg.as_str() {
                "--replace" => replace = true,
                flag if flag.starts_with("--") => {
                    return Err(Error::config(format!(
                        "`feldspar demo` has no flag `{flag}`; it takes `--replace`"
                    )));
                }
                name if which.is_none() => which = Some(name.to_owned()),
                extra => {
                    return Err(Error::config(format!(
                        "`feldspar demo` takes one demo's name, and was also given `{extra}`"
                    )));
                }
            }
        }
        match which.as_deref() {
            Some("analytics") => Ok(DemoArgs {
                which: "analytics".to_owned(),
                replace,
            }),
            Some(other) => Err(Error::config(format!(
                "there is no demo called `{other}`; the demos are: analytics"
            ))),
            None => Err(Error::config(
                "`feldspar demo` needs the demo's name: `feldspar demo analytics`",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn the_arguments_name_the_demo_and_say_whether_to_replace() {
        assert_eq!(
            DemoArgs::parse(&args(&["analytics", "--replace"])).expect("ok"),
            DemoArgs {
                which: "analytics".into(),
                replace: true
            }
        );
        assert!(!DemoArgs::parse(&args(&["analytics"])).expect("ok").replace);
        let err = DemoArgs::parse(&args(&["houses"])).expect_err("no such demo");
        assert!(err.to_string().contains("demos are: analytics"), "{err}");
        assert!(DemoArgs::parse(&args(&[])).is_err());
        assert!(DemoArgs::parse(&args(&["analytics", "--force"])).is_err());
    }
}
