//! Vector fields on the row API (TODO-agent-skills AS2.3–AS2.5).
//!
//! Three things a vector field does that a plain field does not:
//!
//! - **A read can be ordered by it** ([`Nearest`]): the rows nearest a given
//!   vector by cosine distance, nearest first. The vector is given, or is text
//!   embedded with the field's own model — the model its rows were embedded
//!   with, since a distance between vectors of two models means nothing. It is
//!   resolved inside the caller's read ([`prepare`]), so ownership, RLS and the
//!   row cap apply to it as to any order. A read no index answers — every one
//!   on SQLite, and one on Postgres without a vector index — reads every row,
//!   and is refused past [`VECTOR_SCAN_LIMIT`] rows.
//! - **It can be computed** (`embed_from`): a formula over the row whose text
//!   is embedded on every insert, and on every update that changes a field the
//!   formula reads ([`embed_insert`], [`embed_update`]). A write whose
//!   embedding cannot be computed fails, naming the field, as a default value
//!   that cannot be computed does — a row silently missing its embedding is a
//!   row no nearest read will ever find.
//! - **Existing rows can be back-filled** ([`embed_rows`]): the rows whose
//!   vector is still empty, a batch at a time, which is what adding an
//!   `embed_from` field to a table that has rows needs.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::{Catalog, DataField, Table, VECTOR_SCAN_LIMIT, vector_index};
use sc_error::{Error, Result};
use sc_query::{
    Assignment, Expr, OrderBy, Projection, Select, Source, Statement, Update, Value, VectorDistance,
};
use sc_types::BasicType;
use sc_types::vector::{ATTR_EMBED_FROM, ATTR_EMBED_MODEL, ATTR_EMBED_PROVIDER};
use serde_json::{Map, Value as Json};

use crate::rows::RowQuery;
use crate::user_rows::visible_field;

/// The rows a nearest read returns when the caller gives no bound.
pub const DEFAULT_NEAREST: u64 = 10;

/// What a read is ordered nearest to.
#[derive(Debug, Clone, PartialEq)]
pub enum NearestTo {
    /// A vector of the field's dimensions.
    Vector(Vec<f32>),
    /// Text, embedded with the field's model first.
    Text(String),
}

/// A nearest read: the rows whose `field` is nearest `to`, nearest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Nearest {
    /// The vector field.
    pub field: String,
    /// What the rows are nearest to.
    pub to: NearestTo,
}

impl Nearest {
    /// `{ "field": "embedding", "to": [0.1, …] }` or `{ …, "to": "some text" }`
    /// — the shape the rows API, GraphQL and an agent's tool send.
    pub fn from_json(json: &Json) -> Result<Nearest> {
        let field = json
            .get("field")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| {
                Error::invalid("`nearest` needs a `field`: the vector field to order by")
            })?;
        let to = match json.get("to") {
            Some(Json::String(text)) => NearestTo::Text(text.clone()),
            Some(array @ Json::Array(_)) => NearestTo::Vector(sc_types::vector::floats(array)?),
            _ => {
                return Err(Error::invalid(
                    "`nearest` needs `to`: a vector (an array of numbers) or text to embed",
                ));
            }
        };
        Ok(Nearest {
            field: field.to_owned(),
            to,
        })
    }

    /// A query string's `nearest=<field>.<to>`, where `<to>` is a vector
    /// written as a JSON array (`embedding.[0.1,0.2]`) or else text to embed
    /// (`embedding.cheap flights to Lisbon`).
    pub fn parse(spec: &str) -> Result<Nearest> {
        let Some((field, to)) = spec.split_once('.') else {
            return Err(Error::invalid(format!(
                "`nearest={spec}` is not `<field>.<vector or text>`, such as \
                 `nearest=embedding.[0.1,0.2,0.3]` or `nearest=embedding.cheap flights`"
            )));
        };
        let to = to.trim();
        let to = if to.starts_with('[') {
            let json: Json = serde_json::from_str(to).map_err(|_| {
                Error::invalid(format!("`{to}` is not a vector: an array of numbers"))
            })?;
            NearestTo::Vector(sc_types::vector::floats(&json)?)
        } else {
            NearestTo::Text(to.to_owned())
        };
        Ok(Nearest {
            field: field.trim().to_owned(),
            to,
        })
    }
}

/// The vector field `name` of `table` the caller may read, and its dimensions.
pub fn vector_field<'t>(table: &'t Table, name: &str) -> Result<(&'t DataField, u32)> {
    let Some(field) = visible_field(table, name) else {
        return Err(Error::invalid(format!(
            "`{}` has no field `{name}`",
            table.name
        )));
    };
    match field.base.type_.as_basic() {
        Some(BasicType::Vector(n)) if *n > 0 && !field.is_calc() => Ok((field, *n)),
        _ => {
            let vectors: Vec<String> = table
                .fields
                .iter()
                .filter(|f| matches!(f.base.type_.as_basic(), Some(BasicType::Vector(_))))
                .map(|f| format!("`{}`", f.base.name))
                .collect();
            Err(Error::invalid(format!(
                "`{}.{name}` is not a vector field, so rows cannot be ordered nearest by it; \
                 the vector fields are {}",
                table.name,
                if vectors.is_empty() {
                    "none".to_owned()
                } else {
                    vectors.join(", ")
                }
            )))
        }
    }
}

/// The read `query` with its nearest ordering resolved, embedded and checked:
/// the distance first in its order, a bound of [`DEFAULT_NEAREST`] when it has
/// none. The query itself when it has no nearest.
pub(crate) async fn prepare<'q>(
    cat: &Catalog,
    table: &Table,
    query: &'q RowQuery,
) -> Result<Cow<'q, RowQuery>> {
    let Some(nearest) = &query.nearest else {
        return Ok(Cow::Borrowed(query));
    };
    let (field, dimensions) = vector_field(table, &nearest.field)?;
    let to = match &nearest.to {
        NearestTo::Vector(v) => v.clone(),
        NearestTo::Text(text) => embed_texts(cat, table, field, std::slice::from_ref(text))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::msg("the embedding provider returned no vector"))?,
    };
    sc_types::vector::check_length(dimensions, to.len())
        .map_err(|e| Error::invalid(format!("`nearest` on `{}`: {e}", field.base.name)))?;
    require_answerable(cat, table, &field.base.name).await?;
    let distance = Expr::VectorDistance(Box::new(VectorDistance {
        column: Expr::qcol(table.name.clone(), field.base.name.clone()),
        to,
    }));
    // A row with no vector is near nothing, so it is not one of the nearest;
    // a zero vector has no direction, and its distance (NaN on Postgres, NULL
    // on SQLite) sorts after every real one.
    let column = Expr::qcol(table.name.clone(), field.base.name.clone());
    let mut prepared = query
        .clone()
        .and_filter(Expr::unary(sc_query::UnOp::IsNotNull, column));
    prepared.nearest = None;
    prepared.order.insert(
        0,
        OrderBy {
            nulls: Some(sc_query::Nulls::Last),
            ..OrderBy::asc(distance)
        },
    );
    if prepared.limit.is_none() {
        prepared.limit = Some(DEFAULT_NEAREST);
    }
    Ok(Cow::Owned(prepared))
}

/// Refuse a nearest read that no index answers over a table too big to scan.
async fn require_answerable(cat: &Catalog, table: &Table, field: &str) -> Result<()> {
    let sqlite = cat.driver_named(&table.database)?.dialect().is_sqlite();
    if !sqlite && vector_index(table, field).is_some() {
        return Ok(());
    }
    let sample = Select::from(Source::table(table.name.clone()))
        .columns(vec![Projection::expr(Expr::lit(1_i64))])
        .limit(VECTOR_SCAN_LIMIT + 1);
    let count = Select::from(Source::Subquery {
        query: Box::new(sample),
        alias: "_fd_sample".to_owned(),
    })
    .columns(vec![Projection::expr_as(
        Expr::Agg {
            func: "count".to_owned(),
            distinct: false,
            args: Vec::new(),
        },
        "n",
    )]);
    let rows = cat
        .provider(table)?
        .query(&count)
        .await?
        .try_collect()
        .await?;
    let n = match rows.first().and_then(|r| r.get("n")) {
        Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    if n <= VECTOR_SCAN_LIMIT {
        return Ok(());
    }
    let what = if sqlite {
        format!(
            "on SQLite a nearest read over `{}` computes the distance of every row, and it \
             has more than {VECTOR_SCAN_LIMIT} rows. Move the table to PostgreSQL with \
             pgvector and add a vector index, or narrow the read with a filter",
            table.name
        )
    } else {
        format!(
            "`{}.{field}` has no vector index, and the table has more than \
             {VECTOR_SCAN_LIMIT} rows, so a nearest read would compute the distance of every \
             one. Add a vector index on `{field}` (the table's Constraints tab in the admin \
             UI, or the schema operation `add_constraint` with type `vector_index`)",
            table.name
        )
    };
    Err(Error::invalid(what))
}

/// How a vector field is embedded: its provider (`None` for the default), its
/// model, and the formula its text comes from, if it is computed.
struct EmbedConfig {
    provider: Option<String>,
    model: Option<String>,
    from: Option<String>,
}

fn embed_config(field: &DataField) -> EmbedConfig {
    let text = |key: &str| {
        field
            .base
            .attributes
            .get(key)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    EmbedConfig {
        provider: text(ATTR_EMBED_PROVIDER),
        model: text(ATTR_EMBED_MODEL),
        from: text(ATTR_EMBED_FROM),
    }
}

/// Embed `texts` with the model of vector field `field`, each checked against
/// its dimensions.
pub async fn embed_texts(
    cat: &Catalog,
    table: &Table,
    field: &DataField,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let name = &field.base.name;
    let dimensions = match field.base.type_.as_basic() {
        Some(BasicType::Vector(n)) => *n,
        _ => 0,
    };
    let config = embed_config(field);
    let Some(model) = config.model else {
        return Err(Error::invalid(format!(
            "`{}.{name}` has no embedding model (its `{ATTR_EMBED_MODEL}` attribute), so text \
             cannot be turned into one of its vectors; give a vector instead, or set the model",
            table.name
        )));
    };
    let (provider, model_def) = sc_llm::resolve_model(
        cat,
        config.provider.as_deref(),
        Some(&model),
        sc_llm::ModelKind::Embedding,
    )
    .await?;
    let vectors = sc_llm::embed(&provider, &model_def, texts).await?.vectors;
    for vector in &vectors {
        sc_types::vector::check_length(dimensions, vector.len()).map_err(|_| {
            Error::invalid(format!(
                "`{model}` makes vectors of {} dimensions, and `{}.{name}` holds vectors of \
                 {dimensions}: the field's dimensions are the model's output size",
                vector.len(),
                table.name
            ))
        })?;
    }
    Ok(vectors)
}

/// The fields of `table` computed by an `embed_from` formula.
fn embedded_fields(table: &Table) -> impl Iterator<Item = (&DataField, String)> {
    table.fields.iter().filter_map(|f| {
        let from = embed_config(f).from?;
        matches!(f.base.type_.as_basic(), Some(BasicType::Vector(n)) if *n > 0).then_some((f, from))
    })
}

/// The text `source` gives for `row`, or `None` when it gives nothing.
async fn formula_text(
    cat: &Catalog,
    table: &Table,
    source: &str,
    row: &Map<String, Json>,
    op: sc_expr::Operation,
) -> Result<Option<String>> {
    let evaluator = cat
        .formula_evaluator()
        .ok_or_else(|| Error::invalid("no formula evaluator is running"))?;
    let shape = cat.schema_shape()?;
    let formula = sc_expr::Formula::parse(source)?;
    let analysis = formula.validate(&shape, &table.name)?;
    let mut bound = BTreeMap::new();
    for (key, json) in row {
        if table
            .field(key)
            .is_some_and(|f| !f.is_calc() && !f.is_collab_doc())
        {
            bound.insert(key.clone(), crate::rows::column_value(table, key, json)?);
        }
    }
    sc_catalog::prefetch_bindings(cat, table, &analysis, &shape, &mut bound).await?;
    let json = evaluator
        .eval_value(sc_expr::FormulaCall {
            formula,
            op,
            row: bound,
            user: None,
            ambient: Default::default(),
        })
        .await?;
    Ok(match json {
        Json::Null => None,
        Json::String(s) if s.trim().is_empty() => None,
        Json::String(s) => Some(s),
        other => Some(other.to_string()),
    })
}

/// The fields a formula reads, for deciding whether an update changes its
/// text.
fn formula_reads(cat: &Catalog, table: &Table, source: &str) -> Result<BTreeSet<String>> {
    let shape = cat.schema_shape()?;
    let analysis = sc_expr::Formula::parse(source)?.validate(&shape, &table.name)?;
    let mut fields = analysis.fields;
    // A Ⱶ-path starts at a key of this table.
    for path in &analysis.join_paths {
        if let Some(first) = path.segments.first() {
            fields.insert(first.clone());
        }
    }
    Ok(fields)
}

fn embed_failed(table: &Table, field: &DataField, e: &dyn std::fmt::Display) -> Error {
    Error::invalid(format!(
        "the embedding of `{}.{}` could not be computed: {e}",
        table.name, field.base.name
    ))
}

/// The insert `body` with every `embed_from` field it leaves out computed.
pub(crate) async fn embed_insert<'b>(
    cat: &Catalog,
    table: &Table,
    body: &'b Json,
) -> Result<Cow<'b, Json>> {
    let Some(obj) = body.as_object() else {
        return Ok(Cow::Borrowed(body));
    };
    if embedded_fields(table).next().is_none() {
        return Ok(Cow::Borrowed(body));
    }
    let mut out = obj.clone();
    for (field, source) in embedded_fields(table) {
        if obj.get(&field.base.name).is_some_and(|v| !v.is_null()) {
            continue;
        }
        let text = formula_text(cat, table, &source, obj, sc_expr::Operation::Insert)
            .await
            .map_err(|e| embed_failed(table, field, &e))?;
        let value = match text {
            Some(text) => embed_texts(cat, table, field, &[text])
                .await
                .map_err(|e| embed_failed(table, field, &e))?
                .into_iter()
                .next()
                .map_or(Json::Null, |v| sc_types::vector::to_json(&v)),
            None => Json::Null,
        };
        out.insert(field.base.name.clone(), value);
    }
    Ok(Cow::Owned(Json::Object(out)))
}

/// The update `obj` of the row `stored` with every `embed_from` field whose
/// formula reads a field it changes computed again. `stored` is read only when
/// one does.
pub(crate) async fn embed_update<'o, F, Fut>(
    cat: &Catalog,
    table: &Table,
    obj: &'o Map<String, Json>,
    stored: F,
) -> Result<Cow<'o, Map<String, Json>>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<Json>>>,
{
    let mut stale = Vec::new();
    for (field, source) in embedded_fields(table) {
        if obj.contains_key(&field.base.name) {
            continue;
        }
        let reads =
            formula_reads(cat, table, &source).map_err(|e| embed_failed(table, field, &e))?;
        if reads.iter().any(|f| obj.contains_key(f)) {
            stale.push((field, source));
        }
    }
    if stale.is_empty() {
        return Ok(Cow::Borrowed(obj));
    }
    // The row as it will be: what is stored, with this update over it. A row
    // that is not there is the update's own not-found to report.
    let Some(Json::Object(mut row)) = stored().await? else {
        return Ok(Cow::Borrowed(obj));
    };
    for (key, value) in obj {
        row.insert(key.clone(), value.clone());
    }
    let mut out = obj.clone();
    for (field, source) in stale {
        let text = formula_text(cat, table, &source, &row, sc_expr::Operation::Update)
            .await
            .map_err(|e| embed_failed(table, field, &e))?;
        let value = match text {
            Some(text) => embed_texts(cat, table, field, &[text])
                .await
                .map_err(|e| embed_failed(table, field, &e))?
                .into_iter()
                .next()
                .map_or(Json::Null, |v| sc_types::vector::to_json(&v)),
            None => Json::Null,
        };
        out.insert(field.base.name.clone(), value);
    }
    Ok(Cow::Owned(out))
}

/// What [`embed_rows`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct EmbedReport {
    /// Rows given an embedding.
    pub embedded: u64,
    /// Rows whose formula gave no text, left empty.
    pub skipped: u64,
}

/// Back-fill `field` of `table`: compute the embedding of every row whose
/// vector is empty, at most `max_rows` of them (all when `None`), a batch per
/// embedding request.
///
/// A system write: the rows are not the caller's edits, so no trigger runs and
/// no row history is written for them beyond what the database records.
pub async fn embed_rows(
    cat: &Catalog,
    table: &Table,
    field_name: &str,
    max_rows: Option<u64>,
) -> Result<EmbedReport> {
    let (field, _) = vector_field(table, field_name)?;
    let Some(source) = embed_config(field).from else {
        return Err(Error::invalid(format!(
            "`{}.{field_name}` is not computed: it has no `{ATTR_EMBED_FROM}` formula to \
             back-fill it from",
            table.name
        )));
    };
    let pk = crate::rows::single_pk(table)?;
    let provider = cat.provider(table)?;
    let mut report = EmbedReport {
        embedded: 0,
        skipped: 0,
    };
    // The rows already looked at whose formula gave nothing: they stay empty,
    // so the next batch must not read them again.
    let mut passed: Vec<Value> = Vec::new();
    loop {
        let done = report.embedded + report.skipped;
        let room = max_rows.map_or(u64::MAX, |m| m.saturating_sub(done));
        if room == 0 {
            break;
        }
        let mut filter = Expr::unary(sc_query::UnOp::IsNull, Expr::col(field_name));
        if !passed.is_empty() {
            filter = filter.and(Expr::unary(
                sc_query::UnOp::Not,
                Expr::In {
                    e: Box::new(Expr::col(pk.clone())),
                    set: sc_query::InSet::List(passed.iter().cloned().map(Expr::lit).collect()),
                },
            ));
        }
        let mut select = Select::from(Source::table(table.name.clone()))
            .columns(crate::user_rows::projection(table))
            .filter(filter)
            .limit(room.min(sc_llm::embed::EMBED_BATCH as u64));
        select.order = vec![OrderBy::asc(Expr::col(pk.clone()))];
        let rows = provider.query(&select).await?.try_collect().await?;
        if rows.is_empty() {
            break;
        }
        let mut keyed = Vec::new();
        for row in &rows {
            let Json::Object(obj) = crate::rows::row_to_json(row) else {
                continue;
            };
            let key = row.get(&pk).cloned().unwrap_or(Value::Null);
            match formula_text(cat, table, &source, &obj, sc_expr::Operation::Update)
                .await
                .map_err(|e| embed_failed(table, field, &e))?
            {
                Some(text) => keyed.push((key, text)),
                None => {
                    report.skipped += 1;
                    passed.push(key);
                }
            }
        }
        if keyed.is_empty() {
            continue;
        }
        let texts: Vec<String> = keyed.iter().map(|(_, t)| t.clone()).collect();
        let vectors = embed_texts(cat, table, field, &texts)
            .await
            .map_err(|e| embed_failed(table, field, &e))?;
        for ((key, _), vector) in keyed.into_iter().zip(vectors) {
            let update = Update {
                table: table.name.clone(),
                assignments: vec![Assignment::new(
                    field_name.to_owned(),
                    sc_catalog::write_expr(
                        Some(field),
                        Value::Json(sc_types::vector::to_json(&vector)),
                    ),
                )],
                filter: Some(Expr::col(pk.clone()).eq(Expr::lit(key))),
                returning: Vec::new(),
            };
            provider
                .write(&Statement::from(update))
                .await?
                .try_collect()
                .await?;
            report.embedded += 1;
        }
    }
    Ok(report)
}
