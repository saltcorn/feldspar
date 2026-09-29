//! A posterior written back into rows (Stan TODO §16; milestone 31 §1).
//!
//! [`write_posterior`] is the one path from a posterior's summary to rows,
//! shared by the admin API's `writePosterior` and a code body's
//! `m.writePosterior(…)`. It writes through the row layer, so a write-back is
//! validated, ownership-checked under the authority it is given, and fires the
//! target table's own triggers whichever way it was asked for.

use sc_catalog::{CallerContext, Catalog, Table};
use sc_error::{Error, Result};
use sc_model::{
    Model, ModelInstance, PosteriorView, PosteriorWrite, WriteMode, instance_coordinates,
    plan_write, summarise_variable,
};
use sc_types::{Attrs, BasicType};
use serde_json::{Value as Json, json};

use crate::rows;

/// Statistics that may go into an integer field, rounded: the effective sample
/// sizes, which are counts of draws.
const COUNT_STATISTICS: [&str; 2] = ["ess_bulk", "ess_tail"];

/// Write `write` of `instance` (a fit of `model`) into rows, as `authority`,
/// through `executor` — **the** write-back, for the admin API and a code
/// body's handle alike.
///
/// Update mode writes the rows of the table the variable's one axis is about
/// (the rows dimension's dataset's table), matched by key; insert mode makes a
/// row per element in `write.table`. Every target field is checked before any
/// row is written: statistics go into a float field (or an integer one, for
/// an effective sample size).
pub async fn write_posterior(
    catalog: &Catalog,
    model: &Model,
    instance: &ModelInstance,
    write: &PosteriorWrite,
    authority: &CallerContext,
    executor: &rows::Executor,
) -> Result<Json> {
    if instance.model != model.id {
        return Err(Error::invalid(format!(
            "fit {} is not a fit of model `{}`",
            instance.id, model.name
        )));
    }
    let view = PosteriorView::of(instance, &write.variable)?;
    let selection = sc_model::Selection::from_json(write.elements.as_ref())?;
    let summary = summarise_variable(catalog, instance, &write.variable, &selection).await?;
    let plan = plan_write(
        &view,
        &summary,
        write,
        &instance_coordinates(instance)?,
        instance.id,
    )?;
    let table = match plan.mode {
        WriteMode::Update => {
            let dataset = plan.dataset.as_deref().unwrap_or_default();
            catalog.require(dataset_table(model, dataset)?)?
        }
        WriteMode::Insert => catalog.require(plan.table.as_deref().unwrap_or_default())?,
    };
    check_targets(&table, write)?;
    let mut written = 0usize;
    for row in &plan.rows {
        let values = dates_as_days(&table, write, row.values.clone())?;
        let body = Json::Object(round_counts(&table, write, values));
        match (&plan.mode, &row.key) {
            (WriteMode::Update, Some(key)) => {
                rows::update_row_in(catalog, &table, key, &body, Some(authority), executor)
                    .await
                    .map_err(|e| {
                        Error::invalid(format!(
                            "writing `{}` into row `{key}` of `{}`: {e}",
                            write.variable, table.name
                        ))
                    })?;
            }
            _ => {
                rows::create_row_in(catalog, &table, &body, Some(authority), executor)
                    .await
                    .map_err(|e| {
                        Error::invalid(format!(
                            "inserting `{}` into `{}`: {e}",
                            write.variable, table.name
                        ))
                    })?;
            }
        }
        written += 1;
    }
    Ok(json!({
        "variable": write.variable,
        "mode": write.mode,
        "table": table.name,
        "instance": instance.id.to_string(),
        "written": written,
    }))
}

/// The table of `model`'s dataset called `name` — `main` or a related one's.
fn dataset_table<'m>(model: &'m Model, name: &str) -> Result<&'m str> {
    if name == sc_model::MAIN_DATASET {
        return Ok(model.table());
    }
    model
        .related
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.dataset.table.as_str())
        .ok_or_else(|| {
            Error::invalid(format!(
                "model `{}` no longer has the dataset `{name}` this fit's dimension is over",
                model.name
            ))
        })
}

/// A field a write-back may write: one the table has, and not a calculated
/// one.
fn writable_field(table: &Table, field: &str) -> Result<()> {
    match table.field(field) {
        None => Err(Error::invalid(format!(
            "`{}` has no field `{field}`",
            table.name
        ))),
        Some(f) if f.is_calc() => Err(Error::invalid(format!(
            "`{field}` is a calculated field and cannot be written"
        ))),
        Some(_) => Ok(()),
    }
}

/// Every target field exists, is writable, and can hold what goes into it.
fn check_targets(table: &Table, write: &PosteriorWrite) -> Result<()> {
    for (field, statistic) in write.fields() {
        writable_field(table, field)?;
        let Some(statistic) = statistic else {
            continue;
        };
        let ty = table
            .field(field)
            .and_then(|f| f.base.type_.as_basic().cloned());
        let fits = match ty {
            None => true,
            Some(BasicType::Float | BasicType::Decimal) => true,
            Some(BasicType::Int) => COUNT_STATISTICS.contains(&statistic),
            Some(_) => false,
        };
        if !fits {
            return Err(Error::invalid(format!(
                "`{field}` of `{}` is {}, and `{statistic}` is a number: write it into a float \
                 field{}",
                table.name,
                ty.map_or_else(|| "not a number".to_owned(), |t| t.name().to_owned()),
                if COUNT_STATISTICS.contains(&statistic) {
                    " (or an integer one)"
                } else {
                    ""
                }
            )));
        }
    }
    Ok(())
}

/// An effective sample size written into an integer field is rounded down.
fn round_counts(table: &Table, write: &PosteriorWrite, mut values: Attrs) -> Attrs {
    for (statistic, field) in &write.statistics {
        let int = table
            .field(field)
            .and_then(|f| f.base.type_.as_basic().cloned())
            == Some(BasicType::Int);
        if int && COUNT_STATISTICS.contains(&statistic.as_str()) {
            if let Some(x) = values.get(field).and_then(Json::as_f64) {
                values.insert(field.clone(), Json::from(x.floor() as i64));
            }
        }
    }
    values
}

/// A time grid's coordinate is an instant (`2025-05-01T00:00:00Z`), which a
/// `date` field refuses. Written into one, it is its day — when it is a
/// midnight, as every step of a grid of days, weeks, months or years is. An
/// hour's instant would lose its hour, so that is refused, naming a timestamp
/// field as the way out.
fn dates_as_days(table: &Table, write: &PosteriorWrite, mut values: Attrs) -> Result<Attrs> {
    for coordinate in &write.coordinates {
        let field = &coordinate.field;
        let date = table
            .field(field)
            .and_then(|f| f.base.type_.as_basic().cloned())
            == Some(BasicType::Date);
        let Some(Json::String(instant)) = values.get(field).filter(|_| date) else {
            continue;
        };
        let Some((day, time)) = instant.split_once('T') else {
            continue;
        };
        if !matches!(time, "00:00:00Z" | "00:00:00.000Z" | "00:00:00+00:00") {
            return Err(Error::invalid(format!(
                "`{field}` of `{}` is a date, and `{instant}` is not a midnight: write this \
                 axis into a timestamp field",
                table.name
            )));
        }
        let day = day.to_owned();
        values.insert(field.clone(), Json::from(day));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    /// `forecasts(day date, at timestamptz)`.
    fn forecasts() -> Table {
        use sc_catalog::{AccessRules, DataField, DbId, TableId, TableSource};
        use sc_types::TypeRef;
        Table {
            id: TableId("forecasts".into()),
            name: "forecasts".into(),
            database: DbId::primary(),
            source: TableSource::Database,
            fields: vec![
                DataField::plain("day", TypeRef::Basic(BasicType::Date)),
                DataField::plain("at", TypeRef::Basic(BasicType::Timestamp)),
            ],
            primary_key: vec!["id".into()],
            label: "forecasts".into(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    #[test]
    fn a_grids_instant_goes_into_a_date_field_as_its_day() {
        let write = |field: &str| {
            PosteriorWrite::from_json(&json!({
                "variable": "y_future",
                "mode": "insert",
                "table": "forecasts",
                "statistics": { "mean": "mean" },
                "coordinates": [{ "axis": "day.future", "field": field }],
            }))
            .unwrap()
        };
        let row =
            |field: &str, instant: &str| config(&[(field, json!(instant)), ("mean", json!(1.5))]);

        let written = dates_as_days(
            &forecasts(),
            &write("day"),
            row("day", "2025-05-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(written["day"], json!("2025-05-01"));
        assert_eq!(written["mean"], json!(1.5));
        // A timestamp field takes the instant as it is.
        let written = dates_as_days(
            &forecasts(),
            &write("at"),
            row("at", "2025-05-01T06:00:00Z"),
        )
        .unwrap();
        assert_eq!(written["at"], json!("2025-05-01T06:00:00Z"));
        // An hour is not a day.
        let err = dates_as_days(
            &forecasts(),
            &write("day"),
            row("day", "2025-05-01T06:00:00Z"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(
                "`day` of `forecasts` is a date, and `2025-05-01T06:00:00Z` is not a \
                          midnight"
            ),
            "{err}"
        );
    }
}
