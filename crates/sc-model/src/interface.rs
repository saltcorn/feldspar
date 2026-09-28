//! What a Bayesian program declares: its `data` variables and the shapes of
//! its outputs (Stan TODO §5).
//!
//! Here rather than in `sc-stan` for the reason the binder is (§4): a Bayesian
//! provider in another language declares an [`Interface`] and receives bound
//! data, and must not have to reimplement "a foreign key becomes a 1-based
//! index". The parser that *produces* one from a Stan program is `sc-stan`'s;
//! everything that *reads* one — the binder's checks, the labelling of the
//! draws — is the host's and lives in this crate.
//!
//! ## The size expressions are the point
//!
//! `stanc --info` gives names, base types and the number of dimensions, not the
//! size expressions — and those are what everything here runs on. `vector[J]
//! alpha` says `alpha`'s one axis has size `J`; if `J` is bound to the size of
//! the `counties` dimension, `alpha[j]` is about county `j`. `array[N] int
//! county` sized `N` = 919 beside a binding with 85 values is the error CmdStan
//! would give after a minute of compiling, caught before anything is compiled.
//!
//! So a [`SizeExpr`] keeps its text **and**, where it is simple enough, a small
//! integer expression tree ([`SizeTree`]) that [evaluates](SizeExpr::eval)
//! against the sizes already bound. Anything else — a function call — keeps the
//! text alone and simply does not evaluate: the check that needed it is skipped
//! and Stan performs it at runtime.

use std::collections::BTreeSet;

/// What a program declares, block by block.
///
/// `transformed data` is absent on purpose: nothing binds to it and nothing is
/// drawn from it.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Interface {
    /// The `data` block — what the binder must supply, variable by variable.
    pub data: Vec<Declaration>,
    /// The `parameters` block.
    pub parameters: Vec<Declaration>,
    /// The `transformed parameters` block.
    pub transformed: Vec<Declaration>,
    /// The `generated quantities` block.
    pub generated: Vec<Declaration>,
}

impl Interface {
    /// The `data` variable called `name`.
    pub fn data_variable(&self, name: &str) -> Option<&Declaration> {
        self.data.iter().find(|d| d.name == name)
    }

    /// Every variable the sampler writes a draw of — parameters, transformed
    /// parameters and generated quantities, in that order.
    pub fn outputs(&self) -> impl Iterator<Item = &Declaration> {
        self.parameters
            .iter()
            .chain(&self.transformed)
            .chain(&self.generated)
    }

    /// The output variable called `name`.
    pub fn output(&self, name: &str) -> Option<&Declaration> {
        self.outputs().find(|d| d.name == name)
    }
}

/// A variable's element type — what one scalar of it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Element {
    /// `int`.
    Int,
    /// `real`, and every vector and matrix type.
    Real,
    /// `complex` and its vector and matrix forms. Refused in `data` (§5).
    Complex,
    /// A `tuple(…)`. Refused in `data`, and left unlabelled in the outputs.
    Tuple,
}

impl Element {
    /// The word for it in a sentence.
    pub fn name(&self) -> &'static str {
        match self {
            Element::Int => "int",
            Element::Real => "real",
            Element::Complex => "complex",
            Element::Tuple => "tuple",
        }
    }
}

/// One top-level declaration of a program block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Declaration {
    /// The variable's name.
    pub name: String,
    /// What one scalar of it is.
    pub element: Element,
    /// The **full shape, outer to inner**: `array[N] vector[K]` and `matrix[N,
    /// K]` are both `[N, K]`, which is also how CmdStan's JSON nests them. Empty
    /// for a scalar.
    pub dims: Vec<SizeExpr>,
    /// The declared type as written, constraints and all — `array[N] int<lower=1,
    /// upper=J>` — for the screen and for sentences.
    pub stan_type: String,
    /// The `lower=` bound's expression, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower: Option<String>,
    /// The `upper=` bound's expression, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<String>,
}

impl Declaration {
    /// A declaration of `name` with no bounds, typed as written in `stan_type`.
    pub fn new(
        name: impl Into<String>,
        element: Element,
        dims: Vec<SizeExpr>,
        stan_type: impl Into<String>,
    ) -> Declaration {
        Declaration {
            name: name.into(),
            element,
            dims,
            stan_type: stan_type.into(),
            lower: None,
            upper: None,
        }
    }

    /// The same declaration with bounds.
    pub fn bounded(mut self, lower: Option<&str>, upper: Option<&str>) -> Declaration {
        self.lower = lower.map(str::to_owned);
        self.upper = upper.map(str::to_owned);
        self
    }

    /// How many axes it has: 0 for a scalar.
    pub fn rank(&self) -> usize {
        self.dims.len()
    }
}

/// One size expression of a declaration: its text, and its tree where it has
/// one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SizeExpr {
    /// As written — `N`, `J + 1`, `num_elements(y)`.
    pub text: String,
    /// The integer expression it parsed to, or `None` for one that is not only
    /// identifiers, literals and `+ - * %/% %` (see the module docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<SizeTree>,
}

impl SizeExpr {
    /// A size that is the literal `n`.
    pub fn literal(n: i64) -> SizeExpr {
        SizeExpr {
            text: n.to_string(),
            tree: Some(SizeTree::Int(n)),
        }
    }

    /// A size that is the variable `name`.
    pub fn var(name: impl Into<String>) -> SizeExpr {
        let name = name.into();
        SizeExpr {
            text: name.clone(),
            tree: Some(SizeTree::Var(name)),
        }
    }

    /// A size with a tree, and the text it was written as.
    pub fn parsed(text: impl Into<String>, tree: SizeTree) -> SizeExpr {
        SizeExpr {
            text: text.into(),
            tree: Some(tree),
        }
    }

    /// A size that cannot be evaluated here — a function call — kept as text.
    pub fn opaque(text: impl Into<String>) -> SizeExpr {
        SizeExpr {
            text: text.into(),
            tree: None,
        }
    }

    /// The value, given the sizes bound so far; `None` when the expression is
    /// opaque, names a variable `lookup` does not know, divides by zero or
    /// overflows. `None` means "not checked here", never "wrong".
    pub fn eval(&self, lookup: &dyn Fn(&str) -> Option<i64>) -> Option<i64> {
        self.tree.as_ref()?.eval(lookup)
    }

    /// The variable, when the whole expression is one bare identifier — the
    /// case labelling runs on (§15): `vector[J] alpha` is labelled by whatever
    /// `J` is the size of.
    pub fn identifier(&self) -> Option<&str> {
        match &self.tree {
            Some(SizeTree::Var(name)) => Some(name),
            _ => None,
        }
    }

    /// Every variable the expression names.
    pub fn variables(&self) -> BTreeSet<&str> {
        let mut out = BTreeSet::new();
        if let Some(tree) = &self.tree {
            tree.collect_variables(&mut out);
        }
        out
    }
}

impl std::fmt::Display for SizeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// An integer size expression: identifiers, literals, and Stan's integer
/// arithmetic.
///
/// Externally tagged (`{"var": "N"}`, `{"binary": {…}}`) rather than with a
/// `node` field, because serde's internally tagged form wraps the serializer
/// once per level and a recursive type then never stops instantiating.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeTree {
    /// An integer literal.
    Int(i64),
    /// A variable — a `data` or `transformed data` integer.
    Var(String),
    /// Unary minus.
    Neg(Box<SizeTree>),
    /// A binary operation.
    Binary {
        /// Which one.
        op: SizeOp,
        /// Its left operand.
        left: Box<SizeTree>,
        /// Its right operand.
        right: Box<SizeTree>,
    },
}

/// Stan's integer operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `%/%` — integer division, truncating toward zero as C++ does.
    IntDiv,
    /// `%` — the remainder of that division.
    Mod,
}

impl SizeTree {
    /// A binary node.
    pub fn binary(op: SizeOp, left: SizeTree, right: SizeTree) -> SizeTree {
        SizeTree::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    /// The value; see [`SizeExpr::eval`].
    pub fn eval(&self, lookup: &dyn Fn(&str) -> Option<i64>) -> Option<i64> {
        match self {
            SizeTree::Int(n) => Some(*n),
            SizeTree::Var(name) => lookup(name),
            SizeTree::Neg(inner) => inner.eval(lookup)?.checked_neg(),
            SizeTree::Binary { op, left, right } => {
                let (l, r) = (left.eval(lookup)?, right.eval(lookup)?);
                match op {
                    SizeOp::Add => l.checked_add(r),
                    SizeOp::Sub => l.checked_sub(r),
                    SizeOp::Mul => l.checked_mul(r),
                    // `checked_div` and `checked_rem` truncate toward zero and
                    // answer `None` for a zero divisor — Stan's semantics, and
                    // "not checked here" rather than a panic.
                    SizeOp::IntDiv => l.checked_div(r),
                    SizeOp::Mod => l.checked_rem(r),
                }
            }
        }
    }

    fn collect_variables<'a>(&'a self, out: &mut BTreeSet<&'a str>) {
        match self {
            SizeTree::Int(_) => {}
            SizeTree::Var(name) => {
                out.insert(name);
            }
            SizeTree::Neg(inner) => inner.collect_variables(out),
            SizeTree::Binary { left, right, .. } => {
                left.collect_variables(out);
                right.collect_variables(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(name: &str) -> Option<i64> {
        match name {
            "N" => Some(919),
            "J" => Some(85),
            _ => None,
        }
    }

    #[test]
    fn a_size_evaluates_against_the_sizes_bound_so_far() {
        // `(N + 1) * 2 %/% J`, parsed as the tree a parser would build.
        let tree = SizeTree::binary(
            SizeOp::IntDiv,
            SizeTree::binary(
                SizeOp::Mul,
                SizeTree::binary(SizeOp::Add, SizeTree::Var("N".into()), SizeTree::Int(1)),
                SizeTree::Int(2),
            ),
            SizeTree::Var("J".into()),
        );
        let size = SizeExpr::parsed("(N + 1) * 2 %/% J", tree);
        assert_eq!(size.eval(&sizes), Some(1840 / 85));
        assert_eq!(size.variables(), BTreeSet::from(["J", "N"]));
        assert_eq!(size.identifier(), None);
        assert_eq!(SizeExpr::var("J").identifier(), Some("J"));
    }

    #[test]
    fn integer_division_truncates_toward_zero_as_stan_does() {
        let div = |l, r| SizeTree::binary(SizeOp::IntDiv, SizeTree::Int(l), SizeTree::Int(r));
        let rem = |l, r| SizeTree::binary(SizeOp::Mod, SizeTree::Int(l), SizeTree::Int(r));
        assert_eq!(div(-7, 2).eval(&sizes), Some(-3));
        assert_eq!(rem(-7, 2).eval(&sizes), Some(-1));
        assert_eq!(
            SizeTree::Neg(Box::new(SizeTree::Int(3))).eval(&sizes),
            Some(-3)
        );
    }

    #[test]
    fn what_cannot_be_evaluated_is_none_and_never_a_panic() {
        // An unknown variable, a zero divisor, an overflow, and a function call
        // are all "not checked here".
        assert_eq!(SizeExpr::var("K").eval(&sizes), None);
        let by_zero = SizeTree::binary(SizeOp::Mod, SizeTree::Int(1), SizeTree::Int(0));
        assert_eq!(by_zero.eval(&sizes), None);
        let huge = SizeTree::binary(SizeOp::Mul, SizeTree::Int(i64::MAX), SizeTree::Int(2));
        assert_eq!(huge.eval(&sizes), None);
        let call = SizeExpr::opaque("num_elements(y)");
        assert_eq!(call.eval(&sizes), None);
        assert!(call.variables().is_empty());
        assert_eq!(call.to_string(), "num_elements(y)");
    }

    #[test]
    fn an_interface_round_trips_as_the_json_a_fit_records() {
        let interface = Interface {
            data: vec![
                Declaration::new("N", Element::Int, vec![], "int<lower=0>")
                    .bounded(Some("0"), None),
                Declaration::new(
                    "county",
                    Element::Int,
                    vec![SizeExpr::var("N")],
                    "array[N] int<lower=1, upper=J>",
                )
                .bounded(Some("1"), Some("J")),
            ],
            parameters: vec![Declaration::new(
                "alpha",
                Element::Real,
                vec![SizeExpr::var("J")],
                "vector[J]",
            )],
            transformed: vec![],
            generated: vec![Declaration::new(
                "y_rep",
                Element::Real,
                vec![SizeExpr::opaque("num_elements(y)")],
                "array[num_elements(y)] real",
            )],
        };
        let json = serde_json::to_value(&interface).expect("json");
        let back: Interface = serde_json::from_value(json).expect("back");
        assert_eq!(back, interface);
        assert_eq!(back.data_variable("county").map(Declaration::rank), Some(1));
        assert_eq!(
            back.outputs().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "y_rep"]
        );
        assert!(back.output("N").is_none());
    }
}
