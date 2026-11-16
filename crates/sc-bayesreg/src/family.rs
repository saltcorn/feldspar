//! The families (TODO-bayesian-regression §3.6, R1.3): one table entry per
//! family, not code paths scattered through the generator.
//!
//! An entry holds the family's distributional parameters with their default
//! and allowed links, the response it accepts, the Stan density it is written
//! with, whether a CDF exists (for `cens` and `trunc`), the addition terms it
//! takes, and its parameters' default priors.
//!
//! Sources: the default links and the allowed link lists are brms's
//! documented ones (`?brmsfamily`, brms 2.23: "The first link mentioned for
//! each family is the default"; `Gamma` defaults to `log` in brms although
//! stats' `Gamma()` defaults to `inverse`). The allowed links of the
//! positive distributional parameters (`sigma`, `shape`, `phi`) are not listed
//! there; this table allows `log`, `identity` and `softplus`, and `nu` takes
//! `logm1` and `identity`. The densities are the Stan Functions Reference's.

use std::fmt;

use crate::prior::{ClassKind, DefaultPrior};

/// A link function (`?brmsfamily`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Link {
    /// `identity`.
    Identity,
    /// `log`.
    Log,
    /// `logit`.
    Logit,
    /// `probit`.
    Probit,
    /// `probit_approx`.
    ProbitApprox,
    /// `cloglog`.
    Cloglog,
    /// `cauchit`.
    Cauchit,
    /// `inverse`.
    Inverse,
    /// `sqrt`.
    Sqrt,
    /// `softplus`.
    Softplus,
    /// `logm1`: `log(x - 1)`, for ν > 1.
    Logm1,
}

impl Link {
    /// Every link, for parsing.
    pub const ALL: &'static [Link] = &[
        Link::Identity,
        Link::Log,
        Link::Logit,
        Link::Probit,
        Link::ProbitApprox,
        Link::Cloglog,
        Link::Cauchit,
        Link::Inverse,
        Link::Sqrt,
        Link::Softplus,
        Link::Logm1,
    ];

    /// Its name.
    pub fn name(self) -> &'static str {
        match self {
            Link::Identity => "identity",
            Link::Log => "log",
            Link::Logit => "logit",
            Link::Probit => "probit",
            Link::ProbitApprox => "probit_approx",
            Link::Cloglog => "cloglog",
            Link::Cauchit => "cauchit",
            Link::Inverse => "inverse",
            Link::Sqrt => "sqrt",
            Link::Softplus => "softplus",
            Link::Logm1 => "logm1",
        }
    }

    /// The link called `name`.
    pub fn parse(name: &str) -> Option<Link> {
        Link::ALL.iter().copied().find(|l| l.name() == name)
    }

    /// The link applied to a value of the response's scale, where it is
    /// defined and finite — what a data-dependent default prior is computed
    /// over (§3.7). `None` for the links whose value at the data says nothing
    /// useful about the parameter's scale (the binary links over 0/1 data).
    pub fn apply(self, y: f64) -> Option<f64> {
        let v = match self {
            Link::Identity => y,
            Link::Log => y.ln(),
            Link::Sqrt => y.sqrt(),
            Link::Inverse => 1.0 / y,
            Link::Softplus => y.exp_m1().ln(),
            Link::Logm1 => (y - 1.0).ln(),
            Link::Logit | Link::Probit | Link::ProbitApprox | Link::Cloglog | Link::Cauchit => {
                return None;
            }
        };
        v.is_finite().then_some(v)
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a family's response must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// Any real number.
    Real,
    /// Greater than zero.
    Positive,
    /// In (0, 1).
    Unit,
    /// 0 or 1 (a boolean column too).
    Binary,
    /// A whole number ≥ 0 (with `trials`, ≤ the trials).
    Count,
}

impl Response {
    /// What it is, in a sentence: "whole numbers of zero or more".
    pub fn describe(self) -> &'static str {
        match self {
            Response::Real => "numbers",
            Response::Positive => "numbers greater than zero",
            Response::Unit => "numbers strictly between 0 and 1",
            Response::Binary => "0 or 1 (or a yes/no column)",
            Response::Count => "whole numbers of zero or more",
        }
    }

    /// Whether `y` is one.
    pub fn accepts(self, y: f64) -> bool {
        match self {
            Response::Real => y.is_finite(),
            Response::Positive => y.is_finite() && y > 0.0,
            Response::Unit => y > 0.0 && y < 1.0,
            Response::Binary => y == 0.0 || y == 1.0,
            Response::Count => y >= 0.0 && y.fract() == 0.0 && y.is_finite(),
        }
    }

    /// Whether the column must hold integers (as the dataset's type, not just
    /// its values).
    pub fn integer(self) -> bool {
        matches!(self, Response::Binary | Response::Count)
    }
}

/// The response addition terms a family may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ATerms {
    /// `trials(n)`.
    pub trials: bool,
    /// `cens(…)` and `trunc(…)`: the family has a CDF in Stan.
    pub cdf: bool,
    /// `se(s)`.
    pub se: bool,
    /// `rate(d)`.
    pub rate: bool,
}

/// A distributional parameter other than `mu`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dpar {
    /// Its name.
    pub name: &'static str,
    /// Its links, the default first.
    pub links: &'static [Link],
    /// Its support, as a prior class.
    pub kind: ClassKind,
    /// Its default prior when it is a single parameter (no formula of its own).
    pub prior: DefaultPrior,
    /// What it is, for the editor.
    pub about: &'static str,
}

/// One family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Family {
    /// Its name as brms spells it.
    pub name: &'static str,
    /// Other spellings accepted.
    pub aliases: &'static [&'static str],
    /// `mu`'s links, the default first.
    pub mu_links: &'static [Link],
    /// The other distributional parameters, in the family's order.
    pub dpars: &'static [Dpar],
    /// What the response must be.
    pub response: Response,
    /// The Stan density it is written with (Stan Functions Reference),
    /// parameterised by the generator (R1.5): `normal`, `neg_binomial_2`.
    pub stan: &'static str,
    /// The addition terms it takes (`weights` goes with every family).
    pub aterms: ATerms,
}

impl Family {
    /// The distributional parameter called `name`.
    pub fn dpar(&self, name: &str) -> Option<&'static Dpar> {
        self.dpars.iter().find(|d| d.name == name)
    }

    /// The default link of `dpar` (`mu` included).
    pub fn default_link(&self, dpar: &str) -> Option<Link> {
        self.links(dpar).and_then(|l| l.first().copied())
    }

    /// The links `dpar` allows (`mu` included).
    pub fn links(&self, dpar: &str) -> Option<&'static [Link]> {
        if dpar == "mu" {
            Some(self.mu_links)
        } else {
            self.dpar(dpar).map(|d| d.links)
        }
    }

    /// Its distributional parameters' names, `mu` first.
    pub fn dpar_names(&self) -> Vec<&'static str> {
        std::iter::once("mu")
            .chain(self.dpars.iter().map(|d| d.name))
            .collect()
    }
}

const IDENTITY_LINKS: &[Link] = &[Link::Identity, Link::Log, Link::Inverse, Link::Softplus];
const LOGNORMAL_LINKS: &[Link] = &[Link::Identity, Link::Inverse];
const BINARY_LINKS: &[Link] = &[
    Link::Logit,
    Link::Probit,
    Link::ProbitApprox,
    Link::Cloglog,
    Link::Cauchit,
    Link::Identity,
    Link::Log,
];
const COUNT_LINKS: &[Link] = &[Link::Log, Link::Identity, Link::Sqrt, Link::Softplus];
const POSITIVE_LINKS: &[Link] = &[Link::Log, Link::Identity, Link::Inverse, Link::Softplus];
const DPAR_POSITIVE_LINKS: &[Link] = &[Link::Log, Link::Identity, Link::Softplus];

const SIGMA: Dpar = Dpar {
    name: "sigma",
    links: DPAR_POSITIVE_LINKS,
    kind: ClassKind::Positive,
    prior: DefaultPrior::ResponseScale,
    about: "the residual standard deviation",
};
const NU: Dpar = Dpar {
    name: "nu",
    links: &[Link::Logm1, Link::Identity],
    kind: ClassKind::AboveOne,
    prior: DefaultPrior::Fixed("gamma", &[2.0, 0.1]),
    about: "the degrees of freedom",
};
const ALPHA: Dpar = Dpar {
    name: "alpha",
    links: &[Link::Identity],
    kind: ClassKind::Real,
    prior: DefaultPrior::Fixed("normal", &[0.0, 4.0]),
    about: "the skewness",
};
const SHAPE: Dpar = Dpar {
    name: "shape",
    links: DPAR_POSITIVE_LINKS,
    kind: ClassKind::Positive,
    prior: DefaultPrior::Fixed("gamma", &[0.01, 0.01]),
    about: "the shape",
};
const NB_SHAPE: Dpar = Dpar {
    prior: DefaultPrior::Fixed("inv_gamma", &[0.4, 0.3]),
    about: "the shape (overdispersion: the variance is μ + μ²/shape)",
    ..SHAPE
};
const PHI: Dpar = Dpar {
    name: "phi",
    links: DPAR_POSITIVE_LINKS,
    kind: ClassKind::Positive,
    prior: DefaultPrior::Fixed("gamma", &[0.01, 0.01]),
    about: "the precision",
};

const CONTINUOUS: ATerms = ATerms {
    trials: false,
    cdf: true,
    se: false,
    rate: false,
};

/// The R1 families.
pub static FAMILIES: &[Family] = &[
    Family {
        name: "gaussian",
        aliases: &["normal"],
        mu_links: IDENTITY_LINKS,
        dpars: &[SIGMA],
        response: Response::Real,
        stan: "normal",
        aterms: ATerms {
            se: true,
            ..CONTINUOUS
        },
    },
    Family {
        name: "student",
        aliases: &["student_t"],
        mu_links: IDENTITY_LINKS,
        dpars: &[SIGMA, NU],
        response: Response::Real,
        stan: "student_t",
        aterms: ATerms {
            se: true,
            ..CONTINUOUS
        },
    },
    Family {
        name: "lognormal",
        aliases: &[],
        mu_links: LOGNORMAL_LINKS,
        dpars: &[SIGMA],
        response: Response::Positive,
        stan: "lognormal",
        aterms: CONTINUOUS,
    },
    Family {
        name: "skew_normal",
        aliases: &[],
        mu_links: IDENTITY_LINKS,
        dpars: &[SIGMA, ALPHA],
        response: Response::Real,
        stan: "skew_normal",
        aterms: CONTINUOUS,
    },
    Family {
        name: "bernoulli",
        aliases: &[],
        mu_links: BINARY_LINKS,
        dpars: &[],
        response: Response::Binary,
        stan: "bernoulli",
        aterms: ATerms {
            cdf: false,
            ..CONTINUOUS
        },
    },
    Family {
        name: "binomial",
        aliases: &[],
        mu_links: BINARY_LINKS,
        dpars: &[],
        response: Response::Count,
        stan: "binomial",
        aterms: ATerms {
            trials: true,
            cdf: false,
            ..CONTINUOUS
        },
    },
    Family {
        name: "poisson",
        aliases: &[],
        mu_links: COUNT_LINKS,
        dpars: &[],
        response: Response::Count,
        stan: "poisson",
        aterms: ATerms {
            rate: true,
            ..CONTINUOUS
        },
    },
    Family {
        name: "negbinomial",
        aliases: &["negative_binomial", "neg_binomial"],
        mu_links: COUNT_LINKS,
        dpars: &[NB_SHAPE],
        response: Response::Count,
        stan: "neg_binomial_2",
        aterms: ATerms {
            rate: true,
            ..CONTINUOUS
        },
    },
    Family {
        name: "geometric",
        aliases: &[],
        mu_links: COUNT_LINKS,
        dpars: &[],
        response: Response::Count,
        stan: "neg_binomial_2",
        aterms: ATerms {
            rate: true,
            ..CONTINUOUS
        },
    },
    Family {
        name: "Gamma",
        aliases: &["gamma"],
        mu_links: POSITIVE_LINKS,
        dpars: &[SHAPE],
        response: Response::Positive,
        stan: "gamma",
        aterms: CONTINUOUS,
    },
    Family {
        name: "weibull",
        aliases: &[],
        mu_links: POSITIVE_LINKS,
        dpars: &[SHAPE],
        response: Response::Positive,
        stan: "weibull",
        aterms: CONTINUOUS,
    },
    Family {
        name: "exponential",
        aliases: &[],
        mu_links: POSITIVE_LINKS,
        dpars: &[],
        response: Response::Positive,
        stan: "exponential",
        aterms: CONTINUOUS,
    },
    Family {
        name: "Beta",
        aliases: &["beta"],
        mu_links: BINARY_LINKS,
        dpars: &[PHI],
        response: Response::Unit,
        stan: "beta",
        aterms: CONTINUOUS,
    },
];

/// The families a later release adds, so naming one says when rather than
/// "unknown".
const LATER: &[&str] = &[
    "zero_inflated_poisson",
    "zero_inflated_negbinomial",
    "hurdle_poisson",
    "hurdle_negbinomial",
    "hurdle_gamma",
    "hurdle_lognormal",
    "zero_inflated_beta",
    "zero_one_inflated_beta",
    "beta_binomial",
    "cumulative",
    "sratio",
    "cratio",
    "acat",
    "categorical",
    "von_mises",
    "exgaussian",
    "asym_laplace",
];

/// The family called `name` (or one of its aliases), or a sentence saying why
/// there is none.
pub fn family(name: &str) -> Result<&'static Family, String> {
    let name = name.trim();
    if let Some(f) = FAMILIES
        .iter()
        .find(|f| f.name == name || f.aliases.contains(&name))
    {
        return Ok(f);
    }
    if LATER.contains(&name) {
        return Err(format!(
            "the family `{name}` arrives in a later release (R2)"
        ));
    }
    Err(format!(
        "there is no family `{name}`; the families are {}",
        FAMILIES
            .iter()
            .map(|f| format!("`{}`", f.name))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_links_are_brmss() {
        let link = |f: &str, d: &str| family(f).unwrap().default_link(d).unwrap().name();
        assert_eq!(link("gaussian", "mu"), "identity");
        assert_eq!(link("gaussian", "sigma"), "log");
        assert_eq!(link("student", "nu"), "logm1");
        assert_eq!(link("bernoulli", "mu"), "logit");
        assert_eq!(link("poisson", "mu"), "log");
        assert_eq!(link("Gamma", "mu"), "log");
        assert_eq!(link("gamma", "shape"), "log");
        assert_eq!(link("Beta", "phi"), "log");
        assert_eq!(link("Beta", "mu"), "logit");
    }

    #[test]
    fn an_unknown_family_lists_the_known_and_a_later_one_says_when() {
        assert!(family("gausian").unwrap_err().contains("`gaussian`"));
        assert!(family("cumulative").unwrap_err().contains("later release"));
    }
}
