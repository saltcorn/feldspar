//! Converting a formula model to an ordinary Stan model (TODO-bayesian-regression
//! §9, R2.5): the escape hatch for anything a formula cannot say, and the
//! reason this provider never accepts raw Stan.
//!
//! The lowered configuration already *is* a Stan model's: the generated
//! program, and bindings, dimensions, policies and derived columns the host
//! binds with. Converting keeps those, the sampler's settings and the outputs'
//! settings, points `program_store`/`program` at where the program is
//! written, and drops what only this provider reads. Lowered against the rows
//! (as a fit lowers), its prior data are the numbers the fit would use, so the
//! Stan model fits to the same posterior; from there it is edited freely.

use sc_error::{Error, Result};
use sc_stan::config_keys::{PROGRAM, PROGRAM_STORE};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::lower::keys;

/// The keys only this provider reads, which a Stan model has no use for.
const OWN_KEYS: &[&str] = &[
    keys::FORMULA,
    keys::FAMILY,
    keys::LINKS,
    keys::PRIORS,
    keys::CENTER,
    keys::GENERATED_PROGRAM,
    keys::STRUCTURE,
    keys::LAYOUT,
    keys::RESOLVED_PRIORS,
    keys::EPRED,
];

/// A converted model: the program's text and the Stan model's configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Converted {
    /// The program, to be written at `path` in `store`.
    pub program: String,
    /// The `stan` provider's configuration.
    pub config: Attrs,
}

/// Convert a `lowered` configuration (best lowered against the rows, so its
/// prior data are numbers) into a Stan model reading its program from `path`
/// in the file store `store`.
pub fn convert(lowered: &Attrs, store: &str, path: &str) -> Result<Converted> {
    let program = lowered
        .get(keys::GENERATED_PROGRAM)
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid("the configuration has not been lowered: it has no program"))?
        .to_owned();
    let path = path.trim().trim_start_matches('/');
    if path.is_empty() || path.split(['/', '\\']).any(|s| s == "..") {
        return Err(Error::invalid(format!(
            "`{path}` must be a path inside the file store, with no `..`"
        )));
    }
    if !path.ends_with(".stan") {
        return Err(Error::invalid(format!("`{path}` must end in `.stan`")));
    }
    if store.trim().is_empty() {
        return Err(Error::invalid(
            "name the file store to write the program to",
        ));
    }
    let mut config = lowered.clone();
    for k in OWN_KEYS {
        config.remove(*k);
    }
    config.insert(PROGRAM_STORE.to_owned(), Json::from(store.trim()));
    config.insert(PROGRAM.to_owned(), Json::from(path));
    Ok(Converted { program, config })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stan_model_keeps_the_bindings_and_names_its_program() {
        let mut lowered = Attrs::new();
        lowered.insert(keys::GENERATED_PROGRAM.into(), Json::from("data { }"));
        lowered.insert(keys::FORMULA.into(), Json::from("y ~ x"));
        lowered.insert(
            "bindings".into(),
            serde_json::json!({"N": {"kind": "count"}}),
        );
        lowered.insert("seed".into(), Json::from(4711));
        let c = convert(&lowered, "models", "radon/model.stan").unwrap();
        assert_eq!(c.program, "data { }");
        assert_eq!(c.config["program_store"], "models");
        assert_eq!(c.config["program"], "radon/model.stan");
        assert_eq!(c.config["seed"], 4711);
        assert!(c.config.contains_key("bindings"));
        assert!(!c.config.contains_key(keys::FORMULA));
        assert!(convert(&lowered, "models", "../x.stan").is_err());
        assert!(convert(&lowered, "models", "x.txt").is_err());
    }
}
