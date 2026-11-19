//! R1.5: the canonicaliser and the generator.
//!
//! - §5's acceptance cases: structurally equal models are byte-identical
//!   programs with one compile-cache key, and the must-differ cases differ.
//! - Golden files of the program for every R1 family × {no blocks, one
//!   correlated block, one uncorrelated, two blocks sharing an `ID` across
//!   `mu` and a distributional parameter} × {no addition terms, `weights`,
//!   `cens`, `trunc`} — every combination the family allows (the families
//!   without a distributional parameter have no shared-`ID` case, and those
//!   without a CDF none with `cens` or `trunc`). `UPDATE_GOLDEN=1` rewrites
//!   them; any other change to the text fails here, so a silent change of what
//!   a cached executable means cannot be merged (§5 rule 5).
//! - Each golden program through the real `stanc` when a CmdStan is found
//!   (skipped with a notice when none is).

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: runs the real stanc on the golden programs"
)]

use std::path::{Path, PathBuf};

use sc_bayesreg::{Data, FAMILIES, build, program_of};
use sc_stan::cmdstan::{CmdStan, Locations, Source, discover};
use sc_stan::compile::CompileCache;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

pub(crate) fn config(formula: &str, family: &str) -> Attrs {
    let mut c = Attrs::new();
    c.insert("formula".into(), Json::from(formula));
    c.insert("family".into(), Json::from(family));
    c
}

pub(crate) fn program(formula: &str, family: &str) -> String {
    build(&config(formula, family), Data::Nothing)
        .unwrap_or_else(|e| panic!("{formula} ({family}): {e}"))
        .program
}

/// A CmdStan value for computing cache keys — the key reads only its version.
fn any_cmdstan() -> CmdStan {
    CmdStan {
        dir: PathBuf::from("/nonexistent"),
        version: "2.40.0".parse().unwrap(),
        source: Source::Env,
    }
}

fn key(formula: &str, family: &str) -> String {
    CompileCache::key(
        &program_of(&program(formula, family)).unwrap(),
        &any_cmdstan(),
    )
}

#[test]
fn structurally_equal_models_are_one_program_and_one_cache_key() {
    let same = [
        "y ~ x + (1 | g)",
        "price ~ area + (1 | county)",
        "y ~ a + b + c + (1 + a | g)",
        "log_radon ~ floor + region + floor:region + (1 + floor | county)",
    ];
    let first = program(same[0], "gaussian");
    for f in &same[1..] {
        assert_eq!(program(f, "gaussian"), first, "{f}");
        assert_eq!(key(f, "gaussian"), key(same[0], "gaussian"), "{f}");
    }
    // The order blocks are written in does not matter.
    assert_eq!(
        program("y ~ x + (1|h) + (1|g)", "gaussian"),
        program("y ~ (1|g) + x + (1|h)", "gaussian")
    );
    // Nor whether the intercept is written.
    assert_eq!(
        program("y ~ 1 + x", "gaussian"),
        program("y ~ x", "gaussian")
    );
    // Nor a transform (it is evaluated in Rust, not in Stan).
    assert_eq!(
        program("y ~ log(x) + poly(z, 2)", "gaussian"),
        program("y ~ x", "gaussian")
    );
    // No column name reaches the program.
    let text = program("log_radon ~ floor + (1 + floor | county)", "gaussian");
    for name in ["log_radon", "floor", "county"] {
        assert!(!text.contains(name), "`{name}` is in the program:\n{text}");
    }
}

#[test]
fn a_different_structure_is_a_different_program() {
    let base = key("y ~ x + (1 | g)", "gaussian");
    let different = [
        ("y ~ x + (1 | g)", "student"),
        ("y ~ x + (1 || g)", "gaussian"),
        ("y ~ x + (1 | g) + (1 | h)", "gaussian"),
        ("y ~ x", "gaussian"),
        ("y ~ 0 + x + (1 | g)", "gaussian"),
        ("y | weights(w) ~ x + (1 | g)", "gaussian"),
        ("y ~ x + (1 | g) + offset(o)", "gaussian"),
    ];
    for (f, fam) in different {
        assert_ne!(key(f, fam), base, "{f} ({fam})");
    }
    let with_sigma = program("y ~ x + (1 | g)\nsigma ~ x", "gaussian");
    assert_ne!(with_sigma, program("y ~ x + (1 | g)", "gaussian"));
    // A link is structural; so is a bound on `b` (§2.5), and `constant()`.
    assert_ne!(program("y ~ x", "bernoulli"), {
        let mut c = config("y ~ x", "bernoulli");
        c.insert("links".into(), json!({"mu": "probit"}));
        build(&c, Data::Nothing).unwrap().program
    });
    let bounded = {
        let mut c = config("y ~ x", "gaussian");
        c.insert(
            "priors".into(),
            json!([{"class": "b", "coef": "x", "prior": "normal(0, 1)", "lb": 0}]),
        );
        build(&c, Data::Nothing).unwrap().program
    };
    assert_ne!(bounded, program("y ~ x", "gaussian"));
    // A prior's numbers are not: they are data.
    let other_prior = {
        let mut c = config("y ~ x", "gaussian");
        c.insert(
            "priors".into(),
            json!([{"class": "b", "prior": "student_t(3, 0, 2.5)"}, {"class": "sigma", "prior": "exponential(1)"}]),
        );
        build(&c, Data::Nothing).unwrap().program
    };
    assert_eq!(other_prior, program("y ~ x", "gaussian"));
}

/// The block variants of the golden matrix.
const BLOCKS: [(&str, &str); 4] = [
    ("none", ""),
    ("correlated", " + (1 + x | g)"),
    ("uncorrelated", " + (1 + x || g)"),
    ("shared_id", " + (1 | p | g)"),
];

/// The addition-term variants.
const ATERMS: [&str; 4] = ["none", "weights", "cens", "trunc"];

/// Every (name, formula, family) of the golden matrix.
fn golden_cases() -> Vec<(String, String, &'static str)> {
    let mut out = Vec::new();
    for fam in FAMILIES {
        for (block, rhs) in BLOCKS {
            let dpar = fam.dpars.first().map(|d| d.name);
            if block == "shared_id" && dpar.is_none() {
                continue;
            }
            for aterm in ATERMS {
                if (aterm == "cens" || aterm == "trunc") && !fam.aterms.cdf {
                    continue;
                }
                let mut aterms: Vec<&str> = Vec::new();
                if fam.aterms.trials {
                    aterms.push("trials(n)");
                }
                match aterm {
                    "weights" => aterms.push("weights(w)"),
                    "cens" => aterms.push("cens(c, y2)"),
                    "trunc" => aterms.push("trunc(lb = 0, ub = u)"),
                    _ => {}
                }
                let lhs = if aterms.is_empty() {
                    "y".to_owned()
                } else {
                    format!("y | {}", aterms.join(" + "))
                };
                let mut formula = format!("{lhs} ~ x{rhs}");
                if let (true, Some(d)) = (block == "shared_id", dpar) {
                    formula.push_str(&format!("\n{d} ~ x + (1 | p | g)"));
                }
                out.push((
                    format!("{}__{block}__{aterm}", fam.name.to_lowercase()),
                    formula,
                    fam.name,
                ));
            }
        }
    }
    out
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

#[test]
fn every_golden_program_is_the_generators_output() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let dir = golden_dir();
    if update {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let cases = golden_cases();
    let mut stale = Vec::new();
    for (name, formula, family) in &cases {
        let text = format!(
            "// {family}: {}\n{}",
            formula.replace('\n', " | "),
            program(formula, family)
        );
        let path = dir.join(format!("{name}.stan"));
        if update {
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(golden) if golden == text => {}
            _ => stale.push(name.clone()),
        }
    }
    assert!(
        stale.is_empty(),
        "{} of {} golden programs differ from the generator's output: {}\n(if the change is \
         intended, bump GENERATOR_VERSION and rerun with UPDATE_GOLDEN=1)",
        stale.len(),
        cases.len(),
        stale.join(", ")
    );
    // Nothing stale left behind either.
    let on_disk = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(
        on_disk,
        cases.len(),
        "golden files without a case in {}",
        dir.display()
    );
}

#[test]
fn every_golden_program_passes_stanc() {
    let Ok(cmdstan) = discover(&Locations::from_env(None)) else {
        eprintln!("no CmdStan found: the golden programs were not checked with stanc");
        return;
    };
    let stanc = cmdstan.stanc();
    if !stanc.is_file() {
        eprintln!(
            "{} is not built: the golden programs were not checked",
            stanc.display()
        );
        return;
    }
    let cases = golden_cases();
    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = cases
            .chunks(cases.len().div_ceil(8))
            .map(|chunk| {
                let stanc = &stanc;
                scope.spawn(move || {
                    let mut bad = Vec::new();
                    for (name, formula, family) in chunk {
                        let path = std::env::temp_dir()
                            .join(format!("sc-bayesreg-{}-{name}.stan", std::process::id()));
                        std::fs::write(&path, program(formula, family)).unwrap();
                        let out = std::process::Command::new(stanc)
                            .arg(&path)
                            .arg("--o")
                            .arg(path.with_extension("hpp"))
                            .output()
                            .unwrap();
                        let _ = std::fs::remove_file(&path);
                        let _ = std::fs::remove_file(path.with_extension("hpp"));
                        if !out.status.success() {
                            bad.push(format!("{name}: {}", String::from_utf8_lossy(&out.stderr)));
                        }
                    }
                    bad
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(
        failures.is_empty(),
        "stanc refused:\n{}",
        failures.join("\n")
    );
}

#[test]
fn the_program_says_what_it_is() {
    let text = program("y ~ x + (1 | g)", "gaussian");
    let header: Vec<&str> = text.lines().take(4).collect();
    assert!(header[0].contains("generator 1"), "{text}");
    assert!(header[3].starts_with("// structure: {\"generator\":1,\"family\":\"gaussian\""));
}
