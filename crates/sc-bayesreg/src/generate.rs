//! The generator (TODO-bayesian-regression §4, R1.5, R2): a [`Structure`] → the
//! Stan program's text. Every chunk is a function of the structure alone —
//! never of a name or a size — so structurally equal models are byte-identical
//! programs and share one compiled executable (§5).
//!
//! The constructions are the textbook ones, each from a published source:
//!
//! - **Group-level effects**, non-centred (Stan User's Guide, "Multivariate
//!   priors for hierarchical models"; Papaspiliopoulos, Roberts & Sköld 2007):
//!   `z ~ std_normal()`, `r = (diag_pre_multiply(sd, L) * z)'` with
//!   `L ~ lkj_corr_cholesky(η)` (Lewandowski, Kurowicka & Joe 2009) when the
//!   block is correlated, `r = diag_pre_multiply(sd, z)'` when it is not. With
//!   `gr(by = h)` each level of `h` has its own SDs and correlations, and a
//!   group level's effects use those of the level of `h` its rows are in.
//! - **Centring**: the population-level predictors are centred in
//!   `transformed data` and the intercept is a parameter on that scale — "the
//!   expected response when all predictors are at their means", which is what
//!   brms documents its `Intercept` prior as being on (`?brmsformula`,
//!   `center`). The intercept on the original scale is recovered in generated
//!   quantities as `Intercept_c − means_X · b` (an ordinal model's thresholds
//!   as `Intercept_c + means_X · b`, since they enter as `threshold − η`).
//! - **Densities and their parameterisations**: the Stan Functions Reference;
//!   the mean parameterisations of the families are brms's documented ones
//!   (`vignette("brms_families")`): `Gamma` as `gamma(shape, shape / μ)`,
//!   `weibull` with scale `μ / Γ(1 + 1/shape)`, `exponential` with rate
//!   `1/μ`, `Beta` as `beta(μφ, (1 − μ)φ)`, `geometric` as a negative
//!   binomial with shape 1, and `skew_normal` with mean μ and SD σ (Azzalini
//!   1985: δ = α/√(1+α²), ω = σ/√(1 − 2δ²/π), ξ = μ − ωδ√(2/π)). The R2
//!   families: a zero-inflated density mixes a point mass at zero with the
//!   base density, a hurdle density puts every zero in the point mass and
//!   renormalises the base above zero (Mullahy 1986), the ex-Gaussian is
//!   `exp_mod_normal(μ − β, σ, 1/β)` (mean μ), the asymmetric Laplace's
//!   density is `q(1 − q)/σ · exp(−ρ_q((y − μ)/σ))` with the check function
//!   ρ_q(u) = u(q − 1[u < 0]) (Yu & Moyeed 2001), von Mises's mean is `2
//!   atan(η)` under the `tan_half` link.
//! - **Ordinal models** (Bürkner & Vuorre 2019, *AMPPS* 2(1)): with F the
//!   link's CDF and `d` the discrimination, cumulative P(y ≤ k) = F(d(τ_k −
//!   η)); stopping ratio P(y = k | y ≥ k) = F(d(τ_k − η)); continuation ratio
//!   P(y > k | y ≥ k) = F(d(η − τ_k)); adjacent category P(y = k) ∝ ∏_{j<k}
//!   F(d(η − τ_j)) ∏_{j≥k} (1 − F(d(η − τ_j))). A category-specific effect
//!   (`cs()`) moves each threshold by its own coefficient.
//! - **Monotonic effects** (Bürkner & Charpentier 2020, *BJMSP* 73(3)): `b · D ·
//!   Σ_{i ≤ x} ζ_i` with ζ a simplex of D steps and a Dirichlet prior.
//! - **Censoring and truncation** (Stan User's Guide, "Truncated or censored
//!   data"): a row censored on the left contributes its log CDF, on the right
//!   its log complementary CDF (for whole numbers, of `y − 1`: the value is at
//!   least `y`), an interval `(y, y2]` the log of the CDFs' difference; a
//!   truncated row's density is divided by the mass between its bounds.
//! - **Residual correlations** between gaussian responses: one multivariate
//!   normal with covariance `diag(σ) Ω diag(σ)`, Ω's Cholesky factor LKJ
//!   (Stan User's Guide, "Multivariate outcomes").
//! - **Priors as data** (§2.5): a prior is a code and three numbers, and the
//!   program branches on the code in one function, `prior_lpdf`. A prior on a
//!   bounded parameter is truncated at the bound; the truncation's constant
//!   depends only on the prior's numbers, so it is left out of the density.
//!
//! The variable names are this crate's own and positional (§4): `N`, `Y`;
//! per linear predictor *p* `K_p`, `X_p`, `b_p`, `Intercept_p`; per block *k*
//! `N_k`, `J_k`, `M_k_j` and `Z_k_j` per part *j*, `sd_k`, `z_k`, `L_k`,
//! `r_k`, `Cor_k`; the prior data `prior_<parameter>_code` and
//! `prior_<parameter>_args`. A later response of a multivariate model has its
//! own with a suffix: `Y_2`, `sigma_2`.

use crate::canon::{ArgShape, CoefShape, InterceptShape, Structure, suffix};
use crate::family::Kind;

/// The program's path: fixed, because the compile cache hashes it (§5).
pub const PROGRAM_PATH: &str = "model.stan";

/// A parameter whose prior the program reads as data: `prior_<parameter>_code`
/// and `prior_<parameter>_args` (`prior_cor_<k>_eta` for a correlation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorVariable {
    /// The parameter it is the prior of: `b_1`, `Intercept_1`, `sd_2`,
    /// `sigma`, `cor_1`, `rescor`.
    pub parameter: String,
    /// Whether it is a vector (one prior per element) or a scalar.
    pub vector: bool,
}

/// The Stan program for `structure`.
pub fn generate(structure: &Structure) -> String {
    Gen::new(structure).program()
}

/// The prior variables `structure`'s program declares, in order.
pub fn prior_variables(structure: &Structure) -> Vec<PriorVariable> {
    let mut out = Vec::new();
    let v = |parameter: String, vector: bool| PriorVariable { parameter, vector };
    for (i, p) in structure.predictors.iter().enumerate() {
        let n = i + 1;
        if p.b != CoefShape::None {
            out.push(v(format!("b_{n}"), true));
        }
        if p.intercept != InterceptShape::None {
            out.push(v(format!("Intercept_{n}"), false));
        }
        if p.mo > 0 {
            out.push(v(format!("bsp_{n}"), true));
        }
        if p.cs {
            out.push(v(format!("bcs_{n}"), true));
        }
    }
    for (i, b) in structure.blocks.iter().enumerate() {
        let k = i + 1;
        out.push(v(format!("sd_{k}"), true));
        if b.correlated {
            out.push(v(format!("cor_{k}"), false));
        }
    }
    for r in 0..structure.responses() {
        for s in structure.sub(r).0 {
            if !s.fixed {
                out.push(v(format!("{}{}", s.dpar, suffix(r)), false));
            }
        }
    }
    if structure.rescor {
        out.push(v("rescor".to_owned(), false));
    }
    out
}

/// Whether an expression is written for all rows at once or for row `n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Vector,
    Row,
}

impl Form {
    fn div(self) -> &'static str {
        match self {
            Form::Vector => " ./ ",
            Form::Row => " / ",
        }
    }

    fn mul(self) -> &'static str {
        match self {
            Form::Vector => " .* ",
            Form::Row => " * ",
        }
    }
}

struct Gen<'s> {
    s: &'s Structure,
    out: String,
    /// Added to every line's indent: what is emitted again inside a block of
    /// generated quantities.
    base: usize,
    /// The response whose likelihood is being written.
    resp: usize,
    /// The rows a vectorised likelihood reads: `[J_sub]` for a subset.
    rows: String,
}

/// The inverse of `link`, applied to `x`.
fn inverse_link(link: &str, x: &str) -> String {
    match link {
        "identity" => x.to_owned(),
        "log" => format!("exp({x})"),
        "logit" => format!("inv_logit({x})"),
        "probit" => format!("Phi({x})"),
        "probit_approx" => format!("Phi_approx({x})"),
        "cloglog" => format!("inv_cloglog({x})"),
        "cauchit" => format!("atan({x}) / pi() + 0.5"),
        "inverse" => format!("inv({x})"),
        "sqrt" => format!("square({x})"),
        "softplus" => format!("log1p_exp({x})"),
        "logm1" => format!("1 + exp({x})"),
        "tan_half" => format!("2 * atan({x})"),
        other => format!("{other}({x})"),
    }
}

fn bounds(support: &str) -> &'static str {
    match support {
        "positive" => "<lower=0>",
        "above_one" => "<lower=1>",
        "unit" => "<lower=0, upper=1>",
        _ => "",
    }
}

/// An expression and whether it is a vector.
#[derive(Debug, Clone)]
struct E {
    t: String,
    v: bool,
}

impl E {
    fn new(t: impl Into<String>, v: bool) -> E {
        E { t: t.into(), v }
    }

    fn mul(&self, o: &E) -> E {
        let op = if self.v && o.v { " .* " } else { " * " };
        E::new(format!("{}{op}{}", self.t, o.t), self.v || o.v)
    }

    fn div(&self, o: &E) -> E {
        let op = if o.v { " ./ " } else { " / " };
        E::new(format!("{}{op}{}", self.t, o.t), self.v || o.v)
    }

    fn paren(&self) -> E {
        E::new(format!("({})", self.t), self.v)
    }
}

impl<'s> Gen<'s> {
    fn new(s: &'s Structure) -> Gen<'s> {
        Gen {
            s,
            out: String::new(),
            base: 0,
            resp: 0,
            rows: String::new(),
        }
    }

    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent + self.base {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn kind(&self) -> Kind {
        self.s.kind()
    }

    fn integer(&self) -> bool {
        self.s.integer_response()
    }

    /// The type of one response value, for a declaration: `int`, `real`.
    fn y_type(&self) -> &'static str {
        if self.integer() { "int" } else { "real" }
    }

    /// The declaration of a per-row variable of the response's type.
    fn y_decl(&self, name: &str, constraint: &str) -> String {
        self.y_decl_n(name, constraint, "N")
    }

    fn y_decl_n(&self, name: &str, constraint: &str, n: &str) -> String {
        if self.integer() {
            format!("array[{n}] int{constraint} {name};")
        } else {
            format!("vector{constraint}[{n}] {name};")
        }
    }

    /// The categories but the first, of a categorical response.
    fn ncat1(&self) -> &'static str {
        "ncat - 1"
    }

    /// The size of block `k`'s effects as Stan text: `M_k_1`, or `M_k` for a
    /// block over several linear predictors (summed in transformed data) or
    /// of a categorical response (one set per category but the first).
    fn m(&self, k: usize) -> String {
        if self.s.blocks[k - 1].parts.len() == 1 && self.kind() != Kind::Categorical {
            format!("M_{k}_1")
        } else {
            format!("M_{k}")
        }
    }

    /// The sum of block `k`'s part sizes, for a data-block size.
    fn m_sum(&self, k: usize) -> String {
        (1..=self.s.blocks[k - 1].parts.len())
            .map(|j| format!("M_{k}_{j}"))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// Whether `dpar` of response `r` has a linear predictor (and so is a
    /// vector).
    fn varies_in(&self, r: usize, dpar: &str) -> bool {
        self.s
            .predictors
            .iter()
            .any(|p| p.dpar == dpar && p.resp == r)
            || (dpar == "mu" && r == 0 && self.s.nl.is_some())
    }

    /// Whether `dpar` of the current response varies.
    fn varies(&self, dpar: &str) -> bool {
        self.varies_in(self.resp, dpar)
    }

    /// `dpar` of the current response in `form`: `sigma`, `sigma[n]`,
    /// `sigma_2`, `mu[J_sub]`.
    fn dp(&self, dpar: &str, form: Form) -> String {
        let name = format!("{dpar}{}", suffix(self.resp));
        if !self.varies(dpar) {
            return name;
        }
        match form {
            Form::Row => format!("{name}[n]"),
            Form::Vector => format!("{name}{}", self.rows),
        }
    }

    /// A per-row or constant data variable of the current response in
    /// `form`.
    fn arg(&self, name: &str, shape: Option<ArgShape>, form: Form) -> String {
        let name = format!("{name}{}", suffix(self.resp));
        match (shape, form) {
            (Some(ArgShape::Column), Form::Row) => format!("{name}[n]"),
            _ => name,
        }
    }

    /// The current response's variable `name`.
    fn own(&self, name: &str) -> String {
        format!("{name}{}", suffix(self.resp))
    }

    fn program(mut self) -> String {
        let json = self.s.canonical_json();
        self.line(
            0,
            &format!(
                "// Generated by feldspar's bayesian_regression provider (generator {}).",
                self.s.generator
            ),
        );
        self.line(
            0,
            "// The program depends only on the model's structure: names, counts and prior",
        );
        self.line(0, "// numbers are data.");
        self.line(0, &format!("// structure: {json}"));
        self.functions();
        self.data();
        self.transformed_data();
        self.parameters();
        self.transformed_parameters();
        self.model();
        self.generated_quantities();
        self.out
    }

    fn functions(&mut self) {
        self.line(0, "functions {");
        self.line(
            1,
            "// The prior menu (code -> density); a prior on a bounded parameter is truncated",
        );
        self.line(
            1,
            "// there, and the truncation's constant (data) is left out.",
        );
        self.line(1, "real prior_lpdf(real x, int code, array[] real a) {");
        let menu: [(u8, &str); 12] = [
            (0, "0"),
            (1, "normal_lpdf(x | a[1], a[2])"),
            (2, "student_t_lpdf(x | a[1], a[2], a[3])"),
            (3, "cauchy_lpdf(x | a[1], a[2])"),
            (4, "double_exponential_lpdf(x | a[1], a[2])"),
            (5, "logistic_lpdf(x | a[1], a[2])"),
            (6, "exponential_lpdf(x | a[1])"),
            (7, "gamma_lpdf(x | a[1], a[2])"),
            (8, "inv_gamma_lpdf(x | a[1], a[2])"),
            (9, "lognormal_lpdf(x | a[1], a[2])"),
            (10, "uniform_lpdf(x | a[1], a[2])"),
            (11, "beta_lpdf(x | a[1], a[2])"),
        ];
        for (i, (code, density)) in menu.iter().enumerate() {
            let keyword = if i == 0 { "if" } else { "} else if" };
            self.line(2, &format!("{keyword} (code == {code}) {{"));
            self.line(3, &format!("return {density};"));
        }
        self.line(2, "}");
        self.line(2, "reject(\"no prior has the code \", code);");
        self.line(1, "}");
        self.line(
            1,
            "real priors_lpdf(vector x, array[] int code, array[,] real a) {",
        );
        self.line(2, "real lp = 0;");
        self.line(2, "for (i in 1:rows(x)) {");
        self.line(3, "lp += prior_lpdf(x[i] | code[i], a[i]);");
        self.line(2, "}");
        self.line(2, "return lp;");
        self.line(1, "}");
        if self.s.predictors.iter().any(|p| p.mo > 0) {
            self.line(
                1,
                "// A monotonic effect: D times the steps' simplex summed up to each row's level.",
            );
            self.line(1, "vector mo_effect(vector simo, array[] int x) {");
            self.line(
                2,
                "vector[rows(simo) + 1] steps = append_row(0, cumulative_sum(simo));",
            );
            self.line(2, "return rows(simo) * steps[x];");
            self.line(1, "}");
        }
        if self.s.family == "asym_laplace" {
            self.line(
                1,
                "// The asymmetric Laplace with location mu, scale sigma and quantile q.",
            );
            self.line(
                1,
                "real asym_laplace_lpdf(real y, real mu, real sigma, real q) {",
            );
            self.line(2, "real u = (y - mu) / sigma;");
            self.line(
                2,
                "return log(q) + log1m(q) - log(sigma) - u * (q - (u < 0));",
            );
            self.line(1, "}");
        }
        if self.kind() == Kind::Ordinal {
            self.ordinal_functions();
        }
        self.line(0, "}");
    }

    /// The link's log CDF and log complementary CDF, and the family's log
    /// probability of each category.
    fn ordinal_functions(&mut self) {
        let link = self.s.mu_link().to_owned();
        let (lf, l1mf) = match link.as_str() {
            "probit" => ("std_normal_lcdf(x)", "std_normal_lccdf(x)"),
            "probit_approx" => ("log(Phi_approx(x))", "log1m(Phi_approx(x))"),
            "cloglog" => ("log1m_exp(-exp(x))", "-exp(x)"),
            "cauchit" => ("cauchy_lcdf(x | 0, 1)", "cauchy_lccdf(x | 0, 1)"),
            _ => ("log_inv_logit(x)", "log1m_inv_logit(x)"),
        };
        self.line(
            1,
            &format!("// The {link} link's log CDF and its complement."),
        );
        self.line(1, "real ord_lF(real x) {");
        self.line(2, &format!("return {lf};"));
        self.line(1, "}");
        self.line(1, "real ord_l1mF(real x) {");
        self.line(2, &format!("return {l1mf};"));
        self.line(1, "}");
        self.line(
            1,
            &format!(
                "// The log probability of each category of the {} model.",
                self.s.family
            ),
        );
        self.line(1, "vector ord_lprobs(real eta, vector thres, real disc) {");
        self.line(2, "int K = rows(thres) + 1;");
        self.line(2, "vector[K] lp;");
        match self.s.family.as_str() {
            "cumulative" => {
                self.line(2, "lp[1] = ord_lF(disc * (thres[1] - eta));");
                self.line(2, "for (k in 2:(K - 1)) {");
                self.line(
                    3,
                    "lp[k] = log_diff_exp(ord_lF(disc * (thres[k] - eta)), \
                     ord_lF(disc * (thres[k - 1] - eta)));",
                );
                self.line(2, "}");
                self.line(2, "lp[K] = ord_l1mF(disc * (thres[K - 1] - eta));");
            }
            "sratio" | "cratio" => {
                let (x, hit, pass) = if self.s.family == "sratio" {
                    ("disc * (thres[k] - eta)", "ord_lF(x)", "ord_l1mF(x)")
                } else {
                    ("disc * (eta - thres[k])", "ord_l1mF(x)", "ord_lF(x)")
                };
                self.line(2, "real passed = 0;");
                self.line(2, "for (k in 1:(K - 1)) {");
                self.line(3, &format!("real x = {x};"));
                self.line(3, &format!("lp[k] = passed + {hit};"));
                self.line(3, &format!("passed += {pass};"));
                self.line(2, "}");
                self.line(2, "lp[K] = passed;");
            }
            _ => {
                self.line(2, "vector[K - 1] up;");
                self.line(2, "vector[K - 1] down;");
                self.line(2, "for (j in 1:(K - 1)) {");
                self.line(3, "real x = disc * (eta - thres[j]);");
                self.line(3, "up[j] = ord_lF(x);");
                self.line(3, "down[j] = ord_l1mF(x);");
                self.line(2, "}");
                self.line(2, "for (k in 1:K) {");
                self.line(3, "lp[k] = sum(up[1:(k - 1)]) + sum(down[k:(K - 1)]);");
                self.line(2, "}");
                self.line(2, "lp -= log_sum_exp(lp);");
            }
        }
        self.line(2, "return lp;");
        self.line(1, "}");
    }

    #[allow(clippy::too_many_lines)] // every data variable, in order
    fn data(&mut self) {
        self.line(0, "data {");
        self.line(1, "int<lower=1> N;");
        for r in 0..self.s.responses() {
            self.resp = r;
            self.response_data(r);
        }
        self.resp = 0;
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            if p.b != CoefShape::None {
                self.line(1, &format!("int<lower=0> K_{n};"));
                self.line(1, &format!("matrix[N, K_{n}] X_{n};"));
                self.line(1, &format!("array[K_{n}] int prior_b_{n}_code;"));
                self.line(1, &format!("array[K_{n}, 3] real prior_b_{n}_args;"));
                if p.b == CoefShape::Bounded {
                    self.line(1, &format!("vector[K_{n}] lb_b_{n};"));
                    self.line(1, &format!("vector[K_{n}] ub_b_{n};"));
                }
            }
            if p.offset {
                self.line(1, &format!("int<lower=1> K_O_{n};"));
                self.line(1, &format!("matrix[N, K_O_{n}] O_{n};"));
            }
            if p.intercept != InterceptShape::None {
                self.line(1, &format!("int prior_Intercept_{n}_code;"));
                self.line(1, &format!("array[3] real prior_Intercept_{n}_args;"));
            }
            for j in 1..=p.mo {
                self.line(1, &format!("int<lower=2> Lmo_{n}_{j};"));
                self.line(
                    1,
                    &format!("array[N] int<lower=1, upper=Lmo_{n}_{j}> Xmo_{n}_{j};"),
                );
                self.line(
                    1,
                    &format!("vector<lower=0>[Lmo_{n}_{j} - 1] con_simo_{n}_{j};"),
                );
            }
            if p.mo > 0 {
                self.line(1, &format!("array[{}] int prior_bsp_{n}_code;", p.mo));
                self.line(1, &format!("array[{}, 3] real prior_bsp_{n}_args;", p.mo));
            }
            if p.cs {
                self.line(1, &format!("int<lower=1> Kcs_{n};"));
                self.line(1, &format!("matrix[N, Kcs_{n}] Xcs_{n};"));
                self.line(1, &format!("array[Kcs_{n}] int prior_bcs_{n}_code;"));
                self.line(1, &format!("array[Kcs_{n}, 3] real prior_bcs_{n}_args;"));
            }
        }
        if let Some(nl) = &self.s.nl {
            for j in 1..=nl.covariates {
                self.line(1, &format!("vector[N] C_{j};"));
            }
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            self.line(1, &format!("int<lower=1> N_{k};"));
            self.line(1, &format!("array[N] int<lower=1, upper=N_{k}> J_{k};"));
            for j in 1..=b.parts.len() {
                self.line(1, &format!("int<lower=1> M_{k}_{j};"));
                self.line(1, &format!("matrix[N, M_{k}_{j}] Z_{k}_{j};"));
            }
            if b.by {
                self.line(1, &format!("int<lower=1> Nby_{k};"));
                self.line(1, &format!("array[N] int<lower=1, upper=Nby_{k}> Jby_{k};"));
            }
            let size = self.m_sum(k);
            self.line(1, &format!("array[{size}] int prior_sd_{k}_code;"));
            self.line(1, &format!("array[{size}, 3] real prior_sd_{k}_args;"));
            if b.correlated {
                self.line(1, &format!("real<lower=0> prior_cor_{k}_eta;"));
            }
        }
        for r in 0..self.s.responses() {
            let suf = suffix(r);
            for s in &self.s.sub(r).0.to_vec() {
                if s.fixed {
                    self.line(1, &format!("real{} {}{suf};", bounds(&s.support), s.dpar));
                } else {
                    self.line(1, &format!("int prior_{}{suf}_code;", s.dpar));
                    self.line(1, &format!("array[3] real prior_{}{suf}_args;", s.dpar));
                }
            }
        }
        if self.s.rescor {
            self.line(1, "real<lower=0> prior_rescor_eta;");
        }
        self.line(0, "}");
    }

    /// Response `r`'s own data: its values and its addition terms.
    fn response_data(&mut self, r: usize) {
        let (_, resp) = self.s.sub(r);
        let resp = resp.clone();
        let suf = suffix(r);
        if resp.subset {
            self.line(1, &format!("int<lower=0> N_sub{suf};"));
            self.line(
                1,
                &format!("array[N_sub{suf}] int<lower=1, upper=N> J_sub{suf};"),
            );
            let y = self.y_decl_n(&format!("Y{suf}"), "", &format!("N_sub{suf}"));
            self.line(1, &y);
            return;
        }
        match self.kind() {
            Kind::Ordinal => {
                self.line(1, &format!("int<lower=1> nthres{suf};"));
                self.line(
                    1,
                    &format!("array[N] int<lower=1, upper=nthres{suf} + 1> Y{suf};"),
                );
            }
            Kind::Categorical => {
                self.line(1, &format!("int<lower=2> ncat{suf};"));
                self.line(
                    1,
                    &format!("array[N] int<lower=1, upper=ncat{suf}> Y{suf};"),
                );
            }
            Kind::Standard => {
                let y = self.y_decl(&format!("Y{suf}"), "");
                self.line(1, &y);
            }
        }
        if let Some(w) = resp.weights {
            self.line(
                1,
                &match w {
                    ArgShape::Column => format!("vector<lower=0>[N] weights{suf};"),
                    ArgShape::Constant => format!("real<lower=0> weights{suf};"),
                },
            );
        }
        if let Some(t) = resp.trials {
            self.line(
                1,
                &match t {
                    ArgShape::Column => format!("array[N] int<lower=0> trials{suf};"),
                    ArgShape::Constant => format!("int<lower=0> trials{suf};"),
                },
            );
        }
        if let Some(interval) = resp.cens {
            self.line(1, &format!("array[N] int<lower=-1, upper=2> cens{suf};"));
            if interval {
                let y2 = self.y_decl(&format!("Y2{suf}"), "");
                self.line(1, &y2);
            }
        }
        for (name, shape) in [("lb_Y", resp.trunc_lb), ("ub_Y", resp.trunc_ub)] {
            if let Some(shape) = shape {
                let name = format!("{name}{suf}");
                let decl = match shape {
                    ArgShape::Column => self.y_decl(&name, ""),
                    ArgShape::Constant => format!("{} {name};", self.y_type()),
                };
                self.line(1, &decl);
            }
        }
        if resp.se.is_some() {
            self.line(1, &format!("vector<lower=0>[N] se{suf};"));
        }
        if let Some(rate) = resp.rate {
            self.line(
                1,
                &match rate {
                    ArgShape::Column => format!("vector<lower=0>[N] rate{suf};"),
                    ArgShape::Constant => format!("real<lower=0> rate{suf};"),
                },
            );
        }
    }

    fn transformed_data(&mut self) {
        let mut body = Vec::new();
        for (i, p) in self.s.predictors.iter().enumerate() {
            let n = i + 1;
            if p.intercept.centred() {
                body.push(format!("vector[K_{n}] means_X_{n};"));
                body.push(format!("matrix[N, K_{n}] Xc_{n};"));
            }
            if p.offset {
                body.push(format!(
                    "vector[N] offset_{n} = O_{n} * rep_vector(1, K_O_{n});"
                ));
            }
        }
        for (i, b) in self.s.blocks.iter().enumerate() {
            let k = i + 1;
            if self.kind() == Kind::Categorical {
                body.push(format!("int M_{k} = ({}) * (ncat - 1);", self.m_sum(k)));
            } else if b.parts.len() > 1 {
                body.push(format!("int M_{k} = {};", self.m_sum(k)));
            }
        }
        let r = self.s.responses();
        if self.s.rescor {
            body.push(format!("array[N] vector[{r}] Ymv;"));
        }
        let mut centring = Vec::new();
        for (i, p) in self.s.predictors.iter().enumerate() {
            let n = i + 1;
            if p.intercept.centred() {
                centring.push(format!("for (i in 1:K_{n}) {{"));
                centring.push(format!("  means_X_{n}[i] = mean(X_{n}[, i]);"));
                centring.push(format!("  Xc_{n}[, i] = X_{n}[, i] - means_X_{n}[i];"));
                centring.push("}".to_owned());
            }
        }
        if self.s.rescor {
            let ys = (0..r)
                .map(|q| format!("Y{}[n]", suffix(q)))
                .collect::<Vec<_>>()
                .join(", ");
            centring.push("for (n in 1:N) {".to_owned());
            centring.push(format!("  Ymv[n] = [{ys}]';"));
            centring.push("}".to_owned());
        }
        if body.is_empty() && centring.is_empty() {
            return;
        }
        self.line(0, "transformed data {");
        for l in body.iter().chain(&centring) {
            self.line(1, l);
        }
        self.line(0, "}");
    }

    /// The size of an ordinal model's thresholds for predictor `p`'s response.
    fn nthres(&self, resp: usize) -> String {
        format!("nthres{}", suffix(resp))
    }

    fn parameters(&mut self) {
        self.line(0, "parameters {");
        let categorical = self.kind() == Kind::Categorical;
        let ordered = if self.s.family == "cumulative" {
            "ordered"
        } else {
            "vector"
        };
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            match (p.b, categorical) {
                (CoefShape::None, _) => {}
                (_, true) => self.line(1, &format!("matrix[K_{n}, ncat - 1] b_{n};")),
                (CoefShape::Free, false) => self.line(1, &format!("vector[K_{n}] b_{n};")),
                (CoefShape::Bounded, false) => self.line(
                    1,
                    &format!("vector<lower=lb_b_{n}, upper=ub_b_{n}>[K_{n}] b_{n};"),
                ),
            }
            let nthres = self.nthres(p.resp);
            match (p.intercept, categorical) {
                (InterceptShape::None, _) => {}
                (InterceptShape::Centred, true) => {
                    self.line(1, &format!("vector[ncat - 1] Intercept_c_{n};"));
                }
                (InterceptShape::Plain, true) => {
                    self.line(1, &format!("vector[ncat - 1] Intercept_{n};"));
                }
                (InterceptShape::Centred, false) => {
                    self.line(1, &format!("real Intercept_c_{n};"));
                }
                (InterceptShape::Plain, false) => self.line(1, &format!("real Intercept_{n};")),
                (InterceptShape::Thresholds, _) => {
                    self.line(1, &format!("{ordered}[{nthres}] Intercept_c_{n};"));
                }
                (InterceptShape::ThresholdsPlain, _) => {
                    self.line(1, &format!("{ordered}[{nthres}] Intercept_{n};"));
                }
            }
            if p.mo > 0 {
                self.line(1, &format!("vector[{}] bsp_{n};", p.mo));
                for j in 1..=p.mo {
                    self.line(1, &format!("simplex[Lmo_{n}_{j} - 1] simo_{n}_{j};"));
                }
            }
            if p.cs {
                self.line(1, &format!("matrix[Kcs_{n}, {nthres}] bcs_{n};"));
            }
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            let m = self.m(k);
            if b.by {
                self.line(1, &format!("matrix<lower=0>[{m}, Nby_{k}] sd_{k};"));
            } else {
                self.line(1, &format!("vector<lower=0>[{m}] sd_{k};"));
            }
            self.line(1, &format!("matrix[{m}, N_{k}] z_{k};"));
            if b.correlated {
                if b.by {
                    self.line(
                        1,
                        &format!("array[Nby_{k}] cholesky_factor_corr[{m}] L_{k};"),
                    );
                } else {
                    self.line(1, &format!("cholesky_factor_corr[{m}] L_{k};"));
                }
            }
        }
        for r in 0..self.s.responses() {
            let suf = suffix(r);
            for s in &self.s.sub(r).0.to_vec() {
                if !s.fixed {
                    self.line(1, &format!("real{} {}{suf};", bounds(&s.support), s.dpar));
                }
            }
        }
        if self.s.rescor {
            self.line(
                1,
                &format!("cholesky_factor_corr[{}] Lrescor;", self.s.responses()),
            );
        }
        self.line(0, "}");
    }

    fn transformed_parameters(&mut self) {
        if self.s.blocks.is_empty() {
            return;
        }
        self.line(0, "transformed parameters {");
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            let m = self.m(k);
            if b.by {
                // Each group level with the SDs (and correlations) of the
                // level of `by` its rows are in.
                self.line(
                    1,
                    &format!(
                        "matrix[N_{k}, {m}] r_{k} = rep_matrix(0, N_{k}, {m});  // the group-level effects"
                    ),
                );
                self.line(1, "for (n in 1:N) {");
                if b.correlated {
                    self.line(
                        2,
                        &format!(
                            "r_{k}[J_{k}[n]] = (diag_pre_multiply(sd_{k}[, Jby_{k}[n]], \
                             L_{k}[Jby_{k}[n]]) * z_{k}[, J_{k}[n]])';"
                        ),
                    );
                } else {
                    self.line(
                        2,
                        &format!("r_{k}[J_{k}[n]] = (sd_{k}[, Jby_{k}[n]] .* z_{k}[, J_{k}[n]])';"),
                    );
                }
                self.line(1, "}");
                continue;
            }
            self.line(
                1,
                &format!("matrix[N_{k}, {m}] r_{k};  // the group-level effects"),
            );
            if b.correlated {
                self.line(
                    1,
                    &format!("r_{k} = (diag_pre_multiply(sd_{k}, L_{k}) * z_{k})';"),
                );
            } else {
                self.line(1, &format!("r_{k} = diag_pre_multiply(sd_{k}, z_{k})';"));
            }
        }
        self.line(0, "}");
    }

    /// The linear predictor of predictor `n` (from 1), as statements.
    fn eta(&mut self, n: usize) {
        let p = self.s.predictors[n - 1].clone();
        if self.kind() == Kind::Categorical && p.dpar == "mu" {
            self.eta_categorical(n);
            return;
        }
        let mut terms = Vec::new();
        match p.intercept {
            InterceptShape::Centred => terms.push(format!("Intercept_c_{n}")),
            InterceptShape::Plain => terms.push(format!("Intercept_{n}")),
            InterceptShape::None | InterceptShape::Thresholds | InterceptShape::ThresholdsPlain => {
            }
        }
        match (p.b, p.intercept.centred()) {
            (CoefShape::None, _) => {}
            (_, true) => terms.push(format!("Xc_{n} * b_{n}")),
            _ => terms.push(format!("X_{n} * b_{n}")),
        }
        if p.offset {
            terms.push(format!("offset_{n}"));
        }
        let has_intercept = matches!(p.intercept, InterceptShape::Centred | InterceptShape::Plain);
        let init = if terms.is_empty() {
            "rep_vector(0, N)".to_owned()
        } else if has_intercept && terms.len() == 1 {
            format!("rep_vector({}, N)", terms[0])
        } else {
            terms.join(" + ")
        };
        self.line(1, &format!("vector[N] eta_{n} = {init};"));
        for j in 1..=p.mo {
            self.line(
                1,
                &format!("eta_{n} += bsp_{n}[{j}] * mo_effect(simo_{n}_{j}, Xmo_{n}_{j});"),
            );
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            let Some(j) = b.parts.iter().position(|&at| at == n - 1) else {
                continue;
            };
            let j = j + 1;
            let effects = if b.parts.len() == 1 {
                format!("r_{k}[J_{k}]")
            } else {
                let start = (1..j)
                    .map(|q| format!("M_{k}_{q}"))
                    .fold("1".to_owned(), |a, s| format!("{a} + {s}"));
                let end = (1..=j)
                    .map(|q| format!("M_{k}_{q}"))
                    .collect::<Vec<_>>()
                    .join(" + ");
                format!("r_{k}[J_{k}, ({start}):({end})]")
            };
            self.line(
                1,
                &format!("eta_{n} += rows_dot_product({effects}, Z_{k}_{j});"),
            );
        }
    }

    /// A categorical response's linear predictors: one column per category
    /// but the first (the reference, whose is 0).
    fn eta_categorical(&mut self, n: usize) {
        let p = self.s.predictors[n - 1].clone();
        let intercept = match p.intercept {
            InterceptShape::Centred => Some(format!("Intercept_c_{n}")),
            InterceptShape::Plain => Some(format!("Intercept_{n}")),
            _ => None,
        };
        let mut terms = Vec::new();
        if let Some(i) = &intercept {
            terms.push(format!("rep_matrix({i}', N)"));
        }
        match (p.b, p.intercept.centred()) {
            (CoefShape::None, _) => {}
            (_, true) => terms.push(format!("Xc_{n} * b_{n}")),
            _ => terms.push(format!("X_{n} * b_{n}")),
        }
        let init = if terms.is_empty() {
            "rep_matrix(0, N, ncat - 1)".to_owned()
        } else {
            terms.join(" + ")
        };
        self.line(1, &format!("matrix[N, ncat - 1] eta_{n} = {init};"));
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            if !b.parts.contains(&(n - 1)) {
                continue;
            }
            self.line(1, "for (c in 1:(ncat - 1)) {");
            self.line(
                2,
                &format!(
                    "eta_{n}[, c] += rows_dot_product(r_{k}[J_{k}, ((c - 1) * M_{k}_1 + 1):(c * \
                     M_{k}_1)], Z_{k}_1);"
                ),
            );
            self.line(1, "}");
        }
    }

    /// The family's density of row `n` or of every row: its Stan name (without
    /// the suffix) and its arguments after the response.
    #[allow(clippy::too_many_lines)] // one arm per family
    fn density(&self, form: Form, canonical: bool) -> (&'static str, Vec<String>) {
        let r = self.s.sub(self.resp).1;
        let mu_link = self.s.mu_link();
        let mu = self.dp("mu", form);
        let eta = match (self.mu_eta(), form) {
            (Some(e), Form::Vector) => format!("{e}{}", self.rows),
            (Some(e), Form::Row) => format!("{e}[n]"),
            (None, _) => mu.clone(),
        };
        let rate = r.rate.map(|s| self.arg("rate", Some(s), form));
        let scaled = |x: &str| match &rate {
            Some(rate) => format!("{x}{}{rate}", form.mul()),
            None => x.to_owned(),
        };
        let log_rated = |x: &str| match &rate {
            Some(rate) => format!("{x} + log({rate})"),
            None => x.to_owned(),
        };
        let residual_sd = |form: Form| {
            let sigma = self.dp("sigma", form);
            let se = self.arg("se", Some(ArgShape::Column), form);
            match r.se {
                None => sigma,
                Some(true) => format!("sqrt(square({sigma}) + square({se}))"),
                Some(false) => se,
            }
        };
        let canonical = canonical && self.mu_eta().is_some();
        let fam = self.s.family.as_str();
        match fam {
            "gaussian" => ("normal", vec![mu, residual_sd(form)]),
            "student" => (
                "student_t",
                vec![self.dp("nu", form), mu, residual_sd(form)],
            ),
            "lognormal" => ("lognormal", vec![mu, self.dp("sigma", form)]),
            "skew_normal" => {
                let at = |v: &str| match form {
                    Form::Vector => format!("{}{}", self.own(v), self.rows),
                    Form::Row => format!("{}[n]", self.own(v)),
                };
                (
                    "skew_normal",
                    vec![at("xi_sn"), at("omega_sn"), self.dp("alpha", form)],
                )
            }
            "bernoulli" if canonical && mu_link == "logit" => ("bernoulli_logit", vec![eta]),
            "bernoulli" => ("bernoulli", vec![mu]),
            "binomial" => {
                let trials = self.arg("trials", r.trials, form);
                if canonical && mu_link == "logit" {
                    ("binomial_logit", vec![trials, eta])
                } else {
                    ("binomial", vec![trials, mu])
                }
            }
            "poisson" if canonical && mu_link == "log" => ("poisson_log", vec![log_rated(&eta)]),
            "poisson" => ("poisson", vec![scaled(&mu)]),
            "negbinomial" => {
                let shape = scaled(&self.dp("shape", form));
                if canonical && mu_link == "log" {
                    ("neg_binomial_2_log", vec![log_rated(&eta), shape])
                } else {
                    ("neg_binomial_2", vec![scaled(&mu), shape])
                }
            }
            "geometric" => {
                let shape = rate.clone().unwrap_or_else(|| "1".to_owned());
                if canonical && mu_link == "log" {
                    ("neg_binomial_2_log", vec![log_rated(&eta), shape])
                } else {
                    ("neg_binomial_2", vec![scaled(&mu), shape])
                }
            }
            "Gamma" => {
                let shape = self.dp("shape", form);
                (
                    "gamma",
                    vec![shape.clone(), format!("{shape}{}{mu}", form.div())],
                )
            }
            "weibull" => {
                let shape = self.dp("shape", form);
                (
                    "weibull",
                    vec![
                        shape.clone(),
                        format!("{mu}{}tgamma(1 + inv({shape}))", form.div()),
                    ],
                )
            }
            "exponential" => ("exponential", vec![format!("inv({mu})")]),
            "Beta" => {
                let phi = self.dp("phi", form);
                (
                    "beta",
                    vec![
                        format!("{mu}{}{phi}", form.mul()),
                        format!("(1 - {mu}){}{phi}", form.mul()),
                    ],
                )
            }
            "beta_binomial" => {
                let phi = self.dp("phi", form);
                let trials = self.arg("trials", r.trials, form);
                (
                    "beta_binomial",
                    vec![
                        trials,
                        format!("{mu}{}{phi}", form.mul()),
                        format!("(1 - {mu}){}{phi}", form.mul()),
                    ],
                )
            }
            "von_mises" => ("von_mises", vec![mu, self.dp("kappa", form)]),
            "exgaussian" => {
                let beta = self.dp("beta", form);
                (
                    "exp_mod_normal",
                    vec![
                        format!("{mu} - {beta}"),
                        self.dp("sigma", form),
                        format!("inv({beta})"),
                    ],
                )
            }
            // The family table holds exactly the families above, and the
            // mixtures, written row by row in `row_lp`.
            _ => ("normal", vec![mu, self.dp("sigma", form)]),
        }
    }

    /// The linear predictor `mu` comes from for the canonical densities, or
    /// `None` when `mu` is not a linear predictor's (non-linear).
    fn mu_eta(&self) -> Option<String> {
        let n = self
            .s
            .predictors
            .iter()
            .position(|p| p.dpar == "mu" && p.resp == self.resp)?;
        Some(format!("eta_{}", n + 1))
    }

    /// Whether the current family is written row by row, with a statement of
    /// its own (a mixture of a point mass and a density, an ordinal or
    /// categorical model, the asymmetric Laplace).
    fn by_row(&self) -> bool {
        self.kind() != Kind::Standard
            || self.s.family.starts_with("zero_")
            || self.s.family.starts_with("hurdle_")
            || self.s.family == "asym_laplace"
    }

    /// Row `n`'s log likelihood for a family written row by row.
    fn row_lp(&self) -> String {
        let y = format!("{}[n]", self.own("Y"));
        let d = |x: &str| self.dp(x, Form::Row);
        let mu = d("mu");
        let beta =
            |mu: &str, phi: &str| format!("beta_lpdf({y} | {mu} * {phi}, (1 - {mu}) * {phi})");
        match self.s.family.as_str() {
            "zero_inflated_poisson" | "zero_inflated_negbinomial" => {
                let zi = d("zi");
                let base = |v: &str| {
                    if self.s.family == "zero_inflated_poisson" {
                        format!("poisson_lpmf({v} | {mu})")
                    } else {
                        format!("neg_binomial_2_lpmf({v} | {mu}, {})", d("shape"))
                    }
                };
                format!(
                    "{y} == 0 ? log_sum_exp(log({zi}), log1m({zi}) + {}) : log1m({zi}) + {}",
                    base("0"),
                    base(&y)
                )
            }
            "hurdle_poisson" => {
                let hu = d("hu");
                format!(
                    "{y} == 0 ? log({hu}) : log1m({hu}) + poisson_lpmf({y} | {mu}) - \
                     log1m_exp(-{mu})"
                )
            }
            "hurdle_negbinomial" => {
                let hu = d("hu");
                let shape = d("shape");
                format!(
                    "{y} == 0 ? log({hu}) : log1m({hu}) + neg_binomial_2_lpmf({y} | {mu}, \
                     {shape}) - log1m_exp(neg_binomial_2_lpmf(0 | {mu}, {shape}))"
                )
            }
            "hurdle_gamma" => {
                let hu = d("hu");
                let shape = d("shape");
                format!(
                    "{y} == 0 ? log({hu}) : log1m({hu}) + gamma_lpdf({y} | {shape}, {shape} / \
                     {mu})"
                )
            }
            "hurdle_lognormal" => {
                let hu = d("hu");
                format!(
                    "{y} == 0 ? log({hu}) : log1m({hu}) + lognormal_lpdf({y} | {mu}, {})",
                    d("sigma")
                )
            }
            "zero_inflated_beta" => {
                let zi = d("zi");
                format!(
                    "{y} == 0 ? log({zi}) : log1m({zi}) + {}",
                    beta(&mu, &d("phi"))
                )
            }
            "zero_one_inflated_beta" => {
                let zoi = d("zoi");
                let coi = d("coi");
                format!(
                    "{y} == 0 ? log({zoi}) + log1m({coi}) : ({y} == 1 ? log({zoi}) + log({coi}) \
                     : log1m({zoi}) + {})",
                    beta(&mu, &d("phi"))
                )
            }
            "asym_laplace" => format!(
                "asym_laplace_lpdf({y} | {mu}, {}, {})",
                d("sigma"),
                d("quantile")
            ),
            "categorical" => {
                let eta = self.mu_eta().unwrap_or_default();
                format!("categorical_logit_lpmf({y} | append_row(0, {eta}[n]'))")
            }
            _ => {
                // Ordinal.
                format!(
                    "ord_lprobs({}, {}, {})[{y}]",
                    self.ordinal_eta(),
                    self.thresholds_row(),
                    self.disc()
                )
            }
        }
    }

    /// An ordinal model's linear predictor at row `n`.
    fn ordinal_eta(&self) -> String {
        format!("{}[n]", self.mu_eta().unwrap_or_default())
    }

    /// An ordinal model's thresholds at row `n`: category-specific effects
    /// move each.
    fn thresholds_row(&self) -> String {
        let Some(at) = self
            .s
            .predictors
            .iter()
            .position(|p| p.dpar == "mu" && p.resp == self.resp)
        else {
            return String::new();
        };
        let n = at + 1;
        let p = &self.s.predictors[at];
        let thres = if p.intercept == InterceptShape::Thresholds {
            format!("Intercept_c_{n}")
        } else {
            format!("Intercept_{n}")
        };
        if p.cs {
            format!("{thres} - (Xcs_{n}[n] * bcs_{n})'")
        } else {
            thres
        }
    }

    /// An ordinal model's discrimination at row `n`: its formula's, fixed,
    /// or 1.
    fn disc(&self) -> String {
        if self.varies("disc") {
            self.dp("disc", Form::Row)
        } else if self.s.sub(self.resp).0.iter().any(|s| s.dpar == "disc") {
            self.own("disc")
        } else {
            "1".to_owned()
        }
    }

    /// The linear predictors and the distributional parameters they make, as
    /// statements: what the model block and an `epred` both start with.
    fn predictors(&mut self) {
        let n_pred = self.s.predictors.len();
        for n in 1..=n_pred {
            self.eta(n);
        }
        // Each linear predictor through its inverse link.
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            if self.kind() == Kind::Categorical && p.dpar == "mu" {
                continue;
            }
            let link = if self.kind() == Kind::Ordinal && p.dpar == "mu" {
                // The ordinal likelihood applies the link's CDF itself.
                "identity"
            } else {
                p.link.as_str()
            };
            let value = inverse_link(link, &format!("eta_{n}"));
            self.line(
                1,
                &format!("vector[N] {}{} = {value};", p.dpar, suffix(p.resp)),
            );
        }
        if let Some(nl) = self.s.nl.clone() {
            let expr = inverse_link(&nl.link, &nl.expr);
            let vector =
                self.s.predictors.iter().any(|p| p.dpar.starts_with("nlp_")) || nl.covariates > 0;
            if vector {
                self.line(1, &format!("vector[N] mu = {expr};"));
            } else {
                self.line(1, &format!("vector[N] mu = rep_vector({expr}, N);"));
            }
        }
        if self.s.family == "skew_normal" {
            for r in 0..self.s.responses() {
                let suf = suffix(r);
                self.line(
                    1,
                    &format!(
                        "vector[N] delta_sn{suf} = rep_vector(0, N) + alpha{suf} ./ sqrt(1 + \
                         square(alpha{suf}));"
                    ),
                );
                self.line(
                    1,
                    &format!(
                        "vector[N] omega_sn{suf} = rep_vector(0, N) + sigma{suf} ./ sqrt(1 - 2 / \
                         pi() * square(delta_sn{suf}));"
                    ),
                );
                self.line(
                    1,
                    &format!(
                        "vector[N] xi_sn{suf} = mu{suf} - omega_sn{suf} .* delta_sn{suf} * \
                         sqrt(2 / pi());"
                    ),
                );
            }
        }
    }

    fn model(&mut self) {
        self.line(0, "model {");
        self.predictors();
        if self.s.rescor {
            self.rescor_likelihood();
        } else {
            for r in 0..self.s.responses() {
                self.resp = r;
                self.likelihood();
            }
            self.resp = 0;
        }
        self.priors();
        self.line(0, "}");
    }

    /// Several gaussian responses with correlated residuals.
    fn rescor_likelihood(&mut self) {
        let r = self.s.responses();
        let row = |name: &str, varies: &dyn Fn(usize) -> bool| {
            (0..r)
                .map(|q| {
                    if varies(q) {
                        format!("{name}{}[n]", suffix(q))
                    } else {
                        format!("{name}{}", suffix(q))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mus = row("mu", &|q| self.varies_in(q, "mu"));
        let sigma_varies = (0..r).any(|q| self.varies_in(q, "sigma"));
        let sigmas = row("sigma", &|q| self.varies_in(q, "sigma"));
        self.line(1, "{");
        self.line(2, &format!("array[N] vector[{r}] Mu;"));
        self.line(2, "for (n in 1:N) {");
        self.line(3, &format!("Mu[n] = [{mus}]';"));
        self.line(2, "}");
        if sigma_varies {
            self.line(2, "for (n in 1:N) {");
            self.line(
                3,
                &format!(
                    "target += multi_normal_cholesky_lpdf(Ymv[n] | Mu[n], \
                     diag_pre_multiply([{sigmas}]', Lrescor));"
                ),
            );
            self.line(2, "}");
        } else {
            self.line(
                2,
                &format!(
                    "target += multi_normal_cholesky_lpdf(Ymv | Mu, \
                     diag_pre_multiply([{sigmas}]', Lrescor));"
                ),
            );
        }
        self.line(1, "}");
    }

    fn likelihood(&mut self) {
        let r = self.s.sub(self.resp).1.clone();
        let integer = self.integer();
        let mass = if integer { "lpmf" } else { "lpdf" };
        let y = self.own("Y");
        if r.subset {
            self.rows = format!("[{}]", self.own("J_sub"));
            let (dist, args) = self.density(Form::Vector, true);
            self.line(
                1,
                &format!("target += {dist}_{mass}({y} | {});", args.join(", ")),
            );
            self.rows = String::new();
            return;
        }
        if self.by_row() {
            self.line(1, "for (n in 1:N) {");
            let lp = self.row_lp();
            match r.weights {
                Some(w) => {
                    let w = self.arg("weights", Some(w), Form::Row);
                    self.line(2, &format!("target += {w} * ({lp});"));
                }
                None => self.line(2, &format!("target += {lp};")),
            }
            self.line(1, "}");
            return;
        }
        if !r.per_row() {
            let (dist, args) = self.density(Form::Vector, true);
            self.line(
                1,
                &format!("target += {dist}_{mass}({y} | {});", args.join(", ")),
            );
            return;
        }
        let (dist, args) = self.density(Form::Row, true);
        let (cdf, cdf_args) = self.density(Form::Row, false);
        let args = args.join(", ");
        let cdf_args = cdf_args.join(", ");
        let below = |y: &str| {
            if integer {
                format!("{y} - 1")
            } else {
                y.to_owned()
            }
        };
        let yn = format!("{y}[n]");
        let cens = self.own("cens");
        self.line(1, "for (n in 1:N) {");
        self.line(2, "real lp;");
        match r.cens {
            None => self.line(2, &format!("lp = {dist}_{mass}({yn} | {args});")),
            Some(interval) => {
                self.line(2, &format!("if ({cens}[n] == 0) {{"));
                self.line(3, &format!("lp = {dist}_{mass}({yn} | {args});"));
                self.line(2, &format!("}} else if ({cens}[n] == 1) {{"));
                self.line(
                    3,
                    &format!("lp = {cdf}_lccdf({} | {cdf_args});", below(&yn)),
                );
                self.line(2, &format!("}} else if ({cens}[n] == -1) {{"));
                self.line(3, &format!("lp = {cdf}_lcdf({yn} | {cdf_args});"));
                self.line(2, "} else {");
                if interval {
                    let y2 = format!("{}[n]", self.own("Y2"));
                    self.line(
                        3,
                        &format!(
                            "lp = log_diff_exp({cdf}_lcdf({y2} | {cdf_args}), \
                             {cdf}_lcdf({yn} | {cdf_args}));"
                        ),
                    );
                } else {
                    self.line(
                        3,
                        "reject(\"row \", n, \" is interval-censored, and the formula's cens() \
                         names no upper end\");",
                    );
                }
                self.line(2, "}");
            }
        }
        let lb = r.trunc_lb.map(|s| self.arg("lb_Y", Some(s), Form::Row));
        let ub = r.trunc_ub.map(|s| self.arg("ub_Y", Some(s), Form::Row));
        let normaliser = match (&lb, &ub) {
            (Some(lb), Some(ub)) => Some(format!(
                "log_diff_exp({cdf}_lcdf({ub} | {cdf_args}), {cdf}_lcdf({} | {cdf_args}))",
                below(lb)
            )),
            (Some(lb), None) => Some(format!("{cdf}_lccdf({} | {cdf_args})", below(lb))),
            (None, Some(ub)) => Some(format!("{cdf}_lcdf({ub} | {cdf_args})")),
            (None, None) => None,
        };
        if let Some(t) = normaliser {
            self.line(2, &format!("lp -= {t};"));
        }
        match r.weights {
            Some(w) => {
                let w = self.arg("weights", Some(w), Form::Row);
                self.line(2, &format!("target += {w} * lp;"));
            }
            None => self.line(2, "target += lp;"),
        }
        self.line(1, "}");
    }

    #[allow(clippy::too_many_lines)] // one prior after another
    fn priors(&mut self) {
        let categorical = self.kind() == Kind::Categorical;
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            if p.b != CoefShape::None {
                if categorical {
                    self.line(1, "for (c in 1:(ncat - 1)) {");
                    self.line(
                        2,
                        &format!(
                            "target += priors_lpdf(b_{n}[, c] | prior_b_{n}_code, prior_b_{n}_args);"
                        ),
                    );
                    self.line(1, "}");
                } else {
                    self.line(
                        1,
                        &format!(
                            "target += priors_lpdf(b_{n} | prior_b_{n}_code, prior_b_{n}_args);"
                        ),
                    );
                }
            }
            let intercept = match p.intercept {
                InterceptShape::None => None,
                InterceptShape::Centred | InterceptShape::Thresholds => {
                    Some(format!("Intercept_c_{n}"))
                }
                InterceptShape::Plain | InterceptShape::ThresholdsPlain => {
                    Some(format!("Intercept_{n}"))
                }
            };
            if let Some(name) = intercept {
                if categorical || p.intercept.thresholds() {
                    let size = if categorical {
                        self.ncat1().to_owned()
                    } else {
                        self.nthres(p.resp)
                    };
                    self.line(1, &format!("for (i in 1:({size})) {{"));
                    self.line(
                        2,
                        &format!(
                            "target += prior_lpdf({name}[i] | prior_Intercept_{n}_code, \
                             prior_Intercept_{n}_args);"
                        ),
                    );
                    self.line(1, "}");
                } else {
                    self.line(
                        1,
                        &format!(
                            "target += prior_lpdf({name} | prior_Intercept_{n}_code, \
                             prior_Intercept_{n}_args);"
                        ),
                    );
                }
            }
            if p.mo > 0 {
                self.line(
                    1,
                    &format!(
                        "target += priors_lpdf(bsp_{n} | prior_bsp_{n}_code, prior_bsp_{n}_args);"
                    ),
                );
                for j in 1..=p.mo {
                    self.line(
                        1,
                        &format!("target += dirichlet_lpdf(simo_{n}_{j} | con_simo_{n}_{j});"),
                    );
                }
            }
            if p.cs {
                let nthres = self.nthres(p.resp);
                self.line(1, &format!("for (c in 1:{nthres}) {{"));
                self.line(
                    2,
                    &format!(
                        "target += priors_lpdf(bcs_{n}[, c] | prior_bcs_{n}_code, \
                         prior_bcs_{n}_args);"
                    ),
                );
                self.line(1, "}");
            }
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            if b.by {
                self.line(1, &format!("for (b in 1:Nby_{k}) {{"));
                self.line(
                    2,
                    &format!(
                        "target += priors_lpdf(sd_{k}[, b] | prior_sd_{k}_code, prior_sd_{k}_args);"
                    ),
                );
                if b.correlated {
                    self.line(
                        2,
                        &format!("target += lkj_corr_cholesky_lpdf(L_{k}[b] | prior_cor_{k}_eta);"),
                    );
                }
                self.line(1, "}");
                self.line(1, &format!("target += std_normal_lpdf(to_vector(z_{k}));"));
                continue;
            }
            if categorical {
                self.line(1, "for (c in 1:(ncat - 1)) {");
                self.line(
                    2,
                    &format!(
                        "target += priors_lpdf(sd_{k}[((c - 1) * M_{k}_1 + 1):(c * M_{k}_1)] | \
                         prior_sd_{k}_code, prior_sd_{k}_args);"
                    ),
                );
                self.line(1, "}");
            } else {
                self.line(
                    1,
                    &format!(
                        "target += priors_lpdf(sd_{k} | prior_sd_{k}_code, prior_sd_{k}_args);"
                    ),
                );
            }
            self.line(1, &format!("target += std_normal_lpdf(to_vector(z_{k}));"));
            if b.correlated {
                self.line(
                    1,
                    &format!("target += lkj_corr_cholesky_lpdf(L_{k} | prior_cor_{k}_eta);"),
                );
            }
        }
        for r in 0..self.s.responses() {
            let suf = suffix(r);
            for s in &self.s.sub(r).0.to_vec() {
                if !s.fixed {
                    let d = format!("{}{suf}", s.dpar);
                    self.line(
                        1,
                        &format!("target += prior_lpdf({d} | prior_{d}_code, prior_{d}_args);"),
                    );
                }
            }
        }
        if self.s.rescor {
            self.line(
                1,
                "target += lkj_corr_cholesky_lpdf(Lrescor | prior_rescor_eta);",
            );
        }
    }

    /// The expected response of every row, as a vector expression of the
    /// first response's distributional parameters.
    fn epred(&self) -> Option<E> {
        let d = |x: &str| E::new(x, self.varies_in(0, x));
        let mu = d("mu");
        let r = &self.s.response;
        let one_minus = |p: &E| E::new(format!("(1 - {})", p.t), p.v);
        let log_mean = |sigma: &E| {
            E::new(
                format!("exp({} + square({}) / 2)", mu.t, sigma.t),
                mu.v || sigma.v,
            )
        };
        let rate = |x: E| match r.rate {
            Some(ArgShape::Column) => x.mul(&E::new("rate", true)),
            Some(ArgShape::Constant) => x.mul(&E::new("rate", false)),
            None => x,
        };
        let trials = |x: E| match r.trials {
            Some(ArgShape::Column) => x.mul(&E::new("to_vector(trials)", true)),
            Some(ArgShape::Constant) => x.mul(&E::new("trials", false)),
            None => x,
        };
        Some(match self.s.family.as_str() {
            "lognormal" => log_mean(&d("sigma")),
            "binomial" | "beta_binomial" => trials(mu),
            "poisson" | "negbinomial" | "geometric" => rate(mu),
            "zero_inflated_poisson" | "zero_inflated_negbinomial" | "zero_inflated_beta" => {
                one_minus(&d("zi")).mul(&mu)
            }
            "hurdle_poisson" => one_minus(&d("hu"))
                .mul(&mu)
                .div(&E::new(format!("(1 - exp(-{}))", mu.t), mu.v)),
            "hurdle_negbinomial" => {
                let shape = d("shape");
                let ratio = shape.div(&E::new(
                    format!("({} + {})", mu.t, shape.t),
                    mu.v || shape.v,
                ));
                let op = if ratio.v || shape.v { " .^ " } else { " ^ " };
                let p0 = E::new(format!("({}){op}{}", ratio.t, shape.t), ratio.v || shape.v);
                one_minus(&d("hu"))
                    .mul(&mu)
                    .div(&E::new(format!("(1 - {})", p0.t), p0.v))
            }
            "hurdle_gamma" => one_minus(&d("hu")).mul(&mu),
            "hurdle_lognormal" => one_minus(&d("hu")).mul(&log_mean(&d("sigma"))),
            "zero_one_inflated_beta" => {
                let zoi = d("zoi");
                let a = zoi.mul(&d("coi"));
                let b = one_minus(&zoi).mul(&mu);
                E::new(format!("{} + {}", a.t, b.t), a.v || b.v)
            }
            "asym_laplace" => {
                let q = d("quantile");
                let num = d("sigma").mul(&E::new(format!("(1 - 2 * {})", q.t), q.v));
                let den = q.mul(&one_minus(&q)).paren();
                let shift = num.div(&den);
                E::new(format!("{} + {}", mu.t, shift.t), mu.v || shift.v)
            }
            _ if self.kind() == Kind::Categorical => return None,
            _ => mu,
        })
    }

    fn generated_quantities(&mut self) {
        let mut body = Vec::new();
        for (i, p) in self.s.predictors.iter().enumerate() {
            let n = i + 1;
            match p.intercept {
                InterceptShape::Centred if self.kind() == Kind::Categorical => {
                    body.push(format!(
                        "vector[ncat - 1] Intercept_{n} = Intercept_c_{n} - (means_X_{n}' * \
                         b_{n})';  // on the original scale"
                    ));
                }
                InterceptShape::Centred => body.push(format!(
                    "real Intercept_{n} = Intercept_c_{n} - dot_product(means_X_{n}, b_{n});  \
                     // on the original scale"
                )),
                InterceptShape::Thresholds => body.push(format!(
                    "vector[{}] Intercept_{n} = Intercept_c_{n} + dot_product(means_X_{n}, \
                     b_{n});  // on the original scale",
                    self.nthres(p.resp)
                )),
                _ => {}
            }
        }
        for (i, b) in self.s.blocks.iter().enumerate() {
            let k = i + 1;
            if b.correlated {
                let m = self.m(k);
                if b.by {
                    body.push(format!("array[Nby_{k}] matrix[{m}, {m}] Cor_{k};"));
                    body.push(format!("for (b in 1:Nby_{k}) {{"));
                    body.push(format!(
                        "  Cor_{k}[b] = multiply_lower_tri_self_transpose(L_{k}[b]);"
                    ));
                    body.push("}".to_owned());
                } else {
                    body.push(format!(
                        "matrix[{m}, {m}] Cor_{k} = multiply_lower_tri_self_transpose(L_{k});"
                    ));
                }
            }
        }
        if self.s.rescor {
            let r = self.s.responses();
            body.push(format!(
                "matrix[{r}, {r}] Rescor = multiply_lower_tri_self_transpose(Lrescor);"
            ));
        }
        let epred = if self.s.epred { self.epred() } else { None };
        if body.is_empty() && epred.is_none() {
            return;
        }
        self.line(0, "generated quantities {");
        for l in &body {
            self.line(1, l);
        }
        if let Some(e) = epred {
            // The expected response of each row (`posterior_epred`), from
            // the predictors computed again in a block of their own, so only
            // `epred_1` is written out.
            self.line(1, "vector[N] epred_1;");
            self.line(1, "{");
            self.base = 1;
            self.predictors();
            if self.kind() == Kind::Ordinal {
                self.resp = 0;
                let lp = format!(
                    "ord_lprobs({}, {}, {})",
                    self.ordinal_eta(),
                    self.thresholds_row(),
                    self.disc()
                );
                self.line(1, "for (n in 1:N) {");
                self.line(
                    2,
                    &format!(
                        "vector[rows({}) + 1] p = exp({lp});",
                        self.thresholds_row_size()
                    ),
                );
                self.line(
                    2,
                    "epred_1[n] = dot_product(linspaced_vector(rows(p), 1, rows(p)), p);",
                );
                self.line(1, "}");
            } else if e.v {
                self.line(1, &format!("epred_1 = {};", e.t));
            } else {
                self.line(1, &format!("epred_1 = rep_vector({}, N);", e.t));
            }
            self.base = 0;
            self.line(1, "}");
        }
        self.line(0, "}");
    }

    /// The thresholds' expression without a row's category-specific effects
    /// — what the number of thresholds is read from.
    fn thresholds_row_size(&self) -> String {
        let n = self
            .s
            .predictors
            .iter()
            .position(|p| p.dpar == "mu" && p.resp == 0)
            .map_or(1, |i| i + 1);
        match self.s.predictors.get(n - 1).map(|p| p.intercept) {
            Some(InterceptShape::Thresholds) => format!("Intercept_c_{n}"),
            _ => format!("Intercept_{n}"),
        }
    }
}
