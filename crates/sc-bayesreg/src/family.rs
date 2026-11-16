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
//!
//! The R2 families (R2.1) follow the same documentation, with their
//! distributional parameters' meanings from `vignette("brms_families")`: a
//! zero-inflated family mixes a point mass at zero (`zi`) with the base count
//! density, a hurdle family (`hu`) puts all of the zeros in the point mass
//! and truncates the base density above zero, the zero-one-inflated beta has
//! `zoi` (a zero or a one) and `coi` (a one, given that); the ordinal
//! families' four models are Bürkner & Vuorre (2019), *Advances in Methods and
//! Practices in Psychological Science* 2(1), with `disc` fixed at 1 unless it
//! has a formula. The defaults of the R2 dpars' priors are `beta(1, 1)` on a
//! probability, `gamma(2, 0.01)` on von Mises's `kappa` and `gamma(1, 0.1)` on
//! the exponential part `beta` of the ex-Gaussian — this implementation's own
//! choices, stated here and shown on every fit.

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
    /// `tan_half`: `tan(x / 2)`, for an angle (von Mises).
    TanHalf,
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
        Link::TanHalf,
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
            Link::TanHalf => "tan_half",
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
            Link::TanHalf => (y / 2.0).tan(),
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
    /// Zero or more (a hurdle on a positive family).
    NonNegative,
    /// In [0, 1) (zero-inflated beta).
    UnitZero,
    /// In [0, 1] (zero-one-inflated beta).
    UnitClosed,
    /// An ordered category: a whole number from 1.
    Ordinal,
    /// A category of any kind: text, a whole number or yes/no, each distinct
    /// value one category (the first in sorted order the reference).
    Category,
    /// An angle in radians, between −π and π.
    Angle,
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
            Response::NonNegative => "numbers of zero or more",
            Response::UnitZero => "numbers from 0 up to (not including) 1",
            Response::UnitClosed => "numbers from 0 to 1",
            Response::Ordinal => "ordered categories numbered 1, 2, 3, …",
            Response::Category => "categories (text, whole numbers or yes/no)",
            Response::Angle => "angles in radians, between −π and π",
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
            Response::NonNegative => y.is_finite() && y >= 0.0,
            Response::UnitZero => (0.0..1.0).contains(&y),
            Response::UnitClosed => (0.0..=1.0).contains(&y),
            Response::Ordinal => y >= 1.0 && y.fract() == 0.0 && y.is_finite(),
            Response::Category => true,
            Response::Angle => (-std::f64::consts::PI..=std::f64::consts::PI).contains(&y),
        }
    }

    /// Whether the column must hold integers (as the dataset's type, not just
    /// its values).
    pub fn integer(self) -> bool {
        matches!(
            self,
            Response::Binary | Response::Count | Response::Ordinal | Response::Category
        )
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
    /// `thres(k)`: the ordinal families.
    pub thres: bool,
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
    /// When it has no formula: fixed at this value rather than estimated
    /// (an ordinal family's `disc`, brms's documented default 1).
    pub fixed: Option<f64>,
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
    /// What kind of model it makes.
    pub kind: Kind,
}

/// What kind of model a family makes — the three shapes the generator writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One linear predictor per distributional parameter, one density.
    Standard,
    /// An ordinal response: thresholds instead of an intercept, and
    /// optionally category-specific effects (`cs()`).
    Ordinal,
    /// An unordered category: a linear predictor per category but the first.
    Categorical,
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
    fixed: None,
};
const NU: Dpar = Dpar {
    name: "nu",
    links: &[Link::Logm1, Link::Identity],
    kind: ClassKind::AboveOne,
    prior: DefaultPrior::Fixed("gamma", &[2.0, 0.1]),
    about: "the degrees of freedom",
    fixed: None,
};
const ALPHA: Dpar = Dpar {
    name: "alpha",
    links: &[Link::Identity],
    kind: ClassKind::Real,
    prior: DefaultPrior::Fixed("normal", &[0.0, 4.0]),
    about: "the skewness",
    fixed: None,
};
const SHAPE: Dpar = Dpar {
    name: "shape",
    links: DPAR_POSITIVE_LINKS,
    kind: ClassKind::Positive,
    prior: DefaultPrior::Fixed("gamma", &[0.01, 0.01]),
    about: "the shape",
    fixed: None,
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
    fixed: None,
};

const UNIT: Dpar = Dpar {
    name: "zi",
    links: &[Link::Logit, Link::Identity],
    kind: ClassKind::Unit,
    prior: DefaultPrior::Fixed("beta", &[1.0, 1.0]),
    about: "the probability of an extra zero",
    fixed: None,
};
const ZI: Dpar = UNIT;
const HU: Dpar = Dpar {
    name: "hu",
    about: "the probability of a zero (the hurdle)",
    ..UNIT
};
const ZOI: Dpar = Dpar {
    name: "zoi",
    about: "the probability of a zero or a one",
    ..UNIT
};
const COI: Dpar = Dpar {
    name: "coi",
    about: "the probability of a one, given a zero or a one",
    ..UNIT
};
const QUANTILE: Dpar = Dpar {
    name: "quantile",
    about: "the quantile the location is",
    ..UNIT
};
const KAPPA: Dpar = Dpar {
    name: "kappa",
    links: DPAR_POSITIVE_LINKS,
    kind: ClassKind::Positive,
    prior: DefaultPrior::Fixed("gamma", &[2.0, 0.01]),
    about: "the concentration",
    fixed: None,
};
const EX_BETA: Dpar = Dpar {
    name: "beta",
    links: DPAR_POSITIVE_LINKS,
    kind: ClassKind::Positive,
    prior: DefaultPrior::Fixed("gamma", &[1.0, 0.1]),
    about: "the scale of the exponential part",
    fixed: None,
};
const DISC: Dpar = Dpar {
    name: "disc",
    links: &[Link::Log, Link::Identity],
    kind: ClassKind::Positive,
    prior: DefaultPrior::Fixed("lognormal", &[0.0, 1.0]),
    about: "the discrimination (fixed at 1 unless it has a formula)",
    fixed: Some(1.0),
};

const CONTINUOUS: ATerms = ATerms {
    trials: false,
    cdf: true,
    se: false,
    rate: false,
    thres: false,
};
const NO_CDF: ATerms = ATerms {
    cdf: false,
    ..CONTINUOUS
};
const ORDINAL: ATerms = ATerms {
    thres: true,
    ..NO_CDF
};
const ORDINAL_LINKS: &[Link] = &[
    Link::Logit,
    Link::Probit,
    Link::ProbitApprox,
    Link::Cloglog,
    Link::Cauchit,
];

/// The families: R1's, then R2's (§3.6).
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
        kind: Kind::Standard,
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
        kind: Kind::Standard,
    },
    Family {
        name: "lognormal",
        aliases: &[],
        mu_links: LOGNORMAL_LINKS,
        dpars: &[SIGMA],
        response: Response::Positive,
        stan: "lognormal",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "skew_normal",
        aliases: &[],
        mu_links: IDENTITY_LINKS,
        dpars: &[SIGMA, ALPHA],
        response: Response::Real,
        stan: "skew_normal",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
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
        kind: Kind::Standard,
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
        kind: Kind::Standard,
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
        kind: Kind::Standard,
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
        kind: Kind::Standard,
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
        kind: Kind::Standard,
    },
    Family {
        name: "Gamma",
        aliases: &["gamma"],
        mu_links: POSITIVE_LINKS,
        dpars: &[SHAPE],
        response: Response::Positive,
        stan: "gamma",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "weibull",
        aliases: &[],
        mu_links: POSITIVE_LINKS,
        dpars: &[SHAPE],
        response: Response::Positive,
        stan: "weibull",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "exponential",
        aliases: &[],
        mu_links: POSITIVE_LINKS,
        dpars: &[],
        response: Response::Positive,
        stan: "exponential",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "Beta",
        aliases: &["beta"],
        mu_links: BINARY_LINKS,
        dpars: &[PHI],
        response: Response::Unit,
        stan: "beta",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "zero_inflated_poisson",
        aliases: &[],
        mu_links: COUNT_LINKS,
        dpars: &[ZI],
        response: Response::Count,
        stan: "poisson",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "zero_inflated_negbinomial",
        aliases: &[],
        mu_links: COUNT_LINKS,
        dpars: &[NB_SHAPE, ZI],
        response: Response::Count,
        stan: "neg_binomial_2",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "hurdle_poisson",
        aliases: &[],
        mu_links: COUNT_LINKS,
        dpars: &[HU],
        response: Response::Count,
        stan: "poisson",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "hurdle_negbinomial",
        aliases: &[],
        mu_links: COUNT_LINKS,
        dpars: &[NB_SHAPE, HU],
        response: Response::Count,
        stan: "neg_binomial_2",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "hurdle_gamma",
        aliases: &[],
        mu_links: POSITIVE_LINKS,
        dpars: &[SHAPE, HU],
        response: Response::NonNegative,
        stan: "gamma",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "hurdle_lognormal",
        aliases: &[],
        mu_links: LOGNORMAL_LINKS,
        dpars: &[SIGMA, HU],
        response: Response::NonNegative,
        stan: "lognormal",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "zero_inflated_beta",
        aliases: &[],
        mu_links: BINARY_LINKS,
        dpars: &[PHI, ZI],
        response: Response::UnitZero,
        stan: "beta",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "zero_one_inflated_beta",
        aliases: &[],
        mu_links: BINARY_LINKS,
        dpars: &[PHI, ZOI, COI],
        response: Response::UnitClosed,
        stan: "beta",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
    Family {
        name: "beta_binomial",
        aliases: &[],
        mu_links: BINARY_LINKS,
        dpars: &[PHI],
        response: Response::Count,
        stan: "beta_binomial",
        aterms: ATerms {
            trials: true,
            ..NO_CDF
        },
        kind: Kind::Standard,
    },
    Family {
        name: "cumulative",
        aliases: &[],
        mu_links: ORDINAL_LINKS,
        dpars: &[DISC],
        response: Response::Ordinal,
        stan: "cumulative",
        aterms: ORDINAL,
        kind: Kind::Ordinal,
    },
    Family {
        name: "sratio",
        aliases: &[],
        mu_links: ORDINAL_LINKS,
        dpars: &[DISC],
        response: Response::Ordinal,
        stan: "sratio",
        aterms: ORDINAL,
        kind: Kind::Ordinal,
    },
    Family {
        name: "cratio",
        aliases: &[],
        mu_links: ORDINAL_LINKS,
        dpars: &[DISC],
        response: Response::Ordinal,
        stan: "cratio",
        aterms: ORDINAL,
        kind: Kind::Ordinal,
    },
    Family {
        name: "acat",
        aliases: &[],
        mu_links: ORDINAL_LINKS,
        dpars: &[DISC],
        response: Response::Ordinal,
        stan: "acat",
        aterms: ORDINAL,
        kind: Kind::Ordinal,
    },
    Family {
        name: "categorical",
        aliases: &[],
        mu_links: &[Link::Logit],
        dpars: &[],
        response: Response::Category,
        stan: "categorical_logit",
        aterms: NO_CDF,
        kind: Kind::Categorical,
    },
    Family {
        name: "von_mises",
        aliases: &[],
        mu_links: &[Link::TanHalf, Link::Identity],
        dpars: &[KAPPA],
        response: Response::Angle,
        stan: "von_mises",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "exgaussian",
        aliases: &["exponentially_modified_gaussian"],
        mu_links: IDENTITY_LINKS,
        dpars: &[SIGMA, EX_BETA],
        response: Response::Real,
        stan: "exp_mod_normal",
        aterms: CONTINUOUS,
        kind: Kind::Standard,
    },
    Family {
        name: "asym_laplace",
        aliases: &[],
        mu_links: IDENTITY_LINKS,
        dpars: &[SIGMA, QUANTILE],
        response: Response::Real,
        stan: "asym_laplace",
        aterms: NO_CDF,
        kind: Kind::Standard,
    },
];

/// The families a later release adds, so naming one says when rather than
/// "unknown".
const LATER: &[&str] = &[
    "mixture",
    "wiener",
    "shifted_lognormal",
    "skew_student",
    "gen_extreme_value",
    "inverse.gaussian",
    "frechet",
    "dirichlet",
    "multinomial",
    "logistic_normal",
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
        return Err(format!("the family `{name}` is not supported yet"));
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
        assert!(family("mixture").unwrap_err().contains("not supported yet"));
        let link = |f: &str, d: &str| family(f).unwrap().default_link(d).unwrap().name();
        assert_eq!(link("cumulative", "mu"), "logit");
        assert_eq!(link("cumulative", "disc"), "log");
        assert_eq!(link("von_mises", "mu"), "tan_half");
        assert_eq!(link("zero_inflated_poisson", "zi"), "logit");
        assert_eq!(link("hurdle_lognormal", "mu"), "identity");
    }
}
