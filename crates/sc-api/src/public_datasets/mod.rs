//! **Public datasets**: well-known open datasets the Analytics UI's "Get
//! datasets" downloads, makes tables of and opens as a dataset.
//!
//! The catalogue ([`catalogue.toml`](catalogue)) is compiled in and holds only
//! metadata: each table's fields and their types, where its file is and how to
//! read it, and the licence and credit that go with it. The data is fetched
//! from its publisher when an admin asks, so Feldspar redistributes nobody's
//! data and its own licence is untouched by theirs; the licence and the
//! attribution travel with the data instead, into the table's and the
//! dataset's descriptions and onto the picker.
//!
//! Declared rather than deduced, unlike a CSV dropped on the admin UI
//! ([`crate::csv::create_table_from_csv`]): the schema is known ahead, so a
//! table gets the types, labels, keys and references a person would have given
//! it — `gapminder.country` is a key to `gapminder_countries`, the Northwind
//! employees point at their managers — rather than whatever the cells look
//! like. The rows still go in through the row layer ([`crate::rows`]), so they
//! are held to the rules every write is.
//!
//! [`install`] is the whole operation: download every file first (a network
//! failure leaves nothing behind), create the tables, write the rows in one
//! transaction, and create the dataset; a failure after the tables exist drops
//! them again. The caller supplies the network ([`Fetch`]) and hears how far it
//! has got ([`Progress`]), which is what lets the server run it as a background
//! job and a test run it on fixture files.

mod install;
mod read;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::OnceLock;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{BasicType, GeometryKind};
use serde::{Deserialize, Serialize};

pub use install::{Fetch, Installed, Progress, ProgressStage, install};

/// The catalogue, as written.
const CATALOGUE: &str = include_str!("catalogue.toml");

/// What a dataset is mostly about, which is how the picker groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Rows and columns: classification and regression examples.
    Tabular,
    /// Rows within groups, or a tree: lookup tables, references, a parent key.
    Hierarchical,
    /// Observations through time.
    TimeSeries,
    /// Geometry: outlines, lines and points.
    Spatial,
}

/// One entry of the catalogue.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicDataset {
    /// Stable identity, used in the API's paths.
    pub key: String,
    /// The name the dataset is created with.
    pub title: String,
    /// How the picker groups it.
    pub category: Category,
    /// What it is, in a sentence or two of our own.
    pub description: String,
    /// Where to read about it.
    pub homepage: String,
    /// The licence the data is under, as its publisher states it.
    pub licence: String,
    /// The licence's text.
    pub licence_url: String,
    /// The credit the publisher asks for.
    pub attribution: String,
    /// The table the Analytics dataset is created on.
    pub dataset_table: String,
    /// The tables, in creation order: a referenced table comes first.
    #[serde(rename = "table")]
    pub tables: Vec<TableDef>,
}

/// One table of an entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDef {
    /// The table's name.
    pub name: String,
    /// About how many rows it has: the picker's estimate, not a check.
    pub rows: u64,
    /// Keep only the first row of each primary key value — how a lookup table
    /// is cut out of a flat file.
    #[serde(default)]
    pub distinct: bool,
    /// The files its rows come from, read in order.
    pub sources: Vec<SourceDef>,
    /// Its fields.
    pub fields: Vec<FieldDef>,
}

/// The kinds of file a source may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Delimited text.
    Csv,
    /// A JSON array of objects.
    Json,
    /// A GeoJSON FeatureCollection in WGS84.
    Geojson,
}

/// One file to read rows from.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDef {
    /// Where it is.
    pub url: String,
    /// What it is.
    pub format: Format,
    /// About how big it is, for the picker.
    pub bytes: u64,
    /// The column separator: `,` (the default), `;`, `\t`, or `whitespace` for
    /// columns lined up with spaces.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// The column names, for a file without a header row.
    #[serde(default)]
    pub header: Option<Vec<String>>,
    /// A line prefix that marks a comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// Cells that mean "no value" in every column; an empty cell always does.
    #[serde(default)]
    pub na: Vec<String>,
    /// Extra columns with one value for every row of this file.
    #[serde(default)]
    pub constants: BTreeMap<String, String>,
}

/// Where a field's value comes from: one column, or several combined.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Columns {
    /// One column (or `$geometry`, `$id`, `$z`).
    One(String),
    /// Several: `[lon, lat]` for a point, `[year, month(, day)]` for a date.
    Many(Vec<String>),
}

/// How a value is read, beyond its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parse {
    /// `1998 Q1` → the quarter's first day.
    YearQuarter,
    /// `1850-01` → the month's first day.
    YearMonth,
    /// `1949.0833` → the month's first day.
    DecimalYearMonth,
    /// Milliseconds since 1970 → a timestamp.
    EpochMs,
    /// A hierarchical code without its last character: `DE14` for `DE149`;
    /// none for a code of two characters or fewer.
    ParentCode,
}

/// One field of a table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDef {
    /// The column name.
    pub name: String,
    /// A basic type name, or `key`.
    #[serde(rename = "type")]
    pub type_name: String,
    /// Where the value comes from; the field's name when not given.
    #[serde(default)]
    pub from: Option<Columns>,
    /// The human label; derived from the name when not given.
    #[serde(default)]
    pub label: Option<String>,
    /// `NOT NULL`.
    #[serde(default)]
    pub required: bool,
    /// The table's primary key.
    #[serde(default)]
    pub primary_key: bool,
    /// For a `key`, the table it points at.
    #[serde(default)]
    pub references: Option<String>,
    /// For a `key`: `null` writes no reference when the row named is not there,
    /// rather than refusing the row.
    #[serde(default)]
    pub if_missing: Option<String>,
    /// Cells that mean "no value" in this column, beside the source's.
    #[serde(default)]
    pub na: Vec<String>,
    /// How the value is read.
    #[serde(default)]
    pub parse: Option<Parse>,
}

impl FieldDef {
    /// The columns the value is read from.
    pub fn sources(&self) -> Vec<&str> {
        match &self.from {
            None => vec![self.name.as_str()],
            Some(Columns::One(one)) => vec![one.as_str()],
            Some(Columns::Many(many)) => many.iter().map(String::as_str).collect(),
        }
    }

    /// Whether this field is a reference to another table (or its own).
    pub fn is_key(&self) -> bool {
        self.type_name == "key"
    }

    /// The basic type of a field that is not a `key`.
    pub fn basic_type(&self) -> Option<BasicType> {
        (!self.is_key()).then(|| BasicType::from_name(&self.type_name))
    }

    /// The kind of geometry, for a geometry field.
    pub fn geometry_kind(&self) -> Option<GeometryKind> {
        GeometryKind::of_type_name(&self.type_name)
    }

    /// Whether a reference to a row that is not there is written as none.
    pub fn null_if_missing(&self) -> bool {
        self.if_missing.as_deref() == Some("null")
    }
}

impl TableDef {
    /// The field that is the primary key, if one is declared.
    pub fn primary_key(&self) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.primary_key)
    }

    /// The field by name.
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Every source's download size, once per file.
    fn unique_urls(&self) -> impl Iterator<Item = (&str, u64)> {
        self.sources.iter().map(|s| (s.url.as_str(), s.bytes))
    }
}

impl PublicDataset {
    /// The table by name.
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// The table names, in creation order.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.name.as_str()).collect()
    }

    /// Whether the dataset needs PostGIS: a spatial dataset's geometry is the
    /// point of it, so it is offered only where geometry can be stored. A point
    /// made from coordinates in any other dataset (the housing blocks, the
    /// flights' airports) is a bonus, and is left out where it cannot be kept.
    pub fn needs_spatial(&self) -> bool {
        self.category == Category::Spatial
    }

    /// How much is downloaded, counting a file read by two tables once.
    pub fn download_bytes(&self) -> u64 {
        let mut seen = HashSet::new();
        self.tables
            .iter()
            .flat_map(TableDef::unique_urls)
            .filter(|(url, _)| seen.insert(*url))
            .map(|(_, bytes)| bytes)
            .sum()
    }

    /// About how many rows it makes, over all its tables.
    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }

    /// What the dataset says about itself: our description, its licence and
    /// the credit, short enough for the front page's list.
    pub fn dataset_description(&self) -> String {
        format!(
            "{} Licence: {}. Credit: {}",
            self.description, self.licence, self.attribution
        )
    }

    /// What its tables say about themselves: our description, then where the
    /// data is from and under what terms, with the links.
    pub fn long_description(&self) -> String {
        format!(
            "{}\n\nSource: {}\nLicence: {} ({})\nCredit: {}",
            self.description, self.homepage, self.licence, self.licence_url, self.attribution
        )
    }
}

/// The catalogue, parsed and checked once.
///
/// It is compiled in, so a mistake in it is a bug in this build: the test
/// [`tests::the_catalogue_is_valid`] runs [`validate`] on it, and a failure here
/// is reported as an internal error rather than a panic.
pub fn catalogue() -> Result<&'static [PublicDataset]> {
    static PARSED: OnceLock<std::result::Result<Vec<PublicDataset>, String>> = OnceLock::new();
    let parsed = PARSED.get_or_init(|| {
        #[derive(Deserialize)]
        struct File {
            dataset: Vec<PublicDataset>,
        }
        let file: File = toml::from_str(CATALOGUE).map_err(|e| e.to_string())?;
        validate(&file.dataset).map_err(|e| e.to_string())?;
        Ok(file.dataset)
    });
    parsed
        .as_deref()
        .map_err(|e| Error::msg(format!("the public dataset catalogue is invalid: {e}")))
}

/// The catalogue entry called `key`.
pub fn find(key: &str) -> Result<&'static PublicDataset> {
    catalogue()?
        .iter()
        .find(|d| d.key == key)
        .ok_or_else(|| Error::not_found(format!("there is no public dataset called `{key}`")))
}

/// Check a catalogue: unique keys and table names, known types, references to
/// tables created earlier (or the table itself) and to a single primary key,
/// combined sources only where they mean something, and the dataset's table
/// among the entry's own.
pub fn validate(entries: &[PublicDataset]) -> Result<()> {
    let mut keys = HashSet::new();
    let mut all_tables = HashSet::new();
    for entry in entries {
        let at = |what: String| Error::invalid(format!("`{}`: {what}", entry.key));
        if !keys.insert(entry.key.as_str()) {
            return Err(at("the key is used twice".into()));
        }
        if entry.tables.is_empty() {
            return Err(at("it has no tables".into()));
        }
        if entry.table(&entry.dataset_table).is_none() {
            return Err(at(format!(
                "`dataset_table` names `{}`, which it does not create",
                entry.dataset_table
            )));
        }
        let mut earlier: BTreeSet<&str> = BTreeSet::new();
        for table in &entry.tables {
            let at = |what: String| at(format!("table `{}`: {what}", table.name));
            crate::schema_edit::check_identifier(&table.name, "table")
                .map_err(|e| at(e.to_string()))?;
            if !all_tables.insert(table.name.as_str()) {
                return Err(at(
                    "the name is used by another table of the catalogue".into()
                ));
            }
            if table.sources.is_empty() {
                return Err(at("it has no sources".into()));
            }
            if table.fields.iter().filter(|f| f.primary_key).count() > 1 {
                return Err(at("more than one field is the primary key".into()));
            }
            if table.distinct && table.primary_key().is_none() {
                return Err(at(
                    "`distinct` needs a primary key to tell rows apart".into()
                ));
            }
            for source in &table.sources {
                if source.format == Format::Csv
                    && !matches!(
                        source.delimiter.as_deref(),
                        None | Some(",") | Some(";") | Some("\t") | Some("whitespace")
                    )
                {
                    return Err(at(format!(
                        "the delimiter of {} is not one this reads",
                        source.url
                    )));
                }
            }
            let mut names = HashSet::new();
            for field in &table.fields {
                let at = |what: String| at(format!("field `{}`: {what}", field.name));
                crate::schema_edit::check_identifier(&field.name, "field")
                    .map_err(|e| at(e.to_string()))?;
                if !names.insert(field.name.as_str())
                    || field.name == "id" && table.primary_key().is_none()
                {
                    return Err(at(
                        "the name is used twice (a table with no key gets an `id`)".into(),
                    ));
                }
                let combined = field.sources().len() > 1;
                if field.is_key() {
                    let target_name = field
                        .references
                        .as_deref()
                        .ok_or_else(|| at("a `key` needs `references`".into()))?;
                    let target = if target_name == table.name {
                        table
                    } else if earlier.contains(target_name) {
                        entry
                            .table(target_name)
                            .ok_or_else(|| at("unreachable".into()))?
                    } else {
                        return Err(at(format!(
                            "it references `{target_name}`, which is not created before it"
                        )));
                    };
                    let target_key = target.primary_key().ok_or_else(|| {
                        at(format!("`{target_name}` has no primary key to reference"))
                    })?;
                    if target_key.is_key() {
                        return Err(at(format!("`{target_name}`'s key is itself a reference")));
                    }
                    if combined {
                        return Err(at("a `key` is read from one column".into()));
                    }
                } else {
                    if field.references.is_some() || field.if_missing.is_some() {
                        return Err(at("`references` and `if_missing` are for a `key`".into()));
                    }
                    let basic = field.basic_type().unwrap_or(BasicType::Text);
                    match &basic {
                        BasicType::Int
                        | BasicType::Float
                        | BasicType::Text
                        | BasicType::Bool
                        | BasicType::Date
                        | BasicType::Timestamp
                        | BasicType::Geometry(
                            GeometryKind::Point
                            | GeometryKind::MultiPolygon
                            | GeometryKind::MultiLineString,
                        ) => {}
                        _ => {
                            return Err(at(format!(
                                "`{}` is not a type this reads",
                                field.type_name
                            )));
                        }
                    }
                    if combined
                        && !matches!(
                            (&basic, field.sources().len()),
                            (BasicType::Geometry(GeometryKind::Point), 2)
                                | (BasicType::Date, 2 | 3)
                        )
                    {
                        return Err(at(
                            "only a point ([lon, lat]) or a date ([year, month(, day)]) is read from several columns"
                                .into(),
                        ));
                    }
                }
                if field.if_missing.as_deref().is_some_and(|m| m != "null") {
                    return Err(at("`if_missing` can only be `null`".into()));
                }
                if field.null_if_missing() && field.required {
                    return Err(at(
                        "a reference written as none when missing cannot be required".into(),
                    ));
                }
            }
            earlier.insert(table.name.as_str());
        }
    }
    Ok(())
}

/// One catalogue entry as the picker shows it, against this database.
#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    /// The entry.
    pub key: &'static str,
    /// What is created.
    pub title: &'static str,
    /// How the picker groups it.
    pub category: Category,
    /// What it is.
    pub description: &'static str,
    /// Where to read about it.
    pub homepage: &'static str,
    /// Its licence.
    pub licence: &'static str,
    /// The licence's text.
    pub licence_url: &'static str,
    /// The credit it asks for.
    pub attribution: &'static str,
    /// The tables it creates.
    pub tables: Vec<&'static str>,
    /// About how many rows, over all of them.
    pub rows: u64,
    /// About how many bytes are downloaded.
    pub download_bytes: u64,
    /// Whether its tables are all here already.
    pub installed: bool,
    /// Why it cannot be installed here, if it cannot: PostGIS is missing, or
    /// a table of another name is in the way.
    pub unavailable: Option<String>,
    /// A dataset on its main table, when there is one.
    pub dataset_id: Option<String>,
}

/// Every catalogue entry, with whether it is here already and whether it can
/// be got.
pub async fn list(catalog: &Catalog) -> Result<Vec<Listing>> {
    let spatial = catalog.primary().spatial();
    let library = sc_dataset::load_library(catalog).await?;
    let mut out = Vec::new();
    for entry in catalogue()? {
        let present: Vec<bool> = entry
            .tables
            .iter()
            .map(|t| catalog.get(&t.name).map(|found| found.is_some()))
            .collect::<Result<_>>()?;
        let installed = present.iter().all(|p| *p);
        let unavailable = if installed {
            None
        } else if present.iter().any(|p| *p) {
            let clash: Vec<String> = entry
                .tables
                .iter()
                .zip(&present)
                .filter(|(_, p)| **p)
                .map(|(t, _)| format!("`{}`", t.name))
                .collect();
            Some(format!(
                "a table it would create already exists: {}",
                clash.join(", ")
            ))
        } else if entry.needs_spatial()
            && let Err(reason) = spatial.require()
        {
            Some(format!("it needs PostGIS to store its geometry: {reason}"))
        } else {
            None
        };
        let dataset_id = library
            .defs()
            .find(|d| {
                matches!(&d.base, sc_dataset::Base::Table { table } if *table == entry.dataset_table)
            })
            .map(|d| d.id.to_string());
        out.push(Listing {
            key: &entry.key,
            title: &entry.title,
            category: entry.category,
            description: &entry.description,
            homepage: &entry.homepage,
            licence: &entry.licence,
            licence_url: &entry.licence_url,
            attribution: &entry.attribution,
            tables: entry.table_names(),
            rows: entry.rows(),
            download_bytes: entry.download_bytes(),
            installed,
            unavailable,
            dataset_id,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_valid() {
        let entries = catalogue().expect("the catalogue parses and validates");
        assert!(
            (10..=50).contains(&entries.len()),
            "between 10 and 50 datasets, found {}",
            entries.len()
        );
        for category in [
            Category::Tabular,
            Category::Hierarchical,
            Category::TimeSeries,
            Category::Spatial,
        ] {
            assert!(
                entries.iter().filter(|e| e.category == category).count() >= 5,
                "at least five {category:?} datasets"
            );
        }
        // Every entry says where the data is from and under what terms.
        for e in entries {
            assert!(
                !e.licence.is_empty() && !e.attribution.is_empty(),
                "{}",
                e.key
            );
            assert!(e.homepage.starts_with("https://"), "{}", e.key);
            for t in &e.tables {
                for s in &t.sources {
                    assert!(s.url.starts_with("https://"), "{}: {}", e.key, s.url);
                    // A GitHub file is pinned to a commit, so it cannot change
                    // under the field list.
                    if s.url.starts_with("https://raw.githubusercontent.com/") {
                        let rev = s.url.split('/').nth(5).unwrap_or_default();
                        assert!(
                            rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()),
                            "{} is not pinned to a commit",
                            s.url
                        );
                    }
                }
            }
        }
        // A file read by two tables is downloaded once.
        let gapminder = find("gapminder").unwrap();
        assert_eq!(gapminder.download_bytes(), 81932);
        assert!(find("no-such-thing").is_err());
    }

    #[test]
    fn a_reference_to_a_table_made_later_is_refused() {
        let toml = r#"
            [[dataset]]
            key = "k"
            title = "T"
            category = "tabular"
            description = "d"
            homepage = "https://example.com"
            licence = "l"
            licence_url = "https://example.com"
            attribution = "a"
            dataset_table = "child"
            [[dataset.table]]
            name = "child"
            rows = 1
            sources = [{ url = "https://example.com/a.csv", format = "csv", bytes = 1 }]
            fields = [{ name = "parent", type = "key", references = "parent" }]
            [[dataset.table]]
            name = "parent"
            rows = 1
            sources = [{ url = "https://example.com/a.csv", format = "csv", bytes = 1 }]
            fields = [{ name = "code", type = "text", primary_key = true }]
        "#;
        #[derive(Deserialize)]
        struct File {
            dataset: Vec<PublicDataset>,
        }
        let file: File = toml::from_str(toml).unwrap();
        let e = validate(&file.dataset).unwrap_err().to_string();
        assert!(e.contains("not created before it"), "{e}");
    }
}
