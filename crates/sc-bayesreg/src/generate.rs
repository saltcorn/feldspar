//! The generator (TODO-bayesian-regression §4, R1.5): a [`Structure`] → the
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
//!   block is correlated, `r = diag_pre_multiply(sd, z)'` when it is not.
//! - **Centring**: the population-level predictors are centred in
//!   `transformed data` and the intercept is a parameter on that scale — "the
//!   expected response when all predictors are at their means", which is what
//!   brms documents its `Intercept` prior as being on (`?brmsformula`,
//!   `center`). The intercept on the original scale is recovered in generated
//!   quantities as `Intercept_c − means_X · b`.
//! - **Densities and their parameterisations**: the Stan Functions Reference;
//!   the mean parameterisations of the families are brms's documented ones
//!   (`vignette("brms_families")`): `Gamma` as `gamma(shape, shape / μ)`,
//!   `weibull` with scale `μ / Γ(1 + 1/shape)`, `exponential` with rate
//!   `1/μ`, `Beta` as `beta(μφ, (1 − μ)φ)`, `geometric` as a negative
//!   binomial with shape 1, and `skew_normal` with mean μ and SD σ (Azzalini
//!   1985: δ = α/√(1+α²), ω = σ/√(1 − 2δ²/π), ξ = μ − ωδ√(2/π)).
//! - **Censoring and truncation** (Stan User's Guide, "Truncated or censored
//!   data"): a row censored on the left contributes its log CDF, on the right
//!   its log complementary CDF (for whole numbers, of `y − 1`: the value is at
//!   least `y`), an interval `(y, y2]` the log of the CDFs' difference; a
//!   truncated row's density is divided by the mass between its bounds.
//! - **Priors as data** (§2.5): a prior is a code and three numbers, and the
//!   program branches on the code in one function, `prior_lpdf`. A prior on a
//!   bounded parameter is truncated at the bound; the truncation's constant
//!   depends only on the prior's numbers, so it is left out of the density.
//!
//! The variable names are this crate's own and positional (§4): `N`, `Y`;
//! per linear predictor *p* `K_p`, `X_p`, `b_p`, `Intercept_p`; per block *k*
//! `N_k`, `J_k`, `M_k_j` and `Z_k_j` per part *j*, `sd_k`, `z_k`, `L_k`,
//! `r_k`, `Cor_k`; the prior data `prior_<parameter>_code` and
//! `prior_<parameter>_args`.

use crate::canon::{ArgShape, CoefShape, InterceptShape, Structure};

/// The program's path: fixed, because the compile cache hashes it (§5).
pub const PROGRAM_PATH: &str = "model.stan";

/// A parameter whose prior the program reads as data: `prior_<parameter>_code`
/// and `prior_<parameter>_args` (`prior_cor_<k>_eta` for a correlation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorVariable {
    /// The parameter it is the prior of: `b_1`, `Intercept_1`, `sd_2`,
    /// `sigma`, `cor_1`.
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
    for (i, p) in structure.predictors.iter().enumerate() {
        let n = i + 1;
        if p.b != CoefShape::None {
            out.push(PriorVariable {
                parameter: format!("b_{n}"),
                vector: true,
            });
        }
        if p.intercept != InterceptShape::None {
            out.push(PriorVariable {
                parameter: format!("Intercept_{n}"),
                vector: false,
            });
        }
    }
    for (i, b) in structure.blocks.iter().enumerate() {
        let k = i + 1;
        out.push(PriorVariable {
            parameter: format!("sd_{k}"),
            vector: true,
        });
        if b.correlated {
            out.push(PriorVariable {
                parameter: format!("cor_{k}"),
                vector: false,
            });
        }
    }
    for s in &structure.scalars {
        if !s.fixed {
            out.push(PriorVariable {
                parameter: s.dpar.clone(),
                vector: false,
            });
        }
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

impl<'s> Gen<'s> {
    fn new(s: &'s Structure) -> Gen<'s> {
        Gen {
            s,
            out: String::new(),
        }
    }

    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.out.push('\n');
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
        if self.integer() {
            format!("array[N] int{constraint} {name};")
        } else {
            format!("vector{constraint}[N] {name};")
        }
    }

    /// The size of block `k`'s effects as Stan text: `M_k_1`, or `M_k` for a
    /// block over several linear predictors (summed in transformed data).
    fn m(&self, k: usize) -> String {
        if self.s.blocks[k - 1].parts.len() == 1 {
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

    /// Whether `dpar` has a linear predictor (and so is a vector).
    fn varies(&self, dpar: &str) -> bool {
        self.s.predictors.iter().any(|p| p.dpar == dpar)
    }

    /// `dpar` in `form`: `sigma`, `sigma[n]`.
    fn dp(&self, dpar: &str, form: Form) -> String {
        if form == Form::Row && self.varies(dpar) {
            format!("{dpar}[n]")
        } else {
            dpar.to_owned()
        }
    }

    /// A per-row or constant data variable in `form`.
    fn arg(&self, name: &str, shape: Option<ArgShape>, form: Form) -> String {
        match (shape, form) {
            (Some(ArgShape::Column), Form::Row) => format!("{name}[n]"),
            _ => name.to_owned(),
        }
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
        self.line(0, "}");
    }

    fn data(&mut self) {
        let r = self.s.response.clone();
        self.line(0, "data {");
        self.line(1, "int<lower=1> N;");
        let y = self.y_decl("Y", "");
        self.line(1, &y);
        if let Some(w) = r.weights {
            self.line(
                1,
                &match w {
                    ArgShape::Column => "vector<lower=0>[N] weights;".to_owned(),
                    ArgShape::Constant => "real<lower=0> weights;".to_owned(),
                },
            );
        }
        if let Some(t) = r.trials {
            self.line(
                1,
                match t {
                    ArgShape::Column => "array[N] int<lower=0> trials;",
                    ArgShape::Constant => "int<lower=0> trials;",
                },
            );
        }
        if let Some(interval) = r.cens {
            self.line(1, "array[N] int<lower=-1, upper=2> cens;");
            if interval {
                let y2 = self.y_decl("Y2", "");
                self.line(1, &y2);
            }
        }
        for (name, shape) in [("lb_Y", r.trunc_lb), ("ub_Y", r.trunc_ub)] {
            if let Some(shape) = shape {
                let decl = match shape {
                    ArgShape::Column => self.y_decl(name, ""),
                    ArgShape::Constant => format!("{} {name};", self.y_type()),
                };
                self.line(1, &decl);
            }
        }
        if r.se.is_some() {
            self.line(1, "vector<lower=0>[N] se;");
        }
        if let Some(rate) = r.rate {
            self.line(
                1,
                match rate {
                    ArgShape::Column => "vector<lower=0>[N] rate;",
                    ArgShape::Constant => "real<lower=0> rate;",
                },
            );
        }
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
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            self.line(1, &format!("int<lower=1> N_{k};"));
            self.line(1, &format!("array[N] int<lower=1, upper=N_{k}> J_{k};"));
            for j in 1..=b.parts.len() {
                self.line(1, &format!("int<lower=1> M_{k}_{j};"));
                self.line(1, &format!("matrix[N, M_{k}_{j}] Z_{k}_{j};"));
            }
            let size = self.m_sum(k);
            self.line(1, &format!("array[{size}] int prior_sd_{k}_code;"));
            self.line(1, &format!("array[{size}, 3] real prior_sd_{k}_args;"));
            if b.correlated {
                self.line(1, &format!("real<lower=0> prior_cor_{k}_eta;"));
            }
        }
        for s in &self.s.scalars.clone() {
            if s.fixed {
                self.line(1, &format!("real{} {};", bounds(&s.support), s.dpar));
            } else {
                self.line(1, &format!("int prior_{}_code;", s.dpar));
                self.line(1, &format!("array[3] real prior_{}_args;", s.dpar));
            }
        }
        self.line(0, "}");
    }

    fn transformed_data(&mut self) {
        let mut body = Vec::new();
        for (i, p) in self.s.predictors.iter().enumerate() {
            let n = i + 1;
            if p.intercept == InterceptShape::Centred {
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
            if b.parts.len() > 1 {
                body.push(format!("int M_{k} = {};", self.m_sum(k)));
            }
        }
        let mut centring = Vec::new();
        for (i, p) in self.s.predictors.iter().enumerate() {
            let n = i + 1;
            if p.intercept == InterceptShape::Centred {
                centring.push(format!("for (i in 1:K_{n}) {{"));
                centring.push(format!("  means_X_{n}[i] = mean(X_{n}[, i]);"));
                centring.push(format!("  Xc_{n}[, i] = X_{n}[, i] - means_X_{n}[i];"));
                centring.push("}".to_owned());
            }
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

    fn parameters(&mut self) {
        self.line(0, "parameters {");
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            match p.b {
                CoefShape::None => {}
                CoefShape::Free => self.line(1, &format!("vector[K_{n}] b_{n};")),
                CoefShape::Bounded => self.line(
                    1,
                    &format!("vector<lower=lb_b_{n}, upper=ub_b_{n}>[K_{n}] b_{n};"),
                ),
            }
            match p.intercept {
                InterceptShape::None => {}
                InterceptShape::Centred => self.line(1, &format!("real Intercept_c_{n};")),
                InterceptShape::Plain => self.line(1, &format!("real Intercept_{n};")),
            }
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            let m = self.m(k);
            self.line(1, &format!("vector<lower=0>[{m}] sd_{k};"));
            self.line(1, &format!("matrix[{m}, N_{k}] z_{k};"));
            if b.correlated {
                self.line(1, &format!("cholesky_factor_corr[{m}] L_{k};"));
            }
        }
        for s in &self.s.scalars.clone() {
            if !s.fixed {
                self.line(1, &format!("real{} {};", bounds(&s.support), s.dpar));
            }
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
        let mut terms = Vec::new();
        match p.intercept {
            InterceptShape::Centred => terms.push(format!("Intercept_c_{n}")),
            InterceptShape::Plain => terms.push(format!("Intercept_{n}")),
            InterceptShape::None => {}
        }
        match (p.b, p.intercept) {
            (CoefShape::None, _) => {}
            (_, InterceptShape::Centred) => terms.push(format!("Xc_{n} * b_{n}")),
            _ => terms.push(format!("X_{n} * b_{n}")),
        }
        if p.offset {
            terms.push(format!("offset_{n}"));
        }
        let init = if terms.is_empty() {
            "rep_vector(0, N)".to_owned()
        } else if p.intercept != InterceptShape::None && terms.len() == 1 {
            format!("rep_vector({}, N)", terms[0])
        } else {
            terms.join(" + ")
        };
        self.line(1, &format!("vector[N] eta_{n} = {init};"));
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

    /// The family's density of row `n` or of every row: its Stan name (without
    /// the suffix) and its arguments after the response.
    fn density(&self, form: Form, canonical: bool) -> (&'static str, Vec<String>) {
        let r = &self.s.response;
        let mu_link = self.s.predictors[0].link.as_str();
        let mu = self.dp("mu", form);
        let eta = match form {
            Form::Vector => "eta_1".to_owned(),
            Form::Row => "eta_1[n]".to_owned(),
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
                    Form::Vector => v.to_owned(),
                    Form::Row => format!("{v}[n]"),
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
            // The family table holds exactly the families above.
            _ => ("normal", vec![mu, self.dp("sigma", form)]),
        }
    }

    fn model(&mut self) {
        self.line(0, "model {");
        let n_pred = self.s.predictors.len();
        for n in 1..=n_pred {
            self.eta(n);
        }
        // Each linear predictor through its inverse link.
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            let value = inverse_link(&p.link, &format!("eta_{n}"));
            self.line(1, &format!("vector[N] {} = {value};", p.dpar));
        }
        if self.s.family == "skew_normal" {
            self.line(
                1,
                "vector[N] delta_sn = rep_vector(0, N) + alpha ./ sqrt(1 + square(alpha));",
            );
            self.line(
                1,
                "vector[N] omega_sn = rep_vector(0, N) + sigma ./ sqrt(1 - 2 / pi() * square(delta_sn));",
            );
            self.line(
                1,
                "vector[N] xi_sn = mu - omega_sn .* delta_sn * sqrt(2 / pi());",
            );
        }
        self.likelihood();
        self.priors();
        self.line(0, "}");
    }

    fn likelihood(&mut self) {
        let r = self.s.response.clone();
        let integer = self.integer();
        let mass = if integer { "lpmf" } else { "lpdf" };
        if !r.per_row() {
            let (dist, args) = self.density(Form::Vector, true);
            self.line(
                1,
                &format!("target += {dist}_{mass}(Y | {});", args.join(", ")),
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
        self.line(1, "for (n in 1:N) {");
        self.line(2, "real lp;");
        match r.cens {
            None => self.line(2, &format!("lp = {dist}_{mass}(Y[n] | {args});")),
            Some(interval) => {
                self.line(2, "if (cens[n] == 0) {");
                self.line(3, &format!("lp = {dist}_{mass}(Y[n] | {args});"));
                self.line(2, "} else if (cens[n] == 1) {");
                self.line(
                    3,
                    &format!("lp = {cdf}_lccdf({} | {cdf_args});", below("Y[n]")),
                );
                self.line(2, "} else if (cens[n] == -1) {");
                self.line(3, &format!("lp = {cdf}_lcdf(Y[n] | {cdf_args});"));
                self.line(2, "} else {");
                if interval {
                    self.line(
                        3,
                        &format!(
                            "lp = log_diff_exp({cdf}_lcdf(Y2[n] | {cdf_args}), \
                             {cdf}_lcdf(Y[n] | {cdf_args}));"
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

    fn priors(&mut self) {
        for (i, p) in self.s.predictors.clone().iter().enumerate() {
            let n = i + 1;
            if p.b != CoefShape::None {
                self.line(
                    1,
                    &format!("target += priors_lpdf(b_{n} | prior_b_{n}_code, prior_b_{n}_args);"),
                );
            }
            let intercept = match p.intercept {
                InterceptShape::None => None,
                InterceptShape::Centred => Some(format!("Intercept_c_{n}")),
                InterceptShape::Plain => Some(format!("Intercept_{n}")),
            };
            if let Some(name) = intercept {
                self.line(
                    1,
                    &format!(
                        "target += prior_lpdf({name} | prior_Intercept_{n}_code, \
                         prior_Intercept_{n}_args);"
                    ),
                );
            }
        }
        for (i, b) in self.s.blocks.clone().iter().enumerate() {
            let k = i + 1;
            self.line(
                1,
                &format!("target += priors_lpdf(sd_{k} | prior_sd_{k}_code, prior_sd_{k}_args);"),
            );
            self.line(1, &format!("target += std_normal_lpdf(to_vector(z_{k}));"));
            if b.correlated {
                self.line(
                    1,
                    &format!("target += lkj_corr_cholesky_lpdf(L_{k} | prior_cor_{k}_eta);"),
                );
            }
        }
        for s in &self.s.scalars.clone() {
            if !s.fixed {
                let d = &s.dpar;
                self.line(
                    1,
                    &format!("target += prior_lpdf({d} | prior_{d}_code, prior_{d}_args);"),
                );
            }
        }
    }

    fn generated_quantities(&mut self) {
        let mut body = Vec::new();
        for (i, p) in self.s.predictors.iter().enumerate() {
            let n = i + 1;
            if p.intercept == InterceptShape::Centred {
                body.push(format!(
                    "real Intercept_{n} = Intercept_c_{n} - dot_product(means_X_{n}, b_{n});  \
                     // on the original scale"
                ));
            }
        }
        for (i, b) in self.s.blocks.iter().enumerate() {
            let k = i + 1;
            if b.correlated {
                let m = self.m(k);
                body.push(format!(
                    "matrix[{m}, {m}] Cor_{k} = multiply_lower_tri_self_transpose(L_{k});"
                ));
            }
        }
        if body.is_empty() {
            return;
        }
        self.line(0, "generated quantities {");
        for l in &body {
            self.line(1, l);
        }
        self.line(0, "}");
    }
}
