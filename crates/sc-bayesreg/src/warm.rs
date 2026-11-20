//! The commonest structures, for warming the compile cache (§5, R2.5):
//! `feldspar stan warm` compiles each, so a fresh server's first fits do not
//! wait a minute for C++. Every R1 family × no group-level terms / one
//! correlated block / one uncorrelated block — each one program, whatever the
//! column names and counts of the models that will use it (§5 rule 2).

use crate::lower::build;
use crate::resolve::Data;
use crate::{FAMILIES, Family};

/// The R1 families: what the warm list covers.
const R1: &[&str] = &[
    "gaussian",
    "student",
    "lognormal",
    "skew_normal",
    "bernoulli",
    "binomial",
    "poisson",
    "negbinomial",
    "geometric",
    "Gamma",
    "weibull",
    "exponential",
    "Beta",
];

/// The structures to warm, as `(a label, the program)`, for the families
/// named in `only` (all the R1 families when it is empty).
pub fn programs(only: &[String]) -> Result<Vec<(String, String)>, String> {
    let families: Vec<&'static Family> = if only.is_empty() {
        FAMILIES.iter().filter(|f| R1.contains(&f.name)).collect()
    } else {
        only.iter()
            .map(|n| crate::family::family(n))
            .collect::<Result<_, _>>()?
    };
    let mut out = Vec::new();
    for f in families {
        let lhs = if f.aterms.trials {
            "y | trials(n)"
        } else {
            "y"
        };
        for (what, rhs) in [
            ("no group-level terms", ""),
            ("one correlated block", " + (1 + x | g)"),
            ("one uncorrelated block", " + (1 + x || g)"),
        ] {
            let mut config = sc_types::Attrs::new();
            config.insert("formula".into(), format!("{lhs} ~ x{rhs}").into());
            config.insert("family".into(), f.name.into());
            let program = build(&config, Data::Nothing)
                .map_err(|e| format!("{} ({what}): {e}", f.name))?
                .program;
            out.push((format!("{}, {what}", f.name), program));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warm_list_is_every_r1_family_three_ways_and_each_is_a_distinct_program() {
        let all = programs(&[]).unwrap();
        assert_eq!(all.len(), 13 * 3);
        let mut texts: Vec<&String> = all.iter().map(|(_, p)| p).collect();
        texts.sort();
        texts.dedup();
        assert_eq!(texts.len(), all.len());
        assert_eq!(programs(&["poisson".into()]).unwrap().len(), 3);
        assert!(programs(&["nope".into()]).is_err());
    }
}
