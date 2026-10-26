//! Full-text search on a read (TODO-agent-skills AS1.3).
//!
//! A search is a [`TextSearch`] on the [`RowQuery`](crate::rows::RowQuery):
//! its match is ANDed into the read's filter and its rank is what `relevance`
//! orders by, so it rides inside the caller's own read — ownership, RLS, the
//! row cap — and narrows the rows they may see, never some wider set.
//!
//! Two rules decide what a search may look at:
//!
//! - **Only fields the caller can read are matched** (§7.3). A secret is not
//!   indexed at all (its column holds ciphertext), and a column the API never
//!   shows — the users table's password hash — is never searched: a match on it
//!   would let a caller read it back one guess at a time. A search over fewer
//!   columns than the index holds still uses the index on SQLite (an FTS5 column
//!   filter); on Postgres it is a different document from the indexed one, so it
//!   is a scan, and the next rule applies to it.
//! - **A search the index does not answer is a scan**, which is only done on a
//!   table of at most [`SCAN_LIMIT`] rows. A bigger one is refused with what to
//!   do about it: add the full-text constraint.

use sc_catalog::{Catalog, Table, full_text_columns, full_text_constraint, indexed_text_search};
use sc_error::{Error, Result};
use sc_query::{Expr, OrderBy, Projection, Select, Source, TextSearch, Value};

use crate::user_rows::visible_field;

/// The most rows a table without a full-text index may have and still be
/// searched — by reading every one of them.
pub const SCAN_LIMIT: u64 = 5_000;

/// The text-search configuration a table without a full-text constraint is
/// searched in: no stemming and no stop words, which is the only choice that is
/// right in every language.
const SCAN_LANGUAGE: &str = "simple";

/// The word an `order` names relevance by, on a read with a search.
pub const RELEVANCE: &str = "relevance";

/// The search for `phrase` over `fields` of `table` (every readable text field
/// when `None`), ready for [`RowQuery::searching`](crate::rows::RowQuery::searching).
///
/// Refused when there is nothing to look for, when a named field is not a text
/// field the caller can read, or when the table has no text field at all.
pub fn text_search(table: &Table, phrase: &str, fields: Option<&[String]>) -> Result<TextSearch> {
    let phrase = phrase.trim();
    if sc_query::WebQuery::parse(phrase).is_empty() {
        return Err(Error::invalid(
            "a search needs something to look for: words, \"a quoted phrase\", -an exclusion",
        ));
    }
    let searchable: Vec<String> = full_text_columns(table)
        .into_iter()
        .filter(|c| visible_field(table, c).is_some())
        .collect();
    let columns = match fields {
        None => searchable,
        Some(fields) => {
            for name in fields {
                let Some(field) = visible_field(table, name) else {
                    return Err(Error::invalid(format!(
                        "`{}` has no field `{name}` to search",
                        table.name
                    )));
                };
                if !searchable.contains(name) {
                    let why = if field.is_secret() {
                        "a secret"
                    } else if field.is_calc() {
                        "a calculated field"
                    } else if !sc_catalog::is_full_text_field(field) {
                        "not a text field"
                    } else {
                        "not searchable"
                    };
                    return Err(Error::invalid(format!(
                        "`{}.{name}` is {why}, so it cannot be searched; the searchable fields \
                         are {}",
                        table.name,
                        list(&searchable)
                    )));
                }
            }
            // In the table's own order, once each: the document a search over
            // every field builds has to be the indexed one, character for
            // character, and that one is in field order.
            searchable
                .into_iter()
                .filter(|c| fields.contains(c))
                .collect()
        }
    };
    if columns.is_empty() {
        return Err(Error::invalid(format!(
            "`{}` has no text fields to search",
            table.name
        )));
    }
    Ok(
        indexed_text_search(table, columns.clone(), phrase).unwrap_or_else(|| TextSearch {
            table: table.name.clone(),
            columns,
            language: SCAN_LANGUAGE.to_owned(),
            phrase: phrase.to_owned(),
            fts5: None,
        }),
    )
}

/// The ordering by relevance, best match first.
pub fn relevance(search: &TextSearch) -> OrderBy {
    OrderBy::desc(Expr::TextRank(Box::new(search.clone())))
}

/// Whether the table's full-text index answers `search`, on the backend the
/// table lives on.
pub fn is_indexed(table: &Table, search: &TextSearch, sqlite: bool) -> bool {
    if full_text_constraint(table).is_none() {
        return false;
    }
    if sqlite {
        search.fts5.is_some()
    } else {
        search.columns == full_text_columns(table)
    }
}

/// Refuse a search the index does not answer over a table too big to scan.
pub(crate) async fn require_answerable(
    cat: &Catalog,
    table: &Table,
    search: &TextSearch,
) -> Result<()> {
    let sqlite = cat.driver_named(&table.database)?.dialect().is_sqlite();
    if is_indexed(table, search, sqlite) {
        return Ok(());
    }
    // At most one row past the limit is read, so asking costs no more than the
    // limit allows a search to cost.
    let sample = Select::from(Source::table(table.name.clone()))
        .columns(vec![Projection::expr(Expr::lit(1_i64))])
        .limit(SCAN_LIMIT + 1);
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
    if n <= SCAN_LIMIT {
        return Ok(());
    }
    let why = match full_text_constraint(table) {
        None => format!("`{}` has no full-text search index", table.name),
        Some(_) => format!(
            "the full-text search index of `{}` covers every text field together, and this \
             search is over only some of them ({})",
            table.name,
            list(&search.columns)
        ),
    };
    Err(Error::invalid(format!(
        "{why}, and it has more than {SCAN_LIMIT} rows, so searching it would read every row. \
         Add a full-text search constraint to `{}` (the table's Constraints tab in the admin \
         UI, or the schema operation `add_constraint` with type `full_text_search`), or narrow \
         the read with a filter instead",
        table.name
    )))
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        return "none".to_owned();
    }
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
