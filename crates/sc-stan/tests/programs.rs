//! The declaration parser over real programs (TODO 2.3): each asserting the
//! `Interface` it should produce, and each refusal with its sentence.
//!
//! The programs are also `pub(crate)` so the real-CmdStan tests can hand the
//! same texts to `stanc` and check that its reading agrees with ours.

use std::sync::Arc;

use sc_files::{FileStore, LocalFileStore};
use sc_model::{Declaration, Element, Interface, SizeExpr, SizeOp, SizeTree};
use sc_stan::program::{Program, ProgramFile};

/// Gelman & Hill's radon model with a county-level predictor — the
/// milestone's definition of done (TODO §11).
pub(crate) const RADON: &str = r#"
data {
  int<lower=1> N;                              // count(main)
  int<lower=1> J;                              // size(counties)
  array[N] int<lower=1, upper=J> county;       // index(main.county → counties)
  vector[N] x;                                 // column(main.floor)
  vector[J] u;                                 // column(counties.log_uranium)
  vector[N] y;                                 // column(main.log_radon)
}
parameters {
  real gamma0;
  real gamma1;
  real beta;
  real<lower=0> sigma_a;
  real<lower=0> sigma_y;
  vector[J] alpha_raw;
}
transformed parameters {
  vector[J] alpha = gamma0 + gamma1 * u + sigma_a * alpha_raw;   // labelled by county
}
model {
  alpha_raw ~ std_normal();
  gamma0 ~ normal(0, 5);
  gamma1 ~ normal(0, 5);
  beta ~ normal(0, 5);
  sigma_a ~ normal(0, 2);
  sigma_y ~ normal(0, 2);
  y ~ normal(alpha[county] + beta * x, sigma_y);
}
generated quantities {
  vector[N] log_lik;                                               // labelled by home
  for (n in 1:N) log_lik[n] = normal_lpdf(y[n] | alpha[county[n]] + beta * x[n], sigma_y);
}
"#;

/// Eight schools, non-centred.
pub(crate) const EIGHT_SCHOOLS: &str = r#"
data {
  int<lower=0> J;           // number of schools
  array[J] real y;          // estimated treatment effects
  array[J] real<lower=0> sigma; // standard error of effect estimates
}
parameters {
  real mu;
  real<lower=0> tau;
  vector[J] theta_tilde;
}
transformed parameters {
  vector[J] theta = mu + tau * theta_tilde;
}
model {
  mu ~ normal(0, 5);
  tau ~ cauchy(0, 5);
  theta_tilde ~ normal(0, 1);
  y ~ normal(theta, sigma);
}
"#;

/// An AR(1), with a posterior predictive one shorter than the series.
pub(crate) const AR1: &str = r#"
data {
  int<lower=0> N;
  vector[N] y;
}
parameters {
  real alpha;
  real<lower=-1, upper=1> beta;
  real<lower=0> sigma;
}
model {
  y[2:N] ~ normal(alpha + beta * y[1:(N - 1)], sigma);
}
generated quantities {
  vector[N - 1] y_rep;
  for (n in 2:N) {
    y_rep[n - 1] = normal_rng(alpha + beta * y[n - 1], sigma);
  }
}
"#;

/// BYM2 (Morris et al., 2019), as in the Stan case study "Spatial Models in
/// Stan: Intrinsic Auto-Regressive Models for Areal Data".
pub(crate) const BYM2: &str = r#"
functions {
  real icar_normal_lpdf(vector phi, int N, array[] int node1, array[] int node2) {
    return -0.5 * dot_self(phi[node1] - phi[node2])
           + normal_lpdf(sum(phi) | 0, 0.001 * N);
  }
}
data {
  int<lower=0> N;
  int<lower=0> N_edges;
  array[N_edges] int<lower=1, upper=N> node1;  // node1[i] adjacent to node2[i]
  array[N_edges] int<lower=1, upper=N> node2;  // and node1[i] < node2[i]

  array[N] int<lower=0> y;                     // count outcomes
  vector<lower=0>[N] E;                        // exposure
  int<lower=1> K;                              // num covariates
  matrix[N, K] x;                              // design matrix

  real<lower=0> scaling_factor; // scales the variance of the spatial effects
}
transformed data {
  vector[N] log_E = log(E);
}
parameters {
  real beta0;            // intercept
  vector[K] betas;       // covariates

  real<lower=0> sigma;        // overall standard deviation
  real<lower=0, upper=1> rho; // proportion unstructured vs. spatially structured variance

  vector[N] theta;       // heterogeneous effects
  vector[N] phi;         // spatial effects
}
transformed parameters {
  vector[N] convolved_re;
  // variance of each component should be approximately equal to 1
  convolved_re = sqrt(1 - rho) * theta + sqrt(rho / scaling_factor) * phi;
}
model {
  y ~ poisson_log(log_E + beta0 + x * betas + convolved_re * sigma);  // co-variates

  // This is the prior for phi! (up to proportionality)
  phi ~ icar_normal(N, node1, node2);

  beta0 ~ normal(0.0, 1.0);
  betas ~ normal(0.0, 1.0);
  theta ~ normal(0.0, 1.0);
  sigma ~ normal(0, 1.0);
  rho ~ beta(0.5, 0.5);
}
generated quantities {
  real log_precision = -2.0 * log(sigma);
  real logit_rho = log(rho / (1.0 - rho));
  vector[N] eta = log_E + beta0 + x * betas + convolved_re * sigma; // co-variates
  vector[N] mu = exp(eta);
}
"#;

/// Every container and constrained type, in `data` and in `parameters`, and
/// the statement forms the skipper must step over without being fooled.
pub(crate) const EVERY_TYPE: &str = r#"
data {
  int<lower=0> N;
  int<lower=0> K;
  real<lower=-1.5, upper=(N > 2 ? 3 : 4)> r;
  vector<lower=0>[N] v;
  row_vector[K] rv;
  matrix<lower=0, upper=1>[N, K] m;
  array[N, 2] vector[K] av;
  array[3] int<lower=0> counts;
  vector<offset=1, multiplier=2>[K] om;
  array[(N + 1) * 2 %/% K] real odd;
  array[N % 3, N - 1] real mixed;
  vector[num_elements(v)] opaque;
  cov_matrix[K] S;
  corr_matrix[K] R;
  cholesky_factor_corr[K] LR;
  cholesky_factor_cov[K] LS;
  cholesky_factor_cov[N, K] LSR;
}
parameters {
  simplex[K] s;
  unit_vector[K] uv;
  sum_to_zero_vector[K] z;
  sum_to_zero_matrix[N, K] zm;
  ordered[K] o;
  positive_ordered[K] po;
  column_stochastic_matrix[K, K] csm;
  row_stochastic_matrix[K, K] rsm;
  real a, b;
}
generated quantities {
  complex c;
  complex_vector[K] cv;
  complex_matrix[N, K] cm;
  tuple(real, array[2] int) t;
  int j = 1, k = 2;
  {
    real local = 1;           // a nested declaration is a local, not an output
  }
  if (N > 1) {
    real also_local = 2;
  } else if (N > 0) j = 3; else {
    k = 4;
  }
  while (j < 10) j += 1;
  profile("x") { real in_profile = 3; }
  print("} not a brace ", { 1, 2 });
  array[2] real after = { 1.0, 2.0 };
}
"#;

fn program(text: &str) -> Program {
    Program::from_files("models", vec![ProgramFile::new("m.stan", text)]).unwrap()
}

fn parse(text: &str) -> Interface {
    program(text).interface().unwrap_or_else(|e| panic!("{e}"))
}

fn refusal(text: &str) -> String {
    program(text).interface().unwrap_err().to_string()
}

fn var(name: &str) -> SizeExpr {
    SizeExpr::var(name)
}

fn decl(name: &str, element: Element, dims: Vec<SizeExpr>, ty: &str) -> Declaration {
    Declaration::new(name, element, dims, ty)
}

fn names(decls: &[Declaration]) -> Vec<&str> {
    decls.iter().map(|d| d.name.as_str()).collect()
}

#[test]
fn radon() {
    use Element::*;
    let interface = parse(RADON);
    assert_eq!(
        interface.data,
        [
            decl("N", Int, vec![], "int<lower=1>").bounded(Some("1"), None),
            decl("J", Int, vec![], "int<lower=1>").bounded(Some("1"), None),
            decl(
                "county",
                Int,
                vec![var("N")],
                "array[N] int<lower=1, upper=J>"
            )
            .bounded(Some("1"), Some("J")),
            decl("x", Real, vec![var("N")], "vector[N]"),
            decl("u", Real, vec![var("J")], "vector[J]"),
            decl("y", Real, vec![var("N")], "vector[N]"),
        ]
    );
    assert_eq!(
        interface.parameters,
        [
            decl("gamma0", Real, vec![], "real"),
            decl("gamma1", Real, vec![], "real"),
            decl("beta", Real, vec![], "real"),
            decl("sigma_a", Real, vec![], "real<lower=0>").bounded(Some("0"), None),
            decl("sigma_y", Real, vec![], "real<lower=0>").bounded(Some("0"), None),
            decl("alpha_raw", Real, vec![var("J")], "vector[J]"),
        ]
    );
    assert_eq!(
        interface.transformed,
        [decl("alpha", Real, vec![var("J")], "vector[J]")]
    );
    assert_eq!(
        interface.generated,
        [decl("log_lik", Real, vec![var("N")], "vector[N]")]
    );
    // What labelling runs on: `alpha`'s one axis is whatever `J` is the size of.
    assert_eq!(
        interface.output("alpha").unwrap().dims[0].identifier(),
        Some("J")
    );
}

#[test]
fn eight_schools() {
    let interface = parse(EIGHT_SCHOOLS);
    assert_eq!(
        interface.data,
        [
            decl("J", Element::Int, vec![], "int<lower=0>").bounded(Some("0"), None),
            decl("y", Element::Real, vec![var("J")], "array[J] real"),
            decl(
                "sigma",
                Element::Real,
                vec![var("J")],
                "array[J] real<lower=0>"
            )
            .bounded(Some("0"), None),
        ]
    );
    assert_eq!(names(&interface.parameters), ["mu", "tau", "theta_tilde"]);
    assert_eq!(names(&interface.transformed), ["theta"]);
    assert!(interface.generated.is_empty());
}

#[test]
fn ar1() {
    let interface = parse(AR1);
    assert_eq!(names(&interface.data), ["N", "y"]);
    assert_eq!(
        interface.parameters[1],
        decl("beta", Element::Real, vec![], "real<lower=-1, upper=1>")
            .bounded(Some("-1"), Some("1"))
    );
    let y_rep = &interface.generated[0];
    assert_eq!(y_rep.name, "y_rep");
    assert_eq!(
        y_rep.dims,
        [SizeExpr::parsed(
            "N - 1",
            SizeTree::binary(SizeOp::Sub, SizeTree::Var("N".into()), SizeTree::Int(1))
        )]
    );
    assert_eq!(y_rep.dims[0].eval(&|v| (v == "N").then_some(100)), Some(99));
    // The loop body's assignment is a statement, not a second output.
    assert_eq!(interface.generated.len(), 1);
}

#[test]
fn bym2() {
    use Element::*;
    let interface = parse(BYM2);
    assert_eq!(
        names(&interface.data),
        [
            "N",
            "N_edges",
            "node1",
            "node2",
            "y",
            "E",
            "K",
            "x",
            "scaling_factor"
        ]
    );
    let data = |n: &str| interface.data_variable(n).unwrap().clone();
    assert_eq!(
        data("node1"),
        decl(
            "node1",
            Int,
            vec![var("N_edges")],
            "array[N_edges] int<lower=1, upper=N>"
        )
        .bounded(Some("1"), Some("N"))
    );
    assert_eq!(
        data("E"),
        decl("E", Real, vec![var("N")], "vector<lower=0>[N]").bounded(Some("0"), None)
    );
    assert_eq!(
        data("x"),
        decl("x", Real, vec![var("N"), var("K")], "matrix[N, K]")
    );
    // `transformed data` is read past, not reported.
    assert!(interface.data_variable("log_E").is_none());
    assert_eq!(
        names(&interface.parameters),
        ["beta0", "betas", "sigma", "rho", "theta", "phi"]
    );
    assert_eq!(
        interface.parameters[3],
        decl("rho", Real, vec![], "real<lower=0, upper=1>").bounded(Some("0"), Some("1"))
    );
    // A declaration followed by a statement assigning it: one output.
    assert_eq!(names(&interface.transformed), ["convolved_re"]);
    assert_eq!(
        names(&interface.generated),
        ["log_precision", "logit_rho", "eta", "mu"]
    );
}

#[test]
fn every_constrained_type() {
    use Element::*;
    let interface = parse(EVERY_TYPE);
    let shape = |n: &str| {
        let d = interface
            .data_variable(n)
            .or_else(|| interface.output(n))
            .unwrap_or_else(|| panic!("no `{n}`"));
        (
            d.element,
            d.dims.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        )
    };
    let square = |k: &'static str| vec![k, k];

    assert_eq!(shape("N"), (Int, vec![]));
    assert_eq!(shape("v"), (Real, vec!["N"]));
    assert_eq!(shape("rv"), (Real, vec!["K"]));
    assert_eq!(shape("m"), (Real, vec!["N", "K"]));
    // The full shape, outer to inner: an array's sizes, then the vector's.
    assert_eq!(shape("av"), (Real, vec!["N", "2", "K"]));
    assert_eq!(shape("counts"), (Int, vec!["3"]));
    assert_eq!(shape("om"), (Real, vec!["K"]));
    assert_eq!(shape("S"), (Real, square("K")));
    assert_eq!(shape("R"), (Real, square("K")));
    assert_eq!(shape("LR"), (Real, square("K")));
    assert_eq!(shape("LS"), (Real, square("K")));
    assert_eq!(shape("LSR"), (Real, vec!["N", "K"]));
    assert_eq!(shape("s"), (Real, vec!["K"]));
    assert_eq!(shape("uv"), (Real, vec!["K"]));
    assert_eq!(shape("z"), (Real, vec!["K"]));
    assert_eq!(shape("zm"), (Real, vec!["N", "K"]));
    assert_eq!(shape("o"), (Real, vec!["K"]));
    assert_eq!(shape("po"), (Real, vec!["K"]));
    assert_eq!(shape("csm"), (Real, square("K")));
    assert_eq!(shape("rsm"), (Real, square("K")));
    assert_eq!(shape("c"), (Complex, vec![]));
    assert_eq!(shape("cv"), (Complex, vec!["K"]));
    assert_eq!(shape("cm"), (Complex, vec!["N", "K"]));
    assert_eq!(shape("t"), (Tuple, vec![]));

    // Bounds are kept as text, however they are written.
    let r = interface.data_variable("r").unwrap();
    assert_eq!(r.lower.as_deref(), Some("-1.5"));
    assert_eq!(r.upper.as_deref(), Some("(N > 2 ? 3 : 4)"));
    assert_eq!(r.stan_type, "real<lower=-1.5, upper=(N > 2 ? 3 : 4)>");
    let m = interface.data_variable("m").unwrap();
    assert_eq!(
        (m.lower.as_deref(), m.upper.as_deref()),
        (Some("0"), Some("1"))
    );
    // `offset=` and `multiplier=` are not bounds.
    let om = interface.data_variable("om").unwrap();
    assert_eq!((om.lower.as_deref(), om.upper.as_deref()), (None, None));
    assert_eq!(om.stan_type, "vector<offset=1, multiplier=2>[K]");

    // Sizes evaluate where they are integer arithmetic, and are opaque where
    // they are not.
    let sizes = |v: &str| match v {
        "N" => Some(10),
        "K" => Some(4),
        _ => None,
    };
    let odd = &interface.data_variable("odd").unwrap().dims[0];
    assert_eq!(odd.text, "(N + 1) * 2 %/% K");
    assert_eq!(odd.eval(&sizes), Some(5));
    let mixed = &interface.data_variable("mixed").unwrap().dims;
    assert_eq!(mixed[0].eval(&sizes), Some(1));
    assert_eq!(mixed[1].eval(&sizes), Some(9));
    let opaque = &interface.data_variable("opaque").unwrap().dims[0];
    assert_eq!(opaque.text, "num_elements(v)");
    assert!(opaque.tree.is_none());

    // Two names in one declaration are two variables of one type.
    assert_eq!(names(&interface.parameters)[8..], ["a", "b"]);
    // Only the block's top-level declarations are outputs.
    assert_eq!(
        names(&interface.generated),
        ["c", "cv", "cm", "t", "j", "k", "after"]
    );
}

#[test]
fn a_program_with_no_data_and_blocks_in_any_legal_subset() {
    let interface = parse("parameters { real mu; } model { mu ~ std_normal(); }");
    assert!(interface.data.is_empty());
    assert_eq!(names(&interface.parameters), ["mu"]);
    assert_eq!(parse(""), Interface::default());
}

// ---- includes ----

fn store_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-stan-programs-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn store_with(label: &str, files: &[(&str, &str)]) -> Arc<dyn FileStore> {
    let dir = store_dir(label);
    let store = LocalFileStore::new("models", &dir).unwrap();
    for (path, text) in files {
        store
            .write(path, bytes::Bytes::from(text.to_string()))
            .await
            .unwrap();
    }
    Arc::new(store)
}

#[tokio::test]
async fn includes_are_read_from_the_store_beside_the_including_file() {
    let store = store_with(
        "include",
        &[
            (
                "radon/model.stan",
                "functions {\n#include \"lib/icar.stan\"\n}\ndata {\n#include <radon_data.stan>\n}\n\
                 parameters { real mu; }\n// #include \"not/an/include.stan\"\n",
            ),
            ("radon/lib/icar.stan", "#include ../shared.stan\nreal icar(real x) { return x; }\n"),
            ("radon/shared.stan", "real shared(real x) { return x; }\n"),
            ("radon/radon_data.stan", "int<lower=1> N;\nvector[N] y;\n"),
        ],
    )
    .await;
    let program = Program::load(store.as_ref(), "models", "radon/model.stan")
        .await
        .unwrap();
    assert_eq!(
        program
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        [
            "radon/model.stan",
            "radon/lib/icar.stan",
            "radon/radon_data.stan",
            "radon/shared.stan"
        ]
    );
    assert_eq!(program.hashes().len(), 4);
    let interface = program.interface().unwrap();
    assert_eq!(names(&interface.data), ["N", "y"]);
    assert_eq!(names(&interface.parameters), ["mu"]);
}

#[tokio::test]
async fn an_include_that_is_missing_or_leaves_the_store_is_refused_by_name() {
    let store = store_with(
        "include-bad",
        &[
            ("m.stan", "functions {\n#include helpers.stan\n}"),
            ("up.stan", "functions {\n#include ../secret.stan\n}"),
            ("abs.stan", "functions {\n#include /etc/passwd\n}"),
        ],
    )
    .await;
    let err = |path: &'static str| {
        let store = Arc::clone(&store);
        async move {
            Program::load(store.as_ref(), "models", path)
                .await
                .unwrap_err()
                .to_string()
        }
    };
    let missing = err("m.stan").await;
    assert!(
        missing.contains(
            "m.stan:2: `#include helpers.stan` names `helpers.stan`, which is not in the file \
             store `models`"
        ),
        "{missing}"
    );
    let up = err("up.stan").await;
    assert!(
        up.contains("up.stan:2: `#include ../secret.stan` climbs out of the file store"),
        "{up}"
    );
    let abs = err("abs.stan").await;
    assert!(abs.contains("names an absolute path"), "{abs}");
    let gone = err("nope.stan").await;
    assert!(
        gone.contains("the program `nope.stan` is not in the file store `models`"),
        "{gone}"
    );
    let escape = Program::load(store.as_ref(), "models", "../x.stan")
        .await
        .unwrap_err()
        .to_string();
    assert!(escape.contains("climbs out of the file store"), "{escape}");
}

#[test]
fn an_include_cycle_is_refused_naming_the_chain() {
    let program = Program::from_files(
        "models",
        vec![
            ProgramFile::new("m.stan", "functions {\n#include a.stan\n}"),
            ProgramFile::new("a.stan", "#include b.stan\n"),
            ProgramFile::new("b.stan", "#include a.stan\n"),
        ],
    )
    .unwrap();
    let err = program.interface().unwrap_err().to_string();
    assert!(err.contains("a.stan → b.stan → a.stan"), "{err}");
}

#[test]
fn an_error_in_an_included_file_names_that_file() {
    let program = Program::from_files(
        "models",
        vec![
            ProgramFile::new("m.stan", "data {\n#include d.stan\n}"),
            ProgramFile::new("d.stan", "int N;\nreal x[N];\n"),
        ],
    )
    .unwrap();
    let err = program.interface().unwrap_err().to_string();
    assert!(err.contains("d.stan:2:7:"), "{err}");
}

// ---- refusals ----

#[test]
fn a_tuple_or_complex_in_data_is_refused_by_name() {
    let err = refusal("data { tuple(int, real) pair; }");
    assert!(
        err.contains(
            "m.stan:1:8: the data variable `pair` (tuple(int, real)) is a tuple, which no \
             binding produces: bind a tuple's parts as separate variables"
        ),
        "{err}"
    );
    let err = refusal("data { int K; complex_vector[K] z; }");
    assert!(
        err.contains("the data variable `z` (complex_vector[K]) is complex"),
        "{err}"
    );
    assert!(err.contains("bind its real and imaginary parts"), "{err}");
}

#[test]
fn the_old_array_syntax_is_refused_with_the_new_spelling() {
    let err = refusal("data { int N; real y[N]; }");
    assert!(
        err.contains("`real y[…]` is the array syntax Stan 2.33 removed; write `array[…] real y`"),
        "{err}"
    );
}

#[test]
fn what_is_not_a_program_is_refused_with_its_place() {
    let cases = [
        ("data { int N; }\nfoo { }", "m.stan:2:1: expected a block"),
        (
            "parameters { real mu; }\ndata { int N; }",
            "m.stan:2:1: the `data` block must come before the `parameters` block",
        ),
        ("data { int N; }\ndata { int M; }", "a second `data` block"),
        ("data { int N;", "this `{` is never closed"),
        (
            "transformed foo { }",
            "expected `data` or `parameters` after `transformed`",
        ),
        (
            "parameters { real mu }",
            "expected `;` after the declaration of `mu`",
        ),
        (
            "data { int<lowr=0> N; }",
            "expected `lower=`, `upper=`, `offset=` or `multiplier=`",
        ),
        ("data { vector N; }", "expected `[` after `vector`"),
        (
            "data { matrix[3] m; }",
            "`matrix` takes two sizes, and has 1",
        ),
        (
            "data { cov_matrix[3, 3] m; }",
            "`cov_matrix` takes one size, and has 2",
        ),
        (
            "data { array[2] array[3] real a; }",
            "an array of arrays is written with one",
        ),
        (
            "data { real int; }",
            "expected a variable name after `real`, found `int`",
        ),
        (
            "data { vector[] v; }",
            "a size is missing between the brackets",
        ),
        (
            "generated quantities { real x; x = 1 }",
            "this statement does not end with `;`",
        ),
        ("# comment\ndata { }", "removed in 2.33"),
    ];
    for (text, expected) in cases {
        let err = refusal(text);
        assert!(err.contains(expected), "{text:?}: {err}");
    }
}
