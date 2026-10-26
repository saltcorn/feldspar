//! Full-text search: the [`TextSearch`] a read matches and ranks by
//! (TODO-agent-skills AS1.1, AS1.2).
//!
//! One value, two expressions: [`Expr::TextMatch`](crate::Expr::TextMatch) is
//! the predicate and [`Expr::TextRank`](crate::Expr::TextRank) the relevance a
//! read orders by, **higher is better** on both backends. What they render to
//! is the one place the backends genuinely differ, so it is decided here:
//!
//! - **Postgres**: `to_tsvector(cfg, coalesce(a, '') || ' ' || coalesce(b, ''))
//!   @@ websearch_to_tsquery(cfg, $1)`, and `ts_rank` of the same two. The
//!   document is spelled by [`text_search_document`], which is also what the
//!   full-text constraint's index is created over — an index is only used by a
//!   query over *the same expression*, so there is one function that writes it.
//!   The configuration and the empty string are inlined rather than bound for the
//!   same reason: a bound parameter is a different expression from a constant,
//!   and a plan made for it would not use the index.
//! - **SQLite**: an FTS5 table the constraint maintains beside the table
//!   ([`Fts5Index`]). The phrase is translated from web-search syntax into an
//!   FTS5 query string ([`fts5_query`]) and bound; the match is `key IN (SELECT
//!   rowid FROM idx WHERE idx MATCH ?)` and the rank is `-bm25(idx)`. A search
//!   over fewer columns than the index holds is an FTS5 column filter, so it
//!   still uses the index. Without an index (a table small enough to scan) every
//!   term is a `LIKE` over the columns, and the rank is how many of them matched.
//!
//! The phrase is always data: it is bound, and on SQLite every word is quoted
//! as an FTS5 string, so a phrase can say "or" and "-" and nothing else.

use serde::{Deserialize, Serialize};

use crate::SqlDialect;

/// A full-text search over some text columns of one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSearch {
    /// The table (or alias) the columns belong to — what each column reference
    /// is qualified with.
    pub table: String,
    /// The columns searched, in the order the document is built. On Postgres
    /// this is the document, so it must be the index's own column list for the
    /// index to be used.
    pub columns: Vec<String>,
    /// The text-search configuration (`english`, `simple`, …).
    pub language: String,
    /// What to look for, in web-search syntax: words, `"quoted phrases"`,
    /// `-excluded` words and `or` (see [`WebQuery::parse`]).
    pub phrase: String,
    /// The FTS5 table that answers this search on SQLite. `None` scans; Postgres
    /// ignores it, because its index is found by the planner.
    pub fts5: Option<Fts5Index>,
}

/// The FTS5 table a SQLite full-text constraint keeps beside its table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fts5Index {
    /// The virtual table's name.
    pub name: String,
    /// The column of the searched table whose value is the FTS5 rowid — its
    /// single integer key.
    pub key: String,
    /// The columns the index holds, in its order. A search over fewer of them
    /// filters to those columns.
    pub columns: Vec<String>,
}

/// One word or quoted phrase of a [`WebQuery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The words, more than one for a quoted phrase.
    pub words: Vec<String>,
    /// Whether it was written `-word`: a row containing it does not match.
    pub negated: bool,
}

/// A phrase in web-search syntax, parsed: the rows matching **any** group, where
/// a row matches a group when it contains each of its positive terms and none of
/// its negated ones.
///
/// That is also how both engines read it — `a b or c` is `(a & b) | c` to
/// Postgres's `websearch_to_tsquery` and `(a AND b) OR c` to FTS5, where AND
/// binds tighter than OR.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebQuery {
    /// The groups, ORed.
    pub groups: Vec<Vec<Term>>,
}

impl WebQuery {
    /// Parse `phrase`: whitespace separates terms, `"…"` quotes a phrase, a
    /// leading `-` negates a term and a bare `or` (any case) separates groups.
    /// Everything else is a word — there is no syntax error to make, which is
    /// what a search box typed into by a person (or a model) needs.
    pub fn parse(phrase: &str) -> WebQuery {
        let mut groups: Vec<Vec<Term>> = vec![Vec::new()];
        let mut chars = phrase.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            let Some(&first) = chars.peek() else { break };
            let mut negated = false;
            if first == '-' {
                chars.next();
                negated = true;
            }
            let words: Vec<String> = if chars.peek() == Some(&'"') {
                chars.next();
                let mut quoted = String::new();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    quoted.push(c);
                }
                quoted.split_whitespace().map(str::to_owned).collect()
            } else {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                if !negated && word.eq_ignore_ascii_case("or") {
                    if groups.last().is_some_and(|g| !g.is_empty()) {
                        groups.push(Vec::new());
                    }
                    continue;
                }
                vec![word].into_iter().filter(|w| !w.is_empty()).collect()
            };
            if !words.is_empty()
                && let Some(group) = groups.last_mut()
            {
                group.push(Term { words, negated });
            }
        }
        groups.retain(|g| !g.is_empty());
        WebQuery { groups }
    }

    /// Whether there is nothing to look for — a phrase of only spaces, quotes
    /// and `or`s.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// An FTS5 string literal: double-quoted, inner quotes doubled. Quoting every
/// term is what keeps FTS5's own operators (`NEAR`, `*`, `^`, `:`) out of reach
/// of the phrase.
fn fts5_string(words: &[String]) -> String {
    format!("\"{}\"", words.join(" ").replace('"', "\"\""))
}

/// The FTS5 query for the groups of `query` that have a positive term, or `None`
/// when none has — FTS5's `NOT` is binary, so a group of only exclusions is not
/// a query it can run (see [`negative_only_groups`]).
///
/// `columns` restricts the match to those columns of the index, as a column
/// filter (`{a b} : (…)`).
pub fn fts5_query(query: &WebQuery, columns: Option<&[String]>) -> Option<String> {
    let groups: Vec<String> = query
        .groups
        .iter()
        .filter_map(|group| {
            let positive: Vec<String> = group
                .iter()
                .filter(|t| !t.negated)
                .map(|t| fts5_string(&t.words))
                .collect();
            if positive.is_empty() {
                return None;
            }
            let mut text = positive.join(" AND ");
            for term in group.iter().filter(|t| t.negated) {
                text.push_str(" NOT ");
                text.push_str(&fts5_string(&term.words));
            }
            Some(format!("({text})"))
        })
        .collect();
    if groups.is_empty() {
        return None;
    }
    Some(with_column_filter(groups.join(" OR "), columns))
}

/// The FTS5 query matching *any* exclusion of each group that has no positive
/// term — the rows such a group excludes, so the group itself is "not these".
pub fn negative_only_groups(query: &WebQuery, columns: Option<&[String]>) -> Vec<String> {
    query
        .groups
        .iter()
        .filter(|g| g.iter().all(|t| t.negated))
        .map(|group| {
            let any: Vec<String> = group.iter().map(|t| fts5_string(&t.words)).collect();
            with_column_filter(any.join(" OR "), columns)
        })
        .collect()
}

/// `{a b} : (query)` when the search is over a subset of the index's columns.
fn with_column_filter(query: String, columns: Option<&[String]>) -> String {
    match columns {
        Some(columns) => {
            let names: Vec<String> = columns
                .iter()
                .map(|c| fts5_string(std::slice::from_ref(c)))
                .collect();
            format!("{{{}}} : ({query})", names.join(" "))
        }
        None => query,
    }
}

/// A `LIKE` pattern matching `words` anywhere, with `%`, `_` and the escape
/// character itself escaped — a search for `50%` is a search for those three
/// characters.
pub fn like_pattern(words: &[String]) -> String {
    let mut out = String::from("%");
    for c in words.join(" ").chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Whether `name` can be a text-search configuration name. It is inlined into
/// SQL (see the module docs), so anything but a plain lowercase identifier is
/// refused rather than escaped.
pub fn is_language_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The `to_tsvector` document over `columns` — **the** spelling, shared by a
/// full-text constraint's index and every search that hopes to use it.
///
/// Each column is coalesced to the empty string (one null would make the whole
/// concatenation null) and the columns are joined by a space. The configuration
/// is cast to `regconfig`, because the two-argument `to_tsvector` is immutable —
/// and so indexable — only in that form.
pub fn text_search_document<D: SqlDialect + ?Sized>(
    dialect: &D,
    table: Option<&str>,
    columns: &[String],
    language: &str,
) -> sc_error::Result<String> {
    if !is_language_name(language) {
        return Err(sc_error::Error::invalid(format!(
            "`{language}` is not a text-search configuration name"
        )));
    }
    if columns.is_empty() {
        return Err(sc_error::Error::query(
            "a full-text search needs at least one column",
        ));
    }
    let qualifier = table.map(|t| format!("{}.", dialect.quote_ident(t)));
    let parts: Vec<String> = columns
        .iter()
        .map(|c| {
            format!(
                "coalesce({}{}, '')",
                qualifier.as_deref().unwrap_or(""),
                dialect.quote_ident(c)
            )
        })
        .collect();
    Ok(format!(
        "to_tsvector('{language}'::regconfig, {})",
        parts.join(" || ' ' || ")
    ))
}

/// The FTS5 tokenizer a SQLite full-text index uses for `language`.
///
/// FTS5 has one stemmer, Porter's, which is English's; every other language is
/// tokenized without stemming, which is what Postgres's `simple` does too.
pub fn fts5_tokenizer(language: &str) -> &'static str {
    match language {
        "english" => "porter unicode61 remove_diacritics 2",
        _ => "unicode61 remove_diacritics 2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn a_phrase_parses_into_groups_of_terms() {
        let q = WebQuery::parse(r#"signal -"segmentation fault" or crash"#);
        assert_eq!(
            q.groups,
            vec![
                vec![
                    Term {
                        words: words("signal"),
                        negated: false
                    },
                    Term {
                        words: words("segmentation fault"),
                        negated: true
                    },
                ],
                vec![Term {
                    words: words("crash"),
                    negated: false
                }],
            ]
        );
        // Stray `or`s, empty quotes and bare dashes are not errors.
        assert!(WebQuery::parse(r#" or "" - OR "#).is_empty());
        // An unterminated quote runs to the end.
        assert_eq!(
            WebQuery::parse(r#""big cat"#).groups[0][0].words,
            words("big cat")
        );
    }

    #[test]
    fn every_term_is_an_fts5_string() {
        let q = WebQuery::parse(r#"a "b c" -d or NEAR(x) e*"#);
        assert_eq!(
            fts5_query(&q, None).as_deref(),
            Some(r#"("a" AND "b c" NOT "d") OR ("NEAR(x)" AND "e*")"#)
        );
        assert_eq!(
            fts5_query(&WebQuery::parse(r#"say "hi""#), Some(&["title".to_owned()])).as_deref(),
            Some(r#"{"title"} : (("say" AND "hi"))"#)
        );
        let q = WebQuery::parse("-a -b or c");
        assert_eq!(fts5_query(&q, None).as_deref(), Some(r#"("c")"#));
        assert_eq!(
            negative_only_groups(&q, None),
            vec![r#""a" OR "b""#.to_owned()]
        );
    }

    struct Pg;
    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{ident}\"")
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }
    struct Lite;
    impl SqlDialect for Lite {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{ident}\"")
        }
        fn placeholder(&self, position: usize) -> String {
            format!("?{position}")
        }
        fn is_sqlite(&self) -> bool {
            true
        }
    }

    fn search(fts5: bool, columns: &[&str]) -> TextSearch {
        TextSearch {
            table: "post".into(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            language: "english".into(),
            phrase: "rust -java".into(),
            fts5: fts5.then(|| Fts5Index {
                name: "sc_fts_post".into(),
                key: "id".into(),
                columns: vec!["title".into(), "body".into()],
            }),
        }
    }

    fn render(dialect: &dyn SqlDialect, s: TextSearch) -> (String, Vec<crate::Value>) {
        let select = crate::Select::from(crate::Source::table("post"))
            .columns(vec![crate::Projection::expr(crate::Expr::TextRank(
                Box::new(s.clone()),
            ))])
            .filter(crate::Expr::TextMatch(Box::new(s)));
        dialect.render(&select.into()).expect("renders")
    }

    #[test]
    fn postgres_searches_the_indexed_document() {
        let (sql, binds) = render(&Pg, search(false, &["title", "body"]));
        let document = "to_tsvector('english'::regconfig, coalesce(\"post\".\"title\", '') \
                        || ' ' || coalesce(\"post\".\"body\", ''))";
        assert!(
            sql.contains(&format!(
                "WHERE ({document} @@ websearch_to_tsquery('english'::regconfig, $2))"
            )),
            "{sql}"
        );
        assert!(
            sql.starts_with(&format!("SELECT ts_rank({document}, ")),
            "{sql}"
        );
        // The phrase is bound, never inlined.
        assert_eq!(binds, vec![crate::Value::Text("rust -java".into()); 2]);
        // A configuration name is inlined, so it is checked.
        let mut bad = search(false, &["title"]);
        bad.language = "english'; drop".into();
        let select = crate::Select::from(crate::Source::table("post"))
            .filter(crate::Expr::TextMatch(Box::new(bad)));
        assert!(Pg.render(&select.into()).is_err());
    }

    #[test]
    fn sqlite_asks_its_fts5_table_and_filters_columns() {
        let (sql, binds) = render(&Lite, search(true, &["title", "body"]));
        assert!(
            sql.contains(
                "WHERE (\"post\".\"id\" IN (SELECT rowid FROM \"sc_fts_post\" \
                 WHERE \"sc_fts_post\" MATCH ?2))"
            ),
            "{sql}"
        );
        assert!(sql.contains("-bm25(\"sc_fts_post\")"), "{sql}");
        assert_eq!(
            binds[1],
            crate::Value::Text(r#"("rust" NOT "java")"#.into())
        );
        let (_, binds) = render(&Lite, search(true, &["title"]));
        assert_eq!(
            binds[1],
            crate::Value::Text(r#"{"title"} : (("rust" NOT "java"))"#.into())
        );
    }

    #[test]
    fn sqlite_scans_without_an_index() {
        let (sql, binds) = render(&Lite, search(false, &["title"]));
        assert!(
            sql.contains(
                "WHERE (((coalesce(\"post\".\"title\", '') LIKE ?2 ESCAPE '\\') AND \
                 (NOT (coalesce(\"post\".\"title\", '') LIKE ?3 ESCAPE '\\'))))"
            ),
            "{sql}"
        );
        assert_eq!(binds[1], crate::Value::Text("%rust%".into()));
    }

    #[test]
    fn a_like_pattern_escapes_its_wildcards() {
        assert_eq!(like_pattern(&words("50% off_")), r"%50\% off\_%");
    }
}
