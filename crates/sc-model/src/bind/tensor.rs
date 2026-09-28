//! A bound value: typed, shaped, and written as CmdStan reads it (Stan TODO
//! §10).
//!
//! Row-major and flat, with the shape beside it — the shape is what every
//! check compares against a declaration, and the nesting is only how the JSON
//! spells it. CmdStan's JSON convention: ints as ints, reals as reals with
//! `"NaN"`, `"Inf"` and `"-Inf"` as strings, and a matrix as an array of its
//! rows, so `matrix[N, K]` and `array[N] vector[K]` are both `N` arrays of `K`.

use sc_error::{Error, Result};
use serde_json::{Number, Value as Json};

/// The scalars of a bound value, all of one element type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Values {
    Int(Vec<i64>),
    Real(Vec<f64>),
}

impl Values {
    pub(crate) fn len(&self) -> usize {
        match self {
            Values::Int(v) => v.len(),
            Values::Real(v) => v.len(),
        }
    }

    /// The `i`th scalar as a number, for a bounds check.
    pub(crate) fn number(&self, i: usize) -> Option<f64> {
        match self {
            Values::Int(v) => v.get(i).map(|n| *n as f64),
            Values::Real(v) => v.get(i).copied(),
        }
    }

    /// The `i`th scalar as JSON.
    pub(crate) fn json(&self, i: usize) -> Json {
        match self {
            Values::Int(v) => Json::Number(Number::from(v[i])),
            Values::Real(v) => real(v[i]),
        }
    }
}

/// A real as CmdStan's JSON writes one.
pub(crate) fn real(x: f64) -> Json {
    if x.is_nan() {
        Json::String("NaN".to_owned())
    } else if x.is_infinite() {
        Json::String(if x > 0.0 { "Inf" } else { "-Inf" }.to_owned())
    } else {
        Number::from_f64(x).map_or(Json::Null, Json::Number)
    }
}

/// A shaped, typed value — what one binding produces.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tensor {
    /// The shape, outer to inner; empty for a scalar.
    pub shape: Vec<usize>,
    /// The scalars, row-major.
    pub values: Values,
}

impl Tensor {
    pub(crate) fn int(n: i64) -> Tensor {
        Tensor {
            shape: Vec::new(),
            values: Values::Int(vec![n]),
        }
    }

    pub(crate) fn ints(v: Vec<i64>) -> Tensor {
        Tensor {
            shape: vec![v.len()],
            values: Values::Int(v),
        }
    }

    /// The scalar integer, when this is one.
    pub(crate) fn as_int(&self) -> Option<i64> {
        match &self.values {
            Values::Int(v) if self.shape.is_empty() => v.first().copied(),
            _ => None,
        }
    }

    /// The scalar, when this is one — for a bound's identifier.
    pub(crate) fn as_number(&self) -> Option<f64> {
        if self.shape.is_empty() {
            self.values.number(0)
        } else {
            None
        }
    }

    /// How many scalars — what the data-values cap counts.
    pub(crate) fn count(&self) -> usize {
        self.values.len()
    }

    /// The same value with every scalar a real — what a `real` declaration
    /// receives, so the data file says `1.0` where the program reads a real.
    pub(crate) fn into_real(self) -> Tensor {
        match self.values {
            Values::Int(v) => Tensor {
                shape: self.shape,
                values: Values::Real(v.into_iter().map(|n| n as f64).collect()),
            },
            real => Tensor {
                shape: self.shape,
                values: real,
            },
        }
    }

    /// The value in CmdStan's JSON: nested row-major, empty axes as empty
    /// arrays.
    pub(crate) fn to_json(&self) -> Json {
        if self.shape.is_empty() {
            return self.values.json(0);
        }
        let mut next = 0;
        self.nest(0, &mut next)
    }

    fn nest(&self, axis: usize, next: &mut usize) -> Json {
        if axis == self.shape.len() {
            let value = self.values.json(*next);
            *next += 1;
            return value;
        }
        Json::Array(
            (0..self.shape[axis])
                .map(|_| self.nest(axis + 1, next))
                .collect(),
        )
    }

    /// The first few scalars, for the preview.
    pub(crate) fn first(&self, n: usize) -> Vec<Json> {
        (0..self.count().min(n))
            .map(|i| self.values.json(i))
            .collect()
    }

    /// A `value` binding's literal: a number, `"NaN"`/`"Inf"`/`"-Inf"`, a
    /// boolean (as 0/1), or a rectangular nesting of them. Integers stay
    /// integers unless a real is among them.
    pub(crate) fn from_literal(json: &Json) -> Result<Tensor> {
        let mut shape = Vec::new();
        let mut probe = json;
        while let Json::Array(items) = probe {
            shape.push(items.len());
            match items.first() {
                Some(first) => probe = first,
                None => break,
            }
        }
        // An empty array ends the probe early; everything below it is empty
        // too, and nothing deeper can be known — `[]` is `array[0]`.
        let mut scalars = Vec::new();
        collect(json, &shape, 0, &mut scalars)?;
        let values = if scalars.iter().all(|s| matches!(s, Scalar::Int(_))) {
            Values::Int(
                scalars
                    .iter()
                    .map(|s| match s {
                        Scalar::Int(n) => *n,
                        Scalar::Real(_) => unreachable!("all ints"),
                    })
                    .collect(),
            )
        } else {
            Values::Real(
                scalars
                    .iter()
                    .map(|s| match s {
                        Scalar::Int(n) => *n as f64,
                        Scalar::Real(x) => *x,
                    })
                    .collect(),
            )
        };
        Ok(Tensor { shape, values })
    }
}

enum Scalar {
    Int(i64),
    Real(f64),
}

fn collect(json: &Json, shape: &[usize], axis: usize, out: &mut Vec<Scalar>) -> Result<()> {
    if axis == shape.len() {
        out.push(match json {
            Json::Number(n) => match n.as_i64() {
                Some(i) => Scalar::Int(i),
                None => Scalar::Real(n.as_f64().unwrap_or(f64::NAN)),
            },
            Json::Bool(b) => Scalar::Int(i64::from(*b)),
            Json::String(s) if s == "NaN" => Scalar::Real(f64::NAN),
            Json::String(s) if s == "Inf" || s == "+Inf" => Scalar::Real(f64::INFINITY),
            Json::String(s) if s == "-Inf" => Scalar::Real(f64::NEG_INFINITY),
            other => {
                return Err(Error::invalid(format!(
                    "`{other}` is not a number (a value is numbers, `\"NaN\"`, `\"Inf\"` or \
                     `\"-Inf\"`, nested in arrays)"
                )));
            }
        });
        return Ok(());
    }
    match json {
        Json::Array(items) if items.len() == shape[axis] => {
            for item in items {
                collect(item, shape, axis + 1, out)?;
            }
            Ok(())
        }
        _ => Err(Error::invalid(
            "the value is not rectangular: every array at one depth must have the same length",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_matrix_nests_row_major_and_non_finite_reals_are_strings() {
        let t = Tensor {
            shape: vec![2, 3],
            values: Values::Real(vec![
                1.0,
                2.5,
                f64::NAN,
                f64::INFINITY,
                -1.0,
                f64::NEG_INFINITY,
            ]),
        };
        assert_eq!(
            t.to_json(),
            json!([[1.0, 2.5, "NaN"], ["Inf", -1.0, "-Inf"]])
        );
        assert_eq!(Tensor::int(7).to_json(), json!(7));
        assert_eq!(Tensor::ints(vec![]).to_json(), json!([]));
        // An `array[3] vector[0]` is three empty arrays, not one.
        let empty_rows = Tensor {
            shape: vec![3, 0],
            values: Values::Real(vec![]),
        };
        assert_eq!(empty_rows.to_json(), json!([[], [], []]));
    }

    #[test]
    fn a_literal_keeps_its_shape_and_its_type() {
        let t = Tensor::from_literal(&json!([[1, 0], [0, 1]])).unwrap();
        assert_eq!(t.shape, [2, 2]);
        assert_eq!(t.values, Values::Int(vec![1, 0, 0, 1]));
        let t = Tensor::from_literal(&json!([1, 2.5, "NaN"])).unwrap();
        assert_eq!(t.shape, [3]);
        assert!(matches!(t.values, Values::Real(_)));
        assert_eq!(Tensor::from_literal(&json!(3)).unwrap().as_int(), Some(3));
        assert_eq!(Tensor::from_literal(&json!([])).unwrap().shape, [0]);
        assert!(Tensor::from_literal(&json!([[1], [2, 3]])).is_err());
        assert!(Tensor::from_literal(&json!(["x"])).is_err());
    }
}
