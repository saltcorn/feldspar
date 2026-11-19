//! Derived columns (TODO-bayesian-regression R2.1, R2.3): columns a provider
//! asks the binder to add to a dataset before anything is bound, under
//! [`DERIVED_COLUMNS_KEY`](super::DERIVED_COLUMNS_KEY).
//!
//! Two kinds, both small and both computed from the dataset's own columns —
//! never a second way to reach across a key:
//!
//! - **`combine`**: the combination of several columns, one text value per row
//!   (`north_A`, the parts joined by `_`), null where any part is null. What a
//!   grouping factor `g1:g2` indexes into: a `values` dimension over it has one
//!   position per combination that occurs.
//! - **`mask`**: a column's values where another (yes/no) column is true, and
//!   null elsewhere. What a response measured on some rows only (`subset()`)
//!   is bound through: the `present_*` kinds read it, and they leave nulls to
//!   themselves rather than to the dataset's `nulls` policy.
//!
//! A derived column is an ordinary column to every binding: it is named, it is
//! policed like any other when a policed binding reads it, and the save-time
//! check knows it exists.

use std::collections::BTreeMap;

use sc_error::{Error, Result};

use super::dimension::cell_text;
use crate::frame::{Column, Frame};

/// One derived column.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DerivedColumn {
    /// The combination of `columns`, as text joined by `_`.
    Combine {
        /// The dataset it is added to.
        dataset: String,
        /// The columns combined, in order.
        columns: Vec<String>,
    },
    /// `column` where `where` is true, null elsewhere.
    Mask {
        /// The dataset it is added to.
        dataset: String,
        /// The column whose values are kept.
        column: String,
        /// The yes/no column saying where (a number counts as true when it is
        /// not 0).
        #[serde(rename = "where")]
        where_column: String,
    },
}

impl DerivedColumn {
    /// The dataset it is added to.
    pub fn dataset(&self) -> &str {
        match self {
            DerivedColumn::Combine { dataset, .. } | DerivedColumn::Mask { dataset, .. } => dataset,
        }
    }

    /// The columns it reads.
    pub fn reads(&self) -> Vec<&str> {
        match self {
            DerivedColumn::Combine { columns, .. } => columns.iter().map(String::as_str).collect(),
            DerivedColumn::Mask {
                column,
                where_column,
                ..
            } => vec![column, where_column],
        }
    }

    /// Its values over `frame`, which holds the columns it reads.
    pub fn evaluate(&self, name: &str, frame: &Frame) -> Result<Column> {
        let get = |c: &str| {
            frame.column(c).ok_or_else(|| {
                Error::invalid(format!(
                    "the derived column `{name}` reads `{c}`, which is not a column of `{}`",
                    self.dataset()
                ))
            })
        };
        match self {
            DerivedColumn::Combine { columns, .. } => {
                if columns.is_empty() {
                    return Err(Error::invalid(format!(
                        "the derived column `{name}` combines no columns"
                    )));
                }
                let parts = columns.iter().map(|c| get(c)).collect::<Result<Vec<_>>>()?;
                Ok(Column::Str(
                    (0..frame.rows)
                        .map(|i| {
                            parts
                                .iter()
                                .map(|p| cell_text(p, i))
                                .collect::<Option<Vec<String>>>()
                                .map(|v| v.join("_"))
                        })
                        .collect(),
                ))
            }
            DerivedColumn::Mask {
                column,
                where_column,
                ..
            } => {
                let values = get(column)?;
                let flags = get(where_column)?;
                let keep: Vec<bool> = (0..frame.rows).map(|i| truthy(flags, i)).collect();
                Ok(mask(values, &keep))
            }
        }
    }
}

/// Whether row `i` of a yes/no (or number) column is true.
pub(crate) fn truthy(column: &Column, i: usize) -> bool {
    match column {
        Column::Bool(v) => v.get(i).copied().flatten().unwrap_or(false),
        Column::Int(v) => v.get(i).copied().flatten().is_some_and(|x| x != 0),
        Column::Float(v) => v.get(i).copied().flatten().is_some_and(|x| x != 0.0),
        Column::Str(v) => v
            .get(i)
            .cloned()
            .flatten()
            .is_some_and(|s| matches!(s.trim(), "true" | "TRUE" | "yes" | "1")),
        Column::Date(_) | Column::Null(_) => false,
    }
}

fn mask(values: &Column, keep: &[bool]) -> Column {
    fn m<T: Clone>(v: &[Option<T>], keep: &[bool]) -> Vec<Option<T>> {
        v.iter()
            .zip(keep)
            .map(|(x, k)| if *k { x.clone() } else { None })
            .collect()
    }
    match values {
        Column::Float(v) => Column::Float(m(v, keep)),
        Column::Int(v) => Column::Int(m(v, keep)),
        Column::Bool(v) => Column::Bool(m(v, keep)),
        Column::Str(v) => Column::Str(m(v, keep)),
        Column::Date(v) => Column::Date(m(v, keep)),
        Column::Null(n) => Column::Null(*n),
    }
}

/// `frame` with the derived columns of `dataset` added, in name order. A name
/// that is already a column is refused: a derived column never hides one.
pub(crate) fn add_derived(
    dataset: &str,
    frame: &mut Frame,
    derived: &BTreeMap<String, DerivedColumn>,
) -> Result<()> {
    for (name, d) in derived.iter().filter(|(_, d)| d.dataset() == dataset) {
        if frame.column(name).is_some() {
            return Err(Error::invalid(format!(
                "the derived column `{name}` would hide the column of that name in `{dataset}`"
            )));
        }
        let column = d.evaluate(name, frame)?;
        frame.columns.push((name.clone(), column));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        Frame::new(
            vec![
                (
                    "g".to_owned(),
                    Column::Str(vec![Some("a".into()), Some("b".into()), None]),
                ),
                ("h".to_owned(), Column::Int(vec![Some(1), Some(2), Some(1)])),
                (
                    "s".to_owned(),
                    Column::Bool(vec![Some(true), Some(false), Some(true)]),
                ),
            ],
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn a_combination_joins_the_parts_and_is_null_where_any_is() {
        let d = DerivedColumn::Combine {
            dataset: "main".into(),
            columns: vec!["g".into(), "h".into()],
        };
        assert_eq!(
            d.evaluate("gh", &frame()).unwrap(),
            Column::Str(vec![Some("a_1".into()), Some("b_2".into()), None])
        );
    }

    #[test]
    fn a_mask_keeps_the_values_where_the_flag_is_true() {
        let d = DerivedColumn::Mask {
            dataset: "main".into(),
            column: "h".into(),
            where_column: "s".into(),
        };
        assert_eq!(
            d.evaluate("hs", &frame()).unwrap(),
            Column::Int(vec![Some(1), None, Some(1)])
        );
        let parsed: DerivedColumn = serde_json::from_value(serde_json::json!({
            "kind": "mask", "dataset": "main", "column": "h", "where": "s"
        }))
        .unwrap();
        assert_eq!(parsed, d);
    }

    #[test]
    fn a_derived_column_never_hides_a_real_one() {
        let mut f = frame();
        let derived = BTreeMap::from([(
            "g".to_owned(),
            DerivedColumn::Combine {
                dataset: "main".into(),
                columns: vec!["h".into()],
            },
        )]);
        assert!(add_derived("main", &mut f, &derived).is_err());
    }
}
