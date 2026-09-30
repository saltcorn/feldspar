//! What a model is fitted against: a **named dataset** (analytics TODO A1.8).
//!
//! A dataset used to be a list of formulas on the model. It is now a
//! persistent, named definition in `sc-dataset` — a base and an ordered list of
//! operations — shared by the models, panels and datasets that read it. A model
//! stores only its id; [`Dataset`] is that id **resolved**: the definitions it
//! reads (a [`Snapshot`]), the table its rows start from, the columns of its
//! last stage and what a row is. Resolving is what loading a model does, and a
//! dataset that no longer reads — deleted, or with an operation marked invalid —
//! resolves to one carrying the sentence, so the model is still listed with its
//! reason and stays editable.
//!
//! **A fit records what it read.** The snapshot and its hash go on the instance
//! ([`datasets_record`]), so a prediction is computed the way its fit was, and
//! the instance can say when the dataset has changed since ([`dataset_changed`]).
//!
//! The old shape — named columns, one filter, an order — survives as a
//! builder ([`Dataset::new`] and friends), because it is exactly a series of
//! Calculated columns, a Filter, a Sort and a Select columns
//! ([`DatasetDef::from_columns`]); the tests and the migration of older models
//! build it that way.

use std::collections::BTreeMap;

use sc_dataset::{
    Base, ColType, DatasetDef, DatasetId, ForeignKey, Grain, Library, Op, Operation, Options,
    Schema, Snapshot, compile,
};
use sc_error::{Error, Result};
use sc_expr::{Env, Formula, SchemaShape, TranslateError, UserEnv, translate};
use sc_query::Expr;
use serde_json::{Value as Json, json};

use crate::frame::{ColumnType, Frame};

/// One column of a resolved dataset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetColumn {
    /// Its name in the frame, on the form and in the encoding.
    pub name: String,
    /// The formula that computed it, where the dataset was built from formulas
    /// (the builder); its name otherwise.
    pub expr: String,
    /// Its type, as far as the compile knows it.
    pub ty: ColType,
    /// Where it points, when it is a foreign key.
    pub key: Option<ForeignKey>,
}

impl DatasetColumn {
    /// A column named `name` computing `expr`.
    pub fn new(name: impl Into<String>, expr: impl Into<String>) -> DatasetColumn {
        DatasetColumn {
            name: name.into(),
            expr: expr.into(),
            ty: ColType::Unknown,
            key: None,
        }
    }
}

/// One key of a built dataset's order: a formula, and which way it sorts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DatasetOrder {
    /// The formula sorted by.
    pub expr: String,
    /// Largest first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub descending: bool,
}

impl DatasetOrder {
    /// Ascending by `expr`.
    pub fn asc(expr: impl Into<String>) -> DatasetOrder {
        DatasetOrder {
            expr: expr.into(),
            descending: false,
        }
    }

    /// Descending by `expr`.
    pub fn desc(expr: impl Into<String>) -> DatasetOrder {
        DatasetOrder {
            expr: expr.into(),
            descending: true,
        }
    }
}

/// The formulas a [`Dataset::new`] builder has been given, so each builder
/// call can rebuild the definition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Built {
    columns: Vec<(String, String)>,
    filter: Option<String>,
    order: Vec<DatasetOrder>,
}

/// A named dataset as a model reads it. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    /// The named dataset's id — the one thing `_fd_models` stores.
    pub id: DatasetId,
    /// Its name, for the sentences a model's validation writes.
    pub name: String,
    /// The definitions a read compiles: this dataset and every dataset it
    /// reaches. `None` when the dataset does not exist.
    pub snapshot: Option<Snapshot>,
    /// The table its rows start from — its base, or its base's base.
    pub table: String,
    /// The columns of its last stage.
    pub columns: Vec<DatasetColumn>,
    /// What a row of it is, when it reads.
    pub grain: Option<Grain>,
    /// Why it does not read, when it does not.
    pub error: Option<String>,
    built: Option<Built>,
}

impl Dataset {
    /// A dataset known only by its id, until [`resolve`](Dataset::resolve)d.
    pub fn unresolved(id: DatasetId) -> Dataset {
        Dataset {
            id,
            name: id.to_string(),
            snapshot: None,
            table: String::new(),
            columns: Vec::new(),
            grain: None,
            error: Some("the dataset has not been read yet".to_owned()),
            built: None,
        }
    }

    /// The dataset `id` in `library`, compiled against `schema`.
    pub fn resolve(schema: &Schema, library: &Library, id: DatasetId) -> Dataset {
        match library.get(id) {
            Some(def) => Dataset::of_def(schema, library, def),
            None => Dataset {
                error: Some(format!("the dataset {id} no longer exists")),
                ..Dataset::unresolved(id)
            },
        }
    }

    /// `def`, compiled against `schema` with `library` for the datasets it
    /// reaches — whether or not it is saved.
    pub fn of_def(schema: &Schema, library: &Library, def: &DatasetDef) -> Dataset {
        let mut library = library.clone();
        library.insert(def.clone());
        let snapshot = Snapshot::of(&library, def.id);
        let compiled = compile(schema, &library, def, Options::default());
        // A column a Calculated column made reports its formula, as a column
        // of a dataset of formulas always did; any other, its name.
        let formula = |name: &str| {
            def.operations
                .iter()
                .rev()
                .filter(|o| o.enabled)
                .find_map(|o| match &o.op {
                    Op::Calculated(c) if c.name.trim() == name => Some(c.formula.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| name.to_owned())
        };
        let (columns, grain, error) = match compiled.last() {
            Ok(stage) => {
                let shape = stage.shape();
                (
                    shape
                        .columns
                        .into_iter()
                        .map(|c| DatasetColumn {
                            expr: formula(&c.name),
                            name: c.name,
                            ty: c.ty,
                            key: c.key,
                        })
                        .collect(),
                    Some(shape.grain),
                    None,
                )
            }
            Err(e) => (Vec::new(), None, Some(e)),
        };
        Dataset {
            id: def.id,
            name: def.name.clone(),
            table: base_table(&library, def).unwrap_or_default(),
            snapshot,
            columns,
            grain,
            error,
            built: None,
        }
    }

    /// The dataset a fit recorded, as its predictions read it: the snapshot,
    /// and nothing that needs the schema to know.
    pub fn from_snapshot(snapshot: Snapshot) -> Dataset {
        let def = snapshot.def().clone();
        let library = snapshot.library();
        Dataset {
            id: def.id,
            name: def.name.clone(),
            table: base_table(&library, &def).unwrap_or_default(),
            snapshot: Some(snapshot),
            columns: Vec::new(),
            grain: None,
            error: None,
            built: None,
        }
    }

    /// A dataset over `table` with no columns yet — the builder for a dataset
    /// made of formulas (see the module docs). Its id is new, and it is saved
    /// like any dataset before a model that uses it is.
    pub fn new(table: impl Into<String>) -> Dataset {
        let table = table.into();
        let mut ds = Dataset {
            id: DatasetId::new(),
            name: table.clone(),
            snapshot: None,
            table,
            columns: Vec::new(),
            grain: None,
            error: None,
            built: Some(Built::default()),
        };
        ds.rebuild();
        ds
    }

    /// This dataset with `column` appended.
    pub fn column(mut self, name: impl Into<String>, expr: impl Into<String>) -> Dataset {
        if let Some(built) = &mut self.built {
            built.columns.push((name.into(), expr.into()));
        }
        self.rebuild();
        self
    }

    /// This dataset restricted by `filter`.
    pub fn filtered(mut self, filter: impl Into<String>) -> Dataset {
        if let Some(built) = &mut self.built {
            built.filter = Some(filter.into());
        }
        self.rebuild();
        self
    }

    /// This dataset with `order` appended to its sort keys.
    pub fn ordered(mut self, order: DatasetOrder) -> Dataset {
        if let Some(built) = &mut self.built {
            built.order.push(order);
        }
        self.rebuild();
        self
    }

    /// This dataset under `name` — the name it is saved by.
    pub fn named(mut self, name: impl Into<String>) -> Dataset {
        self.name = name.into();
        self.rebuild();
        self
    }

    /// Recompute a built dataset's definition from its formulas. Its rows are
    /// rows of its table, which is what such a dataset always was.
    fn rebuild(&mut self) {
        let Some(built) = &self.built else { return };
        let columns: Vec<(&str, &str)> = built
            .columns
            .iter()
            .map(|(n, e)| (n.as_str(), e.as_str()))
            .collect();
        let order: Vec<(&str, bool)> = built
            .order
            .iter()
            .map(|o| (o.expr.as_str(), o.descending))
            .collect();
        let mut def = DatasetDef::from_columns(
            self.name.clone(),
            self.table.clone(),
            &columns,
            built.filter.as_deref(),
            &order,
        );
        if columns.is_empty() {
            // Nothing to select yet: the table as it is.
            def.operations.clear();
        }
        def.id = self.id;
        self.columns = built
            .columns
            .iter()
            .map(|(n, e)| DatasetColumn::new(n.clone(), e.clone()))
            .collect();
        self.grain = Some(Grain::Table {
            table: self.table.clone(),
            key: String::new(),
        });
        self.snapshot = Some(Snapshot {
            root: def.id,
            datasets: vec![def],
        });
    }

    /// Whether this dataset was built from formulas ([`Dataset::new`]) rather
    /// than resolved from a stored one — what [`save_model`](crate::save_model)
    /// saves beside the model.
    pub fn is_built(&self) -> bool {
        self.built.is_some()
    }

    /// The definition itself, when the dataset exists.
    pub fn def(&self) -> Option<&DatasetDef> {
        self.snapshot.as_ref().map(Snapshot::def)
    }

    /// The snapshot, or the sentence saying why this dataset does not read.
    pub fn readable(&self) -> Result<&Snapshot> {
        if let Some(error) = &self.error {
            return Err(Error::invalid(format!(
                "the dataset `{}` does not read: {error}",
                self.name
            )));
        }
        self.snapshot
            .as_ref()
            .ok_or_else(|| Error::invalid(format!("the dataset `{}` does not exist", self.name)))
    }

    /// The hash of what the dataset means (see [`Snapshot::hash`]).
    pub fn hash(&self) -> Option<String> {
        self.snapshot.as_ref().map(Snapshot::hash)
    }

    /// Whether each row is a row of its table, with that table's primary key
    /// — what a `predict("…")` on the table and a Stan dimension need.
    pub fn keeps_table_grain(&self) -> bool {
        matches!(self.grain, Some(Grain::Table { .. }))
    }

    /// Whether each row has an identity a split can hash: a table's primary
    /// key, or a group's keys.
    pub fn has_row_identity(&self) -> bool {
        matches!(
            self.grain,
            Some(Grain::Table { .. }) | Some(Grain::Group { .. })
        )
    }

    /// Why this dataset's rows cannot be split, when they cannot (§5).
    pub fn split_refusal(&self) -> Option<String> {
        match &self.grain {
            Some(Grain::Table { .. }) | Some(Grain::Group { .. }) | None => None,
            Some(grain) => Some(format!(
                "the rows of `{}` cannot be assigned to a train/validation/test split: {}, so \
                 there is nothing stable to hash",
                self.name,
                grain.describe()
            )),
        }
    }

    /// The same dataset with `label` — a formula over its last stage, or over
    /// the stage before a final Select columns — computed beside its columns as [`LABEL_COLUMN`](crate::LABEL_COLUMN): how
    /// a related dataset's rows are read for binding.
    pub fn with_label(&self, label: &str) -> Dataset {
        let mut ds = self.clone();
        if let Some(snapshot) = &mut ds.snapshot {
            let root = snapshot.root;
            if let Some(def) = snapshot.datasets.iter_mut().find(|d| d.id == root) {
                let op = Operation::new(
                    "_fd_label",
                    Op::calculated(crate::bind::LABEL_COLUMN, label),
                );
                // Before a final Select columns, so a label can name a column
                // the dataset does not keep (`name` for counties whose columns
                // are their measurements), and the selection keeps it.
                let last = def.operations.iter().rposition(|o| o.enabled);
                match last.map(|i| (i, &mut def.operations[i].op)) {
                    Some((i, Op::Select(select))) => {
                        select
                            .columns
                            .push(sc_dataset::SelectColumn::keep(crate::bind::LABEL_COLUMN));
                        def.operations.insert(i, op);
                    }
                    _ => def.operations.push(op),
                }
            }
        }
        ds.columns
            .push(DatasetColumn::new(crate::bind::LABEL_COLUMN, label));
        ds
    }

    /// Check `label` against this dataset's last stage, as a read would compile
    /// it.
    pub fn check_label(&self, schema: &Schema, label: &str) -> Result<()> {
        let labelled = self.with_label(label);
        let snapshot = labelled.readable()?;
        let compiled = compile(
            schema,
            &snapshot.library(),
            snapshot.def(),
            Options::default(),
        );
        match compiled.first_error() {
            Some((_, report)) => Err(Error::invalid(format!(
                "the label `{label}`: {}",
                report.error.as_deref().unwrap_or("it does not compile")
            ))),
            None => Ok(()),
        }
    }
}

/// The table a dataset's rows start from, following datasets based on
/// datasets.
fn base_table(library: &Library, def: &DatasetDef) -> Option<String> {
    let mut at = def;
    for _ in 0..64 {
        match &at.base {
            Base::Table { table } => return Some(table.clone()),
            Base::Dataset { dataset } => at = library.get(*dataset)?,
        }
    }
    None
}

/// The attribute of a fitted instance holding the datasets it read and their
/// hash (analytics TODO A1.8).
pub const ATTR_DATASETS: &str = "datasets";

/// What an instance records about its model's datasets: each one's snapshot,
/// and one hash over all of them.
pub fn datasets_record(main: &Dataset, related: &[crate::NamedDataset]) -> Option<Json> {
    let main_snapshot = main.snapshot.as_ref()?;
    let mut related_json = serde_json::Map::new();
    for r in related {
        if let Some(s) = &r.dataset.snapshot {
            related_json.insert(r.name.clone(), json!({ "snapshot": s, "label": r.label }));
        }
    }
    Some(json!({
        "hash": datasets_hash(main, related)?,
        "main": main_snapshot,
        "related": related_json,
    }))
}

/// One hash over a model's datasets: the main one's and each related one's,
/// with its name and label.
pub fn datasets_hash(main: &Dataset, related: &[crate::NamedDataset]) -> Option<String> {
    use std::fmt::Write as _;
    let mut text = main.hash()?;
    let mut by_name: BTreeMap<&str, (Option<String>, Option<&str>)> = BTreeMap::new();
    for r in related {
        by_name.insert(&r.name, (r.dataset.hash(), r.label.as_deref()));
    }
    for (name, (hash, label)) in by_name {
        let _ = write!(
            text,
            "|{name}={}#{}",
            hash.unwrap_or_default(),
            label.unwrap_or_default()
        );
    }
    if related.is_empty() {
        return Some(text);
    }
    Some(sc_dataset::Snapshot::hash_text(&text))
}

/// Whether the model's datasets mean something different now from what the
/// instance was fitted on — `None` when the instance recorded nothing (it was
/// fitted before datasets had names, or it failed before it read any).
pub fn dataset_changed(
    instance: &crate::ModelInstance,
    main: &Dataset,
    related: &[crate::NamedDataset],
) -> Option<bool> {
    let fitted = instance
        .attributes
        .get(ATTR_DATASETS)?
        .get("hash")?
        .as_str()?;
    Some(datasets_hash(main, related).as_deref() != Some(fitted))
}

/// The main dataset an instance was fitted on, as its snapshot recorded it —
/// what its predictions read, so they are computed the way the fit was.
pub fn fitted_dataset(instance: &crate::ModelInstance) -> Option<Dataset> {
    let snapshot = instance.attributes.get(ATTR_DATASETS)?.get("main")?;
    serde_json::from_value::<Snapshot>(snapshot.clone())
        .ok()
        .map(Dataset::from_snapshot)
}

/// A dataset's columns and their types — what a provider's `config_spec` is
/// handed so a label picker can offer *these* columns rather than a free-text
/// field (§10).
///
/// The types are the **data's**: a sample read answers them exactly, which is
/// why the shape is built from a materialised [`Frame`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DatasetShape {
    /// The dataset's table, for the messages a provider writes.
    pub table: String,
    /// The columns, in the dataset's order.
    pub columns: Vec<DatasetColumnShape>,
}

/// One column of a [`DatasetShape`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DatasetColumnShape {
    /// The column's name.
    pub name: String,
    /// The type its values came back as.
    pub ty: ColumnType,
}

impl DatasetShape {
    /// The shape a materialised frame has.
    pub fn of_frame(table: impl Into<String>, frame: &Frame) -> DatasetShape {
        DatasetShape {
            table: table.into(),
            columns: frame
                .columns
                .iter()
                .map(|(name, column)| DatasetColumnShape {
                    name: name.clone(),
                    ty: column.kind(),
                })
                .collect(),
        }
    }

    /// The type of one column, or `None` when the dataset has no such column.
    pub fn column(&self, name: &str) -> Option<ColumnType> {
        self.columns.iter().find(|c| c.name == name).map(|c| c.ty)
    }

    /// The names of the columns a provider may offer as a numeric label.
    pub fn numeric_columns(&self) -> Vec<&str> {
        self.columns
            .iter()
            .filter(|c| c.ty.is_numeric())
            .map(|c| c.name.as_str())
            .collect()
    }
}

/// The `WHERE` one boolean formula over `table` becomes — what a prediction
/// over "the rows matching this formula" restricts its read by.
///
/// Untranslatable is an **error**: there is no reified evaluator in front of a
/// `SELECT`, so an untranslatable filter would mean reading every row of the
/// table and calling the result a restricted sample.
pub fn translate_filter(table: &str, formula: &str, shape: &SchemaShape) -> Result<Expr> {
    let user = UserEnv::Inline(None);
    let parsed = Formula::parse(formula)?;
    translate(
        &parsed,
        sc_expr::Operation::Read,
        &Env::new(&user),
        shape,
        table,
    )
    .map_err(|e| match e {
        TranslateError::Untranslatable(what) => Error::invalid(format!(
            "a filter must become a `WHERE`, and this one {what}"
        )),
        TranslateError::Error(e) => e,
    })
}

#[cfg(test)]
mod tests {
    use sc_dataset::{StageColumn, TableInfo};
    use sc_expr::TableShape;

    use super::*;

    fn schema() -> Schema {
        let col = |name: &str, ty| StageColumn {
            name: name.into(),
            ty,
            key: None,
        };
        Schema::new(
            SchemaShape::new().table(
                "houses",
                TableShape::new()
                    .field("id")
                    .field("price")
                    .field("area")
                    .primary_key("id"),
            ),
            [TableInfo {
                name: "houses".into(),
                columns: vec![
                    col("id", ColType::Int),
                    col("price", ColType::Float),
                    col("area", ColType::Float),
                ],
                calc: Default::default(),
                primary_key: Some("id".into()),
            }],
        )
    }

    #[test]
    fn a_built_dataset_is_calculated_columns_and_a_selection() {
        let ds = Dataset::new("houses")
            .column("price", "price")
            .column("ppm", "price / area")
            .filtered("price > 1");
        let def = ds.def().expect("a definition");
        assert_eq!(def.base, Base::table("houses"));
        let resolved = Dataset::of_def(&schema(), &Library::default(), def);
        assert_eq!(resolved.error, None);
        let names: Vec<&str> = resolved.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["price", "ppm"]);
        assert_eq!(resolved.columns[1].ty, ColType::Float);
        assert!(resolved.keeps_table_grain());
        assert_eq!(resolved.table, "houses");
    }

    #[test]
    fn a_missing_dataset_resolves_to_its_reason() {
        let ds = Dataset::resolve(&schema(), &Library::default(), DatasetId::new());
        assert!(
            ds.error
                .as_deref()
                .unwrap_or_default()
                .contains("no longer exists")
        );
        assert!(ds.readable().is_err());
    }

    #[test]
    fn a_label_is_a_formula_over_the_last_stage() {
        let ds = Dataset::of_def(
            &schema(),
            &Library::default(),
            Dataset::new("houses")
                .column("price", "price")
                .def()
                .expect("def"),
        );
        ds.check_label(&schema(), "price * 2")
            .expect("a column of the stage");
        // Before the final selection: a column the dataset does not keep can
        // still label its rows.
        ds.check_label(&schema(), "area")
            .expect("a column of the table");
        let err = ds
            .check_label(&schema(), "rooms")
            .expect_err("not a column");
        assert!(err.to_string().contains("`rooms`"), "{err}");
    }

    #[test]
    fn the_hash_notices_an_edit_and_a_fit_says_so() {
        let ds = Dataset::new("houses").column("price", "price");
        let edited = ds.clone().filtered("price > 1");
        assert_ne!(ds.hash(), edited.hash());
        let mut instance = crate::ModelInstance::starting(crate::ModelId::new());
        instance.attributes.insert(
            ATTR_DATASETS.to_owned(),
            datasets_record(&ds, &[]).expect("a record"),
        );
        assert_eq!(dataset_changed(&instance, &ds, &[]), Some(false));
        assert_eq!(dataset_changed(&instance, &edited, &[]), Some(true));
        let fitted = fitted_dataset(&instance).expect("a snapshot");
        assert_eq!(fitted.hash(), ds.hash());
        assert_eq!(fitted.table, "houses");
    }
}
