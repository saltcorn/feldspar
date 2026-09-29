//! `pca` — dimensionality reduction, where a row's answer is a vector
//! (TODO task 4.6).
//!
//! The other unsupervised built-in, and the one that exercises
//! [`Outcome::Embedding`](crate::Outcome::Embedding): there is no label, every
//! column is a feature, and a fitted instance answers each row with its
//! coordinates in the space the components span. `predict("…")` answers it for
//! a JSON field, because a vector is not a scalar and rendering it as text would
//! make it unreadable by anything that wanted to use it.
//!
//! **The number of components is configuration, not a hyperparameter.** It has
//! to be, and not by convention: the outcome carries the vector's length
//! (`Embedding { dimensions }`), the outcome is resolved from the configuration
//! before any fit happens, and a value that a grid search could still change
//! would make the declared length a guess. A search over the component count is
//! also not a search — more components always explain more variance, so the grid
//! would pick the largest every time.
//!
//! **It asks the host to standardise**, for k-means' reason: the components of
//! an unscaled fit are the directions of whichever column happens to be measured
//! in larger units.
//!
//! ## Explained variance is computed, not read
//!
//! smartcore keeps its eigenvalues private and exposes only the projection
//! matrix. The explained variance ratio is therefore computed the way it is
//! defined: the variance of each projected column over the total variance of the
//! features. That is the same number the eigenvalues would give, and it has the
//! advantage of being obviously the thing it claims to be.

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;
use smartcore::decomposition::pca::{PCA, PCAParameters};
use smartcore::linalg::basic::arrays::Array;
use smartcore::linalg::basic::matrix::DenseMatrix;

use crate::encode::Matrix;
use crate::frame::Frame;
use crate::provider::{
    FitResult, ModelProvider, OutcomeSpec, ParameterBlock, ParameterRow, Prediction,
};
use crate::providers::{cell, design, feature_names, from_state, to_state, whole_setting};

/// The configuration key holding how many components to keep.
const COMPONENTS: &str = "components";

/// The smartcore model this provider fits and applies.
type Model = PCA<f64, DenseMatrix<f64>>;

/// A fitted projection, as it is stored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct State {
    /// The feature columns, in the order the projection indexes them.
    features: Vec<String>,
    /// How many components a row's vector has.
    components: usize,
    /// smartcore's serialised model.
    model: Json,
}

/// Principal component analysis.
pub struct Pca;

#[async_trait]
impl ModelProvider for Pca {
    fn name(&self) -> &str {
        "pca"
    }

    fn description(&self) -> &str {
        "Principal component analysis: every row becomes a vector in a smaller space"
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![
            FormField::new(COMPONENTS, BasicType::Int)
                .label("Components")
                .required()
                .default_value(2),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Embedding {
            components: COMPONENTS.to_owned(),
        }
    }

    fn standardise(&self) -> bool {
        true
    }

    fn validate(&self, shape: &crate::dataset::DatasetShape, config: &Attrs) -> Result<()> {
        let components = whole_setting(config, COMPONENTS, 2, 1)?;
        if components > shape.columns.len() {
            return Err(Error::invalid(format!(
                "`{COMPONENTS}`: {components} components cannot be found in a dataset of {} \
                 columns",
                shape.columns.len()
            )));
        }
        Ok(())
    }

    async fn fit(&self, frame: &Frame, config: &Attrs, _hyper: &Attrs) -> Result<FitResult> {
        let components = whole_setting(config, COMPONENTS, 2, 1)?;
        let features = feature_names(frame, None);
        let x = design(frame, &features)?;
        if components > x.width() {
            return Err(Error::invalid(format!(
                "`{COMPONENTS}`: {components} components cannot be found in {} encoded columns",
                x.width()
            )));
        }
        let matrix = dense(&x)?;
        let model: Model = PCA::fit(
            &matrix,
            PCAParameters::default().with_n_components(components),
        )
        .map_err(|e| Error::invalid(format!("this projection could not be fitted: {e}")))?;
        let projected = transform(&model, &matrix, components)?;

        let state = State {
            features,
            components,
            model: to_state(&model)?,
        };
        Ok(FitResult::new(to_state(&state)?)
            .parameter(loadings(&model, &x, components)?)
            .parameter(explained(&x, &projected, components)?)
            .parameter(ParameterBlock::scalar("components", components as f64))
            .parameter(ParameterBlock::scalar("observations", x.rows() as f64)))
    }

    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        let state: State = from_state(self.name(), state)?;
        let x = design(frame, &state.features)?;
        let model: Model = from_state(self.name(), &state.model)?;
        Ok(transform(&model, &dense(&x)?, state.components)?
            .into_iter()
            .map(|values| Prediction::Vector { values })
            .collect())
    }
}

/// A feature matrix as smartcore's.
fn dense(x: &Matrix) -> Result<DenseMatrix<f64>> {
    DenseMatrix::from_2d_vec(&x.to_rows())
        .map_err(|e| Error::msg(format!("this fit's design matrix could not be built: {e}")))
}

/// Every row's coordinates in the fitted space.
fn transform(model: &Model, matrix: &DenseMatrix<f64>, components: usize) -> Result<Vec<Vec<f64>>> {
    let projected = model
        .transform(matrix)
        .map_err(|e| Error::invalid(format!("this projection could not be applied: {e}")))?;
    let (rows, width) = projected.shape();
    if width != components {
        return Err(Error::msg(format!(
            "this projection answered {width} components and was fitted with {components}"
        )));
    }
    Ok((0..rows)
        .map(|i| (0..width).map(|j| *projected.get((i, j))).collect())
        .collect())
}

/// How much each feature weighs in each component.
fn loadings(model: &Model, x: &Matrix, components: usize) -> Result<ParameterBlock> {
    let projection = model.components();
    let (rows, cols) = projection.shape();
    if rows != x.width() || cols != components {
        return Err(Error::msg(format!(
            "this projection is {rows}×{cols} over {} features and {components} components",
            x.width()
        )));
    }
    let table = x
        .columns()
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut cells = vec![Json::String(name.clone())];
            for j in 0..components {
                cells.push(cell(*projection.get((i, j))));
            }
            ParameterRow { cells }
        })
        .collect();
    let mut columns = vec!["feature".to_owned()];
    columns.extend((1..=components).map(|j| format!("PC{j}")));
    ParameterBlock::table("Loadings", columns, table)
}

/// What fraction of the features' variance each component carries, and what the
/// components carry together.
fn explained(x: &Matrix, projected: &[Vec<f64>], components: usize) -> Result<ParameterBlock> {
    let total: f64 = (0..x.width())
        .map(|j| {
            variance(
                &(0..x.rows())
                    .map(|i| x.values()[i * x.width() + j])
                    .collect::<Vec<_>>(),
            )
        })
        .sum();
    let mut cumulative = 0.0;
    let rows = (0..components)
        .map(|j| {
            let column: Vec<f64> = projected.iter().map(|row| row[j]).collect();
            let v = variance(&column);
            let share = if total > 0.0 { v / total } else { f64::NAN };
            cumulative += share;
            ParameterRow::new(vec![
                Json::String(format!("PC{}", j + 1)),
                cell(v),
                cell(share),
                cell(cumulative),
            ])
        })
        .collect();
    ParameterBlock::table(
        "Explained variance",
        ["component", "variance", "proportion", "cumulative"],
        rows,
    )
}

/// The sample variance of a column.
fn variance(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (n - 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testing::{attrs, close, floats, frame, number, scalar, table};

    /// Eleven points on the line *y* = *x*, nudged a little off it. Almost all
    /// the variance is along one direction, and that direction is (1, 1)/√2 —
    /// which is the whole of what a PCA of this should say.
    fn rotated_line() -> crate::frame::Frame {
        let t: Vec<f64> = (-5..=5).map(f64::from).collect();
        let jitter = [
            0.01, -0.01, 0.02, -0.02, 0.0, 0.01, -0.01, 0.0, 0.02, -0.02, 0.01,
        ];
        frame(vec![
            (
                "x",
                floats(&t.iter().zip(jitter).map(|(t, j)| t + j).collect::<Vec<_>>()),
            ),
            (
                "y",
                floats(&t.iter().zip(jitter).map(|(t, j)| t - j).collect::<Vec<_>>()),
            ),
        ])
    }

    #[tokio::test]
    async fn a_rotated_line_is_one_component() {
        let data = rotated_line();
        let config = attrs(&[("components", 1.into())]);
        let fit = Pca.fit(&data, &config, &Attrs::new()).await.unwrap();

        let (columns, rows) = table(&fit.parameters, "Explained variance");
        assert_eq!(
            columns,
            ["component", "variance", "proportion", "cumulative"]
        );
        assert_eq!(rows.len(), 1);
        assert!(
            number(&rows[0][2]) > 0.9999,
            "one component should carry the line: {:?}",
            rows[0]
        );

        // The component is the (1, 1) direction, up to the sign — an eigenvector
        // and its negative are the same axis, so the assertion is on the
        // magnitudes and on the two being equal.
        let (columns, rows) = table(&fit.parameters, "Loadings");
        assert_eq!(columns, ["feature", "PC1"]);
        let (a, b) = (number(&rows[0][1]), number(&rows[1][1]));
        close(a.abs(), std::f64::consts::FRAC_1_SQRT_2, 1e-3);
        close(a.abs(), b.abs(), 1e-3);
        close(scalar(&fit.parameters, "components"), 1.0, 0.0);

        let predictions = Pca.predict(&fit.state, &data).await.unwrap();
        assert_eq!(predictions.len(), 11);
        let Prediction::Vector { values } = &predictions[0] else {
            panic!("a projection answers a vector, not {:?}", predictions[0]);
        };
        assert_eq!(values.len(), 1);
        // The first row is the far end of the line, so its coordinate is the
        // largest in absolute value of the eleven.
        let projected: Vec<f64> = predictions
            .iter()
            .map(|p| match p {
                Prediction::Vector { values } => values[0],
                other => panic!("{other:?}"),
            })
            .collect();
        let extreme = projected.iter().map(|v| v.abs()).fold(0.0f64, f64::max);
        close(projected[0].abs(), extreme, 1e-9);
    }

    /// Two components over two columns is the identity in disguise: together
    /// they carry everything, which is what makes the cumulative column worth
    /// having.
    #[tokio::test]
    async fn every_component_together_carries_all_of_the_variance() {
        let fit = Pca
            .fit(
                &rotated_line(),
                &attrs(&[("components", 2.into())]),
                &Attrs::new(),
            )
            .await
            .unwrap();
        let (_, rows) = table(&fit.parameters, "Explained variance");
        assert_eq!(rows.len(), 2);
        close(number(&rows[1][3]), 1.0, 1e-9);
        // And the components are ordered: the first carries more than the second.
        assert!(number(&rows[0][1]) > number(&rows[1][1]));
    }

    /// More components than columns has no answer, and the form is where the
    /// admin should hear about it — so `validate` refuses it against the dataset
    /// shape, before any fit.
    #[test]
    fn more_components_than_columns_is_refused_on_the_form() {
        let shape = crate::dataset::DatasetShape {
            table: "t".to_owned(),
            columns: vec![crate::dataset::DatasetColumnShape {
                name: "x".to_owned(),
                ty: crate::frame::ColumnType::Float,
            }],
        };
        let err = Pca
            .validate(&shape, &attrs(&[("components", 3.into())]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("components") && err.contains("1 columns"),
            "{err}"
        );
        assert!(
            Pca.validate(&shape, &attrs(&[("components", 1.into())]))
                .is_ok()
        );
    }

    /// The declared outcome carries the vector's length, and it is read off the
    /// *configuration* — which is why the component count is a setting and not a
    /// hyperparameter (see the module docs).
    #[test]
    fn the_outcome_carries_the_number_of_components() {
        let shape = crate::dataset::DatasetShape {
            table: "t".to_owned(),
            columns: Vec::new(),
        };
        let outcome = Pca
            .outcome(&shape, &attrs(&[("components", 3.into())]))
            .unwrap();
        assert_eq!(
            outcome,
            crate::provider::Outcome::Embedding { dimensions: 3 }
        );
    }
}
