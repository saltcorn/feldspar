//! A model matrix from a list of **terms**, with R's `model.matrix` coding
//! (TODO-bayesian-regression §3.2–3.3, R1.4).
//!
//! The general version of the binder's `design`: where a `design` one-hot
//! codes each listed column on its own, a `terms` binding is a formula's
//! right-hand side after expansion: a list of terms, each a product of
//! **variables**, and whether the model has an intercept. Which columns each
//! term gets follows the rules R documents for `model.matrix` (Chambers &
//! Hastie 1992, *Statistical Models in S* §2.3 and §5.3; R's `?formula` and
//! `?model.matrix`). The front end that produced the terms (`sc-bayesreg`, or a
//! second formula front end one day) does not re-implement them.
//!
//! - A **variable** ([`VarExpr`]) is a column, or a transform of columns from a
//!   fixed whitelist: `log`, `log1p`, `exp`, `sqrt`, `abs`, `inv`, `I(…)` over
//!   `+ - * / ^`, `scale(x)` and `poly(x, k)`. A bare text or boolean column is
//!   **categorical**; everything else is a number.
//! - A **term** is a set of variables. A numeric variable contributes one
//!   column (`poly(x, k)` contributes *k*), a categorical one a column per
//!   level, either by **contrasts** (treatment coding: every level but the
//!   first) or by **dummy** coding (every level).
//! - Which of the two follows the heuristic of *Statistical Models in S*: a
//!   categorical variable *F* of a term *T* is coded by contrasts when *T*
//!   without *F* is empty (the intercept's margin) or is contained in a term
//!   that comes earlier; otherwise by dummy variables. With no intercept, the
//!   first categorical variable of the first term that has one is dummy coded,
//!   so `0 + region` has a column per region. So `x:f` with no `f` main effect
//!   has a slope per level, and `f + f:x` nests a slope in each level.
//! - An interaction's columns are the products of its variables' columns,
//!   named by joining theirs with `:`, **the first variable varying fastest**.
//! - The levels of a categorical variable are its distinct values in the
//!   binder's order (text by code point, `FALSE < TRUE`), and the reference
//!   level of treatment coding is the first. Its columns are named by the
//!   variable and the level run together: `regionNorth`, `floorTRUE`.
//!
//! What is fitted — each variable's levels, `scale`'s centre and spread,
//! `poly`'s recurrence coefficients — is fitted **once**, on the rows bound,
//! and stored as a [`TermsEncoding`], so the same matrix can be rebuilt for
//! new rows ([`apply_terms`]) and a level never seen is refused by name rather
//! than coded as zeros (TECHNICAL_DESIGN §14.2, "the encoding belongs to the
//! instance").

use std::collections::BTreeSet;

use sc_error::{Error, Result};

use crate::encode::number_at;
use crate::frame::{Column, ColumnType, Frame};

/// The largest degree `poly(x, k)` takes.
pub const MAX_POLY_DEGREE: u32 = 10;

/// A function a variable may apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Function {
    /// Natural logarithm.
    #[serde(rename = "log")]
    Log,
    /// `log(1 + x)`.
    #[serde(rename = "log1p")]
    Log1p,
    /// `e^x`.
    #[serde(rename = "exp")]
    Exp,
    /// Square root.
    #[serde(rename = "sqrt")]
    Sqrt,
    /// Absolute value.
    #[serde(rename = "abs")]
    Abs,
    /// `1 / x`.
    #[serde(rename = "inv")]
    Inv,
    /// `I(…)`: arithmetic, as arithmetic rather than formula operators.
    #[serde(rename = "I")]
    AsIs,
    /// Centre on the mean and divide by the SD of the rows fitted.
    #[serde(rename = "scale")]
    Scale,
    /// Orthogonal polynomials of degree `k` over the rows fitted.
    #[serde(rename = "poly")]
    Poly,
}

impl Function {
    /// The name it is written with.
    pub fn name(self) -> &'static str {
        match self {
            Function::Log => "log",
            Function::Log1p => "log1p",
            Function::Exp => "exp",
            Function::Sqrt => "sqrt",
            Function::Abs => "abs",
            Function::Inv => "inv",
            Function::AsIs => "I",
            Function::Scale => "scale",
            Function::Poly => "poly",
        }
    }

    /// The function written `name`, if it is one of the whitelist.
    pub fn parse(name: &str) -> Option<Function> {
        Some(match name {
            "log" => Function::Log,
            "log1p" => Function::Log1p,
            "exp" => Function::Exp,
            "sqrt" => Function::Sqrt,
            "abs" => Function::Abs,
            "inv" => Function::Inv,
            "I" => Function::AsIs,
            "scale" => Function::Scale,
            "poly" => Function::Poly,
            _ => return None,
        })
    }

    /// Whether its constants are fitted to the rows (and so stored).
    pub fn is_fitted(self) -> bool {
        matches!(self, Function::Scale | Function::Poly)
    }
}

/// An arithmetic operator inside a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Op {
    /// `a + b`.
    #[serde(rename = "+")]
    Add,
    /// `a - b`.
    #[serde(rename = "-")]
    Sub,
    /// `a * b`.
    #[serde(rename = "*")]
    Mul,
    /// `a / b`.
    #[serde(rename = "/")]
    Div,
    /// `a ^ b`.
    #[serde(rename = "^")]
    Pow,
    /// `-a`.
    #[serde(rename = "neg")]
    Neg,
}

impl Op {
    fn precedence(self) -> u8 {
        match self {
            Op::Add | Op::Sub => 1,
            Op::Mul | Op::Div => 2,
            Op::Neg => 3,
            Op::Pow => 4,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Op::Add => "+",
            Op::Sub | Op::Neg => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Pow => "^",
        }
    }
}

/// One variable of a formula: a column, or a whitelisted transform of columns.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VarExpr {
    /// A column of the dataset, by name.
    Column(String),
    /// A number literal (inside arithmetic, or `poly`'s degree).
    Number(f64),
    /// A function applied to its arguments.
    Call {
        /// The function.
        function: Function,
        /// Its arguments.
        args: Vec<VarExpr>,
    },
    /// Arithmetic: two arguments, or one for [`Op::Neg`].
    Op {
        /// The operator.
        op: Op,
        /// Its operands.
        args: Vec<VarExpr>,
    },
}

impl VarExpr {
    /// A column.
    pub fn column(name: impl Into<String>) -> VarExpr {
        VarExpr::Column(name.into())
    }

    /// `function(args…)`.
    pub fn call(function: Function, args: Vec<VarExpr>) -> VarExpr {
        VarExpr::Call { function, args }
    }

    /// The text R would print it as — what its columns are named by:
    /// `area`, `log(area)`, `I(age^2)`, `poly(age, 2)`.
    pub fn label(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, parent: u8) {
        match self {
            VarExpr::Column(name) => out.push_str(&quote_name(name)),
            VarExpr::Number(x) => out.push_str(&number_text(*x)),
            VarExpr::Call { function, args } => {
                out.push_str(function.name());
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    a.write(out, 0);
                }
                out.push(')');
            }
            VarExpr::Op { op, args } => {
                let prec = op.precedence();
                let paren = prec < parent;
                if paren {
                    out.push('(');
                }
                match (op, args.as_slice()) {
                    (Op::Neg, [a]) => {
                        out.push('-');
                        a.write(out, prec + 1);
                    }
                    (Op::Pow, [a, b]) => {
                        // Right-associative: the base is parenthesised at equal
                        // precedence, the exponent is not.
                        a.write(out, prec + 1);
                        out.push('^');
                        b.write(out, prec);
                    }
                    (_, [a, b]) => {
                        a.write(out, prec);
                        out.push(' ');
                        out.push_str(op.symbol());
                        out.push(' ');
                        // Left-associative: `a - (b - c)` keeps its brackets.
                        b.write(out, prec + 1);
                    }
                    _ => out.push_str("<malformed>"),
                }
                if paren {
                    out.push(')');
                }
            }
        }
    }

    /// Every column it reads, in order of appearance, each once.
    pub fn columns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            VarExpr::Column(c) => {
                if !out.contains(&c.as_str()) {
                    out.push(c);
                }
            }
            VarExpr::Number(_) => {}
            VarExpr::Call { args, .. } | VarExpr::Op { args, .. } => {
                for a in args {
                    a.collect_columns(out);
                }
            }
        }
    }

    /// The bare column it is, if it is one.
    pub fn as_column(&self) -> Option<&str> {
        match self {
            VarExpr::Column(c) => Some(c),
            _ => None,
        }
    }

    /// Whether it is categorical, given the types of the columns: a bare text
    /// or boolean column.
    pub fn is_categorical(&self, ty: &dyn Fn(&str) -> Option<ColumnType>) -> bool {
        match self {
            VarExpr::Column(c) => matches!(ty(c), Some(ColumnType::Str | ColumnType::Bool)),
            _ => false,
        }
    }

    /// The structure is one this module evaluates: argument counts, `scale`
    /// and `poly` outermost, `poly`'s degree a whole number in range, and no
    /// bare number as a variable. Each refusal is a sentence.
    pub fn check(&self) -> Result<()> {
        match self {
            VarExpr::Number(_) => Err(Error::invalid(format!(
                "`{}` is a number, not a variable",
                self.label()
            ))),
            VarExpr::Call {
                function: Function::Scale,
                args,
            } => match args.as_slice() {
                [inner] => inner.check_inner(),
                _ => Err(Error::invalid(
                    "`scale(x)` takes one argument (its centre and scale are fitted)",
                )),
            },
            VarExpr::Call {
                function: Function::Poly,
                args,
            } => match args.as_slice() {
                [inner, VarExpr::Number(k)] => {
                    inner.check_inner()?;
                    poly_degree(*k).map(|_| ())
                }
                _ => Err(Error::invalid(
                    "`poly(x, k)` takes a variable and a whole-number degree",
                )),
            },
            other => other.check_inner(),
        }
    }

    fn check_inner(&self) -> Result<()> {
        match self {
            VarExpr::Column(_) | VarExpr::Number(_) => Ok(()),
            VarExpr::Call { function, args } => {
                if function.is_fitted() {
                    return Err(Error::invalid(format!(
                        "`{}` can only be a whole variable, not part of `{}`",
                        function.name(),
                        self.label()
                    )));
                }
                if args.len() != 1 {
                    return Err(Error::invalid(format!(
                        "`{}` takes one argument",
                        function.name()
                    )));
                }
                args[0].check_inner()
            }
            VarExpr::Op { op, args } => {
                let want = if *op == Op::Neg { 1 } else { 2 };
                if args.len() != want {
                    return Err(Error::invalid(format!(
                        "`{}` takes {want} operand{}",
                        op.symbol(),
                        if want == 1 { "" } else { "s" }
                    )));
                }
                args.iter().try_for_each(VarExpr::check_inner)
            }
        }
    }
}

fn poly_degree(k: f64) -> Result<u32> {
    if k.fract() != 0.0 || k < 1.0 || k > f64::from(MAX_POLY_DEGREE) {
        return Err(Error::invalid(format!(
            "the degree of `poly` must be a whole number from 1 to {MAX_POLY_DEGREE}, and it is \
             {}",
            number_text(k)
        )));
    }
    #[allow(clippy::cast_possible_truncation)] // checked to be in 1..=10 just above
    Ok(k as u32)
}

/// A name as R prints it: bare when it is syntactic, else in backquotes.
fn quote_name(name: &str) -> String {
    let mut chars = name.chars();
    let syntactic = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => true,
        Some('.') => !name.chars().nth(1).is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    } && name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    if syntactic {
        name.to_owned()
    } else {
        format!("`{}`", name.replace('`', "\\`"))
    }
}

/// A number as R prints it in a formula: `2`, `0.5`, `-1`, `1e-10`.
pub fn number_text(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        #[allow(clippy::cast_possible_truncation)] // whole and below 2^53
        let n = x as i64;
        n.to_string()
    } else if x != 0.0 && (x.abs() < 1e-4 || x.abs() >= 1e15) {
        format!("{x:e}")
    } else {
        format!("{x}")
    }
}

/// A right-hand side after expansion: what a `terms` binding holds.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct TermsSpec {
    /// The terms, in the order their columns come; each a product of variables
    /// listed in the order they first appear in the formula.
    pub terms: Vec<Vec<VarExpr>>,
    /// Whether the model has an intercept — which decides the coding even when
    /// the intercept is not a column of this matrix (brms centres it out).
    pub intercept: bool,
    /// Whether the matrix starts with a column of ones named `Intercept` —
    /// `0 + Intercept` in brms's syntax, an intercept that is an ordinary
    /// coefficient. Implies `intercept` for the coding.
    pub intercept_column: bool,
}

/// How a variable of a term is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coding {
    /// A number: its own column (or columns, for `poly`).
    Numeric,
    /// Treatment contrasts: a column per level but the first.
    Contrasts,
    /// Dummy variables: a column per level.
    Dummy,
}

/// The coding of every variable of every term, by the rule in the module docs.
/// `terms` holds each term's variables by label; `categorical` says which
/// labels are categorical.
pub fn code_terms(
    terms: &[Vec<String>],
    intercept: bool,
    categorical: &dyn Fn(&str) -> bool,
) -> Vec<Vec<Coding>> {
    let sets: Vec<BTreeSet<&str>> = terms
        .iter()
        .map(|t| t.iter().map(String::as_str).collect())
        .collect();
    let mut codes: Vec<Vec<Coding>> = Vec::with_capacity(terms.len());
    for (j, term) in terms.iter().enumerate() {
        let mut row = Vec::with_capacity(term.len());
        for v in term {
            if !categorical(v) {
                row.push(Coding::Numeric);
                continue;
            }
            let mut rest = sets[j].clone();
            rest.remove(v.as_str());
            let margin = rest.is_empty() || sets[..j].iter().any(|t| rest.is_subset(t));
            row.push(if margin {
                Coding::Contrasts
            } else {
                Coding::Dummy
            });
        }
        codes.push(row);
    }
    if !intercept {
        'first: for row in &mut codes {
            for code in row.iter_mut() {
                if *code != Coding::Numeric {
                    *code = Coding::Dummy;
                    break 'first;
                }
            }
        }
    }
    codes
}

/// A variable as fitted: what rebuilding its columns for new rows needs.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fitted {
    /// Categorical, with its levels in order (the first is the reference).
    Categorical {
        /// The levels' texts: the values, `FALSE`/`TRUE` for a boolean.
        levels: Vec<String>,
    },
    /// A number, computed row by row.
    Numeric,
    /// `scale(x)`: `(x - center) / scale`.
    Scale {
        /// The mean of the rows fitted.
        center: f64,
        /// Their SD (denominator n − 1).
        scale: f64,
    },
    /// `poly(x, k)`: the three-term recurrence of the orthogonal polynomials
    /// over the rows fitted, as R's `poly` stores it (`alpha`, `norm2`).
    Poly {
        /// The degree.
        degree: u32,
        /// The recurrence's centres, one per degree.
        alpha: Vec<f64>,
        /// The squared norms: 1, then one per degree from 0 to `degree`.
        norm2: Vec<f64>,
    },
}

/// One variable of a [`TermsEncoding`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VariableEncoding {
    /// The variable.
    pub expr: VarExpr,
    /// Its label.
    pub label: String,
    /// What was fitted.
    pub fitted: Fitted,
}

/// One term of a [`TermsEncoding`]: its variables (indices into
/// [`TermsEncoding::variables`]) and how each is coded.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TermEncoding {
    /// The term's label: `area`, `area:region`.
    pub label: String,
    /// Its variables.
    pub variables: Vec<usize>,
    /// The coding of each.
    pub codings: Vec<Coding>,
    /// The columns it produced.
    pub columns: Vec<String>,
}

/// A `terms` model matrix as fitted: stored on the instance, so the same
/// columns can be rebuilt for rows it has not seen.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TermsEncoding {
    /// Whether the matrix starts with `Intercept`.
    pub intercept_column: bool,
    /// The distinct variables, in order of first appearance.
    pub variables: Vec<VariableEncoding>,
    /// The terms.
    pub terms: Vec<TermEncoding>,
}

impl TermsEncoding {
    /// The matrix's column names, in order.
    pub fn columns(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.intercept_column {
            out.push(INTERCEPT_COLUMN.to_owned());
        }
        for t in &self.terms {
            out.extend(t.columns.iter().cloned());
        }
        out
    }

    /// As stored.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).map_err(|e| Error::msg(format!("terms encoding: {e}")))
    }

    /// From storage.
    pub fn from_json(json: &serde_json::Value) -> Result<TermsEncoding> {
        serde_json::from_value(json.clone())
            .map_err(|e| Error::msg(format!("a stored terms encoding does not read: {e}")))
    }
}

/// The name of the column of ones [`TermsSpec::intercept_column`] adds.
pub const INTERCEPT_COLUMN: &str = "Intercept";

/// A model matrix: row-major, possibly zero columns wide (a formula with no
/// population-level terms still has rows).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelMatrix {
    /// Column names.
    pub columns: Vec<String>,
    /// Rows.
    pub rows: usize,
    /// Values, row-major.
    pub values: Vec<f64>,
}

impl ModelMatrix {
    /// The column called `name`, down the rows.
    pub fn column(&self, name: &str) -> Option<Vec<f64>> {
        let j = self.columns.iter().position(|c| c == name)?;
        let w = self.columns.len();
        Some((0..self.rows).map(|i| self.values[i * w + j]).collect())
    }
}

/// Fit the terms to `frame` — levels, constants, codings — and build the
/// matrix. Every column a term reads must be in the frame without nulls (the
/// binder's `nulls` policy has run).
pub fn fit_terms(spec: &TermsSpec, frame: &Frame) -> Result<(TermsEncoding, ModelMatrix)> {
    // The distinct variables, by label, in order of first appearance.
    let mut variables: Vec<VarExpr> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut term_vars: Vec<Vec<usize>> = Vec::new();
    for term in &spec.terms {
        if term.is_empty() {
            return Err(Error::invalid("a term has no variables"));
        }
        let mut ids = Vec::with_capacity(term.len());
        for v in term {
            v.check()?;
            let label = v.label();
            let id = match labels.iter().position(|l| *l == label) {
                Some(id) => id,
                None => {
                    labels.push(label);
                    variables.push(v.clone());
                    labels.len() - 1
                }
            };
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        term_vars.push(ids);
    }
    let ty = |c: &str| frame.column(c).map(Column::kind);
    let mut fitted = Vec::with_capacity(variables.len());
    for (v, label) in variables.iter().zip(&labels) {
        fitted.push(VariableEncoding {
            expr: v.clone(),
            label: label.clone(),
            fitted: fit_variable(v, label, frame, &ty)?,
        });
    }
    let term_labels: Vec<Vec<String>> = term_vars
        .iter()
        .map(|ids| ids.iter().map(|i| labels[*i].clone()).collect())
        .collect();
    let categorical = |label: &str| {
        labels
            .iter()
            .position(|l| l == label)
            .is_some_and(|i| matches!(fitted[i].fitted, Fitted::Categorical { .. }))
    };
    let codes = code_terms(
        &term_labels,
        spec.intercept || spec.intercept_column,
        &categorical,
    );
    let mut terms = Vec::with_capacity(term_vars.len());
    for ((ids, codings), names) in term_vars.into_iter().zip(codes).zip(&term_labels) {
        for (id, coding) in ids.iter().zip(&codings) {
            if let (Coding::Contrasts, Fitted::Categorical { levels }) =
                (coding, &fitted[*id].fitted)
            {
                if levels.len() < 2 {
                    return Err(Error::invalid(format!(
                        "`{}` has only one value (`{}`) in the rows bound, so it cannot be \
                         compared with anything; leave it out of the formula, or filter the \
                         dataset differently",
                        fitted[*id].label,
                        levels.first().map_or("", String::as_str)
                    )));
                }
            }
        }
        terms.push(TermEncoding {
            label: names.join(":"),
            columns: Vec::new(),
            variables: ids,
            codings,
        });
    }
    let mut encoding = TermsEncoding {
        intercept_column: spec.intercept_column,
        variables: fitted,
        terms,
    };
    let matrix = build(&mut encoding, frame, true)?;
    Ok((encoding, matrix))
}

/// The matrix of `encoding` over new rows, with what was fitted: a level not
/// seen at fit time, a null, or a transform with no finite value is refused by
/// name.
pub fn apply_terms(encoding: &TermsEncoding, frame: &Frame) -> Result<ModelMatrix> {
    let mut encoding = encoding.clone();
    build(&mut encoding, frame, false)
}

fn fit_variable(
    v: &VarExpr,
    label: &str,
    frame: &Frame,
    ty: &dyn Fn(&str) -> Option<ColumnType>,
) -> Result<Fitted> {
    for c in v.columns() {
        match ty(c) {
            None => {
                return Err(Error::invalid(format!(
                    "`{label}` reads `{c}`, and the dataset has no such column"
                )));
            }
            Some(ColumnType::Null) => {
                return Err(Error::invalid(format!(
                    "`{c}` is null in every row, so `{label}` cannot be computed"
                )));
            }
            Some(ColumnType::Date) => {
                return Err(Error::invalid(format!(
                    "`{c}` is a date; make it a number in the dataset (days since a start, a \
                     year) to use it in a formula"
                )));
            }
            Some(ColumnType::Str) if v.as_column().is_none() => {
                return Err(Error::invalid(format!(
                    "`{c}` is text, so `{label}` has no numeric value; a text column can only \
                     appear on its own, as a categorical variable"
                )));
            }
            _ => {}
        }
    }
    if let Some(c) = v.as_column() {
        let column = frame
            .column(c)
            .ok_or_else(|| Error::invalid(format!("no column `{c}`")))?;
        match column {
            Column::Str(values) => {
                let set: BTreeSet<&str> = values.iter().flatten().map(String::as_str).collect();
                return Ok(Fitted::Categorical {
                    levels: set.into_iter().map(str::to_owned).collect(),
                });
            }
            Column::Bool(values) => {
                let mut levels = Vec::new();
                if values.contains(&Some(false)) {
                    levels.push("FALSE".to_owned());
                }
                if values.contains(&Some(true)) {
                    levels.push("TRUE".to_owned());
                }
                return Ok(Fitted::Categorical { levels });
            }
            _ => return Ok(Fitted::Numeric),
        }
    }
    match v {
        VarExpr::Call {
            function: Function::Scale,
            args,
        } => {
            let x = numbers(&args[0], label, frame)?;
            let n = x.len() as f64;
            let center = x.iter().sum::<f64>() / n;
            let var = x.iter().map(|v| (v - center).powi(2)).sum::<f64>() / (n - 1.0);
            let scale = var.sqrt();
            if !(scale.is_finite() && scale > 0.0) {
                return Err(Error::invalid(format!(
                    "`{label}` cannot be scaled: `{}` does not vary over the rows bound",
                    args[0].label()
                )));
            }
            Ok(Fitted::Scale { center, scale })
        }
        VarExpr::Call {
            function: Function::Poly,
            args,
        } => {
            let degree = match &args[1] {
                VarExpr::Number(k) => poly_degree(*k)?,
                _ => return Err(Error::invalid("`poly(x, k)` needs a number for `k`")),
            };
            let x = numbers(&args[0], label, frame)?;
            let distinct: BTreeSet<u64> = x.iter().map(|v| v.to_bits()).collect();
            if distinct.len() <= degree as usize {
                return Err(Error::invalid(format!(
                    "`{label}`: the degree must be less than the number of distinct values of \
                     `{}`, which is {}",
                    args[0].label(),
                    distinct.len()
                )));
            }
            let (alpha, norm2) = poly_fit(&x, degree as usize);
            Ok(Fitted::Poly {
                degree,
                alpha,
                norm2,
            })
        }
        _ => Ok(Fitted::Numeric),
    }
}

/// The orthogonal polynomial recurrence over `x` (Stieltjes): `P_0 = 1`,
/// `P_1 = x - α_1`, `P_{i+1} = (x - α_{i+1}) P_i - (n_i / n_{i-1}) P_{i-1}`
/// with `n_i = Σ P_i²` and `α_{i+1} = Σ x P_i² / n_i` — the same basis R's
/// `poly` produces through a QR decomposition, stored the way it stores it.
fn poly_fit(x: &[f64], degree: usize) -> (Vec<f64>, Vec<f64>) {
    let mut alpha = Vec::with_capacity(degree);
    let mut norm2 = vec![1.0];
    let mut prev: Vec<f64> = vec![0.0; x.len()];
    let mut cur: Vec<f64> = vec![1.0; x.len()];
    let mut n_prev = 1.0;
    for i in 0..=degree {
        let n_cur: f64 = cur.iter().map(|p| p * p).sum();
        norm2.push(n_cur);
        if i == degree {
            break;
        }
        let a = x.iter().zip(&cur).map(|(x, p)| x * p * p).sum::<f64>() / n_cur;
        alpha.push(a);
        let ratio = if i == 0 { 0.0 } else { n_cur / n_prev };
        let next: Vec<f64> = (0..x.len())
            .map(|r| (x[r] - a) * cur[r] - ratio * prev[r])
            .collect();
        prev = std::mem::replace(&mut cur, next);
        n_prev = n_cur;
    }
    (alpha, norm2)
}

/// `poly`'s columns for `x`, from its stored recurrence.
fn poly_columns(x: &[f64], alpha: &[f64], norm2: &[f64]) -> Vec<Vec<f64>> {
    let degree = alpha.len();
    let mut out = Vec::with_capacity(degree);
    let mut prev = vec![1.0; x.len()];
    let mut cur: Vec<f64> = x.iter().map(|v| v - alpha[0]).collect();
    for i in 1..=degree {
        out.push(cur.clone());
        if i == degree {
            break;
        }
        // norm2 = [1, n_0, n_1, …]: P_{i+1} = (x - α_{i+1}) P_i - (n_i / n_{i-1}) P_{i-1}.
        let ratio = norm2[i + 1] / norm2[i];
        let next: Vec<f64> = (0..x.len())
            .map(|r| (x[r] - alpha[i]) * cur[r] - ratio * prev[r])
            .collect();
        prev = std::mem::replace(&mut cur, next);
    }
    for (i, col) in out.iter_mut().enumerate() {
        let s = norm2[i + 2].sqrt();
        for v in col.iter_mut() {
            *v /= s;
        }
    }
    out
}

/// A numeric expression's value in every row, refusing nulls, text and
/// values that are not finite.
fn numbers(v: &VarExpr, label: &str, frame: &Frame) -> Result<Vec<f64>> {
    (0..frame.rows)
        .map(|i| {
            let x = eval(v, frame, i, label)?;
            if x.is_finite() {
                Ok(x)
            } else {
                let cols: Vec<String> = v
                    .columns()
                    .iter()
                    .map(|c| {
                        let value = frame
                            .column(c)
                            .and_then(|col| number_at(col, i))
                            .map_or_else(|| "null".to_owned(), number_text);
                        format!("`{c}` = {value}")
                    })
                    .collect();
                Err(Error::invalid(format!(
                    "`{label}` is {} in row {} ({}); filter those rows out of the dataset, or \
                     change the transform",
                    if x.is_nan() {
                        "not a number"
                    } else {
                        "infinite"
                    },
                    i + 1,
                    cols.join(", ")
                )))
            }
        })
        .collect()
}

fn eval(v: &VarExpr, frame: &Frame, i: usize, label: &str) -> Result<f64> {
    Ok(match v {
        VarExpr::Number(x) => *x,
        VarExpr::Column(c) => {
            let column = frame
                .column(c)
                .ok_or_else(|| Error::invalid(format!("no column `{c}`")))?;
            if matches!(column, Column::Str(_)) {
                return Err(Error::invalid(format!(
                    "`{c}` is text, so `{label}` has no numeric value"
                )));
            }
            number_at(column, i).ok_or_else(|| {
                Error::invalid(format!(
                    "`{c}` is null in row {}, so `{label}` cannot be computed (the dataset's \
                     `nulls` policy should have dropped it)",
                    i + 1
                ))
            })?
        }
        VarExpr::Call { function, args } => {
            let x = eval(&args[0], frame, i, label)?;
            match function {
                Function::Log => x.ln(),
                Function::Log1p => x.ln_1p(),
                Function::Exp => x.exp(),
                Function::Sqrt => x.sqrt(),
                Function::Abs => x.abs(),
                Function::Inv => 1.0 / x,
                Function::AsIs => x,
                Function::Scale | Function::Poly => {
                    return Err(Error::msg(format!(
                        "`{label}`: a fitted transform inside another (a bug: `check` refuses it)"
                    )));
                }
            }
        }
        VarExpr::Op { op, args } => {
            let a = eval(&args[0], frame, i, label)?;
            if *op == Op::Neg {
                return Ok(-a);
            }
            let b = eval(&args[1], frame, i, label)?;
            match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div => a / b,
                Op::Pow => a.powf(b),
                Op::Neg => -a,
            }
        }
    })
}

/// One variable's columns over `frame`: `(suffix-free name, values)`.
fn variable_columns(
    var: &VariableEncoding,
    coding: Coding,
    frame: &Frame,
) -> Result<Vec<(String, Vec<f64>)>> {
    let label = &var.label;
    match &var.fitted {
        Fitted::Categorical { levels } => {
            let c = var
                .expr
                .as_column()
                .ok_or_else(|| Error::msg("a categorical variable that is not a column"))?;
            let column = frame
                .column(c)
                .ok_or_else(|| Error::invalid(format!("the rows have no column `{c}`")))?;
            let mut index = Vec::with_capacity(frame.rows);
            for i in 0..frame.rows {
                let text = match column {
                    Column::Str(v) => v.get(i).cloned().flatten(),
                    Column::Bool(v) => v
                        .get(i)
                        .copied()
                        .flatten()
                        .map(|b| if b { "TRUE" } else { "FALSE" }.to_owned()),
                    _ => {
                        return Err(Error::invalid(format!(
                            "`{c}` was categorical when this was fitted, and is a `{}` column \
                             now",
                            column.kind().name()
                        )));
                    }
                };
                let text = text.ok_or_else(|| {
                    Error::invalid(format!(
                        "`{c}` is null in row {}, so its level is unknown",
                        i + 1
                    ))
                })?;
                let at = levels.iter().position(|l| *l == text).ok_or_else(|| {
                    Error::invalid(format!(
                        "`{c}` is `{text}` in row {}, a value it never had when this was \
                         fitted (its levels were {})",
                        i + 1,
                        levels
                            .iter()
                            .map(|l| format!("`{l}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
                index.push(at);
            }
            let first = usize::from(coding == Coding::Contrasts);
            Ok((first..levels.len())
                .map(|l| {
                    (
                        format!("{label}{}", levels[l]),
                        index.iter().map(|i| f64::from(u8::from(*i == l))).collect(),
                    )
                })
                .collect())
        }
        Fitted::Numeric => Ok(vec![(label.clone(), numbers(&var.expr, label, frame)?)]),
        Fitted::Scale { center, scale } => {
            let VarExpr::Call { args, .. } = &var.expr else {
                return Err(Error::msg("a scaled variable that is not `scale(…)`"));
            };
            let x = numbers(&args[0], label, frame)?;
            Ok(vec![(
                label.clone(),
                x.iter().map(|v| (v - center) / scale).collect(),
            )])
        }
        Fitted::Poly { alpha, norm2, .. } => {
            let VarExpr::Call { args, .. } = &var.expr else {
                return Err(Error::msg("a polynomial variable that is not `poly(…)`"));
            };
            let x = numbers(&args[0], label, frame)?;
            Ok(poly_columns(&x, alpha, norm2)
                .into_iter()
                .enumerate()
                .map(|(d, col)| (format!("{label}{}", d + 1), col))
                .collect())
        }
    }
}

/// Columns by name, each down the rows.
type NamedColumns = Vec<(String, Vec<f64>)>;

/// The matrix, filling in each term's column names when `name` is set.
fn build(encoding: &mut TermsEncoding, frame: &Frame, name: bool) -> Result<ModelMatrix> {
    let rows = frame.rows;
    let mut columns: Vec<(String, Vec<f64>)> = Vec::new();
    if encoding.intercept_column {
        columns.push((INTERCEPT_COLUMN.to_owned(), vec![1.0; rows]));
    }
    // Each (variable, coding) pair is computed once, however many terms use it.
    let mut cache: Vec<((usize, Coding), NamedColumns)> = Vec::new();
    for t in 0..encoding.terms.len() {
        let mut cur: Vec<(String, Vec<f64>)> = vec![(String::new(), vec![1.0; rows])];
        let term = encoding.terms[t].clone();
        for (id, coding) in term.variables.iter().zip(&term.codings) {
            let key = (*id, *coding);
            let cols = match cache.iter().find(|(k, _)| *k == key) {
                Some((_, cols)) => cols.clone(),
                None => {
                    let cols = variable_columns(&encoding.variables[*id], *coding, frame)?;
                    cache.push((key, cols.clone()));
                    cols
                }
            };
            // The first variable varies fastest: the new variable is the outer
            // loop.
            let mut next = Vec::with_capacity(cur.len() * cols.len());
            for (cname, cvals) in &cols {
                for (pname, pvals) in &cur {
                    let joined = if pname.is_empty() {
                        cname.clone()
                    } else {
                        format!("{pname}:{cname}")
                    };
                    next.push((
                        joined,
                        pvals.iter().zip(cvals).map(|(a, b)| a * b).collect(),
                    ));
                }
            }
            cur = next;
        }
        let names: Vec<String> = cur.iter().map(|(n, _)| n.clone()).collect();
        if name {
            encoding.terms[t].columns = names;
        } else if names != encoding.terms[t].columns {
            return Err(Error::msg(format!(
                "the term `{}` gave the columns {names:?}, not the {:?} it was fitted with",
                term.label, encoding.terms[t].columns
            )));
        }
        columns.extend(cur);
    }
    let width = columns.len();
    let mut values = vec![0.0; rows * width];
    for (j, (_, col)) in columns.iter().enumerate() {
        for (i, v) in col.iter().enumerate() {
            values[i * width + j] = *v;
        }
    }
    Ok(ModelMatrix {
        columns: columns.into_iter().map(|(n, _)| n).collect(),
        rows,
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> VarExpr {
        VarExpr::column(name)
    }

    fn frame() -> Frame {
        Frame::new(
            vec![
                (
                    "x".into(),
                    Column::Float(vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0), Some(5.0)]),
                ),
                (
                    "z".into(),
                    Column::Int(vec![Some(2), Some(0), Some(1), Some(1), Some(3)]),
                ),
                (
                    "f".into(),
                    Column::Str(vec![
                        Some("b".into()),
                        Some("a".into()),
                        Some("c".into()),
                        Some("a".into()),
                        Some("b".into()),
                    ]),
                ),
                (
                    "g".into(),
                    Column::Bool(vec![
                        Some(true),
                        Some(false),
                        Some(true),
                        Some(false),
                        Some(false),
                    ]),
                ),
            ],
            Vec::new(),
        )
        .unwrap()
    }

    fn names(terms: Vec<Vec<VarExpr>>, intercept: bool) -> Vec<String> {
        let spec = TermsSpec {
            terms,
            intercept,
            intercept_column: false,
        };
        fit_terms(&spec, &frame()).unwrap().1.columns
    }

    #[test]
    fn a_factor_is_treatment_coded_with_an_intercept_and_dummy_coded_without() {
        assert_eq!(names(vec![vec![col("f")]], true), ["fb", "fc"]);
        assert_eq!(names(vec![vec![col("f")]], false), ["fa", "fb", "fc"]);
        // Only the first factor gets the full coding: `0 + f + g`.
        assert_eq!(
            names(vec![vec![col("f")], vec![col("g")]], false),
            ["fa", "fb", "fc", "gTRUE"]
        );
    }

    #[test]
    fn interactions_follow_the_margin_rule() {
        // x:f with no f main effect: a slope per level.
        assert_eq!(
            names(vec![vec![col("x")], vec![col("x"), col("f")]], true),
            ["x", "x:fb", "x:fc"]
        );
        assert_eq!(
            names(vec![vec![col("x"), col("f")]], true),
            ["x:fa", "x:fb", "x:fc"]
        );
        // f + f:x nests a slope in each level.
        assert_eq!(
            names(vec![vec![col("f")], vec![col("f"), col("x")]], true),
            ["fb", "fc", "fa:x", "fb:x", "fc:x"]
        );
        // f * g: contrasts on both, the first varying fastest.
        assert_eq!(
            names(
                vec![vec![col("f")], vec![col("g")], vec![col("f"), col("g")]],
                true
            ),
            ["fb", "fc", "gTRUE", "fb:gTRUE", "fc:gTRUE"]
        );
        // f:g alone: neither margin is in the model, so both are dummy coded
        // — every cell, which with an intercept is R's rank-deficient answer
        // too.
        assert_eq!(
            names(vec![vec![col("f"), col("g")]], true),
            [
                "fa:gFALSE",
                "fb:gFALSE",
                "fc:gFALSE",
                "fa:gTRUE",
                "fb:gTRUE",
                "fc:gTRUE"
            ]
        );
    }

    #[test]
    fn the_values_are_products_and_indicators() {
        let spec = TermsSpec {
            terms: vec![vec![col("x"), col("f")]],
            intercept: true,
            intercept_column: true,
        };
        let (_, m) = fit_terms(&spec, &frame()).unwrap();
        assert_eq!(m.columns, ["Intercept", "x:fa", "x:fb", "x:fc"]);
        assert_eq!(m.column("Intercept").unwrap(), [1.0; 5]);
        assert_eq!(m.column("x:fb").unwrap(), [1.0, 0.0, 0.0, 0.0, 5.0]);
        assert_eq!(m.column("x:fa").unwrap(), [0.0, 2.0, 0.0, 4.0, 0.0]);
    }

    #[test]
    fn transforms_are_labelled_as_r_prints_them_and_evaluated() {
        let sq = VarExpr::call(
            Function::AsIs,
            vec![VarExpr::Op {
                op: Op::Pow,
                args: vec![col("x"), VarExpr::Number(2.0)],
            }],
        );
        let lg = VarExpr::call(
            Function::Log,
            vec![VarExpr::Op {
                op: Op::Add,
                args: vec![col("z"), VarExpr::Number(1.0)],
            }],
        );
        assert_eq!(sq.label(), "I(x^2)");
        assert_eq!(lg.label(), "log(z + 1)");
        let spec = TermsSpec {
            terms: vec![vec![sq], vec![lg]],
            intercept: true,
            intercept_column: false,
        };
        let (_, m) = fit_terms(&spec, &frame()).unwrap();
        assert_eq!(m.column("I(x^2)").unwrap(), [1.0, 4.0, 9.0, 16.0, 25.0]);
        assert!((m.column("log(z + 1)").unwrap()[1]).abs() < 1e-12);
    }

    #[test]
    fn scale_and_poly_store_their_constants_and_apply_them_to_new_rows() {
        let spec = TermsSpec {
            terms: vec![
                vec![VarExpr::call(Function::Scale, vec![col("x")])],
                vec![VarExpr::call(
                    Function::Poly,
                    vec![col("x"), VarExpr::Number(2.0)],
                )],
            ],
            intercept: true,
            intercept_column: false,
        };
        let (enc, m) = fit_terms(&spec, &frame()).unwrap();
        assert_eq!(m.columns, ["scale(x)", "poly(x, 2)1", "poly(x, 2)2"]);
        // R: poly(1:5, 2) — the linear column is (x - 3) / sqrt(10), the
        // quadratic (2, -1, -2, -1, 2) / sqrt(14); scale(1:5) is (x - 3) / sd.
        let p1 = m.column("poly(x, 2)1").unwrap();
        let p2 = m.column("poly(x, 2)2").unwrap();
        let s = 10f64.sqrt();
        for (got, want) in p1.iter().zip([-2.0 / s, -1.0 / s, 0.0, 1.0 / s, 2.0 / s]) {
            assert!((got - want).abs() < 1e-12, "{p1:?}");
        }
        let s = 14f64.sqrt();
        for (got, want) in p2
            .iter()
            .zip([2.0 / s, -1.0 / s, -2.0 / s, -1.0 / s, 2.0 / s])
        {
            assert!((got - want).abs() < 1e-12, "{p2:?}");
        }
        let sd = 2.5f64.sqrt();
        assert!((m.column("scale(x)").unwrap()[4] - 2.0 / sd).abs() < 1e-12);

        // Two new rows, with the fitted constants — not refitted to them.
        let new = Frame::new(
            vec![("x".into(), Column::Float(vec![Some(3.0), Some(6.0)]))],
            Vec::new(),
        )
        .unwrap();
        let round = TermsEncoding::from_json(&enc.to_json().unwrap()).unwrap();
        let m2 = apply_terms(&round, &new).unwrap();
        assert!(m2.column("scale(x)").unwrap()[0].abs() < 1e-12);
        assert!((m2.column("poly(x, 2)1").unwrap()[1] - 3.0 / 10f64.sqrt()).abs() < 1e-12);
        // x = 6 continues the quadratic: (6-3)^2 - 2 = 7.
        assert!((m2.column("poly(x, 2)2").unwrap()[1] - 7.0 / 14f64.sqrt()).abs() < 1e-12);
    }

    #[test]
    fn an_unseen_level_and_a_non_finite_transform_are_refused_by_name() {
        let spec = TermsSpec {
            terms: vec![vec![col("f")]],
            intercept: true,
            intercept_column: false,
        };
        let (enc, _) = fit_terms(&spec, &frame()).unwrap();
        let new = Frame::new(
            vec![(
                "f".into(),
                Column::Str(vec![Some("a".into()), Some("d".into())]),
            )],
            Vec::new(),
        )
        .unwrap();
        let err = apply_terms(&enc, &new).unwrap_err().to_string();
        assert!(
            err.contains("`f` is `d` in row 2, a value it never had"),
            "{err}"
        );

        let spec = TermsSpec {
            terms: vec![vec![VarExpr::call(Function::Log, vec![col("z")])]],
            intercept: true,
            intercept_column: false,
        };
        let err = fit_terms(&spec, &frame()).unwrap_err().to_string();
        assert!(
            err.contains("`log(z)` is infinite in row 2 (`z` = 0)"),
            "{err}"
        );
    }

    #[test]
    fn a_factor_with_one_level_cannot_be_contrasted() {
        let f = Frame::new(
            vec![(
                "f".into(),
                Column::Str(vec![Some("a".into()), Some("a".into())]),
            )],
            Vec::new(),
        )
        .unwrap();
        let spec = TermsSpec {
            terms: vec![vec![col("f")]],
            intercept: true,
            intercept_column: false,
        };
        let err = fit_terms(&spec, &f).unwrap_err().to_string();
        assert!(err.contains("`f` has only one value (`a`)"), "{err}");
    }

    #[test]
    fn the_json_of_a_variable_is_readable() {
        let v = VarExpr::call(Function::Log, vec![col("area")]);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"call": {"function": "log", "args": [{"column": "area"}]}})
        );
        assert_eq!(serde_json::from_value::<VarExpr>(json).unwrap(), v);
    }
}
