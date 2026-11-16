//! Priors (TODO-bayesian-regression §3.7, R1.3): the menu, its argument
//! constraints and the classes each family is allowed on; a row's target and
//! its precedence; the defaults; and resolving the rows the admin set against
//! the parameters a formula creates.
//!
//! A prior is a family from a fixed menu and up to three numbers, so it is
//! **data** to the generated program (§2.5): a code and a real array. Only a
//! bound on `b` and `constant()` change a parameter's declaration, and so the
//! program.
//!
//! The defaults are brms's documented ones (`?set_prior`, brms 2.23): `b`
//! "an improper flat prior over the reals"; `cor` `lkj(1)`; `nu` `gamma(2,
//! 0.1)`; `shape` `gamma(0.01, 0.01)` for the gamma and Weibull families and
//! `inv_gamma(0.4, 0.3)` for the negative binomial; `sd` "a half student-t
//! prior with 3 degrees of freedom and a scale parameter that depends on the
//! standard deviation of the response after applying the link function.
//! Minimally, the scale parameter is 2.5", and `sigma` "scales in the same
//! way". The documentation does not say how that scale or the intercept's
//! default is computed, so the rule here is **this implementation's own**, and
//! stated: over ỹ, the response on the link scale where the link is defined,
//! the intercept's default is `student_t(3, median(ỹ), s)` and the SDs'
//! `student_t(3, 0, s)`, with `s = max(2.5, MAD(ỹ))` (MAD scaled to be
//! consistent for the SD of a normal, as R's `mad`), both rounded to one
//! decimal. For a binary or binomial response, and any link not defined at the
//! data (`logit` at 0 or 1), ỹ says nothing and the default is `student_t(3, 0,
//! 2.5)`. `phi` (Beta) is not documented there either; `gamma(0.01, 0.01)` is
//! used, as for `shape`. The resolved numbers are always stored on the
//! instance and shown, so nobody has to guess.

use std::collections::BTreeMap;
use std::fmt;

use sc_model::terms::number_text;

use crate::parse::expr::{Expr, expression};
use crate::parse::lex::lex;

/// One prior row as stored in the configuration — `set_prior`'s shape. An
/// empty `coef` (or `group`) is a whole-class row.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriorRow {
    /// The class: `b`, `Intercept`, `sd`, `cor`, or a distributional
    /// parameter (`sigma`, `shape`, …).
    pub class: String,
    /// The coefficient, or empty for every coefficient of the class.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub coef: String,
    /// The grouping factor, for `sd` and `cor`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// The distributional parameter whose formula it is in (`sigma ~ x`'s
    /// `b`), or empty for `mu`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dpar: String,
    /// A non-linear parameter (R2).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub nlpar: String,
    /// A response of a multivariate model (R2).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resp: String,
    /// The prior: `normal(0, 5)`. Empty is `flat`.
    pub prior: String,
    /// A lower bound, on `b` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lb: Option<f64>,
    /// An upper bound, on `b` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ub: Option<f64>,
}

impl PriorRow {
    /// Whether `other` sets the same parameter (or the same whole class).
    pub fn same_target(&self, other: &PriorRow) -> bool {
        self.class == other.class
            && self.coef == other.coef
            && self.group == other.group
            && self.dpar == other.dpar
            && self.nlpar == other.nlpar
            && self.resp == other.resp
    }

    /// `b` of `x`, `sd` of `county`, `sigma`: how a sentence names the row.
    pub fn describe(&self) -> String {
        let mut text = format!("`{}`", self.class);
        if !self.coef.is_empty() {
            text.push_str(&format!(" of `{}`", self.coef));
        }
        if !self.group.is_empty() {
            text.push_str(&format!(" in `{}`", self.group));
        }
        if !self.dpar.is_empty() {
            text.push_str(&format!(" for `{}`", self.dpar));
        }
        if !self.nlpar.is_empty() {
            text.push_str(&format!(" of the non-linear parameter `{}`", self.nlpar));
        }
        if !self.resp.is_empty() {
            text.push_str(&format!(" of the response `{}`", self.resp));
        }
        format!("the prior row {text}")
    }
}

/// The rows of `pasted` merged into `existing`: a pasted row with the target
/// of an existing one replaces it, in place. Answers the merged rows and the
/// rows that were replaced, so the editor can name each (§3.7).
pub fn merge_rows(existing: &[PriorRow], pasted: &[PriorRow]) -> (Vec<PriorRow>, Vec<PriorRow>) {
    let mut out = existing.to_vec();
    let mut replaced = Vec::new();
    for row in pasted {
        match out.iter_mut().find(|r| r.same_target(row)) {
            Some(old) => {
                replaced.push(old.clone());
                *old = row.clone();
            }
            None => out.push(row.clone()),
        }
    }
    (out, replaced)
}

/// A family of the prior menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PriorFamily {
    /// Improper, flat over the reals.
    Flat,
    /// `normal(μ, σ)`.
    Normal,
    /// `student_t(ν, μ, σ)`.
    StudentT,
    /// `cauchy(μ, σ)`.
    Cauchy,
    /// `double_exponential(μ, σ)`.
    DoubleExponential,
    /// `logistic(μ, σ)`.
    Logistic,
    /// `exponential(λ)`.
    Exponential,
    /// `gamma(α, β)`.
    Gamma,
    /// `inv_gamma(α, β)`.
    InvGamma,
    /// `lognormal(μ, σ)`.
    Lognormal,
    /// `uniform(a, b)`.
    Uniform,
    /// `beta(α, β)`.
    Beta,
    /// `lkj(η)`.
    Lkj,
    /// `constant(x)`: the parameter fixed — structural.
    Constant,
    /// `dirichlet(α)`: on a simplex (`simo`), every element's concentration
    /// `α` (R2.1).
    Dirichlet,
}

/// One entry of the menu.
struct Entry {
    family: PriorFamily,
    name: &'static str,
    /// The code the generated program branches on (stable: it is data).
    code: u8,
    /// The arguments' names, as the sentences call them.
    args: &'static [&'static str],
}

const MENU: &[Entry] = &[
    Entry {
        family: PriorFamily::Flat,
        name: "flat",
        code: 0,
        args: &[],
    },
    Entry {
        family: PriorFamily::Normal,
        name: "normal",
        code: 1,
        args: &["μ", "σ"],
    },
    Entry {
        family: PriorFamily::StudentT,
        name: "student_t",
        code: 2,
        args: &["ν", "μ", "σ"],
    },
    Entry {
        family: PriorFamily::Cauchy,
        name: "cauchy",
        code: 3,
        args: &["μ", "σ"],
    },
    Entry {
        family: PriorFamily::DoubleExponential,
        name: "double_exponential",
        code: 4,
        args: &["μ", "σ"],
    },
    Entry {
        family: PriorFamily::Logistic,
        name: "logistic",
        code: 5,
        args: &["μ", "σ"],
    },
    Entry {
        family: PriorFamily::Exponential,
        name: "exponential",
        code: 6,
        args: &["λ"],
    },
    Entry {
        family: PriorFamily::Gamma,
        name: "gamma",
        code: 7,
        args: &["α", "β"],
    },
    Entry {
        family: PriorFamily::InvGamma,
        name: "inv_gamma",
        code: 8,
        args: &["α", "β"],
    },
    Entry {
        family: PriorFamily::Lognormal,
        name: "lognormal",
        code: 9,
        args: &["μ", "σ"],
    },
    Entry {
        family: PriorFamily::Uniform,
        name: "uniform",
        code: 10,
        args: &["a", "b"],
    },
    Entry {
        family: PriorFamily::Beta,
        name: "beta",
        code: 11,
        args: &["α", "β"],
    },
    Entry {
        family: PriorFamily::Lkj,
        name: "lkj",
        code: 12,
        args: &["η"],
    },
    Entry {
        family: PriorFamily::Constant,
        name: "constant",
        code: 13,
        args: &["x"],
    },
    Entry {
        family: PriorFamily::Dirichlet,
        name: "dirichlet",
        code: 14,
        args: &["α"],
    },
];

impl PriorFamily {
    fn entry(self) -> &'static Entry {
        MENU.iter().find(|e| e.family == self).unwrap_or(&MENU[0])
    }

    /// Its name.
    pub fn name(self) -> &'static str {
        self.entry().name
    }

    /// The integer the generated program branches on.
    pub fn code(self) -> u8 {
        self.entry().code
    }

    /// Whether it changes the program (rather than only its data).
    pub fn is_structural(self) -> bool {
        self == PriorFamily::Constant
    }

    /// Whether its support is the positive reals only.
    fn positive_only(self) -> bool {
        matches!(
            self,
            PriorFamily::Exponential
                | PriorFamily::Gamma
                | PriorFamily::InvGamma
                | PriorFamily::Lognormal
        )
    }

    /// Every name on the menu, for completion.
    pub fn names() -> Vec<&'static str> {
        MENU.iter().map(|e| e.name).collect()
    }
}

/// A prior: a family of the menu and its numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Prior {
    /// The family.
    pub family: PriorFamily,
    /// Its arguments, as many as it takes.
    pub args: Vec<f64>,
}

impl Prior {
    /// `flat`.
    pub fn flat() -> Prior {
        Prior {
            family: PriorFamily::Flat,
            args: Vec::new(),
        }
    }

    /// `family(args…)`, unchecked.
    pub fn new(family: PriorFamily, args: &[f64]) -> Prior {
        Prior {
            family,
            args: args.to_vec(),
        }
    }

    /// The prior written `text` (empty is `flat`), its arguments checked.
    pub fn parse(text: &str) -> Result<Prior, String> {
        if text.trim().is_empty() {
            return Ok(Prior::flat());
        }
        let tokens = lex(text).map_err(|e| e.message)?;
        let e = expression(&tokens).map_err(|e| e.message)?;
        Prior::from_expr(&e)
    }

    /// The prior an expression writes.
    pub(crate) fn from_expr(e: &Expr) -> Result<Prior, String> {
        let (name, args): (&str, Vec<f64>) = match e.unparen() {
            Expr::Name(n, _) => (n.as_str(), Vec::new()),
            Expr::Call { name, args, .. } => {
                let mut numbers = Vec::with_capacity(args.len());
                for a in args {
                    if let Some((n, _)) = &a.name {
                        return Err(format!(
                            "write the arguments of `{name}` in order, without names (`{n} =`)"
                        ));
                    }
                    numbers.push(
                        a.value
                            .as_number()
                            .ok_or_else(|| format!("the arguments of `{name}` are numbers"))?,
                    );
                }
                (name.as_str(), numbers)
            }
            _ => return Err("a prior is a distribution: `normal(0, 5)`".to_owned()),
        };
        let name = match name {
            "lkj_corr_cholesky" | "lkj_corr" => "lkj",
            other => other,
        };
        let Some(entry) = MENU.iter().find(|m| m.name == name) else {
            let later = matches!(name, "horseshoe" | "R2D2" | "lasso" | "logistic_normal");
            return Err(if later {
                format!("`{name}()` is not supported yet")
            } else {
                format!(
                    "`{name}` is not a prior on the menu; the priors are {}",
                    MENU.iter()
                        .map(|m| format!("`{}`", m.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        };
        let prior = Prior {
            family: entry.family,
            args,
        };
        prior.check_args()?;
        Ok(prior)
    }

    /// Its arguments against the menu's constraints, each violation a
    /// sentence: "σ must be positive".
    pub fn check_args(&self) -> Result<(), String> {
        let entry = self.family.entry();
        if self.args.len() != entry.args.len() {
            return Err(match entry.args.len() {
                0 => format!("`{}` takes no arguments", entry.name),
                n => format!(
                    "`{}` takes {n} argument{} ({}), and {} given",
                    entry.name,
                    if n == 1 { "" } else { "s" },
                    entry.args.join(", "),
                    match self.args.len() {
                        0 => "none is".to_owned(),
                        1 => "1 is".to_owned(),
                        k => format!("{k} are"),
                    }
                ),
            });
        }
        for (x, name) in self.args.iter().zip(entry.args) {
            if !x.is_finite() {
                return Err(format!("{name} must be a finite number"));
            }
        }
        let positive = |i: usize| -> Result<(), String> {
            if self.args[i] > 0.0 {
                Ok(())
            } else {
                Err(format!(
                    "{} must be positive (it is {})",
                    entry.args[i],
                    number_text(self.args[i])
                ))
            }
        };
        match self.family {
            PriorFamily::Flat | PriorFamily::Constant => Ok(()),
            PriorFamily::Normal
            | PriorFamily::Cauchy
            | PriorFamily::DoubleExponential
            | PriorFamily::Logistic
            | PriorFamily::Lognormal => positive(1),
            PriorFamily::StudentT => {
                positive(0)?;
                positive(2)
            }
            PriorFamily::Exponential | PriorFamily::Lkj | PriorFamily::Dirichlet => positive(0),
            PriorFamily::Gamma | PriorFamily::InvGamma | PriorFamily::Beta => {
                positive(0)?;
                positive(1)
            }
            PriorFamily::Uniform => {
                if self.args[0] < self.args[1] {
                    Ok(())
                } else {
                    Err(format!(
                        "a must be less than b (they are {} and {})",
                        number_text(self.args[0]),
                        number_text(self.args[1])
                    ))
                }
            }
        }
    }
}

impl fmt::Display for Prior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.family == PriorFamily::Flat {
            return f.write_str("flat");
        }
        let args: Vec<String> = self.args.iter().map(|x| number_text(*x)).collect();
        write!(f, "{}({})", self.family.name(), args.join(", "))
    }
}

/// What values a class's parameters take — which decides the priors allowed
/// on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    /// Any real number: `b`, `Intercept`, skewness.
    Real,
    /// Greater than zero: `sd`, `sigma`, `shape`, `phi`, `kappa`.
    Positive,
    /// Greater than one: `nu`.
    AboveOne,
    /// A correlation matrix: `cor`.
    Correlation,
    /// In (0, 1): `zi`, `hu`, `zoi`, `coi`, `quantile`.
    Unit,
    /// A simplex: a monotonic effect's `simo`.
    Simplex,
}

/// Whether `prior` may go on a row of `class` (of `kind`), as `row` sets it.
/// `center` is whether the intercept is centred.
pub fn check_allowed(
    prior: &Prior,
    class: &str,
    kind: ClassKind,
    row: &PriorRow,
    center: bool,
) -> Result<(), String> {
    let fam = prior.family;
    let on = |what: &str| format!("`{}` is {what}", fam.name());
    if (row.lb.is_some() || row.ub.is_some()) && class != "b" {
        return Err(format!(
            "bounds (`lb`, `ub`) can only be set on `b`, and this row is `{class}`"
        ));
    }
    match kind {
        ClassKind::Simplex => {
            if fam != PriorFamily::Dirichlet {
                return Err(format!(
                    "{}; a simplex (`simo`) takes `dirichlet(α)`",
                    on("not a prior for a simplex")
                ));
            }
            return Ok(());
        }
        _ if fam == PriorFamily::Dirichlet => {
            return Err(on("for a monotonic effect's simplex (`simo`) only"));
        }
        ClassKind::Correlation => {
            if fam != PriorFamily::Lkj {
                return Err(format!(
                    "{}; a correlation takes `lkj(η)`",
                    on("not a prior for a correlation")
                ));
            }
            return Ok(());
        }
        _ if fam == PriorFamily::Lkj => {
            return Err(on("for correlations (`cor`) only"));
        }
        _ => {}
    }
    match fam {
        PriorFamily::Flat => {
            let allowed = class == "b" || (class == "Intercept" && !center);
            if !allowed {
                return Err(if class == "Intercept" {
                    "`flat` cannot go on a centred intercept; set `center` off, or choose a \
                     proper prior"
                        .to_owned()
                } else {
                    format!("`flat` is allowed on `b` only, and this row is `{class}`")
                });
            }
        }
        PriorFamily::Uniform => {
            if class != "b" {
                return Err(on("allowed on `b` only"));
            }
            let (a, b) = (prior.args[0], prior.args[1]);
            if row.lb != Some(a) || row.ub != Some(b) {
                return Err(format!(
                    "`uniform({}, {})` needs the row's bounds to match it: `lb = {}`, `ub = {}`",
                    number_text(a),
                    number_text(b),
                    number_text(a),
                    number_text(b)
                ));
            }
        }
        PriorFamily::Beta => {
            if kind != ClassKind::Unit {
                return Err(on("for a parameter bounded to (0, 1), and this is not one"));
            }
        }
        f if f.positive_only() => {
            if !matches!(kind, ClassKind::Positive | ClassKind::AboveOne) {
                return Err(format!(
                    "`{}` is defined on the positive numbers only, and `{class}` can be negative",
                    f.name()
                ));
            }
        }
        PriorFamily::Constant => {
            let x = prior.args[0];
            let ok = match kind {
                ClassKind::Positive => x > 0.0,
                ClassKind::AboveOne => x > 1.0,
                ClassKind::Unit => x > 0.0 && x < 1.0,
                _ => true,
            };
            if !ok {
                return Err(format!(
                    "`constant({})` is outside what `{class}` can be",
                    number_text(x)
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// A class's default prior, before the data is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DefaultPrior {
    /// `flat`.
    Flat,
    /// `lkj(1)`.
    Lkj,
    /// `student_t(3, 0, s)`, `s` from the response (§3.7).
    ResponseScale,
    /// `student_t(3, median(ỹ), s)`, from the response (§3.7).
    InterceptScale,
    /// A fixed prior: a menu name and its numbers.
    Fixed(&'static str, &'static [f64]),
}

/// The location and scale a data-dependent default is computed with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DataScale {
    /// `median(ỹ)`, rounded.
    pub location: f64,
    /// `max(2.5, MAD(ỹ))`, rounded.
    pub scale: f64,
}

impl Default for DataScale {
    fn default() -> DataScale {
        DataScale {
            location: 0.0,
            scale: 2.5,
        }
    }
}

impl DataScale {
    /// Over the response values on the link scale (those where the link is
    /// defined). With none, the location is 0 and the scale 2.5.
    pub fn of(link_scale: &[f64]) -> DataScale {
        let mut v: Vec<f64> = link_scale
            .iter()
            .copied()
            .filter(|x| x.is_finite())
            .collect();
        if v.is_empty() {
            return DataScale::default();
        }
        let med = median(&mut v);
        let mut dev: Vec<f64> = v.iter().map(|x| (x - med).abs()).collect();
        let mad = 1.4826 * median(&mut dev);
        DataScale {
            location: round1(med),
            scale: round1(mad.max(2.5)),
        }
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn round1(x: f64) -> f64 {
    let r = (x * 10.0).round() / 10.0;
    if r == 0.0 { 0.0 } else { r }
}

impl DefaultPrior {
    /// The prior, with `scale` filling in the data-dependent ones (`None`
    /// before the data has been read: then 0 and 2.5 stand in, and
    /// [`rule`](DefaultPrior::rule) says what they will be).
    pub fn prior(self, scale: Option<DataScale>) -> Prior {
        let s = scale.unwrap_or_default();
        match self {
            DefaultPrior::Flat => Prior::flat(),
            DefaultPrior::Lkj => Prior::new(PriorFamily::Lkj, &[1.0]),
            DefaultPrior::ResponseScale => Prior::new(PriorFamily::StudentT, &[3.0, 0.0, s.scale]),
            DefaultPrior::InterceptScale => {
                Prior::new(PriorFamily::StudentT, &[3.0, s.location, s.scale])
            }
            DefaultPrior::Fixed(name, args) => {
                let family = MENU
                    .iter()
                    .find(|m| m.name == name)
                    .map_or(PriorFamily::Flat, |m| m.family);
                Prior::new(family, args)
            }
        }
    }

    /// For a data-dependent default, the rule the editor shows until a preview
    /// gives the numbers: "student_t(3, median, scale) of `log_radon`".
    pub fn rule(self, response: &str) -> Option<String> {
        match self {
            DefaultPrior::ResponseScale => Some(format!("student_t(3, 0, scale) of `{response}`")),
            DefaultPrior::InterceptScale => {
                Some(format!("student_t(3, median, scale) of `{response}`"))
            }
            _ => None,
        }
    }
}

/// How a coefficient may be named before the levels of the categorical
/// variables are known: the parts of its name, joined with `:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoefTemplate {
    /// The parts.
    pub parts: Vec<CoefPart>,
}

/// One part of a [`CoefTemplate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoefPart {
    /// Exactly this text: a numeric variable's label.
    Exact(String),
    /// The variable's label followed by one of its levels.
    Level(String),
    /// A variable whose type is not known yet: its label alone, or followed by
    /// anything.
    Unknown(String),
}

/// Whether a name can come from a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// It is the template's name (no levels involved).
    Exact,
    /// It could be, once the levels are known.
    Possible,
    /// It cannot.
    No,
}

impl CoefTemplate {
    /// A template of exact parts.
    pub fn exact(name: &str) -> CoefTemplate {
        CoefTemplate {
            parts: vec![CoefPart::Exact(name.to_owned())],
        }
    }

    /// `region…`, `area:region…` — how the editor shows it before the levels
    /// are known.
    pub fn display(&self) -> String {
        self.parts
            .iter()
            .map(|p| match p {
                CoefPart::Exact(s) => s.clone(),
                CoefPart::Level(s) | CoefPart::Unknown(s) => format!("{s}…"),
            })
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Whether `name` can come from it.
    pub fn matches(&self, name: &str) -> Match {
        fn go(parts: &[CoefPart], rest: &str, levels: bool) -> Match {
            let Some((first, others)) = parts.split_first() else {
                return if rest.is_empty() {
                    if levels {
                        Match::Possible
                    } else {
                        Match::Exact
                    }
                } else {
                    Match::No
                };
            };
            let label = match first {
                CoefPart::Exact(s) | CoefPart::Level(s) | CoefPart::Unknown(s) => s.as_str(),
            };
            let Some(after) = rest.strip_prefix(label) else {
                return Match::No;
            };
            // Where this part may end: at the end, or at any `:` that leaves
            // the next part to match (a level can contain `:`).
            let ends: Vec<usize> = match first {
                CoefPart::Exact(_) => vec![0],
                CoefPart::Level(_) => (1..=after.len())
                    .filter(|i| after.is_char_boundary(*i))
                    .collect(),
                CoefPart::Unknown(_) => (0..=after.len())
                    .filter(|i| after.is_char_boundary(*i))
                    .collect(),
            };
            let levelled = levels || !matches!(first, CoefPart::Exact(_));
            let mut best = Match::No;
            for end in ends {
                let tail = &after[end..];
                let next = if others.is_empty() {
                    tail
                } else if let Some(t) = tail.strip_prefix(':') {
                    t
                } else {
                    continue;
                };
                if others.is_empty() && !next.is_empty() {
                    continue;
                }
                match go(others, next, levelled) {
                    Match::Exact => return Match::Exact,
                    Match::Possible => best = Match::Possible,
                    Match::No => {}
                }
            }
            best
        }
        go(&self.parts, name, false)
    }
}

/// A coefficient of a class: its name, when known, or a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coef {
    /// A name.
    Named(String),
    /// A name not known until the levels are read.
    Template(CoefTemplate),
}

impl Coef {
    /// The name, or the template's display.
    pub fn display(&self) -> String {
        match self {
            Coef::Named(n) => n.clone(),
            Coef::Template(t) => t.display(),
        }
    }

    fn matches(&self, name: &str) -> Match {
        match self {
            Coef::Named(n) => {
                if n == name {
                    Match::Exact
                } else {
                    Match::No
                }
            }
            Coef::Template(t) => t.matches(name),
        }
    }
}

/// One class of parameters a formula creates: what a prior row can target.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassSlot {
    /// `b`, `Intercept`, `sd`, `cor`, or a distributional parameter's name.
    pub class: String,
    /// The distributional parameter's formula it belongs to; empty for `mu`
    /// and for a distributional parameter's own class.
    pub dpar: String,
    /// The grouping factor, for `sd` and `cor`.
    pub group: String,
    /// The coefficients, for `b` and `sd`; empty for a class of one.
    pub coefs: Vec<Coef>,
    /// What its values are.
    pub kind: ClassKind,
    /// Its default prior.
    pub default: DefaultPrior,
    /// The non-linear parameter it belongs to; empty for none.
    pub nlpar: String,
    /// The response it belongs to, in a multivariate model; empty in a
    /// univariate one.
    pub resp: String,
    /// The response whose data scale its data-dependent default uses (an
    /// index; 0 in a univariate model).
    pub response: usize,
}

/// Where a resolved prior came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Source {
    /// An explicit row for exactly this target.
    Explicit,
    /// An explicit whole-class row (`from` names it).
    Inherited {
        /// The row it came from, as a sentence names it.
        from: String,
    },
    /// The default.
    Default,
    /// The default, computed from the data.
    DefaultFromData,
}

/// One row of the editor's prior table (§7): a whole class or one parameter,
/// with its resolved prior and where it came from.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TableRow {
    /// The class.
    pub class: String,
    /// The coefficient, empty for the whole class (`(all)`) or a class of one.
    pub coef: String,
    /// The grouping factor.
    pub group: String,
    /// The distributional parameter.
    pub dpar: String,
    /// The non-linear parameter.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub nlpar: String,
    /// The response, in a multivariate model.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resp: String,
    /// The prior, as text.
    pub prior: String,
    /// Before the data is read, the rule a data-dependent default follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Where it came from.
    pub source: Source,
    /// Whether this row is a whole class's.
    pub whole_class: bool,
    /// Whether the prior changes the program (a bound, `constant`).
    pub structural: bool,
    /// The row's bounds, on `b`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lb: Option<f64>,
    /// The upper bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ub: Option<f64>,
}

/// An explicit row that targets nothing the formula creates, with why. Kept,
/// never dropped (§3.7).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Unmatched {
    /// The row, as stored.
    pub row: PriorRow,
    /// The sentence.
    pub reason: String,
}

/// The parameters' resolved priors, and the rows that matched nothing.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct PriorTable {
    /// The table, class by class.
    pub rows: Vec<TableRow>,
    /// The explicit rows that matched nothing.
    pub unmatched: Vec<Unmatched>,
}

impl PriorTable {
    /// Refuse when any row matched nothing — what a save and a fit do; the
    /// editor shows them instead.
    pub fn require_matched(&self) -> sc_error::Result<()> {
        match self.unmatched.first() {
            None => Ok(()),
            Some(u) => Err(sc_error::Error::invalid(format!(
                "{}: {}; delete the row, or point it at a current parameter",
                u.row.describe(),
                u.reason
            ))),
        }
    }

    /// The parameters' priors only (not the whole-class rows), keyed by their
    /// [`Target`] — what a fit stores and the generator's data carries.
    pub fn parameters(&self) -> BTreeMap<Target, &TableRow> {
        self.rows
            .iter()
            .filter(|r| !r.whole_class || r.coef.is_empty() && !has_coefs(&r.class))
            .map(|r| {
                (
                    Target {
                        class: r.class.clone(),
                        dpar: r.dpar.clone(),
                        nlpar: r.nlpar.clone(),
                        resp: r.resp.clone(),
                        group: r.group.clone(),
                        coef: r.coef.clone(),
                    },
                    r,
                )
            })
            .collect()
    }
}

/// What one parameter's prior is for: a prior row's target, every part
/// spelled out.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Target {
    /// The class.
    pub class: String,
    /// The distributional parameter (empty for `mu`).
    pub dpar: String,
    /// The non-linear parameter.
    pub nlpar: String,
    /// The response, in a multivariate model.
    pub resp: String,
    /// The grouping factor.
    pub group: String,
    /// The coefficient.
    pub coef: String,
}

fn has_coefs(class: &str) -> bool {
    class == "b" || class == "sd" || class == "simo"
}

/// Resolve `rows` against the classes a formula creates: every parameter's
/// prior by precedence — (class, coef, group) → (class, group) → (class) —
/// and the rows that match nothing, each with why.
///
/// `scale` fills in the data-dependent defaults (`None` before the data is
/// read); `response` is named in their rules; `center` is whether the
/// intercept is centred.
pub fn resolve(
    slots: &[ClassSlot],
    rows: &[PriorRow],
    scale: Option<DataScale>,
    response: &str,
    center: bool,
) -> PriorTable {
    let scales = scale.map(|s| vec![s]);
    resolve_each(
        slots,
        rows,
        scales.as_deref(),
        &[response.to_owned()],
        center,
    )
}

/// [`resolve`] for a model of several responses: `scales` (one per
/// response, `None` before the data is read) and the responses' names, which
/// a slot picks by its [`response`](ClassSlot::response).
#[allow(clippy::too_many_lines)]
pub fn resolve_each(
    slots: &[ClassSlot],
    rows: &[PriorRow],
    scales: Option<&[DataScale]>,
    responses: &[String],
    center: bool,
) -> PriorTable {
    let mut table = PriorTable::default();

    // Each explicit row: a target that exists, a prior that parses, and one
    // allowed on its class.
    let mut valid: Vec<(&PriorRow, Prior)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if rows[..i].iter().any(|r| r.same_target(row)) {
            table.unmatched.push(Unmatched {
                row: row.clone(),
                reason: "another row sets the same parameter".to_owned(),
            });
            continue;
        }
        match check_row(slots, row, center) {
            Ok(prior) => valid.push((row, prior)),
            Err(reason) => table.unmatched.push(Unmatched {
                row: row.clone(),
                reason,
            }),
        }
    }

    let explicit = |slot: &ClassSlot, group: &str, coef: &str| {
        valid.iter().find(|(r, _)| {
            r.class == slot.class
                && r.dpar == slot.dpar
                && r.nlpar == slot.nlpar
                && r.resp == slot.resp
                && r.group == group
                && r.coef == coef
        })
    };
    let from_default = |slot: &ClassSlot| {
        let scale = scales.and_then(|s| s.get(slot.response).copied());
        let response = responses
            .get(slot.response)
            .or_else(|| responses.first())
            .map_or("", String::as_str);
        let prior = slot.default.prior(scale);
        let data = matches!(
            slot.default,
            DefaultPrior::ResponseScale | DefaultPrior::InterceptScale
        );
        let rule = if scales.is_none() {
            slot.default.rule(response)
        } else {
            None
        };
        (
            prior,
            if data {
                Source::DefaultFromData
            } else {
                Source::Default
            },
            rule,
        )
    };
    let row_of = |slot: &ClassSlot,
                  coef: &str,
                  whole: bool,
                  prior: &Prior,
                  source: Source,
                  rule: Option<String>,
                  bounds: Option<&PriorRow>| TableRow {
        class: slot.class.clone(),
        coef: coef.to_owned(),
        group: slot.group.clone(),
        dpar: slot.dpar.clone(),
        nlpar: slot.nlpar.clone(),
        resp: slot.resp.clone(),
        prior: prior.to_string(),
        rule,
        source,
        whole_class: whole,
        structural: prior.family.is_structural()
            || bounds.is_some_and(|r| r.lb.is_some() || r.ub.is_some()),
        lb: bounds.and_then(|r| r.lb),
        ub: bounds.and_then(|r| r.ub),
    };

    for slot in slots {
        // The whole class: its own row, else (for `sd`) the all-groups row,
        // else the default.
        let class_row = explicit(slot, &slot.group, "").or_else(|| {
            if slot.group.is_empty() {
                None
            } else {
                explicit(slot, "", "")
            }
        });
        let (class_prior, class_source, class_rule, class_bounds) = match class_row {
            Some((row, prior)) => {
                let source = if row.group == slot.group {
                    Source::Explicit
                } else {
                    Source::Inherited {
                        from: row.describe(),
                    }
                };
                (prior.clone(), source, None, Some(*row))
            }
            None => {
                let (p, s, r) = from_default(slot);
                (p, s, r, None)
            }
        };
        table.rows.push(row_of(
            slot,
            "",
            true,
            &class_prior,
            class_source.clone(),
            class_rule.clone(),
            class_bounds,
        ));
        for coef in &slot.coefs {
            let name = coef.display();
            let (prior, source, rule, bounds) = match explicit(slot, &slot.group, &name) {
                Some((row, prior)) => (prior.clone(), Source::Explicit, None, Some(*row)),
                None => {
                    let source = match (&class_source, class_row) {
                        (_, Some((row, _))) => Source::Inherited {
                            from: row.describe(),
                        },
                        (other, None) => other.clone(),
                    };
                    (
                        class_prior.clone(),
                        source,
                        class_rule.clone(),
                        class_bounds,
                    )
                }
            };
            table
                .rows
                .push(row_of(slot, &name, false, &prior, source, rule, bounds));
        }
    }
    table
}

/// One explicit row against the classes: its target exists and its prior is
/// allowed there. The prior, or why not.
#[allow(clippy::too_many_lines)] // one check after another, each its sentence
fn check_row(slots: &[ClassSlot], row: &PriorRow, center: bool) -> Result<Prior, String> {
    let prior = Prior::parse(&row.prior)?;
    if !row.resp.is_empty() && !slots.iter().any(|s| s.resp == row.resp) {
        let resps: Vec<String> = slots
            .iter()
            .filter(|s| !s.resp.is_empty())
            .map(|s| format!("`{}`", s.resp))
            .fold(Vec::new(), |mut v, r| {
                if !v.contains(&r) {
                    v.push(r);
                }
                v
            });
        return Err(if resps.is_empty() {
            "`resp` names a response of a multivariate model, and this model has one response, \
             so `resp` must be empty"
                .to_owned()
        } else {
            format!(
                "`{}` is not a response of the model (they are {})",
                row.resp,
                resps.join(", ")
            )
        });
    }
    if !row.nlpar.is_empty() && !slots.iter().any(|s| s.nlpar == row.nlpar) {
        return Err(format!(
            "`{}` is not a non-linear parameter of the formula",
            row.nlpar
        ));
    }
    let multivariate = slots.iter().any(|s| !s.resp.is_empty());
    let of_class: Vec<&ClassSlot> = slots
        .iter()
        .filter(|s| s.class == row.class && s.nlpar == row.nlpar)
        .filter(|s| !multivariate || s.class == "cor" || s.class == "rescor" || s.resp == row.resp)
        .collect();
    if of_class.is_empty() {
        let mut classes: Vec<&str> = slots.iter().map(|s| s.class.as_str()).collect();
        classes.dedup();
        let known = matches!(
            row.class.as_str(),
            "b" | "Intercept" | "sd" | "cor" | "L" | "simo" | "rescor"
        ) || crate::family::FAMILIES
            .iter()
            .any(|f| f.dpar(&row.class).is_some());
        return Err(if known {
            let why = match row.class.as_str() {
                "b" => "the formula has no population-level coefficients".to_owned(),
                "Intercept" => {
                    "the formula has no centred intercept (it is removed, or `center` is off \
                     and the intercept is a `b` coefficient)"
                        .to_owned()
                }
                "sd" => "the formula has no group-level terms".to_owned(),
                "cor" => "the formula has no correlated group-level effects".to_owned(),
                "simo" => "the formula has no monotonic effects (`mo()`)".to_owned(),
                "rescor" => "the model has no residual correlations".to_owned(),
                _ if multivariate && row.resp.is_empty() => {
                    "in a multivariate model, a row names its response (`resp`)".to_owned()
                }
                "L" => "set the correlations' prior on `cor`".to_owned(),
                other => format!(
                    "the family has no `{other}`, or `{other}` has a formula of its own (set \
                     its priors with `dpar = \"{other}\"`)"
                ),
            };
            format!("there is no `{}` here: {why}", row.class)
        } else {
            format!(
                "there is no class `{}`; the classes here are {}",
                row.class,
                classes
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }
    let of_dpar: Vec<&&ClassSlot> = of_class.iter().filter(|s| s.dpar == row.dpar).collect();
    if of_dpar.is_empty() {
        return Err(if row.dpar.is_empty() {
            format!(
                "`{}` is only in the formula of {}, so the row needs `dpar`",
                row.class,
                of_class
                    .iter()
                    .map(|s| format!("`{}`", s.dpar))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "`{}` has no formula with a `{}` (`dpar = \"{}\"`)",
                row.dpar, row.class, row.dpar
            )
        });
    }
    let of_group: Vec<&&&ClassSlot> = if row.group.is_empty() {
        of_dpar.iter().collect()
    } else {
        let found: Vec<&&&ClassSlot> = of_dpar.iter().filter(|s| s.group == row.group).collect();
        if found.is_empty() {
            let groups: Vec<String> = of_dpar
                .iter()
                .filter(|s| !s.group.is_empty())
                .map(|s| format!("`{}`", s.group))
                .collect();
            return Err(if groups.is_empty() {
                format!(
                    "`{}` has no grouping factor, so `group` must be empty",
                    row.class
                )
            } else {
                format!(
                    "`{}` is not a grouping factor here (they are {})",
                    row.group,
                    groups.join(", ")
                )
            });
        }
        found
    };
    if !row.coef.is_empty() {
        let any_coefs = of_group.iter().any(|s| !s.coefs.is_empty());
        if !any_coefs {
            return Err(format!(
                "`{}` has no coefficients, so `coef` must be empty",
                row.class
            ));
        }
        let best = of_group
            .iter()
            .flat_map(|s| s.coefs.iter().map(|c| c.matches(&row.coef)))
            .fold(Match::No, |a, b| match (a, b) {
                (Match::Exact, _) | (_, Match::Exact) => Match::Exact,
                (Match::Possible, _) | (_, Match::Possible) => Match::Possible,
                _ => Match::No,
            });
        match best {
            Match::Exact => {}
            Match::Possible => {}
            Match::No => {
                let known: Vec<String> = of_group
                    .iter()
                    .flat_map(|s| s.coefs.iter().map(|c| format!("`{}`", c.display())))
                    .collect();
                return Err(format!(
                    "no term of the formula gives a coefficient `{}` (they are {})",
                    row.coef,
                    known.join(", ")
                ));
            }
        }
    }
    let kind = of_group[0].kind;
    check_allowed(&prior, &row.class, kind, row, center)?;
    Ok(prior)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_parses_and_prints_itself() {
        for text in [
            "normal(0, 5)",
            "student_t(3, 0, 2.5)",
            "cauchy(0, 1)",
            "gamma(2, 0.1)",
            "lkj(2)",
            "flat",
            "uniform(-1, 1)",
            "constant(1)",
        ] {
            assert_eq!(Prior::parse(text).unwrap().to_string(), text);
        }
        assert_eq!(Prior::parse("").unwrap(), Prior::flat());
        assert_eq!(
            Prior::parse("lkj_corr_cholesky(2)").unwrap().to_string(),
            "lkj(2)"
        );
        assert_eq!(Prior::parse("normal(-1, 2)").unwrap().args, [-1.0, 2.0]);
    }

    #[test]
    fn each_argument_constraint_has_its_sentence() {
        let cases = [
            ("normal(0, -1)", "σ must be positive (it is -1)"),
            ("normal(0, 0)", "σ must be positive (it is 0)"),
            ("student_t(0, 0, 1)", "ν must be positive (it is 0)"),
            ("student_t(3, 0, -2)", "σ must be positive (it is -2)"),
            ("exponential(0)", "λ must be positive"),
            ("gamma(-1, 1)", "α must be positive"),
            ("inv_gamma(1, 0)", "β must be positive"),
            ("lognormal(0, -1)", "σ must be positive"),
            ("uniform(1, 0)", "a must be less than b (they are 1 and 0)"),
            ("beta(1, 0)", "β must be positive"),
            ("lkj(0)", "η must be positive"),
            (
                "normal(0)",
                "`normal` takes 2 arguments (μ, σ), and 1 is given",
            ),
            ("flat(1)", "`flat` takes no arguments"),
            ("normal(0, sigma = 1)", "without names"),
            ("normal(a, 1)", "are numbers"),
            ("horseshoe(1)", "not supported yet"),
            ("wishart(1)", "not a prior on the menu"),
        ];
        for (text, want) in cases {
            let err = Prior::parse(text).unwrap_err();
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    fn slot(class: &str, kind: ClassKind, default: DefaultPrior) -> ClassSlot {
        ClassSlot {
            class: class.to_owned(),
            dpar: String::new(),
            group: String::new(),
            coefs: Vec::new(),
            kind,
            default,
            nlpar: String::new(),
            resp: String::new(),
            response: 0,
        }
    }

    fn row(class: &str, coef: &str, prior: &str) -> PriorRow {
        PriorRow {
            class: class.into(),
            coef: coef.into(),
            prior: prior.into(),
            ..PriorRow::default()
        }
    }

    #[test]
    fn a_prior_is_allowed_on_its_classes_only() {
        let real = row("b", "", "");
        let check = |p: &str, class: &str, kind: ClassKind, r: &PriorRow| {
            check_allowed(&Prior::parse(p).unwrap(), class, kind, r, true)
        };
        assert!(
            check("gamma(1, 1)", "b", ClassKind::Real, &real)
                .unwrap_err()
                .contains("positive numbers only")
        );
        assert!(check("gamma(1, 1)", "sd", ClassKind::Positive, &real).is_ok());
        assert!(check("normal(0, 1)", "sd", ClassKind::Positive, &real).is_ok());
        assert!(
            check("flat", "sd", ClassKind::Positive, &real)
                .unwrap_err()
                .contains("`b` only")
        );
        assert!(
            check("flat", "Intercept", ClassKind::Real, &real)
                .unwrap_err()
                .contains("centred intercept")
        );
        assert!(check_allowed(&Prior::flat(), "Intercept", ClassKind::Real, &real, false).is_ok());
        assert!(
            check("normal(0, 1)", "cor", ClassKind::Correlation, &real)
                .unwrap_err()
                .contains("`lkj(η)`")
        );
        assert!(
            check("lkj(1)", "b", ClassKind::Real, &real)
                .unwrap_err()
                .contains("correlations")
        );
        assert!(
            check("beta(1, 1)", "b", ClassKind::Real, &real)
                .unwrap_err()
                .contains("bounded to (0, 1)")
        );
        let bounded = PriorRow {
            lb: Some(0.0),
            ..row("sd", "", "")
        };
        assert!(
            check("normal(0, 1)", "sd", ClassKind::Positive, &bounded)
                .unwrap_err()
                .contains("only be set on `b`")
        );
        let uniform = PriorRow {
            lb: Some(-1.0),
            ub: Some(1.0),
            ..row("b", "", "")
        };
        assert!(check("uniform(-1, 1)", "b", ClassKind::Real, &uniform).is_ok());
        assert!(
            check("uniform(-1, 2)", "b", ClassKind::Real, &uniform)
                .unwrap_err()
                .contains("`lb = -1`, `ub = 2`")
        );
    }

    #[test]
    fn a_template_matches_level_names_once_the_levels_are_known() {
        let t = CoefTemplate {
            parts: vec![
                CoefPart::Exact("area".into()),
                CoefPart::Level("region".into()),
            ],
        };
        assert_eq!(t.display(), "area:region…");
        assert_eq!(t.matches("area:regionNorth"), Match::Possible);
        assert_eq!(t.matches("area:region"), Match::No);
        assert_eq!(t.matches("area"), Match::No);
        assert_eq!(CoefTemplate::exact("x").matches("x"), Match::Exact);
        let u = CoefTemplate {
            parts: vec![CoefPart::Unknown("g".into())],
        };
        assert_eq!(u.matches("g"), Match::Possible);
        assert_eq!(u.matches("gTRUE"), Match::Possible);
    }

    #[test]
    fn precedence_is_coefficient_then_class_and_unmatched_rows_are_kept() {
        let mut b = slot("b", ClassKind::Real, DefaultPrior::Flat);
        b.coefs = vec![Coef::Named("x".into()), Coef::Named("z".into())];
        let mut sd = slot("sd", ClassKind::Positive, DefaultPrior::ResponseScale);
        sd.group = "g".into();
        sd.coefs = vec![Coef::Named("Intercept".into())];
        let sigma = slot("sigma", ClassKind::Positive, DefaultPrior::ResponseScale);
        let slots = [b, sd, sigma];
        let rows = [
            row("b", "", "normal(0, 2)"),
            row("b", "x", "normal(0, 10)"),
            row("b", "regionNorth", "normal(0, 1)"),
            row("sigma", "", "exponential(1)"),
        ];
        let table = resolve(&slots, &rows, None, "y", true);
        let find = |class: &str, coef: &str| {
            table
                .rows
                .iter()
                .find(|r| r.class == class && r.coef == coef)
                .unwrap()
        };
        assert_eq!(find("b", "x").prior, "normal(0, 10)");
        assert_eq!(find("b", "x").source, Source::Explicit);
        assert_eq!(find("b", "z").prior, "normal(0, 2)");
        assert!(matches!(find("b", "z").source, Source::Inherited { .. }));
        assert_eq!(find("sigma", "").prior, "exponential(1)");
        // Before the data: the rule, and 2.5 standing in.
        let sd_row = find("sd", "Intercept");
        assert_eq!(sd_row.prior, "student_t(3, 0, 2.5)");
        assert_eq!(sd_row.source, Source::DefaultFromData);
        assert_eq!(
            sd_row.rule.as_deref(),
            Some("student_t(3, 0, scale) of `y`")
        );
        // The row for a coefficient no term gives is kept, with its reason.
        assert_eq!(table.unmatched.len(), 1);
        assert_eq!(table.unmatched[0].row, rows[2]);
        assert!(table.unmatched[0].reason.contains("`regionNorth`"));
        assert!(table.require_matched().is_err());

        // After the data: the numbers.
        let table = resolve(
            &slots,
            &rows[..2],
            Some(DataScale::of(&[1.0, 2.0, 30.0])),
            "y",
            true,
        );
        assert_eq!(
            find_in(&table, "sd", "Intercept").prior,
            "student_t(3, 0, 2.5)"
        );
        assert!(find_in(&table, "sd", "Intercept").rule.is_none());
    }

    fn find_in<'a>(table: &'a PriorTable, class: &str, coef: &str) -> &'a TableRow {
        table
            .rows
            .iter()
            .find(|r| r.class == class && r.coef == coef)
            .unwrap()
    }

    #[test]
    fn the_data_scale_is_the_median_and_a_floored_mad() {
        let s = DataScale::of(&[1.0, 2.0, 3.0, 4.0, 100.0]);
        assert_eq!(s.location, 3.0);
        assert_eq!(s.scale, 2.5);
        let wide = DataScale::of(&[0.0, 10.0, 20.0, 30.0, 40.0]);
        // MAD = 1.4826 × median(|x − 20|) = 1.4826 × 10.
        assert_eq!(wide.location, 20.0);
        assert_eq!(wide.scale, 14.8);
        assert_eq!(DataScale::of(&[]), DataScale::default());
    }

    #[test]
    fn pasted_rows_replace_rows_with_their_target() {
        let existing = vec![
            row("b", "x", "normal(0, 1)"),
            row("sigma", "", "exponential(1)"),
        ];
        let pasted = vec![row("b", "x", "normal(0, 5)"), row("b", "", "normal(0, 2)")];
        let (merged, replaced) = merge_rows(&existing, &pasted);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].prior, "normal(0, 5)");
        assert_eq!(replaced, vec![row("b", "x", "normal(0, 1)")]);
    }
}
