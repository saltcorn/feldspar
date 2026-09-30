//! The Analytics UI's demo data (analytics TODO A1.18): a small, deterministic
//! set of tables for trying it — what `feldspar demo analytics [--replace]`
//! makes, and what the milestones' definition-of-done tests run over. Here
//! rather than in the CLI so that a server test can make the same rows; each
//! later milestone adds its tables here (paired measurements in A2, districts
//! and incidents in A5).
//!
//! `neighbourhoods`, `houses` and `viewings`, shaped as the models tutorial
//! (`docs/tutorial-models.md`) has them — so its datasets, models and
//! `predict("House prices")` work over the demo as they do over rows typed by
//! hand — plus the columns the Analytics UI's tutorial reaches for: a year
//! built, and a date for each viewing.
//!
//! **Synthetic and deterministic.** Every row comes from a fixed-seed
//! generator, so two runs make the same rows and a tutorial can say what a
//! filter leaves. A price is a linear function of area, bedrooms, the
//! neighbourhood's income and age, plus noise, so a regression over it finds
//! something; about one house in seven is unsold, with no price, for a model
//! to predict.
//!
//! **Nothing is touched without `--replace`.** A database that already has one
//! of the three tables is refused, naming it: they are ordinary tables, and one
//! of them may be the admin's own. With `--replace`, the three are dropped and
//! made again.

use chrono::{Duration, NaiveDate};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, TableId};
use sc_db::ColumnGenerator;
use sc_error::{Context, Error, Result};
use sc_query::{Expr, Insert, Statement, Value};
use sc_types::{BasicType, TypeRef};

/// The tables the demo makes, in the order they are made (a key's target
/// first).
pub const DEMO_TABLES: [&str; 3] = ["neighbourhoods", "houses", "viewings"];

/// How many houses the demo makes.
pub const DEMO_HOUSES: usize = 200;

/// What a run made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoReport {
    /// Each table and how many rows it has.
    pub tables: Vec<(String, usize)>,
    /// The tables that were there and were dropped first.
    pub replaced: Vec<String>,
}

/// A fixed-seed generator: the same sequence on every run and every machine.
struct Rng(u64);

impl Rng {
    /// The next number in `[0, 1)`: a 64-bit LCG (Knuth's MMIX constants),
    /// its top 53 bits.
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// An integer in `lo..=hi`.
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() * (hi - lo + 1) as f64).floor() as i64
    }

    /// A standard normal, by Box–Muller.
    fn normal(&mut self) -> f64 {
        let u = self.next().max(1e-12);
        let v = self.next();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

const NEIGHBOURHOODS: [(&str, f64); 5] = [
    ("Riverside", 62_000.0),
    ("Old Town", 48_000.0),
    ("Northgate", 35_000.0),
    ("Harbour", 55_000.0),
    ("Hillcrest", 71_000.0),
];

const STREETS: [&str; 8] = [
    "Mill Lane",
    "Station Road",
    "Church Street",
    "Elm Grove",
    "Harbour View",
    "Queen's Walk",
    "Orchard Close",
    "Market Street",
];

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

fn id() -> DataField {
    DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
        .generated(ColumnGenerator::Identity)
}

fn key(name: &str, table: &str) -> DataField {
    let mut field = DataField::plain(name, TypeRef::Basic(BasicType::Int));
    field.kind = DataFieldKind::Key {
        target_table: TableId(table.to_owned()),
        target_field: FieldId("id".to_owned()),
        summary_field: None,
    };
    field
}

/// Make the demo's tables in `catalog`, refusing to touch any that are there
/// unless `replace`.
pub async fn demo_analytics(catalog: &Catalog, replace: bool) -> Result<DemoReport> {
    let present: Vec<String> = DEMO_TABLES
        .iter()
        .filter(|t| catalog.get(t).ok().flatten().is_some())
        .map(|t| (*t).to_owned())
        .collect();
    if !present.is_empty() && !replace {
        return Err(Error::invalid(format!(
            "the database already has {}; the demo will not touch a table that is there — \
             run it with `--replace` to drop and remake the demo's tables",
            present
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    // Keys point the other way, so the tables go in reverse.
    for table in DEMO_TABLES.iter().rev() {
        if present.iter().any(|p| p == table) {
            catalog
                .drop_table(table)
                .await
                .with_context(|| format!("dropping `{table}`"))?;
        }
    }

    catalog
        .create_table(
            "neighbourhoods",
            &[
                id(),
                DataField::plain("name", text()).required(),
                DataField::plain("average_income", TypeRef::Basic(BasicType::Float)),
            ],
        )
        .await?;
    catalog
        .create_table(
            "houses",
            &[
                id(),
                DataField::plain("address", text()),
                DataField::plain("area", TypeRef::Basic(BasicType::Float)),
                DataField::plain("bedrooms", TypeRef::Basic(BasicType::Int)),
                key("neighbourhood", "neighbourhoods"),
                DataField::plain("year_built", TypeRef::Basic(BasicType::Int)),
                DataField::plain("sold", TypeRef::Basic(BasicType::Bool)),
                DataField::plain("price", TypeRef::Basic(BasicType::Float)),
            ],
        )
        .await?;
    catalog
        .create_table(
            "viewings",
            &[
                id(),
                key("house", "houses"),
                DataField::plain("viewed_on", TypeRef::Basic(BasicType::Date)),
                DataField::plain("attended", TypeRef::Basic(BasicType::Bool)),
            ],
        )
        .await?;

    let mut rng = Rng(20_260_929);
    let hoods: Vec<Vec<Value>> = NEIGHBOURHOODS
        .iter()
        .map(|(name, income)| vec![Value::Text((*name).to_owned()), Value::Float(*income)])
        .collect();
    insert(
        catalog,
        "neighbourhoods",
        &["name", "average_income"],
        hoods,
    )
    .await?;

    let mut houses = Vec::with_capacity(DEMO_HOUSES);
    let mut viewings = Vec::new();
    let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap_or_default();
    for n in 0..DEMO_HOUSES {
        let hood = rng.int(0, NEIGHBOURHOODS.len() as i64 - 1) as usize;
        let area = (40.0 + rng.next() * 140.0).round();
        let bedrooms = ((area / 35.0).floor() as i64 + rng.int(-1, 1)).clamp(1, 6);
        let year = rng.int(1950, 2022);
        let sold = rng.next() < 0.86;
        let noise = rng.normal() * 18_000.0;
        let price = 1_800.0 * area
            + 12_000.0 * bedrooms as f64
            + 1.2 * NEIGHBOURHOODS[hood].1
            + 700.0 * (year - 1950) as f64
            + noise;
        let price = (price / 1_000.0).round() * 1_000.0;
        houses.push(vec![
            Value::Text(format!(
                "{} {}",
                rng.int(1, 180),
                STREETS[rng.int(0, STREETS.len() as i64 - 1) as usize]
            )),
            Value::Float(area),
            Value::Int(bedrooms),
            Value::Int(hood as i64 + 1),
            Value::Int(year),
            Value::Bool(sold),
            if sold {
                Value::Float(price)
            } else {
                Value::Null
            },
        ]);
        for _ in 0..rng.int(0, 5) {
            viewings.push(vec![
                Value::Int(n as i64 + 1),
                Value::Date(start + Duration::days(rng.int(0, 364))),
                Value::Bool(rng.next() < 0.8),
            ]);
        }
    }
    insert(
        catalog,
        "houses",
        &[
            "address",
            "area",
            "bedrooms",
            "neighbourhood",
            "year_built",
            "sold",
            "price",
        ],
        houses,
    )
    .await?;
    let viewing_count = viewings.len();
    insert(
        catalog,
        "viewings",
        &["house", "viewed_on", "attended"],
        viewings,
    )
    .await?;
    catalog.reload().await?;

    Ok(DemoReport {
        tables: vec![
            ("neighbourhoods".to_owned(), NEIGHBOURHOODS.len()),
            ("houses".to_owned(), DEMO_HOUSES),
            ("viewings".to_owned(), viewing_count),
        ],
        replaced: present,
    })
}

/// Insert `rows` into `table`, a hundred at a time.
async fn insert(
    catalog: &Catalog,
    table: &str,
    columns: &[&str],
    rows: Vec<Vec<Value>>,
) -> Result<()> {
    for chunk in rows.chunks(100) {
        let insert = Insert {
            table: table.to_owned(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            rows: chunk
                .iter()
                .map(|r| r.iter().cloned().map(Expr::Lit).collect())
                .collect(),
            returning: Vec::new(),
        };
        catalog
            .primary()
            .query(&Statement::from(insert))
            .await?
            .try_collect()
            .await
            .with_context(|| format!("writing the demo's rows into `{table}`"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generator_is_the_same_on_every_run() {
        let mut a = Rng(7);
        let mut b = Rng(7);
        let xs: Vec<f64> = (0..5).map(|_| a.next()).collect();
        let ys: Vec<f64> = (0..5).map(|_| b.next()).collect();
        assert_eq!(xs, ys);
        assert!(xs.iter().all(|x| (0.0..1.0).contains(x)));
    }
}
