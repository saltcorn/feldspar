//! What a model is fitted against: a table, a list of formulas, and one
//! optional filter (TODO §2, §3).
//!
//! GOALS asks a dataset for "table fields and derived fields such as
//! calculations, joinfields and aggregations, and any inclusion/exclusion
//! criteria on the rows". This system already has one language that is exactly
//! those four things — the calc-field/ownership expression language, with `Ⱶ`
//! for a join path and `Ↄ` for an aggregation over an incoming key, already
//! translated to SQL by [`translate_value`] and already proven at parity with
//! the reified evaluator. So a dataset column **is** a formula:
//!
//! ```text
//! price
//! price / area
//! neighbourhoodⱵaverage_income
//! viewings.filter(v => v.attended).length
//! ```
//!
//! There is no second vocabulary of "field / joinfield / aggregation" with three
//! shapes in the JSON and three code paths behind it. The admin UI still offers
//! a picker — click a field, click a join path, click an aggregation — but what
//! the picker *writes* is a formula, and a user who wants `log(price)` types it.
//!
//! **`user` and the operation flags are refused**, for the reason a calculated
//! field refuses them: a dataset has no caller, and a fit that meant something
//! different depending on who pressed the button would be indefensible.
//!
//! **A dataset has no store and no name.** It is a JSON column on the model
//! (§3): a shared, named dataset would need a lifecycle — what happens to the
//! four models fitted against it when somebody adds a column, whether an
//! instance fitted against version 1 is still readable — bought for a saving
//! (retyping a column list) that a *Duplicate model* button answers instead.

use std::collections::BTreeSet;

use sc_error::{Error, Result};
use sc_expr::{
    Ambient, Env, Formula, SchemaShape, TranslateError, UserEnv, translate, translate_value,
};
use sc_query::{Expr, Nulls, OrderBy, Projection, Select, Source};

use crate::frame::{ColumnType, Frame};

/// One column of a dataset: the name it will be known by, and the formula that
/// computes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DatasetColumn {
    /// What the column is called in the frame, on the form and in the encoding.
    pub name: String,
    /// The `sc-expr` formula computing it, over the dataset's table.
    pub expr: String,
}

impl DatasetColumn {
    /// A column named `name` computing `expr`.
    pub fn new(name: impl Into<String>, expr: impl Into<String>) -> DatasetColumn {
        DatasetColumn {
            name: name.into(),
            expr: expr.into(),
        }
    }
}

/// One key of a dataset's order: a formula, and which way it sorts (Stan
/// TODO §7).
///
/// A formula for the reason a column is one — `taken_at`, `countyⱵname` and
/// `-price` are all things somebody sorts by, and a second vocabulary for "a
/// field, or a join, or an expression" would be the one this crate refuses for
/// columns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DatasetOrder {
    /// The `sc-expr` formula sorted by, over the dataset's table.
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

/// Which rows and which derived values make up a model's data.
///
/// Stored as the `dataset` JSON column of `_fd_models`, so this is the wire
/// shape as well as the in-memory one.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Dataset {
    /// The table the formulas are written over.
    pub table: String,
    /// The columns, in the order they appear on the form and in the frame.
    pub columns: Vec<DatasetColumn>,
    /// One boolean formula restricting the rows, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The order the rows come back in, **always followed by the primary key**
    /// (see [`order_by`](Dataset::order_by)).
    ///
    /// Nothing that splits by hash cares; a posterior does, twice over. A time
    /// series *is* an order, and MCMC with the same seed over the same rows in a
    /// different order gives different draws — so a total, deterministic order is
    /// what makes "same data, same seed, same draws" true.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<DatasetOrder>,
}

/// A dataset's columns and their types — what a provider's `config_spec` is
/// handed so a label picker can offer *these* columns rather than a free-text
/// field (§10).
///
/// The types are the **data's**, not the schema's, and that is not a shortcut: a
/// [`SchemaShape`] carries no types at all (it carries field names and where the
/// keys point), and the type of `price / area`, of a join path or of an
/// aggregation is not derivable from one. A sample read answers all four exactly,
/// which is why the shape is built from a materialised [`Frame`].
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

impl Dataset {
    /// A dataset over `table` with no columns and no filter.
    pub fn new(table: impl Into<String>) -> Dataset {
        Dataset {
            table: table.into(),
            columns: Vec::new(),
            filter: None,
            order: Vec::new(),
        }
    }

    /// This dataset with `column` appended (builder sugar, mostly for tests).
    pub fn column(mut self, name: impl Into<String>, expr: impl Into<String>) -> Dataset {
        self.columns.push(DatasetColumn::new(name, expr));
        self
    }

    /// This dataset restricted by `filter`.
    pub fn filtered(mut self, filter: impl Into<String>) -> Dataset {
        self.filter = Some(filter.into());
        self
    }

    /// This dataset with `order` appended to its sort keys.
    pub fn ordered(mut self, order: DatasetOrder) -> Dataset {
        self.order.push(order);
        self
    }

    /// The projections this dataset's columns become: one
    /// `Projection::expr_as(…, name)` per column, from [`translate_value`].
    ///
    /// A column whose formula **does not translate** is not an error here. Those
    /// are the ones the row layer falls back to the reified evaluator for, which
    /// is the arrangement calculated fields already have; a caller that needs
    /// every column back checks what actually arrived (see
    /// [`missing`](Dataset::missing)) rather than pre-judging it here.
    pub fn projections(&self, shape: &SchemaShape) -> Result<Vec<Projection>> {
        let user = UserEnv::Inline(None);
        let env = Env::new(&user);
        let mut out = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            let formula = Formula::parse(&column.expr).map_err(|e| self.at(&column.name, &e))?;
            match translate_value(&formula, &env, shape, &self.table) {
                Ok(expr) => out.push(Projection::expr_as(expr, column.name.clone())),
                Err(TranslateError::Untranslatable(_)) => {}
                Err(TranslateError::Error(e)) => return Err(self.at(&column.name, &e)),
            }
        }
        Ok(out)
    }

    /// The `WHERE` this dataset's filter becomes, or `None` when it has none.
    ///
    /// Unlike a column, a filter that does not translate **is** an error: there
    /// is no reified fallback in front of a `SELECT`, so an untranslatable
    /// filter would mean reading every row of the table and calling the result a
    /// restricted sample.
    pub fn filter_expr(&self, shape: &SchemaShape) -> Result<Option<Expr>> {
        let Some(source) = &self.filter else {
            return Ok(None);
        };
        translate_filter(&self.table, source, shape)
            .map(Some)
            .map_err(|e| self.on_filter(&e))
    }

    /// The `ORDER BY` this dataset's order becomes: each declared key, then the
    /// table's primary key.
    ///
    /// **The primary key always comes last**, so the order is total: two rows
    /// that tie on every declared key still come back the same way round on
    /// every read, on every backend. A table with no single primary key gets
    /// the declared keys alone — it reads, and only its ties are unordered.
    ///
    /// Nulls go **last** in both directions, stated rather than left to the
    /// backend: Postgres and SQLite disagree about where an ascending sort puts
    /// them, and an order that changed with the database would not be one.
    ///
    /// Like a filter and unlike a column, a key that does not translate is an
    /// error: there is no reified evaluator in front of an `ORDER BY`.
    pub fn order_by(&self, shape: &SchemaShape) -> Result<Vec<OrderBy>> {
        let user = UserEnv::Inline(None);
        let env = Env::new(&user);
        let mut out = Vec::with_capacity(self.order.len() + 1);
        for key in &self.order {
            let formula = Formula::parse(&key.expr).map_err(|e| self.on_order(&key.expr, &e))?;
            let expr =
                translate_value(&formula, &env, shape, &self.table).map_err(|e| match e {
                    TranslateError::Untranslatable(what) => self.on_order(
                        &key.expr,
                        &Error::invalid(format!(
                            "an order must become an `ORDER BY`, and this one {what}"
                        )),
                    ),
                    TranslateError::Error(e) => self.on_order(&key.expr, &e),
                })?;
            out.push(OrderBy {
                nulls: Some(Nulls::Last),
                ..if key.descending {
                    OrderBy::desc(expr)
                } else {
                    OrderBy::asc(expr)
                }
            });
        }
        if let Some(pk) = shape
            .tables
            .get(&self.table)
            .and_then(|t| t.primary_key.clone())
        {
            out.push(OrderBy::asc(Expr::qcol(self.table.clone(), pk)));
        }
        Ok(out)
    }

    /// The `Select` this dataset is — its columns projected, its filter folded
    /// into the `WHERE`, its order into the `ORDER BY`, over its own table.
    ///
    /// The row layer builds the statement it actually runs (that is what applies
    /// calculated fields, ownership and row-level security); this is the same
    /// question asked standalone, and it is what the unit tests assert against.
    pub fn select(&self, shape: &SchemaShape) -> Result<Select> {
        let mut select =
            Select::from(Source::table(self.table.clone())).columns(self.projections(shape)?);
        select.filter = self.filter_expr(shape)?;
        select.order = self.order_by(shape)?;
        Ok(select)
    }

    /// The dataset columns that are **not** among `arrived` — the columns whose
    /// formulas did not translate and that nothing else filled in.
    ///
    /// Named separately from [`projections`](Dataset::projections) because the
    /// two verdicts are different: not translating is ordinary, and coming back
    /// missing is a frame that does not answer the question the model asked.
    pub fn missing<'a>(&'a self, arrived: &BTreeSet<&str>) -> Vec<&'a str> {
        self.columns
            .iter()
            .map(|c| c.name.as_str())
            .filter(|name| !arrived.contains(name))
            .collect()
    }

    /// The single primary-key column this dataset's rows are split by (§5), or
    /// the refusal saying why there is not one.
    ///
    /// **The fit's restriction, not the dataset's.** A table with a composite or
    /// absent primary key reads perfectly well; what it cannot do is divide its
    /// rows into train, validation and test reproducibly, because there is
    /// nothing stable to hash. An unsupervised fit with no split is unaffected.
    pub fn primary_key(&self, shape: &SchemaShape) -> Result<String> {
        let table = shape
            .tables
            .get(&self.table)
            .ok_or_else(|| Error::invalid(format!("dataset: unknown table `{}`", self.table)))?;
        table.primary_key.clone().ok_or_else(|| {
            Error::invalid(format!(
                "`{}` has no single primary key, so its rows cannot be assigned to a \
                 train/validation/test split: there is nothing stable to hash",
                self.table
            ))
        })
    }

    /// Prefix an error with the column it is about.
    fn at(&self, column: &str, e: &Error) -> Error {
        Error::invalid(format!(
            "dataset column `{column}` on `{}`: {e}",
            self.table
        ))
    }

    /// Prefix an error with the fact that it is the filter's.
    fn on_filter(&self, e: &Error) -> Error {
        Error::invalid(format!("dataset filter on `{}`: {e}", self.table))
    }

    /// Prefix an error with the order key it is about.
    fn on_order(&self, expr: &str, e: &Error) -> Error {
        Error::invalid(format!("dataset order `{expr}` on `{}`: {e}", self.table))
    }
}

/// The `WHERE` one boolean formula over `table` becomes.
///
/// [`Dataset::filter_expr`] is this applied to the dataset's own filter, and it
/// is public for the other caller Phase 5 added: a prediction over "the rows
/// matching this formula" restricts the *same* read the same way, and a second
/// translation path would be a second set of rules about what a filter may say.
///
/// Untranslatable is an **error** rather than a fallback, for the reason a
/// dataset's own filter is: there is no reified evaluator in front of a
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

/// Validate `dataset` against `shape`: every column's formula parses and
/// resolves, no column names `user` or an operation flag, no name is empty or
/// repeated, and the filter is a predicate.
///
/// Run on save **and** on load (the rule every stored, validated entity in this
/// system follows): a model whose dataset stopped validating because a column
/// was dropped from the table is listed with its reason and stays editable,
/// rather than disappearing or being silently repaired.
pub fn validate_dataset(dataset: &Dataset, shape: &SchemaShape) -> Result<()> {
    if !shape.tables.contains_key(&dataset.table) {
        return Err(Error::invalid(format!(
            "dataset: unknown table `{}`",
            dataset.table
        )));
    }
    if dataset.columns.is_empty() {
        return Err(Error::invalid(format!(
            "dataset on `{}`: it has no columns",
            dataset.table
        )));
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for column in &dataset.columns {
        if column.name.trim().is_empty() {
            return Err(Error::invalid(format!(
                "dataset on `{}`: a column has no name",
                dataset.table
            )));
        }
        if !seen.insert(column.name.as_str()) {
            return Err(Error::invalid(format!(
                "dataset on `{}`: two columns are named `{}`",
                dataset.table, column.name
            )));
        }
        check_formula(
            dataset,
            &column.expr,
            shape,
            &format!("column `{}`", column.name),
        )?;
    }
    if let Some(filter) = &dataset.filter {
        check_formula(dataset, filter, shape, "filter")?;
        // A filter must become a `WHERE`, which is the only sense in which a
        // formula here is "in boolean position": the translator refuses a
        // non-predicate, and an untranslatable one is refused by
        // `filter_expr`.
        dataset.filter_expr(shape)?;
    }
    for key in &dataset.order {
        check_formula(dataset, &key.expr, shape, &format!("order `{}`", key.expr))?;
    }
    // Translated as a whole, because an order key that does not become SQL has
    // nowhere to go (see `order_by`).
    dataset.order_by(shape)?;
    Ok(())
}

/// Validate `label` — the formula naming each row of a related dataset on the
/// screen (Stan TODO §7) — against `dataset`'s table.
///
/// Checked like a column, and then held to a filter's standard: it must become
/// SQL, because a label is read beside the row and there is no reified fallback
/// on that path either.
pub(crate) fn validate_label(dataset: &Dataset, label: &str, shape: &SchemaShape) -> Result<()> {
    check_formula(dataset, label, shape, "label")?;
    let user = UserEnv::Inline(None);
    let formula = Formula::parse(label)?;
    match translate_value(&formula, &Env::new(&user), shape, &dataset.table) {
        Ok(_) => Ok(()),
        Err(TranslateError::Untranslatable(what)) => Err(Error::invalid(format!(
            "dataset label on `{}`: a label is read with the row and must become SQL, and \
             this one {what}",
            dataset.table
        ))),
        Err(TranslateError::Error(e)) => Err(Error::invalid(format!(
            "dataset label on `{}`: {e}",
            dataset.table
        ))),
    }
}

/// One formula of a dataset, parsed and resolved like a calculated field's —
/// and with the same two things refused.
fn check_formula(dataset: &Dataset, source: &str, shape: &SchemaShape, what: &str) -> Result<()> {
    let where_ = |e: &Error| Error::invalid(format!("dataset {what} on `{}`: {e}", dataset.table));
    let formula = Formula::parse(source).map_err(|e| where_(&e))?;
    let analysis = formula
        .validate(shape, &dataset.table)
        .map_err(|e| where_(&e))?;
    if analysis.uses(Ambient::User) || !analysis.flags.is_empty() {
        return Err(where_(&Error::invalid(
            "a dataset cannot use `user` or the operation flags — it has no caller, \
             and a fit that meant something different depending on who pressed the \
             button would be indefensible",
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sc_db_postgres::PgDialect;
    use sc_expr::TableShape;

    use super::*;

    /// A `houses` table with a `neighbourhood` key, and the tables the join
    /// paths and aggregations reach.
    fn shape() -> SchemaShape {
        SchemaShape::new()
            .table(
                "houses",
                TableShape::new()
                    .field("price")
                    .field("bedrooms")
                    .field("area")
                    .field("sold")
                    .key_field("neighbourhood", "neighbourhoods", "id")
                    .field("id")
                    .primary_key("id"),
            )
            .table(
                "neighbourhoods",
                TableShape::new()
                    .field("id")
                    .field("average_income")
                    .primary_key("id"),
            )
    }

    fn sql(select: &Select) -> String {
        use sc_query::SqlDialect;
        PgDialect.render(&select.clone().into()).expect("render").0
    }

    #[test]
    fn each_column_becomes_the_projection_it_should() {
        let ds = Dataset::new("houses")
            .column("price", "price")
            .column("per_room", "price / bedrooms")
            .column("income", "neighbourhoodⱵaverage_income");
        let rendered = sql(&ds.select(&shape()).expect("select"));
        assert!(
            rendered.contains(r#""houses"."price" AS "price""#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"("houses"."price" / "houses"."bedrooms") AS "per_room""#),
            "{rendered}"
        );
        // A join path becomes a correlated subselect aliased to the column name.
        assert!(rendered.contains(r#"AS "income""#), "{rendered}");
        assert!(rendered.contains(r#""neighbourhoods""#), "{rendered}");
    }

    #[test]
    fn the_filter_folds_into_the_where() {
        let ds = Dataset::new("houses")
            .column("price", "price")
            .filtered("sold === true");
        let rendered = sql(&ds.select(&shape()).expect("select"));
        let (_, where_) = rendered.split_once("WHERE").expect("a WHERE clause");
        assert!(where_.contains(r#""houses"."sold""#), "{rendered}");
    }

    #[test]
    fn a_bare_identifier_filter_says_what_to_write_instead() {
        // The expression language refuses a bare value as a condition — it
        // cannot know the column is boolean — and the refusal carries the fix
        // rather than leaving the admin to guess it.
        let ds = Dataset::new("houses")
            .column("price", "price")
            .filtered("sold");
        let err = validate_dataset(&ds, &shape()).expect_err("bare identifier");
        assert!(err.to_string().contains("`x === true`"), "{err}");
    }

    #[test]
    fn user_is_refused_by_name_in_a_column_and_in_the_filter() {
        let shape = shape();
        let ds = Dataset::new("houses").column("mine", "user.id");
        let err = validate_dataset(&ds, &shape).expect_err("user refused");
        assert!(err.to_string().contains("cannot use `user`"), "{err}");
        assert!(err.to_string().contains("column `mine`"), "{err}");

        let ds = Dataset::new("houses")
            .column("price", "price")
            .filtered("user.id === 1");
        let err = validate_dataset(&ds, &shape).expect_err("user refused");
        assert!(err.to_string().contains("dataset filter"), "{err}");
    }

    #[test]
    fn duplicate_and_empty_column_names_are_refused() {
        let shape = shape();
        let ds = Dataset::new("houses")
            .column("price", "price")
            .column("price", "price / 2");
        let err = validate_dataset(&ds, &shape).expect_err("duplicate");
        assert!(err.to_string().contains("two columns are named"), "{err}");

        let ds = Dataset::new("houses").column("  ", "price");
        let err = validate_dataset(&ds, &shape).expect_err("empty");
        assert!(err.to_string().contains("a column has no name"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_refused_naming_the_column() {
        let ds = Dataset::new("houses").column("x", "no_such_field");
        let err = validate_dataset(&ds, &shape()).expect_err("unknown field");
        assert!(err.to_string().contains("column `x`"), "{err}");
    }

    #[test]
    fn a_dataset_with_no_columns_is_refused() {
        let err = validate_dataset(&Dataset::new("houses"), &shape()).expect_err("no columns");
        assert!(err.to_string().contains("no columns"), "{err}");
    }

    #[test]
    fn a_table_with_no_single_primary_key_refuses_a_split_but_not_a_read() {
        let shape = SchemaShape::new().table(
            "readings",
            TableShape::new().field("sensor").field("taken_at"),
        );
        let ds = Dataset::new("readings").column("sensor", "sensor");
        // The read is fine …
        validate_dataset(&ds, &shape).expect("a dataset over a keyless table is valid");
        assert!(ds.select(&shape).is_ok());
        // … the split is not.
        let err = ds.primary_key(&shape).expect_err("no primary key");
        assert!(err.to_string().contains("nothing stable to hash"), "{err}");
    }

    #[test]
    fn the_shape_of_a_frame_names_the_numeric_columns() {
        use crate::frame::Column;
        let frame = Frame::new(
            vec![
                ("price".to_owned(), Column::Float(vec![Some(1.0)])),
                ("region".to_owned(), Column::Str(vec![Some("n".into())])),
            ],
            Vec::new(),
        )
        .expect("frame");
        let shape = DatasetShape::of_frame("houses", &frame);
        assert_eq!(shape.column("price"), Some(ColumnType::Float));
        assert_eq!(shape.numeric_columns(), vec!["price"]);
    }

    #[test]
    fn a_column_that_does_not_translate_is_skipped_rather_than_refused() {
        // `typeof` has no SQL counterpart, and the arrangement calculated
        // fields already have is that such a column is left to the reified
        // evaluator rather than making the dataset invalid.
        let ds = Dataset::new("houses")
            .column("price", "price")
            .column("kind", "typeof price");
        let shape = shape();
        validate_dataset(&ds, &shape).expect("valid");
        let projections = ds.projections(&shape).expect("projections");
        assert_eq!(projections.len(), 1);
        // But a frame that came back without it does not answer the question
        // the model asked, and says so.
        let arrived = BTreeSet::from(["price"]);
        assert_eq!(ds.missing(&arrived), vec!["kind"]);
    }

    #[test]
    fn the_order_becomes_the_order_by_with_the_primary_key_last() {
        let ds = Dataset::new("houses")
            .column("price", "price")
            .ordered(DatasetOrder::desc("price / bedrooms"))
            .ordered(DatasetOrder::asc("neighbourhoodⱵaverage_income"));
        validate_dataset(&ds, &shape()).expect("valid");
        let rendered = sql(&ds.select(&shape()).expect("select"));
        let (_, order) = rendered.split_once("ORDER BY").expect("an ORDER BY");
        let first = order
            .find(r#"("houses"."price" / "houses"."bedrooms") DESC NULLS LAST"#)
            .unwrap_or_else(|| panic!("the declared key first: {rendered}"));
        let second = order
            .find("ASC NULLS LAST")
            .unwrap_or_else(|| panic!("the join-path key second: {rendered}"));
        let key = order
            .rfind(r#""houses"."id""#)
            .unwrap_or_else(|| panic!("the primary key last: {rendered}"));
        assert!(first < second && second < key, "{rendered}");
    }

    #[test]
    fn a_dataset_with_no_order_is_still_ordered_by_its_key_and_a_keyless_one_is_not() {
        let ds = Dataset::new("houses").column("price", "price");
        let order = ds.order_by(&shape()).expect("order");
        assert_eq!(order, vec![OrderBy::asc(Expr::qcol("houses", "id"))]);

        let keyless = SchemaShape::new().table("readings", TableShape::new().field("taken_at"));
        let ds = Dataset::new("readings")
            .column("t", "taken_at")
            .ordered(DatasetOrder::asc("taken_at"));
        assert_eq!(ds.order_by(&keyless).expect("order").len(), 1);
    }

    #[test]
    fn an_order_key_is_validated_like_a_column_and_must_translate() {
        let ds = Dataset::new("houses")
            .column("price", "price")
            .ordered(DatasetOrder::asc("no_such_field"));
        let err = validate_dataset(&ds, &shape()).expect_err("unknown field");
        assert!(err.to_string().contains("order `no_such_field`"), "{err}");

        let ds = Dataset::new("houses")
            .column("price", "price")
            .ordered(DatasetOrder::asc("typeof price"));
        let err = validate_dataset(&ds, &shape()).expect_err("untranslatable order");
        assert!(
            err.to_string().contains("must become an `ORDER BY`"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_order_is_not_written_and_an_absent_one_reads_as_empty() {
        let ds = Dataset::new("houses").column("price", "price");
        let json = serde_json::to_value(&ds).expect("json");
        assert!(json.get("order").is_none(), "{json}");
        let back: Dataset = serde_json::from_value(json).expect("back");
        assert!(back.order.is_empty());
        let ordered = ds.ordered(DatasetOrder::desc("price"));
        let json = serde_json::to_value(&ordered).expect("json");
        assert_eq!(
            json["order"],
            serde_json::json!([{ "expr": "price", "descending": true }])
        );
    }

    #[test]
    fn an_untranslatable_filter_is_refused_because_there_is_no_where_to_put_it() {
        let ds = Dataset::new("houses")
            .column("price", "price")
            .filtered("typeof price");
        let err = validate_dataset(&ds, &shape()).expect_err("untranslatable filter");
        assert!(err.to_string().contains("must become a `WHERE`"), "{err}");
    }
}
