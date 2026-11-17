//! The rows being predicted (Stan TODO §19, analytics TODO A7.1).
//!
//! A Stan program predicts new cases if it is written to: extra `data`
//! variables for the new rows, and a generated quantity computing the
//! prediction. The bindings name those rows as the pseudo-dataset
//! [`NEW_DATASET`] — the main dataset's columns, read through its formulas.
//! At fit time `new` has no rows (Stan accepts `array[0]`); at predict time
//! it is the caller's rows, and only the variables bound over it are bound
//! again — **against the fit's stored coordinates**, so a county is found at
//! the position the fit gave it, and one the fit never saw is refused by name,
//! as an unknown category is. Everything else in the data is the fit's.
//!
//! What a fit keeps for this is a [`PredictionPack`]: the generated quantity,
//! the interface and the binding keys as they were fitted, and the fit's bound
//! scalars, which the sizes and bounds of the new variables are checked
//! against.

use std::collections::{BTreeMap, HashMap};

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Map, Value as Json};

use super::dimension::{Coordinates, Dimension};
use super::resolve::{Bound, Sizes, Working, about, check_declaration, evaluate};
use super::spec::{Binding, Spec};
use super::{BINDINGS_KEY, DERIVED_COLUMNS_KEY, DIMENSIONS_KEY, POLICIES_KEY};
use crate::frame::Frame;
use crate::interface::{Declaration, Interface, SizeTree};
use crate::model::MAIN_DATASET;

/// What bindings call the rows being predicted.
pub const NEW_DATASET: &str = "new";

/// The configuration keys a prediction binds the new rows with.
const PACK_KEYS: [&str; 4] = [
    BINDINGS_KEY,
    DIMENSIONS_KEY,
    POLICIES_KEY,
    DERIVED_COLUMNS_KEY,
];

/// What a fit of a posterior that predicts keeps, so the posterior can be
/// applied to new rows without the model's configuration as it is now.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PredictionPack {
    /// The generated quantity a prediction reads: one element per new row.
    pub variable: String,
    /// The program's declarations, as fitted.
    pub interface: Interface,
    /// The binding keys of the configuration, as fitted.
    pub config: Attrs,
    /// Every scalar of the fit's bound data, by name: what the new
    /// variables' sizes and bounds are checked against.
    pub scalars: Map<String, Json>,
}

/// Whether `config`'s bindings name the rows being predicted.
pub fn names_new_rows(config: &Attrs) -> bool {
    let Some(Json::Object(bindings)) = config.get(BINDINGS_KEY) else {
        return false;
    };
    bindings.values().any(|b| {
        b.get("dataset").and_then(Json::as_str) == Some(NEW_DATASET)
            || (b.get("kind").and_then(Json::as_str) == Some("size")
                && b.get("dimension").and_then(Json::as_str) == Some(NEW_DATASET))
    })
}

/// Add the empty `new` dataset to the datasets being bound — the main
/// dataset's columns and no rows — when `config` names it and no related
/// dataset is called `new` (one that is keeps its meaning).
pub fn add_new_rows(datasets: &mut Vec<(String, Frame)>, config: &Attrs) -> Result<()> {
    if !names_new_rows(config) || datasets.iter().any(|(n, _)| n == NEW_DATASET) {
        return Ok(());
    }
    let Some((_, main)) = datasets.iter().find(|(n, _)| n == MAIN_DATASET) else {
        return Ok(());
    };
    let empty = main.take_rows(&[])?;
    datasets.push((NEW_DATASET.to_owned(), empty));
    Ok(())
}

/// Whether `binding` is about the rows being predicted.
fn over_new(binding: &Binding) -> bool {
    binding.dataset() == Some(NEW_DATASET)
        || matches!(binding, Binding::Size { dimension } if dimension == NEW_DATASET)
}

/// The save-time limits of a binding over `new` (§19): the rows being
/// predicted are bound row by row against what the fit already has, so a
/// binding over them is a count, a column, columns, or an index into a
/// dimension the fit's coordinates record — not a model matrix (whose
/// encoding would be fitted to the rows being predicted), not a series, and
/// nothing indexes into them.
pub(crate) fn check_new_rows(spec: &Spec) -> Result<()> {
    for (name, binding) in &spec.bindings {
        if over_new(binding) {
            match binding {
                Binding::Count { .. }
                | Binding::Size { .. }
                | Binding::Column { .. }
                | Binding::Columns { .. } => {}
                Binding::Index {
                    dimension,
                    match_column,
                    ..
                } => {
                    if match_column.is_some() {
                        return Err(Error::invalid(format!(
                            "`{name}` indexes the rows being predicted (`{NEW_DATASET}`) into \
                             `{dimension}` by a `match` column, which a prediction cannot do: \
                             it finds each row by the key the fit's coordinates record; index by \
                             the foreign key instead"
                        )));
                    }
                    let grid = spec
                        .dimensions
                        .get(dimension.strip_suffix(".future").unwrap_or(dimension))
                        .is_some_and(|d| matches!(d, super::DimensionSpec::TimeGrid { .. }));
                    if grid {
                        return Err(Error::invalid(format!(
                            "`{name}` indexes the rows being predicted (`{NEW_DATASET}`) into the \
                             time grid `{dimension}`, which a prediction cannot do: the fit's \
                             coordinates record a grid's steps, not where a new date falls"
                        )));
                    }
                }
                other => {
                    return Err(Error::invalid(format!(
                        "`{name}` is bound by `{}` over the rows being predicted \
                         (`{NEW_DATASET}`); over them a binding is a `count`, a `column`, \
                         `columns`, or an `index` into a dimension the fit has",
                        other.kind()
                    )));
                }
            }
        } else if super::resolve::lookups(spec, binding)
            .iter()
            .any(|l| l.dimension() == NEW_DATASET)
        {
            return Err(Error::invalid(format!(
                "`{name}` looks rows up in `{NEW_DATASET}`, the rows being predicted, which have \
                 no positions until a prediction asks"
            )));
        }
    }
    for (name, dimension) in &spec.dimensions {
        if dimension.dataset() == Some(NEW_DATASET) {
            return Err(Error::invalid(format!(
                "the dimension `{name}` is over `{NEW_DATASET}`, the rows being predicted, which \
                 a fit has none of"
            )));
        }
    }
    Ok(())
}

/// The check of a posterior's `prediction` against the program (§19): it
/// names a generated quantity with one axis, sized by a `data` variable bound
/// to the count of `new` (or the size of its rows).
pub fn check_prediction(interface: &Interface, config: &Attrs, variable: &str) -> Result<()> {
    let Some(decl) = interface.generated.iter().find(|d| d.name == variable) else {
        let names: Vec<String> = interface
            .generated
            .iter()
            .map(|d| format!("`{}`", d.name))
            .collect();
        return Err(Error::invalid(format!(
            "the prediction `{variable}` is not a generated quantity of the program ({})",
            if names.is_empty() {
                "it declares none".to_owned()
            } else {
                format!("it declares {}", names.join(", "))
            }
        )));
    };
    let one_axis = match decl.dims.as_slice() {
        [size] => match &size.tree {
            Some(SizeTree::Var(var)) => Some(var.clone()),
            _ => None,
        },
        _ => None,
    };
    let Some(size) = one_axis else {
        return Err(Error::invalid(format!(
            "the prediction `{variable}` is declared `{}`; it must have one axis, sized by a \
             data variable bound to the count of `{NEW_DATASET}` (`vector[N_new] {variable}`)",
            decl.stan_type
        )));
    };
    let spec = Spec::parse(config)?;
    let sized_by_new = matches!(
        spec.bindings.get(&size),
        Some(Binding::Count { dataset }) if dataset == NEW_DATASET
    ) || matches!(
        spec.bindings.get(&size),
        Some(Binding::Size { dimension }) if dimension == NEW_DATASET
    );
    if !sized_by_new {
        return Err(Error::invalid(format!(
            "the prediction `{variable}` is sized by `{size}`, which must be bound to the count \
             of `{NEW_DATASET}`, the rows being predicted (`{{\"kind\": \"count\", \"dataset\": \
             \"{NEW_DATASET}\"}}`)"
        )));
    }
    Ok(())
}

/// What a fit of a posterior that predicts `variable` keeps (see the module
/// docs), from its interface, its (lowered) configuration and its bound data.
pub fn prediction_pack(
    variable: &str,
    interface: &Interface,
    config: &Attrs,
    data: &Json,
) -> PredictionPack {
    let config = PACK_KEYS
        .iter()
        .filter_map(|k| Some(((*k).to_owned(), config.get(*k)?.clone())))
        .collect();
    let scalars = data
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.is_number())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    PredictionPack {
        variable: variable.to_owned(),
        interface: interface.clone(),
        config,
        scalars,
    }
}

/// Bind the variables over `new` to `frame`'s rows — the rows being predicted,
/// read through the main dataset — against the fit's `coordinates`,
/// answering the data members that change.
///
/// A null where a binding reads a value, and a value with no position in the
/// dimension the fit indexed it into, are each refused naming the row and the
/// column: a prediction answers one value per row, so no row can be dropped.
#[allow(clippy::too_many_lines)]
pub fn bind_new_rows(
    pack: &PredictionPack,
    coordinates: &Coordinates,
    frame: &Frame,
) -> Result<Map<String, Json>> {
    let spec = Spec::parse(&pack.config)?;
    let mut frame = frame.clone();
    super::derived::add_derived(MAIN_DATASET, &mut frame, &spec.derived)?;
    super::derived::add_derived(NEW_DATASET, &mut frame, &spec.derived)?;
    let rows = frame.rows;
    let mut work: BTreeMap<&str, Working> = BTreeMap::new();
    work.insert(
        NEW_DATASET,
        Working {
            frame,
            labels: None,
            read: rows,
        },
    );
    let w = &work[NEW_DATASET];

    let bindings: Vec<(&Declaration, &Binding)> = pack
        .interface
        .data
        .iter()
        .filter_map(|d| {
            spec.bindings
                .get(&d.name)
                .filter(|b| over_new(b))
                .map(|b| (d, b))
        })
        .collect();
    if bindings.is_empty() {
        return Err(Error::invalid(format!(
            "this fit binds nothing to `{NEW_DATASET}`, the rows being predicted, so it cannot \
             predict them"
        )));
    }

    // Every value a binding reads is there.
    for (decl, binding) in &bindings {
        for column in binding.policed_columns() {
            let Some(values) = w.frame.column(column) else {
                return Err(Error::invalid(format!(
                    "{}: the rows being predicted have no column `{column}`",
                    about(decl, binding)
                )));
            };
            if let Some(i) = (0..rows).find(|i| values.is_null_at(*i)) {
                return Err(Error::invalid(format!(
                    "{}: `{column}` is null in row `{}` of the rows being predicted, and a \
                     prediction needs it",
                    about(decl, binding),
                    w.row_name(i)
                )));
            }
        }
    }

    // The fit's dimensions, as its coordinates record them, and the new rows.
    let mut dims: HashMap<String, Dimension> = coordinates
        .dimensions
        .iter()
        .filter_map(|c| Some((c.name.clone(), Dimension::from_coordinates(c)?)))
        .collect();
    dims.insert(
        NEW_DATASET.to_owned(),
        Dimension::rows(NEW_DATASET, &w.frame.keys, None, rows),
    );

    // Every value indexed has a position the fit gave it.
    for (decl, binding) in &bindings {
        let Binding::Index {
            column, dimension, ..
        } = binding
        else {
            continue;
        };
        let Some(dim) = dims.get(dimension.as_str()) else {
            return Err(Error::invalid(format!(
                "{}: the fit's coordinates have no dimension `{dimension}`; fit the model again",
                about(decl, binding)
            )));
        };
        let values = w
            .frame
            .column(column)
            .ok_or_else(|| Error::invalid(format!("no column `{column}`")))?;
        for i in 0..rows {
            if dim.position(values, i).is_none() {
                let value = super::dimension::cell_text(values, i).unwrap_or_default();
                return Err(Error::invalid(format!(
                    "`{column}` is `{value}` in row `{}` of the rows being predicted, which is \
                     not one of the {} {} the fit saw in `{dimension}`, so the model has \
                     nothing to say about it",
                    w.row_name(i),
                    dim.size(),
                    if dim.size() == 1 { "value" } else { "values" },
                )));
            }
        }
    }

    let mut bound: HashMap<&str, Bound> = HashMap::new();
    let mut designs = BTreeMap::new();
    let mut warnings = Vec::new();
    for (decl, binding) in &bindings {
        let value = evaluate(
            decl,
            binding,
            &spec,
            &work,
            &dims,
            &bound,
            &mut designs,
            &mut warnings,
        )?;
        bound.insert(decl.name.as_str(), value);
    }

    // The sizes and bounds the new variables are checked against: the
    // fit's scalars, with the new ones in their place.
    let mut sizes: Sizes<'_> = pack
        .scalars
        .iter()
        .filter_map(|(k, v)| Some((k.as_str(), (v.as_i64()?, None))))
        .collect();
    let mut scalars: HashMap<&str, f64> = pack
        .scalars
        .iter()
        .filter_map(|(k, v)| Some((k.as_str(), v.as_f64()?)))
        .collect();
    for (name, b) in &bound {
        if let Some(n) = b.tensor.as_int() {
            sizes.insert(name, (n, b.what.clone()));
        }
        if let Some(x) = b.tensor.as_number() {
            scalars.insert(name, x);
        }
    }
    let mut out = Map::new();
    for (decl, binding) in &bindings {
        let b = bound
            .remove(decl.name.as_str())
            .ok_or_else(|| Error::msg("binding the new rows: a variable was not evaluated"))?;
        let tensor = check_declaration(decl, binding, b, &sizes, &scalars, &work)?;
        out.insert(decl.name.clone(), tensor.to_json());
    }
    Ok(out)
}
